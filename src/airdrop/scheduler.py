"""Weekly activity scheduler for the airdrop farmer.

Takes the wallet store + opportunity catalog and produces a per-wallet
checklist of things to do this week. The plan is the source of truth;
the operator (or a future per-protocol executor) acts on each row.

Algorithm:

  for each wallet:
      for each opportunity matching the wallet's chain and status filter:
          emit a checklist row with min_actions_per_week and min_size_usd

Each row also notes the LAST week the scheduler tracked the activity
as completed (read from a small persistent state file). When a row's
``last_done_iso_week`` is the current ISO week, the row is annotated
``status="completed"``; otherwise ``status="pending"``.

This is intentionally low-tech: the operator marks rows as done via
``mark_done`` which updates the state file. The output of the scheduler
is a Telegram-friendly checklist.

All three state files (wallet store, completion state, activity snapshots)
default to ``~/.local/share/binance-bot/data``. Set ``AIRDROP_DATA_DIR`` to
relocate them in one shot; the docker image points it at the ``/app/data``
volume so state survives container recreates. Explicit ``--store-path`` /
``--state-path`` / ``--activity-path`` flags still win over the env var.
"""

from __future__ import annotations

import argparse
import getpass
import json
import os
from collections.abc import Sequence
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from src.airdrop.catalog import AirdropOpportunity, filter_by
from src.airdrop.wallet import (
    CHAIN_EVM,
    CHAIN_SOLANA,
    AirdropWalletError,
    EncryptedWallet,
    WalletStore,
    add_wallet,
    load_store,
    save_store,
)


@dataclass(frozen=True, slots=True)
class TaskKey:
    """Stable identifier for one (wallet, opportunity) pair."""

    wallet_name: str
    opportunity_name: str

    def as_str(self) -> str:
        return f"{self.wallet_name}::{self.opportunity_name}"

    @classmethod
    def from_str(cls, raw: str) -> "TaskKey":
        try:
            wallet, opp = raw.split("::", 1)
        except ValueError:
            raise ValueError(f"task key '{raw}' must contain '::'")
        return cls(wallet_name=wallet, opportunity_name=opp)


@dataclass(slots=True)
class CompletionState:
    """Per-task last-completed-week marker."""

    completions: dict[str, str] = field(default_factory=dict)

    def is_completed_this_week(self, key: TaskKey, *, current_iso_week: str) -> bool:
        return self.completions.get(key.as_str()) == current_iso_week

    def mark(self, key: TaskKey, *, iso_week: str) -> None:
        self.completions[key.as_str()] = iso_week

    def unmark(self, key: TaskKey) -> None:
        self.completions.pop(key.as_str(), None)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "CompletionState":
        raw = payload.get("completions") if isinstance(payload, dict) else None
        if not isinstance(raw, dict):
            return cls()
        return cls(completions={str(k): str(v) for k, v in raw.items()})

    def to_dict(self) -> dict[str, Any]:
        return {"completions": dict(self.completions)}


@dataclass(frozen=True, slots=True)
class ChecklistRow:
    wallet_name: str
    wallet_address: str
    chain: str
    opportunity: str
    url: str
    activity_kind: str
    min_actions_per_week: int
    min_size_usd: float
    confidence: str
    status: str          # "completed" | "pending"
    notes: str = ""

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


def current_iso_week(now: datetime | None = None) -> str:
    """Return the ISO-week label (e.g. ``"2026-W22"``) we use as the marker."""
    n = now or datetime.now(tz=timezone.utc)
    iso = n.isocalendar()
    return f"{iso.year}-W{iso.week:02d}"


def build_checklist(
    *,
    store: WalletStore,
    state: CompletionState,
    iso_week: str,
    min_confidence: str | None = None,
    status_filter: str | None = "active",
) -> list[ChecklistRow]:
    rows: list[ChecklistRow] = []
    for wallet in store.wallets:
        opps = filter_by(chain=wallet.chain, status=status_filter,
                          min_confidence=min_confidence)
        for opp in opps:
            key = TaskKey(wallet_name=wallet.name, opportunity_name=opp.name)
            status = ("completed"
                      if state.is_completed_this_week(key, current_iso_week=iso_week)
                      else "pending")
            rows.append(ChecklistRow(
                wallet_name=wallet.name, wallet_address=wallet.address,
                chain=wallet.chain, opportunity=opp.name, url=opp.url,
                activity_kind=opp.activity_kind,
                min_actions_per_week=opp.min_actions_per_week,
                min_size_usd=opp.min_size_usd,
                confidence=opp.confidence,
                status=status, notes=opp.notes,
            ))
    return rows


def load_state(path: Path | str) -> CompletionState:
    p = Path(path)
    if not p.exists():
        return CompletionState()
    try:
        return CompletionState.from_dict(json.loads(p.read_text()))
    except (OSError, json.JSONDecodeError) as exc:
        raise AirdropWalletError(f"could not read scheduler state at {p}: {exc}") from exc


def save_state(state: CompletionState, path: Path | str) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(p.suffix + ".tmp")
    tmp.write_text(json.dumps(state.to_dict(), indent=2, sort_keys=True))
    tmp.replace(p)


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------


def render_checklist(rows: Sequence[ChecklistRow], *, iso_week: str) -> str:
    lines = []
    lines.append("=" * 72)
    lines.append(f"  AIRDROP CHECKLIST  ({iso_week})")
    lines.append("=" * 72)
    if not rows:
        lines.append("  (no tasks scheduled; either wallet store is empty or no active opportunities)")
        return "\n".join(lines)
    by_wallet: dict[str, list[ChecklistRow]] = {}
    for row in rows:
        by_wallet.setdefault(row.wallet_name, []).append(row)
    for wallet, wallet_rows in by_wallet.items():
        chain = wallet_rows[0].chain
        address = wallet_rows[0].wallet_address
        lines.append("")
        lines.append(f"  WALLET: {wallet}  ({chain})")
        lines.append(f"  ADDRESS: {address}")
        for row in wallet_rows:
            mark = "[x]" if row.status == "completed" else "[ ]"
            lines.append(
                f"    {mark} {row.opportunity:30s}  {row.activity_kind}"
            )
            lines.append(
                f"        {row.url}  "
                f"({row.min_actions_per_week}x/wk, min ${row.min_size_usd:.0f}, "
                f"confidence={row.confidence})"
            )
            if row.notes:
                lines.append(f"        note: {row.notes}")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Default paths
# ---------------------------------------------------------------------------

DATA_DIR_ENV = "AIRDROP_DATA_DIR"
_DEFAULT_DATA_DIR = "~/.local/share/binance-bot/data"


def default_data_dir() -> Path:
    """Base directory for airdrop state.

    ``AIRDROP_DATA_DIR`` overrides the systemd-era default. The docker image
    sets it to ``/app/data`` (the mounted volume) so scheduler state survives
    container recreates. Unset, behavior is unchanged: state lives under
    ``~/.local/share/binance-bot/data``.
    """
    raw = os.environ.get(DATA_DIR_ENV, "").strip()
    return Path(raw or _DEFAULT_DATA_DIR).expanduser()


def default_store_path() -> Path:
    return default_data_dir() / "airdrop-wallets.json"


def default_state_path() -> Path:
    return default_data_dir() / "airdrop-scheduler.json"


def default_activity_path() -> Path:
    return default_data_dir() / "airdrop-activity.json"


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def _read_passphrase(*, env: str = "AIRDROP_WALLET_PASSPHRASE") -> str:
    pp = os.environ.get(env, "")
    if pp:
        return pp
    return getpass.getpass("Airdrop wallet store passphrase: ")


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Manage the airdrop wallet store and produce a weekly checklist."
    )
    parser.add_argument("--store-path", default=str(default_store_path()))
    parser.add_argument("--state-path", default=str(default_state_path()))
    sub = parser.add_subparsers(dest="command", required=True)

    p_add = sub.add_parser("add", help="generate + add a new wallet")
    p_add.add_argument("name")
    p_add.add_argument("chain", choices=("solana", "evm"))

    sub.add_parser("list", help="list known wallets (no decryption)")

    p_check = sub.add_parser("checklist", help="render this week's per-wallet checklist")
    p_check.add_argument("--min-confidence", default=None,
                          choices=("low", "medium", "high"))
    p_check.add_argument("--status-filter", default="active",
                          choices=("active", "speculative", "ended", "any"))
    p_check.add_argument("--json", action="store_true")

    p_done = sub.add_parser("done", help="mark a task as done this week")
    p_done.add_argument("wallet_name")
    p_done.add_argument("opportunity_name")

    p_undone = sub.add_parser("undone", help="reset a task back to pending")
    p_undone.add_argument("wallet_name")
    p_undone.add_argument("opportunity_name")

    p_remind = sub.add_parser(
        "remind",
        help="snapshot wallet activity, auto-mark active wallets done, "
             "and push the pending digest to Telegram",
    )
    p_remind.add_argument(
        "--activity-path",
        default=str(default_activity_path()),
    )

    args = parser.parse_args(argv)

    try:
        store = load_store(args.store_path)
        state = load_state(args.state_path)
    except AirdropWalletError as exc:
        print(f"airdrop scheduler: {exc}")
        return 1

    if args.command == "add":
        try:
            pp = _read_passphrase()
            wallet = add_wallet(store, name=args.name, chain=args.chain, passphrase=pp)
            save_store(store, args.store_path)
            print(json.dumps({
                "added": wallet.name, "chain": wallet.chain, "address": wallet.address,
            }, indent=2))
        except AirdropWalletError as exc:
            print(f"airdrop scheduler: {exc}")
            return 2
        return 0

    if args.command == "list":
        print(json.dumps({
            "wallets": [
                {"name": w.name, "chain": w.chain, "address": w.address}
                for w in store.wallets
            ],
        }, indent=2))
        return 0

    iso = current_iso_week()
    if args.command == "checklist":
        status_filter = None if args.status_filter == "any" else args.status_filter
        rows = build_checklist(
            store=store, state=state, iso_week=iso,
            min_confidence=args.min_confidence, status_filter=status_filter,
        )
        if args.json:
            print(json.dumps({
                "iso_week": iso,
                "rows": [r.as_dict() for r in rows],
            }, indent=2))
        else:
            print(render_checklist(rows, iso_week=iso))
        return 0

    if args.command in {"done", "undone"}:
        key = TaskKey(wallet_name=args.wallet_name, opportunity_name=args.opportunity_name)
        if args.command == "done":
            state.mark(key, iso_week=iso)
        else:
            state.unmark(key)
        save_state(state, args.state_path)
        print(json.dumps({"task": key.as_str(), "command": args.command,
                           "iso_week": iso}, indent=2))
        return 0

    if args.command == "remind":
        import asyncio

        from src.airdrop.reminder import ReminderSettings, run_reminder

        settings = ReminderSettings.from_env()
        result = asyncio.run(run_reminder(
            settings=settings,
            store_path=Path(args.store_path),
            state_path=Path(args.state_path),
            activity_path=Path(args.activity_path),
        ))
        print(json.dumps({
            "iso_week": result.iso_week,
            "active_wallets": result.active_wallets,
            "auto_marked": result.auto_marked,
            "pending_rows": result.pending_rows,
            "telegram_pushed": result.telegram_pushed,
        }, indent=2))
        return 0

    return 0


if __name__ == "__main__":
    raise SystemExit(run())
