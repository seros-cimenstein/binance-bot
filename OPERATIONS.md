# Binance Pool Operations

The active development runtime is the user-owned, paper-only modular asset
pool at `~/.local/share/binance-bot`. It reads Binance balances, markets, and
commission schedules, then records virtual allocations in
`data/pool-paper.db`. It does not submit exchange orders.
By default it scores every asset represented in the eligible market set;
`POOL_MAX_SCORING_ASSETS` is an optional resource-control override.

## Safety Posture

The former `binance-bot.service` used fixed-size live BTC order intent. It is
stopped and disabled, and the code refuses fixed-size real-money startup.
Do not enable or restart that service while evaluating the pool.

Verify the boundary:

```bash
systemctl --user show binance-bot.service -p ActiveState -p UnitFileState
systemctl --user show binance-pool-paper.service -p ActiveState -p UnitFileState
```

## Deploy Paper Service

Validate and deploy without `sudo`:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m pytest -q
.venv/bin/python test_pipeline.py
scripts/smoke.sh
git diff --check
scripts/setup_user_deploy.sh
systemctl --user disable --now binance-bot.service
systemctl --user enable --now binance-pool-paper.service
systemctl --user enable --now binance-pool-paper-backup.timer
```

`scripts/setup_user_deploy.sh` preserves `.env`, databases, logs, and
backups. It imports the historic live database only when that database does
not already exist.

## Status And Logs

```bash
cd ~/.local/share/binance-bot
scripts/pool_status.sh
systemctl --user status binance-pool-paper.service --no-pager
journalctl --user -u binance-pool-paper.service -f
```

Run one foreground read-only/paper evaluation cycle when diagnosing catalog or
valuation behavior:

```bash
cd ~/.local/share/binance-bot
venv/bin/python -m src.pool --once
```

Read the deployed paper evidence in plain language without changing its
ledger:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.pool.report \
  --db-path ~/.local/share/binance-bot/data/pool-paper.db
```

This report shows warm-up progress, virtual holdings, latest hold/rebalance
reasons, eligible candidate coverage, market rejection reasons, and ranked
candidates only after sufficient retained observations exist.

Run a fast local replay when you need quick confidence in routing, fees,
minimum-order gates, and scorer behavior without waiting for real-time paper
cycles:

```bash
cd /home/user/projects/binance-bot
scripts/pool_replay.sh
```

Replay uses fixture market data and an isolated SQLite database. It is for
development and regression checks; it is the first live-readiness gate, but it
does not by itself approve live-money pool execution.

Run a fast historical strategy study from public Binance candles for a small,
explicit direct-`USDT` candidate set:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.pool.history_replay --balance BNB=0.0169745 \
  --symbol ETH/USDT --symbol SOL/USDT \
  --start 2026-05-20T00:00:00Z --end 2026-05-26T00:00:00Z \
  --db-path /tmp/binance-bot-history-replay.db
.venv/bin/python -m src.pool.report \
  --db-path /tmp/binance-bot-history-replay.db
```

The historical command automatically includes direct-`USDT` routes for initial
assets and the `BTC`, `PAXG`, and `XAUT` benchmarks. It reads public candles
only, applies modeled cost inputs, and writes only its specified isolated
database. It is research evidence, not permission to trade or a substitute
for live-market Phase 13 evidence.
If minimum-order holds dominate for a small balance, record that result for the
live-readiness review. Do not enlarge an approved exposure limit solely to make
a trade fit Binance minimum notional rules.

## API Permission Diagnostics

Run permission diagnostics before any live-order probe or future live canary:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.api_permissions --feature manual_live_probe
.venv/bin/python -m src.api_permissions --feature spot_live_canary
.venv/bin/python -m src.api_permissions --feature margin_dry_run
.venv/bin/python -m src.api_permissions --feature margin_canary_plan
.venv/bin/python -m src.api_permissions --feature futures_dry_run
.venv/bin/python -m src.api_permissions --feature futures_canary_plan
.venv/bin/python -m src.api_permissions --feature transfer_dry_run
.venv/bin/python -m src.api_permissions --feature transfer_canary_plan
.venv/bin/python -m src.api_permissions --feature unified_pool_dry_run
.venv/bin/python -m src.api_permissions --feature symbol_scope_audit
.venv/bin/python -m src.api_permissions --feature withdrawal_dry_run
.venv/bin/python -m src.api_permissions --feature prediction_dry_run
```

The diagnostic reads `GET /api/v3/account` and uses Binance's safe
`POST /api/v3/order/test` endpoint for spot trading. It does not place an
order. Margin, futures, transfer, and unified-pool dry-run modeling require
reading only in the bot registry; live margin trading, live futures trading,
live universal transfer, prediction trading, and withdrawals remain
policy-blocked even if an API key exposes those permissions.

## Symbol Scope Audit

Binance does not expose the full API-key symbol whitelist through this safe
diagnostic path. Configure the local/operator-owned scope before any
live-capable feature is planned:

```bash
API_SYMBOL_WHITELIST=BNB/USDT,BTC/USDT
API_ASSET_WHITELIST=BNB,USDT
```

Audit requested symbols/assets without exchange writes:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.scope_audit --symbol BNB/USDT --asset BNB
```

Use `--fail-on-blocked` in scripts that must stop when local scope is missing
or too narrow.

## Withdrawal Safety Dry Run

The withdrawal module models allowlists, limits, approvals, cooldowns,
emergency-stop state, and operator identity locally. It has no withdrawal
submission path:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.withdrawals --dry-run --asset USDT --amount 1 \
  --address TAllowlistedAddress --network TRX \
  --allow-address TAllowlistedAddress --allow-network TRX \
  --operator-id local-review --allowed-operator local-review \
  --approval risk --approval ops
```

`--confirm-live-withdrawal` intentionally fails closed.

## Prediction Dry Run

Prediction modeling consumes a local or injected read-only market payload. It
does not call a prediction stake, claim, or settlement write endpoint:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.prediction --dry-run \
  --market-data-file prediction-markets.json --symbol BTC/USDT \
  --direction UP --stake-usdt 1 --bankroll-usdt 100
```

`--confirm-live-prediction` intentionally fails closed.

## Spot Live Canary

The spot live canary is a foreground-only Phase 17 command. Its first version
only supports one direct source-asset sell into `USDT`; it is not a background
allocator and it is not wired to `src.pool`.

Plan the canary without submitting an order:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.spot_canary --plan-only
```

The canary can submit a real order only when all confirmation flags are
provided and `.env` has `USE_TESTNET=false`, `DRY_RUN=false`, and
`CONFIRM_LIVE_TRADING=yes`:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.spot_canary \
  --confirm-live-canary \
  --confirm-replay-passed \
  --confirm-paper-warmup-reviewed
```

Default limits are intentionally tiny: max order notional `6 USDT`, max daily
notional `6 USDT`, and max daily orders `1`. The canary checks
`data/spot-canary.stop` before exchange reads; create that file to block the
canary. It writes JSONL audit rows to `data/spot-canary.jsonl` by default.

## Guarded Live Spot Allocator

The live spot allocator is a foreground-only Phase 18 command. Its first
version supports one direct spot market order per invocation and must not be
installed as a background service. It exists to execute reviewed allocation
intent with hard risk limits, benchmark valuation, balance reconciliation, safe
Binance order-test preflight, audit logging, and Telegram alerting.

Plan an allocation without submitting an order:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.live_spot_allocator --plan-only
```

Submit mode requires every confirmation flag, live-mode environment settings,
passing permission diagnostics, and no emergency stop file:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.live_spot_allocator \
  --confirm-live-allocator \
  --confirm-canary-reviewed \
  --confirm-replay-passed \
  --confirm-paper-warmup-reviewed \
  --confirm-permission-diagnostics
```

Default limits are intentionally small: max order notional `6 USDT`, max daily
notional `12 USDT`, max daily orders `2`, direct route only, and BNB reserve
protection. Create `data/live-spot.stop` to block allocator planning and
submission. Audit rows default to `data/live-spot-allocations.jsonl`.

## Claude Autonomous Spot Brain

The Claude Spot runtime decides one Spot action per cycle (hold or one
dynamically routed rebalance of up to three serial legs). It does not modify
the paper-only pool.

### Decision regime (phase 43, calibrated)

The default decider is `RulesEngineAdvisor`: a deterministic rule that picks
the candidate route whose `market_evidence.expected_edge_after_friction_bps`
is the largest among those that pass the market-evidence gate and stay under
the friction cap, otherwise holds. No LLM call.

Selectable via `CLAUDE_SPOT_DECISION_MODE`:

| Value           | Behavior                                              |
| --------------- | ----------------------------------------------------- |
| `rules_only`    | Deterministic only (default).                         |
| `llm_only`      | Legacy LLM-only decider. Kept for back-compat.        |
| `rules_first`   | Rules engine proposes; LLM can only veto or approve.  |

Plan one decision without submitting orders:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.claude_spot --plan-only
```

### Live submit gates

`binance-claude-spot.timer` should stay disabled until live is intentionally
configured. Live submit requires every gate below:

```bash
CLAUDE_SPOT_ENABLED=true
CLAUDE_SPOT_DYNAMIC_SCOPE=all_eligible_spot
USE_TESTNET=false
DRY_RUN=false
CONFIRM_LIVE_TRADING=yes
CLAUDE_SPOT_PROVEN_EDGE=yes    # set ONLY after a positive backtest
```

`CLAUDE_SPOT_PROVEN_EDGE=yes` is the final gate. The runtime refuses live
submission until an operator has run the historical backtest harness and
seen positive risk-adjusted edge net of fees. To bypass the gate entirely,
set `CLAUDE_SPOT_REQUIRE_PROVEN_EDGE=false`, but that is **not recommended**.

### Cost-aware defaults

The deployed code now defaults to cost-aware behavior:
`CLAUDE_SPOT_ALLOW_NEGATIVE_FRICTION_BPS=40` (was 300) and
`CLAUDE_SPOT_MARKET_PICK_MODE=liquid_top_value` (was small_market_aggressive).
The `override_bps_limits` decision field has been removed; there is no
LLM-controlled bypass of the friction or evidence gates.

Binance market rules, available balances, fee handling, Spot
permission/order-test preflight, configured dynamic scope, `data/claude-spot.stop`,
JSONL audit, reconciliation, and halt-on-ambiguous-leg behavior remain
enforced. Margin, futures, transfer, withdrawal, and prediction writes remain
unavailable. The default context is capped to the top `20` dynamic
candidates so one cycle stays inside Binance request-weight limits.

### Backtesting before live

Run the historical replay harness to measure edge after modeled fees, spread,
and slippage:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.claude_spot_backtest \
  --symbols BTC/USDT,ETH/USDT,SOL/USDT,BNB/USDT \
  --interval 1h \
  --start 2026-01-01 --end 2026-05-01 \
  --output data/claude-spot-backtest.json
```

The JSON output includes a `verdict`:

- `edge_likely` — Sharpe > 1 and total return > 0. Plausibly safe to promote.
- `edge_marginal` — positive return, modest Sharpe. Not enough.
- `no_edge` — strategy did not clear its cost bar. Do **not** set `PROVEN_EDGE=yes`.
- `no_trades` / `no_data` — insufficient window or signals; rerun with more data.

## Margin Dry Run

The margin module is Phase 19 dry-run modeling only. It may read Binance margin
account, max-borrow, and next-hour interest data, then locally models borrow
sizing, debt, interest, repayment, margin level, and liquidation buffer. It has
no borrow, repay, margin-order, or margin-transfer submission path.

Run discovery/modeling without live write actions:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.margin --dry-run --asset USDT --borrow-amount 10
```

Isolated margin modeling requires a symbol:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.margin \
  --dry-run \
  --account-type isolated \
  --isolated-symbol BNB/USDT \
  --asset BNB \
  --asset-usdt-price 600 \
  --borrow-amount 0.01
```

The `--confirm-live-margin` flag intentionally fails. Live margin activation is
future work and requires a separate approval phase.

## Margin Canary Plan

The Phase 24 canary slice is foreground and plan-only. It enforces local
symbol scope, margin risk limits, a documented repayment intent, an emergency
stop, and JSONL audit output. It does not expose margin submission:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.margin_canary --plan-only \
  --margin-symbol BNB/USDT --asset USDT \
  --borrow-amount 1 --repay-amount 1
```

`--submit-action` and `--confirm-live-margin` intentionally fail closed.

## Futures Dry Run

The futures module is Phase 20 USD-M dry-run modeling only. It may read Binance
USD-M account, position-risk, exchangeInfo, premiumIndex funding/mark price,
symbol-config, and leverage-bracket data, then locally models leverage, margin
mode, funding, liquidation distance, reduce-only behavior, precision/minimums,
and position reconciliation. It has no futures order, leverage-change,
margin-mode, position-mode, or position-margin submission path.

Run discovery/modeling without live write actions:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.futures \
  --dry-run \
  --symbol BTC/USDT \
  --side BUY \
  --quantity 0.001
```

The `--confirm-live-futures` flag intentionally fails. Live futures activation
is future work and requires a separate approval phase.

## USD-M Futures Canary Plan

The Phase 25 canary slice is foreground and plan-only. It requires a
reduce-only exit intent and audits risk/reconciliation output without exposing
a futures order path:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.futures_canary --plan-only --symbol BTC/USDT \
  --side BUY --quantity 0.001 --exit-side SELL --exit-reduce-only \
  --exit-trigger "operator approved close"
```

`--submit-order` and `--confirm-live-futures` intentionally fail closed.

## Transfer Dry Run

The transfer module is Phase 21 dry-run planning only. It may read Binance
wallet summary, spot user assets, funding assets, margin account, USD-M futures
account, and transfer history data, then locally models a proposed internal
transfer with route constraints, source/destination balances, wallet activation,
margin debt checks, and resulting allocation state. It has no universal-transfer
submission path.

Plan and audit an internal transfer without live write actions:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.transfers \
  --dry-run \
  --source-wallet spot \
  --destination-wallet funding \
  --asset USDT \
  --amount 5
```

The `--confirm-live-transfer` flag intentionally fails. Live transfer automation
is future work and requires a separate approval phase.

## Universal Transfer Canary Plan

The Phase 26 canary slice is foreground and plan-only. It enforces local asset
scope, wallet/balance/debt limits, isolated symbol requirements, emergency
stop, and audit output without exposing transfer submission:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.transfer_canary --plan-only \
  --source-wallet spot --destination-wallet funding --asset USDT --amount 1
```

`--submit-transfer` and `--confirm-live-transfer` intentionally fail closed.

## Unified Pool Valuation

The unified pool module is Phase 22 local valuation only. It combines spot,
funding, margin, futures, transfer-availability, debt, interest, open PnL,
reserves, leverage, liquidity, volatility, and benchmark prices into one dry-run
allocation report. It does not submit orders, margin actions, futures actions,
or transfers.

Value a local holdings/prices fixture:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.pool.unified \
  --dry-run \
  --holdings-json holdings.json \
  --prices-json prices.json
```

## Advanced Product Modules

The Phase 23 product-module registry is local metadata and gating only. It
tracks prediction trading, withdrawals, live margin/futures/transfer activation,
and future Binance products as optional modules. API-key permission alone never
authorizes these modules.

Inspect the registry without exchange writes:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.product_modules
```

Every module must define its strategy interface, permission mapping, market-data
source, risk model, audit trail, tests, and activation gate before a future
activation phase can be considered.

Review advanced-product evidence and the serial live-canary rule locally:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.advanced_readiness
```

## Manual Live Probe

The one-shot live probe is a separate foreground real-order test. It submits at
most one direct `ASSET/USDT` market sell, writes `data/live-probes.jsonl`, sends
a Telegram message labeled `LIVE PROBE`, and exits. Before submitting the real
order, it runs Binance's safe spot order-test endpoint for the selected order.
It is not a pool allocator and must not be run as a service.

Preflight without placing an order:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.live_probe
```

The preflight must fail closed unless `--confirm-real-order` is present. Only
run the real probe after confirming `.env` has `USE_TESTNET=false`,
`DRY_RUN=false`, and `CONFIRM_LIVE_TRADING=yes`:

```bash
cd /home/user/projects/binance-bot
.venv/bin/python -m src.live_probe --confirm-real-order
```

The pool ledger records the latest full catalog decision set, per-cycle
eligibility summaries, a rolling benchmark-observation window, virtual
holdings, historical portfolio snapshots, and paper decisions. With the
default 15-minute cycle and `POOL_MOMENTUM_OBSERVATIONS=96`, a `hold` decision
is expected for roughly the first 24 hours and whenever a route/risk gate does
not pass.

## Paper Ledger Backups

The paper backup timer retains 35 verified daily SQLite snapshots, enough to
protect recent paper evidence, canary analysis, and rollback investigations.

```bash
systemctl --user status binance-pool-paper-backup.timer --no-pager
BINANCE_BOT_DB_SETTING_KEY=POOL_DB_PATH \
BINANCE_BOT_DB_DEFAULT_PATH=data/pool-paper.db \
~/.local/share/binance-bot/scripts/backup_db.sh
```

Restore paper state only while the paper service is stopped:

```bash
systemctl --user stop binance-pool-paper.service
BINANCE_BOT_SERVICE_NAME=binance-pool-paper.service \
BINANCE_BOT_SERVICE_SCOPE=user \
BINANCE_BOT_DB_SETTING_KEY=POOL_DB_PATH \
BINANCE_BOT_DB_DEFAULT_PATH=data/pool-paper.db \
~/.local/share/binance-bot/scripts/restore_db.sh \
~/.local/share/binance-bot/backups/pool-paper-YYYYMMDDTHHMMSSZ-PID.db
systemctl --user start binance-pool-paper.service
```

## Legacy Live State

Historic live-order data and its backups are retained for audit/recovery. The
old user and system trading units must remain inactive and disabled. The manual
live probe, spot canary, and guarded live spot allocator are foreground-only
tools; none may be run as a background automatic asset-conversion runtime.

## Live Readiness Gate

Do not implement or activate real pool reallocations until all of these gates
are complete and explicitly approved as a separate live-execution phase:

- The replay suite passes default and stress fixtures.
- The paper pool runs on current live Binance data through scorer warm-up and
  records current benchmark, drawdown, route-cost, and eligibility behavior.
- API permission diagnostics pass for the exact feature being activated.
- A tiny spot live canary has passed max order notional, max daily notional,
  max daily order count, emergency stop, audit, reconciliation, and Telegram
  alert checks.
- The canary uses dynamic balances, Binance market rules, authenticated fees,
  precision, minimum notional, spread, liquidity, and slippage limits.
- Any permission error, unexpected route, audit failure, balance mismatch,
  slippage breach, or ambiguous exchange response stops live execution.
