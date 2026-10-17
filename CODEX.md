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
- Phase 32 added a separate Claude agentic Spot runtime (`bb claude-spot`,
  crate `bb-trader`). It is the only approved exception to the Phase 13/31
  live-paper warm-up gate and the foreground-only Spot allocator rule. It may
  control the whole available Spot wallet when `CLAUDE_SPOT_ENABLED=true`,
  `CLAUDE_SPOT_DYNAMIC_SCOPE=all_eligible_spot`, `USE_TESTNET=false`,
  `DRY_RUN=false`, and `CONFIRM_LIVE_TRADING=yes` are all set, and it runs as a
  long-lived daemon (`bb claude-spot --submit-live --daemon`, systemd
  `binance-claude-spot.service`, compose service `claude-spot`). Claude runs
  through omp in RPC mode with one persistent session (resumed across restarts,
  compacted, backed by a durable journal) and acts only through the bot's host
  tools; `execute_route` is the only order path. Claude Spot must not call
  margin, futures, universal-transfer, withdrawal, or prediction write
  endpoints.
- Claude Spot model fallback (operator decisions 2026-10-13, 2026-10-15 and
  2026-10-16): the primary model (`CLAUDE_SPOT_MODEL`) is
  `openai-codex/gpt-6.1-sol` at thinking `high` since 2026-10-16 (before
  that `anthropic/claude-sonnet-5-5` from 2026-10-14 and
  `anthropic/claude-haiku-5-5`), on a 20-minute deployed cycle since
  2026-10-15 so it fits the subscription allowance. The brain chain is
  gpt-6.1-sol (OpenAI Codex) -> `google-antigravity/gemini-4-argon-high` at
  `high` (`CLAUDE_SPOT_FALLBACK_MODELS` and `retry.fallbackChains` in the Pi's
  `omp-agent/config.yml`); each provider uses its own omp login on the Pi.
  When the active model is rate- or usage-limited (or times out, errors or
  fails a compaction), the turn moves to the next model in the chain and
  returns to the primary when its cooldown ends. A fallback model is the
  trader for that turn: the same host tools, the same `execute_route` path
  and every hard-safety item below apply unchanged. Each login is made on
  the Pi with `omp login <provider>`, never a copied token. Adding another
  model or provider needs a new operator decision.
- Claude Spot has no trading guardrails, by operator decision. The edge /
  market-evidence minimum, friction cap, speculative mode, loss-streak limit,
  trade cooldown, daily trade cap, proven-edge gate, rules-engine decision mode,
  and the LLM `go_off` decision flag are removed; their env vars are ignored
  with one warning. Fees, spread, slippage, short-window (`5m`/`15m`) and
  longer momentum, market evidence, social data, and trade memory are
  information in tool output for Claude to weigh, never blocking gates. Do not
  reintroduce any of them as a blocking gate without explicit operator
  approval. `CLAUDE_SPOT_MAX_DAILY_LLM_COST_USD` is LLM cost control, not a
  trading gate. By operator decision (2026-10-14) the trader has no stop-loss
  or take-profit levels: the prompt must not ask Claude for invalidation
  prices, exit triggers or profit targets, and no automatic price-level exit
  may be added to Claude Spot without explicit operator approval.
- Phase 45 Claude-chosen exit plans (operator approval 2026-10-14): Claude may
  pick a sell plan per position from a fixed, deterministic menu (follow the
  peak by percent or ATR, take part, stretch above the Bollinger band,
  slowdown, momentum fade, deadline) by setting its parameters; Claude
  never writes or runs code. Approved: the historical replay
  (`bb claude-spot-exit-replay`) and, after the operator reviewed it, the
  paper trial: the optional `set_exit_plan` host tool and the
  `claude-spot-exit-watch` watcher, which records would-be sells and must
  never place an order or hold an order path. The prompt may offer the tool
  but must not require exit plans.
- Phase 45 live exit plans (operator decision 2026-10-15, before a paper
  run, after the 40-trade backtest): with `CLAUDE_SPOT_EXIT_PLANS_LIVE=true`
  the `claude-spot` daemon follows positions between cycles and sells a
  position into USDT when its plan (Claude's from `set_exit_plan`, or the
  default `CLAUDE_SPOT_DEFAULT_EXIT_PLAN`, deployed `atr=3:1h,deadline=120`
  since 2026-10-17 (was `stretch=2:1,atr=3:1h`),
  for positions opened from `CLAUDE_SPOT_DEFAULT_EXIT_PLAN_FROM`) fires.
  These sells use `Runtime::exit_sell`: the cycle lock (never during a
  Claude route), live arming, the emergency stop file, the failed-route
  halt, operator scope, `order/test` per leg, exchange filters, fill
  reconciliation, and one durable audit row (`cycle_id` `exit-plan`) before
  the alert; an ambiguous sell halts live trading like any route. Only the
  trader writes the watch files then; the paper watcher refuses to start.
  Exit plans only sell, never buy. Changing the default plan or the menu
  needs a new operator decision. Operator decision 2026-10-17 (branch
  `old-gold-way`): back to the v7 system after the fixed plan (2026-10-16)
  and locked rules (2026-10-17) cut winners short. Claude's `set_exit_plan`
  (full menu, deadline included; setting again replaces, `clear` removes)
  overrides the default per position, and the system prompt is the v7 text
  (tag `claude-spot-v11-agentic`). Kept from 2026-10-17: `stretch` sells
  only when its line is above break-even (entry plus buy and sell fees at
  the trip's fee rate), and the cycle prompt and trade history show Claude
  which rule sold each position and the result after fees (information
  only). The trader session was restored to the v7 conversation up to the
  last gpt-6.1-sol cycle before Sonnet took over (cycle 542, 2026-10-16
  05:02 UTC), with the journal cut at the same point.
- Claude Spot hard safety stays always on and fails closed: the live arming
  flags, the emergency stop file checked at cycle start and before every route,
  leg, and order, the exclusive run lock, account READING plus canTrade,
  Binance `order/test` before every leg, exchange filters with orders sized
  from live balances net of authenticated per-symbol taker fees plus slippage
  and rounding reserve, per-leg fill reconciliation that halts on anything
  ambiguous, operator scope (allow/block lists, blocked quotes, default fiat
  blocking), Spot MARKET orders only with at most `CLAUDE_SPOT_MAX_ROUTE_LEGS`
  legs, and the failed-route halt. A route that submitted an order and then
  failed, or whose submission was ambiguous, halts live trading until the
  operator reviews it and runs `bb claude-spot --mark-failed-route-reviewed`;
  nothing else may clear it. Never weaken or bypass any hard-safety item.
- Claude Spot audit is one fsync'd JSONL row per route or cycle, written before
  any Telegram alert, preserving submitted-leg truth after any partial route
  failure. Signed Binance request details must stay out of logs.
- Phase 35 social-signal intelligence is read-only evidence collection and
  context enrichment only. It may ingest Telegram Bot API updates visible to a
  configured bot and official X API search results, normalize/hash records, and
  feed per-asset evidence to Claude Spot through `get_social_evidence`. It must
  not bypass platform terms, scrape private chats, use browser automation for
  X (except the Phase 43 collector below), store unnecessary raw personal data,
  or create an order-submission path. Social evidence is information for Claude
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
- Phase 36 Claude Spot Telegram decision explanations are observability-only.
  They must be derived from the completed cycle result/audit data, must not
  make a separate LLM call, and must not change live routing, prompt, order-test,
  emergency-stop, audit, or reconciliation behavior. Long explanations are
  delivered through safe message splitting.
- The capital pool (`bb capital-pool`, crate `bb-capital`) keeps a
  deterministic reserve and per-module min/max plan. The Claude allocator
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
  localhost-only. It may read bot files and query Binance through signed GET
  endpoints, but it must not write files, toggle the emergency stop, clear the
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
