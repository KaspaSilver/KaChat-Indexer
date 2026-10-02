//! Pure codec + state model for the `.kachat` names registry (KACHAT_NAMES_INDEXER.md).
//!
//! This is the byte-level half the indexer's covenant follower stands on, and it is a
//! faithful port of the `kachat-domains` harness (the reference the CLI's
//! `kachat-names-vectors` validates). It has no Kaspa dependency:
//!   - name key is `blake3(name)` (§B1),
//!   - P2SH is plain BLAKE2b-256 of the redeem script, script `aa 20 <hash> 87`
//!     (matching rusty-kaspa's `pay_to_script_hash_script`),
//!   - state integers are `num8` (fixed 8-byte little-endian sign-magnitude),
//!   - signature-script args are **minimal** pushes / script numbers (§B3).
//!
//! It deliberately stops at pure functions: following spends, the DB and the REST API
//! are built on top, in the indexer.

// ---------------------------------------------------------------------------
// Hashes + keys
// ---------------------------------------------------------------------------

/// §B1: `key = blake3(name)` over the ASCII bytes of the name.
pub fn name_key(name: &[u8]) -> [u8; 32] {
    *blake3::hash(name).as_bytes()
}

/// A name zero-padded to 32 bytes (the `name` field of a name state).
pub fn pad_name(name: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = name.len().min(32);
    out[..n].copy_from_slice(&name[..n]);
    out
}

/// BLAKE2b with a 32-byte digest, no key — exactly rusty-kaspa's script hash.
pub fn blake2b_256(data: &[u8]) -> [u8; 32] {
    let hash = blake2b_simd::Params::new().hash_length(32).to_state().update(data).finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(hash.as_bytes());
    out
}

/// The P2SH `scriptPublicKey` for a redeem script: `OP_BLAKE2B <32: hash> OP_EQUAL`.
pub fn p2sh_script(redeem: &[u8]) -> Vec<u8> {
    let h = blake2b_256(redeem);
    let mut s = Vec::with_capacity(35);
    s.push(0xaa); // OP_BLAKE2B
    s.push(0x20); // push 32 bytes
    s.extend_from_slice(&h);
    s.push(0x87); // OP_EQUAL
    s
}

pub const COMMIT_DOMAIN: &[u8] = b"kachat-commit:v1";

/// `blake3("kachat-commit:v1" || name || ownerKey || salt)`.
pub fn commitment(name: &[u8], owner: &[u8; 32], salt: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(COMMIT_DOMAIN);
    h.update(name);
    h.update(owner);
    h.update(salt);
    *h.finalize().as_bytes()
}

/// The fixed commit redeem script `0x20 <c> OP_DROP 0x20 <ownerKey> OP_CHECKSIG`.
pub fn commit_redeem(c: &[u8; 32], owner: &[u8; 32]) -> Vec<u8> {
    [&[0x20u8][..], c, &[0x75, 0x20], owner, &[0xac]].concat()
}

// ---------------------------------------------------------------------------
// num8 — the fixed 8-byte sign-magnitude LE used INSIDE states (OpNum2Bin 8)
// ---------------------------------------------------------------------------

/// Encode an integer as the state `num8` form. All registry values are non-negative.
pub fn num8_encode(v: i64) -> [u8; 8] {
    debug_assert!(v != i64::MIN);
    let mut out = v.unsigned_abs().to_le_bytes();
    if v < 0 {
        out[7] |= 0x80;
    }
    out
}

/// Decode a state `num8` back to an integer.
pub fn num8_decode(b: &[u8; 8]) -> i64 {
    let mut bytes = *b;
    let negative = bytes[7] & 0x80 != 0;
    bytes[7] &= 0x7f;
    let mag = u64::from_le_bytes(bytes) as i64;
    if negative {
        -mag
    } else {
        mag
    }
}

// ---------------------------------------------------------------------------
// Minimal pushes + script numbers (the signature-script arg encoding, §B3)
// ---------------------------------------------------------------------------

/// One token of a signature script: a number op or a data push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Push {
    /// A number op: OP_0, OP_1..OP_16, OP_1NEGATE.
    Num(i64),
    /// A data push (`OP_DATA_n` / `OP_PUSHDATA{1,2,4}`).
    Data(Vec<u8>),
}

impl Push {
    /// The bytes of a data push, or `None` for a number op.
    pub fn data(&self) -> Option<&[u8]> {
        match self {
            Push::Data(d) => Some(d),
            Push::Num(_) => None,
        }
    }

    /// Interpret the token as an integer argument: a number op directly, or a data push
    /// decoded as a minimal script number (how ints > 16 travel).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Push::Num(n) => Some(*n),
            Push::Data(d) => Some(decode_script_number(d)),
        }
    }
}

/// Decode a minimal little-endian sign-magnitude **script number** (Bitcoin/Kaspa form:
/// the sign is the top bit of the last byte). Empty = 0.
pub fn decode_script_number(b: &[u8]) -> i64 {
    if b.is_empty() {
        return 0;
    }
    let mut result: i64 = 0;
    for (i, &byte) in b.iter().enumerate() {
        result |= (byte as i64) << (8 * i);
    }
    let top = b.len() - 1;
    if b[top] & 0x80 != 0 {
        // Clear the sign bit of the most-significant byte, then negate.
        let mask = 0x80i64 << (8 * top);
        -(result & !mask)
    } else {
        result
    }
}

/// Parse a script that is **only** pushes (a KachatContract signature script:
/// `<args...> <dispatch tag> <push(redeem)>`). Returns the tokens in order, or `None`
/// if it contains a non-push opcode or is truncated.
pub fn parse_pushes(script: &[u8]) -> Option<Vec<Push>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < script.len() {
        let op = script[i];
        i += 1;
        match op {
            0x00 => out.push(Push::Num(0)), // OP_0 / OP_FALSE
            0x4f => out.push(Push::Num(-1)), // OP_1NEGATE
            0x51..=0x60 => out.push(Push::Num((op - 0x50) as i64)), // OP_1..OP_16
            0x01..=0x4b => {
                let n = op as usize;
                let data = script.get(i..i + n)?;
                out.push(Push::Data(data.to_vec()));
                i += n;
            }
            0x4c => {
                let n = *script.get(i)? as usize;
                i += 1;
                let data = script.get(i..i + n)?;
                out.push(Push::Data(data.to_vec()));
                i += n;
            }
            0x4d => {
                let n = u16::from_le_bytes([*script.get(i)?, *script.get(i + 1)?]) as usize;
                i += 2;
                let data = script.get(i..i + n)?;
                out.push(Push::Data(data.to_vec()));
                i += n;
            }
            0x4e => {
                let n = u32::from_le_bytes([
                    *script.get(i)?,
                    *script.get(i + 1)?,
                    *script.get(i + 2)?,
                    *script.get(i + 3)?,
                ]) as usize;
                i += 4;
                let data = script.get(i..i + n)?;
                out.push(Push::Data(data.to_vec()));
                i += n;
            }
            _ => return None, // not a pure-push script
        }
    }
    Some(out)
}

/// The decoded shape of a contract signature script (§B3): the entry's argument pushes,
/// its 4-byte dispatch tag, and the redeem script (`prefix ‖ state ‖ suffix`).
#[derive(Debug, Clone)]
pub struct SigScript {
    pub args: Vec<Push>,
    pub dispatch_tag: [u8; 4],
    pub redeem: Vec<u8>,
}

/// Split a signature script into args, dispatch tag and redeem (the last two pushes are
/// the tag and the redeem). `None` if it isn't a push-only script with at least those two.
pub fn decode_sig_script(script: &[u8]) -> Option<SigScript> {
    let mut pushes = parse_pushes(script)?;
    let redeem = match pushes.pop()? {
        Push::Data(d) => d,
        Push::Num(_) => return None,
    };
    let tag = match pushes.pop()? {
        Push::Data(d) if d.len() == 4 => d,
        _ => return None,
    };
    Some(SigScript {
        args: pushes,
        dispatch_tag: [tag[0], tag[1], tag[2], tag[3]],
        redeem,
    })
}

// ---------------------------------------------------------------------------
// Registry states (decoded from the redeem script's state span)
// ---------------------------------------------------------------------------

/// `0x20 lo[32] 0x20 hi[32]` (66 B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GapState {
    pub lo: [u8; 32],
    pub hi: [u8; 32],
}

/// `0x20 key[32] 0x20 name[32] 0x20 owner[32] 0x08 price[8] 0x08 expiresAt[8]` (117 B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameState {
    pub key: [u8; 32],
    /// Zero-padded name (trim trailing zeros for the string).
    pub name: [u8; 32],
    pub owner: [u8; 32],
    pub price: i64,
    pub expires_at: i64,
}

/// `0x20 key[32] 0x20 buyer[32] 0x08 refundAfter[8]` (75 B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfferState {
    pub key: [u8; 32],
    pub buyer: [u8; 32],
    pub refund_after: i64,
}

fn take32(b: &[u8], at: usize) -> Option<[u8; 32]> {
    b.get(at..at + 32)?.try_into().ok()
}
fn take8(b: &[u8], at: usize) -> Option<[u8; 8]> {
    b.get(at..at + 8)?.try_into().ok()
}

impl GapState {
    pub fn encode(&self) -> Vec<u8> {
        [&[0x20u8][..], &self.lo, &[0x20], &self.hi].concat()
    }
    pub fn decode(state: &[u8]) -> Option<Self> {
        if state.len() != 66 || state[0] != 0x20 || state[33] != 0x20 {
            return None;
        }
        Some(Self { lo: take32(state, 1)?, hi: take32(state, 34)? })
    }
}

impl NameState {
    pub fn encode(&self) -> Vec<u8> {
        [
            &[0x20u8][..], &self.key, &[0x20], &self.name, &[0x20], &self.owner,
            &[0x08], &num8_encode(self.price), &[0x08], &num8_encode(self.expires_at),
        ]
        .concat()
    }
    pub fn decode(state: &[u8]) -> Option<Self> {
        if state.len() != 117
            || state[0] != 0x20
            || state[33] != 0x20
            || state[66] != 0x20
            || state[99] != 0x08
            || state[108] != 0x08
        {
            return None;
        }
        Some(Self {
            key: take32(state, 1)?,
            name: take32(state, 34)?,
            owner: take32(state, 67)?,
            price: num8_decode(&take8(state, 100)?),
            expires_at: num8_decode(&take8(state, 109)?),
        })
    }
    /// The name as a UTF-8 string (trailing zero padding removed).
    pub fn name_str(&self) -> String {
        let end = self.name.iter().position(|&c| c == 0).unwrap_or(self.name.len());
        String::from_utf8_lossy(&self.name[..end]).into_owned()
    }
}

impl OfferState {
    pub fn encode(&self) -> Vec<u8> {
        [&[0x20u8][..], &self.key, &[0x20], &self.buyer, &[0x08], &num8_encode(self.refund_after)].concat()
    }
    pub fn decode(state: &[u8]) -> Option<Self> {
        if state.len() != 75 || state[0] != 0x20 || state[33] != 0x20 || state[66] != 0x08 {
            return None;
        }
        Some(Self {
            key: take32(state, 1)?,
            buyer: take32(state, 34)?,
            refund_after: num8_decode(&take8(state, 67)?),
        })
    }
}

/// Read a contract's state out of its redeem script, given the manifest's `stateSpan`.
pub fn state_span<'a>(redeem: &'a [u8], offset: usize, len: usize) -> Option<&'a [u8]> {
    redeem.get(offset..offset + len)
}

/// Name charset/length rules (§B1): `a-z 0-9 -`, 1..32, no hyphen at either end.
pub fn is_valid_name(name: &[u8]) -> bool {
    if name.is_empty() || name.len() > 32 {
        return false;
    }
    if name[0] == b'-' || name[name.len() - 1] == b'-' {
        return false;
    }
    name.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

pub mod transition;
pub use transition::{Contract, Entry, NameStatus, entry_for_tag, name_status, YEAR_MS};

pub mod ingest;
pub mod follower;
pub mod profile;
pub use profile::{parse_profile, Profile};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod vector_replay;
