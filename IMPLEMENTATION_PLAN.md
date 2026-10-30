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
  `BOT_SPOT_BIZ_MAX_AGE_MINUTES`) and the copier `scripts/biz_feed_to_pi.sh`.
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
- [ ] Live: operator go, rollback tag `binance-bot:pre-algo-<date>`, entry in
  `PROGRESS.md`, watch the first orders.
