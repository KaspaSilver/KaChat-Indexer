use crate::block_processor::BlockNotification;
use crate::util::ToHex64;
use crate::virtual_chain_processor::RealTimeVccNotification;
use anyhow::Context;
use futures_util::future::FutureExt;
use kaspa_consensus_core::tx::TransactionId;
use kaspa_rpc_core::api::ctl::RpcState;
use kaspa_rpc_core::api::ops::RpcApiOps;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_rpc_core::notify::connection::{ChannelConnection, ChannelType};
use kaspa_rpc_core::{
    BlockAddedNotification, GetBlocksRequest, GetBlocksResponse, GetUtxoReturnAddressRequest,
    GetUtxoReturnAddressResponse, GetVirtualChainFromBlockRequest,
    GetVirtualChainFromBlockResponse, Notification, RpcBlock, RpcHash,
};
use kaspa_wrpc_client::KaspaRpcClient;
use kaspa_wrpc_client::client::ConnectOptions;
use kaspa_wrpc_client::prelude::{
    BlockAddedScope, ListenerId, Scope, VirtualChainChangedScope, VirtualDaaScoreChangedScope,
};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use thiserror::Error;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};
use workflow_core::channel::Channel;
use workflow_serializer::prelude::Serializable;

pub struct DataSource {
    /// RPC client for communicating with Kaspa node
    rpc_client: KaspaRpcClient,
    connected: bool,
    /// Channel to send processed blocks to handler
    block_sender: flume::Sender<BlockNotification>,
    block_sender_closed: bool,
    vcc_sender: flume::Sender<RealTimeVccNotification>,
    vcc_sender_closed: bool,
    shutting_down: bool,
    // vcc_sender
    /// Shutdown signal receiver
    shutdown_rx: tokio::sync::mpsc::Receiver<()>,
    // channel supplied to the notification subsystem to receive the node
    // notifications we subscribe to
    notification_channel: Channel<Notification>,
    // listener id used to manage notification scopes we can have multiple IDs for
    // different scopes paired with multiple notification channels
    listener_id: Option<ListenerId>,
    virtual_daa: Arc<AtomicU64>,
    command_rx: workflow_core::channel::Channel<Command>,
    requests_queue: VecDeque<Request>,
    /// Sender lookups that failed with anything but a disconnect, waiting for their next try.
    sender_retries: SenderRetryQueue,
}

impl DataSource {
    pub fn new(
        rpc_client: KaspaRpcClient,
        block_sender: flume::Sender<BlockNotification>,
        vcc_sender: flume::Sender<RealTimeVccNotification>,
        shutdown_rx: tokio::sync::mpsc::Receiver<()>,
        virtual_daa: Arc<AtomicU64>,
        command_rx: workflow_core::channel::Channel<Command>,
    ) -> Self {
        let notification_channel = Channel::bounded(256);

        Self {
            rpc_client,
            connected: false,
            block_sender,
            block_sender_closed: false,
            vcc_sender,
            vcc_sender_closed: false,
            shutting_down: false,
            shutdown_rx,
            notification_channel,
            listener_id: None,
            virtual_daa,
            command_rx,
            requests_queue: VecDeque::new(),
            sender_retries: SenderRetryQueue::default(),
        }
    }
    pub async fn task(&mut self) -> anyhow::Result<()> {
        info!("Data source task started");
        let rpc_ctl_channel = self.rpc_client.rpc_ctl().multiplexer().channel();
        loop {
            tokio::select! {
                biased;
                command = self.command_rx.receiver.recv().fuse() => {
                    debug!("Received command in data source");
                    let con = self
                        .handle_command(command.inspect_err(|e| error!("RPC command channel error: {}", e))?)
                        .await?;
                    if !con {
                        info!("Data source task terminating due to command response");
                        return Ok(())
                    }
                }
                shutdown_result = self.shutdown_rx.recv().fuse() => {
                    match shutdown_result {
                        Some(()) => {
                            info!("Shutdown signal received, initiating data source shutdown");
                            self.handle_shutdown().await?;
                            info!("Data source shutdown initiated, waiting for processors to close");
                            // no need to return, we are waiting for command to mark other processor closed
                        }
                        None => {
                            warn!("Shutdown channel closed unexpectedly");
                            return Err(anyhow::anyhow!("Shutdown channel closed"));
                        }
                    }
                }
                msg = rpc_ctl_channel.receiver.recv().fuse() => {
                    debug!("Received RPC control message in data source");
                    // this will never occur if the RpcClient is owned and properly managed. This can
                    // only occur if RpcClient is deleted while this task is still running.
                    match msg.inspect_err(|e| error!("RPC CTL channel error: {}", e))? {
                        RpcState::Connected => {
                            if let Err(err) = self.handle_connect().await {
                                error!("Error in connect handler: {err}");
                            }
                        },
                        RpcState::Disconnected => {
                            if let Err(err) = self.handle_disconnect().await {
                                error!("Error in disconnect handler: {err}");
                            }
                        },
                    }
                }
                notification = self.notification_channel.receiver.recv().fuse() => {
                    _ =
                        self
                            .handle_notification(
                                notification.inspect_err(|e| error!("RPC notification channel error: {}", e))?,
                            )
                            .await
                            .inspect_err(|err| error!("Error while handling notification: {err}"));
                },
                // The future is built even while the branch is disabled, hence the far default.
                _ = tokio::time::sleep_until(
                    self.sender_retries
                        .next_due()
                        .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600)),
                ), if self.sender_retries.next_due().is_some() => {
                    for retry in self.sender_retries.pop_due(Instant::now()) {
                        self.retry_sender(retry).await;
                    }
                },
            }
        }
    }

    async fn handle_connect_impl(&mut self) -> anyhow::Result<()> {
        info!("Connected to {:?}", self.rpc_client.url());
        // register for notifications
        self.register_notification_listeners().await?;
        let info = self.rpc_client.get_block_dag_info().await?;

        let sink_blue_work = self
            .rpc_client
            .get_block(info.sink, false)
            .await?
            .header
            .blue_work;
        self.connected = true;
        let pair = futures_util::future::join(
            async {
                self.block_sender
                    .send_async(BlockNotification::Connected {
                        sink: info.sink.as_bytes(),
                        pp: info.pruning_point_hash.as_bytes(),
                    })
                    .await
                    .context("block handler connect send failed")
            },
            async {
                self.vcc_sender
                    .send_async(RealTimeVccNotification::Connected {
                        sink: info.sink.as_bytes(),
                        sink_blue_work,
                        pp: info.pruning_point_hash.as_bytes(),
                    })
                    .await
                    .context("vcc handler connect send failed")
            },
        )
        .await;
        pair.0.and(pair.1)?;
        for req in self.requests_queue.drain(..) {
            self.command_rx.send(Command::Request(req)).await?;
        }
        Ok(())
    }

    async fn handle_connect(&mut self) -> anyhow::Result<()> {
        info!("Handling RPC connection event");
        if self.shutting_down {
            info!("Shutting down, disconnecting from RPC client");
            _ = self
                .rpc_client
                .disconnect()
                .await
                .inspect_err(|err| error!("Error disconnecting: {err}"));
            return Ok(());
        }
        match self.handle_connect_impl().await {
            Err(err) => {
                self.connected = false;
                error!("Error while connecting to node: {err}");

                // force disconnect the client if we have failed to negotiate the connection to
                // the node. self.rpc_client().trigger_abort()?;
                warn!("Disconnecting and retrying connection in 3 seconds");
                self.rpc_client.disconnect().await?;
                tokio::time::sleep(Duration::from_secs(3)).await;
                let options = ConnectOptions {
                    block_async_connect: false,
                    ..Default::default()
                };
                info!("Attempting to reconnect to RPC client");
                self.rpc_client.connect(Some(options)).await?;
                Err(err)
            }
            Ok(_) => {
                info!("Successfully connected to RPC client");
                Ok(())
            }
        }
    }

    async fn register_notification_listeners(&mut self) -> anyhow::Result<()> {
        // IMPORTANT: notification scopes are managed by the node for the lifetime of the
        // RPC connection, as such they are "lost" if we disconnect. For that reason we
        // must re-register all notification scopes when we connect.
        let listener_id = self
            .rpc_client
            .register_new_listener(ChannelConnection::new(
                "kasia-subscrtiber",
                self.notification_channel.sender.clone(),
                ChannelType::Persistent,
            ));
        self.listener_id = Some(listener_id);
        self.rpc_client
            .start_notify(listener_id, Scope::BlockAdded(BlockAddedScope {}))
            .await?;
        self.rpc_client
            .start_notify(
                listener_id,
                Scope::VirtualDaaScoreChanged(VirtualDaaScoreChangedScope {}),
            )
            .await?;
        self.rpc_client
            .start_notify(
                listener_id,
                Scope::VirtualChainChanged(VirtualChainChangedScope {
                    include_accepted_transaction_ids: true,
                }),
            )
            .await?;
        Ok(())
    }

    async fn handle_disconnect(&mut self) -> anyhow::Result<()> {
        info!("Handling RPC disconnection event");
        if self.shutting_down {
            info!("Shutting down, disconnecting from RPC client");
            _ = self
                .rpc_client
                .disconnect()
                .await
                .inspect_err(|err| error!("Error disconnecting: {err}"));
            return Ok(());
        }
        info!("Disconnected from {:?}", self.rpc_client.url());
        self.connected = false;
        let pair = futures_util::future::join(
            async {
                self.block_sender
                    .send_async(BlockNotification::Disconnected)
                    .await
                    .context("block handler disconnect send failed")
            },
            async {
                self.vcc_sender
                    .send_async(RealTimeVccNotification::Disconnected)
                    .await
                    .context("vcc handler disconnect send failed")
            },
        )
        .await;
        pair.0.and(pair.1)?;

        // Unregister notifications
        self.unregister_notification_listener().await?;
        Ok(())
    }

    async fn unregister_notification_listener(&mut self) -> anyhow::Result<()> {
        if let Some(listener_id) = self.listener_id.take() {
            self.rpc_client.unregister_listener(listener_id).await?;
        }
        Ok(())
    }

    async fn handle_notification(&mut self, notification: Notification) -> anyhow::Result<()> {
        match notification {
            Notification::BlockAdded(BlockAddedNotification { block }) => {
                if self.block_sender_closed {
                    return Ok(());
                }
                self.block_sender
                    .send_async(BlockNotification::Notification(block))
                    .await
                    .context("block handler send `Notification` failed")?;
            }
            Notification::VirtualChainChanged(vcc) => {
                if self.vcc_sender_closed {
                    return Ok(());
                }
                self.vcc_sender
                    .send_async(RealTimeVccNotification::Notification(vcc))
                    .await
                    .context("vcc send failed")?;
            }
            Notification::VirtualDaaScoreChanged(daa) => {
                self.virtual_daa
                    .store(daa.virtual_daa_score, std::sync::atomic::Ordering::Relaxed);
            }
            _ => {
                warn!("unknown notification: {:?}", notification)
            }
        }
        Ok(())
    }

    async fn handle_shutdown(&mut self) -> anyhow::Result<()> {
        if self.shutting_down {
            return Ok(());
        };
        self.shutting_down = true;
        _ = self
            .rpc_client
            .disconnect()
            .await
            .inspect_err(|err| error!("Error disconnecting: {err}"));

        info!("Sending shutdown signals to block and VCC processors");

        debug!("Sending shutdown to block processor");
        self.block_sender
            .send_async(BlockNotification::Shutdown)
            .await
            .context("block handler `Shutdown` send failed")?;
        debug!("Sending shutdown to VCC processor");
        self.vcc_sender
            .send_async(RealTimeVccNotification::Shutdown)
            .await
            .context("vcc handler `Shutdown` send failed")?;
        info!("Shutdown signals sent to both processors successfully");
        Ok(())
    }

    /// A sender lookup that failed earlier is due again.
    async fn retry_sender(&mut self, retry: SenderRetry) {
        if self.shutting_down {
            return;
        }
        if !self.connected {
            // Queued like any request while disconnected; its count of tries starts over.
            self.requests_queue.push_back(Request::RequestSender {
                daa_score: retry.daa_score,
                tx_id: retry.tx_id,
            });
            return;
        }
        self.request_sender(retry.daa_score, retry.tx_id, retry.failed_tries)
            .await;
    }

    /// Ask the node who sent `tx_id` and hand the answer to the virtual chain processor.
    /// `failed_tries` is how many lookups of it already failed. A failure other than a disconnect
    /// (often "Transaction not found at given accepting daa score") is tried again later, see
    /// SENDER_RETRY_DELAYS. Past the last try the pending key stays in the database, so the next
    /// start asks again.
    async fn request_sender(&mut self, daa_score: u64, tx_id: [u8; 32], failed_tries: usize) {
        match self
            .rpc_client
            .rpc_client()
            .call(
                RpcApiOps::GetUtxoReturnAddress,
                Serializable(GetUtxoReturnAddressRequest::new(
                    TransactionId::from_bytes(tx_id),
                    daa_score,
                )),
            )
            .await
        {
            Ok(res) => {
                let res: Serializable<GetUtxoReturnAddressResponse> = res;
                let address = res.0.return_address;
                debug!("get sender");
                _ = self
                    .vcc_sender
                    .send_async(RealTimeVccNotification::SenderResolution {
                        sender: address,
                        tx_id,
                        daa: daa_score,
                    })
                    .await
                    .inspect_err(|_err| error!("sending sender result err"));
            }
            Err(workflow_rpc::client::error::Error::Disconnect) => {
                self.requests_queue
                    .push_front(Request::RequestSender { daa_score, tx_id });
            }
            Err(e) => {
                let tx = tx_id.to_hex_64();
                if self
                    .sender_retries
                    .schedule(Instant::now(), failed_tries, daa_score, tx_id)
                {
                    info!(%tx, daa_score, failed_tries = failed_tries + 1, "Error getting sender address, will retry: {e}");
                } else {
                    warn!(%tx, daa_score, failed_tries = failed_tries + 1, "Error getting sender address, giving up until the next restart: {e}");
                }
            }
        }
    }

    // todo rewrite rpc client so it doesnt block notifications when waits for response
    async fn handle_command(&mut self, command: Command) -> anyhow::Result<Continue> {
        match command {
            Command::MarkBlockSenderClosed => {
                info!("Block processor has closed");
                if !self.shutting_down {
                    warn!("MarkBlockSenderClosed called but not shutting down");
                }
                self.block_sender_closed = true;
                let continue_running = !self.vcc_sender_closed;
                if !continue_running {
                    info!("Both processors closed, data source can now terminate");
                } else {
                    info!("Block processor closed, waiting for VCC processor");
                    _ = self
                        .vcc_sender
                        .send_async(RealTimeVccNotification::Shutdown)
                        .await
                        .inspect_err(|_err| warn!("Error sending shutdown to VCC processor"));
                }
                Ok(continue_running)
            }
            Command::MarkVccSenderClosed => {
                info!("VCC processor has closed");
                if !self.shutting_down {
                    warn!("MarkVccSenderClosed called but not shutting down");
                }
                self.vcc_sender_closed = true;
                let continue_running = !self.block_sender_closed;
                if !continue_running {
                    info!("Both processors closed, data source can now terminate");
                } else {
                    info!("VCC processor closed, waiting for block processor");
                    _ = self
                        .block_sender
                        .send_async(BlockNotification::Shutdown)
                        .await
                        .inspect_err(|_err| warn!("Error sending shutdown to block processor"));
                }
                Ok(continue_running)
            }
            Command::Request(request) => {
                match request {
                    Request::RequestBlocks {
                        response_channel, ..
                    } if self.shutting_down => {
                        _ = response_channel
                            .send(Err(RequestError::ShuttingDown))
                            .inspect_err(|_err| {
                                error!("Error sending response to get blocks request")
                            });
                    }
                    Request::RequestSender { .. } if self.shutting_down => {
                        // do nothing
                        // _ = response_channel
                        //     .send(Err(RequestError::ShuttingDown))
                        //     .inspect_err(|_err| error!("Error sending response to sender request"));
                    }
                    Request::RequestVirtualChain {
                        response_channel, ..
                    } if self.shutting_down => {
                        _ = response_channel
                            .send(Err(RequestError::ShuttingDown))
                            .inspect_err(|_err| {
                                error!("Error sending response to virtual chain request")
                            });
                    }
                    request if !self.connected => {
                        self.requests_queue.push_back(request);
                    }
                    Request::RequestBlocks {
                        blocks_from,
                        response_channel,
                    } => {
                        match self
                            .rpc_client
                            .rpc_client()
                            .call(
                                RpcApiOps::GetBlocks,
                                Serializable(GetBlocksRequest::new(
                                    Some(RpcHash::from_bytes(blocks_from)),
                                    true,
                                    true,
                                )),
                            )
                            .await
                        {
                            Ok(res) => {
                                let res: Serializable<GetBlocksResponse> = res;
                                let blocks = res.0.blocks;
                                _ = response_channel
                                    .send(Ok(blocks))
                                    .inspect_err(|_err| error!("sending blocks result err"));
                            }
                            Err(workflow_rpc::client::error::Error::Disconnect) => {
                                self.requests_queue.push_front(Request::RequestBlocks {
                                    blocks_from,
                                    response_channel,
                                });
                            }
                            Err(e) => {
                                _ = response_channel
                                    .send(Err(e.into()))
                                    .inspect_err(|_err| error!("sending blocks result err"));
                            }
                        }
                    }
                    Request::RequestSender { daa_score, tx_id } => {
                        self.request_sender(daa_score, tx_id, 0).await;
                    }
                    Request::RequestVirtualChain {
                        vc_from,
                        response_channel,
                    } => {
                        match self
                            .rpc_client
                            .rpc_client()
                            .call(
                                RpcApiOps::GetVirtualChainFromBlock,
                                Serializable(GetVirtualChainFromBlockRequest::new(
                                    RpcHash::from_bytes(vc_from),
                                    true,
                                    None,
                                )),
                            )
                            .await
                        {
                            Ok(res) => {
                                let res: Serializable<GetVirtualChainFromBlockResponse> = res;
                                _ = response_channel
                                    .send(Ok(res.0))
                                    .inspect_err(|_err| error!("sending vcc result err"));
                            }
                            Err(workflow_rpc::client::error::Error::Disconnect) => {
                                self.requests_queue
                                    .push_front(Request::RequestVirtualChain {
                                        vc_from,
                                        response_channel,
                                    });
                            }
                            Err(e) => {
                                _ = response_channel
                                    .send(Err(e.into()))
                                    .inspect_err(|_err| error!("sending vcc result err"));
                            }
                        }
                    }
                }
                Ok(true)
            }
        }
    }
}

type Continue = bool;

/// How long to wait before each new try of a sender lookup that failed with anything but a
/// disconnect. After the last one it is dropped until the next start.
const SENDER_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SenderRetry {
    due: Instant,
    failed_tries: usize,
    daa_score: u64,
    tx_id: [u8; 32],
}

/// Failed sender lookups ordered by when they are due, so a retry never blocks other requests.
#[derive(Default)]
struct SenderRetryQueue(BinaryHeap<Reverse<SenderRetry>>);

impl SenderRetryQueue {
    /// Schedule the next try after `failed_tries` failed lookups (counting the one that just
    /// failed as failed_tries + 1). Returns false when no tries are left.
    fn schedule(&mut self, now: Instant, failed_tries: usize, daa_score: u64, tx_id: [u8; 32]) -> bool {
        let Some(delay) = SENDER_RETRY_DELAYS.get(failed_tries) else {
            return false;
        };
        self.0.push(Reverse(SenderRetry {
            due: now + *delay,
            failed_tries: failed_tries + 1,
            daa_score,
            tx_id,
        }));
        true
    }

    fn next_due(&self) -> Option<Instant> {
        self.0.peek().map(|Reverse(retry)| retry.due)
    }

    /// Every retry due at `now`, earliest first.
    fn pop_due(&mut self, now: Instant) -> Vec<SenderRetry> {
        let mut due = Vec::new();
        while let Some(Reverse(retry)) = self.0.peek()
            && retry.due <= now
        {
            due.push(*retry);
            self.0.pop();
        }
        due
    }
}

#[derive(Debug)]
pub enum Command {
    Request(Request),
    MarkBlockSenderClosed,
    MarkVccSenderClosed,
}

#[derive(Debug)]
pub enum Request {
    RequestBlocks {
        blocks_from: [u8; 32],
        response_channel: tokio::sync::oneshot::Sender<Result<Vec<RpcBlock>, RequestError>>,
    },
    RequestVirtualChain {
        vc_from: [u8; 32],
        response_channel:
            tokio::sync::oneshot::Sender<Result<GetVirtualChainFromBlockResponse, RequestError>>,
    },
    RequestSender {
        daa_score: u64,
        tx_id: [u8; 32],
    },
}

#[derive(Debug, Error)]
pub enum RequestError {
    #[error("Shutting down")]
    ShuttingDown,
    #[error("RPC error: {0}")]
    RpcError(#[from] workflow_rpc::client::error::Error),
}

#[cfg(test)]
mod sender_retry_tests {
    use super::*;

    #[test]
    fn a_failed_sender_lookup_is_retried_with_backoff_then_dropped() {
        let start = Instant::now();
        let mut queue = SenderRetryQueue::default();
        // Two lookups fail at once: the one with fewer failures is due first.
        assert!(queue.schedule(start, 3, 7, [2; 32]));
        assert!(queue.schedule(start, 0, 5, [1; 32]));
        assert_eq!(queue.next_due(), Some(start + Duration::from_secs(2)));
        assert!(queue.pop_due(start + Duration::from_secs(1)).is_empty());
        let due = queue.pop_due(start + Duration::from_secs(2));
        assert_eq!(due.len(), 1);
        assert_eq!((due[0].tx_id, due[0].daa_score, due[0].failed_tries), ([1; 32], 5, 1));
        assert_eq!(queue.next_due(), Some(start + Duration::from_secs(60)));

        // Each failure waits longer: 2 s, 5 s, 15 s, 60 s, 5 min, then the lookup is dropped.
        let mut queue = SenderRetryQueue::default();
        let mut now = start;
        let mut failed_tries = 0;
        let mut waits = Vec::new();
        while queue.schedule(now, failed_tries, 5, [1; 32]) {
            let next = queue.next_due().expect("scheduled");
            waits.push((next - now).as_secs());
            let retry = queue.pop_due(next).pop().expect("due");
            now = next;
            failed_tries = retry.failed_tries;
        }
        assert_eq!(waits, vec![2, 5, 15, 60, 300]);
        assert_eq!(failed_tries, 5);
        assert!(queue.next_due().is_none());
    }
}
