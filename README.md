# Binance Trading Bot

The Bot Spot trader written in Rust: a native Binance Spot REST client,
a bot-driven agentic Spot trader (`bb bot-spot`, LLM or rule-based decider),
its research tools and backtests, and read-only social evidence. It ships as
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
| `bb-omp` | omp RPC client (LLM sessions with persistent context + host tools) |
| `bb-trader` | `bot-spot`: the agentic Spot trader, exit plans, persona backtests, the Markov value model, the rule-based decider |
| `bb-pool` | library for the trader: market catalog, commission/fee service, multi-leg route engine, models (no commands, no order path) |
| `bb-social` | read-only social evidence (Telegram Bot API, official X API) for `get_social_evidence`, `social-signal` |
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
| Social evidence | `social-signal` |

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
- **Schizo trader (Phase 49, prompt `bot-spot-v17-schizo`, operator
  decisions 2026-10-20).** The LLM stays the trader but is told it has four
  personalities (`v13`, `contrarian`, `detached`, `high_variance`, the Phase
  48 personas) and acts out the one in charge. Each cycle the trader reads
  GCP2 (gcp2.net, Global Consciousness Project 2.0 network coherence, public
  and read-only) and works out the schizo state that opens the cycle message
  (`## Your schizo state`):
  - **Mood ring, following GCP2:** when GCP2's colour zone changed (up or
    down) since the previous cycle the mood switches to another personality
    picked by the GCP2 dice (a hash of the GCP2 value, no seed); while the
    zone stays the same, or a reading is missing, the mood holds. No timer.
    The first deploy (`bot-spot-v15-schizo`) ran it inverted: it switched
    while the zone stayed the same.
  - **Voices:** the four personalities' rule votes on the live wallet, the
    mood in charge counting double.
  - **Bets:** one coin with all cash. Each mood has its own exit plan.

  YOLO (all-in on the hottest coin in GCP2's two highest zones) was removed
  on 2026-10-21 by operator decision. A failed GCP2 read (timeout, HTTP
  error, HTTP 429 rate limit) means "GCP2 unavailable" for that cycle: the
  mood holds. The state is recorded as `schizo` in each cycle audit row and
  persisted in `bot-spot-schizo-state.json` next to `BOT_SPOT_STATE_PATH`.
  `BOT_SPOT_GCP2_ENABLED` (default true) switches it; false gives the v14
  prompts. GCP2 has no shown link to prices. See OPERATIONS.md, "Schizo
  trader (Phase 49)".
- **Long-term value voice (Phase 50, operator decision 2026-10-21).** A
  Markov decision process fitted on cached 1m candles (`bb bot-spot-mdp-fit`,
  model file `BOT_SPOT_MDP_MODEL_PATH`, default `data/bot-spot-mdp.json`)
  values holding against selling the held coin, and buying each liquid coin
  against staying in cash, as the discounted sum of 20-minute rewards after
  costs (γ = 0.95 per cycle, about 20 cycles of horizon). States: gain since
  entry, time held, the coin's 1h move, market breadth. The cycle message
  shows it as `## Your long-term value` (audit field `mdp`); the system prompt
  calls it the strongest voice and asks for a written reason to act against
  it. Information, never a gate. `BOT_SPOT_MDP_ENABLED` (default true).
  Offline, `bb bot-spot-persona-backtest --persona schizo_mdp --mdp-model
  <file>` lets the model overrule the schizo votes; the model must be fitted
  on data before the window (`bb bot-spot-mdp-fit --end <time>`).
- **Rule-based decider (Phase 51).** `crate::decider` picks buy, sell or
  hold from fixed rules (no LLM) over /biz/ talk and stance, 1h momentum and
  the long-term value model; `bb bot-spot-persona-backtest --persona
  biz_algo --biz-feeds <history dir>` tunes it on the older half of the
  window and judges it on the newer (OPERATIONS.md, "Persona backtest").
  `BOT_SPOT_DECIDER=algo` (with `BOT_SPOT_ALGO_WEIGHTS`) makes it the
  cycle's decider instead of the LLM; its orders go through `execute_route`
  and every hard-safety check. The default stays `llm`; the compose profile
  `algo-paper` runs it plan-only first (OPERATIONS.md, "Rule-based decider").
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
  It uses the Python schema plus agent fields: tool calls, tokens and cost.
  There are no Telegram or phone alerts (operator decision 2026-10-29); watch
  the dashboard and the audit log.

```bash
bb bot-spot --evidence-only                 # decision context, no LLM call, no orders
bb bot-spot --plan-only                     # The bot decides; execute_route only records plans
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

Exit plans: The bot may give an open position one of these plans with the
`set_exit_plan` host tool (its own parameters; it never writes code; setting
again replaces it, `clear` removes a plan the bot set). Positions opened from
`BOT_SPOT_DEFAULT_EXIT_PLAN_FROM` run `BOT_SPOT_DEFAULT_EXIT_PLAN`
unless the bot sets its own plan. With `BOT_SPOT_EXIT_PLANS_LIVE=true` the
`bot-spot --submit-live --daemon` process follows those positions' bids
every `BOT_SPOT_EXIT_POLL_SECONDS` (5) between cycles and sells a position
into USDT when its plan fires, with the same hard safety as the bot's routes
(audit rows with `cycle_id` `exit-plan`). Otherwise `bb bot-spot-exit-watch`
runs the plans on paper and only records would-be sells. Either way the cycle
prompt shows the bot each plan. Files: `BOT_SPOT_EXIT_PLANS_PATH`,
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
outcome, the failed `attempts` and `fallback_active`. Switches to a fallback
and back are logged as warnings, and an error is logged when
`BOT_SPOT_BRAIN_DOWN_ALERT_CYCLES` (3) consecutive cycles failed on every
model (counter persisted in the agent state
file).

omp needs model credentials: either `ANTHROPIC_API_KEY`, or a one-time
interactive `omp` login (stored under `PI_CODING_AGENT_DIR`).

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
- **The bot:** cycles with the bot's summaries, tool calls and cost, and its
  journal.
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
  (`BOT_SPOT_AUDIT_PATH`, `_JOURNAL_PATH`, `_STATE_PATH`, `_STOP_PATH`, the
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
contains `bb`, `cryptobot-gui`, `omp` (on Bun) and supercronic.

```bash
docker compose build
docker compose run --rm -it bot-spot omp             # one-time omp login (or set ANTHROPIC_API_KEY)
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
