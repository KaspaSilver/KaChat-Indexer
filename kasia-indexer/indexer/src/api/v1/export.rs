// KaChat Indexer fork: raw export / import of chat message data.
//
// Dumps the message partitions (handshakes, contextual messages, payments, self-stash) as a
// single file — one line per record: `<partition> <key_hex> <value_hex>`. Import restores the
// exact bytes into the same partitions, so any indexer instance can rebuild another's chat
// history from one file, no re-parsing or per-address paging. Block/acceptance/processing
// partitions are intentionally NOT exported (they are node-sync state, not content).

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use fjall::{PartitionCreateOptions, TxKeyspace};

/// Message-content partitions that carry chat history (verbatim names from indexer-db).
pub const EXPORT_PARTITIONS: &[&str] = &[
    "handshake_by_sender",
    "handshake_by_receiver",
    "tx-id-to-handshake",
    "contextual_message_by_sender",
    "tx-id-to-contextual-message",
    "payment_by_sender",
    "payment_by_receiver",
    "tx_id_to_payment",
    "self_stash_by_owner",
    "tx-id-to-self-stash",
    // Group chat partitions (added with the group+push branch).
    "group_message_by_blinded_group_id",
    "tx-id-to-group-message",
    "group_sender_binding",
    "group_control_by_sender",
    "group_control_by_recipient",
    "tx-id-to-group-control",
];

#[derive(Clone)]
pub struct ExportApi {
    tx_keyspace: TxKeyspace,
}

impl ExportApi {
    pub fn new(tx_keyspace: TxKeyspace) -> Self {
        Self { tx_keyspace }
    }
}

/// GET /export — dump every message-content partition as `<partition> <key_hex> <value_hex>`.
pub async fn export_all(State(state): State<ExportApi>) -> impl IntoResponse {
    let built = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let mut out: Vec<u8> = Vec::new();
        let rtx = state.tx_keyspace.read_tx();
        for name in EXPORT_PARTITIONS {
            let part = state
                .tx_keyspace
                .open_partition(name, PartitionCreateOptions::default())?;
            for kv in rtx.iter(&part) {
                let (k, v) = kv?;
                out.extend_from_slice(name.as_bytes());
                out.push(b' ');
                out.extend_from_slice(faster_hex::hex_string(&k).as_bytes());
                out.push(b' ');
                out.extend_from_slice(faster_hex::hex_string(&v).as_bytes());
                out.push(b'\n');
            }
        }
        Ok(out)
    })
    .await;

    match built {
        Ok(Ok(bytes)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"kachat-chat-export.txt\"",
                ),
            ],
            bytes,
        )
            .into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "export failed").into_response(),
    }
}

/// POST /import-file — restore records from an exported dump. Body is the raw file.
pub async fn import_file(State(state): State<ExportApi>, body: Bytes) -> impl IntoResponse {
    let data = body.to_vec();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<(usize, usize)> {
        let mut parts = std::collections::HashMap::new();
        for name in EXPORT_PARTITIONS {
            parts.insert(
                *name,
                state
                    .tx_keyspace
                    .open_partition(name, PartitionCreateOptions::default())?,
            );
        }
        let mut wtx = state.tx_keyspace.write_tx()?;
        let mut imported = 0usize;
        let mut skipped = 0usize;
        for line in data.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            // Strip only a trailing CR (tolerate CRLF files) — NOT spaces, because key-only
            // partitions store an empty value, so those lines legitimately end with a space
            // (name key ""), which must still split into three fields.
            let line = match std::str::from_utf8(line) {
                Ok(l) => l.strip_suffix('\r').unwrap_or(l),
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let mut it = line.split(' ');
            let (Some(name), Some(k_hex), Some(v_hex), None) =
                (it.next(), it.next(), it.next(), it.next())
            else {
                skipped += 1;
                continue;
            };
            let Some(part) = parts.get(name) else {
                skipped += 1;
                continue;
            };
            let (Ok(k), Ok(v)) = (decode_hex(k_hex), decode_hex(v_hex)) else {
                skipped += 1;
                continue;
            };
            wtx.insert(part, k, v);
            imported += 1;
        }
        if wtx.commit()?.is_err() {
            anyhow::bail!("commit conflict");
        }
        Ok((imported, skipped))
    })
    .await;

    match res {
        Ok(Ok((imported, skipped))) => (
            StatusCode::OK,
            format!("{{\"imported\":{imported},\"skipped\":{skipped}}}"),
        )
            .into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"import failed\"}").into_response(),
    }
}

/// POST /personal/purge-all — wipe every stored chat record from all message partitions.
/// Used by the admin dashboard's personal-mode purge: clear the shared store, then let
/// personal mode re-accumulate only the operator's own data (and backfill via import-by-address).
pub async fn purge_all(State(state): State<ExportApi>) -> impl IntoResponse {
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
        let mut removed = 0usize;
        for name in EXPORT_PARTITIONS {
            let part = state
                .tx_keyspace
                .open_partition(name, PartitionCreateOptions::default())?;
            let keys: Vec<Vec<u8>> = {
                let rtx = state.tx_keyspace.read_tx();
                rtx.iter(&part)
                    .filter_map(|kv| kv.ok().map(|(k, _)| k.to_vec()))
                    .collect()
            };
            let mut wtx = state.tx_keyspace.write_tx()?;
            for k in keys {
                wtx.remove(&part, k);
                removed += 1;
            }
            if wtx.commit()?.is_err() {
                anyhow::bail!("commit conflict during purge");
            }
        }
        Ok(removed)
    })
    .await;

    match res {
        Ok(Ok(removed)) => (
            StatusCode::OK,
            format!("{{\"removed\":{removed}}}"),
        )
            .into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"purge failed\"}").into_response(),
    }
}

fn decode_hex(s: &str) -> anyhow::Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        anyhow::bail!("odd hex");
    }
    let mut out = vec![0u8; s.len() / 2];
    faster_hex::hex_decode(s.as_bytes(), &mut out)?;
    Ok(out)
}
