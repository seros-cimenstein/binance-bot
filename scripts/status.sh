#!/usr/bin/env bash
set -euo pipefail

PROCESS_PATTERN="${BINANCE_BOT_PROCESS_PATTERN:-bb main}"
SERVICE_NAME="${BINANCE_BOT_SERVICE_NAME:-binance-bot.service}"
SCRIPT_PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT_DIR="${BINANCE_BOT_PROJECT_DIR:-$SCRIPT_PROJECT_DIR}"
ENV_FILE="${BINANCE_BOT_ENV_FILE:-$PROJECT_DIR/.env}"
BACKUP_DIR="${BINANCE_BOT_BACKUP_DIR:-$PROJECT_DIR/backups}"
SERVICE_SCOPE="${BINANCE_BOT_SERVICE_SCOPE:-}"

if [ -z "$SERVICE_SCOPE" ]; then
    case "$PROJECT_DIR" in
        "$HOME"/*) SERVICE_SCOPE="user" ;;
        *) SERVICE_SCOPE="system" ;;
    esac
fi

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

echo "Runtime configuration:"
echo "  USE_TESTNET=$(configured_value USE_TESTNET true)"
echo "  DRY_RUN=$(configured_value DRY_RUN false)"
echo "  MARKET_DATA_MODE=$(configured_value MARKET_DATA_MODE polling)"
echo "  ORDER_UPDATE_MODE=$(configured_value ORDER_UPDATE_MODE disabled)"
echo "  DB_PATH=$DB_PATH"

echo
echo "Process status:"
if pgrep -af "$PROCESS_PATTERN"; then
    :
else
    echo "  no matching bot processes"
fi

echo
echo "Systemd status:"
if command -v systemctl >/dev/null 2>&1; then
    if [ "$SERVICE_SCOPE" = "user" ]; then
        systemctl --user is-active "$SERVICE_NAME" || true
    else
        systemctl is-active "$SERVICE_NAME" || true
    fi
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

echo
echo "Newest database backup:"
if [ -d "$BACKUP_DIR" ]; then
    latest_backup=$(find "$BACKUP_DIR" -maxdepth 1 -type f -name "*.db" \
        -printf '%T@ %p\n' | sort -rn | head -n 1 | cut -d' ' -f2- || true)
else
    latest_backup=""
fi
if [ -n "$latest_backup" ]; then
    ls -lh "$latest_backup"
else
    echo "  no backups found in $BACKUP_DIR"
fi
