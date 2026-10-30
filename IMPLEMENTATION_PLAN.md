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

Feed contract `biz-feed-v1` (one JSON file; the trader is the reader):
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
  Docker image (`LOOP_ALGO.md` C3-C7).
- [x] Alerts and social input: Telegram alerts and their cooldown file, crate
  `bb-social` (Telegram and X evidence) (`LOOP_ALGO.md` C1-C2).
- [x] LLM-era features: GCP2 schizo state, Markov value voice
  (`bot-spot-mdp-fit`), personas and the persona backtest, exit plans
  (`set_exit_plan`, exit watch and replay, the `bot-spot-exit-watch`
  service), the market-evidence service, the 20-coin market breadth read
  and the dashboard's market-mood gauge, the Sessions tab, the 20-minute
  cycle (`--interval-seconds`). A piece the operator later picks for the
  rules comes back from git history (`LOOP_ALGO.md` C8-C13, D7).

### Build

- [x] /biz/ feed reader `crate::biz` (`BOT_SPOT_BIZ_FEED_PATH`,
  `BOT_SPOT_BIZ_MAX_AGE_MINUTES`).
- [x] `bb biz-lead-study`: the move after each feed against the same coin's
  random hours, older and newer half.
- [x] biz-app, outside this repo: `biz-insights feed` writes
  `out/biz-feed.json`; `biz-insights feed --history --step 60` writes one
  feed per hour into `out/biz-feed-history/`; `feed --watch` keeps the live
  feed fresh (`LOOP_ALGO.md` D1-D3).
- [x] Run `bb biz-lead-study` on the real feed history and record the verdict
  here. Verdict (2026-10-29, `reports/biz_lead_study_2026-10-29.{md,csv}`):
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
  `LOOP_ALGO.md` D7: 15 s ticks, a decision row on change plus hourly).
- [x] Backtest of the chosen rules on the feed history and Binance candles,
  report in `reports/` (`reports/algo_backtest_2026-10-25.md`, D6).
- [x] Dashboard: show the algorithm's signals and decisions; drop the LLM
  bot, journal and exit-plan views (Algorithm tab, `LOOP_ALGO.md` C11-C12,
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

## 53. Dynamic Tags and Two Horizons (operator decisions 2026-10-30)

Why: the feed keeps only Binance coins over 24 hours and the rule sees one
fixed formula, so a story like "war is coming, hold silver" never reaches
the algorithm (`docs/decisions.md`, "2026-10-30 dynamic tags"). Target:

- biz-app tags assets, themes and theme-to-asset links per post; new tags
  register themselves, merge with synonyms and go live once enough distinct
  posters use them. No code change for a new tag.
- The trader scores every candidate on terms over 1h to 30d, from a rule
  file the operator writes, in two sleeves (slow core, fast tactical).
- Until the operator tunes it, the rule file reproduces `algo-2026-10-29`
  (fast 100%, every new term at weight 0): same scores, same actions.
- AI still only reads posts and writes numbers; the rule stays the
  operator's; every order keeps every hard-safety item. Spot only.

Feed contract `biz-feed-v2` (one JSON file, replaces v1; the trader is the
reader):

- `schema`, `as_of`, `model`, `prompt_version` (`stance-2`), `posts`,
  `untagged_posts`, `windows` (`1h`, `6h`, `24h`, `7d`, `30d`).
- `assets.<NAME>.<window>`: `mentions`, `posters`, `bullish`, `bearish`
  (score-weighted), `posts` (24h and shorter only). Names are registry ids:
  coins by ticker, others by name (`GOLD`, `SILVER`, `NVDA`).
- `themes.<id>.<window>`: `mentions`, `posters`, `up`, `down`
  (score-weighted: the poster sees the theme growing or easing).
- `links.<theme>.<NAME>.<window>`: `posters`, `bullish`, `bearish`: posts
  that tie the theme to the asset.
- Only live tags appear. The registry `biz-tags.json` (next to the feed)
  lists every tag: `id`, `kind` (asset or theme), `aliases`, `first_seen`,
  `live_since`.

Not in this phase: tuning the new terms and the sleeve split (next phase,
the operator picks); the Geopolitical Risk Index feed (later); futures for
silver, oil and stocks (a separate safety phase, `CODEX.md` forbids futures
today).

### Feed (biz-app, outside this repo; back up first, then copy to the Pi's `~/biz-app`)

- [ ] Prompt `stance-2`: per post the score, assets with stance, themes with
  direction (up, down, neutral) and the assets each theme moves with stance;
  tag cache keyed on `stance-2`.
- [ ] Tag registry `biz-tags.json`: canonical ids and aliases; new names
  merged by one cached AI call per round (like the project merge); a tag
  goes live at 5 or more distinct posters in 7d (setting, default 5).
- [ ] `biz-feed-v2` writer: assets, themes and links over 1h/6h/24h/7d/30d,
  live tags only, replaced by rename; `--watch` and `--history` write v2.
- [ ] Re-tag the archive since 2026-10-01 with `stance-2` (calls and time
  logged) and write the hourly v2 history; 30d windows are full from
  2026-10-31 on.

### Trader

- [ ] `crate::biz` reads `biz-feed-v2` only (assets, themes, links); test
  fixture from a real v2 file.
- [ ] Proxies: operator file `BOT_SPOT_PROXIES` (e.g. `GOLD` -> `PAXG`,
  `XAUT`); a proxy candidate scores on its own tags plus the proxied
  asset's; assets without a Spot market or proxy are scored, never bought.
- [ ] Price horizons: 1h from the trader's polls, 24h from the all-symbol
  24h ticker (one request), 7d from cached 1h klines per candidate; the
  backtest derives the same from cached 1m candles.
- [ ] Term catalog, each −1..1: talk, stance and theme pressure (live
  themes through their links) per forum window; momentum per price horizon.
  An unknown term in the rule refuses to start.
- [ ] Rule file `BOT_SPOT_ALGO_RULE` replaces `BOT_SPOT_ALGO_WEIGHTS` and
  `BOT_SPOT_ALGO_MIN_POSTERS`: per sleeve its share, terms with weights,
  buy and sell levels, poster minimum. `algo-2026-10-29` as a rule file;
  a test shows it gives the old formula's scores and actions.
- [ ] Two sleeves: each holds at most one asset, buys with its share of
  the portfolio value and sells only its own quantity; sleeve holdings in
  the state file, checked against balances every tick (a mismatch halts,
  fail closed); orders only through `execute_route` with every hard-safety
  item.
- [ ] Audit: a decision row when a sleeve's action changes plus hourly, with
  every candidate's terms and the live tags.

### Backtest and dashboard

- [ ] `bb algo-backtest`: v2 history, rule file, sleeves, a report per
  sleeve.
- [ ] Dashboard: /biz/ tab with assets (tradable, proxy, scored only),
  themes, links and the registry (new tags, live since); Algorithm tab per
  sleeve with the rule's terms as columns.
- [ ] Docs (`README.md`, `OPERATIONS.md`, `.env.example`) and `CODEX.md`
  "rules in force" describe the rule file and the sleeves.

### Roll out

- [ ] Deploy feed v2 and the trader together; `bot-spot-paper` plan-only on
  the `algo-2026-10-29` rule file; its decisions match the old rule; the
  operator reviews.
