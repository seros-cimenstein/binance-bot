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
    # Hold mode shape:
    #   "flat"     — buy, sleep ``hold_seconds``, market sell. Default. Best
    #                for the first-hour pop on freshly-listed tokens.
    #   "trailing" — buy, sleep ``hold_seconds`` (initial floor), then poll
    #                ``poll_interval_seconds`` for up to ``max_hold_seconds``;
    #                sell once price drops ``trailing_stop_pct`` from the
    #                running peak. Captures multi-day pumps without giving
    #                back all the gains.
    hold_mode: str = "flat"
    hold_seconds: int = 600  # 10 minutes — also the trailing initial floor
    trailing_stop_pct: float = 15.0
    max_hold_seconds: int = 86_400  # 24 hours, trailing-mode hard cap
    trailing_poll_interval_seconds: int = 60
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
    # Shared watchlist consumed by listing_sniper. Defaults to the same path
    # the sniper reads from. Per-sniper overrides go via env on the sniper.
    watchlist_path: str = "data/listing-watchlist.json"

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
            hold_mode=os.getenv("LISTING_DETECTOR_HOLD_MODE", "flat"),
            hold_seconds=_int("LISTING_DETECTOR_HOLD_SECONDS", 600),
            trailing_stop_pct=_float("LISTING_DETECTOR_TRAILING_STOP_PCT", 15.0),
            max_hold_seconds=_int("LISTING_DETECTOR_MAX_HOLD_SECONDS", 86_400),
            trailing_poll_interval_seconds=_int(
                "LISTING_DETECTOR_TRAILING_POLL_INTERVAL_SECONDS", 60,
            ),
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
            watchlist_path=os.getenv("LISTING_WATCHLIST_PATH", "data/listing-watchlist.json"),
            api_key=api_key, api_secret=api_secret,
        )


@dataclass(frozen=True, slots=True)
class ListingEvent:
    article_id: int
    title: str
    release_time_ms: int
    listing_type: str  # "spot_list" | "spot_add" | "hodler_airdrop" | "futures_only" | "other"
    # All tickers found in the title's parenthesized form, in announcement
    # order. ``detected_pairs`` is filled in by the detector for pairs that
    # actually exist on this venue (used to short-circuit dead listings).
    detected_tokens: tuple[str, ...] = ()
    detected_pairs: tuple[str, ...] = ()

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)

    @property
    def detected_token(self) -> str | None:
        """First detected ticker, for backward-compat with single-token consumers."""
        return self.detected_tokens[0] if self.detected_tokens else None

    @property
    def detected_pair(self) -> str | None:
        """First detected exchange pair, for backward-compat."""
        return self.detected_pairs[0] if self.detected_pairs else None


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


_TICKER_BLOCKLIST = frozenset({
    "UTC", "USD", "USDT", "EU", "USA", "UK", "AI", "BNB", "USDⓈ",
})


def extract_tickers(title: str) -> tuple[str, ...]:
    """Extract every uppercase ticker in parentheses, deduped in announcement order.

    Multi-token announcements (e.g. ``"List X (XYZ) and Y (OPG) with Seed Tag"``)
    return both. Common non-ticker abbreviations (UTC, USDT, etc.) are filtered.
    """
    out: list[str] = []
    seen: set[str] = set()
    for match in _TICKER_PATTERN.finditer(title):
        candidate = match.group(1)
        if candidate in _TICKER_BLOCKLIST or candidate in seen:
            continue
        seen.add(candidate)
        out.append(candidate)
    return tuple(out)


def extract_ticker(title: str) -> str | None:
    """Back-compat wrapper: returns the first ticker only, or None."""
    tokens = extract_tickers(title)
    return tokens[0] if tokens else None


def parse_announcement(article: Mapping[str, Any]) -> ListingEvent:
    title = str(article.get("title") or "")
    return ListingEvent(
        article_id=int(article.get("id") or 0),
        title=title,
        release_time_ms=int(article.get("releaseDate") or 0),
        listing_type=classify_title(title),
        detected_tokens=extract_tickers(title),
        detected_pairs=(),  # filled in by run_once if pairs exist on exchange
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


async def _ticker_last_price(spot_client: Any, symbol: str) -> float | None:
    try:
        ticker = await spot_client.fetch_ticker(symbol)
    except Exception:  # noqa: BLE001 — caller treats None as transient failure
        return None
    if not isinstance(ticker, Mapping):
        return None
    raw = ticker.get("last") or ticker.get("close")
    try:
        return float(raw) if raw is not None else None
    except (TypeError, ValueError):
        return None


async def _hold_then_sell_price(
    *, settings: ListingDetectorSettings, spot_client: Any, sym: str, avg_buy: float,
) -> tuple[float, str]:
    """Decide when to fire the market sell. Returns ``(final_hold_seconds, exit_reason)``."""
    if settings.hold_mode == "flat":
        await asyncio.sleep(settings.hold_seconds)
        return float(settings.hold_seconds), "flat_hold_elapsed"
    if settings.hold_mode != "trailing":
        raise ListingDetectorError(f"unknown hold_mode: {settings.hold_mode}")

    # Trailing-stop hold: sit through the initial floor, then poll the ticker
    # and exit if the peak retraces past ``trailing_stop_pct`` or we hit the
    # hard cap.
    await asyncio.sleep(settings.hold_seconds)
    peak = max(avg_buy, await _ticker_last_price(spot_client, sym) or avg_buy)
    elapsed = float(settings.hold_seconds)
    stop_factor = 1.0 - settings.trailing_stop_pct / 100.0
    while elapsed < settings.max_hold_seconds:
        await asyncio.sleep(settings.trailing_poll_interval_seconds)
        elapsed += settings.trailing_poll_interval_seconds
        last = await _ticker_last_price(spot_client, sym)
        if last is None:
            continue
        if last > peak:
            peak = last
            continue
        if last <= peak * stop_factor:
            return elapsed, (
                f"trailing_stop_hit: last={last:.6f} <= peak*{stop_factor:.4f}="
                f"{peak * stop_factor:.6f}"
            )
    return elapsed, "max_hold_seconds_reached"


async def _execute_event_live(
    *, event: ListingEvent, settings: ListingDetectorSettings,
    spot_client: Any, pair: str,
) -> dict[str, Any]:
    """Buy ``pair`` at market, hold per ``hold_mode``, sell at market."""
    sym = pair
    try:
        buy_order = await spot_client.create_order(
            symbol=sym, type="market", side="buy", amount=None, price=None,
            params={"quoteOrderQty": settings.notional_usdt},
        )
    except Exception as exc:  # noqa: BLE001 — log + skip
        return {"action": "buy_failed", "symbol": sym,
                "reason": f"{type(exc).__name__}: {str(exc)[:200]}"}
    filled_qty = float(buy_order.get("filled") or 0.0)
    avg_buy = float(buy_order.get("average") or 0.0)
    if filled_qty <= 0:
        return {"action": "buy_zero_fill", "symbol": sym, "buy_order": buy_order}

    final_hold_seconds, exit_reason = await _hold_then_sell_price(
        settings=settings, spot_client=spot_client, sym=sym, avg_buy=avg_buy,
    )

    try:
        sell_order = await spot_client.create_order(
            symbol=sym, type="market", side="sell", amount=filled_qty,
        )
    except Exception as exc:  # noqa: BLE001 — position stranded, report
        return {"action": "sell_failed", "symbol": sym, "buy_order": buy_order,
                "reason": f"{type(exc).__name__}: {str(exc)[:200]}",
                "hold_seconds": final_hold_seconds, "exit_reason": exit_reason}
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
        "hold_mode": settings.hold_mode,
        "hold_seconds": final_hold_seconds,
        "exit_reason": exit_reason,
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

    # Append fresh spot_list announcements to the shared watchlist so the
    # listing_sniper can pre-arm the corresponding {token}{quote_asset} pair
    # — that way the sniper fires the moment the symbol transitions to
    # TRADING on exchangeInfo, even if it had been bootstrapped as "known."
    if new_events:
        from src.listing_watchlist import (
            WatchlistError, build_watch_entry, load_watchlist, save_watchlist,
        )
        try:
            wl = load_watchlist(settings.watchlist_path)
            watchlist_added: list[str] = []
            for event in new_events:
                if event.listing_type != "spot_list" or not event.detected_tokens:
                    continue
                for token in event.detected_tokens:
                    if wl.add(build_watch_entry(
                        token=token, article_id=event.article_id,
                        announced_at_ms=event.release_time_ms or _utc_now_ms(),
                    )):
                        watchlist_added.append(token)
            if watchlist_added:
                save_watchlist(wl, settings.watchlist_path)
                actions.append({
                    "action": "watchlist_armed", "tokens": watchlist_added,
                })
        except WatchlistError as exc:
            actions.append({"action": "watchlist_error",
                             "reason": f"{type(exc).__name__}: {str(exc)[:200]}"})

    for event in new_events:
        # Resolve all tradeable pairs for this announcement's tokens.
        if (event.listing_type == "spot_list" and event.detected_tokens
                and spot_client is not None):
            existing_pairs: list[str] = []
            for token in event.detected_tokens:
                if await _exchange_has_pair(spot_client, token, settings.quote_asset):
                    existing_pairs.append(f"{token}/{settings.quote_asset}")
            if existing_pairs:
                event = ListingEvent(
                    article_id=event.article_id, title=event.title,
                    release_time_ms=event.release_time_ms,
                    listing_type=event.listing_type,
                    detected_tokens=event.detected_tokens,
                    detected_pairs=tuple(existing_pairs),
                )
        enriched.append(event)

        # Alert on every fresh spot listing — these are rare enough (a few
        # per week) that every one is worth a ping regardless of mode.
        if event.listing_type == "spot_list" and event.detected_tokens:
            from src.alerts import maybe_telegram
            tokens_str = ", ".join(event.detected_tokens)
            if event.detected_pairs:
                pair_note = f" ({', '.join(event.detected_pairs)} live on spot)"
            else:
                pair_note = " (pair(s) not yet live on spot)"
            await maybe_telegram(
                f"New Binance spot listing detected: {tokens_str}{pair_note}\n"
                f"Title: {event.title[:200]}"
            )

        if mode == "live" and event.listing_type == "spot_list" and event.detected_pairs:
            for pair in event.detected_pairs:
                result = await _execute_event_live(
                    event=event, settings=settings,
                    spot_client=spot_client, pair=pair,
                )
                actions.append({"article_id": event.article_id, "pair": pair, **result})
        elif mode == "paper" and event.listing_type == "spot_list" and event.detected_pairs:
            for pair in event.detected_pairs:
                actions.append({
                    "article_id": event.article_id, "pair": pair,
                    "action": "paper_buy_sell",
                    "symbol": pair,
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
