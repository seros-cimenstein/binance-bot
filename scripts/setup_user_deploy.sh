#!/usr/bin/env bash
set -euo pipefail

SOURCE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT_DIR="${BINANCE_BOT_USER_PROJECT_DIR:-$HOME/.local/share/binance-bot}"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
SOURCE_ENV="${BINANCE_BOT_SOURCE_ENV_FILE:-/opt/binance-bot/.env}"

echo "Staging user-owned Binance Bot in $PROJECT_DIR..."
mkdir -p "$PROJECT_DIR" "$PROJECT_DIR/bin" "$PROJECT_DIR/data" "$PROJECT_DIR/logs" "$PROJECT_DIR/backups" "$UNIT_DIR"

echo "Building the bb and cryptobot-gui release binaries..."
cargo build --release --locked -p bb-cli -p cryptobot-gui --manifest-path "$SOURCE_DIR/Cargo.toml"
for binary in bb cryptobot-gui; do
    install -m 0755 "$SOURCE_DIR/target/release/$binary" "$PROJECT_DIR/bin/$binary.new"
    mv -f -- "$PROJECT_DIR/bin/$binary.new" "$PROJECT_DIR/bin/$binary"
done

# Runtime only needs the binary plus operator scripts; source stays in the checkout.
rsync -a --delete "$SOURCE_DIR/scripts/" "$PROJECT_DIR/scripts/"

if [ ! -f "$PROJECT_DIR/.env" ]; then
    if [ ! -r "$SOURCE_ENV" ]; then
        echo "Cannot seed the user environment from $SOURCE_ENV" >&2
        exit 1
    fi
    install -m 0600 "$SOURCE_ENV" "$PROJECT_DIR/.env"
fi
chmod 0600 "$PROJECT_DIR/.env"

install -m 0644 "$SOURCE_DIR"/deploy/user/*.service "$UNIT_DIR/"
systemctl --user daemon-reload

echo "User deployment staged. No service has been started or restarted."
echo "Start the trader with: systemctl --user enable --now binance-bot-spot.service"
echo "(if it already runs, restart it to load the new binary)."
