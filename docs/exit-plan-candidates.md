# Exit-plan function candidates (Phase 45)

Research from 2026-10-15 on the math traders use to decide when to sell (and
some to decide when to buy). The menu Claude can pick from today is in
`crates/bb-trader/src/exit_plan.rs`; everything else here is a candidate to
add and backtest with `bb claude-spot-exit-replay` before it reaches the menu.

Status: **menu** = in the menu, **candidate** = not built yet.

## Stops and exits

| Function | Plain meaning | Status |
|---|---|---|
| Fixed % trailing stop | Sell if the price drops X% below its best point since you bought. | menu (`trail`) |
| ATR (Average True Range) | The coin's average size of one candle's move: a "normal wiggle" ruler that is bigger for wild coins. | menu (inside `atr`) |
| ATR trailing stop / Chandelier Exit | Hang the stop N wiggles below the highest price; it only moves up on new highs. Standard: highest high of 22 candles minus 3 x ATR(22). | menu (`atr`) |
| Wilder's Volatility Stop | Like the Chandelier, measured from the close; a bit tighter and quicker. | candidate |
| Parabolic SAR | A stop that creeps closer faster the longer the trend runs, so it eventually forces you out; locks profit sooner than the Chandelier. | candidate |
| Donchian channel exit | Sell when the price falls below the lowest price of the last N candles. | candidate |
| Keltner channel | Average price +/- a few wiggles; leaving the band means something unusual is happening. | candidate |
| Take-profit / partial take-profit | Sell all or part once up a set amount. | menu (`part`) |
| Time stop | Sell after a set time if nothing happened. | menu (`deadline`) |
| No-new-high fade | Sell when the coin stops making new highs for a while. | menu (`fade`) |

## Speed and derivatives

| Function | Plain meaning | Status |
|---|---|---|
| Rate of Change (first derivative, velocity) | How fast the price is moving; positive means rising. | menu (inside `slow`) |
| Acceleration (second derivative) | Whether that speed is growing or shrinking. "Still rising but slowing" is an early warning, best used to sell part or tighten, not to sell all; it is jumpy. | menu (`slow`) |
| Jerk (third derivative) | A sudden lurch in acceleration; very noisy, rarely useful. | candidate |
| MACD and its histogram | Gap between a fast and a slow average (smoothed speed); the histogram is roughly acceleration. Weak alone in crypto and slow as an exit. | candidate |
| Awesome / Accelerator Oscillator | Bill Williams' speed and acceleration meters. | candidate |
| RSI | 0-100 score of how one-sided recent moves were; above 70 "overheated". Selling at overheated missed the best stretches in strong trends. | candidate |
| Stochastic oscillator | Where the price sits in its recent high-low range, 0-100. | candidate |
| Moving-average crossover (SMA, EMA, variable-length MA) | Fast average crossing a slow one: "trend changed". Much better on daily than hourly charts. | candidate |

## Mean reversion and statistics

| Function | Plain meaning | Status |
|---|---|---|
| Z-score | How many normal wiggles the price is from its average; +2 is unusually high. | menu (inside `stretch`) |
| Bollinger Bands | Average +/- 2 standard deviations: the z-score drawn on a chart. | menu (`stretch`, 20 minutes) |
| VWAP (plain, rolling, anchored) | Average price weighted by volume: the "fair price" buyers paid. Needs trade volume, which the watcher's bid feed lacks. | candidate |
| Ornstein-Uhlenbeck process and half-life | A rubber-band model of a price pulled back to its average; the half-life is how long a stretch takes to shrink by half. | candidate |
| Hurst exponent | One number for a coin's personality: below 0.5 snaps back, above 0.5 keeps trending. | candidate |
| ADF test | Statistical yes/no on "does this price really return to an average?" | candidate |
| Autocorrelation / short-term reversal | Whatever just happened tends to partly undo itself next. | candidate (entry-side idea) |

## Smoothing and filtering

| Function | Plain meaning | Status |
|---|---|---|
| EMA smoothing | Average that weighs recent prices more, to cut noise. | menu (inside `slow`) |
| Kalman filter | Smart smoother that keeps estimating true price, speed and acceleration. Proven mainly for pairs trading; for one coin it behaves like an MA crossover. | candidate |
| Savitzky-Golay filter | Smoother that fits small curves; common versions peek at future prices in backtests. | candidate (avoid centered versions) |

## Decision theory and filters

| Function | Plain meaning | Status |
|---|---|---|
| Optimal stopping (Leung & Li) | For a rubber-band price, the single best take-profit and stop level including fees. | candidate |
| Trend filter (e.g. 200-period average) | Only allow some rules when the bigger picture agrees. | candidate |
| RSI above/below 50 as a regime filter | Same idea with RSI. | candidate |

## What the research says (2026-10-15)

- Moves shorter than an hour reverse in 90% of 183 Binance pairs, but the
  edge is too small to trade after costs (Kitron & Wengrowicz 2026, preprint,
  https://arxiv.org/abs/2608.21888). This fits Claude buying right after 5-15
  minute jumps and the price slipping afterwards (our inference).
- Short-term edges shrink after fees; daily rules hold up better than intraday
  (https://www.sciencedirect.com/science/article/abs/pii/S1059056024003010;
  https://stratbase.ai/en/blog/rsi-macd-combo-strategy;
  https://www.coinquant.ai/blog/building-a-mean-reversion-strategy-in-cryptocurrency-markets-evidence-from-78-backtests).
- Small coins tend to reverse, the biggest coins trend, over days to weeks
  (https://www.sciencedirect.com/science/article/pii/S1057521921002349).
- Wide or volatility-based stops beat tight ones in trends; the rule that
  wins most often makes the least money
  (https://setup4alpha.substack.com/p/trailing-stop-vs-fixed-stop-tested;
  https://stocksoftresearch.com/which-trailing-stop-is-the-best/). A signal
  exit paired with an ATR stop, whichever fires first, was the most consistent
  improvement (https://www.coinquant.ai/blog/macd-on-bitcoin-what-6-years-of-backtesting-actually-shows).
- Derivative exits need smoothing and are best for partial exits
  (https://medium.com/@blackcat1402.tradingview/exploring-the-secrets-of-the-market-derivatives-are-everywhere-f624b77b2c24).
- Optimal mean-reversion levels: Leung & Li, https://arxiv.org/abs/1411.5062.
- Many indicator sources are vendor or blog backtests; win-rate claims without
  a method (e.g. "77%") are not evidence.
