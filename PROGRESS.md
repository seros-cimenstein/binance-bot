# Binance Bot Progress

Current state only; rewrite it, do not append (`CODEX.md`, "Required
Workflow"). History: git log, `docs/decisions.md`, `docs/archive/`.

Updated 2026-11-01.

## Direction

The trader is a nonstop algorithm: Binance prices plus /biz/ tags from
biz-app, no LLM, no chat, no timer, no Telegram. Since 2026-10-31 20:01 UTC it
trades LIVE with the operator's snap-back rule (`docs/decisions.md`,
"2026-10-31 snap-back rule live"; plan-only step waived by the operator).
Tuning round 1 for the tag rule (2026-11-01) found no rule file that passes
the bar; the paper trader now practises the operator's pick, the tags +
snap-back rule `tagsnap-2026-11-01`. Round 2 runs around 2026-11-08
(`IMPLEMENTATION_PLAN.md` Phase 54).

## On the Pi (`ssh pi-wired`, `~/binance-bot`)

- Image `binance-bot:latest` = eb660f6cc305 (2026-11-01, adds the paper rule
  file and `algo-backtest --candidates`), run by `bot-spot-paper` only.
  `bot-spot` and `cryptobot-gui` run 093b478c3eff (tagged
  `binance-bot:pre-tagsnap-20261101`); recreating them picks up `latest`,
  which holds the same trader code. Rollback tags: `pre-algo-20261031` =
  1ce66703a04b (Phase 53, live stopped), `pre-tags-20261031` = e9efbd3c98dc
  (Phase 52), `pre-algo-20261030` = 428220df4d0f (the old LLM trader).
- LIVE: `bot-spot` (`bb bot-spot --submit-live --nonstop`, since 2026-10-31
  20:01:45 UTC), `.env.docker` `BOT_SPOT_ALGO_RULE=config/rules/snapback-2026-10-31.json`:
  sleeve `fast` share 1, score −1 `momentum.24h`, buy at +0.8 (fell 8%+ in
  24h), sell below -0.3; `core` share 0. Audit `docker-data/bot-spot-decisions.jsonl`,
  state `docker-data/bot-spot-agent-state.json`. Holds 49.3 TRX in sleeve
  `fast` since 2026-10-31 23:20 UTC (see Evidence); about 0.11 USDT free plus
  dust. No stop file.
- Plan-only: `bot-spot-paper` on the paper rule `tagsnap-2026-11-01`
  (`config/rules/tagsnap-2026-11-01.json`: −1 `momentum.24h` + 0.5
  `asset.direction.24h`, buy +1.0, sell below -0.3; set by its own
  `BOT_SPOT_ALGO_RULE` in `docker-compose.yml`; audit
  `docker-data/bot-spot-paper-decisions.jsonl`, state
  `docker-data/bot-spot-paper-state.json`). Its rows say `"mode":"LIVE"` (the
  env arming) but every route is `planned` with no submission. On
  `tagsnap-2026-11-01` since 2026-11-01 07:58 UTC; first row 07:58:50: hold,
  best ZEC +0.697 under +1.0; 20 of 20 listed scores recomputed from the file.
- `cryptobot-gui` (dashboard, `ssh -L 8787:localhost:8787 pi-wired`) reads
  the live audit and state again. `scheduler` (23:45 UTC backup, 00:30 UTC
  `bb tag-scoreboard`) still runs image 1ce66703a04b (no change it needs).
- Never `docker compose up -d` without service names. The boot unit
  `binance-bot-compose.service` is disabled. `silly_swirles` (exited, not a
  compose service) is unrelated and left alone.
- Backups: `.env.docker.bak-20261031T195533Z-snapback`,
  `docker-data/backup-20261031T195533Z-snapback/` (both audits, both states,
  the old `docker-compose.yml`); `docker-data/backup-20261101T075304Z-tagsnap/`
  (paper audit and state, compose file before the paper-rule switch). Back:
  `touch docker-data/bot-spot.stop`, or restore the `.env.docker` backup and
  `docker compose up -d --no-deps bot-spot`; for the old image retag
  `pre-algo-20261031` as `latest` first.

## Feed pipeline (on the Pi)

`biz-watch` and `biz-feed` (systemd user units, enabled at boot) write
`docker-data/tags-feed.json`, the registry and the hourly history every 15
min; both traders take candidates from it and buy nothing from a feed older
than 45 minutes. Details and stop: OPERATIONS.md, "Tags feed (biz-app)".

## Evidence so far

- Live snap-back, first trade: bought 49.3 TRX at 0.303 (14.94 USDT, all the
  USDT) on 2026-10-31 23:20 UTC (24h move −8.1%, score +0.810); filled, booked
  to sleeve `fast`, no halt. Open at 0.3106 on 2026-11-01 06:43 UTC (about
  +0.38 USDT, +2.5%, before the sell fee); it sells when TRX's 24h change goes
  above +3% (−4.1%, score +0.41, at 06:21 UTC).
- Tag-rule tuning round 1 (`reports/phase54/tag_rule_tuning_2026-11-01.md`):
  36 frozen rule files in four families on 2026-10-01..10-25, none positive in
  both halves with 10 or more round trips; fade-the-board loses in every
  variant; tags + snap-back variants are positive in both halves with 4 round
  trips. The tag pick helps the snap-back rule (+8.18% against −42.92% with
  the liquid 20). The judging window 10-26..11-01 (information only): BTC
  −2.66%, liquid 20 −2.60%, snap-back −2.96%, the tag rule +15.73% from one
  HYPE hold.
- Scoreboard on the Pi (2026-11-01, 745 hourly feeds, 1,093 rows): 1 row with
  |t| ≥ 2 and a positive net (`mood.agreement.24h`, n 145); `momentum.24h` at
  +24h t −2.14.

## Next steps

1. Review the first live snap-back sell: fill, sleeve closed, audit rows, no
   halt (Phase 54 box 2).
2. Tuning round 2 around 2026-11-08: rerun the frozen grid
   (`reports/phase54/tag_rule_tuning_2026-11-01_grid.json`), judged only on
   data after 2026-11-01T02:00Z, plus the paper trader's own record; the
   operator picks (Phase 54 box 3).
