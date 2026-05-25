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
  checks.
- [x] Wire additional existing `RiskGuard` capabilities into runtime config:
  maximum total exposure, maximum open orders, minimum balance, allowed
  leverage, daily drawdown, and maximum price deviation.
- [x] Add tests for every configured risk rule and Telegram alerts on rejection.

## 9. Persistence And Recovery

- [x] Restore orders, positions, and daily loss on startup.
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

## Post-Implementation Handoff

The roadmap phases are implemented and locally verified. Future sessions should
use `PROGRESS.md` for active handoff state and only update this roadmap when
planned scope changes or a roadmap status needs correction.

Current handoff focus:

- Review and commit hygiene for the modified and untracked files.
- Live-readiness checks before running real funds: testnet/dry-run validation,
  Binance permissions, environment values, Telegram routing, systemd deployment,
  status/panic scripts, and database persistence paths.
