# Android push (FCM) — server & deployment setup

This wires **native Android notifications** into the KaChat indexer via **Firebase Cloud
Messaging (FCM)**, alongside the existing Apple/APNs path. The Android app (KaChatForAndroid,
`KaChat4.0A` branch) registers its FCM token with `https://kachat.duckdns.org/v1/push/register`,
signed with the user's Kaspa wallet key — the same challenge/Schnorr scheme the iOS app uses.

Server changes are already in the code (`kasia-indexer`): `android` is an accepted platform, FCM
tokens are accepted (they aren't hex like APNs tokens), and the push dispatcher routes each device
to APNs **or** FCM by its registered platform. FCM stays **disabled** until you provide the two
credentials below — registration keeps working regardless.

---

## 1. Create the Firebase project (one time)

You must do this in the Firebase console (it needs a Google login — it can't be scripted here).

1. Go to <https://console.firebase.google.com> → **Add project** (or reuse an existing one).
   Note the **Project ID** (e.g. `kachat-12ab3`) — Settings → *General*.
2. **Add an Android app** to the project:
   - Package name: **`com.kachat.app`** (must match exactly; the debug build uses
     `com.kachat.app.debug`, so add that as a second Android app if you want push in debug builds).
   - Download the generated **`google-services.json`** — this goes in the **Android app** repo at
     `app/google-services.json` (see that repo's `PUSH_ANDROID_SETUP.md`).
3. Generate the **service-account key** for the server:
   - Settings → **Service accounts** → **Generate new private key** → download the JSON.
   - This is a **secret** (it holds a private key). Treat it like the release keystore.

> Cloud Messaging must be enabled (it is by default with the above). The legacy server key is not
> used — this integration uses the modern **HTTP v1** API with the service account.

---

## 2. Install the credential on the box

Put the service-account JSON on the app container's data volume, which mounts at `/app/data`:

```bash
sudo mkdir -p /var/kachat-chat-data/fcm
sudo cp ~/Downloads/<your-project>-firebase-adminsdk-*.json /var/kachat-chat-data/fcm/service-account.json
sudo chmod 600 /var/kachat-chat-data/fcm/service-account.json
```

The container reads it at `/app/data/fcm/service-account.json` (already wired in `compose.yaml`).

---

## 3. Set the project id and restart

In `docker/kachat/.env`:

```dotenv
FCM_PROJECT_ID=kachat-12ab3
```

Then rebuild/restart the app container:

```bash
cd docker/kachat && docker compose up -d --build
```

On startup you should see in `docker logs kachat-app`:

```
[Push] FCM enabled (project kachat-12ab3)
```

If instead you see `[Push] FCM disabled: ...`, the JSON path or project id is wrong — the log line
says which. APNs/iOS is unaffected either way.

Config reference (all read from env by the chat indexer):

| Var | Meaning |
|-----|---------|
| `FCM_PROJECT_ID` | Firebase project id. Enables FCM when set (with the JSON present). |
| `FCM_SERVICE_ACCOUNT_PATH` | Path to the service-account JSON. Default `/app/data/fcm/service-account.json`. |
| `FCM_SERVICE_ACCOUNT_JSON` | Inline alternative to the path (the whole JSON as a string). |

---

## 4. Expose the push API at `kachat.duckdns.org`

The Android app posts registrations to **`https://kachat.duckdns.org`**, which must reach the chat
indexer's push API on **`CHAT_API_PORT` (8600)** — *not* the KaPosts webserver on 3080. Today only
`kaposts.duckdns.org → 3080` is proxied (see `DEPLOY.md`); add a second host:

1. **DuckDNS**: add the `kachat` subdomain (same token as `kaposts`), pointing at your box.
   If you use the `linuxserver/duckdns` updater container, add `kachat` to `SUBDOMAINS`.
2. **nginx-proxy-manager**: add a **Proxy Host**:
   - Domain: `kachat.duckdns.org`
   - Forward to: `172.19.0.1` port **`8600`** (the host-networked chat indexer)
   - Request a Let's Encrypt cert, force SSL.
3. **Harden the push routes.** In the proxy host's **Advanced** tab, adapt
   [`kasia-indexer/nginx/push-security.conf`](../../kasia-indexer/nginx/push-security.conf): it
   rate-limits `/v1/push/(register|update|unregister|challenge)` (keyed on the client's
   `X-Push-Token-Hash`) and 404s deprecated `/push/` routes.

> **Do not** expose `/internal/push/*` publicly — those endpoints (broadcast/KaPosts injection)
> are guarded only by `INTERNAL_PUSH_SECRET` and are meant to be reached in-box only. If you proxy
> the whole host, add a `location ^~ /internal/ { return 404; }` block in the Advanced tab.

Quick check once live:

```bash
curl -s -X POST https://kachat.duckdns.org/v1/push/challenge
# → {"nonce":"...","issued_at_ms":...,"expires_at_ms":...}
```

---

## 5. End-to-end test

1. Build & install the Android app with `google-services.json` in place (see the app repo guide),
   sign in / unlock a wallet, and accept the notification permission prompt.
2. In `docker logs kachat-app` you should see a `[Push] register ... auth=true` line when the app
   registers, and the device is stored with `platform=android`.
3. Trigger a push:
   - **KaPosts**: have someone reply/vote/quote a post by your registered `kaposts_pubkey`.
   - **Broadcast**: post to a channel you have the bell enabled on.
   - **DM**: send the wallet a message from another device.
4. Background or kill the app — the notification should still arrive (data-only FCM at high
   priority wakes `KaChatFirebaseMessagingService`).

Metrics (`/metrics` on the chat indexer) count sends/failures across both transports; a bad FCM
token is pruned on `UNREGISTERED`, exactly like an APNs `Unregistered` token.

---

## What's covered vs. not (this version)

- **Covered:** KaPosts pings, broadcast-channel notifications, and 1:1 DM/payment/handshake
  notifications (the DM body is the server's generic fallback text — the encrypted content is not
  yet decrypted on the Android side; iOS does this in its Notification Service Extension).
- **Not yet:** group-message push on Android (would need the TransitionalGroups auth shape on both
  sides), and inline decryption of DM bodies in the FCM handler.
