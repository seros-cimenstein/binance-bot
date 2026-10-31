# Binance Bot Progress

Current state only; rewrite it, do not append (`CODEX.md`, "Required
Workflow"). History: git log, `docs/decisions.md`, `docs/archive/`.

Updated 2026-10-31.

## Direction

The trader is a nonstop algorithm: Binance prices plus /biz/ tags from
biz-app, no LLM, no chat, no timer, no Telegram (`docs/decisions.md`,
"2026-10-29 algorithm trader"). Phase 53 (dynamic tags, scoreboard, rule
file, sleeves; `IMPLEMENTATION_PLAN.md`, loop `LOOP_TAGS.md`) is deployed on
the Pi and practising plan-only since 2026-10-31 15:05 UTC; the operator's
review after two hours: keep practising. Going live stays the operator's
go.

## On the Pi (`ssh pi-wired`, `~/binance-bot`)

- Image `binance-bot:latest` = 1ce66703a04b (Phase 53, built 2026-10-31).
  Rollback tags: `binance-bot:pre-tags-20261031` = e9efbd3c98dc (Phase 52,
  reads `biz-feed.json`), `binance-bot:pre-algo-20261030` = 428220df4d0f (the
  old LLM trader).
- Running on the new image: `bot-spot-paper` (`bb bot-spot --plan-only
  --nonstop`, rule file `config/rules/algo-2026-10-29.json`: sleeve `fast`
  share 1, `core` share 0; audit `docker-data/bot-spot-paper-decisions.jsonl`,
  state with sleeve holdings `docker-data/bot-spot-paper-state.json`),
  `cryptobot-gui` (dashboard, `ssh -L 8787:localhost:8787 pi-wired`; shows
  the practice run's audit and state while live is stopped, operator
  2026-10-31), `scheduler` (23:45 UTC backup, 00:30 UTC `bb tag-scoreboard`
  into `docker-data/scoreboard/`).
- Stopped: `bot-spot` (live trader), exited since 2026-10-22 on the old
  image; `docker compose up -d bot-spot` would recreate it on the new image
  and trade live (`--submit-live --nonstop`). Never `docker compose up -d`
  without service names. The boot unit `binance-bot-compose.service` is
  disabled. No stop file. Wallet about 17.4 USDT, mostly USDT cash, no open
  position. `silly_swirles` (exited, 4 months old, not a compose service)
  is unrelated and left alone.
- `.env.docker`: `BOT_SPOT_ALGO_RULE`, `BOT_SPOT_TAGS_FEED_PATH=data/tags-feed.json`,
  `BOT_SPOT_PROXIES=config/proxies.json` (gold through PAXG and XAUT); the
  Phase 52 env rule lines are removed; old settings remain, ignored with one
  warning. Backups before Phase 53: `.env.docker.bak-20261031T144929Z-phase53`,
  `docker-data/backup-20261031T144929Z-phase53/` (trader files, the old
  `feed.sqlite3`), `docker-data/backup-20261031T150410Z-phase53-preimport/`
  (`feed.sqlite3` just before the tag import), `~/biz-app-backup-20261031T144929Z-phase53.tgz`
  (biz-app code), `~/biz-app-backup-20261031T144929Z-phase53-biz.sqlite3`
  (archive DB before the `posts_time` index), and the old units
  `~/biz-{feed,watch}.service.bak-20261031-phase53`. Phase 52:
  `.env.docker.bak-20261030T082713Z-phase52`,
  `docker-data/backup-20261030T065538Z-phase52/`.

## Feed pipeline (on the Pi)

Two systemd user units (operator: "Plain Pi programs", 2026-10-30), enabled
at boot (linger on): `biz-watch` (scraper, `biz watch --no-media` in
`~/biz-app/biz_scraper`; archive indexed on `time`) and `biz-feed`
(`biz-insights feed --watch --history-dir docker-data/tags-history`, a round
every 15 min, prompt `tags-1`, Gemini through omp with its own agent dir
`~/.local/share/biz-feed/omp-agent`) writing `docker-data/tags-feed.json`,
`tags-registry.json` and the hourly history `docker-data/tags-history/`
(from 2026-10-01). Tag cache `docker-data/feed.sqlite3` (`post_tags` from the
laptop's October re-tag, `post_stances` kept for the rollback). The old
`docker-data/biz-feed.json` is kept, no longer written. The trader treats a
feed older than 45 minutes as unavailable and then buys nothing. Stop:
`ssh pi-wired 'systemctl --user stop biz-feed biz-watch'` (OPERATIONS.md,
"/biz/ feed").

## Evidence so far

- The Phase 52 lead study on 2026-10-01..25: no /biz/ lead; `bb
  tag-scoreboard` replaces it (first nightly run 2026-11-01 00:30 UTC).
- The chosen rule's backtest: +18.42% on the tuning half, -11.18% on the
  judging half (`reports/algo_backtest_2026-10-25.md`, the old /biz/ feed).
- The same rule on the tags history (Phase 53 E1, candidates as live):
  -21.05% over 2026-10-01T14:00Z..10-25T14:00Z, 15 round trips
  (`reports/phase53/algo_backtest_2026-10-25.md`); the difference to the old
  backtest is the candidate set only (OPERATIONS.md, "Algorithm backtest").
- Practice run review 2026-10-30 09:41 UTC: one planned ETH buy in 42
  minutes; operator: "(a) Keep practising".
- Phase 53 practice run 2026-10-31 15:05-17:05 UTC: feed at most 16 minutes
  old, no error, halt or submission, 220 of 220 scores and 11 of 11 actions
  recomputed from the rule file, core unfunded, 8 planned buys (STX and DOGE
  alternating); first scoreboard on the Pi 1,093 rows; operator: "(a) Keep
  practising plan-only".

## Next steps

1. Close Phase 53 (`LOOP_TAGS.md` G1). Then the tuning phase, where the
   operator picks weights, rule style and the sleeve split from the
   scoreboard (nightly 00:30 UTC). The pre-change binaries are kept in
   `~/binance-bot-prechange/` (README there).
2. Live only on the operator's go (`CODEX.md`, rollout rule): back up
   `.env.docker`, tag a rollback image, remove the dashboard's practice-run
   paths in `docker-compose.yml`, `docker compose up -d bot-spot
   cryptobot-gui`, watch the first decisions and orders.
