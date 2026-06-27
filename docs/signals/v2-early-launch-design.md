# V2 Early-Launch Signal Design

## Goal

V1 DexScreener filtered for pairs that already had meaningful liquidity, 24h
volume, and up to 24 hours of age, so it mostly detected tokens after the
attention/volume move. The 2026-06-27 audit showed 75.3% catastrophic outcomes
and a median live-pair price change of -90.7% after detection. V2 should instead
detect Solana launches in the first 5-30 minutes after pool creation, before 24h
volume becomes the dominant signal, using launch-quality heuristics and a
forward snapshot ledger. At the 4-week mark, success means V2 has at least four
weekly reports, a complete 7-day path for most detections, a materially lower
catastrophic/rug rate than V1, and enough surviving candidates to judge whether
the signal clears the audit's friction assumptions.

## Detection Heuristics

Initial filter: Solana only, WSOL quote only, pair age 0-30 minutes, liquidity
between $3,000 and $200,000, at least one social link, and no narrow scam-pattern
or cheap creator-rug rejection. These thresholds are deliberately conservative;
they should be tuned only after snapshots prove the forward path.

| Heuristic | Starting rule | Data source | Cost | False-positive / false-negative risk |
|---|---:|---|---:|---|
| Chain | `chainId == solana` | DexScreener pair payload | Free, included in pair call | Excludes smaller-chain outliers, but audit sample outside Solana was too small. |
| Quote token | Quote mint is WSOL (`So11111111111111111111111111111111111111112`) or symbol `SOL`/`WSOL` when mint is missing | DexScreener pair payload | Free, included | Rejects USDC launches that may be legitimate; avoids obscure quote traps. |
| Pair age | `0 <= now - pairCreatedAt <= 30m`; prefer 5-30m in reporting buckets | DexScreener pair payload | Free, included | DexScreener indexing lag may miss the first few minutes or make age look late. |
| Initial liquidity | `$3,000 <= liquidity.usd <= $200,000` | DexScreener pair payload | Free, included | Weak heuristic: scammers can seed $3k; upper bound may reject high-quality funded launches. |
| Volume | No minimum 24h volume. Record 5m/1h/24h only as diagnostics. Reject only obvious wash spikes, e.g. 5m volume > 20x liquidity in first 10m. | DexScreener pair payload | Free, included | Wash-trade rule is weak and should stay narrow until proven. |
| Social presence | Require at least one website, Twitter/X, or Telegram URL from token profile / pair info | DexScreener token profile and pair `info` | Free, included if profile/pair returned | Easy to fake; missing links can reject stealth but legitimate launches. |
| Scam text patterns | Reject only narrow evidence-based names/symbols: empty symbol/name, symbols longer than 12 chars, repeated single-char symbols like `AAAAAA`, impersonations of top assets exactly or with `$`/`2.0` suffix (`SOL`, `BTC`, `ETH`, `USDC`, `USDT`, `BONK`, `WIF`, `TRUMP`) unless mint is allowlisted, and symbols containing URL fragments. Do not reject based on profanity alone unless it overlaps known rug labels from prior reports. | DexScreener pair payload plus local static denylist | Free | Copycat detection can reject parody/meme names that still pump; keep list tiny and auditable. |
| Liquidity lock | Phase 2 should record `unknown`; do not gate unless a cheap provider field exists. Defer chain-level LP lock/burn validation to V3 if it requires per-token transaction scans. | DexScreener if present; otherwise Helius transaction history | Free if present; otherwise paid and likely too expensive | Not checking lock means rugs pass; checking it poorly can create false confidence. |
| Creator recent rug | Only apply if cheaply observable from pool-creation transaction or a cached local creator map. Reject if the same creator launched another token in the last 24h whose pair disappeared, liquidity fell >80%, or liquidity is now < $1k. | Helius for selected candidates only; local V2 ledger/snapshots | Paid, capped candidate-only | Strong if accurate, but attribution is messy across launchpads and proxy wallets. |

Detection records must persist the raw pair payload, the specific feature values,
the rejection reasons considered, and the selected threshold version. Confidence
should not reuse V1's liquidity-volume score. For Phase 2, use a simple
categorical `filter_version` and leave ranking for reports.

## Data Sources And API Budget

The detector should continue the existing pattern in
`src/signals/dexscreener.py`: poll DexScreener over HTTP with `aiohttp`, dedupe
seen addresses in memory, and append JSONL rows. V2 writes to a separate ledger.
Smart-money's Helius usage in `src/signals/smart_money.py` shows the paid RPC
pattern and should be copied only for candidate enrichment, not for every token
profile.

Proposed polling cadence: every 60 seconds.

Per cycle budget:

- DexScreener `token-profiles/latest/v1`: 1 free call.
- DexScreener `latest/dex/tokens/{token}`: up to 30 free calls for newly seen
  Solana profile addresses. Stop after the first qualifying WSOL pair per token.
- Helius: 0 by default; up to 5 candidate-only calls per cycle if Phase 2 enables
  creator attribution.
- Birdeye: 0 by default; reserved for later price fallback if DexScreener
  snapshots are missing.

Daily estimate at 1-minute cadence: DexScreener about 1,440 profile calls plus
up to 43,200 token-pair calls, all free/no auth. Paid APIs stay below 5,000/day
by hard cap: Helius max 3,000/day, Birdeye max 1,000/day, leaving reserve for
manual runs. If the cap is hit, V2 records `paid_enrichment_skipped` and still
emits only DexScreener-backed fields.

## Post-Detection Snapshotter

Every V2 detection schedules snapshots at +5m, +15m, +1h, +6h, +24h, +3d, and
+7d. Each snapshot captures pair existence, price, liquidity, volume windows,
FDV/market cap when present, pair URL, and an error/status field. Snapshots are
stored in `data/signals-v2-snapshots.jsonl`, separate from
`data/signals-v2-ledger.jsonl`.

Detection schema:

```json
{
  "detection_id": "v2-solana-<pairAddress>-<detected_at_ms>",
  "filter_version": "v2-launch-2026-06-27",
  "detected_at_ms": 1782550000000,
  "chain": "solana",
  "token_address": "...",
  "pair_address": "...",
  "quote_address": "So11111111111111111111111111111111111111112",
  "features": {"pair_age_seconds": 600, "liquidity_usd": 12000.0},
  "raw_pair": {}
}
```

Snapshot schema:

```json
{
  "detection_id": "v2-solana-<pairAddress>-<detected_at_ms>",
  "scheduled_offset_seconds": 900,
  "scheduled_for_ms": 1782550900000,
  "captured_at_ms": 1782550912000,
  "status": "ok",
  "pair_exists": true,
  "price_usd": 0.0000123,
  "liquidity_usd": 9500.0,
  "volume": {"m5": 1800.0, "h1": 7400.0, "h6": 7400.0, "h24": 7400.0},
  "raw_pair": {}
}
```

Simplest restart-safe structure: run the snapshotter from cron every minute
inside the existing `scheduler` container. It reads the V2 ledger and snapshot
JSONL, computes due offsets that have no captured row yet, fetches current pair
state, appends snapshots, and exits. This survives container restarts better
than in-process timers and does not require a new dependency. A separate
long-running container is useful only if cron latency or API volume becomes a
problem.

## Weekly Evaluation Report

`src/signals/v2_report.py` should walk the detection and snapshot files weekly
and write `data/signals-v2-report-{date}.md`. Metrics:

- Rug rate: pair missing, liquidity < $1k, or liquidity drawdown >80% by any
  post-detection snapshot.
- 2x rate: max post-detection price >= 2.0x detection price before rug.
- Max drawdown distribution from detection price to each token's worst snapshot.
- Median and percentile path returns at +15m, +1h, +6h, +24h, +3d, and +7d.
- Friction-adjusted paper profitability using the audit assumptions: $15.63
  notional, $0.50 Solana round-trip fixed cost, 2% entry slippage, 2% exit
  slippage, so break-even must exceed about 7.2% before spread/MEV. Report both
  best-exit and scheduled-exit variants to avoid claiming impossible fills.

## File And Module Layout

- `src/signals/dexscreener_launches.py`: V2 detector, config dataclass, filter
  feature extraction, and run loop entry point.
- `src/signals/snapshotter.py`: restart-safe due-snapshot collector and JSONL
  persistence helpers.
- `src/signals/v2_report.py`: weekly metrics and markdown renderer.
- `data/signals-v2-ledger.jsonl`: V2 detections only; never mix with V1
  `signals-ledger.jsonl`.
- `data/signals-v2-snapshots.jsonl`: forward path snapshots keyed by
  `detection_id`.
- `docker-compose.yml`: add a commented Phase 2
  `signals-dexscreener-launches` service. Use a new service for A/B isolation
  and independent stop/restart controls while leaving V1 untouched.
- `docker/crontab`: add commented Phase 2 cron lines for the snapshotter and
  weekly report.

## Deployment And Rollout

Phase 2 lands inert by default, then runs V2 alongside V1 for 30 days on the Pi.
V1 remains monitored-only; V2 writes its own ledger and snapshots. Compare weekly
reports against the V1 audit definitions after at least 7 days of complete V2
paths, then again after 30 days. Sunset V1 only if V2 has materially lower rug
and catastrophic rates, enough detections to evaluate, and friction-adjusted
paper exits that are not dominated by one or two outliers.

## Phase 2 Non-Goals

Phase 2 does not add execution wiring, live DEX trading, smart-money rebuilds,
Telegram channel fixes, private social ingestion, wallet funding/bridging/gas
management, SQLite or async framework migrations, ML ranking, or automatic
threshold tuning. Those come only after V2 proves survivable forward paths.

## Risks And Pre-Mortem

- Rug rate still exceeds 50%: V2 is earlier but not better. Failure signal:
  weekly report shows high rug/catastrophic rates even among 5-30 minute WSOL
  launches with social links.
- Signal count collapses below about 5/week: filters may be too strict and noise
  will dominate. Failure signal: weekly report cannot compute stable percentiles.
- Early launches pump before DexScreener indexes them: V2 is still late. Failure
  signal: pair age at detection clusters near 25-30 minutes and detection-time
  5m/1h price changes are already extreme.
- Snapshotter falls behind or misses rows: outcomes become unmeasurable again.
  Failure signal: more than 5% of due snapshots are missing or captured more
  than 5 minutes late for short offsets.
- Social and creator heuristics are too easy to game: fake links and launchpad
  proxies pass. Failure signal: accepted launches have the same rug signature as
  rejected launches.
