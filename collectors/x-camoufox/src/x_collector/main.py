"""CLI: `python -m x_collector [--login | --once]`."""

from __future__ import annotations

import argparse
import asyncio
import logging
import sys

from .config import Config, ConfigError


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="x_collector",
        description=(
            "Phase 43 read-only X collector (Camoufox). Writes x-posts.jsonl, "
            "x-engagement.jsonl, x-replies.jsonl and x-collector-heartbeat.json into "
            "X_COLLECTOR_DATA_DIR. Paper only: it never posts, likes, follows, reposts or "
            "opens DMs, and holds no exchange credentials."
        ),
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--login",
        action="store_true",
        help="open a headful browser once to log in to the burner account; the session "
        "is stored in X_COLLECTOR_PROFILE_DIR",
    )
    mode.add_argument(
        "--once",
        action="store_true",
        help="run one pass over focus work, accounts and searches, then exit",
    )
    parser.add_argument("--verbose", action="store_true", help="debug logging")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    try:
        config = Config.from_env()
    except ConfigError as exc:
        print(f"x_collector: {exc}", file=sys.stderr)
        return 2

    from .runner import Collector

    collector = Collector(config)
    if args.login:
        return asyncio.run(collector.login())
    return asyncio.run(collector.run(once=args.once))


if __name__ == "__main__":
    sys.exit(main())
