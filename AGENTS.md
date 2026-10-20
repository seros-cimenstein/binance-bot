# Agent Instructions

Before changing anything in this repository, read these files in order:

1. `CODEX.md`: workflow and the live-money safety rules. They are binding.
2. `IMPLEMENTATION_PLAN.md`: the roadmap. Active work (2026-10-20): Phase 49
   (schizo live trader) is deployed: the live Bot Spot trader runs prompt
   `bot-spot-v15-schizo` on the full wallet, with a GCP2-driven inverted mood
   among the four Phase 48 personas, their votes, and YOLO in GCP2's two
   highest zones (`LOOP_SCHIZO_LIVE.md`; rollback tag
   `binance-bot:pre-c4-20261020`). Watch its live cycles. Phase 48 (schizoid
   strategy personas) stays backtest only; its report, now with the `schizo`
   row, is `reports/schizoid_backtest_2026-10-18.{md,csv}`. Phase 43 (X
   pre-pump signal insighter) has its collector image built and launch-tested
   on the Pi but is not collecting yet (no burner login there, service not
   started).
3. `PROGRESS.md`: the handoff state; the newest entries are at the top of
   "Current Status".

Update `PROGRESS.md` at the end of every meaningful session and tick finished
items in `IMPLEMENTATION_PLAN.md`, as `CODEX.md` describes.
