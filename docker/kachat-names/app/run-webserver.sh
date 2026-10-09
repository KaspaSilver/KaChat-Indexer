#!/bin/sh
# Names-only API (KACHAT_WEBSERVER_ONLY=names): /names/*, /market/*, /offers/*, /profiles/*,
# /identity/*, /health. Rate limit is per source IP per minute; behind a proxy it honours
# X-Forwarded-For.
exec /app/kachat-webserver \
  --db-host "${DB_HOST:-localhost}" --db-port "${DB_PORT:-5432}" --db-name "${DB_NAME}" \
  --db-user "${DB_USER}" --db-password "${DB_PASSWORD}" \
  --bind-address "0.0.0.0:${WEBSERVER_PORT:-3080}" \
  --worker-threads "${WEBSERVER_THREADS:-2}" --db-max-connections "${WEBSERVER_DB_CONNECTIONS:-8}" \
  --request-timeout 30 --rate-limit "${WEBSERVER_RATE_LIMIT:-6000}"
