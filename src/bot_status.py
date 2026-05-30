"""Unified status snapshot across every bot deployed in this repo.

Single command that summarises:

* The Binance spot/futures wallet (balances + per-asset USD value).
* All known systemd timers (active/inactive, next firing).
* Per-bot state from on-disk JSON: last cycle, current holdings, last
  decision, last executed trade.
* Lifetime / 24h P&L per bot, reconstructed from ledger JSONLs.

Output is a single one-screen plain-text block (default) or JSON
(``--json`` flag) suitable for piping to ``jq`` or shipping to Telegram.

This is a pure read-only tool: no orders, no state mutation. Safe to run
any time.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import subprocess
from collections.abc import Mapping
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from dotenv import load_dotenv


DEFAULT_DATA_DIR = Path(os.environ.get("BOT_STATUS_DATA_DIR",
                                       os.path.expanduser("~/.local/share/binance-bot/data")))


KNOWN_BOTS: dict[str, dict[str, str]] = {
    "trend_filter": {
        "state": "trend-filter-state.json",
        "audit": "trend-filter-decisions.jsonl",
        "ledger": "trend-filter-ledger.jsonl",
        "timer": "binance-trend-filter.timer",
    },
    "funding_arb": {
        "state": "funding-arb-state.json",
        "audit": "funding-arb-decisions.jsonl",
        "ledger": "funding-arb-ledger.jsonl",
        "timer": "binance-funding-arb.timer",
    },
    "listing_detector": {
        "state": "listing-detector-state.json",
        "audit": "listing-detector-events.jsonl",
        "ledger": "listing-detector-ledger.jsonl",
        "timer": "binance-listing-detector.timer",
    },
    "claude_spot": {
        "state": "",  # stateless cycle
        "audit": "claude-spot-decisions.jsonl",
        "ledger": "",
        "timer": "binance-claude-spot.timer",
    },
}


@dataclass(slots=True)
class WalletSnapshot:
    spot_usdt_free: float = 0.0
    futures_usdt_free: float = 0.0
    positions: list[dict[str, Any]] = field(default_factory=list)  # [{asset, qty, value_usdt}]
    total_value_usdt: float = 0.0
    last_prices: dict[str, float] = field(default_factory=dict)  # symbol -> last price


@dataclass(slots=True)
class TimerSnapshot:
    unit: str
    active: bool
    enabled: bool
    next_trigger: str = ""
    last_trigger: str = ""


@dataclass(slots=True)
class BotSnapshot:
    name: str
    timer: TimerSnapshot | None = None
    last_audit: Mapping[str, Any] | None = None
    state_summary: str = ""
    pnl_lifetime_usdt: float = 0.0
    pnl_24h_usdt: float = 0.0
    trades_lifetime: int = 0
    trades_24h: int = 0
    notes: list[str] = field(default_factory=list)


# ---------------------------------------------------------------------------
# Wallet
# ---------------------------------------------------------------------------


async def _fetch_wallet() -> WalletSnapshot:
    snap = WalletSnapshot()
    try:
        import ccxt.async_support as ccxt_async

        load_dotenv(dotenv_path=os.environ.get(
            "DOTENV_PATH", os.path.expanduser("~/.local/share/binance-bot/.env")))
        api_key = os.getenv("BINANCE_API_KEY", "")
        api_secret = os.getenv("BINANCE_API_SECRET", "")
        if not api_key:
            return snap
        spot = ccxt_async.binance({"apiKey": api_key, "secret": api_secret,
                                    "enableRateLimit": True,
                                    "options": {"adjustForTimeDifference": True,
                                                "fetchCurrencies": False}})
        futures = ccxt_async.binanceusdm({"apiKey": api_key, "secret": api_secret,
                                            "enableRateLimit": True,
                                            "options": {"adjustForTimeDifference": True,
                                                        "fetchCurrencies": False}})
        try:
            spot_bal = await spot.fetch_balance()
            spot_total = {k: float(v) for k, v in (spot_bal.get("total") or {}).items()
                          if v and float(v) > 0}
            spot_free = {k: float(v) for k, v in (spot_bal.get("free") or {}).items()
                         if v and float(v) > 0}
            snap.spot_usdt_free = float(spot_free.get("USDT", 0.0))

            tickers = await spot.fetch_tickers()
            for sym, t in tickers.items():
                if t and t.get("last") is not None:
                    try:
                        snap.last_prices[sym] = float(t["last"])
                    except (TypeError, ValueError):
                        continue

            for asset, qty in spot_total.items():
                if asset == "USDT":
                    val = qty
                else:
                    last_px = snap.last_prices.get(f"{asset}/USDT", 0.0)
                    val = qty * last_px
                if val < 0.001:
                    continue
                snap.positions.append({"asset": asset, "qty": qty,
                                       "value_usdt": val, "venue": "spot"})

            try:
                fut_bal = await futures.fetch_balance()
                snap.futures_usdt_free = float(
                    (fut_bal.get("free") or {}).get("USDT") or 0.0
                )
            except Exception:  # noqa: BLE001
                pass
        finally:
            try:
                await spot.close()
            except Exception:  # noqa: BLE001
                pass
            try:
                await futures.close()
            except Exception:  # noqa: BLE001
                pass
    except Exception:  # noqa: BLE001 — wallet read is best-effort
        pass
    snap.total_value_usdt = (
        snap.spot_usdt_free + snap.futures_usdt_free
        + sum(p["value_usdt"] for p in snap.positions if p["asset"] != "USDT")
    )
    return snap


# ---------------------------------------------------------------------------
# systemd timers
# ---------------------------------------------------------------------------


def _systemctl_show(unit: str, *props: str) -> dict[str, str]:
    cmd = ["systemctl", "--user", "show", unit, "-p", ",".join(props)]
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=5)
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return {}
    result: dict[str, str] = {}
    for line in out.stdout.splitlines():
        if "=" in line:
            k, _, v = line.partition("=")
            result[k] = v
    return result


def _systemctl_is(unit: str, what: str) -> bool:
    try:
        out = subprocess.run(["systemctl", "--user", f"is-{what}", unit],
                              capture_output=True, text=True, timeout=5)
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return False
    return out.stdout.strip() == what.split("-", 1)[0] or out.stdout.strip() == what


def _timer_snapshot(unit: str) -> TimerSnapshot:
    info = _systemctl_show(unit, "ActiveState", "UnitFileState", "NextElapseUSecRealtime",
                            "LastTriggerUSec")
    active = info.get("ActiveState") == "active"
    enabled = info.get("UnitFileState") == "enabled"
    return TimerSnapshot(
        unit=unit, active=active, enabled=enabled,
        next_trigger=info.get("NextElapseUSecRealtime", ""),
        last_trigger=info.get("LastTriggerUSec", ""),
    )


# ---------------------------------------------------------------------------
# Per-bot state
# ---------------------------------------------------------------------------


def _read_last_line(path: Path) -> str | None:
    if not path.exists():
        return None
    try:
        with path.open("rb") as fh:
            try:
                fh.seek(-2, os.SEEK_END)
                while fh.read(1) != b"\n":
                    fh.seek(-2, os.SEEK_CUR)
            except OSError:
                fh.seek(0)
            return fh.readline().decode(errors="replace").strip()
    except OSError:
        return None


def _read_jsonl_iter(path: Path):
    if not path.exists():
        return
    with path.open("r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue


def _ledger_pnl(path: Path, *, hours_ago: float | None = None) -> tuple[float, int]:
    """Sum net_pnl_usdt and count trades from a ledger JSONL.

    For per-bot ledger shapes that differ, this picks the first present of:
    ``net_pnl_usdt`` (funding-arb), ``notional_usdt - cost`` not applicable.
    """
    if not path.exists():
        return 0.0, 0
    cutoff_ms: float | None = None
    if hours_ago is not None:
        cutoff_ms = (datetime.now(tz=timezone.utc).timestamp() - hours_ago * 3600) * 1000
    total = 0.0
    count = 0
    for row in _read_jsonl_iter(path):
        ts = row.get("close_time_ms") or row.get("timestamp_ms") or 0
        if cutoff_ms is not None and float(ts) < cutoff_ms:
            continue
        pnl = row.get("net_pnl_usdt")
        if pnl is None:
            # Trend-filter ledger rows are individual fills; we count them but
            # report 0 P&L per row (unrealized P&L is computed elsewhere).
            count += 1
            continue
        try:
            total += float(pnl)
            count += 1
        except (TypeError, ValueError):
            continue
    return total, count


def _bot_snapshot(name: str, *, data_dir: Path, wallet: WalletSnapshot) -> BotSnapshot:
    paths = KNOWN_BOTS[name]
    snap = BotSnapshot(name=name)
    if paths.get("timer"):
        snap.timer = _timer_snapshot(paths["timer"])
    audit_path = data_dir / paths["audit"] if paths.get("audit") else None
    if audit_path is not None:
        last = _read_last_line(audit_path)
        if last is not None:
            try:
                snap.last_audit = json.loads(last)
            except json.JSONDecodeError:
                pass
    state_path = data_dir / paths["state"] if paths.get("state") else None
    if state_path is not None and state_path.exists():
        try:
            state = json.loads(state_path.read_text())
        except json.JSONDecodeError:
            state = {}
        snap.state_summary = _summarize_state(name, state, wallet)
    ledger_path = data_dir / paths["ledger"] if paths.get("ledger") else None
    if ledger_path is not None:
        total, count = _ledger_pnl(ledger_path)
        snap.pnl_lifetime_usdt = total
        snap.trades_lifetime = count
        total_24, count_24 = _ledger_pnl(ledger_path, hours_ago=24.0)
        snap.pnl_24h_usdt = total_24
        snap.trades_24h = count_24
    return snap


def _summarize_state(name: str, state: Mapping[str, Any], wallet: WalletSnapshot) -> str:
    if name == "trend_filter":
        holdings = state.get("holdings") or []
        if not holdings:
            return "no holdings (cash position)"
        parts = []
        for h in holdings:
            sym = h.get("symbol", "")
            qty = float(h.get("qty") or 0.0)
            entry = float(h.get("entry_price") or 0.0)
            asset = sym.split("/", 1)[0]
            cur_price = wallet.last_prices.get(sym, 0.0)
            pnl_pct = (cur_price / entry - 1.0) * 100.0 if entry > 0 and cur_price > 0 else 0.0
            position_value = qty * cur_price if cur_price > 0 else 0.0
            parts.append(
                f"{qty:.6f} {asset} (entry ${entry:.2f}, now ${cur_price:.2f}, "
                f"{pnl_pct:+.2f}%, value ${position_value:.4f})"
            )
        return "; ".join(parts)
    if name == "funding_arb":
        positions = state.get("positions") or []
        if not positions:
            return "no open positions"
        return f"{len(positions)} open: " + ", ".join(p.get("symbol", "") for p in positions)
    if name == "listing_detector":
        cursor = state.get("last_seen_article_id", 0)
        return f"cursor article #{cursor}"
    return ""


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------


def _format_us_timestamp(usec_str: str) -> str:
    if not usec_str or usec_str == "0":
        return "-"
    try:
        usec = int(usec_str)
    except ValueError:
        return usec_str
    if usec <= 0:
        return "-"
    dt = datetime.fromtimestamp(usec / 1_000_000, tz=timezone.utc)
    return dt.strftime("%Y-%m-%d %H:%M UTC")


def _render_plaintext(wallet: WalletSnapshot, bots: list[BotSnapshot]) -> str:
    lines: list[str] = []
    now_str = datetime.now(tz=timezone.utc).strftime("%Y-%m-%d %H:%M:%S UTC")
    lines.append("=" * 72)
    lines.append(f"  BINANCE BOT STATUS  ({now_str})")
    lines.append("=" * 72)
    lines.append("")

    lines.append("WALLET")
    lines.append(f"  Spot USDT free:    ${wallet.spot_usdt_free:.4f}")
    lines.append(f"  Futures USDT free: ${wallet.futures_usdt_free:.4f}")
    nonzero = [p for p in wallet.positions if p["asset"] != "USDT" and p["value_usdt"] >= 0.1]
    if nonzero:
        lines.append("  Positions:")
        for p in sorted(nonzero, key=lambda x: x["value_usdt"], reverse=True):
            lines.append(f"    {p['asset']:8s}  {p['qty']:>18.8f}  ≈ ${p['value_usdt']:.4f}")
    dust_count = sum(1 for p in wallet.positions
                      if p["asset"] != "USDT" and 0 < p["value_usdt"] < 0.1)
    if dust_count:
        lines.append(f"  +{dust_count} dust positions (<$0.10 each)")
    lines.append(f"  TOTAL:             ${wallet.total_value_usdt:.4f}")
    lines.append("")

    lines.append("BOTS")
    for bot in bots:
        timer = bot.timer
        if timer is None:
            status_chip = "[NO TIMER]"
        elif timer.active and timer.enabled:
            status_chip = "[RUNNING]"
        elif timer.enabled and not timer.active:
            status_chip = "[ENABLED, idle]"
        elif timer.active and not timer.enabled:
            status_chip = "[RUNNING, not persistent]"
        else:
            status_chip = "[STOPPED]"
        lines.append(f"  {bot.name:18s} {status_chip}")
        if timer is not None:
            next_t = _format_us_timestamp(timer.next_trigger)
            last_t = _format_us_timestamp(timer.last_trigger)
            lines.append(f"    timer: next={next_t}, last={last_t}")
        if bot.state_summary:
            lines.append(f"    state: {bot.state_summary}")
        if bot.last_audit is not None:
            mode = bot.last_audit.get("mode") or bot.last_audit.get("listing_type") or ""
            ts = bot.last_audit.get("timestamp") or bot.last_audit.get("detected_at_ms") or ""
            if isinstance(ts, int):
                ts = datetime.fromtimestamp(ts / 1000, tz=timezone.utc).isoformat()
            lines.append(f"    last cycle: {ts} (mode={mode})")
        if bot.trades_lifetime:
            lines.append(f"    trades: {bot.trades_lifetime} lifetime, {bot.trades_24h} in 24h")
            lines.append(f"    P&L:    ${bot.pnl_lifetime_usdt:+.4f} lifetime, "
                          f"${bot.pnl_24h_usdt:+.4f} 24h")
        lines.append("")
    return "\n".join(lines)


def _render_json(wallet: WalletSnapshot, bots: list[BotSnapshot]) -> str:
    return json.dumps({
        "timestamp": datetime.now(tz=timezone.utc).isoformat(),
        "wallet": {
            "spot_usdt_free": wallet.spot_usdt_free,
            "futures_usdt_free": wallet.futures_usdt_free,
            "positions": wallet.positions,
            "total_value_usdt": wallet.total_value_usdt,
        },
        "bots": [
            {
                "name": b.name,
                "timer": {
                    "unit": b.timer.unit, "active": b.timer.active,
                    "enabled": b.timer.enabled,
                    "next_trigger": b.timer.next_trigger,
                    "last_trigger": b.timer.last_trigger,
                } if b.timer else None,
                "state_summary": b.state_summary,
                "last_audit": b.last_audit,
                "trades_lifetime": b.trades_lifetime,
                "trades_24h": b.trades_24h,
                "pnl_lifetime_usdt": b.pnl_lifetime_usdt,
                "pnl_24h_usdt": b.pnl_24h_usdt,
            }
            for b in bots
        ],
    }, indent=2, sort_keys=True, default=str)


async def collect_snapshot(data_dir: Path = DEFAULT_DATA_DIR) -> tuple[WalletSnapshot, list[BotSnapshot]]:
    wallet = await _fetch_wallet()
    bots = [_bot_snapshot(name, data_dir=data_dir, wallet=wallet) for name in KNOWN_BOTS]
    return wallet, bots


async def send_telegram_status(message: str) -> bool:
    import html as html_lib

    from src.telegram_alerts import TelegramAlerter
    load_dotenv(dotenv_path=os.environ.get(
        "DOTENV_PATH", os.path.expanduser("~/.local/share/binance-bot/.env")))
    token = os.getenv("TELEGRAM_BOT_TOKEN", "")
    chat = os.getenv("TELEGRAM_CHAT_ID", "")
    alerter = TelegramAlerter(token, chat)
    # Telegram parse_mode=HTML requires & < > escaping; wrap in <pre> for monospace.
    return await alerter.send(f"<pre>{html_lib.escape(message)}</pre>")


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Unified status snapshot across every deployed bot."
    )
    parser.add_argument("--data-dir", default=str(DEFAULT_DATA_DIR))
    parser.add_argument("--json", action="store_true", help="emit JSON instead of plaintext")
    parser.add_argument("--telegram", action="store_true",
                        help="send the rendered snapshot to the configured Telegram chat")
    args = parser.parse_args(argv)

    wallet, bots = asyncio.run(collect_snapshot(data_dir=Path(args.data_dir)))
    rendered = _render_json(wallet, bots) if args.json else _render_plaintext(wallet, bots)
    print(rendered)

    if args.telegram:
        sent = asyncio.run(send_telegram_status(rendered if not args.json else "Bot status JSON attached"))
        if not sent:
            print("(Telegram delivery failed or alerts disabled)", flush=True)
            return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(run())
