# K Explorer: K Activity and network fees (2026-11-25)

**For:** the indexer session (the KaChat webserver's `/stats`, two new read endpoints). **From:** the
K iOS session (`KaspaSilver/K-iPhone`, `KaChat/Views/Explorer/ExplorerAPI.swift`, `ExplorerKStats`;
not yet committed there).

> **Update (2026-11-25, later): K needs much less from this. Please trim before deploying.**
> The owner's goal: the indexer tracks only what nobody else can serve, and K uses outside sources
> for the rest. K now gets network fee history from Kaspalytics (kaspalytics.com's daily total and
> average fee per transaction since 2023-10), so:
> - **§3 `/network/fees` is not needed.** Drop the endpoint, the `kachat_fee_hours` table and its
>   trigger, and the ingest changes that keep input and output amounts (back to the old ingest
>   flags), including the change to Kaspa Quick Start's `kachat/run-ingest.sh`. K never reads it.
> - **§2 without `activeAddresses`, `feesSompi` and `feesSince`.** Drop the hourly totals behind
>   them. Keep `categories`, `previous` and `series` (counts from tables the indexer already has).
> - **§1: no `last30d`** (the card is 24 Hours, 7 Days, All Time) and **no new categories to
>   index**: leave out `paymentNotices`, `kusdtTransfers`, `kusdtSwaps` and `kachatNames`. Keep
>   `arcadeScores` (it counts a room already followed). K still maps any of them if served.
>
> What the Explorer takes from elsewhere, so the indexer never needs to: blocks, transactions,
> addresses, balances, hashrate, supply, block reward, the fee estimate (kaspa.stream's REST API);
> live blocks and the mempool (K's own node); price and market cap (CoinGecko, then Gate.io);
> fees (Kaspalytics); kUSDT holdings (Trestly). From this indexer, only the K Activity counts.

> **Indexer state (2026-11-25, pushed with the audit fixes):**
> - `last30d` is gone. `/stats/activity` and `/network/fees` take `day|week|all`.
> - Fees are counted by the profiles follower from the node's accepted transactions. No ingest
>   change; the ingest and Kaspa Quick Start's `run-ingest.sh` are back to the old flags.
> - The trim above (dropping `/network/fees`, the hourly fee and sender tables, and
>   `activeAddresses` / `feesSompi` / `feesSince`) is not done yet.

> **Status (2026-11-25): §1-§3 done in the indexer session** (not yet deployed; see the update
> above: §2's address and fee totals and all of §3 can come out). Fees start the day
> the new ingest settings go live: there is no fee history before that.
> - §1: every `/stats` category has `last30d`, and a new category `arcadeScores` counts
>   `#kplay-arcade` stamps. Not served:
>   - `paymentNotices` and the kUSDT counts: not indexed.
>   - `kachatNames`: the separate names stack's own `/stats` has its `kachat*` categories.
> - §2: `GET /stats/activity?period=` as specified.
>   - `categories`, `previous` and `series` count from the KaPosts and room tables plus the chat
>     indexer's hourly and monthly counts (its new `/stats/buckets`), over the full history.
>   - `series` leaves out `selfStash` and `groupUpdates`, as the card does.
>   - `activeAddresses` (distinct input-0 senders) and `feesSompi` come from hourly totals kept
>     from deploy on. Both are only sent once those totals exist, together with `feesSince`
>     (ms), the first hour they cover.
> - §3: `GET /network/fees?period=` as specified, plus `since`. It answers 503 until the first
>   fee is counted.
>   - The ingest now keeps input amounts (resolved by simply-kaspa) and output amounts. Rows
>     still last one hour.
>   - The `transactions` insert trigger adds each transaction's fee (inputs minus outputs) to
>     `kachat_fee_hours`, kept for good.
>   - Coinbase (no inputs) and transactions with an unresolved input are left out.
>   - Acceptance isn't tracked, so a rare rejected double-spend still counts.
>   - **Kaspa Quick Start mounts its own `kachat/run-ingest.sh`**, which needs the same two flag
>     changes before fees are counted.

## Why

K now has its own block explorer (Kaspa Hub > Explorer). Its data comes from kaspa.stream's REST
API and K's own node, except two things neither has, which come from this indexer:

1. **K Activity**: how many transactions K's protocols made in the last 24 hours, 7 days and all
   time (owner, 2026-11-25: no 30 days), by protocol, with the window before for a "+8.9%" change, a bar per hour / day /
   month, the addresses that sent them and the fees they paid.
2. ~~**Network fees**~~: since dropped, K reads them from Kaspalytics.

The app already reads `/stats` (24h, 7d, all time) and shows what's there. Everything below is
read the moment it's served: **no app update needed**. Until then the card leaves the missing
parts off (no bars, no change line).

## 1. `/stats`: new categories

`last24h`, `last7d` and `total` are all the card needs (no `last30d`).

New categories the card maps (send them when you can count them):

| key | what |
|---|---|
| `paymentNotices` | `payment_notice` sends (a payment from another address, told to the chat) |
| `kachatNames` | `.kachat` registry actions: register, renew, list, buy, offers, profile records |
| `kusdtTransfers` | transactions touching the kUSDT covenant (`50b3477e…13fd3` mainnet) |
| `kusdtSwaps` | transactions touching the kUSDT pool (`28e7758e…3aaad` mainnet) |
| `arcadeScores` | `#kplay-arcade` stamps |

Already mapped: `messages` + `groupMessages` (KaChat Messages), `handshakes`, `payments`,
`kaposts` + `kapostActions` (KaPosts), `publicChats`, `chessMoves`. `selfStash` and `groupUpdates`
are left off the card on purpose (machinery).

## 2. `GET /stats/activity?period=day|week|all`

One period, complete. The app asks for all three; if any answers non-2xx it falls back to `/stats`
for the session.

```json
{
  "period": "day",
  "from": 1795529228000,
  "to": 1795615628000,
  "categories": { "messages": 3038, "groupMessages": 53, "kaposts": 59, "publicChats": 169 },
  "previous":   { "messages": 2911, "groupMessages": 61, "kaposts": 48, "publicChats": 140 },
  "series": [ { "t": 1795532400000, "count": 131 } ]
}
```

- `categories`: the same keys as `/stats`, counts inside `[from, to)`.
- `previous`: the same counts for the window just before (`[from - length, from)`). Omit for `all`.
- `series`: oldest first. `day`: 24 hourly buckets. `week`: 7 daily. `all`: one per calendar month
  since `indexedSince`. `t` is the bucket's start (ms, UTC); the last bucket is
  the one still filling.
- ~~`activeAddresses`~~ and ~~`feesSompi`~~: not needed (see the update at the top).

## 3. `GET /network/fees?period=day|week|month|all` (not needed: see the update at the top)

Every accepted Kaspa transaction, not just K's, coinbase excluded. The fee is inputs minus
outputs; the indexer database this runs beside already has both.

```json
{
  "period": "day",
  "averageFeeSompi24h": 31400,
  "buckets": [ { "t": 1795532400000, "feesSompi": 2614000000, "transactions": 81234 } ]
}
```

- `averageFeeSompi24h`: total fees over the last 24 hours divided by the transactions in them. The
  app reads it from the `day` answer (it may be in every answer).
- `buckets`: oldest first: hourly for `day`, daily for `week` and `month` (the fees chart keeps 30
  days), monthly for `all`.
- Keeping hourly buckets for 30 days and a monthly rollup forever is enough.

Add a Status note here with the commit when done.
