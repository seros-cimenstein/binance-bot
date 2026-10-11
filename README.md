# Binance Trading Bot

A Binance trading fleet written in Rust: a native Binance REST/WebSocket client,
a Claude-driven agentic Spot trader, systematic strategies (funding arb, trend
filter, mean reversion), a capital pool with a Claude allocator, a listing
sniper, Solana launch sniping, signal collectors, and research backtests. All
of it ships as one binary, `bb`.

## Layout

| Crate | Purpose |
|---|---|
| `bb-core` | env config, `Decimal` money helpers, fsync'd JSONL audit, Telegram alerts (persisted 429 cooldown), run locks, stop files |
| `bb-binance` | native Binance client: HMAC/Ed25519 signing, time sync, rate-limit tracking, exchange rules, spot/wallet/convert/margin/USD-M futures, fills, klines history, WebSocket streams, feeds |
| `bb-omp` | omp RPC client (Claude sessions with persistent context + host tools) |
| `bb-trader` | `claude-spot`: the Claude agentic Spot trader |
| `bb-pool` | paper asset pool, market catalog, multi-leg route engine, commissions, replays, reports |
| `bb-strategies` | funding arb, trend filter, mean reversion, all backtests, `spec-snapshot` |
| `bb-capital` | capital pool + Claude allocator, auto-convert, fleet status, cash distributor |
| `bb-listing` | listing-announcement detector + listing sniper (flat / trailing / momentum) |
| `bb-products` | API permissions, scope audit, margin/futures/transfer/withdrawal/prediction modelling, canaries, live probe |
| `bb-runtime` | legacy strategy runtime (`main`, demo SMA, risk guard, order manager), `ops-check` |
| `bb-signals`, `bb-sniper`, `bb-social`, `bb-polymarket`, `bb-airdrop` | DexScreener/Telegram/smart-money signals, Solana pump.fun sniper, social evidence, Polymarket scanner, airdrop scheduler |
| `bb-cli` | the `bb` binary |

Money (prices, quantities, balances, fees, PnL) is `rust_decimal::Decimal`
end to end; `f64` is used only for statistics.

## Build and test

```bash
cargo build --release -p bb-cli          # -> target/release/bb
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
scripts/pool_replay.sh                   # deterministic paper-pool fixture replay
```

Copy `.env.example` to `.env` and fill in your keys. Never commit `.env` or
`.env.*` files. `bb` loads `.env` from the working directory; process
variables win. Logs go to stderr (`LOG_LEVEL`, `RUST_LOG`, `LOG_FORMAT=json`);
stdout is reserved for command output.

## Commands

Run `bb --help` or `bb <command> --help`. Every former `python -m src.<module>`
entry point is now `bb <module-in-kebab-case>` with the same flags, env vars,
defaults and data files.

| Area | Commands |
|---|---|
| Claude trader | `claude-spot` |
| Live / scheduled strategies | `funding-arb`, `trend-filter`, `mean-reversion`, `listing-detector`, `listing-sniper` |
| Capital | `capital-pool`, `auto-convert`, `bot-status`, `cash-distributor` |
| Paper pool | `pool`, `pool-report`, `pool-replay`, `pool-history-replay`, `pool-unified` |
| Products / diagnostics | `api-permissions`, `scope-audit`, `product-modules`, `advanced-readiness`, `margin`, `margin-canary`, `futures`, `futures-canary`, `transfers`, `transfer-canary`, `withdrawals`, `prediction`, `spot-canary`, `live-spot-allocator`, `live-probe`, `ops-check` |
| Legacy runtime | `main` |
| Signals / Solana / misc | `signals-*`, `sniper-*`, `social-signal`, `polymarket-scanner`, `airdrop-scheduler` |
| Research | `*-backtest`, `spec-snapshot` |

## Claude agentic Spot trader (`bb claude-spot`)

Claude trades the Spot wallet through omp (the oh-my-pi coding agent, npm
`@oh-my-pi/pi-coding-agent`) in RPC mode:

- **Persistent context.** One long-lived omp session per trader, stored in
  `CLAUDE_SPOT_SESSION_DIR`, resumed after restarts and crashes. omp
  auto-compaction stays on, and the bot also compacts at
  `CLAUDE_SPOT_COMPACT_AT_PERCENT`. A durable journal
  (`CLAUDE_SPOT_JOURNAL_PATH`) holds Claude's theses across compactions.
- **Tools, not text.** omp runs with every built-in tool disabled except
  `web_search` (`CLAUDE_SPOT_WEB_SEARCH=true`, the default), so Claude has live
  web search plus the bot's host tools: `get_portfolio`, `list_routes`,
  `get_market_evidence`, `get_klines`, `get_social_evidence`, `get_trade_history`,
  `execute_route`, `journal_read` / `journal_write`, and `end_cycle`. No shell or
  filesystem access. `execute_route` is the only order path. Web results are
  treated as untrusted data in the system prompt.
- **No trading guardrails.** Fees, spread, slippage, momentum, evidence, social
  data and trade memory are information for Claude to weigh. None of them blocks
  a trade. The old edge, friction, speculative, loss-streak, cooldown,
  daily-cap, proven-edge and rules-engine gates are gone. Their env vars are
  ignored, with one warning listing them.
- **Hard safety (always on, fail closed):**
  - live arming: `CLAUDE_SPOT_ENABLED=true`, `CLAUDE_SPOT_DYNAMIC_SCOPE=all_eligible_spot`, `USE_TESTNET=false`, `DRY_RUN=false`, `CONFIRM_LIVE_TRADING=yes`;
  - the emergency stop file (`CLAUDE_SPOT_STOP_PATH`), checked at cycle start, before every route, before every leg and before every order;
  - an exclusive run lock;
  - account READING plus canTrade;
  - Binance `order/test` before every leg;
  - exchange filters, with orders sized from live balances net of a fee, slippage and rounding reserve;
  - per-leg fill reconciliation, halting on anything ambiguous;
  - a failed-route halt, cleared only by `bb claude-spot --mark-failed-route-reviewed`;
  - operator scope (allow/block lists, blocked quotes, fiat blocking);
  - Spot MARKET orders only, at most `CLAUDE_SPOT_MAX_ROUTE_LEGS` legs.
- **Audit.** Each route or cycle is one fsync'd row in `CLAUDE_SPOT_AUDIT_PATH`,
  written before any Telegram alert. It uses the Python schema plus agent
  fields: tool calls, tokens and cost.

```bash
bb claude-spot --evidence-only                 # decision context, no Claude, no orders
bb claude-spot --plan-only                     # Claude decides; execute_route only records plans
bb claude-spot --submit-live --daemon          # live, one cycle every --interval-seconds (240)
bb claude-spot --mark-failed-route-reviewed    # clear the failed-route halt after review
```

Cost control (not a trading gate): `CLAUDE_SPOT_MAX_DAILY_LLM_COST_USD` (0 = unlimited).
Choose the model and thinking level with `CLAUDE_SPOT_MODEL` (default
`anthropic/claude-haiku-5-5`; always use provider-qualified ids) and
`CLAUDE_SPOT_THINKING` (default `high`).

omp needs Claude credentials: either `ANTHROPIC_API_KEY`, or a one-time
interactive `omp` login (stored under `PI_CODING_AGENT_DIR`).

## Capital pool, Claude allocator, auto-convert

`bb capital-pool` shows the plan: a reserve (`CAPITAL_RESERVE_PCT`), per-module
min/max (`CAPITAL_MIN/MAX_MODULE_ALLOCATION_USD`), and score-weighted splits
from ledger scorecards or lifetime PnL. `--advise` asks the Claude allocator,
an omp session with fleet/scorecard/balance tools. Its proposal can only clamp
within the deterministic bounds; any failure falls back to the deterministic
plan.

With `CAPITAL_POOL_ENABLED=true` the `main` runtime requests a lease before
every buy. With `AUTO_CONVERT_TO_QUOTE=true` it market-sells
`AUTO_CONVERT_ASSETS` to cover a quote shortfall, after an `order/test`.
`bb auto-convert --plan-only` previews that conversion. `bb bot-status` and
`bb cash-distributor` report fleet health and an advisory distribution.

## Listing sniper

`bb listing-detector --detect-only` polls Binance listing announcements and
arms a shared watchlist (`LISTING_WATCHLIST_PATH`). `bb listing-sniper` diffs
exchangeInfo against known symbols plus the watchlist and fires flat, trailing
or momentum round trips, in paper (`--paper`) or live (`--submit-live`) mode. A
live fire needs `LISTING_SNIPER_ENABLED=true`, `CONFIRM_LIVE_LISTING_SNIPER=yes`
and `LISTING_SNIPER_PROVEN_EDGE=yes`, and is bounded by
`LISTING_SNIPER_NOTIONAL_USDT` and `LISTING_SNIPER_DAILY_SPEND_CAP_USDT`.

## Research and diagnostics

```bash
bb pool --once
bb pool-report --db-path ~/.local/share/binance-bot/data/pool-paper.db
bb pool-history-replay --balance BNB=0.0169745 --symbol ETH/USDT --symbol SOL/USDT \
  --start 2026-05-20T00:00:00Z --end 2026-05-26T00:00:00Z --db-path /tmp/history-replay.db
bb spec-snapshot                      # data/speculation/<UTC date>/ CSVs + SUMMARY.md
bb api-permissions --feature claude_spot_autonomous
bb scope-audit --symbol BNB/USDT --asset BNB
bb funding-arb                        # plan-only by default
bb claude-spot-backtest --help
```

Historical replay is for strategy research only: it uses public candles and
modelled costs. The margin, futures, transfer, withdrawal and prediction
modules model and plan only; their live writes are not implemented.

## Deployment

Docker is the primary target (Raspberry Pi 5 / ARM64 and x86_64). The image
contains `bb`, `omp` (on Bun), supercronic and sqlite3.

```bash
docker compose build
docker compose run --rm -it claude-spot omp             # one-time Claude login (or set ANTHROPIC_API_KEY)
docker compose run --rm -it signals-telegram bb signals-telegram-setup   # one-time Telegram user login
docker compose --profile all up -d
touch docker-data/claude-spot.stop                      # emergency stop for the Claude trader
```

Profiles:
- `live`: the Claude trader, the live momentum listing sniper and the scheduler.
- `paper`: paper and signal workloads plus the scheduler.
- `all`: both of the above.
- `v2`: the launch detector.
- `main-bot`: the legacy SMA runtime.

Periodic jobs live in `docker/crontab`. systemd user units for non-Docker
hosts are in `deploy/user/`, installed by `scripts/setup_user_deploy.sh`. See
`OPERATIONS.md` for backup, restore and verification procedures.
