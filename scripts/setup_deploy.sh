#!/usr/bin/env bash
set -euo pipefail

PROJECT_DIR="${BINANCE_BOT_PROJECT_DIR:-/opt/binance-bot}"
SERVICE_USER="${BINANCE_BOT_SERVICE_USER:-bot}"
SERVICE_GROUP="${BINANCE_BOT_SERVICE_GROUP:-bot}"
echo "Setting up Binance Bot in $PROJECT_DIR..."

sudo mkdir -p "$PROJECT_DIR"

if ! command -v rsync >/dev/null 2>&1; then
    echo "rsync is required for deployment copy exclusions" >&2
    exit 1
fi

sudo rsync -a --delete \
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
    ./ "$PROJECT_DIR/"

sudo chown -R "$SERVICE_USER:$SERVICE_GROUP" "$PROJECT_DIR"

# Set up venv
sudo -u "$SERVICE_USER" python3 -m venv "$PROJECT_DIR/venv"
sudo -u "$SERVICE_USER" "$PROJECT_DIR/venv/bin/pip" install "$PROJECT_DIR"

# Create logs and data dirs
sudo -u "$SERVICE_USER" mkdir -p "$PROJECT_DIR/logs" "$PROJECT_DIR/data"

echo "Deployment setup complete."
echo "Please configure $PROJECT_DIR/.env"
echo "Then install the service: sudo cp deploy/binance-bot.service /etc/systemd/system/ && sudo systemctl daemon-reload && sudo systemctl enable binance-bot"
