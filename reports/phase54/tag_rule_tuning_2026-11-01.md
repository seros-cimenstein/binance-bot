# Tag-rule tuning, round 1 (2026-11-01)

Research only: `bb algo-backtest` offline on `data/entry-study-cache` (1m candles to 2026-11-01T02:00Z) and the Pi's tags history (`docker-data/tags-history`, 751 hourly feeds 2026-10-01..11-01), no orders. Operator scope and judging: `docs/decisions.md`, "2026-11-01 tag-rule tuning: scope and judging". The live snap-back rule is not tuned (operator).

## Fixed before the runs

- Windows: tuning 2026-10-01T14:00Z..10-25T14:00Z (older half to 10-13T14:00Z, newer half after); judging 2026-10-26T02:00Z..11-01T02:00Z (6 days).
- Costs: fee 7.5 + slippage 5 bps per side (25 bps per round trip); 1000 USDT start; a decision every 20 minutes; candidates as live (`--candidates tags`) unless stated.
- Pass bar: net > 0 in both tuning halves and at least 10 round trips on the whole tuning window. Finalist: the best whole-window net of a family among those that pass. A finalist is judged once against cash, holding BTC, holding the liquid 20 equally and the live snap-back rule on the judging window.
- Contrarian signals for "fade the board" from the point-in-time scoreboard as of 2026-10-25T14:00Z (591 feeds; tag direction signals with t ≤ −2 at the `assets` target): `narrative.direction.6h` (t −2.30 at +1h, −2.20 at +24h), `macro.direction.24h` (t −2.09 at +24h). Edge rules read daily point-in-time scoreboards (`bb tag-scoreboard --as-of <day>T00:30Z`, 2026-10-04..11-01), each used only from its `available_at`; scale 10 for the 1h horizon and 1 for 24h, so typical weights are near 1.
- The grid (36 rule files, all sleeve `fast` share 1, core share 0, 2 posters) is in `tag_rule_tuning_2026-11-01_grid.json`; every run in `tag_rule_tuning_2026-11-01.csv`.

## Result

**No rule file passes the bar, so there is no finalist.** The closest: "tags + snap-back" variants are positive in both halves but trade 4 times; the edge style's best is positive overall but −12.85% in the older half; every fade-the-board variant loses in both halves.

### fade

| Rule | Older half (round trips) | Newer half | Whole (round trips, drawdown) | Bar |
|---|---:|---:|---:|---|
| `fade-narr-b0.6-s-0.3`: `narrative.direction.6h` -1; buy 0.6, sell -0.3 | -0.67% (1) | -6.58% (6) | -4.12% (1, 6.6%) | fail |
| `fade-narr-b0.3-s-0.3`: `narrative.direction.6h` -1; buy 0.3, sell -0.3 | -0.72% (1) | -6.57% (6) | -4.17% (1, 6.6%) | fail |
| `fade-stance-b0.6-s-0.3`: `stance.6h` -1; buy 0.6, sell -0.3 | -10.42% (5) | -10.23% (10) | -14.69% (15, 19.0%) | fail |
| `fade-stance-b0.6-s0.0`: `stance.6h` -1; buy 0.6, sell 0.0 | -11.12% (7) | -9.98% (10) | -14.74% (17, 19.6%) | fail |
| `fade-narr-b0.3-s0.0`: `narrative.direction.6h` -1; buy 0.3, sell 0.0 | -7.49% (8) | -8.13% (7) | -14.90% (15, 19.0%) | fail |
| `fade-narr-b0.6-s0.0`: `narrative.direction.6h` -1; buy 0.6, sell 0.0 | -9.48% (7) | -7.66% (7) | -16.42% (14, 20.2%) | fail |
| `fade-stance-b0.3-s-0.3`: `stance.6h` -1; buy 0.3, sell -0.3 | -13.34% (10) | -10.20% (13) | -16.94% (22, 21.3%) | fail |
| `fade-3-b0.5-s-0.3`: `narrative.direction.6h` -1, `macro.direction.24h` -1, `stance.6h` -1; buy 0.5, sell -0.3 | -11.98% (13) | -11.15% (6) | -18.25% (18, 20.4%) | fail |
| `fade-3-b1.0-s-0.3`: `narrative.direction.6h` -1, `macro.direction.24h` -1, `stance.6h` -1; buy 1.0, sell -0.3 | -9.62% (7) | -8.88% (5) | -18.75% (11, 20.2%) | fail |
| `fade-stance-b0.3-s0.0`: `stance.6h` -1; buy 0.3, sell 0.0 | -15.29% (11) | -12.10% (8) | -20.43% (18, 23.1%) | fail |
| `fade-3-b1.0-s0.0`: `narrative.direction.6h` -1, `macro.direction.24h` -1, `stance.6h` -1; buy 1.0, sell 0.0 | -13.77% (10) | -9.29% (5) | -22.00% (16, 23.4%) | fail |
| `fade-3-b0.5-s0.0`: `narrative.direction.6h` -1, `macro.direction.24h` -1, `stance.6h` -1; buy 0.5, sell 0.0 | -11.80% (15) | -11.61% (8) | -22.26% (24, 23.6%) | fail |

### tags+snap

| Rule | Older half (round trips) | Newer half | Whole (round trips, drawdown) | Bar |
|---|---:|---:|---:|---|
| `snap+asset.direction.24h+0.5-b1.0`: `momentum.24h` -1, `asset.direction.24h` +0.5; buy 1.0, sell -0.3 | +6.89% (1) | +1.56% (2) | +8.50% (4, 12.9%) | fail |
| `snap+stance.6h+0.5-b1.0`: `momentum.24h` -1, `stance.6h` +0.5; buy 1.0, sell -0.3 | +4.28% (1) | -1.14% (3) | +5.25% (4, 14.5%) | fail |
| `snap+stance.6h-0.5-b1.0`: `momentum.24h` -1, `stance.6h` -0.5; buy 1.0, sell -0.3 | +1.17% (1) | +2.56% (3) | +3.88% (4, 8.4%) | fail |
| `snap+narrative.direction.6h-0.5-b1.0`: `momentum.24h` -1, `narrative.direction.6h` -0.5; buy 1.0, sell -0.3 | -11.07% (1) | +20.86% (4) | +2.11% (4, 21.8%) | fail |
| `snap+stance.6h-0.5-b0.8`: `momentum.24h` -1, `stance.6h` -0.5; buy 0.8, sell -0.3 | -6.43% (1) | +3.64% (4) | -2.90% (5, 11.3%) | fail |
| `snap+stance.6h+0.5-b0.8`: `momentum.24h` -1, `stance.6h` +0.5; buy 0.8, sell -0.3 | -14.98% (1) | +11.30% (7) | -5.27% (7, 21.8%) | fail |
| `snap+asset.direction.24h+0.5-b0.8`: `momentum.24h` -1, `asset.direction.24h` +0.5; buy 0.8, sell -0.3 | -11.70% (1) | +5.37% (3) | -6.88% (4, 21.8%) | fail |
| `snap+narrative.direction.6h-0.5-b0.8`: `momentum.24h` -1, `narrative.direction.6h` -0.5; buy 0.8, sell -0.3 | -11.73% (1) | +6.38% (6) | -9.26% (6, 21.8%) | fail |

### edge

| Rule | Older half (round trips) | Newer half | Whole (round trips, drawdown) | Bar |
|---|---:|---:|---:|---|
| `edge-1h-t2.0-b0.3`: edge, horizon 1h, scale 10, cap 1, min t 2; buy 0.3, sell -0.3 | -12.85% (9) | +16.21% (1) | +4.95% (11, 18.8%) | fail |
| `edge-24h-t3.0-b0.6`: edge, horizon 24h, scale 1, cap 1, min t 3; buy 0.6, sell -0.3 | -2.12% (1) | +0.00% (0) | +3.29% (1, 17.1%) | fail |
| `edge-1h-t3.0-b0.3`: edge, horizon 1h, scale 10, cap 1, min t 3; buy 0.3, sell -0.3 | -0.46% (0) | +0.00% (0) | -4.48% (0, 5.3%) | fail |
| `edge-1h-t3.0-b0.6`: edge, horizon 1h, scale 10, cap 1, min t 3; buy 0.6, sell -0.3 | -0.46% (0) | +0.00% (0) | -4.48% (0, 5.3%) | fail |
| `edge-24h-t2.0-b0.6`: edge, horizon 24h, scale 1, cap 1, min t 2; buy 0.6, sell -0.3 | -0.02% (0) | -5.57% (4) | -5.70% (5, 10.8%) | fail |
| `edge-24h-t2.0-b0.3`: edge, horizon 24h, scale 1, cap 1, min t 2; buy 0.3, sell -0.3 | -0.02% (0) | -7.93% (4) | -8.06% (5, 13.0%) | fail |
| `edge-1h-t2.0-b0.6`: edge, horizon 1h, scale 10, cap 1, min t 2; buy 0.6, sell -0.3 | -9.77% (5) | -0.13% (0) | -9.89% (5, 12.1%) | fail |
| `edge-24h-t3.0-b0.3`: edge, horizon 24h, scale 1, cap 1, min t 3; buy 0.3, sell -0.3 | -14.50% (0) | -11.82% (0) | -20.26% (1, 22.5%) | fail |

### risk-off

| Rule | Older half (round trips) | Newer half | Whole (round trips, drawdown) | Bar |
|---|---:|---:|---:|---|
| `riskoff-mood-1-l-0.3`: `stance.6h` +1, `momentum.1h` +1, `market.mood.direction.24h` -1; buy 0.5, sell -0.5; risk-off below -0.3 | +0.31% (1) | +0.00% (0) | +0.31% (1, 0.3%) | fail |
| `riskoff-mood-1-l0.0`: `stance.6h` +1, `momentum.1h` +1, `market.mood.direction.24h` -1; buy 0.5, sell -0.5; risk-off below 0.0 | +0.31% (1) | +0.00% (0) | +0.31% (1, 0.3%) | fail |
| `riskoff-macro-1-l-0.3`: `stance.6h` +1, `momentum.1h` +1, `market.macro.direction.24h` -1; buy 0.5, sell -0.5; risk-off below -0.3 | +0.24% (1) | +0.00% (0) | +0.24% (1, 2.3%) | fail |
| `riskoff-macro-1-l0.0`: `stance.6h` +1, `momentum.1h` +1, `market.macro.direction.24h` -1; buy 0.5, sell -0.5; risk-off below 0.0 | +0.24% (1) | +0.00% (0) | +0.24% (1, 2.3%) | fail |
| `riskoff-mood+1-l-0.3`: `stance.6h` +1, `momentum.1h` +1, `market.mood.direction.24h` +1; buy 0.5, sell -0.5; risk-off below -0.3 | -2.16% (0) | -13.73% (0) | -5.56% (0, 8.9%) | fail |
| `riskoff-mood+1-l0.0`: `stance.6h` +1, `momentum.1h` +1, `market.mood.direction.24h` +1; buy 0.5, sell -0.5; risk-off below 0.0 | -2.16% (0) | -13.73% (0) | -5.56% (0, 8.9%) | fail |
| `riskoff-macro+1-l-0.3`: `stance.6h` +1, `momentum.1h` +1, `market.macro.direction.24h` +1; buy 0.5, sell -0.5; risk-off below -0.3 | -2.16% (0) | -13.73% (0) | -5.56% (0, 8.9%) | fail |
| `riskoff-macro+1-l0.0`: `stance.6h` +1, `momentum.1h` +1, `market.macro.direction.24h` +1; buy 0.5, sell -0.5; risk-off below 0.0 | -2.16% (0) | -13.73% (0) | -5.56% (0, 8.9%) | fail |

With weight −1 the market score is below the level at 1,575 of 1,728 steps (mood, whole window), so the sleeve stays in USDT (one round trip in 24 days); with +1 it is never risk-off and the market term lifts every score, so it buys BTC on 10-03 and holds it.

Reference on the same windows (older / newer / whole): the tag rule `algo-2026-10-29` -14.06% (6) / -8.13% (9) / -21.05% (15); the live snap-back +0.21% (1) / +7.81% (5) / +8.18% (6); holding BTC -0.91% / -3.59% / -4.35%; holding the liquid 20 equally +1.26% / -9.89% / -11.01% (entry cost only, marked at the last close).

## Candidate set: tag pick against the liquid 20 (`--candidates liquid`)

| Rule | Tags: older / newer / whole | Liquid 20: older / newer / whole |
|---|---:|---:|
| tag rule `algo-2026-10-29` | -14.06% (6) / -8.13% (9) / -21.05% (15) | +107.30% (12) / -8.65% (9) / +92.23% (22) |
| live snap-back `snapback-2026-10-31` | +0.21% (1) / +7.81% (5) / +8.18% (6) | -17.86% (5) / -28.25% (4) / -42.92% (9) |
| `fade-narr-b0.6-s-0.3` | -0.67% (1) / -6.58% (6) / -4.12% (1) | +0.02% (0) / -3.92% (4) / -3.46% (0) |
| `snap+asset.direction.24h+0.5-b1.0` | +6.89% (1) / +1.56% (2) / +8.50% (4) | -22.40% (4) / -27.31% (4) / -45.36% (8) |
| `edge-1h-t2.0-b0.3` | -12.85% (9) / +16.21% (1) / +4.95% (11) | -8.81% (7) / -16.59% (8) / -23.85% (15) |
| `riskoff-mood-1-l-0.3` | +0.31% (1) / +0.00% (0) / +0.31% (1) | +43.88% (8) / +0.00% (0) / +43.88% (8) |

The liquid 20 is the 2026-10-18 top 20 by volume, picked after early-October rallies (SAND +40% on 10-01/02 and STRK +26% on 10-09 carry the tag rule's +107.30% older half), so its older half is flattered; no liquid run passes the bar either. For the snap-back rule the tag pick is far better (+8.18% against −42.92% over the whole window).

## Judging window (information only: no finalist)

| | 2026-10-26T02:00Z..11-01T02:00Z | Trades (net) |
|---|---:|---|
| Cash | 0.00% | |
| Hold BTC | -2.66% | |
| Hold the liquid 20 equally | -2.60% | |
| Live snap-back rule | -2.96% | T +41 USDT, NEAR -9 USDT, TRX -62 USDT (open) |
| Tag rule `algo-2026-10-29` | +15.73% | HYPE +157 USDT (open) |
| Best fade: `fade-narr-b0.6-s-0.3` | +11.19% | PEPE -9 USDT, PEPE +130 USDT, HYPE -9 USDT (open) |
| Best tags+snap: `snap+asset.direction.24h+0.5-b1.0` | -6.01% | T +41 USDT, CVX -63 USDT, TRX -39 USDT (open) |
| Best edge: `edge-1h-t2.0-b0.3` | +4.15% | STX +41 USDT (open) |
| Best risk-off: `riskoff-mood-1-l-0.3` | +0.00% | none |

The tag rules' gains here come from one or two holdings each (HYPE held all 6 days; one PEPE trade): not evidence of an edge, and none of these rules passed the tuning bar.

## Caveats

- One month of tags; 6 judging days; 0 to 24 round trips per rule. Thin either way.
- The five families were chosen after seeing the 2026-10-31 and 11-01 scoreboards, which cover the judging window; the signals and weights inside them come only from data available by 2026-10-25.
- Survivorship: candidates are limited to cached coins; the liquid 20 was picked on 2026-10-18.
- Costs 25 bps per round trip against about 15 bps measured live (pessimistic).
