use crate::config::AppConfig;
use crate::database::{DbPool, Transaction, fetch_transaction};
use crate::k_protocol::KProtocolProcessor;
use anyhow::Result;
use futures::FutureExt;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub struct Worker {
    id: usize,
    receiver: mpsc::UnboundedReceiver<String>,
    db_pool: DbPool,
    config: AppConfig,
    k_processor: KProtocolProcessor,
}

impl Worker {
    pub fn new(
        id: usize,
        receiver: mpsc::UnboundedReceiver<String>,
        db_pool: DbPool,
        config: AppConfig,
    ) -> Self {
        let k_processor = KProtocolProcessor::new(db_pool.clone());
        Self {
            id,
            receiver,
            db_pool,
            config,
            k_processor,
        }
    }

    pub async fn start(mut self) {
        info!("Worker {} started", self.id);

        while let Some(transaction_id) = self.receiver.recv().await {
            //info!("Worker {} received notification for transaction: {}", self.id, transaction_id);

            // A panic while processing one transaction (a fetch, a retry, the K processing) is
            // caught, logged with the tx id and the transaction skipped: it must not take this
            // worker down, and every transaction routed to it with it.
            let work = std::panic::AssertUnwindSafe(self.process_transaction(transaction_id.clone()));
            if let Err(panic) = work.catch_unwind().await {
                error!(
                    "Worker {} - Panic while processing transaction {}, skipped: {}",
                    self.id,
                    transaction_id,
                    panic_message(panic.as_ref())
                );
            }
        }

        info!("Worker {} stopped", self.id);
    }

    async fn process_transaction(&self, transaction_id: String) {
        //info!("Worker {} processing transaction: {}", self.id, transaction_id);

        // Taken first, whatever happens next (not found, an error, the retries): a row the
        // startup catch-up re-queued stays quiet on the retry path too, and its entry never
        // lingers in the set.
        let catch_up = crate::reindex::take_catch_up(&transaction_id);

        match self.fetch_and_process_transaction(&transaction_id).await {
            Ok(Some(transaction)) => self.process_fetched(&transaction, catch_up).await,
            Ok(None) => {
                warn!(
                    "Worker {} - Transaction {} not found in database",
                    self.id, transaction_id
                );
            }
            Err(e) => {
                error!(
                    "Worker {} - Error processing transaction {}: {}",
                    self.id, transaction_id, e
                );

                if let Err(retry_err) = self.retry_transaction(&transaction_id, catch_up).await {
                    error!(
                        "Worker {} - Failed to retry transaction {}: {}",
                        self.id, transaction_id, retry_err
                    );
                }
            }
        }
    }

    /// One fetched transaction, first try or retry. KaChat payloads only: canonical `kchat:1:`
    /// (posts + broadcasts) and the pre-rebrand KaChat broadcast prefix `ciph_msg:1:bcast:`.
    /// Legacy `k:1:` is the separate K-social network, not KaChat, so it is deliberately
    /// excluded.
    async fn process_fetched(&self, transaction: &Transaction, catch_up: bool) {
        let transaction_id = &transaction.transaction_id;
        let Some(payload_str) = transaction
            .payload
            .as_deref()
            .and_then(|payload_hex| hex::decode(payload_hex).ok())
            .and_then(|bytes| String::from_utf8(bytes).ok())
        else {
            return;
        };
        if !payload_str.starts_with("kchat:1:") && !payload_str.starts_with("ciph_msg:1:bcast:") {
            info!(
                "Worker {} - Transaction {} does not contain K protocol data",
                self.id, transaction_id
            );
            self.mark_recovered_consumed(transaction_id).await;
            return;
        }
        //info!("Worker {} - Processing K protocol / broadcast transaction: {}", self.id, transaction_id);
        // Pushes stay quiet for re-queued rows: one a reindex recovered (history) or one the
        // startup catch-up re-queued (it may have been pushed before the restart). And for the
        // ingester's backlog after an outage: a transaction more than
        // `BACKLOG_PUSH_MAX_AGE_MS` old is stored but is not news. Within that, a live
        // transaction pushes however far behind we are (a tighter cut-off silenced every like
        // while the processor lagged).
        let age_ms = transaction
            .block_time
            .map(|t| crate::bcast_pending::now_ms() - t);
        let backlog = age_ms.is_some_and(|age| age > crate::push_notify::BACKLOG_PUSH_MAX_AGE_MS);
        let quiet = transaction.recovered || catch_up || backlog;
        if quiet {
            info!(
                "Worker {} - tx {} processed quiet (no pushes): {}, {}s old",
                self.id,
                transaction_id,
                if transaction.recovered {
                    "recovered by a reindex"
                } else if catch_up {
                    "startup catch-up"
                } else {
                    "backlog"
                },
                age_ms.map(|age| age / 1000).unwrap_or(-1)
            );
        }
        match crate::push_notify::QUIET
            .scope(quiet, self.k_processor.process_k_transaction(transaction))
            .await
        {
            Ok(()) => self.mark_recovered_consumed(transaction_id).await,
            Err(k_err) => error!(
                "Worker {} - Error processing K protocol transaction {}: {}",
                self.id, transaction_id, k_err
            ),
        }
    }

    /// A reindex-recovered row is done once processed without error: the next start does not
    /// queue it again, and the prune may drop it (kachat-audits IDX-038). Live transactions
    /// have no such row; the update touches nothing.
    async fn mark_recovered_consumed(&self, transaction_id: &str) {
        let Ok(id) = hex::decode(transaction_id) else { return };
        if let Err(e) = sqlx::query(
            "UPDATE kachat_recovered_transactions SET consumed_at = $2 WHERE transaction_id = $1 AND consumed_at IS NULL",
        )
        .bind(&id)
        .bind(crate::bcast_pending::now_ms())
        .execute(&self.db_pool)
        .await
        {
            warn!("Worker {} - could not mark recovered transaction {} processed: {}", self.id, transaction_id, e);
        }
    }

    async fn fetch_and_process_transaction(
        &self,
        transaction_id: &str,
    ) -> Result<Option<Transaction>> {
        //info!("Worker {} received transaction data for processing: {}", self.id, transaction_id);

        fetch_transaction(&self.db_pool, transaction_id).await
    }

    /*
    fn log_transaction(&self, transaction: &Transaction) {
        info!("=== Transaction Processed by Worker {} ===", self.id);
        //info!("Worker {} - Transaction ID (hex): {}", self.id, transaction.transaction_id);
        //info!("Worker {} - Subnetwork ID: {:?}", self.id, transaction.subnetwork_id);
        //info!("Worker {} - Hash (hex): {:?}", self.id, transaction.hash);
        //info!("Worker {} - Mass: {:?}", self.id, transaction.mass);

        // Format payload with hex prefix and check if it starts with 6b3a

        if let Some(ref payload) = transaction.payload {
            info!("Worker {} - Payload (hex): 0x{}", self.id, payload);
            if payload.starts_with("6b3a") {
                info!("Worker {} - Payload starts with 6b3a ✓ (trigger condition met)", self.id);
            }
            info!("Worker {} - Payload length: {} bytes", self.id, payload.len() / 2);
        } else {
            info!("Worker {} - Payload (hex): None", self.id);
        }

        info!("Worker {} - Block Time: {:?}", self.id, transaction.block_time);
        info!("Worker {} - ==========================================", self.id);

    }*/

    /// Fetch again after a fetch error, `retry_attempts` times. A row that comes back is
    /// processed exactly like a first try (`process_fetched`): the same KaChat gate, the same
    /// quiet rules (`catch_up` is what the first try took from the catch-up set), panics caught.
    async fn retry_transaction(&self, transaction_id: &str, catch_up: bool) -> Result<()> {
        for attempt in 1..=self.config.processing.retry_attempts {
            warn!(
                "Worker {} - Retry attempt {} for transaction {}",
                self.id, attempt, transaction_id
            );

            tokio::time::sleep(tokio::time::Duration::from_millis(
                self.config.processing.retry_delay_ms,
            ))
            .await;

            match self.fetch_and_process_transaction(transaction_id).await {
                Ok(Some(transaction)) => {
                    info!(
                        "Worker {} - Retry successful for transaction {}",
                        self.id, transaction_id
                    );
                    self.process_fetched(&transaction, catch_up).await;
                    return Ok(());
                }
                Ok(None) => {
                    warn!(
                        "Worker {} - Transaction {} still not found on retry {}",
                        self.id, transaction_id, attempt
                    );
                }
                Err(e) => {
                    error!(
                        "Worker {} - Retry {} failed for transaction {}: {}",
                        self.id, attempt, transaction_id, e
                    );
                }
            }
        }

        error!(
            "Worker {} - All retry attempts exhausted for transaction {}",
            self.id, transaction_id
        );
        Ok(())
    }
}

/// The text of a caught panic: what `panic!` was given, when it was a string.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-string panic".to_string())
}

pub struct WorkerPool {
    workers: Vec<Worker>,
}

impl WorkerPool {
    pub fn new(
        worker_receivers: Vec<mpsc::UnboundedReceiver<String>>,
        db_pool: DbPool,
        config: AppConfig,
    ) -> Self {
        let workers = worker_receivers
            .into_iter()
            .enumerate()
            .map(|(id, receiver)| Worker::new(id, receiver, db_pool.clone(), config.clone()))
            .collect();

        Self { workers }
    }

    pub async fn start(self) {
        info!("Starting worker pool with {} workers", self.workers.len());

        let mut handles = Vec::new();

        for worker in self.workers {
            let handle = tokio::spawn(async move {
                worker.start().await;
            });
            handles.push(handle);
        }

        for handle in handles {
            if let Err(e) = handle.await {
                error!("Worker task failed: {}", e);
            }
        }

        info!("Worker pool stopped");
    }
}

#[cfg(test)]
mod tests {
    use futures::FutureExt;

    #[tokio::test]
    async fn a_panicking_transaction_is_caught_and_named() {
        let caught = std::panic::AssertUnwindSafe(async { panic!("bad payload {}", 7) })
            .catch_unwind()
            .await;
        let panic = caught.expect_err("the panic is caught");
        assert_eq!(super::panic_message(panic.as_ref()), "bad payload 7");
        let caught = std::panic::AssertUnwindSafe(async { std::panic::panic_any(42u8) }).catch_unwind().await;
        assert_eq!(super::panic_message(caught.unwrap_err().as_ref()), "a non-string panic");
        // The task-local quiet flag is unwound with it: the next transaction is not quiet.
        let quiet = crate::push_notify::QUIET.scope(true, async { panic!("inside quiet") });
        assert!(std::panic::AssertUnwindSafe(quiet).catch_unwind().await.is_err());
        assert!(crate::push_notify::QUIET.try_with(|q| *q).is_err());
    }
}
