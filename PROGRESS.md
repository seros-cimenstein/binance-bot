# Binance Bot Progress

Use this file for cross-session handoff notes. Keep `IMPLEMENTATION_PLAN.md` as
the durable roadmap and use this file for the current working state.

## Current Status

- 2026-05-25: Roadmap phases 1 through 11 are implemented and locally verified.
- 2026-05-25: Documentation tracking was cleaned up by completing the phase 11
  coverage parent item and moving active handoff work into this file.
- 2026-05-25: Review/commit hygiene pass completed. The dirty tree contains
  expected roadmap implementation files, docs, scripts, and tests; no conflict
  markers or whitespace errors were found.
- 2026-05-25: Live-readiness assessment found an active deployed service running
  from `/opt/binance-bot` in real-money mode. Do not start another runtime or
  deploy this working tree until the current service is intentionally handled.
- 2026-05-25: The old `/opt/binance-bot` systemd service was stopped on request.
  The first deployment attempt was blocked because `/opt/binance-bot` was owned
  by `bot:bot` and `sudo` required interactive fingerprint/password auth.
- 2026-05-25: The current working tree was deployed to `/opt/binance-bot` after
  interactive fingerprint auth. The new `binance-bot.service` is active with
  PID `121585`, `NRestarts=0`, and startup logs from the new `src.main` path.
- 2026-05-25: Fixed deployment hygiene discovered during rollout: `src/data` was
  accidentally excluded by broad `data/` ignore/deploy patterns, so the rules
  now target only root `/data/` and `/logs/`.
- 2026-05-25: Removed stale deploy-only directories from `/opt/binance-bot` and
  updated `scripts/status.sh` so it reports the configured `DB_PATH`
  (`/opt/binance-bot/data/live.db`) instead of assuming `data/bot.db`.
- 2026-05-25: Post-deploy live monitoring completed for the requested live
  rollout. The service stayed active with PID `121585`, `NRestarts=0`, received
  BTC/USDT candles, and journal logs since startup showed no errors or
  tracebacks.

## Last Verified Commands

```bash
.venv/bin/python -m pytest -q
.venv/bin/python test_pipeline.py
scripts/smoke.sh
```

Last known results:

- 2026-05-25 `.venv/bin/python -m pytest -q`: `142 passed`
- 2026-05-25 `.venv/bin/python test_pipeline.py`: passed
- 2026-05-25 `scripts/smoke.sh`: passed
- 2026-05-25 `bash -n scripts/setup_deploy.sh && scripts/smoke.sh`: passed
- 2026-05-25 deployed import check:
  `cd /opt/binance-bot && venv/bin/python -c "import src.main; print(src.main.__file__)"`
  returned `/opt/binance-bot/src/main.py`.
- 2026-05-25 `/opt/binance-bot/scripts/status.sh`: service active, PID `121585`,
  runtime DB `/opt/binance-bot/data/live.db`, log `/opt/binance-bot/logs/bot.log`.
- 2026-05-25 final `.venv/bin/python -m pytest -q`: `142 passed`
- 2026-05-25 final `.venv/bin/python test_pipeline.py`: passed
- 2026-05-25 final `bash -n scripts/setup_deploy.sh && bash -n scripts/status.sh && scripts/smoke.sh`: passed
- 2026-05-25 final `git diff --check`: passed

## Remaining Handoff Tasks

- [x] Doc cleanup: complete stale roadmap checkbox state, replace stale first
  batch guidance, and add this active progress tracker.
- [x] Review/commit hygiene: inspect the modified and untracked files, confirm
  they belong in the final change set, and create a clean commit strategy.
- [x] Deployment: copy this working tree to `/opt/binance-bot`, reinstall the
  package in `/opt/binance-bot/venv`, and start `binance-bot.service` after
  privileged access is available.
- [x] Live-readiness: continue post-deploy monitoring. This rollout was started
  directly in live mode on request; no separate testnet/dry-run soak was run
  after the final deploy fixes.

## Review/Commit Hygiene

- `git diff --check`: passed.
- Conflict marker scan: passed.
- Secret-name scan found only placeholders, config field names, and test values
  in tracked/untracked work; real `.env` files remain ignored and were only
  inspected in redacted form.
- Ignored runtime artifacts confirmed: `.env`, logs, SQLite databases, and
  `__pycache__` files are not part of the dry-run add set.
- Shell scripts are executable: `panic.sh`, `setup_deploy.sh`, `smoke.sh`, and
  `status.sh`.

Recommended commit strategy:

1. Commit the completed implementation as one cohesive roadmap commit including
   source, tests, config examples, deployment scripts, and ignore rules.
2. Commit `CODEX.md`, `IMPLEMENTATION_PLAN.md`, and `PROGRESS.md` as a separate
   handoff/documentation commit if a cleaner review split is desired.

## Known Risks

- Live exchange behavior is the main remaining risk; local tests cover runtime
  wiring, sizing, risk rejection, persistence, and alert paths, but they do not
  prove Binance account permissions, symbol metadata, network behavior, or
  deployment environment correctness.
- `/opt/binance-bot` was running `binance-bot.service` in real-money mode before
  it was stopped. Its redacted environment showed `USE_TESTNET=false`,
  `DRY_RUN=false`, and `CONFIRM_LIVE_TRADING=yes`.
- The deployed service appears to be a different or older application shape than
  this working tree, based on log paths such as `/opt/binance-bot/src/orchestrator.py`.
- The service is currently active in live mode from `/opt/binance-bot`, using the
  existing `/opt/binance-bot/.env` values: `USE_TESTNET=false`, `DRY_RUN=false`,
  and `CONFIRM_LIVE_TRADING=yes`.
- The active import path is the new `/opt/binance-bot/src/main.py`; stale
  deploy-only directories from the previous app shape were removed after start.

## Next Recommended Action

Monitor the live service with `systemctl status binance-bot.service`,
`journalctl -u binance-bot.service -f`, and `/opt/binance-bot/logs/bot.log`.
The remaining repository task is to keep the committed tree clean for future
sessions.
