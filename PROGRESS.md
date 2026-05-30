# Binance Bot Progress

Use this file for cross-session handoff notes. Keep `IMPLEMENTATION_PLAN.md` as
the durable roadmap and use this file for the current working state.

## Current Status

- 2026-05-30: Phase 43 calibration landed. Replaced the "hyper-aggressive"
  mission and "holding is for cowards" framing with a cost-aware mission,
  removed the `override_bps_limits` decision field and its bypass of the
  market-evidence gate, tightened `allow_negative_friction_bps` default
  300 → 40 bps, and switched `market_pick_mode` default to
  `liquid_top_value`. Added a new `CLAUDE_SPOT_PROVEN_EDGE=yes` preflight
  gate that keeps live submission paper-only until a positive backtest
  result is in hand. Prompt version v2-aggressive → v3-calibrated for audit
  trail clarity.
- 2026-05-30: Added `src/claude_spot_backtest.py` — a deterministic CCXT
  historical replay harness that models taker fees, half-spread, and
  slippage, then routes capital only when expected edge clears the cost
  bar. Outputs verdict (`edge_likely` / `edge_marginal` / `no_edge` /
  `no_trades` / `no_data`), Sharpe, drawdown, fee drag, win rate, and
  per-asset holding time. Smoke-tested against last 10 days of BTC/ETH 1h
  klines: verdict `no_edge`, -5.78% total return, Sharpe -3.0. Confirms
  the existing signal does **not** clear its cost bar; live gate
  correctly stays closed.
- 2026-05-30: Added `RulesEngineAdvisor` (deterministic edge-bar decider)
  and `RulesFirstAdvisor` (rules propose, LLM can veto/approve only,
  cannot pick a different route). Selectable via
  `CLAUDE_SPOT_DECISION_MODE` ∈ `{rules_only, llm_only, rules_first}`,
  default `rules_only`. Discretionary LLM behavior is no longer on the
  default live path.
- 2026-05-30: `binance-claude-spot.timer` stopped while the calibration
  patches were verified. Full suite: 367 passed. The timer remains
  stopped pending an explicit operator decision; even if restarted, the
  `CLAUDE_SPOT_PROVEN_EDGE` gate keeps live submission blocked until a
  positive backtest exists.
- 2026-05-30: Phase 44 — funding-rate harvest research and runtime
  scaffolding. Added `src/funding_arb_backtest.py` (delta-neutral
  replay with realistic fees/spread/slippage and universe selection) and
  `src/funding_arb.py` (live runtime: settings, persistent positions,
  deterministic open/close/hold advisor, plan-only and paper modes).
  Backtest result: top 60 USDT perps (2025-12-01 to 2026-05-30, 1h):
  +5.84% over 6 months at 5 bps/leg slippage, Sharpe 5.4, 59% win rate,
  max drawdown 0.39%, verdict `edge_likely`. Majors-only verdict:
  `no_edge` (-9.1%). Edge lives in alt-perp funding. Layer C (live
  execution: dual-leg orders, collateral transfer, rollback,
  reconciliation) deliberately not yet implemented; runtime currently
  raises on `--submit-live` after the new `FUNDING_ARB_PROVEN_EDGE`
  preflight gate. Smoke-tested `--paper` cycle live: 5 positions opened
  at 0.5 bps entry threshold, funding accrual correct on second cycle.
  Full suite: 398 passed.
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
- 2026-05-25: Reconciled handoff documentation after deployment: all existing
  roadmap work is complete, with future operational engineering to be recorded
  as new scope before implementation.
- 2026-05-25: Phase 12 live operations hardening was approved as new scope:
  online SQLite backups with daily retention, operator verification, health
  alerts, and a controlled live WebSocket-mode cutover after verification.
- 2026-05-25: Implemented the repository portion of phase 12: `OPERATIONS.md`,
  online backup/guarded restore scripts, daily timer units, read-only
  `src.ops_check`, and throttled terminal/WebSocket Telegram alert paths.
- 2026-05-25: The initial live WebSocket cutover exposed Binance retirement of
  REST `listenKey` user streams (`HTTP 410 Gone`). Public candle WebSocket
  remained active; the candidate fix now uses
  `userDataStream.subscribe.signature` and passed a read-only live subscription
  probe.
- 2026-05-25: User-owned deployment/migration tooling was added to eliminate
  recurring sudo use after one final stop/disable of the existing system
  service.
- 2026-05-25: Staged the patched user-owned runtime under
  `~/.local/share/binance-bot` without activating it. Its read-only exchange
  check and signed private WebSocket subscription probe passed while the old
  system service remained the only active trading process.
- 2026-05-25: Completed the user-service handoff. The legacy system
  `binance-bot.service` and backup timer are inactive/disabled; the user-owned
  service is active from `~/.local/share/binance-bot` with PID `137762`,
  `NRestarts=0`, and its daily backup timer enabled.
- 2026-05-25: Post-handoff live validation completed: public kline and signed
  private user-data WebSockets connected, two closed BTC/USDT candles were
  processed, the read-only exchange plus Telegram test passed, and both live
  database and newest backup returned SQLite `quick_check=ok`.
- 2026-05-25: Hardened the sudo-free user deployment path after migration:
  `scripts/setup_user_deploy.sh` now preserves an existing user-owned live
  database instead of reimporting stale `/opt` state, and the one-time handoff
  script refuses to mutate an already active user runtime. The corrected helper
  was synced into the deployed user directory without restarting trading.
- 2026-05-26: A balance/sizing audit found the live SMA runtime still configured
  with fixed `ORDER_SIZE=0.001` for `BTC/USDT`, while the read-only account
  balance contained only `0.0169745 BNB` and no spendable BTC/USDT balance.
  The fixed live user service was stopped and disabled before new development.
- 2026-05-26: Phase 13 was implemented as a separate paper-only modular asset
  pool rather than a BTC-specific sizing patch. It catalogs Binance spot
  markets, gates eligible routes, models up to three conversion legs with
  dynamic authenticated commissions and exchange rules, and values virtual
  portfolios against `USDT`, `BTC`, `PAXG`, and `XAUT`.
- 2026-05-26: The paper runtime was deployed as
  `binance-pool-paper.service`; its backup timer retains 35 daily verified
  snapshots. The legacy `binance-bot.service` remains inactive/disabled and
  now refuses fixed-size real-money startup in code.
- 2026-05-26: Deployment validation found and corrected a paper warm-up bug
  where `POOL_MOMENTUM_OBSERVATIONS` was treated only as a history limit. The
  pre-fix virtual ledger, which contained one simulated `BNB -> EDEN`
  reallocation and no real order, was archived; a clean ledger was started
  with the corrected default of 96 15-minute observations before paper
  reallocation.
- 2026-05-26: Removed the temporary 100-asset scoring default so final paper
  operation evaluates every asset represented by eligible markets. The
  prior capped validation ledger was archived and a new final evidence ledger
  was seeded from the unchanged BNB holding.
- 2026-05-26: Bounded paper-ledger growth before beginning evidence collection:
  it now retains the latest complete market catalog, per-cycle eligibility
  summaries, and the rolling scorer observation window while keeping
  historical portfolio snapshots and decisions. The pre-change validation
  ledger was archived and final accumulation restarted cleanly.
- 2026-05-26: Added phase 14 fast replay validation. The new
  `src.pool.replay` command drives the existing paper runtime with deterministic
  fixture markets, isolated replay ledgers, dynamic fee modeling, minimum-order
  gates, and accelerated scorer warm-up/reallocation checks.
- 2026-05-26: Extended phase 14 with `scripts/pool_replay.sh`, which runs the
  default replay fixture set into isolated `/tmp` databases. Added a stress
  fixture that verifies wide-spread and low-liquidity markets stay rejected.
- 2026-05-26: Added phase 15 manual one-shot live probe. The probe is a
  foreground command only, requires explicit real-order confirmation, selects
  one direct USDT sell from free balances, writes a JSONL audit record, and
  sends a Telegram message labeled `LIVE PROBE`.
- 2026-05-26: Replaced the hard month-long paper gate in the roadmap with a faster
  replay, live-market paper warm-up, permission-diagnostic, and tiny live-canary
  gate. Added future roadmap phases for permission-aware spot, margin, futures,
  transfers, unified asset-pool valuation, and advanced Binance product modules.
- 2026-05-26: Implemented phase 16 API permission diagnostics. The new
  `src.api_permissions` CLI reads account capability, safely probes spot trading
  through Binance `order/test`, maps current and future bot features to required
  capabilities, policy-blocks withdrawals/future modules, and gives explicit
  `-2015` permission/IP guidance. The manual live probe now runs the safe spot
  order-test guard before any real order submission.
- 2026-05-26: Implemented phase 17 spot live canary as a separate
  foreground-only module. The first canary version supports one direct source
  asset sell into `USDT`, requires replay/warm-up/operator confirmations before
  submission, uses dynamic balance/market/fee/spread/minimum data, enforces
  hard order/daily/emergency-stop limits, audits plan/submission/reconciliation
  state, sends Telegram canary alerts, and does not modify `src.pool`.
- 2026-05-26: Implemented phase 18 guarded live spot allocator as a separate
  foreground-only module. The first allocator version supports one direct spot
  market order per invocation, requires canary/replay/warm-up/permission
  confirmations before submission, enforces exposure/turnover/drawdown/daily
  loss/order-count/reserve/route-risk limits, records benchmark valuation and
  reconciliation, sends Telegram alerts, and is not a background service.
- 2026-05-26: Implemented phase 19 dry-run margin modeling. The new
  `src.margin` module discovers cross/isolated margin accounts with read-only
  endpoints, models borrow limits, debt, interest, repayment, margin level, and
  liquidation buffer, and keeps live margin borrow/repay/order/transfer writes
  unavailable.
- 2026-05-26: Implemented phase 20 dry-run USD-M futures modeling. The new
  `src.futures` module discovers USD-M account, position, market-rule, funding,
  symbol-config, and leverage-bracket state with read-only endpoints, models
  leverage, margin mode, funding, liquidation distance, reduce-only behavior,
  precision/minimums, and reconciliation, and keeps live futures write endpoints
  unavailable.
- 2026-05-26: Implemented phase 21 dry-run capital transfer planning. The new
  `src.transfers` module discovers wallet, spot, funding, margin, USD-M futures,
  and transfer-history state with read-only endpoints, models Binance universal
  transfer routes, wallet activation, source/destination balances, margin debt
  constraints, isolated-margin symbol requirements, resulting allocation state,
  and JSONL audit rows, while keeping live transfer submission unavailable.
- 2026-05-26: Implemented phase 22 unified asset-pool valuation. The new
  `src.pool.unified` module combines spot, funding, margin, futures, transfer
  availability, debt, interest, open PnL, collateral, reserves, leverage,
  liquidity, volatility, and benchmark prices into a local dry-run allocation
  report without adding any live order, margin, futures, or transfer write path.
- 2026-05-26: Executed the available phase 13 readiness-gate checks. The
  legacy live `binance-bot.service` is inactive/disabled, the paper-only
  `binance-pool-paper.service` is active/enabled, focused pool tests, full
  tests, replay, pipeline, shell smoke, permission diagnostics, and a spot
  canary plan all passed without submitting an order. The gate remains open
  because the live-market paper ledger still has only `18` of the required
  `96` catalog cycles and both scorers still hold with insufficient benchmark
  history.
- 2026-05-26: Implemented phase 23 advanced product-module framework. The new
  `src.product_modules` registry tracks dry-run modules, live margin/futures/
  transfer activation, withdrawals, prediction trading, and future Binance
  products with strategy-interface, permission, market-data, risk-model, audit,
  and activation-gate contracts. It is local metadata/gating only and does not
  enable live advanced-product writes.
- 2026-05-26: Added roadmap phases 24 through 30 for the remaining Binance API
  capabilities: margin live canary, USD-M futures live canary, universal
  transfer live canary, withdrawal safety, prediction dry run, symbol
  whitelist/API-key scope audit, and advanced-product readiness review. These
  are planned future scope only; live advanced-product automation remains
  disabled.
- 2026-05-26: Paper warm-up continued while roadmap planning was updated. The
  paper service is active and the ledger reached `21` of `96` required catalog
  cycles, with `42` paper decisions recorded.
- 2026-05-26: Implemented phase 27 withdrawal safety as local/dry-run planning:
  allowlisted destinations and networks, daily/request caps, approvals,
  cooldowns, emergency stop, operator identity, and JSONL audits are modeled,
  while live withdrawal activation fails closed.
- 2026-05-26: Implemented phase 28 prediction dry-run modeling from local or
  injected read-only market payloads with stake, settlement, liquidity,
  drawdown, availability, and emergency-stop controls. No prediction write
  endpoint is exposed.
- 2026-05-26: Implemented phase 29 local API scope auditing with
  `API_SYMBOL_WHITELIST`/`API_ASSET_WHITELIST`. The manual live probe, spot
  canary, and live spot allocator now reject an out-of-scope resolved market
  before any order can be submitted.
- 2026-05-26: Implemented plan-only Phase 24-26 canary slices. The new
  margin, USD-M futures, and universal-transfer foreground commands wrap the
  existing dry-run models with local scope, emergency-stop, risk, and JSONL
  audit gates while all submission flags fail closed.
- 2026-05-26: Implemented Phase 30 local advanced-product readiness review.
  It records focused/full-test, replay, dry-run, permission, canary-plan,
  audit-review, and approval evidence; rejects concurrent mutable canary
  proposals; and never enables live mutation or background automation.
- 2026-05-26: Added Phase 31 scope before implementation: protect local
  credential backup artifacts, expose human-readable paper decision evidence,
  and support isolated public-candle historical replay without altering the
  running Phase 13 ledger or authorizing live Spot trading.
- 2026-05-26: Implemented the code/documentation portion of Phase 31.
  `.env.*` credential backups are ignored; `src.pool.report` reads paper
  evidence without writes; `src.pool.history_replay` evaluates explicit
  direct-`USDT` assets from public historical candles in an isolated replay
  database. Phase 13 warm-up review and any small-account live decision remain
  open.
- 2026-05-26: Ran an isolated six-day public-candle research replay for
  `BNB/USDT`, `BTC/USDT`, `ETH/USDT`, `SOL/USDT`, `PAXG/USDT`, and
  `XAUT/USDT` seeded with `0.0169745 BNB`. It produced `1154` holds, including
  `742` routed-minimum/cost holds; the configured 10% reallocation is generally
  too small for a minimum valid conversion at this balance. Treat small-account
  minimum-notional feasibility as an explicit pre-live decision, not a reason
  to bypass risk caps.
- 2026-05-26: The paper ledger was re-read during Phase 24-30 work and reached
  `24` of `96` required catalog cycles with `48` paper decisions. Sandbox
  restrictions prevented re-checking the user-service bus in this pass; the
  ledger remains paper-only.
- 2026-05-26: The Phase 31 read-only report re-read the running paper ledger at
  `30/96` catalog cycles and `60` decisions. All current deployed-ledger
  decisions remain expected warm-up holds; no live order was introduced or
  executed by this work.
- 2026-05-26: Added and implemented Phase 32 for Claude autonomous Spot
  execution. `src.claude_spot` is a separate live runtime with a headless Claude
  adapter, schema-validated decisions, whole-Spot-wallet routed candidates,
  serial execution of up to three Spot legs, JSONL audit, emergency stop,
  reconciliation, and user-service timer files. This deliberately replaces the
  unfinished Phase 13 warm-up as the Claude-mode gate. The deployed timer was
  enabled after live Binance permission/order-test and Claude plan-only probes;
  the first live timer cycle completed as `held` and submitted no order because
  every supplied route had worse estimated value than holding BNB.
- 2026-05-26: Fixed Claude Spot small-market live sizing to parse Binance
  `LOT_SIZE`/`MARKET_LOT_SIZE` step sizes and use those steps for route
  simulation, order-test, and live order quantities. This corrected the
  `HBAR/BNB` `LOT_SIZE` rejection where `128.4` was offered against a whole-unit
  lot step. The deployed timer was re-enabled after validation; the first live
  timer cycle submitted and filled a one-leg `SUI/BNB` buy for `11.0 SUI`.
- 2026-05-27: After the laptop rebooted, `binance-claude-spot.timer` resumed and
  started a live cycle at `08:42:23 +03`. The cycle completed successfully at
  `08:43:58 +03`, selected `SUI->POL:59cee8bf25e5`, and filled both route legs:
  sold `11.0 SUI` on `SUI/JPY`, then bought `119.4 POL` on `POL/JPY`. The audit
  shows residual `5.604 JPY` after the second leg and the next timer due at
  `08:57:23 +03`.
- 2026-05-27: Paused `binance-claude-spot.timer` after the `10:46:23 +03`
  cycle failed on `POL/IDR` with Binance `-2010`, `This symbol is not permitted
  for this account.` The audit shows no route legs submitted for the failed
  cycle, and the live balance check showed the main position still at
  `113.2299 POL` plus residual dust in previous assets. This is a symbol-scope
  failure, not an API-key-reading failure, balance failure, or lot-size failure.
  Next implementation should add a Claude Spot local symbol allowlist/blocklist
  gate before re-enabling the timer.
- 2026-05-27: Added and deployed Claude Spot route-scope filtering. `IDR` was
  first locally blocked through `CLAUDE_SPOT_BLOCKED_QUOTES=IDR`; the guard was
  then widened to `CLAUDE_SPOT_BLOCK_FIAT_ASSETS=true` so traditional fiat
  assets such as `TRY`, `JPY`, `EUR`, `GBP`, `AUD`, `BRL`, and `IDR` are
  excluded from route generation and rechecked before plan/live submission.
  Stablecoins such as `USDT`, `USDC`, `FDUSD`, `DAI`, `BUSD`, and `USD1` remain
  allowed. A deployed no-order context validation returned `candidate_count=10`,
  `contains_fiat=False`, and offered only `FDUSD`/`USDT` route legs. The timer
  remains disabled until the operator explicitly re-enables live cycles.
- 2026-05-27: Re-enabled `binance-claude-spot.timer` and started one live
  cycle. The timer is active and scheduled the next run for
  `2026-05-27 11:45:16 +03`. The initial cycle completed successfully with
  status `submitted`, selected `POL->CRV:1ba1a17504f8`, and filled both
  crypto/stablecoin route legs: sold `113.2 POL` on `POL/USDT`, then bought
  `47.4 CRV` on `CRV/USDT`. Audit reconciliation showed remaining free
  balances from the route at `POL=0.0299`, `USDT=0.04433177`, and
  `CRV=47.3526`.
- 2026-05-27: The later `12:16:23 +03` Claude Spot timer cycle failed before
  any order submission because the headless Claude command timed out after
  `120` seconds. Audit row `27` had `status=failed`,
  `submission_attempted=False`, no selected route, and no submitted legs. The
  previous successful cycle had rotated `CRV -> AAVE`; current free balance
  check showed the main position at `0.120879 AAVE` plus dust/residual balances.
  Patched Claude Spot so decision timeouts now become clean `held` no-order
  audit rows, raised `CLAUDE_SPOT_TIMEOUT_SECONDS` to `240`, and added
  `OnActiveSec=15min` to the user timer so re-enabling it schedules the next
  run without forcing an immediate live trade. The deployed timer is active
  again; its first post-deploy run at `12:24:21 +03` cleanly held AAVE with
  `submission_attempted=False`, and the next run is scheduled for
  `2026-05-27 12:37:50 +03`.

## Last Verified Commands

```bash
.venv/bin/python -m pytest -q
.venv/bin/python -m pytest -q tests/test_live_probe.py
.venv/bin/python -m pytest -q tests/test_api_permissions.py tests/test_live_probe.py
.venv/bin/python -m pytest -q tests/test_api_permissions.py tests/test_spot_canary.py tests/test_live_probe.py
.venv/bin/python -m pytest -q tests/test_pool_config.py tests/test_pool_market_routes.py tests/test_pool_runtime.py tests/test_pool_replay.py
.venv/bin/python -m src.api_permissions --feature manual_live_probe
.venv/bin/python -m src.spot_canary --help
.venv/bin/python -m src.api_permissions --feature spot_live_canary
.venv/bin/python -m src.spot_canary --plan-only --audit-path /tmp/binance-bot-spot-canary-plan.jsonl --emergency-stop-path /tmp/binance-bot-spot-canary.stop
.venv/bin/python -m pytest -q tests/test_live_spot_allocator.py tests/test_api_permissions.py tests/test_spot_canary.py
.venv/bin/python -m src.live_spot_allocator --help
.venv/bin/python -m src.api_permissions --feature spot_live_pool
.venv/bin/python -m src.live_spot_allocator --plan-only --audit-path /tmp/binance-bot-live-spot-plan.jsonl --emergency-stop-path /tmp/binance-bot-live-spot.stop
.venv/bin/python -m pytest -q tests/test_margin.py tests/test_api_permissions.py
.venv/bin/python -m src.margin --help
.venv/bin/python -m src.margin --dry-run --confirm-live-margin
.venv/bin/python -m pytest -q tests/test_futures.py tests/test_api_permissions.py
.venv/bin/python -m src.futures --help
.venv/bin/python -m src.futures --dry-run --confirm-live-futures
.venv/bin/python -m pytest -q tests/test_transfers.py tests/test_api_permissions.py
.venv/bin/python -m src.transfers --help
.venv/bin/python -m src.transfers --dry-run --confirm-live-transfer
.venv/bin/python -m pytest -q tests/test_unified_pool.py tests/test_api_permissions.py
.venv/bin/python -m src.pool.unified --help
.venv/bin/python -m pytest -q tests/test_product_modules.py tests/test_api_permissions.py
.venv/bin/python -m src.product_modules --module prediction_trading
.venv/bin/python -m pytest -q tests/test_withdrawals.py tests/test_prediction.py tests/test_scope_audit.py tests/test_config.py tests/test_api_permissions.py tests/test_product_modules.py tests/test_live_probe.py tests/test_spot_canary.py tests/test_live_spot_allocator.py
.venv/bin/python -m src.scope_audit --help
.venv/bin/python -m src.withdrawals --help
.venv/bin/python -m src.prediction --help
.venv/bin/python -m src.product_modules --module prediction_dry_run
.venv/bin/python -m src.product_modules --module withdrawal_dry_run
.venv/bin/python -m pytest -q tests/test_margin_canary.py tests/test_futures_canary.py tests/test_transfer_canary.py tests/test_margin.py tests/test_futures.py tests/test_transfers.py tests/test_scope_audit.py tests/test_api_permissions.py tests/test_product_modules.py tests/test_advanced_readiness.py
.venv/bin/python -m src.margin_canary --help
.venv/bin/python -m src.futures_canary --help
.venv/bin/python -m src.transfer_canary --help
.venv/bin/python -m src.advanced_readiness --help
.venv/bin/python -m pytest -q tests/test_claude_spot.py tests/test_api_permissions.py tests/test_live_spot_allocator.py
.venv/bin/python -m py_compile src/claude_spot.py src/api_permissions.py
scripts/pool_replay.sh
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
- 2026-05-25 `systemctl status binance-bot.service --no-pager`: active
  (`python -m src.main`, PID `121585`) during the documentation review.
- 2026-05-25 focused phase 12 validation:
  `.venv/bin/python -m pytest -q tests/test_config.py tests/test_runtime.py tests/test_ops_check.py tests/test_operational_scripts.py`
  returned `52 passed`; `bash -n scripts/*.sh && scripts/smoke.sh && git diff --check`
  passed.
- 2026-05-25 final patched/migration candidate validation:
  `.venv/bin/python -m pytest -q` returned `154 passed`;
  `.venv/bin/python test_pipeline.py`, `bash -n scripts/*.sh`,
  `scripts/smoke.sh`, and `git diff --check` passed.
- 2026-05-25 live signed-subscription probe from the staged candidate:
  Binance `userDataStream.subscribe.signature` succeeded without placing an
  order.
- 2026-05-25 final user-service deployment checks:
  `systemctl --user show binance-bot.service` returned active with PID `137762`
  and `NRestarts=0`; `systemctl --user show binance-bot-backup.timer` returned
  active/enabled with next run at `2026-05-26 02:15:00 +03`.
- 2026-05-25 final deployed `venv/bin/python -m src.ops_check --telegram-test`:
  live ticker and balance reads passed and the Telegram test was delivered.
- 2026-05-25 final journal observation: public WebSocket connected, signed
  user-data stream subscribed, and closed candles logged at `22:44:00 +03`
  and `22:45:00 +03`.
- 2026-05-25 final no-sudo safeguard validation:
  `.venv/bin/python -m pytest -q` returned `155 passed`; pipeline, shell smoke,
  and `git diff --check` passed. User-service candles continued through
  `22:49:00 +03` with PID `137762` and `NRestarts=0`.
- 2026-05-26 phase 13 local verification:
  `.venv/bin/python -m pytest -q` returned `167 passed`;
  `.venv/bin/python test_pipeline.py`, `bash -n scripts/*.sh`,
  `scripts/smoke.sh`, and `git diff --check` passed.
- 2026-05-26 final deployed paper validation:
  `binance-pool-paper.service` is active/enabled with PID `161421` and
  `NRestarts=0`; `binance-bot.service` is inactive/disabled with PID `0`.
- 2026-05-26 clean first paper cycle:
  the catalog recorded `3590` spot market entries, `695` passed conservative
  execution eligibility gates, and `395` distinct assets received benchmark
  observations. Both virtual scorer portfolios were seeded from `0.0169745
  BNB` and recorded `hold` because paper ranking requires `96` cycles.
- 2026-05-26 paper-ledger backup:
  `pool-paper-20260525T211203Z-161544.db` passed SQLite `quick_check=ok`;
  `binance-pool-paper-backup.timer` is enabled for `02:30:00 +03`.
- 2026-05-26 phase 14 focused replay validation:
  `.venv/bin/python -m pytest -q tests/test_pool_config.py tests/test_pool_market_routes.py tests/test_pool_runtime.py tests/test_pool_replay.py`
  returned `13 passed`.
- 2026-05-26 phase 14 replay CLI validation:
  `.venv/bin/python -m src.pool.replay --fixture tests/fixtures/pool_market_replay.json --cycles 24 --db-path /tmp/binance-bot-pool-replay.db --quiet`
  completed `24` accelerated cycles with `46` holds, `2` simulated
  rebalances, `24` market/ticker loads, and `2` authenticated fee reads.
- 2026-05-26 phase 14 final local verification:
  `.venv/bin/python -m pytest -q` returned `170 passed`;
  `.venv/bin/python test_pipeline.py`, `bash -n scripts/*.sh`,
  `scripts/smoke.sh`, and `git diff --check` passed.
- 2026-05-26 replay-suite extension validation:
  `.venv/bin/python -m pytest -q tests/test_pool_replay.py tests/test_operational_scripts.py`
  returned `13 passed`; `POOL_REPLAY_CYCLES=6 POOL_REPLAY_DB_DIR=/tmp/binance-bot-replay-suite scripts/pool_replay.sh`
  passed both replay fixtures; `bash -n scripts/*.sh` passed.
- 2026-05-26 replay-suite final local verification:
  `.venv/bin/python -m pytest -q` returned `173 passed`;
  `scripts/pool_replay.sh` passed both default fixtures with isolated `/tmp`
  replay databases; `.venv/bin/python test_pipeline.py`, `bash -n scripts/*.sh`,
  `scripts/smoke.sh`, and `git diff --check` passed.
- 2026-05-26 live-probe focused validation:
  `.venv/bin/python -m pytest -q tests/test_live_probe.py` returned `7 passed`;
  `.venv/bin/python -m src.live_probe` without `--confirm-real-order` failed
  closed before any order path with `live probe requires --confirm-real-order`;
  `.venv/bin/python -m py_compile src/live_probe.py` passed.
- 2026-05-26 live-probe final local verification:
  `.venv/bin/python -m pytest -q` returned `180 passed`;
  `scripts/pool_replay.sh`, `.venv/bin/python test_pipeline.py`,
  `bash -n scripts/*.sh`, `scripts/smoke.sh`, and `git diff --check` passed.
- 2026-05-26 roadmap gate update verification:
  stale month-long evidence-gate wording scan returned no matches in roadmap,
  operations, README, tests, and scripts; touched-doc trailing-whitespace scan
  passed; `git diff --check` passed. No test suite was run because this session
  changed documentation only.
- 2026-05-26 phase 16 focused validation:
  `.venv/bin/python -m pytest -q tests/test_api_permissions.py tests/test_live_probe.py`
  returned `12 passed`; `.venv/bin/python -m src.api_permissions --help`
  passed.
- 2026-05-26 phase 16 final local verification:
  `.venv/bin/python -m pytest -q` returned `185 passed`;
  `scripts/pool_replay.sh`, `.venv/bin/python test_pipeline.py`,
  `bash -n scripts/*.sh && scripts/smoke.sh`, and `git diff --check` passed.
- 2026-05-26 live-safe permission diagnostic:
  `.venv/bin/python -m src.api_permissions --feature manual_live_probe`
  completed without placing an order. It reported LIVE mode, account type
  `SPOT`, reading OK, safe spot `order/test` OK for `BTC/USDT`,
  `manual_live_probe` allowed, and margin/futures/transfer/withdrawal/prediction
  capabilities policy-blocked for future phases.
- 2026-05-26 phase 17 focused validation:
  `.venv/bin/python -m pytest -q tests/test_api_permissions.py tests/test_spot_canary.py tests/test_live_probe.py`
  returned `21 passed`; `.venv/bin/python -m src.spot_canary --help` passed.
- 2026-05-26 phase 17 final local verification:
  `.venv/bin/python -m pytest -q` returned `194 passed`;
  `scripts/pool_replay.sh`, `.venv/bin/python test_pipeline.py`,
  `bash -n scripts/*.sh && scripts/smoke.sh`, and `git diff --check` passed.
- 2026-05-26 live-safe spot canary diagnostics:
  `.venv/bin/python -m src.api_permissions --feature spot_live_canary`
  completed without placing an order and reported the canary feature allowed.
  `.venv/bin/python -m src.spot_canary --plan-only --audit-path /tmp/binance-bot-spot-canary-plan.jsonl --emergency-stop-path /tmp/binance-bot-spot-canary.stop`
  completed without submitting an order. It planned a `BNB/USDT` sell of
  `0.009 BNB`, estimated notional `5.96619 USDT`, dynamic taker fee rate
  `0.00075`, and wrote the audit row under `/tmp`.
- 2026-05-26 phase 18 focused validation:
  `.venv/bin/python -m pytest -q tests/test_live_spot_allocator.py tests/test_api_permissions.py tests/test_spot_canary.py tests/test_live_probe.py`
  returned `30 passed`; `.venv/bin/python -m src.live_spot_allocator --help`
  and `.venv/bin/python -m py_compile src/live_spot_allocator.py src/spot_canary.py src/api_permissions.py`
  passed.
- 2026-05-26 phase 18 final local verification:
  `.venv/bin/python -m pytest -q` returned `203 passed`;
  `scripts/pool_replay.sh`, `.venv/bin/python test_pipeline.py`,
  `bash -n scripts/*.sh && scripts/smoke.sh`, and `git diff --check` passed.
- 2026-05-26 live-safe allocator diagnostics:
  `.venv/bin/python -m src.api_permissions --feature spot_live_pool` completed
  without placing an order and reported the guarded allocator feature allowed.
  `.venv/bin/python -m src.live_spot_allocator --plan-only --audit-path /tmp/binance-bot-live-spot-plan.jsonl --emergency-stop-path /tmp/binance-bot-live-spot.stop`
  completed without submitting an order. It planned a `BNB/USDT` sell of
  `0.009 BNB`, estimated notional `5.9616 USDT`, dynamic taker fee rate
  `0.00075`, benchmark values for `USDT`, `BTC`, `PAXG`, and `XAUT`, and wrote
  the audit row under `/tmp`.
- 2026-05-26 phase 19 focused validation:
  `.venv/bin/python -m pytest -q tests/test_margin.py tests/test_api_permissions.py`
  returned `12 passed`; `.venv/bin/python -m src.margin --help` and
  `.venv/bin/python -m py_compile src/margin.py src/api_permissions.py` passed.
  `.venv/bin/python -m src.margin --dry-run --confirm-live-margin` failed
  closed with live margin activation not implemented.
- 2026-05-26 phase 19 final local verification:
  `.venv/bin/python -m pytest -q` returned `211 passed`;
  `scripts/pool_replay.sh`, `.venv/bin/python test_pipeline.py`,
  `bash -n scripts/*.sh && scripts/smoke.sh`, and `git diff --check` passed.
- 2026-05-26 phase 20 focused validation:
  `.venv/bin/python -m pytest -q tests/test_futures.py tests/test_api_permissions.py`
  returned `11 passed`; `.venv/bin/python -m src.futures --help` and
  `.venv/bin/python -m py_compile src/futures.py src/api_permissions.py` passed.
  `.venv/bin/python -m src.futures --dry-run --confirm-live-futures` failed
  closed with live futures activation not implemented.
- 2026-05-26 phase 20 final local verification:
  `.venv/bin/python -m pytest -q` returned `218 passed`;
  `scripts/pool_replay.sh`, `.venv/bin/python test_pipeline.py`,
  `bash -n scripts/*.sh && scripts/smoke.sh`, and `git diff --check` passed.
- 2026-05-26 phase 21 focused validation:
  `.venv/bin/python -m pytest -q tests/test_transfers.py tests/test_api_permissions.py`
  returned `11 passed`; `.venv/bin/python -m src.transfers --help` and
  `.venv/bin/python -m py_compile src/transfers.py src/api_permissions.py`
  passed. `.venv/bin/python -m src.transfers --dry-run --confirm-live-transfer`
  failed closed with live transfer activation not implemented.
- 2026-05-26 phase 21 final local verification:
  `.venv/bin/python -m pytest -q` returned `225 passed`;
  `scripts/pool_replay.sh`, `.venv/bin/python test_pipeline.py`,
  `bash -n scripts/*.sh && scripts/smoke.sh`, and `git diff --check` passed.
- 2026-05-26 phase 22 focused validation:
  `.venv/bin/python -m pytest -q tests/test_unified_pool.py tests/test_api_permissions.py`
  returned `8 passed`; `.venv/bin/python -m src.pool.unified --help` and
  `.venv/bin/python -m py_compile src/pool/unified.py src/api_permissions.py src/pool/__init__.py`
  passed.
- 2026-05-26 phase 22 final local verification:
  `.venv/bin/python -m pytest -q` returned `229 passed`;
  `scripts/pool_replay.sh`, `.venv/bin/python test_pipeline.py`,
  `bash -n scripts/*.sh && scripts/smoke.sh`, and `git diff --check` passed.
- 2026-05-26 phase 13 readiness-gate partial execution:
  `systemctl --user show binance-bot.service` returned inactive/disabled and
  `systemctl --user show binance-pool-paper.service` returned active/enabled.
  The deployed paper ledger had `18` catalog cycles, `36` paper decisions, and
  both scorers still reported `hold` because paper ranking requires `96` cycles.
- 2026-05-26 phase 13 readiness-gate validation:
  `.venv/bin/python -m pytest -q tests/test_pool_config.py tests/test_pool_market_routes.py tests/test_pool_runtime.py tests/test_pool_replay.py`
  returned `15 passed`; `.venv/bin/python -m pytest -q` returned `229 passed`;
  `scripts/pool_replay.sh`, `.venv/bin/python test_pipeline.py`, and
  `bash -n scripts/*.sh && scripts/smoke.sh` passed.
- 2026-05-26 live-safe phase 13 diagnostics:
  `.venv/bin/python -m src.api_permissions --feature spot_live_canary` and
  `.venv/bin/python -m src.api_permissions --feature spot_live_pool` completed
  without placing an order and reported both spot features allowed. Margin,
  futures, universal transfer, withdrawals, and prediction remained
  policy-blocked. `.venv/bin/python -m src.spot_canary --plan-only --audit-path /tmp/binance-bot-phase13-spot-canary-plan.jsonl --emergency-stop-path /tmp/binance-bot-phase13-spot-canary.stop`
  completed without submitting an order; it planned a `BNB/USDT` sell of
  `0.009 BNB`, estimated notional `5.96718 USDT`, dynamic taker fee rate
  `0.00075`, and wrote the audit row under `/tmp`.
- 2026-05-26 phase 23 focused validation:
  `.venv/bin/python -m pytest -q tests/test_product_modules.py tests/test_api_permissions.py`
  returned `11 passed`; `.venv/bin/python -m src.product_modules --module prediction_trading`
  and `.venv/bin/python -m py_compile src/product_modules.py src/api_permissions.py`
  passed.
- 2026-05-26 phase 23 final local verification:
  `.venv/bin/python -m pytest -q` returned `236 passed`;
  `scripts/pool_replay.sh`, `.venv/bin/python test_pipeline.py`,
  `bash -n scripts/*.sh && scripts/smoke.sh`, and `git diff --check` passed.
- 2026-05-26 capability-roadmap documentation update:
  added phases 24 through 30 to `IMPLEMENTATION_PLAN.md` and updated `CODEX.md`
  and `PROGRESS.md`. No test suite was run because this update changed roadmap
  documentation only; trailing-whitespace scan and `git diff --check` passed.
- 2026-05-26 phases 27-29 focused validation:
  `.venv/bin/python -m pytest -q tests/test_withdrawals.py tests/test_prediction.py tests/test_scope_audit.py tests/test_config.py tests/test_api_permissions.py tests/test_product_modules.py tests/test_live_probe.py tests/test_spot_canary.py tests/test_live_spot_allocator.py`
  returned `77 passed`; module `--help` and local product-gate reports passed.
- 2026-05-26 phases 27-29 final local verification:
  `.venv/bin/python -m pytest -q` returned `256 passed`;
  `scripts/pool_replay.sh` passed both default fixtures without live writes.
- 2026-05-26 Phase 24-26 plan-only canary and Phase 30 focused validation:
  `.venv/bin/python -m pytest -q tests/test_margin_canary.py tests/test_futures_canary.py tests/test_transfer_canary.py tests/test_margin.py tests/test_futures.py tests/test_transfers.py tests/test_scope_audit.py tests/test_api_permissions.py tests/test_product_modules.py tests/test_advanced_readiness.py`
  returned `69 passed`; the related command `--help` paths passed and
  `git diff --check` returned clean.
- 2026-05-26 Phase 24-30 final local verification:
  `.venv/bin/python -m pytest -q` returned `287 passed`;
  `scripts/pool_replay.sh` passed both replay fixtures;
  `.venv/bin/python test_pipeline.py`, `scripts/smoke.sh`,
  `git diff --check`, and the conflict-marker scan passed.
- 2026-05-26 Phase 31 focused validation:
  `.venv/bin/python -m pytest -q tests/test_pool_report.py tests/test_pool_history_replay.py tests/test_pool_replay.py tests/test_pool_runtime.py tests/test_history.py tests/test_operational_scripts.py`
  returned `23 passed`; the paper report read the deployed ledger without
  writes; the isolated public-candle historical replay command completed.
- 2026-05-26 Phase 31 final local verification:
  `.venv/bin/python -m pytest -q` returned `292 passed`;
  `scripts/pool_replay.sh`, `.venv/bin/python test_pipeline.py`,
  `scripts/smoke.sh`, `git diff --check`, and the conflict-marker scan passed.
- 2026-05-26 Phase 32 focused validation:
  `.venv/bin/python -m pytest -q tests/test_claude_spot.py tests/test_api_permissions.py tests/test_live_spot_allocator.py`
  returned `21 passed`; `.venv/bin/python -m py_compile src/claude_spot.py src/api_permissions.py`
  passed.
- 2026-05-26 Phase 32 final local verification:
  `.venv/bin/python -m pytest -q` returned `299 passed`;
  `scripts/pool_replay.sh`, `.venv/bin/python test_pipeline.py`,
  `scripts/smoke.sh`, `git diff --check`, and the conflict-marker scan passed.
- 2026-05-26 Phase 32 lot-step/live validation:
  `.venv/bin/python -m pytest -q tests/test_pool_market_routes.py tests/test_claude_spot.py`
  returned `17 passed`; `.venv/bin/python -m pytest -q` returned `303 passed`.
  `scripts/setup_user_deploy.sh` refreshed the user deployment. The deployed
  `python -m src.claude_spot --plan-only` completed without submitting an order
  and offered whole-unit `HBAR/BNB` sizing. `binance-claude-spot.timer` was then
  enabled and active; its first live cycle submitted `SUI/BNB` market buy
  `11.0 SUI`, status `closed`, cost `0.016918 BNB`, fee `0.00001264 BNB`, with
  current free balances `SUI=11` and `BNB=0.00004386`.
- 2026-05-27 reboot/resume live observation:
  the new boot started `binance-claude-spot.service` at `08:42:23 +03`; the
  cycle finished with status `submitted`. It sold `11.0 SUI` via `SUI/JPY`,
  bought `119.4 POL` via `POL/JPY`, left `5.604 JPY` according to leg
  reconciliation, recorded `telegram_sent=false`, and scheduled the next timer for
  `08:57:23 +03`.
- 2026-05-27 symbol-permission failure observation:
  the `10:46:23 +03` cycle selected `POL->SUI:6315f257f3af`, whose first leg was
  `POL/IDR`. Binance rejected the live order with `-2010`/symbol not permitted
  even though earlier cycles had successfully traded symbols such as `POL/TRY`,
  `POL/FDUSD`, `SUI/JPY`, `DOT/FDUSD`, `APT/ETH`, and `KNC/USDT`. The timer was
  disabled at `10:56:49 +03`; `systemctl --user status
  binance-claude-spot.timer` reported inactive/disabled. Current live free
  balance check showed `POL 113.2299` as the main holding.
- 2026-05-27 Claude Spot route-scope fix:
  `.venv/bin/python -m pytest -q tests/test_claude_spot.py tests/test_pool_market_routes.py`
  returned `19 passed`; `.venv/bin/python -m pytest -q` returned `305 passed`.
  After widening the guard to block all traditional fiat route assets,
  `.venv/bin/python -m pytest -q tests/test_claude_spot.py tests/test_pool_market_routes.py`
  returned `20 passed` and `.venv/bin/python -m pytest -q` returned
  `306 passed`. `scripts/setup_user_deploy.sh` deployed the fix to
  `~/.local/share/binance-bot`, and the deployed `.env` now contains
  `CLAUDE_SPOT_BLOCK_FIAT_ASSETS=true` plus
  `CLAUDE_SPOT_BLOCKED_QUOTES=IDR`. A no-order deployed context check returned
  `block_fiat_assets True`, `blocked_quotes ['IDR']`, `candidate_count 10`,
  `contains_fiat False`, and route symbols
  `POL/FDUSD,OPEN/FDUSD,POL/USDT,SHIB/USDT,POL/FDUSD,ADA/FDUSD,POL/USDT,
  STX/USDT,POL/USDT,CRV/USDT,POL/FDUSD,SUI/FDUSD,POL/FDUSD,XRP/FDUSD,
  POL/USDT,XAUT/USDT,POL/USDT,HBAR/USDT,POL/USDT,AAVE/USDT`.
- 2026-05-27 Claude Spot live restart:
  `systemctl --user enable --now binance-claude-spot.timer` enabled the timer.
  `systemctl --user start binance-claude-spot.service` completed with
  `Result=success` and exit status `0`. The audit row at
  `~/.local/share/binance-bot/data/claude-spot-decisions.jsonl` had
  `status=submitted`, `submission_attempted=True`, `decision_route=
  POL->CRV:1ba1a17504f8`, and submitted symbols `POL/USDT,CRV/USDT`. The timer
  listed the next run at `2026-05-27 11:45:16 +03`.
- 2026-05-27 Claude Spot timeout fix:
  the `12:16:23 +03` timer cycle failed because Claude did not return a
  decision before the old `120` second timeout; no order was submitted.
  `.venv/bin/python -m pytest -q tests/test_claude_spot.py tests/test_pool_market_routes.py`
  returned `22 passed`; after the timer scheduling fix,
  `.venv/bin/python -m pytest -q tests/test_claude_spot.py tests/test_operational_scripts.py tests/test_pool_market_routes.py`
  returned `30 passed`; `.venv/bin/python -m pytest -q` returned `308 passed`.
  `scripts/setup_user_deploy.sh` deployed the fix, the deployed `.env` has
  `CLAUDE_SPOT_TIMEOUT_SECONDS=240`, and
  `~/.config/systemd/user/binance-claude-spot.timer` has both
  `OnActiveSec=15min` and `OnUnitActiveSec=15min`. `git diff --check` passed.
  The post-deploy audit row at `2026-05-27T09:24:21.538936+00:00` had
  `status=held`, `submission_attempted=False`, and no error.

## Completed Handoff Tasks

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

These completed tasks cover phases 1 through 11. Phase 12 is tracked below as
new operational-hardening work.

## Phase 12 Active Tasks

- [x] Add the operations runbook, backup/restore scripts, and daily backup
  service/timer.
- [x] Add read-only exchange/Telegram verification and WebSocket health alerting.
- [x] Pass local tests and create a verified live database backup.
- [x] Deploy the validated changes, enable both WebSocket runtime modes, and
  record post-deploy monitoring results.
- [x] Move the active runtime to `systemctl --user` after installing the
  corrected private WebSocket implementation.

## Phase 13 Active Tasks

- [x] Stop and disable fixed-size live execution and add a code-level rejection
  for fixed-size real-money startup.
- [x] Implement the paper-only modular pool, dynamic market/fee/routing engine,
  benchmark valuation, paper ledgers, and scorer plugins.
- [x] Deploy and verify the paper user service plus verified paper-ledger
  backups without enabling a real order path.
- [ ] Pass replay, live-market paper warm-up, permission diagnostics, and a tiny
  live-canary readiness review before considering a separately approved live
  pool-execution phase.

## Phase 14 Active Tasks

- [x] Add a fast deterministic replay harness for the existing paper runtime.
- [x] Add replay fixtures/tests for routed reallocations and tiny-balance
  minimum-order holds.
- [x] Keep replay databases isolated from the production paper ledger.
- [x] Update operations and verification docs with the fast replay command.
- [x] Add a replay-suite wrapper and stress fixture for spread/liquidity
  rejection coverage.

## Phase 15 Active Tasks

- [x] Add a manual one-shot live probe command with explicit confirmation gates.
- [x] Select one smallest valid direct-USDT market sell from free balances.
- [x] Audit submitted probe orders and send a `LIVE PROBE` Telegram alert.
- [x] Test candidate selection, no direct-market aborts, confirmation gates,
  audit/alert labeling, and no retry after ambiguous order failure.

## Phase 16 Active Tasks

- [x] Add the Binance API permission and capability inspector.
- [x] Map bot features to required permissions and fail closed when missing.
- [x] Add clear diagnostics for permission/IP failures such as `-2015`.
- [x] Keep withdrawals blocked by policy unless a future safety phase approves
  them.
- [x] Test permission parsing, feature gating, diagnostics, and disabled defaults.

## Phase 17 Active Tasks

- [x] Add a separate spot live-canary module outside the paper-only pool runtime.
- [x] Gate canary execution on replay, live-market paper warm-up, permission
  diagnostics, explicit confirmation, and canary limits.
- [x] Use dynamic balance, market-rule, liquidity, fee, precision, minimum-order,
  and slippage data for every canary order.
- [x] Enforce max notional, daily notional, daily order count, emergency stop,
  audit, Telegram alerts, and no retry after ambiguous exchange failures.

## Phase 18 Active Tasks

- [x] Promote canary mechanics into a controlled live spot allocator only after
  explicit approval.
- [x] Enforce exposure, turnover, liquidity, drawdown, daily loss, order count,
  reserve, and route-risk limits.
- [x] Reconcile balances, open orders, fills, fees, and live intent after each
  allocation cycle.

## Phase 19 Active Tasks

- [x] Add margin account discovery and dry-run modeling for isolated/cross margin.
- [x] Model borrow limits, debt, interest, repayment, margin level, and
  liquidation risk.
- [x] Keep live margin disabled until config, permissions, tests, and approval
  are complete.

## Phase 20 Active Tasks

- [x] Add USD-M futures modeling before any live futures order path.
- [x] Model leverage, margin mode, funding, liquidation distance, reduce-only
  orders, and futures reconciliation.
- [x] Keep live futures disabled until paper/replay validation, permissions,
  risk limits, and approval are complete.

## Phase 21 Active Tasks

- [x] Add dry-run capital transfer planning across spot, funding, margin, and
  futures wallets.
- [x] Require explicit approval before any automatic transfer.
- [x] Audit proposed and submitted transfers with source, destination, asset,
  amount, reason, and resulting allocation state.

## Phase 22 Active Tasks

- [x] Extend pool valuation across spot, margin equity/debt, futures collateral,
  open PnL, interest, funding, reserves, and transfer availability.
- [x] Support `USDT`, `BTC`, `PAXG`, `XAUT`, and configurable-basket benchmarks.
- [x] Add allocation constraints by asset, product, account type, leverage,
  liquidity, volatility, and reserves.

## Phase 23 Active Tasks

- [x] Track prediction trading and future Binance products as optional modules.
- [x] Require each module to define its strategy interface, permission mapping,
  risk model, audit trail, tests, and activation gate.
- [x] Keep API-key permission alone from authorizing any bot feature.

## Phase 24 Active Tasks

- [x] Add a foreground-only plan-only margin canary using the existing dry-run margin
  model.
- [x] Enforce debt, borrow, repayment-intent, margin-level,
  liquidation-buffer, emergency-stop, audit, and explicit-confirmation gates
  in the plan-only path.
- [x] Keep live margin write endpoints disabled during plan-only readiness work.
- [ ] Add live submission, post-action reconciliation, complete alert
  behavior, and activation tests only after a separate explicit approval.
- [ ] Keep live margin write endpoints disabled until any approved execution work is
  tested, reviewed, and explicitly approved.

## Phase 25 Active Tasks

- [x] Add a foreground-only plan-only USD-M futures canary using the existing dry-run
  futures model.
- [x] Enforce max notional, max leverage, margin-mode, liquidation-distance,
  funding, reduce-only exit-plan, emergency-stop, audit, and
  explicit-confirmation gates in the plan-only path.
- [x] Keep live futures write endpoints disabled during plan-only readiness work.
- [ ] Add live submission, post-action reconciliation, complete alert
  behavior, and activation tests only after a separate explicit approval.
- [ ] Keep live futures write endpoints disabled until any approved execution work is
  tested, reviewed, and explicitly approved.

## Phase 26 Active Tasks

- [x] Add a foreground-only plan-only universal-transfer canary using the existing
  dry-run transfer planner.
- [x] Enforce wallet activation, transferable balance, transfer caps, margin-debt
  block, isolated-symbol requirements, emergency stop, audit, and explicit
  confirmation in the plan-only path.
- [x] Keep live universal-transfer submission disabled during plan-only readiness work.
- [ ] Add live submission, post-action reconciliation, complete alert
  behavior, and activation tests only after a separate explicit approval.
- [ ] Keep live universal-transfer submission disabled until any approved execution work is
  implemented, tested, reviewed, and explicitly approved.

## Phase 27 Implemented Tasks

- [x] Add a dry-run withdrawal safety module while keeping withdrawals
  hard-blocked.
- [x] Model address allowlists, network selection, daily/request caps, approvals,
  cooldown windows, emergency stop, operator identity, and audit.
- [x] Require a future withdrawal-specific activation phase before any withdrawal
  endpoint can be called.

## Phase 28 Implemented Tasks

- [x] Add a prediction-trading dry-run scaffold for local/injected read-only
  market payloads until a verified safe Binance endpoint is available.
- [x] Model market selection, stake caps, settlement windows, settlement risk,
  drawdown, availability checks, emergency stop, and audit.
- [x] Keep live prediction trading disabled until a later activation phase is
  implemented, tested, reviewed, and explicitly approved.

## Phase 29 Implemented Tasks

- [x] Add local/operator symbol-whitelist configuration and API-key scope audit
  diagnostics.
- [x] Audit configured symbols, pool routes, canary symbols, margin/futures
  symbols, transfer assets, withdrawal assets, and prediction markets against
  the local whitelist; enforce resolved spot markets on live-capable commands.
- [x] Keep unknown Binance-side whitelist state fail-closed for live features.

## Phase 30 Implemented Tasks

- [x] Add an advanced-product readiness checklist mirroring the phase 13 gate.
- [x] Require serial live canary execution for transfer, margin, and futures.
- [x] Keep advanced-product automation disabled until each product passes its
  readiness review and a later activation phase approves background behavior.

## Phase 32 Implemented Tasks

- [x] Add the Claude Spot brain adapter with the configured money-maker mission
  prompt and structured JSON decision parsing.
- [x] Add a separate autonomous Spot runtime that builds dynamic routed
  candidates, asks Claude for one action, and keeps `src.pool` paper-only.
- [x] Allow Claude mode to use the whole available Spot wallet and execute one
  selected route of up to three serial Spot order legs per 15-minute cycle;
  default route context now uses aggressive small-market discovery with up to
  10 scored candidates and one route quote per candidate after the top-liquid
  live mode repeatedly held because every large route burned value.
- [x] Add small-market audit fields for quote volume, spread, liquidity tier,
  route legs, immediate friction, and the candidate-offer reason; keep
  `liquid_top_value` as the rollback market-picking mode.
- [x] Replace the Phase 13 warm-up gate for Claude mode while retaining
  exchange-validity, Binance lot-size step sizing, permission/order-test,
  emergency-stop, audit, reconciliation, and halted-route invariants.
- [x] Add disabled-by-default user service/timer files for the 15-minute Claude
  Spot cycle.
- [x] Run a real local Claude structured-response probe and Binance Spot
  permission/order-test preflight in the deployed environment before enabling
  `binance-claude-spot.timer`.
- [x] Enable the Claude Spot timer only after the live environment gates are
  intentionally set.

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

- Paper modeling risk remains: ranking quality, liquidity gates, benchmark
  choice, and modeled slippage/fees require observation before they justify any
  real execution design.
- Fast replay is a regression and strategy-iteration tool. It validates
  mechanics quickly but does not by itself approve live-money pool execution.
- The faster live-readiness path still requires live-market paper warm-up,
  permission diagnostics, explicit approval, tiny canary limits, audit, and
  stop conditions before any live pool allocation.
- Advanced-product code work may be developed in parallel, but live transfer,
  margin, and futures canary execution must be serial to keep account-state
  changes attributable and reconcilable.
- The live probe is intentionally limited to a single foreground direct-USDT
  sell. It does not approve or implement live pool allocation.
- Deployment history: the service replaced during rollout was also running in
  real-money mode and referenced an older application path such as
  `/opt/binance-bot/src/orchestrator.py`. It was stopped before the current
  deployment was started.
- The deployed `.env` still contains the historical live settings, but
  `binance-bot.service` is inactive/disabled and fixed-size live startup is
  blocked by code. Do not manually re-enable that unit.
- The active `binance-pool-paper.service` is paper-only; it reads market/account
  information and mutates only its virtual ledger.
- The old `/opt/binance-bot` system service remains disabled/inactive; routine
  paper management uses user systemd commands without sudo.
- Six-day public-candle research with the current `0.0169745 BNB` seed found
  `742` routed-minimum/cost holds. A minimum valid Spot conversion may be too
  large as a percentage of this account for the current allocation policy; this
  must be reviewed before any small-account live activation.
- Claude Spot Phase 32 deliberately permits whole-Spot-wallet allocation and
  removes the Phase 13 warm-up/canary-cap gate for Claude mode. Its code,
  tests, deployed Claude/Binance preflight, user timer activation, and
  lot-step live-sizing fix are complete. It now defaults to aggressive
  small-market discovery for the small account while retaining
  exchange-validity, audit, reconciliation, and emergency-stop invariants. The
  current live Spot position after the 2026-05-27 restart is `47.3526 CRV`
  plus route residuals and prior dust. Subsequent cycles rotated the main
  position through `AAVE`, `STX`, `TRUMP`, `SHIB`, and finally `BONK`; the
  Claude timer is now disabled after the anti-churn pause below.

## Next Recommended Action

Phase 32 is implemented as the new Claude autonomous Spot path. It replaces the
unfinished Phase 13 warm-up gate for Claude mode only, uses headless Claude
decisions, can use the whole available Spot wallet, and can execute one dynamic
routed Spot action of up to three serial legs per 15-minute timer cycle. The
deployed timer was re-enabled after Binance rejected `POL/IDR` and Claude Spot
was updated to block traditional fiat route assets locally. The timer is now
stopped and disabled again after live audit showed excessive churn and a
partially failed route. Phase 33 now adds market evidence, trade memory, and a
strict live evidence gate on top of those anti-churn controls. The current main
live position is `1719992.29 BONK` plus route residuals and dust, and the next
action is operator review before any re-enable. The separate Phase 13 paper-pool
evidence, approved live-write canaries for margin/futures/transfer, withdrawal
automation, prediction automation, and other future product automation remain
outside Phase 32/33.

## Claude Spot Anti-Churn Pause - 2026-05-27 14:22 +03

- User reported the autonomous Claude Spot strategy was trading too much and
  losing money. The live Claude Spot timer was stopped and disabled at
  `2026-05-27 14:13:02 +03`; `binance-claude-spot.timer` is now
  `inactive (dead)` with no next trigger.
- Audit review confirmed excessive churn after activation:
  `CRV -> AAVE`, `AAVE -> STX`, `STX -> TRUMP`, a partially failed
  `TRUMP -> OPEN`, `FDUSD -> SHIB`, and `SHIB -> BONK`. The last submitted
  route completed at `2026-05-27 11:12:02 UTC`.
- Current account read after the pause shows the main free Spot holding as
  `1719992.29 BONK`, plus small residual/dust balances in `JPY`, `TRY`,
  `SHIB`, `STX`, `EUR`, `CRV`, `XLM`, `POL`, `FDUSD`, `USDT`, and older dust
  assets.
- Implemented and deployed hard anti-churn controls for Claude Spot:
  immediate-friction routes above the configured bps cap are removed before
  Claude can choose them, live submitted rebalances obey a configured cooldown,
  live submitted rebalances obey a configured daily cap, and live submission
  holds after a failed mutating route until operator review.
- Deployed runtime configuration now sets
  `CLAUDE_SPOT_ALLOW_NEGATIVE_FRICTION_BPS=50`,
  `CLAUDE_SPOT_MIN_TRADE_INTERVAL_SECONDS=21600`,
  `CLAUDE_SPOT_MAX_SUBMITTED_TRADES_PER_DAY=2`,
  `CLAUDE_SPOT_HALT_AFTER_FAILED_SUBMISSION=true`,
  `CLAUDE_SPOT_TIMEOUT_SECONDS=240`, and
  `CLAUDE_SPOT_BLOCK_FIAT_ASSETS=true`.
- Verification: focused Claude Spot/ops/route tests passed (`34 passed`), the
  full suite passed (`312 passed`), and a deployed no-submit discipline check
  now returns `trade cooldown active after the last submitted rebalance`.

Next action: keep `binance-claude-spot.timer` disabled until the operator
explicitly decides whether to re-enable it under the new anti-churn settings.

## Phase 33 Claude Spot Evidence Gate - 2026-05-27 14:47 +03

- Implemented Phase 33 after the anti-churn pause. Claude Spot candidates now
  carry compact market evidence covering `1h`, `4h`, `24h`, and `7d` returns,
  hourly volatility, 24h volume change, route break-even move, and expected
  edge after route friction.
- Added Claude Spot trade memory from `data/claude-spot-decisions.jsonl`.
  The context now summarizes recent submitted routes, current/open holding
  state, reconstructed closed/open PnL where possible, and recent loss streak.
- Added a strict live-only evidence gate. Even if Claude chooses a rebalance,
  live submission now holds unless the selected route has ready evidence, its
  expected edge clears `CLAUDE_SPOT_MARKET_EVIDENCE_MIN_EDGE_BPS`, and the
  trade-memory loss-streak gate is clear.
- Added `python -m src.claude_spot --evidence-only` to print the evidence
  context without asking Claude or submitting orders.
- Deployed runtime config now explicitly sets
  `CLAUDE_SPOT_MARKET_EVIDENCE_ENABLED=true`,
  `CLAUDE_SPOT_REQUIRE_MARKET_EVIDENCE_FOR_LIVE=true`,
  `CLAUDE_SPOT_MARKET_EVIDENCE_MIN_EDGE_BPS=100`,
  `CLAUDE_SPOT_MARKET_EVIDENCE_LOOKBACK_HOURS=168`,
  `CLAUDE_SPOT_MARKET_EVIDENCE_MAX_SYMBOLS=12`,
  `CLAUDE_SPOT_MARKET_EVIDENCE_TIMEFRAME=1h`,
  `CLAUDE_SPOT_TRADE_MEMORY_LIMIT=8`, and
  `CLAUDE_SPOT_MAX_RECENT_LOSS_STREAK=2`.
- Verification: `tests/test_claude_spot.py` passed (`25 passed`), the focused
  Claude/history/ops/route set passed (`43 passed`), and the full suite passed
  (`318 passed`). The user deploy was refreshed with `scripts/setup_user_deploy.sh`.
- Deployed no-submit context check loaded the new settings successfully and
  found `candidate_count=0` under the current conservative route/friction
  filters. `binance-claude-spot.timer` and `binance-claude-spot.service`
  remain inactive/disabled; no live order was submitted.

Next action: keep Claude Spot disabled until the operator explicitly requests a
review/re-enable. If re-enabled later, run `--evidence-only` first and only
then consider the timer after reviewing the evidence context.

## Claude Spot Re-Enabled - 2026-05-27 14:54 +03

- User explicitly requested live re-enable after Phase 33. A deployed
  no-submit evidence precheck was run first. It confirmed
  `CLAUDE_SPOT_MARKET_EVIDENCE_ENABLED=true`,
  `CLAUDE_SPOT_REQUIRE_MARKET_EVIDENCE_FOR_LIVE=true`,
  `CLAUDE_SPOT_MARKET_EVIDENCE_MIN_EDGE_BPS=100`, trade memory status
  `ready`, recent loss streak `0`, and `candidate_count=0`.
- Re-enabled `binance-claude-spot.timer` with
  `systemctl --user enable --now binance-claude-spot.timer`. The timer is
  `enabled` and `active (waiting)`.
- The enable operation triggered one immediate `binance-claude-spot.service`
  cycle at `2026-05-27 14:53:30 +03`. It exited successfully at
  `14:53:49 +03` with `status=held`, `candidate_route_count=0`,
  `submission_attempted=false`, and the hold reason
  `trade cooldown active after the last submitted rebalance`.
- No live order was submitted during the re-enable cycle. The next scheduled
  Claude Spot timer trigger was `2026-05-27 15:08:30 +03`.

Next action: monitor the next timer cycles and audit rows. Keep the emergency
stop/timer-disable path ready if it starts submitting unexpectedly.

## Phase 34 Five-Minute Claude Spot Live Profile - 2026-05-27 19:13 +03

- Implemented the requested Claude Spot live-profile change. The repo and
  deployed runtime now keep the Phase 33 compact evidence windows (`1h`, `4h`,
  `24h`, `7d`) and dynamic per-leg fee/friction context, but fixed cooldown
  and daily submitted-trade cap are disabled with
  `CLAUDE_SPOT_MIN_TRADE_INTERVAL_SECONDS=0` and
  `CLAUDE_SPOT_MAX_SUBMITTED_TRADES_PER_DAY=0`.
- Updated Claude's prompt to make after-fee value growth explicit: think every
  cycle, do not churn the wallet under fees/spread/slippage, hold when no
  supplied route beats the current long Spot position after costs, and take a
  supplied route only when the evidence shows a real after-cost opportunity.
- Updated the user timer from 15 minutes to 5 minutes and raised the Claude
  decision timeout from `240` seconds to `300` seconds. The deployed unit now
  shows `OnActiveSec=5min` and `OnUnitActiveSec=5min`.
- 2026-05-29: Claude CLI session-limit failures started after the 5-minute
  schedule had been running overnight. Per operator preference, increased the
  Claude Spot user timer cadence from 5 minutes to 10 minutes while leaving
  visible failed cycles in place. The deployed unit and repo template now use
  `OnActiveSec=10min` and `OnUnitActiveSec=10min`.
- Preserved hard live gates: Binance market rules, free balances, dynamic
  route fees, local scope/fiat blocking, order-test preflight, friction cap,
  market-evidence gate, trade-memory loss-streak gate, emergency stop,
  failed-route halt, audit, reconciliation, and no margin/futures/transfer/
  withdrawal/prediction writes.
- Verification before deploy: `.venv/bin/python -m pytest -q
  tests/test_claude_spot.py tests/test_operational_scripts.py` passed
  (`34 passed`), full `.venv/bin/python -m pytest -q` passed (`319 passed`),
  and `git diff --check` passed.
- Deployment: stopped the old timer, refreshed the user deployment with
  `scripts/setup_user_deploy.sh`, patched the deployed `.env`, reloaded
  systemd, and re-enabled `binance-claude-spot.timer`.
- Deployed no-submit evidence check succeeded before re-enable. It confirmed
  cooldown `0`, daily cap `0`, timeout `300`, evidence gate enabled, current
  main holding `1719992.29 BONK`, portfolio estimate around `10.526 USDT`,
  trade memory ready, current BONK open PnL around `+134.9 bps`, and
  `candidate_routes=[]` under the current friction/evidence filters.
- First live 5-minute cycle after re-enable called Claude and completed held
  with no order submitted. Audit timestamp:
  `2026-05-27T16:12:33.986258+00:00`; status `held`;
  `candidate_route_count=0`; reason: no executable supplied route, BONK is
  currently positive, and churning would destroy value.

Next action: monitor the next 5-minute audit rows. If candidate routes remain
empty for many cycles, inspect whether the `50 bps` immediate-friction cap is
too restrictive for the current BONK source route universe before loosening any
live gate.

## Phase 35 Social Signal Intelligence - 2026-05-27 23:00 +03

- Implemented the roadmap-approved read-only social intelligence layer. New
  `src.social_signal` normalizes permitted Telegram Bot API updates and
  official X recent-search payloads, extracts mentioned assets, writes local
  JSONL audit rows with hashed author/channel identifiers, and summarizes
  recent evidence per asset.
- Added heuristic signal classification for bullish calls, bearish warnings,
  neutral mentions, and manipulation/pump-risk language. Added an optional
  headless-Claude classifier path (`SOCIAL_SIGNAL_AI_ENABLED=false` by
  default) that uses schema-validated JSON output and no tools.
- Integrated social evidence into Claude Spot candidate context when
  `SOCIAL_SIGNAL_ENABLED=true`. If
  `SOCIAL_SIGNAL_BLOCK_MANIPULATION_RISK=true` and the per-asset risk score
  clears the threshold, the affected route is removed before Claude can select
  it. The feature is disabled by default and does not add any live order path.
- Updated `.env.example`, `README.md`, `CODEX.md`, and
  `IMPLEMENTATION_PLAN.md` for Phase 35. Social evidence remains additive and
  cannot bypass Binance market rules, dynamic fees/friction, market-evidence,
  trade-memory, order-test, emergency-stop, audit, or reconciliation gates.
- Current live-bot assessment before this implementation: the 5-minute Claude
  Spot timer is active and repeatedly holding with no submitted orders. Latest
  read-only evidence check estimated total portfolio value at about
  `10.4206 USDT`, main holding `1719992.29 BONK`, and BONK open PnL about
  `+33.2 bps` versus its entry. The first recorded Claude route had source
  value about `11.1169 USDT`, so the full Claude live sequence is still down
  roughly `6.3%`, mostly from the earlier churn before Phase 33/34 controls.

- Verification: focused `.venv/bin/python -m pytest -q tests/test_social_signal.py
  tests/test_claude_spot.py` passed (`32 passed`), full
  `.venv/bin/python -m pytest -q` passed (`325 passed`), and
  `git diff --check` passed.
- Deployed the updated user copy with `scripts/setup_user_deploy.sh`, added
  explicit disabled social-signal env defaults to the deployed `.env`, and
  re-enabled `binance-claude-spot.timer`. The immediate post-deploy cycle at
  `2026-05-27T20:08:06.516797+00:00` completed `held` with
  `candidate_route_count=0` and `submission_attempted=false`.

Next action: keep monitoring 5-minute audit rows. Social collection remains
disabled until API credentials/source scopes are explicitly configured.

## Phase 36 Telegram Decision Explanations - 2026-05-27 23:20 +03

- Implemented deterministic Claude Spot Telegram decision explanations for
  every cycle status: `held`, `planned`, `submitted`, `failed`, and `blocked`.
  The alert now includes status, action, route, ELI5, candidate count,
  submission state, the concise reason, and the audit path.
- Hold explanations now distinguish the important reason paths: no candidates,
  weak/insufficient market evidence after fees and slippage, Claude timeout,
  failed-route halt, cooldown/cap discipline, loss-streak protection, or
  Claude's own hold judgment.
- Added `CLAUDE_SPOT_TELEGRAM_EXPLAIN=true` and
  `CLAUDE_SPOT_TELEGRAM_REASON_CHARS=450`. The explanation is derived from the
  existing result/audit data, does not call Claude again, and does not change
  any live trading gate, prompt, route selection, order-test, emergency-stop,
  audit, or reconciliation behavior.
- Updated `.env.example`, `README.md`, `CODEX.md`, and
  `IMPLEMENTATION_PLAN.md` for Phase 36.
- Verification: focused `.venv/bin/python -m pytest -q tests/test_claude_spot.py`
  passed (`35 passed`), full `.venv/bin/python -m pytest -q` passed
  (`332 passed`), and `git diff --check` passed.
- Deployment: refreshed the user-owned deployed copy with
  `scripts/setup_user_deploy.sh` and re-enabled `binance-claude-spot.timer`.
  The deployed `src.claude_spot` now contains the Phase 36 alert formatter, and
  the timer is active/enabled.
- Live verification: the scheduled deployed cycles at `23:23:53 +03` and
  `23:26:03 +03` completed successfully with `status=held`,
  `submission_attempted=false`, and `telegram_sent=true` in the service output.
  Rendering the latest audit row through the deployed formatter produced the
  new `ELI5`, `Candidates`, `Submitted`, and `Why` fields.

Next action: continue monitoring normal 5-minute Claude Spot cycles.

## Phase 37 Small-Market Friction Cap Tuning - 2026-05-27 23:40 +03

- User reported that `candidate_routes=[]` was too shallow for the intended
  small-account risk profile. The immediate-friction cap was widened from
  `50` bps to `200` bps for Claude Spot small-market candidate generation.
- The change is intentionally limited to the hard candidate-offer ceiling:
  routes above `200` bps immediate fee/spread/slippage friction are still
  hidden from Claude. The market-evidence gate, break-even math, trade memory,
  Binance market rules, order-test preflight, local scope/fiat blocking,
  emergency stop, audit, reconciliation, and failed-route halt remain active.
- Updated the code/default profile, `.env.example`, `README.md`, `CODEX.md`,
  and `IMPLEMENTATION_PLAN.md`. Added a focused test that
  `ClaudeSpotSettings.from_env()` defaults to the new `200` bps cap when the
  environment does not override it.
- Deployment: refreshed the user-owned deployed copy, patched
  `/home/user/.local/share/binance-bot/.env` to
  `CLAUDE_SPOT_ALLOW_NEGATIVE_FRICTION_BPS=200`, and re-enabled
  `binance-claude-spot.timer`.
- Live verification: the next scheduled cycle completed successfully with
  `candidate_route_count=9`, `status=held`, and `submission_attempted=false`.
  The wider cap solved the empty-menu problem. Claude held because every route
  still failed the strict `100` bps market-evidence gate; the best cited route
  was `BONK -> LTC` with about `+16` bps expected edge after friction, below
  the live submission threshold.
- Verification: focused `.venv/bin/python -m pytest -q tests/test_claude_spot.py`
  passed (`36 passed`), full `.venv/bin/python -m pytest -q` passed
  (`333 passed`), and `git diff --check` passed. The timer is active and the
  service is inactive between scheduled runs.

Next action: monitor the next cycles. If the bot keeps seeing candidates but
holding, the next tuning decision is the evidence threshold, not the friction
candidate cap.

## Phase 38 Bps Retuning - 2026-05-28 00:07 +03

- User requested increasing both current bps settings by `100`. Updated the
  profile to `CLAUDE_SPOT_ALLOW_NEGATIVE_FRICTION_BPS=300` and
  `CLAUDE_SPOT_MARKET_EVIDENCE_MIN_EDGE_BPS=200`.
- The friction-cap increase is a looser candidate-generation setting. The
  evidence-minimum increase is a stricter live-submission setting: routes now
  need `200` bps expected edge after friction instead of `100` bps before a
  live order can be submitted.
- Updated code defaults, `.env.example`, `README.md`, `CODEX.md`, and
  `IMPLEMENTATION_PLAN.md`. Added/updated focused test coverage for the new
  environment defaults.
- Verification: focused `.venv/bin/python -m pytest -q tests/test_claude_spot.py`
  passed (`36 passed`), full `.venv/bin/python -m pytest -q` passed
  (`333 passed`), and `git diff --check` passed.
- Deployment: refreshed the user-owned runtime, patched
  `/home/user/.local/share/binance-bot/.env` to
  `CLAUDE_SPOT_ALLOW_NEGATIVE_FRICTION_BPS=300` and
  `CLAUDE_SPOT_MARKET_EVIDENCE_MIN_EDGE_BPS=200`, reloaded systemd, and
  re-enabled `binance-claude-spot.timer`.
- Live verification: the next scheduled cycle completed `held` with
  `candidate_route_count=9`, `submission_attempted=false`, and audit evidence
  packets showing `minimum_expected_edge_bps=200`. Claude held because all
  candidates were below the new stricter evidence gate; the best cited route
  was `BONK -> ERA` at about `+10.5` bps expected edge after friction.

Next action: monitor normal 5-minute cycles under the `300/200` bps profile.

## Phase 39 Lower Evidence Minimum - 2026-05-28 00:13 +03

- User requested lowering the strict market-evidence minimum to `50` bps.
  Updated the live profile to keep
  `CLAUDE_SPOT_ALLOW_NEGATIVE_FRICTION_BPS=300` while lowering
  `CLAUDE_SPOT_MARKET_EVIDENCE_MIN_EDGE_BPS=50`.
- This is a live-risk increase: candidates can still cost up to `300` bps
  immediately, but the selected route now only needs `50` bps expected edge
  after friction instead of `200` bps before the live evidence gate can pass.
- Updated code defaults, `.env.example`, `README.md`, `CODEX.md`, and
  `IMPLEMENTATION_PLAN.md`. Updated focused test coverage for the new
  environment default.
- Verification: focused `.venv/bin/python -m pytest -q tests/test_claude_spot.py`
  passed (`36 passed`), full `.venv/bin/python -m pytest -q` passed
  (`333 passed`), and `git diff --check` passed.
- Deployment: waited for the in-flight scheduled Claude Spot cycle to finish,
  refreshed the user-owned runtime, patched
  `/home/user/.local/share/binance-bot/.env` to
  `CLAUDE_SPOT_ALLOW_NEGATIVE_FRICTION_BPS=300` and
  `CLAUDE_SPOT_MARKET_EVIDENCE_MIN_EDGE_BPS=50`, reloaded systemd, and
  re-enabled `binance-claude-spot.timer`.

Next action: monitor the next scheduled cycle under the more aggressive
`300/50` bps profile.

## Claude Spot Partial Route Pause - 2026-05-28 18:14 +03

- Status check after the `300/50` profile found an active Claude Spot cycle.
  With the lower evidence gate, Claude selected `BONK -> HBAR` because several
  candidate routes now passed the evidence gate.
- The route partially executed: the `BONK/USDT` sell leg submitted, then the
  `HBAR/USDT` buy leg failed with Binance `-2010` insufficient balance. The
  wallet is now mostly `USDT` (`9.18000843` free at the time of inspection)
  with only dust-sized `BONK` remaining (`0.29` free).
- A follow-up scheduled cycle recorded a safety hold:
  `previous live route failed after submitting at least one order; holding until
  operator review`.
- Stopped `binance-claude-spot.timer` to prevent further scheduled cycles while
  the partial route and sizing issue are reviewed. The timer and service were
  both inactive after the pause.
- Important follow-up: the journal traceback exposed sensitive Binance request
  details/API metadata in local logs. Add a log-sanitization fix before
  re-enabling autonomous live cycles.

Next action: keep the Claude Spot timer stopped. Fix the multi-leg spend sizing
for second-leg buys after a first-leg sell, add tests for insufficient-balance
avoidance and audit consistency, sanitize exchange error logging, then run a
no-submit/evidence check before re-enabling the timer.

## Phase 40 Dynamic Route Execution Reserve - 2026-05-28 18:41 +03

- Implemented the partial-route hardening fix. Live buy-leg sizing now reads the
  authenticated per-symbol taker fee during final execution and reserves
  dynamic fee bps + `CLAUDE_SPOT_SLIPPAGE_BUFFER_BPS` +
  `CLAUDE_SPOT_LEG_ROUNDING_RESERVE_BPS`, floored by
  `CLAUDE_SPOT_MIN_LEG_QUOTE_RESERVE_BPS`, before Binance order-test and real
  submission.
- Added deployed defaults:
  `CLAUDE_SPOT_LEG_ROUNDING_RESERVE_BPS=5`,
  `CLAUDE_SPOT_MIN_LEG_QUOTE_RESERVE_BPS=25`, and
  `CLAUDE_SPOT_TELEGRAM_REASON_CHARS=0`.
- Fixed partial-route audit consistency: if any leg has already submitted and a
  later leg fails during dynamic fee read, order sizing, order-test, order
  submission, or reconciliation, the cycle writes one failed audit row with the
  already submitted legs preserved.
- Removed Telegram Why truncation. `CLAUDE_SPOT_TELEGRAM_REASON_CHARS=0` now
  means full Why text, and long alert reasons are split into Telegram-safe
  continuation messages instead of being cut.
- Hardened Binance failure logging so signed request tracebacks are not emitted
  with API keys, request headers, request bodies, signatures, timestamps, or
  secrets.
- Verification:
  `.venv/bin/python -m pytest -q tests/test_claude_spot.py tests/test_client.py`
  passed (`46 passed`), full `.venv/bin/python -m pytest -q` passed
  (`340 passed`), and `git diff --check` passed.
- Deployment: disabled the Claude Spot timer with
  `systemctl --user disable --now binance-claude-spot.timer`, refreshed the
  user-owned deployed copy with `scripts/setup_user_deploy.sh`, patched the
  deployed `.env`, and confirmed
  `binance-claude-spot.timer=inactive/disabled` and
  `binance-claude-spot.service=inactive`.
- Deployed evidence-only check completed without submitting any order. The
  wallet is still mostly `USDT` (`9.18000843` free in the check), candidate
  routes are visible, and `USDT -> HBAR` currently has evidence gate pass, but
  no Claude decision or live order ran because the check was evidence-only and
  the timer remains disabled.

Next action: review the deployed evidence context before any live re-enable. If
the timer is re-enabled, the new dynamic reserve applies to live buy execution,
but Claude can still choose an evidence-passing route under the current
`300/50` profile.

## Claude Spot Re-Enabled After Phase 40 - 2026-05-28 23:15 +03

- User requested making Claude Spot live again. The timer was first enabled, but
  the cycle correctly held because the prior partial `BONK -> HBAR` failed
  submitted route was still latched as requiring operator review.
- Added and deployed an explicit operator-review audit marker path:
  `python -m src.claude_spot --mark-failed-route-reviewed`. This clears only
  the reviewed failed-submission latch while keeping
  `CLAUDE_SPOT_HALT_AFTER_FAILED_SUBMISSION=true` active for future partial
  failures.
- Wrote the deployed review marker for failed route
  `BONK->HBAR:40772cb3797f` with reason: operator reviewed the partial route
  after the Phase 40 dynamic reserve fix and approved live re-enable.
- Verification after the marker change: focused
  `.venv/bin/python -m pytest -q tests/test_claude_spot.py` passed
  (`43 passed`), full `.venv/bin/python -m pytest -q` passed (`341 passed`),
  and `git diff --check` passed.
- Re-enabled `binance-claude-spot.timer`. It is now `active/enabled`, the
  service is inactive between runs, and the next trigger after the observed
  cycle was `2026-05-28 23:18:17 +03`.
- The first post-review live cycle ran normally, asked Claude, and completed
  `held` with `submission_attempted=false`. It did not hit the old failed-route
  latch. Claude held because all 9 supplied routes failed the strict evidence
  gate; the reason said no route cleared the `50` bps after-friction minimum
  and preserving the remaining `9.18 USDT` was the defensible action.

Next action: monitor the enabled 5-minute timer. Future cycles can submit if
Claude selects a route that passes the normal evidence, fee/friction, scope,
order-test, dynamic reserve, emergency-stop, and reconciliation gates.

## Claude Spot Binance Capital-Config Read Failure - 2026-05-29 09:31 +03

- Investigated the `2026-05-29 09:25:06 +03` Claude Spot failure. It happened
  before Claude was called and before any order submission. The failed request
  was Binance's signed wallet coin metadata endpoint
  `/sapi/v1/capital/config/getall`, reached by ccxt's default
  `load_markets() -> fetch_currencies()` path.
- The endpoint is unnecessary for Claude Spot route generation because the bot
  needs Spot market metadata/rules, balances, fees, and tickers, not wallet
  withdrawal/network coin configuration. A manual read-only balance probe
  immediately after the failure succeeded, so the incident was most consistent
  with a transient Binance/ccxt read dependency rather than a trade failure.
- Hardened `BinanceClient` by setting ccxt option `fetchCurrencies=false`,
  causing `load_markets()` to build markets without the signed capital-config
  call. This removes that avoidable failure point without changing live order
  submission, order-test, fee, market-rule, balance, evidence, emergency-stop,
  audit, or reconciliation gates.
- Verification: `.venv/bin/python -m pytest tests/test_client.py -q` passed
  (`5 passed`), `.venv/bin/python -m pytest tests/test_claude_spot.py -q`
  passed (`43 passed`), full `.venv/bin/python -m pytest -q` passed
  (`342 passed`), and the deployed read-only
  `python -m src.claude_spot --evidence-only` check succeeded with
  `candidate_routes=10` and no Claude call/order submission.
- Deployment: refreshed the user-owned runtime with
  `scripts/setup_user_deploy.sh`, confirmed deployed
  `src/exchange/client.py` contains `fetchCurrencies=false`, and re-enabled
  the `binance-claude-spot.timer` on the 10-minute cadence.

Next action: monitor the next scheduled Claude Spot timer cycle. If it fails
again, the failure should now be from a different endpoint/path; inspect the
new audit row and journal before changing trading behavior.

## Phase 41 Claude Spot Speculative Entry Mode - 2026-05-29 15:32 +03

- Implemented opt-in speculative live config:
  `CLAUDE_SPOT_SPECULATIVE_MODE_ENABLED`,
  `CLAUDE_SPOT_SPECULATIVE_MIN_EDGE_BPS`, and
  `CLAUDE_SPOT_SPECULATIVE_REQUIRE_THESIS`.
- Normal mode remains unchanged by default. When deployed/enabled, speculative
  mode can allow a selected route that fails the normal market-evidence minimum
  only if evidence is ready, expected edge after friction is above the
  speculative floor, and Claude's reason includes a concrete thesis such as
  reversal, breakout, volume anomaly, oversold mean reversion, or exit risk.
- Preserved the remaining live gates: Binance market rules, balances,
  authenticated dynamic fees/reserves, order-test preflight, local scope, fiat
  blocking, trade memory, emergency stop, failed-route halt, audit, and
  reconciliation.
- Added selected-route `speculative_gate` audit context plus Telegram
  speculative status/ELI5 details so allowed or blocked speculative decisions
  expose edge, floor, thesis, and reason details.
- Updated `CODEX.md`, `IMPLEMENTATION_PLAN.md`, `README.md`, and
  `.env.example` to document Phase 41 behavior and constraints.
- Verification: focused `.venv/bin/python -m pytest tests/test_claude_spot.py`
  passed (`49 passed`), full `.venv/bin/python -m pytest -q` passed
  (`348 passed`), and `git diff --check` passed.
- Deployment: disabled the Claude Spot timer, refreshed the user-owned runtime
  with `scripts/setup_user_deploy.sh`, patched the deployed `.env` with
  `CLAUDE_SPOT_SPECULATIVE_MODE_ENABLED=true`,
  `CLAUDE_SPOT_SPECULATIVE_MIN_EDGE_BPS=-1000`, and
  `CLAUDE_SPOT_SPECULATIVE_REQUIRE_THESIS=true`, then ran deployed
  `python -m src.claude_spot --evidence-only` successfully with no Claude call
  and no order submission.
- Re-enabled `binance-claude-spot.timer`. The immediate live cycle on the new
  deployment completed `held` with `submission_attempted=false`: Claude held
  HBAR because all candidate routes still had negative after-friction edge and
  HBAR had stronger current evidence. The timer is active/enabled and the next
  trigger observed was `2026-05-29 15:40:23 +03`.

Next action: monitor the next scheduled live cycles. Speculative mode can now
submit below the normal `50` bps evidence gate only when Claude chooses a route
with a concrete thesis and the route stays above the `-1000` bps speculative
floor; trade memory and the other live gates can still hold the cycle.

## Phase 42 Claude Spot Short-Window Evidence - 2026-05-29 15:56 +03

- Added minute-candle market evidence to Claude Spot. Each evidence asset now
  fetches the existing hourly window plus a `1m` short slice, then computes
  `short_returns_bps` for `5m` and `15m`.
- Added short evidence metadata to the prompt context:
  `short_returns_bps`, `short_status`, `short_reason`, `short_timeframe`,
  `short_candles`, `short_relative_momentum_bps`, and route-scope short-window
  labels.
- Included the `5m` and `15m` relative returns in
  `expected_edge_after_friction_bps` with modest weights, while preserving the
  existing `1h`, `4h`, `24h`, and `7d` evidence windows.
- Updated Claude's prompt to treat `5m`/`15m` as the current tape and not call
  an asset strongest purely from 24h return or volume when short windows are
  adverse unless it explains the after-fee thesis.
- Preserved live gates: Binance market rules, balances, dynamic fees/reserves,
  order-test preflight, route scope, fiat blocking, trade memory, emergency
  stop, failed-route halt, audit, and reconciliation.
- Verification: focused `.venv/bin/python -m pytest tests/test_claude_spot.py -q`
  passed (`50 passed`), full `.venv/bin/python -m pytest -q` passed
  (`349 passed`), and `git diff --check` passed.
- Deployment: disabled the Claude Spot timer, refreshed the user-owned runtime
  with `scripts/setup_user_deploy.sh`, and ran deployed evidence-only
  successfully. The evidence-only check showed live short windows flowing; for
  the first candidate, HBAR had `5m +53.5 bps` and `15m +45.7 bps`, while GLM
  had weaker short momentum.
- Re-enabled `binance-claude-spot.timer`. The immediate live cycle on the new
  deployment completed `held` with `submission_attempted=false`. Claude used the
  new short tape directly in its reason: HBAR was `5m +36.75 bps` and
  `15m +47.94 bps`, candidate routes were still negative after friction, and no
  concrete speculative thesis existed. The timer is active/enabled and the next
  trigger observed was `2026-05-29 16:04:20 +03`.

Next action: monitor cycles with the new short-window evidence. If HBAR or the
current holding starts printing negative `5m`/`15m` while another candidate has
better short tape and tolerable friction, Claude now has that information in the
decision context and the edge calculation.

## Telegram Command Bypass Version Review - 2026-05-29 17:32 +03

- Reviewed the local user-edited changes in `src.claude_spot`,
  `src.risk.guard`, `src.telegram_alerts`, and the two new top-level helper
  scripts.
- Fixed coding hygiene issues only: removed trailing whitespace/unused local
  imports and added `if __name__ == "__main__"` guards so the new helper
  scripts do not perform Binance/Telegram network calls during pytest
  collection.
- Verification before deployment: `git diff --check` passed,
  `.venv/bin/python -m py_compile src/claude_spot.py src/risk/guard.py
  src/telegram_alerts.py test_hbar_momentum.py test_telegram_commands.py`
  passed, focused
  `.venv/bin/python -m pytest -q tests/test_claude_spot.py
  tests/test_risk_guard.py tests/test_runtime.py` passed (`118 passed`), and
  full `.venv/bin/python -m pytest -q` passed (`349 passed`).
- Pre-deploy systemd check found `binance-claude-spot.timer` active/enabled and
  `binance-claude-spot.service` in a failed state from an external
  connectivity/DNS failure while fetching Binance exchangeInfo and sending
  Telegram, not from a local compile/test failure.
- Deployment: stopped the Claude Spot timer, refreshed the user-owned runtime
  with `scripts/setup_user_deploy.sh`, verified the deployed copy contains the
  new `override_bps_limits` and `/go_off` command code paths, reset the prior
  failed service state, and re-enabled `binance-claude-spot.timer`.
- The immediate deployed live cycle completed successfully with
  `status=held`, `submission_attempted=false`, `submitted_legs=0`, and
  `override_bps_limits=false`. Claude held HBAR because HBAR short tape was
  weak but every candidate destination was also weak and none justified paying
  the modeled immediate friction.
- Timer state after deployment: `binance-claude-spot.timer` is active/enabled
  and waiting for the next trigger at `2026-05-29 17:42:47 +03`; the service is
  inactive with `Result=success`.

Next action: monitor the next scheduled cycle under this deployed command-bypass
version.
