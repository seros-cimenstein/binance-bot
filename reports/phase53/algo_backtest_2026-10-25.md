# Algorithm backtest: `algo-2026-10-29` (2026-10-01T14:00:00Z → 2026-10-25T14:00:00Z)

Research only: `bb algo-backtest`, offline on `data/entry-study-cache` (88 coins), no network, no orders.

## Settings

- Window: 2026-10-01T14:00:00Z → 2026-10-25T14:00:00Z (24 days), a decision step every 20 minutes (1728 steps).
- Starts with 1000.00 USDT. Costs per side: fee 7.5 bps + slippage 5 bps (25 bps per round trip).
- Rule file `config/rules/algo-2026-10-29.json`: `rules_version` algo-2026-10-29, style fixed, sha256 `79a329690c03e154d02e3625ba0d3a30475e07a645b8857afd920dfeba61cec9`.
  - `fast`: share 1, horizon any, defensive USDT, risk-off none; fast: talk.6h +0, stance.6h +1, momentum.1h +1; buy at score ≥ +0.5, sell below -0.5.
  - `core`: share 0, horizon any, defensive USDT, risk-off none; core: no weights; buy at score ≥ +0.5, sell below -0.5.
- Tags feeds: `/home/user/biz-tags-retag/tags-history`, 578 feeds from 2026-10-01T13:00:00Z to 2026-10-25T14:00:00Z; AI readers google-antigravity/gemini-3.8-flash@tags-1; proxies `config/proxies.json`.

## Results

| Scope | Net USDT | Net % | Max drawdown % | Round trips | Win rate % | Sharpe | Fees USDT | Open at end | Skipped orders |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| portfolio | -210.51 | -21.05 | 25.88 | 15 | 40.0 | -4.921 | 20.00 | 1 | 0 |
| sleeve `fast` | -210.51 | -21.05 | — | 15 | 40.0 | — | 20.00 | 1 | 0 |

Net = ending equity − starting balance after every fee and slippage; positions still open at the end are marked at their last close. A sleeve's net is the sum of its trades (its % is of the starting balance); the sleeves share the USDT, so drawdown and Sharpe are the portfolio's only. Win rate counts closed round trips with a positive net. Sharpe = mean / population std of step-equity returns × √(525600 / step minutes). Full numbers: the `.csv`; every trade: `_trades.csv`.

## Details

- Ending equity 789.49 USDT; 15 closed round trips (6 wins), 1 still open at the end.
- Skipped orders: none.
- tags feed visible at 1728 of 1728 steps; 16.7 candidates per step on average
- scored-only asset tags (no cached market, no proxy; never bought), 169 distinct, most steps: USD 1398, SILVER 1374, BBBYQ 1326, SPY 1323, GME 1317, SPX 1251, OIL 1242, BONDS 1224, XMR 1221, CRYPTO 1218
- sleeve `fast` (fast: talk.6h +0, stance.6h +1, momentum.1h +1; buy at score ≥ +0.5, sell below -0.5): decided at 1728 steps
- sleeve `core` (core: no weights; buy at score ≥ +0.5, sell below -0.5): decided at 0 steps (no share and nothing held: never active)
- Sleeve `fast` per-step trace: `algo_backtest_2026-10-25_fast_trace.csv` (1728 rows, one per step it decided: `time,feed_as_of,candidates,scoreboard,best_coin,best_score,held_coin,held_score,market_score,risk_off,action`).

## Assumptions

- Decisions see only bars closed by the step time and the newest tags feed with `as_of` at or before it (none when older than the age limit); intents fill at the open of the 1m bar that opens at the step, sells before buys.
- Candidates, as live: the cached coins the visible feed's live asset tags reach, directly or through the proxy file; held coins are always scored. A coin without a cached market is scored-only and never bought.
- Sleeves: each sleeve with a share or a holding decides on its own holdings; a sell sells exactly its quantity, a buy spends its share of the portfolio value at the step (cash plus every sleeve's holdings), capped by the cash left.
- The 5 USDT minimum applies to buys only; a held position can always be sold.
- A buy with no cash left is counted as `no_cash`.
- Equity, drawdown and positions still open at the end are marked at the last close without exit costs.

## Caveats

- Survivorship: the candidates are limited to the coins in the candle cache; the scored-only note above lists the live asset tags that had none.
- Costs: 25 bps per round trip, against the measured 14.8 bps of the 40 live trips (deliberately pessimistic).
- No tuning: the rule file is run as it is; a rule picked after looking at this window is in-sample here.
