#!/bin/sh
# kachat-app entrypoint (kachat-audits IDX-011). Runs as root once per container start, only to
# fix up what the unprivileged services need, then hands over to supervisord, which starts every
# program as the `kachat` user (supervisord.conf `user=kachat`). supervisord itself stays root:
# it owns the container's stdout, which a non-root process cannot reopen as /dev/stdout.
set -e

# /app/data (the chat indexer's fjall store + personal-mode allowlists): installs from before
# this change created the volume as root. Re-own it once; later starts find nothing to fix.
mkdir -p /app/data
if [ -n "$(find /app/data ! -user kachat -print -quit)" ]; then
  echo "[entrypoint] giving /app/data to kachat"
  chown -R kachat:kachat /app/data
fi

# Push credentials: Kaspa Quick Start mounts them read-only at /push, written 0600 by the host
# user, so kachat cannot read them in place. Copy them to a kachat-only directory and point
# every env var that names a /push/ path there. A changed key takes effect on the next restart.
if [ -d /push ]; then
  rm -rf /run/kachat-push
  mkdir -p /run/kachat-push
  cp -R /push/. /run/kachat-push/ 2>/dev/null || true
  chown -R kachat:kachat /run/kachat-push
  find /run/kachat-push -type d -exec chmod 500 {} +
  find /run/kachat-push -type f -exec chmod 400 {} +
  for kv in $(env | grep -E '^[A-Za-z_][A-Za-z0-9_]*=/push/'); do
    name="${kv%%=*}"
    value="${kv#*=}"
    export "${name}=/run/kachat-push/${value#/push/}"
  done
fi

exec supervisord -c /etc/supervisord.conf
