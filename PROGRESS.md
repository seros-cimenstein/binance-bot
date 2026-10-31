# Binance Bot Progress

Current state only; rewrite it, do not append (`CODEX.md`, "Required
Workflow"). History: git log, `docs/decisions.md`, `docs/archive/`.

Updated 2026-10-31.

## Direction

The trader is a nonstop algorithm: Binance prices plus /biz/ tags from
biz-app, no LLM, no chat, no timer, no Telegram (`docs/decisions.md`,
"2026-10-29 algorithm trader"). Phase 53 (dynamic tags, scoreboard, rule
file, sleeves; `docs/archive/implementation-plan-phase-53.md`) is finished and
practising plan-only on the Pi; operator review 2026-10-31: keep practising.
Next is the tuning phase; going live stays the operator's go.

## On the Pi (`ssh pi-wired`, `~/binance-bot`)

- Image `binance-bot:latest` = 1ce66703a04b (Phase 53, 2026-10-31).
  Rollback tags: `binance-bot:pre-tags-20261031` = e9efbd3c98dc (Phase 52,
  reads `biz-feed.json`), `binance-bot:pre-algo-20261030` = 428220df4d0f (the
  old LLM trader).
- Running: `bot-spot-paper` (`bb bot-spot --plan-only --nonstop`, rule file
  `config/rules/algo-2026-10-29.json`: sleeve `fast` share 1, `core` share 0;
  audit `docker-data/bot-spot-paper-decisions.jsonl`, state with sleeve
  holdings `docker-data/bot-spot-paper-state.json`, since 2026-10-31 15:05
  UTC), `cryptobot-gui` (dashboard, `ssh -L 8787:localhost:8787 pi-wired`;
  shows the practice run while live is stopped), `scheduler` (23:45 UTC
  backup, 00:30 UTC `bb tag-scoreboard` into `docker-data/scoreboard/`).
- Stopped: `bot-spot` (live trader), exited since 2026-10-22 on the old
  image; `docker compose up -d bot-spot` would recreate it on the new image
  and trade live. Never `docker compose up -d` without service names. The
  boot unit `binance-bot-compose.service` is disabled. No stop file. Wallet
  about 17 USDT, mostly cash, no position. `silly_swirles` (exited, not a
  compose service) is unrelated and left alone.
- `.env.docker`: `BOT_SPOT_ALGO_RULE`, `BOT_SPOT_TAGS_FEED_PATH`,
  `BOT_SPOT_PROXIES=config/proxies.json` (gold through PAXG and XAUT); old
  settings ignored with one warning. Phase 53 backups:
  `.env.docker.bak-20261031T144929Z-phase53`,
  `docker-data/backup-20261031T144929Z-phase53/`,
  `docker-data/backup-20261031T150410Z-phase53-preimport/`,
  `~/biz-app-backup-20261031T144929Z-phase53{.tgz,-biz.sqlite3}`,
  `~/biz-{feed,watch}.service.bak-20261031-phase53`.

## Feed pipeline (on the Pi)

systemd user units, enabled at boot (linger on): `biz-watch` (scraper,
`biz watch --no-media` in `~/biz-app/biz_scraper`) and `biz-feed`
(`biz-insights feed --watch --history-dir docker-data/tags-history`, a round
every 15 min, prompt `tags-1`, Gemini through omp, agent dir
`~/.local/share/biz-feed/omp-agent`) writing `docker-data/tags-feed.json`,
`tags-registry.json` and the hourly history `docker-data/tags-history/`.
Tag cache `docker-data/feed.sqlite3` (`post_stances` kept for the rollback,
as is the old `biz-feed.json`). A feed older than 45 minutes is unavailable
and nothing is bought. Stop: `ssh pi-wired 'systemctl --user stop biz-feed
biz-watch'` (OPERATIONS.md, "/biz/ feed").

## Evidence so far

- Phase 53 practice run 2026-10-31 15:05-17:05 UTC: feed at most 16 minutes
  old, no error, halt or submission, 220 of 220 scores and 11 of 11 actions
  recomputed from the rule file, 8 planned buys (STX and DOGE alternating).
- First scoreboard on the Pi: 736 hourly feeds, 1,093 rows
  (`docker-data/scoreboard/latest.json`, Scoreboard tab).
- The rule on the tags history (E1, candidates as live): -21.05% over
  2026-10-01T14:00Z..10-25T14:00Z, 15 round trips
  (`reports/phase53/algo_backtest_2026-10-25.md`); on the old feed +18.42% /
  -11.18% tuning / judging half (`reports/algo_backtest_2026-10-25.md`).

## Next steps

1. The tuning phase (to be planned with the operator): weights, rule style
   (fixed or edge) and the sleeve split, from the nightly scoreboard; 30-day
   counts are comparable from about 2026-11-07.
2. Live only on the operator's go (`CODEX.md`, rollout rule;
   `IMPLEMENTATION_PLAN.md` Phase 52 "Live"): back up `.env.docker`, tag a
   rollback image, remove the dashboard's practice-run paths in
   `docker-compose.yml`, `docker compose up -d bot-spot cryptobot-gui`, watch
   the first decisions and orders.
