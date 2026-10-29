# LLM Trader Era, Superseded 2026-10-29

Moved here verbatim on 2026-10-29 when the operator replaced the LLM trader
with a nonstop algorithm (`docs/decisions.md`, "2026-10-29 algorithm
trader"). Nothing in this file is in force; `CODEX.md` holds the rules.

## `CODEX.md` "Live-Money Safety Rules" as of 2026-10-29

- Bot Spot (below) is the only trader in this repo. The other modules (the
  legacy `bb main` runtime, the paper pool runtime, the canaries and live
  probe, the capital pool, margin, futures, transfer, withdrawal, prediction,
  the listing sniper, signals, the X insighter, the Solana sniper, Polymarket,
  airdrop, funding arb, trend filter and mean reversion) were moved to
  `../redundancy/binance-bot-modules` on 2026-10-29; bringing any back needs an
  operator decision and its own approved phase (their old rules are in that
  folder's `docs/rules-other-modules.md`).
- `bb-pool` is a paper/read-only library for the trader (market catalog, fees,
  route engine) with no order path; orders go only through Bot Spot.
- Market selection, conversion routing, minimum-order checks, fee estimates,
  and slippage must be derived from current Binance market data and rules, not
  from coin-specific constants.
- Phase 32 added a separate bot agentic Spot runtime (`bb bot-spot`,
  crate `bb-trader`). It was approved as the exception to the old modules'
  Phase 13/31 live-paper warm-up gate and foreground-only allocator rule. It may
  control the whole available Spot wallet when `BOT_SPOT_ENABLED=true`,
  `BOT_SPOT_DYNAMIC_SCOPE=all_eligible_spot`, `USE_TESTNET=false`,
  `DRY_RUN=false`, and `CONFIRM_LIVE_TRADING=yes` are all set, and it runs as a
  long-lived daemon (`bb bot-spot --submit-live --daemon`, systemd
  `binance-bot-spot.service`, compose service `bot-spot`). The bot runs
  through omp in RPC mode with one persistent session (resumed across restarts,
  compacted, backed by a durable journal) and acts only through the bot's host
  tools; `execute_route` is the only order path. Bot Spot must not call
  margin, futures, universal-transfer, withdrawal, or prediction write
  endpoints.
- Bot Spot model: `BOT_SPOT_MODEL` is `openai-codex/gpt-6.1-sol` at thinking
  `high` on a 20-minute cycle, with no fallback model
  (`BOT_SPOT_FALLBACK_MODELS` empty; the Pi's `omp-agent/config.yml` has
  `retry.modelFallback: false`). The failover code stays; a configured
  fallback would run with the same host tools, `execute_route` path and hard
  safety. Logins are made on the Pi with `omp login <provider>`, never a
  copied token. Adding a fallback, model or provider needs a new operator
  decision. History: `docs/decisions.md`.
- Bot Spot has no trading guardrails, by operator decision. The edge /
  market-evidence minimum, friction cap, speculative mode, loss-streak limit,
  trade cooldown, daily trade cap, proven-edge gate, rules-engine decision mode,
  and the LLM `go_off` decision flag are removed; their env vars are ignored
  with one warning. Fees, spread, slippage, short-window (`5m`/`15m`) and
  longer momentum, market evidence, social data, and trade memory are
  information in tool output for the bot to weigh, never blocking gates. Do not
  reintroduce any of them as a blocking gate without explicit operator
  approval. `BOT_SPOT_MAX_DAILY_LLM_COST_USD` is LLM cost control, not a
  trading gate. By operator decision (2026-10-14) the trader has no stop-loss
  or take-profit levels: the prompt must not ask the bot for invalidation
  prices, exit triggers or profit targets, and no automatic price-level exit
  may be added to Bot Spot without explicit operator approval.
- Exit plans (Phase 45; history and prompt tags v11-v14 in
  `docs/decisions.md`): the bot may pick a sell plan per position from the
  fixed deterministic menu (follow the peak by percent or ATR, take part,
  stretch above the Bollinger band, slowdown, momentum fade, deadline) with
  the optional `set_exit_plan` tool (setting again replaces, `clear` removes);
  otherwise `BOT_SPOT_DEFAULT_EXIT_PLAN` (`atr=3.5:1h,deadline=360`) applies
  to positions opened from `BOT_SPOT_DEFAULT_EXIT_PLAN_FROM`. The bot never
  writes or runs code, and the prompt may offer exit plans but must not
  require them. With `BOT_SPOT_EXIT_PLANS_LIVE=true` the `bot-spot` daemon
  sells a position into USDT when its plan fires, only through
  `Runtime::exit_sell`: the cycle lock (never during a bot route), live
  arming, the emergency stop file, the failed-route halt, operator scope,
  `order/test` per leg, exchange filters, fill reconciliation, and one durable
  audit row (`cycle_id` `exit-plan`) before the alert; an ambiguous sell halts
  live trading like any route. Only the trader writes the watch files then;
  the paper watcher `bot-spot-exit-watch` refuses to start and never holds an
  order path. Exit plans only sell, never buy; `stretch` sells only above
  break-even (entry plus both fees). Changing the default plan or the menu
  needs a new operator decision.
- Schizo trader and long-term value voice (Phases 49-50, LLM decider; history
  in `docs/decisions.md`): the LLM stays the trader and acts only through the
  host tools. GCP2 (gcp2.net) public data is read-only information: never log
  in or post. Mood switches are bound to GCP2 only (no timer, fixed switch
  probability or seed; zone bounds from GCP2's `getcolorzones`, dice from
  GCP2 readings): the mood switches when the zone changed between cycles and
  holds while it stays the same; a failed read means no switch. YOLO is
  removed and there is no panic sell; bringing either in needs a new operator
  decision. The Markov value voice (`crate::mdp`, `BOT_SPOT_MDP_*`) is
  information, never a gate; a missing model means "voice unavailable". A
  `schizo_mdp` backtest model must be fitted on data that ends before the
  window.
- Phase 51 rule-based decider (operator decisions 2026-10-22 "llm picking
  stuff was a bad idea", "i always should have traded via functions";
  2026-10-28 "from now on decisions are not taken by an llm but by an
  algorithm" fed by /biz/ readings from biz-app and Binance data; Levels 1-5
  of the Phase 51 plan approved 2026-10-28/29). This replaces the earlier
  removal of a rules-engine decision mode for this decider only
  (`BOT_SPOT_DECISION_MODE` stays removed).
  - `BOT_SPOT_DECIDER` picks who decides each cycle: `llm` (the default, the
    omp session) or `algo` (`crate::decider`, no LLM is started). The algo
    needs `BOT_SPOT_ALGO_WEIGHTS`; a missing or bad value refuses to start.
  - Algo orders go only through `execute_route`, so every hard-safety item
    below applies unchanged; a failed order stops that cycle's remaining
    orders. The rule sells on its score only: no stop-loss or take-profit
    price levels are added (the 2026-10-14 decision stands).
  - Order of rollout: the `bot-spot-algo-paper` plan-only service first;
    switching the live `bot-spot` to `algo` needs the operator's go after that
    practice run, a rollback image tag (`binance-bot:pre-algo-<date>`) and an
    entry in `PROGRESS.md`. Going back is `BOT_SPOT_DECIDER=llm` and a
    restart.
  - The /biz/ feed stays information for the LLM path (recorded, not shown in
    the prompt); only the algo reads it to decide.
- Bot Spot hard safety stays always on and fails closed: the live arming
  flags, the emergency stop file checked at cycle start and before every route,
  leg, and order, the exclusive run lock, account READING plus canTrade,
  Binance `order/test` before every leg, exchange filters with orders sized
  from live balances net of authenticated per-symbol taker fees plus slippage
  and rounding reserve, per-leg fill reconciliation that halts on anything
  ambiguous, operator scope (allow/block lists, blocked quotes, default fiat
  blocking), Spot MARKET orders only with at most `BOT_SPOT_MAX_ROUTE_LEGS`
  legs, and the failed-route halt. A route that submitted an order and then
  failed, or whose submission was ambiguous, halts live trading until the
  operator reviews it and runs `bb bot-spot --mark-failed-route-reviewed`;
  nothing else may clear it. Never weaken or bypass any hard-safety item.
- Bot Spot audit is one fsync'd JSONL row per route or cycle, written before
  any Telegram alert, preserving submitted-leg truth after any partial route
  failure. Signed Binance request details must stay out of logs.
- Phase 35 social-signal intelligence is read-only evidence collection and
  context enrichment only. It may ingest Telegram Bot API updates visible to a
  configured bot and official X API search results, normalize/hash records, and
  feed per-asset evidence to Bot Spot through `get_social_evidence`. It must
  not bypass platform terms, scrape private chats, use browser automation for
  X, store unnecessary raw personal data,
  or create an order-submission path. Social evidence is information for the bot
  only and must never bypass exchange rules, order-test, emergency-stop, or
  reconciliation.
- Phase 36 Bot Spot Telegram decision explanations are observability-only.
  They must be derived from the completed cycle result/audit data, must not
  make a separate LLM call, and must not change live routing, prompt, order-test,
  emergency-stop, audit, or reconciliation behavior. Long explanations are
  delivered through safe message splitting.
- Binance does not safely expose the full API-key symbol whitelist. Live-capable
  features must use the local/operator-owned scope and fail closed when a
  requested symbol or asset is outside `API_SYMBOL_WHITELIST` or
  `API_ASSET_WHITELIST`.
- Withdrawals remain hard-blocked by policy unless a future withdrawal-specific
  safety phase is explicitly approved.
- The `cryptobot-gui` dashboard (crate `crates/cryptobot-gui`) is read-only and
  localhost-only. It may read bot files and query Binance through signed and
  public market-data GET endpoints, but it must not write files, toggle the emergency stop, clear the
  failed-route halt, or call any order, transfer, withdrawal, margin or futures
  write endpoint. It keeps the loopback `Host` check and must not be published
  on a non-loopback interface; adding controls needs a separate reviewed phase
  with authentication.

## `docs/decisions.md` entries as of 2026-10-29

### Bot Spot model and fallback

- Bot Spot model fallback (operator decisions 2026-10-13, 2026-10-15,
  2026-10-16 and 2026-10-19): the primary model (`BOT_SPOT_MODEL`) is
  `openai-codex/gpt-6.1-sol` at thinking `high` since 2026-10-16 (before
  that `anthropic/claude-sonnet-5-5` from 2026-10-14 and
  `anthropic/claude-haiku-5-5`), on a 20-minute deployed cycle since
  2026-10-15 so it fits the subscription allowance. Since 2026-10-19 there is
  no fallback model (operator: "remove backup", after the
  `google-antigravity/gemini-4-argon-high` fallback answered "quota
  reached"): `BOT_SPOT_FALLBACK_MODELS` is empty and the Pi's
  `omp-agent/config.yml` has `retry.modelFallback: false` and no
  `fallbackChains`. A failed turn is not retried on another model; the
  brain-down alert (`BOT_SPOT_BRAIN_DOWN_ALERT_CYCLES`) still fires. The
  failover code stays: when a fallback is configured, a fallback model is
  the trader for that turn with the same host tools, the same
  `execute_route` path and every hard-safety item below unchanged. Each
  login is made on the Pi with `omp login <provider>`, never a copied token.
  Adding a fallback or another model or provider needs a new operator
  decision.

### Phase 45 exit plans and prompt tags v11-v14

- Phase 45 bot-chosen exit plans (operator approval 2026-10-14): The bot may
  pick a sell plan per position from a fixed, deterministic menu (follow the
  peak by percent or ATR, take part, stretch above the Bollinger band,
  slowdown, momentum fade, deadline) by setting its parameters; The bot
  never writes or runs code. Approved: the historical replay
  (`bb bot-spot-exit-replay`) and, after the operator reviewed it, the
  paper trial: the optional `set_exit_plan` host tool and the
  `bot-spot-exit-watch` watcher, which records would-be sells and must
  never place an order or hold an order path. The prompt may offer the tool
  but must not require exit plans.
- Phase 45 live exit plans (operator decision 2026-10-15, before a paper
  run, after the 40-trade backtest): with `BOT_SPOT_EXIT_PLANS_LIVE=true`
  the `bot-spot` daemon follows positions between cycles and sells a
  position into USDT when its plan (The bot's from `set_exit_plan`, or the
  default `BOT_SPOT_DEFAULT_EXIT_PLAN`, deployed `deadline=120` since
  2026-10-17 (was `stretch=2:1,atr=3:1h`),
  for positions opened from `BOT_SPOT_DEFAULT_EXIT_PLAN_FROM`) fires.
  These sells use `Runtime::exit_sell`: the cycle lock (never during a
  bot route), live arming, the emergency stop file, the failed-route
  halt, operator scope, `order/test` per leg, exchange filters, fill
  reconciliation, and one durable audit row (`cycle_id` `exit-plan`) before
  the alert; an ambiguous sell halts live trading like any route. Only the
  trader writes the watch files then; the paper watcher refuses to start.
  Exit plans only sell, never buy. Changing the default plan or the menu
  needs a new operator decision. Operator decision 2026-10-17 (branch
  `old-gold-way`): back to the v7 system after the fixed plan (2026-10-16)
  and locked rules (2026-10-17) cut winners short. The bot's `set_exit_plan`
  (full menu, deadline included; setting again replaces, `clear` removes)
  overrides the default per position, and the system prompt is the v7 text
  (tag `bot-spot-v11-agentic`). Kept from 2026-10-17: `stretch` sells
  only when its line is above break-even (entry plus buy and sell fees at
  the trip's fee rate), and the cycle prompt and trade history show the bot
  which rule sold each position and the result after fees (information
  only). The trader session was restored to the v7 conversation up to the
  last gpt-6.1-sol cycle before Sonnet took over (cycle 542, 2026-10-16
  05:02 UTC), with the journal cut at the same point. Tag
  `bot-spot-v12-agentic` (2026-10-18) keeps the v7 system prompt and changes
  only information in the cycle message: the route table's trailing momentum
  is no longer labelled "edge" (it is a past move, not a forecast). Tag
  `bot-spot-v13-agentic` (operator approval 2026-10-18, Phase 47) adds 20-coin
  market breadth (`15m/1h/4h/24h`), hold-duration buckets and `+4h` entry timing
  to the cycle prompt, switches the candidate table to `liquid_top_value`
  (excluding stablecoins, fiat and `TRD_GRP_261` tokenized stocks as buy
  targets), updates the default exit plan to `atr=3.5:1h,deadline=360`, and
  replaces the `"Holding is a decision, not a default"` line in the system
  prompt with asymmetric regime/swing guidance (information and guidance only,
  never a blocking gate). Tag `bot-spot-v14-agentic` (operator decision
  2026-10-19) changes only information in the cycle message: live, the
  exit-plan section ends with the real results of the positions a plan sold
  ("positions sold by a plan: N (W wins), net X USDT after fees") instead of
  "plan vs your own exits", which in live mode only measured trigger price
  against fill.

### Phases 49-50 schizo trader and long-term value voice

- Phase 49 schizo trader (operator decisions 2026-10-20,
  `docs/archive/loops/LOOP_SCHIZO_LIVE.md`).
  The operator asked for a "schizo" Bot Spot: "it is not schizo enough", "ai
  needs to be schizoified or become schizo thats it", "on top of those it looks
  at gcp2 data for reference …", and "Backtest, then trade live as schizo", with
  GCP2 as "Mood ring" and "YOLO button".
  - The LLM trader stays the trader. It becomes schizo through its prompt and
    a per-cycle state: an active mood (one of the Phase 48 personas), dice and
    the personas' votes. Its behaviour is not replaced by rules, and it still
    acts only through the host tools; `execute_route` stays the only order path.
  - GCP2 (gcp2.net, Global Consciousness Project 2.0) public data is read-only
    information. It may be read without login (public colour zones,
    per-minute aggregate, monthly history) and must never be posted to or
    logged into. Mood switches are bound to GCP2 only ("nothing hardcoded …
    we are going to be gcp bound in mood switches"): no timer, no fixed
    switch probability, no fixed seed. The zone bounds come from GCP2's own
    `getcolorzones`, and the dice come from GCP2 readings.
  - The mood follows GCP2 (operator decision 2026-10-20, after the first
    inverted deploy: "reverse mood change meaning if gcp is normal he is
    normal aight inverse of what it is rn"): the bot switches mood when the
    GCP2 zone changed between cycles (up or down) and keeps it while the zone
    stays the same. The first deploy ran it inverted ("mood is inverted not
    yolo": switch on the same zone, keep on a change). A failed GCP2 read means
    no mood switch for that cycle.
  - YOLO is removed (operator decision 2026-10-21: "remove yolo as well if it
    still persists it is a stupid decision making way"). Until then, while
    GCP2 was Very High or Extreme, the bot was told to sell other holdings and
    go all-in on the hottest liquid coin. Bringing it back needs a new operator
    decision.
  - There is no panic sell ("no panic sell"). The 2026-10-14 no-stop-loss rule
    above stays in force, and exit plans keep the existing menu and default.
  - Go-live is not gated on the backtest ("go live no tiny shit whatever we
    said earlier will be live"): the schizo trader runs on the full wallet with
    no size cap.
  - Every Bot Spot hard-safety item below stays unchanged.
  - Tag `bot-spot-v15-schizo` (`BOT_SPOT_GCP2_ENABLED=true`, the default) was
    the first schizo trader: the system prompt replaces the v13 regime/swing
    bullets with the four personalities, the inverted GCP2 mood ring, the
    voices (votes), the one-coin all-cash bet and the YOLO order, and the
    cycle message opens with `## Your schizo state`. Its no-stop-loss,
    exit-plan, hard-safety and tool text is the v14 text. Tag
    `bot-spot-v16-schizo` changes only the mood ring: the mood follows GCP2.
    `BOT_SPOT_GCP2_ENABLED=false` gives the v14 prompts.
- Phase 50 long-term value voice (operator decisions 2026-10-21): "we need to
  create a reward function alike system that has gamma value around 0.6 or
  something that would make him more wanting to achieve longer term rewards
  that influences directly the ai's decisions", "we will use markov's stuff
  closer to it", "backtest as well". Chosen through the ask tool: γ = 0.95 per
  20-minute cycle (the operator's 0.6 per cycle would have a 50-minute
  horizon, shorter than the trader's holds), a strong voice in the prompt, and
  live directly.
  - `bb bot-spot-mdp-fit` fits a Markov decision process on cached 1m candles
    (`crate::mdp`): states of a held coin (gain since entry, time held, 1h
    move, market breadth), hold or sell, rewards in percent after costs, value
    iteration with γ. The trader reads the model file
    (`BOT_SPOT_MDP_MODEL_PATH`) every cycle and shows Q(hold) against Q(sell)
    for the held coin and the value of buying each liquid coin
    (`## Your long-term value`, audit field `mdp`).
  - The voice is information and guidance for the LLM, never a gate: the
    system prompt calls it the strongest voice and asks for a written reason
    to act against it, and the bot still decides. A missing or unreadable
    model means "voice unavailable" for that cycle.
  - Tag `bot-spot-v17-schizo` (`BOT_SPOT_MDP_ENABLED=true`, the default) is
    v16 plus this voice and minus YOLO. The no-stop-loss rule, exit plans and
    hard safety are unchanged.
  - The backtest persona `schizo_mdp` (offline only) lets the model overrule
    the votes; its model must be fitted on data that ends before the window.

### Phase 51 rule-based decider

Active: the decision wording and the rules are in `CODEX.md`, "Phase 51
rule-based decider". Move them here when the phase is finished.

## `IMPLEMENTATION_PLAN.md` Phase 51 as of 2026-10-29 (superseded by Phase 52)

### 51. /biz/ Feed And Rule-Based Decider (operator request 2026-10-28)

Why: the operator wants buys and sells taken by an algorithm instead of an
LLM, fed by Binance data and by /biz/ (4chan) readings from biz-app
(`~/projects/automation/imageboards/biz-app`), whose AI (Gemini 3.8 Flash
through omp) already tags notable posts' coins bullish, bearish or neutral.
The operator chose to study the /biz/ insight first and design the algorithm
from what the study finds. A rule-based decision mode was removed by an
earlier operator decision (`CODEX.md`), so going live needs a new recorded
decision.

Feed contract `biz-feed-v1` (one JSON file; the bot is the reader):
`schema`, `as_of` (only posts the bot could have seen by then), `model`,
`prompt_version`, `coins.<BASE>.<1h|6h|24h>` with `mentions` (every post, no
AI), `posters` (distinct), `bullish` / `bearish` (AI tags weighted by the
1-3 post score) and `posts` (post numbers).

- [ ] Level 1, biz-app: `biz-insights feed [--at <time>]` writes
  `out/biz-feed.json` (mention counter over every post, name merge to Binance
  symbols, visibility delay). Not built: outside this repository.
- [ ] Level 2, biz-app: `biz-insights feed --history --step 60` writes one
  feed per hour into `out/biz-feed-history/`.
- [x] Level 2, bot: `bb biz-lead-study` (`bb-strategies::biz_lead_study`)
  measures the move after each feed against the same coin's random hours,
  older and newer half; `reports/biz_lead_study_<date>.{md,csv}`.
- [ ] Run the study on the real feed history and record the verdict.
- [x] Level 3, code: `scripts/biz_feed_to_pi.sh` copies the feed to the Pi;
  `crate::biz` reads it every cycle (`BOT_SPOT_BIZ_FEED_PATH`,
  `BOT_SPOT_BIZ_MAX_AGE_MINUTES`) into the cycle summary (audit field `biz`),
  information only; the dashboard's /biz/ tab shows the newest one.
- [ ] Level 3, deploy: rebuild the image on the Pi, start the copier, check
  the first `biz` fields.
- [x] Level 4, code: `crate::decider` (score = weighted /biz/ talk, stance,
  1h momentum, long-term value; sell below / buy at levels) and persona
  `biz_algo` with `--biz-feeds` (point-in-time `BizHistory`), the
  older/newer split, a 72/144-set weight grid tuned on the older half or
  `--biz-weights`, and the `biz_algo` rule section and trace.
- [ ] Level 4, run: on the real feed history (needs biz-app's `--history`)
  with weights from the lead study or the grid; record the `@newer` verdict.
- [x] Level 5, code: `BOT_SPOT_DECIDER=llm|algo` (`BOT_SPOT_ALGO_WEIGHTS`,
  `BOT_SPOT_ALGO_MIN_POSTERS`); algo cycles skip omp and send the decider's
  orders through `execute_route`; audit `decider` on every row and `algo` on
  algo cycle summaries; Telegram `Decider:` line; Bot tab details; compose
  service `bot-spot-algo-paper` (profile `algo-paper`, plan-only); operator
  decision recorded in `CODEX.md`.
- [ ] Level 5, practice: run `bot-spot-algo-paper` on the Pi and review its
  planned routes.
- [ ] Level 5, live: operator go, rollback tag `binance-bot:pre-algo-<date>`,
  `BOT_SPOT_DECIDER=algo` for `bot-spot`, watch the first cycles.
