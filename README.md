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
| `bb-trader` | `bot-spot`: the algorithm trader (the operator's rule), exit plans, persona backtests, the Markov value model (the last three until Phase 52 removes them) |
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
| Bot trader | `bot-spot`, `bot-spot-exit-watch`, `bot-spot-exit-replay` |
| Bot research | `bot-spot-persona-backtest`, `bot-spot-mdp-fit`, `gcp2-history`, `bot-spot-backtest`, `spec-snapshot`, `entry-study`, `biz-lead-study` |

## Algorithm Spot trader (`bb bot-spot`)

The trader is an algorithm (operator decision 2026-10-29, Phase 52): no LLM, no
chat, no host tools. Each cycle it reads the wallet and the /biz/ feed, reads 1m
prices for the feed's coins with an eligible USDT market and for the coins it
holds, and runs the operator's rule (`crate::decider`, `BOT_SPOT_ALGO_WEIGHTS`,
required):

- **Score.** `w_talk·talk + w_stance·stance + w_momentum·momentum + w_value·value`
  per coin: /biz/ talk surge, /biz/ AI stance (needs
  `BOT_SPOT_ALGO_MIN_POSTERS`), the coin's own 1h move, and the long-term
  value model (below, until Phase 52 removes it).
- **Orders.** A held coin scoring below the sell level is sold into USDT; with
  nothing held, the best coin at or above the buy level is bought with all
  USDT. Every order goes through `execute_route` and every hard-safety check
  below. Without a fresh feed it buys nothing.
- **No added trading limits.** No stop-loss, take-profit, cooldown or caps
  beyond the rule the operator chose. Old guardrail env vars are ignored with
  one warning listing them.
- **Backtest.** `bb bot-spot-persona-backtest --persona biz_algo --biz-feeds
  <history dir>` tunes the weights on the older half of the window and judges
  them on the newer (OPERATIONS.md, "Persona backtest"). The compose profile
  `algo-paper` runs the trader plan-only.
- **Still in the code until Phase 52 removes it:** the GCP2 schizo state
  (recorded per cycle as `schizo` when `BOT_SPOT_GCP2_ENABLED`), the Markov
  long-term value model (`bb bot-spot-mdp-fit`, `BOT_SPOT_MDP_*`), exit plans
  and the 20-minute cycle.
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
- **Audit.** Each route or cycle is one fsync'd row in `BOT_SPOT_AUDIT_PATH`.
  It uses the Python schema plus `rules_version` (`BOT_SPOT_RULES_VERSION`,
  required) and, on cycle summaries, the rule's verdict (`algo`).
  There are no Telegram or phone alerts (operator decision 2026-10-29); watch
  the dashboard and the audit log.

```bash
bb bot-spot --evidence-only                 # decision context, no LLM call, no orders
bb bot-spot --plan-only                     # the algorithm decides; execute_route only records plans
bb bot-spot --submit-live --daemon          # live, one cycle every --interval-seconds (240)
bb bot-spot --mark-failed-route-reviewed    # clear the failed-route halt after review
bb bot-spot-exit-replay --table             # replay the Phase 45 sell-plan menu on real round trips
bb bot-spot-exit-watch --daemon             # paper exit-plan watcher (no orders)
bb bot-spot-exit-watch --summary            # closed plan trials vs the bot's own exits (live plan-sold positions apart)
bb bot-spot-persona-backtest --end 2026-10-18T10:43:00Z   # Phase 48 rule personas, offline backtest (no orders)
bb gcp2-history --year 2026 --month 8 --month 9 --month 10 --cache-dir data/gcp2-cache   # GCP2 history for the schizo backtest
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

Exit plans (until Phase 52 removes them): positions opened from
`BOT_SPOT_DEFAULT_EXIT_PLAN_FROM` run `BOT_SPOT_DEFAULT_EXIT_PLAN`; plans the
LLM set earlier stay in the plan book. With `BOT_SPOT_EXIT_PLANS_LIVE=true` the
`bot-spot --submit-live --daemon` process follows those positions' bids
every `BOT_SPOT_EXIT_POLL_SECONDS` (5) between cycles and sells a position
into USDT when its plan fires, with the same hard safety as the bot's routes
(audit rows with `cycle_id` `exit-plan`). Otherwise `bb bot-spot-exit-watch`
runs the plans on paper and only records would-be sells. Files: `BOT_SPOT_EXIT_PLANS_PATH`,
`BOT_SPOT_EXIT_WATCH_STATE_PATH`, `BOT_SPOT_EXIT_WATCH_LOG_PATH`.

`bb bot-spot-persona-backtest` (Phase 48) is research only and runs offline: it
reads only the cached 1m candles in `--cache-dir` (default
`data/entry-study-cache`), makes no network call, has no order path and never
touches the live trader. It runs deterministic rule personas over `--days`
(60) before `--end`: the `v13` rule proxy, `contrarian`, `detached`,
`high_variance`, `rotation` and `ensemble` (`--persona all`), plus the Phase 49
`schizo` persona by name, which also reads the GCP2 history that
`bb gcp2-history` caches in `--gcp2-cache-dir` (default `data/gcp2-cache`). It
writes `reports/schizoid_backtest_<end-date>.md`, `.csv`, `_trades.csv` and,
for `schizo`, `_schizo_trace.csv`. See OPERATIONS.md, "Persona backtest".

## Dashboard (`cryptobot-gui`)

A read-only web page for the trader, in seven tabs that refresh every 30 s:

- **Overview:** portfolio value and 24h change, the bot's P&L and win rate, the
  open position, the bot's status (running / idle / stopped by the stop file /
  halted on a failed route), the cumulative P&L curve, allocation, the bot's
  latest call and recent trades.
- **History:** every Bot Spot trade from the whole audit log with net P&L,
  win rate, average win/loss, profit factor, fees and hold time; cumulative
  P&L, per-trade and daily P&L charts, outcome mix, P&L by coin and a trade
  table (click a row for the bot's reasoning); 24h / 7d / 30d / all ranges.
- **Sessions:** results per trading session (one setup of the trader: prompt
  version, omp session, run configuration), ranked by realized P&L, with each
  session's configuration and the exit plans it ran.
- **Wallet:** spot balances (free and in orders) with price, 24h change, value
  and share, Simple Earn positions, Binance wallet totals, account fees and an
  API-key safety check (withdrawals, IP restriction, extra permissions).
- **The bot:** cycles with the rule's verdicts (older LLM cycles keep their
  tool calls and cost).
- **Exits:** the Phase 45 exit plans: run settings, the plan book, running
  plans with their sell lines, and the newest watch-log events.
- **/biz/:** the /biz/ feed the trader read in its newest cycle (mentions,
  posters and stance per coin, coins without a Binance market); only the
  rule-based decider trades on it.

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
  (`BOT_SPOT_AUDIT_PATH`, `BOT_SPOT_STOP_PATH`, the
  exit-plan files, relative to the working directory like the trader) and
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
- `exit-paper`: the paper exit-plan watcher `bot-spot-exit-watch`.
- `algo-paper`: the plan-only rule-based decider `bot-spot-algo-paper`.

Periodic jobs (the nightly Bot Spot data backup) live in `docker/crontab`.
systemd user units for non-Docker hosts are in `deploy/user/`, installed by
`scripts/setup_user_deploy.sh`. See `OPERATIONS.md` for deploy, backup and
verification procedures.
