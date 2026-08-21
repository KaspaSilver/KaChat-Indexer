#!/usr/bin/env bash
# ============================================================================
# KaChat Indexer — one-command uninstaller
# ============================================================================
# Removes everything the one-click installer put on this machine:
#   • the KaChat stack containers (kaspad, kachat-db, kachat-app) and, if the
#     installer started them, nginx-proxy-manager + Portainer
#   • all named data volumes (Kaspa node data, Postgres, chat store, NPM, Portainer)
#   • the images that were downloaded/built for the stack
#   • the cloned repo working directory
#
# It does NOT uninstall Docker itself (that may be in use by other things). It asks
# for confirmation before deleting anything.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/KaspaSilver/kachat-indexer/main/uninstall.sh | bash
#   (add  KACHAT_YES=1  in front to skip the prompt)
# ============================================================================
set -euo pipefail

INSTALL_DIR="${KACHAT_HOME:-$HOME/kachat/kachat-indexer}"
COMPOSE_DIR="$INSTALL_DIR/docker/kachat/selfhost"

c_blue='\033[1;34m'; c_grn='\033[1;32m'; c_yel='\033[1;33m'; c_red='\033[1;31m'; c_rst='\033[0m'
say()  { printf "${c_blue}==>${c_rst} %s\n" "$*"; }
ok()   { printf "${c_grn}  ✓${c_rst} %s\n" "$*"; }
warn() { printf "${c_yel}  !${c_rst} %s\n" "$*"; }

SUDO=""
if [ "$(id -u)" -ne 0 ]; then command -v sudo >/dev/null 2>&1 && SUDO="sudo"; fi
DK="docker"; docker info >/dev/null 2>&1 || { [ -n "$SUDO" ] && $SUDO docker info >/dev/null 2>&1 && DK="$SUDO docker"; }
dk() { $DK "$@"; }

# Images the stack pulls or builds (built app image is named "kachat-kachat-app").
IMAGES=(
  "kachat-kachat-app"
  "supertypo/rusty-kaspad:latest"
  "postgres:17-alpine"
  "jc21/nginx-proxy-manager:latest"
  "portainer/portainer-ce:latest"
)

printf "${c_red}This will PERMANENTLY DELETE the KaChat Indexer stack, its data volumes\n(node data, database, chat store), its images, and $INSTALL_DIR.${c_rst}\n\n"

if [ "${KACHAT_YES:-0}" != "1" ]; then
  ans=""
  if [ -r /dev/tty ]; then printf "Type 'yes' to proceed: "; read -r ans </dev/tty
  else warn "No terminal for a prompt. Re-run with:  KACHAT_YES=1 curl -fsSL …/uninstall.sh | bash"; exit 1; fi
  [ "$ans" = "yes" ] || { echo "Aborted — nothing was removed."; exit 0; }
fi

# 1. Compose down (with volumes) — covers containers + named volumes + network.
if [ -f "$COMPOSE_DIR/compose.yaml" ]; then
  say "Stopping and removing the stack + its volumes…"
  ( cd "$COMPOSE_DIR" && dk compose --profile proxy --profile monitor down --volumes --remove-orphans ) || warn "compose down reported an issue — continuing."
  ok "Stack stopped and volumes removed"
else
  warn "No compose file at $COMPOSE_DIR — removing any leftover containers by name."
  for c in kachat-app kachat-db kaspad nginx-proxy-manager portainer; do dk rm -f "$c" >/dev/null 2>&1 || true; done
fi

# 2. Remove the images that were downloaded/built.
say "Removing downloaded/built images…"
for img in "${IMAGES[@]}"; do dk rmi "$img" >/dev/null 2>&1 && ok "removed image $img" || true; done

# 3. Remove the cloned working directory.
if [ -d "$INSTALL_DIR" ]; then
  say "Removing $INSTALL_DIR …"
  rm -rf "$INSTALL_DIR"
  # Clean up the parent ~/kachat dir too if it's now empty.
  rmdir "$(dirname "$INSTALL_DIR")" 2>/dev/null || true
  ok "Working directory removed"
fi

printf "\n${c_grn}KaChat Indexer removed.${c_rst} Docker itself was left installed.\n"
