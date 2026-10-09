#!/bin/sh
# Address profiles follower (docs/KACHAT_PROFILES.md): feeds /profiles/* and the profile half
# of /identity/*. Needs no manifest.
export KACHAT_NAMES_NODE_URL="${KACHAT_NAMES_NODE_URL:-${KASPA_NODE_WBORSH_URL:-ws://kaspad:17110}}"
exec /app/kachat-names-follower --profiles
