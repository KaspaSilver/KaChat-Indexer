//! KaPosts tips (KAPOSTS_INDEXER.md §5.11).
//!
//! `kchat:1:tip:<pubkey>:<signature>:<post_id>:<author_pubkey>:<sealed_hex>` is a payment to a
//! post's author that names the post. The payload says WHICH post; the transaction's outputs say
//! HOW MUCH: the tip is the sum of the outputs that pay the author's p2pk address (the x-only key
//! of `<author_pubkey>`, script `0x20 || X || 0xac`, script version 0).
//!
//! Output amounts are not in the ingest database (run-ingest.sh excludes `tx_out_amount`), so the
//! outputs are read from the node: `getBlock(<the tx's block>, includeTransactions)` over the same
//! wRPC connection the public-chat sender check uses (`bcast_sender`). The block that holds the
//! tx is known immediately, so this does not wait for acceptance (posts and votes are indexed on
//! inclusion the same way).
//!
//! Everything here except `fetch_tx_outputs` is pure and unit-tested.

use anyhow::{Result, anyhow};
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_rpc_core::model::RpcHash;
use serde::{Deserialize, Serialize};

/// A parsed tip action. All fields are the payload's text verbatim.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct KTip {
    /// The tipper's 66-hex compressed key.
    pub sender_pubkey: String,
    pub sender_signature: String,
    /// The tipped content's txid (a post or a reply).
    pub post_id: String,
    /// The tipped content's author (as the app read it from the post object: 66-hex compressed).
    pub author_pubkey: String,
    /// Lowercase hex of the sealed payment record (the same record a `kchat:1:pay:` carries).
    pub sealed_hex: String,
}

impl KTip {
    /// The signed string: `tip:<post_id>:<author_pubkey>:<sealed_hex>` (literal `tip:` prefix).
    pub fn signing_string(&self) -> String {
        format!("tip:{}:{}:{}", self.post_id, self.author_pubkey, self.sealed_hex)
    }
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_lowercase_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Parse the `:`-split fields of a tip payload after the `kchat:1:` prefix
/// (`["tip", pubkey, signature, post_id, author_pubkey, sealed_hex]`).
///
/// Exactly six fields (hex never contains `:`). `post_id` is a 64-hex txid, `author_pubkey` a
/// 66-hex compressed (or 64-hex x-only) key, and `sealed_hex` non-empty, even-length, LOWERCASE
/// hex. The tipper's key and signature are checked by the signature verification.
pub fn parse_tip_parts(parts: &[&str]) -> Result<KTip> {
    if parts.first() != Some(&"tip") {
        return Err(anyhow!("not a tip payload"));
    }
    if parts.len() != 6 {
        return Err(anyhow!("Invalid tip format: expected 6 parts, got {}", parts.len()));
    }
    let (pubkey, signature, post_id, author, sealed) = (parts[1], parts[2], parts[3], parts[4], parts[5]);
    if pubkey.is_empty() || signature.is_empty() {
        return Err(anyhow!("tip: missing pubkey or signature"));
    }
    if post_id.len() != 64 || !is_hex(post_id) {
        return Err(anyhow!("tip: post_id is not a 64-hex txid"));
    }
    if !(author.len() == 66 || author.len() == 64) || !is_hex(author) {
        return Err(anyhow!("tip: author_pubkey is not a 66-hex (or 64-hex) key"));
    }
    if !is_lowercase_hex(sealed) || sealed.len() % 2 != 0 {
        return Err(anyhow!("tip: sealed part is not non-empty lowercase hex"));
    }
    Ok(KTip {
        sender_pubkey: pubkey.to_string(),
        sender_signature: signature.to_string(),
        post_id: post_id.to_string(),
        author_pubkey: author.to_string(),
        sealed_hex: sealed.to_string(),
    })
}

/// The x-only key (32 bytes) of a 66-hex compressed or 64-hex x-only pubkey.
pub fn xonly_bytes(pubkey_hex: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(pubkey_hex.trim()).ok()?;
    let x = match bytes.len() {
        33 => &bytes[1..],
        32 => &bytes[..],
        _ => return None,
    };
    x.try_into().ok()
}

/// The p2pk (Schnorr, version 0) script of an x-only key: `OP_DATA_32 <X> OP_CHECKSIG`.
pub fn p2pk_script(xonly: &[u8; 32]) -> [u8; 34] {
    let mut script = [0u8; 34];
    script[0] = 0x20;
    script[1..33].copy_from_slice(xonly);
    script[33] = 0xac;
    script
}

/// One transaction output, as needed for the tip check: (amount in sompi, script version, script).
pub type TxOutput = (u64, u16, Vec<u8>);

/// What the outputs pay the p2pk address of `xonly`: the sum of every version-0 output whose
/// script is exactly that p2pk script. Saturating (a real tx can never overflow u64 anyway).
pub fn amount_paid_to(outputs: &[TxOutput], xonly: &[u8; 32]) -> u64 {
    let script = p2pk_script(xonly);
    outputs
        .iter()
        .filter(|(_, version, spk)| *version == 0 && spk.as_slice() == script.as_slice())
        .fold(0u64, |sum, (amount, _, _)| sum.saturating_add(*amount))
}

/// Sompi as KAS for a notification body, trailing zeros trimmed: 150000000 -> "1.5",
/// 100000000 -> "1", 1 -> "0.00000001".
pub fn format_kas(sompi: u64) -> String {
    const SOMPI_PER_KAS: u64 = 100_000_000;
    let whole = sompi / SOMPI_PER_KAS;
    let frac = sompi % SOMPI_PER_KAS;
    if frac == 0 {
        return whole.to_string();
    }
    let frac = format!("{frac:08}");
    format!("{whole}.{}", frac.trim_end_matches('0'))
}

/// The outputs of `tx_id` as the node has it in `block_hash` (the block that holds it, from the
/// ingest's `transactions` row). `Ok(None)`: the node has the block but not this tx in it.
/// `Err`: no node configured/connected or the call failed (callers may retry). The tx is matched
/// by its id, or by its payload when the node left the verbose data out.
pub async fn fetch_tx_outputs(tx_id: &[u8], block_hash: &[u8], payload: &[u8]) -> Result<Option<Vec<TxOutput>>> {
    let node = crate::bcast_sender::node().ok_or_else(|| anyhow!("no node configured"))?;
    if !node.is_connected() {
        return Err(anyhow!("node not connected"));
    }
    let hash: [u8; 32] = block_hash.try_into().map_err(|_| anyhow!("block hash is not 32 bytes"))?;
    let block = node
        .get_block(RpcHash::from_bytes(hash), true)
        .await
        .map_err(|e| anyhow!("getBlock: {e}"))?;
    let found = block.transactions.iter().find(|tx| {
        tx.verbose_data
            .as_ref()
            .map(|v| v.transaction_id.as_bytes().as_slice() == tx_id)
            .unwrap_or_else(|| tx.payload.as_slice() == payload)
    });
    Ok(found.map(|tx| {
        tx.outputs
            .iter()
            .map(|o| (o.value, o.script_public_key.version(), o.script_public_key.script().to_vec()))
            .collect()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIPPER: &str = "03aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const AUTHOR: &str = "02bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const POST: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const SIG: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    fn parts(payload: &str) -> Vec<&str> {
        payload.strip_prefix("kchat:1:").unwrap().split(':').collect()
    }

    #[test]
    fn parses_a_tip_payload_verbatim() {
        let p = format!("kchat:1:tip:{TIPPER}:{SIG}:{POST}:{AUTHOR}:0a1b2c");
        let tip = parse_tip_parts(&parts(&p)).expect("valid tip");
        assert_eq!(tip.sender_pubkey, TIPPER);
        assert_eq!(tip.sender_signature, SIG);
        assert_eq!(tip.post_id, POST);
        assert_eq!(tip.author_pubkey, AUTHOR);
        assert_eq!(tip.sealed_hex, "0a1b2c");
        assert_eq!(tip.signing_string(), format!("tip:{POST}:{AUTHOR}:0a1b2c"));
    }

    #[test]
    fn rejects_bad_sealed_part() {
        for sealed in ["", "0A1B", "abc", "zz", "0a1b:00"] {
            let p = format!("kchat:1:tip:{TIPPER}:{SIG}:{POST}:{AUTHOR}:{sealed}");
            assert!(parse_tip_parts(&parts(&p)).is_err(), "sealed {sealed:?} must be rejected");
        }
    }

    #[test]
    fn rejects_wrong_shape() {
        // Missing the sealed part.
        let p = format!("kchat:1:tip:{TIPPER}:{SIG}:{POST}:{AUTHOR}");
        assert!(parse_tip_parts(&parts(&p)).is_err());
        // Post id not a txid.
        let p = format!("kchat:1:tip:{TIPPER}:{SIG}:abcd:{AUTHOR}:00");
        assert!(parse_tip_parts(&parts(&p)).is_err());
        // Author not a key.
        let p = format!("kchat:1:tip:{TIPPER}:{SIG}:{POST}:abcd:00");
        assert!(parse_tip_parts(&parts(&p)).is_err());
        // Not a tip at all.
        assert!(parse_tip_parts(&["pay", "00"]).is_err());
    }

    #[test]
    fn xonly_drops_the_parity_byte() {
        let x = xonly_bytes(AUTHOR).unwrap();
        assert_eq!(x, [0xbb; 32]);
        assert_eq!(xonly_bytes(&AUTHOR[2..]).unwrap(), [0xbb; 32]);
        assert!(xonly_bytes("02bb").is_none());
        assert!(xonly_bytes("not hex").is_none());
    }

    #[test]
    fn p2pk_script_shape() {
        let s = p2pk_script(&[0x11; 32]);
        assert_eq!(s.len(), 34);
        assert_eq!(s[0], 0x20);
        assert_eq!(&s[1..33], &[0x11; 32]);
        assert_eq!(s[33], 0xac);
    }

    #[test]
    fn amount_sums_only_outputs_paying_the_author() {
        let author = xonly_bytes(AUTHOR).unwrap();
        let tipper = xonly_bytes(TIPPER).unwrap();
        let outputs: Vec<TxOutput> = vec![
            (150_000_000, 0, p2pk_script(&author).to_vec()), // the tip
            (987_654_321, 0, p2pk_script(&tipper).to_vec()), // change back to the tipper
            (50_000_000, 0, p2pk_script(&author).to_vec()),  // a second output to the author
            (77, 1, p2pk_script(&author).to_vec()),          // wrong script version
            (88, 0, {
                // ECDSA p2pk (33-byte key) for the same x: not the Schnorr address
                let mut s = vec![0x21, 0x02];
                s.extend_from_slice(&author);
                s.push(0xab);
                s
            }),
        ];
        assert_eq!(amount_paid_to(&outputs, &author), 200_000_000);
        assert_eq!(amount_paid_to(&outputs, &tipper), 987_654_321);
        assert_eq!(amount_paid_to(&outputs, &[0x99; 32]), 0);
        assert_eq!(amount_paid_to(&[], &author), 0);
    }

    #[test]
    fn kas_formatting_trims_trailing_zeros() {
        assert_eq!(format_kas(150_000_000), "1.5");
        assert_eq!(format_kas(100_000_000), "1");
        assert_eq!(format_kas(1), "0.00000001");
        assert_eq!(format_kas(1_234_500_000), "12.345");
        assert_eq!(format_kas(0), "0");
    }
}
