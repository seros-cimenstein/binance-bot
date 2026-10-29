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
  up -d` with no service names, or a Pi reboot (the boot unit
  `binance-bot-compose.service` runs `--profile all up -d`), would start it.
- The Pi also runs the old compose file. The next deploy uses
  `docker compose --profile all up -d --remove-orphans`, or names the
  services (`... up -d --remove-orphans cryptobot-gui scheduler`) while the
  trader must stay stopped (`OPERATIONS.md`, "Docker").

## Repo

- Pruned to the trader on 2026-10-29 (`docs/decisions.md`); the moved modules
  are in `../redundancy/binance-bot-modules/`.
- The code is still the LLM trader; Phase 52 removes it and builds the
  algorithm. `README.md` and `OPERATIONS.md` describe the code as it is until
  then.

## Next steps

1. Decide the algorithm's rules and the live price source with the operator
   (Phase 52, "Decide with the operator").
2. Remove the old trader code (Phase 52, "Remove the old trader").
3. biz-app writes `biz-feed-v1` and its hourly history; run the lead study.
4. Build the nonstop trader and its backtest, then a plan-only run on the Pi.
