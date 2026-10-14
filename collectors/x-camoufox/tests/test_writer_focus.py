import json

from x_collector.config import Config, read_accounts, read_list_file
from x_collector.focus import FOCUS_STALE_MS, FocusTracker, load_focus, parse_focus
from x_collector.writer import JsonlWriter, atomic_write_json, post_key, reply_key

T0 = 1_791_900_000_000


def test_post_dedupe_survives_restart_and_repairs_torn_line(tmp_path):
    path = tmp_path / "x-posts.jsonl"
    writer = JsonlWriter(path, post_key)
    written = writer.append([{"post_id": "1", "b": 1, "a": 2}, {"post_id": "1"}, {"post_id": "2"}])
    assert [row["post_id"] for row in written] == ["1", "2"]
    assert path.read_text().splitlines()[0] == '{"a":2,"b":1,"post_id":"1"}'  # key-sorted
    with path.open("a") as handle:
        handle.write('{"post_id": "3", "trunc')  # crash mid-write

    restarted = JsonlWriter(path, post_key)
    assert restarted.append([{"post_id": "1"}, {"post_id": "2"}]) == []
    assert [row["post_id"] for row in restarted.append([{"post_id": "3"}, {"post_id": "4"}])] == ["3", "4"]
    lines = path.read_text().splitlines()
    assert json.loads(lines[-1]) == {"post_id": "4"}
    assert json.loads(lines[-2]) == {"post_id": "3"}


def test_seen_set_restored_from_tail_only(tmp_path):
    path = tmp_path / "x-posts.jsonl"
    path.write_text("".join(json.dumps({"post_id": str(i), "pad": "x" * 50}) + "\n" for i in range(100)))
    writer = JsonlWriter(path, post_key, tail_bytes=700)
    assert writer.append([{"post_id": "99"}]) == []
    assert writer.append([{"post_id": "0"}]) != []  # older than the tail window


def test_reply_dedupe_is_per_parent(tmp_path):
    writer = JsonlWriter(tmp_path / "x-replies.jsonl", reply_key)
    rows = [
        {"parent_post_id": "1", "post_id": "10"},
        {"parent_post_id": "2", "post_id": "10"},
        {"parent_post_id": "1", "post_id": "10"},
    ]
    assert len(writer.append(rows)) == 2


def test_atomic_heartbeat_write(tmp_path):
    path = tmp_path / "x-collector-heartbeat.json"
    atomic_write_json(path, {"schema": "x-heartbeat-v1", "status": "ok"})
    atomic_write_json(path, {"schema": "x-heartbeat-v1", "status": "backoff"})
    assert json.loads(path.read_text())["status"] == "backoff"
    assert [p.name for p in tmp_path.iterdir()] == ["x-collector-heartbeat.json"]


def focus_doc(generated_at_ms, **overrides):
    doc = {
        "schema": "x-focus-v1",
        "generated_at_ms": generated_at_ms,
        "coins": [
            {"token_key": "solana:M", "tier": "hot", "cashtag": "PEPE", "search_queries": ["$PEPE", "M"]},
            {"token_key": "binance:PEPE", "tier": "focus", "cashtag": "PEPE", "search_queries": ["$PEPE"]},
        ],
        "resample_post_ids": ["11", "12"],
        "reply_sample_post_ids": ["11"],
    }
    doc.update(overrides)
    return doc


def test_parse_focus_caps_and_rejects_other_schemas():
    focus = parse_focus(
        focus_doc(T0, resample_post_ids=[str(i) for i in range(60)], reply_sample_post_ids=[str(i) for i in range(9)])
    )
    assert len(focus.resample_post_ids) == 40 and len(focus.reply_sample_post_ids) == 6
    assert focus.search_queries == ["$PEPE", "M"]
    assert parse_focus({"schema": "x-focus-v2", "generated_at_ms": T0}) is None
    assert parse_focus(focus_doc("soon")) is None


def test_focus_ids_due_again_only_after_newer_focus_file():
    tracker = FocusTracker()
    focus = parse_focus(focus_doc(T0))
    assert tracker.due_resamples(focus, T0 + 1_000) == ["11", "12"]
    tracker.mark_resampled("11", T0 + 2_000)
    # The same (older) file still lists 11: the engine has not seen the new row yet.
    assert tracker.due_resamples(focus, T0 + 120_000) == ["12"]
    newer = parse_focus(focus_doc(T0 + 60_000))
    # Newer file still lists it -> due again (after the minimum gap).
    assert tracker.due_resamples(newer, T0 + 61_000) == ["12"]  # 59s since the last read: too soon
    assert tracker.due_resamples(newer, T0 + 62_000) == ["11", "12"]
    assert tracker.due_reply_samples(newer, T0 + 62_000) == ["11"]
    tracker.mark_reply_sampled("11", T0 + 2_000)
    assert tracker.due_reply_samples(parse_focus(focus_doc(T0 + 3_000)), T0 + 4_000) == []


def test_stale_focus_file_is_ignored():
    tracker = FocusTracker()
    focus = parse_focus(focus_doc(T0))
    assert tracker.due_searches(focus, T0 + FOCUS_STALE_MS) == ["$PEPE", "M"]
    assert tracker.due_searches(focus, T0 + FOCUS_STALE_MS + 1) == []
    assert tracker.due_resamples(focus, T0 + FOCUS_STALE_MS + 1) == []


def test_focus_search_interval():
    tracker = FocusTracker()
    focus = parse_focus(focus_doc(T0))
    tracker.mark_searched("$PEPE", T0 + 200_000)
    assert tracker.due_searches(focus, T0 + 1_000) == ["M"]
    assert tracker.due_searches(focus, T0 + 200_000) == ["$PEPE", "M"]


def test_load_focus_missing_or_corrupt(tmp_path):
    assert load_focus(tmp_path / "missing.json") is None
    (tmp_path / "bad.json").write_text("{not json")
    assert load_focus(tmp_path / "bad.json") is None


def test_list_files_and_config_defaults(tmp_path):
    accounts = tmp_path / "accounts.txt"
    accounts.write_text("# example\n@LaunchWatcher\nlaunchwatcher\n\n  other_acct  \n")
    assert read_accounts(accounts) == ["launchwatcher", "other_acct"]
    searches = tmp_path / "searches.txt"
    searches.write_text('pump.fun\n#comment\n"CA:" #solana\npump.fun\n')
    assert read_list_file(searches) == ["pump.fun", '"CA:" #solana']

    config = Config.from_env({"X_COLLECTOR_DATA_DIR": str(tmp_path)})
    assert config.max_pages_per_hour == 120 and config.max_replies_per_hour == 400
    assert (config.poll_seconds, config.focus_poll_seconds) == (300, 180)
    assert (config.jitter_min, config.jitter_max) == (0.20, 0.40)
    assert config.focus_path == tmp_path / "x-focus.json"
    assert config.profile_dir == tmp_path / "x-camoufox-profile"
    assert config.headless is True
    assert Config.from_env({"X_COLLECTOR_HEADLESS": "virtual"}).headless == "virtual"
