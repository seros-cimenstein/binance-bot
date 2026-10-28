#!/usr/bin/env bash
# Copy the /biz/ feed from this machine to the Pi, once or every COPY_SECONDS.
#
# biz-app writes biz-feed.json (biz-feed-v1) on the laptop; the trader on the Pi reads
# data/biz-feed.json (BOT_SPOT_BIZ_FEED_PATH) every cycle and records it in the cycle
# summary, information only. This is the reverse of scripts/cryptobot_gui_pi.sh: the only
# remote write is that one file, and rsync replaces it by rename, so the trader never reads
# a half-copied feed. A missing local feed is skipped with a message; the trader then
# records "/biz/ unavailable" once its copy is older than BOT_SPOT_BIZ_MAX_AGE_MINUTES.
#
# Usage: scripts/biz_feed_to_pi.sh [--once]
#
# Settings (env): PI_HOST (pi-wired), PI_DATA_DIR (binance-bot/docker-data),
# BIZ_FEED (~/projects/automation/imageboards/biz-app/biz_insights/out/biz-feed.json),
# COPY_SECONDS (300).
set -euo pipefail

PI_HOST="${PI_HOST:-pi-wired}"
PI_DATA_DIR="${PI_DATA_DIR:-binance-bot/docker-data}"
BIZ_FEED="${BIZ_FEED:-$HOME/projects/automation/imageboards/biz-app/biz_insights/out/biz-feed.json}"
COPY_SECONDS="${COPY_SECONDS:-300}"

once=false
case "${1:-}" in
    "") ;;
    --once) once=true ;;
    *) echo "usage: $0 [--once]" >&2; exit 2 ;;
esac

copy() {
    if [[ ! -f "$BIZ_FEED" ]]; then
        echo "biz-feed-to-pi: $BIZ_FEED not found (is biz-app writing the feed?); skipped" >&2
        return 1
    fi
    rsync -t --chmod=F644 --timeout=20 "$BIZ_FEED" "$PI_HOST:$PI_DATA_DIR/biz-feed.json"
}

echo "biz-feed-to-pi: $BIZ_FEED -> $PI_HOST:$PI_DATA_DIR/biz-feed.json" >&2
if $once; then
    copy
    exit
fi
while true; do
    copy || echo "biz-feed-to-pi: copy failed; retrying in ${COPY_SECONDS}s" >&2
    sleep "$COPY_SECONDS"
done
