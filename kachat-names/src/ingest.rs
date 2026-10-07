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

use crate::transition::{self, Contract, Entry};
use crate::{decode_sig_script, GapState, NameState, OfferState, PriceState};

/// A transaction outpoint: `(txid, output index)`.
pub type Outpoint = ([u8; 32], u32);

/// One tracked registry UTXO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tracked {
    Gap(GapState),
    Name(NameState),
    Offer(OfferState),
    /// Registry v3: a price shard (its own covenant).
    Shard(PriceState),
}

impl Tracked {
    pub fn contract(&self) -> Contract {
        match self {
            Tracked::Gap(_) => Contract::Gap,
            Tracked::Name(_) => Contract::Name,
            Tracked::Offer(_) => Contract::Offer,
            Tracked::Shard(_) => Contract::Price,
        }
    }
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

/// The contract templates and rules for one registry (built from the manifest).
///
/// Registry v2 manifests give the gap/name/offer templates; v3 adds the price template, the
/// two covenant ids, `periodMs` and per-contract dispatch tags (docs/KACHAT_NAMES_REGISTRY_V3.md);
/// v4 is v3 without the price record (fixed prices baked into the contracts, one covenant id,
/// docs/KACHAT_NAMES_REGISTRY_V4.md). Everything version-specific is decided from these, so one
/// applier serves all three.
#[derive(Debug, Clone)]
pub struct Templates {
    /// The manifest's `registryVersion` (2 when absent).
    pub version: u32,
    pub gap: ContractTemplate,
    pub name: ContractTemplate,
    pub offer: ContractTemplate,
    /// Registry v3: the price shard template.
    pub price: Option<ContractTemplate>,
    /// Per-contract dispatch tags from the manifest. Empty = the pinned v2 tags.
    pub tags: Vec<(Contract, [u8; 4], Entry)>,
    /// One registration period (v3 `periodMs`; a year before).
    pub period_ms: i64,
    /// Registry v3: registry outputs must carry this covenant id (and the authorizing input).
    pub registry_id: Option<[u8; 32]>,
    /// Registry v3: price shard outputs must carry this covenant id.
    pub price_id: Option<[u8; 32]>,
}

impl Templates {
    /// Registry v2 templates with the pinned tags and a one-year period (tests, old manifests).
    pub fn new(gap: ContractTemplate, name: ContractTemplate, offer: ContractTemplate) -> Self {
        Self {
            version: 2,
            gap,
            name,
            offer,
            price: None,
            tags: Vec::new(),
            period_ms: transition::YEAR_MS,
            registry_id: None,
            price_id: None,
        }
    }

    /// Build the templates from a names manifest's `artifacts` (`KachatGap`, `KachatName`,
    /// `KachatOffer`, v3 `KachatPrice`: `prefixHex`, `suffixHex`, `stateSpan {offset,len}`,
    /// `dispatchTags`), plus v3's `params.periodMs`, `registryCovenantId`, `priceCovenantId`.
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
        let mut t = Templates::new(one("KachatGap")?, one("KachatName")?, one("KachatOffer")?);
        let version = manifest
            .get("registryVersion")
            .or_else(|| manifest.get("params").and_then(|p| p.get("registryVersion")))
            .and_then(|v| v.as_u64())
            .unwrap_or(2);
        let id = |key: &str| manifest.get(key).and_then(|v| v.as_str()).and_then(|s| decode32(s).ok());
        if version > 4 {
            return None; // a registry this follower does not know: refuse, never guess
        }
        t.version = version as u32;
        if version >= 3 {
            // v3 and v4: covenant-bound registry outputs and a manifest period.
            t.registry_id = Some(id("registryCovenantId")?);
            t.period_ms = manifest.get("params")?.get("periodMs")?.as_i64()?;
        }
        if version == 3 {
            // Only v3 has the price record (v4 bakes fixed prices into the contracts).
            t.price = Some(one("KachatPrice")?);
            t.price_id = Some(id("priceCovenantId")?);
        }
        // Tags only mean something per contract (v3 reuses none of v2's register/renew/
        // extend/accept tags), so they are matched against the contract being spent.
        for (artifact, contract) in [
            ("KachatGap", Contract::Gap),
            ("KachatName", Contract::Name),
            ("KachatOffer", Contract::Offer),
            ("KachatPrice", Contract::Price),
        ] {
            let Some(tags) = manifest.get("artifacts").and_then(|a| a.get(artifact)).and_then(|a| a.get("dispatchTags"))
            else {
                continue;
            };
            for (name, tag) in tags.as_object()? {
                let entry = Entry::from_manifest_name(contract, name)?;
                let bytes: [u8; 4] = hex::decode(tag.as_str()?).ok()?.try_into().ok()?;
                t.tags.push((contract, bytes, entry));
            }
        }
        if version >= 3 && t.tags.is_empty() {
            return None; // a v3 manifest must pin its tags
        }
        Some(t)
    }

    /// Registry v3 or later: seller-bound offers, covenant-checked outputs, a manifest period.
    pub fn is_covenant_registry(&self) -> bool {
        self.version >= 3
    }

    /// Registry v3 only: the price record (shards under their own covenant).
    pub fn has_price_record(&self) -> bool {
        self.price.is_some()
    }

    /// The entry a spend of `contract` with this dispatch tag calls.
    pub fn entry(&self, contract: Contract, tag: &[u8; 4]) -> Option<Entry> {
        if self.tags.is_empty() {
            return transition::entry_for_tag(tag).filter(|e| e.contract() == contract);
        }
        self.tags.iter().find(|(c, t, _)| *c == contract && t == tag).map(|(_, _, e)| *e)
    }

    pub fn gap_spk(&self, s: &GapState) -> Vec<u8> {
        self.gap.spk(&s.encode())
    }
    pub fn name_spk(&self, s: &NameState) -> Vec<u8> {
        self.name.spk(&s.encode())
    }
    pub fn offer_spk(&self, s: &OfferState) -> Vec<u8> {
        self.offer.spk(&s.encode())
    }
    /// A shard's script (v3 only; empty otherwise, which matches no output).
    pub fn price_spk(&self, s: &PriceState) -> Vec<u8> {
        self.price.as_ref().map(|p| p.spk(&s.encode())).unwrap_or_default()
    }
    /// The script of any tracked UTXO.
    pub fn tracked_spk(&self, t: &Tracked) -> Vec<u8> {
        match t {
            Tracked::Gap(g) => self.gap_spk(g),
            Tracked::Name(n) => self.name_spk(n),
            Tracked::Offer(o) => self.offer_spk(o),
            Tracked::Shard(p) => self.price_spk(p),
        }
    }
}

/// A transaction input, as the follower needs it.
#[derive(Debug, Clone)]
pub struct TxInput {
    pub previous_outpoint: Outpoint,
    pub signature_script: Vec<u8>,
    /// The scriptPublicKey of the output this input spends (empty when unknown). A profile
    /// record is honoured only on a real self-send: an input spent from the same address.
    pub spent_script: Vec<u8>,
}

/// A transaction output.
#[derive(Debug, Clone)]
pub struct TxOutput {
    pub script_public_key: Vec<u8>,
    pub value: u64,
    /// The output's covenant binding (Toccata), when the node reports one.
    pub covenant: Option<CovenantBinding>,
}

/// Which input authorized a covenant output, under which covenant id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CovenantBinding {
    pub authorizing_input: u16,
    pub covenant_id: [u8; 32],
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

    /// Seed a registry v3 price shard from the price genesis.
    pub fn seed_shard(&mut self, outpoint: Outpoint, shard: PriceState) {
        self.utxos.insert(outpoint, Tracked::Shard(shard));
    }

    /// The live price shards, in shard order (registry v3).
    pub fn shards(&self) -> Vec<(Outpoint, PriceState)> {
        let mut v: Vec<_> = self
            .utxos
            .iter()
            .filter_map(|(o, t)| match t {
                Tracked::Shard(p) => Some((*o, *p)),
                _ => None,
            })
            .collect();
        v.sort_by_key(|(_, p)| p.shard);
        v
    }

    /// Find an unclaimed output whose scriptPublicKey is `spk`.
    ///
    /// `bind`: registry v3 covenant outputs must also be authorized by that input under that
    /// covenant id, so a look-alike output someone pays to the same script (no covenant, or
    /// another input) is never taken for the real continuation. When the node reported no
    /// covenant on any output of the transaction, the binding is unknown and only the script
    /// is matched (as in v2).
    fn find_output(tx: &Tx, spk: &[u8], used: &mut Vec<usize>, bind: Option<(usize, [u8; 32])>) -> Option<usize> {
        let bindings_known = tx.outputs.iter().any(|o| o.covenant.is_some());
        for (i, o) in tx.outputs.iter().enumerate() {
            if used.contains(&i) || o.script_public_key != spk {
                continue;
            }
            if let (Some((auth, id)), true) = (bind, bindings_known)
                && o.covenant != Some(CovenantBinding { authorizing_input: auth as u16, covenant_id: id })
            {
                continue;
            }
            used.push(i);
            return Some(i);
        }
        None
    }

    /// The binding a registry output predicted from input `auth` must carry (v3 only).
    fn reg_bind(templates: &Templates, auth: usize) -> Option<(usize, [u8; 32])> {
        templates.registry_id.map(|id| (auth, id))
    }

    /// Apply one accepted transaction. Returns the registry events it produced.
    pub fn apply(&mut self, templates: &Templates, tx: &Tx) -> Vec<Event> {
        let mut events = Vec::new();
        // Snapshot the UTXO keys so the undo journal can record exactly what this tx added.
        let before: std::collections::HashSet<Outpoint> = self.utxos.keys().copied().collect();

        // Which tracked UTXOs does this tx spend, and under which entry? A tag only means
        // something for the contract being spent.
        let mut spends: Vec<(usize, Outpoint, Tracked, Entry, crate::SigScript)> = Vec::new();
        for (idx, input) in tx.inputs.iter().enumerate() {
            let Some(tracked) = self.utxos.get(&input.previous_outpoint).cloned() else {
                continue;
            };
            let Some(sig) = decode_sig_script(&input.signature_script) else {
                continue;
            };
            let Some(entry) = templates.entry(tracked.contract(), &sig.dispatch_tag) else {
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
            self.apply_spends(templates, tx, &spends, &mut used_outputs, &mut events);
        }

        // Record the undo entry (§4.1) and consume the spent UTXOs. `added` is whatever this
        // tx inserted (new gaps/names/offers/shards); `removed` is the spent UTXOs' prior states.
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

    fn apply_spends(
        &mut self,
        templates: &Templates,
        tx: &Tx,
        spends: &[(usize, Outpoint, Tracked, Entry, crate::SigScript)],
        used: &mut Vec<usize>,
        events: &mut Vec<Event>,
    ) {
        let has = |e: Entry| spends.iter().any(|s| s.3 == e);

        // Price shards (v3): `use` continues unchanged; `update` (shard 0) rewrites every
        // shard in the tx, each continuation authorized by that shard's own input.
        self.apply_price_spends(templates, tx, spends, used, events);

        // Offer exits that carry no name: decline (v3), withdraw, refund.
        for (_, _, tracked, entry, _) in spends {
            if let Tracked::Offer(o) = tracked {
                let op = match entry {
                    Entry::OfferDecline => "offer_decline",
                    Entry::OfferWithdraw => "offer_withdraw",
                    Entry::OfferRefund => "offer_refund",
                    _ => continue,
                };
                let mut e = self.event(op, o.key, tx);
                e.to = Some(o.buyer);
                e.from = o.seller;
                events.push(e);
            }
        }

        if has(Entry::GapMerge) {
            // Exit: collapse the two gaps around the released/reclaimed name.
            let lo = spends.iter().find_map(|s| match (&s.2, s.3) {
                (Tracked::Gap(g), Entry::GapMerge) => Some((s.0, *g)),
                _ => None,
            });
            let hi_gap = spends.iter().find_map(|s| match (&s.2, s.3) {
                (Tracked::Gap(g), Entry::GapAbsorbed) => Some(*g),
                _ => None,
            });
            if let (Some((auth, lo_gap)), Some(hi_gap)) = (lo, hi_gap) {
                let merged = transition::merge_gaps(&lo_gap, &hi_gap);
                if let Some(i) = Self::find_output(tx, &templates.gap_spk(&merged), used, Self::reg_bind(templates, auth)) {
                    self.utxos.insert((tx.id, i as u32), Tracked::Gap(merged));
                }
            }
            for s in spends {
                if let Tracked::Name(n) = &s.2 {
                    let mut e = self.event(if s.3 == Entry::NameReclaim { "reclaim" } else { "release" }, n.key, tx);
                    e.from = Some(n.owner);
                    events.push(e);
                }
            }
        } else if has(Entry::GapRegister) {
            self.apply_register(templates, tx, spends, used, events);
        } else {
            // Per-name continuations (transfer / list / buy / renew / extend) + offer accept.
            for (auth, _, tracked, entry, sig) in spends {
                let auth = *auth;
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
                            self.record_name(templates, tx, auth, &ns, used, events, op, Some(old), price, None);
                        }
                    }
                    (Tracked::Name(old), Entry::NameList) => {
                        if let Some(price) = sig.args.first().and_then(|a| a.as_i64()) {
                            let ns = transition::name_list(old, price);
                            self.record_name(templates, tx, auth, &ns, used, events,
                                if price == 0 { "delist" } else { "list" }, Some(old), Some(price), None);
                        }
                    }
                    (Tracked::Name(old), Entry::NameRenew) => {
                        if let Some(years) = sig.args.first().and_then(|a| a.as_i64()) {
                            let ns = transition::name_renew_with_period(old, years, templates.period_ms);
                            self.record_name(templates, tx, auth, &ns, used, events, "renew", Some(old), None, Some(years));
                        }
                    }
                    (Tracked::Name(old), Entry::NameExtend) => {
                        if let Some(years) = sig.args.first().and_then(|a| a.as_i64()) {
                            let ns = transition::name_extend_with_period(old, years, templates.period_ms);
                            self.record_name(templates, tx, auth, &ns, used, events, "extend", Some(old), None, Some(years));
                        }
                    }
                    (Tracked::Offer(offer), Entry::OfferAccept) if !has(Entry::NameTransfer) => {
                        // v2: the accepted name goes to the buyer; verify a name output matches.
                        // (With a name transfer in the same tx, that spend records it.)
                        if let Some((_, name)) = self.name_by_key(&offer.key) {
                            let ns = transition::offer_accept(&name, offer);
                            self.record_name(templates, tx, auth, &ns, used, events, "offer_accepted", Some(&name), None, None);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn apply_price_spends(
        &mut self,
        templates: &Templates,
        tx: &Tx,
        spends: &[(usize, Outpoint, Tracked, Entry, crate::SigScript)],
        used: &mut Vec<usize>,
        events: &mut Vec<Event>,
    ) {
        let Some(price_id) = templates.price_id else { return };
        let shard_ins: Vec<(usize, PriceState)> = spends
            .iter()
            .filter_map(|s| match &s.2 {
                Tracked::Shard(p) => Some((s.0, *p)),
                _ => None,
            })
            .collect();
        for (auth, _, tracked, entry, sig) in spends {
            let Tracked::Shard(cur) = tracked else { continue };
            match entry {
                Entry::PriceUse => {
                    if let Some(i) = Self::find_output(tx, &templates.price_spk(cur), used, Some((*auth, price_id))) {
                        self.utxos.insert((tx.id, i as u32), Tracked::Shard(*cur));
                    }
                }
                Entry::PriceUpdate => {
                    // update(newAuthority, n1..n5, sig)
                    let authority: Option<[u8; 32]> = sig.args.first().and_then(|a| a.data()).and_then(|d| d.try_into().ok());
                    let prices: Option<Vec<i64>> = (1..=5).map(|k| sig.args.get(k).and_then(|a| a.as_i64())).collect();
                    let (Some(authority), Some(prices)) = (authority, prices) else { continue };
                    let prices: [i64; 5] = prices.try_into().unwrap_or([0; 5]);
                    if prices.iter().any(|p| *p < 0) {
                        continue;
                    }
                    let mut moved = 0;
                    for (j, other) in &shard_ins {
                        let next = PriceState { shard: other.shard, authority, prices };
                        if let Some(i) = Self::find_output(tx, &templates.price_spk(&next), used, Some((*j, price_id))) {
                            self.utxos.insert((tx.id, i as u32), Tracked::Shard(next));
                            moved += 1;
                        }
                    }
                    if moved > 0 {
                        let op = if prices == cur.prices { "price_authority" } else { "prices" };
                        let mut e = self.event(op, [0u8; 32], tx);
                        e.from = Some(cur.authority);
                        e.to = Some(authority);
                        events.push(e);
                    }
                }
                _ => {} // follow: its continuation is written by update
            }
        }
    }

    /// Apply only the profile rules (§C) to one accepted transaction: the profiles follower,
    /// which runs on every network whether or not a registry exists. Journaled like
    /// [`Self::apply`] so a reorg rolls it back. True when the record was accepted.
    pub fn apply_profile_only(&mut self, tx: &Tx) -> bool {
        let Some(prior) = self.apply_profile(tx) else { return false };
        self.journal.push(UndoEntry { block: tx.accepting_block, added: Vec::new(), removed: Vec::new(), profile: Some(prior) });
        true
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
        let Some((auth, _, Tracked::Gap(gap), _, sig)) =
            spends.iter().find(|s| s.3 == Entry::GapRegister).cloned()
        else {
            return;
        };
        // register(name, ownerKey, salt, now, years, namePrefix, nameSuffix[, priceIdx (v3)])
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
        let (left, right, nm) = transition::register_with_period(&gap, &name, owner, now, years, templates.period_ms);
        let bind = Self::reg_bind(templates, auth);
        for g in [left, right] {
            if let Some(i) = Self::find_output(tx, &templates.gap_spk(&g), used, bind) {
                self.utxos.insert((tx.id, i as u32), Tracked::Gap(g));
            }
        }
        if let Some(i) = Self::find_output(tx, &templates.name_spk(&nm), used, bind) {
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
        auth: usize,
        ns: &NameState,
        used: &mut Vec<usize>,
        events: &mut Vec<Event>,
        op: &'static str,
        old: Option<&NameState>,
        price: Option<i64>,
        years: Option<i64>,
    ) {
        if let Some(i) = Self::find_output(tx, &templates.name_spk(ns), used, Self::reg_bind(templates, auth)) {
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

    /// §B4: an offer-creating tx carries `kchat:1:offer:<keyHex>:<buyerHex>:<refundAfterDaa>`
    /// (v2), or `kchat:1:offer:<keyHex>:<buyerHex>:<sellerHex>:<refundAfterDaa>` (v3, 108-B
    /// state). Trust it only if `P2SH(offerState)` matches an output that carries no covenant.
    fn apply_offer_marker(&mut self, templates: &Templates, tx: &Tx, events: &mut Vec<Event>) {
        let Ok(text) = std::str::from_utf8(&tx.payload) else { return };
        let Some(rest) = text.strip_prefix("kchat:1:offer:") else { return };
        let parts: Vec<&str> = rest.split(':').collect();
        let offer = match (templates.offer.state_len, parts.as_slice()) {
            (75, [key_hex, buyer_hex, refund_dec]) => {
                let (Ok(key), Ok(buyer), Ok(refund_after)) = (decode32(key_hex), decode32(buyer_hex), refund_dec.parse::<i64>()) else {
                    return;
                };
                OfferState { key, buyer, seller: None, refund_after }
            }
            (108, [key_hex, buyer_hex, seller_hex, refund_dec]) => {
                let (Ok(key), Ok(buyer), Ok(seller), Ok(refund_after)) =
                    (decode32(key_hex), decode32(buyer_hex), decode32(seller_hex), refund_dec.parse::<i64>())
                else {
                    return;
                };
                OfferState { key, buyer, seller: Some(seller), refund_after }
            }
            _ => return,
        };
        if offer.refund_after < 0 {
            return;
        }
        let spk = templates.offer_spk(&offer);
        for (i, o) in tx.outputs.iter().enumerate() {
            if o.script_public_key == spk && o.covenant.is_none() {
                self.utxos.insert((tx.id, i as u32), Tracked::Offer(offer));
                let mut e = self.event("offer", offer.key, tx);
                e.to = Some(offer.buyer);
                e.from = offer.seller;
                e.price = Some(o.value as i64);
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
        // A self-send: the record's address is its first output, and the tx must spend from
        // that same address. Otherwise anyone could pay you dust with a profile payload and
        // set yours. An input whose spent script is unknown proves nothing.
        let addr = tx.outputs.first()?.script_public_key.clone();
        if !tx.inputs.iter().any(|i| !i.spent_script.is_empty() && i.spent_script == addr) {
            return None;
        }
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
