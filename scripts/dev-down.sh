#!/usr/bin/env bash
# scripts/dev-down.sh — tear down the local dev fleet, volumes included.
set -euo pipefail
cd "$(dirname "$0")/.."
# JOIN_TOKEN is only needed to START the device service; `down` doesn't
# interpolate it, but set a dummy so compose never prompts.
JOIN_TOKEN=unused docker compose -f compose.dev.yml down -v --remove-orphans
echo "dev fleet down (volumes removed)."
