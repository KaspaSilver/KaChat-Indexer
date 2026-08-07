use crate::api::to_rpc_address;
use crate::context::IndexerContext;
use anyhow::Context;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use indexer_actors::metrics::SharedMetrics;
use indexer_db::messages::group_message::{
    BLINDED_GROUP_ID_LEN, GroupMessageByBlindedGroupIdPartition, GroupMessageKeyByBlindedGroupId,
    TxIdToGroupMessagePartition,
};
use indexer_db::processing::tx_id_to_acceptance::TxIDToAcceptancePartition;
use indexer_db::{IntoBytes, TryFromBytes};
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
        Router::new().route(
            "/by-blinded-group-id",
            get(get_group_messages_by_blinded_group_id),
        )
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
