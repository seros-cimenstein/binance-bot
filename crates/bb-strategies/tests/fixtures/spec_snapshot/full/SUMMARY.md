# Speculation snapshot — public Binance REST data

- Fetch timestamp (UTC): @GENERATED_AT@
- Spot endpoint: `GET https://api.binance.com/api/v3/ticker/24hr`; klines: `GET https://api.binance.com/api/v3/klines`
- Futures endpoints: `GET https://fapi.binance.com/fapi/v1/premiumIndex`, `GET https://fapi.binance.com/fapi/v1/ticker/24hr`, `GET https://fapi.binance.com/fapi/v1/fundingInfo`, `GET https://fapi.binance.com/futures/data/openInterestHist`, `GET https://fapi.binance.com/futures/data/globalLongShortAccountRatio`
- Filter: spot USDT pairs quoteVolume >= 1,000,000 USDT, leveraged tokens (UP/DOWN/BULL/BEAR suffixes) and stablecoin/fiat bases excluded; perps quoteVolume >= 5,000,000 USDT.
- Futures API status: OK (HTTP 200)
- Output directory: `@OUT_DIR@`

## Row counts

| dataset | rows |
|---|---|
| spot_tickers.csv | 7 |
| funding.csv | 4 |
| open_interest.csv | 3 |
| momentum.csv | 4 |

## Endpoint failures

- `fapi globalLongShortAccountRatio`: 1 failed call(s), last status HTTP 404
- `spot klines`: 2 failed call(s), last status HTTP 404
- momentum: 3/7 per-symbol failures (klines unavailable)
- open_interest: 1/4 per-symbol failures (history unavailable)

## Top 15 spot gainers 24h (quoteVolume >= 5M USDT)

| symbol | lastPrice | chg_24h | quoteVolume | range_24h |
|---|---|---|---|---|
| `MOVRUSDT` | 3.14 | 79.74% | 101.9M | 93.7984% |
| `BTCUSDT` | 84940 | 1.32% | 1.4B | 2.5096% |
| `ETHUSDT` | 2705.56 | 1.15% | 704.5M | 2.0263% |
| `ZEROLOWUSDT` | 0 | -0% | 6M |  |
| `牛来USDT` | 0.09822 | -6.76% | 11.1M | 9.529% |
| `JUPUSDT` | 0.4 | -9.42% | 24.7M | 18.4211% |

## Top 15 spot losers 24h (quoteVolume >= 5M USDT)

| symbol | lastPrice | chg_24h | quoteVolume | range_24h |
|---|---|---|---|---|
| `JUPUSDT` | 0.4 | -9.42% | 24.7M | 18.4211% |
| `牛来USDT` | 0.09822 | -6.76% | 11.1M | 9.529% |
| `ZEROLOWUSDT` | 0 | -0% | 6M |  |
| `ETHUSDT` | 2705.56 | 1.15% | 704.5M | 2.0263% |
| `BTCUSDT` | 84940 | 1.32% | 1.4B | 2.5096% |
| `MOVRUSDT` | 3.14 | 79.74% | 101.9M | 93.7984% |

## Top 15 by volume_ratio (last 24h vs previous 6d daily avg)

| symbol | volume_ratio | chg_24h | realized_vol_24h | quoteVolume |
|---|---|---|---|---|
| `MOVRUSDT` | 3.7636x | 14.76% | 1.5311 | 101.9M |
| `BTCUSDT` | 1.3039x | -0.21% | 1.3324 | 1.4B |
| `牛来USDT` | 0.5471x | -7.59% | 1.1895 | 11.1M |

## Top 10 most positive funding (annualized)

| symbol | annualized | rate_per_interval | interval | basis_bps | markPrice | volume |
|---|---|---|---|---|---|---|
| `BTCUSDT` | 9.48% | 0.0001% | 8h | -4.009 | 84900.1 | 12.1B |
| `ETHUSDT` | 4.35% | 0% | 8h | -6.8023 | 2704.5 | 8.7B |
| `ZEROIDXUSDT` | 0% | 0% | 8h |  | 0.5 | 7M |
| `MOVRUSDT` | -262.8% | -0.0012% | 4h | 15.9236 | 3.145 | 300M |

## Top 10 most negative funding (annualized)

| symbol | annualized | rate_per_interval | interval | basis_bps | markPrice | volume |
|---|---|---|---|---|---|---|
| `MOVRUSDT` | -262.8% | -0.0012% | 4h | 15.9236 | 3.145 | 300M |
| `ZEROIDXUSDT` | 0% | 0% | 8h |  | 0.5 | 7M |
| `ETHUSDT` | 4.35% | 0% | 8h | -6.8023 | 2704.5 | 8.7B |
| `BTCUSDT` | 9.48% | 0.0001% | 8h | -4.009 | 84900.1 | 12.1B |

Funding intervals sourced from `/fapi/v1/fundingInfo`: 2 symbols explicitly configured; 1/4 of the kept perps use a non-8h interval.

## Top 10 OI 24h change up (with price change)

| symbol | oi_chg_24h | oi_now_usd | price_chg_24h | long/short acct |
|---|---|---|---|---|
| `BTCUSDT` | 5.02% | 8.4B | 1.31% | 1.0751 |
| `MOVRUSDT` | -17.53% | 41.2M | 80.1% |  |

## Top 10 OI 24h change down (with price change)

| symbol | oi_chg_24h | oi_now_usd | price_chg_24h | long/short acct |
|---|---|---|---|---|
| `MOVRUSDT` | -17.53% | 41.2M | 80.1% |  |
| `BTCUSDT` | 5.02% | 8.4B | 1.31% | 1.0751 |

## RSI(14, 1h) > 70 (n=1, from top 60 spot by volume)

| symbol | rsi | chg_24h | chg_7d | dist_from_7d_high |
|---|---|---|---|---|
| `MOVRUSDT` | 100 | 14.76% | 168.25% | -0.19% |

## RSI(14, 1h) < 30 (n=1, from top 60 spot by volume)

| symbol | rsi | chg_24h | chg_7d | dist_from_7d_high |
|---|---|---|---|---|
| `牛来USDT` | 0.3262 | -7.59% | -45.91% | -46.03% |

## Notes

- Facts only; no trade recommendations.
- `volume_ratio` uses hourly kline quote volume: last 24 bars / mean daily volume of the preceding 6 days; the latest hourly bar is still forming.
- `oi_change_24h_pct` compares latest `sumOpenInterestValue` to the value 24 hourly samples earlier (limit=25).
- `annualized_pct` = lastFundingRate x (24 / funding_interval_h) x 365 x 100; `funding_interval_h` comes from `/fapi/v1/fundingInfo` (default 8h when the symbol is absent from that list).
- `range_pct` = (high24h - low24h) / low24h x 100.
- `realized_vol_24h` = stdev of the last 24 hourly log returns x sqrt(24) x 100 (%).
- `dist_from_7d_high_pct` = (last close / max high of the last 168 hourly bars - 1) x 100.
- Stablecoin/fiat bases excluded: AEUR, ALUSD, ARS, AUD, BIDR, BKRW, BRL, BUSD, BVND, CEUR, CUSD, DAI, DOLA, EUR, EURI, EURR, FDUSD, FRAX, GBP, GUSD, GYEN, HUSD, IDR, IDRT, JPY, JPYC, LUSD, MIM, NGN, PAX, PYUSD, RLUSD, RUB, SUSD, TRY, TUSD, UAH, USD1, USDA, USDC, USDD, USDE, USDF, USDG, USDP, USDR, USDS, USDT, USDX, USDY, UST, USTC, VAI, XSGD, XUSD, ZAR, ZUSD
- Leveraged tokens excluded by UP/DOWN/BULL/BEAR base suffix, with allowlist ['JUP', 'SYRUP'].
