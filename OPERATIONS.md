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

The only order path is Bot Spot's `execute_route`. `bot-spot-paper`
(plan-only) and the dashboard place no orders. The legacy
`binance-bot.service` and the paper
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
docker compose --profile all build   # every service has a profile; plain `build` builds nothing
docker compose --profile all up -d --remove-orphans
```

Profiles: `live` and `all` (bot trader, dashboard, scheduler;
`deploy/seref/binance-bot-compose.service` brings up `all` at boot), `gui`
(dashboard only), `paper` (the nonstop trader plan-only, `bot-spot-paper`).
The `scheduler` container runs the nightly
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
profiles `gui`/`live`/`all`) serves the Bot Spot trades, the Algorithm tab
(one panel per sleeve of the rule: share, levels, risk-off state, holdings
from `BOT_SPOT_STATE_PATH`, the newest decision's listed coins under the
sleeve's signal names and weights; every decision row with its sleeve, halt
rows marked; "Running" while a row is at most an hour old, the heartbeat; its
`/api/status` with the newest 120 rows is about 1.06 MB at 17-18 KB D6 rows,
0.56 MB on the Pi's paper log of 2026-10-31), the Scoreboard tab (the newest
`bb tag-scoreboard` file, `CRYPTOBOT_GUI_SCOREBOARD_PATH`, default
`data/scoreboard/latest.json`: per horizon and target the signals by `t` with
excess, net, hit rate, halves and decay), the
tags feed and tag registry as biz-app wrote them (Tags tab, `/api/tags`, read
from `BOT_SPOT_TAGS_FEED_PATH` with `tags-registry.json` and
`binance-spot-assets.json` from the same directory and the proxy file
`BOT_SPOT_PROXIES`: every live tag by kind with its 24h and 7d posters and
lean, new tags (live within 24 hours of the feed's `as_of`), registry aliases,
asset tags marked Binance market / through a proxy / scored only, baskets,
links with 7d posters, events by date, claims with target, deadline and status
`open` or `deadline passed`, no price check), stop-file and
failed-route-halt state at `http://localhost:8787` on the Docker host. It is
published on `127.0.0.1` only and mounts the data dir read-only, and shows the
live trader's audit and state (the env file's paths). From another
machine: `ssh -L 8787:localhost:8787 <host>`. On a systemd host,
`scripts/setup_user_deploy.sh` installs it next to `bb`; run
`cd ~/.local/share/binance-bot && bin/cryptobot-gui`.

Laptop on the Pi's live data: `scripts/cryptobot_gui_pi.sh --open` (the
`cryptobot-gui` fish command). It rsyncs the Pi's `docker-data` into
`~/.cache/binance-bot/pi-data` every 30 s (old omp logins, the old bot
session and session files, the candle caches and the tags history are not
copied; nothing is written on the Pi), maps the trader's file paths (audit,
stop, state, tags feed) from the Pi's running `cryptobot-gui` container (its
`.env.docker` when the container is absent), points the Scoreboard
tab at the mirrored `scoreboard/latest.json`, uses this repo's
`config/proxies.json` for the Tags tab,
and runs the local release build against the mirror with the laptop's
read-only keys for the wallet. Started without Pi data, the page says so
instead of showing empty tabs. Overrides: `PI_HOST`, `PI_DATA_DIR`,
`PI_ENV_FILE`, `MIRROR_DIR`, `MIRROR_SECONDS`.

## Emergency Stop Files

Bot Spot checks its stop file on every tick and before every route, leg and
order, and refuses to trade while it exists. Create it to stop, delete it to
resume. `bot-spot-paper` shares the same file. Paths are relative to the
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
  checked on every 15 s tick (before any read), before every route, before
  every leg and before every order. A one-shot run refuses to start under
  it; the `--nonstop` trader keeps running and idles (no read, no decision,
  one log line when the stop starts and one when it ends), so a paused
  trader survives restarts and deploys. It decides again on the first tick
  after the file is deleted.
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

Each route and each recorded decision is one fsync'd row in
`BOT_SPOT_AUDIT_PATH` (default `data/bot-spot-decisions.jsonl`); decision rows
(`row_type` `cycle_summary`) carry the rule's verdict (`algo`). There are no Telegram or phone alerts (operator decision
2026-10-29).

### Commands

```bash
bb bot-spot --evidence-only                 # decision context and quotable routes, no row, no order
bb bot-spot --plan-only                     # one decision; execute_route only records plans
bb bot-spot --plan-only --nonstop           # the nonstop trader, plan-only
bb bot-spot --submit-live --nonstop         # the nonstop trader, live
bb bot-spot --mark-failed-route-reviewed --review-reason "reconciled by hand"
```

### Deployment

Bot Spot runs nonstop (operator decisions 2026-10-29), not on a timer:

- Every 15 s a tick checks the stop file, polls every price with one
  `ticker/price` request (weight 4), reads the tags feed file and decides.
  Each coin's minute closes come from those polls (seeded once from 1m
  klines), so its 1h move is the trader's own; its 24h move is the USDT
  market's 24h ticker change from the catalog (no extra request; unknown
  when that ticker is over 10 minutes old); its 7d move compares the newest
  price with the hourly close 7 days back, from 200 1h klines read at most
  once per clock hour per coin (weight 2 each: about 60 weight an hour for
  30 coins; unknown when no close lies within the hour before). The catalog, fees and
  balances reload every 5 minutes and on the tick after every executed
  route. A decision row is written only when the decided action changes
  (its orders, or the coins it keeps) and once an hour as a heartbeat; an
  unchanged tick writes nothing and sends nothing, so an order that failed
  is tried again when the decision changes or at the heartbeat. The log has
  `used_weight_1m` (Binance's request weight) on every decision row, and on
  every tick at `RUST_LOG=bb_trader=debug`.
- Docker: compose service `bot-spot` (profiles `live` and `all`), running
  `bb bot-spot --submit-live --nonstop`.
- systemd: `binance-bot-spot.service` (same command, `Restart=always`):

```bash
systemctl --user enable --now binance-bot-spot.service
journalctl --user -u binance-bot-spot.service -f
```

The service always passes `--submit-live`; whether it trades is decided by the
live arming variables in `.env` and the stop file.

### Tags feed (biz-app)

biz-app (`~/projects/automation/imageboards/biz-app` on the laptop, a copy in
`~/biz-app` on the Pi) writes the tags feed `tags-feed-v1` (`tags-feed.json`)
with `uv run --directory biz-app/biz_insights biz-insights feed` (`--at <UTC>`
for a past moment): the tags live at `as_of` (5 or more distinct posters in
7 days) of seven kinds (asset, narrative, macro, event, entity, claim, mood),
each with per-window (1h, 6h, 24h, 7d, 30d) mentions, distinct posters and
threads, the AI's up/down weighted by the post's 1-3 score and the horizon
mix; plus narrative baskets (`members`) and tag-to-asset `links` (contract:
`docs/archive/implementation-plan-phase-53.md`). Asset tags of Binance spot coins count
the posts naming the coin without AI. Distinct posters are a lower bound:
/biz/ shows poster IDs only on some posts. The real output of
2026-10-31T00:00Z is the test fixture
`crates/bb-trader/tests/fixtures/tags-feed-v1.json`.
`biz-insights feed --history --step 60` writes one point-in-time feed per
hour into `--history-dir` (default `<out>/tags-history/<as_of>.json`), the
input of `bb algo-backtest --tags-history`. Every post is tagged once (cache
`feed.sqlite3`), so a rerun makes no model call.

Live, two systemd user units on the Pi keep the feed fresh (operator,
2026-10-30, `docs/decisions.md`; unit files in `deploy/seref/`, installed in
`~/.config/systemd/user/`, enabled, the user has linger so they start at boot):

- `biz-watch.service`: the scraper, `biz watch --no-media` in
  `~/biz-app/biz_scraper` (a sync every 60 s into `data/biz.sqlite3`). Posts
  only: the feed reads post text, and `i.4cdn.org` (images) does not connect
  from the Pi's network.
- `biz-feed.service`: `biz-insights --out ~/binance-bot/docker-data feed
  --watch --history-dir ~/binance-bot/docker-data/tags-history` in
  `~/biz-app/biz_insights`, a round every 900 s: it tags the new
  posts with Gemini through omp (prompt `tags-1`), merges new tag names into
  the registry, and replaces `docker-data/tags-feed.json` and
  `docker-data/tags-registry.json` by rename, so the trader and the dashboard
  never read half a file; a failed round keeps the old files, which then age.
  After each good round it writes the hourly history files due since the
  newest one in `docker-data/tags-history/` (into an empty dir all from
  2026-10-01, a few minutes once), the input of the nightly scoreboard.
  `docker-data/` also holds its tag cache (`feed.sqlite3`) and Binance asset
  list (`binance-spot-assets.json`). omp
  (Bun, `~/.bun/bin/omp`) uses `PI_CODING_AGENT_DIR=~/.local/share/biz-feed/omp-agent`,
  which holds only the `google-antigravity` login (re-login: `PI_CODING_AGENT_DIR=…
  omp login`, or `omp auth export | ssh pi-wired 'PI_CODING_AGENT_DIR=… ~/.bun/bin/omp auth import -'`).

```bash
ssh pi-wired 'systemctl --user status biz-watch biz-feed'
ssh pi-wired 'journalctl _SYSTEMD_USER_UNIT=biz-feed.service -n 20 --no-pager'
ssh pi-wired 'systemctl --user stop biz-feed biz-watch'   # the feed then ages; after 45 min no buys
```

`as_of` is the start of the scraper's newest sync pass, so it advances only
while `biz-watch` runs. The trader reads `BOT_SPOT_TAGS_FEED_PATH` (default
`data/tags-feed.json`) every tick and records a summary on each decision row
as `tags`: `status` `fresh` with `as_of`, `age_minutes`, the file's sha256
`digest`, `sources` (model, prompt and registry version, posts, untagged),
live tags per kind (`kinds`), the asset tags with an eligible Binance market
(`tradable`), those reaching one through the operator's proxy file
(`proxied`, e.g. `{"GOLD": ["PAXG", "XAUT"]}`), those with neither
(`scored_only`, never bought), the paths from tags to tradable assets per
kind (`reach_paths`) and per kind the 10 loudest tags (`top`: name,
`posters_24h`, `posters_7d`, `up_24h`, `down_24h`; most 24h posters first), or
`unavailable` with the reason (missing or unreadable file, another schema, a
tag id that is not `<kind>:<name>`, `as_of` older than
`BOT_SPOT_TAGS_MAX_AGE_MINUTES` (45) or more than 5 minutes ahead of the
Pi's clock). `context_hash` covers the feed by its `as_of` and digest. The
algorithm (below) scores the asset tags. The dashboard's Algorithm tab shows
the newest recorded feed status; its Tags tab reads the feed itself. The
version 1 file `docker-data/biz-feed.json` from before Phase 53 is kept, no
longer written (the rollback to `binance-bot:pre-tags-20261031` reads it).

Reach (`crate::reach`): a tradable asset collects signal from its own asset
tag, from an asset tag without a Spot market through the operator's proxy file
`BOT_SPOT_PROXIES` (shipped `config/proxies.json`: gold through PAXG and XAUT,
operator 2026-10-30; unset means no proxies, a named missing or invalid file
stops the trader at start), from narrative baskets listing it (weighted by its
share) and from links tying any tag to it. Baskets and links landing on GOLD
follow its proxies. Only live asset tags, directly or through a proxy, make buy
candidates; the `algo-2026-10-29` rule weights only `talk.6h`, `stance.6h` (a
coin's own asset tag) and `momentum.1h`, so a proxy such as PAXG is scored on
its momentum until a rule gives the new signals weight. The Pi's `.env.docker`
sets `BOT_SPOT_PROXIES=config/proxies.json` (Phase 53 rollout, 2026-10-31).

Signals (`crate::signals`): the catalog of names a rule may weight. The
legacy `talk.6h`, `stance.6h` and `momentum.1h` are the `algo-2026-10-29` terms
(the decider reads them from there); `momentum.24h` and `momentum.7d` (caps
10% and 20%); per tradable asset `<kind>.<signal>[.<window>]` (the tag signal
over every reach path from a tag of that kind, averaged by path weight; link
paths use the link's own up/down for `direction` and `agreement`); market-wide
`market.<macro|mood>.<signal>[.<window>]` (averaged by the tags' posters) and
`market.mood.tone.<window>`, the board's fear/greed: (posters of bullish moods −
posters of bearish moods) / their sum, from the mood names in
`signals::MOOD_BULLISH` (bullish, euphoria, fomo, greed, hopium, optimism) and
`MOOD_BEARISH` (bearish, capitulation, despair, doom, fear, panic); other moods
are left out (operator 2026-11-01). A mood or macro tag's `direction` means the
poster sees it growing, so `market.mood.direction` and
`market.macro.direction` say the board's moods and macro themes are growing,
not fear against greed. Signals: `direction`, `level`, `surge`, `novelty`,
`breadth`, `persistence`, `agreement`, `horizon`, `price_confirmed`; each −1..1
or 0..1 except the legacy `talk.6h` (−1..3). The module docs list each
formula, its windows and range; an unknown name is an error, and a rule file
naming one refuses to start.

### The rule file

`BOT_SPOT_ALGO_RULE` (required) is the path of the operator's rule file
(`crate::rules`; shipped in `config/rules/`, in the image
`/app/config/rules/`). It replaces the env rule (the five weights, the poster
minimum and `BOT_SPOT_RULES_VERSION`, listed in `REMOVED_VARS`, now ignored with one
warning). Fields: `rules_version` (recorded on every audit row and in
`run_config`), `style` (`fixed` or `edge`), and `sleeves`, each with `name`, `share` (the
shares sum to 1), `horizon` (`any`, `short`, `swing`, `long`), `defensive`
assets, `risk_off_below` (`null` = never), `buy_above`, `sell_below` (not
above `buy_above`), `min_posters` and `weights` by signal name. Unknown fields,
an unknown signal, shares not summing to 1, a missing or unreadable file: the
trader refuses to start, as for a sleeve name other than letters, digits, `_`
or `-`. `run_config.rule` records the path, style and sleeves.

Sleeves (Phase 53 D5, operator 2026-10-31): every sleeve with a share or a
holding decides on its own each tick (a 0-share sleeve holding nothing, like
the shipped `core`, is skipped and writes no rows). Its own decision key and
hourly heartbeat decide when it writes its decision row; rows and its route
rows carry `sleeve`. A `horizon` other than `any` buys only coins whose own
asset tag's 7d horizon mix leans that way (the largest of short, swing and
long; ties count for each; no stated horizon: not bought); held coins are
always scored and may be sold. The sleeve's market-wide score is `Σ weight ×
value` over its weighted `market.*` names (0 without a feed); below
`risk_off_below` the sleeve moves to the `defensive` asset with the highest
score on its other names among those priced (USDT counts 0, ties go to the
earlier one listed): it sells a holding that is not that asset and buys it (no
buy level; sized and min-notional checked as any buy), and buys nothing else;
the row says "risk-off". After a sleeve's routes ran, later sleeves decide on
the next tick, once balances are reloaded. Trades and trade memory pair each
buy with the next sale of the same asset by the same sleeve.

Style `edge` (`crate::edge`; operator 2026-10-30 "Both, later decide", the
formula chosen 2026-10-31): the weights follow the tag scoreboard instead of
the operator's numbers. Rule-level `edge_scale`, `edge_cap`,
`edge_refresh_hours` and `edge_min_t` are required (no defaults),
`edge_scoreboard` defaults to `data/scoreboard/latest.json`; each sleeve names
its `edge_horizon` (`1h`, `24h`, `7d`) and has empty `weights`. Every scoreboard
row at that horizon with |t| ≥ `edge_min_t` and not decayed weighs its signal
`clamp(edge_scale × excess_pct, ±edge_cap)` (per-asset names from the `assets`
row, `market.*` names from the `basket` row); the others weigh 0. The trader
reads the file at start, after `edge_refresh_hours` and after a failed read,
and uses it for a decision only when it is `tag-scoreboard-v1`, its
`available_at` (its last outcome's close, which can be days after its history
ends) is not after the decision time, its `min_posters` equals the sleeve's,
and it has the horizon. Otherwise the sleeve holds with no orders and the row
says why (`algo.blocked`, `algo.edge.reason`; decision "hold, no orders (no
valid scoreboard: …)"). No edge rule is shipped or enabled.

### The algorithm

`crate::decider` decides on every tick for each active sleeve of the operator's
rule file. It reads the wallet and the tags feed, and the prices of
the coins it may buy (the tradable assets the feed's live asset tags reach,
directly or through a proxy, with an eligible USDT market,
stablecoins left out) and for the coins it holds (operator decision
2026-10-29: no market breadth read). It scores those coins with
`Σ weight · signal` over the sleeve's weighted names (stances need the
sleeve's `min_posters`), sells a held coin
scoring below the sell level into USDT and, with nothing held, buys the best
coin at or above the buy level. Orders are sized per sleeve (Phase 53 D4): a
sell spends exactly the sleeve's quantity, a buy spends the sleeve's `share` ×
the portfolio value (free USDT plus what the sleeves own; unowned balances
left out), capped at the free USDT (with `algo-2026-10-29`'s fast share 1 and
nothing held: the whole free USDT, as before). An order worth less than its
USDT market's minimum order is not sent: the sleeve holds and the decision row
says `below_min_notional` (`algo.below_min_notional`; the planned spend of each
order is in `algo.orders[].spend`, the route row's `spend_amount`). Every
order is sent through `execute_route`, so the stop file,
arming, scope, `order/test`, reconciliation and the failed-route halt apply;
the first failed order ends that decision's orders. Without a fresh feed
nothing is a buy candidate, so it can only hold or sell; a coin without a
price is left out of that tick.

The rule in force live (operator, 2026-11-02, `docs/decisions.md`, "2026-11-02
tags + snap-back rule live"): `BOT_SPOT_ALGO_RULE=config/rules/tagsnap-2026-11-01.json`,
rule set `tagsnap-2026-11-01`: fast sleeve share 1, score = −1 `momentum.24h`
(the coin's 24h move capped at ±10%, divided by 10%) + 0.5
`asset.direction.24h` (2 posters); buy at +1.0, sell below -0.3; core sleeve
share 0. Live, the 24h move is the USDT market's 24h ticker change (see
"Deployment"); a coin whose ticker is stale scores 0 and is not bought. Its
backtest: `reports/phase54/tag_rule_tuning_2026-11-01.md` (+8.50%, 4 round
trips; does not pass the round-1 bar). `bot-spot-paper` runs the same rule
plan-only; it sizes from the real wallet, so it plans no buy while the live
trader holds the cash.

The snap-back rule `snapback-2026-10-31` (live 2026-10-31 20:01 to 2026-11-02
07:06, `config/rules/snapback-2026-10-31.json`): fast sleeve share 1, score =
−1 `momentum.24h`; buy at +0.8, sell below -0.3; backtest on the tags history
2026-10-01T14:00Z..10-25T14:00Z: +8.18%, 6 round trips (in-sample).

The earlier tag rule `algo-2026-10-29` (`config/rules/algo-2026-10-29.json`,
operator 2026-10-29), no longer run: fast sleeve share 1, score =
`stance.6h` + `momentum.1h` (`talk.6h` weight 0, 2 posters); buy at +0.5,
sell below -0.5; core sleeve share 0. Its backtest:
`reports/algo_backtest_2026-10-25.md` (`bb algo-backtest` on the old /biz/
coin feed history, `--end 2026-10-25T14:00:00Z --days 24`): +18.42% on the
tuning half, -11.18% on the judging half (BTC -3.47%, liquid basket -9.78%). On the tags
history with candidates as live (Phase 53 E1,
`reports/phase53/algo_backtest_2026-10-25.md`, same window): -21.05%, 15
round trips; the difference is the candidate set only (see "Algorithm
backtest").

- **Records.** Decision rows carry `algo` (sleeve, rule, levels, best candidate
  and score, held coins and scores, orders, `coins_scored`, and per listed coin
  `signals` (each weighted name and its value), `ret_1h_pct`, `ret_24h_pct`,
  `ret_7d_pct`, `score`; the held coins and the 20 best candidates are listed,
  best first; `market_signals`: every `market.*` name's value),
  `rules_version` (from the rule file), `run_config.rule` (`path`,
  `rules_version`, `style`, `sha256` of the file's text, `sleeves`) and
  `session_id` `algo`. An order's reason names at most 8 terms (the largest
  `|weight × value|`, then "N more"); the shipped rule's 3 all show.
- **Row budget** (Phase 53 D6). A decision row is at most 32 KB serialized
  (`audit::ROW_BUDGET_BYTES`; the dashboard reads up to 120 rows per poll).
  Over it, `audit::fit_budget` drops detail step by step until it fits,
  recording each step in `trimmed`: `algo.rule`, the weights in
  `run_config.rule` (as counts; `sha256` names the file), signals worth 0,
  `tags.top` down to 3 per kind, `tags.tradable`/`scored_only`/`proxied` as
  `*_count`, candidates one by one down to 5, `tags.top`, candidates down to 1,
  the coins' signals, `algo.market_signals`, `algo.edge`, `algo.coins`, the
  feed summary beyond status, `as_of`, age, digest and kinds, then `run_config`
  beyond the mode and the rule's identity. The decision (action, reason), the
  verdict, orders, sizing and sleeve are never cut. The shipped rule on the
  full feed (462 tags) writes about 17 KB with nothing trimmed.
- **Practice run.** `docker compose --profile paper up -d bot-spot-paper`
  runs the nonstop trader plan-only with its own files
  (`bot-spot-paper-{decisions.jsonl,state.json}`, lock `bot-spot-paper.lock`)
  and the shared stop file, on its own rule: `docker-compose.yml` sets its
  `BOT_SPOT_ALGO_RULE` to the paper rule `tagsnap-2026-11-01` (operator
  2026-11-01), whatever the env file's live rule is. Plan-only rows do not
  move the wallet, so it keeps deciding on the real balances.
- **Going live with a new rule** (after its practice run, unless the operator
  waives it for that rule set, and the operator's go): `docker tag
  binance-bot:latest binance-bot:pre-algo-<date>`, back up `.env.docker`, set
  the operator's `BOT_SPOT_ALGO_RULE` in `~/binance-bot/.env.docker`, `docker
  compose up -d --no-deps bot-spot`, watch the first decisions. Back: retag
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

The halt is recomputed from the audit log, so the next decision sees the marker.
Remove the stop file to resume.

### Sleeve holdings and the `sleeve_mismatch` halt

The state file (`BOT_SPOT_STATE_PATH`) holds what each sleeve owns
(`sleeves.holdings.<sleeve>`: `asset`, `qty`, `opened_at`; `crate::sleeves`).
The rule decides only on owned coins: other balances (a BNB fee balance, a
manual buy) are unowned, never sold, never counted as held or as portfolio
value, and never block a buy. A sell spends exactly the sleeve's quantity; a
live buy books the quantity that arrived (destination free after − before of
the route's last leg); plan-only books nothing.

- **First start** (no state file, or one from before Phase 53 without
  `sleeves`): a single non-USDT, non-cash-like free balance worth at least its
  market's minimum order is adopted by the trading sleeve; several are listed
  in `sleeves.unowned` and left alone.
- **Every tick** the state loads strictly and each asset's sleeve quantities
  must not exceed its free balance plus one step of its USDT market. A state
  file that cannot be read or whose `sleeves` is malformed, or a shortfall,
  writes a blocked decision row with `sleeve_check.kind` `sleeve_mismatch`
  (again when the reason changes and hourly) and halts: no decisions, and
  `execute_route` refuses every route in plan-only and live. The halt is read
  from the audit log, so restarts keep it; the state file is never overwritten
  while it does not load.
- **To resume**: fix the cause (the wallet, or the state file by hand after a
  backup), then append the review row. The command re-reads the balances and
  is refused unless the state loads and reconciles now; it never rewrites the
  state (operator, 2026-10-31):

```bash
bb bot-spot --mark-sleeves-reviewed --review-reason "<what you checked>"
docker compose run --rm bot-spot-paper bb bot-spot --mark-sleeves-reviewed
```

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
Its breadth feature is offline research over cached candles; the trader and
the dashboard no longer read market breadth (Phase 52).

```bash
bb entry-study --audit docker-data/bot-spot-decisions.jsonl \
  --output data/entry-study.json --markdown docs/research/entry-study.md
```

#### Algorithm backtest

`bb algo-backtest` runs the operator's rule file (`crate::rules`, as the live
trader reads it) over the same 1m cache and the tags feed history, every
sleeve as the trader runs it (Phase 53 E1). It is offline and research only:
it reads only `--cache-dir`, `--tags-history`, `--rule`, `--proxies` and
`--scoreboards`, never fetches or calls Binance, has no order path, and
leaves the live `bot-spot` untouched. If the cache is missing a liquid coin or
a gold proxy (PAXG, XAUT; `engine::COVERAGE`), one ends before `--end`, or
none has 7 days of history before the window start (the first step's 7d move),
it exits 1 with `missing cache`. Each step's 1h, 24h and 7d moves come from the
cached 1m closes.

```bash
cargo run --release -p bb-cli --bin bb -- algo-backtest \
  --rule config/rules/algo-2026-10-29.json \
  --tags-history <history dir> --cache-dir data/entry-study-cache \
  --end 2026-10-25T14:00:00Z --days 24
```

The other flags are `--proxies` (`config/proxies.json`), `--scoreboards`
(edge rules only), `--candidates` (`tags`, as live; `liquid`: the liquid 20
priced at the step instead of the tag pick, still nothing without a visible
feed, to measure what the tag pick adds), `--starting-balance` (1000),
`--fee-bps` (7.5), `--slippage-bps` (5), `--step-minutes` (20),
`--tags-max-age-minutes` (90) and `--output-dir` (`reports`). Add missing coins to the cache with
`bb entry-study --symbols PAXG/USDT,XAUT/USDT --end <end> --days <n>` (public
klines).

At each step the newest feed with `as_of` at or before the step is visible
(for `--tags-max-age-minutes`; none after that, as a stale feed live). As
live: the candidates are the cached coins its live asset tags reach, directly
or through the proxy file (a tag without a cached market or proxy is
scored-only; the report lists the most frequent ones); every sleeve with a
share or a holding decides with `crate::decider` on its own holdings (horizon
filter, risk-off and every signal name of the rule, the whole feed and its
reach); a sell sells exactly the sleeve's quantity and a buy spends its share
of the portfolio value at the step (cash plus every sleeve's holdings), capped
by the cash left. An edge rule needs `--scoreboards <dir>`: point-in-time
scoreboards (`bb tag-scoreboard --as-of T`); each step uses the newest whose
outcomes all closed by then (`available_at`), and a sleeve without one holds
with no orders. Nothing is tuned (the next phase does that).

Cost model: fills happen at the next 1m open, with the fee plus slippage
charged on each side (25 bps per round trip by default, against the measured
live 14.8 bps). Buys under 5 USDT are skipped. Equity is marked at the last
close with no exit costs.

Output: `reports/algo_backtest_<end-date>.md` (settings with the rule file's
path, `rules_version`, style, sha256 and sleeves; results for the portfolio
and per sleeve; skip reasons, notes, assumptions, caveats), `.csv` (rows
`portfolio` and `sleeve:<name>`; a sleeve's net is the sum of its trades, its
% of the starting balance), `_trades.csv` (one row per trade with its sleeve)
and `_<sleeve>_trace.csv` per sleeve that decided (time, feed `as_of`,
candidates, scoreboard `available_at`, best coin and score, held coin and
score, market score, risk-off, action). The files are byte-identical across
reruns. The cache comes from `bb entry-study` (`data/` is gitignored); the
candidates are limited to the cached coins (survivorship).

Comparison with the pre-change backtest (E1, 2026-10-31): the binary before
this rewrite (`~/binance-bot-prechange/bb-pre-e1-84e3a01`, README there)
picked candidates from the fixed `LIQUID` 20 (top 10 by 24h volume), split the
window into halves and held one coin with all cash. On its halves of
2026-10-01T14:00Z..10-25T14:00Z with the shipped rule, a throwaway build of
the new engine with those candidates restored gives the same trades and the
same action at every step (864 + 864). The shipped rule's differences are
therefore the candidate set only: tags-reached coins with a cached market
(16.7 per step, e.g. QNT, CVX, LINK, DOGE) instead of the liquid top 10 (SUI,
WLD, NEAR, STRK, ...). Same action at 841 and 850 of 864 steps; 2 of 12 and 4
of 8 trades identical; net -140.63 and -81.32 USDT against +75.18 and -52.99.
The whole window: `reports/phase53/algo_backtest_2026-10-25.md` (-21.05%,
15 round trips).

#### Tag scoreboard

`bb tag-scoreboard` measures which tags and signals moved prices their way
(`crate::scoreboard`). Research only: public klines, no account, no order
path. For every hourly `tags-feed-v1` file of `--history-dir` and every signal
name (`crate::signals`) it takes the non-zero values at the feed's hour: per
asset for every tradable asset the feed reaches (with `--proxies`, default
`config/proxies.json`), market-wide against BTC and the liquid basket. It
compares the forward return at +1h, +24h and +7d with the same target's
random hours (every hour of the history), weighted by the signal and in its
direction: `excess_pct`, `net_pct` after the round-trip fee (`--fee-bps`, 15),
hit rate and `t` over horizon-long blocks (0 under 5 blocks, so a +7d `t`
needs a history of 5 weeks), for the whole history and its older
and newer half, `decayed` when the newer half keeps under half of a positive
older edge.

```bash
cargo run --release -p bb-cli --bin bb -- tag-scoreboard \
  --history-dir ~/biz-tags-retag/tags-history \
  --cache-dir data/scoreboard-cache --output-dir data/scoreboard
```

On the Pi the `scheduler` runs it at 00:30 UTC (`docker/crontab`) over
`data/tags-history` (the history `biz-feed` keeps), with the cache in
`data/scoreboard-cache` and the output in `data/scoreboard/`; by hand:
`docker compose run --rm scheduler bb tag-scoreboard --history-dir
data/tags-history --cache-dir data/scoreboard-cache --output-dir data/scoreboard`.

It writes `scoreboard-<as_of date>.json` and `latest.json` (schema
`tag-scoreboard-v1`). Point in time: `--as-of` (default now) drops feeds
after it and every outcome not observed by it; the file is stamped with
`available_at`, its latest outcome close (per horizon `signals_to` and
`outcomes_to`), so a +7d result appears only 7 days after its signal and a
decision may use a scoreboard only from `available_at` on. Candles come from
an incremental cache of closed 1h bars per asset (`1h_<ASSET>_USDT.json`):
a run reads only the hours before or after what is cached that the history
needs (`--max-bars`, 10000, per asset), and logs its kline reads. The
universe is today's Spot USDT markets (survivorship). It replaces the Phase 52
lead study.

## Bot Spot Data Backups

Bot Spot's record is files: the decisions
audit (the only record of every trade) and the trader state (the old journal,
omp session directory, entry-timing cache and exit-plan files are no longer
archived; earlier archives keep them).
`scripts/backup_bot_data.sh` archives them into
`backups/bot-spot-data-<UTC>-<pid>.tar.gz`. It keeps 14
(`BINANCE_BOT_BACKUP_RETENTION`) and resolves each path from the trader's
`BOT_SPOT_*_PATH` settings. On Docker the `scheduler`
runs it at 23:45 UTC into `docker-backups/`. The archive's paths start with
`data/`, which is `docker-data/` on the Docker host. To restore, stop
`bot-spot`, extract into a temporary directory, copy its `data/` contents over
`docker-data/`, then start `bot-spot`.
