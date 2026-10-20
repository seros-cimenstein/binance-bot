# Loop ledger: make the live trader schizo (GCP2 mood ring + YOLO), backtest it, then go live (2026-10-20)

STATUS: OPEN

Source: the operator's requests on 2026-10-20 after Phase 48 (`LOOP_SCHIZOID.md`, `reports/schizoid_backtest_2026-10-18.md`).

Operator decisions, quoted:
- "it is not schizo enough". The new bot gets four of the proposed traits: mood swings, random coin flips, argues with itself and extreme bets. Paranoid exits were dropped: "no panic sell".
- "on top of those it looks at gcp2 data for reference that he pulls via webhook or anything that lets him pull realtime data".
- Run it "Backtest, then trade live as schizo". GCP2 roles: "Mood ring" and "YOLO button".
- Go-live bar: "go live no tiny shit whatever we said earlier will be live". It goes live whatever the backtest shows, on the full wallet, with no size cap.
- "ai needs to be schizoified or become schizo thats it". The LLM trader stays and becomes schizo; deterministic rules do not replace it.
- "nothing hardcoded i see you did 3 hours no not like that we are going to be gcp bound in mood switches". Mood switches are driven by GCP2 only: no timer, no fixed switch probability, no fixed seed. The zone bounds come from GCP2's own `getcolorzones`, stored with the data, and the "coin flips" use GCP2 readings as the dice.
- "now that he is a schizo he should live inverted to gcp2 aight if mood is stable on gcp2 make him inverted if not again inverted", then "mood is inverted not yolo". The mood runs opposite to GCP2: when GCP2's zone stays the same between steps he switches mood, and when the zone changes he keeps it. YOLO is not inverted.

This file is the loop's memory and its stop switch. The gate `~/projects/seref_omp_tools/seref_loop/bin/seref-loop-gate LOOP_SCHIZO_LIVE.md` is run by `/seref_manager run LOOP_SCHIZO_LIVE.md` or by `/loop --until`. It stops the loop only when all of these hold:
- the line above reads `STATUS: DONE`;
- no `- [ ]` item is left;
- every `- [x]` carries `DONE:`/`INVALID:` evidence;
- every `- [!]` carries a `BLOCKED:` reason.

Background (measured 2026-10-20 by the setup agent):

GCP2 = Global Consciousness Project 2.0, HeartMath, https://gcp2.net. There is no webhook.
- **Live reads.** These public endpoints answer a bearer token that the site itself publishes in `https://gcp2.net/js/data/api_token.js`. Fetch it at runtime, because it may rotate; on 2026-10-20 it was `5|M1bz…`.
  - `GET https://gcp2.net/api/getcurrentnetvar` returns `{"netvar":[{"netvar":"114.3403"}]}`.
  - `GET /api/getNetVarAggregate24H` returns 1,440 per-minute `{end_epoch, netvar_aggregate}` values covering the last 24 h.
  - `GET /api/getcolorzones` returns the zones Normal 0–140, Elevated 140–166, High 166–219, Very High 219–279, Extreme ≥ 279.
- **History.** `POST https://gcp2.net/data-results/network-coherence-download` (multipart: `_token` CSRF from the download page plus its session cookie, `group_2=Global Network`, `year_2`, `month_2`; no login, no captcha) returns a zip with one CSV per month. Columns are `epoch_time_utc,network_coherence,active_devices`, one row per second. Fetched for 2026: Aug 2,678,401 lines, Sep 2,592,001, Oct 1,641,601 (ends 2026-10-19T23:59:59Z). Data exists from 2024-03 on.
- **The metric.** `netvar_aggregate(E)` is the sum of `network_coherence` over the 3,600 seconds ending at E. Of 972 overlap minutes on 2026-10-19, 409 match exactly (< 6e-4) and 902 (93%) fall in the same colour zone; the median |Δ| is 30.3 and the p95 is 310. In those 16 h live minutes were Normal 852, Elevated 60, High 40, Very High 20, Extreme 0.
- **No evidence that GCP2 predicts prices.** It is the operator's chosen input; the backtest measures it.

Live trader:
- **Trader.** `bot-spot` runs on the Pi (`ssh pi-wired`): LLM `openai-codex/gpt-6.1-sol`, prompt `bot-spot-v14-agentic`, 20-min cycles, wallet ≈ 17.6 USDT in cash on 2026-10-20 05:43Z.
- **Live config.** `BOT_SPOT_DEFAULT_EXIT_PLAN=atr=3.5:1h,deadline=360`, `BOT_SPOT_EXIT_PLANS_LIVE=true`, `liquid_top_value`, 10 candidates.
- **Prompt.** The system prompt is `system_prompt()` (`crates/bb-trader/src/agent.rs:83-136`) and the cycle message is `cycle_prompt()` (`agent.rs:171-~455`); the prompt tag is `config.rs:25`.
- **Exit plans.** Defined in `crates/bb-trader/src/exit_plan.rs` (menu, `ExitWatch`) and `exit_book.rs` (plans set by `set_exit_plan`, the default plan).
- **Phase 48 pieces to reuse.** In `crates/bb-trader/src/persona_backtest/`: `Persona`, `Snapshot`, `run`, `V13`/`Contrarian`/`Detached`/`HighVariance`, `report.rs`.
- **Phase 48 results** (60 days): v13 −18.40%, contrarian −63.80%, detached −91.83%, high_variance +19.38%, rotation −15.98%, ensemble −83.29%.

## Protocol (every iteration)

1. Read this file and `CODEX.md` (binding live-money rules), then the newest entries in `PROGRESS.md`. Take the first `- [ ]` item in order: sections A, B, C, D in order, and respect the "do X first" dependencies named in items.
2. Before editing, read the code the item names and re-check that the finding still holds. Already fixed or wrong: mark `- [x] … — INVALID: <evidence>`.
3. Fix it at the root with the project's existing patterns, migrate every caller, and update the docs that describe the changed behaviour in the same step. That means `README.md`, `OPERATIONS.md`, `CODEX.md` when a rule's wording changes, and `.env.example` for new or removed variables.
4. Verify on the laptop, in the repo root:
   - `cargo test -p bb-trader`, plus `cargo test -p bb-cli` when a subcommand changes.
   - Then `cargo test --workspace` (last: 1047 passed), `cargo clippy --workspace --all-targets -- -D warnings`, `scripts/smoke.sh`, `scripts/pool_replay.sh` and `git diff --check`.
   - Capture exit statuses directly: never pipe `cargo` through `grep`/`head` without saving the status.
   - Add a regression test for each defining behaviour; tests catch behaviour, not wording. Use `wiremock` (already a dev-dependency) for HTTP.

   Smoke the changed path where it really runs:
   - (a) **Backtest:** `unshare -rn cargo run --release -p bb-cli --bin bb -- bot-spot-persona-backtest …` on the laptop, offline, with no network.
   - (b) **GCP2 fetch:** run it once against gcp2.net, which is public and needs no login.
   - (c) **Trader prompt/config:** an isolated run on the Pi that never touches the live session, audit or stop file. On `ssh pi-wired`:

     ```
     cd ~/binance-bot && docker compose run --rm -e BOT_SPOT_SESSION_DIR=data/loop-smoke/session -e BOT_SPOT_AUDIT_PATH=data/loop-smoke/decisions.jsonl -e BOT_SPOT_JOURNAL_PATH=data/loop-smoke/journal.jsonl -e BOT_SPOT_STATE_PATH=data/loop-smoke/state.json -e BOT_SPOT_LOCK_PATH=data/loop-smoke/lock -e BOT_SPOT_STOP_PATH=data/loop-smoke/stop -e BOT_SPOT_EXIT_PLANS_LIVE=false -e BOT_SPOT_EXIT_PLANS_PATH=data/loop-smoke/plans.json -e BOT_SPOT_EXIT_WATCH_STATE_PATH=data/loop-smoke/watch-state.json -e BOT_SPOT_EXIT_WATCH_LOG_PATH=data/loop-smoke/watch.jsonl -e TELEGRAM_BOT_TOKEN= bot-spot bb bot-spot --evidence-only
     ```

     Use `--plan-only` when the LLM turn must run (it records plans and places no order). Read the output, then `rm -rf docker-data/loop-smoke`.
5. Deploy only in C4, never earlier. The host is `pi-wired`, dir `~/binance-bot`, compose service `bot-spot`, env `~/binance-bot/.env.docker`, data `~/binance-bot/docker-data`. The Pi clock shows UTC+3; every time in this file is UTC.
   - **Order:**
     1. Commit.
     2. `ssh pi-wired 'docker tag binance-bot:latest binance-bot:pre-<item>-<YYYYMMDD>'`.
     3. `git ls-files -z | grep -zv '^\.env' | rsync -a --from0 --files-from=- ./ pi-wired:binance-bot/`, and `ssh … rm` any file the commit deleted or renamed.
     4. `ssh pi-wired 'cd binance-bot && docker compose build bot-spot'` (10–17 min).
     5. `cp -p .env.docker .env.docker.bak-<UTC>-<label>` before any env edit.
     6. `docker compose up -d --force-recreate bot-spot`.
   - **Verify** after the deploy:
     - `docker exec bot-spot env | grep -c '^BOT_SPOT_'` (no secrets printed);
     - the startup log lines `keeping the cycle cadence` and `live exit plans enabled`, with no new ` WARN `;
     - the next audit rows' `prompt_version` and `schizo` fields.
   - **Rollback:** `docker tag binance-bot:pre-<item>-<YYYYMMDD> binance-bot:latest`, restore the env backup, recreate.
   - **When it is safe:**
     - Recreate only right after a cycle row has landed in `docker-data/bot-spot-decisions.jsonl`, and at least 4 min before the next one.
     - Never while a route is in flight (a `route` row without its `cycle_summary`).
     - Prefer a moment with no open position.
     - Never create the stop file while a position is open, because it also blocks exit-plan sells (`cycle.rs` `exit_sell`).
     - Never reboot the Pi.
   - **Other services:** do not recreate `scheduler`, and run no heavy builds or prunes, between 23:10 and 23:55 UTC (backups).
   - **Backups:** copy any `docker-data` file to `docker-data/backup-<UTC>-<label>/` before changing it.
6. Edit this file: `- [ ]` -> `- [x]`, and append ` — DONE: <files>; <test/smoke evidence>` on the same line. One item per iteration is fine; finish an item completely or leave it `- [ ]`.
7. Blocked only for a user decision or an unreachable host or credential.
   - For a decision, call the `ask` tool. Nobody may be at the keyboard, so under `/seref_manager` the loop manager decides for the user, and its answer is the user's decision (also where a rule reserves a choice for the user or operator). Act on it instead of marking the item blocked. Never end a turn waiting for a reply.
   - Mark a real blocker `- [!] … — BLOCKED: <exact missing thing, what was tried>`. Difficulty, size or scope pressure is not a blocker.
   - A `— MANAGER(itN): …` note is the loop manager's review. An item it reopened stays open until you fix what the note names and tick it again with new evidence. A decision in the note is the user's decision.
8. When no `- [ ]` is left, run every suite and the final check:
   - the C4 live verification;
   - `docker ps` shows `bot-spot` Up;
   - the newest audit row carries `prompt_version` `bot-spot-v15-schizo` and a `schizo` object.

   Then replace the STATUS line with `STATUS: DONE`, append a summary (done / invalid / blocked) at the bottom, and run the gate command above. It must exit 0; if it does not, fix what its message names before yielding.

Rules:
- **Commits:** commit after each item with explicit paths (`git add <paths>`, never `-A`), message `loop(<ID>): <what>`. Never stage `LOOP_BOT_HEALTH.manager.md`, `LOOP_SCHIZOID.manager.md` or `LOOP_SCHIZO_LIVE.manager.md`. If `git status` shows modified files you did not touch, leave them unstaged.
- **Live path:** by the operator decisions above, sections C and D may change `crates/bb-trader` (prompt, cycle message, exit book, a GCP2 reader) and the Pi env. Hard safety must stay exactly as it is:
  - arming flags, the stop file, the run lock, permissions;
  - `order/test`, exchange filters, sizing reserve, fill reconciliation;
  - the failed-route halt, operator scope, Spot MARKET only, ≤ 3 legs;
  - `execute_route` as the only order path;
  - audit rows before alerts.

  No margin, futures, transfer or withdrawal calls.
- **GCP2 reads:** GET/POST of public data only. Never post, register a device, or log in.
- No destructive commands on live data without a backup.

## A. Decisions and GCP2 data

- [x] A1 The operator decisions above are not in the binding rules yet, and `IMPLEMENTATION_PLAN.md` ends at `## 48.`. The 2026-10-14 no-stop-loss rule (`CODEX.md:100-112`) stays as it is: "no panic sell". Make the following edits; touch nothing in hard safety. Acceptance: both files contain the decisions with their quotes; `git diff --check` exits 0. - **`CODEX.md`:** add a "Phase 49 schizo trader (operator decisions 2026-10-20)" rule that quotes the decisions and states: - the LLM trader becomes schizo by prompt and per-cycle state; its behaviour is not replaced by rules; - GCP2 public data is read-only information that drives the mood ring and YOLO state; - it goes live whatever the backtest result, on the full wallet, with no size cap; - hard safety is unchanged. - **`IMPLEMENTATION_PLAN.md`:** add `## 49. Schizo Live Trader (operator request 2026-10-20)` with one box per item of this ledger. — DONE: `CODEX.md:165-193` gets a new "Phase 49 schizo trader (operator decisions 2026-10-20)" rule, placed before the hard-safety rule. It quotes the operator ("it is not schizo enough", "ai needs to be schizoified…", "on top of those it looks at gcp2 data…", "Backtest, then trade live as schizo" with Mood ring and YOLO button, "nothing hardcoded … gcp bound in mood switches", "mood is inverted not yolo", "no panic sell", "go live no tiny shit…"). It states: the LLM trader stays and is made schizo by prompt and state, with `execute_route` still the only order path; GCP2 is read-only public data, never written or logged into; mood switches are GCP2-bound with no timer, probability or seed, zones come from `getcolorzones` and dice from GCP2; the mood is inverted while YOLO is not, and a failed read means no switch and no YOLO; there is no panic sell and the 2026-10-14 no-stop-loss rule stays; go-live is not gated, on the full wallet; hard safety is unchanged. No other CODEX line was touched; the no-stop-loss rule at `CODEX.md:100-112` is unchanged. `IMPLEMENTATION_PLAN.md:1262-1313` adds `## 49. Schizo Live Trader (operator request 2026-10-20)` with why, scope, non-goals and 12 boxes tagged A1–D2 (`sed … | grep -c '^- \[[ x]\]'` = 12, equal to the 12 ledger items); the A1 box is ticked. `git diff --check` exit 0.
- [x] A2 There is no GCP2 history in the repo; the download is a CSRF form that returns zip files (Background). Add `crates/bb-trader/src/gcp2.rs` with a history fetcher, plus the command `bb gcp2-history --year 2026 --month 8 --month 9 --month 10 --cache-dir data/gcp2-cache`. Wire it in `cli.rs` like `bot-spot-exit-replay`. Never touch the trader cycle. If the zip crate is not in the workspace, add `zip` as a workspace dependency; do not shell out to `unzip`. - **Monthly data:** the fetcher GETs the download page, keeps its cookie and `_token`, POSTs `group_2=Global Network`, unzips the response and writes `data/gcp2-cache/GCP2_Network_Coherence_Global_Network_<YYYY>_<MM>.csv`. - **Colour zones:** it also saves GCP2's own colour zones (`getcolorzones`, through the token from `api_token.js`) as `data/gcp2-cache/colorzones.json` with the fetch time. No zone bound may be written in code. Acceptance: a wiremock test covering page → token → POST → zip → CSV and the zones file; a real run writes the three CSVs (Aug 2,678,401, Sep 2,592,001 and Oct ≥ 1,641,601 lines) plus `colorzones.json`. — DONE: new `crates/bb-trader/src/gcp2.rs` with `Gcp2Client`, `download_history`, `Gcp2HistoryArgs`, `run_history_cli`, `ColorZones` and `parse_color_zones`/`parse_bearer`/`csrf_token`/`month_file_name`. Flow: GET `/data-results/data-download` keeps the `Set-Cookie` session and the form `_token`; POST `/data-results/network-coherence-download` with a hand-built `multipart/form-data` body (`_token`, `group_2`, `year_2`, `month_2`) and the cookie header, so no extra reqwest features are needed; `zip::ZipArchive` unzips the CSV, the file name must equal the expected `GCP2_Network_Coherence_<group>_<YYYY>_<MM>.csv`, and it is written atomically (tmp then rename); the bearer comes from `/js/data/api_token.js`; `getcolorzones` is saved to `data/gcp2-cache/colorzones.json` with `fetched_at`, zones sorted, numbers accepted as string or number, no zone bound in code. `cli.rs` adds `Command::Gcp2History`, wired like exit-replay (`GCP2 history download failed: …`, exit 1); `lib.rs` registers `gcp2` in the docs list. Dependencies: workspace `zip = { version = "9", default-features = false, features = ["deflate"] }` (resolved 9.0.5), and `bb-trader` now also uses workspace `reqwest`. Tests (3, `gcp2::tests`): parsers (token, bearer, zones sorted, empty list rejected); `downloads_a_month_and_the_zones_into_the_cache` (wiremock page → cookie+token → POST matched on cookie, token, group and `month_2=9` → zipped CSV byte-equal on disk → bearer → zones JSON with fetch time; month 13 rejected); `a_mismatched_or_non_zip_download_fails_without_writing` (HTML instead of zip → "not a zip", cache dir left empty). Real run: `cargo run --release -p bb-cli --bin bb -- gcp2-history --year 2026 --month 8 --month 9 --month 10 --cache-dir data/gcp2-cache` exit 0, writing Aug 2,678,401, Sep 2,592,001 and Oct 1,641,601 lines (equal to the setup measurements) plus `colorzones.json` (fetched 2026-10-20T08:23:14Z: Normal 0–140, Elevated 140–166, High 166–219, Very High 219–279, Extreme 279–10000). `data/` is gitignored, so the cache stays local. `--help` lists `--year --month --group --cache-dir --base-url`. Suites: the first workspace run failed on "No space left on device" (disk 100%: `target/debug` had grown to 31 GB of stale artifacts). `cargo clean --profile dev -p <17 workspace crates>` removed 27.1 GiB of rebuildable artifacts (disk now 92%); then `cargo test --workspace` exit 0 (1050 passed, 0 failed), clippy `--workspace --all-targets -D warnings` 0, `cargo test -p bb-cli` 0, `scripts/smoke.sh` 0, `scripts/pool_replay.sh` 0, `git diff --check` 0.
- [ ] A3 The GCP2 signal is only partly matched. The live minute value is the 3,600 s sum of per-second `network_coherence` ending at the minute: 409/972 overlap minutes match exactly and 902/972 share the zone (Background). Do A2 first.
  - **Implement in `gcp2.rs`:** a `Gcp2Series` loaded from the cache CSVs; `hour_sum_at(t)`, the sum of the 3,600 seconds ending at or before t; and `zone(value, zones)`, using the colour zones read from GCP2 (`getcolorzones`, stored by A2 as `data/gcp2-cache/colorzones.json` with its fetch time; live from C1), never from constants in code. On 2026-10-20 they read Normal < 140 ≤ Elevated < 166 ≤ High < 219 ≤ Very High < 279 ≤ Extreme.
  - **GCP2 dice:** `gcp2_dice(t) -> u64`, a hash (SplitMix64 finalizer) of that minute's GCP2 value written to 4 decimals. That is the same text live reads from `netvar_aggregate` and the backtest computes as `hour_sum_at(t)`, so there is no seed constant. Backtest and live give the same roll whenever the two values agree, which was exact for 409/972 minutes on 2026-10-19; record the rate again in the DONE note.
  - **Investigate the 563 non-exact minutes:** missing seconds, gcp2.net's exclusion dates (`/data-results/display-exclusion-dates`), late data revisions, or a changed device set. Fix the formula if a cheap cause explains them; otherwise document the gap.
  - **Missing seconds:** a missing second counts as absent, and an hour with less than 90% coverage reads as unavailable.

  Acceptance: unit tests on a fixture for the sum, gaps and zone bounds; a recomputation on a fresh `getNetVarAggregate24H` against the October CSV (refetch it if the overlap moved) with the exact-match and same-zone counts recorded in the DONE note.

## B. Backtest the schizo persona

- [ ] B1 `Snapshot` has no GCP2 view (`crates/bb-trader/src/persona_backtest/engine.rs`, `Snapshot::at`). Do A3 first.
  - Add `gcp2: Option<Gcp2View { hour_sum, zone }>` at the newest minute ending at or before T, read from the cache and never from data after T.
  - Load it through a new `--gcp2-cache-dir` flag on `bb bot-spot-persona-backtest` (default `data/gcp2-cache`), with `None` when the flag points at a missing dir. Existing personas ignore it, so their Phase 48 numbers must stay byte-identical.

  Acceptance: a look-ahead test (truncating GCP2 after T gives the same view up to T); a rerun of the Phase 48 command with `--persona v13` reproduces the `v13` CSV row of `reports/schizoid_backtest_2026-10-18.csv` exactly.
- [ ] B2 There is no `schizo` persona. Add `Schizo` in `persona_backtest/personas.rs`, with its rules in its doc comment. Do B1 first. These are the setup agent's defaults; the operator can veto them.
  - **Dice:** GCP2 dice (A3): the roll for step T is `gcp2_dice` of the minute ending at T. No seed constant. Without a GCP2 reading there is no roll, and nothing random happens that step.
  - **Mood swings (GCP-bound, inverted):** the active mood is one of `v13`, `contrarian`, `detached`, `high_variance`. It runs opposite to GCP2:
    - when the GCP2 zone is the same as at the previous step (GCP2 stable), the mood switches to the other mood picked by the GCP2 dice (roll mod 3 over the other three, in a fixed name order);
    - when the zone changed since the previous step, up or down, the mood is kept.

    There is no timer and no switch probability. Without a GCP2 reading at this step or the previous one, the mood is kept. On 2026-10-19's 72 twenty-minute steps this gives 56 switches and 15 holds.
  - **Mood ring:** the inverted rule above is the mood ring. The audit and report record every step's from and to zones and the result: switched or held.
  - **YOLO button:** while the zone is Very High or Extreme, sell any other holding and put all cash into the liquid-top coin with the highest 1h return. That position is held until the zone drops below High. Then normal mood rules and the mood's own exit plan take over again.
  - **Argues with itself:** outside YOLO, all four personas decide on the schizo book. The active mood's vote counts 2, the others 1. When flat, buy the coin with the most votes if it reaches 2 (ties: higher 24h volume). When holding, sell on 2 or more sell votes.
  - **Extreme bets:** one position, all cash.
  - **Exits:** every position runs the active mood's own exit plan unchanged (`v13` and the YOLO hold: `atr=3.5:1h,deadline=360`; `contrarian`: `stretch=2:1,deadline=360`; `high_variance`: `atr=6:1h,deadline=1440`; `detached`: none). No added stop-loss or panic sell.

  Acceptance: fixture tests for each trait:
  - the same GCP2 minute gives the same dice, and a different minute gives a different roll on the fixture;
  - the zone staying the same switches the mood to the dice-picked one, every step;
  - a zone change, up or down, keeps the mood;
  - a missing GCP2 reading keeps the mood;
  - zones are read from the stored `colorzones.json`, and a test with shifted zones changes which steps switch;
  - a Very High zone buys the hottest coin all-in and sells the others;
  - the mood's vote counts double;

  The A4-style look-ahead test also passes for `schizo`.
- [ ] B3 No schizo result exists. Do B2 first.
  - **Run:** `unshare -rn cargo run --release -p bb-cli --bin bb -- bot-spot-persona-backtest --cache-dir data/entry-study-cache --gcp2-cache-dir data/gcp2-cache --end 2026-10-18T10:43:00Z --days 60 --persona all --persona schizo`, after making sure `all` does not already include `schizo`.
  - **Output:** `reports/schizoid_backtest_2026-10-18.*`, with a seventh row. Add a GCP2 section to the `.md`: zone minutes over the window, mood switches by zone change (from → to), YOLO entries and their net. Add the caveat that GCP2 has no shown link to prices.
  - **Checks:** recompute the `schizo` row from `_trades.csv` as in Phase 48 C3; a second run gives the same `sha256sum` for all three files; commit them.
  - **Go-live:** by operator decision the result does not gate it; report it either way.

  Acceptance: exit 0, 0 `ERROR`, 7 CSV rows, hashes equal, cross-check differences ≤ 1e-6 USDT.

## C. Live schizo trader

- [ ] C1 The trader cannot read GCP2 live. In `gcp2.rs`, add an async live reader and call it once per cycle from `Runtime::build_snapshot` (`cycle.rs:199-262`). Do A3 first. On any failure, the cycle runs with "GCP2 unavailable" and no mood-ring or YOLO effect. Acceptance: wiremock tests (normal, 401, then a token refresh, timeout), plus a laptop run that prints a live reading.
  - **Requests:** GET the token from `/js/data/api_token.js` (cache it; re-fetch on 401/403), then `/api/getNetVarAggregate24H` and `/api/getcolorzones`, each with a 10 s timeout.
  - **Returns:** the latest minute value, its zone (from the live colour zones), the zone 20 min earlier, and the last 24 h of minute values.
- [ ] C2 The trader has no schizo state. Add a per-cycle schizo state from the same rules as B2, with live inputs. Do B2 and C1 first. Acceptance: tests for state persistence across a restart, the same GCP2 minute giving the same dice, a switch while the zone holds, a kept mood on a zone change (up or down) or a missing reading, the YOLO flag at Very High, and the audit field.
  - **Mood:** persisted next to `bot-spot-agent-state.json` and surviving restarts.
  - **Dice:** GCP2 dice of the latest minute (A3); no seed. The mood is inverted to GCP2: it switches when the zone is unchanged since the previous cycle and holds when the zone changed, never on a timer. The previous cycle's zone is persisted with the mood.
  - **Votes:** the four base personas' votes computed from a live `Snapshot`, built from the breadth klines `market_mood::fetch` already reads and the 24h tickers. Add a builder from those; do not fetch twice.
  - **Audit:** record the state in each cycle audit row as `schizo: {mood, mood_reason, gcp2: {value, zone}, dice, votes, yolo}`.
- [ ] C3 The prompt has no schizo voice. Set the prompt tag to `bot-spot-v15-schizo` (`config.rs:25`) and update `system_prompt()` and `cycle_prompt()` (`agent.rs`). Keep the hard-safety text and the tool contract unchanged. Do C2 first. Acceptance: prompt tests showing the section with a mood, a YOLO directive at Very High, and "GCP2 unavailable" on failure; an isolated `--plan-only` run on the Pi (Protocol 4c) completes a turn, and the bot's plan names its mood.
  - **System prompt:** the bot is a schizo trader with four personalities. Each cycle it acts out the mood the system hands it, weighs the personas' votes as the voices arguing in its head, and bets big: one coin, all cash. It obeys the YOLO directive.
  - **Cycle message:** a `## Your schizo state` section with the mood and why it changed, the GCP2 value and zone with its last-24h range, the dice roll, the four votes, and, in YOLO, the order "go all-in on <coin> now".
- [ ] C4 Nothing is deployed. Deploy C1–C3 to the Pi per Protocol 5 at a safe moment. Do B3 and C3 first. The default exit plan stays `atr=3.5:1h,deadline=360`; there is no panic sell.
  - **Env** (back up first): `BOT_SPOT_PROMPT_VERSION=bot-spot-v15-schizo` and any new GCP2 variables (e.g. `BOT_SPOT_GCP2_ENABLED=true`).
  - **Verify:** the startup log; the next two cycle rows carry `bot-spot-v15-schizo` and a `schizo` object with a GCP2 value.

  Acceptance: the verification above plus the rollback tag name recorded in the DONE note.

## D. Docs and handoff

- [ ] D1 Nothing documents GCP2 or the schizo trader. Do C4 first. Acceptance: all files name `bb gcp2-history` and `bot-spot-v15-schizo` consistently; `git diff --check` exits 0.
  - **`README.md`** (Bot Spot section): the schizo mood, GCP2 and YOLO.
  - **`OPERATIONS.md`:** `bb gcp2-history`, the live GCP2 reader, the `schizo` audit field, and how to roll back to v14.
  - **`.env.example`:** the new variables.
- [ ] D2 The handoff files do not mention Phase 49. Do D1 first. Acceptance: the `PROGRESS.md` table matches the report CSV; `git diff --check` exits 0.
  - **`IMPLEMENTATION_PLAN.md`:** tick the Phase 49 boxes.
  - **`PROGRESS.md`:** add a dated top entry with the B3 results table (7 rows), the deploy time and rollback tag, and the first live schizo cycles (mood, GCP2 zone, action).
  - **`AGENTS.md`:** update the active-work line.
