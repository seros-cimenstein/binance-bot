# Binance Bot Progress

Current state only; rewrite it, do not append (`CODEX.md`, "Required
Workflow"). History: git log, `docs/decisions.md`, and the old log in
`docs/archive/progress-2026-05-25_to_2026-10-29.md`.

Updated 2026-10-29.

## Live trader

- Bot Spot (`bot-spot` on the Pi, `ssh pi-wired`, `~/binance-bot`, env
  `.env.docker`, data `docker-data/`) is **stopped** since 2026-10-22 05:00
  UTC (operator: "stop the bot aight"). `docker compose stop`, exit 0. No stop
  file exists. No open position: about 17.41 USDT, 15.05 of it USDT cash, plus
  BNB and dust.
- The deployed image is still `bot-spot-v17-schizo` with `BOT_SPOT_DECIDER`
  unset (`llm`); the Phase 51 code is not on the Pi. Restarting it now would
  run the v17 LLM trader: `cd ~/binance-bot && docker compose up -d bot-spot`.
- Rollback tags on the Pi: `binance-bot:pre-v17-20261021` (v16),
  `binance-bot:pre-v16-20261020` (v15), `binance-bot:pre-c4-20261020` (v14).
- Every other live runtime is off since 2026-10-12 (`OPERATIONS.md`, "Live
  Posture").

## Active work: Phase 51, /biz/ feed and rule-based decider

Plan and boxes: `IMPLEMENTATION_PLAN.md`, Phase 51. Rules: `CODEX.md`,
"Phase 51 rule-based decider". Commit `4a31d5f`.

- Done in code: `bb biz-lead-study`, `crate::biz` (feed read every cycle,
  audit field `biz`), `crate::decider` and the `biz_algo` backtest persona,
  `BOT_SPOT_DECIDER=llm|algo`, compose service `bot-spot-algo-paper`
  (profile `algo-paper`, plan-only), `scripts/biz_feed_to_pi.sh`, dashboard
  /biz/ tab.
- Verified 2026-10-29: `cargo check -p bb-trader -p bb-strategies -p
  cryptobot-gui --all-targets` passed; `cargo test -p bb-trader -p
  bb-strategies` passed (0 failed). The full workspace suite, clippy and the
  smoke scripts have not been run on this code yet.
- Blocked: biz-app (`~/projects/automation/imageboards/biz-app`, outside this
  repository) does not write `biz-feed-v1` yet (Levels 1-2), so there are no
  feeds to run the lead study or the `biz_algo` backtest on, and the algo
  weights have no evidence behind them.

## Next steps

1. biz-app: `biz-insights feed` and `--history --step 60`.
2. Run `bb biz-lead-study` and the `biz_algo` backtest on the real history;
   record the `@newer` verdict in `IMPLEMENTATION_PLAN.md`.
3. Full checks (`cargo test --workspace`, clippy `-D warnings`,
   `scripts/smoke.sh`, `scripts/pool_replay.sh`), then deploy Level 3 to the
   Pi and start the feed copier.
4. `bot-spot-algo-paper` practice run. Going live needs the operator's go and
   a rollback tag `binance-bot:pre-algo-<date>`.

## Other open items

- Phase 43 X collector: image built and launch-tested on the Pi; not
  collecting (no burner login there, service not started).
- The laptop's untracked `.env.docker` still has
  `BOT_SPOT_MARKET_PICK_MODE=small_market_aggressive` (the Pi has
  `liquid_top_value`); deploys never copy it.
- Phase 50 note: refit the Markov model (`bb bot-spot-mdp-fit`) when the
  candle cache is extended.
