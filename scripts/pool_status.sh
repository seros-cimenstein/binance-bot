#!/usr/bin/env bash
set -euo pipefail

PROJECT_DIR="${BINANCE_POOL_PROJECT_DIR:-$HOME/.local/share/binance-bot}"
ENV_FILE="${BINANCE_POOL_ENV_FILE:-$PROJECT_DIR/.env}"
SERVICE_NAME="${BINANCE_POOL_SERVICE_NAME:-binance-pool-paper.service}"

configured_value() {
    local key="$1"
    local default="$2"
    local line
    line=$(grep -E "^${key}=" "$ENV_FILE" 2>/dev/null | tail -n 1 || true)
    if [ -n "$line" ]; then
        printf '%s\n' "${line#*=}"
    else
        printf '%s\n' "$default"
    fi
}

DB_PATH=$(configured_value POOL_DB_PATH data/pool-paper.db)
case "$DB_PATH" in
    /*) ;;
    *) DB_PATH="$PROJECT_DIR/$DB_PATH" ;;
esac

echo "Pool runtime: paper only"
echo "  Benchmarks=$(configured_value POOL_BENCHMARKS USDT,BTC,PAXG,XAUT)"
echo "  Max route legs=$(configured_value POOL_MAX_ROUTE_LEGS 3)"
echo "  DB_PATH=$DB_PATH"
echo
systemctl --user is-active "$SERVICE_NAME" || true
if [ -f "$DB_PATH" ]; then
    sqlite3 "$DB_PATH" \
        "select 'Catalog cycles: ' || count(*) from market_catalog_cycles;
         select 'Paper decisions: ' || count(*) from paper_decisions;
         select portfolio_id || ': ' || asset || '=' || printf('%.12g', amount)
           from virtual_balances order by portfolio_id, asset;"
else
    echo "No paper ledger yet."
fi
