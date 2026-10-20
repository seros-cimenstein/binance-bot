# Schizoid persona backtest (2026-08-19T10:43:00Z → 2026-10-18T10:43:00Z)

Research only: `bb bot-spot-persona-backtest`, offline on `data/entry-study-cache` (32 coins), no network, no orders. Phase 48, `IMPLEMENTATION_PLAN.md`.

## Settings

- Window: 2026-08-19T10:43:00Z → 2026-10-18T10:43:00Z (60 days), a decision step every 20 minutes (4320 steps).
- Each variant starts with 1000.00 USDT. Costs per side: fee 7.5 bps + slippage 5 bps (25 bps per round trip).
- Liquid universe (20 coins): BTC, ETH, ZEC, SOL, NEAR, XRP, BNB, WLD, HYPE, DOGE, SUI, SAND, TRX, UNI, ENA, STRK, TAO, HBAR, TIA, LINK.

## Results

| Variant | Net USDT | Net % | Max drawdown % | Round trips | Win rate % | Sharpe | Fees USDT | Skipped buys/sells |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| v13 | -183.99 | -18.40 | 39.36 | 100 | 40.0 | -0.865 | 161.52 | 0 |
| contrarian | -638.01 | -63.80 | 65.17 | 499 | 57.9 | -8.873 | 511.34 | 0 |
| detached | -918.29 | -91.83 | 95.80 | 1552 | 34.9 | -6.655 | 775.60 | 0 |
| high_variance | 193.82 | 19.38 | 35.59 | 174 | 50.0 | 1.517 | 124.51 | 354 |
| rotation | -159.82 | -15.98 | 55.95 | 436 | 42.7 | -0.163 | 547.39 | 787 |
| ensemble | -832.86 | -83.29 | 86.20 | 623 | 34.8 | -9.860 | 544.38 | 0 |

Net = ending equity − starting balance after every fee and slippage; positions still open at the end are marked at their last close. Win rate counts closed round trips with a positive net. Sharpe = mean / population std of step-equity returns × √(525600 / step minutes). Full numbers: the `.csv`; every trade: `_trades.csv`.

## Personas

- `v13`: rule proxy of the live v13/v14 guidance (not the LLM): liquid top 10; buy when flat, breadth ≥ +15 and 4h market median ≥ +1%: the best 4h coin with positive 1h and 4h, all cash; exit `atr=3.5:1h,deadline=360` or sell at breadth ≤ −15.
- `contrarian`: fades breadth: liquid top 10; buy when flat and breadth ≤ −15: the worst 4h coin, all cash; exit `stretch=2:1,deadline=360`, or sell at a step on a +1.5% 1h bounce or breadth ≥ +15.
- `detached`: raw price action only (no breadth, volume, liquid filter or exit plan): all cached coins; buy when flat: the best 1h coin above +0.25% with a rising 15m, all cash; sell when 15m and 1h are both negative; rotate when another coin leads on 1h by more than 0.5 points.
- `high_variance`: liquid top 10; every step buys each coin not held that is up ≥ +0.5% over 1h, strongest first, up to 3 positions, sized clamp(|1h| / 2%, 0.25, 1.0) of equity capped by cash; exit `atr=6:1h,deadline=1440` only.
- `rotation`: shadow-runs the four base personas; at the first step of each UTC day follows the one with the best trailing-72h shadow return (v13 on ties and before 72h exist).
- `ensemble`: the four base personas vote on its own book; buys the coin with ≥ 2 buy votes (most votes, then 24h volume), all cash; sells on ≥ 2 sell votes or when `atr=3.5:1h,deadline=360` fires.

## Per-variant details

### `v13`

- Ending equity 816.01 USDT; 100 closed round trips (40 wins), 0 still open at the end.
- Exit reasons: deadline 30, follow_peak 8, regime_turn 62.
- Skipped orders: none.

### `contrarian`

- Ending equity 361.99 USDT; 499 closed round trips (289 wins), 0 still open at the end.
- Exit reasons: bounce 18, breadth_strength 80, deadline 18, stretch 383.
- Skipped orders: none.

### `detached`

- Ending equity 81.71 USDT; 1552 closed round trips (542 wins), 1 still open at the end.
- Exit reasons: open_at_end 1, price_turn 490, rotate 1062.
- Skipped orders: none.

### `high_variance`

- Ending equity 1193.82 USDT; 174 closed round trips (87 wins), 2 still open at the end.
- Exit reasons: deadline 145, follow_peak 29, open_at_end 2.
- Skipped orders: no_cash 354.

### `rotation`

- Ending equity 840.18 USDT; 436 closed round trips (186 wins), 0 still open at the end.
- Exit reasons: bounce 3, breadth_strength 7, deadline 35, follow_peak 10, price_turn 86, regime_turn 29, rotate 186, stretch 55, switch 25.
- Skipped orders: no_cash 787.
- switches: 24
- active steps: v13 1584, contrarian 504, detached 792, high_variance 1440
- 2026-08-23 00:03 UTC: v13 -> high_variance
- 2026-08-25 00:03 UTC: high_variance -> contrarian
- 2026-08-26 00:03 UTC: contrarian -> detached
- 2026-08-27 00:03 UTC: detached -> v13
- 2026-08-28 00:03 UTC: v13 -> high_variance
- 2026-08-29 00:03 UTC: high_variance -> contrarian
- 2026-08-31 00:03 UTC: contrarian -> v13
- 2026-09-03 00:03 UTC: v13 -> high_variance
- 2026-09-07 00:03 UTC: high_variance -> v13
- 2026-09-08 00:03 UTC: v13 -> detached
- 2026-09-09 00:03 UTC: detached -> v13
- 2026-09-10 00:03 UTC: v13 -> contrarian
- 2026-09-11 00:03 UTC: contrarian -> v13
- 2026-09-17 00:03 UTC: v13 -> high_variance
- 2026-09-24 00:03 UTC: high_variance -> v13
- 2026-09-25 00:03 UTC: v13 -> contrarian
- 2026-09-26 00:03 UTC: contrarian -> high_variance
- 2026-09-30 00:03 UTC: high_variance -> v13
- 2026-10-03 00:03 UTC: v13 -> detached
- 2026-10-06 00:03 UTC: detached -> high_variance
- 2026-10-08 00:03 UTC: high_variance -> v13
- 2026-10-09 00:03 UTC: v13 -> detached
- 2026-10-15 00:03 UTC: detached -> contrarian
- 2026-10-17 00:03 UTC: contrarian -> v13

### `ensemble`

- Ending equity 167.14 USDT; 623 closed round trips (217 wins), 1 still open at the end.
- Exit reasons: follow_peak 3, open_at_end 1, vote 620.
- Skipped orders: none.
- buy intents: 557 with 2 votes, 67 with 3 votes
- vote sells: 620

## Assumptions

- Decisions see only bars closed by the step time; intents fill at the open of the 1m bar that opens at the step, sells before buys.
- The 5 USDT minimum applies to buys only; a held position can always be sold.
- A buy whose exit plan cannot start (e.g. no hourly ATR yet) is skipped and counted (`exit_plan_unavailable`); a buy with no cash left is counted as `no_cash`.
- Equity, drawdown and positions still open at the end are marked at the last close without exit costs.
- At an exact 15-minute boundary, breadth uses the bucket that holds the last closed minute.
- The `stretch` exit counts break-even with fees only (as the live rule does), not slippage.
- Exit plans read each 1m bar low before high (the pessimistic order `bb bot-spot-exit-replay` uses); ATR comes from the closed 1h bars before entry and stretch/slowdown get 60 minutes of 1m warm-up.
- `rotation` scores its shadows at the first scheduled step of each new UTC day (00:03 in the default window, keeping the step cadence). A switch step only sells the old book; the new persona trades from the next step on a flat book.

## Caveats

- The `v13` row is a deterministic rule proxy of the v13/v14 guidance, not the LLM trader, which cannot be replayed (cost, quota, live-only tools).
- Survivorship: the liquid universe is the top 20 by volume as of 2026-10-18, applied to the whole window.
- Costs: 25 bps per round trip, against the measured 14.8 bps of the 40 live trips (deliberately pessimistic).
- The window ends before the Phase 47 (v13) deploy at 2026-10-18T14:02:04Z, so the `v13` row cannot be compared with live v13 trades on the same dates.
- No out-of-sample split: the rules were fixed in `LOOP_SCHIZOID.md` before any real-cache run and not tuned on it, but they are tested on this one period only.
