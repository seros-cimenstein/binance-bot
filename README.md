# Binance Trading Bot

A Binance trading bot implemented in Python using `ccxt` and `asyncio`.

The current development runtime is a paper-only modular asset pool. It
discovers eligible Binance spot routes, models current fees and market
constraints, and records virtual allocations against configurable benchmarks.
Automatic live trading is disabled by default. Phase 32 adds a separate
Claude-controlled Spot runtime that can be explicitly enabled outside the
paper pool.

## Setup

1. Copy `.env.example` to `.env` and fill in your API keys. Keep `.env` and
   `.env.*` backup copies out of commits because they may contain credentials.
2. Install dependencies: `pip install .`

For deployed paper-service operation, backup/restore, and verification
procedures, see `OPERATIONS.md`.

Run one paper evaluation cycle locally:

```bash
python -m src.pool --once
```

Run a fast deterministic replay without touching the live paper ledger:

```bash
scripts/pool_replay.sh
```

Explain current paper decisions and available candidate evidence without
writing to the running ledger:

```bash
python -m src.pool.report --db-path ~/.local/share/binance-bot/data/pool-paper.db
```

Evaluate selected direct-`USDT` Spot assets quickly using historical public
candles in an isolated research database:

```bash
python -m src.pool.history_replay --balance BNB=0.0169745 \
  --symbol ETH/USDT --symbol SOL/USDT \
  --start 2026-05-20T00:00:00Z --end 2026-05-26T00:00:00Z \
  --db-path /tmp/binance-bot-history-replay.db
python -m src.pool.report --db-path /tmp/binance-bot-history-replay.db
```

Historical replay is for strategy research only. It uses public candles and
modeled costs, so it does not replace current-market paper warm-up, dynamic
exchange-rule checks, permission diagnostics, or a tiny live-canary review.
For a small account, a valid Binance minimum order may exceed a permitted
allocation percentage; that is a risk decision to review, not a limit to
silently bypass.

Check API-key capabilities before any live-order probe:

```bash
python -m src.api_permissions --feature manual_live_probe
python -m src.api_permissions --feature spot_live_canary
python -m src.api_permissions --feature claude_spot_autonomous
python -m src.api_permissions --feature margin_dry_run
python -m src.api_permissions --feature margin_canary_plan
python -m src.api_permissions --feature futures_dry_run
python -m src.api_permissions --feature futures_canary_plan
python -m src.api_permissions --feature transfer_dry_run
python -m src.api_permissions --feature transfer_canary_plan
python -m src.api_permissions --feature unified_pool_dry_run
python -m src.api_permissions --feature symbol_scope_audit
python -m src.api_permissions --feature withdrawal_dry_run
python -m src.api_permissions --feature prediction_dry_run
```

Inspect disabled-by-default advanced product module gates:

```bash
python -m src.product_modules
```

Audit local API symbol/asset scope before live-capable planning:

```bash
python -m src.scope_audit --symbol BNB/USDT --asset BNB
```

Create a plan-only spot live canary audit without submitting an order:

```bash
python -m src.spot_canary --plan-only
```

Create a plan-only guarded live spot allocation audit without submitting an
order:

```bash
python -m src.live_spot_allocator --plan-only
```

Create a plan-only Claude autonomous Spot decision audit without submitting an
order:

```bash
python -m src.claude_spot --plan-only
```

Claude Spot uses headless Claude Code with schema-validated decisions and the
stored money-maker mission prompt. When live submission is enabled, it may use
the whole available Spot wallet and execute one dynamically routed Spot action
of up to three serial order legs per 5-minute timer cycle. The default live
mode is `small_market_aggressive`: it uses looser small-market volume/spread
gates, ranks tiny-account-fit opportunities ahead of top-volume assets, and
sends up to `10` scored candidates with one route quote per candidate so the
cycle stays within Binance request-weight limits and avoids overspending Claude
calls on a small account. `CLAUDE_SPOT_MARKET_PICK_MODE=liquid_top_value`
restores the older top-liquid behavior. Submit mode requires
`CLAUDE_SPOT_ENABLED=true`,
`CLAUDE_SPOT_DYNAMIC_SCOPE=all_eligible_spot`, `USE_TESTNET=false`,
`DRY_RUN=false`, and `CONFIRM_LIVE_TRADING=yes`.
`CLAUDE_SPOT_BLOCK_FIAT_ASSETS=true` is the default, so the route builder avoids
fiat rails such as `TRY`, `JPY`, `EUR`, `GBP`, `AUD`, `BRL`, and `IDR` that may
be visible in market data but rejected by some accounts for Spot API orders.
Stablecoin assets such as `USDT`, `USDC`, `FDUSD`, `DAI`, `BUSD`, and `USD1`
are not treated as fiat by this guard. `CLAUDE_SPOT_ALLOWED_SYMBOLS` and
`CLAUDE_SPOT_ALLOWED_ASSETS` can be set for a stricter local scope; when set,
every live route leg must stay inside those lists.

The runtime still enforces Binance market rules, available balances, fee
handling, Spot permission/order-test preflight, emergency stop, audit,
reconciliation, and halt-on-ambiguous-leg behavior. Aggressive mode lets Claude
accept upfront fee/slippage friction for a stated small-market thesis, but it
does not add Margin, Futures, transfer, withdrawal, or prediction write access.
If Claude does not return a decision before `CLAUDE_SPOT_TIMEOUT_SECONDS`, the
cycle records a held/no-order audit row instead of submitting an order.
To avoid churn on a small account, live submit mode feeds Claude the modeled
fee, spread, slippage, route friction, break-even, market evidence, and recent
trade memory before any decision. `CLAUDE_SPOT_MIN_TRADE_INTERVAL_SECONDS=0`
and `CLAUDE_SPOT_MAX_SUBMITTED_TRADES_PER_DAY=0` disable the older fixed
cooldown/day cap so Claude can think and trade on each 5-minute cycle when the
hard gates pass. `CLAUDE_SPOT_HALT_AFTER_FAILED_SUBMISSION=true` still holds
after a partial failed route until operator review, and
`CLAUDE_SPOT_ALLOW_NEGATIVE_FRICTION_BPS` is a hard maximum immediate
fee/slippage/spread loss for routes offered to Claude.
The small-market live profile uses `300` bps by default so tiny-account routes
are not starved by shallow spread/friction ceilings, while still filtering
obviously expensive routes.
Live buy-leg execution now sizes from a dynamic quote reserve before order-test
and submission: authenticated buy-side taker fee, the configured slippage
buffer, and `CLAUDE_SPOT_LEG_ROUNDING_RESERVE_BPS`, with
`CLAUDE_SPOT_MIN_LEG_QUOTE_RESERVE_BPS` as the floor. This reserve prevents a
multi-leg route from spending the exact post-sell quote balance and failing the
next buy from fees, rounding, or price movement.
Before any live submission, Claude Spot also builds a compact evidence packet
when `CLAUDE_SPOT_MARKET_EVIDENCE_ENABLED=true`: `5m`, `15m`, `1h`, `4h`,
`24h`, and `7d` returns, volatility, volume change, break-even move, and recent
Claude trade memory from the audit log. The `5m` and `15m` windows come from
minute candles and are fed as `short_returns_bps` so Claude can see the current
tape instead of leaning only on slower momentum. With
`CLAUDE_SPOT_REQUIRE_MARKET_EVIDENCE_FOR_LIVE=true`,
live routes fail closed unless the selected route clears
`CLAUDE_SPOT_MARKET_EVIDENCE_MIN_EDGE_BPS` after route friction and the recent
loss-streak gate is not active. The current default minimum is `50` bps, which
is more aggressive than the earlier `200` bps gate. Use
`python -m src.claude_spot --evidence-only` to inspect the context without
asking Claude or submitting orders.
`CLAUDE_SPOT_SPECULATIVE_MODE_ENABLED=true` is an explicit live-risk profile:
it lets a selected route miss the normal evidence minimum only when evidence is
ready, expected edge remains above `CLAUDE_SPOT_SPECULATIVE_MIN_EDGE_BPS`, and
Claude gives a concrete speculative thesis. It does not bypass route scope,
fiat blocking, balances, dynamic fees/reserves, order-test, emergency stop,
failed-route halt, audit, or reconciliation.
Telegram alerts include a deterministic ELI5 explanation for every Claude Spot
cycle when `CLAUDE_SPOT_TELEGRAM_EXPLAIN=true`: held, planned, submitted,
failed, and blocked. `CLAUDE_SPOT_TELEGRAM_REASON_CHARS=0` sends the full Why
text; messages are split into Telegram-safe chunks when needed, while the full
decision context remains in the audit JSONL.

Collect and inspect optional read-only social signal evidence:

```bash
python -m src.social_signal --asset BONK --asset EDEN --evidence-only
```

When `SOCIAL_SIGNAL_ENABLED=true`, Claude Spot adds stored social evidence to
candidate routes. Telegram collection uses only Bot API updates visible to the
configured bot, and X collection uses official API search. Social evidence is
additive; it cannot bypass Binance market rules, fee/friction gates, evidence
gates, or emergency stops. If manipulation risk is high and
`SOCIAL_SIGNAL_BLOCK_MANIPULATION_RISK=true`, the affected route is withheld
before Claude can choose it.

Run read-only margin discovery and local borrow/repay modeling:

```bash
python -m src.margin --dry-run --asset USDT --borrow-amount 10
```

Create a plan-only margin canary with a repayment intent:

```bash
python -m src.margin_canary --plan-only --margin-symbol BNB/USDT \
  --asset USDT --borrow-amount 1 --repay-amount 1
```

Run read-only USD-M futures discovery and local order-risk modeling:

```bash
python -m src.futures --dry-run --symbol BTC/USDT --side BUY --quantity 0.001
```

Create a plan-only USD-M futures canary with a reduce-only exit intent:

```bash
python -m src.futures_canary --plan-only --symbol BTC/USDT \
  --side BUY --quantity 0.001 --exit-side SELL --exit-reduce-only \
  --exit-trigger "operator approved close"
```

Plan a dry-run internal transfer and audit the proposal:

```bash
python -m src.transfers --dry-run --source-wallet spot --destination-wallet funding --asset USDT --amount 5
```

Create a plan-only universal-transfer canary audit:

```bash
python -m src.transfer_canary --plan-only --source-wallet spot \
  --destination-wallet funding --asset USDT --amount 1
```

Value a local unified dry-run asset pool from JSON inputs:

```bash
python -m src.pool.unified --dry-run --holdings-json holdings.json --prices-json prices.json
```

Build a local withdrawal safety plan without submitting a withdrawal:

```bash
python -m src.withdrawals --dry-run --asset USDT --amount 1 \
  --address TAllowlistedAddress --network TRX \
  --allow-address TAllowlistedAddress --allow-network TRX \
  --operator-id local-review --allowed-operator local-review \
  --approval risk --approval ops
```

Model a prediction position from a local market-data fixture without placing a
stake:

```bash
python -m src.prediction --dry-run --market-data-file prediction-markets.json \
  --symbol BTC/USDT --direction UP --stake-usdt 1 --bankroll-usdt 100
```

Run the manual real-order probe only when explicitly testing one live order:

```bash
python -m src.live_probe --confirm-real-order
```

The probe is separate from the paper pool and submits at most one direct
USDT-market sell.

The spot canary is also separate from the paper pool. Its first version is a
foreground-only direct sell into `USDT` with replay/warm-up confirmation flags,
hard daily limits, safe Binance order-test preflight, audit logging, and
Telegram alerting.

The live spot allocator is foreground-only as well. Its first version handles
one direct spot market order per invocation with risk limits, benchmark
valuation, balance reconciliation, safe order-test preflight, audit logging,
and Telegram alerting.

The margin module is dry-run only. It can read margin account, max-borrow, and
interest data for local modeling, but live margin borrow, repay, order, and
transfer actions are not implemented.

The USD-M futures module is also dry-run only. It can read futures account,
position, market-rule, funding, symbol-config, and leverage-bracket data for
local risk modeling, but live futures order and account-mode writes are not
implemented.

The transfer module is dry-run only. It can read wallet balances and transfer
history for local planning/audit, but live universal-transfer submission is not
implemented.

The margin, USD-M futures, and universal-transfer canary commands are
foreground plan-only readiness checks. They add scope, emergency-stop, risk,
and audit gates around the dry-run models; all submission flags fail closed.

The unified pool module is local valuation only. It combines account-type
holdings, debt, PnL, reserves, transfer availability, and benchmark prices into
a dry-run allocation report without live writes.

Advanced Binance product modules are tracked through a local Phase 23 registry.
Withdrawal safety and prediction risk now have dry-run/local modeling modules.
Live prediction trading, live withdrawals, future products, and live
margin/futures/transfer activation remain disabled unless a future
module-specific activation phase adds risk controls, tests, audit, and explicit
approval.

The scope audit is local/operator-owned. Binance does not expose the full
API-key symbol whitelist safely, so foreground live-capable spot features fail closed
unless `API_SYMBOL_WHITELIST` and, where assets are involved,
`API_ASSET_WHITELIST` cover the requested scope.

Review cross-product readiness evidence and serial canary policy locally:

```bash
python -m src.advanced_readiness
```
