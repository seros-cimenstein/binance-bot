# Binance Trading Bot

A Binance trading fleet written in Rust: a native Binance REST/WebSocket client,
a bot-driven agentic Spot trader, systematic strategies (funding arb, trend
filter, mean reversion), a capital pool with a bot allocator, a listing
sniper, Solana launch sniping, signal collectors, and research backtests. All
of it ships as one binary, `bb`, next to a read-only web dashboard,
`cryptobot-gui`.

## Layout

| Crate | Purpose |
|---|---|
| `bb-core` | env config, `Decimal` money helpers, fsync'd JSONL audit, Telegram alerts (persisted 429 cooldown), run locks, stop files |
| `bb-binance` | native Binance client: HMAC/Ed25519 signing, time sync, rate-limit tracking, exchange rules, spot/wallet/convert/margin/USD-M futures, fills, klines history, WebSocket streams, feeds |
| `bb-omp` | omp RPC client (LLM sessions with persistent context + host tools) |
| `bb-trader` | `bot-spot`: the agentic Spot trader |
| `bb-pool` | paper asset pool, market catalog, multi-leg route engine, commissions, replays, reports |
| `bb-strategies` | funding arb, trend filter, mean reversion, all backtests, `spec-snapshot` |
| `bb-capital` | capital pool + bot allocator, auto-convert, fleet status, cash distributor |
| `bb-listing` | listing-announcement detector + listing sniper (flat / trailing / momentum) |
| `bb-products` | API permissions, scope audit, margin/futures/transfer/withdrawal/prediction modelling, canaries, live probe |
| `bb-runtime` | legacy strategy runtime (`main`, demo SMA, risk guard, order manager), `ops-check` |
| `bb-signals`, `bb-sniper`, `bb-social`, `bb-polymarket`, `bb-airdrop` | DexScreener/Telegram/smart-money signals, Solana pump.fun sniper, social evidence, Polymarket scanner, airdrop scheduler |
| `bb-cli` | the `bb` binary |
| `cryptobot-gui` | read-only localhost web dashboard (`cryptobot-gui` binary) |

Money (prices, quantities, balances, fees, PnL) is `rust_decimal::Decimal`
end to end; `f64` is used only for statistics.

## Build and test

```bash
cargo build --release -p bb-cli          # -> target/release/bb
cargo build --release -p cryptobot-gui   # -> target/release/cryptobot-gui
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
| Bot trader | `bot-spot` |
| Live / scheduled strategies | `funding-arb`, `trend-filter`, `mean-reversion`, `listing-detector`, `listing-sniper` |
| Capital | `capital-pool`, `auto-convert`, `bot-status`, `cash-distributor` |
| Paper pool | `pool`, `pool-report`, `pool-replay`, `pool-history-replay`, `pool-unified` |
| Products / diagnostics | `api-permissions`, `scope-audit`, `product-modules`, `advanced-readiness`, `margin`, `margin-canary`, `futures`, `futures-canary`, `transfers`, `transfer-canary`, `withdrawals`, `prediction`, `spot-canary`, `live-spot-allocator`, `live-probe`, `ops-check` |
| Legacy runtime | `main` |
| Signals / Solana / misc | `signals-*`, `sniper-*`, `social-signal`, `polymarket-scanner`, `airdrop-scheduler` |
| Research | `*-backtest`, `spec-snapshot` |

## Bot agentic Spot trader (`bb bot-spot`)

The bot trades the Spot wallet through omp (the oh-my-pi coding agent, npm
`@oh-my-pi/pi-coding-agent`) in RPC mode:

- **Persistent context.** One long-lived omp session per trader, stored in
  `BOT_SPOT_SESSION_DIR`, resumed after restarts and crashes. omp
  auto-compaction stays on, and the bot also compacts at
  `BOT_SPOT_COMPACT_AT_PERCENT`. A durable journal
  (`BOT_SPOT_JOURNAL_PATH`) holds the bot's theses across compactions.
- **Tools, not text.** omp runs with every built-in tool disabled except
  `web_search` (`BOT_SPOT_WEB_SEARCH=true`, the default), so the bot has live
  web search plus the bot's host tools: `get_portfolio`, `list_routes`,
  `get_market_evidence`, `get_klines`, `get_social_evidence`, `get_trade_history`,
  `execute_route`, `journal_read` / `journal_write`, and `end_cycle`. No shell or
  filesystem access. `execute_route` is the only order path. Web results are
  treated as untrusted data in the system prompt.
- **No trading guardrails.** Fees, spread, slippage, momentum, evidence, social
  data and trade memory are information for the bot to weigh. None of them blocks
  a trade. The old edge, friction, speculative, loss-streak, cooldown,
  daily-cap, proven-edge and rules-engine gates are gone. Their env vars are
  ignored, with one warning listing them. There are no stop-loss or
  take-profit levels either: theses record why, expected move and horizon,
  and the prompt tells the bot not to set invalidation prices or exit triggers.
- **Hard safety (always on, fail closed):**
  - live arming: `BOT_SPOT_ENABLED=true`, `BOT_SPOT_DYNAMIC_SCOPE=all_eligible_spot`, `USE_TESTNET=false`, `DRY_RUN=false`, `CONFIRM_LIVE_TRADING=yes`;
  - the emergency stop file (`BOT_SPOT_STOP_PATH`), checked at cycle start, before every route, before every leg and before every order;
  - an exclusive run lock;
  - account READING plus canTrade;
  - Binance `order/test` before every leg;
  - exchange filters, with orders sized from live balances net of a fee, slippage and rounding reserve;
  - per-leg fill reconciliation, halting on anything ambiguous;
  - a failed-route halt, cleared only by `bb bot-spot --mark-failed-route-reviewed`;
  - operator scope (allow/block lists, blocked quotes, fiat blocking);
  - Spot MARKET orders only, at most `BOT_SPOT_MAX_ROUTE_LEGS` legs.
- **Audit.** Each route or cycle is one fsync'd row in `BOT_SPOT_AUDIT_PATH`,
  written before any Telegram alert. It uses the Python schema plus agent
  fields: tool calls, tokens and cost.

```bash
bb bot-spot --evidence-only                 # decision context, no LLM call, no orders
bb bot-spot --plan-only                     # The bot decides; execute_route only records plans
bb bot-spot --submit-live --daemon          # live, one cycle every --interval-seconds (240)
bb bot-spot --mark-failed-route-reviewed    # clear the failed-route halt after review
bb bot-spot-exit-replay --table             # replay the Phase 45 sell-plan menu on real round trips
bb bot-spot-exit-watch --daemon             # paper exit-plan watcher (no orders)
bb bot-spot-exit-watch --summary            # closed plan trials vs the bot's own exits (live plan-sold positions apart)
```

`bb bot-spot-exit-replay` (Phase 45, research only, public klines, no
orders) pairs each real round trip in the audit log (current and older row
schema, every market; `--quote USDT` keeps one quote), replays sell plans on
the entry market's klines from entry to `--horizon-minutes` (1440) later and
compares them with the bot's actual exit and with the best price seen. P&L is
the net return times the USDT value the route spent. Plans come from a fixed
menu, repeatable as `--plan`: `trail=<pct>` or `atr=<multiple>:<1m|5m|15m|1h>`
(follow the peak by a percent or by average true ranges, Chandelier exit),
`part=<gain_pct>:<fraction>` (take part), `stretch=<z>:<fraction>` (sell at
the upper Bollinger band of the last 20 minutes, only above entry plus the
buy and sell fees),
`slow=<fraction>:<minutes>` (still rising but slowing 3 minutes in a row),
`fade=<min_gain_pct>:<minutes>` (momentum fade), `deadline=<minutes>` and
`hold`. `docs/exit-plan-candidates.md` lists the researched functions not in
the menu yet. Simulated sells pay the entry leg's fee rate plus
`--slippage-bps` (5); `--interval 1s` gives a finer replay. A round trip whose
klines are unavailable is listed under `skipped`.

Exit plans: The bot may give an open position one of these plans with the
`set_exit_plan` host tool (its own parameters; it never writes code; setting
again replaces it, `clear` removes it). Positions opened from
`BOT_SPOT_DEFAULT_EXIT_PLAN_FROM` run `BOT_SPOT_DEFAULT_EXIT_PLAN`
unless the bot sets its own plan. With `BOT_SPOT_EXIT_PLANS_LIVE=true` the
`bot-spot --submit-live --daemon` process follows those positions' bids
every `BOT_SPOT_EXIT_POLL_SECONDS` (5) between cycles and sells a position
into USDT when its plan fires, with the same hard safety as the bot's routes
(audit rows with `cycle_id` `exit-plan`). Otherwise `bb bot-spot-exit-watch`
runs the plans on paper and only records would-be sells. Either way the cycle
prompt shows the bot each plan. Files: `BOT_SPOT_EXIT_PLANS_PATH`,
`BOT_SPOT_EXIT_WATCH_STATE_PATH`, `BOT_SPOT_EXIT_WATCH_LOG_PATH`.

Cost control (not a trading gate): `BOT_SPOT_MAX_DAILY_LLM_COST_USD` (0 = unlimited).
Choose the model and thinking level with `BOT_SPOT_MODEL` (default
`anthropic/claude-haiku-5-5`; always use provider-qualified ids) and
`BOT_SPOT_THINKING` (default `high`).

Model failover (bot-owned, independent of omp's `retry.fallbackChains`):
`BOT_SPOT_FALLBACK_MODELS` is a comma list of `provider/model-id[:thinking]`
(default empty = no failover). While fallbacks exist the primary gets
`BOT_SPOT_PRIMARY_TIMEOUT_SECONDS` (150) per attempt. A turn that times
out, errors, crashes or hits a rate/usage limit is retried once per remaining
model, but never when the failed attempt called `execute_route` (orders may
exist) or already recorded `end_cycle`; a failed compaction also moves to the
next model. After `BOT_SPOT_FALLBACK_COOLDOWN_SECONDS` (1800) the next
cycle returns to the primary, compacting first if the context exceeds the
primary's window threshold. Cycle rows record the model that produced the
outcome, the failed `attempts` and `fallback_active`. Telegram gets one
message per switch to a fallback (also when omp switched by itself), one when
back on the primary, and one when `BOT_SPOT_BRAIN_DOWN_ALERT_CYCLES` (3)
consecutive cycles failed on every model (counter persisted in the agent state
file).

omp needs model credentials: either `ANTHROPIC_API_KEY`, or a one-time
interactive `omp` login (stored under `PI_CODING_AGENT_DIR`).

## Dashboard (`cryptobot-gui`)

A read-only web page for the fleet, in six tabs that refresh every 30 s:

- **Overview:** portfolio value and 24h change, the bot's P&L and win rate, the
  open position, the bot's status (running / idle / stopped by the stop file /
  halted on a failed route), the cumulative P&L curve, allocation, the bot's
  latest call and recent trades.
- **History:** every Bot Spot trade from the whole audit log with net P&L,
  win rate, average win/loss, profit factor, fees and hold time; cumulative
  P&L, per-trade and daily P&L charts, outcome mix, P&L by coin and a trade
  table (click a row for the bot's reasoning); 24h / 7d / 30d / all ranges.
- **Wallet:** spot balances (free and in orders) with price, 24h change, value
  and share, Simple Earn positions, Binance wallet totals, account fees and an
  API-key safety check (withdrawals, IP restriction, extra permissions).
- **The bot:** cycles with the bot's summaries, tool calls and cost, and its
  journal.
- **Fleet:** every bot from `bb bot-status` with activity and P&L.
- **Signals:** the Phase 43 X insighter (paper only): collector health (stale
  after 5 min without a heartbeat), the focus list, 6h traction and exhaustion
  curves of focus/hot tokens with stage changes and the latest Haiku take, the
  newest 50 signals, and the weekly report (per-venue hit, rug and late rates,
  lead time, arm P&L, gate criteria, paper equity curve). It reads the `x-*`
  files in `--data-dir`; the growing JSONL files are tailed incrementally.

Trade results come from the route quotes the trader logs: each submitted route
opens a trade in the coin it bought, which closes when the bot later sells that
coin (or is marked at the current price while held). Cost is the USDT value
given up, so fees and spread count against the trade and the trade results add
up to the whole run. Moves into stablecoins count toward net P&L but not toward
wins and losses.

```bash
cryptobot-gui                                  # http://localhost:8787/
cryptobot-gui --open                           # same, and open it in the default browser
                                               # (if one is already running, just open it)
cryptobot-gui --bind 127.0.0.1:9000 --data-dir ~/.local/share/binance-bot/data
ssh -L 8787:localhost:8787 <bot-host>          # view a remote box from your laptop
```

- **Read-only.** GET routes only. It reads the Bot Spot files
  (`BOT_SPOT_AUDIT_PATH`, `_JOURNAL_PATH`, `_STATE_PATH`, `_STOP_PATH`,
  relative to the working directory like the trader), the fleet and X
  insighter files under `BOT_STATUS_DATA_DIR`, and Binance through signed GETs
  (account, 24h tickers, wallet totals, API-key restrictions; one read is
  reused for 20 s).
  It never writes a file or calls an order endpoint. Stopping the trader is
  still `touch` on the stop file.
- **Localhost only.** It listens on `127.0.0.1:8787` (`--bind` /
  `CRYPTOBOT_GUI_BIND`) and answers only requests whose `Host` is `localhost`,
  `127.0.0.1` or `[::1]`, which blocks DNS-rebinding pages. There is no login,
  so don't publish it on a network interface.
- Without Binance credentials the wallet tab stays empty and open trades are
  not marked; everything else still works.

## Capital pool, bot allocator, auto-convert

`bb capital-pool` shows the plan: a reserve (`CAPITAL_RESERVE_PCT`), per-module
min/max (`CAPITAL_MIN/MAX_MODULE_ALLOCATION_USD`), and score-weighted splits
from ledger scorecards or lifetime PnL. `--advise` asks the bot allocator,
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
bb api-permissions --feature bot_spot_autonomous
bb scope-audit --symbol BNB/USDT --asset BNB
bb funding-arb                        # plan-only by default
bb bot-spot-backtest --help
```

Historical replay is for strategy research only: it uses public candles and
modelled costs. The margin, futures, transfer, withdrawal and prediction
modules model and plan only; their live writes are not implemented.

## Deployment

Docker is the primary target (Raspberry Pi 5 / ARM64 and x86_64). The image
contains `bb`, `omp` (on Bun), supercronic and sqlite3.

```bash
docker compose build
docker compose run --rm -it bot-spot omp             # one-time omp login (or set ANTHROPIC_API_KEY)
docker compose run --rm -it signals-telegram bb signals-telegram-setup   # one-time Telegram user login
docker compose --profile all up -d
touch docker-data/bot-spot.stop                      # emergency stop for the bot trader
# dashboard: http://localhost:8787 on the Docker host (published on 127.0.0.1 only)
```

Profiles:
- `live`: the bot trader (the only live trader) and the scheduler.
- `paper`: paper and signal workloads plus the scheduler.
- `all`: both of the above.
- `v2`: the launch detector.
- `main-bot`: the legacy SMA runtime.
- `gui`: the `cryptobot-gui` dashboard alone (it also starts with `paper`, `live` and `all`; the data volume is mounted read-only).
- `listing-sniper-live`: the live momentum listing sniper (off in production).

Periodic jobs live in `docker/crontab`. systemd user units for non-Docker
hosts are in `deploy/user/`, installed by `scripts/setup_user_deploy.sh`. See
`OPERATIONS.md` for backup, restore and verification procedures.
