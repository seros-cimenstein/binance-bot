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

- Image `binance-bot:latest` = 1f6647c1b187 (2026-11-01, adds
  `market.mood.tone`), run by `bot-spot` and `scheduler`. `bot-spot-paper`
  runs eb660f6cc305 (`pre-tone-20261101`), `cryptobot-gui` 093b478c3eff.
  Rollback tags: `pre-algo-20261102` = 093b478c3eff (the image snap-back ran
  live on), `pre-algo-20261031` = 1ce66703a04b (Phase 53, live stopped),
  `pre-tags-20261031` = e9efbd3c98dc (Phase 52), `pre-algo-20261030` =
  428220df4d0f (the old LLM trader).
- LIVE: `bot-spot` (`bb bot-spot --submit-live --nonstop`, on
  `tagsnap-2026-11-01` since 2026-11-02 07:06:02 UTC), `.env.docker`
  `BOT_SPOT_ALGO_RULE=config/rules/tagsnap-2026-11-01.json`: sleeve `fast`
  share 1, score −1 `momentum.24h` + 0.5 `asset.direction.24h`, buy at +1.0,
  sell below -0.3; `core` share 0. Audit `docker-data/bot-spot-decisions.jsonl`,
  state `docker-data/bot-spot-agent-state.json`. Holds 27.1 RUNE in sleeve
  `fast` since 2026-11-01 21:22 UTC (operator: "Hold it"); first row 07:06:31:
  keep RUNE, score +1.000; 21 of 21 listed scores recomputed from the file.
  0.15 USDT free plus dust; wallet about 16.3 USDT. No stop file.
- Plan-only: `bot-spot-paper` on the same rule (its own `BOT_SPOT_ALGO_RULE`
  in `docker-compose.yml`; audit `docker-data/bot-spot-paper-decisions.jsonl`,
  state `docker-data/bot-spot-paper-state.json`). It sizes from the real
  wallet, so while live holds the cash every buy is "below min notional" (19
  such rows since 2026-11-01 12:38).
- `cryptobot-gui` (dashboard, `ssh -L 8787:localhost:8787 pi-wired`) reads
  the live audit and state. `scheduler`: 23:45 UTC backup, 00:30 UTC
  `bb tag-scoreboard` into `docker-data/scoreboard/`.
- Never `docker compose up -d` without service names. The boot unit
  `binance-bot-compose.service` is disabled. `silly_swirles` (exited, not a
  compose service) is unrelated and left alone.
- Backups: `.env.docker.bak-20261102T070513Z-tagsnap-live` and
  `docker-data/backup-20261102T070513Z-tagsnap-live/` (both audits, both
  states, compose file) before the tagsnap switch; older:
  `.env.docker.bak-20261031T195533Z-snapback`,
  `docker-data/backup-20261031T195533Z-snapback/`,
  `docker-data/backup-20261101T075304Z-tagsnap/`. Back: `touch
  docker-data/bot-spot.stop`, or restore the `.env.docker` backup and `docker
  compose up -d --no-deps bot-spot`; for the old image retag
  `pre-algo-20261102` as `latest` first.

## Feed pipeline (on the Pi)

`biz-watch` and `biz-feed` (systemd user units, enabled at boot) write
`docker-data/tags-feed.json`, the registry and the hourly history every 15
min; both traders take candidates from it and buy nothing from a feed older
than 45 minutes. Details and stop: OPERATIONS.md, "Tags feed (biz-app)".

## Evidence so far

- Snap-back live (2026-10-31 20:01 to 2026-11-02 07:06 UTC): TRX bought 10-31
  23:20 at 0.303 (14.94 USDT), sold 11-01 21:22 at 0.313 (+0.49 USDT);
  the same minute 27.1 RUNE bought at 0.568 (15.39 USDT) after a 10%+ 24h
  drop; RUNE 0.515 at 2026-11-02 06:57 (−1.44 USDT, −9.3%, not sold). Net
  −0.94 USDT, −6.3% of the 15.05 USDT traded. All orders filled, sleeve
  booking right, no halt. The paper rule had signalled RUNE at about 0.618
  (11-01 12:38): about −16.7% had it held the money.
- Tag-rule tuning round 1 (`reports/phase54/tag_rule_tuning_2026-11-01.md`):
  36 frozen rule files in four families on 2026-10-01..10-25, none positive in
  both halves with 10 or more round trips; fade-the-board loses in every
  variant; tags + snap-back variants are positive in both halves with 4 round
  trips. The tag pick helps the snap-back rule (+8.18% against −42.92% with
  the liquid 20). The judging window 10-26..11-01 (information only): BTC
  −2.66%, liquid 20 −2.60%, snap-back −2.96%, the tag rule +15.73% from one
  HYPE hold.
- Fear/greed signal `market.mood.tone` (2026-11-01,
  `reports/phase54/fear_greed_2026-11-01.md`): the old `market.mood.direction`
  counts fear and euphoria alike; the new tone has scoreboard |t| at most 1.35
  (contrarian at +24h in the older half only), and none of 8 snap-back variants
  with it passes the bar. Scoreboard on the Pi (2026-11-01, 745 hourly feeds):
  1 row with |t| ≥ 2 and a positive net; `momentum.24h` at +24h t −2.14.

## Next steps

1. Watch the live tagsnap rule: RUNE sells when it scores below -0.3 (24h
   move above +3%, less half the board lean); no stop-loss or maximum hold.
2. Tuning round 2 around 2026-11-08: rerun the frozen grids
   (`reports/phase54/tag_rule_tuning_2026-11-01_grid.json`,
   `fear_greed_2026-11-01_grid.json`) plus the attention test, judged only on
   data after 2026-11-01T02:00Z, with the paper trader's own record; the
   operator picks (Phase 54 box 3).
