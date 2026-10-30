# Algorithm backtest (2026-10-01T14:00:00Z → 2026-10-25T14:00:00Z)

Research only: `bb algo-backtest`, offline on `data/entry-study-cache` (81 coins), no network, no orders.

## Settings

- Window: 2026-10-01T14:00:00Z → 2026-10-25T14:00:00Z (24 days), a decision step every 20 minutes (864 steps).
- Each half starts with 1000.00 USDT. Costs per side: fee 7.5 bps + slippage 5 bps (25 bps per round trip).
- Liquid universe (20 coins): BTC, ETH, ZEC, SOL, NEAR, XRP, BNB, WLD, HYPE, DOGE, SUI, SAND, TRX, UNI, ENA, STRK, TAO, HBAR, TIA, LINK.
- Split: the older half 2026-10-01T14:00:00Z → 2026-10-13T14:00:00Z tunes the rule, the newer half 2026-10-13T14:00:00Z → 2026-10-25T14:00:00Z judges it (`biz_algo@older`, `biz_algo@newer`); the step count above is the older half's.
- /biz/ feeds: `/home/user/projects/automation/imageboards/biz-app/biz_insights/out/biz-feed-history`, 590 feeds from 2026-10-01T00:00:00Z to 2026-10-25T13:00:00Z; AI readers google-antigravity/gemini-3.8-flash@stance-1.

## Results

| Variant | Net USDT | Net % | Max drawdown % | Round trips | Win rate % | Sharpe | Fees USDT | Skipped buys/sells |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| biz_algo@older | 184.19 | 18.42 | 11.46 | 12 | 66.7 | 5.449 | 19.10 | 0 |
| biz_algo@newer | -111.82 | -11.18 | 13.00 | 11 | 18.2 | -5.529 | 16.32 | 0 |

Net = ending equity − starting balance after every fee and slippage; positions still open at the end are marked at their last close. Win rate counts closed round trips with a positive net. Sharpe = mean / population std of step-equity returns × √(525600 / step minutes). Full numbers: the `.csv`; every trade: `_trades.csv`.

## Rule

- `biz_algo`: the trader's algorithm, no LLM: scores each liquid-top coin as w_talk·talk + w_stance·stance + w_momentum·momentum (talk = the /biz/ 6h mention rate over its 24h rate − 1; stance = (bullish − bearish) / tagged of the 6h AI tags with enough posters; momentum = 1h return / 5%, capped); a held coin scoring below the sell level is sold, and with nothing held the best coin is bought with all cash at or above the buy level; no exit plan.

## Per-variant details

### `biz_algo@older`

- Ending equity 1184.19 USDT; 12 closed round trips (8 wins), 0 still open at the end.
- Exit reasons: biz_algo 12.
- Skipped orders: none.
- rule: talk +0, stance +1, momentum +1; buy at score ≥ +0.5, sell below -0.5; stances need 2 posters; /biz/ feed visible at 864 of 864 steps
- Per-step trace: `algo_backtest_2026-10-25_biz_algo@older_trace.csv` (864 rows, one per step: `time,feed_as_of,best_coin,best_score,held_coin,held_score,action`).

### `biz_algo@newer`

- Ending equity 888.18 USDT; 11 closed round trips (2 wins), 1 still open at the end.
- Exit reasons: biz_algo 11, open_at_end 1.
- Skipped orders: none.
- rule: talk +0, stance +1, momentum +1; buy at score ≥ +0.5, sell below -0.5; stances need 2 posters; /biz/ feed visible at 864 of 864 steps
- Per-step trace: `algo_backtest_2026-10-25_biz_algo@newer_trace.csv` (864 rows, one per step: `time,feed_as_of,best_coin,best_score,held_coin,held_score,action`).

## `biz_algo` rule

- Rule: talk +0, stance +1, momentum +1; buy at score ≥ +0.5, sell below -0.5; stances need 2 distinct posters in the 6h window.
- Picked on the older half from 72 weight sets by net %; judge it by `biz_algo@newer` (the `@older` row is in-sample). Best 5 on the older half:

| Weights | Net % | Round trips | Max drawdown % |
|---|---:|---:|---:|
| talk +0, stance +1, momentum +1; buy at score ≥ +0.5, sell below -0.5 | 18.42 | 12 | 11.46 |
| talk -1, stance +0, momentum +1; buy at score ≥ +0.5, sell below -0.5 | 9.57 | 16 | 14.37 |
| talk -1, stance -1, momentum +1; buy at score ≥ +0.5, sell below -0.5 | 4.56 | 16 | 15.57 |
| talk -1, stance +1, momentum +1; buy at score ≥ +0.5, sell below -0.5 | 3.79 | 18 | 18.15 |
| talk +0, stance +0, momentum +0; buy at score ≥ +0.5, sell below -0.5 | 0.00 | 0 | 0.00 |

## Assumptions

- Decisions see only bars closed by the step time; intents fill at the open of the 1m bar that opens at the step, sells before buys.
- The 5 USDT minimum applies to buys only; a held position can always be sold.
- A buy with no cash left is counted as `no_cash`.
- Equity, drawdown and positions still open at the end are marked at the last close without exit costs.

## Caveats

- Survivorship: the liquid universe is the top 20 by volume as of 2026-10-18, applied to the whole window.
- Costs: 25 bps per round trip, against the measured 14.8 bps of the 40 live trips (deliberately pessimistic).
- Out of sample: when the weights were picked on the older half, the `@newer` row is the honest result. Each half is about half the window: few trades, a small sample.
