#!/usr/bin/env bash
# Prints messgr-control stats once -- meant to be re-run under `watch`
# while scripts/e2e.sh's steps run, e.g.:
#   watch -n5 ./scripts/watch-stats.sh
#   watch -n5 ./scripts/watch-stats.sh acme --since 2026-09-01
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

STATE_FILE="$ROOT_DIR/.e2e/state.env"
TENANT_SLUG="acme"
if [[ -f "$STATE_FILE" ]]; then
    # shellcheck disable=SC1090
    source "$STATE_FILE"
    TENANT_SLUG="${TENANT_SLUG:-acme}"
fi

if [[ $# -gt 0 && "$1" != --* ]]; then
    TENANT_SLUG="$1"
    shift
fi

echo "[watch-stats] tenant=$TENANT_SLUG $(date -u +%H:%M:%S)"
cargo run --quiet --bin messgr-control -- stats --tenant-slug "$TENANT_SLUG" "$@"
