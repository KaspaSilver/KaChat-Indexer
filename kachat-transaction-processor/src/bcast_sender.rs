//! Public-chat (`kchat:1:bcast:`) author check (kachat-audits XP-012).
//!
//! A bcast payload carries no signature, so its author has to come from the transaction:
//! whoever signed it spends input 0. A post is accepted only in the self-send shape every
//! KaChat client writes, where output 0 pays the SAME address input 0 spends from; that
//! address is the sender. Anything else is dropped. Same rule as KaChat-Desktop
//! `judgeBroadcastSender` and the iOS block scan.
//!
//! Where input 0's address comes from: not the database. The ingester (run-ingest.sh) runs
//! simply-kaspa with `transactions_inputs` and `virtual_chain_processing` disabled, and even
//! with inputs on, its block path never resolves previous outpoints (only its VCP path does)
//! and 1 h retention prunes the previous transaction. So the node is asked:
//! `getVirtualChainFromBlockV2` (High verbosity) from the block that holds the tx returns
//! the tx as accepted, with input 0's spent UTXO (`utxo_entry`) attached — the same call the
//! profiles follower uses to prove a self-send.

use anyhow::{Result, anyhow};
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_rpc_core::model::{RpcDataVerbosityLevel, RpcHash};
use kaspa_wrpc_client::client::{ConnectOptions, ConnectStrategy};
use kaspa_wrpc_client::{KaspaRpcClient, WrpcEncoding};
use once_cell::sync::OnceCell;
use std::time::Duration;
use tracing::{info, warn};

static NODE: OnceCell<KaspaRpcClient> = OnceCell::new();

/// Chain blocks (blue score) after the tx's block searched for its accepting block. A
/// block's transactions are normally accepted a few chain blocks later; the second, wider
/// pass covers a congested DAG without pulling minutes of High-verbosity data.
const WINDOWS: [u64; 2] = [30, 300];

/// Connect (in the background, retrying) to the node's wRPC Borsh endpoint. With no URL,
/// every broadcast stays unverified and is dropped.
pub fn init(url: Option<String>) {
    let Some(url) = url.map(|u| u.trim().to_string()).filter(|u| !u.is_empty()) else {
        warn!("No node wRPC URL (--node-url / KASPA_NODE_WBORSH_URL): public-chat broadcasts cannot be verified and are dropped");
        return;
    };
    let client = match KaspaRpcClient::new(WrpcEncoding::Borsh, Some(&url), None, None, None) {
        Ok(c) => c,
        Err(e) => {
            warn!("Broadcast sender check: bad node URL {url}: {e}; broadcasts are dropped");
            return;
        }
    };
    if NODE.set(client.clone()).is_err() {
        return;
    }
    tokio::spawn(async move {
        let options = ConnectOptions {
            block_async_connect: false,
            connect_timeout: Some(Duration::from_secs(15)),
            strategy: ConnectStrategy::Retry,
            ..Default::default()
        };
        match client.connect(Some(options)).await {
            Ok(_) => info!("Broadcast sender check: node wRPC at {url}"),
            Err(e) => warn!("Broadcast sender check: connecting to {url}: {e}"),
        }
    });
}

/// A node URL was given (even if not connected yet). Without one nothing can ever be
/// verified, so broadcasts are dropped instead of queued (kachat-audits IDX-020).
pub fn configured() -> bool {
    NODE.get().is_some()
}

/// The wRPC connection is up right now.
pub fn is_connected() -> bool {
    NODE.get().is_some_and(|n| n.is_connected())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Self-send shape: the sender is this address (input 0's = output 0's).
    Verified(String),
    /// Output 0 pays someone other than the input-0 spender.
    Forged { input: String, output: String },
    /// Not known (yet): not accepted, node unreachable, or data missing. Never attributed.
    Unknown,
}

/// The one acceptance rule (pure, so it is unit-tested).
pub fn judge(input_address: Option<&str>, output_address: Option<&str>) -> Verdict {
    let clean = |a: Option<&str>| a.map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    match (clean(input_address), clean(output_address)) {
        (Some(i), Some(o)) if i == o => Verdict::Verified(i),
        (Some(input), Some(output)) => Verdict::Forged { input, output },
        _ => Verdict::Unknown,
    }
}

/// Look `tx_id` up as accepted after `block_hash` (the block holding it) and judge it.
/// `Unknown` when the tx is not accepted yet or the node is unavailable; callers retry.
pub async fn resolve(tx_id: &[u8], block_hash: &[u8]) -> Result<Verdict> {
    let node = NODE.get().ok_or_else(|| anyhow!("no node configured"))?;
    if !node.is_connected() {
        return Err(anyhow!("node not connected"));
    }
    let tx_id: [u8; 32] = tx_id.try_into().map_err(|_| anyhow!("tx id is not 32 bytes"))?;
    let start: [u8; 32] = block_hash.try_into().map_err(|_| anyhow!("block hash is not 32 bytes"))?;
    let start = RpcHash::from_bytes(start);

    let start_score = node.get_block(start, false).await.map_err(|e| anyhow!("getBlock: {e}"))?.header.blue_score;
    for window in WINDOWS {
        let sink_score = node.get_sink_blue_score().await.map_err(|e| anyhow!("getSinkBlueScore: {e}"))?;
        // The node trims a batch only by confirmations: hold back everything past
        // `start + window` so the response stays small.
        let hold_back = sink_score.saturating_sub(start_score + window);
        let resp = node
            .get_virtual_chain_from_block_v2(start, Some(RpcDataVerbosityLevel::High), Some(hold_back))
            .await
            .map_err(|e| anyhow!("getVirtualChainFromBlockV2: {e}"))?;
        let found = resp.chain_block_accepted_transactions.iter().flat_map(|b| b.accepted_transactions.iter()).find(|tx| {
            tx.verbose_data.as_ref().and_then(|v| v.transaction_id.as_ref()).is_some_and(|id| id.as_bytes() == tx_id)
        });
        if let Some(tx) = found {
            let input = tx
                .inputs
                .first()
                .and_then(|i| i.verbose_data.as_ref())
                .and_then(|v| v.utxo_entry.as_ref())
                .and_then(|u| u.verbose_data.as_ref())
                .and_then(|v| v.script_public_key_address.as_ref())
                .map(|a| a.to_string());
            let output = tx
                .outputs
                .first()
                .and_then(|o| o.verbose_data.as_ref())
                .and_then(|v| v.script_public_key_address.as_ref())
                .map(|a| a.to_string());
            return Ok(judge(input.as_deref(), output.as_deref()));
        }
        // Not past this window yet: the tx may simply not be accepted; let the caller retry.
        if sink_score < start_score + window {
            break;
        }
    }
    Ok(Verdict::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "kaspa:qypyrhxkfd055qulcvu6zccq4qe63qajrzgf7t4u4uusveguw6zzc3grrceeuex";
    const B: &str = "kaspa:qrjefk2r8wp607rmyvxmgjansqcwugjazpu2kk2r7057gltxetdvk8gl9fs0w";

    #[test]
    fn self_send_is_verified() {
        assert_eq!(judge(Some(A), Some(A)), Verdict::Verified(A.to_string()));
        assert_eq!(judge(Some(&A.to_uppercase()), Some(A)), Verdict::Verified(A.to_string()));
    }

    #[test]
    fn paying_someone_else_is_forged() {
        assert!(matches!(judge(Some(A), Some(B)), Verdict::Forged { .. }));
    }

    #[test]
    fn missing_side_is_unknown() {
        assert_eq!(judge(None, Some(A)), Verdict::Unknown);
        assert_eq!(judge(Some(A), None), Verdict::Unknown);
        assert_eq!(judge(Some(""), Some(A)), Verdict::Unknown);
    }
}
