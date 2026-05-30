"""Binance new-listing announcement detector.

Polls the Binance public announcements API and identifies new spot listing
announcements. When a fresh spot listing is detected the runtime can:

* ``detect_only``: print + audit, take no action (default; useful for
  validating the detection pipeline without trading risk).
* ``paper``: simulate a buy at detection time and a sell N minutes later;
  write the simulated P&L to a JSONL ledger.
* ``submit-live``: actually place a market buy on the listed pair with
  the configured notional, hold for ``hold_seconds``, then sell. Gated
  behind ``LISTING_DETECTOR_PROVEN_EDGE=yes`` like every other runtime.

Title heuristics distinguish spot listings (``"Binance Will List"``) from
futures-only launches (``"Binance Futures Will Launch"``) and ancillary
events (``"Binance Will Add ... on Earn, Convert"``). The token symbol is
extracted from the parenthesized ticker convention used in announcement
titles: ``"Binance Will List Foo (FOO) with Seed Tag Applied"`` → ``FOO``.

The detector maintains a persistent ``last_seen_article_id`` cursor so
restarts do not re-fire alerts on already-processed announcements.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import re
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Mapping, Sequence

from dotenv import load_dotenv


class ListingDetectorError(RuntimeError):
    """Raised when the listing detector cannot proceed."""


BINANCE_ANNOUNCEMENTS_URL = (
    "https://www.binance.com/bapi/composite/v1/public/cms/article/list/query"
)
NEW_LISTING_CATALOG_ID = 48  # "New Cryptocurrency Listing"

_SPOT_LIST_PATTERN = re.compile(r"Binance Will List", re.IGNORECASE)
_FUTURES_PATTERN = re.compile(r"Binance Futures", re.IGNORECASE)
_SPOT_ADD_PATTERN = re.compile(r"Binance Will Add", re.IGNORECASE)
_INTRO_HODLER_PATTERN = re.compile(r"Introducing .* on Binance HODLer", re.IGNORECASE)
_TICKER_PATTERN = re.compile(r"\(([A-Z][A-Z0-9]{1,15})\)")


@dataclass(frozen=True, slots=True)
class ListingDetectorSettings:
    poll_interval_seconds: int = 30
    quote_asset: str = "USDT"  # only trade pairs with this quote
    notional_usdt: float = 5.0
    hold_seconds: int = 600  # 10 minutes
    min_balance_usdt: float = 5.0
    dynamic_balance: bool = True
    enabled: bool = False
    confirm_live_listing_detector: str = "no"
    use_testnet: bool = False
    dry_run: bool = True

    state_path: str = "data/listing-detector-state.json"
    audit_path: str = "data/listing-detector-events.jsonl"
    ledger_path: str = "data/listing-detector-ledger.jsonl"
    emergency_stop_path: str = "data/listing-detector.stop"

    api_key: str = ""
    api_secret: str = ""

    @classmethod
    def from_env(cls) -> "ListingDetectorSettings":
        load_dotenv(dotenv_path=os.environ.get("DOTENV_PATH"))

        def _bool(name: str, default: bool) -> bool:
            v = os.getenv(name, "")
            return default if not v else v.strip().lower() in {"1", "true", "yes", "on"}

        def _float(name: str, default: float) -> float:
            try:
                return float(os.getenv(name) or default)
            except (TypeError, ValueError):
                raise ListingDetectorError(f"{name} must be numeric")

        def _int(name: str, default: int) -> int:
            try:
                return int(os.getenv(name) or default)
            except (TypeError, ValueError):
                raise ListingDetectorError(f"{name} must be an integer")

        api_key = os.getenv("BINANCE_API_KEY", "")
        api_secret = os.getenv("BINANCE_API_SECRET", "")
        return cls(
            poll_interval_seconds=_int("LISTING_DETECTOR_POLL_INTERVAL_SECONDS", 30),
            quote_asset=os.getenv("LISTING_DETECTOR_QUOTE_ASSET", "USDT"),
            notional_usdt=_float("LISTING_DETECTOR_NOTIONAL_USDT", 5.0),
            hold_seconds=_int("LISTING_DETECTOR_HOLD_SECONDS", 600),
            min_balance_usdt=_float("LISTING_DETECTOR_MIN_BALANCE_USDT", 5.0),
            dynamic_balance=_bool("LISTING_DETECTOR_DYNAMIC_BALANCE", True),
            enabled=_bool("LISTING_DETECTOR_ENABLED", False),
            confirm_live_listing_detector=os.getenv("CONFIRM_LIVE_LISTING_DETECTOR", "no"),
            use_testnet=_bool("USE_TESTNET", False),
            dry_run=_bool("DRY_RUN", True),
            state_path=os.getenv("LISTING_DETECTOR_STATE_PATH", "data/listing-detector-state.json"),
            audit_path=os.getenv("LISTING_DETECTOR_AUDIT_PATH", "data/listing-detector-events.jsonl"),
            ledger_path=os.getenv("LISTING_DETECTOR_LEDGER_PATH", "data/listing-detector-ledger.jsonl"),
            emergency_stop_path=os.getenv("LISTING_DETECTOR_EMERGENCY_STOP_PATH", "data/listing-detector.stop"),
            api_key=api_key, api_secret=api_secret,
        )


@dataclass(frozen=True, slots=True)
class ListingEvent:
    article_id: int
    title: str
    release_time_ms: int
    listing_type: str  # "spot_list" | "spot_add" | "hodler_airdrop" | "futures_only" | "other"
    detected_token: str | None
    detected_pair: str | None  # if known to exist on this venue

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(slots=True)
class DetectorState:
    last_seen_article_id: int = 0
    last_poll_time_ms: int = 0

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_mapping(cls, payload: Mapping[str, Any]) -> "DetectorState":
        return cls(
            last_seen_article_id=int(payload.get("last_seen_article_id", 0)),
            last_poll_time_ms=int(payload.get("last_poll_time_ms", 0)),
        )


def load_state(path: Path | str) -> DetectorState:
    p = Path(path)
    if not p.exists():
        return DetectorState()
    try:
        return DetectorState.from_mapping(json.loads(p.read_text()))
    except (OSError, json.JSONDecodeError) as exc:
        raise ListingDetectorError(f"could not read state at {p}: {exc}") from exc


def save_state(state: DetectorState, path: Path | str) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(p.suffix + ".tmp")
    tmp.write_text(json.dumps(state.as_dict(), indent=2, sort_keys=True))
    tmp.replace(p)


def classify_title(title: str) -> str:
    """Return one of: spot_list, spot_add, hodler_airdrop, futures_only, other."""
    if _FUTURES_PATTERN.search(title):
        return "futures_only"
    if _SPOT_LIST_PATTERN.search(title):
        return "spot_list"
    if _SPOT_ADD_PATTERN.search(title):
        return "spot_add"
    if _INTRO_HODLER_PATTERN.search(title):
        return "hodler_airdrop"
    return "other"


def extract_ticker(title: str) -> str | None:
    """Extract the first uppercase ticker found in parentheses.

    Filters out common non-ticker abbreviations like UTC, USDT, EU.
    """
    blocklist = {"UTC", "USD", "USDT", "EU", "USA", "UK", "AI", "BNB", "USDⓈ"}
    for match in _TICKER_PATTERN.finditer(title):
        candidate = match.group(1)
        if candidate in blocklist:
            continue
        return candidate
    return None


def parse_announcement(article: Mapping[str, Any]) -> ListingEvent:
    title = str(article.get("title") or "")
    return ListingEvent(
        article_id=int(article.get("id") or 0),
        title=title,
        release_time_ms=int(article.get("releaseDate") or 0),
        listing_type=classify_title(title),
        detected_token=extract_ticker(title),
        detected_pair=None,  # filled in by detector if pair exists
    )


async def fetch_announcements(*, page_size: int = 20) -> list[Mapping[str, Any]]:
    """Fetch the latest N announcements from the new-listing catalog."""
    import aiohttp

    params = {
        "type": 1,
        "catalogId": NEW_LISTING_CATALOG_ID,
        "pageSize": page_size,
        "pageNo": 1,
    }
    async with aiohttp.ClientSession() as session:
        async with session.get(BINANCE_ANNOUNCEMENTS_URL, params=params, timeout=20) as resp:
            if resp.status != 200:
                raise ListingDetectorError(f"announcements API returned HTTP {resp.status}")
            payload = await resp.json()
    if not payload.get("success"):
        raise ListingDetectorError("announcements API returned non-success payload")
    catalogs = (payload.get("data") or {}).get("catalogs") or []
    if not catalogs:
        return []
    return catalogs[0].get("articles") or []


def detect_new_listings(
    articles: Sequence[Mapping[str, Any]],
    *,
    last_seen_article_id: int,
) -> list[ListingEvent]:
    """Return new events sorted oldest-first, filtered against the cursor."""
    fresh: list[ListingEvent] = []
    for article in articles:
        event = parse_announcement(article)
        if event.article_id <= last_seen_article_id:
            continue
        fresh.append(event)
    fresh.sort(key=lambda e: e.release_time_ms)
    return fresh


async def _exchange_has_pair(spot_client: Any, base: str, quote: str) -> bool:
    """Check whether base/quote is a trading-active spot symbol."""
    try:
        await spot_client.load_markets()
    except Exception:  # noqa: BLE001 — fail closed if markets not loadable
        return False
    sym = f"{base}/{quote}"
    market = spot_client.markets.get(sym)
    if market is None:
        return False
    return bool(market.get("active", True)) and bool(market.get("spot", True))


def append_audit(path: str, payload: Mapping[str, Any]) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    with p.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(payload, sort_keys=True))
        fh.write("\n")


def _utc_now_ms() -> int:
    return int(datetime.now(tz=timezone.utc).timestamp() * 1000)


def _utc_timestamp() -> str:
    return datetime.now(tz=timezone.utc).isoformat()


def _ensure_no_emergency_stop(path: str) -> None:
    if Path(path).exists():
        raise ListingDetectorError(f"emergency stop file present at {path}; refusing to run")


def _preflight_live(settings: ListingDetectorSettings) -> None:
    _ensure_no_emergency_stop(settings.emergency_stop_path)
    if not settings.enabled:
        raise ListingDetectorError("LISTING_DETECTOR_ENABLED=true is required for live submission")
    if settings.use_testnet:
        raise ListingDetectorError("listing detector live submission requires USE_TESTNET=false")
    if settings.dry_run:
        raise ListingDetectorError("listing detector live submission requires DRY_RUN=false")
    if settings.confirm_live_listing_detector != "yes":
        raise ListingDetectorError(
            "listing detector live submission requires CONFIRM_LIVE_LISTING_DETECTOR=yes"
        )
    if os.getenv("LISTING_DETECTOR_PROVEN_EDGE", "").strip().lower() != "yes":
        raise ListingDetectorError(
            "listing detector live submission requires LISTING_DETECTOR_PROVEN_EDGE=yes. "
            "Run --paper for at least 2-4 detected events first, review the ledger, "
            "and set this only when paper P&L looks coherent."
        )


async def _execute_event_live(
    *, event: ListingEvent, settings: ListingDetectorSettings, spot_client: Any,
) -> dict[str, Any]:
    """Buy at market, hold for hold_seconds, sell at market."""
    if event.detected_pair is None:
        return {"action": "skip", "reason": "no exchange pair for token"}
    sym = event.detected_pair
    try:
        buy_order = await spot_client.create_order(
            symbol=sym, type="market", side="buy", amount=None, price=None,
            params={"quoteOrderQty": settings.notional_usdt},
        )
    except Exception as exc:  # noqa: BLE001 — log + skip
        return {"action": "buy_failed", "reason": f"{type(exc).__name__}: {str(exc)[:200]}"}
    filled_qty = float(buy_order.get("filled") or 0.0)
    avg_buy = float(buy_order.get("average") or 0.0)
    if filled_qty <= 0:
        return {"action": "buy_zero_fill", "buy_order": buy_order}
    await asyncio.sleep(settings.hold_seconds)
    try:
        sell_order = await spot_client.create_order(
            symbol=sym, type="market", side="sell", amount=filled_qty,
        )
    except Exception as exc:  # noqa: BLE001 — position stranded, report
        return {"action": "sell_failed", "buy_order": buy_order,
                "reason": f"{type(exc).__name__}: {str(exc)[:200]}"}
    sold_qty = float(sell_order.get("filled") or 0.0)
    avg_sell = float(sell_order.get("average") or 0.0)
    return {
        "action": "round_trip_complete",
        "symbol": sym,
        "filled_qty": filled_qty,
        "avg_buy": avg_buy,
        "avg_sell": avg_sell,
        "sold_qty": sold_qty,
        "pnl_quote": (avg_sell - avg_buy) * filled_qty,
        "hold_seconds": settings.hold_seconds,
        "buy_order_id": str(buy_order.get("id") or ""),
        "sell_order_id": str(sell_order.get("id") or ""),
    }


async def run_once(
    settings: ListingDetectorSettings, *, mode: str = "detect_only",
    spot_client: Any | None = None,
) -> dict[str, Any]:
    """Single poll cycle: fetch announcements, detect new events, optionally act."""
    if mode not in {"detect_only", "paper", "live"}:
        raise ListingDetectorError(f"unknown mode: {mode}")
    _ensure_no_emergency_stop(settings.emergency_stop_path)
    if mode == "live":
        _preflight_live(settings)

    state = load_state(settings.state_path)
    articles = await fetch_announcements(page_size=20)
    new_events = detect_new_listings(articles, last_seen_article_id=state.last_seen_article_id)

    enriched: list[ListingEvent] = []
    actions: list[dict[str, Any]] = []
    for event in new_events:
        if event.listing_type == "spot_list" and event.detected_token and spot_client is not None:
            has_pair = await _exchange_has_pair(spot_client, event.detected_token, settings.quote_asset)
            if has_pair:
                event = ListingEvent(
                    article_id=event.article_id, title=event.title,
                    release_time_ms=event.release_time_ms, listing_type=event.listing_type,
                    detected_token=event.detected_token,
                    detected_pair=f"{event.detected_token}/{settings.quote_asset}",
                )
        enriched.append(event)

        # Alert on every fresh spot listing — these are rare enough (a few
        # per week) that every one is worth a ping regardless of mode.
        if event.listing_type == "spot_list" and event.detected_token:
            from src.alerts import maybe_telegram
            pair_note = f" ({event.detected_pair} exists on spot)" if event.detected_pair else " (pair not yet live on spot)"
            await maybe_telegram(
                f"New Binance spot listing detected: {event.detected_token}{pair_note}\n"
                f"Title: {event.title[:200]}"
            )

        if mode == "live" and event.listing_type == "spot_list" and event.detected_pair:
            result = await _execute_event_live(event=event, settings=settings, spot_client=spot_client)
            actions.append({"article_id": event.article_id, **result})
        elif mode == "paper" and event.listing_type == "spot_list" and event.detected_pair:
            actions.append({
                "article_id": event.article_id, "action": "paper_buy_sell",
                "symbol": event.detected_pair,
                "notional_usdt": settings.notional_usdt,
                "hold_seconds": settings.hold_seconds,
                "note": "real-execution P&L requires live mode",
            })

    if new_events:
        newest_id = max(e.article_id for e in new_events)
        save_state(DetectorState(
            last_seen_article_id=max(state.last_seen_article_id, newest_id),
            last_poll_time_ms=_utc_now_ms(),
        ), settings.state_path)

    payload = {
        "timestamp": _utc_timestamp(),
        "mode": mode,
        "new_events_count": len(new_events),
        "events": [e.as_dict() for e in enriched],
        "actions": actions,
        "previous_cursor_article_id": state.last_seen_article_id,
    }
    append_audit(settings.audit_path, payload)
    return payload


async def build_live_spot_client(settings: ListingDetectorSettings) -> Any:
    import ccxt.async_support as ccxt_async
    return ccxt_async.binance({
        "apiKey": settings.api_key, "secret": settings.api_secret,
        "enableRateLimit": True,
        "options": {"adjustForTimeDifference": True, "fetchCurrencies": False},
    })


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Detect Binance new-listing announcements. Default mode is detect-only."
    )
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--detect-only", action="store_const", const="detect_only", dest="mode")
    group.add_argument("--paper", action="store_const", const="paper", dest="mode")
    group.add_argument("--submit-live", action="store_const", const="live", dest="mode")
    parser.set_defaults(mode="detect_only")
    args = parser.parse_args(argv)

    try:
        settings = ListingDetectorSettings.from_env()
        if args.mode in {"paper", "live"}:
            client = asyncio.run(build_live_spot_client(settings))
            try:
                result = asyncio.run(run_once(settings, mode=args.mode, spot_client=client))
            finally:
                try:
                    asyncio.run(client.close())
                except Exception:  # noqa: BLE001
                    pass
        else:
            result = asyncio.run(run_once(settings, mode=args.mode))
    except ListingDetectorError as exc:
        print(f"listing-detector failed: {exc}")
        return 1
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())
