//! The node side: `getVirtualChainFromBlockV2` over wRPC, mapped into the engine's shapes.

use anyhow::{Context, Result, anyhow};
use kachat_names::follower::VccBatch;
use kachat_names::ingest::{Tx, TxInput, TxOutput};
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_rpc_core::model::{
    GetVirtualChainFromBlockV2Response, RpcChainBlockAcceptedTransactions, RpcDataVerbosityLevel, RpcHash,
    RpcOptionalTransaction,
};
use kaspa_wrpc_client::client::{ConnectOptions, ConnectStrategy};
use kaspa_wrpc_client::{KaspaRpcClient, Resolver, WrpcEncoding};
use std::time::Duration;

pub struct Node {
    pub client: KaspaRpcClient,
}

impl Node {
    /// `url` is a `ws(s)://` wRPC Borsh endpoint, or `resolver` to use a public node from
    /// Kaspa's resolver network for `network` (e.g. `testnet-10`).
    pub async fn connect(url: &str, network: &str) -> Result<Self> {
        let client = if url == "resolver" {
            let network_id = network.parse().map_err(|e| anyhow!("network id {network}: {e}"))?;
            KaspaRpcClient::new(WrpcEncoding::Borsh, None, Some(Resolver::default()), Some(network_id), None)
        } else {
            KaspaRpcClient::new(WrpcEncoding::Borsh, Some(url), None, None, None)
        }
        .context("creating the wRPC client")?;
        let options = ConnectOptions {
            block_async_connect: true,
            connect_timeout: Some(Duration::from_secs(15)),
            strategy: ConnectStrategy::Retry,
            ..Default::default()
        };
        client.connect(Some(options)).await.context("connecting to the node")?;
        Ok(Self { client })
    }

    /// One batch of the virtual chain after `from`, holding back the newest
    /// `min_confirmations` chain blocks so a shallow reorg never reaches indexed state.
    /// Returns the batch and the DAA score of its last added chain block.
    pub async fn next_batch(&self, from: [u8; 32], min_confirmations: u64) -> Result<(VccBatch, Option<u64>)> {
        let resp = self
            .client
            .get_virtual_chain_from_block_v2(
                RpcHash::from_bytes(from),
                Some(RpcDataVerbosityLevel::High),
                Some(min_confirmations),
            )
            .await
            .map_err(|e| anyhow!("getVirtualChainFromBlockV2: {e}"))?;
        map_response(&resp)
    }

    /// The node's current virtual DAA score (to judge whether we are caught up).
    pub async fn virtual_daa(&self) -> Result<u64> {
        let info = self.client.get_block_dag_info().await.map_err(|e| anyhow!("getBlockDagInfo: {e}"))?;
        Ok(info.virtual_daa_score)
    }
}

fn bytes32(h: &RpcHash) -> [u8; 32] {
    h.as_bytes()
}

fn map_response(resp: &GetVirtualChainFromBlockV2Response) -> Result<(VccBatch, Option<u64>)> {
    let removed_blocks = resp.removed_chain_block_hashes.iter().map(bytes32).collect();
    let mut accepted = Vec::new();
    let mut last_daa = None;
    for block in resp.chain_block_accepted_transactions.iter() {
        let (hash, daa, time) = block_identity(block)?;
        last_daa = Some(daa);
        for tx in &block.accepted_transactions {
            accepted.push(map_tx(tx, hash, daa, time)?);
        }
    }
    let tip = resp.added_chain_block_hashes.last().map(bytes32);
    Ok((VccBatch { removed_blocks, accepted, tip }, last_daa))
}

fn block_identity(block: &RpcChainBlockAcceptedTransactions) -> Result<([u8; 32], u64, i64)> {
    let h = &block.chain_block_header;
    let hash = h.hash.as_ref().ok_or_else(|| anyhow!("chain block without a hash"))?;
    let daa = h.daa_score.ok_or_else(|| anyhow!("chain block without a DAA score"))?;
    let time = h.timestamp.ok_or_else(|| anyhow!("chain block without a timestamp"))? as i64;
    Ok((bytes32(hash), daa, time))
}

fn map_tx(tx: &RpcOptionalTransaction, block: [u8; 32], daa: u64, time: i64) -> Result<Tx> {
    let id = tx
        .verbose_data
        .as_ref()
        .and_then(|v| v.transaction_id.as_ref())
        .ok_or_else(|| anyhow!("accepted transaction without an id (verbosity too low?)"))?;
    let mut inputs = Vec::with_capacity(tx.inputs.len());
    for input in &tx.inputs {
        let prev = input.previous_outpoint.as_ref().ok_or_else(|| anyhow!("input without an outpoint"))?;
        let txid = prev.transaction_id.as_ref().ok_or_else(|| anyhow!("outpoint without a txid"))?;
        let index = prev.index.ok_or_else(|| anyhow!("outpoint without an index"))?;
        inputs.push(TxInput {
            previous_outpoint: (bytes32(txid), index),
            signature_script: input.signature_script.clone().unwrap_or_default(),
        });
    }
    let outputs = tx
        .outputs
        .iter()
        .map(|o| TxOutput {
            script_public_key: o.script_public_key.as_ref().map(|s| s.script().to_vec()).unwrap_or_default(),
            value: o.value.unwrap_or(0),
        })
        .collect();
    Ok(Tx {
        id: bytes32(id),
        inputs,
        outputs,
        payload: tx.payload.clone().unwrap_or_default(),
        accepting_block: block,
        accepting_daa: daa,
        block_time: time,
    })
}
