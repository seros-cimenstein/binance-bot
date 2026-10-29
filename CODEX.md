# Codex Working Notes

Binding for every session and subagent. `AGENTS.md` says which file answers
which question; read others only when the task needs them.

## Required Workflow

- One place per fact:
  - `CODEX.md`: rules in force (workflow, live-money safety).
  - `PROGRESS.md`: the current state only (live or stopped, deployed image
    and rollback tag, open items, next step). Rewrite it; do not append a
    log. Keep it under about 80 lines.
  - `IMPLEMENTATION_PLAN.md`: the active phases with checkboxes.
  - `docs/decisions.md`: dated operator decisions with their wording.
  - git commit messages: what changed and how it was verified.
  - `docs/archive/`: finished phases, the old progress log, finished loop
    ledgers. Read on demand; do not edit except to append moved sections.
- Before a meaningful roadmap change, make sure the active phase has checkbox
  items. Tick a box in the same commit as the work that finishes it; no
  separate tick, marker or progress-only commits. When a phase is complete,
  move its section to `docs/archive/implementation-plan-phases-1-50.md` (or a
  new archive file) and its finished decisions to `docs/decisions.md`.
- At the end of a meaningful session, update `PROGRESS.md` to the new state
  in the same commit as the work.
- A new operator decision goes into `docs/decisions.md` (wording, date) and,
  if it changes a rule, into `CODEX.md`.
- Do not claim a plan item is complete unless tests or a concrete code path back
  up the claim.
- Prefer small implementation batches that end with verification: `cargo
  check` and the targeted tests while working, the full suite before handing
  off. Mention any check that could not be run; a session that cannot run
  commands must say its code is unverified.
- Give subagents the files and contract their slice needs, not the whole
  reading list.

## Live-Money Safety Rules

- Bot Spot (below) is the only live trader. Every other module (the legacy
  `bb main` runtime, the paper pool `bb pool`, the one-shot probe, the spot
  canary and allocator, the capital pool, margin, USD-M futures, universal
  transfer, the unified pool, withdrawal, prediction and the advanced-product
  registry) is disabled, paper, dry-run/read-only or a foreground-confirmed
  one-order command, and none may gain a live write path or a background
  service without a separate approved phase. Their full rules are in
  `docs/rules-other-modules.md`: read it before touching any of them.
- Market selection, conversion routing, minimum-order checks, fee estimates,
  and slippage must be derived from current Binance market data and rules, not
  from coin-specific constants.
- Phase 32 added a separate bot agentic Spot runtime (`bb bot-spot`,
  crate `bb-trader`). It is the only approved exception to the Phase 13/31
  live-paper warm-up gate and the foreground-only Spot allocator rule. It may
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
  X (except the Phase 43 collector below), store unnecessary raw personal data,
  or create an order-submission path. Social evidence is information for the bot
  only and must never bypass exchange rules, order-test, emergency-stop, or
  reconciliation.
- Phase 43 X pre-pump signal insighter: by operator decision (2026-10-13) the
  Camoufox collector may read public X posts and searches through browser
  automation from a dedicated burner account. It is read-only: it must not post,
  like, follow, repost, DM, or read private content; it keeps its pacing caps and
  privacy reduction; and it never holds Binance credentials or an order path.
  The operator owns the X Terms of Service and account-ban risk. Haiku's
  traction and exhaustion calls are information for deterministic entry/exit
  rules only. Nothing executes until the Phase 43 paper gate passes and a
  separate execution approval is given.
- Phase 36 Bot Spot Telegram decision explanations are observability-only.
  They must be derived from the completed cycle result/audit data, must not
  make a separate LLM call, and must not change live routing, prompt, order-test,
  emergency-stop, audit, or reconciliation behavior. Long explanations are
  delivered through safe message splitting.
- Binance does not safely expose the full API-key symbol whitelist. Live-capable
  features must use the local/operator-owned scope audit and fail closed when a
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

## Current Verification Commands

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
scripts/smoke.sh
scripts/pool_replay.sh
```
