# Step-aside on the board's tones, 2026-11-02

Operator, 2026-11-02: "lets address these issues" (`docs/decisions.md`, "2026-11-02
tagging fixes"). After the fixes a market-wide name no longer shifts any coin's
score; it only makes the sleeve's market score, and `risk_off_below` moves the
sleeve to USDT while that score is under the line. This tests that switch on the
two crypto-scoped tones.

## Setup

- Base: the live `tagsnap-2026-11-01` (`momentum.24h` −1, `asset.direction.24h`
  0.5, buy 1.0, sell −0.3). Each variant adds one market name with weight ±1 and a
  line: the sleeve steps aside while weight × tone < line.
- Lines: the 20th or 35th percentile of the tone's hourly values over the tuning
  window (2026-10-01T14Z..10-25T14Z), fixed from the signal alone before any run.
  Values over those 576 hours: `market.mood.tone.24h` median −0.39 (p20 −0.53),
  `market.macro.tone.24h` median −0.58 (p20 −0.73); the 7d tones barely move
  (p20 to p80 within 0.06 for mood, 0.12 for macro).
- History: the tags-2 history rebuilt on the Pi 2026-11-02 (778 hourly files
  2026-10-01..11-02). Frozen grid: `step_aside_2026-11-02_grid.json`.
- Bar (round 1): net > 0 in both tuning halves (older: 12 days to 10-13T14Z,
  newer: 12 days to 10-25T14Z) and at least 10 round trips on the whole 24 days;
  25 bps per round trip. Judge window (6 days to 11-01T02Z) is information only.
- `bb algo-backtest --candidates liquid` (the bar's candidate set); `tags` shown
  as information. Runs: `~/bb-tuning/out/sa-results.json`.

## Result: no variant passes

Candidates `liquid`:

| rule | older net% | newer net% | whole net% (rt) | whole dd% | judge net% (rt) | bar |
|---|---|---|---|---|---|---|
| sa-control | −22.40 | −27.31 | −45.36 (8) | 48.44 | −0.39 (1) | no |
| sa-mood24-fear-p20 | −25.09 | −20.32 | −42.19 (13) | 47.78 | +2.15 (3) | no |
| sa-mood24-fear-p35 | −17.12 | −22.07 | −35.33 (13) | 39.84 | −5.17 (3) | no |
| sa-mood7d-fear-p20 | −17.35 | −26.92 | −39.52 (15) | 43.01 | −5.37 (3) | no |
| sa-mood24-greed-p80 | −25.42 | −27.63 | −47.23 (9) | 48.84 | −0.39 (1) | no |
| sa-macro24-down-p20 | −11.19 | −15.60 | −28.61 (14) | 35.45 | −0.86 (2) | no |
| sa-macro24-down-p35 | −15.54 | −16.65 | −35.31 (15) | 39.17 | −1.12 (3) | no |
| sa-macro7d-down-p20 | −23.85 | −27.11 | −47.98 (7) | 50.91 | −0.39 (1) | no |
| sa-macro24-up-p80 | −11.11 | −34.55 | −41.75 (9) | 48.99 | +0.60 (1) | no |

Candidates `tags` (information):

| rule | older net% | newer net% | whole net% (rt) | whole dd% | judge net% (rt) |
|---|---|---|---|---|---|
| sa-control | −2.65 | +2.83 | +1.81 (4) | 15.87 | −6.01 (2) |
| sa-mood24-fear-p20 | −4.28 | +1.16 | −1.52 (6) | 14.53 | +4.38 (5) |
| sa-mood24-fear-p35 | −9.45 | −6.17 | −14.96 (7) | 20.19 | −8.08 (5) |
| sa-mood7d-fear-p20 | −8.87 | +4.16 | −4.98 (8) | 17.02 | +1.23 (4) |
| sa-mood24-greed-p80 | −2.65 | +4.58 | +3.13 (6) | 11.13 | −1.98 (4) |
| sa-macro24-down-p20 | −1.42 | +8.37 | +7.27 (5) | 14.81 | −10.03 (4) |
| sa-macro24-down-p35 | −1.42 | −5.52 | −9.48 (6) | 14.45 | −8.92 (4) |
| sa-macro7d-down-p20 | −2.65 | −2.79 | −5.46 (4) | 15.87 | −6.01 (2) |
| sa-macro24-up-p80 | −6.90 | +3.91 | −3.17 (5) | 17.12 | +4.72 (3) |

## Reading

- The base rule loses heavily on `liquid` candidates in the tuning window; no
  switch turns that positive. Stepping aside when the crypto threads say macro
  forces hurt risk (`sa-macro24-down-p20`) cuts the loss most (−28.6% against
  −45.4%, drawdown 35% against 48%) and is the best `tags` row (+7.3% against
  +1.8%), but its older half stays negative and its `tags` judge window is the
  worst (−10.0%). One month, few round trips: not an edge.
- Stepping aside on fear helps a little at p20 and hurts at p35 on `tags`; on
  greed it does nothing measurable.
- The new tones go into the nightly scoreboard (scheduler image 2026-11-02) and
  into round 2 (around 11-08), judged only on data after 2026-11-01T02:00Z.
