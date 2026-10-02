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
    /// Build the three templates from a names manifest's `artifacts` (`KachatGap`,
    /// `KachatName`, `KachatOffer`: `prefixHex`, `suffixHex`, `stateSpan {offset,len}`).
    pub fn from_manifest(manifest: &serde_json::Value) -> Option<Self> {
        let one = |contract: &str| -> Option<ContractTemplate> {
            let a = manifest.get("artifacts")?.get(contract)?;
            Some(ContractTemplate {
                prefix: hex::decode(a.get("prefixHex")?.as_str()?).ok()?,
                suffix: hex::decode(a.get("suffixHex")?.as_str()?).ok()?,
                state_offset: a.get("stateSpan")?.get("offset")?.as_u64()? as usize,
                state_len: a.get("stateSpan")?.get("len")?.as_u64()? as usize,
            })
        };
        Some(Templates { gap: one("KachatGap")?, name: one("KachatName")?, offer: one("KachatOffer")? })
    }

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
    /// The chain block that accepted this tx — the undo journal is keyed by it, so a reorg
    /// that removes the block undoes exactly its transactions.
    pub accepting_block: [u8; 32],
    pub accepting_daa: u64,
    pub block_time: i64,
}

/// What one applied transaction changed, enough to reverse it on a reorg (§4.1): the UTXOs
/// it added (to delete), the UTXOs it consumed (to restore), and any profile it replaced.
#[derive(Debug, Clone)]
struct UndoEntry {
    block: [u8; 32],
    added: Vec<Outpoint>,
    removed: Vec<(Outpoint, Tracked)>,
    /// (address, prior value) — `None` prior means the profile didn't exist before.
    profile: Option<(Vec<u8>, Option<(String, (u64, [u8; 32]))>)>,
}

/// A name-registry event, emitted as the applier mutates state (for history + pushes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub op: &'static str,
    pub key: [u8; 32],
    pub tx_id: [u8; 32],
    /// The chain block that accepted the tx (history rows are dropped when it is reorged out).
    pub block: [u8; 32],
    pub daa: u64,
    pub at: i64,
    /// Owner before (transfer/sale/release/reclaim/offer_accepted) — x-only key.
    pub from: Option<[u8; 32]>,
    /// Owner after (register/transfer/sale/offer_accepted) — x-only key; the buyer for `offer`.
    pub to: Option<[u8; 32]>,
    /// Sompi: the listing price (`list`), the price paid (`sale`).
    pub price: Option<i64>,
    /// Years bought (`register`, `renew`).
    pub years: Option<i64>,
}

/// The tracked registry: gaps/names/offers by outpoint, profiles by address.
#[derive(Debug, Default)]
pub struct Registry {
    pub utxos: HashMap<Outpoint, Tracked>,
    /// address (script-public-key bytes) -> (profile json, accepting order key)
    pub profiles: HashMap<Vec<u8>, (String, (u64, [u8; 32]))>,
    /// Reorg undo log, oldest first. Each entry reverses one applied transaction.
    journal: Vec<UndoEntry>,
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
        // Snapshot the UTXO keys so the undo journal can record exactly what this tx added.
        let before: std::collections::HashSet<Outpoint> = self.utxos.keys().copied().collect();

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
        // Profiles (§C): a self-send carrying kchat:1:profile:<json>. Capture the prior
        // value for undo.
        let profile_prior = self.apply_profile(tx);

        let mut used_outputs: Vec<usize> = Vec::new();
        if !spends.is_empty() {

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
                    let mut e = self.event(if s.3 == Entry::NameReclaim { "reclaim" } else { "release" }, n.key, tx);
                    e.from = Some(n.owner);
                    events.push(e);
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
                            // A transfer co-spent with an offer accept is the offer being taken.
                            let op = if *entry == Entry::NameBuy {
                                "sale"
                            } else if has(Entry::OfferAccept) {
                                "offer_accepted"
                            } else {
                                "transfer"
                            };
                            let price = (*entry == Entry::NameBuy).then_some(old.price);
                            self.record_name(templates, tx, &ns, &mut used_outputs, &mut events, op, Some(old), price, None);
                        }
                    }
                    (Tracked::Name(old), Entry::NameList) => {
                        if let Some(price) = sig.args.first().and_then(|a| a.as_i64()) {
                            let ns = transition::name_list(old, price);
                            self.record_name(templates, tx, &ns, &mut used_outputs, &mut events,
                                if price == 0 { "delist" } else { "list" }, Some(old), Some(price), None);
                        }
                    }
                    (Tracked::Name(old), Entry::NameRenew) => {
                        if let Some(years) = sig.args.first().and_then(|a| a.as_i64()) {
                            let ns = transition::name_renew(old, years);
                            self.record_name(templates, tx, &ns, &mut used_outputs, &mut events, "renew", Some(old), None, Some(years));
                        }
                    }
                    (Tracked::Name(old), Entry::NameExtend) => {
                        if let Some(years) = sig.args.first().and_then(|a| a.as_i64()) {
                            let ns = transition::name_extend(old, years);
                            self.record_name(templates, tx, &ns, &mut used_outputs, &mut events, "extend", Some(old), None, Some(years));
                        }
                    }
                    (Tracked::Offer(offer), Entry::OfferAccept) => {
                        // The accepted name goes to the buyer; verify a name output matches.
                        if let Some((_, name)) = self.name_by_key(&offer.key) {
                            let ns = transition::offer_accept(&name, offer);
                            self.record_name(templates, tx, &ns, &mut used_outputs, &mut events, "offer_accepted", Some(&name), None, None);
                        }
                    }
                    _ => {}
                }
            }
        }
        }

        // Record the undo entry (§4.1) and consume the spent UTXOs. `added` is whatever this
        // tx inserted (new gaps/names/offers); `removed` is the spent UTXOs' prior states.
        let removed: Vec<(Outpoint, Tracked)> =
            spends.iter().map(|(_, op, t, _, _)| (*op, t.clone())).collect();
        for (op, _) in &removed {
            self.utxos.remove(op);
        }
        let added: Vec<Outpoint> =
            self.utxos.keys().filter(|k| !before.contains(*k)).copied().collect();
        if !added.is_empty() || !removed.is_empty() || profile_prior.is_some() {
            self.journal.push(UndoEntry {
                block: tx.accepting_block,
                added,
                removed,
                profile: profile_prior,
            });
        }

        events
    }

    /// Undo every transaction accepted by a removed chain block (a reorg), newest first,
    /// restoring the exact prior rows (§4.1 / B6). Idempotent for an unknown block.
    pub fn undo_block(&mut self, block: &[u8; 32]) {
        let mut undo = Vec::new();
        let mut keep = Vec::new();
        for e in self.journal.drain(..) {
            if &e.block == block {
                undo.push(e);
            } else {
                keep.push(e);
            }
        }
        self.journal = keep;
        for e in undo.into_iter().rev() {
            for op in &e.added {
                self.utxos.remove(op);
            }
            for (op, tracked) in e.removed {
                self.utxos.insert(op, tracked);
            }
            if let Some((addr, prior)) = e.profile {
                match prior {
                    Some(v) => {
                        self.profiles.insert(addr, v);
                    }
                    None => {
                        self.profiles.remove(&addr);
                    }
                }
            }
        }
    }

    /// Drop undo entries beyond the most recent `keep_last` transactions (past finality, a
    /// reorg can't reach them). Called by the reader once a checkpoint is finalized.
    pub fn prune_journal(&mut self, keep_last: usize) {
        if self.journal.len() > keep_last {
            let drop = self.journal.len() - keep_last;
            self.journal.drain(0..drop);
        }
    }

    /// Number of undo entries currently retained (for the reader's pruning + tests).
    pub fn journal_len(&self) -> usize {
        self.journal.len()
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
            let mut e = self.event("register", nm.key, tx);
            e.to = Some(nm.owner);
            e.years = Some(years);
            events.push(e);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record_name(
        &mut self,
        templates: &Templates,
        tx: &Tx,
        ns: &NameState,
        used: &mut Vec<usize>,
        events: &mut Vec<Event>,
        op: &'static str,
        old: Option<&NameState>,
        price: Option<i64>,
        years: Option<i64>,
    ) {
        if let Some(i) = Self::find_output(tx, &templates.name_spk(ns), used) {
            self.utxos.insert((tx.id, i as u32), Tracked::Name(*ns));
            let mut e = self.event(op, ns.key, tx);
            if let Some(old) = old
                && old.owner != ns.owner
            {
                e.from = Some(old.owner);
                e.to = Some(ns.owner);
            }
            e.price = price;
            e.years = years;
            events.push(e);
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
                let mut e = self.event("offer", key, tx);
                e.to = Some(buyer);
                events.push(e);
                return;
            }
        }
    }

    /// §C: a profile record is `kchat:1:profile:<json>` on a self-send (an input of the
    /// address, an output back to it). The newest by accepting order wins. The sender
    /// resolution is left to the chain reader; here we record on the marker + a self output.
    #[allow(clippy::type_complexity)]
    fn apply_profile(&mut self, tx: &Tx) -> Option<(Vec<u8>, Option<(String, (u64, [u8; 32]))>)> {
        let text = std::str::from_utf8(&tx.payload).ok()?;
        let json = text.strip_prefix("kchat:1:profile:")?;
        // Validate against the 2026-10-02 format (one allowlisted social + linktr.ee +
        // primaryName, <= 2 KB); reject old/oversized/invalid records outright.
        crate::parse_profile(json)?;
        // The chain reader supplies the resolved address via the first output's spk as the
        // identity key (self-send). A fuller sender check lives in the reader.
        let addr = tx.outputs.first()?.script_public_key.clone();
        let order = (tx.accepting_daa, tx.id);
        let newer = self.profiles.get(&addr).map(|(_, o)| order > *o).unwrap_or(true);
        if !newer {
            return None;
        }
        let prior = self.profiles.insert(addr.clone(), (json.to_string(), order));
        Some((addr, prior))
    }

    fn event(&self, op: &'static str, key: [u8; 32], tx: &Tx) -> Event {
        Event { op, key, tx_id: tx.id, block: tx.accepting_block, daa: tx.accepting_daa, at: tx.block_time, from: None, to: None, price: None, years: None }
    }
}

fn decode32(s: &str) -> Result<[u8; 32], ()> {
    let v = hex::decode(s).map_err(|_| ())?;
    v.try_into().map_err(|_| ())
}

#[cfg(test)]
mod tests;
