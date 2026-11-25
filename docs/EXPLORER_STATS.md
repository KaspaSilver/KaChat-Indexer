# K Explorer: K Activity and network fees (2026-11-25)

**For:** the indexer session (the KaChat webserver's `/stats`, two new read endpoints). **From:** the
K iOS session (`KaspaSilver/K-iPhone`, `KaChat/Views/Explorer/ExplorerAPI.swift`, `ExplorerKStats`;
not yet committed there).

> **Status (2026-11-25): §1 partly done in the indexer session** (not yet deployed).
> - Every `/stats` category now has `last30d`: the Postgres ones, and the chat indexer's (its
>   window scan counts 30 days too). New category: `arcadeScores` (`#kplay-arcade` stamps).
> - Not done in §1: `paymentNotices`, `kachatNames` (the names live in the separate names stack;
>   its own `/stats` already has the `kachat*` categories) and the two kUSDT counts (no covenant
>   transactions are indexed).
> - §2 and §3 are not started.
>   - **§3 (network fees) can't be served from this indexer as it stands.** Its transaction feed
>     keeps one hour, without input amounts or output values, so no fee can be computed. It would
>     need a UTXO-resolving ingest or another source (kaspa.stream).
>   - `feesSompi` in §2 has the same problem.
>   - §2's per-protocol series and windows are doable, from the tables and the chat indexer's
>     window scan; they wait on the owner's call.

## Why

K now has its own block explorer (Kaspa Hub > Explorer). Its data comes from kaspa.stream's REST
API and K's own node, except two things neither has, which come from this indexer:

1. **K Activity**: how many transactions K's protocols made in the last 24 hours, 7 days, 30 days
   and all time, by protocol, with the window before for a "+8.9%" change, a bar per hour / day /
   month, the addresses that sent them and the fees they paid.
2. **Network fees**: the average fee one Kaspa transaction paid over the last 24 hours, and total
   fees per hour, day or month.

The app already reads `/stats` (24h, 7d, all time) and shows what's there. Everything below is
read the moment it's served: **no app update needed**. Until then the card leaves the missing
parts off (no 30 Days, no bars, no change line, no footer) and the fee tile says "demo".

## 1. `/stats`: add `last30d`

Each category gains `last30d`, like `last24h` and `last7d`. With it the card offers 30 Days even
before section 2 exists.

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

## 2. `GET /stats/activity?period=day|week|month|all`

One period, complete. The app asks for all four; if any answers non-2xx it falls back to `/stats`
for the session.

```json
{
  "period": "day",
  "from": 1795529228000,
  "to": 1795615628000,
  "categories": { "messages": 3038, "groupMessages": 53, "kaposts": 59, "publicChats": 169 },
  "previous":   { "messages": 2911, "groupMessages": 61, "kaposts": 48, "publicChats": 140 },
  "series": [ { "t": 1795532400000, "count": 131 } ],
  "activeAddresses": 812,
  "feesSompi": 2418000000
}
```

- `categories`: the same keys as `/stats`, counts inside `[from, to)`.
- `previous`: the same counts for the window just before (`[from - length, from)`). Omit for `all`.
- `series`: oldest first. `day`: 24 hourly buckets. `week`: 7 daily. `month`: 30 daily. `all`: one
  per calendar month since `indexedSince`. `t` is the bucket's start (ms, UTC); the last bucket is
  the one still filling.
- `activeAddresses`: distinct sender addresses of the counted transactions in the window.
- `feesSompi`: total network fees those transactions paid in the window.

## 3. `GET /network/fees?period=day|week|month|all`

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
- `buckets`: oldest first, the same layout as `series` above (hourly for `day`, daily for `week` and
  `month`, monthly for `all`).
- Keeping hourly buckets for 30 days and a monthly rollup forever is enough.

Add a Status note here with the commit when done.
