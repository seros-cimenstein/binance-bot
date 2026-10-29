# Binance Bot Operations

The repo ships one Rust binary, `bb` (see `README.md`), for the Bot Spot
trader, next to the read-only `cryptobot-gui` dashboard. Since 2026-10-29 every
other module (the legacy `main` runtime, the paper pool runtime, the canaries
and live probe, the capital pool, the listing sniper, the signal collectors and
X insighter, the Solana sniper, Polymarket, the airdrop scheduler, funding arb,
trend filter and mean reversion) lives in `../redundancy/binance-bot-modules/`
and in git history; their old sections of this file are in that folder's
`OPERATIONS-removed-sections.md`.

## Running `bb`

`bb` loads `.env` from the working directory and resolves relative `data/...`
paths against it, so run it from the directory that holds `.env` and `data/`.

| Host | Binary | Working directory |
|---|---|---|
| Development checkout | `target/release/bb` (`cargo build --release -p bb-cli`) | the checkout |
| systemd user host | `~/.local/share/binance-bot/bin/bb` | `~/.local/share/binance-bot` |
| Docker | `docker compose run --rm <service> bb ...` | `/app` (host `docker-data/` is `/app/data`) |

Commands below are written as `bb <command>`; substitute the binary or the
`docker compose run --rm <service>` prefix for your host. Every command has
`bb <command> --help`.

## Live Posture

Bot Spot (`bot-spot`) is the only trader in this repo, Spot only. It cannot
write margin, futures, universal-transfer, withdrawal or prediction endpoints.
Whether it trades is decided by the live arming variables and the stop file
(see "Algorithm Spot Trader"); whether it runs now and which image is
deployed is in `PROGRESS.md`.

## Safety Posture

The only order path is Bot Spot's `execute_route`; with
`BOT_SPOT_EXIT_PLANS_LIVE=true` its exit-plan sells go through the same hard
safety. `bot-spot-exit-watch` (paper), `bot-spot-algo-paper` (plan-only) and
the dashboard place no orders. The legacy `binance-bot.service` and the paper
pool units are no longer shipped; on a host that still has them installed,
keep them disabled. Verify the boundary on a systemd host:

```bash
systemctl --user list-unit-files 'binance-*'   # only binance-bot-spot*.service may be enabled
```

## Validate And Deploy

Validate in the checkout:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
scripts/smoke.sh
git diff --check
```

### systemd user host

Deploy without `sudo`:

```bash
scripts/setup_user_deploy.sh
systemctl --user enable --now binance-bot-spot.service
```

`scripts/setup_user_deploy.sh` builds `bb` and `cryptobot-gui` in release
mode, installs them atomically to `~/.local/share/binance-bot/bin/`, syncs
`scripts/`, installs every unit from `deploy/user/` and reloads the user
manager. It preserves `.env`, data, logs, and backups, and it starts no
service. Restart
running services after a deploy to load the new binary.

### Docker

```bash
docker compose build
docker compose --profile all up -d --remove-orphans
```

Profiles: `live` and `all` (bot trader, dashboard, scheduler;
`deploy/seref/binance-bot-compose.service` brings up `all` at boot), `gui`
(dashboard only), `exit-paper` (paper exit-plan watcher), `algo-paper`
(plan-only rule-based decider). The `scheduler` container runs the nightly
Bot Spot data backup from `docker/crontab` (baked into the image, so a crontab
change needs a rebuild).

`--remove-orphans` removes the containers of services that are no longer in
`docker-compose.yml`. The first deploy after the 2026-10-29 prune needs it (or
`docker rm -f` of each old container): otherwise the old paper, sniper,
signals, `main-bot` and X containers keep running, and the old `scheduler`
keeps running the airdrop digest and X insighter jobs. `up -d` also starts
`bot-spot`; while the trader should stay stopped, name the services instead:
`docker compose --profile all up -d --remove-orphans cryptobot-gui scheduler`.

## Status And Logs

```bash
cd ~/.local/share/binance-bot
systemctl --user status binance-bot-spot.service --no-pager
journalctl --user -u binance-bot-spot.service -f
```

Docker: `docker compose ps` and `docker compose logs -f <service>`.

Dashboard: the read-only `cryptobot-gui` (compose service `cryptobot-gui`,
profiles `gui`/`live`/`all`) serves the Bot Spot trades, cycles (the rule's
verdicts; older LLM cycles keep brain, tools and cost), exit plans (Exits tab), results per session
(Sessions tab: each setup ranked by realized P&L, the winner, and the
session's configuration from the `run_config` the trader writes on every
cycle summary), the /biz/ feed the trader read (/biz/ tab), a market mood
gauge on the Overview (bearish to bullish breadth of the 20 most-traded USDT
crypto markets over 15m/1h/4h/24h, from public Binance data), stop-file and
failed-route-halt state at `http://localhost:8787` on the Docker host. It is
published on `127.0.0.1` only and mounts the data dir read-only. From another
machine: `ssh -L 8787:localhost:8787 <host>`. On a systemd host,
`scripts/setup_user_deploy.sh` installs it next to `bb`; run
`cd ~/.local/share/binance-bot && bin/cryptobot-gui`.

Laptop on the Pi's live data: `scripts/cryptobot_gui_pi.sh --open` (the
`cryptobot-gui` fish command). It rsyncs the Pi's `docker-data` into
`~/.cache/binance-bot/pi-data` every 30 s (old omp logins, the old bot
session and session files are not copied; nothing is written on the Pi), maps the
trader's file paths and the exit-plan settings from the Pi's `.env.docker`,
and runs the local release build against the mirror with the laptop's
read-only keys for the wallet. Started without Pi data, the page says so
instead of showing empty tabs. Overrides: `PI_HOST`, `PI_DATA_DIR`,
`PI_ENV_FILE`, `MIRROR_DIR`, `MIRROR_SECONDS`.

## Emergency Stop Files

Bot Spot checks its stop file at cycle start and before every route, leg and
order, and refuses to trade while it exists. Create it to stop, delete it to
resume. `bot-spot-algo-paper` shares the same file. Paths are relative to the
bot directory (`~/.local/share/binance-bot/data/...` on systemd hosts,
`docker-data/...` on the Docker host).

| Runtime | Stop file (default) | Override |
|---|---|---|
| `bot-spot` | `data/bot-spot.stop` | `BOT_SPOT_STOP_PATH` |

```bash
touch ~/.local/share/binance-bot/data/bot-spot.stop   # systemd host
touch docker-data/bot-spot.stop                       # Docker host
```

## Algorithm Spot Trader

`bb bot-spot` trades the Spot wallet with the operator's rule (operator
decision 2026-10-29, Phase 52): no LLM, no omp session, no host tools. How it
decides is in "The algorithm" below; every order goes through `execute_route`
and the hard safety listed here.

The route quote reserves the account's undiscounted taker fee, the
`BOT_SPOT_SLIPPAGE_BUFFER_BPS` buffer (25) and lot rounding; that is the basis
for order sizing. Over 40 round trips to 2026-10-18 the real fees were about
7.5 bps per side, and fills beat the quote by about 25 bps.

### No added trading limits

The trader has no limits beyond the rule the operator chose: no edge minimum,
friction cap, loss-streak limit, cooldown, daily cap, stop-loss or take-profit.
Old guardrail variables (`BOT_SPOT_ALLOW_NEGATIVE_FRICTION_BPS`,
`BOT_SPOT_MARKET_EVIDENCE_MIN_EDGE_BPS`, `BOT_SPOT_SPECULATIVE_*`,
`BOT_SPOT_MAX_RECENT_LOSS_STREAK`, `BOT_SPOT_MIN_TRADE_INTERVAL_SECONDS`,
`BOT_SPOT_MAX_SUBMITTED_TRADES_PER_DAY`, `BOT_SPOT_PROVEN_EDGE`,
`BOT_SPOT_REQUIRE_PROVEN_EDGE`, `BOT_SPOT_DECISION_MODE`, ...) and the Phase
52 removals (`BOT_SPOT_DECIDER`, `TELEGRAM_*`, `SOCIAL_SIGNAL_ENABLED`, ...)
are ignored; the trader logs one warning listing any that are still set.

### Hard safety (always on, fail closed)

- Live arming: `BOT_SPOT_ENABLED=true`,
  `BOT_SPOT_DYNAMIC_SCOPE=all_eligible_spot`, `USE_TESTNET=false`,
  `DRY_RUN=false`, `CONFIRM_LIVE_TRADING=yes`.
- The emergency stop file `data/bot-spot.stop` (`BOT_SPOT_STOP_PATH`),
  checked at cycle start, before every route, before every leg and before
  every order. A one-shot run refuses to start under it; the `--daemon`
  keeps running and idles (no cycle, one log line when the stop
  starts and one when it ends), so a paused trader survives restarts and
  deploys. Cycles resume at the next interval after the file is deleted.
- An exclusive run lock (`BOT_SPOT_LOCK_PATH`).
- Account READING plus canTrade.
- Binance `order/test` before every leg.
- Exchange filters, with orders sized from live balances net of a fee,
  slippage and rounding reserve.
- Per-leg fill reconciliation, halting on anything ambiguous.
- The failed-route halt (below).
- Operator scope: allow/block lists, blocked quotes, fiat blocking.
- Spot MARKET orders only, at most `BOT_SPOT_MAX_ROUTE_LEGS` legs.
- Margin, futures, transfer, withdrawal, and prediction writes are unavailable.

Each route or cycle is one fsync'd row in `BOT_SPOT_AUDIT_PATH` (default
`data/bot-spot-decisions.jsonl`); cycle summaries carry the rule's verdict
(`algo`). There are no Telegram or phone alerts (operator decision
2026-10-29).

### Commands

```bash
bb bot-spot --evidence-only                 # decision context, no orders
bb bot-spot --plan-only                     # the algorithm decides; execute_route only records plans
bb bot-spot --submit-live --daemon          # live, one cycle every --interval-seconds (240)
bb bot-spot --mark-failed-route-reviewed --review-reason "reconciled by hand"
```

### Deployment

Bot Spot is a long-running daemon, not a timer:

- Docker: compose service `bot-spot` (profiles `live` and `all`), running
  `bb bot-spot --submit-live --daemon --interval-seconds 1200` (every 20
  minutes until Phase 52 makes the trader continuous).
- systemd: `binance-bot-spot.service` (same command, `Restart=always`):

```bash
systemctl --user enable --now binance-bot-spot.service
journalctl --user -u binance-bot-spot.service -f
```

The service always passes `--submit-live`; whether it trades is decided by the
live arming variables in `.env` and the stop file.

### Exit plans (Phase 45)

Phase 52 removes this; since the LLM trader is gone (2026-10-29) the prompt and tool parts below no longer run.

The bot may give an open position a sell plan with the `set_exit_plan` tool
(fixed menu: follow the peak by percent or ATR, take part, stretch, slowdown,
momentum fade, deadline; setting again replaces it, `clear` removes a plan the
bot set and never the default plan).
Positions opened from `BOT_SPOT_DEFAULT_EXIT_PLAN_FROM` run
`BOT_SPOT_DEFAULT_EXIT_PLAN` (`.env.docker`: `deadline=120`, sell all 2
hours after entry; v7 used `stretch=2:1,atr=3:1h`)
unless the bot sets its own plan; in the v7 runs it set deadline plans
(`deadline=60..180`). This is the v7 system restored on 2026-10-17 (branch
`old-gold-way`) after the fixed plan and the locked rules cut winners short.
`stretch` fires only when its line is above break-even (entry plus the buy
and sell fees at the trip's fee rate), so it cannot sell a fee-negative
"win" (UNI 2026-10-16, ZEC 2026-10-17).

- **Live** (`BOT_SPOT_EXIT_PLANS_LIVE=true`, deployed since 2026-10-15):
  between cycles `bot-spot` checks each planned position's bid every
  `BOT_SPOT_EXIT_POLL_SECONDS` (5) and sells it into USDT when its plan
  fires, through the same hard safety as the bot's routes. Each sell is a
  route row with `cycle_id` `exit-plan` in the audit;
  the watch log records `would_sell`, `sold`, `sell_failed` and
  `sell_abandoned`. The emergency stop file stops these sells too. To turn
  live plans off, set `BOT_SPOT_EXIT_PLANS_LIVE=false` and recreate
  `bot-spot`.
- **Paper** (`BOT_SPOT_EXIT_PLANS_LIVE=false`): compose service
  `bot-spot-exit-watch` (profile `exit-paper`) or
  `binance-bot-spot-exit-watch.service` records would-be sells and places
  no orders. It refuses to start while plans run live.
- Files: `BOT_SPOT_EXIT_PLANS_PATH` (written by the trader's tool),
  `BOT_SPOT_EXIT_WATCH_STATE_PATH` and `BOT_SPOT_EXIT_WATCH_LOG_PATH`
  (written by whichever of the two runs). The cycle prompt shows each plan
  and the 5 newest closed positions with the rule that sold, prices and the
  net result after fees (watch state `recent_closed`); trade history marks
  plan sells with the plan, rule, share and trigger price (`exit_plan`).
- Results: `bb bot-spot-exit-watch --summary` compares, for each closed
  position with a plan, "with the plan" and "The bot alone". A position a
  live plan sell closed has no bot exit to compare with: it is listed as
  `plan (live)` with its real result and left out of the plan-vs-bot total
  (its watch-log row has `closed_by_plan`, `exit_net_pct` and
  `fill_vs_trigger_usdt`; rows from before 2026-10-19 are matched against
  the audit's `exit-plan` sells, so `--summary` also reads
  `BOT_SPOT_AUDIT_PATH`). The dashboard's Exits tab shows the same split.

### /biz/ feed (Phase 51)

biz-app (`~/projects/automation/imageboards/biz-app`, on the laptop) writes
the /biz/ feed `biz-feed-v1`: per coin and per window (1h, 6h, 24h) the
mentions over every post, the distinct posters, and its AI's bullish and
bearish tags weighted by post score. `scripts/biz_feed_to_pi.sh` copies it to
`~/binance-bot/docker-data/biz-feed.json` on the Pi every 5 minutes
(`--once` for one copy; `PI_HOST`, `PI_DATA_DIR`, `BIZ_FEED`,
`COPY_SECONDS`). The trader reads `BOT_SPOT_BIZ_FEED_PATH` (default
`data/biz-feed.json`) every cycle and records it in the cycle summary as
`biz`: `status` `fresh` with `as_of`, `age_minutes`, `model`,
`prompt_version`, the coins with an eligible Binance market and the `dropped`
names, or `unavailable` with the reason (missing or unreadable file, another
schema, `as_of` older than `BOT_SPOT_BIZ_MAX_AGE_MINUTES` (45) or more than 5
minutes ahead of the Pi's clock). The algorithm (below) scores its coins on
it. The dashboard's /biz/ tab shows the newest recorded feed.

### The algorithm

`crate::decider` decides every cycle with the operator's rule
(`BOT_SPOT_ALGO_WEIGHTS`, required; the trader refuses to start without it).
Each cycle it reads the wallet and the /biz/ feed, then reads 1m prices for
the coins it may buy (the feed's coins with an eligible USDT market,
stablecoins left out) and for the coins it holds (operator decision
2026-10-29: no market breadth read). It scores those coins with
`talk,stance,momentum,buy_above,sell_below` (momentum is the coin's own
1h move; stances need `BOT_SPOT_ALGO_MIN_POSTERS`, 2), sells a held coin
scoring below the sell level into USDT and, with nothing held, buys the best
coin with all USDT at or above the buy level. Every order is quoted for the
whole free balance and sent through `execute_route`, so the stop file,
arming, scope, `order/test`, reconciliation and the failed-route halt apply;
the first failed order ends the cycle's orders. Without a fresh feed nothing
is a buy candidate, so it can only hold or sell; a coin whose price read fails
is left out of that cycle.

- **Records.** Cycle summaries carry `algo` (rule, best candidate and score,
  held coins and scores, orders), `rules_version` (`BOT_SPOT_RULES_VERSION`,
  required) and `session_id` `algo`. The Bot tab's cycle details show the
  rule's scores.
- **Practice run.** `docker compose --profile algo-paper up -d
  bot-spot-algo-paper` runs it plan-only with its own files
  (`bot-spot-algo-paper-{decisions.jsonl,state.json}`, lock
  `bot-spot-algo-paper.lock`) and the shared stop file. Plan-only rows do not
  move the wallet, so it re-decides on the real balances every cycle.
- **Going live** (after the practice run and the operator's go):
  `docker tag binance-bot:latest binance-bot:pre-algo-<date>`, set the
  operator's `BOT_SPOT_ALGO_WEIGHTS` in `~/binance-bot/.env.docker`, `docker
  compose up -d bot-spot`, watch the first cycles. With
  `BOT_SPOT_EXIT_PLANS_LIVE=true` the default exit plan still sells positions
  between cycles; set it to `false` for the rule's own sells only. Back: retag
  the rollback image and recreate. `touch docker-data/bot-spot.stop` stops it.

### Failed-route halt

A live route that submitted at least one order and then failed, or whose
submission was ambiguous, halts live trading. An unreadable audit history also
halts. To resume: stop the trader (stop file or service), reconcile the Spot
wallet against the audit row by hand, then append the review marker:

```bash
bb bot-spot --mark-failed-route-reviewed --review-reason "<what you checked>"
docker compose run --rm bot-spot bb bot-spot --mark-failed-route-reviewed
```

The halt is recomputed from the audit log, so the next cycle sees the marker.
Remove the stop file to resume.

### Backtesting

`bb bot-spot-backtest` replays Bot Spot's cost-aware momentum rule
against historical klines. It is research only; its verdict no longer gates
live trading.

```bash
bb bot-spot-backtest \
  --symbols BTC/USDT,ETH/USDT,SOL/USDT,BNB/USDT \
  --interval 1h \
  --start 2026-01-01 --end 2026-05-01 \
  --output data/bot-spot-backtest.json
```

The JSON output includes a `verdict`: `edge_likely` (Sharpe > 1 and total
return > 0), `edge_marginal` (positive return, modest Sharpe), `no_edge` (did
not clear its cost bar), or `no_trades` / `no_data` (insufficient window or
signals; rerun with more data).

`bb entry-study` asks when buying pays at all. It samples every 20 minutes
× the 20 most-traded USDT crypto markets over the last `--days` (default 90)
of public 1m candles (cached in `data/entry-study-cache`). For each sample it
records the 5m-24h returns, the volume ratio and the market breadth, and the
move to +20/+60/+90/+120 minutes. It then reports the mean net return (after
`--fee-bps`, default 15 per round trip, plus a spread proxy) per bucket,
train period vs the last `--test-days` (default 30). `--audit` also measures
the bot's real entries with the same candles. Public data only, no order path.

```bash
bb entry-study --audit docker-data/bot-spot-decisions.jsonl \
  --output data/entry-study.json --markdown docs/research/entry-study.md
```

#### Algorithm backtest

`bb algo-backtest` backtests the trader's algorithm (`crate::decider`, as
`biz_algo`) on the same 1m cache. It is offline and research only: it reads
only `--cache-dir` (and `--biz-feeds`), never fetches or calls Binance, has
no order path, and leaves the live `bot-spot` untouched. If the cache is
missing a liquid coin, ends before `--end`, or lacks 48h of history before
the window start, it exits 1 with `missing cache`.

```bash
cargo run --release -p bb-cli --bin bb -- algo-backtest \
  --cache-dir data/entry-study-cache --biz-feeds <history dir> \
  --end 2026-10-18T10:43:00Z --days 60
```

The other flags are `--starting-balance` (1000), `--fee-bps` (7.5),
`--slippage-bps` (5), `--step-minutes` (20) and `--output-dir` (`reports`).

The rule: each liquid-top coin scores
`w_talk·talk + w_stance·stance + w_momentum·momentum`
(talk: /biz/ 6h mention rate over its 24h rate − 1, from 4 mentions in 24h,
capped −1..3; stance: (bullish − bearish) / tagged of the 6h AI tags with
`--biz-min-posters` (2) posters; momentum: 1h return / 5%, capped). A held
coin scoring below the sell level is sold; with nothing held the best coin at
or above the buy level is bought with all cash. `--biz-feeds <dir>` is the
hourly `biz-feed-v1` history from biz-app (a feed counts at a step from its
`as_of` on, for `--biz-max-age-minutes`, 90); without it the /biz/ terms are
0. The window is split: the weights are picked on the older half from a grid
(talk and stance −1/0/+1, momentum 0/1, buy at 0.5/1, sell below −0.5/0; 72
sets with feeds) by net %, or given with
`--biz-weights talk,stance,momentum,buy,sell` (e.g. from
`bb biz-lead-study`). The rule then runs on both halves as `biz_algo@older`
and `biz_algo@newer`; the `@newer` row is the out-of-sample result. The
report has a `biz_algo` rule section with the best grid rows, and
`_biz_algo@<half>_trace.csv` (time, feed `as_of`, best coin and score, held
coin and score, action). Not run on real feeds yet: biz-app does not write
the history.

Cost model: fills happen at the next 1m open, with the fee plus slippage
charged on each side (25 bps per round trip by default, against the measured
live 14.8 bps). Buys under 5 USDT are skipped. Equity is marked at the last
close with no exit costs.

Output: `reports/algo_backtest_<end-date>.md` (settings, results,
per-half exit and skip reasons, assumptions, caveats), `.csv` (one row per
half), `_trades.csv` (one row per trade) and `_biz_algo@<half>_trace.csv`
(one row per step). The files are byte-identical
across reruns. The cache comes from `bb entry-study` (`data/` is gitignored).
The liquid universe is the 2026-10-18 top 20, so results carry survivorship
bias.

## Bot Spot Data Backups

Bot Spot's record is files: the decisions
audit (the only record of every trade), trader state, exit-plan book and
watch files (the old journal, omp session directory and entry-timing cache are
no longer archived; earlier archives keep them).
`scripts/backup_bot_data.sh` archives them into
`backups/bot-spot-data-<UTC>-<pid>.tar.gz`. It keeps 14
(`BINANCE_BOT_BACKUP_RETENTION`) and resolves each path from the trader's
`BOT_SPOT_*_PATH` settings. On Docker the `scheduler`
runs it at 23:45 UTC into `docker-backups/`. The archive's paths start with
`data/`, which is `docker-data/` on the Docker host. To restore, stop
`bot-spot`, extract into a temporary directory, copy its `data/` contents over
`docker-data/`, then start `bot-spot`.
