# Binance Bot Implementation Plan

Active phases only. Finished and superseded phases are in `docs/archive/`;
read a section there only when the task touches it.

Rules for this file (`CODEX.md`, "Required Workflow"): every phase has
checkbox items; tick a box in the same commit as the work that finishes it;
when a phase is complete, move its whole section to `docs/archive/`.

## 54. Snap-back Live, Tag-Rule Tuning (operator decisions 2026-10-31)

Why: the operator, 2026-10-31: "get the snapback live first then we will look
into how to make it tuned" (`docs/decisions.md`, "2026-10-31 snap-back rule
live"). Phase 52 (the nonstop algorithm trader) is archived in
`docs/archive/implementation-plan-phase-52.md`.

- [x] Snap-back live: rule file `config/rules/snapback-2026-10-31.json` with
  its test, rollback tag `binance-bot:pre-algo-20261031`, `bot-spot-paper` on
  the tag rule through its own `BOT_SPOT_ALGO_RULE`, the dashboard on the live
  files, `bot-spot` started live, its first decisions checked against the
  rule file, `PROGRESS.md` entry (plan-only step waived by the operator).
- [ ] The first live snap-back orders reviewed: fills, sleeve booking, audit
  rows, no halt.
- [ ] Tag-rule tuning: plan with the operator how the tag rule
  (`algo-2026-10-29`, plan-only in `bot-spot-paper`) should operate; agents
  propose and back proposals with backtests, the operator picks.
