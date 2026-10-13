#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "Usage: $0 /path/to/verified-backup.db" >&2
    exit 2
fi

SCRIPT_PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT_DIR="${BINANCE_BOT_PROJECT_DIR:-$SCRIPT_PROJECT_DIR}"
ENV_FILE="${BINANCE_BOT_ENV_FILE:-$PROJECT_DIR/.env}"
SERVICE_NAME="${BINANCE_BOT_SERVICE_NAME:-binance-bot.service}"
SERVICE_SCOPE="${BINANCE_BOT_SERVICE_SCOPE:-}"
DB_SETTING_KEY="${BINANCE_BOT_DB_SETTING_KEY:-DB_PATH}"
DB_DEFAULT_PATH="${BINANCE_BOT_DB_DEFAULT_PATH:-data/bot.db}"
BACKUP_FILE="$1"

if [ -z "$SERVICE_SCOPE" ]; then
    case "$PROJECT_DIR" in
        "$HOME"/*) SERVICE_SCOPE="user" ;;
        *) SERVICE_SCOPE="system" ;;
    esac
fi

# Process environment wins (Docker passes settings via env_file, not .env),
# then the .env file, then the default.
configured_value() {
    local key="$1"
    local default="$2"
    local line

    if [ -n "${!key:-}" ]; then
        printf '%s\n' "${!key}"
        return
    fi
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

if [ ! -f "$BACKUP_FILE" ]; then
    echo "Backup file does not exist: $BACKUP_FILE" >&2
    exit 1
fi
if command -v systemctl >/dev/null 2>&1 &&
   { [ "$SERVICE_SCOPE" = "user" ] &&
       systemctl --user is-active --quiet "$SERVICE_NAME" ||
     [ "$SERVICE_SCOPE" != "user" ] &&
       systemctl is-active --quiet "$SERVICE_NAME"; }; then
    echo "Refusing restore while $SERVICE_NAME is active" >&2
    exit 1
fi
if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "sqlite3 is required for database restore validation" >&2
    exit 1
fi

QUICK_CHECK=$(sqlite3 "$BACKUP_FILE" "PRAGMA quick_check;" | tr -d '\r')
if [ "$QUICK_CHECK" != "ok" ]; then
    echo "Backup failed SQLite integrity check: $QUICK_CHECK" >&2
    exit 1
fi

DB_PATH=$(resolve_runtime_path "$(configured_value "$DB_SETTING_KEY" "$DB_DEFAULT_PATH")")
mkdir -p "$(dirname "$DB_PATH")"
if [ -f "$DB_PATH" ]; then
    "$(dirname "$0")/backup_db.sh"
fi

RESTORE_TMP="${DB_PATH}.restore.$$"
cp -- "$BACKUP_FILE" "$RESTORE_TMP"
if [ -f "$DB_PATH" ]; then
    chown --reference="$DB_PATH" "$RESTORE_TMP"
    chmod --reference="$DB_PATH" "$RESTORE_TMP"
fi
mv -f -- "$RESTORE_TMP" "$DB_PATH"

echo "Restored SQLite database: $DB_PATH"
