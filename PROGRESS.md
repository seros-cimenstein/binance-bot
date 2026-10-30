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
- The Pi still has the old LLM trader image. Do not start it: `docker compose
  up -d` with no service names would start it. A reboot does not: the boot
  unit `binance-bot-compose.service` is disabled, and `bot-spot` (restart
  `unless-stopped`) was stopped by hand, so Docker leaves it exited (checked
  2026-10-29).
- The Pi also runs the old compose file. The next deploy uses
  `docker compose --profile all up -d --remove-orphans`, or names the
  services (`... up -d --remove-orphans cryptobot-gui scheduler`) while the
  trader must stay stopped (`OPERATIONS.md`, "Docker").

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
- Feed pipeline (laptop, biz-app): `biz-insights feed --watch` (live feed),
  `--history --step 60` (backtest input), `scripts/biz_feed_to_pi.sh` (copy).
  The lead study found no /biz/ lead and the chosen rule lost 11.18% on the
  backtest's judging half (`IMPLEMENTATION_PLAN.md`, Phase 52).

## Next steps

1. Pi rollout (`LOOP_ALGO.md` E1-E6): backup and rollback tag, new image,
   feed copy, plan-only practice run (`bot-spot-paper`), operator review,
   live only on the operator's go.
