//! Emergency recovery tool for the chat (kasia) indexer's fjall store.
//!
//! When the tiny `metadata` partition's LSM manifest gets a torn write (process killed
//! mid-compaction), fjall fails to open the keyspace with `Storage(Unrecoverable)` and the
//! chat service flaps forever in supervisord BACKOFF. Because the `metadata` partition holds
//! only three small values (LatestBlockCursor, LatestAcceptingBlockCursor, DBVersion) and the
//! actual messages live in OTHER partitions, the safe fix is to delete the corrupt `metadata`
//! partition and let fjall recreate it — BUT you must then set `DBVersion = 1`, otherwise a
//! fresh (version 0) metadata makes the indexer re-run the v0->v1 daa_index migration on
//! already-migrated data and refuse to start. With DBVersion=1 and no cursor, the indexer
//! backfills from the node pruning point to tip on next boot (idempotent, no message loss).
//!
//! Usage (run AFTER removing the corrupt partition dir, inside the container as root):
//!   rm -rf /app/data/mainnet/partitions/metadata
//!   ./reseed-metadata /app/data/mainnet
//!
//! Mirrors indexer/src/main.rs keyspace open and indexer-db/src/metadata.rs exactly:
//!   Config::new(path).max_write_buffer_size(512 MiB).open_transactional()
//!   open_partition("metadata", block_size(1024).compression(None))
//!   DBVersion key = [2], value = u32 little-endian.
use fjall::{CompressionType, Config, PartitionCreateOptions, PersistMode};

const DB_VERSION_KEY: [u8; 1] = [2u8]; // MetadataKey::DBVersion
const CURRENT_DB_VERSION: u32 = 1;

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: reseed-metadata <db_path>   (e.g. /app/data/mainnet)");
            eprintln!("NOTE: remove the corrupt partitions/metadata dir BEFORE running this.");
            std::process::exit(2);
        }
    };

    eprintln!("opening keyspace at {path} (recovers all partitions; metadata recreated fresh)...");
    let config = Config::new(&path).max_write_buffer_size(512 * 1024 * 1024);
    let ks = config
        .open_transactional()
        .expect("open_transactional failed — is the path right and partitions/metadata removed?");
    let part = ks
        .open_partition(
            "metadata",
            PartitionCreateOptions::default()
                .block_size(1024)
                .compression(CompressionType::None),
        )
        .expect("open metadata partition failed");

    let before = part.get(DB_VERSION_KEY).expect("read db_version failed");
    eprintln!("db_version raw before: {:?}", before.as_ref().map(|b| b.to_vec()));

    part.insert(DB_VERSION_KEY, CURRENT_DB_VERSION.to_le_bytes())
        .expect("write db_version failed");
    ks.persist(PersistMode::SyncAll).expect("persist failed");

    let after = part.get(DB_VERSION_KEY).expect("re-read db_version failed");
    eprintln!("db_version raw after : {:?}", after.as_ref().map(|b| b.to_vec()));
    eprintln!("OK: metadata partition recreated and db_version set to {CURRENT_DB_VERSION}");
}
