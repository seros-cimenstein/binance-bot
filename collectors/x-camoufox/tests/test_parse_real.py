"""Parser regression tests on trimmed real X responses (captured 2026-10-13, author
identity redacted). They pin the current web-app shapes: user counters in
`relationship_counts`, and the profile timeline query `UserOriginalsTimeline`."""

import json
from pathlib import Path

from x_collector import parse

FIXTURES = Path(__file__).parent / "fixtures"


def load(name: str) -> dict:
    return json.loads((FIXTURES / name).read_text(encoding="utf-8"))


def raw_followers(payload: dict) -> dict[str, int]:
    """user rest_id -> relationship_counts.followers, read straight from the payload."""
    out: dict[str, int] = {}

    def walk(node):
        if isinstance(node, dict):
            if node.get("__typename") == "User" and "relationship_counts" in node:
                out[node["rest_id"]] = node["relationship_counts"]["followers"]
            for value in node.values():
                walk(value)
        elif isinstance(node, list):
            for value in node:
                walk(value)

    walk(payload)
    return out


def test_search_followers_come_from_relationship_counts():
    payload = load("real_search_timeline.json")
    followers = raw_followers(payload)
    tweets = parse.parse_tweets("SearchTimeline", payload)
    assert len(tweets) == 8
    for tweet in tweets:
        assert tweet.author_followers == followers[tweet.author_id] > 0
        assert tweet.author_created_at_ms is not None


def test_search_post_row_with_cashtag_and_pump_mint():
    tweets = {t.post_id: t for t in parse.parse_tweets("SearchTimeline", load("real_search_timeline.json"))}
    row = parse.post_row(
        tweets["2110063880626118760"],
        curated_handles=frozenset(),
        source_kind="search",
        query="pump.fun",
        first_seen_ms=1791913500000,
    )
    assert row["cashtags"] == ["AUTOAPE"]
    assert row["solana_addresses"] == ["7SzbS8BgvRzqkhiJVZxFtaCwN7ULWnbA1rGNGPqdpump"]
    assert row["author_followers"] == 364
    assert row["author_verified"] is True
    assert row["author_handle"] is None
    assert row["metrics"]["views"] == 6


def test_profile_timeline_parses_under_both_query_names():
    payload = load("real_user_originals_timeline.json")
    current = parse.parse_tweets("UserOriginalsTimeline", payload)
    assert len(current) == 10
    assert [t.post_id for t in parse.parse_tweets("UserTweets", payload)] == [t.post_id for t in current]
    first = current[0]
    assert first.post_id == "2109998220936204691"
    assert first.author_followers == 4220062
    assert (first.metrics.replies, first.metrics.likes, first.metrics.views) == (144, 447, 54517)


def test_tweet_detail_samples_only_direct_replies_newest_first():
    payload = load("real_tweet_detail.json")
    root = parse.parse_tweets("TweetDetail", payload)[0]
    assert root.post_id == "2109998220936204691"
    picked = parse.newest_replies_and_quotes(root.post_id, [("TweetDetail", payload)], [], 20)
    ids = [tweet.post_id for _, tweet in picked]
    # 2110008308816105515 answers a reply, not the root, so it is not sampled.
    assert "2110008308816105515" not in ids
    assert ids == [
        "2110005445376999893",
        "2110004542813184161",
        "2110004253263831078",
        "2110002483762487305",
        "2110000259640213914",
        "2109998717071728746",
    ]
    assert all(kind == "reply" for kind, _ in picked)
