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
  rows, no halt. (Buy reviewed 2026-11-01: 49.3 TRX at 0.303 on 10-31 23:20
  UTC, filled, booked to sleeve `fast`, no halt; the first sell is still to
  come.)
- [ ] Tag-rule tuning: plan with the operator how the tag rule should
  operate; agents propose and back proposals with backtests, the operator
  picks.
  - [x] Round 1 (operator 2026-11-01: all five families, judge on 10-26..11-01):
    `bb algo-backtest --candidates liquid`, 36 frozen rule files on the Pi's
    tags history, report `reports/phase54/tag_rule_tuning_2026-11-01.md`: no
    rule file passes the bar.
  - [x] Paper rule (operator 2026-11-01, "Tags + snap-back"):
    `config/rules/tagsnap-2026-11-01.json` with its test, practised plan-only
    by `bot-spot-paper` on the Pi.
  - [ ] Round 2 around 2026-11-08: the same frozen grid, judged only on data
    after 2026-11-01T02:00Z; the operator picks.
