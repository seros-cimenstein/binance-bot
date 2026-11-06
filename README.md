# binance-bot

A Binance Spot trading system in Rust: a native REST client with HMAC-SHA256 and Ed25519 request
signing, a rule-driven trader that fails closed, an fsync'd JSONL audit log, offline backtesters
and a read-only localhost dashboard. I ran it live on a Raspberry Pi with a wallet of under
20 USDT in 2026.

**None of its strategies showed an edge.** Every rule, entry study and backtest in `reports/` and
`docs/` either lost after fees out of sample or traded too rarely to judge. The engineering is the
part worth reading; the trading results are published as an honest record, not as a strategy.

## What is in it

| Crate | Purpose |
| --- | --- |
| `bb-core` | env config, `Decimal` money helpers, fsync'd JSONL audit, run locks, stop files |
| `bb-binance` | native Binance Spot REST client: HMAC/Ed25519 signing, time sync, rate-limit tracking, exchange filters, market data, orders, wallet totals, klines history |
| `bb-trader` | `bb bot-spot` (the trader), its offline backtest `algo-backtest`, and `tag-scoreboard` |
| `bb-pool` | market catalog, commission/fee service, multi-leg route engine (library only, no order path) |
| `bb-strategies` | research commands: `bot-spot-backtest`, `spec-snapshot`, `entry-study` |
| `bb-cli` | the `bb` binary |
| `cryptobot-gui` | read-only web dashboard bound to localhost |

Money is `rust_decimal::Decimal` end to end; `f64` only appears in statistics.

Safety checks that are always on, in `bb bot-spot`: an explicit set of env switches to arm live
trading, an emergency stop file checked before every tick, route, leg and order, an exclusive run
lock, account permission checks, Binance `order/test` before every leg, exchange-filter sizing from
live balances net of a fee/slippage reserve, per-leg fill reconciliation that halts on anything
ambiguous, a failed-route halt that needs manual review to clear, and state-vs-balance
reconciliation every tick. Details: [docs/overview.md](docs/overview.md).

## What it trades on

The trader scores coins with a JSON rule file (`config/rules/`) over weighted signals: price
momentum and tags/stance counts from a feed of 4chan /biz/ posts tagged by an LLM. That feed is
produced by a separate project of mine (biz-app) that is **not in this repository**; without a
fresh feed the trader buys nothing. The backtests need cached 1-minute candles and a tags history
that are not committed either.

## Results

- `docs/research/entry-study-2026-10-18.md`: no entry bucket or dip threshold had a positive
  out-of-sample mean with n ≥ 200 and t > 2. A random entry earned about +0.02 to +0.05% gross
  against about 0.21% round-trip cost.
- `reports/phase54/tag_rule_tuning_2026-11-01.md` and `tags2_ab_2026-11-02.md`: 0 of 55 rule files
  passed the bar (net positive in both halves and at least 10 round trips), with either version of
  the tags.
- `docs/archive/loops/LOOP_MONEY.md`: the LLM-driven trader of October 2026 lost 1.07 USDT over
  40 live round trips on a ~17.8 USDT wallet.

`docs/decisions.md` records each decision and why; `docs/archive/` holds the progress logs of the
earlier phases (a Python version, an LLM trader driven through the omp coding agent,
paper pools, a Solana listing sniper, Polymarket and airdrop modules). Those modules were removed
from the tree on 2026-10-29 and remain in git history.

## Build

Rust 1.90+ (edition 2024).

```sh
cargo build --release -p bb-cli          # target/release/bb
cargo build --release -p cryptobot-gui   # target/release/cryptobot-gui
cargo test --workspace
```

Copy `.env.example` to `.env` and fill in your keys; `bb` reads `.env` from the working directory.
Run `bb --help` for the commands. Docker (`docker-compose.yml`, ARM64 and x86_64) and systemd user
units (`deploy/user/`) are described in `OPERATIONS.md`.

## Status and limitations

- Tested only against Binance's production Spot API with one small account. Not security-audited.
- Several documents (`PROGRESS.md`, `OPERATIONS.md`, `LOOP_TAGS.md`, `CODEX.md`, `AGENTS.md`) are
  working notes written for me and for coding agents: they describe my deployment (a Raspberry Pi
  reached as `pi-wired`) and are kept as they were.
- Do not trade real money with it expecting a profit.

## License

MIT, see [LICENSE](LICENSE).
