# Binance Bot Progress

Current state only; rewrite it, do not append (`CODEX.md`, "Required
Workflow"). History: git log, `docs/decisions.md`, `docs/archive/`.

Updated 2026-10-29.

## Direction

The trader becomes a nonstop algorithm: Binance data plus /biz/ scores from
biz-app, no LLM, no chat, no timer, no Telegram (`docs/decisions.md`,
"2026-10-29 algorithm trader"). Plan: `IMPLEMENTATION_PLAN.md`, Phase 52.

## Live trader

- `bot-spot` on the Pi (`ssh pi-wired`, `~/binance-bot`, env
  `.env.docker`, data `docker-data/`) is **stopped** since 2026-10-22 05:00
  UTC. No open position: about 17.41 USDT, 15.05 of it USDT cash, plus BNB
  and dust. No stop file exists.
- The Pi runs the Phase 52 image since 2026-10-30 08:16 UTC (`binance-bot:latest`
  = e9efbd3c98dc; rollback `binance-bot:pre-algo-20261030` = the old LLM
  image 428220df4d0f). `cryptobot-gui` and `scheduler` (only the 23:45 backup
  job) run on it; the old paper/sniper/signals containers are removed.
  `bot-spot` stays exited on the old image until the operator's go; never
  `docker compose up -d` without service names. The boot unit
  `binance-bot-compose.service` is disabled. Backup of the trader files:
  `docker-data/backup-20261030T065538Z-phase52/`; `.env.docker` before the
  rule lines: `.env.docker.bak-20261030T082713Z-phase52`.
- Feed pipeline, running detached on the laptop since 2026-10-30 (operator,
  `docs/decisions.md`): the /biz/ scraper `biz watch` on the real archive,
  `biz-insights feed --watch` (a round every 15 min) and
  `scripts/biz_feed_to_pi.sh` (a copy every 5 min) into the Pi's
  `docker-data/biz-feed.json`. Stop: `pkill -f 'biz watch'`,
  `pkill -f 'biz-insights.*feed --watch'`, `pkill -f biz_feed_to_pi`.

## Repo

- Pruned to the trader on 2026-10-29 (`docs/decisions.md`); the moved modules
  are in `../redundancy/binance-bot-modules/`.
- Phase 52 code is done on the laptop (`LOOP_ALGO.md` C1-D9): the LLM path,
  Telegram, bb-social, GCP2/schizo, MDP, personas, exit plans, the 20-coin
  breadth read and market-mood gauge, the dashboard's Sessions tab and its LLM
  views are gone. `bb bot-spot --nonstop` ticks every 15 s and writes a
  decision row on change plus hourly; `bb algo-backtest` backtests the rule;
  the dashboard's Algorithm tab shows every scored coin. The rule in force is
  `algo-2026-10-29` (`docs/decisions.md`). None of this is on the Pi yet. The
  Pi's `.env.docker` still sets the `BOT_SPOT_EXIT_*` and
  `BOT_SPOT_DEFAULT_EXIT_PLAN*` variables; the new image ignores them with one
  warning, so remove them at the Pi rollout.
- Feed history for backtests: `biz-insights feed --history --step 60`. The
  lead study found no /biz/ lead and the chosen rule lost 11.18% on the
  backtest's judging half (`IMPLEMENTATION_PLAN.md`, Phase 52).

## Next steps

1. The plan-only practice run `bot-spot-paper` runs on the Pi since
   2026-10-30 08:59 UTC (audit `docker-data/bot-spot-paper-decisions.jsonl`).
   Operator review 2026-10-30: "(a) Keep practising", so it stays plan-only
   and `bot-spot` stays stopped (`docs/decisions.md`). Review it again; live
   only on the operator's go: `.env.docker` backup, `docker compose up -d
   bot-spot` (it runs `--submit-live --nonstop`), rollback tag
   `binance-bot:pre-algo-20261030`.
