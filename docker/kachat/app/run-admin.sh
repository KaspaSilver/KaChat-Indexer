#!/bin/sh
# Admin dashboard (KaChat Indexer GUI) — bound to loopback, reached via SSH tunnel.
# DB password comes from env DB_PASSWORD (kept off the command line, kachat-audits IDX-011).
exec /app/kachat-admin \
  --db-host localhost --db-port "${DB_PORT}" --db-name "${DB_NAME}" \
  --db-user "${DB_USER}" \
  --db-max-connections 4 --bind-address "127.0.0.1:${ADMIN_PORT}"
