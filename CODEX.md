# Codex Working Notes

## Session Pre-Prompt

At the start of every new Codex session in this repository, read these files
before making changes:

1. `CODEX.md`
2. `IMPLEMENTATION_PLAN.md`
3. `PROGRESS.md`

Treat `IMPLEMENTATION_PLAN.md` as the durable roadmap and historical scope.
Treat `PROGRESS.md` as the active cross-session handoff checklist. Update
`PROGRESS.md` at the end of every meaningful session with current status,
commands run, remaining tasks, and blockers. Only update
`IMPLEMENTATION_PLAN.md` when roadmap scope changes or a roadmap checkbox/status
needs correction.

Future Codex sessions should treat this repository as a roadmap-and-progress
driven project.

## Required Workflow

- Keep `IMPLEMENTATION_PLAN.md` as the source of truth for planned roadmap work.
- Keep `PROGRESS.md` as the source of truth for the current handoff state.
- Before starting a meaningful roadmap change, make sure the relevant plan
  section has checkbox items.
- As roadmap work completes, tick each finished item in `IMPLEMENTATION_PLAN.md`.
- As session work completes, update `PROGRESS.md` with the current state.
- Add a dated progress note when a phase or major slice is completed.
- Do not claim a plan item is complete unless tests or a concrete code path back
  up the claim.
- Prefer small implementation batches that end with verification.
- Run the relevant targeted tests first, then the full suite before handing off.
- Mention any tests that could not be run.

## Live-Money Safety Rules

- The legacy strategy runtime (`bb main`) must not run fixed-size orders on a
  real-money account. Keep the `binance-bot.service` user unit and the
  `main-bot` compose service disabled unless a reviewed balance-aware live mode
  is deliberately approved.
- The modular asset-pool runtime (`bb pool`, crate `bb-pool`) is paper-only. It
  may read balances, markets, and authenticated commission schedules, but it
  must not acquire a live order-submission path.
- Paper decision reports must open the paper ledger read-only. Historical-candle
  research replay must write only to an isolated replay database, use no live
  order path, and must not replace the Phase 13 live-market warm-up gate.
- Market selection, conversion routing, minimum-order checks, fee estimates,
  and slippage must be derived from current Binance market data and rules, not
  from coin-specific constants.
- Do not plan or enable continuous real pool reallocation until the replay
  suite, live-market paper warm-up, permission diagnostics, and tiny capped
  live canary results have been reviewed and explicitly approved as a separate
  live-execution phase.
- A manual one-shot live probe may exist only as an explicitly confirmed
  foreground command. It must not be wired to systemd or to the paper pool
  allocator, and it must submit at most one direct-USDT order per invocation.
- A spot live canary may exist only as an explicitly confirmed foreground
  command. The first canary version is limited to one direct sell into `USDT`
  per invocation, hard daily/order caps, emergency stop checks, safe Binance
  order-test preflight, audit logging, and Telegram alerting.
- The guarded live spot allocator may exist only as an explicitly confirmed
  foreground command. The first allocator version is limited to one direct spot
  market order per invocation, with permission diagnostics, safe order-test
  preflight, risk limits, reconciliation, audit logging, and Telegram alerting.
  It must not be installed as a background service without a separate reviewed
  phase.
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
- Phase 49 schizo trader (operator decisions 2026-10-20, `LOOP_SCHIZO_LIVE.md`).
  The operator asked for a "schizo" Bot Spot: "it is not schizo enough", "ai
  needs to be schizoified or become schizo thats it", "on top of those it looks
  at gcp2 data for reference …", and "Backtest, then trade live as schizo", with
  GCP2 as "Mood ring" and "YOLO button".
  - The LLM trader stays the trader. It becomes schizo through its prompt and
    a per-cycle state: an active mood (one of the Phase 48 personas), dice,
    the personas' votes, and a YOLO directive. Its behaviour is not replaced by
    rules, and it still acts only through the host tools; `execute_route`
    stays the only order path.
  - GCP2 (gcp2.net, Global Consciousness Project 2.0) public data is read-only
    information. It may be read without login (public colour zones,
    per-minute aggregate, monthly history) and must never be posted to or
    logged into. Mood switches are bound to GCP2 only ("nothing hardcoded …
    we are going to be gcp bound in mood switches"): no timer, no fixed
    switch probability, no fixed seed. The zone bounds come from GCP2's own
    `getcolorzones`, and the dice come from GCP2 readings.
  - The mood runs inverted to GCP2 ("mood is inverted not yolo"): the bot
    switches mood when the GCP2 zone stays the same between cycles and keeps
    it when the zone changes. YOLO is not inverted: while the zone is Very High
    or Extreme the bot is told to sell other holdings and go all-in on the
    hottest liquid coin, until the zone drops below High. A failed GCP2 read
    means no mood switch and no YOLO for that cycle.
  - There is no panic sell ("no panic sell"). The 2026-10-14 no-stop-loss rule
    above stays in force, and exit plans keep the existing menu and default.
  - Go-live is not gated on the backtest ("go live no tiny shit whatever we
    said earlier will be live"): the schizo trader runs on the full wallet with
    no size cap.
  - Every Bot Spot hard-safety item below stays unchanged.
  - Tag `bot-spot-v15-schizo` (`BOT_SPOT_GCP2_ENABLED=true`, the default) is
    this trader: the system prompt replaces the v13 regime/swing bullets with
    the four personalities, the inverted GCP2 mood ring, the voices (votes), the
    one-coin all-cash bet and the YOLO order, and the cycle message opens with
    `## Your schizo state`. Its no-stop-loss, exit-plan, hard-safety and tool
    text is the v14 text. `BOT_SPOT_GCP2_ENABLED=false` gives the v14 prompts.
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
- The capital pool (`bb capital-pool`, crate `bb-capital`) keeps a
  deterministic reserve and per-module min/max plan. The bot allocator
  (`--advise`) may only clamp within those deterministic bounds, and any
  allocator failure must fall back to the deterministic plan. Auto-convert
  (`AUTO_CONVERT_TO_QUOTE=true`) may only market-sell `AUTO_CONVERT_ASSETS` to
  cover a quote shortfall after a Binance `order/test`, and must never sell the
  target symbol's base asset.
- The margin module is dry-run/read-only only. It may query margin account,
  max-borrow, and interest data for local modeling, but it must not call margin
  borrow, repay, order, or transfer write endpoints. Live margin permissions
  must not be enabled until a separate activation phase adds controls, tests,
  and explicit approval gates.
- The USD-M futures module is dry-run/read-only only. It may query futures
  account, position, exchange-rule, funding, symbol-config, and
  leverage-bracket data for local modeling, but it must not call futures order,
  leverage-change, margin-mode, position-mode, or position-margin write
  endpoints. Live futures permissions must not be enabled until a separate
  activation phase adds controls, tests, and explicit approval gates.
- The universal transfer module is dry-run/read-only only. It may query wallet,
  spot, funding, margin, futures, and transfer-history data for local transfer
  planning and audit, but it must not call the universal-transfer write
  endpoint. Live transfer permissions must not be enabled until a separate
  activation phase adds controls, tests, and explicit approval gates.
- The unified asset-pool module is dry-run/local valuation only. It may combine
  read-only spot, funding, margin, futures, and transfer-availability data into
  benchmark valuation and allocation-constraint reports, but it must not call
  any live order, margin, futures, or transfer write endpoint.
- Withdrawal and prediction dry-run modules may model local safety/risk state
  only. They must not call withdrawal, prediction order, stake, claim, or
  settlement write endpoints; live activation remains blocked by policy.
- The advanced product-module registry is metadata/gating only. It may describe
  strategy interfaces, permissions, market-data sources, risk models, audit
  requirements, and activation gates, but it must not enable a live write path.
- Every new Binance product capability must start as read-only/dry-run modeling
  or as an explicitly confirmed foreground-only canary. API permission alone
  must never activate live behavior.
- Code for independent advanced-product modules may be developed in parallel,
  but live canary execution that mutates account state must be serial so
  balances, debt, exposure, fees, and reconciliation remain attributable to one
  action at a time.
- Transfer, margin, and futures live canaries must remain separate foreground
  commands until each passes its own audit, reconciliation, emergency-stop,
  Telegram-alert, and explicit approval gates.
- The current margin, USD-M futures, and universal-transfer canary modules are
  plan-only readiness commands. They may run read-only discovery, local scope
  checks, risk modeling, and JSONL audit output, but their submission flags
  must fail closed until a later reviewed live-execution approval.
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
