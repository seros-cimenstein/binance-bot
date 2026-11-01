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

Superseded for live trading on 2026-10-31 ("2026-10-31 snap-back rule live");
the rule keeps running plan-only in `bot-spot-paper` for its tuning.

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

## 2026-10-31 Phase 53 loop: sleeve holdings (`LOOP_TAGS.md` D3)

Three answers through the ask tool, given by the loop manager for the
operator:

- Order sizing in D3, "Sells + booking in D3": a sell spends exactly the
  sleeve's quantity (capped at the free balance), a filled live buy books
  destination free after − before into the sleeve, a filled sell closes the
  holding; plan-only books nothing. Share-sized buys and the
  below-min-notional hold stay in D4.
- Review command, "Only if it reconciles now": `--mark-sleeves-reviewed`
  re-reads the balances and records the review only when the state loads and
  reconciles; it never rewrites the state ("acknowledgment cannot silently
  reduce or discard persisted sleeve ownership").
- Scope, "Plan-only and live": both traders reconcile every tick and halt;
  `execute_route` refuses every route while the halt stands.

Manager note (iteration 19): a missing state file or one without `sleeves`
migrates; an unreadable file or malformed `sleeves` halts; unowned balances
never block buys and are never sold; the halt survives restarts and no tick
clears it.

## 2026-10-31 Phase 53 loop: per-sleeve decisions (`LOOP_TAGS.md` D5)

Three answers through the ask tool, given by the loop manager for the
operator:

- Horizon filter, "Own tag, 7d, dominant": a sleeve with `horizon` short,
  swing or long buys only coins whose own asset tag's 7d horizon mix has that
  horizon largest (ties count for each; no stated horizon: excluded); held
  coins are always scored and may be sold.
- Risk-off, "Market names, best defensive": the sleeve's market-wide score is
  Σ weight × value over its weighted `market.*` names (0 without a feed);
  below `risk_off_below` the target is its priced defensive asset with the
  highest score (USDT 0, ties to the earlier listed; compared on the
  sleeve's other names); a non-target holding is sold and the target bought
  without the buy level; nothing else is bought.
- Rows, "Active sleeves only": a sleeve with a share or a holding decides,
  with its own decision key, heartbeat and rows; the shipped rule (core 0,
  nothing held) writes exactly today's rows.

Manager note (iteration 21): sleeve identity on decisions, orders, fill
booking, trades and memory; the shipped fast=1/core=0 rule unchanged;
alternate splits, horizon filters and defensive assets only in tests or
isolated smoke fixtures.

## 2026-10-31 Phase 53 loop: decision-row budget (`LOOP_TAGS.md` D6)

Manager note (iteration 22): the budget applies to the whole serialized row,
`run_config` and every summary included, not only the candidate list; tested
on the full B5 feed with enough weighted signals to force trimming; the
sleeve's action and reason survive every trim; `run_config.rule` records the
rule file's `rules_version`, style and content hash; the shipped rule file is
unchanged. Defaults from the ledger: 32 KB, top 20 candidates, top 10 tags per
kind by posters.

## 2026-10-31 Phase 53 loop: the rule-file backtest (`LOOP_TAGS.md` E1)

Manager note (iteration 23): keep a runnable pre-E1 binary with its
invocation and baseline on the comparison window
(`~/binance-bot-prechange/bb-pre-e1-84e3a01`, README there; the older
`bb-3eb390c` reads the v1 feed and is no tags-input baseline); compare on the
same tags history with the unchanged shipped rule, separating candidate-set
differences from behavioral regressions; exercise two sleeves sharing an
asset and edge walk-forward on scoreboards whose outcomes are available by
each decision time.

Implementation choices (no trading parameter): no tuning and no
older/newer split (the next phase tunes); sleeves share the USDT and size as
live (a buy spends its share of the portfolio value, capped by the cash
left); an edge rule reads a directory of point-in-time scoreboards
(`--scoreboards`), the newest usable at each step; the cache must hold PAXG
and XAUT besides the liquid 20.

## 2026-10-31 Phase 53 loop: the dashboard's Tags tab (`LOOP_TAGS.md` E2)

Asked through the ask tool, answered by the loop manager for the operator:
claims show "Status only": `open` until the deadline's day ends, `deadline
passed` after (against the feed's `as_of`), with the posters' 7d lean; no
price check ("free-text targets cannot support a reliable price-based outcome
without guessing").

## 2026-10-31 Phase 53 loop: the Pi dashboard shows the practice run (`LOOP_TAGS.md` F2)

Asked through the ask tool at the rollout, answered by the loop manager for
the operator: "Paper while live is stopped" ("Use the running paper trader's
files so the dashboard shows current sleeves and decisions, with reversible
compose overrides documented for removal when live trading resumes").
`docker-compose.yml` gives `cryptobot-gui` `BOT_SPOT_AUDIT_PATH` and
`BOT_SPOT_STATE_PATH` of `bot-spot-paper`; the going-live steps
(OPERATIONS.md) remove them. Before, the Pi dashboard showed the live
trader's files, last written 2026-10-22.

## 2026-10-31 Phase 53 rollout review (`LOOP_TAGS.md` F3)

Asked through the ask tool after two hours of the Phase 53 plan-only run on
the Pi (15:05:38-17:05:38 UTC: feed fresh, at most 16 minutes old; no error
row, halt or submission; 220 of 220 recorded scores and 11 of 11 actions
recomputed from `config/rules/algo-2026-10-29.json`; core sleeve unfunded;
scoreboard written; 8 planned buys alternating STX and DOGE), answered by the
loop manager for the operator: "(a) Keep practising plan-only" ("the two-hour
checks passed, while live bot-spot remains stopped and the trading rule stays
unchanged"). `bot-spot-paper` keeps running; nothing was changed on the Pi.

## 2026-10-31 snap-back rule live

Shown to the operator: the Pi's tag scoreboard of 2026-10-31 (736 hourly
feeds; no tag signal with |t| ≥ 2 and a positive net; `stance.6h` slightly
contrarian at +1h, t −2.02; `momentum.1h` about zero; `momentum.24h` at +24h
−1.36% excess, t −2.48, the same sign in both halves) and agent backtests of
candidate rule files on the tags history 2026-10-01T14:00Z..10-25T14:00Z (25
bps per round trip): shipped −21.05% (15 round trips); `momentum.24h` −1 with
buy 0.8 / sell −0.3 +0.21% older half, +7.81% newer half, +8.18% whole window
(6 round trips); the same weight at ±0.5 −5.96%. Risks named: 6 round trips,
in-sample (the scoreboard that suggested it covers the window), sensitive to
the levels.

- Operator, 2026-10-31: "I want that snapback winning algorithm to be on live
  and for the tagging enabled one I want to tune up its way of operation ...
  let's get the snapback live first then we will look into how to make it
  tuned".
- Rule: `config/rules/snapback-2026-10-31.json`, rule set
  `snapback-2026-10-31`, style fixed: sleeve `fast` share 1, score = −1
  `momentum.24h` (the coin's 24h move capped at ±10%, divided by 10%); buy the
  best candidate with all USDT at score ≥ +0.8 (it fell 8% or more in 24h);
  sell below −0.3 (it rose more than 3% in 24h). Sleeve `core` share 0, no
  weights. Candidates as before: the coins the fresh tags feed's live asset
  tags reach; no fresh feed, no buys.
- Rollout, through the ask tool: "Live now, skip plan-only": the `CODEX.md`
  plan-only step is waived for this rule set only; the rest of the rollout
  rule (rollback image tag, `PROGRESS.md` entry) stands.
- Practice run, through the ask tool: "Keep the tag rule (p0)":
  `bot-spot-paper` keeps running `algo-2026-10-29` plan-only (its own
  `BOT_SPOT_ALGO_RULE` in `docker-compose.yml`) as the base for tuning the
  tag rule; the dashboard reads the live files again.

## 2026-11-01 tag-rule tuning: scope and judging

Shown to the operator: the live snap-back's first trade (49.3 TRX at 0.303
on 2026-10-31 23:20 UTC, open), the scoreboard of 2026-11-01 (745 feeds; tag
direction signals of narrative, macro and event negative at |t| ≥ 2, only 10
of 1,093 rows at |t| ≥ 2) and a tuning process: search on 10-01..10-25, judge
finalists once on later data, the winner practises plan-only, the operator
decides live. Answers through the ask tool:

- Families, all five ("lets test them all basically"): fade the board
  (contrarian tag weights), tags + snap-back, edge style, risk-off from
  macro/mood, candidate set (tag-picked coins against the liquid 20).
- Judging: "Now, 10-25 to today".
- Snap-back: "No, leave it alone": the live rule is not tuned.

Pass bar, fixed before any judging run (agent proposal shown with the
process): on the tuning window 2026-10-01T14:00Z..10-25T14:00Z, net > 0 in
both 12-day halves and at least 10 round trips, 25 bps per round trip; then,
once, on the judging window 2026-10-26T02:00Z..11-01T02:00Z, net above cash,
holding BTC, holding the liquid 20 equally and the live snap-back rule on the
same window. Signals for fade-the-board and edge weights come only from
scoreboards usable by the decision time (`--as-of`), never from later data.
