use crate::api::to_rpc_address;
use crate::context::IndexerContext;
use anyhow::bail;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::middleware::from_fn;
use axum::routing::{get, post};
use axum::{Json, Router};
use indexer_actors::metrics::SharedMetrics;
use indexer_db::AddressPayload;
use indexer_db::messages::contextual_message::{
    ContextualMessageByInboxPartition, ContextualMessageBySenderKey,
    ContextualMessageBySenderPartition, INBOX_TAG_LEN, TxIdToContextualMessagePartition,
};
use kaspa_rpc_core::RpcAddress;
use protocol::operation::SealedOperation;
use protocol::operation::deserializer::parse_sealed_operation;
use indexer_db::processing::tx_id_to_acceptance::TxIDToAcceptancePartition;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use tokio::task::spawn_blocking;
use utoipa::{IntoParams, ToSchema};

#[derive(Clone)]
pub struct ContextualMessageApi {
    tx_keyspace: fjall::TxKeyspace,
    contextual_message_by_sender_partition: ContextualMessageBySenderPartition,
    contextual_message_by_inbox_partition: ContextualMessageByInboxPartition,
    tx_id_to_contextual_message_partition: TxIdToContextualMessagePartition,
    tx_id_to_acceptance_partition: TxIDToAcceptancePartition,
    metrics: SharedMetrics,
    context: IndexerContext,
}

impl ContextualMessageApi {
    pub fn new(
        tx_keyspace: fjall::TxKeyspace,
        contextual_message_by_sender_partition: ContextualMessageBySenderPartition,
        contextual_message_by_inbox_partition: ContextualMessageByInboxPartition,
        tx_id_to_acceptance_partition: TxIDToAcceptancePartition,
        tx_id_to_contextual_message_partition: TxIdToContextualMessagePartition,
        metrics: SharedMetrics,
        context: IndexerContext,
    ) -> Self {
        Self {
            tx_keyspace,
            contextual_message_by_sender_partition,
            contextual_message_by_inbox_partition,
            tx_id_to_contextual_message_partition,
            tx_id_to_acceptance_partition,
            metrics,
            context,
        }
    }

    pub fn router() -> Router<Self> {
        Router::new()
            .route("/by-sender", get(get_contextual_messages_by_sender))
            .route("/by-inbox", get(get_contextual_messages_by_inbox))
            // kachat-audits IDX-001: bulk insert, internal callers only (kachat-admin).
            .route(
                "/import",
                post(import_contextual_messages)
                    .route_layer(from_fn(super::admin_auth::require_internal_secret)),
            )
    }
}

// --- KaChat fork: contextual-message import (explorer-sourced DM backfill) ---

#[derive(Debug, Deserialize)]
pub struct ImportTx {
    pub tx_id: String,
    pub payload: String,
    pub block_time: u64,
    pub block_hash: String,
    pub address: String,
}

#[derive(Debug, Serialize)]
pub struct ImportResult {
    pub imported: usize,
    pub skipped: usize,
}

fn import_hex_array<const N: usize>(s: &str) -> anyhow::Result<[u8; N]> {
    if s.len() != N * 2 {
        anyhow::bail!("expected {} hex chars, got {}", N * 2, s.len());
    }
    let mut out = [0u8; N];
    faster_hex::hex_decode(s.as_bytes(), &mut out)?;
    Ok(out)
}

fn import_hex_vec(s: &str) -> anyhow::Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        anyhow::bail!("odd hex");
    }
    let mut out = vec![0u8; s.len() / 2];
    faster_hex::hex_decode(s.as_bytes(), &mut out)?;
    Ok(out)
}

async fn import_contextual_messages(
    State(state): State<ContextualMessageApi>,
    Json(txs): Json<Vec<ImportTx>>,
) -> impl IntoResponse {
    let outcome = spawn_blocking(move || -> anyhow::Result<ImportResult> {
        let mut imported = 0usize;
        let mut skipped = 0usize;
        let mut wtx = state.tx_keyspace.write_tx()?;
        for t in &txs {
            let Ok(payload) = import_hex_vec(&t.payload) else {
                skipped += 1;
                continue;
            };
            let cm = match parse_sealed_operation(&payload) {
                Some(SealedOperation::ContextualMessageV1(cm)) => cm,
                _ => {
                    skipped += 1;
                    continue;
                }
            };
            let addr = match RpcAddress::try_from(t.address.clone())
                .ok()
                .and_then(|rpc| AddressPayload::try_from(&rpc).ok())
            {
                Some(a) => a,
                None => {
                    skipped += 1;
                    continue;
                }
            };
            let (Ok(tx_id), Ok(block_hash)) = (
                import_hex_array::<32>(&t.tx_id),
                import_hex_array::<32>(&t.block_hash),
            ) else {
                skipped += 1;
                continue;
            };
            let mut alias = [0u8; 16];
            let len = cm.alias.len().min(16);
            alias[..len].copy_from_slice(&cm.alias[..len]);
            state
                .tx_id_to_contextual_message_partition
                .insert_wtx(&mut wtx, &tx_id, cm.sealed_hex);
            let cmk = ContextualMessageBySenderKey {
                sender: addr,
                alias,
                block_time: t.block_time.into(),
                block_hash,
                receiver: addr,
                version: 1,
                tx_id,
            };
            state
                .contextual_message_by_sender_partition
                .insert(&mut wtx, &cmk);
            imported += 1;
        }
        let _ = wtx.commit()?;
        Ok(ImportResult { imported, skipped })
    })
    .await;

    match outcome {
        Ok(Ok(r)) => (StatusCode::OK, Json(r)).into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "import failed").into_response(),
    }
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct ContextualMessagePaginationParams {
    pub limit: Option<usize>,
    pub block_time: Option<u64>,
    pub address: String,
    pub alias: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ContextualMessageResponse {
    pub tx_id: String,
    pub sender: String,
    pub alias: String,
    pub block_time: u64,
    pub accepting_block: Option<String>,
    pub accepting_daa_score: Option<u64>,
    pub message_payload: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}

#[utoipa::path(
    get,
    path = "/contextual-messages/by-sender",
    params(ContextualMessagePaginationParams),
    responses(
        (status = 200, description = "Get contextual messages by sender", body = [ContextualMessageResponse]),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
async fn get_contextual_messages_by_sender(
    State(state): State<ContextualMessageApi>,
    Query(params): Query<ContextualMessagePaginationParams>,
) -> impl IntoResponse {
    let limit = params.limit.unwrap_or(10).min(50);
    let cursor = params.block_time.unwrap_or(0);

    let sender_rpc = match kaspa_rpc_core::RpcAddress::try_from(params.address) {
        Ok(addr) => addr,
        Err(e) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("Invalid address: {e}"),
                }),
            ));
        }
    };
    let sender = match AddressPayload::try_from(&sender_rpc) {
        Ok(payload) => payload,
        Err(e) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("Invalid address payload: {e}"),
                }),
            ));
        }
    };

    // Decode alias hex (max 32 hex chars = 16 bytes)
    if params.alias.len() > 32 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Alias hex length cannot exceed 32 characters".to_string(),
            }),
        ));
    }

    let mut alias_bytes = [0u8; 16];
    match faster_hex::hex_decode(
        params.alias.as_bytes(),
        &mut alias_bytes[..params.alias.len() / 2],
    ) {
        Ok(_) => (),
        Err(e) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("Invalid alias hex: {e}"),
                }),
            ));
        }
    };

    let alias = params.alias;

    let metrics = state.metrics.clone();
    let db_read_started = std::time::Instant::now();
    let result = spawn_blocking(move || {
        let rtx = state.tx_keyspace.read_tx();

        let mut seen_tx_ids = std::collections::HashSet::with_capacity(limit);

        state
            .contextual_message_by_sender_partition
            .get_by_sender_alias_from_block_time(&rtx, &sender, &alias_bytes, cursor)
            .process_results(|iter| {
                iter.filter(|message| seen_tx_ids.insert(message.tx_id))
                    .take(limit)
                    .map(|message_key| {
                        let block_time = message_key.block_time.into();

                        let sender_str =
                            match to_rpc_address(&message_key.sender, state.context.network_type) {
                                Ok(Some(addr)) => addr.to_string(),
                                Ok(None) => String::new(),
                                Err(e) => bail!("Address conversion error: {}", e),
                            };

                        let acceptance = state
                            .tx_id_to_acceptance_partition
                            .acceptance_by_tx_id_rtx(&rtx, &message_key.tx_id)?;

                        let (accepting_block, accepting_daa_score) =
                            if let Some(acceptance) = acceptance {
                                (
                                    Some(faster_hex::hex_string(
                                        &acceptance.header.accepting_block_hash,
                                    )),
                                    Some(acceptance.header.accepting_daa.into()),
                                )
                            } else {
                                (None, None)
                            };
                        let sealed_hex = state
                            .tx_id_to_contextual_message_partition
                            .get_rtx(&rtx, &message_key.tx_id)?
                            .expect("Message not found");
                        let message_payload = faster_hex::hex_string(sealed_hex.as_ref());

                        Ok(ContextualMessageResponse {
                            tx_id: faster_hex::hex_string(&message_key.tx_id),
                            sender: sender_str,
                            alias: alias.clone(), // todo use byteview
                            block_time,
                            accepting_block,
                            accepting_daa_score,
                            message_payload,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .flatten()
    })
    .await;
    metrics.increment_db_read_ops_total(1);
    metrics.increment_db_read_time_ms_total(
        db_read_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
    );
    if result.as_ref().is_err() || matches!(&result, Ok(Err(_))) {
        metrics.increment_db_errors_total();
    }

    match result {
        Ok(Ok(messages)) => Ok(Json(messages)),
        Ok(Err(e)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )),
        Err(join_err) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Task error: {join_err}"),
            }),
        )),
    }
}

// --- No-handshake messaging (KaChat 5.2): discovery by inbox tag ---

#[derive(Debug, Deserialize, IntoParams)]
pub struct InboxPaginationParams {
    /// Recipient inbox tag: 32 lowercase hex characters (NO_HANDSHAKE_MESSAGING.md §1).
    pub tag: String,
    pub limit: Option<usize>,
    pub block_time: Option<u64>,
}

/// GET /contextual-messages/by-inbox — the recipient asks for its own tag and learns who wrote,
/// without a handshake (NO_HANDSHAKE_MESSAGING.md §5.3). Same objects as `/by-sender`, with the
/// resolved sender filled from the index value; `alias` is empty here (the client derives it from
/// the sender). Ascending by block time, newer than `block_time`. A malformed tag is 400; an
/// unknown tag is `200 []`. The route merely existing is the client's "supported" probe.
#[utoipa::path(
    get,
    path = "/contextual-messages/by-inbox",
    params(InboxPaginationParams),
    responses(
        (status = 200, description = "Get contextual messages by inbox tag", body = [ContextualMessageResponse]),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
async fn get_contextual_messages_by_inbox(
    State(state): State<ContextualMessageApi>,
    Query(params): Query<InboxPaginationParams>,
) -> impl IntoResponse {
    let limit = params.limit.unwrap_or(100).min(500);
    let cursor = params.block_time.unwrap_or(0);

    let mut tag = [0u8; INBOX_TAG_LEN];
    if params.tag.len() != INBOX_TAG_LEN * 2
        || params.tag.bytes().any(|b| b.is_ascii_uppercase())
        || faster_hex::hex_decode(params.tag.as_bytes(), &mut tag).is_err()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "tag must be 32 lowercase hex characters".to_string(),
            }),
        ));
    }

    let metrics = state.metrics.clone();
    let db_read_started = std::time::Instant::now();
    let result = spawn_blocking(move || {
        let rtx = state.tx_keyspace.read_tx();
        let mut seen_tx_ids = std::collections::HashSet::with_capacity(limit);

        state
            .contextual_message_by_inbox_partition
            .iter_by_tag_from_block_time(&rtx, &tag, cursor)
            .process_results(|iter| {
                iter.filter(|(key, _sender)| seen_tx_ids.insert(key.tx_id))
                    .take(limit)
                    .map(|(key, sender_payload)| {
                        let block_time = key.block_time.into();

                        let sender_str =
                            match to_rpc_address(&sender_payload, state.context.network_type) {
                                Ok(Some(addr)) => addr.to_string(),
                                Ok(None) => String::new(),
                                Err(e) => bail!("Address conversion error: {}", e),
                            };

                        let acceptance = state
                            .tx_id_to_acceptance_partition
                            .acceptance_by_tx_id_rtx(&rtx, &key.tx_id)?;
                        let (accepting_block, accepting_daa_score) =
                            if let Some(acceptance) = acceptance {
                                (
                                    Some(faster_hex::hex_string(
                                        &acceptance.header.accepting_block_hash,
                                    )),
                                    Some(acceptance.header.accepting_daa.into()),
                                )
                            } else {
                                (None, None)
                            };

                        let message_payload = state
                            .tx_id_to_contextual_message_partition
                            .get_rtx(&rtx, &key.tx_id)?
                            .map(|sealed| faster_hex::hex_string(sealed.as_ref()))
                            .unwrap_or_default();

                        Ok(ContextualMessageResponse {
                            tx_id: faster_hex::hex_string(&key.tx_id),
                            sender: sender_str,
                            // Not stored in the inbox index; the client derives it from the sender.
                            alias: String::new(),
                            block_time,
                            accepting_block,
                            accepting_daa_score,
                            message_payload,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .flatten()
    })
    .await;
    metrics.increment_db_read_ops_total(1);
    metrics.increment_db_read_time_ms_total(
        db_read_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
    );
    if result.as_ref().is_err() || matches!(&result, Ok(Err(_))) {
        metrics.increment_db_errors_total();
    }

    match result {
        Ok(Ok(messages)) => Ok(Json(messages)),
        Ok(Err(e)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )),
        Err(join_err) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Task error: {join_err}"),
            }),
        )),
    }
}
