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
- [x] The first live snap-back orders reviewed: fills, sleeve booking, audit
  rows, no halt. (Reviewed 2026-11-02: buy 49.3 TRX at 0.303 on 10-31 23:20,
  sell 49.3 TRX at 0.313 and buy 27.1 RUNE at 0.568 on 11-01 21:22 UTC; all
  filled, the fast sleeve booked TRX, closed it and booked RUNE; no halt.)
- [x] Tags + snap-back live (operator 2026-11-02, "Switch to tagsnap now",
  plan-only step waived): `.env.docker` on `tagsnap-2026-11-01`, rollback tag
  `binance-bot:pre-algo-20261102`, `bot-spot` recreated on image 1f6647c1b187
  at 07:06 UTC; first row keeps RUNE (score +1.0), 21 of 21 listed scores
  recomputed from the file, no halt.
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
  - [x] Fear/greed signal (operator 2026-11-01, "implement these"):
    `market.mood.tone.<window>` in `crate::signals` with its test, measured by
    the nightly scoreboard; retest `reports/phase54/fear_greed_2026-11-01.md`:
    no variant passes the bar.
  - [x] Tagging fixes (operator 2026-11-02, "lets address these issues"; see
    `docs/decisions.md`): blind 200-post audit
    (`reports/phase54/tagger_audit_2026-11-02.md`); prompt `tags-2` and a
    re-tag since 2026-10-01 (81,596 posts, 0 failed); market names out of coin
    scores; `crypto` scope and `market.macro.tone`; the Pi's feed and tags
    history rebuilt, `scheduler` on fc7137f6f500; step-aside grid
    (`reports/phase54/step_aside_2026-11-02.md`): no variant passes.
  - [ ] Round 2 around 2026-11-08: the round-1 grid, the fear/greed grid and an
    attention test (talk surge as a filter for snap-back candidates; operator
    2026-11-01 "keep testing"), judged only on data after 2026-11-01T02:00Z;
    the operator picks.
