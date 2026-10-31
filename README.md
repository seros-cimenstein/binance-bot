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
| `bb-trader` | `bot-spot`: the algorithm trader (the operator's rule), its offline backtest `algo-backtest` and the `tag-scoreboard` |
| `bb-pool` | library for the trader: market catalog, commission/fee service, multi-leg route engine, models (no commands, no order path) |
| `bb-strategies` | bot research: `bot-spot-backtest`, `spec-snapshot`, `entry-study` |
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
| Bot research | `algo-backtest`, `tag-scoreboard`, `bot-spot-backtest`, `spec-snapshot`, `entry-study` |

## Algorithm Spot trader (`bb bot-spot`)

The trader is an algorithm (operator decision 2026-10-29, Phase 52): no LLM, no
chat, no host tools. With `--nonstop` it ticks every 15 s: it polls every
price with one request, reads the tags feed (`tags-feed-v1` from biz-app,
`BOT_SPOT_TAGS_FEED_PATH`), and runs the operator's rule file (`crate::rules`,
`BOT_SPOT_ALGO_RULE`, required) on the coins the feed's live asset tags reach
with an eligible USDT market (directly, or through the operator's proxy file
`BOT_SPOT_PROXIES`, e.g. gold through PAXG and XAUT) and the coins it holds. A decision row is written when
the decided action changes and once an hour; an unchanged tick writes nothing:

- **Score.** `Σ weight · signal` per coin over the rule file's weighted signal
  names (`crate::signals`: `talk.6h` /biz/ talk surge, `stance.6h` /biz/ AI
  stance, needs the sleeve's `min_posters`, `momentum.1h` the coin's own 1h
  move, and the tag and market-wide signals). The file holds `rules_version`
  (recorded on every row), sleeves (share, horizon filter, defensive assets,
  risk-off level, buy and sell levels, poster minimum, weights) and `style`:
  `fixed` (the operator's weights) or `edge` (weights from the tag scoreboard's
  measured edge, scaled and capped; only a scoreboard whose outcomes all closed
  before the decision counts, else the sleeve holds with no orders and the row
  says why; none shipped). A missing file, an unknown signal, shares not
  summing to 1 or a sell level above the buy level: the trader refuses to
  start. Each sleeve with a share or a holding decides on its own (own decision
  key, heartbeat and rows): its `horizon` filters candidates by their own tag's
  7d horizon mix, and below `risk_off_below` (its weighted `market.*` score)
  it moves to its best-scoring `defensive` asset (USDT, PAXG, XAUT).
- **Orders.** A held coin scoring below the sell level is sold into USDT (exactly
  the sleeve's quantity); with nothing held, the best coin at or above the buy
  level is bought for the sleeve's share of the portfolio value (USDT plus what
  the sleeves own), capped at the free USDT. An order under its market's
  minimum order is not sent (`below_min_notional`, the sleeve holds). Every
  order goes through `execute_route` and every hard-safety check
  below. Without a fresh feed it buys nothing.
- **No added trading limits.** No stop-loss, take-profit, cooldown or caps
  beyond the rule the operator chose. Old guardrail env vars are ignored with
  one warning listing them.
- **Backtest.** `bb algo-backtest --rule <rule file> --tags-history <history
  dir>` runs the rule file's sleeves over the window as the trader would, with
  candidates from the tags history (OPERATIONS.md, "Algorithm backtest"). The compose profile
  `paper` runs the nonstop trader plan-only.
- **Rule in force live:** `config/rules/snapback-2026-10-31.json` (fast
  sleeve, share 1: `momentum.24h` −1, buy at +0.8, sell below -0.3; core
  sleeve share 0; `docs/decisions.md`). The tag rule
  `config/rules/algo-2026-10-29.json` (`talk.6h` 0, `stance.6h` 1,
  `momentum.1h` 1, buy at +0.5, sell below -0.5, 2 posters) runs plan-only in
  `bot-spot-paper` for its tuning.
- **Hard safety (always on, fail closed):**
  - live arming: `BOT_SPOT_ENABLED=true`, `BOT_SPOT_DYNAMIC_SCOPE=all_eligible_spot`, `USE_TESTNET=false`, `DRY_RUN=false`, `CONFIRM_LIVE_TRADING=yes`;
  - the emergency stop file (`BOT_SPOT_STOP_PATH`), checked on every tick, before every route, before every leg and before every order;
  - an exclusive run lock;
  - account READING plus canTrade;
  - Binance `order/test` before every leg;
  - exchange filters, with orders sized from live balances net of a fee, slippage and rounding reserve;
  - per-leg fill reconciliation, halting on anything ambiguous;
  - a failed-route halt, cleared only by `bb bot-spot --mark-failed-route-reviewed`;
  - sleeve holdings in the state file (strict load) reconciled with the free balances every tick: a mismatch or unreadable state halts plan-only and live (`sleeve_mismatch`), cleared only by `bb bot-spot --mark-sleeves-reviewed` once they reconcile; balances no sleeve owns are never sold or counted;
  - operator scope (allow/block lists, blocked quotes, fiat blocking);
  - Spot MARKET orders only, at most `BOT_SPOT_MAX_ROUTE_LEGS` legs.
- **Audit.** Each route and each recorded decision is one fsync'd row in
  `BOT_SPOT_AUDIT_PATH`. It uses the Python schema plus `rules_version`
  (the rule file's) and, on decision rows, the rule's
  verdict (`algo`), the market-wide signals, a live-tags summary and the rule
  file's `sha256` (`run_config.rule`); a row is kept under 32 KB by dropping
  detail, never the decision (`trimmed` lists what went).
  There are no Telegram or phone alerts (operator decision 2026-10-29); watch
  the dashboard and the audit log.

```bash
bb bot-spot --evidence-only                 # decision context and quotable routes, no row, no order
bb bot-spot --plan-only                     # one decision; execute_route only records plans
bb bot-spot --submit-live --nonstop         # the nonstop trader, live (a tick every 15 s)
bb bot-spot --mark-failed-route-reviewed    # clear the failed-route halt after review
bb bot-spot --mark-sleeves-reviewed         # clear the sleeve_mismatch halt once the sleeves reconcile
bb algo-backtest --end 2026-10-18T10:43:00Z   # the algorithm, offline backtest (no orders)
```

`bb algo-backtest` is research only and runs offline: it reads only the
cached 1m candles in `--cache-dir` (default `data/entry-study-cache`), the tags
history and the rule file, makes no network call, has no order path and never
touches the live trader. It runs every sleeve of `--rule` over `--days` (24)
before `--end` (an edge rule walk-forward on `--scoreboards`) and writes
`reports/algo_backtest_<end-date>.md`, `.csv` (portfolio and per sleeve),
`_trades.csv` and `_<sleeve>_trace.csv`. See OPERATIONS.md, "Algorithm
backtest".

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
  least hourly, stopped by the stop file, halted, the `sleeve_mismatch` halt
  included), the rule file (version, style, sha256) and one panel per sleeve:
  share, horizon, levels, risk-off level and state (market score), what it
  holds per the state file, and its newest decision with its listed coins
  (held coins and the 20 best candidates) under the sleeve's own signal names
  and weights, own 1h, 24h and 7d moves and score; then every sleeve's
  decision rows newest first (click one for its scores; halt rows marked);
  rows from before Phase 52 show their decision only.
- **Scoreboard:** the newest `bb tag-scoreboard` file (`data/scoreboard/latest.json`,
  `CRYPTOBOT_GUI_SCOREBOARD_PATH`): per horizon and target each signal's
  samples, excess, net of the fee, hit rate and `t` (strongest first, `|t| ≥ 2`
  by default), the older and newer halves' excess and the decay flag.
- **Tags:** the tags feed and tag registry as biz-app wrote them: every live tag
  by kind (24h and 7d posters and lean, horizon, aliases, new ones marked),
  asset tags marked Binance market / through a proxy / scored only, baskets,
  links, events by date and claims with target, deadline and status.

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
  (`BOT_SPOT_AUDIT_PATH`, `BOT_SPOT_STOP_PATH`, `BOT_SPOT_STATE_PATH`,
  `BOT_SPOT_TAGS_FEED_PATH` with the registry and asset list beside it,
  `BOT_SPOT_PROXIES`, relative to the working directory like the trader), the
  scoreboard (`CRYPTOBOT_GUI_SCOREBOARD_PATH`, default
  `data/scoreboard/latest.json`) and
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
bb tag-scoreboard --history-dir <tags history>   # which tags and signals led prices (data/scoreboard/)
```

Historical replay is for strategy research only: it uses public candles and
modelled costs.

## Deployment

Docker is the primary target (Raspberry Pi 5 / ARM64 and x86_64). The image
contains `bb`, `cryptobot-gui` and supercronic.

```bash
docker compose --profile all build   # every service has a profile; plain `build` builds nothing
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
