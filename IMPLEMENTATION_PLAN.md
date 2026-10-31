# Binance Bot Implementation Plan

Active phases only. Finished and superseded phases are in `docs/archive/`;
read a section there only when the task touches it.

Rules for this file (`CODEX.md`, "Required Workflow"): every phase has
checkbox items; tick a box in the same commit as the work that finishes it;
when a phase is complete, move its whole section to `docs/archive/`.

## 52. Nonstop Algorithm Trader (operator decisions 2026-10-29)

Why: the operator replaced the LLM trader with an algorithm
(`docs/decisions.md`, "2026-10-29 algorithm trader"). Target:

- You start, stop and watch; you decide the algorithm's rules.
- The algorithm on the Pi runs continuously on live Binance data plus the
  /biz/ scores; every order goes through the existing hard safety.
- biz-app (outside this repo) is the only place AI is used: it reads /biz/
  threads and writes them as scores.
- The dashboard shows wallet, trades and the algorithm's signals; Research
  backtests the rules offline. No LLM, no chat, no timer, no Telegram.

Feed contract version 1 of the /biz/ feed (one JSON file; the trader was the
reader until Phase 53 replaced it with `tags-feed-v1`):
`schema`, `as_of` (only posts that were visible by then), `model`,
`prompt_version`, `coins.<BASE>.<1h|6h|24h>` with `mentions` (every post, no
AI), `posters` (distinct), `bullish` / `bearish` (AI tags weighted by the
1-3 post score) and `posts` (post numbers).

### Decide with the operator

- [x] The algorithm's rules: inputs (feed fields, Binance data), buy, sell
  and hold logic, position size, how often it may act, what it does while
  the feed is unavailable. Agents propose and backtest; the operator picks.
  Chosen 2026-10-29 (`docs/decisions.md`, "2026-10-29 algorithm rules";
  `reports/algo_backtest_2026-10-25.md`): weights `0,1,1,0.5,-0.5`, all cash
  into one coin, no buys without a fresh feed (held coin still scored).
- [x] How live prices arrive: REST polling every 15 s with the existing spot
  client (operator, 2026-10-29). Coins the algorithm may buy: the /biz/ feed
  coins with an eligible USDT market plus the coin held; 1h moves from the
  trader's own price polls.

### Remove the old trader

- [x] LLM path: omp session, host tools, prompts, journal, model failover,
  LLM cost control, `BOT_SPOT_DECIDER`, crate `bb-omp`, `omp` and Bun in the
  Docker image (`docs/archive/loops/LOOP_ALGO.md` C3-C7).
- [x] Alerts and social input: Telegram alerts and their cooldown file, crate
  `bb-social` (Telegram and X evidence) (`docs/archive/loops/LOOP_ALGO.md` C1-C2).
- [x] LLM-era features: GCP2 schizo state, Markov value voice
  (`bot-spot-mdp-fit`), personas and the persona backtest, exit plans
  (`set_exit_plan`, exit watch and replay, the `bot-spot-exit-watch`
  service), the market-evidence service, the 20-coin market breadth read
  and the dashboard's market-mood gauge, the Sessions tab, the 20-minute
  cycle (`--interval-seconds`). A piece the operator later picks for the
  rules comes back from git history (`docs/archive/loops/LOOP_ALGO.md` C8-C13, D7).

### Build

- [x] /biz/ feed reader `crate::biz` with its own path and age variables
  (replaced by `crate::tags` in Phase 53 C1; the old variables are in
  `REMOVED_VARS`).
- [x] The /biz/ lead study (a `bb` subcommand, replaced by `bb tag-scoreboard`
  in Phase 53 C6): the move after each feed against the same coin's random
  hours, older and newer half.
- [x] biz-app, outside this repo: `biz-insights feed` writes
  `out/biz-feed.json`; `biz-insights feed --history --step 60` writes one
  feed per hour into `out/biz-feed-history/`; `feed --watch` keeps the live
  feed fresh (`docs/archive/loops/LOOP_ALGO.md` D1-D3).
- [x] Run the lead study on the real feed history and record the verdict
  here. Verdict (2026-10-29, the lead-study report of that date in `reports/`):
  no lead. The window is short: 590 hourly feeds from 2026-10-01 to
  2026-10-25T13:00Z (24.5 days; older half to 2026-10-13T06:31Z), 30 coins,
  17,700 coin-feed samples, one AI reader (gemini-3.8-flash, `stance-1`), and
  poster IDs only on most posts from 2026-10-22. Of 45 signal buckets (24h
  talk rank, 6h-vs-24h talk surge, 6h and 24h stance; +1h/+6h/+24h), none has
  an excess over the coin's own random hours with |t_c| >= 2 in the older
  half. In the newer half only "24h stance bearish" does (+0.05% at +1h, t_c
  2.4; +0.29% at +6h, t_c 2.2): contrarian, and the older half has the other
  sign (-0.03% at +6h). Nothing holds in both halves. After the 15 bps fee 88
  of the 90 half-period bucket rows lose on average; the two that gain (24h
  talk rank 11+, older, +24h: +0.13%; 6h stance untagged, newer, +24h, n=48:
  +0.17%) lose in the other half (-1.48%, -2.02%). Thin evidence either way.
- [x] Nonstop trader: reacts to new prices and new feeds, runs the chosen
  rules, orders only through the route path with every hard-safety item,
  writes an audit row per decision and per route (`bb bot-spot --nonstop`,
  `docs/archive/loops/LOOP_ALGO.md` D7: 15 s ticks, a decision row on change plus hourly).
- [x] Backtest of the chosen rules on the feed history and Binance candles,
  report in `reports/` (`reports/algo_backtest_2026-10-25.md`, D6).
- [x] Dashboard: show the algorithm's signals and decisions; drop the LLM
  bot, journal and exit-plan views (Algorithm tab, `docs/archive/loops/LOOP_ALGO.md` C11-C12,
  D8).
- [x] Docs and config describe only the algorithm trader: `README.md`,
  `OPERATIONS.md`, `.env.example`, `docker-compose.yml` (no
  `bot-spot-exit-watch`, `bot-spot-algo-paper` or omp login; the plan-only
  service is `bot-spot-paper`), `Dockerfile`, `deploy/user/` (D9).

### Roll out

- [x] Plan-only run on the Pi; the operator reviews the planned orders
  (`bot-spot-paper` since 2026-10-30; review: "(a) Keep practising",
  `docs/decisions.md`).
- [x] Feed pipeline on the Pi (operator, 2026-10-30, "Plain Pi programs"):
  `biz-watch` and `biz-feed` systemd user units (`deploy/seref/`) write
  `docker-data/biz-feed.json` directly; the laptop copier is removed.
- [ ] Live: operator go, rollback tag `binance-bot:pre-algo-<date>`, entry in
  `PROGRESS.md`, watch the first orders.

## 53. Dynamic Tags, Scoreboard and Two Horizons (operator decisions 2026-10-30)

Why: the feed keeps only Binance coins over 24 hours and the rule is one
fixed formula, so whatever /biz/ talks about beyond coins (narratives,
macro, events, people, predictions, the board's mood) never reaches the
algorithm, and nothing measures which talk carries information
(`docs/decisions.md`, "2026-10-30 dynamic tags"). Target:

- A source-neutral tag pipeline: sources (now /biz/ only) -> AI tagger
  (reads only) -> tag registry -> signals over 1h to 30d -> the operator's
  rule. A new tag, kind of talk or basket needs no code change.
- Seven tag kinds: asset, narrative (a basket of coins), macro, event
  (dated), entity, claim (target and deadline), board mood. Every tag
  carries the posters' horizon (short, swing, long), so long theses and
  short calls feed different sleeves.
- A scoreboard measures, continuously and out of sample after fees, which
  tags and signals moved prices their way at which horizon.
- Two sleeves (slow core, fast tactical), each able to hold a coin, a gold
  proxy or USDT (risk-off). The rule file supports fixed weights and an
  edge-following meta-rule; the operator picks at tuning.
- Until then the rule file reproduces `algo-2026-10-29` (fast 100%, every
  new signal at weight 0): same scores, same actions.
- AI only reads posts and writes numbers; the rule stays the operator's;
  every order keeps every hard-safety item. Spot only.

Feed contract `tags-feed-v1` (one JSON file, replaces the version 1 /biz/ feed; the
trader, the backtest, the scoreboard and the dashboard read it):

- `schema`, `as_of`, `windows` (`1h`, `6h`, `24h`, `7d`, `30d`),
  `sources` (per source: `id`, `as_of`, `model`, `prompt_version`,
  `registry_version`, `items`, `untagged`).
- `tags.<id>` (`<kind>:<name>`, the registry id): `kind`, `name`,
  `first_seen`, `live_since`; `event_at` for events; `target` and
  `deadline` for claims; `windows.<window>`: `mentions`, `posters`,
  `threads`, `up`, `down` (score-weighted: bullish/bearish for assets and
  baskets, rising/easing for macro, events, entities and mood), `horizon`
  (score-weighted `short`, `swing`, `long`, `unstated`) and `posts` (24h and
  shorter only). Live Binance spot coins count AI-free mentions (as the
  version 1 feed did).
- `members.<narrative>`: its assets with weights (from the AI's tags and
  co-mentions).
- `links.<tag>.<asset>.<window>`: `posters`, `up`, `down`: posts that tie a
  tag to an asset.
- Only tags live at `as_of` appear. The registry
  `tags-registry.json` lists every tag: `id`, `kind`, `aliases`,
  `merged_from`, `first_seen`, `live_since`.

Not in this phase: tuning (next phase, the operator picks weights, rule
style and sleeve split); other sources (news, Polymarket, Binance futures
data, the Geopolitical Risk Index); futures trading for silver, oil and
stocks (a separate safety phase; `CODEX.md` forbids futures today).

### Feed (biz-app, outside this repo; back up first, then copy to the Pi's `~/biz-app`)

- [x] Prompt `tags-1`: per post the score and every tag of the seven kinds
  with direction, horizon, the assets it moves (with direction), the event
  date or the claim's target and deadline; tag cache keyed on `tags-1`
  (laptop, LOOP_TAGS B2; the Pi gets it in F2).
- [x] Tag registry: canonical ids, aliases, kinds; new names merged by one
  cached AI call per round (like the project merge); live at 5 or more
  distinct posters in 7d (setting); narrative baskets from tags and
  co-mentions (LOOP_TAGS B4).
- [x] `tags-feed-v1` writer for source `biz`: tags, members and links over
  1h/6h/24h/7d/30d, live tags only, replaced by rename; `--watch` and
  `--history` write it (LOOP_TAGS B1, B5, B6).
- [x] Re-tag the archive since 2026-10-01 with `tags-1` (calls and time
  logged) and write the hourly history; 30d windows full from 2026-10-31
  (laptop, LOOP_TAGS B3, B6: 721 hourly feeds to 2026-10-31T00:00Z).

### Signals and scoreboard

- [x] `crate::tags` (replaces `crate::biz`) reads `tags-feed-v1` only; test
  fixture from a real file (`tests/fixtures/tags-feed-v1.json`, LOOP_TAGS C1).
- [x] Proxies: operator file `BOT_SPOT_PROXIES` (e.g. `GOLD` -> `PAXG`,
  `XAUT`); a tag reaches a tradable asset directly, through a proxy, a
  basket membership or a link; others are scored, never bought
  (`crate::reach`, `config/proxies.json`, LOOP_TAGS C4).
- [x] Price horizons: 1h from the trader's polls, 24h from the all-symbol
  24h ticker (one request), 7d from cached 1h klines per candidate; the
  backtest derives the same from cached 1m candles (LOOP_TAGS C2, C3).
- [x] Signal catalog, each −1..1, per tag and window: direction, level,
  surge, novelty, breadth, persistence, agreement, horizon mix,
  price-confirmed; per asset the sum through direct tags, baskets, links
  and proxies; market-wide signals from macro and mood; momentum per price
  horizon. Unknown names in the rule refuse to start (LOOP_TAGS C5, D1;
  the legacy `talk.6h` stays −1..3 so `algo-2026-10-29` reproduces).
- [x] Scoreboard `bb tag-scoreboard` (replaces the /biz/ lead study): every
  tag kind and signal against its assets' (market-wide: BTC and the liquid
  basket's) moves at +1h/+24h/+7d versus random hours, after fees, older
  and newer half, decay flagged; nightly on the Pi (`docker/crontab`, 00:30
  UTC), written to `data/` (LOOP_TAGS C6, F2, F3: first Pi run 2026-10-31,
  736 hourly feeds, 1,093 rows).

### Trader

- [x] Rule file `BOT_SPOT_ALGO_RULE` replaces the env rule (weights and
  poster minimum): per sleeve its share, horizon filter,
  defensive assets, risk-off level, buy and sell levels, poster minimum,
  and either fixed weights (selected by kind, signal, window) or the
  meta-rule (weights from the newest scoreboard, capped, refreshed at a
  set interval). `algo-2026-10-29` as a rule file; a test shows it gives
  the old formula's scores and actions (LOOP_TAGS D1, D2).
- [x] Two sleeves: each holds one asset (coin, gold proxy or USDT), buys
  with its share of the portfolio value, sells only its own quantity, moves
  to its defensive asset when market-wide signals fall below its risk-off
  level; sleeve holdings in the state file, checked against balances every
  tick (a mismatch halts, fail closed); orders only through
  `execute_route` with every hard-safety item (LOOP_TAGS D3-D5).
- [x] Audit: a decision row when a sleeve's action changes plus hourly, with
  each candidate's signals, the market-wide signals and the live tags
  (LOOP_TAGS D5, D6: top 20 candidates, 10 loudest tags per kind, 32 KB row
  budget, rule file hash).

### Backtest and dashboard

- [x] `bb algo-backtest`: tag history, rule file (both styles), sleeves, a
  report per sleeve (LOOP_TAGS E1: candidates from the tags history as live,
  edge rules walk-forward on point-in-time scoreboards,
  `reports/phase53/algo_backtest_2026-10-25.md`).
- [x] Dashboard: Tags tab (live and new tags by kind, baskets, links,
  events by date, claims and their outcome); Scoreboard tab; Algorithm tab
  per sleeve with the rule's signals as columns (LOOP_TAGS E2, E3).
- [x] Docs (`README.md`, `OPERATIONS.md`, `.env.example`) and `CODEX.md`
  "rules in force" describe the pipeline, the rule file and the sleeves
  (LOOP_TAGS E4).

### Roll out

- [x] Deploy feed, scoreboard and trader together; `bot-spot-paper`
  plan-only on the `algo-2026-10-29` rule file; its decisions match the
  old rule; the operator reviews (LOOP_TAGS F2, F3: deployed 2026-10-31
  15:05 UTC; 2 h: 220/220 scores and 11/11 actions recomputed from the rule
  file, no error, halt or submission; operator via the loop manager: "(a)
  Keep practising plan-only").
