//! Registry transitions (KACHAT_NAMES_INDEXER.md §B3) — pure functions that derive the
//! new covenant state(s) a spend produces, from the old state + the entry's arguments.
//!
//! The follower pairs each with a **P2SH verify** against the actual output
//! (`p2sh(prefix ‖ computed ‖ suffix) == output.spk`): a wrong computation simply fails
//! to verify and is skipped, so these can never write a bad state — they only decide what
//! the follower looks for. `YEAR` is 365 days.

use crate::{GapState, NameState, OfferState};

pub const YEAR_MS: i64 = 31_536_000_000;

/// Which registry contract a tracked UTXO is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contract {
    Gap,
    Name,
    Offer,
}

/// A contract entry, identified by its 4-byte dispatch tag (§B3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    GapRegister,
    GapMerge,
    GapAbsorbed,
    NameTransfer,
    NameList,
    NameBuy,
    NameRenew,
    /// Registry v2: add years to the current paid period (capped at 2 years ahead).
    NameExtend,
    NameRelease,
    NameReclaim,
    OfferAccept,
    OfferWithdraw,
    OfferRefund,
}

impl Entry {
    pub fn contract(self) -> Contract {
        use Entry::*;
        match self {
            GapRegister | GapMerge | GapAbsorbed => Contract::Gap,
            NameTransfer | NameList | NameBuy | NameRenew | NameExtend | NameRelease | NameReclaim => Contract::Name,
            OfferAccept | OfferWithdraw | OfferRefund => Contract::Offer,
        }
    }

    /// Entries that remove the UTXO without a same-contract continuation.
    pub fn is_exit(self) -> bool {
        matches!(
            self,
            Entry::GapMerge
                | Entry::GapAbsorbed
                | Entry::NameRelease
                | Entry::NameReclaim
                | Entry::OfferAccept
                | Entry::OfferWithdraw
                | Entry::OfferRefund
        )
    }
}

/// Map a dispatch tag to its entry. Tags are pinned in the manifest + spec (§B3).
pub fn entry_for_tag(tag: &[u8; 4]) -> Option<Entry> {
    Some(match tag {
        [0x86, 0x67, 0xaf, 0x5e] => Entry::GapRegister,
        [0x63, 0xd2, 0x5b, 0xc2] => Entry::GapMerge,
        [0xda, 0xb7, 0x63, 0x55] => Entry::GapAbsorbed,
        [0x79, 0x4d, 0xca, 0x54] => Entry::NameTransfer,
        [0x67, 0x4a, 0x8e, 0xa4] => Entry::NameList,
        [0x76, 0xa0, 0x2e, 0xb9] => Entry::NameBuy,
        [0xb7, 0x06, 0xac, 0x38] => Entry::NameRenew,
        [0x2c, 0xe7, 0xcc, 0xeb] => Entry::NameExtend,
        [0x38, 0x8a, 0xd0, 0xb4] => Entry::NameRelease,
        [0xf5, 0x6a, 0xf4, 0xdf] => Entry::NameReclaim,
        [0x9d, 0x40, 0x43, 0xb4] => Entry::OfferAccept,
        [0x80, 0x34, 0x4f, 0xf1] => Entry::OfferWithdraw,
        [0x77, 0x7f, 0x5b, 0x11] => Entry::OfferRefund,
        _ => None?,
    })
}

// --------------------------------------------------------------- transitions --

/// gap `register(name, ownerKey, …, now, years)` on gap `(lo, hi)` produces, in output
/// order 0/1/2: gap `(lo, key)`, gap `(key, hi)`, and the new name
/// `(key, pad(name), ownerKey, price=0, periodStart = now, expiresAt = now + years·YEAR)`.
pub fn register(
    gap: &GapState,
    name: &[u8],
    owner_key: [u8; 32],
    now_ms: i64,
    years: i64,
) -> (GapState, GapState, NameState) {
    let key = crate::name_key(name);
    let left = GapState { lo: gap.lo, hi: key };
    let right = GapState { lo: key, hi: gap.hi };
    let nm = NameState {
        key,
        name: crate::pad_name(name),
        owner: owner_key,
        price: 0,
        period_start: now_ms,
        expires_at: now_ms + years * YEAR_MS,
    };
    (left, right, nm)
}

/// name `transfer(newOwner)`: new owner, price 0, expiry unchanged.
pub fn name_transfer(name: &NameState, new_owner: [u8; 32]) -> NameState {
    NameState { owner: new_owner, price: 0, ..*name }
}

/// name `list(price)`: set the price (0 delists).
pub fn name_list(name: &NameState, price: i64) -> NameState {
    NameState { price, ..*name }
}

/// name `buy(newOwner)`: new owner, price 0 (the output continuation+1 pays the seller).
pub fn name_buy(name: &NameState, new_owner: [u8; 32]) -> NameState {
    NameState { owner: new_owner, price: 0, ..*name }
}

/// name `renew(years)` (v2): a new paid period starting at the OLD expiry (even in grace):
/// `periodStart = old expiresAt`, `expiresAt = old expiresAt + years·YEAR`.
pub fn name_renew(name: &NameState, years: i64) -> NameState {
    NameState { period_start: name.expires_at, expires_at: name.expires_at + years * YEAR_MS, ..*name }
}

/// name `extend(years)` (v2): more years on the current period; `periodStart` unchanged.
pub fn name_extend(name: &NameState, years: i64) -> NameState {
    NameState { expires_at: name.expires_at + years * YEAR_MS, ..*name }
}

/// offer `accept` moves the name to the offer's buyer (price 0); the offer is gone.
pub fn offer_accept(name: &NameState, offer: &OfferState) -> NameState {
    NameState { owner: offer.buyer, price: 0, ..*name }
}

/// The exit: gap `merge(input0)` + name `release|reclaim(input1)` + gap `absorbed(input2)`
/// collapse to one gap `(lo of input0, hi of input2)`; the name is removed.
pub fn merge_gaps(lo_gap: &GapState, hi_gap: &GapState) -> GapState {
    GapState { lo: lo_gap.lo, hi: hi_gap.hi }
}

// --------------------------------------------------------------- name status --

/// Name status (§B5). `now` and `expires_at` are unix ms; `grace_ms` from the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameStatus {
    Active,
    Grace,
    Lapsed,
}

pub fn name_status(expires_at: i64, grace_ms: i64, now_ms: i64) -> NameStatus {
    if now_ms < expires_at {
        NameStatus::Active
    } else if now_ms < expires_at + grace_ms {
        NameStatus::Grace
    } else {
        NameStatus::Lapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_round_trip_every_entry() {
        // The twelve pinned dispatch tags map to their entries (and nothing else does).
        assert_eq!(entry_for_tag(&[0x86, 0x67, 0xaf, 0x5e]), Some(Entry::GapRegister));
        assert_eq!(entry_for_tag(&[0x79, 0x4d, 0xca, 0x54]), Some(Entry::NameTransfer));
        assert_eq!(entry_for_tag(&[0x9d, 0x40, 0x43, 0xb4]), Some(Entry::OfferAccept));
        assert_eq!(entry_for_tag(&[0x2c, 0xe7, 0xcc, 0xeb]), Some(Entry::NameExtend));
        assert_eq!(Entry::NameExtend.contract(), Contract::Name);
        assert_eq!(entry_for_tag(&[0, 0, 0, 0]), None);
        assert_eq!(Entry::NameTransfer.contract(), Contract::Name);
        assert!(Entry::OfferWithdraw.is_exit());
        assert!(!Entry::NameList.is_exit());
    }

    #[test]
    fn register_splits_the_gap_and_mints_the_name() {
        let gap = GapState { lo: [0u8; 32], hi: [0xff; 32] };
        let owner = [7u8; 32];
        let now = 1_790_000_000_000;
        let (l, r, nm) = register(&gap, b"alice", owner, now, 2);
        let key = crate::name_key(b"alice");
        assert_eq!(l, GapState { lo: [0u8; 32], hi: key });
        assert_eq!(r, GapState { lo: key, hi: [0xff; 32] });
        assert_eq!(nm.key, key);
        assert_eq!(nm.owner, owner);
        assert_eq!(nm.price, 0);
        assert_eq!(nm.period_start, now);
        assert_eq!(nm.expires_at, now + 2 * YEAR_MS);
        assert_eq!(nm.name_str(), "alice");
    }

    #[test]
    fn name_entries() {
        let nm = NameState {
            key: crate::name_key(b"alice"),
            name: crate::pad_name(b"alice"),
            owner: [1u8; 32],
            price: 0,
            period_start: 500,
            expires_at: 1_000,
        };
        assert_eq!(name_list(&nm, 500).price, 500);
        assert_eq!(name_transfer(&nm, [2u8; 32]).owner, [2u8; 32]);
        assert_eq!(name_transfer(&name_list(&nm, 500), [2u8; 32]).price, 0, "transfer delists");
        assert_eq!(name_buy(&name_list(&nm, 500), [3u8; 32]).owner, [3u8; 32]);
        // v2 renew starts a new period at the old expiry; extend keeps the period.
        assert_eq!(name_renew(&nm, 2).period_start, 1_000);
        assert_eq!(name_renew(&nm, 2).expires_at, 1_000 + 2 * YEAR_MS);
        assert_eq!(name_extend(&nm, 1).period_start, 500);
        assert_eq!(name_extend(&nm, 1).expires_at, 1_000 + YEAR_MS);
        // period + expiry unchanged on transfer/list/buy
        assert_eq!(name_transfer(&nm, [2u8; 32]).expires_at, 1_000);
        assert_eq!(name_buy(&nm, [3u8; 32]).period_start, 500);
    }

    #[test]
    fn exit_merges_the_two_gaps() {
        let lo_gap = GapState { lo: [1u8; 32], hi: [5u8; 32] };
        let hi_gap = GapState { lo: [5u8; 32], hi: [9u8; 32] };
        assert_eq!(merge_gaps(&lo_gap, &hi_gap), GapState { lo: [1u8; 32], hi: [9u8; 32] });
    }

    #[test]
    fn status_from_expiry() {
        let grace = 864_000_000; // 10 days
        assert_eq!(name_status(1000, grace, 500), NameStatus::Active);
        assert_eq!(name_status(1000, grace, 1000 + 1), NameStatus::Grace);
        assert_eq!(name_status(1000, grace, 1000 + grace + 1), NameStatus::Lapsed);
    }
}
