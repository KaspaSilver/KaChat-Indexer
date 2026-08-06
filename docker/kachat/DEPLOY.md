# Deploying the KaPosts indexer as the public default (`kaposts.duckdns.org`)

This turns the local stack into the public endpoint the KaChat app connects to, fronted by
the **nginx-proxy-manager** already running on this box, with TLS from Let's Encrypt.

Values below are specific to this machine (discovered at setup time):

| Thing | Value |
|---|---|
| Public hostname | `kaposts.duckdns.org` |
| Box LAN IP | `192.168.3.228` (behind NAT) |
| nginx-proxy-manager | container `nginx-proxy-manager`, admin UI on `:81`, network `npm_default` (172.19.0.0/16) |
| **NPM → webserver forward target** | **`172.19.0.1` port `3080`** (the npm_default gateway = the host, where the host-networked webserver listens) |
| Admin dashboard | `127.0.0.1:3081` (loopback only, never proxied) |

The box is **behind NAT**, so two things outside Docker must be true before TLS can issue:

1. **DuckDNS points at your public IP.** Set `kaposts.duckdns.org`'s IP in the DuckDNS
   dashboard, and keep it current with an updater (below).
2. **Router port-forwards 80 and 443 → 192.168.3.228.** Port 80 is required for the Let's
   Encrypt HTTP-01 challenge; 443 serves the app. (Do **not** forward 3080/3081/5442/8500.)

---

## 1. Bring the stack up

```bash
cd K-indexer/docker/KAPOSTS
# set a real DB_PASSWORD in .env first
docker compose up -d --build
```

Sanity check the webserver is answering on the host:

```bash
curl -s http://172.19.0.1:3080/health   # the exact address NPM will use
```

## 2. Keep DuckDNS updated (optional but recommended)

Run the standard updater alongside the stack (replace `TOKEN`):

```bash
docker run -d --name duckdns --restart unless-stopped \
  -e SUBDOMAINS=kaposts -e TOKEN=<your-duckdns-token> -e TZ=Etc/UTC \
  lscr.io/linuxserver/duckdns:latest
```

It pins `kaposts.duckdns.org` to whatever public IP the box currently has.

## 3. Create the proxy host in nginx-proxy-manager

Open the NPM admin UI (`http://<box>:81`) → **Hosts → Proxy Hosts → Add Proxy Host**:

- **Details tab**
  - Domain Names: `kaposts.duckdns.org`
  - Scheme: `http`
  - Forward Hostname / IP: `172.19.0.1`
  - Forward Port: `3080`
  - Block Common Exploits: on
  - Websockets Support: off (the API is plain REST)
- **SSL tab**
  - SSL Certificate: **Request a new SSL Certificate**
  - Force SSL: on, HTTP/2: on
  - Agree to Let's Encrypt ToS, enter your email, Save.

NPM will solve the HTTP-01 challenge over port 80 and issue the cert. Then:

```bash
curl -s https://kaposts.duckdns.org/health
curl -s "https://kaposts.duckdns.org/get-posts-watching?requesterPubkey=<PUBKEY>&limit=5" | jq .
```

## 4. Per-client rate limiting at the edge (important for a public default)

The webserver's `--rate-limit` keys on the TCP peer. Behind NPM that peer is always the
proxy, so it becomes a single **global** cap (set to 12000/min in `compose.yaml`). Real
**per-client** limiting must happen at nginx, which sees the true client IP. In the proxy
host → **Advanced** tab, add:

```nginx
# per-client-IP limit; tune rate/burst to taste
limit_req_zone $binary_remote_addr zone=kaposts:10m rate=100r/m;

location / {
    limit_req zone=kaposts burst=40 nodelay;
    proxy_pass http://172.19.0.1:3080;
    proxy_set_header Host              $host;
    proxy_set_header X-Real-IP         $remote_addr;
    proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
}
```

(If you'd rather the app itself enforce per-client limits, I can add `X-Forwarded-For`
support to the webserver's limiter — see "Follow-ups" below.)

## 5. Admin dashboard access (loopback only)

The admin service — including the unauthenticated **delete-by-pubkey** endpoint — is bound to
`127.0.0.1:3081` and is not proxied. Reach it through an SSH tunnel:

```bash
ssh -L 3081:localhost:3081 <user>@<box>
# then open http://localhost:3081 in your browser
```

## 6. Firewall (defense-in-depth)

Because the box is behind NAT and only 80/443 are forwarded, 3080/3081/5442/8500 are not
internet-reachable. If you also want them blocked on the LAN, and if `ufw` is in use — note
host-networked binds (this stack) are covered by `ufw` normally, unlike bridge port-maps:

```bash
sudo ufw allow 22,80,443/tcp
sudo ufw allow from 172.16.0.0/12 to any port 3080 proto tcp   # let Docker bridges reach the webserver
sudo ufw enable
```

---

## Broadcast indexer — same URL

The stack also serves KaChat **broadcast** history (`GET /get-broadcasts`) from the same
webserver, so no second domain/proxy is needed. In the app, set **Broadcast Indexer URL** to
the **same** value as the KaPost Indexer: `https://kaposts.duckdns.org`. Only `#kaspa` and
`#kachat-bugs` are indexed. (Verify: `curl "https://kaposts.duckdns.org/get-broadcasts?channel=kaspa&limit=5"`.)

## Running it as "everyone's default"

The indexer is now the public endpoint. The remaining change is app-side (you're doing this
yourself): set `AppSettings.defaultKaPostIndexerURL` in `KaChat/Models/Models.swift` to
`https://kaposts.duckdns.org` and ship. Users who never customized the setting migrate
automatically; anyone who set a custom URL keeps theirs.

## History model — fresh KaChat-only network

KaPosts starts fresh and has **no relation to the K social network**. Consequences:

- **No archival node / genesis backfill needed** (and none is wanted). The node here is
  pruned; that's fine — forward-only coverage is the intended design.
- Only KaChat-marked content is indexed, and votes are kept only when they target indexed
  KaChat content, so the database never accumulates K-network content or engagement.
- To begin a clean epoch (drop the few K-network rows caught during the first run), wipe the
  DB volume once when you deploy the vote-filter build:

  ```bash
  cd K-indexer/docker/KAPOSTS
  docker compose down
  sudo rm -rf /var/kaposts-db        # Postgres data dir (DB_NAME=kaposts-db)
  docker compose up -d --build
  ```

  Everything from that point on is permanent and KaChat-only; the node's pruning horizon
  never limits it going forward.

## Uptime notes (it's now everyone's default)

- The stack depends on the local mainnet node (`kaspad`, BORSH wRPC `:17110`). If the node
  goes down, ingestion stalls — the admin dashboard's health tile will show it (node lag).
- All services are `restart: unless-stopped`; they come back after a reboot once Docker and
  the node are up.
- Postgres data lives in `/var/kaposts-db/`. Back that path up if you care about not
  re-backfilling from genesis.

## Follow-ups I can do on request

- **True per-client rate limiting in the app** (honor `X-Forwarded-For` in the webserver's
  limiter) instead of / in addition to the nginx `limit_req` above.
- **Social push notifications** (mirror the chat `PushNotificationActor`).
- A **watchdog / alert** if node lag or ingestion stalls.
