#!/usr/bin/env bash
# ============================================================================
# KaChat Indexer — one-command self-host installer
# ============================================================================
# Run your own KaChat Indexer with a single command. This script:
#   1. Makes sure git + Docker (with the compose plugin) are installed and running,
#      installing Docker automatically on Linux if it is missing.
#   2. Clones (or updates) the KaChat Indexer repo.
#   3. Generates a .env with fresh random secrets on first run.
#   4. Starts nginx-proxy-manager and Portainer only if you are not already running them.
#   5. Builds and launches the whole stack in Docker: a bundled Kaspa node
#      (kaspad, RPC on 16110 + 17110), Postgres, and the KaChat app.
#
# Usage (Linux, macOS, or Windows via WSL2 / Git Bash):
#   curl -fsSL https://raw.githubusercontent.com/KaspaSilver/kachat-indexer/main/install.sh | bash
#
# Remove everything again with uninstall.sh (see the README).
# ============================================================================
set -euo pipefail

REPO_URL="${KACHAT_REPO_URL:-https://github.com/KaspaSilver/kachat-indexer.git}"
BRANCH="${KACHAT_BRANCH:-main}"
INSTALL_DIR="${KACHAT_HOME:-$HOME/kachat/kachat-indexer}"
COMPOSE_DIR_REL="docker/kachat/selfhost"

c_blue='\033[1;34m'; c_grn='\033[1;32m'; c_yel='\033[1;33m'; c_red='\033[1;31m'; c_rst='\033[0m'
say()  { printf "${c_blue}==>${c_rst} %s\n" "$*"; }
ok()   { printf "${c_grn}  ✓${c_rst} %s\n" "$*"; }
warn() { printf "${c_yel}  !${c_rst} %s\n" "$*"; }
die()  { printf "${c_red}  ✗ %s${c_rst}\n" "$*" >&2; exit 1; }

# --- privilege + docker wrappers ------------------------------------------------------
SUDO=""
if [ "$(id -u)" -ne 0 ]; then command -v sudo >/dev/null 2>&1 && SUDO="sudo"; fi
DK="docker"                         # may become "sudo docker" below
dk() { $DK "$@"; }

OS="$(uname -s)"

# --- 1. prerequisites -----------------------------------------------------------------
ensure_git() {
  command -v git >/dev/null 2>&1 && { ok "git present"; return; }
  say "Installing git…"
  case "$OS" in
    Linux)
      if   command -v apt-get >/dev/null 2>&1; then $SUDO apt-get update -qq && $SUDO apt-get install -y -qq git
      elif command -v dnf     >/dev/null 2>&1; then $SUDO dnf install -y -q git
      elif command -v yum     >/dev/null 2>&1; then $SUDO yum install -y -q git
      elif command -v pacman  >/dev/null 2>&1; then $SUDO pacman -Sy --noconfirm git
      elif command -v apk     >/dev/null 2>&1; then $SUDO apk add --no-cache git
      else die "Could not find a package manager to install git. Install git and re-run."; fi ;;
    *) die "git is required. Install it (e.g. 'brew install git' on macOS) and re-run." ;;
  esac
  ok "git installed"
}

ensure_docker() {
  if command -v docker >/dev/null 2>&1; then
    if docker info >/dev/null 2>&1; then DK="docker"
    elif [ -n "$SUDO" ] && $SUDO docker info >/dev/null 2>&1; then DK="$SUDO docker"; warn "Using sudo for docker (your user is not in the 'docker' group)."
    else die "Docker is installed but the daemon isn't reachable. Start Docker (or Docker Desktop) and re-run."; fi
  else
    case "$OS" in
      Linux)
        say "Docker not found — installing via get.docker.com …"
        curl -fsSL https://get.docker.com | $SUDO sh || die "Docker install failed. See https://docs.docker.com/engine/install/"
        $SUDO systemctl enable --now docker 2>/dev/null || true
        if docker info >/dev/null 2>&1; then DK="docker"; else DK="$SUDO docker"; fi ;;
      Darwin) die "Install Docker Desktop for Mac (https://www.docker.com/products/docker-desktop/), start it, then re-run." ;;
      *)      die "Install Docker Desktop and run this inside WSL2 or Git Bash (https://www.docker.com/products/docker-desktop/), then re-run." ;;
    esac
  fi
  dk compose version >/dev/null 2>&1 || die "The 'docker compose' plugin is missing. Update Docker to a recent version and re-run."
  ok "Docker ready ($(dk --version | cut -d, -f1))"
}

# --- 2. clone / update ----------------------------------------------------------------
fetch_repo() {
  if [ -d "$INSTALL_DIR/.git" ]; then
    say "Updating existing checkout at $INSTALL_DIR …"
    git -C "$INSTALL_DIR" fetch --quiet origin "$BRANCH"
    git -C "$INSTALL_DIR" checkout --quiet "$BRANCH"
    git -C "$INSTALL_DIR" pull --ff-only --quiet origin "$BRANCH" || warn "Could not fast-forward (local changes?) — using what's on disk."
  else
    say "Cloning KaChat Indexer into $INSTALL_DIR …"
    mkdir -p "$(dirname "$INSTALL_DIR")"
    git clone --quiet --branch "$BRANCH" "$REPO_URL" "$INSTALL_DIR"
  fi
  ok "Repo ready"
}

# --- 3. .env with random secrets on first run -----------------------------------------
rand_hex() {
  if command -v openssl >/dev/null 2>&1; then openssl rand -hex 24
  else head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n'; fi
}
prepare_env() {
  local dir="$INSTALL_DIR/$COMPOSE_DIR_REL" env="$INSTALL_DIR/$COMPOSE_DIR_REL/.env"
  if [ -f "$env" ]; then ok ".env already present — leaving your settings untouched"; return; fi
  cp "$dir/.env.example" "$env"
  # sed portability: use a temp file rather than -i (differs between GNU/BSD sed).
  local dbpass; dbpass="$(rand_hex)"
  local secret; secret="$(rand_hex)"
  sed -e "s|^DB_PASSWORD=.*|DB_PASSWORD=${dbpass}|" \
      -e "s|^INTERNAL_PUSH_SECRET=.*|INTERNAL_PUSH_SECRET=${secret}|" \
      "$env" > "$env.tmp" && mv "$env.tmp" "$env"
  chmod 600 "$env"
  ok "Generated .env with fresh random DB password + push secret"
}

# --- 4. decide whether to also start NPM / Portainer ----------------------------------
running_image() { dk ps --format '{{.Image}}' 2>/dev/null | grep -qi "$1"; }
PROFILES=()
decide_profiles() {
  if running_image "nginx-proxy-manager"; then warn "nginx-proxy-manager already running — not starting another."
  else PROFILES+=(--profile proxy); ok "Will start nginx-proxy-manager (HTTPS front door)"; fi
  if running_image "portainer";          then warn "Portainer already running — not starting another."
  else PROFILES+=(--profile monitor); ok "Will start Portainer (monitoring UI)"; fi
}

# --- 5. build + launch ----------------------------------------------------------------
launch() {
  say "Building and starting the stack (first build compiles from source — this can take a while)…"
  ( cd "$INSTALL_DIR/$COMPOSE_DIR_REL" && dk compose ${PROFILES[@]+"${PROFILES[@]}"} up -d --build )
  ok "Stack is up"
}

summary() {
  local env="$INSTALL_DIR/$COMPOSE_DIR_REL/.env"
  local wp cp; wp="$(grep '^WEBSERVER_PORT=' "$env" | cut -d= -f2)"; cp="$(grep '^CHAT_API_PORT=' "$env" | cut -d= -f2)"
  printf "\n${c_grn}KaChat Indexer is running.${c_rst}\n\n"
  echo   "  Kaspa node RPC ...... 127.0.0.1:16110 (gRPC)  •  127.0.0.1:17110 (BORSH wRPC)"
  echo   "  KaPosts REST API .... http://localhost:${wp:-3080}   (try /health)"
  echo   "  Chat indexer API .... http://localhost:${cp:-8600}"
  running_image "portainer"           && echo "  Monitoring (Portainer) http://localhost:9000   (set an admin password on first visit)"
  running_image "nginx-proxy-manager" && echo "  HTTPS proxy (NPM) ... http://localhost:81      (login admin@example.com / changeme — change it)"
  cat <<EOF

  First run: the bundled Kaspa node has to sync before chat/KaPosts data appears.
  Watch progress:   docker logs -f kaspad
  All services:     open Portainer, or run  docker compose -f "$INSTALL_DIR/$COMPOSE_DIR_REL/compose.yaml" ps

  Remove everything this installed:
    curl -fsSL https://raw.githubusercontent.com/KaspaSilver/kachat-indexer/main/uninstall.sh | bash
EOF
}

main() {
  printf "${c_blue}KaChat Indexer — self-host installer${c_rst}\n\n"
  ensure_git
  ensure_docker
  fetch_repo
  prepare_env
  decide_profiles
  launch
  summary
}
main "$@"
