#!/usr/bin/env bash
set -euo pipefail

SCRIPT_PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT_DIR="${BINANCE_BOT_PROJECT_DIR:-$SCRIPT_PROJECT_DIR}"
ENV_FILE="${BINANCE_BOT_ENV_FILE:-$PROJECT_DIR/.env}"
BACKUP_DIR="${BINANCE_BOT_BACKUP_DIR:-$PROJECT_DIR/backups}"
RETENTION="${BINANCE_BOT_BACKUP_RETENTION:-14}"
DB_SETTING_KEY="${BINANCE_BOT_DB_SETTING_KEY:-DB_PATH}"
DB_DEFAULT_PATH="${BINANCE_BOT_DB_DEFAULT_PATH:-data/bot.db}"

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

if ! [[ "$RETENTION" =~ ^[1-9][0-9]*$ ]]; then
    echo "BINANCE_BOT_BACKUP_RETENTION must be a positive integer" >&2
    exit 1
fi
if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "sqlite3 is required for online database backups" >&2
    exit 1
fi

DB_PATH=$(resolve_runtime_path "$(configured_value "$DB_SETTING_KEY" "$DB_DEFAULT_PATH")")
if [ ! -f "$DB_PATH" ]; then
    echo "Runtime database does not exist: $DB_PATH" >&2
    exit 1
fi

mkdir -p "$BACKUP_DIR"
DB_NAME=$(basename "$DB_PATH")
DB_STEM="${DB_NAME%.db}"
TIMESTAMP=$(date -u +"%Y%m%dT%H%M%SZ")
BACKUP_PATH="$BACKUP_DIR/${DB_STEM}-${TIMESTAMP}-$$.db"

sqlite3 "$DB_PATH" ".backup '$BACKUP_PATH'"
QUICK_CHECK=$(sqlite3 "$BACKUP_PATH" "PRAGMA quick_check;" | tr -d '\r')
if [ "$QUICK_CHECK" != "ok" ]; then
    rm -f -- "$BACKUP_PATH"
    echo "Backup failed SQLite integrity check: $QUICK_CHECK" >&2
    exit 1
fi

mapfile -t BACKUPS < <(
    find "$BACKUP_DIR" -maxdepth 1 -type f -name "${DB_STEM}-*.db" \
        -printf '%T@ %p\n' | sort -rn | cut -d' ' -f2-
)
for ((index=RETENTION; index<${#BACKUPS[@]}; index++)); do
    rm -f -- "${BACKUPS[$index]}"
done

echo "Verified SQLite backup: $BACKUP_PATH"
