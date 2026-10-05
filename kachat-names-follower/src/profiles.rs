//! Address profiles follower (docs/KACHAT_PROFILES.md): `--profiles` mode.
//!
//! Profiles are stamped to an address by a `kchat:1:profile:<json>` self-send and need no
//! registry, so this runs on every network, always on, whether or not a names manifest is
//! loaded. It is the **only writer** of `names_profiles` (the names follower no longer
//! touches it), plus `profile_saves` (one row per accepted record, for the panel's stats)
//! and its own `profiles_state` checkpoint row.
//!
//! The rules are the engine's (`Registry::apply_profile_only`): v1 JSON ≤ 2 KB, a real
//! self-send, newest wins by (accepting DAA, txid), reorgs roll back.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use kachat_names::follower::Follower;
use kaspa_addresses::Prefix;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_rpc_core::model::RpcHash;
use sqlx::{PgPool, Row};
use tracing::{info, warn};

use crate::node::Node;
use crate::{Args, Prefetched, Window, spk_address};

/// Where the profiles follower stands (one row).
#[derive(Debug, Default)]
struct State {
    checkpoint: Option<[u8; 32]>,
    indexed_daa: u64,
    virtual_daa: u64,
    synced: bool,
}

async fn create_schema(pool: &PgPool) -> Result<()> {
    for stmt in [
        r#"CREATE TABLE IF NOT EXISTS profiles_state (
            id SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
            network TEXT NOT NULL,
            scan_from BYTEA,
            checkpoint BYTEA,
            indexed_daa BIGINT NOT NULL DEFAULT 0,
            virtual_daa BIGINT NOT NULL DEFAULT 0,
            synced BOOLEAN NOT NULL DEFAULT FALSE,
            updated_at BIGINT NOT NULL DEFAULT 0
        )"#,
        r#"CREATE TABLE IF NOT EXISTS names_profiles (
            spk BYTEA PRIMARY KEY,
            address TEXT NOT NULL,
            profile TEXT NOT NULL,
            daa BIGINT NOT NULL,
            tx_id BYTEA NOT NULL,
            updated_at BIGINT NOT NULL
        )"#,
        "CREATE INDEX IF NOT EXISTS names_profiles_address ON names_profiles (address)",
        // First accepted record's block time, kept across later saves ("new profiles").
        "ALTER TABLE names_profiles ADD COLUMN IF NOT EXISTS created_at BIGINT",
        "UPDATE names_profiles SET created_at = updated_at WHERE created_at IS NULL",
        "CREATE INDEX IF NOT EXISTS names_profiles_created ON names_profiles (created_at)",
        r#"CREATE TABLE IF NOT EXISTS profile_saves (
            tx_id BYTEA PRIMARY KEY,
            spk BYTEA NOT NULL,
            address TEXT NOT NULL,
            block BYTEA NOT NULL,
            daa BIGINT NOT NULL,
            block_time BIGINT NOT NULL
        )"#,
        "CREATE INDEX IF NOT EXISTS profile_saves_time ON profile_saves (block_time)",
        "CREATE INDEX IF NOT EXISTS profile_saves_block ON profile_saves (block)",
        "CREATE INDEX IF NOT EXISTS profile_saves_spk ON profile_saves (spk)",
        // The record itself, for the panel's all-time history (/profiles/history). Saves
        // indexed before this column existed get it back where it is still the current one.
        "ALTER TABLE profile_saves ADD COLUMN IF NOT EXISTS profile TEXT",
        r#"UPDATE profile_saves s SET profile = p.profile FROM names_profiles p
           WHERE s.profile IS NULL AND s.tx_id = p.tx_id"#,
    ] {
        sqlx::query(stmt).execute(pool).await?;
    }
    Ok(())
}

/// The stored state, if it belongs to `network`. Otherwise (first run, or another
/// network's data) the profile tables are wiped and the follower starts from scratch, so
/// `profile_saves` and `created_at` count from the same scan as the profiles themselves.
async fn load_state(pool: &PgPool, network: &str) -> Result<State> {
    let row = sqlx::query("SELECT network, checkpoint, indexed_daa, virtual_daa FROM profiles_state WHERE id = 1")
        .fetch_optional(pool)
        .await?;
    if let Some(r) = row
        && r.get::<String, _>("network") == network
    {
        return Ok(State {
            checkpoint: r.get::<Option<Vec<u8>>, _>("checkpoint").and_then(|b| b.try_into().ok()),
            indexed_daa: r.get::<i64, _>("indexed_daa") as u64,
            virtual_daa: r.get::<i64, _>("virtual_daa") as u64,
            synced: false,
        });
    }
    let mut tx = pool.begin().await?;
    for t in ["names_profiles", "profile_saves", "profiles_state"] {
        sqlx::query(&format!("DELETE FROM {t}")).execute(&mut *tx).await?;
    }
    sqlx::query("INSERT INTO profiles_state (id, network) VALUES (1, $1)").bind(network).execute(&mut *tx).await?;
    tx.commit().await?;
    info!("[profiles] fresh profile tables for {network}");
    Ok(State::default())
}

async fn write_state(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, s: &State, scan_from: Option<[u8; 32]>) -> Result<()> {
    sqlx::query(
        r#"UPDATE profiles_state SET checkpoint = $1, indexed_daa = $2, virtual_daa = $3, synced = $4,
               scan_from = COALESCE(scan_from, $5),
               updated_at = (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT
           WHERE id = 1"#,
    )
    .bind(s.checkpoint.map(|c| c.to_vec()))
    .bind(s.indexed_daa as i64)
    .bind(s.virtual_daa as i64)
    .bind(s.synced)
    .bind(scan_from.map(|c| c.to_vec()))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Where a first run starts: `KACHAT_PROFILES_SCAN_FROM`, else the names manifest's
/// `genesis.scanFrom` (testnet: profiles have existed since the names work began), else the
/// node's pruning point. A pruned node can only follow the chain from its pruning point, so
/// a configured block older than that (or unknown to the node) falls back to it.
async fn scan_start(node: &Node, args: &Args) -> Result<[u8; 32]> {
    let mut wanted: Option<([u8; 32], &str)> = None;
    if let Some(hash) = args.profiles_scan_from.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let h = hex::decode(hash)?.try_into().map_err(|_| anyhow!("KACHAT_PROFILES_SCAN_FROM is not a 32-byte hash"))?;
        wanted = Some((h, "KACHAT_PROFILES_SCAN_FROM"));
    } else if let Some(path) = args.manifest.as_deref().filter(|p| std::path::Path::new(p).is_file()) {
        let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        if let Some(hash) = raw["genesis"]["scanFrom"].as_str() {
            let h = hex::decode(hash)?.try_into().map_err(|_| anyhow!("manifest scanFrom is not a 32-byte hash"))?;
            wanted = Some((h, "the names manifest's scanFrom"));
        }
    }
    let info = node.client.get_block_dag_info().await.map_err(|e| anyhow!("getBlockDagInfo: {e}"))?;
    let pruning = info.pruning_point_hash;
    if let Some((hash, source)) = wanted {
        let pruning_score = node.client.get_block(pruning, false).await.map(|b| b.header.blue_score).ok();
        match node.client.get_block(RpcHash::from_bytes(hash), false).await {
            Ok(b) if pruning_score.is_none_or(|p| b.header.blue_score >= p) => {
                info!("[profiles] starting from {source} {}", hex::encode(hash));
                return Ok(hash);
            }
            Ok(_) => warn!("[profiles] {source} {} is older than the node's pruning point", hex::encode(hash)),
            Err(e) => warn!("[profiles] the node does not have {source} {} ({e})", hex::encode(hash)),
        }
    }
    info!("[profiles] starting from the node's pruning point {}", hex::encode(pruning.as_bytes()));
    Ok(pruning.as_bytes())
}

pub async fn run(args: &Args) -> Result<()> {
    let network = args.network.clone();
    let prefix = if network == "mainnet" { Prefix::Mainnet } else { Prefix::Testnet };
    info!("[profiles] following address profiles on {network}");

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&format!(
            "postgres://{}:{}@{}:{}/{}",
            args.db_user, args.db_password, args.db_host, args.db_port, args.db_name
        ))
        .await
        .context("connecting to Postgres")?;
    create_schema(&pool).await?;
    let mut state = load_state(&pool, &network).await?;

    let mut follower = Follower::new(args.journal_keep);
    for r in sqlx::query("SELECT spk, profile, daa, tx_id FROM names_profiles").fetch_all(&pool).await? {
        let txid: [u8; 32] = r.get::<Vec<u8>, _>("tx_id").try_into().unwrap_or_default();
        follower.registry.profiles.insert(r.get("spk"), (r.get("profile"), (r.get::<i64, _>("daa") as u64, txid)));
    }

    let node = Node::connect(&args.node_url, &network).await?;
    info!("[profiles] connected to {}", args.node_url);
    let mut scan_from = None;
    match state.checkpoint {
        Some(cp) => {
            follower.checkpoint = Some(cp);
            info!("[profiles] resuming from {} ({} profiles)", hex::encode(cp), follower.registry.profiles.len());
        }
        None => {
            let start = scan_start(&node, args).await?;
            follower.checkpoint = Some(start);
            scan_from = Some(start);
        }
    }

    let mut window = Window::default();
    let mut last_heartbeat = Instant::now() - Duration::from_secs(60);
    loop {
        let from = follower.checkpoint.ok_or_else(|| anyhow!("lost the checkpoint"))?;
        let (batch, last_daa) = match node.next_batch(from, args.min_confirmations, window.0).await {
            Ok(b) => {
                window.succeeded();
                b
            }
            Err(e) => {
                window.failed();
                warn!("[profiles] fetch failed ({e:#}); next batch capped to {:?} blue score", window.0);
                tokio::time::sleep(Duration::from_millis(args.poll_ms * 5)).await;
                continue;
            }
        };
        let fetched_blocks = batch.tip.is_some();
        let has_profile_tx = batch.accepted.iter().any(|t| t.payload.starts_with(b"kchat:1:profile:"));
        let touches = !batch.removed_blocks.is_empty() || has_profile_tx;
        // Only diffed when something could have changed: the map can be large on mainnet.
        let before = touches.then(|| follower.registry.profiles.clone());
        let (batch, applied) = follower.step_profiles(&mut Prefetched(Some(batch)))?;

        if let Some(d) = last_daa {
            state.indexed_daa = d;
        }
        state.checkpoint = follower.checkpoint;
        state.virtual_daa = node.virtual_daa().await.unwrap_or(state.virtual_daa);
        let was_synced = state.synced;
        state.synced = state.virtual_daa.saturating_sub(state.indexed_daa)
            <= args.synced_within_daa + args.min_confirmations * 10;

        let mut tx = pool.begin().await?;
        if let Some(before) = before {
            for block in &batch.removed_blocks {
                sqlx::query("DELETE FROM profile_saves WHERE block = $1").bind(block.to_vec()).execute(&mut *tx).await?;
            }
            let applied_set: HashSet<[u8; 32]> = applied.iter().copied().collect();
            for t in batch.accepted.iter().filter(|t| applied_set.contains(&t.id)) {
                let spk = &t.outputs[0].script_public_key;
                let Some(address) = spk_address(prefix, spk) else { continue };
                sqlx::query(
                    r#"INSERT INTO profile_saves (tx_id, spk, address, block, daa, block_time, profile)
                       VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (tx_id) DO NOTHING"#,
                )
                .bind(t.id.to_vec())
                .bind(spk)
                .bind(&address)
                .bind(t.accepting_block.to_vec())
                .bind(t.accepting_daa as i64)
                .bind(t.block_time)
                .bind(std::str::from_utf8(&t.payload).ok().and_then(|p| p.strip_prefix("kchat:1:profile:")))
                .execute(&mut *tx)
                .await?;
                info!("[profiles] saved {address} tx {}", hex::encode(t.id));
            }
            let changed: HashSet<&Vec<u8>> = before
                .keys()
                .chain(follower.registry.profiles.keys())
                .filter(|spk| before.get(*spk) != follower.registry.profiles.get(*spk))
                .collect();
            for spk in changed {
                match follower.registry.profiles.get(spk) {
                    Some((json, (daa, txid))) => {
                        let Some(address) = spk_address(prefix, spk) else { continue };
                        // updated_at = the record's own block time (also right after a reorg
                        // restores an older record); created_at only on the first insert.
                        sqlx::query(
                            r#"INSERT INTO names_profiles (spk, address, profile, daa, tx_id, updated_at, created_at)
                               SELECT $1, $2, $3, $4, $5, t.at,
                                      COALESCE((SELECT MIN(block_time) FROM profile_saves WHERE spk = $1), t.at)
                               FROM (SELECT COALESCE((SELECT block_time FROM profile_saves WHERE tx_id = $5), 0) AS at) t
                               ON CONFLICT (spk) DO UPDATE SET address = EXCLUDED.address, profile = EXCLUDED.profile,
                                   daa = EXCLUDED.daa, tx_id = EXCLUDED.tx_id, updated_at = EXCLUDED.updated_at"#,
                        )
                        .bind(spk)
                        .bind(address)
                        .bind(json)
                        .bind(*daa as i64)
                        .bind(txid.to_vec())
                        .execute(&mut *tx)
                        .await?;
                    }
                    None => {
                        sqlx::query("DELETE FROM names_profiles WHERE spk = $1").bind(spk).execute(&mut *tx).await?;
                    }
                }
            }
            if !batch.removed_blocks.is_empty() {
                info!("[profiles] reorg: undid {} chain block(s)", batch.removed_blocks.len());
            }
        }
        write_state(&mut tx, &state, scan_from).await?;
        tx.commit().await?;

        // Progress heartbeat, so a long catch-up is not silent (KACHAT_NAMES_PANEL_LOGS.md style).
        if state.synced != was_synced || last_heartbeat.elapsed() >= Duration::from_secs(60) {
            last_heartbeat = Instant::now();
            info!(
                "[profiles] at DAA {} (tip {}, {} behind, synced={}, {} profiles)",
                state.indexed_daa,
                state.virtual_daa,
                state.virtual_daa.saturating_sub(state.indexed_daa),
                state.synced,
                follower.registry.profiles.len()
            );
        }
        if batch.accepted.is_empty() && !fetched_blocks {
            tokio::time::sleep(Duration::from_millis(args.poll_ms)).await;
        }
        if follower.checkpoint.is_none() {
            bail!("lost the checkpoint");
        }
    }
}
