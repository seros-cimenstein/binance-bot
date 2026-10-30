# Binance Trading Bot

The Bot Spot trader written in Rust: a native Binance Spot REST client,
the Spot algorithm trader (`bb bot-spot`, the operator's rule, no LLM),
and its research tools and backtests. It ships as
one binary, `bb`, next to a read-only web dashboard, `cryptobot-gui`.

The other modules this repo used to hold (legacy runtime, paper pool runtime,
capital pool, listing sniper, products and canaries, signals, Solana sniper,
Polymarket, airdrop, funding arb, trend filter, mean reversion) were moved to
`../redundancy/binance-bot-modules/` on 2026-10-29; they are also in git history.

## Layout

| Crate | Purpose |
|---|---|
| `bb-core` | env config, `Decimal` money helpers, fsync'd JSONL audit, run locks, stop files |
| `bb-binance` | native Binance Spot REST client: HMAC/Ed25519 signing, time sync, rate-limit tracking, exchange rules, spot market data and orders, wallet balance totals, klines history |
| `bb-trader` | `bot-spot`: the algorithm trader (the operator's rule) and its offline backtest `algo-backtest` |
| `bb-pool` | library for the trader: market catalog, commission/fee service, multi-leg route engine, models (no commands, no order path) |
| `bb-strategies` | bot research: `bot-spot-backtest`, `spec-snapshot`, `entry-study`, `biz-lead-study` |
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
scripts/smoke.sh                         # bash -n on every script
```

Copy `.env.example` to `.env` and fill in your keys. Never commit `.env` or
`.env.*` files. `bb` loads `.env` from the working directory; process
variables win. Logs go to stderr (`LOG_LEVEL`, `RUST_LOG`, `LOG_FORMAT=json`);
stdout is reserved for command output.
## Commands

Run `bb --help` or `bb <command> --help`.

| Area | Commands |
|---|---|
| Bot trader | `bot-spot` |
| Bot research | `algo-backtest`, `bot-spot-backtest`, `spec-snapshot`, `entry-study`, `biz-lead-study` |

## Algorithm Spot trader (`bb bot-spot`)

The trader is an algorithm (operator decision 2026-10-29, Phase 52): no LLM, no
chat, no host tools. With `--nonstop` it ticks every 15 s: it polls every
price with one request, reads the /biz/ feed, and runs the operator's rule
(`crate::decider`, `BOT_SPOT_ALGO_WEIGHTS`, required) on the feed's coins with
an eligible USDT market and the coins it holds. A decision row is written when
the decided action changes and once an hour; an unchanged tick writes nothing:

- **Score.** `w_talk·talk + w_stance·stance + w_momentum·momentum` per coin:
  /biz/ talk surge, /biz/ AI stance (needs `BOT_SPOT_ALGO_MIN_POSTERS`) and the
  coin's own 1h move. `BOT_SPOT_ALGO_WEIGHTS` is
  `talk,stance,momentum,buy_above,sell_below`; `BOT_SPOT_RULES_VERSION` names
  the rule set.
- **Orders.** A held coin scoring below the sell level is sold into USDT; with
  nothing held, the best coin at or above the buy level is bought with all
  USDT. Every order goes through `execute_route` and every hard-safety check
  below. Without a fresh feed it buys nothing.
- **No added trading limits.** No stop-loss, take-profit, cooldown or caps
  beyond the rule the operator chose. Old guardrail env vars are ignored with
  one warning listing them.
- **Backtest.** `bb algo-backtest --biz-feeds <history dir>` tunes the
  weights on the older half of the window and judges them on the newer
  (OPERATIONS.md, "Algorithm backtest"). The compose profile
  `paper` runs the nonstop trader plan-only.
- **Rule in force:** `0,1,1,0.5,-0.5` (`algo-2026-10-29`, `docs/decisions.md`).
- **Hard safety (always on, fail closed):**
  - live arming: `BOT_SPOT_ENABLED=true`, `BOT_SPOT_DYNAMIC_SCOPE=all_eligible_spot`, `USE_TESTNET=false`, `DRY_RUN=false`, `CONFIRM_LIVE_TRADING=yes`;
  - the emergency stop file (`BOT_SPOT_STOP_PATH`), checked on every tick, before every route, before every leg and before every order;
  - an exclusive run lock;
  - account READING plus canTrade;
  - Binance `order/test` before every leg;
  - exchange filters, with orders sized from live balances net of a fee, slippage and rounding reserve;
  - per-leg fill reconciliation, halting on anything ambiguous;
  - a failed-route halt, cleared only by `bb bot-spot --mark-failed-route-reviewed`;
  - operator scope (allow/block lists, blocked quotes, fiat blocking);
  - Spot MARKET orders only, at most `BOT_SPOT_MAX_ROUTE_LEGS` legs.
- **Audit.** Each route and each recorded decision is one fsync'd row in
  `BOT_SPOT_AUDIT_PATH`. It uses the Python schema plus `rules_version`
  (`BOT_SPOT_RULES_VERSION`, required) and, on decision rows, the rule's
  verdict (`algo`).
  There are no Telegram or phone alerts (operator decision 2026-10-29); watch
  the dashboard and the audit log.

```bash
bb bot-spot --evidence-only                 # decision context and quotable routes, no row, no order
bb bot-spot --plan-only                     # one decision; execute_route only records plans
bb bot-spot --submit-live --nonstop         # the nonstop trader, live (a tick every 15 s)
bb bot-spot --mark-failed-route-reviewed    # clear the failed-route halt after review
bb algo-backtest --end 2026-10-18T10:43:00Z   # the algorithm, offline backtest (no orders)
```

`bb algo-backtest` is research only and runs offline: it reads only the
cached 1m candles in `--cache-dir` (default `data/entry-study-cache`), makes
no network call, has no order path and never touches the live trader. It
tunes the algorithm (`biz_algo`) on the older half of `--days` (60) before
`--end` and judges it on the newer half. It writes
`reports/algo_backtest_<end-date>.md`, `.csv`, `_trades.csv` and
`_biz_algo@<half>_trace.csv`. See OPERATIONS.md, "Algorithm backtest".

## Dashboard (`cryptobot-gui`)

A read-only web page for the trader, in five tabs that refresh every 30 s:

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
- **Algorithm:** the trader's status (running while it writes a decision row at
  least hourly, stopped by the stop file, halted), the newest decision with
  every scored coin's /biz/ talk and stance, own 1h move, momentum and score,
  and the decision rows newest first (click one for its scores); rows from
  before Phase 52 show their decision only.
- **/biz/:** the /biz/ feed the trader read at its newest decision (mentions,
  posters and stance per coin, coins without a Binance market); the algorithm
  scores its coins.

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
cryptobot-gui --bind 127.0.0.1:9000
ssh -L 8787:localhost:8787 <bot-host>          # view a remote box from your laptop
```

- **Read-only.** GET routes only. It reads the Bot Spot files
  (`BOT_SPOT_AUDIT_PATH`, `BOT_SPOT_STOP_PATH`, relative to the working
  directory like the trader) and
  Binance through signed GETs (account, 24h tickers, wallet totals, API-key
  restrictions; one read is reused for 20 s).
  It never writes a file or calls an order endpoint. Stopping the trader is
  still `touch` on the stop file.
- **Localhost only.** It listens on `127.0.0.1:8787` (`--bind` /
  `CRYPTOBOT_GUI_BIND`) and answers only requests whose `Host` is `localhost`,
  `127.0.0.1` or `[::1]`, which blocks DNS-rebinding pages. There is no login,
  so don't publish it on a network interface.
- Without Binance credentials the wallet tab stays empty and open trades are
  not marked; everything else still works.
## Research

```bash
bb spec-snapshot                      # data/speculation/<UTC date>/ CSVs + SUMMARY.md
bb bot-spot-backtest --help
bb entry-study --help
bb biz-lead-study --help
```

Historical replay is for strategy research only: it uses public candles and
modelled costs.

## Deployment

Docker is the primary target (Raspberry Pi 5 / ARM64 and x86_64). The image
contains `bb`, `cryptobot-gui` and supercronic.

```bash
docker compose build
docker compose --profile all up -d --remove-orphans
touch docker-data/bot-spot.stop                      # emergency stop for the bot trader
# dashboard: http://localhost:8787 on the Docker host (published on 127.0.0.1 only)
```

Profiles:
- `live` / `all`: the bot trader, the dashboard and the scheduler.
- `gui`: the `cryptobot-gui` dashboard alone (the data volume is mounted read-only).
- `paper`: the nonstop trader plan-only, `bot-spot-paper` (its own audit, state and lock).

Periodic jobs (the nightly Bot Spot data backup) live in `docker/crontab`.
systemd user units for non-Docker hosts are in `deploy/user/`, installed by
`scripts/setup_user_deploy.sh`. See `OPERATIONS.md` for deploy, backup and
verification procedures.
