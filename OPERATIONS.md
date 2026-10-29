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
service. It warns when `omp` is not on `PATH` (no longer needed; removed in Phase 52). Restart
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
profiles `gui`/`live`/`all`) serves the Bot Spot trades, cycles (brain, tools,
cost per cycle), journal, exit plans (Exits tab), results per session
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
`~/.cache/binance-bot/pi-data` every 30 s (omp logins, the bot session and
the Telegram session are not copied; nothing is written on the Pi), maps the
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

### Schizo trader (Phase 49)

Phase 52 removes this; since the LLM trader is gone (2026-10-29) the prompt and tool parts below no longer run.

Prompt `bot-spot-v17-schizo` (deployed 2026-10-21; v15 and v16 ran on
2026-10-20; operator decisions in
`docs/decisions.md`). The LLM stays the trader and acts only
through the host tools; the hard safety above and the default exit plan
`atr=3.5:1h,deadline=360` are unchanged, and there is no panic sell. It runs
on the full wallet with no size cap. Switch: `BOT_SPOT_GCP2_ENABLED` (default
true); `BOT_SPOT_GCP2_BASE_URL` (default `https://gcp2.net`).

- **Live GCP2 reader.** Once per cycle the trader GETs the bearer token GCP2
  publishes in `/js/data/api_token.js` (cached, fetched again once on HTTP
  401/403), then `/api/getNetVarAggregate24H` (the last 24 h of per-minute
  values) and `/api/getcolorzones` (GCP2's own zone bounds, never constants in
  code), each with a 10 s timeout. Read-only: no login, no posts. The
  `--evidence-only` payload shows it as `gcp2`.
- **GCP2 unavailable.** Any failed read (timeout, HTTP error, bad data, or
  HTTP 429 `Too Many Requests`, seen live at 2026-10-20 12:03 UTC) logs one
  `WARN GCP2 unavailable this cycle` and the cycle runs without GCP2: the
  mood holds (`mood_step` `no reading`), there is no dice roll,
  and the next cycle with a reading also holds because it has nothing to
  compare with. The bot still trades on its own judgement. Whether 429s
  recur is not known yet; watch for repeated warnings.
- **Schizo state.** Each cycle it applies the backtest `schizo` rules
  (`persona_backtest::personas`) to live inputs:
  - the mood (one of `v13`, `contrarian`, `detached`, `high_variance`)
    follows GCP2: it switches to the personality picked by the GCP2 dice when
    the zone changed (up or down) since the previous cycle, and holds while
    the zone stays the same or a reading is missing; the first reading picks
    it (v15 ran this inverted: switch on the same zone, hold on a change);
  - the dice are the SplitMix64 finalizer of the latest minute's 4-decimal
    value (no seed);
  - the four personalities vote on the live wallet (the breadth candles and
    24h tickers the cycle already reads; holdings worth less than their USDT
    market's minimum order count as flat), the mood in charge counting double.
  - YOLO (all-in on the hottest liquid coin in GCP2's two highest zones) was
    removed on 2026-10-21 by operator decision.

  The cycle message opens with `## Your schizo state` (mood and why, GCP2
  value, zone, last cycle's zone and 24h range, dice, each voice and the vote
  verdict). The system prompt describes
  the four personalities and their exit plans; `detached` keeps the default
  plan and sells by hand.
- **State file.** `bot-spot-schizo-state.json` in the directory of
  `BOT_SPOT_STATE_PATH` (`docker-data/` on the Pi): mood and the cycle's zone
  (older files with `yolo` fields still load). It is saved after the cycle's audit row and read at the
  next cycle, so the mood survives restarts. A missing file starts fresh; an
  unreadable one logs an ERROR and that cycle runs without a schizo state
  (back it up and fix or delete it). `--evidence-only` never writes it.
- **Audit field.** Each cycle summary carries `schizo`: `mood`,
  `previous_mood`, `mood_step` (`initial`, `switched`, `held`, `no reading`),
  `mood_reason`, `gcp2` (`value`, `zone`, `previous_zone`, `end_s`; `null`
  when unavailable), `dice`, `votes` (`by_persona` with `weight`, `buy`,
  `sell`, plus weighted `buy`/`sell` tallies), `vote_buy`, `vote_sell`.
  Recent moods and the long-term value verdict:

```bash
tail -n 50 docker-data/bot-spot-decisions.jsonl | python3 -c 'import json,sys; [print(r["timestamp"][:19], r["prompt_version"], (r.get("schizo") or {}).get("mood"), (r.get("schizo") or {}).get("mood_step"), ((r.get("schizo") or {}).get("gcp2") or {}).get("zone"), ((r.get("mdp") or {}).get("held") or {}).get("verdict"), r["decision"]["action"]) for r in map(json.loads, sys.stdin) if r["row_type"] == "cycle_summary"]'
```

- **Turn off the schizo voice, same image.** Set `BOT_SPOT_GCP2_ENABLED=false`
  and `BOT_SPOT_PROMPT_VERSION=bot-spot-v14-agentic` in `.env.docker` (back
  it up first) and recreate `bot-spot`: no GCP2 reads, no schizo state or
  audit field, and the v14 system and cycle prompt text. The code stays the
  v17 build.
- **Roll back (image and env).** The v16 image (with YOLO, no long-term value
  voice) is tagged `binance-bot:pre-v17-20261021`; the v15 image (inverted mood) is tagged
  `binance-bot:pre-v16-20261020`; the v14 image is tagged
  `binance-bot:pre-c4-20261020` and the env before the first schizo deploy is
  `.env.docker.bak-20261020T100630Z-c4`; the first v15 image (before the
  `detached` wording fix) is `binance-bot:pre-c4b-20261020`. Follow the
  Bot Spot recreate rules: recreate only right after a `cycle_summary` row
  lands in `docker-data/bot-spot-decisions.jsonl` and at least 4 minutes
  before the next cycle, never while a `route` row is waiting for its
  `cycle_summary`, preferably with no open position, never with the stop file
  while a position is open, and never by rebooting the Pi. Then:

```bash
cd ~/binance-bot
cp -p .env.docker .env.docker.bak-$(date -u +%Y%m%dT%H%M%SZ)-pre-rollback
docker tag binance-bot:pre-c4-20261020 binance-bot:latest
cp -p .env.docker.bak-20261020T100630Z-c4 .env.docker
docker compose up -d --force-recreate bot-spot
docker logs bot-spot 2>&1 | grep -E 'keeping the cycle cadence|live exit plans enabled'
```

  The next cycle rows then carry `bot-spot-v14-agentic` and no `schizo`.
  `bot-spot-schizo-state.json` stays on disk unused.

  Back to v15 only (the inverted mood): the same recreate rules, then
  `docker tag binance-bot:pre-v16-20261020 binance-bot:latest`, restore
  `.env.docker.bak-20261020T142041Z-v16` (it has
  `BOT_SPOT_PROMPT_VERSION=bot-spot-v15-schizo`) and
  `docker compose up -d --force-recreate bot-spot`. The schizo state file is
  shared by both rules and needs no change.

### Long-term value voice (Phase 50)

Phase 52 removes this; since the LLM trader is gone (2026-10-29) the prompt and tool parts below no longer run.

Operator decisions 2026-10-21 (`docs/decisions.md`): a reward-and-discount ("Markov")
voice with γ = 0.95 per 20-minute cycle, shown to the LLM as its strongest
voice; information, never a gate. Switch: `BOT_SPOT_MDP_ENABLED` (default
true); model file `BOT_SPOT_MDP_MODEL_PATH` (default
`data/bot-spot-mdp.json`, `docker-data/bot-spot-mdp.json` on the Pi).

- **Model.** `bb bot-spot-mdp-fit --cache-dir data/entry-study-cache --output
  data/bot-spot-mdp.json` (offline, seconds). Every liquid coin at every
  20-minute step of the cache is an entry followed for 24 h. State: gain since
  entry (`<-3, -3..-1, -1..0, 0..1, 1..2, 2..4, >=4` %), time held (`<1, 1-3,
  3-6, >=6` h), the coin's 1h move (`<-1, -1..0, 0..1, >=1` %), market breadth
  (`<-15, -15..15, >=15`). Hold pays the next 20-minute return; sell pays
  minus the exit cost (fee plus slippage, 0.125% by default). Value iteration
  with `--gamma` (0.95). A full state seen fewer than 100 times, a coin without
  a 1h move or a market without breadth falls back to gain and time held only.
  `--test-days 20` (default) also fits on the earlier days and follows the
  model on the last 20, written as `evaluation` in the file and printed.
- **Each cycle.** The trader reads the file and values the open position
  (trade memory: entry value, current value, entry time) and buying each
  liquid-top coin now (value after the entry cost; cash is 0). The cycle
  message shows `## Your long-term value`; the audit row carries `mdp`
  (`status` `ready` with `held` and `buys`, or `unavailable` with the
  reason, which also logs `WARN MDP voice unavailable this cycle`).
- **Refit** with fresh candles: extend the cache with `bb entry-study`, rerun
  the fit, copy the file to `~/binance-bot/docker-data/bot-spot-mdp.json`
  (back up the old one). The trader reads it at the next cycle; no restart.
- **What it says.** On the 2026-07-20 → 2026-10-18 cache almost every held
  state values holding above selling: with γ < 1 a sale costs its exit fee now
  while holding defers it, so the model only sells states whose expected next
  steps lose clearly (e.g. a +4% spike within the first hour). Its main
  effect is patience: it argues against selling on a red 15 minutes.

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
`talk,stance,momentum,value,buy_above,sell_below` (momentum is the coin's own
1h move; stances need `BOT_SPOT_ALGO_MIN_POSTERS`, 2), sells a held coin
scoring below the sell level into USDT and, with nothing held, buys the best
coin with all USDT at or above the buy level. Every order is quoted for the
whole free balance and sent through `execute_route`, so the stop file,
arming, scope, `order/test`, reconciliation and the failed-route halt apply;
the first failed order ends the cycle's orders. Without a fresh feed nothing
is a buy candidate, so it can only hold or sell; a coin whose price read fails
is left out of that cycle.

- **Records.** Cycle summaries carry `algo` (rule, best candidate and score,
  held coins and scores, orders), `agent.model` `rule-based decider` and
  `session_id` `algo`. The Bot tab's cycle details show the rule's scores.
- **Practice run.** `docker compose --profile algo-paper up -d
  bot-spot-algo-paper` runs it plan-only with its own files
  (`bot-spot-algo-paper-{decisions.jsonl,journal.jsonl,state.json}`, lock
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

#### Persona backtest

`bb bot-spot-persona-backtest` (Phase 48) backtests deterministic rule
personas on the same 1m cache. It is offline and research only: it reads only
`--cache-dir` and `--gcp2-cache-dir`, never fetches or calls Binance, has no
order path, and leaves the live `bot-spot` untouched. If the cache is missing
a liquid coin, ends before `--end`, or lacks 48h of history before the window
start, it exits 1 with `missing cache`.

```bash
cargo run --release -p bb-cli --bin bb -- bot-spot-persona-backtest \
  --cache-dir data/entry-study-cache --gcp2-cache-dir data/gcp2-cache \
  --end 2026-10-18T10:43:00Z --days 60 --persona all --persona schizo
```

The other flags are `--starting-balance` (1000), `--fee-bps` (7.5),
`--slippage-bps` (5), `--step-minutes` (20) and `--output-dir` (`reports`).
`--persona` repeats and defaults to `all` (the six Phase 48 variants;
`schizo` is named on its own).

GCP2 history for `schizo` comes from `bb gcp2-history` (public download form,
no login; it also saves GCP2's colour zones as `colorzones.json` with their
fetch time):

```bash
bb gcp2-history --year 2026 --month 8 --month 9 --month 10 --cache-dir data/gcp2-cache
```

It writes `GCP2_Network_Coherence_Global_Network_<YYYY>_<MM>.csv` (one row per
second) per month. `--gcp2-cache-dir` pointing at a missing directory gives
every persona no GCP2 view (`schizo` then never rolls); a directory without
its CSVs or `colorzones.json` is an error.

Personas:

- `v13` is a rule proxy of the live v13/v14 guidance, not the LLM. It trades
  the liquid top 10 and buys when flat, breadth is at least +15 and the 4h
  market median is at least +1%. Exits use `atr=3.5:1h,deadline=360`, or a
  sell when breadth falls to −15 or below.
- `contrarian` buys the worst 4h coin when breadth is −15 or below. It sells
  on `stretch=2:1,deadline=360`, a +1.5% 1h bounce, or breadth of +15 or more.
- `detached` trades all cached coins on 1h/15m price only, with no breadth,
  volume filter or exit plan. It buys the best 1h coin above +0.25% while its
  15m is rising, sells when 15m and 1h are both negative, and rotates when
  another coin leads by more than 0.5 points.
- `high_variance` holds up to 3 positions, buying any coin with a 1h move of
  at least +0.5%. Each position is sized `clamp(|1h| / 2%, 0.25, 1.0)` of
  equity and exits only on `atr=6:1h,deadline=1440`.
- `rotation` shadow-runs the four base personas and, at the first step of
  each UTC day, follows the best trailing-72h shadow (`v13` on ties or with
  less than 72h of history). A switch step only sells.
- `ensemble` buys the coin with at least 2 of 4 base-persona buy votes. It
  sells on 2 sell votes or the `v13` exit plan.
- `schizo` (Phase 49, the live schizo rules) runs the four base personas in
  one head ruled by GCP2: the mood switches by the GCP2 dice when the GCP2
  zone changes between steps and holds while it stays the same; the personas
  vote (the mood counts twice, buy or sell at 2), one coin with all cash, and
  a position keeps the exit plan of the mood that bought it. On the 60 days
  to 2026-10-18 it lost 93.53% (`reports/schizoid_backtest_2026-10-18.md`;
  with YOLO, removed 2026-10-21, it lost 94.03%; the inverted v15 rule 96.71%).
- `schizo_mdp` (Phase 50, not part of `all`, needs `--mdp-model`) is `schizo`
  with the long-term value model as the strongest voice: a vote sell goes
  through only when the model values selling above holding, the model sells
  on its own when it does, a buy needs a long-term value above 0 after its
  entry cost, and a `detached` buy runs the live default plan. The model must
  be fitted on data that ends before the window (the command refuses
  otherwise). Out of sample, fit to 2026-09-18 and tested on the 30 days to
  2026-10-18 (`reports/mdp/`, model `reports/mdp/mdp_model_to_2026-09-18.json`):

  | Variant | Net % | Max DD % | Trips | Win % | Fees USDT |
  |---|---:|---:|---:|---:|---:|
  | v13 | −32.23 | 37.80 | 44 | 38.6 | 57.89 |
  | high_variance | −26.82 | 40.11 | 76 | 43.4 | 43.50 |
  | schizo | −82.41 | 83.01 | 614 | 37.3 | 405.51 |
  | schizo_mdp | −31.61 | 43.56 | 75 | 58.7 | 106.40 |

  The model vetoed 1,515 vote sells and 12 buys and sold twice itself; most
  exits were the exit plans' deadlines. Every variant lost in that window.
- `biz_algo` (Phase 51, not part of `all`, research only) is the rule-based
  decider `crate::decider`, no LLM: each liquid-top coin scores
  `w_talk·talk + w_stance·stance + w_momentum·momentum + w_value·value`
  (talk: /biz/ 6h mention rate over its 24h rate − 1, from 4 mentions in 24h,
  capped −1..3; stance: (bullish − bearish) / tagged of the 6h AI tags with
  `--biz-min-posters` (2) posters; momentum: 1h return / 5%, capped; value:
  the `--mdp-model` verdict, 0 without one). A held coin scoring below the
  sell level is sold; with nothing held the best coin at or above the buy
  level is bought with all cash; no exit plan. `--biz-feeds <dir>` is the
  hourly `biz-feed-v1` history from biz-app (a feed counts at a step from its
  `as_of` on, for `--biz-max-age-minutes`, 90); without it the /biz/ terms
  are 0. Selecting `biz_algo` splits the window: the weights are picked on
  the older half from a grid (talk and stance −1/0/+1, momentum 0/1, value
  0/1, buy at 0.5/1, sell below −0.5/0; 72 sets with feeds and no model) by
  net %, or given with `--biz-weights talk,stance,momentum,value,buy,sell`
  (e.g. from `bb biz-lead-study`); every persona then runs on both halves as
  `<persona>@older` and `<persona>@newer`, and the `@newer` row is the
  out-of-sample result. The report adds a `biz_algo` rule section with the
  best grid rows, and `_biz_algo@<half>_trace.csv` (time, feed `as_of`, best
  coin and score, held coin and score, action). Not run on real feeds yet:
  biz-app does not write the history.

Cost model: fills happen at the next 1m open, with the fee plus slippage
charged on each side (25 bps per round trip by default, against the measured
live 14.8 bps). Buys under 5 USDT are skipped. Equity is marked at the last
close with no exit costs.

Output: `reports/schizoid_backtest_<end-date>.md` (settings, results,
per-variant exit and skip reasons, the GCP2 section for `schizo`,
assumptions, caveats), `.csv` (one row per variant), `_trades.csv` (one row
per trade) and `_<variant>_trace.csv` for `schizo` and `schizo_mdp` (one row
per step: time, GCP2 value, from and to zone, dice,
switched/held/initial/no reading, mood before and after). The files are byte-identical
across reruns. The cache comes from `bb entry-study` (`data/` is gitignored).
The liquid universe is the 2026-10-18 top 20, so results carry survivorship
bias.

## Bot Spot Data Backups

Bot Spot's record is files: the decisions
audit (the only record of every trade), journal, agent state, exit-plan book
and watch files, the entry-timing cache, and the omp session directory.
`scripts/backup_bot_data.sh` archives them into
`backups/bot-spot-data-<UTC>-<pid>.tar.gz`. It keeps 14
(`BINANCE_BOT_BACKUP_RETENTION`) and resolves each path from the trader's
`BOT_SPOT_*_PATH` / `BOT_SPOT_SESSION_DIR` settings. On Docker the `scheduler`
runs it at 23:45 UTC into `docker-backups/`. The archive's paths start with
`data/`, which is `docker-data/` on the Docker host. To restore, stop
`bot-spot`, extract into a temporary directory, copy its `data/` contents over
`docker-data/`, then start `bot-spot`. The Phase 49
`bot-spot-schizo-state.json` is not in the archive (its mood is also in every
cycle row's `schizo`); without it the next cycle's GCP2 reading picks a fresh
mood.
