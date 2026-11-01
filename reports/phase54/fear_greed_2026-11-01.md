# Fear/greed signal `market.mood.tone`: scoreboard and backtests (2026-11-01)

Research only, offline, no orders. Operator 2026-11-01: "implement these" (the polarity-aware fear/greed signal and its retest; `docs/decisions.md`, "2026-11-01 fear/greed signal").

## The signal

`market.mood.tone.<1h|6h|24h|7d|30d>` = (posters of bullish moods − posters of bearish moods) / their sum over the live mood tags; bullish: bullish, euphoria, fomo, greed, hopium, optimism; bearish: bearish, capitulation, despair, doom, fear, panic; other moods left out; 0 without either (`crate::signals`). The old `market.mood.direction` counts every growing mood as +1 (fear and euphoria alike) and stayed between +0.73 and +1.00 all October.

## Scoreboard (`bb tag-scoreboard`, 751 hourly feeds 2026-10-01..11-01T06:00Z, fee 15 bps)

Market-wide names are measured against BTC and the liquid basket; a positive excess means the move went the signal's way (greed before rises).

| Name | Target | +1h t | +24h excess | +24h t (older / newer) |
|---|---|---:|---:|---:|
| `market.mood.tone.1h` | BTC | +0.37 | -0.031% | -0.33 (-0.53 / -0.06) |
| `market.mood.tone.1h` | basket | +0.31 | +0.020% | -0.44 (-0.88 / +0.53) |
| `market.mood.tone.6h` | BTC | +0.28 | -0.061% | -0.56 (-0.70 / -0.21) |
| `market.mood.tone.6h` | basket | -0.17 | -0.024% | -0.31 (-0.70 / +0.53) |
| `market.mood.tone.24h` | BTC | +0.05 | -0.173% | -0.70 (-1.35 / +0.10) |
| `market.mood.tone.24h` | basket | -0.32 | -0.215% | -0.65 (-1.29 / +0.68) |
| `market.mood.tone.7d` | BTC | +0.17 | -0.111% | -0.02 (-0.13 / +0.09) |
| `market.mood.tone.7d` | basket | +0.01 | -0.184% | -0.12 (-0.55 / +0.67) |
| `market.mood.direction.24h` | BTC | -0.27 | -0.037% | -0.08 (-0.08 / -0.06) |
| `market.mood.direction.24h` | basket | -0.08 | -0.058% | +0.13 (+0.53 / -0.63) |

No row reaches |t| 1.5; the contrarian sign at +24h (more fear before rises) holds in the older half only (t −1.35 against BTC for the 24h tone). The +7d rows have t 0 (fewer than 5 weekly blocks).

## Backtests on the live snap-back rule

Base: `snapback-2026-10-31` (`momentum.24h` −1, buy 0.8, sell −0.3). Frozen before the runs (`fear_greed_2026-11-01_grid.json`); the round-1 bar: net > 0 in both tuning halves (2026-10-01T14:00Z..10-13T14:00Z, ..10-25T14:00Z) and at least 10 round trips on the whole window; 25 bps per round trip. A market-wide weight adds weight × tone to every coin's score (the engine scores market names with the coin), and risk-off reads the same market score.

| Family | Rule | Older half | Newer half | Whole (round trips) | 10-26..11-01 | Bar |
|---|---|---:|---:|---:|---:|---|
| reference | live snap-back | +0.21% (1) | +7.81% (5) | +8.18% (6) | -2.96% (2) | fail |
| fade the fear | `fg-fade-24h-0.5`: `momentum.24h` -1, `market.mood.tone.24h` -0.5; buy 0.8, sell -0.3 | -13.56% (1) | +9.03% (4) | -8.27% (4) | -2.90% (2) | fail |
| fade the fear | `fg-fade-24h-1`: `momentum.24h` -1, `market.mood.tone.24h` -1; buy 0.8, sell -0.3 | -4.98% (1) | -0.87% (3) | -9.60% (1) | -0.60% (2) | fail |
| fade the fear | `fg-fade-7d-0.5`: `momentum.24h` -1, `market.mood.tone.7d` -0.5; buy 0.8, sell -0.3 | -12.62% (1) | +4.83% (4) | -10.85% (4) | -2.93% (2) | fail |
| fade the fear | `fg-fade-7d-1`: `momentum.24h` -1, `market.mood.tone.7d` -1; buy 0.8, sell -0.3 | -2.16% (0) | +53.54% (4) | -5.56% (0) | -0.60% (2) | fail |
| risk-off on greed | `fg-offgreed-24h-l-0.2`: `momentum.24h` -1, `market.mood.tone.24h` -1; buy 0.8, sell -0.3; risk-off below -0.2 | -4.98% (1) | -0.87% (3) | -9.60% (1) | -0.60% (2) | fail |
| risk-off on greed | `fg-offgreed-24h-l0`: `momentum.24h` -1, `market.mood.tone.24h` -1; buy 0.8, sell -0.3; risk-off below 0.0 | -6.38% (1) | -0.87% (3) | -10.93% (1) | -0.60% (2) | fail |
| risk-off on fear | `fg-offfear-24h-l-0.4`: `momentum.24h` -1, `market.mood.tone.24h` +1; buy 0.8, sell -0.3; risk-off below -0.4 | +0.00% (0) | +0.00% (0) | +0.00% (0) | -5.03% (1) | fail |
| risk-off on fear | `fg-offfear-24h-l-0.6`: `momentum.24h` -1, `market.mood.tone.24h` +1; buy 0.8, sell -0.3; risk-off below -0.6 | +0.00% (0) | +0.00% (0) | +0.00% (0) | -3.63% (1) | fail |

**No variant passes.** What happened:

- Fade the fear (negative tone weight): the board is fearful most of the time (tone about −0.4), so the term lifts every score by about 0.4. Coins clear the buy level early and rarely fall below the sell level: `fg-fade-24h-1` bought BTC on 10-03 and XRP on 10-07 and held XRP to the end (−112 USDT); `fg-fade-7d-1`'s +53.54% newer half is one T trade (+507 USDT), and over the whole window it held BTC from 10-03 (0 round trips).
- Risk-off on greed: tone above the level only at 135 of 1,728 steps; the score lift does the rest (same trades as fading the fear).
- Risk-off on fear: the +1 weight lowers every score by about 0.4, so no coin reaches the buy level: no trade in 24 days (fearful at 729 of 1,728 steps).

## Caveats

- One month of tags; the tone sets are a fixed list from the 2026-11-01 check (`signals::MOOD_BULLISH` / `MOOD_BEARISH`).
- To use tone as a pure timing gate (risk-off without shifting coin scores) the engine needs a market-only term; not built.
