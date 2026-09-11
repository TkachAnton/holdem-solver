#!/usr/bin/env bash
# Linux/macOS launcher: build if needed, start server, print URL.
set -e
cd "$(dirname "$0")/.."
BIN=target/release/holdem-solver-server
PORT=${PORT:-8080}
DATA=${DATA:-$HOME/holdem-solver-data}
if [ ! -x "$BIN" ] || [ "${1:-}" = "rebuild" ]; then
  cargo build --release -p holdem-solver-server
fi
exec "$BIN" --bind "0.0.0.0:$PORT" --data-dir "$DATA" --max-active-jobs 1
