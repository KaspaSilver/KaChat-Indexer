//! The registry applier (KACHAT_NAMES_INDEXER.md §B) — the heart of the follower.
//!
//! Given an accepted transaction's inputs (each `previous_outpoint` + signature script)
//! and outputs (`scriptPublicKey` + value), it mutates the tracked registry: it finds
//! inputs that spend a tracked gap/name/offer, decodes the spend (dispatch tag + args),
//! computes the new state(s) with the transition rules, **verifies every new state's
//! P2SH against a real output**, and records the resulting UTXOs. Nothing is tracked that
//! doesn't verify.
//!
//! This layer is pure and in-memory so it can be driven by synthetic transactions in
//! tests; the chain reader (virtual-chain order + reorg) and the Postgres persistence are
//! thin wrappers over it.

use std::collections::HashMap;

use crate::transition::{self, Entry};
use crate::{decode_sig_script, GapState, NameState, OfferState};

/// A transaction outpoint: `(txid, output index)`.
pub type Outpoint = ([u8; 32], u32);

/// One tracked registry UTXO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tracked {
    Gap(GapState),
    Name(NameState),
    Offer(OfferState),
}

/// The per-contract redeem layout from the manifest: `redeem = prefix ‖ state ‖ suffix`,
/// `spk = P2SH(redeem)`. `state_offset`/`state_len` locate the state within the redeem.
#[derive(Debug, Clone)]
pub struct ContractTemplate {
    pub prefix: Vec<u8>,
    pub suffix: Vec<u8>,
    pub state_offset: usize,
    pub state_len: usize,
}

impl ContractTemplate {
    pub fn redeem(&self, state: &[u8]) -> Vec<u8> {
        [self.prefix.as_slice(), state, self.suffix.as_slice()].concat()
    }
    pub fn spk(&self, state: &[u8]) -> Vec<u8> {
        crate::p2sh_script(&self.redeem(state))
    }
}

/// All three contract templates for one network (built from the manifest's artifacts).
#[derive(Debug, Clone)]
pub struct Templates {
    pub gap: ContractTemplate,
    pub name: ContractTemplate,
    pub offer: ContractTemplate,
}

impl Templates {
    fn gap_spk(&self, s: &GapState) -> Vec<u8> {
        self.gap.spk(&s.encode())
    }
    fn name_spk(&self, s: &NameState) -> Vec<u8> {
        self.name.spk(&s.encode())
    }
    fn offer_spk(&self, s: &OfferState) -> Vec<u8> {
        self.offer.spk(&s.encode())
    }
}

/// A transaction input, as the follower needs it.
#[derive(Debug, Clone)]
pub struct TxInput {
    pub previous_outpoint: Outpoint,
    pub signature_script: Vec<u8>,
}

/// A transaction output.
#[derive(Debug, Clone)]
pub struct TxOutput {
    pub script_public_key: Vec<u8>,
    pub value: u64,
}

/// An accepted transaction, in the shape the applier consumes (the chain reader maps the
/// node's txs into this).
#[derive(Debug, Clone)]
pub struct Tx {
    pub id: [u8; 32],
    pub inputs: Vec<TxInput>,
    pub outputs: Vec<TxOutput>,
    pub payload: Vec<u8>,
    pub accepting_daa: u64,
    pub block_time: i64,
}

/// A name-registry event, emitted as the applier mutates state (for history + pushes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub op: &'static str,
    pub key: [u8; 32],
    pub tx_id: [u8; 32],
    pub daa: u64,
    pub at: i64,
}

/// The tracked registry: gaps/names/offers by outpoint, profiles by address.
#[derive(Debug, Default)]
pub struct Registry {
    pub utxos: HashMap<Outpoint, Tracked>,
    /// address (33-ish bytes / key) -> (profile json, accepting order key)
    pub profiles: HashMap<Vec<u8>, (String, (u64, [u8; 32]))>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the genesis gap (`(00..00, ff..ff)` at the genesis outpoint).
    pub fn seed_genesis(&mut self, outpoint: Outpoint, gap: GapState) {
        self.utxos.insert(outpoint, Tracked::Gap(gap));
    }

    pub fn names(&self) -> impl Iterator<Item = (&Outpoint, &NameState)> {
        self.utxos.iter().filter_map(|(o, t)| match t {
            Tracked::Name(n) => Some((o, n)),
            _ => None,
        })
    }

    pub fn name_by_key(&self, key: &[u8; 32]) -> Option<(Outpoint, NameState)> {
        self.names().find(|(_, n)| &n.key == key).map(|(o, n)| (*o, *n))
    }

    /// Find the output index whose scriptPublicKey matches `spk`, not already claimed.
    fn find_output(tx: &Tx, spk: &[u8], used: &mut Vec<usize>) -> Option<usize> {
        for (i, o) in tx.outputs.iter().enumerate() {
            if !used.contains(&i) && o.script_public_key == spk {
                used.push(i);
                return Some(i);
            }
        }
        None
    }

    /// Apply one accepted transaction. Returns the registry events it produced.
    pub fn apply(&mut self, templates: &Templates, tx: &Tx) -> Vec<Event> {
        let mut events = Vec::new();

        // Which tracked UTXOs does this tx spend, and under which entry?
        let mut spends: Vec<(usize, Outpoint, Tracked, Entry, crate::SigScript)> = Vec::new();
        for (idx, input) in tx.inputs.iter().enumerate() {
            let Some(tracked) = self.utxos.get(&input.previous_outpoint).cloned() else {
                continue;
            };
            let Some(sig) = decode_sig_script(&input.signature_script) else {
                continue;
            };
            let Some(entry) = transition::entry_for_tag(&sig.dispatch_tag) else {
                continue;
            };
            spends.push((idx, input.previous_outpoint, tracked, entry, sig));
        }

        // Offers created by this tx (payload marker, §B4) are handled regardless of spends.
        self.apply_offer_marker(templates, tx, &mut events);
        // Profiles (§C): a self-send carrying kchat:1:profile:<json>.
        self.apply_profile(tx);

        if spends.is_empty() {
            return events;
        }

        let mut used_outputs: Vec<usize> = Vec::new();

        // The multi-input exit (gap merge + name release/reclaim + gap absorbed) and the
        // offer-accept pair are recognised across inputs; everything else is per-name.
        let has = |e: Entry| spends.iter().any(|s| s.3 == e);

        if has(Entry::GapMerge) {
            // Exit: collapse the two gaps around the released/reclaimed name.
            let lo_gap = spends.iter().find_map(|s| match (&s.2, s.3) {
                (Tracked::Gap(g), Entry::GapMerge) => Some(*g),
                _ => None,
            });
            let hi_gap = spends.iter().find_map(|s| match (&s.2, s.3) {
                (Tracked::Gap(g), Entry::GapAbsorbed) => Some(*g),
                _ => None,
            });
            if let (Some(lo), Some(hi)) = (lo_gap, hi_gap) {
                let merged = transition::merge_gaps(&lo, &hi);
                if let Some(i) = Self::find_output(tx, &templates.gap_spk(&merged), &mut used_outputs) {
                    self.utxos.insert((tx.id, i as u32), Tracked::Gap(merged));
                }
            }
            for s in &spends {
                if let Tracked::Name(n) = &s.2 {
                    events.push(self.event(if s.3 == Entry::NameReclaim { "reclaim" } else { "release" }, n.key, tx));
                }
            }
        } else if has(Entry::GapRegister) {
            self.apply_register(templates, tx, &spends, &mut used_outputs, &mut events);
        } else {
            // Per-name continuations (transfer / list / buy / renew) + offer accept.
            for (_, _, tracked, entry, sig) in &spends {
                match (tracked, entry) {
                    (Tracked::Name(old), Entry::NameTransfer | Entry::NameBuy) => {
                        let new_owner = sig.args.first().and_then(|a| a.data()).and_then(|d| d.try_into().ok());
                        if let Some(owner) = new_owner {
                            let ns = transition::name_transfer(old, owner);
                            self.record_name(templates, tx, &ns, &mut used_outputs, &mut events,
                                if *entry == Entry::NameBuy { "sale" } else { "transfer" });
                        }
                    }
                    (Tracked::Name(old), Entry::NameList) => {
                        if let Some(price) = sig.args.first().and_then(|a| a.as_i64()) {
                            let ns = transition::name_list(old, price);
                            self.record_name(templates, tx, &ns, &mut used_outputs, &mut events,
                                if price == 0 { "delist" } else { "list" });
                        }
                    }
                    (Tracked::Name(old), Entry::NameRenew) => {
                        if let Some(years) = sig.args.first().and_then(|a| a.as_i64()) {
                            let ns = transition::name_renew(old, years);
                            self.record_name(templates, tx, &ns, &mut used_outputs, &mut events, "renew");
                        }
                    }
                    (Tracked::Offer(offer), Entry::OfferAccept) => {
                        // The accepted name goes to the buyer; verify a name output matches.
                        if let Some((_, name)) = self.name_by_key(&offer.key) {
                            let ns = transition::offer_accept(&name, offer);
                            self.record_name(templates, tx, &ns, &mut used_outputs, &mut events, "offer_accepted");
                        }
                    }
                    _ => {}
                }
            }
        }

        // Every spent tracked UTXO is consumed.
        for (_, outpoint, _, _, _) in &spends {
            self.utxos.remove(outpoint);
        }

        events
    }

    fn apply_register(
        &mut self,
        templates: &Templates,
        tx: &Tx,
        spends: &[(usize, Outpoint, Tracked, Entry, crate::SigScript)],
        used: &mut Vec<usize>,
        events: &mut Vec<Event>,
    ) {
        let Some((_, _, Tracked::Gap(gap), _, sig)) =
            spends.iter().find(|s| s.3 == Entry::GapRegister).cloned()
        else {
            return;
        };
        // register(name, ownerKey, salt, now, years, namePrefix, nameSuffix)
        let name = sig.args.first().and_then(|a| a.data()).map(|d| d.to_vec());
        let owner: Option<[u8; 32]> = sig.args.get(1).and_then(|a| a.data()).and_then(|d| d.try_into().ok());
        let now = sig.args.get(3).and_then(|a| a.as_i64());
        let years = sig.args.get(4).and_then(|a| a.as_i64());
        let (Some(name), Some(owner), Some(now), Some(years)) = (name, owner, now, years) else {
            return;
        };
        if !crate::is_valid_name(&name) {
            return;
        }
        let (left, right, nm) = transition::register(&gap, &name, owner, now, years);
        for g in [left, right] {
            if let Some(i) = Self::find_output(tx, &templates.gap_spk(&g), used) {
                self.utxos.insert((tx.id, i as u32), Tracked::Gap(g));
            }
        }
        if let Some(i) = Self::find_output(tx, &templates.name_spk(&nm), used) {
            self.utxos.insert((tx.id, i as u32), Tracked::Name(nm));
            events.push(self.event("register", nm.key, tx));
        }
    }

    fn record_name(
        &mut self,
        templates: &Templates,
        tx: &Tx,
        ns: &NameState,
        used: &mut Vec<usize>,
        events: &mut Vec<Event>,
        op: &'static str,
    ) {
        if let Some(i) = Self::find_output(tx, &templates.name_spk(ns), used) {
            self.utxos.insert((tx.id, i as u32), Tracked::Name(*ns));
            events.push(self.event(op, ns.key, tx));
        }
    }

    /// §B4: an offer-creating tx carries `kchat:1:offer:<keyHex>:<buyerHex>:<refundAfterDaa>`;
    /// trust it only if `P2SH(offerState)` matches one of the outputs.
    fn apply_offer_marker(&mut self, templates: &Templates, tx: &Tx, events: &mut Vec<Event>) {
        let Ok(text) = std::str::from_utf8(&tx.payload) else { return };
        let Some(rest) = text.strip_prefix("kchat:1:offer:") else { return };
        let parts: Vec<&str> = rest.split(':').collect();
        let [key_hex, buyer_hex, refund_dec] = parts.as_slice() else { return };
        let (Ok(key), Ok(buyer)) = (decode32(key_hex), decode32(buyer_hex)) else { return };
        let Ok(refund_after) = refund_dec.parse::<i64>() else { return };
        let offer = OfferState { key, buyer, refund_after };
        let spk = templates.offer_spk(&offer);
        for (i, o) in tx.outputs.iter().enumerate() {
            if o.script_public_key == spk {
                self.utxos.insert((tx.id, i as u32), Tracked::Offer(offer));
                events.push(self.event("offer", key, tx));
                return;
            }
        }
    }

    /// §C: a profile record is `kchat:1:profile:<json>` on a self-send (an input of the
    /// address, an output back to it). The newest by accepting order wins. The sender
    /// resolution is left to the chain reader; here we record on the marker + a self output.
    fn apply_profile(&mut self, tx: &Tx) {
        let Ok(text) = std::str::from_utf8(&tx.payload) else { return };
        let Some(json) = text.strip_prefix("kchat:1:profile:") else { return };
        // Validate against the 2026-10-02 format (one allowlisted social + linktr.ee +
        // primaryName, <= 2 KB); reject old/oversized/invalid records outright.
        if crate::parse_profile(json).is_none() {
            return;
        }
        // The chain reader supplies the resolved address via the first output's spk as the
        // identity key (self-send). A fuller sender check lives in the reader.
        if let Some(first) = tx.outputs.first() {
            let addr = first.script_public_key.clone();
            let order = (tx.accepting_daa, tx.id);
            let newer = self.profiles.get(&addr).map(|(_, o)| order > *o).unwrap_or(true);
            if newer {
                self.profiles.insert(addr, (json.to_string(), order));
            }
        }
    }

    fn event(&self, op: &'static str, key: [u8; 32], tx: &Tx) -> Event {
        Event { op, key, tx_id: tx.id, daa: tx.accepting_daa, at: tx.block_time }
    }
}

fn decode32(s: &str) -> Result<[u8; 32], ()> {
    let v = hex::decode(s).map_err(|_| ())?;
    v.try_into().map_err(|_| ())
}

#[cfg(test)]
mod tests;
