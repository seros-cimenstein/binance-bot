#!/usr/bin/env bash
set -euo pipefail

if [ "${EUID:-$(id -u)}" -ne 0 ]; then
    echo "Run this cutover once with sudo: sudo scripts/live_websocket_cutover.sh" >&2
    exit 1
fi

SOURCE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT_DIR="${BINANCE_BOT_PROJECT_DIR:-/opt/binance-bot}"
ENV_FILE="${BINANCE_BOT_ENV_FILE:-$PROJECT_DIR/.env}"
BACKUP_DIR="${BINANCE_BOT_BACKUP_DIR:-$PROJECT_DIR/backups}"
SERVICE_NAME="${BINANCE_BOT_SERVICE_NAME:-binance-bot.service}"

configured_value() {
    local key="$1"
    local default="$2"
    local line

    line=$(grep -E "^${key}=" "$ENV_FILE" | tail -n 1 || true)
    if [ -n "$line" ]; then
        printf '%s\n' "${line#*=}"
        return
    fi
    printf '%s\n' "$default"
}

if [ ! -f "$ENV_FILE" ]; then
    echo "Live environment file does not exist: $ENV_FILE" >&2
    exit 1
fi
if [ "$(configured_value USE_TESTNET true)" != "false" ] ||
   [ "$(configured_value DRY_RUN false)" != "false" ] ||
   [ "$(configured_value CONFIRM_LIVE_TRADING no)" != "yes" ]; then
    echo "This cutover requires the already-approved live runtime configuration." >&2
    exit 1
fi

echo "Deploying the validated working tree..."
(
    cd "$SOURCE_DIR"
    scripts/setup_deploy.sh
)

echo "Installing daily database backup units..."
install -m 0644 \
    "$PROJECT_DIR/deploy/binance-bot-backup.service" \
    "$PROJECT_DIR/deploy/binance-bot-backup.timer" \
    /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now binance-bot-backup.timer
systemctl start binance-bot-backup.service

latest_backup=$(find "$BACKUP_DIR" -maxdepth 1 -type f -name "live-*.db" \
    -printf '%T@ %p\n' | sort -rn | head -n 1 | cut -d' ' -f2-)
if [ -z "$latest_backup" ] ||
   [ "$(sqlite3 "$latest_backup" "PRAGMA quick_check;" | tr -d '\r')" != "ok" ]; then
    echo "Verified deployed backup was not created; refusing runtime restart." >&2
    exit 1
fi
echo "Verified deployed backup before restart: $latest_backup"

env_tmp=$(mktemp)
trap 'rm -f "$env_tmp"' EXIT
awk '
    BEGIN { market=0; orders=0; cooldown=0 }
    /^MARKET_DATA_MODE=/ { print "MARKET_DATA_MODE=websocket"; market=1; next }
    /^ORDER_UPDATE_MODE=/ { print "ORDER_UPDATE_MODE=websocket"; orders=1; next }
    /^WS_ALERT_COOLDOWN_SECONDS=/ { print "WS_ALERT_COOLDOWN_SECONDS=300"; cooldown=1; next }
    { print }
    END {
        if (!market) print "MARKET_DATA_MODE=websocket"
        if (!orders) print "ORDER_UPDATE_MODE=websocket"
        if (!cooldown) print "WS_ALERT_COOLDOWN_SECONDS=300"
    }
' "$ENV_FILE" > "$env_tmp"
chown --reference="$ENV_FILE" "$env_tmp"
chmod --reference="$ENV_FILE" "$env_tmp"
mv -f -- "$env_tmp" "$ENV_FILE"
trap - EXIT

echo "Restarting $SERVICE_NAME with WebSocket modes enabled..."
systemctl restart "$SERVICE_NAME"
systemctl show "$SERVICE_NAME" -p ActiveState -p MainPID -p NRestarts --no-pager
"$PROJECT_DIR/scripts/status.sh"

echo "Cutover submitted. Monitor journal logs for public and user-data WebSocket connections."
