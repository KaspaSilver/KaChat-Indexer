use crate::config::AppConfig;
use anyhow::Result;
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use tracing::{error, info, warn};

pub type DbPool = PgPool;

// Schema version management
const SCHEMA_VERSION: i32 = 2;

/// kachat-transaction-processor Database Client
/// Similar to KaspaDbClient in Simply Kaspa Indexer
pub struct KDbClient {
    pool: DbPool,
}

impl KDbClient {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &DbPool {
        &self.pool
    }

    /// Verify that transactions table exists (required for trigger)
    /// Loops with warning and 10-second wait if not found
    async fn verify_transactions_table_exists(&self) -> Result<()> {
        loop {
            let table_exists = sqlx::query(
                "SELECT EXISTS(SELECT 1 FROM information_schema.tables WHERE table_name = 'transactions')"
            )
            .fetch_one(&self.pool)
            .await?
            .get::<bool, _>(0);

            if table_exists {
                info!(
                    "✓ Transactions table found - proceeding with kachat-transaction-processor schema setup"
                );
                return Ok(());
            } else {
                warn!(
                    "⚠️  Transactions table not found - kachat-transaction-processor requires the main Kaspa indexer to be running first"
                );
                warn!("   Waiting 10 seconds before checking again...");
                tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
            }
        }
    }

    /// Drop existing schema (equivalent to KaspaDbClient::drop_schema)
    pub async fn drop_schema(&self) -> Result<()> {
        info!("Dropping existing schema");
        execute_ddl(SCHEMA_DOWN_SQL, &self.pool).await?;
        info!("Schema dropped successfully");
        Ok(())
    }

    /// Set or verify network type in k_vars table
    pub async fn set_and_verify_network(&self, network: &str) -> Result<()> {
        info!("Setting and verifying network type: {}", network);

        // Check if network is already set in k_vars
        let existing_network = sqlx::query("SELECT value FROM k_vars WHERE key = 'network'")
            .fetch_optional(&self.pool)
            .await?;

        match existing_network {
            Some(row) => {
                let stored_network: String = row.get("value");
                if stored_network != network {
                    return Err(anyhow::anyhow!(
                        "Network mismatch! Database is configured for '{}' but kachat-transaction-processor is set to '{}'. \
                        This could lead to data corruption. Please use the correct network parameter or initialize a new database.",
                        stored_network,
                        network
                    ));
                } else {
                    info!("✓ Network type verified: {}", network);
                }
            }
            None => {
                // Insert network type for the first time
                info!("Setting network type in database: {}", network);
                sqlx::query("INSERT INTO k_vars (key, value) VALUES ('network', $1)")
                    .bind(network)
                    .execute(&self.pool)
                    .await?;
                info!("✓ Network type set to: {}", network);
            }
        }

        Ok(())
    }

    /// Create or upgrade schema (equivalent to KaspaDbClient::create_schema)
    pub async fn create_schema(&self, upgrade_db: bool) -> Result<()> {
        info!("Starting schema creation/upgrade process");

        // Verify transactions table exists (required for trigger)
        self.verify_transactions_table_exists().await?;

        // Check current schema version
        let current_version = get_schema_version(&self.pool).await?;

        match current_version {
            Some(version) => {
                info!("Found existing schema version: {}", version);

                if version < SCHEMA_VERSION {
                    if upgrade_db {
                        warn!("Upgrading schema from v{} to v{}", version, SCHEMA_VERSION);

                        // Perform sequential upgrades
                        let mut current_version = version;

                        // v0 -> v1: Add all indexes, constraints, and extensions
                        if current_version == 0 {
                            info!("Applying migration v0 -> v1 (indexes, constraints, extensions)");
                            execute_ddl(MIGRATION_V0_TO_V1_SQL, &self.pool).await?;
                            current_version = 1;
                            info!("Migration v0 -> v1 completed successfully");
                        }

                        // v1 -> v2: Add hashtags table and indexes
                        if current_version == 1 {
                            info!("Applying migration v1 -> v2 (hashtags support)");
                            execute_ddl(MIGRATION_V1_TO_V2_SQL, &self.pool).await?;
                            current_version = 2;
                            info!("Migration v1 -> v2 completed successfully");
                        }

                        info!(
                            "Schema upgrade completed successfully (final version: {})",
                            current_version
                        );
                    } else {
                        return Err(anyhow::anyhow!(
                            "Found outdated schema v{}. Set flag '--upgrade-db' to upgrade",
                            version
                        ));
                    }
                } else if version > SCHEMA_VERSION {
                    return Err(anyhow::anyhow!(
                        "Found newer & unsupported schema version {}. Current supported version is {}",
                        version,
                        SCHEMA_VERSION
                    ));
                } else {
                    info!("Schema version {} is up to date", version);
                }
            }
            None => {
                info!(
                    "No existing schema found, creating fresh schema v{}",
                    SCHEMA_VERSION
                );
                execute_ddl(SCHEMA_UP_SQL, &self.pool).await?;

                info!("Fresh schema creation completed successfully");
            }
        }

        // Step 1b: idempotently ensure the KaChat broadcast table exists (fork addition).
        self.create_broadcast_schema().await?;

        // Step 1c: idempotently ensure the KaPosts personal-mode block/mute denylist exists.
        self.create_denylist_schema().await?;

        // Step 1d: idempotently ensure the post-translation cache table exists (fork addition).
        self.create_translations_schema().await?;

        // Step 2: idempotently (re)assert the notification function + trigger on EVERY startup,
        // regardless of fresh/upgrade/up-to-date branch and regardless of `upgrade_db`.
        // This self-heals a trigger dropped by a simply-kaspa-indexer schema migration
        // (e.g. the v10 -> v20 denormalization recreates the `transactions` table).
        self.create_notification_system().await?;

        // Verify schema setup
        verify_schema_setup(&self.pool).await?;

        info!("Schema creation/upgrade process completed");
        Ok(())
    }

    /// Create the KaChat broadcast table (fork addition). Durable store for `ciph_msg:1:bcast:`
    /// channel messages; separate from the K `broadcast` action's `k_broadcasts` table.
    async fn create_broadcast_schema(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS kachat_broadcasts (
                id BIGSERIAL PRIMARY KEY,
                transaction_id BYTEA UNIQUE NOT NULL,
                block_time BIGINT NOT NULL,
                channel VARCHAR(36) NOT NULL,
                sender_address TEXT NOT NULL,
                content TEXT NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await?;
        // Drop the earlier index name that collided with the schema verifier's `idx_k_%`
        // pattern (self-heals any DB created by a pre-fix build).
        sqlx::query("DROP INDEX IF EXISTS idx_kachat_broadcasts_channel_time")
            .execute(&self.pool)
            .await?;
        // NOTE: index name must NOT match the schema verifier's `idx_k_%` pattern (where `_`
        // is a SQL single-char wildcard), so it is not miscounted as a K protocol index.
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_bcast_channel_time \
             ON kachat_broadcasts(channel, block_time DESC, id DESC)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Post-translation cache (fork addition). A KaPost is immutable, so a translation of
    /// (transaction_id, target_lang) is correct permanently — no TTL, no invalidation. Only the
    /// server's own verified copy of a post is ever cached here (see the /translate handler); text
    /// supplied in a request is never written under a txid. `post_id` is BYTEA to match
    /// `k_contents.transaction_id`.
    async fn create_translations_schema(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS post_translations (
                post_id BYTEA NOT NULL,
                target_lang TEXT NOT NULL,
                source_lang TEXT NOT NULL,
                text TEXT NOT NULL,
                created_at BIGINT NOT NULL,
                PRIMARY KEY (post_id, target_lang)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// KaPosts personal-mode denylist: authors (compressed pubkeys) the operator has blocked or
    /// muted. The processor skips storing any content from these pubkeys, and the admin dashboard
    /// purges their existing rows when they are added. Empty table = index everyone (default).
    async fn create_denylist_schema(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS kachat_kaposts_denylist (
                pubkey BYTEA PRIMARY KEY,
                kind VARCHAR(8) NOT NULL DEFAULT 'block',
                added_at BIGINT NOT NULL DEFAULT 0
            )
            "#,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Create the notification function and trigger separately to avoid DDL parsing issues
    async fn create_notification_system(&self) -> Result<()> {
        info!("Creating notification function and trigger");

        // Fire on:
        //   - canonical KaChat payloads ('kchat:1:' = hex 6b636861743a313a) — covers both KaChat
        //     posts and KaChat broadcasts, since both start with `kchat:1:`;
        //   - legacy K social payloads ('k:1:' = hex 6b3a313a);
        //   - legacy KaChat broadcast payloads ('ciph_msg:1:bcast:' =
        //     hex 636970685f6d73673a313a62636173743a).
        // The two legacy prefixes are read-only history support after the KaChat rebrand.
        sqlx::query(
            r#"
            CREATE OR REPLACE FUNCTION notify_transaction() RETURNS TRIGGER AS $$
            BEGIN
                IF substr(encode(NEW.payload, 'hex'), 1, 16) = '6b636861743a313a'
                   OR substr(encode(NEW.payload, 'hex'), 1, 8) = '6b3a313a'
                   OR substr(encode(NEW.payload, 'hex'), 1, 34) = '636970685f6d73673a313a62636173743a' THEN
                    PERFORM pg_notify('transaction_channel', encode(NEW.transaction_id, 'hex'));
                END IF;
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql
        "#,
        )
        .execute(&self.pool)
        .await?;

        // Drop first so the trigger can be (re)asserted idempotently on every startup.
        sqlx::query("DROP TRIGGER IF EXISTS transaction_notify_trigger ON transactions")
            .execute(&self.pool)
            .await?;

        // Create the trigger
        sqlx::query(
            r#"
            CREATE TRIGGER transaction_notify_trigger
            AFTER INSERT ON transactions
            FOR EACH ROW EXECUTE FUNCTION notify_transaction()
        "#,
        )
        .execute(&self.pool)
        .await?;

        info!("Notification system created successfully");
        Ok(())
    }
}

// Embedded SQL migration files
const SCHEMA_UP_SQL: &str = include_str!("migrations/schema/up.sql");
const SCHEMA_DOWN_SQL: &str = include_str!("migrations/schema/down.sql");
const MIGRATION_V0_TO_V1_SQL: &str = include_str!("migrations/schema/v0_to_v1.sql");
const MIGRATION_V1_TO_V2_SQL: &str = include_str!("migrations/schema/v1_to_v2.sql");

pub async fn create_pool(config: &AppConfig) -> Result<DbPool> {
    let connection_string = config.connection_string();

    loop {
        match PgPoolOptions::new()
            .max_connections(config.database.max_connections as u32)
            .connect(&connection_string)
            .await
        {
            Ok(pool) => {
                // Test the pool connection
                match sqlx::query("SELECT 1").fetch_one(&pool).await {
                    Ok(_) => {
                        info!("Database connection pool created and tested successfully");
                        return Ok(pool);
                    }
                    Err(e) => {
                        warn!(
                            "Database connection pool created but test query failed: {}",
                            e
                        );
                        warn!("Retrying in 10 seconds...");
                        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                    }
                }
            }
            Err(e) => {
                warn!("Failed to create database connection pool: {}", e);
                warn!("Retrying in 10 seconds...");
                tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Transaction {
    pub transaction_id: String,
    pub payload: Option<String>,
    pub block_time: Option<i64>,
}

pub async fn fetch_transaction(
    pool: &DbPool,
    transaction_id_hex: &str,
) -> Result<Option<Transaction>> {
    // Convert hex string back to bytea for database query
    let transaction_id_bytes = hex::decode(transaction_id_hex)?;

    let row = sqlx::query(
        r#"
        SELECT 
            transaction_id,
            payload,
            block_time
        FROM transactions 
        WHERE transaction_id = $1
        "#,
    )
    .bind(&transaction_id_bytes)
    .fetch_optional(pool)
    .await?;

    if let Some(row) = row {
        let transaction_id: Vec<u8> = row.get("transaction_id");
        let payload: Option<Vec<u8>> = row.get("payload");

        Ok(Some(Transaction {
            transaction_id: hex::encode(&transaction_id),
            payload: payload.map(|p| hex::encode(&p)),
            block_time: row.get("block_time"),
        }))
    } else {
        Ok(None)
    }
}

async fn get_schema_version(pool: &DbPool) -> Result<Option<i32>> {
    // Check if k_vars table exists
    let table_exists = sqlx::query(
        "SELECT EXISTS(SELECT 1 FROM information_schema.tables WHERE table_name = 'k_vars')",
    )
    .fetch_one(pool)
    .await?
    .get::<bool, _>(0);

    if !table_exists {
        return Ok(None);
    }

    // Get schema version from k_vars table
    let result = sqlx::query("SELECT value FROM k_vars WHERE key = 'schema_version'")
        .fetch_optional(pool)
        .await?;

    match result {
        Some(row) => {
            let version_str: String = row.get("value");
            let version = version_str
                .parse::<i32>()
                .map_err(|_| anyhow::anyhow!("Invalid schema version format: {}", version_str))?;
            Ok(Some(version))
        }
        None => Ok(None),
    }
}

async fn execute_ddl(ddl: &str, pool: &DbPool) -> Result<()> {
    // Split DDL into individual statements and execute each one
    // This matches the Simply Kaspa Indexer implementation pattern
    for statement in ddl.split(";").filter(|stmt| !stmt.trim().is_empty()) {
        sqlx::query(statement).execute(pool).await?;
    }
    Ok(())
}

async fn verify_schema_setup(pool: &DbPool) -> Result<()> {
    info!("Verifying schema setup");

    // Check k_vars table and schema version
    let version = get_schema_version(pool).await?;
    match version {
        Some(v) if v == SCHEMA_VERSION => {
            info!("  ✓ k_vars table and schema version {} verified", v);
        }
        Some(v) => {
            error!(
                "  ✗ Incorrect schema version: expected {}, found {}",
                SCHEMA_VERSION, v
            );
            return Err(anyhow::anyhow!("Schema version mismatch"));
        }
        None => {
            error!("  ✗ k_vars table or schema_version not found");
            return Err(anyhow::anyhow!("Schema version not found"));
        }
    }

    // Check K protocol tables
    let tables = vec![
        "k_contents",
        "k_broadcasts",
        "k_votes",
        "k_mentions",
        "k_blocks",
        "k_follows",
        "k_hashtags",
    ];
    let mut all_verified = true;

    for table in &tables {
        let table_exists = sqlx::query(
            "SELECT EXISTS(SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
        )
        .bind(table)
        .fetch_one(pool)
        .await?
        .get::<bool, _>(0);

        if table_exists {
            info!("  ✓ Table '{}' verified", table);
        } else {
            error!("  ✗ Table '{}' NOT found", table);
            all_verified = false;
        }
    }

    // Check transaction trigger
    let function_exists =
        sqlx::query("SELECT EXISTS(SELECT 1 FROM pg_proc WHERE proname = 'notify_transaction')")
            .fetch_one(pool)
            .await?
            .get::<bool, _>(0);

    let trigger_exists = sqlx::query(
        "SELECT EXISTS(SELECT 1 FROM pg_trigger WHERE tgname = 'transaction_notify_trigger')",
    )
    .fetch_one(pool)
    .await?
    .get::<bool, _>(0);

    if function_exists && trigger_exists {
        info!("  ✓ Transaction notification system verified");
    } else {
        error!("  ✗ Transaction notification system verification failed");
        all_verified = false;
    }

    // Explicit verification of all 37 expected K protocol indexes
    let expected_indexes = vec![
        // k_broadcasts indexes
        "idx_k_broadcasts_transaction_id",
        "idx_k_broadcasts_sender_pubkey",
        "idx_k_broadcasts_block_time",
        // k_votes indexes
        "idx_k_votes_transaction_id",
        "idx_k_votes_sender_pubkey",
        "idx_k_votes_sender_signature_unique",
        "idx_k_votes_post_id",
        "idx_k_votes_vote",
        "idx_k_votes_block_time",
        "idx_k_votes_post_id_sender",
        // k_mentions indexes
        "idx_k_mentions_comprehensive",
        "idx_k_mentions_content_id",
        "idx_k_mentions_mentioned_pubkey",
        // k_blocks indexes
        "idx_k_blocks_sender_signature_unique",
        "idx_k_blocks_sender_blocked_user_unique",
        "idx_k_blocks_sender_pubkey",
        "idx_k_blocks_blocked_user_pubkey",
        "idx_k_blocks_block_time",
        // k_contents indexes
        "idx_k_contents_transaction_id",
        "idx_k_contents_sender_signature_unique",
        "idx_k_contents_sender_pubkey",
        "idx_k_contents_block_time",
        "idx_k_contents_replies",
        "idx_k_contents_reposts",
        "idx_k_contents_quotes",
        "idx_k_contents_feed_optimized",
        "idx_k_contents_content_type",
        "idx_k_contents_sender_content_type",
        // k_follows indexes
        "idx_k_follows_sender_signature_unique",
        "idx_k_follows_sender_followed_user_unique",
        "idx_k_follows_followed_user_pubkey",
        "idx_k_follows_sender_pubkey",
        "idx_k_follows_block_time",
        // k_hashtags indexes
        "idx_k_hashtags_by_hashtag_time",
        "idx_k_hashtags_pattern",
        "idx_k_hashtags_trending",
        "idx_k_hashtags_by_hashtag_sender",
    ];

    let mut missing_indexes = Vec::new();

    for index_name in &expected_indexes {
        let index_exists =
            sqlx::query("SELECT EXISTS(SELECT 1 FROM pg_indexes WHERE indexname = $1)")
                .bind(index_name)
                .fetch_one(pool)
                .await?
                .get::<bool, _>(0);

        if index_exists {
            info!("  ✓ Index '{}' verified", index_name);
        } else {
            error!("  ✗ Index '{}' NOT found", index_name);
            missing_indexes.push(index_name);
            all_verified = false;
        }
    }

    // Verify total count matches expected (37 indexes)
    let index_count = sqlx::query("SELECT COUNT(*) FROM pg_indexes WHERE indexname LIKE 'idx_k_%'")
        .fetch_one(pool)
        .await?
        .get::<i64, _>(0);

    if index_count == 37 {
        info!(
            "  ✓ Expected 37 K protocol indexes verified (found {})",
            index_count
        );
    } else {
        error!("  ✗ Expected 37 K protocol indexes, found {}", index_count);
        all_verified = false;
    }

    if !missing_indexes.is_empty() {
        error!("  ✗ Missing indexes: {:?}", missing_indexes);
    }

    // Verify k_contents table (v4+)
    if version.unwrap_or(0) >= 4 {
        info!("Verifying k_contents table");

        let k_contents_count: i64 = sqlx::query("SELECT COUNT(*) FROM k_contents")
            .fetch_one(pool)
            .await?
            .get(0);

        info!("  k_contents records: {}", k_contents_count);
        info!("  ✓ k_contents is the unified content table (k_posts and k_replies removed in v6)");
    }

    if all_verified {
        info!("✓ Schema setup verification PASSED");
    } else {
        error!("✗ Schema setup verification FAILED");
        return Err(anyhow::anyhow!("Schema setup verification failed"));
    }

    Ok(())
}
