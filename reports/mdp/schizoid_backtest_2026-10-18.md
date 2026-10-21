# Schizoid persona backtest (2026-09-18T10:43:00Z → 2026-10-18T10:43:00Z)

Research only: `bb bot-spot-persona-backtest`, offline on `data/entry-study-cache` (32 coins), no network, no orders. Phase 48, `IMPLEMENTATION_PLAN.md`.

## Settings

- Window: 2026-09-18T10:43:00Z → 2026-10-18T10:43:00Z (30 days), a decision step every 20 minutes (2160 steps).
- Each variant starts with 1000.00 USDT. Costs per side: fee 7.5 bps + slippage 5 bps (25 bps per round trip).
- Liquid universe (20 coins): BTC, ETH, ZEC, SOL, NEAR, XRP, BNB, WLD, HYPE, DOGE, SUI, SAND, TRX, UNI, ENA, STRK, TAO, HBAR, TIA, LINK.
- GCP2: `data/gcp2-cache`, per-second network coherence up to 2026-10-19T23:59:59Z; colour zones as published by GCP2 at 2026-10-20T08:23:14Z (Normal from 0, Elevated from 140, High from 166, Very High from 219, Extreme from 279).

## Results

| Variant | Net USDT | Net % | Max drawdown % | Round trips | Win rate % | Sharpe | Fees USDT | Skipped buys/sells |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| v13 | -322.27 | -32.23 | 37.80 | 44 | 38.6 | -5.166 | 57.89 | 0 |
| high_variance | -268.23 | -26.82 | 40.11 | 76 | 43.4 | -2.069 | 43.50 | 643 |
| schizo | -824.10 | -82.41 | 83.01 | 614 | 37.3 | -16.179 | 405.51 | 0 |
| schizo_mdp | -316.08 | -31.61 | 43.56 | 75 | 58.7 | -2.435 | 106.40 | 0 |

Net = ending equity − starting balance after every fee and slippage; positions still open at the end are marked at their last close. Win rate counts closed round trips with a positive net. Sharpe = mean / population std of step-equity returns × √(525600 / step minutes). Full numbers: the `.csv`; every trade: `_trades.csv`.

## Personas

- `v13`: rule proxy of the live v13/v14 guidance (not the LLM): liquid top 10; buy when flat, breadth ≥ +15 and 4h market median ≥ +1%: the best 4h coin with positive 1h and 4h, all cash; exit `atr=3.5:1h,deadline=360` or sell at breadth ≤ −15.
- `high_variance`: liquid top 10; every step buys each coin not held that is up ≥ +0.5% over 1h, strongest first, up to 3 positions, sized clamp(|1h| / 2%, 0.25, 1.0) of equity capped by cash; exit `atr=6:1h,deadline=1440` only.
- `schizo`: Phase 49, ruled by GCP2: the mood (one of the four base personas) is picked by the GCP2 dice and switches only when the GCP2 zone changes between steps (the mood follows GCP2: an unchanged zone keeps it); the four base personas vote with the mood's vote counting twice (buy or sell at ≥ 2), one coin with all cash; a position keeps the exit plan of the mood that bought it; no stop-loss.
- `schizo_mdp`: Phase 50: `schizo` with the long-term value voice (`bb bot-spot-mdp-fit`, a Markov model of 20-minute steps, fitted before the window) as the strongest voice: a vote sell goes through only when the model values selling above holding, the model sells on its own when it does, and a buy goes through only when its long-term value after the entry cost is above 0 (cash); a buy whose mood has no exit plan (`detached`) runs the live default plan `atr=3.5:1h,deadline=360`.

## Per-variant details

### `v13`

- Ending equity 677.73 USDT; 44 closed round trips (17 wins), 0 still open at the end.
- Exit reasons: deadline 16, follow_peak 2, regime_turn 26.
- Skipped orders: none.

### `high_variance`

- Ending equity 731.77 USDT; 76 closed round trips (33 wins), 2 still open at the end.
- Exit reasons: deadline 63, follow_peak 13, open_at_end 2.
- Skipped orders: no_cash 643.

### `schizo`

- Ending equity 175.90 USDT; 614 closed round trips (229 wins), 1 still open at the end.
- Exit reasons: follow_peak 1, open_at_end 1, stretch 91, vote 522.
- Skipped orders: none.
- mood steps: 1 initial, 338 switched, 1821 held, 0 without a GCP2 reading
- active steps: contrarian 399, detached 527, high_variance 606, v13 628
- zone Elevated -> Elevated: held 16
- zone Elevated -> High: switched 17
- zone Elevated -> Normal: switched 81
- zone Elevated -> Very High: switched 4
- zone High -> Elevated: switched 15
- zone High -> High: held 17
- zone High -> Normal: switched 57
- zone High -> Very High: switched 3
- zone Normal -> Elevated: switched 84
- zone Normal -> High: switched 56
- zone Normal -> Normal: held 1788
- zone Normal -> Very High: switched 7
- zone Very High -> Elevated: switched 3
- zone Very High -> High: switched 2
- zone Very High -> Normal: switched 9
- Per-step trace: `schizoid_backtest_2026-10-18_schizo_trace.csv` (2160 rows, one per step: `time,gcp2_value,from_zone,to_zone,dice,result,mood_before,mood_after`).

### `schizo_mdp`

- Ending equity 683.92 USDT; 75 closed round trips (44 wins), 1 still open at the end.
- Exit reasons: deadline 47, follow_peak 5, mdp 2, open_at_end 1, stretch 19, vote 2.
- Skipped orders: none.
- MDP model: γ=0.95 per step, fitted on 2026-07-20T08:20:00Z → 2026-09-18T10:40:00Z; model sells 2, vote sells it vetoed 1515, buys it vetoed 12
- mood steps: 1 initial, 338 switched, 1821 held, 0 without a GCP2 reading
- active steps: contrarian 399, detached 527, high_variance 606, v13 628
- zone Elevated -> Elevated: held 16
- zone Elevated -> High: switched 17
- zone Elevated -> Normal: switched 81
- zone Elevated -> Very High: switched 4
- zone High -> Elevated: switched 15
- zone High -> High: held 17
- zone High -> Normal: switched 57
- zone High -> Very High: switched 3
- zone Normal -> Elevated: switched 84
- zone Normal -> High: switched 56
- zone Normal -> Normal: held 1788
- zone Normal -> Very High: switched 7
- zone Very High -> Elevated: switched 3
- zone Very High -> High: switched 2
- zone Very High -> Normal: switched 9
- Per-step trace: `schizoid_backtest_2026-10-18_schizo_mdp_trace.csv` (2160 rows, one per step: `time,gcp2_value,from_zone,to_zone,dice,result,mood_before,mood_after`).

## GCP2

- Zone minutes over the window (each minute's reading is the hour ending at the second before it): Normal 38701 (89.6%), Elevated 2308 (5.3%), High 1906 (4.4%), Very High 276 (0.6%), Extreme 9 (0.0%); unavailable 0 (0.0%).

## Assumptions

- Decisions see only bars closed by the step time; intents fill at the open of the 1m bar that opens at the step, sells before buys.
- The 5 USDT minimum applies to buys only; a held position can always be sold.
- A buy whose exit plan cannot start (e.g. no hourly ATR yet) is skipped and counted (`exit_plan_unavailable`); a buy with no cash left is counted as `no_cash`.
- Equity, drawdown and positions still open at the end are marked at the last close without exit costs.
- At an exact 15-minute boundary, breadth uses the bucket that holds the last closed minute.
- The `stretch` exit counts break-even with fees only (as the live rule does), not slippage.
- Exit plans read each 1m bar low before high (the pessimistic order `bb bot-spot-exit-replay` uses); ATR comes from the closed 1h bars before entry and stretch/slowdown get 60 minutes of 1m warm-up.
- `rotation` scores its shadows at the first scheduled step of each new UTC day (00:03 in the default window, keeping the step cadence). A switch step only sells the old book; the new persona trades from the next step on a flat book.
- GCP2 at a step is `|Σ network_coherence|` over the hour ending at the last second before the step's minute (the live `getNetVarAggregate24H` value); an hour with under 90% of its seconds is unavailable, and zones are GCP2's own published bounds.

## Caveats

- The `v13` row is a deterministic rule proxy of the v13/v14 guidance, not the LLM trader, which cannot be replayed (cost, quota, live-only tools).
- Survivorship: the liquid universe is the top 20 by volume as of 2026-10-18, applied to the whole window.
- Costs: 25 bps per round trip, against the measured 14.8 bps of the 40 live trips (deliberately pessimistic).
- The window ends before the Phase 47 (v13) deploy at 2026-10-18T14:02:04Z, so the `v13` row cannot be compared with live v13 trades on the same dates.
- No out-of-sample split: the rules were fixed in `LOOP_SCHIZOID.md` before any real-cache run and not tuned on it, but they are tested on this one period only.
- GCP2 (gcp2.net, a network of random-number generators) has no shown link to crypto prices; `schizo` reads it only because the operator chose it (`LOOP_SCHIZO_LIVE.md`).
