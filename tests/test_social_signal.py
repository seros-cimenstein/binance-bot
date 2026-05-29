from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

from src import social_signal
from src.social_signal import (
    HeadlessSocialAnalyzer,
    SocialSignalRecord,
    SocialSignalSettings,
    parse_telegram_updates,
    parse_x_recent_search,
    social_evidence_for_assets,
    write_social_audit,
)


def test_parse_telegram_updates_redacts_identity_and_classifies_risk() -> None:
    payload = {
        "result": [
            {
                "update_id": 1,
                "message": {
                    "message_id": 22,
                    "date": 1_700_000_000,
                    "text": "$BONK pump group says buy now 100x",
                    "from": {"id": 12345},
                    "chat": {"id": -999},
                },
            }
        ]
    }

    records = parse_telegram_updates(payload, ("BONK", "ETH"))

    assert len(records) == 1
    record = records[0]
    assert record.source == "telegram"
    assert record.assets == ("BONK",)
    assert record.author_hash and record.author_hash != "12345"
    assert record.channel_hash and record.channel_hash != "-999"
    assert record.label == social_signal.SOCIAL_LABEL_MANIPULATION
    assert record.manipulation_risk >= 0.8


def test_parse_x_recent_search_extracts_assets_and_engagement() -> None:
    payload = {
        "data": [
            {
                "id": "post-1",
                "author_id": "user-1",
                "created_at": "2026-05-27T12:00:00+00:00",
                "text": "Watching #EDEN breakout after listing rumor",
                "public_metrics": {"retweet_count": 3, "like_count": 9},
            }
        ]
    }

    records = parse_x_recent_search(payload, ("EDEN", "BONK"))

    assert len(records) == 1
    assert records[0].source == "x"
    assert records[0].assets == ("EDEN",)
    assert records[0].label == social_signal.SOCIAL_LABEL_BULLISH
    assert records[0].engagement["like_count"] == 9


def test_social_evidence_summarizes_recent_records_and_blocks_manipulation(
    tmp_path: Path,
) -> None:
    now = datetime.now(timezone.utc)
    records = [
        SocialSignalRecord(
            source="telegram",
            message_id="1",
            timestamp=now.isoformat(),
            author_hash="a",
            channel_hash="c",
            excerpt="$EDEN pump group 100x",
            urls=(),
            assets=("EDEN",),
            label=social_signal.SOCIAL_LABEL_MANIPULATION,
            confidence=0.9,
            manipulation_risk=0.9,
        ),
        SocialSignalRecord(
            source="x",
            message_id="2",
            timestamp=(now - timedelta(minutes=1)).isoformat(),
            author_hash="b",
            channel_hash="public_search",
            excerpt="EDEN breakout watch",
            urls=(),
            assets=("EDEN",),
            label=social_signal.SOCIAL_LABEL_BULLISH,
            confidence=0.7,
            manipulation_risk=0.0,
        ),
    ]
    audit_path = tmp_path / "social.jsonl"
    write_social_audit(audit_path, records)

    evidence = social_evidence_for_assets(
        SocialSignalSettings(
            enabled=True,
            audit_path=str(audit_path),
            max_age_minutes=10,
            min_source_diversity=2,
            block_manipulation_risk=True,
            manipulation_block_score=0.8,
        ),
        ("EDEN", "BONK"),
        now=now,
    )

    assert evidence["EDEN"]["mention_count"] == 2
    assert evidence["EDEN"]["source_diversity"] == 2
    assert evidence["EDEN"]["block_route"] is True
    assert evidence["BONK"]["status"] == "empty"


def test_headless_social_analyzer_overrides_heuristic_label(
    monkeypatch,
) -> None:
    record = SocialSignalRecord(
        source="telegram",
        message_id="m1",
        timestamp=datetime.now(timezone.utc).isoformat(),
        author_hash="a",
        channel_hash="c",
        excerpt="EDEN neutral mention",
        urls=(),
        assets=("EDEN",),
        label=social_signal.SOCIAL_LABEL_NEUTRAL,
        confidence=0.4,
        manipulation_risk=0.0,
    )

    class Completed:
        returncode = 0
        stderr = ""
        stdout = json.dumps(
            {
                "structured_output": {
                    "signals": [
                        {
                            "message_id": "m1",
                            "label": social_signal.SOCIAL_LABEL_BEARISH,
                            "confidence": 0.82,
                            "manipulation_risk": 0.25,
                        }
                    ]
                }
            }
        )

    def fake_run(argv: list[str], **kwargs: Any) -> Completed:
        assert "--tools" in argv
        assert argv[argv.index("--tools") + 1] == ""
        assert kwargs["timeout"] == 120.0
        return Completed()

    monkeypatch.setattr(social_signal.subprocess, "run", fake_run)

    classified = HeadlessSocialAnalyzer(SocialSignalSettings(ai_enabled=True)).classify(
        [record]
    )

    assert classified[0].label == social_signal.SOCIAL_LABEL_BEARISH
    assert classified[0].confidence == 0.82
    assert classified[0].manipulation_risk == 0.25
