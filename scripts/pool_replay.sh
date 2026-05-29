#!/usr/bin/env bash
set -euo pipefail

SCRIPT_PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT_DIR="${BINANCE_BOT_PROJECT_DIR:-$SCRIPT_PROJECT_DIR}"
PYTHON_BIN="${BINANCE_BOT_PYTHON:-$PROJECT_DIR/.venv/bin/python}"
CYCLES="${POOL_REPLAY_CYCLES:-24}"
DB_DIR="${POOL_REPLAY_DB_DIR:-/tmp}"

if [ "$#" -gt 0 ]; then
    fixtures=("$@")
else
    fixtures=(
        "$PROJECT_DIR/tests/fixtures/pool_market_replay.json"
        "$PROJECT_DIR/tests/fixtures/pool_market_replay_stress.json"
    )
fi

mkdir -p "$DB_DIR"

for fixture in "${fixtures[@]}"; do
    name="$(basename "$fixture" .json)"
    db_path="$DB_DIR/binance-bot-${name}.db"
    echo "Replay fixture: $fixture"
    "$PYTHON_BIN" -m src.pool.replay \
        --fixture "$fixture" \
        --cycles "$CYCLES" \
        --db-path "$db_path" \
        --quiet
done
