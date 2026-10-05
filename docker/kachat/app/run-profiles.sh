#!/bin/sh
# Address profiles follower (docs/KACHAT_PROFILES.md). Always on, on every network: profiles
# are stamped to an address by a kchat:1:profile: self-send and need no names manifest.
# The only writer of names_profiles; feeds /profiles/*, /identity/* and /profiles/stats.
export KACHAT_NAMES_NODE_URL="${KACHAT_NAMES_NODE_URL:-${KASPA_NODE_WBORSH_URL:-ws://127.0.0.1:17210}}"
export DB_HOST="${DB_HOST:-localhost}"
exec /app/kachat-names-follower --profiles
