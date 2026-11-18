# The KaChat apps: one family, four mobile apps (2026-11-18)

**For:** every indexer session. **From:** the indexer session, at the owner's request.

KaChat on mobile is no longer one app. It's four apps, each on iPhone and Android, and each has
its own repo, its own push topic and its own slice of this indexer. Read this before changing
anything an app depends on, and before deciding a feature is unused: a route one app dropped
may still be called by another.

## The apps

| App | What it is | iPhone repo | Android repo | iPhone APNs topic | Android `apns_topic` (package) | Pushes it may get |
|---|---|---|---|---|---|---|
| **KaChat** | 1:1 and group chats, payments in a chat | `KaChat-iPhone` | `KaChat-Android` | none sent (default `com.kachat.app`) | none sent | `contextual`, `handshake`, `payment`, `group_message`, `group_control` |
| **KaPost** | KaPosts (posts, replies, votes, follows) and Public Chats rooms | `KaPost-iPhone` | `KaPost-Android` | `com.kachat.kaposts` | `kapost.kachat.app` | `kaposts`, `broadcast` |
| **KaPay** | The wallet (formerly KaChat Wallet): Portfolio, Cold Storage, Address Book, `.kachat` names | `KaPay-iPhone` | `KaPay-Android` | `com.kachat.wallet` | `kapay.kachat.app` | `address_activity`, `name_event` |
| **KaPlay** | Chess (formerly KaChat Chess): tournaments and on-phone bots | `KaPlay-iPhone` | `KaPlay-Android` | never registers | never registers (`kaplay.kachat.app`) | none |

All repos are under `KaspaSilver`. The push rules live in `kasia-indexer/indexer/src/push.rs`
(`KNOWN_APP_TOPICS`, `topic_takes_push` and the `*_PUSH_TYPES` lists) and are explained in
`ANDROID_APP_TOPICS.md` and `KAPOSTS_PUSH_KINDS.md`. One wallet can be signed in to several of
these apps on one phone, so one `primary_address` can have a KaChat, a KaPost and a KaPay
registration at once. The topic decides which pushes each one gets.

Other clients of the same indexer:

- **KaChat Desktop** (`KaChat-Desktop`, desktop.kachat.app): a web app that holds all four
  apps in one, so it calls everything the mobile apps call.
- **KaPay Browser Extension** (`KaPay-Browser-Extension`): the wallet in Chrome, Brave, Edge and
  Firefox.
- **Kasia** (upstream messenger, not ours): reads and writes the same chat protocol
  (`ciph_msg:1:` prefixes), including handshakes. Keep it working.
- **Kaspa Quick Start** (the panel): reads stats and the KaChat admin API, not the app routes.

## What each app uses here

From the live proxy log over 48 hours (2026-11-16 to 18), by iPhone User-Agent. The Android apps
all send `okhttp/…`, so they can't be told apart in the log; their traffic is the four apps
together.

| App (iPhone User-Agent) | Services | Main routes |
|---|---|---|
| KaChat (`KaChat/…`) | chat indexer (:8600), push, names | `/contextual-messages/by-sender`, `/by-inbox`; `/handshakes/by-sender`, `/by-receiver`; `/self-stash/by-owner`; `/group-messages/*`, `/group-control/*`; `/identity/{address}`; `/v1/push/*` |
| KaPost (`KaPosts/…`) | content webserver (:3080), push, names | `/get-broadcasts`, `/get-notifications`, `/get-posts-watching`, `/get-replies`, `/get-users-following`, `/broadcast-channels`; `/profiles/{address}`, `/identity/batch`; `/v1/push/*` |
| KaPay (`KaChatWallet/…`) | names, profiles, push | `/names/*` (by-owner, status, expiring, grace, activity, per-name offers), `/offers/by-buyer/*`, `/market/listings`, `/profiles/{address}`; `/v1/push/*` |
| KaPlay (`KaChatChess/…`) | content webserver, chat indexer, names | `/chess/*` (leaderboard, player, tournaments, bots leaderboard); the `chess-arena` and `chess-bots` broadcast channels; it also reads `/contextual-messages/*`, `/handshakes/*` and `/self-stash/by-owner` for the account it shares with KaChat |

Rough volume over those 48 hours: Android (all four) about 43,000 requests, Desktop about
3,100, KaChat iPhone about 2,900, Kasia iOS about 250, KaPost iPhone about 80, KaPay iPhone
about 50, KaPlay iPhone about 35.

## What each app writes on chain

| App | Payloads | Indexed by |
|---|---|---|
| KaChat | `kchat:1:comm`, `kchat:1:dm` (tagged first contact, no handshake), `kchat:1:handshake` (old builds and Kasia only), `kchat:1:pay`, group `gcomm`/`gctl`, `kchat:1:self_stash` | `kasia-indexer` |
| KaPost | KaPosts `kchat:1:post` and friends, Public Chats rooms `kchat:1:bcast:<channel>` | `kachat-transaction-processor` → Postgres |
| KaPay | plain payments; `.kachat` covenant transactions; the account profile (`kchat:1:profile`) | names follower (`kachat-names-follower`) and its profiles follower; payments through `address_activity` pushes |
| KaPlay | tournaments on `kchat:1:bcast:chess-arena`, one bot-game result per game on `kchat:1:bcast:chess-bots` (`CHESS_BOT_LEADERBOARD.md`) | `kachat-transaction-processor` |

## Rules of thumb for indexer work

- **A push goes to one app.** A new push kind must be added to exactly one app's list in
  `push.rs`, or it reaches apps that can't show it. KaPost and KaPay have no notification
  service extension, so a push they can't read shows as a bare banner.
- **Don't remove a route because one app stopped calling it.** Check the proxy log by
  User-Agent (`docker logs kaspa-node-proxy`) first, and remember Android, Desktop and Kasia.
  Handshakes are the example: KaChat now starts chats with tagged `kchat:1:dm` messages, but
  Android, Desktop, older iPhone builds, KaPlay and Kasia still read `/handshakes/*` for
  requests from old clients and for restoring old chats. New handshakes still land on chain
  (6 in the day to 2026-11-18).
- **Handoffs name their app.** Each handoff in `docs/` says which app's session sent it
  ("From: the KaPost iOS session", and so on). Answer in the same doc, with a status line.
- **Android looks like one client in the logs.** If you need per-app Android numbers, ask the
  apps sessions to send a `User-Agent` naming the app.

## Older names you'll still see

- "KaChat Wallet" and `com.kachat.wallet` are KaPay. Older docs (`KACHAT_WALLET_PUSH.md`) use
  the old name.
- "KaChat Chess" and the `KaChatChess` User-Agent are KaPlay.
- `HANDOFF.md`'s repo table predates the split.
