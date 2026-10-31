# Agent Rules

Binding for every agent, session, loop worker and subagent (Claude, Codex,
omp). `AGENTS.md` says which file answers which question; read others only
when the task needs them.

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
  move its section to a file in `docs/archive/`.
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
- A `LOOP_*.md` ledger at the repo root is an active loop: its Protocol
  applies on top of this file. When the loop is done, move the ledger and
  its `.manager.md` to `docs/archive/loops/`.
- Never commit, stash, reset or check out over uncommitted changes you did
  not make; leave them and say so.
- The Pi (`ssh pi-wired`, `~/binance-bot`) is the live host. Read-only
  checks are fine. Rebuild or redeploy only to ship a feature or fix, never
  just to make the image match the tree.

## Live-Money Safety Rules

- The algorithm trader (`bb bot-spot`, crate `bb-trader`) is the only trader
  in this repo. Modules moved to `../redundancy/binance-bot-modules` on
  2026-10-29 stay out; bringing one back needs an operator decision and its
  own approved phase.
- The trader is an algorithm that runs continuously. No fixed cycle or timer,
  no LLM, no AI model, no omp session and no chat anywhere in the trading
  path. Its buy, sell and sizing rules are decided by the operator in person:
  agents may propose rules and back them with backtests, but never pick or
  change them, and a loop manager may not answer for the operator on them.
  Do not add trading limits (caps, cooldowns, stop-loss or take-profit
  levels) unless the operator chooses them as part of the rules.
- The rules in force live (operator, 2026-10-31, `docs/decisions.md`
  "2026-10-31 snap-back rule live"), in the rule file
  `config/rules/snapback-2026-10-31.json` (`BOT_SPOT_ALGO_RULE`, style
  `fixed`): sleeve `fast`, share 1: score = −1 `momentum.24h` (24h move capped
  at ±10%, divided by 10%); buy the best-scoring candidate for the sleeve's
  share of the portfolio (so all USDT) when its score is +0.8 or more, one
  coin at a time; sell exactly the sleeve's quantity into USDT when it scores
  below -0.3. Sleeve `core`: share 0, no weights, never active while it holds
  nothing. Candidates: the coins the tags feed's live asset tags reach with an
  eligible USDT market, directly or through the operator's proxy file
  (`BOT_SPOT_PROXIES`, gold through PAXG and XAUT). Without a fresh tags feed
  nothing is bought. Rule set name `snapback-2026-10-31`. The tag rule
  `algo-2026-10-29` (`config/rules/algo-2026-10-29.json`: 0 `talk.6h` + 1
  `stance.6h` + 1 `momentum.1h`, buy +0.5, sell below -0.5, stances need 2
  posters) runs plan-only in `bot-spot-paper` for its tuning. The edge style
  (weights from the tag scoreboard), horizon filters, risk-off and other
  sleeve splits exist but are not in force; choosing them is the operator's
  (the tuning phase).
- AI only reads supplied content. biz-app
  (`~/projects/automation/imageboards/biz-app`, outside this repo; its live
  copy runs on the Pi in `~/biz-app`) reads /biz/ threads and writes the
  `tags-feed-v1` tags file and the tag registry; the trader reads that file
  as data, next to Binance data. A missing, unreadable or stale feed is
  recorded as unavailable, never guessed.
- No Telegram or other phone alerts from any process. Watching is the
  dashboard and the audit log.
- `bb-pool` (market catalog, fees, route engine) has no order path. Market
  selection, routing, minimum-order checks, fee estimates and slippage come
  from current Binance market data and rules, never from coin-specific
  constants.
- Live arming: the trader may use the whole available Spot wallet only when
  `BOT_SPOT_ENABLED=true`, `BOT_SPOT_DYNAMIC_SCOPE=all_eligible_spot`,
  `USE_TESTNET=false`, `DRY_RUN=false` and `CONFIRM_LIVE_TRADING=yes` are all
  set. It must not call margin, futures, universal-transfer, withdrawal or
  prediction write endpoints.
- Hard safety stays always on and fails closed: the live arming flags; the
  emergency stop file, checked before every route, leg and order; the
  exclusive run lock; account READING plus canTrade; Binance `order/test`
  before every leg; exchange filters with orders sized from live balances net
  of authenticated per-symbol taker fees plus slippage and rounding reserve;
  per-leg fill reconciliation that halts on anything ambiguous; operator
  scope (allow/block lists, blocked quotes, default fiat blocking); Spot
  MARKET orders only, at most `BOT_SPOT_MAX_ROUTE_LEGS` legs; the
  failed-route halt; and the `sleeve_mismatch` halt. A route that submitted an
  order and then failed, or whose submission was ambiguous, halts live trading
  until the operator reviews it and runs `bb bot-spot
  --mark-failed-route-reviewed`. Sleeve holdings in the state file that cannot
  be read, or that exceed the free balance by more than one step of the
  asset's market, halt the trader (plan-only and live: no decisions, no
  routes) until `bb bot-spot --mark-sleeves-reviewed`, which re-reads the
  balances and is refused unless the sleeves reconcile. Nothing else may clear
  either halt. Never weaken or bypass any hard-safety item.
- Audit: one fsync'd JSONL row per decision and per route, written before
  anything else reports it, preserving submitted-leg truth after any partial
  route failure. Signed Binance request details stay out of logs.
- Rollout: a new rule set runs plan-only (decides and records orders, places
  none) before it trades live; only the operator may waive that step, per rule
  set (waived once: `snapback-2026-10-31`, 2026-10-31). Going live needs the
  operator's go, a rollback image tag (`binance-bot:pre-algo-<date>`) and an
  entry in `PROGRESS.md`.
- Binance does not safely expose the API-key symbol whitelist. Live-capable
  features use the operator-owned scope and fail closed when a symbol or
  asset is outside `API_SYMBOL_WHITELIST` or `API_ASSET_WHITELIST`.
- Withdrawals stay hard-blocked unless a withdrawal-specific safety phase is
  explicitly approved.
- The `cryptobot-gui` dashboard is read-only and localhost-only. It may read
  the trader's files and query Binance through signed and public GET
  endpoints, but must not write files, toggle the emergency stop, clear the
  failed-route or sleeve halt, or call any order, transfer, withdrawal, margin or
  futures write endpoint. It keeps the loopback `Host` check and is never
  published on a non-loopback interface; adding controls needs its own
  reviewed phase with authentication.

## Current Verification Commands

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
scripts/smoke.sh
```
