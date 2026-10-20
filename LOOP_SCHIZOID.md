# Loop ledger: "schizoid" strategy personas, backtested against the v13 baseline (2026-10-20)

STATUS: OPEN

Source: the 2026-10-20 guided-goal interview. The operator said every change made the bot worse and asked for a "schizoid" bot covering all four proposed modes (contrarian, detached, multi-persona vote/rotate, high-variance). They want backtests only and will judge the results themselves. The agreed draft sets these limits:
- 4 modes plus an ensemble, compared against the current v13 config over the same 60-day liquid-universe window.
- The report goes to `reports/schizoid_backtest_<date>.md` plus a CSV, with net PnL after fees, max drawdown, trade count, win rate and Sharpe.
- No live or paper trading, and the live trading path is not touched.
- Stop after 5 failed attempts to get the backtest running.

The setup agent checked the following against the code, the on-disk caches and the Pi:
- Scouts: `agent://TraderV13` and `agent://BacktestInfra`.
- Read directly: `exit_plan.rs`, `market_mood.rs`, the Cargo deps, the cache files, and the Pi env and audit (read-only).

This file is the loop's memory and its stop switch. The gate `~/projects/seref_omp_tools/seref_loop/bin/seref-loop-gate LOOP_SCHIZOID.md` is run by `/seref_manager run LOOP_SCHIZOID.md` or by `/loop --until`. It stops the loop only when all of these hold:
- the line above reads `STATUS: DONE`;
- no `- [ ]` item is left;
- every `- [x]` carries `DONE:` or `INVALID:` evidence;
- every `- [!]` carries a `BLOCKED:` reason.

Background (measured 2026-10-20 05:43 UTC):
- **The live trader cannot be backtested.** It is an LLM agent: omp RPC, `openai-codex/gpt-6.1-sol`, 20-min cycles, prompt `bot-spot-v14-agentic`. That prompt is v13 plus an information-only exit-plan line (`crates/bb-trader/src/config.rs:17-25`).
  - Cost: replaying 60 days is about 4,320 cycles × ~$0.135 ≈ $580 notional, far over the subscription caps (`PROGRESS.md:857-860`). The fallback model was removed on 2026-10-19.
  - Look-ahead: cycle tools and breadth read live data with no as-of clock (`market_mood.rs:311-355`, `tools.rs:62-185`).
  - **So every variant here is a deterministic rule persona, and the "v13" row is a rule proxy of the v13 guidance.** The report must say so.
- **No live persona switch exists** (no strategy enum; the only selector is `MarketPickMode`, `config.rs:58-90`). Adding one would touch the live path, which is out of bounds. Personas are therefore selectable only in the backtest command (`--persona`); the live default cannot change.
- **Live v13 config** (Pi `~/binance-bot/.env.docker`, read 2026-10-20):
  - `BOT_SPOT_MARKET_PICK_MODE=liquid_top_value`, `BOT_SPOT_CANDIDATE_LIMIT=10`;
  - `BOT_SPOT_DEFAULT_EXIT_PLAN=atr=3.5:1h,deadline=360`;
  - `BOT_SPOT_MIN_QUOTE_VOLUME_USDT=100000`, `BOT_SPOT_MAX_SPREAD_BPS=50`, `BOT_SPOT_MARKET_EVIDENCE_ENABLED=true`;
  - `--interval-seconds 1200` (`docker-compose.yml:69`).
  - The trader was up and holding 14.95 USDT in cash; there are 900 audit rows.
- **Reusable pure pieces in `bb-trader`:**
  - `exit_plan::ExitWatch::new(plan, entry_price, entry_ms, atr, fee_rate)` and `on_bar` (`crates/bb-trader/src/exit_plan.rs:563,680`). Bars hit their low before their high.
  - `ExitPlan::parse_spec` (`:282`), `average_true_range` (`:422`), `ATR_PERIODS` = 22.
  - ATR is fetched the way `exit_replay.rs:698-740` does it.
  - `market_mood::breadth_at` and `score` are private (`market_mood.rs:221,250`). `mood()` scores only the newest bar (`:262-308`). Constants: `LEAN_SCORE` 15, `STRONG_SCORE` 50, `FLAT_BAND_PCT` 0.1, `HORIZONS` 15m/1h/4h/24h = 1/4/16/96 bars of 15m.
  - `bb-strategies` does not depend on `bb-trader` (`crates/bb-strategies/Cargo.toml:10-11`), and `bb-trader` does not depend on `bb-strategies`. So the persona backtest lives in `bb-trader` next to `exit_replay` (the precedent for research inside the trader crate).
- **Data on disk** (`data/` is gitignored):
  - `data/entry-study-cache/1m_<SYM>_USDT_1784448780_1792320180.json`: 32 files, ≈292 MB, rows `{t,o,h,l,c,v}`, 2026-07-19T08:13Z → 2026-10-18T10:43Z.
  - BTC has 131,191 rows. HYPE has only 34,544 rows and starts 2026-09-24T11:00Z (listed late).
  - The 20 "liquid" coins (entry-study top 20 by volume on 2026-10-18): BTC ETH ZEC SOL NEAR XRP BNB WLD HYPE DOGE SUI SAND TRX UNI ENA STRK TAO HBAR TIA LINK.
  - The 12 bot-only coins: BCH AAVE ICP APT RENDER LTC INJ STX FIL PENDLE OP LDO.
  - **The window is 2026-08-19T10:43Z → 2026-10-18T10:43Z (60 days, `--end 2026-10-18T10:43:00Z --days 60`), fully offline.** It ends before the Phase 47 deploy (2026-10-18 14:02Z), so the proxy cannot be compared with v13's live trips on the same dates.
- **Costs:** real fills average 7.5 bps per side and 14.8 bps per round trip over 40 trips (`LOOP_MONEY.md:11,94`). The default model here is 7.5 bps fee + 5 bps slippage per side = 25 bps per round trip, which is deliberately worse than measured.

## Protocol (every iteration)

1. Read this file and `CODEX.md` (binding live-money rules), then the newest entries in `PROGRESS.md`. Take the first `- [ ]` item in order: sections A, B, C, D in order, and respect the "do X first" dependencies named in items.
2. Before editing, read the code the item names and re-check that the finding still holds. Already fixed or wrong: mark `- [x] … — INVALID: <evidence>`.
3. Fix it at the root with the project's existing patterns; migrate every caller; update the docs that describe the changed behavior in the same step.
   - Code goes in `crates/bb-trader/src/persona_backtest/` (`mod.rs`, `data.rs`, `engine.rs`, `personas.rs`, `report.rs`; split differently only if a file passes ~800 lines). Register it in `crates/bb-trader/src/lib.rs`.
   - Add one subcommand variant in `crates/bb-trader/src/cli.rs`, exactly as `bot-spot-exit-replay` is wired.
   - The persona engine uses no Binance client and no network. Unit tests live in inline `#[cfg(test)] mod tests` (the crate convention; there is no `tests/` dir).
   - Use `rust_decimal::Decimal` at the `ExitWatch` boundary. `f64` is fine inside signals and metrics.
4. Verify on the laptop in the repo root:
   - `cargo test -p bb-trader`.
   - `cargo test -p bb-cli` (it holds `subcommand_tree_is_consistent`, `crates/bb-cli/src/main.rs:86-92`) when the CLI changes.
   - Then `cargo test --workspace` (last recorded: 1010 passed), `cargo clippy --workspace --all-targets -- -D warnings`, `scripts/smoke.sh`, `scripts/pool_replay.sh` and `git diff --check`.
   - Add a regression test for each defining behavior. Tests must catch behavior, not wording.
   - Smoke the changed path where it runs: the laptop, offline, with `cargo run --release -p bb-cli --bin bb -- bot-spot-persona-backtest --cache-dir data/entry-study-cache --end 2026-10-18T10:43:00Z --days 60 …` once C1 exists. Before that, use the item's unit tests plus a throwaway `cargo test -p bb-trader -- --ignored` style run on the real cache, deleted afterwards.
5. Deploy: none. Nothing in this ledger goes to the Pi (`pi-wired`); the live `bot-spot` keeps its current image.
   - Do not run `docker compose build`, `up`, `rsync` or `scp` to the Pi.
   - Do not edit the Pi's `.env.docker`, the stop file, `docker-data`, `omp-agent` or the systemd units.
   - Read-only `ssh pi-wired` checks (env grep without secrets, `docker ps`, audit `tail`) are allowed. There is no forbidden window, because nothing is deployed and the Pi is only read. Still avoid heavy reads on the Pi between 23:10 and 23:55 UTC, when its backups run.
   - Back up any DB before a schema change or backfill (none is expected here).
6. Edit this file: `- [ ]` -> `- [x]` and append ` — DONE: <files>; <test/smoke evidence>`.
   One item per iteration is fine; finish an item completely or leave it `- [ ]`.
7. Blocked only for a user decision or an unreachable host/credential. For a decision, call the `ask`
   tool. Nobody may be at the keyboard, so under `/seref_manager` the loop manager decides it for the
   user, and its answer is the user's decision (also where a rule reserves a choice for the user or
   operator): act on it instead of marking the item blocked. Never end a turn waiting for a reply.
   Mark a blocker `- [!] … — BLOCKED: <exact missing thing, what was tried>`. Difficulty, size or
   scope pressure is not a blocker.
   - Goal stop conditions, from the interview:
     - halt and `ask` if a needed piece of data or access would require new paid data, API keys or exchange access;
     - halt and `ask` if any change would touch the live trading path (any file listed under Rules below, or anything that places orders);
     - halt and `ask` if a persona spec is ambiguous in a way that materially changes results;
     - after 5 failed attempts to get the C2 backtest run to exit 0 with a complete report, mark C2 `- [!]` with the failure log.
   A `— MANAGER(itN): …` note is the loop manager's review: an item it reopened stays open until
   you fix what the note names and tick it again with new evidence; a decision in the note is the
   user's decision.
8. When no `- [ ]` is left, run the final check:
   - every test suite: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `scripts/smoke.sh`, `scripts/pool_replay.sh` and `git diff --check`;
   - a fresh full persona backtest run (the C2 command), which must exit 0 and reproduce the committed CSV byte for byte (`sha256sum`).

   Then replace the STATUS line with `STATUS: DONE`, append a summary (done / invalid / blocked) at the bottom, and run the gate command above. It must exit 0; if it does not, fix what its message names before yielding.

Rules:
- **Commits:** the tree was clean on 2026-10-20 except the untracked `LOOP_BOT_HEALTH.manager.md`, which belongs to the previous loop: never stage or delete it.
  - Commit after each finished item with explicit paths (`git add <paths>`, never `git add -A`/`.`), using a message in the repo's loop style: `loop(<ID>): <what>`.
  - If `git status` shows modified files you did not touch, they are the user's: leave them unstaged and do not commit over them.
- **Do not touch (live trading path)**:
  - `crates/bb-trader/src/{cycle,executor,agent,tools,exit_guard,exit_book,exit_watch,config,universe,scope,audit,context,journal,memory,trades,evidence,alerts}.rs`;
  - `crates/bb-omp/`, `crates/bb-pool/`, `crates/bb-binance/`;
  - `docker-compose.yml`, `Dockerfile`, `deploy/`, `.env*`, the Pi;
  - the `bot-spot` subcommand's behavior in `cli.rs`.
- **Allowed shared edits:**
  - `crates/bb-trader/src/market_mood.rs`: additive only, existing functions' output unchanged (A3);
  - `crates/bb-trader/src/cli.rs` and `lib.rs`: one new subcommand or module registration;
  - `exit_plan.rs`: read-only use.
- Never call a Binance endpoint from the persona backtest. Never place, test or simulate an order through `execute_route`, `Runtime::exit_sell` or any `bb-pool` order path.
- Docs go only in `README.md`, `OPERATIONS.md`, `IMPLEMENTATION_PLAN.md`, `PROGRESS.md` and `AGENTS.md` (active-work line). `CODEX.md` changes only if a live-money rule's wording must change, which this goal should not need.
- No destructive commands on live data without a backup.

## A. Plan and backtest foundation

- [x] A1 The roadmap has no section for this work, though `CODEX.md:26-27` requires checkbox items before a roadmap change (`IMPLEMENTATION_PLAN.md:1107`, last section `## 47.`). Add `## 48. Schizoid Strategy Personas — Backtest Only (operator request 2026-10-20)` with why (each live change made results worse), scope (deterministic personas plus a v13 proxy, offline 60-day backtest, report only), non-goals (no live persona switch, no paper run, no deploy) and one checkbox per item of sections A–D. Acceptance: the section exists with unchecked boxes matching this ledger's items; `git diff --check` exits 0. — DONE: `IMPLEMENTATION_PLAN.md:1197-1261` adds `## 48.` before "Post-Implementation Handoff", with why/scope/non-goals and 15 boxes tagged A1–D2 (`sed -n 1197,1261p | grep -c '^- \[[ x]\]'` = 15; IDs A1, A2…D2 all present); A1's own box is ticked per `CODEX.md:28`, the other 14 are unchecked; `git diff --check` exit 0.
- [ ] A2 `bb-trader` has no offline loader for the entry-study cache (`crates/bb-strategies/src/entry_study.rs:78-115,447-487` reads it, but `bb-trader` cannot depend on `bb-strategies`; rows `{t,o,h,l,c,v}`, 32 files). Add `persona_backtest/data.rs`: parse `1m_<SYM>_USDT_<start>_<end>.json` into per-symbol bars keyed by open time, sorted and deduplicated; aggregate to 15m and 1h bars by open-time floor; expose trailing 24h quote volume (`Σ v·c` over the last 1,440 closed 1m bars) and window returns over closed bars; keep each symbol's own first bar (HYPE starts 2026-09-24T11:00Z) and leave a coin out wherever it has no bar. Acceptance: unit tests on an inline fixture cover aggregated OHLC, gaps and the quote-volume sum. A throwaway real-cache run prints 32 symbols, BTC with 131,191 bars and HYPE's first bar at 2026-09-24T11:00Z.
- [ ] A3 Breadth is only scorable for the newest bar (`crates/bb-trader/src/market_mood.rs:221,250,262-279`): `mood()` calls the private `breadth_at(closes, now)` and `score`, so a backtest cannot score the 20-coin breadth at a past time without copying code. Add an additive public wrapper over `breadth_at` and `score`, e.g. `pub fn breadth_score_at(closes: &[HashMap<i64, f64>], t: i64) -> (Vec<HorizonBreadth>, Option<f64>)`. Leave `mood()`'s behavior unchanged. In the backtest, the newest 15m bar at decision time T is the partial bar built from the 1m bars closed by T. That mirrors live, where the newest kline is in progress, and never reads a bar closing after T. Acceptance: a test shows the wrapper at the newest open time equals `mood(...).score` and `.horizons` on the existing fixture; the 9 existing `market_mood` tests still pass.
- [ ] A4 No portfolio engine runs rule personas over many symbols with the exit-plan menu: `ExitWatch` (`crates/bb-trader/src/exit_plan.rs:563,680`) is used only by `exit_replay` and `exit_watch`. Add `persona_backtest/engine.rs` (do A2 and A3 first). **Clock:** a step every `--step-minutes` (default 20, the live cadence). **Persona trait:** returns intents (buy coin X with a size fraction, sell held coin Y) from a point-in-time view: closed 1m bars, partial 15m bars, breadth at T, liquid top 10 at T. **Portfolio:** USDT cash plus up to N positions, each with an optional `ExitWatch`. **Fills:** intents fill at the next 1m open ± slippage; fee `--fee-bps` (default 7.5) and `--slippage-bps` (default 5) apply per side; orders under 5 USDT notional are skipped; exit-plan sells fill at the `on_bar` `ExitFill` price, minus fee and slippage; ATR comes from the 22 closed 1h bars before entry, as in `exit_replay.rs:725-737`. **Equity:** marked to market at every 1m close. **Metrics:** net PnL in USDT and %, after all fees; max drawdown % on the 1m equity; round-trip count; win rate: % of round trips with net > 0 after both fees; Sharpe: mean/pstdev of step-equity returns × √(525600/step-minutes); fees paid in USDT; a per-trade list (symbol, entry/exit time and price, reason, net PnL). Acceptance, as tests: (i) a one-trade fixture's net PnL equals a hand calculation with both fees and slippage, to 1e-9; (ii) look-ahead: a series truncated right after step T gives the same intents up to T as the full series; (iii) max drawdown on a known equity path; (iv) the sum of per-trade net PnL equals ending minus starting equity when the run ends flat.
- [ ] A5 The `v13` baseline must be a deterministic proxy, because the LLM cannot be replayed (Background). Live config: `liquid_top_value`, `CANDIDATE_LIMIT=10`, `atr=3.5:1h,deadline=360` (Pi `.env.docker`, 2026-10-20). The Phase 47 entry finding is at `PROGRESS.md:189-199`. Add persona `v13` in `persona_backtest/personas.rs` (do A4 first); its doc comment says it is a proxy. **Candidates:** the top 10 of the 20 liquid coins by trailing 24h quote volume at T. **Buy:** only when flat, the breadth score is ≥ +15 (`LEAN_SCORE`) and the 4h median return is ≥ +1.0%. Take the highest 4h return among candidates with positive 1h and 4h returns, with 100% of cash. **Exit:** the plan `atr=3.5:1h,deadline=360` via `ExitWatch`, plus a step-time sell when the breadth score is ≤ −15. Acceptance, as fixture tests: no buy at a score of +14.9 or a 4h median of +0.9%; it picks the strongest qualifying coin; a regime-turn sell happens at −15; a deadline sell happens at 360 min.

## B. Schizoid personas

Each persona implements the A4 trait in `persona_backtest/personas.rs`, with its rules as its doc comment. The defaults below are the setup agent's; the user may veto any of them before the run.

- [ ] B1 There is no contrarian mode, and every past variant followed momentum or breadth (`LOOP_MONEY.md:12`: all 40 live entries came after a 30-min rise). Add persona `contrarian`, which fades breadth: buy weakness, sell strength (do A5 first). **Universe:** the liquid top 10, as in `v13`. **Buy:** when flat and the breadth score is ≤ −15, the candidate with the lowest 4h return, with 100% of cash. **Exit:** the plan `stretch=2:1,deadline=360`, plus a step-time sell when the held coin's 1h return is ≥ +1.5% or the breadth score is ≥ +15. Acceptance, as fixture tests: it buys the worst 4h coin under bearish breadth; no buy at −14.9; it sells on a +1.5% 1h move; it sells when the score flips to +15.
- [ ] B2 There is no detached mode: Phase 45 exit plans and the Phase 47 breadth and liquid-universe filters frame every decision (`crates/bb-trader/src/universe.rs:229-257`, `market_mood.rs`). Add persona `detached`: no breadth, no exit-plan engine, no liquid filter, raw price action only. **Universe:** all 32 cached coins with bars at T. It never reads breadth or 24h volume. **Buy:** when flat, the coin with the highest 1h return, if that return is > 25 bps (one modeled round trip) and its last 15m return is > 0, with 100% of cash. **Sell:** at a step, when the held coin's 15m and 1h returns are both < 0. **Rotate:** sell, then buy the best coin, when another coin's 1h return beats the held coin's by > 50 bps. Acceptance, as fixture tests: the intents are identical with breadth forced to −100 and to +100; it can pick a non-top-20 coin (INJ); it rotates at a +51 bps gap and holds at +49 bps.
- [ ] B3 There is no high-variance mode: live sizing is all-in or nothing with one position (`PROGRESS.md:12`, wallet ~17.8 USDT). Add persona `high_variance`: more trades, wider stops and swinging size. **Universe:** the liquid top 10. **Buy:** at every step, any non-held candidate with a 1h return ≥ +0.5%, regardless of breadth, up to 3 concurrent positions. **Size:** `clamp(|1h return| / 2%, 0.25, 1.0) × equity`, capped by cash. **Exit:** the plan `atr=6:1h,deadline=1440` only; no step-time sells. Acceptance, as fixture tests: size 0.25 at +0.5% and 1.0 at +2% or more; a 4th candidate is refused while 3 positions are open; an ATR-6 trail does not fire on a drop that ATR-3.5 sells.
- [ ] B4 There is no multi-persona rotation (the "rotate" half of the operator's multi-persona choice). Add persona `rotation` (do A5, B1, B2 and B3 first). It shadow-runs `v13`, `contrarian`, `detached` and `high_variance` on the same engine, reading only shadow equity up to T. At each 00:00 UTC boundary it activates the persona with the best trailing-72h shadow net return. Ties and the first 72h go to `v13`. On a switch it closes the previous persona's positions at the next 1m open (both fees), then follows the new persona's intents. Acceptance, as tests: the switch picks the best trailing-72h shadow; a switch closes positions and charges fees; the A4 look-ahead truncation test holds for `rotation`.
- [ ] B5 There is no multi-persona vote (the "vote" half). Add persona `ensemble` (do A5, B1, B2 and B3 first). It holds one position with 100% of cash. **Votes:** at each step, each of the four base personas emits intents for the current state, one vote per coin. **Buy:** when flat, the coin with ≥ 2 buy votes. Ties go to the most votes, then to the highest 24h quote volume. **Sell:** when ≥ 2 personas vote sell on the held coin, or when the `v13` plan `atr=3.5:1h,deadline=360` fires. Acceptance, as fixture tests: no buy with 1 vote; a buy with 2; the tie-break orders by votes, then volume; it sells on 2 sell votes; an exit-plan sell happens with 0 votes.

## C. Runner, report and checks

- [ ] C1 There is no command to run the personas: `crates/bb-trader/src/cli.rs:24,62` wires only bot-spot, exit-replay and exit-watch. Add `bb bot-spot-persona-backtest`, wired like `bot-spot-exit-replay` (do B1–B5 first). **Flags:** `--cache-dir` (default `data/entry-study-cache`); `--end` (required, ISO); `--days` (default 60); `--persona <name>`, repeatable; `all` means `v13,contrarian,detached,high_variance,rotation,ensemble`; `--starting-balance` (default 1000); `--fee-bps` (default 7.5); `--slippage-bps` (default 5); `--step-minutes` (default 20); `--output-dir` (default `reports`). **Outputs:** `reports/schizoid_backtest_<end-date>.md`: window, assumptions, one row per variant, caveats; `.csv` with columns `variant,net_pnl_usdt,net_pnl_pct,max_drawdown_pct,round_trips,win_rate_pct,sharpe,fees_usdt`; `_trades.csv`: one row per trade. **Missing data:** fails with a clear "missing cache" error and never fetches. Acceptance: `cargo test -p bb-cli` (subcommand tree) and `cargo test -p bb-trader` pass; `bb bot-spot-persona-backtest --help` lists every flag; a run with an empty `--cache-dir` exits non-zero with a missing-cache message.
- [ ] C2 No backtest result exists yet. Window: 2026-08-19T10:43Z → 2026-10-18T10:43Z, inside the on-disk cache `data/entry-study-cache` (2026-07-19T08:13Z → 2026-10-18T10:43Z). Do C1 first. **Run:** `cargo run --release -p bb-cli --bin bb -- bot-spot-persona-backtest --cache-dir data/entry-study-cache --end 2026-10-18T10:43:00Z --days 60 --persona all`. **Caveats the `.md` must state:** the `v13` row is a rule proxy, not the LLM; survivorship: the universe is the top 20 as of 2026-10-18; the cost is 25 bps per round trip, against a measured 14.8; the window ends before the Phase 47 deploy; there is no out-of-sample split. **Commit:** the three report files. **Stop condition:** after 5 failed attempts to get a complete run that exits 0, mark this `- [!]` with the error output. Acceptance: exit 0, with no network access and no `ERROR` line; `reports/schizoid_backtest_2026-10-18.md`, `.csv` and `_trades.csv` exist; the CSV has exactly 6 data rows (`v13`, `contrarian`, `detached`, `high_variance`, `rotation`, `ensemble`) with every metric filled; a second run gives the same `sha256sum` for all three files.
- [ ] C3 Nothing cross-checks the report, so an engine bug could pass as a result (C2 output). Do C2 first. For every variant: recompute net PnL from `_trades.csv` (realized trades plus the window-end mark-to-market of any open position) against `net_pnl_usdt`; recompute the win rate and the round-trip count; check that `fees_usdt` ≈ Σ notional × 7.5 bps; for any variant with 0 round trips, explain in the `.md` which rule never fired and over how many steps. Acceptance: differences ≤ 1e-6 USDT and exact counts, recorded in the DONE note. A mismatch is fixed at the root and C2 is re-run.

## D. Docs and handoff

- [ ] D1 The new command is undocumented (`README.md:107-112` documents `bb bot-spot-exit-replay`; `OPERATIONS.md:590-601` documents `bb entry-study`). Do C2 first. **`README.md`:** one usage line plus "research only, offline, never touches the live trader". **`OPERATIONS.md`:** a short section with one line per persona rule, the cost model, the cache it needs and the exact C2 command. Acceptance: both files show `bb bot-spot-persona-backtest` with flags that match `--help`; `git diff --check` exits 0.
- [ ] D2 The hand-off files do not mention Phase 48 (`IMPLEMENTATION_PLAN.md`, `PROGRESS.md:6-8`, `AGENTS.md` active-work line). Do D1 first. Tick the Phase 48 boxes in `IMPLEMENTATION_PLAN.md`. Add a dated top entry in `PROGRESS.md`: the six-row results table from the CSV, the report paths, the caveats, the commands run and the test counts. In `AGENTS.md`, set the active-work line to: Phase 48 backtest delivered, awaiting operator review, nothing deployed. Acceptance: the `PROGRESS.md` table matches the CSV; `git diff --check` exits 0.

## Noticed, no item (outside this goal's boundaries; for the operator)

- The laptop `.env.example` is stale against the live Pi: `BOT_SPOT_MARKET_PICK_MODE=small_market_aggressive` (`:117`), blank `DEFAULT_EXIT_PLAN` (`:110`), `CANDIDATE_LIMIT=5`. The `config.rs:750-752` test still asserts `deadline=120`. The Pi uses `liquid_top_value`, 10 and `atr=3.5:1h,deadline=360`. These are config/live-path files, so they are out of scope here.
- The Phase 47 basket is the top 20 by current volume, both live and in this cache, so every backtest over the cache has survivorship bias. C2 states it; fixing it would need historical volume ranks for coins outside the cache, which means new data.
