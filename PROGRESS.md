# Binance Bot Progress

Current state only; rewrite it, do not append (`CODEX.md`, "Required
Workflow"). History: git log, `docs/decisions.md`, `docs/archive/`.

Updated 2026-10-30.

## Direction

The trader is a nonstop algorithm: Binance prices plus /biz/ scores from
biz-app, no LLM, no chat, no timer, no Telegram (`docs/decisions.md`,
"2026-10-29 algorithm trader"). Phase 52 (`IMPLEMENTATION_PLAN.md`) is built
and practising plan-only on the Pi; only going live is open, on the
operator's go.

## On the Pi (`ssh pi-wired`, `~/binance-bot`)

- Image `binance-bot:latest` = e9efbd3c98dc (Phase 52, 2026-10-30). Rollback
  tag `binance-bot:pre-algo-20261030` = 428220df4d0f (the old LLM trader).
- Running: `bot-spot-paper` (the nonstop trader, `--plan-only`, rule
  `algo-2026-10-29`, audit `docker-data/bot-spot-paper-decisions.jsonl`,
  since 2026-10-30 08:59 UTC), `cryptobot-gui` (dashboard,
  `ssh -L 8787:localhost:8787 pi-wired`), `scheduler` (only the 23:45 UTC
  backup job).
- Stopped: `bot-spot` (live trader), exited since 2026-10-22 on the old
  image; `docker compose up -d bot-spot` would recreate it on the new image
  and trade live (`--submit-live --nonstop`). Never `docker compose up -d`
  without service names. The boot unit `binance-bot-compose.service` is
  disabled. No stop file. Wallet about 17.4 USDT, mostly USDT cash, no open
  position. `silly_swirles` (exited, 4 months old, not a compose service)
  is unrelated and left alone.
- `.env.docker` has the rule (`BOT_SPOT_ALGO_WEIGHTS=0,1,1,0.5,-0.5`,
  `BOT_SPOT_ALGO_MIN_POSTERS=2`, `BOT_SPOT_RULES_VERSION=algo-2026-10-29`)
  and still the old settings, which the trader ignores with one warning.
  Backups: `.env.docker.bak-20261030T082713Z-phase52`,
  `docker-data/backup-20261030T065538Z-phase52/` (the trader files before
  the rollout).

## Feed pipeline (on the Pi)

Since 2026-10-30 (operator: "Plain Pi programs", `docs/decisions.md`) two
systemd user units on the Pi, enabled at boot (linger on): `biz-watch`
(scraper, `biz watch --no-media` in `~/biz-app/biz_scraper`) and `biz-feed`
(`biz-insights feed --watch`, a round every 15 min, Gemini through omp with
its own agent dir `~/.local/share/biz-feed/omp-agent`) writing
`docker-data/biz-feed.json` directly. Bun 1.4.4, omp 18.14.0 and uv installed
for user `seref`. The laptop copier is removed; the laptop's own scraper
(`biz watch`, with images) still runs for the operator's archive. The trader
treats a feed older than 45 minutes as unavailable and then buys nothing.
Stop: `ssh pi-wired 'systemctl --user stop biz-feed biz-watch'`
(OPERATIONS.md, "/biz/ feed"). Backtest input: `biz-insights feed --history
--step 60` on the laptop.

## Evidence so far

- `bb biz-lead-study` on 2026-10-01..25: no /biz/ lead (`IMPLEMENTATION_PLAN.md`).
- The chosen rule's backtest: +18.42% on the tuning half, -11.18% on the
  judging half (`reports/algo_backtest_2026-10-25.md`).
- Practice run review 2026-10-30 09:41 UTC: one planned ETH buy in 42
  minutes; operator: "(a) Keep practising".

## Next steps

1. Phase 53, dynamic tags, scoreboard and two horizons
   (`IMPLEMENTATION_PLAN.md`; operator decisions 2026-10-30): feed (seven
   tag kinds), signals and scoreboard, trader (rule file, sleeves, risk-off),
   backtest, dashboard, plan-only rollout. Then a tuning phase where the
   operator picks weights, rule style and the sleeve split.
2. Live only on the operator's go (`CODEX.md`, rollout rule): back up
   `.env.docker`, `docker compose up -d bot-spot`, watch the first decisions
   and orders; back: retag `binance-bot:pre-algo-20261030` and recreate.
