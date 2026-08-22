#!/bin/sh
# Public REST API (KaPosts + broadcasts) — fronted by nginx-proxy-manager for TLS.
# NOTE: --rate-limit is per source IP per minute. Behind nginx the webserver sees ONE source
# (the proxy) for every user, so this is effectively a global cap. Kept high so mass browser
# testing (many users sharing the proxy IP) doesn't hit 429s; real backpressure is the worker/
# DB pool. Proper per-user limiting would require honoring X-Forwarded-For in the app.
exec /app/kachat-webserver \
  --db-host localhost --db-port "${DB_PORT}" --db-name "${DB_NAME}" \
  --db-user "${DB_USER}" --db-password "${DB_PASSWORD}" \
  --bind-address "0.0.0.0:${WEBSERVER_PORT}" \
  --worker-threads 6 --db-max-connections 18 --request-timeout 30 --rate-limit 600000
