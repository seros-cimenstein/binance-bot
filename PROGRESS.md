# Binance Bot Progress

Current state only; rewrite it, do not append (`CODEX.md`, "Required
Workflow"). History: git log, `docs/decisions.md`, `docs/archive/`.

Updated 2026-11-02.

## Direction

The trader is a nonstop algorithm: Binance prices plus /biz/ tags from
biz-app, no LLM, no chat, no timer, no Telegram. Since 2026-11-02 07:06 UTC it
trades LIVE with the operator's tags + snap-back rule `tagsnap-2026-11-01`
(`docs/decisions.md`, "2026-11-02 tags + snap-back rule live"; plan-only step
waived). The snap-back rule traded live from 2026-10-31 20:01 to then (net
−0.94 USDT so far, see Evidence). Tuning round 2 runs around 2026-11-08
(`IMPLEMENTATION_PLAN.md` Phase 54).

## On the Pi (`ssh pi-wired`, `~/binance-bot`)

- Image `binance-bot:latest` = fc7137f6f500 (2026-11-02, tagging fixes:
  market names out of coin scores, `market.macro.tone`, crypto scope), run by
  `bot-spot` (since 17:00:20 UTC, operator "get the new one rolled in live
  buddy") and `scheduler`. `bot-spot-paper` eb660f6cc305, `cryptobot-gui`
  093b478c3eff. Rollback tags: `pre-tags2-20261102` = 1f6647c1b187 (live
  07:06..17:00), `pre-algo-20261102` = 093b478c3eff, `pre-algo-20261031` =
  1ce66703a04b, `pre-algo-20261030` = 428220df4d0f (the old LLM trader).
- LIVE: `bot-spot` (`bb bot-spot --submit-live --nonstop`, on
  `tagsnap-2026-11-01` since 2026-11-02 07:06:02 UTC), `.env.docker`
  `BOT_SPOT_ALGO_RULE=config/rules/tagsnap-2026-11-01.json`: sleeve `fast`
  share 1, score −1 `momentum.24h` + 0.5 `asset.direction.24h`, buy at +1.0,
  sell below -0.3; `core` share 0. Audit `docker-data/bot-spot-decisions.jsonl`,
  state `docker-data/bot-spot-agent-state.json`. Holds 27.1 RUNE in sleeve
  `fast` since 2026-11-01 21:22 UTC (operator: "Hold it"). First row on the
  new image 17:00:48: keep RUNE, score +0.753, 72 market signals, tags-2 feed
  fresh; 21 of 21 scores recomputed from the file, no market name in any coin
  score. Wallet about 16 USDT. No stop file.
- Plan-only: `bot-spot-paper` on the same rule (audit
  `docker-data/bot-spot-paper-decisions.jsonl`); it sizes from the real
  wallet, so while live holds the cash every buy is "below min notional".
- `cryptobot-gui` (dashboard, `ssh -L 8787:localhost:8787 pi-wired`) reads
  the live audit and state. `scheduler`: 23:45 UTC backup, 00:30 UTC
  `bb tag-scoreboard` into `docker-data/scoreboard/`.
- Never `docker compose up -d` without service names. The boot unit
  `binance-bot-compose.service` is disabled. `silly_swirles` (exited, not a
  compose service) is unrelated and left alone.
- Backups before the image switch: `.env.docker.bak-20261102T170001Z-tags2-live`,
  `docker-data/backup-20261102T170001Z-tags2-live/` (both audits, both states,
  compose file; older: 11-02 tagsnap-live, 10-31 snapback, 11-01 tagsnap).
  Back: `touch docker-data/bot-spot.stop`, or `docker tag
  binance-bot:pre-tags2-20261102 binance-bot:latest` and `docker compose up -d
  --no-deps bot-spot`.

## Feed pipeline (on the Pi)

`biz-watch` and `biz-feed` (systemd user units, enabled at boot) write
`docker-data/tags-feed.json`, the registry and the hourly history every 15
min; both traders take candidates from it and buy nothing from a feed older
than 45 minutes. Since 2026-11-02 09:31 UTC the feed is `tags-2` (every post
since 10-01 re-tagged: 81,596 posts, 680 calls, 0 failed) with `crypto` and
`risk_up`/`risk_down` numbers; history rebuilt (778 files). Backups:
`docker-data/feed.sqlite3.bak-20261102T091620Z-tags1`,
`tags-history-tags1-20261102T091620Z/`, `tags-registry.json.bak-…-tags1`,
`~/biz-app-backup-20261102T091620Z-tags2.tgz`. Details and stop:
OPERATIONS.md, "Tags feed (biz-app)".

## Evidence so far

- Snap-back live (10-31 20:01 to 11-02 07:06 UTC): TRX +0.49 USDT (closed);
  27.1 RUNE bought 11-01 21:22 at 0.568 after a 10%+ 24h drop, 0.515 at
  11-02 06:57; net −0.94 USDT (−6.3%). All orders filled, no halt.
- Tag-rule tuning round 1 (`reports/phase54/tag_rule_tuning_2026-11-01.md`):
  none of 36 frozen rule files passes the bar; the tag pick helps snap-back
  (+8.18% against −42.92% with the liquid 20); judge window 10-26..11-01:
  the tag rule +15.73% from one HYPE hold.
- Fear/greed signal `market.mood.tone` (2026-11-01,
  `reports/phase54/fear_greed_2026-11-01.md`): no snap-back variant passes.
- Tagging fixes (2026-11-02, `reports/phase54/tagger_audit_2026-11-02.md`,
  agent labels): coin direction agreement 77% → 82%; moods 16:2 bearish →
  14:6 (labels 9:12). Step-aside grid (`step_aside_2026-11-02.md`): none of 8
  passes. Scoreboard 11-02: 60 tone rows, |t| ≤ 1.03.
- tags-1 against tags-2 (`reports/phase54/tags2_ab_2026-11-02.md`, 55 rules,
  880 runs): no rule passes with either. The live rule +8.50% → +1.81% on the
  tuning window (one first pick: QNT vs SOL); plain snap-back +8.18% under
  both and 3 points ahead in the judge window: the AI direction term adds
  nothing yet.

## Next steps

1. Watch the live tagsnap rule: RUNE sells when it scores below -0.3 (24h
   move above +3%, less half the board lean); no stop-loss or maximum hold.
2. Tuning round 2 around 2026-11-08: rerun the frozen grids
   (`reports/phase54/tag_rule_tuning_2026-11-01_grid.json`,
   `fear_greed_2026-11-01_grid.json`, `step_aside_2026-11-02_grid.json`) on
   the tags-2 history plus the attention test, judged only on data after
   2026-11-01T02:00Z, with the paper trader's own record; the operator picks
   (Phase 54 box 3).
