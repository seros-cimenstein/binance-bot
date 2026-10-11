# Speculation snapshot — public Binance REST data

- Fetch timestamp (UTC): @GENERATED_AT@
- Spot endpoint: `GET https://api.binance.com/api/v3/ticker/24hr`; klines: `GET https://api.binance.com/api/v3/klines`
- Futures endpoints: `GET https://fapi.binance.com/fapi/v1/premiumIndex`, `GET https://fapi.binance.com/fapi/v1/ticker/24hr`, `GET https://fapi.binance.com/fapi/v1/fundingInfo`, `GET https://fapi.binance.com/futures/data/openInterestHist`, `GET https://fapi.binance.com/futures/data/globalLongShortAccountRatio`
- Filter: spot USDT pairs quoteVolume >= 1,000,000 USDT, leveraged tokens (UP/DOWN/BULL/BEAR suffixes) and stablecoin/fiat bases excluded; perps quoteVolume >= 5,000,000 USDT.
- Spot 24h ticker fetch FAILED; snapshot is incomplete (see Endpoint failures).
- Futures API status: OK (HTTP 200)
- Output directory: `@OUT_DIR@`

## Row counts

| dataset | rows |
|---|---|
| spot_tickers.csv | 0 |
| funding.csv | 4 |
| open_interest.csv | 3 |
| momentum.csv | 0 |

## Endpoint failures

- `fapi globalLongShortAccountRatio`: 1 failed call(s), last status HTTP 404
- `spot ticker/24hr`: 1 failed call(s), last status HTTP 503
- open_interest: 1/4 per-symbol failures (history unavailable)

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
