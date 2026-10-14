"""Collector settings from `X_COLLECTOR_*` environment variables."""

from __future__ import annotations

import os
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

Headless = bool | Literal["virtual"]


class ConfigError(ValueError):
    pass


def _int(env: Mapping[str, str], name: str, default: int, minimum: int = 0) -> int:
    raw = env.get(name, "").strip()
    if not raw:
        return default
    try:
        value = int(raw)
    except ValueError as exc:
        raise ConfigError(f"{name} must be an integer, got {raw!r}") from exc
    if value < minimum:
        raise ConfigError(f"{name} must be >= {minimum}, got {value}")
    return value


def _float(env: Mapping[str, str], name: str, default: float, minimum: float = 0.0) -> float:
    raw = env.get(name, "").strip()
    if not raw:
        return default
    try:
        value = float(raw)
    except ValueError as exc:
        raise ConfigError(f"{name} must be a number, got {raw!r}") from exc
    if value < minimum:
        raise ConfigError(f"{name} must be >= {minimum}, got {value}")
    return value


def _bool(env: Mapping[str, str], name: str, default: bool) -> bool:
    raw = env.get(name, "").strip().lower()
    if not raw:
        return default
    if raw in ("1", "true", "yes", "on"):
        return True
    if raw in ("0", "false", "no", "off"):
        return False
    raise ConfigError(f"{name} must be true/false, got {raw!r}")


def _headless(env: Mapping[str, str]) -> Headless:
    raw = env.get("X_COLLECTOR_HEADLESS", "").strip().lower()
    if raw == "virtual":
        return "virtual"
    return _bool(env, "X_COLLECTOR_HEADLESS", True)


def _path(env: Mapping[str, str], name: str, default: Path) -> Path:
    raw = env.get(name, "").strip()
    return Path(raw) if raw else default


@dataclass(frozen=True)
class Config:
    data_dir: Path
    accounts_file: Path
    searches_file: Path
    profile_dir: Path
    posts_path: Path
    engagement_path: Path
    replies_path: Path
    heartbeat_path: Path
    focus_path: Path
    max_pages_per_hour: int = 120
    max_replies_per_hour: int = 400
    poll_seconds: int = 300
    focus_poll_seconds: int = 180
    jitter_min: float = 0.20
    jitter_max: float = 0.40
    headless: Headless = True
    block_images: bool = True
    page_timeout_seconds: int = 45

    @classmethod
    def from_env(cls, env: Mapping[str, str] | None = None) -> Config:
        env = os.environ if env is None else env
        data_dir = _path(env, "X_COLLECTOR_DATA_DIR", Path("/app/data"))
        jitter_min = _float(env, "X_COLLECTOR_JITTER_MIN", 0.20)
        jitter_max = _float(env, "X_COLLECTOR_JITTER_MAX", 0.40)
        if jitter_max < jitter_min:
            raise ConfigError("X_COLLECTOR_JITTER_MAX must be >= X_COLLECTOR_JITTER_MIN")
        return cls(
            data_dir=data_dir,
            accounts_file=_path(env, "X_COLLECTOR_ACCOUNTS_FILE", data_dir / "x-accounts.txt"),
            searches_file=_path(env, "X_COLLECTOR_SEARCHES_FILE", data_dir / "x-searches.txt"),
            profile_dir=_path(env, "X_COLLECTOR_PROFILE_DIR", data_dir / "x-camoufox-profile"),
            # Same file names as the engine's `XPaths::in_dir`.
            posts_path=data_dir / "x-posts.jsonl",
            engagement_path=data_dir / "x-engagement.jsonl",
            replies_path=data_dir / "x-replies.jsonl",
            heartbeat_path=data_dir / "x-collector-heartbeat.json",
            focus_path=data_dir / "x-focus.json",
            max_pages_per_hour=_int(env, "X_COLLECTOR_MAX_PAGES_PER_HOUR", 120, minimum=1),
            max_replies_per_hour=_int(env, "X_COLLECTOR_MAX_REPLIES_PER_HOUR", 400),
            poll_seconds=_int(env, "X_COLLECTOR_POLL_SECONDS", 300, minimum=60),
            focus_poll_seconds=_int(env, "X_COLLECTOR_FOCUS_POLL_SECONDS", 180, minimum=60),
            jitter_min=jitter_min,
            jitter_max=jitter_max,
            headless=_headless(env),
            block_images=_bool(env, "X_COLLECTOR_BLOCK_IMAGES", True),
            page_timeout_seconds=_int(env, "X_COLLECTOR_PAGE_TIMEOUT_SECONDS", 45, minimum=10),
        )

    @property
    def fingerprint_path(self) -> Path:
        """Stable fingerprint next to the profile, so restarts keep one identity."""
        return self.profile_dir.with_name(self.profile_dir.name + ".fingerprint.json")


def read_list_file(path: Path) -> list[str]:
    """Non-empty lines, deduplicated in order. Lines starting with `#` are comments
    (inline `#` is kept: search queries may contain hashtags)."""
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return []
    seen: set[str] = set()
    out: list[str] = []
    for line in text.splitlines():
        value = "" if line.lstrip().startswith("#") else line.strip()
        if value and value not in seen:
            seen.add(value)
            out.append(value)
    return out


def read_accounts(path: Path) -> list[str]:
    """Handles without `@`, lowercased for matching."""
    handles = [line.lstrip("@").strip().lower() for line in read_list_file(path)]
    return [handle for handle in dict.fromkeys(handles) if handle]
