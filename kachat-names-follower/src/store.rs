//! Postgres persistence for the registry (KACHAT_NAMES_INDEXER.md §B5 / app contract §2).
//!
//! The registry's live set is small (one row per gap / name / offer UTXO), so after any batch
//! that changed it the whole set is rewritten in one transaction together with the checkpoint;
//! a crash between batches resumes from the last committed checkpoint with matching rows.
//! Index names deliberately avoid `idx_k_` (the KaPosts schema verifier counts those).

use std::collections::HashMap;

use anyhow::Result;
use kachat_names::ingest::{Event, Outpoint, Registry, Tracked};
use kachat_names::{GapState, NameState, OfferState, PriceState};
use sqlx::{PgPool, Row};

/// What the engine doesn't keep about a tracked UTXO but the API serves: its value and when
/// it was created.
#[derive(Debug, Clone, Copy)]
pub struct UtxoMeta {
    pub value: u64,
    pub created_at: i64,
    pub created_daa: u64,
}

/// Sync status row (one per database).
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub checkpoint: Option<[u8; 32]>,
    pub indexed_daa: u64,
    pub virtual_daa: u64,
    pub synced: bool,
    pub self_test_ok: bool,
    pub self_test_at: i64,
    /// Why the follower cannot progress (`start_block_pruned`), for /names/status.
    pub fatal_reason: Option<String>,
    /// The block it cannot start from.
    pub start_block: Option<[u8; 32]>,
}

pub async fn create_schema(pool: &PgPool) -> Result<()> {
    for stmt in [
        r#"CREATE TABLE IF NOT EXISTS names_state (
            id SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
            registry_covenant_id TEXT NOT NULL,
            network TEXT,
            genesis_txid TEXT,
            checkpoint BYTEA,
            indexed_daa BIGINT NOT NULL DEFAULT 0,
            virtual_daa BIGINT NOT NULL DEFAULT 0,
            synced BOOLEAN NOT NULL DEFAULT FALSE,
            self_test_ok BOOLEAN NOT NULL DEFAULT FALSE,
            self_test_at BIGINT NOT NULL DEFAULT 0,
            grace_ms BIGINT NOT NULL DEFAULT 0,
            updated_at BIGINT NOT NULL DEFAULT 0
        )"#,
        r#"CREATE TABLE IF NOT EXISTS names_utxos (
            txid BYTEA NOT NULL,
            idx INTEGER NOT NULL,
            kind TEXT NOT NULL,
            value BIGINT NOT NULL,
            lo BYTEA, hi BYTEA,
            key BYTEA, name TEXT, owner BYTEA, price BIGINT, period_start BIGINT, expires_at BIGINT,
            buyer BYTEA, refund_after BIGINT,
            created_at BIGINT NOT NULL,
            created_daa BIGINT NOT NULL,
            refuted BOOLEAN NOT NULL DEFAULT FALSE,
            PRIMARY KEY (txid, idx)
        )"#,
        // Registry v2 added periodStart; tables created before it get the column here.
        "ALTER TABLE names_utxos ADD COLUMN IF NOT EXISTS period_start BIGINT",
        // Registry v3 (docs/KACHAT_NAMES_REGISTRY_V3.md): offers name their seller, and price
        // shards are tracked rows (kind 'shard': shard, authority, prices "p1,..,p5").
        "ALTER TABLE names_utxos ADD COLUMN IF NOT EXISTS seller BYTEA",
        "ALTER TABLE names_utxos ADD COLUMN IF NOT EXISTS shard BIGINT",
        "ALTER TABLE names_utxos ADD COLUMN IF NOT EXISTS authority BYTEA",
        "ALTER TABLE names_utxos ADD COLUMN IF NOT EXISTS prices TEXT",
        "ALTER TABLE names_state ADD COLUMN IF NOT EXISTS price_covenant_id TEXT",
        "ALTER TABLE names_state ADD COLUMN IF NOT EXISTS registry_version INTEGER",
        "ALTER TABLE names_state ADD COLUMN IF NOT EXISTS period_ms BIGINT",
        // KACHAT_NAMES_PRUNED_START.md §2: a follower that cannot reach its start block says so.
        "ALTER TABLE names_state ADD COLUMN IF NOT EXISTS fatal_reason TEXT",
        "ALTER TABLE names_state ADD COLUMN IF NOT EXISTS start_block BYTEA",
        "CREATE INDEX IF NOT EXISTS names_utxos_key ON names_utxos (key)",
        "CREATE INDEX IF NOT EXISTS names_utxos_owner ON names_utxos (owner)",
        "CREATE INDEX IF NOT EXISTS names_utxos_buyer ON names_utxos (buyer)",
        r#"CREATE TABLE IF NOT EXISTS names_history (
            id BIGSERIAL PRIMARY KEY,
            block BYTEA NOT NULL,
            tx_id BYTEA NOT NULL,
            op TEXT NOT NULL,
            key BYTEA NOT NULL,
            name TEXT,
            at BIGINT NOT NULL,
            daa BIGINT NOT NULL,
            from_key BYTEA,
            to_key BYTEA,
            price BIGINT,
            years BIGINT
        )"#,
        "CREATE INDEX IF NOT EXISTS names_history_key ON names_history (key, id)",
        "CREATE INDEX IF NOT EXISTS names_history_block ON names_history (block)",
        r#"CREATE TABLE IF NOT EXISTS names_profiles (
            spk BYTEA PRIMARY KEY,
            address TEXT NOT NULL,
            profile TEXT NOT NULL,
            daa BIGINT NOT NULL,
            tx_id BYTEA NOT NULL,
            updated_at BIGINT NOT NULL
        )"#,
        "CREATE INDEX IF NOT EXISTS names_profiles_address ON names_profiles (address)",
        // Part E reminders sent: one row per (name key, expiry, kind) so each is sent once,
        // and a renewal (new expiresAt) starts a fresh schedule.
        r#"CREATE TABLE IF NOT EXISTS names_reminders (
            key BYTEA NOT NULL,
            expires_at BIGINT NOT NULL,
            kind TEXT NOT NULL,
            sent_at BIGINT NOT NULL,
            PRIMARY KEY (key, expires_at, kind)
        )"#,
    ] {
        sqlx::query(stmt).execute(pool).await?;
    }
    Ok(())
}

/// The registry id the stored rows belong to (`None` = empty database).
pub async fn stored_registry(pool: &PgPool) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT registry_covenant_id FROM names_state WHERE id = 1")
        .fetch_optional(pool)
        .await?)
}

/// What a reset records about the registry the tables follow.
pub struct RegistryInfo<'a> {
    pub registry: &'a str,
    pub network: &'a str,
    pub genesis_txid: &'a str,
    pub grace_ms: i64,
    /// Registry v3: the price covenant id.
    pub price_covenant_id: Option<&'a str>,
    pub version: i32,
    pub period_ms: i64,
}

/// Wipe everything and record which registry the tables now follow.
pub async fn reset(pool: &PgPool, info: &RegistryInfo<'_>) -> Result<()> {
    let mut tx = pool.begin().await?;
    // names_profiles is not ours: the profiles follower owns it (docs/KACHAT_PROFILES.md).
    for t in ["names_utxos", "names_history", "names_reminders", "names_state"] {
        sqlx::query(&format!("DELETE FROM {t}")).execute(&mut *tx).await?;
    }
    sqlx::query(
        r#"INSERT INTO names_state (id, registry_covenant_id, network, genesis_txid, grace_ms, price_covenant_id,
               registry_version, period_ms)
           VALUES (1, $1, $2, $3, $4, $5, $6, $7)"#,
    )
    .bind(info.registry)
    .bind(info.network)
    .bind(info.genesis_txid)
    .bind(info.grace_ms)
    .bind(info.price_covenant_id)
    .bind(info.version)
    .bind(info.period_ms)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

fn b32(v: Option<Vec<u8>>) -> Option<[u8; 32]> {
    v.and_then(|b| b.try_into().ok())
}

/// Load the persisted registry, per-UTXO metadata, profile times and checkpoint.
pub async fn load(pool: &PgPool) -> Result<(Registry, HashMap<Outpoint, UtxoMeta>, Status)> {
    let mut reg = Registry::new();
    let mut meta = HashMap::new();
    for r in sqlx::query("SELECT * FROM names_utxos").fetch_all(pool).await? {
        let op: Outpoint = (b32(r.get("txid")).unwrap_or_default(), r.get::<i32, _>("idx") as u32);
        let kind: String = r.get("kind");
        let tracked = match kind.as_str() {
            "gap" => Tracked::Gap(GapState {
                lo: b32(r.get("lo")).unwrap_or_default(),
                hi: b32(r.get("hi")).unwrap_or_default(),
            }),
            "name" => Tracked::Name(NameState {
                key: b32(r.get("key")).unwrap_or_default(),
                name: kachat_names::pad_name(r.get::<Option<String>, _>("name").unwrap_or_default().as_bytes()),
                owner: b32(r.get("owner")).unwrap_or_default(),
                price: r.get::<Option<i64>, _>("price").unwrap_or(0),
                period_start: r.get::<Option<i64>, _>("period_start").unwrap_or(0),
                expires_at: r.get::<Option<i64>, _>("expires_at").unwrap_or(0),
            }),
            "offer" => Tracked::Offer(OfferState {
                key: b32(r.get("key")).unwrap_or_default(),
                buyer: b32(r.get("buyer")).unwrap_or_default(),
                seller: b32(r.get("seller")),
                refund_after: r.get::<Option<i64>, _>("refund_after").unwrap_or(0),
            }),
            "shard" => {
                let prices: Vec<i64> = r
                    .get::<Option<String>, _>("prices")
                    .unwrap_or_default()
                    .split(',')
                    .filter_map(|p| p.parse().ok())
                    .collect();
                let Ok(prices) = <[i64; 5]>::try_from(prices) else { continue };
                Tracked::Shard(PriceState {
                    shard: r.get::<Option<i64>, _>("shard").unwrap_or(0),
                    authority: b32(r.get("authority")).unwrap_or_default(),
                    prices,
                })
            }
            _ => continue,
        };
        reg.utxos.insert(op, tracked);
        meta.insert(
            op,
            UtxoMeta {
                value: r.get::<i64, _>("value") as u64,
                created_at: r.get("created_at"),
                created_daa: r.get::<i64, _>("created_daa") as u64,
            },
        );
    }
    let status = match sqlx::query("SELECT * FROM names_state WHERE id = 1").fetch_optional(pool).await? {
        Some(r) => Status {
            checkpoint: b32(r.get("checkpoint")),
            indexed_daa: r.get::<i64, _>("indexed_daa") as u64,
            virtual_daa: r.get::<i64, _>("virtual_daa") as u64,
            synced: r.get("synced"),
            self_test_ok: r.get("self_test_ok"),
            self_test_at: r.get("self_test_at"),
            fatal_reason: None,
            start_block: None,
        },
        None => Status::default(),
    };
    Ok((reg, meta, status))
}

/// One committed write after a batch: the full live set (when it changed), the history
/// rows of removed blocks dropped, the new events appended, and the checkpoint advanced.
#[allow(clippy::too_many_arguments)]
pub async fn persist(
    pool: &PgPool,
    reg: &Registry,
    meta: &HashMap<Outpoint, UtxoMeta>,
    refuted: &std::collections::HashSet<Outpoint>,
    removed_blocks: &[[u8; 32]],
    events: &[(Event, Option<String>)],
    rewrite_live_set: bool,
    status: &Status,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    if rewrite_live_set {
        sqlx::query("DELETE FROM names_utxos").execute(&mut *tx).await?;
        for (op, tracked) in &reg.utxos {
            let m = meta.get(op).copied().unwrap_or(UtxoMeta { value: 0, created_at: 0, created_daa: 0 });
            // One row per tracked UTXO; the columns a kind does not use stay NULL.
            let (mut lo, mut hi, mut key, mut name, mut owner) = (None, None, None, None, None);
            let (mut price, mut period_start, mut expires_at, mut buyer, mut refund_after) = (None, None, None, None, None);
            let (mut seller, mut shard, mut authority, mut prices) = (None, None, None, None);
            let kind = match tracked {
                Tracked::Gap(g) => {
                    lo = Some(g.lo.to_vec());
                    hi = Some(g.hi.to_vec());
                    "gap"
                }
                Tracked::Name(n) => {
                    key = Some(n.key.to_vec());
                    name = Some(n.name_str());
                    owner = Some(n.owner.to_vec());
                    price = Some(n.price);
                    period_start = Some(n.period_start);
                    expires_at = Some(n.expires_at);
                    "name"
                }
                Tracked::Offer(o) => {
                    key = Some(o.key.to_vec());
                    buyer = Some(o.buyer.to_vec());
                    refund_after = Some(o.refund_after);
                    seller = o.seller.map(|k| k.to_vec());
                    "offer"
                }
                Tracked::Shard(p) => {
                    shard = Some(p.shard);
                    authority = Some(p.authority.to_vec());
                    prices = Some(p.prices.iter().map(i64::to_string).collect::<Vec<_>>().join(","));
                    "shard"
                }
            };
            let q = sqlx::query(
                r#"INSERT INTO names_utxos (txid, idx, kind, value, lo, hi, key, name, owner, price, period_start,
                       expires_at, buyer, refund_after, created_at, created_daa, refuted, seller, shard, authority, prices)
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21)"#,
            )
            .bind(op.0.to_vec())
            .bind(op.1 as i32)
            .bind(kind)
            .bind(m.value as i64)
            .bind(lo)
            .bind(hi)
            .bind(key)
            .bind(name)
            .bind(owner)
            .bind(price)
            .bind(period_start)
            .bind(expires_at)
            .bind(buyer)
            .bind(refund_after);
            q.bind(m.created_at)
                .bind(m.created_daa as i64)
                .bind(refuted.contains(op))
                .bind(seller)
                .bind(shard)
                .bind(authority)
                .bind(prices)
                .execute(&mut *tx)
                .await?;
        }
    }
    for block in removed_blocks {
        sqlx::query("DELETE FROM names_history WHERE block = $1").bind(block.to_vec()).execute(&mut *tx).await?;
    }
    for (e, name) in events {
        sqlx::query(
            r#"INSERT INTO names_history (block, tx_id, op, key, name, at, daa, from_key, to_key, price, years)
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)"#,
        )
        .bind(e.block.to_vec())
        .bind(e.tx_id.to_vec())
        .bind(e.op)
        .bind(e.key.to_vec())
        .bind(name)
        .bind(e.at)
        .bind(e.daa as i64)
        .bind(e.from.map(|k| k.to_vec()))
        .bind(e.to.map(|k| k.to_vec()))
        .bind(e.price)
        .bind(e.years)
        .execute(&mut *tx)
        .await?;
    }
    write_status(&mut tx, status).await?;
    tx.commit().await?;
    Ok(())
}

async fn write_status(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, s: &Status) -> Result<()> {
    sqlx::query(
        r#"UPDATE names_state SET checkpoint = $1, indexed_daa = $2, virtual_daa = $3, synced = $4,
               self_test_ok = $5, self_test_at = $6, fatal_reason = $7, start_block = $8,
               updated_at = (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT
           WHERE id = 1"#,
    )
    .bind(s.checkpoint.map(|c| c.to_vec()))
    .bind(s.indexed_daa as i64)
    .bind(s.virtual_daa as i64)
    .bind(s.synced)
    .bind(s.self_test_ok)
    .bind(s.self_test_at)
    .bind(&s.fatal_reason)
    .bind(s.start_block.map(|b| b.to_vec()))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Claim a reminder; true only the first time (so it is pushed exactly once).
pub async fn claim_reminder(pool: &PgPool, key: &[u8; 32], expires_at: i64, kind: &str, now: i64) -> Result<bool> {
    let r = sqlx::query(
        "INSERT INTO names_reminders (key, expires_at, kind, sent_at) VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING",
    )
    .bind(key.to_vec())
    .bind(expires_at)
    .bind(kind)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Status-only write (no registry change in this batch).
pub async fn save_status(pool: &PgPool, s: &Status) -> Result<()> {
    let mut tx = pool.begin().await?;
    write_status(&mut tx, s).await?;
    tx.commit().await?;
    Ok(())
}

/// Mark which live rows the self-test found missing from the node's UTXO set.
pub async fn set_refuted(pool: &PgPool, refuted: &std::collections::HashSet<Outpoint>) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE names_utxos SET refuted = FALSE").execute(&mut *tx).await?;
    for op in refuted {
        sqlx::query("UPDATE names_utxos SET refuted = TRUE WHERE txid = $1 AND idx = $2")
            .bind(op.0.to_vec())
            .bind(op.1 as i32)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
