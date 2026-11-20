# KaPosts: list follows in get-notifications (2026-11-20)

**For:** the indexer session (`kachat-transaction-processor`, `kachat-webserver`). **From:** the KaPost
iOS session (`KaspaSilver/KaPost-iPhone`).

## What's wrong

The owner was followed and nothing showed on KaPost's Notifications screen.

`/get-notifications` (and `/get-notifications-count`) read only `k_mentions`. A follow never
writes a `k_mentions` row: `k_protocol.rs`'s `"follow"` arm inserts into `k_follows` and sends the
`kaposts` push (`notify_kaposts(..., "follow", ...)`), and that's all. So a follow can ring as a
push, but it never appears in the list, never counts toward the unseen badge, and the app's
in-app poller never sees it. That poller reads `get-notifications` too.

Likes, replies, quotes, tips and mentions all work, because each writes a `k_mentions` row.

## What the app expects (already built, no app change needed)

KaPost, KaChat's old KaPosts code and the Android KaPost all decode a follow entry as below. It's
the same `NotificationPost` shape as the other kinds:

| Field | Value for a follow |
|---|---|
| `id` | the follow transaction id (hex) |
| `userPublicKey` | the follower's pubkey |
| `contentType` | `"follow"` |
| `timestamp` | the follow's block time (ms), as the other kinds |
| `postContent` | empty / null |
| `contentId`, `voteType`, `tipAmountSompi` | null |

The row reads "<name> followed you". A tap opens the follower's profile, since a follow has no
post. It's hidden when the person turned off the Follows notification switch, which is the same
`kaposts_notify_follows` setting the push already honours.

## The change

1. **Processor, `"follow"` arm** (k_protocol.rs, around the `INSERT INTO k_follows`): when the
   insert actually adds a row (`rows_affected() > 0`, the same branch that sends the push), also
   `INSERT INTO k_mentions (content_id, content_type, mentioned_pubkey, block_time, sender_pubkey)`
   with content_id = the follow tx, content_type = `'follow'`, mentioned_pubkey = the followed
   user's pubkey, and sender_pubkey = the follower. A repeat follow that `DO NOTHING`s adds no
   second row.
2. **Processor, `"unfollow"` arm:** delete that person's `'follow'` `k_mentions` row for the
   followed user (x-only compare on both keys, `block_time <=` the unfollow's, like the
   `k_follows` delete), so an unfollow takes the notification away, as an un-vote does.
3. **Webserver, `get_notifications`:**
   - Add a `LEFT JOIN k_follows f ON fn.content_type = 'follow' AND fn.content_id = f.transaction_id`.
   - Give `follow` a non-null `id` (`WHEN 'follow' THEN f.id`), so the orphan skip
     (`id` null → `continue`) doesn't drop the row.
   - Its `base64_encoded_message` is `''`.
   - Add a `content_type == "follow"` branch that emits the entry above. Any record shape is
     fine as long as the handler serializes `contentType: "follow"` with `contentId: null`.
     For example, a `ContentRecord::Vote` with an empty `post_id` and `vote`, with the handler
     then setting `content_type = "follow"`, as it already does for tips.
4. **`get_notification_count`** needs no change: it counts every `k_mentions` row, so follows
   count once step 1 writes them.
5. **Backfill:** existing follows have no `k_mentions` row. Insert one for each live `k_follows`
   row that doesn't have one yet (content_id = its transaction_id, `'follow'`, followed →
   mentioned, sender → sender, its block_time). That way the follow the owner just got, and
   older ones, show up without anyone re-following.

The account purge (`DELETE FROM k_mentions WHERE sender_pubkey = $1`) already covers follow
rows, and the block filter in the query already hides blocked followers.

## Testing

- Follow account B from account A in KaPost. B's Notifications shows "A followed you" at the
  top, with the person icon, and tapping it opens A's profile.
- `/get-notifications-count` for B goes up by one.
- A unfollows: the row is gone on B's next load.
- A follows again: one row, not two.
- Turn off Notifications > Follows on B: the row is hidden (client side) and no push comes.
- After the backfill, a follow made before this change appears at its original time.
