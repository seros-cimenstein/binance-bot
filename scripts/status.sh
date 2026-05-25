#!/usr/bin/env bash
set -euo pipefail

PROCESS_PATTERN="${BINANCE_BOT_PROCESS_PATTERN:-python.*-m src.main}"
SERVICE_NAME="${BINANCE_BOT_SERVICE_NAME:-binance-bot.service}"
PROJECT_DIR="${BINANCE_BOT_PROJECT_DIR:-/opt/binance-bot}"
ENV_FILE="${BINANCE_BOT_ENV_FILE:-$PROJECT_DIR/.env}"

configured_value() {
    local key="$1"
    local default="$2"
    local line

    if [ -f "$ENV_FILE" ]; then
        line=$(grep -E "^${key}=" "$ENV_FILE" | tail -n 1 || true)
        if [ -n "$line" ]; then
            printf '%s\n' "${line#*=}"
            return
        fi
    fi

    printf '%s\n' "$default"
}

resolve_runtime_path() {
    local path="$1"
    case "$path" in
        /*) printf '%s\n' "$path" ;;
        *) printf '%s/%s\n' "$PROJECT_DIR" "$path" ;;
    esac
}

DB_PATH=$(resolve_runtime_path "$(configured_value DB_PATH data/bot.db)")
LOG_PATH=$(resolve_runtime_path "$(configured_value LOG_PATH logs/bot.log)")

echo "Process status:"
if pgrep -af "$PROCESS_PATTERN"; then
    :
else
    echo "  no matching bot processes"
fi

echo
echo "Systemd status:"
if command -v systemctl >/dev/null 2>&1; then
    systemctl is-active "$SERVICE_NAME" || true
else
    echo "  systemctl not available"
fi

echo
echo "Runtime files:"
for path in "$DB_PATH" "$LOG_PATH"; do
    if [ -e "$path" ]; then
        ls -lh "$path"
    else
        echo "  missing: $path"
    fi
done
