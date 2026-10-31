# Binance Bot Progress

Current state only; rewrite it, do not append (`CODEX.md`, "Required
Workflow"). History: git log, `docs/decisions.md`, `docs/archive/`.

Updated 2026-10-31.

## Direction

The trader is a nonstop algorithm: Binance prices plus /biz/ tags from
biz-app, no LLM, no chat, no timer, no Telegram. Since 2026-10-31 20:01 UTC it
trades LIVE with the operator's snap-back rule (`docs/decisions.md`,
"2026-10-31 snap-back rule live"; plan-only step waived by the operator). The
tag rule `algo-2026-10-29` keeps practising plan-only; tuning how it operates
is next (`IMPLEMENTATION_PLAN.md` Phase 54).

## On the Pi (`ssh pi-wired`, `~/binance-bot`)

- Image `binance-bot:latest` = 093b478c3eff (2026-10-31, adds the snap-back
  rule file). Rollback tags: `binance-bot:pre-algo-20261031` = 1ce66703a04b
  (Phase 53 image, live stopped), `binance-bot:pre-tags-20261031` =
  e9efbd3c98dc (Phase 52), `binance-bot:pre-algo-20261030` = 428220df4d0f (the
  old LLM trader).
- LIVE: `bot-spot` (`bb bot-spot --submit-live --nonstop`, since 2026-10-31
  20:01:45 UTC), `.env.docker` `BOT_SPOT_ALGO_RULE=config/rules/snapback-2026-10-31.json`:
  sleeve `fast` share 1, score −1 `momentum.24h`, buy at +0.8 (fell 8%+ in
  24h), sell below -0.3; `core` share 0. Audit `docker-data/bot-spot-decisions.jsonl`,
  state `docker-data/bot-spot-agent-state.json`. First decision 20:02:07 UTC:
  hold in cash, best TRX +0.722 (24h −7.22%) under +0.8; 32 coins scored; 20
  of 20 listed scores recomputed from the rule file. Wallet about 17.28 USDT
  (15.05 free USDT plus dust), no position. No stop file.
- Plan-only: `bot-spot-paper` on the tag rule `algo-2026-10-29` (its own
  `BOT_SPOT_ALGO_RULE` in `docker-compose.yml`; audit
  `docker-data/bot-spot-paper-decisions.jsonl`, state
  `docker-data/bot-spot-paper-state.json`). Its rows say `"mode":"LIVE"` (the
  env arming) but every route is `planned` with no submission.
- `cryptobot-gui` (dashboard, `ssh -L 8787:localhost:8787 pi-wired`) reads
  the live audit and state again. `scheduler` (23:45 UTC backup, 00:30 UTC
  `bb tag-scoreboard`) still runs image 1ce66703a04b (no change it needs).
- Never `docker compose up -d` without service names. The boot unit
  `binance-bot-compose.service` is disabled. `silly_swirles` (exited, not a
  compose service) is unrelated and left alone.
- Backups of this rollout: `.env.docker.bak-20261031T195533Z-snapback`,
  `docker-data/backup-20261031T195533Z-snapback/` (both audits, both states,
  the old `docker-compose.yml`). Back: `touch docker-data/bot-spot.stop`, or
  restore the `.env.docker` backup and `docker compose up -d --no-deps
  bot-spot`; for the old image retag `pre-algo-20261031` as `latest` first.

## Feed pipeline (on the Pi)

systemd user units, enabled at boot (linger on): `biz-watch` (scraper,
`biz watch --no-media` in `~/biz-app/biz_scraper`) and `biz-feed`
(`biz-insights feed --watch --history-dir docker-data/tags-history`, a round
every 15 min, prompt `tags-1`, Gemini through omp, agent dir
`~/.local/share/biz-feed/omp-agent`) writing `docker-data/tags-feed.json`,
`tags-registry.json` and the hourly history `docker-data/tags-history/`.
Both traders take candidates from it: a feed older than 45 minutes means no
buys. Stop: `ssh pi-wired 'systemctl --user stop biz-feed biz-watch'`
(OPERATIONS.md, "/biz/ feed").

## Evidence so far

- Scoreboard on the Pi (2026-10-31, 736 hourly feeds, 1,093 rows): no tag
  signal with |t| ≥ 2 and a positive net; `momentum.24h` at +24h −1.36%
  excess, t −2.48, same sign in both halves.
- Snap-back rule on the tags history 2026-10-01T14:00Z..10-25T14:00Z: +0.21%
  older half, +7.81% newer half, +8.18% whole window, 6 round trips
  (in-sample). Tag rule on the same window: −21.05%, 15 round trips
  (`reports/phase53/algo_backtest_2026-10-25.md`).

## Next steps

1. Watch the first live snap-back orders: fills, sleeve booking in the state
   file, audit rows, no halt (Phase 54 box 2).
2. Tag-rule tuning, planned with the operator (Phase 54 box 3); 30-day
   scoreboard counts are comparable from about 2026-11-07.
