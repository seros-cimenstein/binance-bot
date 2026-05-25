#!/usr/bin/env bash
set -euo pipefail

# Send SIGUSR1 to every matching bot process so runtime rollback closes tracked
# positions before shutdown.
PROCESS_PATTERN="${BINANCE_BOT_PROCESS_PATTERN:-python.*-m src.main}"

mapfile -t PIDS < <(pgrep -f "$PROCESS_PATTERN" || true)

if [ "${#PIDS[@]}" -eq 0 ]; then
    echo "Binance bot is not running."
    exit 0
fi

echo "Sending SIGUSR1 (panic rollback) to ${#PIDS[@]} binance-bot process(es): ${PIDS[*]}"
for pid in "${PIDS[@]}"; do
    kill -USR1 "$pid"
done
