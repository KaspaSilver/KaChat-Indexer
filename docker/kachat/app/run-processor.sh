#!/bin/sh
# KaChat protocol processor (KaPosts + broadcasts) + heartbeat + retention pruner.
exec /app/kachat-transaction-processor \
  --upgrade-db --network "${NETWORK}" \
  --db-host localhost --db-port "${DB_PORT}" --db-name "${DB_NAME}" \
  --db-user "${DB_USER}" --db-password "${DB_PASSWORD}" \
  --db-max-connections 10 --workers 4 --channel transaction_channel \
  --retry-attempts 3 --retry-delay 1000 --broadcast-retention-days 30
