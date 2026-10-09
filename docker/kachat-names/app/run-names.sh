#!/bin/sh
# .kachat names follower. Idles unless KACHAT_NAMES_MANIFEST names a manifest file
# (.kachat Domains publishes it). PUSH_INTERNAL_URL empty = no pushes.
if [ -z "${KACHAT_NAMES_MANIFEST}" ] || [ ! -f "${KACHAT_NAMES_MANIFEST}" ]; then
  echo "[names] no manifest configured; follower idle"
  exec sleep 2147483647
fi
export KACHAT_NAMES_NODE_URL="${KACHAT_NAMES_NODE_URL:-${KASPA_NODE_WBORSH_URL:-ws://kaspad:17110}}"
if [ -z "${PUSH_INTERNAL_URL}" ]; then
  # An empty env value is not a reliable "off" for clap; say it on the command line.
  unset PUSH_INTERNAL_URL
  exec /app/kachat-names-follower --push-url=
fi
exec /app/kachat-names-follower
