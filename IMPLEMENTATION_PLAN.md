# Binance Bot Implementation Plan

This plan expands the bot from the current single-symbol SMA runtime into a
better covered, more operational trading system. Each phase should land with
focused tests before the next phase starts.

## Progress

- 2026-05-25: Started phases 1 and 2 by extracting `BotRuntime`, preserving
  `run_bot()` and top-level import compatibility, adding runtime lifecycle
  tests, and aligning `.env.example` with the current settings parser.
- 2026-05-25: Completed phase 1 and phase 2 coverage: config validation,
  env-alias tests, dry-run/live-mode tests, runtime lifecycle injection tests,
  rollback/shutdown cleanup tests, and restart retry tests.
- 2026-05-25: Completed phase 3 by adding `MARKET_DATA_MODE`, WebSocket kline
  events, a WebSocket candle feed adapter, runtime feed-mode selection, and
  tests for malformed messages, candle normalization, duplicate closed candles,
  fallback polling mode, and reconnect-triggering server shutdown.
- 2026-05-25: Completed phase 4 by adding `TRADING_SYMBOLS`, per-symbol
  settings copies, one strategy/client pair per symbol, and tests proving
  independent feed subscriptions for multiple symbols.
- 2026-05-25: Completed phase 5 by adding `STRATEGY`, a registry-backed
  strategy loader, a demo SMA config adapter, and tests for strategy creation,
  unknown strategy names, and missing config builders.
- 2026-05-25: Completed phase 6 by adding opt-in `ORDER_UPDATE_MODE=websocket`,
  execution report routing into `OrderManager.apply_order_update`, strategy
  fill dispatch, duplicate fill suppression, and runtime execution-stream
  lifecycle tests.
- 2026-05-25: Completed phase 7 by adding portfolio snapshots, configurable
  fixed/percent/risk-based order sizing, min/max notional and balance guards,
  exchange precision/lot-size checks, runtime sizing wiring, rollback sizing
  bypass, and targeted config/sizing/runtime tests.
- 2026-05-25: Completed phase 8 by wiring expanded `RiskGuard` settings into
  runtime config, using structured portfolio-backed risk validation when those
  limits are configured, and adding runtime rejection/Telegram alert coverage for
  total exposure, open orders, minimum balance, leverage, drawdown, and price
  deviation.
- 2026-05-25: Completed phase 9 by rebuilding positions from persisted fills,
  reconciling restored active orders with Binance on startup, adding a recovery
  audit log, making corrupt SQLite startup failures explicit, and covering
  restore/restart/missing-database/corrupt-database paths.
- 2026-05-25: Completed phase 10 by hardening panic signaling for multiple bot
  PIDs, adding a status script, excluding runtime/secrets artifacts from deploy
  and Docker contexts, and adding shell syntax smoke checks.
- 2026-05-25: Completed phase 11 by adding an integration-style runtime test
  with a fake exchange, fake execution WebSocket, fake Telegram alerter, and
  temporary SQLite database.
- 2026-05-25: Replaced the BTC-specific live-sizing direction with phase 13:
  a paper-only modular asset pool that discovers the Binance spot universe,
  models dynamic market rules/fees/routes, and evaluates virtual allocations
  against configurable benchmarks before any future live allocation decision.
- 2026-05-26: Added phase 14 fast replay validation so pool mechanics,
  routing, fees, minimum-order gates, and scorer behavior can be exercised in
  seconds without weakening the live-readiness gate.
- 2026-05-26: Extended phase 14 with a single replay-suite script and a stress
  fixture covering spread/liquidity rejections and no-rebalance behavior.
- 2026-05-26: Added phase 15 as a manual one-shot live-order probe, separate
  from the disabled legacy bot and the paper-only pool allocator.
- 2026-05-26: Replaced the fixed month-long paper gate with a faster replay,
  live-market paper warm-up, permission-diagnostic, and tiny live-canary gate;
  added future phases for spot, margin, futures, transfers, unified capital
  allocation, and advanced Binance product modules.
- 2026-05-26: Completed phase 16 by adding API permission diagnostics, feature
  capability mapping, `-2015` guidance, withdrawal/future-module policy blocks,
  and a safe spot order-test guard before the manual live probe submits orders.
- 2026-05-26: Completed phase 17 as a foreground-only direct spot sell canary
  into `USDT`, with replay/warm-up confirmation gates, daily/order limits,
  emergency stop, safe order-test preflight, audit logging, reconciliation, and
  Telegram alerts.
- 2026-05-26: Completed phase 18 as a guarded foreground live spot allocator
  for one direct market order per invocation, with canary/replay/warm-up
  approvals, permission diagnostics, risk limits, benchmark valuation,
  reconciliation, audit logging, and Telegram alerts.
- 2026-05-26: Completed phase 23 as a local advanced product-module registry.
  Prediction, withdrawals, future Binance products, and live margin/futures/
  transfer activation are tracked with required interfaces, permission mapping,
  risk/audit contracts, tests, and disabled-by-default activation gates.
- 2026-05-26: Added phases 24 through 30 as the capability-completion roadmap
  for the remaining Binance API permissions: margin live canary, futures live
  canary, universal-transfer live canary, withdrawal safety, prediction dry run,
  symbol-whitelist/API-scope audit, and an advanced-product readiness review.
- 2026-05-26: Completed phase 27 as a dry-run withdrawal safety planner with
  local allowlist/cap/approval/cooldown/operator audit controls and no live
  withdrawal endpoint.
- 2026-05-26: Completed phase 28 as local/injected prediction-market dry-run
  modeling with stake, settlement, liquidity, drawdown, and emergency-stop
  controls and no prediction write endpoint.
- 2026-05-26: Completed phase 29 with operator-owned symbol/asset scope
  configuration and diagnostics, enforced on all foreground spot order
  commands before a submitted order can occur.
- 2026-05-26: Implemented plan-only readiness slices for phases 24 through 26.
  Margin, USD-M futures, and universal-transfer canary commands now perform
  foreground scope/risk/emergency-stop/audit checks, but every mutation flag
  still fails closed pending a later live-execution approval.
- 2026-05-26: Completed phase 30 as a local advanced-product readiness review
  and serial-canary policy gate; it cannot authorize live mutation or
  background automation.
- 2026-05-26: Added phase 31 for small-balance Spot decision visibility and
  faster historical strategy evaluation while the phase 13 live-paper gate
  continues unchanged.
- 2026-05-26: Added phase 32 for Claude-controlled autonomous Spot trading.
  This is a separate live runtime that replaces the unfinished phase 13 warm-up
  gate for Claude mode, uses the whole available Spot wallet, and allows
  dynamic routed Spot execution up to three serial legs per 15-minute action.
- 2026-05-27: Completed phase 33 by adding Claude Spot market evidence, trade
  memory, break-even math, a strict live evidence gate, and evidence-only
  inspection before any re-enable.
- 2026-05-27: Completed phase 35 read-only social-signal intelligence with
  authorized Telegram/X ingestion adapters, AI-capable classification,
  privacy-reduced audit rows, Claude Spot context enrichment, and
  manipulation-risk candidate blocking when explicitly enabled.
- 2026-05-27: Completed phase 36 by adding deterministic Telegram ELI5
  explanations for every Claude Spot decision status while preserving the full
  audit JSONL as the detailed source of truth.
- 2026-05-27: Completed phase 37 by widening Claude Spot's small-market
  immediate-friction cap from `50` bps to `200` bps so candidate generation is
  less shallow while the evidence and safety gates remain active.
- 2026-05-28: Completed phase 38 by increasing both live bps controls by
  `100`: candidate friction cap `200 -> 300` bps and strict evidence minimum
  `100 -> 200` bps.
- 2026-05-28: Completed phase 39 by lowering the strict evidence minimum from
  `200` bps to `50` bps while keeping the candidate friction cap at `300` bps.
- 2026-05-28: Completed phase 40 by hardening Claude Spot live route execution
  with dynamic buy-leg fee/slippage/rounding reserves, single-row partial-route
  failure audits, sanitized Binance failure logging, and full Telegram Why
  delivery through message splitting.

## 1. Stabilize Runtime Surface

- [x] Decide whether `src/simple_exchange.py` and `src/simple_strategy.py` stay as
  examples or get removed.
- [x] Finish configuration validation and expand `.env.example`.
- [x] Add tests for top-level `main.py` imports, config aliases, dry-run mode,
  testnet/live guard, and required environment variables.

## 2. Refactor Main Loop Into Runtime Object

- [x] Extract a `BotRuntime` or `TradingBot` class from `src/main.py`.
- [x] Own lifecycle operations explicitly: start, stop, rollback, restart, and
  cleanup.
- [x] Make market feed, strategy, order manager, risk guard, database, and alerter
  injectable.
- [x] Add tests for shutdown, rollback signal path, startup failure cleanup, and
  restart retry behavior.

## 3. Market Data Upgrade

- [x] Keep REST polling as a fallback.
- [x] Wire `src/exchange/websocket.py` into the main loop.
- [x] Add kline/candle WebSocket support if missing.
- [x] Support `MARKET_DATA_MODE=websocket|polling`.
- [x] Add tests for reconnect behavior, malformed messages, candle normalization,
  duplicate candle handling, and fallback mode.

## 4. Multi-Symbol Support

- [x] Change config from single `TRADING_SYMBOL` to list support, for example
  `TRADING_SYMBOLS=BTC/USDT,ETH/USDT`.
- [x] Run one strategy instance per symbol.
- [x] Keep per-symbol state isolated: price windows, positions, order sizing, and
  risk context.
- [x] Add tests proving two symbols produce independent signals and orders.

## 5. Strategy Framework

- [x] Keep `DemoSMAStrategy`, but formalize strategy loading.
- [x] Allow config such as `STRATEGY=demo-sma`.
- [x] Use the existing strategy registry in `src/strategies/base.py`.
- [x] Add tests for strategy factory loading, bad strategy names, and per-strategy
  config validation.

## 6. Execution And Order Updates

- [x] Keep REST order placement through `OrderManager`.
- [x] Wire user-data WebSocket execution reports into
  `OrderManager.apply_order_update`.
- [x] Feed fills back to active strategies through `on_fill`.
- [x] Add tests for execution report parsing, order state updates, position updates
  from fill deltas, and duplicate fill handling.

## 7. Dynamic Balance And Position Sizing

- [x] Current state: `BinanceClient.get_balance()` exists, and `RiskGuard` has a
  structured `PortfolioState`, but the main runtime does not use account
  balance to size orders dynamically.
- [x] Add a portfolio snapshot service that reads balances, open orders, positions,
  realized daily PnL, unrealized daily PnL, and total exposure.
- [x] Add order sizing modes:
  - `fixed`: current `ORDER_SIZE` behavior.
  - `quote_percent`: spend a percentage of available quote balance.
  - `equity_percent`: size from total account equity.
  - `risk_percent`: size from maximum allowed loss or exposure per trade.
- [x] Add config such as `ORDER_SIZE_MODE`, `ORDER_SIZE_PERCENT`,
  `MIN_ORDER_NOTIONAL_USD`, `MAX_ORDER_NOTIONAL_USD`, and `MIN_BALANCE_USDT`.
- [x] Round quantities to exchange precision and reject orders below Binance minimum
  notional or lot size.
- [x] Add tests for balance snapshots, sizing calculations, min/max clamps,
  precision rounding, insufficient balance handling, and dry-run sizing.

## 8. Risk Coverage Expansion

- [x] Current runtime uses max position size, daily loss limit, and duplicate-order
  checks. (Daily loss limit removed in Phase 44.)
- [x] Wire additional existing `RiskGuard` capabilities into runtime config:
  maximum total exposure, maximum open orders, minimum balance, allowed
  leverage, daily drawdown, and maximum price deviation. (Daily drawdown
  removed in Phase 44.)
- [x] Add tests for every configured risk rule and Telegram alerts on rejection.

## 9. Persistence And Recovery

- [x] Restore orders, positions, and daily loss on startup. (Daily loss
  persistence removed in Phase 44.)
- [x] Add a recovery audit log for restored positions, active orders, and risk
  state.
- [x] Reconcile locally active orders with Binance on startup.
- [x] Add tests for database restore, restart with existing positions, missing
  database creation, and corrupt database handling.

## 10. Operational Tooling

- [x] Make `scripts/panic.sh` robust against multiple PIDs.
- [x] Add `scripts/status.sh` or a CLI status command.
- [x] Improve deployment so `.env`, virtualenvs, logs, and database files are not
  copied accidentally.
- [x] Add shell smoke checks where practical.

## 11. Coverage Gate

- [x] Keep all existing unit tests green.
- [x] Add integration-style tests with fake exchange, fake WebSocket, fake
  Telegram, and temporary SQLite databases.
- [x] Cover:
  - [x] config parsing
  - [x] runtime lifecycle
  - [x] market data ingestion
  - [x] strategy signals
  - [x] order placement and updates
  - [x] dynamic balance sizing
  - [x] risk rejection
  - [x] persistence restore
  - [x] Telegram alert paths

## 12. Live Operations Hardening

- [x] Add an operator runbook for status, alerts, panic rollback, backup,
  restore, deployment, monitoring, and runtime-mode rollback.
- [x] Add verified online SQLite backup/restore tooling and a daily systemd
  backup timer retaining the latest 14 valid backups.
- [x] Add a read-only exchange/connectivity check with opt-in Telegram delivery
  verification.
- [x] Alert on terminal runtime failures and enabled WebSocket stream failures,
  with configurable cooldown-based suppression of repeated alerts.
- [x] Verify locally, back up the live database, deploy, and enable
  `MARKET_DATA_MODE=websocket` and `ORDER_UPDATE_MODE=websocket`.
- [x] Monitor post-deploy service, WebSocket, alert, and database health; retain
  a documented mode rollback path.
- [x] Migrate the root-managed live service to a user-owned systemd service so
  routine deployments, backups, monitoring, and restarts no longer require
  `sudo`.

## 13. Modular Asset Pool And Paper Allocation

- [x] Disable the fixed-size live SMA runtime and reject future real-money
  startup when its sizing mode is `fixed`.
- [x] Add a paper-only pool runtime with a separate SQLite virtual ledger seeded
  from read-only account holdings.
- [x] Discover the active Binance spot universe and persist conservative
  market-eligibility decisions based on current metadata, prices, spread, and
  USDT-valued liquidity.
- [x] Evaluate every asset represented by eligible markets by default, with an
  optional scoring cap only for documented operational resource control.
- [x] Build bounded asset-graph routing across eligible markets, supporting up
  to three conversion legs and deriving precision/minimum-order constraints,
  slippage, and authenticated commission estimates dynamically per route.
- [x] Add configurable benchmark reporting for `USDT`, `BTC`, `PAXG`, and
  `XAUT`, with initial equal-weight basket evaluation.
- [x] Add independent paper scorer portfolios for simple momentum and
  risk-adjusted momentum, with allocation, turnover, drawdown, reserve, and
  cooldown gates.
- [x] Persist paper evidence with bounded growth: latest full catalog,
  per-cycle eligibility counts, rolling scorer observations, and historical
  portfolio snapshots/decisions.
- [x] Add a user-owned paper service/status path and documentation/tests proving
  that pool evaluation does not submit real orders.
- [ ] Pass the replay suite, live-market paper warm-up, permission diagnostics,
  and tiny live-canary readiness review before defining any separately approved
  live pool phase.

## 14. Fast Pool Replay Testing

- [x] Add a deterministic replay command that runs many paper-pool cycles
  quickly against fixture market data.
- [x] Keep replay ledgers isolated from `data/pool-paper.db` and reject the
  production paper-ledger filename for replay runs.
- [x] Add fixture coverage for benchmark valuation, dynamic fee reads, routed
  reallocations, tiny-balance minimum-order holds, and no live exchange order
  path.
- [x] Add a one-command replay-suite wrapper that runs the default fixture set
  into isolated `/tmp` databases.
- [x] Add stress replay coverage for wide-spread and low-liquidity market
  rejection behavior.
- [x] Document that replay proves mechanics and speeds strategy iteration but
  does not by itself approve live-money pool execution.

## 15. Manual One-Shot Live Probe

- [x] Add a foreground-only live probe command requiring
  `--confirm-real-order`, `USE_TESTNET=false`, `DRY_RUN=false`, and
  `CONFIRM_LIVE_TRADING=yes`.
- [x] Select exactly one direct `ASSET/USDT` market sell from free balances,
  preferring `BNB/USDT`, using Binance market minimums and precision.
- [x] Submit at most one real market sell per invocation, with no automatic
  second order after an ambiguous exchange failure.
- [x] Persist a JSONL audit record and send a Telegram alert labeled
  `LIVE PROBE` after a submitted probe order.
- [x] Keep live pool allocation and background live services out of scope.

## 16. API Permission And Capability Registry

- [x] Add a Binance permission/capability inspector for reading, spot trading,
  margin, futures, universal transfer, withdrawals, prediction trading, symbol
  whitelist, and IP restriction status where Binance exposes it.
- [x] Map every bot feature to required API capabilities and fail closed before
  runtime startup or order submission when a permission is missing.
- [x] Convert Binance permission/IP failures such as `-2015` into clear
  diagnostics with current mode, endpoint class, and next operator action.
- [x] Keep withdrawals disabled by policy even if the API key exposes withdrawal
  permission, unless a future withdrawal-specific safety phase is approved.
- [x] Add tests for capability parsing, feature-to-permission mapping, fail-closed
  startup behavior, and `-2015` diagnostics.

## 17. Spot Pool Live Canary

- [x] Add a separate spot live-canary execution module without adding order
  submission to the existing paper-only `src.pool` runtime.
- [x] Require the replay suite, live-market paper warm-up, permission diagnostics,
  explicit confirmation, and configured canary limits before a canary order can
  be submitted.
- [x] Size every canary order from current balances, market rules, liquidity,
  spread, precision, minimum notional, slippage, and authenticated fee estimates.
- [x] Enforce max order notional, max daily notional, max daily order count,
  emergency stop, no automatic retry after ambiguous exchange failures, and
  foreground-only operation for the first canary version.
- [x] Persist a full audit trail for proposed orders, route quotes, balance
  snapshots, fee estimates, exchange responses, reconciliation, and Telegram
  canary alerts.
- [x] Add tests for small-account sizing, minimum-order rejection, fee/slippage
  rejection, insufficient balance, permission failure, audit persistence, and
  stop conditions.

## 18. Full Live Spot Pool Execution

- [x] Promote spot canary mechanics into a controlled live spot allocator only
  after canary evidence is reviewed and explicitly approved.
- [x] Keep exposure, turnover, liquidity, drawdown, daily loss, order count,
  reserve, and route-risk limits configurable and enforced before every order.
- [x] Reconcile live balances, open orders, fills, fees, and virtual intent after
  each allocation cycle.
- [x] Continue benchmark reporting against `USDT`, `BTC`, `PAXG`, `XAUT`, and a
  configurable basket while live spot allocation is active.
- [x] Add tests for allocator gating, reconciliation drift, order lifecycle
  recovery, emergency stop, and benchmark/audit reporting.

## 19. Margin Trading Module

- [x] Add isolated and cross-margin account discovery without enabling margin
  order submission by default.
- [x] Model borrow limits, debt, interest, repayment, margin level, and
  liquidation-risk checks before any margin strategy can trade.
- [x] Require explicit margin config, permission registry approval, dry-run/paper
  validation, and separate user approval before live margin activation.
- [x] Add tests for borrow sizing, interest accrual, repayment, liquidation-risk
  rejection, permission gating, and disabled-by-default behavior.

## 20. Futures Trading Module

- [x] Add USD-M futures support first; treat COIN-M futures as a later extension.
- [x] Model leverage, margin mode, funding rates, liquidation distance,
  reduce-only orders, futures precision/minimums, and position reconciliation.
- [x] Require futures-specific risk limits, paper/replay validation, permission
  registry approval, and separate user approval before live futures activation.
- [x] Add tests for leverage limits, funding impact, liquidation-risk rejection,
  reduce-only behavior, reconciliation, and disabled-by-default behavior.

## 21. Universal Transfer And Capital Router

- [x] Add dry-run internal transfer planning across spot, funding, margin, and
  futures wallets, with transfer availability and account-type constraints.
- [x] Require explicit approval before any automatic capital movement between
  account types.
- [x] Audit every proposed and submitted transfer with source wallet, destination
  wallet, asset, amount, reason, and resulting allocation state.
- [x] Add tests for transfer permission gating, dry-run planning, account
  constraints, audit persistence, and hard stops on unexpected balances.

## 22. Unified Asset Pool Across Account Types

- [x] Extend pool valuation to include spot balances, margin equity/debt,
  futures collateral, open PnL, interest, funding, reserves, and transfer
  availability.
- [x] Support pool-value benchmarks in `USDT`, `BTC`, `PAXG`, `XAUT`, and a
  configurable basket.
- [x] Add allocation constraints by asset, product type, account type, leverage,
  liquidity, volatility, and reserve requirements.
- [x] Add tests for unified valuation, benchmark conversion, debt/PnL handling,
  allocation constraints, and account-type isolation.

## 23. Advanced Binance Product Modules

- [x] Track prediction trading and future Binance products as optional modules,
  not as automatically enabled capabilities.
- [x] Require each product module to define its own strategy interface,
  permission mapping, market-data source, risk model, audit trail, tests, and
  activation gate.
- [x] Keep a policy rule that API-key permission alone never authorizes a bot
  feature to run.
- [x] Add tests for disabled-by-default behavior and capability-gated activation
  when each advanced module is implemented.

## 24. Margin Live Canary

- [x] Add a foreground-only plan-only margin canary that uses the existing
  read-only/dry-run margin model before any live margin write is possible.
- [x] Enforce debt caps, borrow caps, repayment intent, minimum margin level,
  liquidation buffer, emergency stop, and explicit confirmation flags.
- [ ] Audit borrow, repay, order/transfer intent, before/after balances, debt,
  interest, margin level, reconciliation state, and Telegram alert delivery.
- [x] Keep live margin borrow, repay, order, and transfer calls unavailable until
  this phase is implemented, tested, reviewed, and explicitly approved.
- [ ] Add tests for permission gating, disabled defaults, confirmation flags,
  emergency stop, risk-limit rejection, audit persistence, and fail-closed
  exchange errors.

## 25. USD-M Futures Live Canary

- [x] Add a foreground-only plan-only USD-M futures canary that uses the existing
  read-only/dry-run futures model before any live futures write is possible.
- [x] Enforce max notional, max leverage, margin-mode policy, liquidation
  distance, maintenance-buffer, funding-rate, reduce-only exit-plan, emergency
  stop, and explicit confirmation flags.
- [ ] Audit leverage, margin mode, planned order, reduce-only exit plan, funding
  snapshot, before/after positions, reconciliation state, and Telegram alert
  delivery.
- [x] Keep live futures order, leverage-change, margin-mode, position-mode, and
  position-margin calls unavailable until this phase is implemented, tested,
  reviewed, and explicitly approved.
- [ ] Add tests for permission gating, disabled defaults, confirmation flags,
  emergency stop, leverage/liquidation rejection, reduce-only behavior, audit
  persistence, and fail-closed exchange errors.

## 26. Universal Transfer Live Canary

- [x] Add a foreground-only plan-only internal-transfer canary that uses the existing
  read-only/dry-run transfer planner before any live universal transfer is
  possible.
- [x] Enforce source/destination wallet activation, transferable balance,
  transfer cap, margin-debt block, isolated-symbol requirements, emergency stop,
  and explicit confirmation flags.
- [ ] Audit proposed transfer, submitted transfer, source/destination balances,
  route type, reason, before/after allocation state, reconciliation state, and
  Telegram alert delivery.
- [x] Keep live universal-transfer submission unavailable until this phase is
  implemented, tested, reviewed, and explicitly approved.
- [ ] Add tests for permission gating, disabled defaults, confirmation flags,
  emergency stop, balance/debt rejection, audit persistence, reconciliation, and
  fail-closed exchange errors.

## 27. Withdrawal Safety Module

- [x] Keep withdrawals hard-blocked while adding local withdrawal safety inputs
  and dry-run withdrawal planning.
- [x] Model address allowlists, network selection, daily caps, per-request caps,
  approval state, cooldown windows, emergency stop, and operator identity audit.
- [x] Require a future withdrawal-specific activation phase before any withdrawal
  endpoint can be called.
- [x] Add tests for hard-blocked live submission, allowlist validation, cap
  rejection, approval-state rejection, audit persistence, and permission
  diagnostics.

## 28. Prediction Trading Dry-Run Module

- [x] Add a dry-run prediction-trading module scaffold using local or injected
  read-only prediction market payloads until a verified safe endpoint exists.
- [x] Model market selection, stake caps, settlement windows, settlement risk,
  drawdown limits, liquidity/availability checks, emergency stop, and audit
  requirements.
- [x] Keep live prediction trading unavailable until a separate activation phase
  defines write endpoints, risk controls, tests, and explicit approval gates.
- [x] Add tests for disabled defaults, permission gating, market-data parsing,
  stake/risk rejection, audit output, and fail-closed unavailable-data behavior.

## 29. Symbol Whitelist And API-Key Scope Audit

- [x] Add local/operator symbol-whitelist configuration because Binance does not
  expose the full API-key whitelist through the safe diagnostics.
- [x] Provide audit coverage for configured trading symbols, paper-pool route
  universe, canary symbols, margin/futures symbols, transfer assets,
  withdrawal assets, and prediction markets; enforce it on foreground spot
  order commands before submission.
- [x] Report API-key scope mismatches with actionable diagnostics while keeping
  unknown Binance-side whitelist state fail-closed for live features.
- [x] Add tests for allow/deny matching, missing whitelist config, route-universe
  filtering, canary rejection, and diagnostic rendering.

## 30. Advanced Product Readiness Review

- [x] Add a cross-product readiness checklist that mirrors the phase 13 gate for
  advanced products: focused tests, full tests, replay, dry-run reports,
  permission diagnostics, canary plans, audit review, and explicit approval.
- [x] Require live canary execution to be serial across transfer, margin, and
  futures so balance, debt, exposure, fees, and reconciliation remain
  attributable to one action at a time.
- [x] Keep all advanced-product automation disabled until each product has passed
  its own readiness review and a separate activation phase approves background
  behavior.
- [x] Add tests or static checks that advanced products cannot be enabled by API
  permission alone or by background service configuration.

## 31. Spot Decision Visibility And Historical Evaluation

- [x] Protect local `.env` backup artifacts from accidental Git inclusion and
  document that credential material must stay out of commits.
- [x] Add a read-only paper-pool report that explains warm-up progress, holdings,
  latest decision reasons, current eligible/rejected markets, and scored
  candidates when sufficient observations exist.
- [x] Add an isolated historical-candle replay path using public Spot candles so
  strategy choices can be evaluated quickly without altering the deployed paper
  ledger or weakening the live-readiness gate.
- [x] Document and test the new analysis commands, while keeping first live Spot
  execution restricted to the existing separately approved tiny-canary process.
- [ ] Let the deployed Phase 13 warm-up complete without replacing its evidence
  with historical research, then review its ranked and simulated decision
  results through the read-only report.
- [ ] Only after that review, make a separate explicit small-account Spot
  activation decision with a narrow symbol scope and tiny caps; Phase 31 does
  not approve or activate live trading.
- [ ] Before that activation decision, quantify whether Binance route minimums
  force a trade above the approved share of this small account; do not loosen
  allocation caps merely to produce a trade.

## 32. Claude Autonomous Spot Brain

- [x] Add a headless Claude Code adapter that uses schema-validated JSON output
  and includes the configured money-maker mission prompt in every decision.
- [x] Add a separate `src.claude_spot` live Spot runtime so `src.pool` remains
  paper-only while Claude can choose `hold` or one routed `rebalance` action.
- [x] Let Claude mode consider dynamically eligible Spot routes, control the
  whole available Spot wallet, and execute one selected route of up to three
  serial Spot order legs per 15-minute cycle; default to aggressive
  small-market discovery with up to 10 scored candidates and one route quote per
  candidate, while keeping `liquid_top_value` available as a rollback mode.
- [x] Add Claude Spot small-market candidate scoring and audit metadata covering
  quote volume, spread, liquidity tier, route legs, immediate friction, and the
  reason each candidate was offered.
- [x] Remove Phase 13 warm-up, canary caps, daily notional/order limits,
  turnover limits, allocation percentage caps, and drawdown gates from
  Claude-mode authorization while keeping exchange-validity invariants.
- [x] Enforce non-overridable Binance market rules, free balance, fee handling,
  Binance lot-size step sizing, Spot permission/order-test preflight,
  configured dynamic scope, emergency-stop, audit, reconciliation, and
  halt-on-failed/ambiguous leg behavior.
- [x] Add Claude Spot local route-scope filters so public markets that the
  account cannot submit against, such as IDR-quoted Spot symbols, are removed
  before Claude sees candidates and rechecked before live submission.
- [x] Block traditional fiat assets in Claude Spot routes by default so dynamic
  routing stays on crypto/stablecoin Spot paths unless an operator deliberately
  disables the fiat guard.
- [x] Treat Claude decision timeouts as no-order holds and make the user timer
  self-schedule after activation so a restart does not require forcing an
  immediate live service run.
- [x] Add anti-churn trade discipline for Claude Spot: hard immediate-friction
  filtering, submitted-trade cooldown, daily submitted-trade cap, and
  hold-after-failed-submission behavior.
- [x] Add a user-owned oneshot service and 15-minute timer for Claude Spot; keep
  it disabled unless the deployed environment explicitly enables Claude live
  mode.
- [x] Add tests for Claude schema parsing, prompt construction, dynamic multi-hop
  route selection, plan-only audit, live gate rejection, serial route
  execution, and halted-route behavior.
- [x] Run a real local Claude structured-response probe and Binance Spot
  permission/order-test preflight in the deployed environment before enabling
  the `binance-claude-spot.timer`.
- [x] Enable the Claude Spot timer only after `CLAUDE_SPOT_ENABLED=true`,
  `CLAUDE_SPOT_DYNAMIC_SCOPE=all_eligible_spot`, `USE_TESTNET=false`,
  `DRY_RUN=false`, and `CONFIRM_LIVE_TRADING=yes` are intentionally set.

## 33. Claude Spot Market Evidence And Memory Gate

- [x] Add compact market evidence to every Claude Spot candidate covering
  `1h`, `4h`, `24h`, and `7d` returns, volatility, volume change,
  required break-even move, and expected edge after route friction.
- [x] Add Claude Spot trade memory from the audit JSONL, including recent
  submitted routes, open/current holding context, and estimated closed or open
  PnL where it can be reconstructed.
- [x] Add a strict live-only evidence gate so selected routes cannot submit
  unless evidence is present, the expected edge clears the configured minimum,
  and recent Claude outcomes do not trigger a loss-streak hold.
- [x] Add a plan-only evidence inspection command/path that prints the context
  without submitting orders or requiring Claude to decide.
- [x] Keep the existing anti-churn, fiat-block, permission/order-test,
  emergency-stop, audit, reconciliation, and no-advanced-product-write
  invariants active.
- [x] Add focused tests for evidence metrics, audit memory, fail-closed live
  holds, plan-only evidence reporting, and live submission only when the strict
  evidence gate passes.

## 34. Five-Minute Claude Spot Fee-Aware Live Profile

- [x] Keep the Phase 33 compact evidence windows (`1h`, `4h`, `24h`, `7d`) and
  dynamic per-leg fee/friction packet in every candidate context.
- [x] Make the live profile call Claude on a 5-minute timer and give Claude up
  to 300 seconds to return one schema-validated Spot decision.
- [x] Disable the fixed cooldown and fixed daily submitted-trade cap for Claude
  Spot by setting both controls to `0`, while keeping the failed-route halt,
  emergency stop, scope, permission/order-test, friction, market-evidence,
  break-even, trade-memory, audit, and reconciliation gates active.
- [x] Update the Claude prompt to discourage fee churn and require an
  after-fee, after-spread, after-slippage case before rotating the wallet.
- [x] Add tests for disabled fixed limits, prompt wording, and 5-minute timer
  deployment metadata.

## 35. Social Signal Intelligence For Claude Spot

- [x] Add a read-only social-signal module with disabled-by-default settings,
  JSONL audit persistence, Telegram Bot API parsing, official X API parsing,
  symbol extraction, heuristic labeling, and optional headless-Claude
  classification without tools.
- [x] Normalize permitted social posts into privacy-reduced records with hashed
  author/channel identifiers, short excerpts, mentioned assets, label,
  confidence, manipulation risk, and engagement metadata where available.
- [x] Summarize stored social records into per-asset evidence covering mention
  count, source diversity, bullish/bearish balance, manipulation risk, and
  recent excerpts.
- [x] Feed social evidence into Claude Spot candidate context when
  `SOCIAL_SIGNAL_ENABLED=true`; if manipulation risk exceeds the configured
  threshold and blocking is enabled, remove the affected route before Claude
  can select it.
- [x] Keep social signals additive only: they cannot bypass Binance market
  rules, fee/friction, market-evidence, trade-memory, emergency-stop,
  order-test, audit, or reconciliation gates.
- [x] Add tests for Telegram/X parsing, privacy reduction, evidence summary,
  AI classification wiring, Claude context enrichment, and manipulation-risk
  candidate blocking.

## 36. Telegram Decision Explanations For Claude Spot

- [x] Add deterministic ELI5 Telegram explanations for every Claude Spot cycle
  status: held, planned, submitted, failed, and blocked.
- [x] Explain hold decisions with the concrete reason path, including no
  candidates, evidence rejection, timeout, failed-route halt, trade discipline,
  or Claude's own hold reason.
- [x] Include concise structured fields in the Telegram alert for status,
  action, route, candidates, submission state, explanation, reason, and audit
  path.
- [x] Keep alert formatting safe and concise with HTML escaping and a
  configurable reason-length limit, while leaving the full detail in the audit
  JSONL.
- [x] Add focused tests for decision explanations and HTML escaping without
  changing any live trading gate or prompt behavior.

## 37. Claude Spot Small-Market Friction Cap Tuning

- [x] Raise the default/deployed
  `CLAUDE_SPOT_ALLOW_NEGATIVE_FRICTION_BPS` profile from `50` bps to `200` bps
  so small-account candidate generation can offer routes with realistic
  small-market spread, slippage, and multi-leg costs.
- [x] Keep the widened cap finite and hard: routes above `200` bps immediate
  friction remain hidden from Claude before decision time.
- [x] Preserve the existing live gates: market evidence, break-even, trade
  memory, Binance market rules, order-test, local scope, fiat blocking,
  emergency stop, audit, reconciliation, and failed-route halt.
- [x] Update docs/tests so future sessions know this is a deliberate
  small-market risk profile, not a removal of fee discipline.

## 38. Claude Spot Bps Retuning

- [x] Increase `CLAUDE_SPOT_ALLOW_NEGATIVE_FRICTION_BPS` from `200` bps to
  `300` bps so candidate generation can tolerate another `100` bps of
  immediate small-market fee/spread/slippage friction.
- [x] Increase `CLAUDE_SPOT_MARKET_EVIDENCE_MIN_EDGE_BPS` from `100` bps to
  `200` bps as requested. This is stricter for live submission, so it must be
  treated as a deliberate higher-conviction gate rather than a loosening.
- [x] Keep all non-negotiable live gates active: Binance market rules, balance
  checks, order-test preflight, local scope, fiat blocking, trade memory,
  emergency stop, failed-route halt, audit, and reconciliation.
- [x] Update docs/tests and deploy the same bps values to the active
  user-owned runtime.

## 39. Claude Spot Lower Evidence Minimum

- [x] Lower `CLAUDE_SPOT_MARKET_EVIDENCE_MIN_EDGE_BPS` from `200` bps to `50`
  bps so routes with smaller after-friction expected edge can pass the strict
  live evidence gate.
- [x] Keep `CLAUDE_SPOT_ALLOW_NEGATIVE_FRICTION_BPS=300` so route candidates
  remain bounded by a finite immediate fee/spread/slippage ceiling.
- [x] Preserve the remaining live gates: Binance market rules, balance checks,
  order-test preflight, local scope, fiat blocking, trade memory, emergency
  stop, failed-route halt, audit, and reconciliation.
- [x] Update docs/tests and deploy the same evidence minimum to the active
  user-owned runtime.

## 40. Claude Spot Dynamic Route Execution Reserve

- [x] Size live buy legs with an authenticated per-symbol taker-fee reserve plus
  configured slippage and rounding reserve before order-test and submission.
- [x] Add `CLAUDE_SPOT_LEG_ROUNDING_RESERVE_BPS` and
  `CLAUDE_SPOT_MIN_LEG_QUOTE_RESERVE_BPS` so the reserve is dynamic but still
  has a small execution floor.
- [x] Preserve partial-route audit truth in one failed row whenever any leg has
  already submitted and a later leg fails preflight, order-test, submission, or
  reconciliation.
- [x] Add an explicit operator-review audit marker so a reviewed partial-route
  failure can be cleared without disabling failed-submission halt globally.
- [x] Remove Telegram Why truncation by making
  `CLAUDE_SPOT_TELEGRAM_REASON_CHARS=0` mean full text and splitting long
  Telegram messages instead of cutting the reason.
- [x] Sanitize Binance exchange failure logs so signed request headers, bodies,
  signatures, timestamps, and secrets are not emitted in journal tracebacks.
- [x] Keep the Claude Spot timer stopped after the partial-route incident until
  the operator explicitly reviews and re-enables live cycles.

## 41. Claude Spot Speculative Entry Mode

- [x] Add opt-in speculative live config:
  `CLAUDE_SPOT_SPECULATIVE_MODE_ENABLED`,
  `CLAUDE_SPOT_SPECULATIVE_MIN_EDGE_BPS`, and
  `CLAUDE_SPOT_SPECULATIVE_REQUIRE_THESIS`.
- [x] Keep normal mode unchanged by default. When speculative mode is enabled,
  allow a selected route to miss the normal market-evidence edge minimum only
  if evidence is ready, expected edge after friction is above the configured
  speculative floor, and Claude's reason includes a concrete speculative thesis
  when thesis enforcement is enabled.
- [x] Preserve all non-negotiable live gates: Binance market rules, balances,
  authenticated dynamic fees/reserves, order-test preflight, local scope, fiat
  blocking, trade memory, emergency stop, failed-route halt, audit, and
  reconciliation.
- [x] Record speculative gate state in selected-route audit context and Telegram
  alerts so allowed/blocked speculative decisions show edge, floor, thesis, and
  reason details.
- [x] Update docs/tests and deploy the approved speculative profile to the
  active user-owned runtime only after focused and full verification pass.

## 42. Claude Spot Short-Window Market Evidence

- [x] Fetch minute-candle evidence for every source/destination asset already in
  Claude Spot market evidence and compute `5m` and `15m` returns as
  `short_returns_bps`.
- [x] Feed `short_returns_bps`, short evidence status, short timeframe, and short
  relative-momentum summary into the selected candidate context without removing
  existing `1h`, `4h`, `24h`, or `7d` evidence.
- [x] Include `5m` and `15m` relative momentum in
  `expected_edge_after_friction_bps` with modest weights so a current dump can
  affect live route approval instead of being hidden behind 24h/7d momentum.
- [x] Update Claude's prompt to treat `5m`/`15m` as the current tape and to avoid
  calling an asset strongest purely from 24h return or volume when short windows
  are adverse without an explicit after-fee thesis.
- [x] Preserve all live gates: Binance market rules, balances, dynamic
  fees/reserves, order-test preflight, route scope, fiat blocking, trade memory,
  emergency stop, failed-route halt, audit, and reconciliation.

## 43. X Pre-Pump Signal Insighter (Camoufox + Haiku)

Goal: catch coins in the minutes before a social-driven burst, with Claude
Haiku as an analyst that scores X chatter, not as a trader. Execution is a
separate, later gate. Reuse the `bb-signals` ledger / snapshotter / weekly
report pattern and the `bb-social` record shape. Prior evidence to beat
(`docs/signals/audit-2026-06-27.md`): V1 signals arrived after the move,
Solana median -92.5% after detection, 45.3% rug rate, and idealized paper
(`sniper-paper`) turned into -$697 once friction was modeled.

Gate 0, decisions before code:

- [x] Operator approval to collect from X with Camoufox browser automation
  (2026-10-13). `CODEX.md` now carves the Phase 43 collector out of the
  Phase 35 "no browser automation for X" rule: read-only, burner account, no
  posting or engaging. The operator owns the X Terms of Service risk,
  burner-account bans and login challenges.
- [x] Pick the target venue first, because it decides the market join:
  Binance Spot-listed coins (tradeable through the existing executor) or
  on-chain Solana launches (DexScreener / Jupiter; needs a Solana wallet the
  bot cannot fund, since withdrawals stay hard-blocked). Operator decision
  (2026-10-13): both, paper-scored side by side. Each token resolves to
  whichever market exists, every report and the gate are split per venue, and
  the execution phase still picks one venue.
- [x] Pre-register the success criteria and friction model in
  `docs/signals/x-insighter-design.md` before collecting data.

Collector:

- [ ] Python sidecar `collectors/x-camoufox/` (uv project; Camoufox is
  Python/Playwright only) in its own container and compose profile
  `x-collector`, outside the `bb` image (the browser is ~650 MB). Pin a
  known-good Camoufox release and verify the `lin.arm64` binary on the Pi 5
  (64-bit OS; the 16 KB-page kernel may need `kernel8.img`). Built and pinned
  (`camoufox[geoip]==0.5.8`, browser 156.0.1-beta.36; image built and
  launched on amd64, 2026-10-13). Open: arm64 build and run on the Pi 5.
- [x] Sources: a curated account list (callers, KOLs, launchpad trackers) plus
  cashtag and contract-address searches. Dedicated burner account with its
  session on the data volume; jittered polling, a hard pages-per-hour cap,
  backoff on login walls and rate limits, and a heartbeat for the dashboard.
- [x] Append-only `data/x-posts.jsonl` in `bb-social`'s privacy-reduced shape:
  hashed author id (public handle kept only for curated accounts), excerpt,
  cashtags, Solana/EVM contract addresses, author follower count and account
  age, post time, first-seen time, and every visible engagement meter:
  replies, reposts, quotes, likes, bookmarks and views.
- [x] Engagement re-sampling: re-read the counters of each focused coin's top
  posts on a schedule (e.g. +5m, +15m, +30m, +1h, +3h) into
  `data/x-engagement.jsonl`, so the meters become velocities instead of one
  snapshot.
- [x] Reply and quote sampling for focused coins: collect the newest replies
  and quote posts under their top posts (capped per post and per hour), with
  each replier's hashed id, follower count and account age, for the reply
  quality checks below.

Features:

- [x] Rust aggregator in `bb-signals` that turns posts into per-token windows:
  mention velocity and acceleration (5m vs 1h baseline), unique authors,
  curated-author hits, engagement velocity, first-seen time, and coordination
  flags (near-duplicate text, fresh accounts, same-minute bursts).
- [x] Resolve each token to a market (Binance symbol, or Solana mint via
  DexScreener) and attach a detection-time snapshot: price, liquidity,
  volume, FDV, pair age, 5m/1h change, so "already pumped" is visible.

Haiku insighter:

- [x] Claude Haiku through omp RPC with no tools, run only on candidate
  windows that pass deterministic pre-filters, never on every post. Strict
  JSON out: token, catalyst type, narrative summary, traction stage,
  exhaustion flags, shill / bot / coordination risk, rug red flags,
  conviction 0-1, reasons.
- [x] Post text is untrusted data in the prompt (prompt injection via tweets).
- [x] Hard daily cost cap `X_INSIGHTER_MAX_DAILY_LLM_COST_USD` (default 0.50);
  log tokens and cost per call.
- [x] Append each scored candidate to `data/x-signals.jsonl`: detection id,
  features, market snapshot, Haiku verdict, prompt version.

Traction, focus and exhaustion:

- [x] Traction score per coin, recomputed every window and kept as history in
  `data/x-traction.jsonl`. Social side: mention velocity and acceleration,
  unique-author growth, spread from small to large accounts, curated-author
  hits, and the engagement meters as velocities: reply, repost, quote, like,
  bookmark and view growth per post and per coin. Weight replies, quotes and
  bookmarks above likes and views (they take more effort to fake), and
  discount meters from fresh or follower-less accounts. Market side: price,
  volume and liquidity trend, plus holder growth for Solana mints when
  cheaply available.
- [x] Reply quality, scored by Haiku on the sampled replies: organic
  (questions, real discussion, distinct repliers) vs bot / shill ("LFG",
  rocket spam, copy-paste, fresh accounts) vs warnings ("rug", "honeypot",
  "can't sell", "dev sold"). Report the organic share and any warnings per
  coin; a high organic share strengthens traction, a high bot share does not
  count toward it.
- [x] Haiku labels the traction stage on every re-score: `ignition` (first
  credible mentions), `building` (accelerating, broadening authors), `peak`
  (euphoria, saturation) or `fading`, with the reasons it saw. Stage changes
  are logged.
- [x] Focus escalation: coins move watch -> focus -> hot as traction rises.
  Focused coins get targeted cashtag / contract searches every few minutes,
  1-minute market snapshots and a Haiku re-score on material change; they
  drop back when traction fades. Cap the focused set (e.g. 5 coins) and its
  Haiku spend so attention goes to the strongest few.
- [x] Exhaustion ("too much traction") detector as the assumed peak: mention
  velocity turning down after a spike, engagement saturating (likes and
  reposts flattening while views keep climbing, replies-to-likes shifting
  toward complaints or argument), late-stage spread (large generic accounts,
  "buy now" and price-target hype, giveaway spam), a rising bot / shill share
  in posts and replies, rug or "can't sell" warnings appearing in replies,
  price going vertical while volume or buy pressure weakens, and on-chain rug
  precursors where available (liquidity pulled, LP unlocked, dev or
  top-holder wallets selling, concentration rising). Output an exhaustion
  score with the flags that triggered it.
- [x] Deterministic exit rules, with Haiku as one input: scale out as the
  exhaustion score crosses thresholds (e.g. half at the first, the rest at
  the second), exit in full on any rug precursor, and keep a 24h time stop
  as the backstop (no hard or trailing stops, operator decision 2026-10-14).
  "Assumed peak" means the exhaustion signal; the real peak is only known
  afterwards.

Evaluation, paper only:

- [x] Snapshot every x-signal with the V2 snapshotter at +5m, +15m, +1h, +6h,
  +24h, +3d and +7d (price, liquidity, pair existence).
- [x] Weekly report: lead time (signal before or after the move), hit rate
  (+X% before -Y% within 6h), rug rate, path returns, and friction-adjusted
  paper P&L under scheduled exits and rule exits (exhaustion scale-out, rug
  precursor, time stop). "Sold at the peak" is shown only as an upper bound,
  never as expected return.
- [x] Ablation: deterministic features alone vs features plus Haiku's
  verdict, so Haiku has to earn its cost.
- [x] Exit quality: peak capture ratio (realized gain / best gain available
  from entry), how often the exhaustion exit fired before vs after the actual
  peak, how often it got out before the rug proxy, and exhaustion exits vs a
  24h-hold (`hold_24h`) baseline on the same signals.
- [ ] Pre-registered gate: at least 4 weeks and 100 scored signals; positive
  friction-adjusted P&L under rule exits that survives dropping the top two
  outliers; rug rate below V1's 45.3%; median lead time of at least 10 minutes
  before a +30% move; exhaustion exits beat the 24h-hold baseline
  after friction. Friction: the audit's >= 7.2% round trip for ~$15 on
  Solana, or authenticated taker fees plus spread for Binance-listed coins.
  The weekly report computes this verdict per venue; it stays open until the
  data passes it (needs `X_INSIGHTER_FREEZE_DATE` set after burn-in).

Dashboard:

- [x] `cryptobot-gui` Signals tab: live candidates with Haiku's take, the
  focused coins with their traction curve, stage changes and exhaustion
  score, collector health, hit and rug rates, the weekly report and the
  paper equity curve.

Execution, separate approval after the gate:

- [ ] Only after the gate passes and the operator approves: a capped
  executor for the chosen venue, either Binance Spot through the existing
  order-test / reconciliation / stop-file path, or on-chain Solana through
  `bb-sniper` with its own wallet, per-trade and daily caps and an emergency
  stop. Haiku never places orders; the deterministic entry and exhaustion /
  rug / time exit rules do.

Non-goals: private chats or DMs, posting or engaging from the account,
coordinating or promoting pumps, and any change to Claude Spot.

## 44. No Fixed Exit Levels Or Loss Caps (operator decision 2026-10-14)

The live Claude Spot trader was opening and closing positions within minutes
on self-set "invalidation" prices, paying roughly 75-110 bps of fees and
spread per round trip. The operator ordered fixed exit levels and loss caps
out of the selected modules.

- [x] Claude Spot prompt: theses carry why, expected move and horizon, no
  invalidation price; the system prompt states there are no stop-loss or
  take-profit levels and that a small move is not by itself a reason to exit
  (guidance, not a gate). Prompt version `claude-spot-v5-agentic`.
- [x] X insighter paper exits: hard and trailing stops removed; the
  `trailing_only` baseline arm is now `hold_24h`.
- [x] Legacy runtime (`bb main`): daily loss limit and daily drawdown limit
  removed with their daily-loss persistence.
- Not selected by the operator, so unchanged: listing sniper/detector stops
  and the mean reversion stop/target (paper or disabled), and every Claude
  Spot hard-safety item in `CODEX.md`.

## 45. Claude-Chosen Exit Plans Between Cycles (operator approval 2026-10-14)

Claude Spot looks at the market once per cycle (240 s); a coin can rise and
fall again in between. Claude picks a sell plan for each position from a fixed
menu, and a watcher enforces it between cycles. Claude never writes code: the
menu is fixed, deterministic Rust, and Claude only fills in its parameters.
"Max win" cannot be guaranteed; each plan is a bet on when a move is over, so
every step is proven on data before it touches money.

Step 1, replay (research only, no order path):

- [x] Exit-plan menu evaluator in `bb-trader` (`exit_plan`), money in
  `Decimal`: follow the peak (sell all once price falls X% below the highest
  price since entry, X fixed or a multiple of the coin's normal 1-minute
  swing), take part (sell a fraction once at +X%), momentum fade (after being
  up at least X%, sell when no new high for N minutes), deadline (sell after N
  minutes). Bars are evaluated pessimistically (low before high); a tick is a
  one-price bar, so the live watcher can reuse it unchanged.
- [x] `bb claude-spot-exit-replay`: pair real round trips from the Claude Spot
  audit, fetch public klines from entry to entry + horizon, run a grid of
  plans, and compare each with Claude's actual exit and with holding to the
  horizon, net of the real fee rate plus modelled exit slippage.
- [x] Run it on the live audit and record the result in `PROGRESS.md`
  (2026-10-14: 9 round trips; no plan made money, see `PROGRESS.md`).

Step 2, paper watcher (after the operator reviews the replay):

- [x] `set_exit_plan` host tool (menu parameters only, validated, paper
  only) and `bb claude-spot-exit-watch`: follows the bid between cycles,
  logs would-be sells, no orders; the cycle prompt shows each plan's
  results; `--summary` compares "Claude with the plan" and "Claude alone".
- [ ] Run it for about a week next to the live trader and review
  `bb claude-spot-exit-watch --summary`.
- [x] Research the math traders use for exits (2026-10-15) and keep every
  candidate in `docs/exit-plan-candidates.md`.
- [x] Menu expansion from that research: ATR trail / Chandelier exit
  (`atr=<k>:<1m|5m|15m|1h>`, replaces the 1-minute swing multiple),
  stretch above the 20-minute upper Bollinger band (`stretch=<z>:<fraction>`)
  and slowdown, still rising but slowing 3 minutes in a row
  (`slow=<fraction>:<minutes>`); `set_exit_plan`, replay and watcher updated.
- [x] Expanded menu deployed with the live step below.

Step 3, live (operator decision 2026-10-15: live now, without the paper week):

- [x] Sells through `Runtime::exit_sell` (cycle lock, arming, stop file,
  failed-route halt, scope, order/test, filters, reconciliation, audit row
  before alert) from an exit guard that runs between cycles in the
  `claude-spot` daemon (`CLAUDE_SPOT_EXIT_PLANS_LIVE`); default plan
  `stretch=2:1,atr=3:1h` for positions opened from
  `CLAUDE_SPOT_DEFAULT_EXIT_PLAN_FROM`; partial sells keep the position open;
  failed sells before any order retry up to 3 times.
- [ ] Review after about a week: live plan sells vs Claude's own exits.

Step 4, one fixed plan (operator decision 2026-10-16, after the 67-plan
backtest on all 3 audits: `atr=3:1h,stretch=2:1` lost least, and Claude's
deadline-only plans had been switching it off; the operator chose "only
this plan, locked" over layering Claude's rules on top):

- [x] With a default plan set, the watcher and live exit guard run it
  unchanged on every position opened from
  `CLAUDE_SPOT_DEFAULT_EXIT_PLAN_FROM` and ignore Claude's plan book.
- [x] `set_exit_plan` is left out of the tool catalogue and refuses calls
  while a default plan is set (still offered without one, paper trial).
- [x] System prompt names the fixed plan and its rules in plain words, says
  Claude cannot change it; dashboard wording; prompt version v8.
- [x] Tests: a Claude plan is ignored and the default still sells; the tool
  is withdrawn and refuses without writing the plan book.
- [x] Deploy to the Pi (image rebuild, `CLAUDE_SPOT_PROMPT_VERSION=
  claude-spot-v8-agentic` in the Pi's `.env.docker`, recreate `claude-spot`).

Step 5, locked rules with Claude's numbers (operator decision 2026-10-17;
replaces the fixed default plan of step 4):

- [x] `CLAUDE_SPOT_EXIT_PLAN_RULES` (menu keys, `.env.docker` `atr,stretch`):
  every plan sets exactly those rules; Claude picks the numbers (menu
  limits only) once per position; `set_exit_plan` shows only those fields,
  all required, and refuses other rules, `clear` and a second plan for the
  same position. No plan = no automatic sell. `CLAUDE_SPOT_DEFAULT_EXIT_PLAN`
  and `_FROM` removed.
- [x] Watcher and live guard ignore book plans with other rules.
- [x] System prompt, tool text, dashboard Exits tab, docs; prompt v9.
- [x] Deploy to the Pi (image rebuild, `.env.docker`
  `CLAUDE_SPOT_EXIT_PLAN_RULES=atr,stretch`, default-plan vars removed,
  `CLAUDE_SPOT_PROMPT_VERSION=claude-spot-v9-agentic`, recreate
  `claude-spot`).

Step 6, fee floor and feedback (operator decision 2026-10-17, after the ZEC
stretch sale at +6 bps gross, a net loss):

- [x] `stretch` fires only when its line is above break-even: entry ×
  (1 + fee) / (1 − fee) at the trip's fee rate (watcher, live guard, replay).
- [x] The cycle prompt lists the 5 newest closed positions with a plan: the
  rule that sold, share, trigger price, closing price, hold time and net
  result after fees (watch state `recent_closed`).
- [x] Trade outcomes (prompt and `get_trade_history`) mark exit-plan sells
  with the plan, rule, share and trigger price (`exit_plan`); prompt v10.
- [x] Deploy to the Pi (`CLAUDE_SPOT_PROMPT_VERSION=claude-spot-v10-agentic`).

Step 7, old-gold-way (operator decision 2026-10-17, branch `old-gold-way`):
the fixed plan (step 4) and locked rules (step 5) cut winners short (v7: 10
trips, mean hold 68 min, +7.5% summed; v8-v10: 9 trips, 30 min, -4.1%), so
the v7 system comes back.

- [x] Trader and dashboard exit-plan code of the v7 system (default plan,
  Claude's `set_exit_plan` override with the full menu incl. deadline); the
  v7 system prompt (tag `claude-spot-v11-agentic`); step 6 fee floor and
  feedback kept; `set_exit_plan` reply says LIVE or paper.
- [x] Trader session restored to the v7 conversation up to the last
  gpt-6.1-sol cycle before Sonnet took over (cycle 542, 2026-10-16 05:02
  UTC; 7394 of 7885 lines) and the journal cut at the same point (476 of
  582 notes); staged on the Pi in `docker-data/archive/old-gold-staging`.
- [x] Deploy: image from the branch, `.env.docker` v7 exit settings, swap
  in the staged session and journal between cycles with no open position.

## 46. Dashboard On The Pi's Live Data (operator request 2026-10-15)

`cryptobot-gui` started on the laptop showed empty History and Claude tabs:
the trader writes its files on the Pi, and the page said "history was reset"
instead of saying it had no data. The dashboard stays read-only and
localhost-only.

- [x] `scripts/cryptobot_gui_pi.sh`: rsync mirror of the Pi's data dir
  (no omp logins, Claude session or Telegram session), refreshed every 30 s,
  trader paths and the non-secret exit-plan settings mapped from the Pi's env
  file; the dashboard runs against the mirror. The `cryptobot-gui` fish
  command uses it.
- [x] `/api/status` reports whether the audit file exists and when it last
  changed; the page names the missing path and the launcher instead of empty
  states, and the header shows how old the trader's data is.
- [x] Claude tab: running/idle from the observed cycle cadence (not a fixed
  4 minutes), a brains card (cycles, failures and cost per model, fallback
  state, tool use) and expandable cycles with the brain, fallback attempts,
  tokens, cost, prompt version and every tool call with its arguments.
- [x] Exits tab (`/api/exits`): mode and default plan, each watched position
  with its rules in plain words, sell line, bid, peak and fills, plans
  waiting for the watcher, trades closed by a plan and the watch log; unreadable
  state or plan files are shown as errors. History marks trades closed by an
  exit plan, the overview shows the open position's sell line.
- [x] History P&L from the exchange fills (cost, filled quantity and fees of
  each order) instead of the trader's pre-trade quote, which counted the
  planned spend; fees of both orders subtracted, quote fallback for routes
  without USDT-quoted fills.

## Post-Implementation Handoff

Roadmap phases 1 through 11 are implemented and locally verified. Future
sessions should use `PROGRESS.md` for active handoff state and only update this
roadmap when planned scope changes or a roadmap status needs correction.

Current handoff state:

- Review/commit hygiene, deployment, and initial live monitoring are complete.
- Phase 12 was added as new operational-hardening scope before implementation;
  it is not incomplete work from phases 1 through 11.
- Phase 12 is complete: the live runtime now runs as a user service with both
  WebSocket modes active, signed user-data subscriptions, and scheduled
  verified local backups.
- Phase 13 implementation replaces live trading with a paper-only modular pool.
  The old live user unit is stopped/disabled and fixed-size live startup is
  rejected in code. Live allocation remains intentionally out of scope pending
  a replay, live-market paper warm-up, permission-diagnostic, and tiny-canary
  readiness gate.
- Phase 14 adds fast replay validation for the paper pool. It is a development
  and regression tool only; replay by itself does not approve live allocation.
- Phase 15 adds a manual one-shot live-order probe only. It is not a live pool
  allocator and must not be run as a background service.
- Phase 16 adds permission diagnostics and a safe spot order-test guard before
  the manual live probe submits orders.
- Phase 17 adds a foreground-only tiny spot canary. The first version is limited
  to one direct sell into `USDT`; it is not continuous pool allocation.
- Phase 18 adds a guarded foreground live spot allocator. The first version is
  limited to one direct spot market order per invocation; it is not a background
  continuous allocator.
- Phase 19 adds read-only margin discovery and local borrow/repay risk modeling
  only. Live margin borrow, repay, order, and transfer endpoints remain disabled
  until a separate activation phase is designed, tested, and approved.
- Phase 20 adds read-only USD-M futures discovery and local order-risk modeling
  only. Live futures order, leverage-change, margin-mode, position-mode, and
  position-margin endpoints remain disabled until a separate activation phase is
  designed, tested, and approved.
- Phase 21 adds read-only wallet discovery and local universal-transfer planning
  only. Live transfer submission remains disabled until a separate activation
  phase is designed, tested, and approved.
- Phase 22 adds local unified account-type valuation and allocation-constraint
  reporting only. It does not add order, margin, futures, or transfer
  automation.
- Phase 23 adds a local advanced product-module registry and gate report only.
  Prediction trading, withdrawals, future Binance products, and live
  margin/futures/transfer activation remain disabled until separate
  product-specific activation phases are designed, tested, and explicitly
  approved.
- Phases 27 and 28 add dry-run/local-only withdrawal safety and prediction risk
  modeling; neither exposes a live write endpoint.
- Phase 29 adds local symbol/asset scope diagnostics and gates all existing
  foreground spot order commands against the operator-owned whitelist.
- Phases 24 through 26 now include plan-only canary commands. Their live
  submission/reconciliation/alert activation work remains open and no
  account-mutating path exists.
- Phase 30 adds local readiness-evidence and serial-canary policy reporting;
  it does not enable advanced-product automation.
- Phase 31 is read-only Spot decision observability and isolated public-candle
  historical evaluation; it must not shorten or bypass Phase 13 operational
  approval.
- Phase 32 adds Claude-controlled autonomous Spot execution as a new exception:
  Claude mode no longer waits for Phase 13 paper warm-up, may use the whole
  available Spot wallet, and may execute up to three serial Spot legs per cycle
  when explicitly enabled. Margin, futures, transfer, withdrawal, and
  prediction writes remain outside this phase.
