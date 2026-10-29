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

## 2026-10-29 Phase 52 loop decisions (`LOOP_ALGO.md` section A)

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
