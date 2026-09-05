# KaChat Indexer

> ## 🖥️ Want to run KaChat? → **[Kaspa Quick Start](https://github.com/KaspaSilver/Kaspa-Quick-Start)**
>
> **[Kaspa Quick Start](https://github.com/KaspaSilver/Kaspa-Quick-Start)** is a web control panel
> that brings up a Kaspa node and lets you switch on services like KaChat from a friendly GUI. It
> builds, wires up, configures, secures (HTTPS), and updates this indexer for you.
>
> **This repository is source only — the engine under the hood.** You don't run it directly; Kaspa
> Quick Start builds it from this repo's `main` branch and operates it. To stand up KaChat, install
> Kaspa Quick Start and enable the **KaChat Indexer** app.

---

**KaChat Indexer** is the backend for **KaChat**, a Kaspa-native social + chat network. It reads
KaChat / KaPosts protocol transactions from the chain and serves them over REST, chat, and push APIs.

## Running it

Everything is driven by **[Kaspa Quick Start](https://github.com/KaspaSilver/Kaspa-Quick-Start)**:

1. Install Kaspa Quick Start and open its control panel.
2. Let the node sync, then enable **KaChat Indexer**.
3. Configure it (network, FCM, translation languages, broadcast channels, …) and publish a domain —
   all from the panel. Updates are the panel's **Update** button, which rebuilds this repo's `main`.

There is no standalone / self-host path in this repo — Kaspa Quick Start owns the node, database,
reverse proxy, HTTPS, and configuration.

## What's inside

| Path | Role |
|---|---|
| `kachat-transaction-processor/` | Parses KaChat/KaPosts transactions (`kchat:1:` / legacy `k:1:` / `bcast:` broadcasts) into Postgres. |
| `kachat-webserver/` | KaPosts REST API + post translation (`/translate`, backed by LibreTranslate). |
| `kasia-indexer/` | Chat indexer (handshakes, contextual messages, groups, payments, self-stash) + chat & push API. |
| `kachat-admin/` | Admin **API** (`/api/*`) that the Kaspa Quick Start panel proxies to for stats, feature toggles, broadcast channels, translation status, and moderation. |
| `docker/kachat/` | The container build Quick Start uses — `Dockerfile.kachat-app`, `supervisord.conf`, and the per-service run scripts. |

> The old standalone admin *dashboard GUI* has been removed — indexer management now lives in the
> Kaspa Quick Start control panel, which talks to the admin API above.

## Configuration

All configuration is managed by the Kaspa Quick Start panel (it writes the container's environment
and rebuilds as needed). See the Quick Start docs.

## License

See [LICENSE](LICENSE).
