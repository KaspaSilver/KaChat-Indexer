#!/bin/sh
# Names-only API (KACHAT_WEBSERVER_ONLY=names): /names/*, /market/*, /offers/*, /profiles/*,
# /identity/*, /health. Rate limit is per client IP per minute (IPv6 per /64); /identity/batch
# counts one per address. Forwarded headers are honoured only from a trusted proxy
# (WEBSERVER_TRUSTED_PROXIES, default loopback + private ranges): the LAST X-Forwarded-For hop
# (the one nginx appended) wins, X-Real-IP only when there is no X-Forwarded-For; anyone else
# is the TCP peer.
exec /app/kachat-webserver \
  --db-host "${DB_HOST:-localhost}" --db-port "${DB_PORT:-5432}" --db-name "${DB_NAME}" \
  --db-user "${DB_USER}" \
  --bind-address "0.0.0.0:${WEBSERVER_PORT:-3080}" \
  --worker-threads "${WEBSERVER_THREADS:-2}" --db-max-connections "${WEBSERVER_DB_CONNECTIONS:-8}" \
  --request-timeout 30 --rate-limit "${WEBSERVER_RATE_LIMIT:-6000}"
