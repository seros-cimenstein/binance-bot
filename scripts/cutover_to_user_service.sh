#!/usr/bin/env bash
set -euo pipefail

if [ "${EUID:-$(id -u)}" -eq 0 ]; then
    echo "Run this script as your normal user; it requests sudo only to stop the old system service." >&2
    exit 1
fi

SOURCE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT_DIR="${BINANCE_BOT_USER_PROJECT_DIR:-$HOME/.local/share/binance-bot}"
SOURCE_DB="${BINANCE_BOT_SOURCE_DB:-/opt/binance-bot/data/live.db}"

if systemctl --user is-active --quiet binance-bot.service; then
    echo "User bot service is already active; the one-time handoff is complete." >&2
    echo "Use scripts/setup_user_deploy.sh followed by systemctl --user restart binance-bot.service for future deploys." >&2
    exit 1
fi

"$SOURCE_DIR/scripts/setup_user_deploy.sh"

echo "Verifying the staged bot without placing orders..."
(
    cd "$PROJECT_DIR"
    "$PROJECT_DIR/venv/bin/python" -m src.ops_check
)

echo "Stopping and disabling the old system service. This is the final sudo operation required for routine bot management."
sudo systemctl disable --now binance-bot-backup.timer binance-bot.service

if systemctl is-active --quiet binance-bot.service; then
    echo "Old system service is still active; refusing to start user runtime." >&2
    exit 1
fi

final_db="$PROJECT_DIR/data/live.db.handoff.$$"
sqlite3 "$SOURCE_DB" ".backup '$final_db'"
if [ "$(sqlite3 "$final_db" "PRAGMA quick_check;" | tr -d '\r')" != "ok" ]; then
    rm -f -- "$final_db"
    echo "Final database handoff backup failed integrity check" >&2
    exit 1
fi
mv -f -- "$final_db" "$PROJECT_DIR/data/live.db"

systemctl --user enable --now binance-bot-backup.timer
systemctl --user start binance-bot-backup.service
systemctl --user enable --now binance-bot.service

"$PROJECT_DIR/scripts/status.sh"
echo "User-service handoff complete. Future bot management uses systemctl --user without sudo."
