use crate::api::to_rpc_address;
use crate::context::IndexerContext;
use anyhow::bail;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use indexer_actors::metrics::SharedMetrics;
use indexer_db::AddressPayload;
use indexer_db::messages::group_control::{
    GroupControlByRecipientPartition, GroupControlBySenderPartition, GroupControlKeyByRecipient,
    GroupControlKeyBySender, TxIdToGroupControlPartition,
};
use indexer_db::processing::tx_id_to_acceptance::TxIDToAcceptancePartition;
use indexer_db::{IntoBytes, TryFromBytes};
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use std::mem::size_of;
use tokio::task::spawn_blocking;
use utoipa::{IntoParams, ToSchema};

#[derive(Clone)]
pub struct GroupControlApi {
    tx_keyspace: fjall::TxKeyspace,
    group_control_by_sender_partition: GroupControlBySenderPartition,
    group_control_by_recipient_partition: GroupControlByRecipientPartition,
    tx_id_to_acceptance_partition: TxIDToAcceptancePartition,
    tx_id_to_group_control_partition: TxIdToGroupControlPartition,
    metrics: SharedMetrics,
    context: IndexerContext,
}

impl GroupControlApi {
    pub fn new(
        tx_keyspace: fjall::TxKeyspace,
        group_control_by_sender_partition: GroupControlBySenderPartition,
        group_control_by_recipient_partition: GroupControlByRecipientPartition,
        tx_id_to_acceptance_partition: TxIDToAcceptancePartition,
        tx_id_to_group_control_partition: TxIdToGroupControlPartition,
        metrics: SharedMetrics,
        context: IndexerContext,
    ) -> Self {
        Self {
            tx_keyspace,
            group_control_by_sender_partition,
            group_control_by_recipient_partition,
            tx_id_to_acceptance_partition,
            tx_id_to_group_control_partition,
            metrics,
            context,
        }
    }

    pub fn router() -> Router<Self> {
        Router::new()
            .route("/by-sender", get(get_group_control_by_sender))
            .route("/by-recipient", get(get_group_control_by_recipient))
    }
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct GroupControlPaginationParams {
    pub limit: Option<usize>,
    pub block_time: Option<u64>,
    pub cursor: Option<String>,
    pub sender: String,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct GroupControlByRecipientPaginationParams {
    pub limit: Option<usize>,
    pub block_time: Option<u64>,
    pub cursor: Option<String>,
    pub recipient: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GroupControlResponse {
    pub tx_id: String,
    pub sender: String,
    pub recipient: Option<String>,
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
    path = "/group-control/by-sender",
    params(GroupControlPaginationParams),
    responses(
        (status = 200, description = "Get group control messages by sender", body = [GroupControlResponse]),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
async fn get_group_control_by_sender(
    State(state): State<GroupControlApi>,
    Query(params): Query<GroupControlPaginationParams>,
) -> impl IntoResponse {
    let limit = params.limit.unwrap_or(10).min(50);

    let sender_rpc = match kaspa_rpc_core::RpcAddress::try_from(params.sender) {
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

    let cursor_key = match params.cursor.as_deref() {
        Some(cursor) => {
            let mut bytes = vec![0u8; size_of::<GroupControlKeyBySender>()];
            if cursor.len() != bytes.len() * 2
                || faster_hex::hex_decode(cursor.as_bytes(), &mut bytes).is_err()
            {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: "Invalid group control cursor".to_string(),
                    }),
                ));
            }
            let Ok(key) = GroupControlKeyBySender::try_read_from_bytes(&bytes) else {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: "Invalid group control cursor".to_string(),
                    }),
                ));
            };
            if key.sender != sender {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: "Cursor does not belong to sender".to_string(),
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

        let mut seen_tx_ids = std::collections::HashSet::with_capacity(limit);

        state
            .group_control_by_sender_partition
            .get_by_sender_from_block_time(&rtx, &sender, from_block_time)
            .process_results(|iter| {
                iter.filter(|message| {
                    cursor_key
                        .as_ref()
                        .is_none_or(|cursor| message.as_bytes() > cursor.as_bytes())
                        && seen_tx_ids.insert(message.tx_id)
                })
                .take(limit)
                .map(|message_key| {
                    let block_time = message_key.block_time.into();

                    let sender_str =
                        match to_rpc_address(&message_key.sender, state.context.network_type) {
                            Ok(Some(addr)) => addr.to_string(),
                            Ok(None) => String::new(),
                            Err(e) => bail!("Address conversion error: {}", e),
                        };
                    let recipient =
                        to_rpc_address(&message_key.recipient, state.context.network_type)?
                            .map(|address| address.to_string());

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
                        .tx_id_to_group_control_partition
                        .get_rtx(&rtx, &message_key.tx_id)?
                        .ok_or_else(|| anyhow::anyhow!("Group control payload not found"))?;
                    let message_payload = faster_hex::hex_string(sealed_hex.as_ref());

                    Ok(GroupControlResponse {
                        tx_id: faster_hex::hex_string(&message_key.tx_id),
                        sender: sender_str,
                        recipient,
                        block_time,
                        cursor: faster_hex::hex_string(message_key.as_bytes()),
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

#[utoipa::path(
    get,
    path = "/group-control/by-recipient",
    params(GroupControlByRecipientPaginationParams),
    responses(
        (status = 200, description = "Get addressed group control messages by recipient", body = [GroupControlResponse]),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
async fn get_group_control_by_recipient(
    State(state): State<GroupControlApi>,
    Query(params): Query<GroupControlByRecipientPaginationParams>,
) -> impl IntoResponse {
    let limit = params.limit.unwrap_or(10).min(50);
    let recipient_rpc = match kaspa_rpc_core::RpcAddress::try_from(params.recipient) {
        Ok(address) => address,
        Err(error) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("Invalid address: {error}"),
                }),
            ));
        }
    };
    let recipient = match AddressPayload::try_from(&recipient_rpc) {
        Ok(payload) => payload,
        Err(error) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("Invalid address payload: {error}"),
                }),
            ));
        }
    };

    let cursor_key = match params.cursor.as_deref() {
        Some(cursor) => {
            let mut bytes = vec![0u8; size_of::<GroupControlKeyByRecipient>()];
            if cursor.len() != bytes.len() * 2
                || faster_hex::hex_decode(cursor.as_bytes(), &mut bytes).is_err()
            {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: "Invalid group control cursor".to_string(),
                    }),
                ));
            }
            let Ok(key) = GroupControlKeyByRecipient::try_read_from_bytes(&bytes) else {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: "Invalid group control cursor".to_string(),
                    }),
                ));
            };
            if key.recipient != recipient {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: "Cursor does not belong to recipient".to_string(),
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
        let mut seen_tx_ids = std::collections::HashSet::with_capacity(limit);

        state
            .group_control_by_recipient_partition
            .get_by_recipient_from_block_time(&rtx, &recipient, from_block_time)
            .process_results(|iter| {
                iter.filter(|(message, _sender)| {
                    cursor_key
                        .as_ref()
                        .is_none_or(|cursor| message.as_bytes() > cursor.as_bytes())
                        && seen_tx_ids.insert(message.tx_id)
                })
                .take(limit)
                .map(|(message_key, sender_payload)| {
                    let sender = to_rpc_address(&sender_payload, state.context.network_type)?
                        .map(|address| address.to_string())
                        .unwrap_or_default();
                    let recipient =
                        to_rpc_address(&message_key.recipient, state.context.network_type)?
                            .map(|address| address.to_string());
                    let acceptance = state
                        .tx_id_to_acceptance_partition
                        .acceptance_by_tx_id_rtx(&rtx, &message_key.tx_id)?;
                    let (accepting_block, accepting_daa_score) = acceptance
                        .map(|acceptance| {
                            (
                                Some(faster_hex::hex_string(
                                    &acceptance.header.accepting_block_hash,
                                )),
                                Some(acceptance.header.accepting_daa.into()),
                            )
                        })
                        .unwrap_or((None, None));
                    let payload = state
                        .tx_id_to_group_control_partition
                        .get_rtx(&rtx, &message_key.tx_id)?
                        .ok_or_else(|| anyhow::anyhow!("Group control payload not found"))?;

                    Ok(GroupControlResponse {
                        tx_id: faster_hex::hex_string(&message_key.tx_id),
                        sender,
                        recipient,
                        block_time: message_key.block_time.get(),
                        cursor: faster_hex::hex_string(message_key.as_bytes()),
                        accepting_block,
                        accepting_daa_score,
                        message_payload: faster_hex::hex_string(payload.as_ref()),
                    })
                })
                .collect::<Result<Vec<_>, anyhow::Error>>()
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
        Ok(Err(error)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: error.to_string(),
            }),
        )),
        Err(join_error) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Task error: {join_error}"),
            }),
        )),
    }
}
