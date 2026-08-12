# Self‑Hosting the KaChat Indexer

This guide walks you from a fresh machine to a running **KaChat Indexer** — the service that
indexes KaPosts (the on‑chain social feed), KaChat broadcasts, and encrypted chat, and serves
them to the KaChat app. It covers **Linux, Windows, and macOS**, plus optional **Portainer**
(a container dashboard) and **nginx‑proxy‑manager** (HTTPS + a public URL).

> **New to this?** You only need to follow the steps in order. Every command is copy‑paste‑able.

---

## What you're setting up

```
                    ┌──────────────────────────────────────────────┐
   Kaspa network ──▶│  Kaspa node (rusty-kaspa, --utxoindex, wRPC)  │
                    └───────────────┬──────────────────────────────┘
                                    │ ws://127.0.0.1:17110
                    ┌───────────────▼──────────────────────────────┐
                    │  KaChat Indexer  (docker compose)             │
                    │   • kachat-db     Postgres                    │
                    │   • kachat-app    ingest + processor +        │
                    │                   webserver + chat + admin    │
                    └───────┬───────────────────────┬──────────────┘
              :3080 API ────┘        :8600 chat/push │        :3081 admin (local)
                    │                                │
              ┌─────▼────────────────────────────────▼─────┐
              │  nginx-proxy-manager (HTTPS, your domain)   │  ← optional but recommended
              └─────────────────────────────────────────────┘
```

Two containers do the work: **`kachat-db`** (Postgres) and **`kachat-app`** (everything else, run
under supervisord). They read live data from a **Kaspa node** you point them at.

### Requirements
- A machine that's **on 24/7** if you want an always‑available indexer (a Linux server/VPS, a
  home server, or a spare desktop). ~4 GB RAM minimum, an SSD, and disk for the node + indexes.
- **A Kaspa node** the indexer can reach (see [Step 2](#step-2--a-kaspa-node)). This is the single
  biggest prerequisite — the indexer has nothing to index without it.
- Basic terminal familiarity.

> **Platform note:** the reference deployment uses Docker **host networking**, which is native on
> **Linux**. On **Windows/macOS** Docker runs inside a lightweight VM, so host networking behaves
> differently — a Linux host (or a Linux VPS) is the smoothest path and what production uses.
> Windows/macOS work for trying it out; see [Platform notes](#platform-notes).

---

## Step 1 — Install Docker

You need **Docker Engine + the Compose plugin**. Pick your OS.

### Linux (recommended)
The official convenience script installs Docker Engine + Compose:
```bash
curl -fsSL https://get.docker.com | sh
```
Then let your user run Docker without `sudo` (log out/in afterward):
```bash
sudo usermod -aG docker "$USER"
```
Verify:
```bash
docker --version && docker compose version
```
Docs: <https://docs.docker.com/engine/install/>

### Windows
1. Install **Docker Desktop for Windows**: <https://www.docker.com/products/docker-desktop/>
2. During setup, keep the **WSL 2** backend enabled (recommended).
3. Reboot, launch Docker Desktop, and wait for it to say "Engine running."
4. Verify in **PowerShell**:
   ```powershell
   docker --version; docker compose version
   ```

### macOS
1. Install **Docker Desktop for Mac** (choose Apple Silicon or Intel):
   <https://www.docker.com/products/docker-desktop/>
2. Launch Docker Desktop and wait for "Engine running."
3. Verify in **Terminal**:
   ```bash
   docker --version && docker compose version
   ```

---

## Step 2 — A Kaspa node

The indexer reads blocks and transactions from a **rusty‑kaspa** node over **BORSH wRPC**
(WebSocket). The node **must** be run with the transaction index enabled.

You have two options:

**A) Run your own node (recommended for a real indexer).** Follow the official rusty‑kaspa
instructions (<https://github.com/kaspanet/rusty-kaspa>) and start `kaspad` with:
```
--utxoindex           # required
--rpclisten-borsh=0.0.0.0:17110   # BORSH wRPC (WebSocket) the indexer connects to
```
Give it time to fully sync mainnet before expecting complete data.

**B) Point at an existing node.** If you already have a node (or a trusted one) exposing BORSH
wRPC, just set its address in the config in [Step 3](#step-3--get-the-code--configure).

By default the indexer expects the node on the **same machine** at `127.0.0.1:17110`.

---

## Step 3 — Get the code + configure

Clone the repository and open the deployment folder:
```bash
git clone https://github.com/KaspaSilver/kachat-indexer.git
cd kachat-indexer/docker/kachat
```

All settings live in **`docker/kachat/.env`**. Open it in an editor and set at minimum:

| Variable | What it is | Example |
|---|---|---|
| `NETWORK` | `mainnet` or `testnet-10` | `mainnet` |
| `KASPA_NODE_ADDRESS` | Your node's host | `127.0.0.1` |
| `KASPA_NODE_PORT` | Node BORSH wRPC port | `17110` |
| `DB_PASSWORD` | **Set a strong password** for Postgres | `change_me_to_something_long` |
| `INTERNAL_PUSH_SECRET` | Random string guarding internal push endpoints | run `openssl rand -hex 24` |
| `WEBSERVER_PORT` | Public API port | `3080` |
| `ADMIN_PORT` | Admin dashboard (kept local) | `3081` |
| `CHAT_API_PORT` | Chat + push API port | `8600` |
| `SKI_PORT` / `DB_PORT` | Internal ports (change only if they clash) | `8500` / `5442` |

> **Push notifications (APNs)** are optional. Leave the `APNS_*` values blank to run without push
> (device registration still works). To enable it later you supply your own Apple **Team ID**,
> **Key ID**, and `.p8` auth key — put the `.p8` under the data volume (`/var/kachat-chat-data/apns/`)
> and set `APNS_KEY_PATH` to it.

**Windows/macOS only:** in `compose.yaml`, host networking + `127.0.0.1` won't reach a node on your
host the same way — see [Platform notes](#platform-notes) before launching.

---

## Step 4 — Launch the indexer

From `kachat-indexer/docker/kachat`:
```bash
docker compose up -d --build
```
The first run **builds the indexer from source** (this repo), so it takes a while. Subsequent
starts are instant. Watch it come up:
```bash
docker compose logs -f kachat-app
```

Stop / start / update later:
```bash
docker compose down                 # stop
docker compose up -d                 # start
git pull && docker compose up -d --build   # update to the latest code
```

---

## Step 5 — Verify it's running

**Health check** (the public API):
```bash
curl http://localhost:3080/health
```
You should get an `ok`/healthy response once the node is connected and syncing.

**Admin dashboard** — a full ops UI (service health, KaPosts/broadcast/chat stats, moderation,
personal‑mode toggles). It's bound to **localhost only** for safety. On the server itself open
`http://localhost:3081`. From your laptop, use an SSH tunnel:
```bash
ssh -L 3081:localhost:3081 you@your-server
# then open http://localhost:3081 in your browser
```

**Point the KaChat app at your indexer** (optional): in the app's Settings → Connection, set the
KaPost/Broadcast Indexer URL to your server (e.g. `https://your-domain`) once you've done Step 7.

---

## Step 6 — Portainer (optional container dashboard)

Portainer gives you a web UI to see containers, logs, and stats — handy if you'd rather not use the
terminal. Run it once:

**Linux / Windows / macOS:**
```bash
docker volume create portainer_data
docker run -d -p 9443:9443 --name portainer --restart=always \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v portainer_data:/data \
  portainer/portainer-ce:latest
```
Then open **https://localhost:9443** (accept the self‑signed cert), create your admin user, and
choose the **local** Docker environment. You'll see `kachat-db` and `kachat-app` there.

> On Windows, the socket path is the same in Docker Desktop's Linux engine — the command above works
> as‑is in PowerShell.

---

## Step 7 — nginx‑proxy‑manager (HTTPS + a public URL)

To reach your indexer from the internet over **HTTPS** with a real domain, put
**nginx‑proxy‑manager (NPM)** in front of it. NPM handles Let's Encrypt certificates and routing
through a friendly web UI.

### 7a. Run NPM
```bash
mkdir -p ~/npm && cd ~/npm
cat > docker-compose.yml <<'YAML'
services:
  npm:
    image: jc21/nginx-proxy-manager:latest
    container_name: nginx-proxy-manager
    restart: unless-stopped
    ports:
      - "80:80"
      - "443:443"
      - "81:81"     # admin UI
    volumes:
      - ./data:/data
      - ./letsencrypt:/etc/letsencrypt
YAML
docker compose up -d
```
Open the admin UI at **http://your-server:81** (default login `admin@example.com` / `changeme` —
change it immediately).

### 7b. Point a domain at your server
Use any domain/subdomain you control (a free one from e.g. DuckDNS works). Create an **A record**
pointing it at your server's public IP, and forward ports **80** and **443** to the server if it's
behind a home router.

### 7c. Add the Proxy Host
In NPM → **Hosts → Proxy Hosts → Add Proxy Host**:
- **Domain Names:** `your-domain`
- **Scheme:** `http` · **Forward Hostname / IP:** your server's LAN/gateway IP · **Forward Port:** `3080`
- **SSL tab:** request a new Let's Encrypt certificate, enable **Force SSL** + **HTTP/2**.

That serves the KaPosts/broadcast content API. To also serve **chat + push** on the same domain,
add a **Custom Location** on that same proxy host:
- **location:** `/v1/push` · **Scheme:** `http` · **Forward Hostname/IP:** your server IP · **Port:** `8600`

> Leave the admin dashboard (`:3081`) **off** the reverse proxy — reach it only via SSH tunnel.
> The internal push endpoints (`/internal/push`) must **never** be exposed publicly.

---

## Ports reference

| Port | Service | Exposure |
|---|---|---|
| `3080` | KaPosts + broadcast API (public content) | Behind NPM / LAN |
| `8600` | Chat + push API (`/v1/push`, group/DM endpoints) | Behind NPM (`/v1/push`) |
| `3081` | **Admin dashboard** | **Localhost only** (SSH tunnel) |
| `8500` | simply‑kaspa‑indexer status | Internal |
| `5442` | Postgres | Internal |
| `9443` | Portainer (if installed) | Your choice |
| `81` / `80` / `443` | nginx‑proxy‑manager | As needed |

---

## Everyday operations

```bash
cd kachat-indexer/docker/kachat

docker compose ps                     # what's running
docker compose logs -f kachat-app     # follow logs
docker exec kachat-app supervisorctl status   # per-process status inside the app container
docker compose restart kachat-app     # restart just the app
docker compose down && docker compose up -d   # full restart
```

The admin dashboard (Step 5) shows per‑service health, ingestion freshness, and stats without the
terminal.

---

## Platform notes

- **Linux** — fully supported; `network_mode: host` and `127.0.0.1` for the node work directly.
  This is what production runs on.
- **Windows / macOS** — Docker Desktop runs containers in a VM, so:
  - **Host networking** doesn't publish container ports to your host the way it does on Linux. To
    experiment, you can change `network_mode: "host"` to explicit `ports:` mappings in
    `compose.yaml` (e.g. `- "3080:3080"`), and reach the node via `host.docker.internal` instead of
    `127.0.0.1` (set `KASPA_NODE_ADDRESS=host.docker.internal`).
  - For a **real, always‑on indexer**, a small Linux **VPS** is simpler and cheaper than keeping a
    desktop on 24/7.

---

## Troubleshooting

- **`/health` never turns healthy / no data** — the node isn't reachable or isn't synced. Confirm
  `kaspad` is running with `--utxoindex` and BORSH wRPC on the port in your `.env`, and that
  `KASPA_NODE_ADDRESS`/`KASPA_NODE_PORT` point at it.
- **"WebSocket is not connected" in logs** — the node restarted; the app auto‑retries. If it
  persists, restart the node then `docker compose restart kachat-app`.
- **Port already in use** — change the clashing `*_PORT` in `.env` and re‑run `docker compose up -d`.
- **Build is slow the first time** — expected; it compiles the Rust indexer from source. It's cached
  afterward.
- **Can't reach the admin dashboard** — it's localhost‑only by design; use the SSH tunnel in Step 5.

---

## What's under the hood

The KaChat Indexer is a fork of [thesheepcat/K‑indexer](https://github.com/thesheepcat/K-indexer)
extended with KaChat‑only exclusivity, broadcast + KaPosts indexing, encrypted chat/group indexing,
APNs push, and the admin dashboard. See [`README.md`](README.md) for the project overview and the
`docs`/`*.md` files for protocol and API details.
