import random

import pytest

from x_collector.pacer import (
    BACKOFF_MAX_MS,
    BACKOFF_START_MS,
    HOUR_MS,
    BlockKind,
    Pacer,
    classify_block,
)

T0 = 1_791_900_000_000


def test_hourly_page_cap_is_rolling():
    pacer = Pacer(max_pages_per_hour=3, max_replies_per_hour=400)
    for offset in (0, 1_000, 2_000):
        assert pacer.can_load(T0 + offset)
        pacer.record_page(T0 + offset)
    assert not pacer.can_load(T0 + 3_000)
    assert pacer.next_page_slot_ms(T0 + 3_000) == T0 + HOUR_MS
    assert not pacer.can_load(T0 + HOUR_MS - 1)
    assert pacer.can_load(T0 + HOUR_MS)  # the first load left the window
    assert pacer.page_budget_left(T0 + HOUR_MS) == 1


def test_reply_budget():
    pacer = Pacer(max_pages_per_hour=120, max_replies_per_hour=400)
    pacer.replies.add(T0, 390)
    assert pacer.reply_budget_left(T0 + 1) == 10
    pacer.replies.add(T0 + 2, 15)
    assert pacer.reply_budget_left(T0 + 3) == 0
    assert pacer.reply_budget_left(T0 + HOUR_MS) == 385


def test_jitter_is_20_to_40_percent_slower():
    pacer = Pacer(120, 400, rng=random.Random(7))
    samples = [pacer.jittered_ms(300_000) for _ in range(2_000)]
    assert min(samples) >= 360_000 and max(samples) <= 420_000
    assert max(samples) - min(samples) > 50_000


def test_backoff_doubles_from_5_minutes_to_2_hours_and_resets():
    pacer = Pacer(120, 400)
    delays = []
    for step in range(8):
        now = T0 + step
        pacer.on_block(BlockKind.RATE_LIMIT, now)
        delays.append(pacer.backoff_until_ms - now)
        assert not pacer.can_load(now)
        assert pacer.status(now) == "backoff"
    assert delays == [BACKOFF_START_MS * 2**i for i in range(5)] + [BACKOFF_MAX_MS] * 3
    pacer.on_success()
    assert pacer.backoff_until_ms is None and pacer.status(T0) == "ok"
    pacer.on_block(BlockKind.RATE_LIMIT, T0)
    assert pacer.backoff_until_ms - T0 == BACKOFF_START_MS


def test_login_wall_reports_login_required_until_success():
    pacer = Pacer(120, 400)
    pacer.on_block(BlockKind.LOGIN_WALL, T0)
    assert pacer.status(T0) == "login_required"
    assert pacer.status(T0 + BACKOFF_MAX_MS) == "login_required"
    assert pacer.can_load(T0 + BACKOFF_START_MS)
    pacer.on_success()
    assert pacer.status(T0) == "ok"


def test_three_consecutive_challenges_stop_until_restart():
    pacer = Pacer(120, 400)
    pacer.on_block(BlockKind.CHALLENGE, T0)
    pacer.on_block(BlockKind.CHALLENGE, T0 + 1)
    assert not pacer.stopped and pacer.status(T0 + 1) == "backoff"
    pacer.on_block(BlockKind.CHALLENGE, T0 + 2)
    assert pacer.stopped
    assert pacer.status(T0 + 10 * HOUR_MS) == "challenge"
    assert not pacer.can_load(T0 + 10 * HOUR_MS)


def test_success_between_challenges_resets_the_count():
    pacer = Pacer(120, 400)
    pacer.on_block(BlockKind.CHALLENGE, T0)
    pacer.on_block(BlockKind.CHALLENGE, T0 + 1)
    pacer.on_success()
    pacer.on_block(BlockKind.CHALLENGE, T0 + 2)
    pacer.on_block(BlockKind.RATE_LIMIT, T0 + 3)  # not a success: does not reset
    pacer.on_block(BlockKind.CHALLENGE, T0 + 4)
    assert not pacer.stopped
    pacer.on_block(BlockKind.CHALLENGE, T0 + 5)
    assert pacer.stopped


@pytest.mark.parametrize(
    ("url", "statuses", "codes", "logged_in", "expected"),
    [
        ("https://x.com/search?q=a&f=live", [200, 200], [], True, None),
        ("https://x.com/search?q=a&f=live", [200, 429], [], True, BlockKind.RATE_LIMIT),
        ("https://x.com/search?q=a&f=live", [200], [88], True, BlockKind.RATE_LIMIT),
        ("https://x.com/i/flow/login?redirect_after_login=%2Fsearch", [200], [], True, BlockKind.LOGIN_WALL),
        ("https://x.com/search?q=a", [200], [], False, BlockKind.LOGIN_WALL),
        ("https://x.com/account/access", [200], [], True, BlockKind.CHALLENGE),
        ("https://x.com/home", [200], [326], True, BlockKind.CHALLENGE),
        # A handle that merely starts with "login" is a normal profile page.
        ("https://x.com/loginbot", [200], [], True, None),
    ],
)
def test_classify_block(url, statuses, codes, logged_in, expected):
    assert classify_block(url, statuses, codes, logged_in) == expected
