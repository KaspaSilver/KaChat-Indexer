//! Cached answers that are expensive to compute (the game leaderboards, the Explorer), refreshed
//! by one caller at a time.
//!
//! - A fresh value is served as is. A stale one is served while another caller refreshes it.
//! - The refresh runs in its own task, so a caller that goes away does not cancel it halfway.
//! - A failed refresh is never cached: the last good value stays, and with none there is
//!   nothing to serve (the handler answers 503).
//!
//! `RowMemo` keeps the leaderboards incremental: each room row is judged once (its stamp is
//! immutable), and a refresh only reads the rows added since the last one.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::OwnedMutexGuard;

use crate::chess::ArenaRow;

/// A refresh: computes the new value with the cache's refresh state (`S`) in hand.
pub type Refresh<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

pub struct SnapshotCache<T, S = ()> {
    value: std::sync::Mutex<Option<(Instant, T)>>,
    /// Held for the whole refresh: this is the single flight. Also guards the state a refresh
    /// carries over to the next (a `RowMemo`).
    refresh: Arc<tokio::sync::Mutex<S>>,
}

enum Lookup<T, S> {
    Ready(T),
    /// This caller refreshes, with the refresh lock held and the stale value, if any.
    Refresh(OwnedMutexGuard<S>, Option<T>),
}

impl<T: Clone + Send + 'static, S: Send + 'static> SnapshotCache<T, S> {
    pub fn new(state: S) -> Self {
        SnapshotCache { value: std::sync::Mutex::new(None), refresh: Arc::new(tokio::sync::Mutex::new(state)) }
    }

    fn current(&self) -> Option<(Instant, T)> {
        self.value.lock().ok()?.clone()
    }

    fn store(&self, value: T) {
        if let Ok(mut guard) = self.value.lock() {
            *guard = Some((Instant::now(), value));
        }
    }

    async fn lookup(&self, ttl: Duration) -> Lookup<T, S> {
        let current = self.current();
        if let Some((at, value)) = &current
            && at.elapsed() < ttl
        {
            return Lookup::Ready(value.clone());
        }
        let guard = match self.refresh.clone().try_lock_owned() {
            Ok(guard) => guard,
            // Someone is refreshing: serve the stale value, or wait for theirs when there is none.
            Err(_) => match current {
                Some((_, value)) => return Lookup::Ready(value),
                None => self.refresh.clone().lock_owned().await,
            },
        };
        // A refresh may have finished between the read and the lock.
        match self.current() {
            Some((at, value)) if at.elapsed() < ttl => Lookup::Ready(value),
            latest => Lookup::Refresh(guard, latest.map(|(_, value)| value)),
        }
    }

    /// The value, refreshed by `refresh` when older than `ttl`. `None` only when the refresh
    /// failed and there is no earlier value to serve.
    pub async fn get<F>(self: &Arc<Self>, ttl: Duration, what: &'static str, refresh: F) -> Option<T>
    where
        F: for<'a> FnOnce(&'a mut S) -> Refresh<'a, T> + Send + 'static,
    {
        let (mut guard, stale) = match self.lookup(ttl).await {
            Lookup::Ready(value) => return Some(value),
            Lookup::Refresh(guard, stale) => (guard, stale),
        };
        let cache = Arc::clone(self);
        let task = tokio::spawn(async move {
            match refresh(&mut *guard).await {
                Ok(value) => {
                    // Stored before the lock is released.
                    cache.store(value.clone());
                    Some(value)
                }
                Err(e) => {
                    tracing::error!("{what}: refresh failed (not cached): {e}");
                    None
                }
            }
        });
        match task.await {
            Ok(Some(value)) => Some(value),
            Ok(None) => stale,
            Err(e) => {
                tracing::error!("{what}: refresh task failed: {e}");
                stale
            }
        }
    }
}

/// How far back each refresh re-reads by insertion id: a row whose transaction committed after
/// a later id's is still picked up (deduped by tx id).
const ID_OVERLAP: i64 = 512;

/// Every judged row of one room: its verdict by tx id (`None` = not a counted row), and the
/// highest insertion id read.
pub struct RowMemo<V> {
    verdicts: HashMap<String, Option<V>>,
    last_id: i64,
}

impl<V> Default for RowMemo<V> {
    fn default() -> Self {
        RowMemo { verdicts: HashMap::new(), last_id: 0 }
    }
}

impl<V> RowMemo<V> {
    /// Read rows with an insertion id above this.
    pub fn fetch_after(&self) -> i64 {
        (self.last_id - ID_OVERLAP).max(0)
    }

    /// The highest insertion id read: every judged row's id is at or below it.
    pub fn last_id(&self) -> i64 {
        self.last_id
    }

    pub fn len(&self) -> usize {
        self.verdicts.len()
    }

    /// The rows not judged yet, deduped by tx id. Moves the read cursor past every row given.
    pub fn unseen(&mut self, rows: Vec<(i64, ArenaRow)>) -> Vec<ArenaRow> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for (id, row) in rows {
            self.last_id = self.last_id.max(id);
            if !self.verdicts.contains_key(&row.tx_id) && seen.insert(row.tx_id.clone()) {
                out.push(row);
            }
        }
        out
    }

    /// Of the tx ids stored now, those not judged yet.
    pub fn missing<'a>(&self, stored: &'a [String]) -> Vec<&'a String> {
        stored.iter().filter(|tx| !self.verdicts.contains_key(*tx)).collect()
    }

    /// Forget rows that are no longer stored (deleted by an admin or a blocklist purge).
    pub fn keep_only(&mut self, stored: &[String]) {
        let stored: HashSet<&str> = stored.iter().map(String::as_str).collect();
        self.verdicts.retain(|tx, _| stored.contains(tx.as_str()));
    }

    pub fn judge(&mut self, rows: Vec<ArenaRow>, verdict: impl Fn(&ArenaRow) -> Option<V>) {
        for row in rows {
            let v = verdict(&row);
            self.verdicts.insert(row.tx_id, v);
        }
    }

    /// Every counted row, as (tx id, verdict).
    pub fn counted(&self) -> impl Iterator<Item = (&str, &V)> {
        self.verdicts.iter().filter_map(|(tx, v)| v.as_ref().map(|v| (tx.as_str(), v)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(tx: &str, content: &str) -> ArenaRow {
        ArenaRow { tx_id: tx.into(), sender: "kaspa:a".into(), block_time: 1, content: content.into() }
    }

    #[test]
    fn rows_are_judged_once_and_the_cursor_overlaps() {
        let mut memo: RowMemo<usize> = RowMemo::default();
        assert_eq!(memo.fetch_after(), 0);
        let fresh = memo.unseen(vec![(1, row("a", "x")), (2, row("b", "yy")), (2, row("b", "yy"))]);
        assert_eq!(fresh.len(), 2);
        memo.judge(fresh, |r| (r.content.len() > 1).then_some(r.content.len()));
        assert_eq!(memo.len(), 2);
        assert_eq!(memo.counted().collect::<Vec<_>>(), vec![("b", &2)]);
        // Re-read through the overlap: known rows are not judged again; a late one is.
        let fresh = memo.unseen(vec![(1, row("a", "x")), (2, row("b", "yy")), (1_000, row("c", "zzz"))]);
        assert_eq!(fresh.iter().map(|r| r.tx_id.as_str()).collect::<Vec<_>>(), vec!["c"]);
        assert_eq!(memo.fetch_after(), 1_000 - ID_OVERLAP);
    }

    #[test]
    fn deleted_rows_are_forgotten() {
        let mut memo: RowMemo<()> = RowMemo::default();
        let fresh = memo.unseen(vec![(1, row("a", "")), (2, row("b", ""))]);
        memo.judge(fresh, |_| Some(()));
        let stored = vec!["b".to_string(), "c".to_string()];
        assert_eq!(memo.missing(&stored), vec![&"c".to_string()]);
        memo.keep_only(&stored);
        assert_eq!(memo.counted().map(|(tx, _)| tx).collect::<Vec<_>>(), vec!["b"]);
    }

    #[tokio::test]
    async fn a_failed_refresh_is_not_cached() {
        let cache: Arc<SnapshotCache<u32, u32>> = Arc::new(SnapshotCache::new(0));
        let ttl = Duration::from_secs(60);
        // Nothing yet and the refresh fails: nothing to serve.
        assert_eq!(cache.get(ttl, "test", |_| Box::pin(async { Err("db down".to_string()) })).await, None);
        // It succeeds: cached, and the refresh state carries over.
        let got = cache.get(ttl, "test", |n| Box::pin(async move { *n += 1; Ok(7) })).await;
        assert_eq!(got, Some(7));
        assert_eq!(cache.get(ttl, "test", |_| Box::pin(async { Ok(8) })).await, Some(7));
        // Stale, and the refresh fails: the last good value.
        let stale: Arc<SnapshotCache<u32, u32>> = Arc::new(SnapshotCache::new(0));
        stale.get(Duration::ZERO, "test", |_| Box::pin(async { Ok(1) })).await;
        assert_eq!(stale.get(Duration::ZERO, "test", |_| Box::pin(async { Err("db down".to_string()) })).await, Some(1));
        assert_eq!(stale.get(Duration::ZERO, "test", |n| Box::pin(async move { Ok(*n + 2) })).await, Some(2));
    }

    #[tokio::test]
    async fn one_refresh_at_a_time_serves_the_stale_value() {
        let cache: Arc<SnapshotCache<u32>> = Arc::new(SnapshotCache::new(()));
        cache.get(Duration::ZERO, "test", |_| Box::pin(async { Ok(1) })).await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let slow = {
            let cache = cache.clone();
            tokio::spawn(async move {
                cache
                    .get(Duration::ZERO, "test", move |_| {
                        Box::pin(async move {
                            let _ = started_tx.send(());
                            let _ = release_rx.await;
                            Ok(2)
                        })
                    })
                    .await
            })
        };
        started_rx.await.unwrap();
        // While that refresh runs, another caller gets the stale value without refreshing.
        let other = cache.get(Duration::ZERO, "test", |_| Box::pin(async { Ok(99) })).await;
        assert_eq!(other, Some(1));
        release_tx.send(()).unwrap();
        assert_eq!(slow.await.unwrap(), Some(2));
    }
}
