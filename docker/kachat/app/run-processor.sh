#!/bin/sh
# KaChat protocol processor (KaPosts + broadcasts) + heartbeat + retention pruner.
# DB password comes from env DB_PASSWORD (kept off the command line, kachat-audits IDX-011).
# Public-chat sender checks ask the node over wRPC: KASPA_NODE_WBORSH_URL (XP-012).
export KASPA_NODE_WBORSH_URL="${KASPA_NODE_WBORSH_URL:-ws://${KASPA_NODE_ADDRESS}:${KASPA_NODE_PORT}}"
exec /app/kachat-transaction-processor \
  --upgrade-db --network "${NETWORK}" \
  --db-host localhost --db-port "${DB_PORT}" --db-name "${DB_NAME}" \
  --db-user "${DB_USER}" \
  --db-max-connections 10 --workers 4 --channel transaction_channel \
  --retry-attempts 3 --retry-delay 1000 --broadcast-retention-days 30
