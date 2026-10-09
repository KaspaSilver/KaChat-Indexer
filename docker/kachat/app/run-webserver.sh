#!/bin/sh
# Public REST API (KaPosts + broadcasts) — fronted by nginx-proxy-manager for TLS.
# Rate limit is per client IP per minute (IPv6 per /64). X-Real-IP / X-Forwarded-For are
# honoured only from a trusted proxy (WEBSERVER_TRUSTED_PROXIES, default loopback + private
# ranges), so behind nginx each user counts on their own and a direct caller cannot spoof one.
# The DB password comes from DB_PASSWORD in the environment, never the command line.
exec /app/kachat-webserver \
  --db-host localhost --db-port "${DB_PORT}" --db-name "${DB_NAME}" \
  --db-user "${DB_USER}" \
  --bind-address "0.0.0.0:${WEBSERVER_PORT}" \
  --worker-threads 6 --db-max-connections 18 --request-timeout 30 --rate-limit "${WEBSERVER_RATE_LIMIT:-6000}" \
  --libretranslate-url "${LIBRETRANSLATE_URL:-http://127.0.0.1:5000}"
