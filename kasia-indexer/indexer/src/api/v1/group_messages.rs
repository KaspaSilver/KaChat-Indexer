use crate::api::to_rpc_address;
use crate::context::IndexerContext;
use anyhow::Context;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use indexer_actors::metrics::SharedMetrics;
use indexer_db::messages::group_message::{
    BLINDED_GROUP_ID_LEN, GroupMessageByBlindedGroupIdPartition, GroupMessageKeyByBlindedGroupId,
    TxIdToGroupMessagePartition,
};
use indexer_db::processing::tx_id_to_acceptance::TxIDToAcceptancePartition;
use indexer_db::{AddressPayload, IntoBytes, TryFromBytes};
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::mem::size_of;
use tokio::task::spawn_blocking;
use utoipa::{IntoParams, ToSchema};

#[derive(Clone)]
pub struct GroupMessageApi {
    tx_keyspace: fjall::TxKeyspace,
    group_message_by_blinded_group_id_partition: GroupMessageByBlindedGroupIdPartition,
    tx_id_to_acceptance_partition: TxIDToAcceptancePartition,
    tx_id_to_group_message_partition: TxIdToGroupMessagePartition,
    metrics: SharedMetrics,
    context: IndexerContext,
}

impl GroupMessageApi {
    pub fn new(
        tx_keyspace: fjall::TxKeyspace,
        group_message_by_blinded_group_id_partition: GroupMessageByBlindedGroupIdPartition,
        tx_id_to_acceptance_partition: TxIDToAcceptancePartition,
        tx_id_to_group_message_partition: TxIdToGroupMessagePartition,
        metrics: SharedMetrics,
        context: IndexerContext,
    ) -> Self {
        Self {
            tx_keyspace,
            group_message_by_blinded_group_id_partition,
            tx_id_to_acceptance_partition,
            tx_id_to_group_message_partition,
            metrics,
            context,
        }
    }

    pub fn router() -> Router<Self> {
        Router::new()
            .route(
                "/by-blinded-group-id",
                get(get_group_messages_by_blinded_group_id),
            )
            // Batched read: one request carries every member's lane (blinded id + cursor).
            .route(
                "/by-blinded-group-ids",
                post(post_group_messages_by_blinded_group_ids),
            )
            // Live polling: everything new across a set of ids since a block time (replaces the
            // per-block stream for groups).
            .route("/since", post(post_group_messages_since))
    }
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct GroupMessagePaginationParams {
    pub limit: Option<usize>,
    pub block_time: Option<u64>,
    /// Opaque cursor returned by the previous page. Preferred over `block_time`.
    pub cursor: Option<String>,
    pub blinded_group_id: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GroupMessageResponse {
    pub tx_id: String,
    pub sender: Option<String>,
    pub blinded_group_id: String,
    pub block_time: u64,
    pub cursor: String,
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
    path = "/group-messages/by-blinded-group-id",
    params(GroupMessagePaginationParams),
    responses(
        (status = 200, description = "Get group messages by blinded group id", body = [GroupMessageResponse]),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
async fn get_group_messages_by_blinded_group_id(
    State(state): State<GroupMessageApi>,
    Query(params): Query<GroupMessagePaginationParams>,
) -> impl IntoResponse {
    let limit = params.limit.unwrap_or(10).min(50);

    if params.blinded_group_id.len() != BLINDED_GROUP_ID_LEN * 2 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!(
                    "blinded_group_id hex length must be exactly {} characters",
                    BLINDED_GROUP_ID_LEN * 2
                ),
            }),
        ));
    }

    let mut blinded_group_id = [0u8; BLINDED_GROUP_ID_LEN];
    if let Err(e) =
        faster_hex::hex_decode(params.blinded_group_id.as_bytes(), &mut blinded_group_id)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid blinded_group_id hex: {e}"),
            }),
        ));
    }

    let cursor_key = match params.cursor.as_deref() {
        Some(cursor) => {
            let mut bytes = vec![0u8; size_of::<GroupMessageKeyByBlindedGroupId>()];
            if cursor.len() != bytes.len() * 2
                || faster_hex::hex_decode(cursor.as_bytes(), &mut bytes).is_err()
            {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: "Invalid group message cursor".to_string(),
                    }),
                ));
            }
            let Ok(key) = GroupMessageKeyByBlindedGroupId::try_read_from_bytes(&bytes) else {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: "Invalid group message cursor".to_string(),
                    }),
                ));
            };
            if key.blinded_group_id != blinded_group_id {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: "Cursor does not belong to blinded_group_id".to_string(),
                    }),
                ));
            }
            Some(key)
        }
        None => None,
    };
    let from_block_time = params
        .block_time
        .unwrap_or_else(|| cursor_key.map(|key| key.block_time.get()).unwrap_or(0));

    let metrics = state.metrics.clone();
    let db_read_started = std::time::Instant::now();
    let result = spawn_blocking(move || {
        let rtx = state.tx_keyspace.read_tx();
        let mut seen_tx_ids = HashSet::with_capacity(limit);

        state
            .group_message_by_blinded_group_id_partition
            .iter_by_blinded_group_id_from_block_time_rtx(&rtx, &blinded_group_id, from_block_time)
            .process_results(|iter| {
                iter.filter(|(key, _sender)| {
                    cursor_key
                        .as_ref()
                        .is_none_or(|cursor| key.as_bytes() > cursor.as_bytes())
                        && seen_tx_ids.insert(key.tx_id)
                })
                .take(limit)
                .map(|(key, sender_payload)| {
                    let block_time = key.block_time.get();
                    let sender = to_rpc_address(&sender_payload, state.context.network_type)
                        .context("Sender address conversion error")?
                        .map(|addr| addr.to_string());

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

                    let sealed_hex = state
                        .tx_id_to_group_message_partition
                        .get_rtx(&rtx, &key.tx_id)?
                        .context("Missing group message payload")?;
                    let message_payload = faster_hex::hex_string(sealed_hex.as_ref());

                    Ok(GroupMessageResponse {
                        tx_id: faster_hex::hex_string(&key.tx_id),
                        sender,
                        blinded_group_id: params.blinded_group_id.clone(),
                        block_time,
                        cursor: faster_hex::hex_string(key.as_bytes()),
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

/// Up to this many lanes per batched request (§1).
const MAX_BATCH_QUERIES: usize = 64;
/// Up to this many ids per `since` request (§2).
const MAX_SINCE_IDS: usize = 256;

/// Decode a blinded-group-id hex string to bytes, or a 400-ready error string.
fn decode_blinded_group_id(hex: &str) -> Result<[u8; BLINDED_GROUP_ID_LEN], String> {
    if hex.len() != BLINDED_GROUP_ID_LEN * 2 {
        return Err(format!(
            "blinded_group_id hex length must be exactly {} characters",
            BLINDED_GROUP_ID_LEN * 2
        ));
    }
    let mut out = [0u8; BLINDED_GROUP_ID_LEN];
    faster_hex::hex_decode(hex.as_bytes(), &mut out)
        .map_err(|e| format!("Invalid blinded_group_id hex: {e}"))?;
    Ok(out)
}

/// Build one response row (shared by the batched + since reads; identical shape to the GET).
fn build_message_row(
    state: &GroupMessageApi,
    rtx: &fjall::ReadTransaction,
    key: &GroupMessageKeyByBlindedGroupId,
    sender_payload: &AddressPayload,
    id_hex: &str,
) -> anyhow::Result<GroupMessageResponse> {
    let sender = to_rpc_address(sender_payload, state.context.network_type)
        .context("Sender address conversion error")?
        .map(|addr| addr.to_string());
    let acceptance = state
        .tx_id_to_acceptance_partition
        .acceptance_by_tx_id_rtx(rtx, &key.tx_id)?;
    let (accepting_block, accepting_daa_score) = if let Some(acceptance) = acceptance {
        (
            Some(faster_hex::hex_string(&acceptance.header.accepting_block_hash)),
            Some(acceptance.header.accepting_daa.into()),
        )
    } else {
        (None, None)
    };
    let sealed_hex = state
        .tx_id_to_group_message_partition
        .get_rtx(rtx, &key.tx_id)?
        .context("Missing group message payload")?;
    Ok(GroupMessageResponse {
        tx_id: faster_hex::hex_string(&key.tx_id),
        sender,
        blinded_group_id: id_hex.to_string(),
        block_time: key.block_time.get(),
        cursor: faster_hex::hex_string(key.as_bytes()),
        accepting_block,
        accepting_daa_score,
        message_payload: faster_hex::hex_string(sealed_hex.as_ref()),
    })
}

// ------------------------------------------------------------------- batched ---

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchedQuery {
    pub blinded_group_id: String,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct BatchedRequest {
    pub queries: Vec<BatchedQuery>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BatchedResultEntry {
    pub blinded_group_id: String,
    pub messages: Vec<GroupMessageResponse>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BatchedResponse {
    pub results: Vec<BatchedResultEntry>,
}

/// A validated lane, ready to read inside the blocking task.
struct PreparedLane {
    id_bytes: [u8; BLINDED_GROUP_ID_LEN],
    id_hex: String,
    cursor_key: Option<GroupMessageKeyByBlindedGroupId>,
    from_block_time: u64,
    limit: usize,
}

/// POST /group-messages/by-blinded-group-ids — one lane per member, each result exactly what the
/// GET for that id/cursor/limit would return. Empty (not omitted) for lanes with nothing.
async fn post_group_messages_by_blinded_group_ids(
    State(state): State<GroupMessageApi>,
    Json(request): Json<BatchedRequest>,
) -> impl IntoResponse {
    if request.queries.len() > MAX_BATCH_QUERIES {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("at most {MAX_BATCH_QUERIES} queries per request"),
            }),
        ));
    }

    // Validate + decode every lane up front so a bad input is a clean 400 (same as the GET).
    let mut lanes: Vec<PreparedLane> = Vec::with_capacity(request.queries.len());
    for q in &request.queries {
        let id_bytes = decode_blinded_group_id(&q.blinded_group_id)
            .map_err(|error| (StatusCode::BAD_REQUEST, Json(ErrorResponse { error })))?;
        let cursor_key = match q.cursor.as_deref() {
            Some(cursor) => {
                let mut bytes = vec![0u8; size_of::<GroupMessageKeyByBlindedGroupId>()];
                if cursor.len() != bytes.len() * 2
                    || faster_hex::hex_decode(cursor.as_bytes(), &mut bytes).is_err()
                {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        Json(ErrorResponse {
                            error: "Invalid group message cursor".to_string(),
                        }),
                    ));
                }
                let Ok(key) = GroupMessageKeyByBlindedGroupId::try_read_from_bytes(&bytes) else {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        Json(ErrorResponse {
                            error: "Invalid group message cursor".to_string(),
                        }),
                    ));
                };
                if key.blinded_group_id != id_bytes {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        Json(ErrorResponse {
                            error: "Cursor does not belong to blinded_group_id".to_string(),
                        }),
                    ));
                }
                Some(key)
            }
            None => None,
        };
        let from_block_time = cursor_key.map(|key| key.block_time.get()).unwrap_or(0);
        lanes.push(PreparedLane {
            id_bytes,
            id_hex: q.blinded_group_id.clone(),
            cursor_key,
            from_block_time,
            limit: q.limit.unwrap_or(10).min(50),
        });
    }

    let metrics = state.metrics.clone();
    let db_read_started = std::time::Instant::now();
    let result = spawn_blocking(move || {
        let rtx = state.tx_keyspace.read_tx();
        let mut results = Vec::with_capacity(lanes.len());
        for lane in lanes {
            let mut seen_tx_ids = HashSet::with_capacity(lane.limit);
            let messages = state
                .group_message_by_blinded_group_id_partition
                .iter_by_blinded_group_id_from_block_time_rtx(
                    &rtx,
                    &lane.id_bytes,
                    lane.from_block_time,
                )
                .process_results(|iter| {
                    iter.filter(|(key, _sender)| {
                        lane.cursor_key
                            .as_ref()
                            .is_none_or(|cursor| key.as_bytes() > cursor.as_bytes())
                            && seen_tx_ids.insert(key.tx_id)
                    })
                    .take(lane.limit)
                    .map(|(key, sender_payload)| {
                        build_message_row(&state, &rtx, &key, &sender_payload, &lane.id_hex)
                    })
                    .collect::<Result<Vec<_>, _>>()
                })
                .flatten()?;
            results.push(BatchedResultEntry {
                blinded_group_id: lane.id_hex,
                messages,
            });
        }
        Ok::<_, anyhow::Error>(results)
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
        Ok(Ok(results)) => Ok(Json(BatchedResponse { results })),
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

// --------------------------------------------------------------------- since ---

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SinceRequest {
    pub blinded_group_ids: Vec<String>,
    pub since_block_time: u64,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SinceResponse {
    pub messages: Vec<GroupMessageResponse>,
    pub latest_block_time: u64,
}

/// POST /group-messages/since — every row across the given ids with blockTime > sinceBlockTime,
/// oldest first, capped at limit. Replaces the per-block stream for groups.
async fn post_group_messages_since(
    State(state): State<GroupMessageApi>,
    Json(request): Json<SinceRequest>,
) -> impl IntoResponse {
    if request.blinded_group_ids.len() > MAX_SINCE_IDS {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("at most {MAX_SINCE_IDS} ids per request"),
            }),
        ));
    }
    let limit = request.limit.unwrap_or(200).min(500);
    let since = request.since_block_time;

    let mut ids: Vec<([u8; BLINDED_GROUP_ID_LEN], String)> =
        Vec::with_capacity(request.blinded_group_ids.len());
    for hex in &request.blinded_group_ids {
        let bytes = decode_blinded_group_id(hex)
            .map_err(|error| (StatusCode::BAD_REQUEST, Json(ErrorResponse { error })))?;
        ids.push((bytes, hex.clone()));
    }

    let metrics = state.metrics.clone();
    let db_read_started = std::time::Instant::now();
    let result = spawn_blocking(move || {
        let rtx = state.tx_keyspace.read_tx();
        let mut all: Vec<GroupMessageResponse> = Vec::new();
        for (id_bytes, id_hex) in &ids {
            let mut seen_tx_ids = HashSet::new();
            let rows = state
                .group_message_by_blinded_group_id_partition
                .iter_by_blinded_group_id_from_block_time_rtx(&rtx, id_bytes, since)
                .process_results(|iter| {
                    iter.filter(|(key, _sender)| {
                        key.block_time.get() > since && seen_tx_ids.insert(key.tx_id)
                    })
                    .take(limit)
                    .map(|(key, sender_payload)| {
                        build_message_row(&state, &rtx, &key, &sender_payload, id_hex)
                    })
                    .collect::<Result<Vec<_>, _>>()
                })
                .flatten()?;
            all.extend(rows);
        }
        // Merge to a single stream: oldest first by (blockTime, txId), then cap.
        all.sort_by(|a, b| {
            a.block_time
                .cmp(&b.block_time)
                .then_with(|| a.tx_id.cmp(&b.tx_id))
        });
        all.truncate(limit);
        let latest_block_time = all.iter().map(|m| m.block_time).max().unwrap_or(since);
        Ok::<_, anyhow::Error>(SinceResponse {
            messages: all,
            latest_block_time,
        })
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
        Ok(Ok(response)) => Ok(Json(response)),
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
