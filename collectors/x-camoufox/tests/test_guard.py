import pytest

from x_collector.guard import decide, graphql_operation

GQL = "https://x.com/i/api/graphql/AbCdEf123456/{op}?variables=%7B%7D&features=%7B%7D"


@pytest.mark.parametrize(
    "op", ["SearchTimeline", "UserTweets", "UserByScreenName", "TweetDetail", "TweetResultByRestId"]
)
def test_read_operations_allowed(op):
    assert decide("GET", GQL.format(op=op)).allowed


@pytest.mark.parametrize(
    ("method", "url"),
    [
        ("POST", "https://x.com/i/api/graphql/a1b2c3/CreateTweet"),
        ("POST", "https://x.com/i/api/graphql/a1b2c3/FavoriteTweet"),
        ("POST", "https://x.com/i/api/graphql/a1b2c3/CreateRetweet"),
        ("POST", "https://x.com/i/api/graphql/a1b2c3/FollowUser"),
        ("POST", "https://x.com/i/api/graphql/a1b2c3/CreateBookmark"),
        ("POST", "https://twitter.com/i/api/graphql/a1b2c3/DeleteTweet"),
        ("POST", "https://api.x.com/graphql/a1b2c3/CreateTweet"),
        # Mutations sent as GET, private-content reads and nameless operations.
        ("GET", "https://x.com/i/api/graphql/a1b2c3/CreateTweet"),
        ("GET", "https://x.com/i/api/graphql/a1b2c3/DMInboxTimeline"),
        ("GET", "https://x.com/i/api/graphql/a1b2c3/Bookmarks"),
        ("GET", "https://x.com/i/api/graphql/a1b2c3/NotificationsTimeline"),
        ("GET", "https://x.com/i/api/graphql/a1b2c3"),
        ("GET", "https://x.com/i/api/1.1/dm/inbox_initial_state.json"),
        ("GET", "https://x.com/i/api/2/notifications/all.json"),
        # REST writes.
        ("POST", "https://x.com/i/api/1.1/friendships/create.json"),
        ("POST", "https://x.com/i/api/1.1/dm/new2.json"),
        ("DELETE", "https://x.com/i/api/2/timeline/foo.json"),
        ("POST", "https://api.x.com/1.1/statuses/update.json"),
        ("PUT", "https://www.x.com/i/api/1.1/account/settings.json"),
        ("POST", "https://x.com/i/api/1.1/jot/client_event.json"),
        ("POST", "https://upload.x.com/i/media/upload.json"),
    ],
)
def test_writes_and_unlisted_operations_blocked(method, url):
    assert not decide(method, url).allowed


@pytest.mark.parametrize(
    ("method", "url"),
    [
        ("GET", "https://x.com/search?q=pump.fun&src=typed_query&f=live"),
        ("GET", "https://x.com/i/api/2/badge_count/badge_count.json"),
        # Page bootstrap reads that are not parsed but must not break the page.
        ("GET", "https://x.com/i/api/graphql/a1b2c3/Viewer"),
        ("GET", "https://x.com/i/api/graphql/a1b2c3/HomeTimeline"),
        ("GET", "https://abs.twimg.com/responsive-web/client-web/main.js"),
        ("POST", "https://example.com/i/api/1.1/whatever"),
    ],
)
def test_non_write_traffic_allowed(method, url):
    assert decide(method, url).allowed


def test_login_policy_allows_login_flow_but_not_writes():
    assert decide("POST", "https://api.x.com/1.1/onboarding/task.json?flow_name=login", "login").allowed
    assert decide("POST", "https://x.com/i/api/1.1/onboarding/task.json", "login").allowed
    assert decide("GET", "https://x.com/i/api/graphql/a1b2c3/Viewer", "login").allowed
    assert not decide("POST", "https://x.com/i/api/graphql/a1b2c3/CreateTweet", "login").allowed
    assert not decide("POST", "https://x.com/i/api/graphql/a1b2c3/FavoriteTweet", "login").allowed
    assert not decide("POST", "https://x.com/i/api/1.1/friendships/create.json", "login").allowed
    # The collect policy blocks the login flow's REST writes.
    assert not decide("POST", "https://api.x.com/1.1/onboarding/task.json").allowed


def test_operation_name_is_last_path_segment():
    assert graphql_operation("https://x.com/i/api/graphql/h/SearchTimeline?variables=x") == "SearchTimeline"
    assert graphql_operation("https://api.twitter.com/graphql/h/TweetDetail") == "TweetDetail"
    assert graphql_operation("https://x.com/i/api/1.1/friendships/create.json") is None
    assert graphql_operation("https://evil.example/i/api/graphql/h/CreateTweet") is None
