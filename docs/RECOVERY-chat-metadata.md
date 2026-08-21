# Recovery: chat indexer `metadata` partition corruption

**Symptom.** The `chat` program (the vendored kasia indexer) in the `kachat-app` container keeps
restarting and never binds its API (`:8600` refused). `docker exec kachat-app supervisorctl status`
shows:

```
chat    BACKOFF   Exited too quickly (process log may have details)
```

Running the binary directly shows the real error at the end of a ~1-minute recovery:

```
ERROR lsm_tree::tree: Recovered less segments than expected: [ ... ]
Error: FjallError: Storage(Unrecoverable)
```

**Cause.** The chat indexer's embedded **fjall** store has a tiny `metadata` partition that is
compacted periodically (`major_compact()`, ~every 6h). If the process is killed **mid-compaction**
— e.g. a deploy/recreate that SIGKILLs it before it flushes, or a crash — the LSM `levels` manifest
can end up referencing segments that no longer exist on disk, which fjall cannot reconcile. Because
`metadata` is the last partition recovered on open, startup runs the full recovery and then dies at
the very end, every time.

**Why your data is safe.** The `metadata` partition is ~32 KB and holds only three values
(`LatestBlockCursor`, `LatestAcceptingBlockCursor`, `DBVersion` — see
`kasia-indexer/indexer-db/src/metadata.rs`). **All messages live in other partitions and are not
touched by this recovery.** When the cursor is missing, the indexer backfills from the node's
pruning point to the current tip (`block_processor::handle_first_connect`), which covers any
realistic outage; re-indexing is idempotent (keyed by tx id), so there is **no message loss**.

## Prevention (already in the repo)

- `docker/kachat/supervisord.conf` → `[program:chat]` sets `stopwaitsecs=45`.
- `docker/kachat/compose.yaml` and `docker/kachat/selfhost/compose.yaml` → `kachat-app` sets
  `stop_grace_period: 60s` (must exceed `stopwaitsecs`).

Together these let the chat indexer's orderly SIGINT shutdown finish flushing before Docker
SIGKILLs the container on a recreate. This closes the **deploy/recreate** torn-write hole. A
crash *during* normal operation is not covered — if that recurs, capture the dying process output
(a panic line, or `dmesg` for an OOM kill) to find the real trigger.

## Recovery procedure (~5 minutes)

Paths: the store is `/app/data/mainnet` inside the container = `/home/vahome/kachat-import/mainnet`
on the host (overlay bind mount). Files are root-owned; operate **inside the container as root**.

1. **Stop the flapping service** and confirm nothing holds the store:
   ```bash
   docker exec kachat-app supervisorctl stop chat
   docker exec kachat-app sh -c 'ps aux | grep -i chat-indexer | grep -v grep || echo "none running"'
   ```

2. **Back up** the metadata partition + journals (tiny), then move the archive off the store dir:
   ```bash
   TS=$(date +%Y%m%d-%H%M%S)
   docker exec kachat-app sh -c "tar czf /app/data/mainnet/_bk_${TS}.tgz -C /app/data/mainnet partitions/metadata journals version && chmod 644 /app/data/mainnet/_bk_${TS}.tgz"
   mv "/home/vahome/kachat-import/mainnet/_bk_${TS}.tgz" "/home/vahome/kachat-store-metadata-backup-${TS}.tgz"
   gzip -t "/home/vahome/kachat-store-metadata-backup-${TS}.tgz" && echo "backup OK"
   ```

3. **Build the reseed tool** (host needs Rust + the musl target) and copy it into the container:
   ```bash
   rustup target add x86_64-unknown-linux-musl
   ( cd tools/reseed-metadata && cargo build --release --target x86_64-unknown-linux-musl )
   docker cp tools/reseed-metadata/target/x86_64-unknown-linux-musl/release/reseed-metadata kachat-app:/tmp/reseed-metadata
   docker exec kachat-app chmod +x /tmp/reseed-metadata
   ```

4. **Remove the corrupt partition and reseed** `db_version=1`:
   ```bash
   docker exec kachat-app rm -rf /app/data/mainnet/partitions/metadata
   docker exec kachat-app /tmp/reseed-metadata /app/data/mainnet
   # expect: "db_version raw after : Some([1, 0, 0, 0])" ... "OK: ... db_version set to 1"
   ```

5. **Restart chat** and watch it recover, backfill, and bind:
   ```bash
   docker exec kachat-app supervisorctl start chat
   # ~1 min recovery, then :8600 goes from refused (000) to responding (404 = server up)
   curl -s -o /dev/null -w '%{http_code}\n' http://localhost:8600/
   docker logs kachat-app --tail 50 | grep "Committed"   # "Last tx: <~now>" means caught up
   ```

6. **Clean up** and keep the backup until you're satisfied:
   ```bash
   docker exec kachat-app rm -f /tmp/reseed-metadata
   # /home/vahome/kachat-store-metadata-backup-*.tgz can be deleted once stable
   ```

If anything looks wrong at step 5, restore the backup over `partitions/metadata` + `journals` and
re-assess before retrying.
