#!/bin/sh
# Public REST API (KaPosts + broadcasts) — fronted by nginx-proxy-manager for TLS.
exec /app/kachat-webserver \
  --db-host localhost --db-port "${DB_PORT}" --db-name "${DB_NAME}" \
  --db-user "${DB_USER}" --db-password "${DB_PASSWORD}" \
  --bind-address "0.0.0.0:${WEBSERVER_PORT}" \
  --worker-threads 6 --db-max-connections 18 --request-timeout 30 --rate-limit 12000
