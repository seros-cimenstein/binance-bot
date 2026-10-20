# Agent Instructions

Before changing anything in this repository, read these files in order:

1. `CODEX.md`: workflow and the live-money safety rules. They are binding.
2. `IMPLEMENTATION_PLAN.md`: the roadmap. Active work (2026-10-20): Phase 48
   (schizoid strategy personas) is backtest only. The offline 60-day report
   `reports/schizoid_backtest_2026-10-18.{md,csv}` is delivered and awaiting
   operator review; nothing is deployed and the live trader is unchanged.
   Phase 47 (market breadth, liquid universe, swing prompt v13/v14) stays
   deployed and monitored on the live Bot Spot trader. Phase 43 (X pre-pump
   signal insighter) has its collector image built and launch-tested on the
   Pi but is not collecting yet (no burner login there, service not started).
3. `PROGRESS.md`: the handoff state; the newest entries are at the top of
   "Current Status".

Update `PROGRESS.md` at the end of every meaningful session and tick finished
items in `IMPLEMENTATION_PLAN.md`, as `CODEX.md` describes.
