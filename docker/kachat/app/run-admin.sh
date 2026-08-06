#!/bin/sh
# Admin dashboard (KaChat Indexer GUI) — bound to loopback, reached via SSH tunnel.
exec /app/K-admin \
  --db-host localhost --db-port "${DB_PORT}" --db-name "${DB_NAME}" \
  --db-user "${DB_USER}" --db-password "${DB_PASSWORD}" \
  --db-max-connections 4 --bind-address "127.0.0.1:${ADMIN_PORT}"
