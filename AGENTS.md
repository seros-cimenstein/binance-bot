# Agent Instructions

Before changing anything in this repository, read these files in order:

1. `CODEX.md`: workflow and the live-money safety rules. They are binding.
2. `IMPLEMENTATION_PLAN.md`: the roadmap. Active work (2026-10-21): Phase 50
   (long-term value voice) is deployed: the live Bot Spot trader runs prompt
   `bot-spot-v17-schizo` on the full wallet: the Phase 49 schizo trader (a
   mood that follows GCP2 among the four Phase 48 personas, their votes; YOLO
   removed) plus a Markov value voice fitted by `bb bot-spot-mdp-fit`
   (γ=0.95 per cycle, model `docker-data/bot-spot-mdp.json` on the Pi).
   Rollback tags: `binance-bot:pre-v17-20261021` for v16,
   `binance-bot:pre-v16-20261020` for v15, `binance-bot:pre-c4-20261020`
   for v14. Watch its live cycles. Phase 48 (schizoid
   strategy personas) stays backtest only; its report, now with the `schizo`
   row, is `reports/schizoid_backtest_2026-10-18.{md,csv}`. Phase 43 (X
   pre-pump signal insighter) has its collector image built and launch-tested
   on the Pi but is not collecting yet (no burner login there, service not
   started).
3. `PROGRESS.md`: the handoff state; the newest entries are at the top of
   "Current Status".

Update `PROGRESS.md` at the end of every meaningful session and tick finished
items in `IMPLEMENTATION_PLAN.md`, as `CODEX.md` describes.
