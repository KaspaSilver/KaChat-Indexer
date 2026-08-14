use crate::config::get_indexer_config;
use crate::context::{IndexerContext, get_indexer_context};
use crate::push::{
    PUSH_REGISTRY_COMMAND_CAPACITY, PushDispatcher, PushRegistry, PushRegistryActor,
};
use dotenv::dotenv;
use fjall::Config;
use futures_util::TryFutureExt;
use indexer_actors::block_processor::BlockProcessor;
use indexer_actors::data_source::DataSource;
use indexer_actors::metrics::{IndexerMetricsSnapshot, create_shared_metrics_from_snapshot};
use indexer_actors::periodic_processor::PeriodicProcessor;
use indexer_actors::ticker::Ticker;
use indexer_actors::util::ToHex64;
use indexer_actors::virtual_chain_processor::{CompactHeader, VirtualProcessor};
use indexer_db::headers::block_compact_headers::BlockCompactHeaderPartition;
use indexer_db::headers::block_gaps::{BlockGap, BlockGapsPartition};
use indexer_db::headers::daa_index::DaaIndexPartition;
use indexer_db::messages::contextual_message::{
    ContextualMessageBySenderPartition, TxIdToContextualMessagePartition,
};
use indexer_db::messages::group_control::{
    GroupControlByRecipientPartition, GroupControlBySenderPartition, TxIdToGroupControlPartition,
};
use indexer_db::messages::group_message::{
    GroupMessageByBlindedGroupIdPartition, GroupSenderBindingPartition, TxIdToGroupMessagePartition,
};
use indexer_db::messages::handshake::{
    HandshakeByReceiverPartition, HandshakeBySenderPartition, TxIdToHandshakePartition,
};
use indexer_db::messages::payment::{
    PaymentByReceiverPartition, PaymentBySenderPartition, TxIdToPaymentPartition,
};
use indexer_db::messages::self_stash::{SelfStashByOwnerPartition, TxIdToSelfStashPartition};
use indexer_db::metadata::MetadataPartition;
use indexer_db::migration::apply_migrations;
use indexer_db::processing::accepting_block_to_txs::AcceptingBlockToTxIDPartition;
use indexer_db::processing::pending_senders::PendingSenderResolutionPartition;
use indexer_db::processing::tx_id_to_acceptance::TxIDToAcceptancePartition;
use indexer_db::push::{
    DeviceRegistrationPartition, PrimaryAddressPartition, WatchedAddressPartition,
    WatchedGroupIdPartition,
};
use kaspa_rpc_core::RpcBlueWorkType;
use kaspa_wrpc_client::client::{ConnectOptions, ConnectStrategy};
use kaspa_wrpc_client::prelude::NetworkType;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use time::macros::format_description;
use tracing::level_filters::LevelFilter;
use tracing::{error, info};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};
use workflow_core::channel::Channel;

mod api;
mod config;
mod context;
mod fcm;
mod push;
mod signals;

/// Load the personal-mode address allowlist (KaChat fork). The admin dashboard writes one
/// address per line to `KASIA_PERSONAL_FILE` (default `/app/data/personal_addresses.txt`) and
/// restarts this process; an absent/empty file means personal mode is off (index everything).
fn load_personal_addresses() {
    let path = std::env::var("KASIA_PERSONAL_FILE")
        .unwrap_or_else(|_| "/app/data/personal_addresses.txt".to_string());
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => {
            indexer_actors::set_personal_addresses(Vec::new());
            return;
        }
    };
    let mut addrs = Vec::new();
    for token in content.split(['\n', '\r', ',', ' ', '\t']) {
        let t = token.trim();
        if t.is_empty() {
            continue;
        }
        match kaspa_rpc_core::RpcAddress::try_from(t.to_string())
            .ok()
            .and_then(|rpc| indexer_db::AddressPayload::try_from(&rpc).ok())
        {
            Some(ap) => addrs.push(ap),
            None => tracing::warn!("Personal mode: skipping unparseable address '{t}'"),
        }
    }
    if addrs.is_empty() {
        info!("Personal indexing mode OFF (no addresses configured) — indexing all chat data");
    } else {
        info!(
            "Personal indexing mode ON — restricting chat storage to {} address(es)",
            addrs.len()
        );
    }
    indexer_actors::set_personal_addresses(addrs);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ignore faillures as .env might not be present at runtime, and this use-case is tolerated
    dotenv()
        .inspect_err(|err| println!("[WARN] reading .env files is failed with err {err}"))
        .ok();

    let config = get_indexer_config()?;
    let context = get_indexer_context(&config)?;

    let _g = init_logs(&context)?;

    info!("Using DB Path: {}", context.db_path.to_string_lossy());

    load_personal_addresses();

    let config = Config::new(context.clone().db_path).max_write_buffer_size(512 * 1024 * 1024);
    let tx_keyspace = config.open_transactional()?;
    let virtual_daa = Arc::new(AtomicU64::new(0));
    // Partitions
    let metadata_partition = MetadataPartition::new(&tx_keyspace)?;
    {
        metadata_partition.0.inner().major_compact()?;
    }
    apply_migrations(&metadata_partition, &tx_keyspace)?;

    let handshake_by_receiver_partition = HandshakeByReceiverPartition::new(&tx_keyspace)?;
    let tx_id_to_handshake_partition = TxIdToHandshakePartition::new(&tx_keyspace)?;
    let contextual_message_partition = ContextualMessageBySenderPartition::new(&tx_keyspace)?;
    let tx_id_to_contextual_message_partition =
        TxIdToContextualMessagePartition::new(&tx_keyspace)?;
    let payment_by_receiver_partition = PaymentByReceiverPartition::new(&tx_keyspace)?;
    let tx_id_to_payment_partition = TxIdToPaymentPartition::new(&tx_keyspace)?;
    let self_stash_by_owner_partition = SelfStashByOwnerPartition::new(&tx_keyspace)?;
    let tx_id_to_self_stash_partition = TxIdToSelfStashPartition::new(&tx_keyspace)?;
    let group_message_by_blinded_group_id_partition =
        GroupMessageByBlindedGroupIdPartition::new(&tx_keyspace)?;
    let tx_id_to_group_message_partition = TxIdToGroupMessagePartition::new(&tx_keyspace)?;
    let group_sender_binding_partition = GroupSenderBindingPartition::new(&tx_keyspace)?;
    let group_control_by_sender_partition = GroupControlBySenderPartition::new(&tx_keyspace)?;
    let group_control_by_recipient_partition = GroupControlByRecipientPartition::new(&tx_keyspace)?;
    let tx_id_to_group_control_partition = TxIdToGroupControlPartition::new(&tx_keyspace)?;
    let tx_id_to_acceptance_partition = TxIDToAcceptancePartition::new(&tx_keyspace)?;
    let block_compact_header_partition = BlockCompactHeaderPartition::new(&tx_keyspace)?;
    let acceptance_to_tx_id_partition = AcceptingBlockToTxIDPartition::new(&tx_keyspace)?;
    let pending_sender_resolution_partition = PendingSenderResolutionPartition::new(&tx_keyspace)?;
    let handshake_by_sender_partition = HandshakeBySenderPartition::new(&tx_keyspace)?;
    let payment_by_sender_partition = PaymentBySenderPartition::new(&tx_keyspace)?;
    let block_gaps_partition = BlockGapsPartition::new(&tx_keyspace)?;
    let block_daa_index_partition = DaaIndexPartition::new(&tx_keyspace)?;
    let device_registration_partition = DeviceRegistrationPartition::new(&tx_keyspace)?;
    let watched_address_partition = WatchedAddressPartition::new(&tx_keyspace)?;
    let watched_group_id_partition = WatchedGroupIdPartition::new(&tx_keyspace)?;
    let primary_address_partition = PrimaryAddressPartition::new(&tx_keyspace)?;

    let gaps = block_gaps_partition
        .get_all_gaps()
        .collect::<Result<Vec<_>, _>>()?;
    print_gaps(&gaps);

    let push_registered_devices = device_registration_partition.approximate_len() as u64;

    let metrics = create_shared_metrics_from_snapshot(IndexerMetricsSnapshot {
        handshakes_by_sender: handshake_by_sender_partition.approximate_len() as u64,
        uniq_handshakes_by_receiver: tx_id_to_handshake_partition.approximate_len() as u64,
        payments_by_sender: payment_by_sender_partition.approximate_len() as u64,
        uniq_payments_by_receiver: tx_id_to_payment_partition.approximate_len() as u64,
        contextual_messages: contextual_message_partition.len()? as u64,
        group_messages: group_message_by_blinded_group_id_partition.approximate_len() as u64,
        group_controls: tx_id_to_group_control_partition.approximate_len() as u64,
        blocks_processed: block_compact_header_partition.len()? as u64,
        latest_block: metadata_partition
            .get_latest_block_cursor_rtx(&tx_keyspace.read_tx())?
            .unwrap_or_default()
            .to_hex_64(),
        latest_accepting_block: metadata_partition
            .get_latest_accepting_block_cursor()?
            .unwrap_or_default()
            .block_hash
            .to_hex_64(),
        unknown_sender_entries: pending_sender_resolution_partition.len()? as u64,
        resolved_senders: 0,
        pruned_blocks: 0,
        push_registered_devices,
        push_register_calls_total: 0,
        push_update_calls_total: 0,
        push_unregister_calls_total: 0,
        push_fast_path_skips_total: 0,
        push_events_total: 0,
        push_tokens_looked_up_total: 0,
        push_filtered_alias_total: 0,
        push_filtered_primary_total: 0,
        push_dedup_dropped_total: 0,
        push_sent_ok_total: 0,
        push_send_failed_total: 0,
        push_unregistered_removed_total: 0,
        push_invalid_token_total: 0,
        db_read_ops_total: 0,
        db_write_ops_total: 0,
        db_read_time_ms_total: 0,
        db_write_time_ms_total: 0,
        db_commit_conflicts_total: 0,
        db_errors_total: 0,
    });

    let push_registry = PushRegistry::new(
        tx_keyspace.clone(),
        device_registration_partition,
        watched_address_partition,
        watched_group_id_partition,
        primary_address_partition,
        metrics.clone(),
    );
    // Address Activity: seed the block processor's watch-only gate from persisted registrations
    // before the registry is moved into its actor (rebuild reads the device partition).
    push_registry.rebuild_watch_only_global();
    let (push_registry_actor, push_registry) =
        PushRegistryActor::new(push_registry, PUSH_REGISTRY_COMMAND_CAPACITY);
    let _push_registry_actor_handle = std::thread::Builder::new()
        .name("push-registry".to_string())
        .spawn(move || push_registry_actor.process())?;
    let (push_tx, push_rx) = flume::bounded(2048);
    // KaChat fork: broadcast/KaPosts pushes injected by the K-processor over HTTP.
    let (ext_push_tx, ext_push_rx) = flume::bounded(1024);
    // Address Activity: funds-received events from the block processor to the push dispatcher.
    let (funds_push_tx, funds_push_rx) = flume::bounded(2048);
    let push_dispatcher = PushDispatcher::new(
        push_rx,
        ext_push_rx,
        funds_push_rx,
        push_registry.clone(),
        &context,
    );
    let _push_handle = tokio::spawn(push_dispatcher.run());
    let (block_intake_tx, block_intake_rx) = flume::bounded(4096);
    let (vcc_intake_tx, vcc_intake_rx) = flume::bounded(4096);
    let (gap_result_tx, gap_result_rx) = flume::bounded(1024);
    let (processed_block_tx, processed_block_rx) = flume::bounded(4096);
    let command_channel = {
        let (command_tx, command_rx) = workflow_core::channel::bounded(1024);
        Channel {
            sender: command_tx,
            receiver: command_rx,
        }
    };
    let (syncer_tx, syncer_rx) = flume::bounded(4);
    let (shutdown_data_source_tx, shutdown_data_source_rx) = tokio::sync::mpsc::channel(2);
    let (periodic_intake_tx, periodic_intake_rx) = workflow_core::channel::bounded(1);
    let (periodic_resp_tx, periodic_resp_rx) = workflow_core::channel::bounded(1);
    let (shutdown_ticker_tx, shutdown_ticker_rx) = tokio::sync::mpsc::channel(2);

    let mut block_processor = BlockProcessor::builder()
        .notification_rx(block_intake_rx.clone())
        .gap_result_rx(gap_result_rx)
        .gap_result_tx(gap_result_tx)
        .processed_block_tx(processed_block_tx)
        .command_tx(command_channel.sender.clone())
        .tx_keyspace(tx_keyspace.clone())
        .blocks_gap_partition(block_gaps_partition.clone())
        .runtime_handle(tokio::runtime::Handle::current())
        .metadata_partition(metadata_partition.clone())
        .block_compact_header_partition(block_compact_header_partition.clone())
        .daa_index_partition(block_daa_index_partition.clone())
        .handshake_by_receiver_partition(handshake_by_receiver_partition.clone())
        .handshake_by_sender_partition(handshake_by_sender_partition.clone())
        .tx_id_to_handshake_partition(tx_id_to_handshake_partition.clone())
        .contextual_message_by_sender_partition(contextual_message_partition.clone())
        .tx_id_to_contextual_message_partition(tx_id_to_contextual_message_partition.clone())
        .payment_by_receiver_partition(payment_by_receiver_partition.clone())
        .payment_by_sender_partition(payment_by_sender_partition.clone())
        .self_stash_by_owner_partition(self_stash_by_owner_partition.clone())
        .tx_id_to_self_stash_partition(tx_id_to_self_stash_partition.clone())
        .tx_id_to_payment_partition(tx_id_to_payment_partition.clone())
        .group_message_by_blinded_group_id_partition(
            group_message_by_blinded_group_id_partition.clone(),
        )
        .tx_id_to_group_message_partition(tx_id_to_group_message_partition.clone())
        .group_sender_binding_partition(group_sender_binding_partition.clone())
        .group_control_by_sender_partition(group_control_by_sender_partition.clone())
        .group_control_by_recipient_partition(group_control_by_recipient_partition.clone())
        .tx_id_to_group_control_partition(tx_id_to_group_control_partition.clone())
        .tx_id_to_acceptance_partition(tx_id_to_acceptance_partition.clone())
        .shared_metrics(metrics.clone())
        .push_tx(push_tx.clone())
        .push_funds_tx(funds_push_tx.clone())
        .build();
    let mut virtual_processor = VirtualProcessor::builder()
        .synced_capacity(3_000_000)
        .processed_block_tx(processed_block_rx)
        .realtime_vcc_tx(vcc_intake_rx)
        .syncer_rx(syncer_rx)
        .syncer_tx(syncer_tx)
        .command_tx(command_channel.sender.clone())
        .tx_keyspace(tx_keyspace.clone())
        .metadata_partition(metadata_partition.clone())
        .tx_id_to_acceptance_partition(tx_id_to_acceptance_partition.clone())
        .accepting_block_to_tx_id_partition(acceptance_to_tx_id_partition.clone())
        .pending_sender_resolution_partition(pending_sender_resolution_partition.clone())
        .handshake_by_receiver_partition(handshake_by_receiver_partition.clone())
        .handshake_by_sender_partition(handshake_by_sender_partition.clone())
        .contextual_message_by_sender_partition(contextual_message_partition.clone())
        .payment_by_receiver_partition(payment_by_receiver_partition.clone())
        .payment_by_sender_partition(payment_by_sender_partition.clone())
        .self_stash_by_owner_partition(self_stash_by_owner_partition.clone())
        .tx_id_to_payment_partition(tx_id_to_payment_partition.clone())
        .tx_id_to_handshake_partition(tx_id_to_handshake_partition.clone())
        .tx_id_to_contextual_message_partition(tx_id_to_contextual_message_partition.clone())
        .tx_id_to_self_stash_partition(tx_id_to_self_stash_partition.clone())
        .group_message_by_blinded_group_id_partition(
            group_message_by_blinded_group_id_partition.clone(),
        )
        .tx_id_to_group_message_partition(tx_id_to_group_message_partition.clone())
        .group_sender_binding_partition(group_sender_binding_partition)
        .group_control_by_sender_partition(group_control_by_sender_partition.clone())
        .group_control_by_recipient_partition(group_control_by_recipient_partition.clone())
        .tx_id_to_group_control_partition(tx_id_to_group_control_partition.clone())
        .runtime(tokio::runtime::Handle::current())
        .push_tx(push_tx.clone())
        .build();

    let mut ticker = Ticker::new(
        Duration::from_secs(context.config.periodic_processor_interval_secs),
        shutdown_ticker_rx,
        periodic_intake_tx,
        periodic_resp_rx,
    );

    let periodic_processor = PeriodicProcessor::builder()
        .pruning_depth(3_000_000)
        .job_trigger_rx(periodic_intake_rx)
        .resp_tx(periodic_resp_tx)
        .metrics(metrics.clone())
        .virtual_daa(virtual_daa.clone())
        .tx_keyspace(tx_keyspace.clone())
        .daa_index_partition(block_daa_index_partition.clone())
        .block_compact_header_partition(block_compact_header_partition.clone())
        .accepting_block_to_tx_id_partition(acceptance_to_tx_id_partition.clone())
        .metadata_partition(metadata_partition.clone())
        .tx_id_to_handshake_partition(tx_id_to_handshake_partition.clone())
        .tx_id_to_payment_partition(tx_id_to_payment_partition.clone())
        .payment_by_sender_partition(payment_by_sender_partition.clone())
        .handshake_by_sender_partition(handshake_by_sender_partition.clone())
        .contextual_message_by_sender_partition(contextual_message_partition.clone())
        .build();

    let mut data_source = DataSource::new(
        context.rpc_client.clone(),
        block_intake_tx,
        vcc_intake_tx,
        shutdown_data_source_rx,
        virtual_daa.clone(),
        command_channel,
    );
    info!("start filling processed block cache");
    let rtx = tx_keyspace.read_tx();
    let processed_blocks = block_daa_index_partition
        .iter_lt(&rtx, u64::MAX)
        .rev() // we need to get values from the end (desc order)
        .map(|r| {
            r.map(|k| CompactHeader {
                block_hash: k.block_hash,
                blue_work: RpcBlueWorkType::from_be_bytes(k.blue_work),
                daa_score: k.daa_score.get(),
            })
        })
        .take(3_000_000)
        .collect::<Result<Vec<_>, _>>()?;
    let block_processor_handle = std::thread::spawn(move || block_processor.process());
    let virtual_processor_handle =
        std::thread::spawn(move || virtual_processor.process(processed_blocks, false));
    let periodic_processor_handle = std::thread::spawn(move || periodic_processor.process());
    let data_source_handle = tokio::spawn(async move { data_source.task().await });
    let ticker_handle = tokio::spawn(async move { ticker.process().await });

    let api_service = api::v1::Api::new(
        tx_keyspace.clone(),
        handshake_by_sender_partition,
        handshake_by_receiver_partition,
        contextual_message_partition,
        tx_id_to_contextual_message_partition,
        payment_by_sender_partition,
        payment_by_receiver_partition,
        tx_id_to_acceptance_partition,
        tx_id_to_handshake_partition,
        tx_id_to_payment_partition,
        self_stash_by_owner_partition,
        tx_id_to_self_stash_partition,
        group_message_by_blinded_group_id_partition,
        tx_id_to_group_message_partition,
        group_control_by_sender_partition,
        group_control_by_recipient_partition,
        tx_id_to_group_control_partition,
        metrics.clone(),
        api::v1::push::PushApi::new(
            push_registry.clone(),
            context.network_type,
            context.config.push_auth_mode,
            context.config.apns_team_id.clone(),
            context.config.apns_topic.clone(),
            ext_push_tx.clone(),
        ),
        context.clone(),
    );
    let (api_shutdown_tx, api_shutdown_rx) = tokio::sync::mpsc::channel(2);
    // API bind is configurable (KaChat Indexer fork) so it can share a host without colliding
    // on :8080. Defaults to the upstream 0.0.0.0:8080. Leaked to &'static for the spawned task.
    let api_bind: &'static str = Box::leak(
        std::env::var("KASIA_API_BIND")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
            .into_boxed_str(),
    );
    let api_handle = tokio::spawn(api_service.serve(api_bind, api_shutdown_rx));

    let options = ConnectOptions {
        block_async_connect: false,
        connect_timeout: Some(Duration::from_millis(10_000)),
        strategy: ConnectStrategy::Retry,
        ..Default::default()
    };

    tokio::time::sleep(Duration::from_secs(5)).await; // let time to spawn everything
    info!("Connecting to Kaspa node...");
    // dbg!(context.config.kaspa_node_wborsh_url)
    context
        .rpc_client
        .connect(Some(options))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to node: {}", e))?;
    let shutdown = Shutdown {
        api: api_shutdown_tx,
        data_source: shutdown_data_source_tx,
        ticker: shutdown_ticker_tx,
    };
    tokio::select! {
        _ = signals::wait_for_signal_impl() => {
            info!("Termination signal received. Shutting down...");
            shutdown.shutdown(None).await
        },
        r = api_handle => {
           _ = r.inspect(|_| info!("api has stopped"))
                .inspect_err(|err| error!("api has stopped with error: {}", err));
            shutdown.shutdown(Some(Exclude::Api)).await;
        },
        r = data_source_handle => {
           _ = r.inspect(|_| info!("data source has stopped")).inspect_err(|err| error!("data_source finished with err: {}", err));
             shutdown.shutdown(Some(Exclude::DataSource)).await;
        },
        r = ticker_handle => {
            _ = r.inspect(|_| info!("ticker has stopped")).inspect_err(|err| error!("ticker processing error: {}", err));
             shutdown.shutdown(Some(Exclude::Ticker)).await;
        },
    }

    info!("waiting for virtual processor finish");
    _ = virtual_processor_handle
        .join()
        .expect("failed to join virtual_processor thread")
        .inspect_err(|err| error!("virtual_processor stopped error: {}", err));

    info!("waiting for block processor finish");
    _ = block_processor_handle
        .join()
        .expect("failed to join block_processor thread")
        .inspect_err(|err| error!("block_processor stopped error: {}", err));

    info!("waiting for periodic processor finish");
    _ = periodic_processor_handle
        .join()
        .expect("failed to join periodic_processor thread")
        .inspect_err(|err| error!("periodic_processor stopped error: {}", err));

    info!("All tasks shut down.");

    Ok(())
}

pub fn init_logs(context: &IndexerContext) -> anyhow::Result<(WorkerGuard, WorkerGuard)> {
    let file_appender = rolling_file::BasicRollingFileAppender::new(
        context.log_path.join(format!(
            "kasia-indexer.{}.log",
            NetworkType::to_string(&context.network_type)
        )),
        rolling_file::RollingConditionBasic::new()
            .max_size(1024 * 1024 * 8)
            .daily(),
        14,
    )?;

    let (non_blocking_appender, guard_file) = tracing_appender::non_blocking(file_appender);
    let file_subscriber = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(non_blocking_appender)
        .with_filter(
            EnvFilter::builder()
                .with_env_var("RUST_LOG_FILE")
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        );
    let (non_blocking_appender, guard_stdout) = tracing_appender::non_blocking(std::io::stdout());
    let stdout_subscriber = tracing_subscriber::fmt::layer()
        .with_timer(tracing_subscriber::fmt::time::LocalTime::new(
            format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"),
        ))
        .with_writer(non_blocking_appender)
        .with_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        );

    tracing_subscriber::registry()
        .with(file_subscriber)
        .with(stdout_subscriber)
        .init();

    Ok((guard_file, guard_stdout))
}

fn print_gaps(gaps: &[BlockGap]) {
    let gaps = gaps.iter().map(format_gap).collect::<Vec<_>>();
    let gaps = gaps.join(", ");
    info!("Block Gaps found: {gaps}");
}

fn format_gap(bg: &BlockGap) -> String {
    format!(
        "BlockGap(from: {}, to: {})",
        bg.from.to_hex_64(),
        bg.to.to_hex_64()
    )
}

#[derive(Clone)]
struct Shutdown {
    api: tokio::sync::mpsc::Sender<()>,
    data_source: tokio::sync::mpsc::Sender<()>,
    ticker: tokio::sync::mpsc::Sender<()>,
}

enum Exclude {
    Api,
    DataSource,
    Ticker,
}
impl Shutdown {
    async fn shutdown(&self, exclude: Option<Exclude>) {
        match exclude {
            None => {
                _ = tokio::join!(
                    self.api
                        .send(())
                        .inspect_err(|_err| error!("failed to shutdown api")),
                    self.data_source
                        .send(())
                        .inspect_err(|_err| error!("failed to shutdown data source")),
                    self.ticker
                        .send(())
                        .inspect_err(|_err| error!("failed to shutdown ticker"))
                );
            }
            Some(Exclude::Api) => {
                _ = tokio::join!(
                    self.data_source
                        .send(())
                        .inspect_err(|_err| error!("failed to shutdown data source")),
                    self.ticker
                        .send(())
                        .inspect_err(|_err| error!("failed to shutdown ticker"))
                );
            }
            Some(Exclude::DataSource) => {
                _ = tokio::join!(
                    self.api
                        .send(())
                        .inspect_err(|_err| error!("failed to shutdown api")),
                    self.ticker
                        .send(())
                        .inspect_err(|_err| error!("failed to shutdown ticker"))
                );
            }
            Some(Exclude::Ticker) => {
                _ = tokio::join!(
                    self.api
                        .send(())
                        .inspect_err(|_err| error!("failed to shutdown api")),
                    self.data_source
                        .send(())
                        .inspect_err(|_err| error!("failed to shutdown data source")),
                );
            }
        }
    }
}
