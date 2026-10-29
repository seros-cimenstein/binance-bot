#!/usr/bin/env bash
# cryptobot-gui on the Pi's live data, from another machine.
#
# The trader runs on the Pi, so a dashboard started elsewhere has no Bot Spot data. This
# pulls the Pi's bot data dir into a local mirror with rsync (once, then every
# MIRROR_SECONDS), points cryptobot-gui at the mirror and runs it in the foreground. Ctrl+C
# stops both. Nothing is written on the Pi: the only remote commands are rsync reads and one
# grep of the non-secret exit-plan settings in the Pi's env file. omp logins, the bot
# session and the Telegram session file are not copied.
#
# Extra arguments go to cryptobot-gui, e.g. `scripts/cryptobot_gui_pi.sh --open`.
#
# Settings (env): PI_HOST (pi-wired), PI_DATA_DIR (binance-bot/docker-data),
# PI_ENV_FILE (binance-bot/.env.docker), MIRROR_DIR (~/.cache/binance-bot/pi-data),
# MIRROR_SECONDS (30), CRYPTOBOT_GUI_BIN (target/release/cryptobot-gui of this repo),
# RUNTIME_DIR (~/.local/share/binance-bot: its .env holds the read-only Binance keys the
# wallet tab uses; the repo dir when missing).
set -euo pipefail

PI_HOST="${PI_HOST:-pi-wired}"
PI_DATA_DIR="${PI_DATA_DIR:-binance-bot/docker-data}"
PI_ENV_FILE="${PI_ENV_FILE:-binance-bot/.env.docker}"
MIRROR_DIR="${MIRROR_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/binance-bot/pi-data}"
MIRROR_SECONDS="${MIRROR_SECONDS:-30}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GUI_BIN="${CRYPTOBOT_GUI_BIN:-$REPO_DIR/target/release/cryptobot-gui}"
RUNTIME_DIR="${RUNTIME_DIR:-$HOME/.local/share/binance-bot}"
[[ -d "$RUNTIME_DIR" ]] || RUNTIME_DIR="$REPO_DIR"

if [[ ! -x "$GUI_BIN" ]]; then
    echo "error: $GUI_BIN not found; build it with: cargo build --release -p cryptobot-gui" >&2
    exit 1
fi
# --delete below must only ever touch the mirror.
case "$MIRROR_DIR" in
    "" | "/" | "$HOME" | "$HOME/") echo "error: refusing MIRROR_DIR=$MIRROR_DIR" >&2; exit 1 ;;
esac
mkdir -p "$MIRROR_DIR"

# A dashboard already on the port that is not reading this mirror (an older local run, or an
# older build) would just be opened by `--open` again; stop it so this one can listen.
bind="${CRYPTOBOT_GUI_BIND:-127.0.0.1:8787}"
args=("$@")
for i in "${!args[@]}"; do
    case "${args[$i]}" in
        --bind) bind="${args[$((i + 1))]:-$bind}" ;;
        --bind=*) bind="${args[$i]#--bind=}" ;;
    esac
done
port="${bind##*:}"
if holder="$(ss -Hltnp "sport = :$port" 2>/dev/null | grep -o 'pid=[0-9]*' | head -n1 | cut -d= -f2)" \
    && [[ -n "$holder" ]] \
    && [[ "$(cat "/proc/$holder/comm" 2>/dev/null)" == "cryptobot-gui" ]] \
    && ! tr '\0' '\n' 2>/dev/null < "/proc/$holder/environ" | grep -qxF -- "BOT_SPOT_AUDIT_PATH=$MIRROR_DIR/bot-spot-decisions.jsonl"; then
    echo "cryptobot-gui-pi: stopping the old dashboard on port $port (pid $holder, not on the Pi data)" >&2
    kill "$holder"
    for _ in {1..50}; do kill -0 "$holder" 2>/dev/null || break; sleep 0.1; done
fi

mirror() {
    # --delete keeps removals visible (e.g. a deleted emergency stop file); rsync replaces
    # files by rename, so the dashboard never reads a half-copied file.
    rsync -a --delete --timeout=20 \
        --exclude 'omp-agent/' --exclude 'bot-spot-session/' --exclude '*.session' \
        --exclude 'archive/' \
        "$PI_HOST:$PI_DATA_DIR/" "$MIRROR_DIR/"
}

echo "cryptobot-gui-pi: mirroring $PI_HOST:$PI_DATA_DIR -> $MIRROR_DIR" >&2
mirror

# The trader's file paths are relative to /app inside its container (data/... or
# /app/data/...); map them onto the mirror. Unset ones use the trader's defaults.
declare -A paths=(
    [BOT_SPOT_AUDIT_PATH]=bot-spot-decisions.jsonl
    [BOT_SPOT_JOURNAL_PATH]=bot-spot-journal.jsonl
    [BOT_SPOT_STATE_PATH]=bot-spot-agent-state.json
    [BOT_SPOT_STOP_PATH]=bot-spot.stop
    [BOT_SPOT_EXIT_PLANS_PATH]=bot-spot-exit-plans.json
    [BOT_SPOT_EXIT_WATCH_STATE_PATH]=bot-spot-exit-watch-state.json
    [BOT_SPOT_EXIT_WATCH_LOG_PATH]=bot-spot-exit-watch.jsonl
)
for name in "${!paths[@]}"; do
    export "$name=$MIRROR_DIR/${paths[$name]}"
done
settings_pattern='^BOT_SPOT_(EXIT_PLANS_LIVE|DEFAULT_EXIT_PLAN|DEFAULT_EXIT_PLAN_FROM|EXIT_POLL_SECONDS|AUDIT_PATH|JOURNAL_PATH|STATE_PATH|STOP_PATH|EXIT_PLANS_PATH|EXIT_WATCH_STATE_PATH|EXIT_WATCH_LOG_PATH)='
if remote_settings="$(ssh -o ConnectTimeout=10 "$PI_HOST" "grep -E '$settings_pattern' $PI_ENV_FILE" 2>/dev/null)"; then
    while IFS='=' read -r name value; do
        [[ "$name" =~ ^BOT_SPOT_[A-Z_]+$ ]] || continue
        value="${value%\"}"; value="${value#\"}"; value="${value%\'}"; value="${value#\'}"
        if [[ "$name" == *_PATH ]]; then
            case "$value" in
                data/*) value="$MIRROR_DIR/${value#data/}" ;;
                /app/data/*) value="$MIRROR_DIR/${value#/app/data/}" ;;
                *) echo "cryptobot-gui-pi: $name=$value is outside the data dir; using the default" >&2; continue ;;
            esac
        fi
        export "$name=$value"
    done <<< "$remote_settings"
else
    echo "cryptobot-gui-pi: could not read the exit-plan settings from $PI_HOST:$PI_ENV_FILE; the Exits tab shows the mode as unknown" >&2
fi

(
    while sleep "$MIRROR_SECONDS"; do
        mirror || echo "cryptobot-gui-pi: mirror refresh failed; retrying in ${MIRROR_SECONDS}s" >&2
    done
) &
refresher=$!
trap 'kill "$refresher" 2>/dev/null || true' EXIT

cd "$RUNTIME_DIR"
"$GUI_BIN" "$@"
