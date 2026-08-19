# KaChat `kchat:` Protocol Migration — Client Implementation Spec

**Audience:** the AI/engineer updating the KaChat clients (iOS, Android, PC/desktop).
**Goal:** move every client from the borrowed Kasia (`ciph_msg:`) and K-social (`k:1:`) on-chain
prefixes to KaChat's own identifier **`kchat:`**, so KaChat is its own cross-platform network on
Kaspa (one app, all platforms, users talk to each other — no longer to the Kasia/K networks).

**Server status:** DONE and deployed. The indexer already **dual-reads** — it accepts `kchat:` and
still reads the legacy prefixes for old on-chain history. It also **dual-accepts** the push-auth
domain. So the server is ready; the clients are the remaining half. Nothing changes for users until
the clients start *writing* `kchat:`.

---

## 0. TL;DR of the wire change

| Content | Old prefix (still readable) | New prefix to WRITE |
|---|---|---|
| 1:1 chat, handshake, payment, group msg/control, broadcast | `ciph_msg:1:<op>:…` | `kchat:1:<op>:…` |
| Versionless sealed message/handshake | `ciph_msg:<hex>` | `kchat:<hex>` |
| KaPosts (post/reply/quote/vote/follow/block/…) | `k:1:<action>:…` | `kchat:1:<action>:…` |
| Push-registration signature domain | `kasia-push-auth:*` | `kchat-push-auth:*` |

**The rule is: swap the prefix, keep everything after it byte-for-byte identical.** No op names,
field orders, encodings, hashing, or crypto change.

---

## 1. CRITICAL — rollout order (read before writing)

A client that **writes** `kchat:` produces content that a client which only **reads** the old
prefixes cannot see. To avoid a window where updated users go invisible to not-yet-updated users,
roll out in this order on **every** platform:

1. **Phase 1 — READ both.** Ship a release that parses **both** `kchat:` *and* the legacy prefix
   everywhere the client currently reads chain payloads. Change nothing about what it writes yet.
   (This mirrors what the server already does.)
2. **Phase 2 — WRITE `kchat:`.** Only after Phase 1 is broadly adopted, flip the client to *write*
   `kchat:`. From here on, new content is KaChat-network-only.

If you ship both phases in one release, that's fine too — just understand that users on **older app
versions** (still legacy-read-only) won't see the new `kchat:` messages until they update. Keep the
legacy readers in the client indefinitely so historical chats/posts still render.

> Interop note: this is a deliberate, one-way break from the Kasia messaging network and the K
> social network. Reading old content keeps working; writing goes KaChat-only.

---

## 2. Chat protocol (DMs, handshakes, payments, groups, broadcasts)

On-chain the payload is the **UTF-8 bytes of the string below**. Only the leading `kchat:` /
`ciph_msg:` differs between new and old; the remainder is unchanged.

| Op | Format (write with `kchat:` root) |
|---|---|
| Contextual message (DM) | `kchat:1:comm:{alias}:{SealedContextualMessage_as_hex}` |
| Payment | `kchat:1:payment:{SealedPayment_as_hex}`  (legacy short form `kchat:1:pay:…` also accepted) |
| Handshake (V2) | `kchat:1:handshake:{SealedHandshake_as_hex}` |
| Self-stash | `kchat:1:self_stash:{key}:{sealed_hex}`  or  `kchat:1:self_stash:{sealed_hex}` |
| Group message | `kchat:1:gcomm:{blinded_group_id}:{epoch}:{sender_id}:{sender_pub}:{msg_id}:{ciphertext}:{signature}` |
| Group control | `kchat:1:gctl:{hex}` (legacy)  or  `kchat:1:gctl:{recipient_xonly_pubkey}:{hex}` (addressed) |
| Broadcast | `kchat:1:bcast:{channel}:{content}` |
| Versionless sealed msg/handshake | `kchat:{SealedMessage_as_hex}` |

Field semantics, hex encodings, the sealed-payload crypto (ECDH → HKDF-SHA256 → ChaCha20-Poly1305),
blinded group IDs, and signatures are all **unchanged**. Do not re-hash or re-encode anything — only
the 6-byte `kchat:` (vs 9-byte `ciph_msg:`) prefix changes.

---

## 3. KaPosts protocol (the K social actions)

Swap the root only:

- **Write:** `kchat:1:<action>:…`  (was `k:1:<action>:…`)
- **Read:** accept **both** `kchat:1:` and `k:1:`.

`<action>` and everything after it are **identical** to today (e.g. post, reply, quote, vote,
follow, block, unblock, broadcast/profile, …). The invisible **`U+2060` (WORD JOINER) exclusivity
marker** you already prepend inside the decoded KaPosts body **stays exactly as-is** — keep writing
it; it's already KaChat-owned and the server still requires it for KaPosts.

---

## 4. Push registration — signature domain

Device push registration (`POST /v1/push/register` and `/v1/push/update`) is authenticated with a
BIP-340 Schnorr signature over a canonical, `\n`-joined preimage. **The only change is the value of
the first `domain=` line:**

| Signature | Old domain | New domain to sign |
|---|---|---|
| Wallet auth, LegacyV1 / TransitionalGroups | `kasia-push-auth:v1` | `kchat-push-auth:v1` |
| Wallet auth, V2 | `kasia-push-auth:v2` | `kchat-push-auth:v2` |
| Device-key auth | `kasia-push-device-auth:v1` | `kchat-push-device-auth:v1` |

Preimage line order (unchanged except the domain value):

```
domain=kchat-push-auth:v2      ← only this value changes
auth_version=2                 ← V2 only
nonce=…
method=…
path=…
device_token_hash=…
watched_addresses_hash=…
watched_group_ids_hash=…       ← Transitional + V2
capabilities_hash=…            ← V2 only
primary_address=…
aliases_hash=…
wallet_pubkey=…
wallet_address=…
timestamp_ms=…
expires_at_ms=…
```

Sign `SHA256(preimage_bytes)` exactly as today. **No coordination window needed:** the server
already dual-accepts, verifying the new `kchat-` domain first and falling back to the legacy
`kasia-` domain, so devices signing either domain authenticate during the transition. Switch clients
to the `kchat-` domains whenever convenient.

---

## 5. What does NOT change

- Sealed-message / handshake / group crypto, key derivation, blinded group IDs, signatures.
- Op/action names, field order, hex encodings, JSON shapes after the prefix.
- The `U+2060` KaPosts exclusivity marker.
- Push **notification** payloads the client *receives* (types `contextual`/`handshake`/`payment`/
  `group_message`/`group_control`/`broadcast`/`kaposts`/`address_activity`) — those are server→client
  and unaffected. Only the registration **signing domain** changes.
- Server API endpoints, ports, and request/response schemas.

---

## 6. Per-client checklist

For **each** of iOS, Android, and PC/desktop:

- [ ] **Find every WRITE site** that builds a Kaspa tx payload with `ciph_msg:` or `k:1:` and change
      the literal to `kchat:` / `kchat:1:` (keep the rest identical). Search for: `ciph_msg:`,
      `"k:1:"`, `6b3a` / `6970685f6d7367` (hex), any prefix-builder constant.
- [ ] **Find every READ/parse site** and make it accept **both** the new and legacy prefix (dual-read),
      so history and not-yet-migrated peers still render.
- [ ] **Push registration:** change the signed preimage's `domain=` value to the `kchat-push-auth:*`
      / `kchat-push-device-auth:v1` strings. Leave the rest of the preimage untouched.
- [ ] Keep the `U+2060` marker on KaPosts bodies.
- [ ] Confirm the app's on-chain **read** filter/subscription also matches the `kchat:` prefix (not
      just the writer), or you'll miss your own new messages.
- [ ] Ship **Phase 1 (read-both)** first, then **Phase 2 (write-kchat)** — see §1.

Known Android touch-points (verify against current source): the FCM handler
(`KaChatFirebaseMessagingService`) and `PushRegistrationManager` (auth preimage). iOS/PC have the
equivalent chain-writer, chain-reader, and push-registration-signer — locate them the same way.

---

## 7. Test vectors

Payloads are the raw UTF-8 bytes of the string (this is what goes in the Kaspa tx payload):

| String | Hex prefix (first bytes) |
|---|---|
| `kchat:` | `6b 63 68 61 74 3a` → `6b636861743a` |
| `kchat:1:` | `6b636861743a313a` |
| `kchat:1:comm:` | `6b636861743a313a636f6d6d3a` |
| `kchat:1:bcast:` | `6b636861743a313a62636173743a` |

Server-side acceptance (already deployed) for reference — a payload is picked up if it starts with:
- `kchat:1:` (`6b636861743a313a`) — canonical (covers KaChat posts **and** broadcasts), **or**
- legacy `k:1:` (`6b3a313a`), **or**
- legacy `ciph_msg:1:bcast:` (`636970685f6d73673a313a62636173743a`).

Round-trip check: a message written as `kchat:1:comm:{alias}:{hex}` must decode to the identical
`SealedContextualMessage` as the old `ciph_msg:1:comm:{alias}:{hex}` — only the prefix differs.

---

*Server reference (for context): commit that shipped the dual-read + dual-accept is on
`main` of the kachat-indexer repo. The indexer parses chats in
`kasia-indexer/protocol/src/operation/deserializer.rs`, KaPosts in
`kachat-transaction-processor/src/k_protocol.rs`, and push-auth in
`kasia-indexer/indexer/src/api/v1/push.rs`.*
