-- Migration: v2_to_v3
-- Description: One person per x-only key for profiles, follows and blocks (audits IDX-031, IDX-032).
-- The 02/03 parity byte takes no part in BIP340 signature checks, so 02<x> and 03<x> are the same
-- person. Reads merge both forms since 12543eb, and this makes the write side agree. Rows already
-- written under the other form are collapsed first (the newest wins).
-- Date: 2026-10-20

-- Profiles: one broadcast row per person (a replayed broadcast under the flipped parity byte
-- could otherwise never be replaced by its owner, IDX-031).
DELETE FROM k_broadcasts a USING k_broadcasts b
WHERE substring(b.sender_pubkey from 2) = substring(a.sender_pubkey from 2)
  AND (b.block_time > a.block_time
       OR (b.block_time = a.block_time AND b.transaction_id > a.transaction_id));

-- Follows: collapse variant pairs of the same target, then dedupe on the x-only target so an
-- unfollow with either form removes the follow (IDX-032).
DELETE FROM k_follows a USING k_follows b
WHERE a.sender_pubkey = b.sender_pubkey
  AND substring(a.followed_user_pubkey from 2) = substring(b.followed_user_pubkey from 2)
  AND (b.block_time > a.block_time OR (b.block_time = a.block_time AND b.id > a.id));

DROP INDEX IF EXISTS idx_k_follows_sender_followed_user_unique;

CREATE UNIQUE INDEX IF NOT EXISTS idx_k_follows_sender_followed_xonly_unique
    ON k_follows(sender_pubkey, (substring(followed_user_pubkey from 2)));

-- Blocks: the same.
DELETE FROM k_blocks a USING k_blocks b
WHERE a.sender_pubkey = b.sender_pubkey
  AND substring(a.blocked_user_pubkey from 2) = substring(b.blocked_user_pubkey from 2)
  AND (b.block_time > a.block_time OR (b.block_time = a.block_time AND b.id > a.id));

DROP INDEX IF EXISTS idx_k_blocks_sender_blocked_user_unique;

CREATE UNIQUE INDEX IF NOT EXISTS idx_k_blocks_sender_blocked_xonly_unique
    ON k_blocks(sender_pubkey, (substring(blocked_user_pubkey from 2)));

-- Every statement above is a no-op the second time (DROP IF EXISTS, CREATE IF NOT EXISTS,
-- deletes that find nothing), so a run that got this far once can safely run again.
UPDATE k_vars SET value = '3' WHERE key = 'schema_version'
