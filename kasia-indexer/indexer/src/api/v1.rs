use crate::api::v1::contextual_messages::ContextualMessageApi;
use crate::api::v1::group_control::GroupControlApi;
use crate::api::v1::group_messages::GroupMessageApi;
use crate::api::v1::handshakes::HandshakeApi;
use crate::api::v1::payments::PaymentApi;
use crate::api::v1::push::PushApi;
use crate::api::v1::self_stash::SelfStashApi;
use crate::context::IndexerContext;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use indexer_actors::metrics::{IndexerMetricsSnapshot, SharedMetrics};
use indexer_db::messages::contextual_message::{
    ContextualMessageBySenderPartition, TxIdToContextualMessagePartition,
};
use indexer_db::messages::group_control::{
    GroupControlByRecipientPartition, GroupControlBySenderPartition, TxIdToGroupControlPartition,
};
use indexer_db::messages::group_message::{
    GroupMessageByBlindedGroupIdPartition, TxIdToGroupMessagePartition,
};
use indexer_db::messages::handshake::{
    HandshakeByReceiverPartition, HandshakeBySenderPartition, TxIdToHandshakePartition,
};
use indexer_db::messages::payment::{
    PaymentByReceiverPartition, PaymentBySenderPartition, TxIdToPaymentPartition,
};
use indexer_db::messages::self_stash::{SelfStashByOwnerPartition, TxIdToSelfStashPartition};
use indexer_db::processing::tx_id_to_acceptance::TxIDToAcceptancePartition;
use std::net::SocketAddr;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

pub mod contextual_messages;
pub mod export;
pub mod group_control;
pub mod group_messages;
pub mod handshakes;
pub mod payments;
mod prometheus;
pub mod push;
pub mod self_stash;

#[derive(OpenApi)]
#[openapi(
    paths(
        handshakes::get_handshakes_by_sender,
        handshakes::get_handshakes_by_receiver,
        contextual_messages::get_contextual_messages_by_sender,
        payments::get_payments_by_sender,
        payments::get_payments_by_receiver,
        self_stash::get_self_stash_by_owner,
        push::create_challenge,
        push::register_device,
        push::update_registration,
        push::unregister_device,
        group_messages::get_group_messages_by_blinded_group_id,
        group_control::get_group_control_by_sender,
        group_control::get_group_control_by_recipient,
        get_metrics,
        get_prometheus_metrics,
    ),
    components(
        schemas(handshakes::HandshakeResponse, contextual_messages::ContextualMessageResponse, payments::PaymentResponse, self_stash::SelfStashResponse, push::PushRegistrationRequest, push::PushUpdateRequest, push::PushUnregisterRequest, push::PushAuthRequest, push::PushChallengeResponse, push::PushResponse, push::ErrorResponse, group_messages::GroupMessageResponse, group_control::GroupControlResponse, IndexerMetricsSnapshot)
    ),
    tags(
        (name = "Kasia Indexer API", description = "Kasia Indexer API")
    )
)]
pub struct ApiDoc;

#[derive(Clone)]
pub struct Api {
    handshake_api: HandshakeApi,
    contextual_message_api: ContextualMessageApi,
    payment_api: PaymentApi,
    self_stash_api: SelfStashApi,
    push_api: PushApi,
    group_message_api: GroupMessageApi,
    group_control_api: GroupControlApi,
    export_api: export::ExportApi,
    metrics: SharedMetrics,
}

const PUSH_REQUEST_BODY_MAX_BYTES: usize = 64 * 1024;

impl Api {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_keyspace: fjall::TxKeyspace,
        handshake_by_sender_partition: HandshakeBySenderPartition,
        handshake_by_receiver_partition: HandshakeByReceiverPartition,
        contextual_message_by_sender_partition: ContextualMessageBySenderPartition,
        tx_id_to_contextual_message_partition: TxIdToContextualMessagePartition,
        payment_by_sender_partition: PaymentBySenderPartition,
        payment_by_receiver_partition: PaymentByReceiverPartition,
        tx_id_to_acceptance_partition: TxIDToAcceptancePartition,
        tx_id_to_handshake_partition: TxIdToHandshakePartition,
        tx_id_to_payment_partition: TxIdToPaymentPartition,
        self_stash_by_owner_partition: SelfStashByOwnerPartition,
        tx_id_to_self_stash_partition: TxIdToSelfStashPartition,
        group_message_by_blinded_group_id_partition: GroupMessageByBlindedGroupIdPartition,
        tx_id_to_group_message_partition: TxIdToGroupMessagePartition,
        group_control_by_sender_partition: GroupControlBySenderPartition,
        group_control_by_recipient_partition: GroupControlByRecipientPartition,
        tx_id_to_group_control_partition: TxIdToGroupControlPartition,
        metrics: SharedMetrics,
        push_api: PushApi,
        context: IndexerContext,
    ) -> Self {
        let export_api = export::ExportApi::new(tx_keyspace.clone());

        let handshake_api = HandshakeApi::new(
            tx_keyspace.clone(),
            handshake_by_sender_partition,
            handshake_by_receiver_partition,
            tx_id_to_acceptance_partition.clone(),
            tx_id_to_handshake_partition,
            metrics.clone(),
            context.clone(),
        );

        let contextual_message_api = ContextualMessageApi::new(
            tx_keyspace.clone(),
            contextual_message_by_sender_partition,
            tx_id_to_acceptance_partition.clone(),
            tx_id_to_contextual_message_partition,
            metrics.clone(),
            context.clone(),
        );

        let payment_api = PaymentApi::new(
            tx_keyspace.clone(),
            payment_by_sender_partition,
            payment_by_receiver_partition,
            tx_id_to_payment_partition,
            tx_id_to_acceptance_partition.clone(),
            metrics.clone(),
            context.clone(),
        );

        let self_stash_api = SelfStashApi::new(
            tx_keyspace.clone(),
            self_stash_by_owner_partition,
            tx_id_to_acceptance_partition.clone(),
            tx_id_to_self_stash_partition,
            metrics.clone(),
            context.clone(),
        );

        let group_message_api = GroupMessageApi::new(
            tx_keyspace.clone(),
            group_message_by_blinded_group_id_partition,
            tx_id_to_acceptance_partition.clone(),
            tx_id_to_group_message_partition,
            metrics.clone(),
            context.clone(),
        );

        let group_control_api = GroupControlApi::new(
            tx_keyspace,
            group_control_by_sender_partition,
            group_control_by_recipient_partition,
            tx_id_to_acceptance_partition,
            tx_id_to_group_control_partition,
            metrics.clone(),
            context,
        );

        Self {
            handshake_api,
            contextual_message_api,
            payment_api,
            self_stash_api,
            push_api,
            group_message_api,
            group_control_api,
            export_api,
            metrics,
        }
    }

    pub async fn serve(
        self,
        bind_address: &str,
        mut shutdown: tokio::sync::mpsc::Receiver<()>,
    ) -> anyhow::Result<()> {
        let addr: SocketAddr = bind_address.parse()?;
        let app = self.router();
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!("Starting API server on {}", addr);
        axum::serve(listener, app.into_make_service())
            .with_graceful_shutdown(async move {
                shutdown.recv().await;
            })
            .await?;
        Ok(())
    }

    fn router(&self) -> Router {
        let push_router = PushApi::router()
            .with_state(self.push_api.clone())
            .layer(DefaultBodyLimit::max(PUSH_REQUEST_BODY_MAX_BYTES));
        // KaChat fork: internal-only broadcast/KaPosts push injection (not proxied publicly).
        let internal_push_router = PushApi::internal_router().with_state(self.push_api.clone());

        Router::new()
            .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
            .nest(
                "/handshakes",
                HandshakeApi::router().with_state(self.handshake_api.clone()),
            )
            .nest(
                "/contextual-messages",
                ContextualMessageApi::router().with_state(self.contextual_message_api.clone()),
            )
            .nest(
                "/payments",
                PaymentApi::router().with_state(self.payment_api.clone()),
            )
            .nest(
                "/self-stash",
                SelfStashApi::router().with_state(self.self_stash_api.clone()),
            )
            .nest("/v1/push", push_router)
            .nest("/internal/push", internal_push_router)
            .nest(
                "/group-messages",
                GroupMessageApi::router().with_state(self.group_message_api.clone()),
            )
            .nest(
                "/group-control",
                GroupControlApi::router().with_state(self.group_control_api.clone()),
            )
            .route(
                "/metrics",
                get(get_metrics).with_state(self.metrics.clone()),
            )
            .route(
                "/metrics/prometheus",
                get(get_prometheus_metrics).with_state(self.metrics.clone()),
            )
            // KaChat fork: full-store export + import-file (large-body).
            .route(
                "/export",
                get(export::export_all).with_state(self.export_api.clone()),
            )
            .route(
                "/import-file",
                post(export::import_file)
                    .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
                    .with_state(self.export_api.clone()),
            )
            // KaChat fork: personal-mode purge — wipe the entire chat store.
            .route(
                "/personal/purge-all",
                post(export::purge_all).with_state(self.export_api.clone()),
            )
            // KaChat fork: GC self-stash index entries orphaned by a missing payload row.
            // Top-level path (not under the "/self-stash" nest) to avoid a route conflict.
            .route(
                "/self-stash-gc-orphans",
                post(export::gc_self_stash_orphans).with_state(self.export_api.clone()),
            )
    }
}

#[utoipa::path(
    get,
    path = "/metrics",
    responses(
        (status = 200, description = "Get metrics as JSON", body = IndexerMetricsSnapshot)
    )
)]
async fn get_metrics(State(metrics): State<SharedMetrics>) -> Json<IndexerMetricsSnapshot> {
    Json(metrics.snapshot())
}

#[utoipa::path(
    get,
    path = "/metrics/prometheus",
    responses(
        (status = 200, description = "Get Prometheus metrics", content_type = "text/plain", body = String)
    )
)]
async fn get_prometheus_metrics(State(metrics): State<SharedMetrics>) -> impl IntoResponse {
    (
        [(CONTENT_TYPE, prometheus::CONTENT_TYPE)],
        prometheus::render(&metrics.snapshot()),
    )
}
