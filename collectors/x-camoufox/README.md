# x-camoufox: Phase 43 read-only X collector

Python sidecar for the Phase 43 X pre-pump signal insighter
(`docs/signals/x-insighter-design.md`). It reads public X posts through a
Camoufox (Firefox) browser logged in to a burner account and writes JSONL for
`bb x-insighter`. Paper only: it holds no exchange credentials and has no order
path.

## What it writes (all in `X_COLLECTOR_DATA_DIR`, `/app/data` in compose)

| File | Schema | Content |
|---|---|---|
| `x-posts.jsonl` | `x-post-v1` | First sighting of each post (dedupe by `post_id`, restored from the file tail on restart) |
| `x-engagement.jsonl` | `x-engagement-v1` | Counter re-reads of posts listed in `x-focus.json` |
| `x-replies.jsonl` | `x-reply-v1` | Newest replies (status page) and quotes (`quoted_tweet_id:` search), max 20 per parent per sample |
| `x-collector-heartbeat.json` | `x-heartbeat-v1` | Status, hourly counters, last error, backoff (atomic rewrite every loop) |

It reads only `x-focus.json` (written by the engine) and the two list files.
Authors are reduced to `author_hash` (first 16 hex of SHA-256 of the numeric
user id), follower count, account age and verified flag; handles are stored
only for curated accounts; excerpts are whitespace-collapsed and cut at 280
characters.

## Read-only guarantees (enforced in code, `src/x_collector/guard.py`)

- Every browser request goes through a route filter. Every non-GET X GraphQL
  call is aborted (X sends all mutations as POST), as is any GraphQL operation
  named like a write or a private read (DMs, bookmarks, notifications), every
  non-GET request to X's REST API (`/i/api/1.1`, `/i/api/2`, other `/i/api/`
  paths, the api and upload hosts) and any REST read of DMs or notifications.
  Other GET reads run, because the web app needs them to render a page. Only
  `SearchTimeline`, `UserOriginalsTimeline` / `UserTweets`, `TweetDetail` and
  `TweetResultByRestId` responses are parsed into rows. Service workers are
  blocked so nothing bypasses the filter. Blocked requests are logged as
  `guard blocked ...`; the web app's own telemetry POSTs show up there.
- The collector only navigates to profile, search
  (`https://x.com/search?q=...&f=live`) and status URLs. It never clicks,
  never types, never likes, follows, reposts, replies or opens DMs.
- Data comes from intercepted GraphQL JSON, not from the DOM.

## Pacing

- Curated accounts and discovery searches: every `X_COLLECTOR_POLL_SECONDS`
  (300) plus 20-40% jitter. Focus searches: every
  `X_COLLECTOR_FOCUS_POLL_SECONDS` (180) plus jitter. 4-11 s between page loads.
- Hard cap `X_COLLECTOR_MAX_PAGES_PER_HOUR` (120) page loads in any rolling
  hour across everything; focus work (re-reads, reply samples, focus searches)
  goes first, then the oldest-due account/search. Keep accounts + searches to
  about 10 lines or discovery falls behind while coins are in focus.
- `X_COLLECTOR_MAX_REPLIES_PER_HOUR` (400) caps written reply/quote rows.
- Login wall, challenge or HTTP 429 (or X error 88): exponential backoff
  5 min, 10, 20, 40, 80, then 2 h. Three consecutive challenges set status
  `challenge` and the collector stops loading pages until it is restarted
  (the container stays up and keeps the heartbeat fresh).
- A focus file older than 15 min is ignored (engine not running).

## Environment

| Variable | Default | Meaning |
|---|---|---|
| `X_COLLECTOR_DATA_DIR` | `/app/data` | Shared bot data dir |
| `X_COLLECTOR_ACCOUNTS_FILE` | `$DATA_DIR/x-accounts.txt` | Curated handles, one per line (`accounts.example.txt`) |
| `X_COLLECTOR_SEARCHES_FILE` | `$DATA_DIR/x-searches.txt` | Discovery queries, one per line (`searches.example.txt`) |
| `X_COLLECTOR_PROFILE_DIR` | `$DATA_DIR/x-camoufox-profile` | Persistent browser profile (the burner session); a stable fingerprint is kept next to it as `<dir>.fingerprint.json` |
| `X_COLLECTOR_MAX_PAGES_PER_HOUR` | `120` | Page-load cap |
| `X_COLLECTOR_MAX_REPLIES_PER_HOUR` | `400` | Reply/quote row cap |
| `X_COLLECTOR_POLL_SECONDS` | `300` | Account/search interval |
| `X_COLLECTOR_FOCUS_POLL_SECONDS` | `180` | Focus search interval |
| `X_COLLECTOR_JITTER_MIN` / `_MAX` | `0.20` / `0.40` | Interval jitter |
| `X_COLLECTOR_HEADLESS` | `true` | `true`, `false` or `virtual` (Xvfb) |
| `X_COLLECTOR_BLOCK_IMAGES` | `true` | Do not load images |
| `X_COLLECTOR_PAGE_TIMEOUT_SECONDS` | `45` | Navigation/response wait |

The account and search files are re-read every loop, so edits apply without a
restart.

## Login (once per profile)

The burner session lives in `X_COLLECTOR_PROFILE_DIR` on the data volume and
survives restarts. Log in once in a headful browser:

- On a desktop with a display (simplest):

  ```sh
  cd collectors/x-camoufox
  uv sync && uv run python -m camoufox fetch
  X_COLLECTOR_DATA_DIR=./login-data uv run python -m x_collector --login
  ```

  Log in to the burner account in the window; it closes by itself once the
  session cookie exists. Then copy `login-data/x-camoufox-profile/` and
  `login-data/x-camoufox-profile.fingerprint.json` into the Pi's
  `${BOT_DATA_DIR}` (owned by uid 1000).
- On the Pi over SSH X forwarding (`ssh -X`), with the container's display
  pointed at the forwarded one:

  ```sh
  docker compose --profile x-collector run --rm \
    -e DISPLAY -v /tmp/.X11-unix:/tmp/.X11-unix x-camoufox python -m x_collector --login
  ```

During `--login` the filter allows the login flow (`onboarding/`,
`guest/activate.json`) and still aborts write operations. Do nothing in that
window except log in. If the heartbeat later shows `login_required`, repeat.

## Run

```sh
cp accounts.example.txt "$BOT_DATA_DIR/x-accounts.txt"   # then edit
cp searches.example.txt "$BOT_DATA_DIR/x-searches.txt"   # then edit
docker compose --profile x-collector up -d --build x-camoufox
docker compose --profile x-collector run --rm x-camoufox python -m x_collector --once
```

Local development: `uv sync`, `uv run pytest`, `uv run python -m x_collector --help`.

## Raspberry Pi 5 (arm64)

The image builds on arm64 (`python:3.12-slim-bookworm`, Playwright's Firefox
dependencies, and the `lin.arm64` Camoufox build fetched at build time; the
browser download is about 1.3 GB). Raspberry Pi OS on the Pi 5 boots a kernel
with 16K memory pages by default, which Firefox builds can crash under; if the
browser fails to start, add `kernel=kernel8.img` to `/boot/firmware/config.txt`
(4K pages) and reboot. Checked 2026-10-19 on the Pi 5 (kernel
`6.12.75+rpt-rpi-2712`, 16K pages): browser 156.0.1-beta.36 launched headless
and opened a page without that change.

`camoufox fetch` asks GitHub's API for the release; unauthenticated, that is
60 requests per hour per public IP. When the limit is used up the fetch exits 0
with nothing installed, so the Dockerfile checks the browser after it and the
build fails instead: wait for the hourly reset (`curl -s
https://api.github.com/rate_limit`) and build again.

## Operator responsibility

Automated reading of X with a logged-in account is against X's terms of
service and can get the burner account locked or banned. That risk, the choice
of account, curated handles and queries are the operator's (operator decision
2026-10-13, `CODEX.md`). Never use a personal or valuable account.
