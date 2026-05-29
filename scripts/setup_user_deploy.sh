#!/usr/bin/env bash
set -euo pipefail

SOURCE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT_DIR="${BINANCE_BOT_USER_PROJECT_DIR:-$HOME/.local/share/binance-bot}"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
SOURCE_ENV="${BINANCE_BOT_SOURCE_ENV_FILE:-/opt/binance-bot/.env}"
SOURCE_DB="${BINANCE_BOT_SOURCE_DB:-/opt/binance-bot/data/live.db}"

echo "Staging user-owned Binance Bot in $PROJECT_DIR..."
mkdir -p "$PROJECT_DIR" "$PROJECT_DIR/data" "$PROJECT_DIR/logs" "$PROJECT_DIR/backups" "$UNIT_DIR"

rsync -a --delete \
    --exclude "/.env" \
    --exclude "/.env.*" \
    --exclude "/.git/" \
    --exclude "/.backtest_cache/" \
    --exclude "/.venv/" \
    --exclude "/venv/" \
    --exclude "__pycache__/" \
    --exclude ".pytest_cache/" \
    --exclude "build/" \
    --exclude "dist/" \
    --exclude "*.egg-info/" \
    --exclude "*.log" \
    --exclude "*.db" \
    --exclude "/data/" \
    --exclude "/logs/" \
    --exclude "/backups/" \
    "$SOURCE_DIR/" "$PROJECT_DIR/"

if [ ! -f "$PROJECT_DIR/.env" ]; then
    if [ ! -r "$SOURCE_ENV" ]; then
        echo "Cannot seed the user environment from $SOURCE_ENV" >&2
        exit 1
    fi
    install -m 0600 "$SOURCE_ENV" "$PROJECT_DIR/.env"
fi
chmod 0600 "$PROJECT_DIR/.env"

if [ ! -f "$PROJECT_DIR/data/live.db" ] && [ -r "$SOURCE_DB" ]; then
    staging_db="$PROJECT_DIR/data/live.db.stage.$$"
    sqlite3 "$SOURCE_DB" ".backup '$staging_db'"
    if [ "$(sqlite3 "$staging_db" "PRAGMA quick_check;" | tr -d '\r')" != "ok" ]; then
        rm -f -- "$staging_db"
        echo "Staged database failed integrity check" >&2
        exit 1
    fi
    mv -f -- "$staging_db" "$PROJECT_DIR/data/live.db"
    echo "Seeded initial user database from $SOURCE_DB."
else
    echo "Preserving existing user database at $PROJECT_DIR/data/live.db."
fi

python3 -m venv "$PROJECT_DIR/venv"
"$PROJECT_DIR/venv/bin/pip" install "$PROJECT_DIR"

install -m 0644 "$SOURCE_DIR"/deploy/user/*.service "$SOURCE_DIR"/deploy/user/*.timer "$UNIT_DIR/"
systemctl --user daemon-reload

if systemctl --user is-active --quiet binance-pool-paper.service; then
    echo "Paper pool deployment refreshed. Restart binance-pool-paper.service to load updated code."
elif systemctl --user is-active --quiet binance-bot.service; then
    echo "Legacy trading service is active; disable it before starting binance-pool-paper.service." >&2
else
    echo "User deployment staged. No trading service has been started."
    echo "Start paper evaluation with: systemctl --user enable --now binance-pool-paper.service"
fi
