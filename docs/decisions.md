# Operator Decision Log

The wording and dates of the operator decisions behind the rules in
`CODEX.md`. `CODEX.md` holds the rules in force; this file says why and when.
Newest last. Append a new entry when the operator makes a decision; do not
rewrite old entries (mark one superseded instead). Decisions from the LLM
trader era are superseded and archived in
`docs/archive/llm-trader-era-to-2026-10-29.md`.

## 2026-10-29 prune to the Bot Spot trader

- Operator, 2026-10-29: "get switched off modules off of this project prune it
  into binance-bot only. get them out into ../redundancy". On the airdrop
  reminder, whose daily `docker/crontab` line was still active: "Move it out".
- What moved (not deleted) to `../redundancy/binance-bot-modules/`, mirroring
  the repo paths: the crates `bb-runtime` (legacy `main`, `ops-check`),
  `bb-products` (API permissions, scope audit, product modules, margin,
  futures, transfers, withdrawals, prediction, canaries, live spot allocator,
  live probe), `bb-capital` (capital pool, auto-convert, bot status, cash
  distributor), `bb-listing`, `bb-signals` (signals, X insighter, Telegram
  ingester), `bb-sniper`, `bb-polymarket`, `bb-airdrop`; from `bb-strategies`
  funding arb, trend filter, mean reversion and the basis-arb, grid,
  news-event and stat-arb backtests; from `bb-pool` the paper-pool runtime,
  SQLite ledger, replays, reports, unified pool and every `pool*` command;
  from `bb-binance` the client code only those modules used (USD-M futures,
  margin, convert, fills, WebSocket streams, feeds and their queue, and the
  universal-transfer, withdrawal, deposit and coin-config wallet endpoints;
  the client now talks to the spot host only);
  `collectors/x-camoufox`, `docs/signals/`, `docs/rules-other-modules.md`, the
  moved modules' `deploy/user` units, scripts (`panic.sh`, `status.sh`,
  `backup_db.sh`, `restore_db.sh`, `pool_status.sh`, `pool_replay.sh`), their
  compose services and crontab lines, their `.env.example` keys
  (`env.example-removed.txt`) and their `OPERATIONS.md` sections
  (`OPERATIONS-removed-sections.md`).
- What stays: `bb-core`, `bb-binance` (spot REST and the wallet balance read),
  `bb-omp`, `bb-trader`, `bb-pool` (market catalog, fee service, route engine,
  models only), `bb-social`, `bb-strategies` (bot research only), `bb-cli`,
  `cryptobot-gui` (Fleet and Signals tabs removed). Bringing a module back
  needs an operator decision and its own approved phase (`CODEX.md`).

## 2026-10-29 algorithm trader

- Operator, 2026-10-29: "1 will be an continous algorithm from now on no 20
  min crap and no ai anymore we will decide what will that algo will be but
  trader is an algo from now on 2 no more telegram or phone alerts 3 ai is
  from now on only will read contents of provided data. /biz threads wil le
  read by ai and fed into algorithm in a format that algorithm use along with
  binance data. 4 no chat with ai an algorithm cant chat with ai. it is just
  an algortihm with those all stated". The operator approved the target
  diagram and asked to change the agent docs to it: "change all of those to
  our way. i dont want to see any of old stuff."
- The trader is an algorithm that runs continuously: no 20-minute cycle, no
  timer, no LLM, no AI chat. Its rules are decided with the operator.
- No Telegram or other phone alerts.
- AI only reads supplied content: biz-app (outside this repo) reads /biz/
  threads and writes them as numbers in the feed format the algorithm reads,
  next to Binance data.
- This supersedes every LLM trader decision (model, fallback, prompts, exit
  plans, schizo trader, long-term value voice, Phase 51 `llm|algo` switch)
  and the 2026-10-29 prune's "what stays" for `bb-omp` and `bb-social`,
  which Phase 52 removes.

## 2026-10-29 Phase 52 loop decisions (`docs/archive/loops/LOOP_ALGO.md` section A)

Answers the operator gave through the ask tool on 2026-10-29:

- Commits (A1): "Loop commits tree, then per item": the loop first commits the
  current tree (the prune and the doc rewrite) as one commit, then commits
  each finished item, code and ledger together, with the item ID in the
  message.
- biz-app (A2): "Yes, AI tags every post": the loop may build the feed writer
  inside `~/projects/automation/imageboards/biz-app/biz_insights` (backed up
  first), and the AI tags every post, not only posts that name a coin.
- Live prices (A3): "REST polling every 15 s" with the existing spot client.
- Pace (A4): "No limit": no minimum time between the algorithm's actions; it
  acts whenever its rule says so. Decision rows: "On change + hourly
  heartbeat": a row when the decided action changes or an order is placed,
  plus one per hour.
- Removals (A5): accepted every default: delete the dashboard Sessions tab;
  drop the Markov value model (decider weights become
  `talk,stance,momentum,buy_above,sell_below`); remove the market-evidence
  service; drop the "sold by exit plan" label; audit `prompt_version` becomes
  `rules_version`. Added by the operator: "i also want 20 breadth to be gone
  as well along with all above": the 20-coin market breadth read
  (`market_mood`) and the dashboard's market-mood gauge go too.
- Replacement for the breadth read: "Feed coins + own prices": the coins the
  algorithm may buy are the coins in the /biz/ feed with an eligible USDT
  market, plus the coin held; each coin's 1h move comes from the trader's own
  15 s price polls, seeded at start from 1m candles.

## 2026-10-29 algorithm rules (`docs/archive/loops/LOOP_ALGO.md` D6)

Shown to the operator: `reports/algo_backtest_2026-10-25.md`, the 2026-10-01
to 2026-10-25 /biz/ feed history and 1m candles, tuned on the older half
(2026-10-01T14:00Z to 2026-10-13T14:00Z, 72 weight sets) and judged on the
newer half. The grid best, talk 0, stance +1, momentum +1, buy at score
+0.5 or more, sell below -0.5, made +18.42% on the older half (in sample, 12
round trips) and -11.18% on the newer half (11 round trips, 2 wins), while
BTC moved -3.47% and the 20-coin liquid basket -9.78% over the newer half.
`bb biz-lead-study` found no /biz/ lead (`IMPLEMENTATION_PLAN.md`, Phase 52).

Answers the operator gave through the ask tool on 2026-10-29:

- Rule: "(a) Grid-best weights": `BOT_SPOT_ALGO_WEIGHTS=0,1,1,0.5,-0.5`
  (talk 0, stance +1, 1h momentum +1, buy at score >= +0.5, sell below
  -0.5), stances counted with at least 2 distinct posters
  (`BOT_SPOT_ALGO_MIN_POSTERS=2`), rule set name `algo-2026-10-29`.
- Size: "All cash, one coin": all USDT into the best-scoring coin, one
  position at a time.
- Without a fresh feed: "No buys; score the held coin": no buys; the held
  coin is still scored (talk and stance count 0, momentum only) and sold when
  it falls below the sell level.

## 2026-10-30 feed pipeline on the laptop (`docs/archive/loops/LOOP_ALGO.md` E3)

Answers the operator gave through the ask tool on 2026-10-30:

- Scraper: "Loop may start it": the loop starts the /biz/ scraper (`biz watch`)
  on the real archive (`biz_scraper/data/biz.sqlite3`) as a background
  process, next to the feed watcher and the copier.
- After the loop: "Keep them running": the scraper, `biz-insights feed
  --watch` and `scripts/biz_feed_to_pi.sh` keep running detached on the
  laptop after the loop ends.

## 2026-10-30 feed pipeline on the Pi

Asked why the laptop feeds the /biz/ scores and whether all of it can run on
the Pi; shown the Pi's checks (4chan API reachable, AI calls are remote,
35 GB free so posts only, omp and uv missing). Options: its own Docker
container, plain Pi programs, or keep it on the laptop.

Answer through the ask tool on 2026-10-30: "Plain Pi programs". The scraper
(`biz watch --no-media`) and the feed writer (`biz-insights feed --watch`)
run as systemd user units on the Pi (`deploy/seref/biz-watch.service`,
`deploy/seref/biz-feed.service`) and write `docker-data/biz-feed.json`
directly. The laptop feed watcher and `scripts/biz_feed_to_pi.sh` are
stopped and the copier removed. This supersedes "feed pipeline on the laptop".

## 2026-10-30 practice run review (`docs/archive/loops/LOOP_ALGO.md` E5, E6)

Shown to the operator at 09:41 UTC: the Pi's plan-only practice run
(`bot-spot-paper`, rule `algo-2026-10-29`) had run 42 minutes with one
decision, a planned ETH buy at 08:59:28 UTC (score +0.929: stance +1.00, talk
-0.40, momentum -0.07), unchanged since and no order sent; at current prices
+0.06% gross, -0.19% after a 25 bps round trip (about -0.03 USDT on 15.05
USDT), BTC -0.40% meanwhile.

Answer through the ask tool on 2026-10-30: "(a) Keep practising".
`bot-spot-paper` keeps running plan-only on the Pi and `bot-spot` stays
stopped: kept plan-only, no go for live trading (E6 done without going live).

## 2026-10-30 dynamic tags and two horizons (Phase 53)

Shown to the operator: the research on news-reading AI traders and
geopolitical risk (FinMem, FINSABER, Alpha Arena, Lopez-Lira & Tang, Baur &
Smales, the Geopolitical Risk Index), the count of Gold (1,021) and Silver
(1,124) tags the feed drops today, and Binance's markets: Spot has only gold
tokens (PAXG, XAUT); silver, oil, metals and stocks trade only as USDⓈ-M
"TradFi" perpetual futures (XAGUSDT, CLUSDT, NVDAUSDT, ...).

Operator: "first we plan ... dynamic tag to algorithm set up well, then
afterwards we will tune the algorithm". Answers through the ask tool:

- Themes: "Open + auto-merge": the AI names themes freely, a registry merges
  synonyms and a theme goes live after enough distinct posters in 7d.
- Theme to asset: "From the posts": the AI tags per post which assets a
  theme moves and which way; the algorithm uses those links.
- Non-crypto assets: "Spot proxies now, futures later": every asset is
  scored; trading only through Spot stand-ins the operator approves (gold:
  PAXG/XAUT); futures (silver, oil, stocks) are a later safety phase.
- Horizons: "Add 7d + 30d" (forum 1h/6h/24h/7d/30d; prices 1h/24h/7d).
- Portfolio: "Two sleeves": a slow core and a fast tactical sleeve, the
  split chosen at tuning; until then fast 100% = `algo-2026-10-29`.
- History: "Re-tag all" posts since 2026-10-01 with the new prompt.
- Geopolitical Risk Index as an outside slow feed: "Later phase".

Same day, the operator: "not just war ... see the bigger picture". Shown a
source-neutral tag pipeline (seven tag kinds with the posters' horizon,
signals per tag, data-built tag-to-asset links, a scoreboard of measured
tag value, market-wide risk-off). Answers through the ask tool:

- Tag kinds: "All seven": asset, narrative/sector, macro, event/catalyst,
  entity, claim/prediction, board mood.
- Scoreboard: "Both, later decide": build it and support both rule styles
  (operator-set weights; an operator-chosen meta-rule whose weights follow
  the measured edge); the operator picks at tuning.
- Defensive: "Yes": a sleeve may hold USDT or a gold proxy (PAXG/XAUT) when
  its rule says risk-off.
- Sources: "Source-neutral, /biz/ only": the format accepts any source;
  only /biz/ is wired in this phase.

## 2026-10-31 Phase 53 loop: laptop disk (`LOOP_TAGS.md` A1)

The laptop's root disk had 32M free, so nothing could build. After the
default cleanup (`cargo clean`, dangling docker images: 7.7G free), two
answers through the ask tool, given by the loop manager for the operator:

- "Pause scraper + uv clean + unused images": the laptop `biz watch` was
  paused for `uv cache clean` (4.5GiB) and restarted; `docker image prune
  -a` removed unused tagged images (4.98GB).
- "Stale Playwright + Camoufox + puccinialin": removed Playwright 1208 and
  1217 (newest 1223 kept), the Camoufox browser (re-download with `camoufox
  fetch`) and `~/.cache/puccinialin`. Docker volumes, archive data and the
  dashboard mirror kept. Result: 16G free with the debug build present.

## 2026-10-31 Phase 53 loop: rule file (`LOOP_TAGS.md` D1)

Three answers through the ask tool, given by the loop manager for the
operator:

- Sleeve features before D3-D5, "Refuse until D5": the trader decides with
  the one sleeve holding a share and refuses to start on a rule file with
  two funded sleeves, a horizon filter other than `any` or a
  `risk_off_below` ("a validated rule file never promises trading behavior
  the trader cannot yet perform").
- Fields the ledger left open in `config/rules/algo-2026-10-29.json`,
  "Neutral, no new trading choice": fast `horizon` `any`, `defensive`
  `["USDT"]`, `risk_off_below` null; core share 0, the same neutral fields,
  buy 0.5 / sell -0.5 / 2 posters copied from fast, no weights. The core
  sleeve's trading choices stay for the tuning phase.
- Rule name, "File only": the rule file's `rules_version` is the one source;
  `BOT_SPOT_RULES_VERSION` joins `REMOVED_VARS` (warns once) and leaves the
  Pi's `.env.docker` in F2.

Manager note (iteration 17): the catalog's legacy names are `talk.6h` and
`stance.6h` (renamed from `talk` and `stance`, no aliases).

## 2026-10-31 Phase 53 loop: edge-style rule (`LOOP_TAGS.md` D2)

Three answers through the ask tool, given by the loop manager for the
operator:

- Settings, "Rule-level, all required": `edge_scoreboard` (default
  `data/scoreboard/latest.json`), `edge_scale`, `edge_cap`,
  `edge_refresh_hours`, `edge_min_t` at the rule level with no code default;
  per sleeve `edge_horizon` (1h/24h/7d) and empty `weights`.
- Formula, "Excess, t-gated, decay off": weight = clamp(edge_scale ×
  all.excess_pct, ±edge_cap) for rows with |all.t| ≥ edge_min_t that are not
  decayed; per-asset names from the `assets` row, market-wide names from the
  `basket` row.
- Valid evidence, "Point-in-time + same gate": schema `tag-scoreboard-v1`,
  usable at the decision time (`available_at`, manager note iteration 18),
  the sleeve's `min_posters`, rows for the horizon; no maximum age. Otherwise
  hold with no orders and the reason on the row.

No edge rule ships enabled; `algo-2026-10-29` stays the rule in force.
