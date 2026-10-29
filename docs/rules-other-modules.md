# Live-Money Rules For Modules Other Than Bot Spot

Binding, like `CODEX.md`. Read this before changing the legacy runtime, the
paper pool, the probe, canary or allocator commands, the capital pool, or the
margin, futures, transfer, unified-pool, withdrawal, prediction or
advanced-product modules.

- The legacy strategy runtime (`bb main`) must not run fixed-size orders on a
  real-money account. Keep the `binance-bot.service` user unit and the
  `main-bot` compose service disabled unless a reviewed balance-aware live mode
  is deliberately approved.
- The modular asset-pool runtime (`bb pool`, crate `bb-pool`) is paper-only. It
  may read balances, markets, and authenticated commission schedules, but it
  must not acquire a live order-submission path.
- Paper decision reports must open the paper ledger read-only. Historical-candle
  research replay must write only to an isolated replay database, use no live
  order path, and must not replace the Phase 13 live-market warm-up gate.
- Market selection, conversion routing, minimum-order checks, fee estimates,
  and slippage must be derived from current Binance market data and rules, not
  from coin-specific constants.
- Do not plan or enable continuous real pool reallocation until the replay
  suite, live-market paper warm-up, permission diagnostics, and tiny capped
  live canary results have been reviewed and explicitly approved as a separate
  live-execution phase.
- A manual one-shot live probe may exist only as an explicitly confirmed
  foreground command. It must not be wired to systemd or to the paper pool
  allocator, and it must submit at most one direct-USDT order per invocation.
- A spot live canary may exist only as an explicitly confirmed foreground
  command. The first canary version is limited to one direct sell into `USDT`
  per invocation, hard daily/order caps, emergency stop checks, safe Binance
  order-test preflight, audit logging, and Telegram alerting.
- The guarded live spot allocator may exist only as an explicitly confirmed
  foreground command. The first allocator version is limited to one direct spot
  market order per invocation, with permission diagnostics, safe order-test
  preflight, risk limits, reconciliation, audit logging, and Telegram alerting.
  It must not be installed as a background service without a separate reviewed
  phase.
- The capital pool (`bb capital-pool`, crate `bb-capital`) keeps a
  deterministic reserve and per-module min/max plan. The bot allocator
  (`--advise`) may only clamp within those deterministic bounds, and any
  allocator failure must fall back to the deterministic plan. Auto-convert
  (`AUTO_CONVERT_TO_QUOTE=true`) may only market-sell `AUTO_CONVERT_ASSETS` to
  cover a quote shortfall after a Binance `order/test`, and must never sell the
  target symbol's base asset.
- The margin module is dry-run/read-only only. It may query margin account,
  max-borrow, and interest data for local modeling, but it must not call margin
  borrow, repay, order, or transfer write endpoints. Live margin permissions
  must not be enabled until a separate activation phase adds controls, tests,
  and explicit approval gates.
- The USD-M futures module is dry-run/read-only only. It may query futures
  account, position, exchange-rule, funding, symbol-config, and
  leverage-bracket data for local modeling, but it must not call futures order,
  leverage-change, margin-mode, position-mode, or position-margin write
  endpoints. Live futures permissions must not be enabled until a separate
  activation phase adds controls, tests, and explicit approval gates.
- The universal transfer module is dry-run/read-only only. It may query wallet,
  spot, funding, margin, futures, and transfer-history data for local transfer
  planning and audit, but it must not call the universal-transfer write
  endpoint. Live transfer permissions must not be enabled until a separate
  activation phase adds controls, tests, and explicit approval gates.
- The unified asset-pool module is dry-run/local valuation only. It may combine
  read-only spot, funding, margin, futures, and transfer-availability data into
  benchmark valuation and allocation-constraint reports, but it must not call
  any live order, margin, futures, or transfer write endpoint.
- Withdrawal and prediction dry-run modules may model local safety/risk state
  only. They must not call withdrawal, prediction order, stake, claim, or
  settlement write endpoints; live activation remains blocked by policy.
- The advanced product-module registry is metadata/gating only. It may describe
  strategy interfaces, permissions, market-data sources, risk models, audit
  requirements, and activation gates, but it must not enable a live write path.
- Every new Binance product capability must start as read-only/dry-run modeling
  or as an explicitly confirmed foreground-only canary. API permission alone
  must never activate live behavior.
- Code for independent advanced-product modules may be developed in parallel,
  but live canary execution that mutates account state must be serial so
  balances, debt, exposure, fees, and reconciliation remain attributable to one
  action at a time.
- Transfer, margin, and futures live canaries must remain separate foreground
  commands until each passes its own audit, reconciliation, emergency-stop,
  Telegram-alert, and explicit approval gates.
- The current margin, USD-M futures, and universal-transfer canary modules are
  plan-only readiness commands. They may run read-only discovery, local scope
  checks, risk modeling, and JSONL audit output, but their submission flags
  must fail closed until a later reviewed live-execution approval.

