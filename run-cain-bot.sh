#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NODE_BIN="${NODE_BIN:-node}"

cd "$SCRIPT_DIR"
export CAINBOT_CONFIG="${CAINBOT_CONFIG:-$SCRIPT_DIR/config.json}"
exec "$NODE_BIN" src/index.mjs
