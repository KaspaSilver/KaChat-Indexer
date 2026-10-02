#!/bin/sh
# .kachat names registry follower (KACHAT_NAMES_INDEXER.md §B). Reads the node's virtual chain
# and keeps the registry in this stack's Postgres for the webserver's /names/* API.
# Off (idles) unless KACHAT_NAMES_MANIFEST points at a genesis manifest file.
if [ -z "${KACHAT_NAMES_MANIFEST}" ] || [ ! -f "${KACHAT_NAMES_MANIFEST}" ]; then
  echo "[names] no manifest configured; follower idle"
  exec sleep 2147483647
fi
export KACHAT_NAMES_NODE_URL="${KACHAT_NAMES_NODE_URL:-${KASPA_NODE_WBORSH_URL:-ws://127.0.0.1:17210}}"
export DB_HOST="${DB_HOST:-localhost}"
exec /app/kachat-names-follower
