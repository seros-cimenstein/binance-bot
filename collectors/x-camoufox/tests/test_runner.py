import asyncio
import json
from pathlib import Path

from x_collector.config import Config
from x_collector.pacer import BlockKind
from x_collector.runner import Collector, Job, PageLoad, search_url, status_url

FIXTURES = Path(__file__).parent / "fixtures"
T0 = 1_791_900_000_000
PARENT = "1845000000000000001"


def fixture(name):
    return json.loads((FIXTURES / name).read_text(encoding="utf-8"))


class FakeClock:
    def __init__(self):
        self.now = T0

    def __call__(self):
        return self.now


def make_collector(tmp_path, pages, **env):
    config = Config.from_env({"X_COLLECTOR_DATA_DIR": str(tmp_path), **env})
    collector = Collector(config, clock=FakeClock())
    loads = []

    async def fake_load(url, expect):
        loads.append(url)
        collector.pacer.record_page(collector.clock())
        return pages[url]

    async def no_pause(seconds=None):
        return None

    collector.load = fake_load
    collector.pause = no_pause
    return collector, loads


def rows(path):
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


def test_status_job_writes_engagement_and_reply_sample(tmp_path):
    pages = {
        status_url(PARENT): PageLoad(status_url(PARENT), [("TweetDetail", fixture("tweet_detail.json"))]),
        search_url(f"quoted_tweet_id:{PARENT}"): PageLoad(
            search_url(f"quoted_tweet_id:{PARENT}"), [("SearchTimeline", fixture("search_timeline.json"))]
        ),
    }
    collector, loads = make_collector(tmp_path, pages)
    assert asyncio.run(collector.run_job(Job("status", PARENT, resample=True, reply_sample=True)))
    assert loads == [status_url(PARENT), search_url(f"quoted_tweet_id:{PARENT}")]
    [engagement] = rows(collector.config.engagement_path)
    assert engagement["status"] == "ok" and engagement["metrics"]["likes"] == 95
    replies = rows(collector.config.replies_path)
    assert [(r["kind"], r["post_id"]) for r in replies] == [
        ("reply", "1845000000000000011"),
        ("quote", "1845000000000000003"),
        ("reply", "1845000000000000010"),
    ]
    assert collector.pacer.replies.total(T0) == 3
    # A second sample does not duplicate replies already written for this parent.
    assert asyncio.run(collector.run_job(Job("status", PARENT, reply_sample=True)))
    assert len(rows(collector.config.replies_path)) == 3


def test_reply_sample_respects_hourly_reply_budget(tmp_path):
    pages = {
        status_url(PARENT): PageLoad(status_url(PARENT), [("TweetDetail", fixture("tweet_detail.json"))]),
        search_url(f"quoted_tweet_id:{PARENT}"): PageLoad(search_url(f"quoted_tweet_id:{PARENT}"), []),
    }
    collector, _ = make_collector(tmp_path, pages, X_COLLECTOR_MAX_REPLIES_PER_HOUR="1")
    asyncio.run(collector.run_job(Job("status", PARENT, reply_sample=True)))
    assert [r["post_id"] for r in rows(collector.config.replies_path)] == ["1845000000000000011"]


def test_gone_post_skips_quote_search(tmp_path):
    gone_id = "1845000000000000013"
    pages = {status_url(gone_id): PageLoad(status_url(gone_id), [("TweetDetail", fixture("tweet_detail.json"))])}
    collector, loads = make_collector(tmp_path, pages)
    asyncio.run(collector.run_job(Job("status", gone_id, resample=True, reply_sample=True)))
    assert loads == [status_url(gone_id)]
    [engagement] = rows(collector.config.engagement_path)
    assert engagement == {
        "metrics": None,
        "post_id": gone_id,
        "sampled_at_ms": T0,
        "schema": "x-engagement-v1",
        "status": "gone",
    }


def test_blocked_load_backs_off_without_rows_and_keeps_job_due(tmp_path):
    url = search_url("pump.fun")
    pages = {url: PageLoad(url, [], block=BlockKind.RATE_LIMIT)}
    collector, _ = make_collector(tmp_path, pages)
    assert not asyncio.run(collector.run_job(Job("search", "pump.fun")))
    assert rows(collector.config.posts_path) == []
    assert collector.pacer.backoff_until_ms == T0 + 5 * 60_000
    assert ("search", "pump.fun") not in collector.next_source_ms
    heartbeat = json.loads(collector.config.heartbeat_path.read_text())
    assert heartbeat["schema"] == "x-heartbeat-v1"
    assert heartbeat["status"] == "backoff" and heartbeat["backoff_until_ms"] == T0 + 5 * 60_000
    assert heartbeat["pages_last_hour"] == 1


def test_search_job_writes_new_posts_once_and_reschedules_with_jitter(tmp_path):
    url = search_url("pump.fun")
    pages = {url: PageLoad(url, [("SearchTimeline", fixture("search_timeline.json"))])}
    collector, _ = make_collector(tmp_path, pages)
    (tmp_path / "x-accounts.txt").write_text("LaunchWatcher\n")
    assert asyncio.run(collector.run_job(Job("search", "pump.fun")))
    posts = rows(collector.config.posts_path)
    assert [p["post_id"] for p in posts] == [
        "1845000000000000001",
        "1845000000000000002",
        "1845000000000000003",
        "1845000000000000005",
    ]
    # Curated author found through a search keeps its handle; others do not.
    assert posts[0]["curated"] is True and posts[0]["author_handle"] == "LaunchWatcher"
    assert posts[2]["author_handle"] is None
    assert all(p["source"] == {"kind": "search", "query": "pump.fun"} for p in posts)
    due = collector.next_source_ms[("search", "pump.fun")]
    assert T0 + 360_000 <= due <= T0 + 420_000
    asyncio.run(collector.run_job(Job("search", "pump.fun")))
    assert len(rows(collector.config.posts_path)) == 4


def test_focus_work_comes_before_sources_and_merges_status_jobs(tmp_path):
    collector, _ = make_collector(tmp_path, {})
    (tmp_path / "x-searches.txt").write_text("pump.fun\n")
    (tmp_path / "x-focus.json").write_text(
        json.dumps(
            {
                "schema": "x-focus-v1",
                "generated_at_ms": T0,
                "coins": [{"token_key": "binance:PEPE", "tier": "hot", "cashtag": "PEPE", "search_queries": ["$PEPE"]}],
                "resample_post_ids": ["11", "12"],
                "reply_sample_post_ids": ["11"],
            }
        )
    )
    jobs = collector.focus_jobs(T0)
    assert jobs == [
        Job("status", "11", resample=True, reply_sample=True),
        Job("status", "12", resample=True, reply_sample=False),
        Job("focus_search", "$PEPE"),
    ]
    job, _ = collector.next_job(T0)
    assert job == jobs[0]
