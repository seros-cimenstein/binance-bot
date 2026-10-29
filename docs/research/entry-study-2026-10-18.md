# Entry study, 2026-10-18 (LOOP_MONEY C2-C4)

Question: does any simple entry on Binance's liquid USDT coins make money after costs over
the bot's horizons (20 min to 2 h)? Research only: public 1m candles, no order path.

- Tool: `bb entry-study` (`crates/bb-strategies/src/entry_study.rs`, commit 3fbde04).
  Command: `bb entry-study --days 90 --top 20 --audit <Pi audit mirror>`.
- Data: 2026-07-20T09:00 to 2026-10-18T08:43 UTC, a sample every 20 min × 20 coins
  (BTC, ETH, ZEC, SOL, NEAR, XRP, BNB, WLD, HYPE, DOGE, SUI, SAND, TRX, UNI, ENA, STRK, TAO, HBAR, TIA, LINK), 124,770 samples.
  The universe is ranked by today's volume, so it carries survivorship bias, which favours buying.
- Train: the first 60 days. Test: the last 30 (from 2026-09-18T08:43).
- Costs: 15 bps per round trip (what the bot really paid with the BNB discount) plus half the
  entry minute's high-low range (1-10 bps). Mean cost about 0.21%.
- t is the plain t-statistic; (t_c) divides it by the square root of the overlap between
  windows (horizon / 20 min), the honest one for overlapping forward returns.

## C2. The harness matches reality

The bot's 40 real entries (from the audit), measured with the harness's candles; gross
returns against the fill price:

| | 30m before | +20m | +60m | +90m | +120m |
|---|---:|---:|---:|---:|---:|
| harness (`bb entry-study --audit`) | +1.202% | -0.128% | -0.161% | -0.206% | -0.209% |
| independent Python script (B6), same definitions | +1.202% | -0.128% | | -0.206% | |
| bot record shown in the prompt (B6, Pi) | +1.20% | -0.13% | | -0.21% | |
| first quick measure (07:55, next-minute open as entry price) | +1.154% | -0.154% | -0.078% | -0.165% | -0.197% |

The harness reproduces the same-definition numbers exactly. The first quick measure used
the next minute's open instead of the fill; it differs by at most 0.05 percentage points at
+20/+90/+120 and on the run-up, as required.

## C3. Net forward return by bucket (train | test)

Columns per period: n | mean net | median net | t (t_c).

| family | bucket | horizon | train n | mean | median | t | test n | mean | median | t |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| all | all | 90m | 82080 | -0.158% | -0.172% | -45.7 (-21.5) | 42690 | -0.187% | -0.195% | -26.8 (-12.6) |
| run-up 30m | <= -1% | 90m | 2491 | -0.074% | -0.100% | -2.4 (-1.2) | 2611 | -0.064% | -0.100% | -1.6 (-0.7) |
| run-up 30m | -1..-0.25% | 90m | 15410 | -0.151% | -0.148% | -17.8 (-8.4) | 8826 | -0.220% | -0.213% | -15.0 (-7.1) |
| run-up 30m | -0.25..+0.25% | 90m | 45882 | -0.160% | -0.165% | -45.5 (-21.5) | 19437 | -0.197% | -0.180% | -27.3 (-12.9) |
| run-up 30m | +0.25..+1% | 90m | 15279 | -0.170% | -0.219% | -19.0 (-8.9) | 9000 | -0.202% | -0.243% | -12.9 (-6.1) |
| run-up 30m | >= +1% | 90m | 3018 | -0.163% | -0.313% | -4.4 (-2.1) | 2816 | -0.074% | -0.369% | -1.4 (-0.7) |
| trend 1h | up | 90m | 40663 | -0.170% | -0.196% | -32.1 (-15.1) | 20955 | -0.187% | -0.218% | -17.5 (-8.3) |
| trend 1h | down | 90m | 41417 | -0.146% | -0.150% | -32.8 (-15.4) | 21735 | -0.187% | -0.179% | -20.6 (-9.7) |
| trend 4h | up | 90m | 41250 | -0.148% | -0.188% | -27.3 (-12.9) | 21146 | -0.167% | -0.214% | -15.4 (-7.3) |
| trend 4h | down | 90m | 40830 | -0.167% | -0.160% | -39.2 (-18.5) | 21544 | -0.206% | -0.180% | -23.4 (-11.0) |
| breadth 1h | < 1/3 up | 90m | 30438 | -0.128% | -0.135% | -24.1 (-11.3) | 15521 | -0.156% | -0.155% | -13.6 (-6.4) |
| breadth 1h | 1/3..2/3 up | 90m | 21888 | -0.179% | -0.181% | -30.2 (-14.3) | 12413 | -0.191% | -0.203% | -14.8 (-7.0) |
| breadth 1h | > 2/3 up | 90m | 29754 | -0.172% | -0.201% | -26.5 (-12.5) | 14756 | -0.215% | -0.229% | -18.0 (-8.5) |
| volume 30m vs 24h | < 0.75x | 90m | 40829 | -0.168% | -0.168% | -40.3 (-19.0) | 21274 | -0.170% | -0.181% | -19.8 (-9.3) |
| volume 30m vs 24h | 0.75..1.5x | 90m | 26952 | -0.156% | -0.175% | -25.3 (-11.9) | 14111 | -0.221% | -0.205% | -18.8 (-8.9) |
| volume 30m vs 24h | >= 1.5x | 90m | 14299 | -0.130% | -0.175% | -12.1 (-5.7) | 7303 | -0.167% | -0.253% | -7.4 (-3.5) |
| run-up >= +1% and 1h up | yes | 90m | 2799 | -0.175% | -0.334% | -4.5 (-2.1) | 2559 | -0.068% | -0.376% | -1.3 (-0.6) |
| dip: 30m <= -1% and 4h up | yes | 90m | 778 | -0.107% | -0.154% | -1.7 (-0.8) | 780 | +0.050% | -0.150% | +0.5 (+0.2) |

Every horizon, the whole universe:

| horizon | train gross | train net | test gross | test net |
|---:|---:|---:|---:|---:|
| 20m | +0.008% | -0.187% | +0.007% | -0.203% |
| 60m | +0.025% | -0.170% | +0.020% | -0.189% |
| 90m | +0.038% | -0.158% | +0.023% | -0.187% |
| 120m | +0.050% | -0.145% | +0.039% | -0.170% |

The five best test buckets at any horizon:

| family | bucket | horizon | n | mean net | t (t_c) | same bucket in train |
|---|---|---:|---:|---:|---:|---:|
| dip: 30m <= -1% and 4h up | yes | 120m | 780 | +0.174% | +1.6 (+0.7) | -0.064% |
| dip: 30m <= -1% and 4h up | yes | 90m | 780 | +0.050% | +0.5 (+0.2) | -0.107% |
| run-up 30m | >= +1% | 120m | 2816 | -0.020% | -0.3 (-0.1) | -0.177% |
| run-up >= +1% and 1h up | yes | 120m | 2559 | -0.021% | -0.3 (-0.1) | -0.172% |
| run-up 30m | <= -1% | 120m | 2611 | -0.025% | -0.5 (-0.2) | -0.043% |

## C3 findings

1. **Buying after a >= +1% 30-min run-up loses after costs.** Mean net at +90 min was
   -0.163% in train and -0.074% in test, and the median was
   -0.369% in test (most of these trades lose; a few large winners lift the mean).
   The bot's own variant (run-up and the hour up) gives the same picture.
2. **No bucket has a positive net mean with n >= 200 and t > 2 out of sample.** No bucket is
   positive in both train and test at any horizon. The best test bucket (a 1% dip in a 4h
   uptrend, +120 min, +0.174%, n=780, t +1.6) was negative in train
   (-0.064%) and is not significant.
3. **The cost is the wall.** A random entry gains +0.02 to +0.05% gross over 20-120 min, and
   the round trip costs about 0.21%. Every family loses 0.15-0.22% net per trip, train and
   test alike. Gross edges this small do not survive a 15-bps round trip at these horizons.
4. **The bot picked worse than random, but not significantly.** Its 40 entries went
   -0.206% gross at +90 min (sd 1.48%) against the universe's +0.023%:
   -0.229 percentage points, t -1.0. Even a perfectly average picker would have lost about 0.19% per
   trip after costs.

## C4. Dip buying: `bb mean-reversion-backtest` sweep

(The command moved to `../redundancy/binance-bot-modules/` with the 2026-10-29 prune.)

Same 20 coins and windows (train 2026-07-20 09:00 to 2026-09-18 08:43, test to 2026-10-18 08:43),
1m candles, fee 7.5 bps per side plus 5 bps slippage, 100 USDT per position, at most 5 open.
Default exits: take profit at a 50% retrace of the dump, stop at a further -3%, or 60 minutes.
"Per trade" is the mean net P&L in % of the position; t is over trades.

| dump | lookback | train trades | train net % (wallet) | per trade | t | Sharpe | win % | verdict | test trades | test net % (wallet) | per trade | t | Sharpe | win % | verdict |
|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|---|
| -1% | 15m | 2595 | -47.6 | -0.184% | -9.2 | -14.4 | 67 | no_edge | 3018 | -58.2 | -0.193% | -9.4 | -22.9 | 71 | no_edge |
| -2% | 15m | 405 | -0.7 | -0.018% | -0.2 | -0.3 | 65 | no_edge | 773 | -1.8 | -0.023% | -0.4 | -0.9 | 66 | no_edge |
| -3% | 15m | 129 | +3.0 | +0.236% | +1.2 | +2.0 | 68 | edge_likely | 252 | +0.8 | +0.033% | +0.3 | +0.8 | 62 | edge_marginal |
| -1% | 30m | 3059 | -61.0 | -0.200% | -11.7 | -17.8 | 62 | no_edge | 3124 | -63.3 | -0.203% | -10.4 | -24.1 | 67 | no_edge |
| -2% | 30m | 702 | -5.7 | -0.082% | -1.5 | -2.3 | 58 | no_edge | 1008 | -14.4 | -0.143% | -2.9 | -7.1 | 61 | no_edge |
| -3% | 30m | 206 | +5.5 | +0.269% | +1.9 | +3.3 | 65 | edge_likely | 396 | -4.2 | -0.107% | -1.1 | -2.8 | 54 | no_edge |

C4 findings: buying small dumps (1-2%) loses after costs in both periods: -0.18% to -0.20% per
trade at 1%, t around -9 to -12, and win rates of 60-70% that do not cover the losers and fees.
Only the deepest dump (-3%) was positive in train: +0.24%/trade over 15m (n=129, t 1.2) and +0.27%
over 30m (n=206, t 1.9). Neither held up out of sample: +0.03%/trade over 15m (n=252, t 0.3) and
-0.11% over 30m (n=396, t -1.1). No threshold reaches n >= 200 with t > 2 in test. The harness's
own verdict calls the -3%/15m test `edge_marginal`, but that is +0.8% on the wallet over 30 days,
inside noise.

## E1. Verdict: no edge found

No entry tested here pays after costs out of sample. None of the C3 entry buckets and none of
the C4 dip thresholds has a positive test mean with n >= 200 and t > 2. No C3 bucket is
positive in both train and test. The only bucket that looks positive in test (a 1% dip
in a 4h uptrend, +120 min: +0.17%, n=780, t 1.6, t_c 0.7) lost in train, and the -3% dip rule
that won in train (+0.24 to +0.27% per trade) went flat or negative in test. On these coins and
horizons, a random entry earns about +0.02 to +0.05% gross against a round-trip cost of about
0.21%.

Wallet math (17.8 USDT; about 15 bps of fees per round trip, plus spread):

- Real record: 40 round trips, -1.074 USDT (-0.027 per trip, -0.38% per trip on ~7.9 USDT
  positions), about 8 trips a day, so about -0.21 USDT a day while trading.
- An average entry at the bot's size and horizons loses about 0.19% x 8 USDT = -0.015 USDT per
  trip, about -0.12 USDT a day at 8 trips. The bot did worse than average (-0.23 pp at +90 min,
  not significant).
- Even the best test bucket's +0.17% (not significant) would earn +0.014 USDT per 8-USDT
  trip, cents per day. On this wallet, no tested short-horizon spot entry can earn more
  than cents a day, and none is reliably positive.

Options for the operator: keep the trader paused (default); resume as is, with the B section
information now in its prompt; or resume with a changed entry style. No entry style here
qualifies as a proposal under the n >= 200, t > 2 rule.
