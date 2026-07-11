#!/usr/bin/env bash
# scripts/dev-up.sh — stand up a local dev fleet: one reeve-server +
# N virtual devices, admin created, remote terminal enabled fleet-wide,
# devices enrolled. Then open http://localhost:8420.
#
#   ./scripts/dev-up.sh [N]      # N devices, default 3
#
# Re-runnable: safe to run again (setup is skipped once the admin
# exists). `./scripts/dev-down.sh` tears everything down.
set -euo pipefail

N="${1:-3}"
BASE="http://localhost:8420"
# One source of truth: export these so compose.dev.yml interpolates the
# SAME values the server seeds and we log in with. Override by setting
# REEVE_ADMIN_USER / REEVE_ADMIN_PASSWORD before running.
export REEVE_ADMIN_USER="${REEVE_ADMIN_USER:-admin}"
export REEVE_ADMIN_PASSWORD="${REEVE_ADMIN_PASSWORD:-password}"
ADMIN_USER="$REEVE_ADMIN_USER"
ADMIN_PASS="$REEVE_ADMIN_PASSWORD"
COMPOSE=(docker compose -f compose.dev.yml)
COOKIES="$(mktemp)"
trap 'rm -f "$COOKIES"' EXIT

cd "$(dirname "$0")/.."

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }

say "building + starting reeve-server"
"${COMPOSE[@]}" up -d --build reeve-server

say "waiting for the server to be healthy"
for _ in $(seq 1 60); do
  if curl -fsS "$BASE/healthz" >/dev/null 2>&1; then break; fi
  sleep 1
done
curl -fsS "$BASE/healthz" >/dev/null

# --- admin (password mode first-boot: setup token is logged once) ----
login() {
  curl -fsS -X POST "$BASE/api/auth/login" \
    -H 'content-type: application/json' \
    -d "{\"username\":\"$ADMIN_USER\",\"password\":\"$ADMIN_PASS\"}" \
    -c "$COOKIES" >/dev/null 2>&1
}
if login; then
  say "admin already exists — logged in"
else
  say "creating the admin user ($ADMIN_USER / $ADMIN_PASS)"
  SETUP_TOKEN="$("${COMPOSE[@]}" logs reeve-server 2>&1 \
    | grep -oE 'rvs_[a-f0-9]+' | tail -1)"
  if [ -n "$SETUP_TOKEN" ]; then
    # Don't -f: a 409 (admin already exists) is not fatal here — we
    # fall through and let the login below be the real gate.
    code="$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/auth/setup" \
      -H 'content-type: application/json' \
      -d "{\"setup_token\":\"$SETUP_TOKEN\",\"username\":\"$ADMIN_USER\",\"password\":\"$ADMIN_PASS\"}")"
    say "setup returned HTTP $code"
  fi
  if ! login; then
    echo >&2
    echo "Could not log in as $ADMIN_USER after setup." >&2
    echo "The server volume likely has a stale admin from an earlier run with" >&2
    echo "different credentials. Reset it and try again:" >&2
    echo "    just dev-down && just dev-up ${N}" >&2
    exit 1
  fi
fi

# --- enable the remote terminal fleet-wide ---------------------------
# Author config/terminal.yaml into the fleet layer; render places it in
# every device's bundle, and the agent + server both honour it.
say "enabling the remote terminal fleet-wide"
TERMINAL_CFG_B64="$(printf 'enabled: true\nidleTimeoutSecs: 600\nhardCapSecs: 3600\n' | base64 | tr -d '\n')"
curl -fsS -X PUT "$BASE/api/tree/layers/00-fleet" \
  -H 'content-type: application/json' -b "$COOKIES" \
  -d "{\"message\":\"dev: enable remote terminal fleet-wide\",\"files\":{\"config/terminal.yaml\":\"$TERMINAL_CFG_B64\"}}" \
  >/dev/null

# --- mint a multi-use join token -------------------------------------
say "minting a join token for $N devices"
JOIN_RESP="$(curl -fsS -X POST "$BASE/api/join-tokens" \
  -H 'content-type: application/json' -b "$COOKIES" \
  -d "{\"max_uses\":$N,\"ttl_secs\":86400}")"
JOIN_TOKEN="$(printf '%s' "$JOIN_RESP" \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["join_token"])')"

# --- start the devices -----------------------------------------------
say "starting $N virtual devices"
JOIN_TOKEN="$JOIN_TOKEN" "${COMPOSE[@]}" up -d --build --scale device="$N" device

cat <<EOF

  Fleet is up.

    UI:        $BASE
    login:     $ADMIN_USER / $ADMIN_PASS
    devices:   $N (Devices page — presence turns green as they connect)
    terminal:  open a device → Terminal tab → you get a shell in it

    logs:      docker compose -f compose.dev.yml logs -f
    teardown:  ./scripts/dev-down.sh
EOF
