# Binance Bot Operations

The fleet ships as one Rust binary, `bb` (see `README.md`). The active
development runtime is the user-owned, paper-only modular asset pool at
`~/.local/share/binance-bot`. It reads Binance balances, markets, and
commission schedules, then records virtual allocations in
`data/pool-paper.db`. It does not submit exchange orders.
By default it scores every asset represented in the eligible market set;
`POOL_MAX_SCORING_ASSETS` is an optional resource-control override.

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

Bot Spot (`bot-spot`) is the only live trader, Spot only. Since
2026-10-12 every other live runtime is off so nothing else trades the shared
wallet: funding arb (spot + USD-M futures), the listing detector and momentum
listing sniper, and the trend filter. Their `docker/crontab` lines are
commented out, `.env.docker` sets their `*_ENABLED` / `CONFIRM_LIVE_*` /
`*_PROVEN_EDGE` gates off, and the live sniper moved to the opt-in compose
profile `listing-sniper-live`. Bot Spot cannot write margin or futures.

## Safety Posture

The legacy strategy runtime (`bb main`, user unit `binance-bot.service`,
compose service `main-bot`) used fixed-size live BTC order intent. It is
stopped and disabled, and the code refuses fixed-size real-money startup.
Do not enable or restart that service while evaluating the pool.

Verify the boundary:

```bash
systemctl --user show binance-bot.service -p ActiveState -p UnitFileState
systemctl --user show binance-pool-paper.service -p ActiveState -p UnitFileState
```

## Validate And Deploy

Validate in the checkout:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
scripts/smoke.sh
scripts/pool_replay.sh
git diff --check
```

### systemd user host

Deploy without `sudo`:

```bash
scripts/setup_user_deploy.sh
systemctl --user disable --now binance-bot.service
systemctl --user enable --now binance-pool-paper.service
systemctl --user enable --now binance-pool-paper-backup.timer
```

`scripts/setup_user_deploy.sh` builds `bb` in release mode, installs it
atomically to `~/.local/share/binance-bot/bin/bb`, syncs `scripts/`, installs
every unit from `deploy/user/` and reloads the user manager. It preserves
`.env`, databases, logs, and backups, and it starts no trading service. It
warns when `omp` is not on `PATH` (needed by `bot-spot` and
`capital-pool --advise`). Restart running services after a deploy to load the
new binary.

### Docker

```bash
docker compose build
docker compose --profile all up -d
```

Profiles: `live` (bot trader, scheduler), `paper` (paper and signal
workloads, scheduler), `all` (both), `v2` (launch detector), `main-bot`
(legacy SMA runtime), `listing-sniper-live` (live momentum sniper, off).
Periodic jobs (airdrop digest, SQLite backups) run in the `scheduler`
container from `docker/crontab`; the funding-arb, listing-detector and
trend-filter lines are disabled.

## Status And Logs

```bash
cd ~/.local/share/binance-bot
scripts/pool_status.sh
bin/bb bot-status
systemctl --user status binance-pool-paper.service --no-pager
journalctl --user -u binance-pool-paper.service -f
```

Docker: `docker compose ps`, `docker compose logs -f <service>`, and
`docker compose run --rm scheduler bb bot-status`.

Dashboard: the read-only `cryptobot-gui` (compose service `cryptobot-gui`,
profiles `gui`/`paper`/`live`/`all`) serves the same fleet view plus the Bot
Spot trades, cycles (brain, tools, cost per cycle), journal, exit plans
(Exits tab), results per session (Sessions tab: each setup ranked by realized
P&L, the winner, and the session's configuration from the `run_config` the
trader writes on every cycle summary), a market mood gauge on the Overview
(bearish to bullish breadth of the 20 most-traded USDT crypto markets over
15m/1h/4h/24h, from public Binance data), stop-file and failed-route-halt state at
`http://localhost:8787` on the Docker host. It is published on `127.0.0.1`
only and mounts the data dir read-only. From another machine:
`ssh -L 8787:localhost:8787 <host>`. On a systemd host,
`scripts/setup_user_deploy.sh` installs it next to `bb`; run
`cd ~/.local/share/binance-bot && bin/cryptobot-gui`.

Laptop on the Pi's live data: `scripts/cryptobot_gui_pi.sh --open` (the
`cryptobot-gui` fish command). It rsyncs the Pi's `docker-data` into
`~/.cache/binance-bot/pi-data` every 30 s (omp logins, the bot session and
the Telegram session are not copied; nothing is written on the Pi), maps the
trader's file paths and the exit-plan settings from the Pi's `.env.docker`,
and runs the local release build against the mirror with the laptop's
read-only keys for the wallet. Started without Pi data, the page says so
instead of showing empty tabs. Overrides: `PI_HOST`, `PI_DATA_DIR`,
`PI_ENV_FILE`, `MIRROR_DIR`, `MIRROR_SECONDS`.

Run one foreground read-only/paper evaluation cycle when diagnosing catalog or
valuation behavior:

```bash
bb pool --once
```

Read the deployed paper evidence in plain language without changing its
ledger:

```bash
bb pool-report --db-path ~/.local/share/binance-bot/data/pool-paper.db
```

This report shows warm-up progress, virtual holdings, latest hold/rebalance
reasons, eligible candidate coverage, market rejection reasons, and ranked
candidates only after sufficient retained observations exist.

Run a fast local replay when you need quick confidence in routing, fees,
minimum-order gates, and scorer behavior without waiting for real-time paper
cycles:

```bash
scripts/pool_replay.sh
```

Replay uses fixture market data and an isolated SQLite database. It is for
development and regression checks; it is the first live-readiness gate, but it
does not by itself approve live-money pool execution.

Run a fast historical strategy study from public Binance candles for a small,
explicit direct-`USDT` candidate set:

```bash
bb pool-history-replay --balance BNB=0.0169745 \
  --symbol ETH/USDT --symbol SOL/USDT \
  --start 2026-05-20T00:00:00Z --end 2026-05-26T00:00:00Z \
  --db-path /tmp/binance-bot-history-replay.db
bb pool-report --db-path /tmp/binance-bot-history-replay.db
```

The historical command automatically includes direct-`USDT` routes for initial
assets and the `BTC`, `PAXG`, and `XAUT` benchmarks. It reads public candles
only, applies modeled cost inputs, and writes only its specified isolated
database. It is research evidence, not permission to trade or a substitute
for live-market Phase 13 evidence.
If minimum-order holds dominate for a small balance, record that result for the
live-readiness review. Do not enlarge an approved exposure limit solely to make
a trade fit Binance minimum notional rules.

## Emergency Stop Files

Every live-capable runtime checks a stop file and refuses to trade while it
exists. Create it to stop, delete it to resume. Paths are relative to the bot
directory (`~/.local/share/binance-bot/data/...` on systemd hosts,
`docker-data/...` on the Docker host).

| Runtime | Stop file (default) | Override |
|---|---|---|
| `bot-spot` | `data/bot-spot.stop` | `BOT_SPOT_STOP_PATH` |
| `listing-sniper` (flat / trailing / momentum services) | `data/listing-sniper-flat.stop`, `data/listing-sniper-trailing.stop`, `data/listing-sniper-momentum.stop` (bare default `data/listing-sniper.stop`) | `LISTING_SNIPER_EMERGENCY_STOP_PATH` |
| `listing-detector` | `data/listing-detector.stop` | `LISTING_DETECTOR_EMERGENCY_STOP_PATH` |
| `funding-arb` | `data/funding-arb.stop` | `FUNDING_ARB_EMERGENCY_STOP_PATH` |
| `trend-filter` | `data/trend-filter.stop` | `TREND_FILTER_EMERGENCY_STOP_PATH` |
| `mean-reversion` | `data/mean-reversion.stop` | `MEAN_REV_EMERGENCY_STOP_PATH` |
| `spot-canary` | `data/spot-canary.stop` | `SPOT_CANARY_STOP_PATH` |
| `live-spot-allocator` | `data/live-spot.stop` | `LIVE_SPOT_ALLOCATOR_STOP_PATH` |
| `margin-canary` / `futures-canary` / `transfer-canary` | `data/margin-canary.stop`, `data/futures-canary.stop`, `data/transfer-canary.stop` | `--emergency-stop-path` |
| `signals-dexscreener` / `signals-smart-money` / `signals-telegram-ingester` | `data/signals-dexscreener.stop`, `data/signals-smart-money.stop`, `data/signals-telegram.stop` | `SIGNALS_*_STOP_PATH` |
| `sniper-detector` | `data/sniper.stop` | `SNIPER_EMERGENCY_STOP_PATH` |

```bash
touch ~/.local/share/binance-bot/data/bot-spot.stop   # systemd host
touch docker-data/bot-spot.stop                       # Docker host
```

The legacy `bb main` runtime has no stop file: `scripts/panic.sh` sends
`SIGUSR1` to every `bb main` process so its rollback closes tracked positions
before shutdown. The paper pool never submits orders.

## API Permission Diagnostics

Run permission diagnostics before any live-order probe or future live canary:

```bash
bb api-permissions --feature manual_live_probe
bb api-permissions --feature spot_live_canary
bb api-permissions --feature bot_spot_autonomous
bb api-permissions --feature margin_dry_run
bb api-permissions --feature margin_canary_plan
bb api-permissions --feature futures_dry_run
bb api-permissions --feature futures_canary_plan
bb api-permissions --feature transfer_dry_run
bb api-permissions --feature transfer_canary_plan
bb api-permissions --feature unified_pool_dry_run
bb api-permissions --feature symbol_scope_audit
bb api-permissions --feature withdrawal_dry_run
bb api-permissions --feature prediction_dry_run
```

The diagnostic reads `GET /api/v3/account` and uses Binance's safe
`POST /api/v3/order/test` endpoint for spot trading. It does not place an
order. Margin, futures, transfer, and unified-pool dry-run modeling require
reading only in the bot registry; live margin trading, live futures trading,
live universal transfer, prediction trading, and withdrawals remain
policy-blocked even if an API key exposes those permissions.

## Symbol Scope Audit

Binance does not expose the full API-key symbol whitelist through this safe
diagnostic path. Configure the local/operator-owned scope before any
live-capable feature is planned:

```bash
API_SYMBOL_WHITELIST=BNB/USDT,BTC/USDT
API_ASSET_WHITELIST=BNB,USDT
```

Audit requested symbols/assets without exchange writes:

```bash
bb scope-audit --symbol BNB/USDT --asset BNB
```

Use `--fail-on-blocked` in scripts that must stop when local scope is missing
or too narrow.

## Withdrawal Safety Dry Run

The withdrawal module models allowlists, limits, approvals, cooldowns,
emergency-stop state, and operator identity locally. It has no withdrawal
submission path:

```bash
bb withdrawals --dry-run --asset USDT --amount 1 \
  --address TAllowlistedAddress --network TRX \
  --allow-address TAllowlistedAddress --allow-network TRX \
  --operator-id local-review --allowed-operator local-review \
  --approval risk --approval ops
```

`--confirm-live-withdrawal` intentionally fails closed.

## Prediction Dry Run

Prediction modeling consumes a local or injected read-only market payload. It
does not call a prediction stake, claim, or settlement write endpoint:

```bash
bb prediction --dry-run \
  --market-data-file prediction-markets.json --symbol BTC/USDT \
  --direction UP --stake-usdt 1 --bankroll-usdt 100
```

`--confirm-live-prediction` intentionally fails closed.

## Spot Live Canary

The spot live canary is a foreground-only Phase 17 command. Its first version
only supports one direct source-asset sell into `USDT`; it is not a background
allocator and it is not wired to the paper pool (`bb pool`).

Plan the canary without submitting an order:

```bash
bb spot-canary --plan-only
```

The canary can submit a real order only when all confirmation flags are
provided and `.env` has `USE_TESTNET=false`, `DRY_RUN=false`, and
`CONFIRM_LIVE_TRADING=yes`:

```bash
bb spot-canary \
  --confirm-live-canary \
  --confirm-replay-passed \
  --confirm-paper-warmup-reviewed
```

Default limits are intentionally tiny: max order notional `6 USDT`, max daily
notional `6 USDT`, and max daily orders `1`. The canary checks
`data/spot-canary.stop` before exchange reads; create that file to block the
canary. It writes JSONL audit rows to `data/spot-canary.jsonl` by default.

## Guarded Live Spot Allocator

The live spot allocator is a foreground-only Phase 18 command. Its first
version supports one direct spot market order per invocation and must not be
installed as a background service. It exists to execute reviewed allocation
intent with hard risk limits, benchmark valuation, balance reconciliation, safe
Binance order-test preflight, audit logging, and Telegram alerting.

Plan an allocation without submitting an order:

```bash
bb live-spot-allocator --plan-only
```

Submit mode requires every confirmation flag, live-mode environment settings,
passing permission diagnostics, and no emergency stop file:

```bash
bb live-spot-allocator \
  --confirm-live-allocator \
  --confirm-canary-reviewed \
  --confirm-replay-passed \
  --confirm-paper-warmup-reviewed \
  --confirm-permission-diagnostics
```

Default limits are intentionally small: max order notional `6 USDT`, max daily
notional `12 USDT`, max daily orders `2`, direct route only, and BNB reserve
protection. Create `data/live-spot.stop` to block allocator planning and
submission. Audit rows default to `data/live-spot-allocations.jsonl`.

## Bot Agentic Spot Trader

`bb bot-spot` lets the bot trade the Spot wallet through omp (the oh-my-pi
coding agent, npm `@oh-my-pi/pi-coding-agent`) in RPC mode. It does not modify
the paper-only pool.

- **Persistent session.** One long-lived omp session per trader, stored in
  `BOT_SPOT_SESSION_DIR` and resumed after restarts and crashes. omp
  auto-compaction stays on and the bot also compacts at
  `BOT_SPOT_COMPACT_AT_PERCENT` or `BOT_SPOT_COMPACT_AT_TOKENS` (deployed
  150000), logging `compacted trader context tokens_before=… tokens_after=…
  local_tokens_after=… method=… short_summary=…`. `tokens_after` and
  `method` are what omp's `compact` reply carries (omp 18.8-18.12 send
  neither, so `None`); `local_tokens_after` is omp's count of the local
  transcript, which after a remote compaction (`short_summary="Remote
  compaction"`) is only a small placeholder: the provider keeps the history
  and the session file's `compaction` entry records the real `tokensAfter`.
  The bot's theses survive compaction in the journal at
  `BOT_SPOT_JOURNAL_PATH`.
- **Host tools plus web search.** omp runs with all built-in tools disabled
  except `web_search` (`BOT_SPOT_WEB_SEARCH`, default true; set false for
  `--no-tools`). The bot acts through the bot's host tools `get_portfolio`,
  `list_routes`, `get_market_evidence`, `get_klines`, `get_social_evidence`,
  `get_trade_history`, `execute_route`, `journal_read` / `journal_write`, and
  `end_cycle`. `execute_route` is the only order path.

The route table shows two costs per route. "Quoted friction" is what the
order quote reserves: the account's undiscounted taker fee, the
`BOT_SPOT_SLIPPAGE_BUFFER_BPS` buffer (25) and lot rounding. It stays the
basis for order sizing. "Expected cost" is the account's effective taker fee
from `account/commission` (BNB-discounted while the wallet holds BNB) plus
half the spread, per side. Over 40 round trips to 2026-10-18 the real fees
were about 7.5 bps per side, and fills beat the quote by about 25 bps.

Trade memory and the "Your record" section use the same fill-based ledger as
the dashboard (`bb_trader::trades`): each outcome's P&L is net of the buy and
sell fees. "Your record" summarizes the whole audit log every cycle: closed
coin trades, wins, net P&L after fees, fees paid and mean hold, all time and
for the last 7 days, plus entry timing: the mean price move in the 30
minutes before the bot's buys and 20 and 90 minutes after them, from 1-minute
candles against the entry fill. Each finished entry is measured once and kept
in `bot-spot-entry-timing.json` next to the audit log. It is information,
never a gate.

### Trading guardrails removed

By operator decision Bot Spot has no trading guardrails. Fees, spread,
slippage, momentum, market evidence, social data and trade memory are
information the bot weighs; none of them blocks a trade. Removed: the
edge/market-evidence minimum, the friction cap, speculative mode, the
loss-streak limit, the trade cooldown, the daily trade cap, the proven-edge
gate, the rules-engine decision mode, and the LLM `go_off` decision flag.
Their variables (`BOT_SPOT_ALLOW_NEGATIVE_FRICTION_BPS`,
`BOT_SPOT_MARKET_EVIDENCE_MIN_EDGE_BPS`, `BOT_SPOT_SPECULATIVE_*`,
`BOT_SPOT_MAX_RECENT_LOSS_STREAK`, `BOT_SPOT_MIN_TRADE_INTERVAL_SECONDS`,
`BOT_SPOT_MAX_SUBMITTED_TRADES_PER_DAY`, `BOT_SPOT_PROVEN_EDGE`,
`BOT_SPOT_REQUIRE_PROVEN_EDGE`, `BOT_SPOT_DECISION_MODE`, ...) are
ignored; the trader logs one warning listing any that are still set.

Cost control (not a trading gate): `BOT_SPOT_MAX_DAILY_LLM_COST_USD`
(0 = unlimited). Model and thinking level: `BOT_SPOT_MODEL` (default
`anthropic/claude-haiku-5-5`; use provider-qualified ids, a bare `haiku-5.5`
can resolve to another provider), `BOT_SPOT_THINKING` (default `high`).

Model failover: `BOT_SPOT_FALLBACK_MODELS` (comma list of
`provider/model-id[:thinking]`, default empty = off; empty on the Pi since
2026-10-19 by operator decision, so the trader runs on
`openai-codex/gpt-6.1-sol` at `high` alone, with `retry.modelFallback: false`
in `omp-agent/config.yml`; a new fallback needs an operator decision; use the
exact id from
`omp models <provider>` inside the container, the trader's switch does no fuzzy
matching), `BOT_SPOT_PRIMARY_TIMEOUT_SECONDS` (150,
primary per-attempt timeout while fallbacks exist),
`BOT_SPOT_FALLBACK_COOLDOWN_SECONDS` (1800) and
`BOT_SPOT_BRAIN_DOWN_ALERT_CYCLES` (3, 0 = never). A failed turn is retried
on the next model only when the failed attempt made no `execute_route` call
and recorded no `end_cycle`; a failed compaction moves to the next model and
compacts there. Model switches and brain outages are Telegram notices sent
after the cycle's audit row; they change nothing in the hard-safety list
below. A turn that failed on every model holds with
`brain unavailable on all models (no orders placed)` as its reason.

### Hard safety (always on, fail closed)

- Live arming: `BOT_SPOT_ENABLED=true`,
  `BOT_SPOT_DYNAMIC_SCOPE=all_eligible_spot`, `USE_TESTNET=false`,
  `DRY_RUN=false`, `CONFIRM_LIVE_TRADING=yes`.
- The emergency stop file `data/bot-spot.stop` (`BOT_SPOT_STOP_PATH`),
  checked at cycle start, before every route, before every leg and before
  every order. A one-shot run refuses to start under it; the `--daemon`
  keeps running and idles (no cycle, no LLM call, one log line when the stop
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
`data/bot-spot-decisions.jsonl`), written before any Telegram alert, with
tool calls, tokens and cost.

### Commands

```bash
bb bot-spot --evidence-only                 # decision context, no LLM call, no orders
bb bot-spot --plan-only                     # The bot decides; execute_route only records plans
bb bot-spot --submit-live --daemon          # live, one cycle every --interval-seconds (240)
bb bot-spot --mark-failed-route-reviewed --review-reason "reconciled by hand"
```

### Deployment

Bot Spot is a long-running daemon, not a timer:

- Docker: compose service `bot-spot` (profiles `live` and `all`), running
  `bb bot-spot --submit-live --daemon --interval-seconds 1200` (every 20
  minutes since 2026-10-15: Sonnet 5.5 on the subscription cannot sustain
  4-minute cycles).
- systemd: `binance-bot-spot.service` (same command, `Restart=always`):

```bash
systemctl --user enable --now binance-bot-spot.service
journalctl --user -u binance-bot-spot.service -f
```

The service always passes `--submit-live`; whether it trades is decided by the
live arming variables in `.env` and the stop file.

### Exit plans (Phase 45)

The bot may give an open position a sell plan with the `set_exit_plan` tool
(fixed menu: follow the peak by percent or ATR, take part, stretch, slowdown,
momentum fade, deadline; setting again replaces it, `clear` removes it).
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
  route row with `cycle_id` `exit-plan` in the audit and a Telegram alert;
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

### omp authentication

omp needs model credentials once per data directory: either set
`ANTHROPIC_API_KEY` in `.env`, or log in once interactively with `omp`
(`/login`, then exit). omp stores credentials, settings and sessions under
`PI_CODING_AGENT_DIR`; the Docker image sets it to `/app/data/omp-agent`, on
the persistent data volume, so the login survives container recreation.

```bash
docker compose run --rm -it bot-spot omp
```

The brain (`BOT_SPOT_MODEL`, plus `BOT_SPOT_FALLBACK_MODELS` when set) needs
one login per provider in that same directory: `openai-codex` (device flow)
today; `google-antigravity` was the fallback's login until 2026-10-19.
`omp usage` inside the container shows each account's 5-hour and 7-day
allowance.

Google Antigravity redirects the browser to `127.0.0.1:51121` on the machine
running omp, so on the Pi log in from a host-network container and tunnel that
port from the laptop whose browser signs in:

```bash
# laptop terminal 1: forward the callback port to the Pi
ssh -L 51121:127.0.0.1:51121 pi-wired
# in that session, on the Pi:
docker run --rm -it --network host -v ~/binance-bot/docker-data:/app/data \
  binance-bot:latest omp login google-antigravity
# open the printed URL in the laptop browser and sign in; omp prints success
```

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

## Capital Pool, Bot Allocator, Auto-Convert

`bb capital-pool` shows the capital plan and persisted state
(`CAPITAL_POOL_STATE_FILE`, default `data/capital-pool-state.json`): a reserve
(`CAPITAL_RESERVE_PCT`), per-module min/max
(`CAPITAL_MIN_MODULE_ALLOCATION_USD`, `CAPITAL_MAX_MODULE_ALLOCATION_USD`), and
score-weighted splits from ledger scorecards (`--scoring scorecard`, default)
or lifetime PnL (`--scoring lifetime-pnl`). Equity defaults to the Binance
wallet total; override with `--equity-usdt`.

```bash
bb capital-pool
bb capital-pool --advise          # ask the bot allocator once
bb capital-pool --advise --json
```

`--advise` runs the bot allocator, an omp session
(`BOT_ALLOCATOR_MODEL`, `BOT_ALLOCATOR_SESSION_DIR`) with
fleet/scorecard/balance tools. Its proposal can only clamp within the
deterministic bounds; any failure falls back to the deterministic plan. It
needs the same omp authentication as Bot Spot.

With `CAPITAL_POOL_ENABLED=true` the `bb main` runtime requests a lease before
every buy. With `AUTO_CONVERT_TO_QUOTE=true` it market-sells
`AUTO_CONVERT_ASSETS` (bounded by `AUTO_CONVERT_MIN_QUOTE_USD` /
`AUTO_CONVERT_MAX_QUOTE_USD`, buffer `AUTO_CONVERT_BUFFER_PCT`) to cover a
quote shortfall, after an `order/test`. Preview or run a conversion by hand:

```bash
bb auto-convert --amount 20 --symbol ETH/USDT --plan-only   # order/test only (default)
bb auto-convert --amount 20 --symbol ETH/USDT --submit-live # needs CONFIRM_LIVE_TRADING=yes and AUTO_CONVERT_TO_QUOTE=true
```

The target symbol's base asset is never sold. Audit rows go to
`AUTO_CONVERT_AUDIT_PATH`. `bb bot-status` and `bb cash-distributor` report
fleet health and an advisory distribution.

## Listing Detector And Snipers

**Off since 2026-10-12** (see Live Posture). When enabled,
`bb listing-detector --detect-only` polls Binance listing announcements and
arms the shared watchlist `LISTING_WATCHLIST_PATH`
(`data/listing-watchlist.json`) every 2 minutes from the Docker `scheduler`
(`docker/crontab`) or from `binance-listing-detector.timer`.

`bb listing-sniper` diffs exchangeInfo against known symbols plus the
watchlist and fires flat, trailing or momentum round trips
(`LISTING_SNIPER_HOLD_MODE`). Each mode is its own long-running service with
its own state, audit, ledger and stop file:

| Mode | Compose service / systemd unit | Command | Stop file |
|---|---|---|---|
| flat | `listing-sniper-flat` / `binance-listing-sniper-flat.service` | `bb listing-sniper --paper --daemon` | `data/listing-sniper-flat.stop` |
| trailing | `listing-sniper-trailing` / `binance-listing-sniper-trailing.service` | `bb listing-sniper --paper --daemon` | `data/listing-sniper-trailing.stop` |
| momentum | `listing-sniper-momentum` / `binance-listing-sniper-momentum.service` | `bb listing-sniper --submit-live --daemon` | `data/listing-sniper-momentum.stop` |

The flat and trailing services are in the `paper`/`all` profiles; momentum is
in the opt-in `listing-sniper-live` profile (off). A live fire needs `LISTING_SNIPER_ENABLED=true`,
`CONFIRM_LIVE_LISTING_SNIPER=yes` and `LISTING_SNIPER_PROVEN_EDGE=yes`, and is
bounded by `LISTING_SNIPER_NOTIONAL_USDT` and
`LISTING_SNIPER_DAILY_SPEND_CAP_USDT`. Without those gates the momentum service
does not fire live. Stop one sniper:

```bash
touch docker-data/listing-sniper-momentum.stop
docker compose stop listing-sniper-momentum
```

## Telegram Signal Login

The Telegram signal ingester (`bb signals-telegram-ingester`, compose service
`signals-telegram`) reads channels as a Telegram user account. The Rust client
uses a new session file, `data/telegram-session.grammers.json`
(`TELEGRAM_SESSION_PATH`); the old `data/telegram-session.session` file is
never read, so log in once again after the cutover. `TELEGRAM_API_ID` and
`TELEGRAM_API_HASH` (from my.telegram.org/apps) must be set; the phone number
comes from `TELEGRAM_PHONE` or a prompt, followed by the login code and, for
2FA accounts, the cloud password.

```bash
docker compose run --rm -it signals-telegram bb signals-telegram-setup   # Docker
bb signals-telegram-setup                                                # bot directory, interactive shell
docker compose up -d signals-telegram
```

The ingester refuses to start until the session file exists. Its stop file is
`data/signals-telegram.stop`.

## Trend Filter

**Disabled since 2026-10-12.** The scheduler line in `docker/crontab` is
commented out and `.env.docker` sets `TREND_FILTER_ENABLED=false` and
`TREND_FILTER_PROVEN_EDGE=no`. A backtest of the live config (top-1, 90-day
lookback, 30-day rebalance, 11-coin universe) lost to holding BTC: from
2023-01 +155% vs +399%; from 2025-01 −35% vs −12% (`no_edge`), with ~69% max
drawdown. Re-enable only after `bb trend-filter-backtest` shows a real edge.

When enabled, `bb trend-filter --submit-live` runs daily (00:30 UTC) and
rebalances every `TREND_FILTER_REBALANCE_EVERY_DAYS` into the top
`TREND_FILTER_TOP_K` assets by trailing return. Its tracked holdings live in
`data/trend-filter-state.json`; every order attempt goes to
`data/trend-filter-ledger.jsonl`.

- A tracked holding worth less than `TREND_FILTER_MIN_ORDER_NOTIONAL_USDT` is
  released (`release_dust`): Binance rejects such sells (`-1013 NOTIONAL`), so
  the strategy stops tracking it and the coins stay in the wallet as dust.
- Live buys spend at most the USDT actually free after the sells (the plan
  assumes sells fill at the signal price); a buy that would fall below the
  minimum notional is skipped and ledgered as failed.
- Any failed or skipped order keeps the previous rebalance timestamp, so the
  next daily run retries instead of waiting a full rebalance period.

The trend filter sizes from the shared wallet's free USDT, which Bot Spot
also trades.

## Delta-Neutral Funding-Rate Harvest

**Off since 2026-10-12** (see Live Posture): its scheduler line is commented
out and `FUNDING_ARB_ENABLED=false`.

`bb funding-arb-backtest` is the research harness; `bb funding-arb` is the
runtime (crate `bb-strategies`). Strategy: long spot + short USD-M perp 1:1,
harvest the funding rate every 8h, close when funding falls below the exit
threshold, basis blows out, or max-hold elapses.

### Backtest first

```bash
bb funding-arb-backtest \
  --universe perp_usdt_top \
  --universe-size 60 \
  --min-quote-volume-usdt 50000000 \
  --start 2025-12-01 --end 2026-05-30 \
  --funding-cache /tmp/funding-cache \
  --entry-funding-bps 3 --exit-funding-bps -1 \
  --slippage-per-leg-bps 5 \
  --output data/funding-backtest.json
```

Recent 6-month verdict on top-60 perps at 5 bps/leg slippage:
`edge_likely`, +5.84% return, Sharpe 5.4, 59% win rate, max drawdown 0.39%.
Majors-only over the same window: `no_edge` (-9.1%) — alt funding is where
the edge lives.

### Runtime modes

`bb funding-arb` has three modes selectable via mutually-exclusive flags:

| Flag             | Behavior                                                                |
| ---------------- | ----------------------------------------------------------------------- |
| `--plan-only`    | Compute decisions, audit them. Do not mutate state. Default.            |
| `--paper`        | Materialize decisions in `data/funding-arb-state.json`, accrue funding, write closed trades to `data/funding-arb-ledger.jsonl`. No real orders. |
| `--submit-live`  | Execute dual-leg spot+perp orders via the funding-arb executor. Reconciles vs on-exchange state first; halts on mismatch. Behind every preflight gate. |

```bash
bb funding-arb --plan-only
bb funding-arb --paper
```

The paper layer accrues funding **once per cycle** at the current rate.
Run it on an 8h schedule aligned with Binance funding windows (00, 08, 16 UTC)
for a fidelity match to the backtest model.

### Scheduling

systemd: `deploy/user/binance-funding-arb.{service,timer}` are installed by
`scripts/setup_user_deploy.sh`. The timer fires at 00:01, 08:01, 16:01 UTC —
one minute after each Binance funding event so the just-applied rate is
captured in the cycle's signal snapshot. The service invokes `--paper`.
**Do not edit ExecStart to `--submit-live` until you have at least 1-2 weeks
of paper ledger data showing the runtime fires correctly and the realized P&L
roughly tracks the backtest's expectation.**

```bash
systemctl --user enable --now binance-funding-arb.timer
journalctl --user -u binance-funding-arb.service -f
```

Docker: the `scheduler` runs `bb funding-arb --submit-live` at the same
times; it stays inert until the live gates below are set and the wallet clears
`FUNDING_ARB_MIN_BALANCE_USDT` (default 50).

### Live-submit gates

`--submit-live` requires every gate below:

```bash
FUNDING_ARB_ENABLED=true
CONFIRM_LIVE_FUNDING_ARB=yes
USE_TESTNET=false
DRY_RUN=false
FUNDING_ARB_PROVEN_EDGE=yes   # the dedicated gate
```

`FUNDING_ARB_PROVEN_EDGE=yes` should only be set after the backtest harness
reports `edge_likely` *under conservative cost assumptions* and the operator
has reviewed at least 1-2 weeks of paper-mode ledger data on the same
universe.

### Live executor: dual-leg invariants

The funding-arb executor enforces the following invariants:

* **Open**: spot BUY runs first, then perp SHORT. If the perp leg fails
  (network, rejection, zero fill), the executor immediately places a spot
  SELL to flatten before raising. If the rollback SELL itself fails, the
  error text contains `manual reconciliation required` and the
  position is **not** added to local state — operator must reconcile by
  hand.
* **Close**: perp BUY (with `reduceOnly=true`) runs first to release
  margin, then spot SELL. If the spot leg fails after the perp leg
  succeeds, the error text reflects this and the operator must sell
  the spot leg manually.
* **Reconcile**: every live cycle compares on-exchange spot free balances
  and futures position sizes to local state *before* processing decisions.
  Any mismatch halts the cycle — no orders are sent.
* **Collateral**: USDT is transferred spot→futures before each open via
  `sapi/v1/asset/transfer` (type `MAIN_UMFUTURE`) and back after each
  close (type `UMFUTURE_MAIN`). Failed sweeps are non-fatal and logged.

### Unmodeled risks (read before going live)

1. **Spot/perp basis tracking error.** The backtest assumes perfect delta
   neutrality. In practice the spot leg's price may diverge from the perp
   mark by 10-50 bps even on liquid pairs.
2. **Liquidation risk on the short perp** if basis spikes against you on
   isolated margin. Use cross-margin or set
   `FUNDING_ARB_BASIS_CIRCUIT_BREAKER_BPS` tight enough that the circuit fires
   before liquidation does.
3. **Survivorship bias.** The `--universe perp_usdt_top` selector ranks
   by *current* 24h volume; coins delisted during the backtest window are
   excluded, biasing the historical result upward.
4. **Funding rate caps.** Binance caps positive funding at the venue's
   maximum; truly extreme funding (e.g. 75 bps/event) is rare and the
   historical record may be capped lower than the on-chain reality.

## Margin Dry Run

The margin module is Phase 19 dry-run modeling only. It may read Binance margin
account, max-borrow, and next-hour interest data, then locally models borrow
sizing, debt, interest, repayment, margin level, and liquidation buffer. It has
no borrow, repay, margin-order, or margin-transfer submission path.

Run discovery/modeling without live write actions:

```bash
bb margin --dry-run --asset USDT --borrow-amount 10
```

Isolated margin modeling requires a symbol:

```bash
bb margin \
  --dry-run \
  --account-type isolated \
  --isolated-symbol BNB/USDT \
  --asset BNB \
  --asset-usdt-price 600 \
  --borrow-amount 0.01
```

The `--confirm-live-margin` flag intentionally fails. Live margin activation is
future work and requires a separate approval phase.

## Margin Canary Plan

The Phase 24 canary slice is foreground and plan-only. It enforces local
symbol scope, margin risk limits, a documented repayment intent, an emergency
stop, and JSONL audit output. It does not expose margin submission:

```bash
bb margin-canary --plan-only \
  --margin-symbol BNB/USDT --asset USDT \
  --borrow-amount 1 --repay-amount 1
```

`--submit-action` and `--confirm-live-margin` intentionally fail closed.

## Futures Dry Run

The futures module is Phase 20 USD-M dry-run modeling only. It may read Binance
USD-M account, position-risk, exchangeInfo, premiumIndex funding/mark price,
symbol-config, and leverage-bracket data, then locally models leverage, margin
mode, funding, liquidation distance, reduce-only behavior, precision/minimums,
and position reconciliation. It has no futures order, leverage-change,
margin-mode, position-mode, or position-margin submission path.

Run discovery/modeling without live write actions:

```bash
bb futures \
  --dry-run \
  --symbol BTC/USDT \
  --side BUY \
  --quantity 0.001
```

The `--confirm-live-futures` flag intentionally fails. Live futures activation
is future work and requires a separate approval phase.

## USD-M Futures Canary Plan

The Phase 25 canary slice is foreground and plan-only. It requires a
reduce-only exit intent and audits risk/reconciliation output without exposing
a futures order path:

```bash
bb futures-canary --plan-only --symbol BTC/USDT \
  --side BUY --quantity 0.001 --exit-side SELL --exit-reduce-only \
  --exit-trigger "operator approved close"
```

`--submit-order` and `--confirm-live-futures` intentionally fail closed.

## Transfer Dry Run

The transfer module is Phase 21 dry-run planning only. It may read Binance
wallet summary, spot user assets, funding assets, margin account, USD-M futures
account, and transfer history data, then locally models a proposed internal
transfer with route constraints, source/destination balances, wallet activation,
margin debt checks, and resulting allocation state. It has no universal-transfer
submission path.

Plan and audit an internal transfer without live write actions:

```bash
bb transfers \
  --dry-run \
  --source-wallet spot \
  --destination-wallet funding \
  --asset USDT \
  --amount 5
```

The `--confirm-live-transfer` flag intentionally fails. Live transfer automation
is future work and requires a separate approval phase.

## Universal Transfer Canary Plan

The Phase 26 canary slice is foreground and plan-only. It enforces local asset
scope, wallet/balance/debt limits, isolated symbol requirements, emergency
stop, and audit output without exposing transfer submission:

```bash
bb transfer-canary --plan-only \
  --source-wallet spot --destination-wallet funding --asset USDT --amount 1
```

`--submit-transfer` and `--confirm-live-transfer` intentionally fail closed.

## Unified Pool Valuation

The unified pool module is Phase 22 local valuation only. It combines spot,
funding, margin, futures, transfer-availability, debt, interest, open PnL,
reserves, leverage, liquidity, volatility, and benchmark prices into one dry-run
allocation report. It does not submit orders, margin actions, futures actions,
or transfers.

Value a local holdings/prices fixture:

```bash
bb pool-unified \
  --dry-run \
  --holdings-json holdings.json \
  --prices-json prices.json
```

## Advanced Product Modules

The Phase 23 product-module registry is local metadata and gating only. It
tracks prediction trading, withdrawals, live margin/futures/transfer activation,
and future Binance products as optional modules. API-key permission alone never
authorizes these modules.

Inspect the registry without exchange writes:

```bash
bb product-modules
```

Every module must define its strategy interface, permission mapping, market-data
source, risk model, audit trail, tests, and activation gate before a future
activation phase can be considered.

Review advanced-product evidence and the serial live-canary rule locally:

```bash
bb advanced-readiness
```

## Manual Live Probe

The one-shot live probe is a separate foreground real-order test. It submits at
most one direct `ASSET/USDT` market sell, writes `data/live-probes.jsonl`, sends
a Telegram message labeled `LIVE PROBE`, and exits. Before submitting the real
order, it runs Binance's safe spot order-test endpoint for the selected order.
It is not a pool allocator and must not be run as a service.

Preflight without placing an order:

```bash
bb live-probe
```

The preflight must fail closed unless `--confirm-real-order` is present. Only
run the real probe after confirming `.env` has `USE_TESTNET=false`,
`DRY_RUN=false`, and `CONFIRM_LIVE_TRADING=yes`:

```bash
bb live-probe --confirm-real-order
```

The pool ledger records the latest full catalog decision set, per-cycle
eligibility summaries, a rolling benchmark-observation window, virtual
holdings, historical portfolio snapshots, and paper decisions. With the
default 15-minute cycle and `POOL_MOMENTUM_OBSERVATIONS=96`, a `hold` decision
is expected for roughly the first 24 hours and whenever a route/risk gate does
not pass.

## Paper Ledger Backups

The paper backup timer retains 35 verified daily SQLite snapshots, enough to
protect recent paper evidence, canary analysis, and rollback investigations.
On Docker the `scheduler` runs the same backups nightly (`docker/crontab`).
The backup/restore scripts resolve the database setting (`DB_PATH`, or the
key named by `BINANCE_BOT_DB_SETTING_KEY`) from the process environment first,
then `.env`, then the default; in Docker that means `.env.docker`'s values.

```bash
systemctl --user status binance-pool-paper-backup.timer --no-pager
BINANCE_BOT_DB_SETTING_KEY=POOL_DB_PATH \
BINANCE_BOT_DB_DEFAULT_PATH=data/pool-paper.db \
~/.local/share/binance-bot/scripts/backup_db.sh
```

Restore paper state only while the paper service is stopped:

```bash
systemctl --user stop binance-pool-paper.service
BINANCE_BOT_SERVICE_NAME=binance-pool-paper.service \
BINANCE_BOT_SERVICE_SCOPE=user \
BINANCE_BOT_DB_SETTING_KEY=POOL_DB_PATH \
BINANCE_BOT_DB_DEFAULT_PATH=data/pool-paper.db \
~/.local/share/binance-bot/scripts/restore_db.sh \
~/.local/share/binance-bot/backups/pool-paper-YYYYMMDDTHHMMSSZ-PID.db
systemctl --user start binance-pool-paper.service
```

### Bot Spot data backups

The SQLite backups do not cover Bot Spot. Its record is files: the decisions
audit (the only record of every trade), journal, agent state, exit-plan book
and watch files, the entry-timing cache, and the omp session directory.
`scripts/backup_bot_data.sh` archives them into
`backups/bot-spot-data-<UTC>-<pid>.tar.gz`. It keeps 14
(`BINANCE_BOT_BACKUP_RETENTION`) and resolves each path from the trader's
`BOT_SPOT_*_PATH` / `BOT_SPOT_SESSION_DIR` settings. On Docker the `scheduler`
runs it at 23:45 UTC into `docker-backups/`. The archive's paths start with
`data/`, which is `docker-data/` on the Docker host. To restore, stop
`bot-spot`, extract into a temporary directory, copy its `data/` contents over
`docker-data/`, then start `bot-spot`.

## Legacy Live State

Historic live-order data (`data/live.db`) and its backups are retained for
audit/recovery. The legacy `binance-bot.service` user unit and the `main-bot`
compose service must remain inactive and disabled. The manual live probe,
spot canary, and guarded live spot allocator are foreground-only tools; none
may be run as a background automatic asset-conversion runtime.

## Live Readiness Gate

This gate governs real paper-pool reallocation. It does not apply to Bot
Spot, which is a separately approved live runtime (see above).

Do not implement or activate real pool reallocations until all of these gates
are complete and explicitly approved as a separate live-execution phase:

- The replay suite passes default and stress fixtures.
- The paper pool runs on current live Binance data through scorer warm-up and
  records current benchmark, drawdown, route-cost, and eligibility behavior.
- API permission diagnostics pass for the exact feature being activated.
- A tiny spot live canary has passed max order notional, max daily notional,
  max daily order count, emergency stop, audit, reconciliation, and Telegram
  alert checks.
- The canary uses dynamic balances, Binance market rules, authenticated fees,
  precision, minimum notional, spread, liquidity, and slippage limits.
- Any permission error, unexpected route, audit failure, balance mismatch,
  slippage breach, or ambiguous exchange response stops live execution.
