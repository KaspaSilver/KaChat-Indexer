//! K Explorer (EXPLORER_STATS.md §2-§3): `/stats/activity?period=` and `/network/fees?period=`.
//!
//! Periods share one bucket layout: `day` = the last 24 hours, `week` and `month` = the last 7 and
//! 30 UTC days, `all` = every calendar month with data. The last bucket is the one still filling.
//! Counts come from the KaPosts and room tables, and from the chat indexer's hourly and monthly
//! counts (`/stats/buckets`). Fees and senders come from the hourly totals the profiles follower
//! keeps from the node's accepted transactions (`kachat_fee_hours`, `kachat_k_hours`,
//! `kachat_k_senders`), so they start the day that follower first counted them.

use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;
use sqlx::Row;

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

/// The chat indexer's categories (`/stats` keys).
const CHAT_CATEGORIES: [&str; 6] = ["messages", "handshakes", "payments", "groupMessages", "groupUpdates", "selfStash"];

/// Categories the Explorer card leaves out on purpose (machinery): counted, but not in `series`.
const NOT_IN_SERIES: [&str; 2] = ["groupUpdates", "selfStash"];

/// KaPosts, room and KaPlay events, one row per counted transaction: (category, block time).
/// The same definitions as `/stats`.
const K_EVENTS: &str = "\
    SELECT 'kaposts' c, block_time bt FROM k_contents WHERE content_type IN ('post','reply','quote','poll') \
    UNION ALL SELECT 'kapostActions', block_time FROM k_votes \
    UNION ALL SELECT 'kapostActions', block_time FROM k_follows \
    UNION ALL SELECT 'kapostActions', block_time FROM k_edits \
    UNION ALL SELECT 'kapostActions', block_time FROM k_deletes \
    UNION ALL SELECT 'kapostActions', block_time FROM k_poll_votes \
    UNION ALL SELECT 'publicChats', block_time FROM kachat_broadcasts WHERE channel NOT IN ('chess-arena', 'chess-bots', 'kplay-arcade') \
    UNION ALL SELECT 'chessMoves', block_time FROM kachat_broadcasts WHERE channel = 'chess-arena' AND content LIKE '%\"a\":\"move\"%' \
    UNION ALL SELECT 'arcadeScores', block_time FROM kachat_broadcasts WHERE channel = 'kplay-arcade' AND content LIKE '%\"type\":\"arcade\"%'";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Period {
    Day,
    Week,
    Month,
    All,
}

impl Period {
    pub fn parse(value: Option<&str>) -> Option<Period> {
        match value.unwrap_or("day").trim() {
            "day" => Some(Period::Day),
            "week" => Some(Period::Week),
            "month" => Some(Period::Month),
            "all" => Some(Period::All),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Period::Day => "day",
            Period::Week => "week",
            Period::Month => "month",
            Period::All => "all",
        }
    }

    /// (bucket length, buckets) for the fixed-length periods.
    fn fixed(self) -> Option<(i64, i64)> {
        match self {
            Period::Day => Some((HOUR_MS, 24)),
            Period::Week => Some((DAY_MS, 7)),
            Period::Month => Some((DAY_MS, 30)),
            Period::All => None,
        }
    }
}

/// The start (ms UTC) of the calendar month holding `ms`.
pub fn month_start_ms(ms: i64) -> i64 {
    let days = ms.div_euclid(DAY_MS);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let first_doy = (153 * mp + 2) / 5;
    (era * 146_097 + 365 * yoe + yoe / 4 - yoe / 100 + first_doy - 719_468) * DAY_MS
}

/// The start of the month after the one starting at `month`.
fn next_month_ms(month: i64) -> i64 {
    month_start_ms(month + 32 * DAY_MS)
}

/// A period's buckets: their starts (oldest first), and the window before them for `previous`.
#[derive(Debug, PartialEq)]
pub struct Layout {
    pub starts: Vec<i64>,
    /// `[from, to)` of the period: `from` = the first bucket's start, `to` = now.
    pub from: i64,
    pub to: i64,
    /// Start of the previous window (`[previous_from, from)`), for fixed-length periods.
    pub previous_from: Option<i64>,
    bucket_ms: Option<i64>,
}

impl Layout {
    /// `earliest` = the oldest data point, where `all` starts.
    pub fn new(period: Period, now: i64, earliest: Option<i64>) -> Layout {
        match period.fixed() {
            Some((size, n)) => {
                let last = now.div_euclid(size) * size;
                let from = last - (n - 1) * size;
                Layout {
                    starts: (0..n).map(|i| from + i * size).collect(),
                    from,
                    to: now,
                    // The window before is as long as this one (`now - from`, the last bucket being
                    // partly filled), so a flat rate reads as no change.
                    previous_from: Some(from - (now - from)),
                    bucket_ms: Some(size),
                }
            }
            None => {
                let last = month_start_ms(now);
                let mut month = month_start_ms(earliest.unwrap_or(now).min(now));
                let mut starts = Vec::new();
                while month <= last {
                    starts.push(month);
                    month = next_month_ms(month);
                }
                Layout { from: starts[0], starts, to: now, previous_from: None, bucket_ms: None }
            }
        }
    }

    /// The bucket a block time falls in: Some(start) inside the period, None outside it.
    pub fn bucket_of(&self, ms: i64) -> Option<i64> {
        if ms < self.from || ms >= self.to {
            return None;
        }
        Some(match self.bucket_ms {
            Some(size) => ms.div_euclid(size) * size,
            None => month_start_ms(ms),
        })
    }

    /// Whether a block time is in the window before the period.
    pub fn in_previous(&self, ms: i64) -> bool {
        self.previous_from.is_some_and(|p| ms >= p && ms < self.from)
    }
}

/// The chat indexer's `/stats/buckets` answer.
#[derive(Deserialize, Default)]
pub struct ChatBuckets {
    #[serde(default)]
    hours: HashMap<String, BTreeMap<i64, i64>>,
    #[serde(default)]
    months: HashMap<String, BTreeMap<i64, i64>>,
}

/// Counts for one period, by category: in the period, in the window before, and per bucket.
#[derive(Default)]
struct Counts {
    current: BTreeMap<String, i64>,
    previous: BTreeMap<String, i64>,
    series: BTreeMap<i64, i64>,
}

impl Counts {
    fn add(&mut self, layout: &Layout, category: &str, at: i64, n: i64) {
        if let Some(bucket) = layout.bucket_of(at) {
            *self.current.entry(category.to_string()).or_default() += n;
            if !NOT_IN_SERIES.contains(&category) {
                *self.series.entry(bucket).or_default() += n;
            }
        } else if layout.in_previous(at) {
            *self.previous.entry(category.to_string()).or_default() += n;
        }
    }
}

/// GET /stats/activity body for one period. A database error is an error, never a body with
/// counts missing (the webserver serves its last good answer instead).
pub async fn activity(
    pool: &sqlx::PgPool,
    chat: Option<ChatBuckets>,
    period: Period,
    now: i64,
) -> Result<serde_json::Value, sqlx::Error> {
    let chat = chat.unwrap_or_default();
    // `all` starts at the oldest month anything was counted in.
    let earliest = if period == Period::All {
        let k_min: Option<i64> = sqlx::query_scalar(&format!("SELECT min(bt) FROM ({K_EVENTS}) e"))
            .fetch_one(pool)
            .await?;
        let chat_min = chat.months.values().filter_map(|m| m.keys().next().copied()).min();
        k_min.into_iter().chain(chat_min).min()
    } else {
        None
    };
    let layout = Layout::new(period, now, earliest);
    let range_from = layout.previous_from.unwrap_or(layout.from);

    let mut counts = Counts::default();
    // KaPosts, rooms and KaPlay, grouped by hour (every bucket is a whole number of hours).
    let sql = format!(
        "SELECT c, (bt / {HOUR_MS}) * {HOUR_MS} AS h, count(*)::BIGINT n FROM ({K_EVENTS}) e \
         WHERE bt >= $1 AND bt < $2 GROUP BY 1, 2"
    );
    for r in sqlx::query(&sql).bind(range_from).bind(layout.to).fetch_all(pool).await? {
        counts.add(&layout, r.get::<String, _>("c").as_str(), r.get("h"), r.get("n"));
    }
    // Chats: hourly counts reach 60 days back, enough for a month and the month before it.
    let chat_counts = if period == Period::All { &chat.months } else { &chat.hours };
    for category in CHAT_CATEGORIES {
        for (&at, &n) in chat_counts.get(category).into_iter().flatten() {
            counts.add(&layout, category, at, n);
        }
    }

    let hour_from = layout.from.div_euclid(HOUR_MS) * HOUR_MS;
    let active: i64 =
        sqlx::query_scalar("SELECT count(DISTINCT sender)::BIGINT FROM kachat_k_senders WHERE hour >= $1 AND hour < $2")
            .bind(hour_from)
            .bind(layout.to)
            .fetch_one(pool)
            .await?;
    let k_fees = sqlx::query(
        "SELECT COALESCE(sum(fees_sompi), 0)::BIGINT f, (SELECT min(hour) FROM kachat_k_hours) s \
         FROM kachat_k_hours WHERE hour >= $1 AND hour < $2",
    )
    .bind(hour_from)
    .bind(layout.to)
    .fetch_one(pool)
    .await?;

    let mut out = serde_json::json!({
        "period": period.name(),
        "from": layout.from,
        "to": layout.to,
        "categories": counts.current,
        "series": layout.starts.iter().map(|t| serde_json::json!({ "t": t, "count": counts.series.get(t).copied().unwrap_or(0) })).collect::<Vec<_>>(),
    });
    if period != Period::All {
        out["previous"] = serde_json::json!(counts.previous);
    }
    // Senders and fees exist from the first hour the profiles follower counted (`feesSince`).
    if let Ok(Some(since)) = k_fees.try_get::<Option<i64>, _>("s") {
        out["feesSompi"] = serde_json::json!(k_fees.get::<i64, _>("f"));
        out["feesSince"] = serde_json::json!(since);
        out["activeAddresses"] = serde_json::json!(active);
    }
    Ok(out)
}

/// GET /network/fees body for one period. None until any fee has been counted; a database
/// error is an error.
pub async fn network_fees(pool: &sqlx::PgPool, period: Period, now: i64) -> Result<Option<serde_json::Value>, sqlx::Error> {
    let rows = sqlx::query("SELECT hour, fees_sompi, transactions FROM kachat_fee_hours ORDER BY hour")
        .fetch_all(pool)
        .await?;
    let hours: Vec<(i64, i64, i64)> =
        rows.iter().map(|r| (r.get("hour"), r.get("fees_sompi"), r.get("transactions"))).collect();
    let Some(since) = hours.first().map(|h| h.0) else {
        return Ok(None);
    };
    let layout = Layout::new(period, now, Some(since));
    let mut buckets: BTreeMap<i64, (i64, i64)> = layout.starts.iter().map(|&t| (t, (0, 0))).collect();
    for &(hour, fees, txs) in &hours {
        if let Some(b) = layout.bucket_of(hour).and_then(|t| buckets.get_mut(&t)) {
            b.0 += fees;
            b.1 += txs;
        }
    }
    // The last 24 hourly buckets, the current one included.
    let day_from = now.div_euclid(HOUR_MS) * HOUR_MS - 23 * HOUR_MS;
    let (fees_24h, txs_24h) = hours
        .iter()
        .filter(|(hour, ..)| *hour >= day_from)
        .fold((0i64, 0i64), |(f, t), &(_, fees, txs)| (f + fees, t + txs));
    let mut out = serde_json::json!({
        "period": period.name(),
        "since": since,
        "buckets": buckets.iter().map(|(t, (fees, txs))| serde_json::json!({ "t": t, "feesSompi": fees, "transactions": txs })).collect::<Vec<_>>(),
    });
    if txs_24h > 0 {
        out["averageFeeSompi24h"] = serde_json::json!(fees_24h / txs_24h);
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-25 15:30 UTC
    const NOW: i64 = 1_795_620_600_000;

    #[test]
    fn a_day_is_24_hours_ending_with_the_one_filling() {
        let l = Layout::new(Period::Day, NOW, None);
        assert_eq!(l.starts.len(), 24);
        assert_eq!(*l.starts.last().unwrap(), 1_795_618_800_000); // 15:00
        assert_eq!(l.from, 1_795_618_800_000 - 23 * HOUR_MS);
        // 23.5 h so far, against the 23.5 h before them.
        assert_eq!(l.previous_from, Some(l.from - (NOW - l.from)));
        assert_eq!(NOW - l.from, 23 * HOUR_MS + HOUR_MS / 2);
        assert_eq!(l.bucket_of(NOW - 1), Some(1_795_618_800_000));
        assert_eq!(l.bucket_of(l.from - 1), None);
        assert!(l.in_previous(l.from - 1));
        assert!(!l.in_previous(l.from - (NOW - l.from) - 1));
    }

    #[test]
    fn a_month_is_30_days_and_all_is_calendar_months() {
        let l = Layout::new(Period::Month, NOW, None);
        assert_eq!(l.starts.len(), 30);
        assert_eq!(*l.starts.last().unwrap(), NOW.div_euclid(DAY_MS) * DAY_MS);
        // Data since 2026-09-15 -> September, October, November.
        let l = Layout::new(Period::All, NOW, Some(1_789_430_400_000));
        assert_eq!(l.starts, vec![1_788_220_800_000, 1_790_812_800_000, 1_793_491_200_000]);
        assert_eq!(l.bucket_of(1_791_000_000_000), Some(1_790_812_800_000));
        assert_eq!(l.previous_from, None);
    }

    #[test]
    fn month_starts_roll_over_the_year() {
        assert_eq!(month_start_ms(1_767_225_599_999), 1_764_547_200_000); // 2025-12-31 -> Dec 1
        assert_eq!(next_month_ms(1_764_547_200_000), 1_767_225_600_000); // -> 2026-01-01
    }

    #[test]
    fn machinery_is_counted_but_not_charted() {
        let l = Layout::new(Period::Day, NOW, None);
        let mut c = Counts::default();
        c.add(&l, "messages", NOW - 10, 3);
        c.add(&l, "selfStash", NOW - 10, 5);
        c.add(&l, "messages", l.from - 10, 2);
        assert_eq!(c.current["messages"], 3);
        assert_eq!(c.current["selfStash"], 5);
        assert_eq!(c.previous["messages"], 2);
        assert_eq!(c.series.values().sum::<i64>(), 3);
    }
}
