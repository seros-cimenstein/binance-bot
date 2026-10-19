# Loop ledger: follow-ups from the 2026-10-19 Bot Spot health check (2026-10-19)

STATUS: OPEN

Source: the 2026-10-19 health check of the Pi trader (`PROGRESS.md`, entry "2026-10-19 (12:50 UTC)"). The setup agent re-checked every finding against the code (`crates/bb-trader`, `crates/bb-omp`, `collectors/x-camoufox`, `IMPLEMENTATION_PLAN.md`) and the Pi (`ssh pi-wired`: container state, `docker-data` audit/session/watch files, logs, `docker system df`), downgraded the findings that do not hold (see "Checked, no item" at the bottom) and added its own (B1, C1, E1). This file is the loop's memory and its stop switch: the gate `~/projects/seref_omp_tools/seref_loop/bin/seref-loop-gate LOOP_BOT_HEALTH.md`
(run by `/seref_manager run LOOP_BOT_HEALTH.md` or `/loop --until`) stops the loop only when the line
above reads `STATUS: DONE`, no `- [ ]` is left, every `- [x]` carries `DONE:`/`INVALID:` evidence and
every `- [!]` a `BLOCKED:` reason.

Background (measured 2026-10-19 12:50 UTC):
- `bot-spot` and `scheduler` Up 23 h on the Phase 47 image (deployed 2026-10-18 14:02:04 UTC), `RestartCount=0`, no stop file, no failed-route halt. 63 cycles since the deploy (~21 min apart), all `openai-codex/gpt-6.1-sol`, all turns completed, no fallback.
- 3 round trips since the deploy (SOL +0.131, SOL -0.013, WLD -0.097 USDT after BNB fees), net +0.021 USDT, all sold by the bot. In cash since 09:10 UTC (15.15 USDT, wallet ~17.83 USDT).
- Live money: every Pi change below runs next to a live trader with full-wallet scope. `CODEX.md` "Live-Money Safety Rules" are binding.

## Protocol (every iteration)

1. Read this file and `CODEX.md` (binding live-money rules), then `PROGRESS.md`'s newest entries. Take the first `- [ ]` item in order (sections A, B, C, D, E in order; respect the dependencies named in items).
2. Before editing, read the code the item names and re-check that the finding still holds. Already
   fixed or wrong: mark `- [x] … — INVALID: <evidence>`.
3. Fix it at the root with the project's existing patterns; migrate every caller; update the docs
   that describe the changed behavior in the same step (README.md, OPERATIONS.md, CODEX.md when a rule's wording changes, `.env.example` for new or removed variables, `collectors/x-camoufox/README.md` for the collector).
4. Verify on the laptop in the repo root:
   - `cargo test -p <crate>` for each crate you changed: `bb-omp` (omp RPC types), `bb-trader` (trader), `cryptobot-gui` (dashboard).
   - Collector changes: `cd collectors/x-camoufox && uv run pytest`.
   - Then `cargo test --workspace` (last recorded run: 1005 passed), `cargo clippy --workspace --all-targets -- -D warnings`, `scripts/smoke.sh` and `git diff --check`.
   - Add a regression test when the bug can be shown in one. Tests must catch behavior, not wording.

   Smoke the changed path where it really runs:
   - (a) **Trader config/startup changes:** an isolated evidence-only run on the Pi, which skips the LLM, places no orders and never touches the live session, audit or stop file. On `ssh pi-wired`:

     ```
     cd ~/binance-bot && docker compose run --rm -e BOT_SPOT_SESSION_DIR=data/loop-smoke/session -e BOT_SPOT_AUDIT_PATH=data/loop-smoke/decisions.jsonl -e BOT_SPOT_JOURNAL_PATH=data/loop-smoke/journal.jsonl -e BOT_SPOT_STATE_PATH=data/loop-smoke/state.json -e BOT_SPOT_LOCK_PATH=data/loop-smoke/lock -e BOT_SPOT_STOP_PATH=data/loop-smoke/stop -e BOT_SPOT_EXIT_PLANS_LIVE=false -e BOT_SPOT_EXIT_PLANS_PATH=data/loop-smoke/plans.json -e BOT_SPOT_EXIT_WATCH_STATE_PATH=data/loop-smoke/watch-state.json -e BOT_SPOT_EXIT_WATCH_LOG_PATH=data/loop-smoke/watch.jsonl -e TELEGRAM_BOT_TOKEN= bot-spot bb bot-spot --evidence-only
     ```

     Use `--plan-only` instead when the LLM turn itself must run. Read the output, then `rm -rf docker-data/loop-smoke`.
   - (b) **Compaction/agent changes:** they only show on the live daemon. After the deploy, wait for the next `compacting trader context` / `compacted trader context` pair in `docker logs bot-spot` (one every ~1.5-2 h at the 150K threshold) and compare it with the last `"type":"compaction"` line of `docker-data/bot-spot-session/*.jsonl` (read with `grep`/`tail` only).
   - (c) **Collector:** build and launch on the Pi as the item says; never against x.com without operator approval.
5. Deploy to the Pi. Host `pi-wired`, dir `~/binance-bot`, compose service `bot-spot`, env `~/binance-bot/.env.docker`, data `~/binance-bot/docker-data`. The Pi clock shows UTC+3; every time in this file is UTC.
   - **Order:**
     1. Commit.
     2. `ssh pi-wired 'docker tag binance-bot:latest binance-bot:pre-<item>-<YYYYMMDD>'`.
     3. `git ls-files -z | grep -zv '^\.env' | rsync -a --from0 --files-from=- ./ pi-wired:binance-bot/`, and `ssh … rm` any file the commit deleted or renamed.
     4. `ssh pi-wired 'cd binance-bot && docker compose build bot-spot'` (10-17 min).
     5. `cp -p .env.docker .env.docker.bak-<UTC>-<label>` before any env edit.
     6. Recreate with `docker compose up -d --force-recreate bot-spot`.
   - **Verify:**
     - the container env (`docker exec bot-spot env | grep -c '^BOT_SPOT_'`, no secrets printed);
     - the log lines `keeping the cycle cadence`, `live exit plans enabled` and (when cycles run) `omp trader session ready … resumed=true`, and no ` WARN ` line at startup after B1;
     - the next audit row's `prompt_version` (`bot-spot-v13-agentic`) and `run_config`.
   - **Rollback:** `docker tag binance-bot:pre-<item>-<YYYYMMDD> binance-bot:latest`, restore the env backup, then recreate.
   - **When it is safe:** crypto trades 24/7, so the safe windows are between cycles.
     - Recreate only right after a cycle row has landed in `docker-data/bot-spot-decisions.jsonl`, and at least 4 min before the next one (compaction starts about 2-3 min before a cycle).
     - Never while a route is in flight (a `route` row without its `cycle_summary` yet).
     - Prefer a moment with no open position; with one open, the exit guard restarts with the daemon (fine), but never create the stop file then, because it also blocks exit-plan sells (`crates/bb-trader/src/cycle.rs:674-679` in `exit_sell`).
     - Never reboot the Pi or change its kernel/boot config without explicit operator approval: it stops the live trader.
   - **Other services:** `scheduler` runs `bb x-insighter-snapshotter --once` every minute, SQLite backups at 23:15/23:30 UTC and `scripts/backup_bot_data.sh` at 23:45 UTC. Do not recreate `scheduler`, and do not run heavy builds or prunes, between 23:10 and 23:55 UTC.
   - **Backups:** before any change to files in `docker-data` (renames, key rewrites, state edits), copy them to `docker-data/backup-<UTC>-<label>/`. Env edits use the `.env.docker.bak-*` copy above.
6. Edit this file: `- [ ]` -> `- [x]` and append ` — DONE: <files>; <test/smoke evidence>`.
   One item per iteration is fine; finish an item completely or leave it `- [ ]`.
7. Blocked only for a user decision or an unreachable host/credential. For a decision, call the `ask`
   tool; nobody may be at the keyboard, so under `/seref_manager` the loop manager answers it or
   tells you to defer. Never end a turn waiting for a reply. Mark a blocker
   `- [!] … — BLOCKED: <exact missing thing, what was tried>`. Difficulty, size or scope pressure
   is not a blocker.
   A `— MANAGER(itN): …` note is the loop manager's review: an item it reopened stays open until
   you fix what the note names and tick it again with new evidence.
8. When no `- [ ]` is left:
   - Run every test suite and the final check: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `scripts/smoke.sh`, `scripts/pool_replay.sh`, `(cd collectors/x-camoufox && uv run pytest)`, `git diff --check`.
   - Then on the Pi: `docker compose ps` shows `bot-spot` and `scheduler` Up, `docker inspect -f '{{.RestartCount}}' bot-spot` is 0, the latest audit row is newer than the last deploy, `ls docker-data/bot-spot.stop` fails (no stop file), and no failed-route halt is set.
   - Add a dated `PROGRESS.md` entry with the loop summary and tick the finished `IMPLEMENTATION_PLAN.md` items.
   - Replace the STATUS line with `STATUS: DONE`, append a summary (done / invalid / blocked) at the bottom, then run the gate command above. It must exit 0; if it does not, fix what its message names before yielding.

Rules:
- Commit policy: at setup the only uncommitted change was this session's own `PROGRESS.md` health-check entry (no user work in the tree). Commit it together with this ledger in the first item's commit, then one commit per item (`loop(<ID>): …`). If `git status` ever shows changes you did not make, stop committing and leave them alone.
- Paths not to touch: `docker-data/bot-spot-session/` (read-only), `docker-data/bot-spot-journal.jsonl` and `docker-data/bot-spot-decisions.jsonl` (the trader appends; you only read), `docker-data/bot-spot-agent-state.json`, `docker-data/bot-spot-exit-plans.json`, `docker-data/bot-spot-exit-watch*`, `docker-data/archive/`, `docker-data/omp-agent/` (omp logins), the burner X profile `~/.local/share/x-collector-live/` on the laptop and `docker-data/x-camoufox-profile` on the Pi, the laptop's legacy `.env` and `~/.local/share/binance-bot`, `LOOP_MONEY.md`. Never touch `docker-data/bot-spot.stop` or `--mark-failed-route-reviewed`.
- No live-trading behavior change: no new gate, cap, stop-loss/take-profit, prompt text or exit-plan default (CODEX.md rules); any of those needs a new operator decision.
- No destructive commands on live data without a backup.

## A. Deadline 2026-10-22: Phase 45 live exit-plan review

- [x] A1 The Phase 45 "review after about a week" of live plan sells vs the bot's own exits is still open (`IMPLEMENTATION_PLAN.md:995`; live exit plans since 2026-10-15 12:15 UTC; 9 audit rows with `cycle_id` `exit-plan`, all submitted: 2 on 10-15, 5 on 10-16, 2 on 10-17, none since; `docker-data/bot-spot-exit-watch-state.json` `closed_trials` 33, `plan_vs_bot_usdt` -0.039677; the default plan changed 10-16, 10-17 (`deadline=120`) and 10-18 (`atr=3.5:1h,deadline=360`); confidence high). Chosen default: run the review now with all data up to the run time instead of waiting for 10-22, because no single default plan has run a full week; split the result per default-plan period. Read the audit, `bot-spot-exit-watch.jsonl` and the watch state read-only (from the Pi, or the `scripts/cryptobot_gui_pi.sh` mirror at `~/.cache/binance-bot/pi-data`); use `bb bot-spot-exit-watch --summary` only if it runs without starting a watcher against the live files. Per period: plan sells (count, net USDT after fees, mean hold) vs the bot's own sells, and `plan_vs_bot_usdt`. Record it in `PROGRESS.md`; no config change (a new default plan needs an operator decision). Acceptance: a dated `PROGRESS.md` entry with the per-period table and the commands that produced it, and `IMPLEMENTATION_PLAN.md:995` ticked with a pointer to that entry. — DONE: `PROGRESS.md` entry "2026-10-19 (13:10 UTC)" (per-period table P1-P5 + commands); `IMPLEMENTATION_PLAN.md:995` ticked with the pointer. Evidence: read-only script over the Pi audit found 33 closed trips since 2026-10-15 09:17:34 UTC (= watch state `closed_trials` 33, none open): 9 plan sells +0.3943 USDT (6 wins, mean hold 54 min; 4 default `stretch` sells ≈ flat, +0.019; 5 bot-set plans +0.375), 24 bot sells -0.2621 (7 wins, 65 min); no plan sell since 10-17 18:53 (P4 bot set `deadline=90`, P5 `deadline=480` on every trip). `docker exec bot-spot bb bot-spot-exit-watch --summary` printed 33 trials, total -0.0397, without starting a watcher (`exit_watch.rs:706-709`). No config change. New finding from it: A2/A3.

- [ ] A2 In live mode the watcher's "plan vs bot" figure is not a plan-vs-bot comparison but trigger price vs actual fill on plan-sold trips, and the dashboard and `--summary` label it as the comparison (`crates/bb-trader/src/exit_watch.rs:344-369`: `close` takes the trip's real exit as "the bot alone"; for a plan-sold trip that exit is the plan's own sell, so `diff` = trigger price minus modelled slippage vs fill; Pi `--summary` 2026-10-19: the 9 non-zero rows of 33 are exactly the 9 plan-sold trips, total -0.0397; `crates/cryptobot-gui/src/index.html:1347` prints "plan vs bot" on every `closed` event, `exit_watch.rs:671` heads the table "Exit-plan paper trial"; confidence high). Observability only (dashboard is read-only, `--summary` is a read-only CLI), no trader change. Fix: when a trial closes on a plan sell in live mode (its plan fills cover the exit), record the comparison as a fill-slippage field instead (e.g. `plan_fill_vs_trigger_usdt`) and leave `plan_vs_bot_usdt` 0 / absent for it; `--summary` and the dashboard Exits tab say "live: trigger vs fill" for those rows and drop the "paper trial" title in live mode; keep paper-mode behavior unchanged. Do not rewrite existing watch log/state on the Pi. Test: a live-mode trial closed by its own plan sell does not add to `plan_vs_bot_usdt`; a paper trial still does. Acceptance: `cargo test -p bb-trader -p cryptobot-gui` passes with that test; dashboard checked in Chromium on the `scripts/cryptobot_gui_pi.sh` mirror (Exits tab, no page errors); deployed with the next trader deploy (B2) or on its own per Protocol 5.

- [ ] A3 The live cycle prompt tells the bot "plan vs your own exits: -0.0397 USDT", which is the A2 slippage figure, not a comparison (`crates/bb-trader/src/exit_book.rs:487-493`, printed in live and paper mode alike; confidence high). This changes information in the live trader's cycle prompt, which this ledger's rules reserve for an operator decision. Do A2 first. Call `ask`: (1) in live mode, replace the line with the real numbers of plan-sold positions ("positions sold by a plan: N, net X USDT after fees") (default), (2) drop the line in live mode, (3) leave it. If the answer is deferred or no one decides, mark `- [!] A3 … — BLOCKED: prompt wording change needs the operator's choice`. On approval: change it, prompt tag `bot-spot-v14-agentic` (`BOT_SPOT_PROMPT_VERSION` in both `.env.docker`, backup first), test the live-mode line, deploy per Protocol 5. Acceptance: chosen option recorded; if changed, `cargo test -p bb-trader` passes and the first audit row after the deploy has `prompt_version` `bot-spot-v14-agentic`; a `--plan-only` smoke (Protocol 4(a)) prints the new line.

## B. Trader config and observability (one deploy)

- [ ] B1 Ten removed guardrail variables are still set in the Pi's env file, so every start logs a WARN and the file looks like it has live limits that do nothing (`crates/bb-trader/src/config.rs:38-54` `REMOVED_VARS`, `:235-241` warning; Pi log 2026-10-18T14:02:04Z `WARN ignoring removed Bot Spot guardrail settings … vars=BOT_SPOT_ALLOW_NEGATIVE_FRICTION_BPS,BOT_SPOT_REQUIRE_MARKET_EVIDENCE_FOR_LIVE,BOT_SPOT_MARKET_EVIDENCE_MIN_EDGE_BPS,BOT_SPOT_SPECULATIVE_MODE_ENABLED,BOT_SPOT_SPECULATIVE_MIN_EDGE_BPS,BOT_SPOT_SPECULATIVE_REQUIRE_THESIS,BOT_SPOT_MAX_RECENT_LOSS_STREAK,BOT_SPOT_MIN_TRADE_INTERVAL_SECONDS,BOT_SPOT_MAX_SUBMITTED_TRADES_PER_DAY,BOT_SPOT_HALT_AFTER_FAILED_SUBMISSION`; each appears once in the Pi's `~/binance-bot/.env.docker` and in the laptop's untracked `.env.docker`; `OPERATIONS.md:393-396` already documents them as removed; the failed-route halt stays always on in code, config.rs:52; confidence high). Fix: back up both env files (`.env.docker.bak-<UTC>-pre-b1`), delete those 10 lines on the Pi and the laptop; no code change, no recreate in this item (B2's recreate applies it). Acceptance: `grep -cE '^BOT_SPOT_(ALLOW_NEGATIVE_FRICTION_BPS|REQUIRE_MARKET_EVIDENCE_FOR_LIVE|MARKET_EVIDENCE_MIN_EDGE_BPS|SPECULATIVE_MODE_ENABLED|SPECULATIVE_MIN_EDGE_BPS|SPECULATIVE_REQUIRE_THESIS|MAX_RECENT_LOSS_STREAK|MIN_TRADE_INTERVAL_SECONDS|MAX_SUBMITTED_TRADES_PER_DAY|HALT_AFTER_FAILED_SUBMISSION)=' .env.docker` prints 0 on both machines, and the Protocol 4(a) evidence-only run on the Pi prints no `ignoring removed Bot Spot guardrail settings` line.

- [ ] B2 The compaction log reports the wrong context size after compaction, which reads as the bot losing its memory every 1.5-2 h (`crates/bb-trader/src/agent.rs:960-978` takes `tokens_after` from `get_state`'s local estimate; `crates/bb-omp/src/types.rs:336-346` `CompactResult` keeps only `summary` and `tokens_before`; Pi: all 13 compactions since the deploy, 2026-10-18 15:29 → 2026-10-19 11:43 UTC, log `tokens_after=Some(3549) summary_chars=111`, while the session file's `compaction` entries record `method` `remote` and e.g. `tokensAfter` 97323 at 11:43; the 111 chars are omp's placeholder "Remote compaction preserved provider-native history…", and the bot cites cycles 749/757 afterwards, so memory is intact; confidence high on the symptom, medium that omp's RPC reply carries `tokensAfter`: the `omp` binary's strings contain `tokensAfter` in its compaction result). Do B1 first so one recreate applies both. Fix: add `tokens_after` and `method` to `CompactResult`; log and store omp's `tokens_after` and `method`, and log the `get_state` figure separately as `local_tokens_after`. If omp sends no `tokensAfter`, `tokens_after` stays `None` (never filled from the local estimate); do not parse the session file. Update the test at `agent.rs:1715-1737` (its fake `compact` reply gets `tokensAfter`). Deploy per Protocol 5. Acceptance: `cargo test -p bb-omp -p bb-trader` passes, including a test that the logged/stored after-size is omp's `tokensAfter`; on the Pi, the first `compacted trader context` line after the deploy shows `tokens_after` equal to the `tokensAfter` of the session file's newest `compaction` entry, plus `method=remote`; the startup log has no ` WARN ` line (B1); the next audit row is `bot-spot-v13-agentic`.

## C. Pi housekeeping

- [ ] C1 Docker build cache fills the Pi's disk (Pi `docker system df`: build cache 37.37 GB, 35.33 GB reclaimable, 238 entries; images 38.44 GB, 21 `binance-bot` image IDs under 22 tags, mostly `pre-*` rollback tags from 10-17/10-18; `df -h /` 80% used, 177G of 235G, 46G free; every deploy build adds cache; confidence high). Do B2 first (its build should reuse the cache). Chosen default: `ssh pi-wired 'docker builder prune -f --filter until=72h'` (keeps the last 3 days' cache so the next build stays incremental); keep every image tag (they are the rollback points); outside 23:10-23:55 UTC. Acceptance: `docker system df` and `df -h /` recorded before and after, with the build cache smaller; `bot-spot` and `scheduler` still Up with `RestartCount=0`.

## D. Phase 43 collector on the Pi

- [ ] D1 The X collector image has never been built or launched on the Pi, so the Phase 43 pipeline has no input there (`IMPLEMENTATION_PLAN.md:791-797` "Open: arm64 build and run on the Pi 5"; Pi `docker images` has no `binance-bot-x-camoufox`, no `x-posts.jsonl` in `docker-data`; `scheduler` ran `bb x-insighter-snapshotter --once` 2728 times since 10-18 14:00 UTC, each "found no due snapshots"; Pi is `aarch64`, kernel `6.12.75+rpt-rpi-2712`, `getconf PAGESIZE` 16384, and `collectors/x-camoufox/README.md:117-124` warns Firefox can crash under 16K pages; confidence high). Chosen default: build and launch-test only; do not start the `x-camoufox` service, do not log in, do not copy the burner profile, do not open x.com. Steps: rsync per Protocol 5 step 3, `ssh pi-wired 'cd binance-bot && docker compose --profile x-collector build x-camoufox'` (browser download ~1.3 GB; not 23:10-23:55 UTC), then a one-off `docker compose --profile x-collector run --rm --no-deps x-camoufox python -c '…'` that launches Camoufox headless, opens `about:blank`, prints the browser version and closes. If the browser crashes under the 16K-page kernel, do not touch `/boot/firmware/config.txt`: mark `- [!] D1 … — BLOCKED: kernel8.img (4K pages) needs operator approval and a Pi reboot, which stops the live trader; <crash output>`. Acceptance: `docker images binance-bot-x-camoufox` lists the image on the Pi, the launch test prints a version and exits 0, `docker ps` shows no `x-camoufox` container afterwards, and `IMPLEMENTATION_PLAN.md:791-797` records the Pi result (ticked only when build and launch both pass).

## E. Roadmap pointers

- [ ] E1 The repo's pointers name stale work (`AGENTS.md:6-7` "Active work: Phase 43 (X pre-pump signal insighter)" while the last sessions were LOOP_MONEY and Phase 47, all of whose items are ticked at `IMPLEMENTATION_PLAN.md:1129-1188`; `IMPLEMENTATION_PLAN.md:975` "Run it for about a week next to the live trader" (Phase 45 paper watcher) is still `- [ ]` although step 3 at `:986` went "live now, without the paper week" and the paper watcher refuses to start while live exit plans run (`CODEX.md:133`); confidence high). Do A1 and D1 first (their results decide the wording). Fix: rewrite the AGENTS.md active-work line to the real state (Phase 47 deployed and monitored; Phase 43 status from D1; Phase 45 review from A1); mark `:975` as superseded, e.g. `- [x] … (superseded 2026-10-15 by step 3: live without the paper week)`. Acceptance: `grep -n 'Active work' AGENTS.md` shows the new line; `IMPLEMENTATION_PLAN.md:975` is no longer `- [ ]`; no other line changed meaning.

## Checked, no item

These health-check observations were re-checked and do not need a fix:
- Daily LLM cost $5.63 by 12:26 UTC with no cap (`BOT_SPOT_MAX_DAILY_LLM_COST_USD` unset = 0 = unlimited, `crates/bb-trader/src/config.rs:183-184,322-323`, `cycle.rs:946`). It is omp's notional `usage.cost.total` on subscription logins (`crates/bb-omp/src/types.rs:143-160`; `LOOP_MONEY.md:15`), not money spent. A cap would stop the trader asking the model, which is a trading change and needs an operator decision.
- Squashed decision text ("Held15.15473857USDT;verified …"): gpt-6.1-sol writes it like that in its own `end_cycle` arguments (session file); no code collapses whitespace.
- Session file 82.6 MB / 10,345 lines after 6 days (~14 MB/day): small next to 46 GB free, and the nightly `backup_bot_data.sh` archive covers it.
- Trades use about half the cash (7.85-7.93 of ~15.1 USDT): the bot's own sizing; Bot Spot has no trading guardrails by operator decision (`CODEX.md:98-110`).
- Cadence 21.3 min against the 20-min interval: compaction (2.5-3 min, every 4-5 cycles) and turn time; works as designed.
