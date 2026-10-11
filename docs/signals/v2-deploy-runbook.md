# V2 Signal Deploy Runbook

V2 lands inert. Do not enable it until the Phase 2 code and this runbook have
been reviewed. Paths below assume the local checkout at
`/home/user/projects/binance-bot`; on the Pi the repo lives at
`/home/seref/binance-bot` — substitute accordingly.

```bash
cd ~/binance-bot   # repo checkout (on the Pi: ~/binance-bot)
cargo test -p bb-signals
git diff --check
docker compose build
```

Enable only the V2 detector service:

```bash
docker compose --profile v2 up -d signals-dexscreener-launches
docker compose ps signals-dexscreener-launches
docker compose logs -f signals-dexscreener-launches
```

Enable snapshots and reports. The crontab is **baked into the image**
(`COPY docker ./docker` in the Dockerfile), so editing `docker/crontab` requires a rebuild
and force-recreate of the scheduler — restarting alone keeps the old crontab.

```bash
$EDITOR docker/crontab          # uncomment the two TODO Phase 2 lines only
docker compose build scheduler
docker compose --profile all up -d --force-recreate scheduler
docker compose logs -f scheduler
```

Uncomment only the two `TODO Phase 2 enable on Pi` lines for
`bb signals-snapshotter` and `bb signals-v2-report`.

Verify V2 state is growing without touching V1 ledgers:

```bash
docker compose exec scheduler sh -lc 'wc -l /app/data/signals-v2-ledger.jsonl /app/data/signals-v2-snapshots.jsonl || true'
docker compose exec scheduler sh -lc 'tail -n 3 /app/data/signals-v2-ledger.jsonl || true'
docker compose exec scheduler bb signals-v2-report
ls -lh docker-data/signals-v2-report-*.md
```

Roll back V2 only:

```bash
docker compose stop signals-dexscreener-launches
docker compose rm -f signals-dexscreener-launches
$EDITOR docker/crontab          # re-comment the two TODO Phase 2 lines
docker compose build scheduler
docker compose --profile all up -d --force-recreate scheduler
```

Leave the V2 JSONL files in place for audit unless there is a separate
operator decision to archive them.
