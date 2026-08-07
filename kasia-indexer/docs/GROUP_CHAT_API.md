# Group chat indexer API and KaChat integration

This branch rebases group persistence onto `feature/apns-support` and keeps the existing 1:1,
payment, self-stash, APNs, and metrics behavior compatible. Group support is additive: clients that
do not send group fields continue to use the original push-auth preimage and never receive group
pushes.

## What changed from the initial PR

| Area | Initial PR | This branch |
| --- | --- | --- |
| Invitation/control discovery | Public `ginv` tag plus sender-indexed `gctl` | No invitation beacon. New addressed `gctl` carries the recipient x-only pubkey and is queryable by recipient; legacy `gctl` remains queryable by sender. |
| Control push routing | Fell back to every registered device when no admin watcher matched | Addressed controls go only to devices whose authenticated `primary_address` is the recipient. Legacy controls go only to capable devices already watching the sender. There is no global fallback. |
| Group push opt-in | Group IDs alone | `group_v1` capability gate. The transitional KaChat 3.0 registration is recognized and gets this capability implicitly. |
| Push authentication | Always added `watched_group_ids_hash`, invalidating older signatures | Preserves legacy v1 exactly, detects the transitional group-v1 field even when its array is empty, and adds an explicit v2 shape for new clients. |
| Group-message validation | Permissive fields; malformed IDs became all-zero keys | Exact field count, lengths, decimal epoch, and hex are required. `sender_pub` must match the transaction sender, and a blinded ID is pinned to its first validated sender key. |
| Pagination | Inclusive `block_time` only | `block_time` remains for older clients; every item now also returns an opaque composite `cursor` for lossless pagination when timestamps collide. |
| Metrics | Conflicted with the APNs branch's Prometheus route | JSON stays at `/metrics`; Prometheus is at `/metrics/prometheus`. |

The stable blinded-ID metadata privacy limitation remains: the indexer can correlate repeated use of
the same blinded ID. It cannot recover the real group ID, keys, roster, or plaintext.

## On-chain formats

### Group message

```text
ciph_msg:1:gcomm:{blinded_group_id}:{epoch}:{sender_id}:{sender_pub}:{msg_id}:{ciphertext}:{signature}
```

- `blinded_group_id`, `sender_id`, and `sender_pub`: 32-byte hex.
- `epoch`: unsigned decimal `u64`.
- `msg_id`: 24-byte hex (`device_id || counter_le`).
- `ciphertext`: non-empty, even-length hex.
- `signature`: 64-byte Schnorr signature hex.

The indexer verifies the wire shape and that `sender_pub` is the x-only key of the transaction
sender. It cannot verify the inner `gcomm` signature because its signed AAD contains the unblinded
group ID, which is deliberately not published. KaChat must still verify the inner signature,
sender ID, roster membership, epoch, AEAD tag, and replay state after retrieval or push.

### Group control

Legacy KaChat payloads remain valid:

```text
ciph_msg:1:gctl:{encrypted_payload}
```

New KaChat clients should send recipient-addressed control payloads:

```text
ciph_msg:1:gctl:{recipient_xonly_pubkey}:{encrypted_payload}
```

Both fields are hex; `recipient_xonly_pubkey` is exactly 32 bytes. The API returns
`message_payload` in the legacy-compatible form: hex encoding of the ASCII encrypted-payload hex.
In other words, the recipient routing prefix is not included in `message_payload`, so the existing
`ciph_msg:1:gctl:` reconstruction/decryption path still works.

In `GroupChatService.sendControlMessage`, the new wire payload is conceptually:

```swift
let payload = "ciph_msg:1:gctl:\(recipientXOnlyPubKey.hexString):\(encrypted.toHex())"
```

Use this for `gctl_root`, epoch rotation, add-member, and remove-member delivery. The encrypted
control document and its admin signature remain unchanged.

## HTTP APIs

All examples assume the indexer base URL is already configured in KaChat.

### Read group messages

```http
GET /group-messages/by-blinded-group-id?blinded_group_id={64_hex}&limit=50
GET /group-messages/by-blinded-group-id?blinded_group_id={64_hex}&limit=50&cursor={cursor}
```

KaChat must query once per known member because the blinded ID is sender-specific. Responses keep
all fields from the initial PR and add `cursor`:

```json
{
  "tx_id": "...",
  "sender": "kaspa:...",
  "blinded_group_id": "...",
  "block_time": 1750000000000,
  "cursor": "...",
  "accepting_block": "...",
  "accepting_daa_score": 123,
  "message_payload": "..."
}
```

Pass the last processed item's `cursor` on the next request. Treat it as opaque. `block_time` is
still accepted for older clients, but new code should persist cursors because many transactions can
share a timestamp.

### Read control by recipient

Use this before a wallet has any local groups; it solves first-invite discovery without knowing the
admin address:

```http
GET /group-control/by-recipient?recipient={wallet_address}&limit=50
GET /group-control/by-recipient?recipient={wallet_address}&limit=50&cursor={cursor}
```

The response uses the same `GroupControlResponse` as the sender query and contains `sender`,
`recipient`, `cursor`, acceptance data, and `message_payload`. Decrypt each payload with the wallet
private key, validate the `gctl_root` admin signature, and deduplicate by `tx_id` before applying it.

Suggested KaChat startup flow:

1. Load the active wallet and the saved recipient-control cursor.
2. Page `/group-control/by-recipient` until fewer than `limit` items are returned.
3. Decrypt and validate each control document.
4. Create or update the local group only after signature and roster validation succeeds.
5. Persist the last successfully processed opaque cursor.

### Read control by sender

```http
GET /group-control/by-sender?sender={admin_address}&limit=50
GET /group-control/by-sender?sender={admin_address}&limit=50&cursor={cursor}
```

Keep this query for existing groups and for legacy controls. Addressed controls are also present so
old catch-up logic remains useful; ECIES decryption naturally ignores controls for other members.

For source compatibility with an older indexer, make new response properties optional in KaChat:

```swift
struct GroupControlResponse: Codable {
    let txId: String
    let sender: String
    let recipient: String?
    let blockTime: UInt64
    let cursor: String?
    let acceptingBlock: String?
    let acceptingDaaScore: UInt64?
    let messagePayload: String
}

struct GroupMessageResponse: Codable {
    // Existing properties omitted here.
    let cursor: String?
}
```

## Push registration and compatibility

New clients send these additive top-level registration/update fields:

```json
{
  "watched_group_ids": ["64-hex-id-for-each-remote-sender"],
  "capabilities": ["group_v1"],
  "auth": {
    "auth_version": 2
  }
}
```

Continue sending the normal non-empty `watched_addresses` array. `watched_group_ids` may be empty;
the explicit `group_v1` capability is what lets a brand-new wallet receive recipient-addressed
control pushes before it knows any group IDs.

`primary_address` must equal the authenticated wallet address when `group_v1` is enabled. A client
may register `group_v1` with an empty `watched_group_ids` array; this is how a brand-new wallet can
receive recipient-addressed `gctl_root` pushes before it knows any group IDs. The request must be
signed even when the server is configured in `legacy` or `mixed` push-auth mode.

Compatibility rules are:

- No `auth_version`, no `watched_group_ids` field, no capabilities: original
  `kasia-push-auth:v1` preimage, byte-for-byte compatible with older KaChat.
- No `auth_version`, a present `watched_group_ids` field (empty or non-empty), and no capabilities:
  transitional KaChat 3.0 preimage with `watched_group_ids_hash`; the indexer infers `group_v1`.
- `auth_version: 2`: `kasia-push-auth:v2`, with both group IDs and capabilities signed.

This distinction is intentional: JSON field presence lets the indexer distinguish pre-group
clients from KaChat 3.0 when both currently have zero watched groups.

### Client compatibility

- Pre-group KaChat continues to register and sign exactly as before; it ignores additive response
  properties and never receives group pushes.
- The current `KaChat3.0i` branch continues to send legacy `gctl`, query control by sender, and use
  its transitional signed registration. Those paths remain supported.
- Updated KaChat should send recipient-addressed `gctl`, discover controls by recipient, paginate
  with opaque cursors, and use auth v2. The sender query and legacy `block_time` pagination remain
  available during a staged rollout.

For v2, canonicalize group IDs and capabilities by trimming, lowercasing, deduplicating, sorting,
and joining with `\n`. The signed lines are:

```text
domain=kasia-push-auth:v2
auth_version=2
nonce={nonce}
method={HTTP_METHOD}
path={request_path}
device_token_hash={sha256_hex(normalized_device_token)}
watched_addresses_hash={sha256_hex(canonical_addresses_joined_with_newlines)}
watched_group_ids_hash={sha256_hex(canonical_group_ids_joined_with_newlines)}
capabilities_hash={sha256_hex(canonical_capabilities_joined_with_newlines)}
primary_address={normalized_primary_address}
aliases_hash={sha256_hex(canonical_aliases_joined_with_newlines)}
wallet_pubkey={lowercase_xonly_pubkey_hex}
wallet_address={normalized_wallet_address}
timestamp_ms={timestamp_ms}
expires_at_ms={expires_at_ms}
```

Join the lines with `\n`, SHA-256 the UTF-8 bytes, and Schnorr-sign that digest exactly as the v1
implementation does.

Group push payload types are `group_message` and `group_control`. `group_message` includes
`blinded_group_id`; both types may include the encrypted payload when it fits the APNs payload cap.
Always use `tx_id` for deduplication and run the same validation/decryption path as catch-up sync.

## Operational note

Update the VictoriaMetrics scrape target from `/metrics` to `/metrics/prometheus` when deploying
this image. `/metrics` remains the JSON endpoint used by KaChat health/diagnostic code.
