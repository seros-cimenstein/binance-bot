#!/usr/bin/env bash
# Nightly archive of the Bot Spot data files: the decisions audit
# (the only record of every trade) and the trader state.
# Keeps the newest
# BINANCE_BOT_BACKUP_RETENTION archives (default 14) in BINANCE_BOT_BACKUP_DIR.
#
# Paths come from the trader's own settings (process env, then the .env file, then the
# trader's defaults), relative to the project dir like the trader resolves them. Missing files
# are skipped (a fresh install has none yet); an archive with none of them fails.
# Reads only; the archive is written to a temp name, checked with `tar -tzf`, then renamed.
set -euo pipefail

SCRIPT_PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT_DIR="${BINANCE_BOT_PROJECT_DIR:-$SCRIPT_PROJECT_DIR}"
ENV_FILE="${BINANCE_BOT_ENV_FILE:-$PROJECT_DIR/.env}"
BACKUP_DIR="${BINANCE_BOT_BACKUP_DIR:-$PROJECT_DIR/backups}"
RETENTION="${BINANCE_BOT_BACKUP_RETENTION:-14}"

if ! [[ "$RETENTION" =~ ^[1-9][0-9]*$ ]]; then
    echo "BINANCE_BOT_BACKUP_RETENTION must be a positive integer" >&2
    exit 1
fi

# Process environment wins (Docker passes settings via env_file), then .env, then default.
configured_value() {
    local key="$1" default="$2" line
    if [ -n "${!key:-}" ]; then
        printf '%s\n' "${!key}"
        return
    fi
    if [ -f "$ENV_FILE" ]; then
        line=$(grep -E "^${key}=" "$ENV_FILE" | tail -n 1 || true)
        if [ -n "$line" ] && [ -n "${line#*=}" ]; then
            printf '%s\n' "${line#*=}"
            return
        fi
    fi
    printf '%s\n' "$default"
}

# Relative to the project dir, as the trader opens them; absolute paths stay absolute.
relative_path() {
    local path="$1"
    case "$path" in
        /*) printf '%s\n' "$path" ;;
        *) printf '%s\n' "${path#./}" ;;
    esac
}

AUDIT=$(relative_path "$(configured_value BOT_SPOT_AUDIT_PATH data/bot-spot-decisions.jsonl)")
declare -a WANTED=(
    "$AUDIT"
    "$(relative_path "$(configured_value BOT_SPOT_STATE_PATH data/bot-spot-agent-state.json)")"
)

cd "$PROJECT_DIR"
declare -a PRESENT=()
for path in "${WANTED[@]}"; do
    if [ -e "$path" ]; then
        PRESENT+=("$path")
    fi
done
if [ "${#PRESENT[@]}" -eq 0 ]; then
    echo "No Bot Spot data found under $PROJECT_DIR" >&2
    exit 1
fi

mkdir -p "$BACKUP_DIR"
TIMESTAMP=$(date -u +"%Y%m%dT%H%M%SZ")
ARCHIVE="$BACKUP_DIR/bot-spot-data-${TIMESTAMP}-$$.tar.gz"
TMP="$ARCHIVE.part"
# The trader appends while this runs; a line cut at the end of a JSONL file is skipped by
# every reader, so a live copy is consistent enough for a restore.
tar -czf "$TMP" -- "${PRESENT[@]}"
tar -tzf "$TMP" >/dev/null
mv -- "$TMP" "$ARCHIVE"

mapfile -t ARCHIVES < <(
    find "$BACKUP_DIR" -maxdepth 1 -type f -name 'bot-spot-data-*.tar.gz' \
        -printf '%T@ %p\n' | sort -rn | cut -d' ' -f2-
)
for ((index=RETENTION; index<${#ARCHIVES[@]}; index++)); do
    rm -f -- "${ARCHIVES[$index]}"
done

echo "Bot Spot data archive: $ARCHIVE (${#PRESENT[@]} paths)"
