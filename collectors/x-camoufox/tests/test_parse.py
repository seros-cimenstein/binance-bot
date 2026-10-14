import hashlib
import json
from datetime import UTC, datetime
from pathlib import Path

import pytest

from x_collector import parse

FIXTURES = Path(__file__).parent / "fixtures"
MINT_A = "6MQ9dDq6siEgRShJa2xbkz6QoECHiqv6MP18FA6hov3Z"
MINT_B = "F6ANxSg3z9P7tjV7u9MvsRuBZsXaKVosMMw4EgW9DDmv"


def load(name: str) -> dict:
    return json.loads((FIXTURES / name).read_text(encoding="utf-8"))


def ms(text: str) -> int:
    return int(datetime.fromisoformat(text).replace(tzinfo=UTC).timestamp() * 1000)


def by_id(tweets):
    return {tweet.post_id: tweet for tweet in tweets}


def test_search_timeline_rows_match_x_post_v1():
    tweets = by_id(parse.parse_tweets("SearchTimeline", load("search_timeline.json")))
    # Promoted entry and the who-to-follow module are skipped; the repost is unwrapped.
    assert list(tweets) == [
        "1845000000000000001",
        "1845000000000000002",
        "1845000000000000003",
        "1845000000000000005",
    ]
    row = parse.post_row(
        tweets["1845000000000000001"],
        curated_handles=frozenset(),
        source_kind="search",
        query="pump.fun",
        first_seen_ms=1_791_900_042_000,
    )
    assert row == {
        "schema": "x-post-v1",
        "post_id": "1845000000000000001",
        "author_hash": hashlib.sha256(b"1500000001").hexdigest()[:16],
        "author_handle": None,
        "curated": False,
        "author_followers": 1520,
        "author_created_at_ms": ms("2022-03-06T12:00:00"),
        "author_verified": True,
        "created_at_ms": ms("2024-10-11T09:20:00"),
        "first_seen_ms": 1_791_900_042_000,
        "excerpt": "Ape in $PEPE and $wif2 now! US$100 is nothing, $1000 neither, $TOOLONGTICKER no & more "
        f"https://pump.fun/coin/{MINT_A}",
        "lang": "en",
        "cashtags": ["PEPE", "WIF2"],
        "solana_addresses": [MINT_A],
        "evm_addresses": [],
        "urls": [f"https://pump.fun/coin/{MINT_A}"],
        "is_reply": False,
        "is_quote": False,
        "in_reply_to_post_id": None,
        "quoted_post_id": None,
        "metrics": {"replies": 3, "reposts": 5, "quotes": 1, "likes": 40, "bookmarks": 2, "views": 3100},
        "source": {"kind": "search", "query": "pump.fun"},
    }


def test_visibility_wrapper_note_tweet_reply():
    tweet = by_id(parse.parse_tweets("SearchTimeline", load("search_timeline.json")))["1845000000000000002"]
    assert tweet.in_reply_to_post_id == "1845000000000000001"
    # Long-form note text wins over the truncated legacy text; leading reply mentions are dropped.
    assert tweet.excerpt.startswith("this long note mentions $BONK")
    assert tweet.cashtags == ["BONK"]
    assert tweet.solana_addresses == [MINT_B]
    assert tweet.evm_addresses == ["0xabcdef0123456789abcdef0123456789abcdef01"]
    assert tweet.metrics.views is None  # views hidden (no count)
    assert tweet.author_followers == 12
    assert tweet.author_created_at_ms == ms("2024-10-01T08:00:00")
    assert tweet.author_verified is False


def test_quote_and_repost():
    tweets = by_id(parse.parse_tweets("SearchTimeline", load("search_timeline.json")))
    quote = tweets["1845000000000000003"]
    assert quote.is_quote and quote.quoted_post_id == "1845000000000000001"
    assert quote.cashtags == ["PEPE"]  # "$pepe" and the PEPE symbol entity collapse
    assert quote.metrics.views == 120000
    original = tweets["1845000000000000005"]
    assert original.cashtags == ["DOGE"]
    assert original.metrics.likes == 7


def test_curated_handle_kept_only_for_curated_accounts():
    tweets = parse.parse_tweets("UserTweets", load("user_tweets.json"))
    assert [tweet.post_id for tweet in tweets] == ["1700000000000000001", "1845000000000000020"]
    curated = parse.post_row(
        tweets[1],
        curated_handles=frozenset({"launchwatcher"}),
        source_kind="account",
        query="launchwatcher",
        first_seen_ms=1,
    )
    assert curated["curated"] is True and curated["author_handle"] == "LaunchWatcher"
    assert curated["cashtags"] == ["MOODENG"]
    other = parse.post_row(
        tweets[1], curated_handles=frozenset({"someoneelse"}), source_kind="account", query=None, first_seen_ms=1
    )
    assert other["curated"] is False and other["author_handle"] is None


def test_tweet_detail_engagement_and_newest_replies():
    detail = [("TweetDetail", load("tweet_detail.json"))]
    state, tweet = parse.post_state(detail, "1845000000000000001")
    assert state == "ok"
    row = parse.engagement_row("1845000000000000001", sampled_at_ms=5, status=state, tweet=tweet)
    assert row == {
        "schema": "x-engagement-v1",
        "post_id": "1845000000000000001",
        "sampled_at_ms": 5,
        "status": "ok",
        "metrics": {"replies": 18, "reposts": 12, "quotes": 4, "likes": 95, "bookmarks": 6, "views": 5200},
    }
    assert parse.post_state(detail, "1845000000000000013") == ("gone", None)
    assert parse.post_state(detail, "1845000000000000777") == ("error", None)

    quotes = [("SearchTimeline", load("search_timeline.json"))]
    picked = parse.newest_replies_and_quotes("1845000000000000001", detail, quotes, limit=20)
    # Direct replies and quotes only (the nested reply to 010 is excluded), newest first.
    assert [(kind, t.post_id) for kind, t in picked] == [
        ("reply", "1845000000000000011"),
        ("quote", "1845000000000000003"),
        ("reply", "1845000000000000010"),
    ]
    assert [t.post_id for _, t in parse.newest_replies_and_quotes("1845000000000000001", detail, quotes, 2)] == [
        "1845000000000000011",
        "1845000000000000003",
    ]
    reply = parse.reply_row(picked[0][1], parent_post_id="1845000000000000001", kind="reply", first_seen_ms=9)
    assert reply["schema"] == "x-reply-v1"
    assert reply["excerpt"] == "can't sell, honeypot!!"
    assert reply["author_hash"] == hashlib.sha256(b"1500000011").hexdigest()[:16]
    assert set(reply) == {
        "schema",
        "parent_post_id",
        "post_id",
        "kind",
        "author_hash",
        "author_followers",
        "author_created_at_ms",
        "created_at_ms",
        "first_seen_ms",
        "excerpt",
        "metrics",
    }


def test_tweet_result_by_rest_id_ok_and_gone():
    detail = load("tweet_detail.json")
    focal = detail["data"]["threaded_conversation_with_injections_v2"]["instructions"][0]["entries"][0]
    result = focal["content"]["itemContent"]["tweet_results"]["result"]
    ok = {"data": {"tweetResult": {"result": result}}}
    assert parse.post_state([("TweetResultByRestId", ok)], "1845000000000000001")[0] == "ok"
    unavailable = {"data": {"tweetResult": {"result": {"__typename": "TweetUnavailable", "reason": "Suspended"}}}}
    assert parse.post_state([("TweetResultByRestId", unavailable)], "1845000000000000001") == ("gone", None)
    assert parse.post_state([("TweetResultByRestId", {"data": {"tweetResult": {}}})], "1") == ("gone", None)
    missing = {"errors": [{"code": 144, "message": "_Missing: No status found with that ID."}], "data": {}}
    assert parse.post_state([("TweetDetail", missing)], "1") == ("gone", None)
    # A rate-limited response is not evidence the post is gone.
    limited = {"errors": [{"code": 88, "message": "Rate limit exceeded"}]}
    assert parse.post_state([("TweetResultByRestId", limited)], "1") == ("error", None)


@pytest.mark.parametrize(
    ("text", "expected"),
    [
        ("$PEPE and $pepe and $Wif", ["PEPE", "WIF"]),
        ("$1000 and $5 and $3.50", []),
        ("$5k pump", ["5K"]),  # pre-registered rule: 1-10 [A-Z0-9] with at least one letter
        ("$ABCDEFGHIJ ok, $ABCDEFGHIJK too long", ["ABCDEFGHIJ"]),
        ("US$100 and a$BC and $$XYZ", []),
        ("($BTC), $ETH. $SOL!", ["BTC", "ETH", "SOL"]),
        ("https://example.com/?a=$NOTAG", []),
        ("$_UNDER $ÄÖÜ $tag_x", []),
    ],
)
def test_cashtag_rules(text, expected):
    assert parse.extract_cashtags(text) == expected


def test_solana_and_evm_extraction():
    usdc = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
    wsol = "So11111111111111111111111111111111111111112"
    text = f"CA: {usdc} again {usdc}, wsol {wsol}; word abcdefghijkmnopqrstuvwxyzABCDEFGH"
    # 33 base58 letters that do not decode to 32 bytes are not a public key.
    assert parse.extract_solana_addresses(text, f"https://dexscreener.com/solana/{MINT_A}") == [usdc, wsol, MINT_A]
    # 45 chars: too long; contains 0/O/I/l: not base58.
    assert parse.extract_solana_addresses(usdc + "a", "0OIl" * 9) == []
    evm = "0x" + "aB" * 20
    assert parse.extract_evm_addresses(f"{evm} {evm.lower()} 0x{'a' * 41} x0x{'b' * 40}") == [evm.lower()]


def test_x_datetime_and_excerpt():
    assert parse.parse_x_datetime("Wed Oct 10 20:19:24 +0000 2018") == ms("2018-10-10T20:19:24")
    assert parse.parse_x_datetime("Wed Oct 10 22:19:24 +0200 2018") == ms("2018-10-10T20:19:24")
    assert parse.parse_x_datetime("2018-10-10T20:19:24Z") is None
    assert parse.parse_x_datetime(None) is None
    long_text = "word  \n\t " * 100
    excerpt = parse.collapse_excerpt(long_text)
    assert len(excerpt) == 280 and "  " not in excerpt and "\n" not in excerpt


def test_author_hash_matches_bb_social_hash_identity():
    assert parse.author_hash("1500000001") == hashlib.sha256(b"1500000001").hexdigest()[:16]
    assert parse.author_hash("") == ""
