#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CAINBOT_EXE="${CAINBOT_EXE:-$SCRIPT_DIR/target/release/cainbot-rs}"
CARGO_BIN="${CARGO_BIN:-/root/.cargo/bin/cargo}"

cd "$SCRIPT_DIR"
export CAINBOT_CONFIG="${CAINBOT_CONFIG:-$SCRIPT_DIR/config.json}"
if [[ ! -f "$CAINBOT_EXE" ]]; then
  echo "[INFO] Rust binary not found, building release binary..."
  "$CARGO_BIN" build --release --bin cainbot-rs
fi
exec "$CAINBOT_EXE"
