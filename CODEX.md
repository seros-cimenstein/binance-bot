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

- The legacy strategy runtime must not run fixed-size orders on a real-money
  account. Keep `binance-bot.service` disabled unless a reviewed
  balance-aware live mode is deliberately approved.
- The modular asset-pool runtime in `src.pool` is paper-only. It may read
  balances, markets, and authenticated commission schedules, but it must not
  acquire a live order-submission path.
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
- Phase 32 adds a separate Claude autonomous Spot runtime in `src.claude_spot`.
  It is the only approved exception to the Phase 13/31 live-paper warm-up gate
  and the foreground-only Spot allocator rule. Claude mode may override scorer
  holds, paper warm-up, canary caps, allocation percentages, turnover limits,
  drawdown limits, and daily notional/order limits for Spot only. It may control
  the whole available Spot wallet when `CLAUDE_SPOT_ENABLED=true`,
  `CLAUDE_SPOT_DYNAMIC_SCOPE=all_eligible_spot`, `USE_TESTNET=false`,
  `DRY_RUN=false`, and `CONFIRM_LIVE_TRADING=yes` are all set. It may use
  aggressive small-market discovery for tiny-account Spot fishing and may let
  Claude accept upfront fee/slippage friction for a stated thesis. It still must
  enforce Binance market rules including lot-size step sizing, available
  balances, fee handling, Spot permission/order-test preflight, dynamic-scope
  configuration, local route scope, default fiat-asset route blocking,
  configured fixed trade cooldown/daily cap when enabled, hard friction cap,
  halt-after-failed-submission behavior, market evidence/break-even/trade-memory
  live gates, emergency stop, audit logging, reconciliation, and
  halt-on-ambiguous-leg behavior. The Phase 34 live profile may set cooldown
  and daily cap to `0` so Claude can think and potentially trade every
  5-minute cycle, but it must still receive dynamic fee/friction evidence and
  the hard evidence/failed-route gates must remain active. Claude mode must not
  call margin, futures, universal-transfer, withdrawal, or prediction write
  endpoints.
- Phase 35 social-signal intelligence is read-only evidence collection and
  context enrichment only. It may ingest Telegram Bot API updates visible to a
  configured bot and official X API search results, normalize/hash records, and
  feed per-asset evidence to Claude Spot. It must not bypass platform terms,
  scrape private chats, use browser automation for X, store unnecessary raw
  personal data, or create an order-submission path. Social evidence is
  additive and manipulation-risk blocks may remove candidate routes, but social
  hype must never bypass exchange, fee/friction, evidence, emergency-stop, or
  reconciliation gates.
- Phase 36 Claude Spot Telegram decision explanations are observability-only.
  They must be derived from the completed cycle result/audit data, must not make
  a second LLM call, and must not change live routing, prompt, risk, evidence,
  fee/friction, order-test, emergency-stop, audit, or reconciliation behavior.
- Phase 37 Claude Spot friction-cap tuning may widen the hard immediate
  fee/spread/slippage ceiling for small-market discovery, but it must remain a
  finite configured cap and must not bypass market evidence, trade memory,
  Binance market rules, order-test, local scope, fiat blocking, emergency-stop,
  audit, or reconciliation behavior.
- Phase 38 Claude Spot bps retuning may adjust both the candidate friction cap
  and the strict evidence minimum. Increasing the evidence minimum is a
  stricter live-submission gate, not a risk loosening; it must still be
  documented and tested as a deliberate live profile change.
- Phase 39 Claude Spot evidence-minimum lowering is a live-risk increase. It
  may make live Spot submissions more likely, but it must keep the finite
  candidate friction cap, Binance market rules, balance checks, order-test,
  local scope, fiat blocking, trade memory, emergency stop, audit, and
  reconciliation behavior active.
- Phase 40 Claude Spot route-execution hardening keeps live buy sizing dynamic:
  use authenticated per-symbol taker fees plus slippage and rounding reserve
  before order-test/submission, preserve submitted-leg audit truth after any
  partial route failure, sanitize signed Binance request details from logs, and
  allow full Telegram Why text through safe message splitting. Keep the timer
  stopped after a partial failed route until operator review explicitly
  re-enables it.
- Phase 41 Claude Spot speculative entry mode is an explicit live-risk profile:
  it may relax only the normal market-evidence edge minimum when evidence is
  ready, expected edge stays above the configured speculative floor, and Claude
  provides a concrete speculative thesis. It must not bypass Binance market
  rules, balances, dynamic fees/reserves, order-test, local scope, fiat
  blocking, trade memory, emergency stop, failed-route halt, audit, or
  reconciliation.
- Phase 42 Claude Spot short-window evidence adds `5m` and `15m` minute-candle
  returns to the candidate context and relative-momentum edge. Keep the normal
  live gates active, and do not let 24h/7d language override clearly adverse
  short-window tape without an explicit after-fee thesis.
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

## Current Verification Commands

```bash
.venv/bin/python -m pytest -q
scripts/pool_replay.sh
.venv/bin/python test_pipeline.py
scripts/smoke.sh
```
