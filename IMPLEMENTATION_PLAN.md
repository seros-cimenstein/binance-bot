# Binance Bot Implementation Plan

Active phases only. Finished phases 1-50, the old dated progress list and the
post-implementation handoff are in
`docs/archive/implementation-plan-phases-1-50.md`; read a section there only
when the task touches that phase.

Rules for this file (`CODEX.md`, "Required Workflow"): every phase has
checkbox items; tick a box in the same commit as the work that finishes it;
when a phase is complete, move its whole section to the archive file.

## 51. /biz/ Feed And Rule-Based Decider (operator request 2026-10-28)

Why: the operator wants buys and sells taken by an algorithm instead of an
LLM, fed by Binance data and by /biz/ (4chan) readings from biz-app
(`~/projects/automation/imageboards/biz-app`), whose AI (Gemini 3.8 Flash
through omp) already tags notable posts' coins bullish, bearish or neutral.
The operator chose to study the /biz/ insight first and design the algorithm
from what the study finds. A rule-based decision mode was removed by an
earlier operator decision (`CODEX.md`), so going live needs a new recorded
decision.

Feed contract `biz-feed-v1` (one JSON file; the bot is the reader):
`schema`, `as_of` (only posts the bot could have seen by then), `model`,
`prompt_version`, `coins.<BASE>.<1h|6h|24h>` with `mentions` (every post, no
AI), `posters` (distinct), `bullish` / `bearish` (AI tags weighted by the
1-3 post score) and `posts` (post numbers).

- [ ] Level 1, biz-app: `biz-insights feed [--at <time>]` writes
  `out/biz-feed.json` (mention counter over every post, name merge to Binance
  symbols, visibility delay). Not built: outside this repository.
- [ ] Level 2, biz-app: `biz-insights feed --history --step 60` writes one
  feed per hour into `out/biz-feed-history/`.
- [x] Level 2, bot: `bb biz-lead-study` (`bb-strategies::biz_lead_study`)
  measures the move after each feed against the same coin's random hours,
  older and newer half; `reports/biz_lead_study_<date>.{md,csv}`.
- [ ] Run the study on the real feed history and record the verdict.
- [x] Level 3, code: `scripts/biz_feed_to_pi.sh` copies the feed to the Pi;
  `crate::biz` reads it every cycle (`BOT_SPOT_BIZ_FEED_PATH`,
  `BOT_SPOT_BIZ_MAX_AGE_MINUTES`) into the cycle summary (audit field `biz`),
  information only; the dashboard's /biz/ tab shows the newest one.
- [ ] Level 3, deploy: rebuild the image on the Pi, start the copier, check
  the first `biz` fields.
- [x] Level 4, code: `crate::decider` (score = weighted /biz/ talk, stance,
  1h momentum, long-term value; sell below / buy at levels) and persona
  `biz_algo` with `--biz-feeds` (point-in-time `BizHistory`), the
  older/newer split, a 72/144-set weight grid tuned on the older half or
  `--biz-weights`, and the `biz_algo` rule section and trace.
- [ ] Level 4, run: on the real feed history (needs biz-app's `--history`)
  with weights from the lead study or the grid; record the `@newer` verdict.
- [x] Level 5, code: `BOT_SPOT_DECIDER=llm|algo` (`BOT_SPOT_ALGO_WEIGHTS`,
  `BOT_SPOT_ALGO_MIN_POSTERS`); algo cycles skip omp and send the decider's
  orders through `execute_route`; audit `decider` on every row and `algo` on
  algo cycle summaries; Telegram `Decider:` line; Bot tab details; compose
  service `bot-spot-algo-paper` (profile `algo-paper`, plan-only); operator
  decision recorded in `CODEX.md`.
- [ ] Level 5, practice: run `bot-spot-algo-paper` on the Pi and review its
  planned routes.
- [ ] Level 5, live: operator go, rollback tag `binance-bot:pre-algo-<date>`,
  `BOT_SPOT_DECIDER=algo` for `bot-spot`, watch the first cycles.

