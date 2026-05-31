"""Listing sniper bot.

Polls Binance ``GET /api/v3/exchangeInfo`` on a tight cycle and detects
the exact moment a new symbol enters ``status=TRADING``. When that
happens — and the pair matches the configured quote asset — fires a
market BUY with the configured notional, then holds with either a flat
timer or a trailing-stop until exit.

This is a complementary bot to ``src.listing_detector``:

  * **detector**  — slow announcement-feed polling (every ~2 min) for
    human-readable Telegram alerts and audit trails; no trading by default.
  * **sniper**    — fast exchangeInfo polling (every ~5 s) to detect the
    trading-start signal itself, and actually fire orders. Designed to run
    as a long-lived daemon under Restart=always.

Both consume the same per-bot safety pattern in this repo
(plan_only / paper / live, PROVEN_EDGE gate, emergency stop, daily caps).
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable, Mapping

import aiohttp
from dotenv import load_dotenv


EXCHANGE_INFO_URL = "https://api.binance.com/api/v3/exchangeInfo"
PUBLIC_TICKER_URL = "https://api.binance.com/api/v3/ticker/price"


class ListingSniperError(RuntimeError):
    """Raised when the listing sniper cannot proceed."""


# ---------------------------------------------------------------------------
# Settings
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class ListingSniperSettings:
    api_key: str
    api_secret: str
    use_testnet: bool = False
    dry_run: bool = True
    enabled: bool = False
    confirm_live_listing_sniper: str = "no"

    quote_asset: str = "USDT"
    poll_interval_seconds: float = 5.0
    notional_usdt: float = 5.0

    # Hold shape:
    #   "flat"     — sell at the end of hold_seconds, no thinking.
    #   "trailing" — fixed retrace from peak triggers exit.
    #   "momentum" — combination of velocity sign, lower-high pattern, and
    #                short/long MA cross. Smarter exit, more false positives.
    hold_mode: str = "trailing"
    hold_seconds: int = 60          # flat hold OR initial-floor for trailing/momentum
    trailing_stop_pct: float = 15.0
    max_hold_seconds: int = 14_400  # 4 hours
    trailing_poll_interval_seconds: int = 10

    # Universal hard stop loss — active from tick 1 in ALL hold modes,
    # before the noise-filter floor (hold_seconds) finishes. Catches rug
    # pulls and immediate-dump scenarios where the configured smart-exit
    # logic would be sleeping. Set to 0 (or positive) to disable.
    hard_stop_loss_pct: float = -25.0

    # Momentum knobs (used when hold_mode="momentum"):
    momentum_negative_polls_to_exit: int = 2  # consecutive negative-velocity polls
    momentum_no_new_high_polls_to_exit: int = 5  # polls without a new peak
    momentum_ma_short_samples: int = 3
    momentum_ma_long_samples: int = 6
    momentum_ma_cross_min_pullback_pct: float = 5.0  # pullback from peak required

    daily_spend_cap_usdt: float = 50.0
    min_balance_usdt: float = 10.0
    dynamic_balance: bool = True

    state_path: str = "data/listing-sniper-state.json"
    audit_path: str = "data/listing-sniper-events.jsonl"
    ledger_path: str = "data/listing-sniper-ledger.jsonl"
    emergency_stop_path: str = "data/listing-sniper.stop"

    def validate(self) -> None:
        if self.poll_interval_seconds < 1.0:
            raise ListingSniperError("poll_interval_seconds must be >= 1.0")
        if self.notional_usdt <= 0:
            raise ListingSniperError("notional_usdt must be positive")
        if self.hold_mode not in {"flat", "trailing", "momentum"}:
            raise ListingSniperError("hold_mode must be 'flat', 'trailing', or 'momentum'")
        if self.momentum_ma_short_samples >= self.momentum_ma_long_samples:
            raise ListingSniperError(
                "momentum_ma_short_samples must be strictly less than "
                "momentum_ma_long_samples"
            )
        if self.daily_spend_cap_usdt <= 0:
            raise ListingSniperError("daily_spend_cap_usdt must be positive")
        if self.min_balance_usdt < 0:
            raise ListingSniperError("min_balance_usdt must be non-negative")
        for path_name in ("state_path", "audit_path", "ledger_path", "emergency_stop_path"):
            if not getattr(self, path_name).strip():
                raise ListingSniperError(f"{path_name} must not be blank")

    @classmethod
    def from_env(cls) -> "ListingSniperSettings":
        load_dotenv(dotenv_path=os.environ.get("DOTENV_PATH"))
        api_key = os.getenv("BINANCE_API_KEY", "")
        api_secret = os.getenv("BINANCE_API_SECRET", "")
        if not api_key or not api_secret:
            raise ListingSniperError("BINANCE_API_KEY and BINANCE_API_SECRET must be set")

        def _bool(name: str, default: bool) -> bool:
            v = os.getenv(name, "")
            return default if not v else v.strip().lower() in {"1", "true", "yes", "on"}

        def _float(name: str, default: float) -> float:
            try:
                return float(os.getenv(name) or default)
            except (TypeError, ValueError):
                raise ListingSniperError(f"{name} must be numeric")

        def _int(name: str, default: int) -> int:
            try:
                return int(os.getenv(name) or default)
            except (TypeError, ValueError):
                raise ListingSniperError(f"{name} must be an integer")

        defaults = cls(api_key="x", api_secret="x")
        settings = cls(
            api_key=api_key, api_secret=api_secret,
            use_testnet=_bool("USE_TESTNET", False),
            dry_run=_bool("DRY_RUN", True),
            enabled=_bool("LISTING_SNIPER_ENABLED", False),
            confirm_live_listing_sniper=os.getenv("CONFIRM_LIVE_LISTING_SNIPER", "no"),
            quote_asset=os.getenv("LISTING_SNIPER_QUOTE_ASSET", defaults.quote_asset),
            poll_interval_seconds=_float(
                "LISTING_SNIPER_POLL_INTERVAL_SECONDS", defaults.poll_interval_seconds,
            ),
            notional_usdt=_float("LISTING_SNIPER_NOTIONAL_USDT", defaults.notional_usdt),
            hold_mode=os.getenv("LISTING_SNIPER_HOLD_MODE", defaults.hold_mode),
            hold_seconds=_int("LISTING_SNIPER_HOLD_SECONDS", defaults.hold_seconds),
            trailing_stop_pct=_float(
                "LISTING_SNIPER_TRAILING_STOP_PCT", defaults.trailing_stop_pct,
            ),
            max_hold_seconds=_int(
                "LISTING_SNIPER_MAX_HOLD_SECONDS", defaults.max_hold_seconds,
            ),
            trailing_poll_interval_seconds=_int(
                "LISTING_SNIPER_TRAILING_POLL_INTERVAL_SECONDS",
                defaults.trailing_poll_interval_seconds,
            ),
            momentum_negative_polls_to_exit=_int(
                "LISTING_SNIPER_MOMENTUM_NEGATIVE_POLLS_TO_EXIT",
                defaults.momentum_negative_polls_to_exit,
            ),
            momentum_no_new_high_polls_to_exit=_int(
                "LISTING_SNIPER_MOMENTUM_NO_NEW_HIGH_POLLS_TO_EXIT",
                defaults.momentum_no_new_high_polls_to_exit,
            ),
            momentum_ma_short_samples=_int(
                "LISTING_SNIPER_MOMENTUM_MA_SHORT_SAMPLES",
                defaults.momentum_ma_short_samples,
            ),
            momentum_ma_long_samples=_int(
                "LISTING_SNIPER_MOMENTUM_MA_LONG_SAMPLES",
                defaults.momentum_ma_long_samples,
            ),
            momentum_ma_cross_min_pullback_pct=_float(
                "LISTING_SNIPER_MOMENTUM_MA_CROSS_MIN_PULLBACK_PCT",
                defaults.momentum_ma_cross_min_pullback_pct,
            ),
            hard_stop_loss_pct=_float(
                "LISTING_SNIPER_HARD_STOP_LOSS_PCT", defaults.hard_stop_loss_pct,
            ),
            daily_spend_cap_usdt=_float(
                "LISTING_SNIPER_DAILY_SPEND_CAP_USDT", defaults.daily_spend_cap_usdt,
            ),
            min_balance_usdt=_float(
                "LISTING_SNIPER_MIN_BALANCE_USDT", defaults.min_balance_usdt,
            ),
            dynamic_balance=_bool("LISTING_SNIPER_DYNAMIC_BALANCE", defaults.dynamic_balance),
            state_path=os.getenv("LISTING_SNIPER_STATE_PATH", defaults.state_path),
            audit_path=os.getenv("LISTING_SNIPER_AUDIT_PATH", defaults.audit_path),
            ledger_path=os.getenv("LISTING_SNIPER_LEDGER_PATH", defaults.ledger_path),
            emergency_stop_path=os.getenv(
                "LISTING_SNIPER_EMERGENCY_STOP_PATH", defaults.emergency_stop_path,
            ),
        )
        settings.validate()
        return settings


# ---------------------------------------------------------------------------
# State
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class SnipedTrade:
    """One sniped trade we recorded."""

    symbol: str
    detected_at_ms: int
    fired_at_ms: int
    result: dict[str, Any]


@dataclass(slots=True)
class DailySpend:
    date_utc: str
    spent_usdt: float = 0.0

    def reset_if_new_day(self) -> None:
        today = datetime.now(tz=timezone.utc).strftime("%Y-%m-%d")
        if today != self.date_utc:
            self.date_utc = today
            self.spent_usdt = 0.0

    def would_exceed(self, amount: float, cap: float) -> bool:
        self.reset_if_new_day()
        return self.spent_usdt + amount > cap

    def add(self, amount: float) -> None:
        self.reset_if_new_day()
        self.spent_usdt += amount


@dataclass(slots=True)
class ListingSniperState:
    """In-memory + persistent state for the sniper daemon."""

    # The set of symbols the venue currently reports as TRADING. Updated
    # every cycle. Used to compute the new-symbol diff.
    known_trading_symbols: set[str] = field(default_factory=set)
    # Bootstrap flag: the FIRST poll after start does not fire on
    # everything it sees — those are the existing universe, not new
    # listings. After the first poll we set this True and start firing on
    # diffs.
    bootstrapped: bool = False
    # Symbols we've already sniped this run; prevents duplicate fills if
    # the daemon restarts and re-detects.
    sniped_symbols: set[str] = field(default_factory=set)
    daily_spend: DailySpend = field(default_factory=lambda: DailySpend(
        date_utc=datetime.now(tz=timezone.utc).strftime("%Y-%m-%d")
    ))

    def to_dict(self) -> dict[str, Any]:
        return {
            "known_trading_symbols": sorted(self.known_trading_symbols),
            "bootstrapped": self.bootstrapped,
            "sniped_symbols": sorted(self.sniped_symbols),
            "daily_spend": {"date_utc": self.daily_spend.date_utc,
                             "spent_usdt": self.daily_spend.spent_usdt},
        }

    @classmethod
    def from_dict(cls, payload: Mapping[str, Any]) -> "ListingSniperState":
        ds = payload.get("daily_spend") or {}
        return cls(
            known_trading_symbols=set(payload.get("known_trading_symbols") or ()),
            bootstrapped=bool(payload.get("bootstrapped")),
            sniped_symbols=set(payload.get("sniped_symbols") or ()),
            daily_spend=DailySpend(
                date_utc=str(ds.get("date_utc")
                              or datetime.now(tz=timezone.utc).strftime("%Y-%m-%d")),
                spent_usdt=float(ds.get("spent_usdt") or 0.0),
            ),
        )


def load_state(path: Path | str) -> ListingSniperState:
    p = Path(path)
    if not p.exists():
        return ListingSniperState()
    try:
        return ListingSniperState.from_dict(json.loads(p.read_text()))
    except (OSError, json.JSONDecodeError) as exc:
        raise ListingSniperError(f"could not read state at {p}: {exc}") from exc


def save_state(state: ListingSniperState, path: Path | str) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(p.suffix + ".tmp")
    tmp.write_text(json.dumps(state.to_dict(), indent=2, sort_keys=True))
    tmp.replace(p)


# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------


def _ensure_no_emergency_stop(path: str) -> None:
    if Path(path).exists():
        raise ListingSniperError(
            f"emergency stop file present at {path}; refusing to run"
        )


def _preflight(settings: ListingSniperSettings, *, submit_order: bool) -> None:
    _ensure_no_emergency_stop(settings.emergency_stop_path)
    if not submit_order:
        return
    if not settings.enabled:
        raise ListingSniperError("LISTING_SNIPER_ENABLED=true is required for live submission")
    if settings.use_testnet:
        raise ListingSniperError("listing sniper live submission requires USE_TESTNET=false")
    if settings.dry_run:
        raise ListingSniperError("listing sniper live submission requires DRY_RUN=false")
    if settings.confirm_live_listing_sniper != "yes":
        raise ListingSniperError(
            "listing sniper live submission requires CONFIRM_LIVE_LISTING_SNIPER=yes"
        )
    if os.getenv("LISTING_SNIPER_PROVEN_EDGE", "").strip().lower() != "yes":
        raise ListingSniperError(
            "listing sniper live submission requires LISTING_SNIPER_PROVEN_EDGE=yes. "
            "Run --paper for at least a handful of detected listings first; only "
            "flip this when paper ledger expectancy is positive."
        )


# ---------------------------------------------------------------------------
# exchangeInfo polling
# ---------------------------------------------------------------------------


async def fetch_trading_symbols_for_quote(
    *,
    session: aiohttp.ClientSession,
    quote_asset: str,
    timeout: float = 10.0,
) -> set[str]:
    """Return the set of ``symbol`` names currently TRADING for ``quote_asset``."""
    async with session.get(EXCHANGE_INFO_URL, timeout=timeout) as resp:
        if resp.status != 200:
            preview = (await resp.text())[:200]
            raise ListingSniperError(f"exchangeInfo HTTP {resp.status}: {preview}")
        payload = await resp.json()
    if not isinstance(payload, Mapping):
        raise ListingSniperError("exchangeInfo payload was not an object")
    symbols = payload.get("symbols") or []
    out: set[str] = set()
    if not isinstance(symbols, list):
        return out
    for row in symbols:
        if not isinstance(row, Mapping):
            continue
        if row.get("status") != "TRADING":
            continue
        if row.get("quoteAsset") != quote_asset:
            continue
        sym = row.get("symbol")
        if sym:
            out.add(str(sym))
    return out


# ---------------------------------------------------------------------------
# Symbol diff (pure logic)
# ---------------------------------------------------------------------------


def compute_new_listings(
    *,
    state: ListingSniperState,
    current_symbols: Iterable[str],
) -> list[str]:
    """Return symbols freshly entering TRADING status this cycle.

    The first cycle after process start populates ``known_trading_symbols``
    without firing — we don't want to "snipe" the entire existing universe.
    Subsequent cycles return the set difference, excluding anything we've
    already sniped this run.
    """
    current = set(current_symbols)
    if not state.bootstrapped:
        return []
    fresh = sorted(current - state.known_trading_symbols - state.sniped_symbols)
    return fresh


# ---------------------------------------------------------------------------
# Execution (paper + live)
# ---------------------------------------------------------------------------


def _utc_now_ms() -> int:
    return int(datetime.now(tz=timezone.utc).timestamp() * 1000)


def _utc_timestamp() -> str:
    return datetime.now(tz=timezone.utc).isoformat()


def _append_audit(path: str, payload: Mapping[str, Any]) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    with p.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(payload, sort_keys=True))
        fh.write("\n")


def _append_ledger(path: str, payload: Mapping[str, Any]) -> None:
    _append_audit(path, payload)


async def fetch_public_last_price(
    *, session: aiohttp.ClientSession, rest_symbol: str,
) -> float | None:
    """Public no-auth /api/v3/ticker/price fetch. Used by paper-mode polling."""
    try:
        async with session.get(
            PUBLIC_TICKER_URL, params={"symbol": rest_symbol}, timeout=5,
        ) as resp:
            if resp.status != 200:
                return None
            payload = await resp.json()
    except (aiohttp.ClientError, asyncio.TimeoutError):
        return None
    if not isinstance(payload, Mapping):
        return None
    raw = payload.get("price")
    try:
        return float(raw) if raw is not None else None
    except (TypeError, ValueError):
        return None


def momentum_should_exit(
    *,
    settings: ListingSniperSettings,
    samples: list[tuple[float, float]],
    peak: float,
) -> tuple[bool, str]:
    """Return ``(should_exit, reason)`` for the momentum exit logic.

    Three independent triggers (any one fires):
      1. velocity negative for ``momentum_negative_polls_to_exit`` consecutive polls
      2. no new high in ``momentum_no_new_high_polls_to_exit`` polls
      3. short MA crosses below long MA AND price has pulled back at
         least ``momentum_ma_cross_min_pullback_pct`` from peak
    """
    if len(samples) < 2:
        return False, ""

    # 1) Velocity-sign run length.
    k = settings.momentum_negative_polls_to_exit
    if len(samples) > k:
        # Compute velocity between consecutive pairs over the last k+1 samples.
        velocities: list[float] = []
        for i in range(-(k + 1), -1):
            dt = samples[i + 1][0] - samples[i][0]
            if dt <= 0:
                continue
            velocities.append((samples[i + 1][1] - samples[i][1]) / dt)
        if len(velocities) >= k and all(v < 0 for v in velocities[-k:]):
            return True, f"momentum_negative_for_{k}_polls"

    # 2) No new high in N polls.
    peak_idx = max(range(len(samples)), key=lambda i: samples[i][1])
    polls_since_peak = len(samples) - 1 - peak_idx
    if polls_since_peak >= settings.momentum_no_new_high_polls_to_exit:
        return True, f"no_new_high_for_{polls_since_peak}_polls"

    # 3) Short MA crosses below long MA with confirming pullback.
    if len(samples) >= settings.momentum_ma_long_samples:
        short_window = samples[-settings.momentum_ma_short_samples:]
        long_window = samples[-settings.momentum_ma_long_samples:]
        short_ma = sum(p for _, p in short_window) / len(short_window)
        long_ma = sum(p for _, p in long_window) / len(long_window)
        if short_ma < long_ma and peak > 0:
            pullback_pct = (peak - samples[-1][1]) / peak * 100.0
            if pullback_pct >= settings.momentum_ma_cross_min_pullback_pct:
                return True, (
                    f"ma_cross_with_pullback: short_ma={short_ma:.8f} < "
                    f"long_ma={long_ma:.8f}, pullback_pct={pullback_pct:.2f}"
                )
    return False, ""


async def _hold_paper(
    *,
    settings: ListingSniperSettings,
    session: aiohttp.ClientSession,
    rest_symbol: str,
    entry_price: float,
) -> tuple[float, str, float]:
    """Unified polling hold loop. Returns (exit_price, reason, elapsed).

    Polls every ``trailing_poll_interval_seconds`` from tick 1. At each poll:

      1. Hard stop loss: if ``hard_stop_loss_pct`` is negative AND the last
         price is at or below the stop level, exit immediately — regardless
         of which hold_mode or whether we're still inside the noise-filter
         floor. This is the rug-pull catch.

      2. Floor period (elapsed < hold_seconds): only the hard stop applies;
         smart-exit logic is silent because the orderbook is too noisy to
         read reliably right after listing open.

      3. Post-floor: mode-specific exit checks fire (flat / trailing /
         momentum). The hard stop still applies in addition.

      4. Max hold cap (elapsed >= max_hold_seconds): force exit.
    """
    samples: list[tuple[float, float]] = [(0.0, entry_price)]
    peak = entry_price
    elapsed = 0.0
    stop_price = (
        entry_price * (1.0 + settings.hard_stop_loss_pct / 100.0)
        if settings.hard_stop_loss_pct < 0 else 0.0
    )
    trailing_stop_factor = 1.0 - settings.trailing_stop_pct / 100.0

    while elapsed < settings.max_hold_seconds:
        await asyncio.sleep(settings.trailing_poll_interval_seconds)
        elapsed += settings.trailing_poll_interval_seconds
        last = await fetch_public_last_price(session=session, rest_symbol=rest_symbol)
        if last is None:
            continue
        samples.append((elapsed, last))
        if last > peak:
            peak = last

        # 1. Universal hard stop — active from tick 1.
        if stop_price > 0 and last <= stop_price:
            return last, (
                f"hard_stop_loss_hit: last={last:.8f} <= stop_price="
                f"{stop_price:.8f} ({settings.hard_stop_loss_pct:.1f}% from entry)"
            ), elapsed

        # 2. Inside noise-filter floor: skip smart-exit logic.
        if elapsed < settings.hold_seconds:
            continue

        # 3. Post-floor mode-specific exit.
        if settings.hold_mode == "flat":
            return last, "flat_hold_elapsed", elapsed
        if settings.hold_mode == "trailing":
            if last <= peak * trailing_stop_factor:
                return last, (
                    f"trailing_stop_hit: last={last:.8f} <= "
                    f"peak*{trailing_stop_factor:.4f}={peak * trailing_stop_factor:.8f}"
                ), elapsed
        elif settings.hold_mode == "momentum":
            should_exit, reason = momentum_should_exit(
                settings=settings, samples=samples, peak=peak,
            )
            if should_exit:
                return last, reason, elapsed

    last_price = samples[-1][1] if samples else entry_price
    return last_price, "max_hold_seconds_reached", elapsed


async def fire_paper(
    *,
    symbol: str,                # exchangeInfo-style, e.g. "FOOUSDT"
    settings: ListingSniperSettings,
    session: aiohttp.ClientSession,
) -> dict[str, Any]:
    """Paper-mode execution: simulate the trade end-to-end via public ticker.

    Entry price = current public ticker at detection.
    Exit price  = ticker at the moment the chosen ``hold_mode`` triggers.
    Net P&L     = (exit - entry) / entry minus 2x taker fees (~20 bps).
    """
    rest_symbol = symbol
    entry_price = await fetch_public_last_price(session=session, rest_symbol=rest_symbol)
    if entry_price is None or entry_price <= 0:
        return {
            "mode": "paper", "action": "buy_skipped_no_price",
            "symbol": symbol, "hold_mode": settings.hold_mode,
        }

    exit_price, exit_reason, elapsed = await _hold_paper(
        settings=settings, session=session,
        rest_symbol=rest_symbol, entry_price=entry_price,
    )

    gross_pct = (exit_price / entry_price - 1.0) * 100.0
    # Approximate round-trip fees: 10 bps taker each side = 20 bps total.
    fee_drag_pct = 0.2
    net_pct = gross_pct - fee_drag_pct
    notional = settings.notional_usdt
    pnl_usdt = notional * net_pct / 100.0

    return {
        "mode": "paper",
        "action": "round_trip_complete",
        "symbol": symbol,
        "entry_price": entry_price,
        "exit_price": exit_price,
        "gross_pnl_pct": gross_pct,
        "net_pnl_pct": net_pct,
        "pnl_usdt": pnl_usdt,
        "notional_usdt": notional,
        "hold_mode": settings.hold_mode,
        "exit_reason": exit_reason,
        "elapsed_seconds": elapsed,
    }


async def fire_live(
    *,
    symbol: str,
    settings: ListingSniperSettings,
    spot_client: Any,
) -> dict[str, Any]:
    """Live execution: market BUY → flat or trailing hold → market SELL."""
    pair = f"{symbol[:-len(settings.quote_asset)]}/{settings.quote_asset}"
    try:
        buy_order = await spot_client.create_order(
            symbol=pair, type="market", side="buy", amount=None, price=None,
            params={"quoteOrderQty": settings.notional_usdt},
        )
    except Exception as exc:  # noqa: BLE001 — log + skip
        return {"mode": "live", "action": "buy_failed", "symbol": pair,
                "reason": f"{type(exc).__name__}: {str(exc)[:200]}"}
    filled_qty = float(buy_order.get("filled") or 0.0)
    avg_buy = float(buy_order.get("average") or 0.0)
    if filled_qty <= 0:
        return {"mode": "live", "action": "buy_zero_fill", "symbol": pair,
                "buy_order": buy_order}

    # Unified hold loop with hard stop loss active from tick 1 —
    # exactly mirrors _hold_paper. See its docstring for the rules.
    from src.listing_detector import _ticker_last_price
    samples: list[tuple[float, float]] = [(0.0, avg_buy)]
    peak = avg_buy
    elapsed = 0.0
    exit_reason = ""
    stop_price = (
        avg_buy * (1.0 + settings.hard_stop_loss_pct / 100.0)
        if settings.hard_stop_loss_pct < 0 else 0.0
    )
    trailing_stop_factor = 1.0 - settings.trailing_stop_pct / 100.0
    while elapsed < settings.max_hold_seconds:
        await asyncio.sleep(settings.trailing_poll_interval_seconds)
        elapsed += settings.trailing_poll_interval_seconds
        last = await _ticker_last_price(spot_client, pair)
        if last is None:
            continue
        samples.append((elapsed, last))
        if last > peak:
            peak = last

        if stop_price > 0 and last <= stop_price:
            exit_reason = (
                f"hard_stop_loss_hit: last={last:.8f} <= stop_price="
                f"{stop_price:.8f} ({settings.hard_stop_loss_pct:.1f}% from entry)"
            )
            break
        if elapsed < settings.hold_seconds:
            continue
        if settings.hold_mode == "flat":
            exit_reason = "flat_hold_elapsed"
            break
        if settings.hold_mode == "trailing":
            if last <= peak * trailing_stop_factor:
                exit_reason = (
                    f"trailing_stop_hit: last={last:.8f} <= peak*"
                    f"{trailing_stop_factor:.4f}={peak * trailing_stop_factor:.8f}"
                )
                break
        elif settings.hold_mode == "momentum":
            should_exit, reason = momentum_should_exit(
                settings=settings, samples=samples, peak=peak,
            )
            if should_exit:
                exit_reason = reason
                break
    else:
        exit_reason = "max_hold_seconds_reached"

    try:
        sell_order = await spot_client.create_order(
            symbol=pair, type="market", side="sell", amount=filled_qty,
        )
    except Exception as exc:  # noqa: BLE001 — position stranded
        return {"mode": "live", "action": "sell_failed", "symbol": pair,
                "buy_order": buy_order,
                "reason": f"{type(exc).__name__}: {str(exc)[:200]}",
                "elapsed_seconds": elapsed, "exit_reason": exit_reason}
    sold_qty = float(sell_order.get("filled") or 0.0)
    avg_sell = float(sell_order.get("average") or 0.0)
    return {
        "mode": "live",
        "action": "round_trip_complete",
        "symbol": pair,
        "filled_qty": filled_qty,
        "avg_buy": avg_buy,
        "avg_sell": avg_sell,
        "sold_qty": sold_qty,
        "pnl_quote": (avg_sell - avg_buy) * filled_qty,
        "hold_mode": settings.hold_mode,
        "elapsed_seconds": elapsed,
        "exit_reason": exit_reason,
        "buy_order_id": str(buy_order.get("id") or ""),
        "sell_order_id": str(sell_order.get("id") or ""),
    }


# ---------------------------------------------------------------------------
# Cycle + daemon
# ---------------------------------------------------------------------------


async def run_cycle(
    *,
    settings: ListingSniperSettings,
    state: ListingSniperState,
    mode: str = "plan_only",
    session: aiohttp.ClientSession,
    spot_client: Any | None = None,
) -> dict[str, Any]:
    """One poll cycle: fetch, diff, optionally fire."""
    if mode not in {"plan_only", "paper", "live"}:
        raise ListingSniperError(f"unknown mode: {mode}")
    _preflight(settings, submit_order=(mode == "live"))

    current = await fetch_trading_symbols_for_quote(
        session=session, quote_asset=settings.quote_asset,
    )
    new_symbols = compute_new_listings(state=state, current_symbols=current)

    actions: list[dict[str, Any]] = []
    for symbol in new_symbols:
        from src.alerts import maybe_telegram
        await maybe_telegram(
            f"Listing sniper: new {settings.quote_asset} pair detected → {symbol}"
        )
        if mode in {"paper", "live"} and state.daily_spend.would_exceed(
            settings.notional_usdt, settings.daily_spend_cap_usdt,
        ):
            actions.append({"symbol": symbol, "action": "skipped_daily_cap",
                             "spent_usdt": state.daily_spend.spent_usdt,
                             "cap_usdt": settings.daily_spend_cap_usdt})
            continue

        if mode == "live" and spot_client is not None:
            result = await fire_live(
                symbol=symbol, settings=settings, spot_client=spot_client,
            )
            actions.append(result)
            state.daily_spend.add(settings.notional_usdt)
            state.sniped_symbols.add(symbol)
            _append_ledger(settings.ledger_path, {
                "timestamp": _utc_timestamp(), "symbol": symbol, **result,
            })
        elif mode == "paper":
            result = await fire_paper(
                symbol=symbol, settings=settings, session=session,
            )
            actions.append(result)
            state.daily_spend.add(settings.notional_usdt)
            state.sniped_symbols.add(symbol)
            _append_ledger(settings.ledger_path, {
                "timestamp": _utc_timestamp(), "symbol": symbol, **result,
            })

    # Update the snapshot for next cycle's diff.
    state.known_trading_symbols = current
    state.bootstrapped = True

    payload = {
        "timestamp": _utc_timestamp(),
        "mode": mode,
        "quote_asset": settings.quote_asset,
        "current_universe_size": len(current),
        "new_symbols": new_symbols,
        "actions": actions,
        "daily_spent_usdt": state.daily_spend.spent_usdt,
        "daily_cap_usdt": settings.daily_spend_cap_usdt,
        "sniped_lifetime": len(state.sniped_symbols),
    }
    _append_audit(settings.audit_path, payload)
    save_state(state, settings.state_path)
    return payload


async def _build_spot_client(settings: ListingSniperSettings) -> Any:
    import ccxt.async_support as ccxt_async
    return ccxt_async.binance({
        "apiKey": settings.api_key, "secret": settings.api_secret,
        "enableRateLimit": True,
        "options": {"adjustForTimeDifference": True, "fetchCurrencies": False},
    })


async def run_daemon(settings: ListingSniperSettings, *, mode: str) -> None:
    state = load_state(settings.state_path)
    async with aiohttp.ClientSession() as session:
        spot_client: Any | None = None
        if mode == "live":
            spot_client = await _build_spot_client(settings)
        try:
            while True:
                try:
                    await run_cycle(
                        settings=settings, state=state,
                        mode=mode, session=session,
                        spot_client=spot_client,
                    )
                except ListingSniperError as exc:
                    _append_audit(settings.audit_path, {
                        "timestamp": _utc_timestamp(), "error": str(exc),
                    })
                except Exception as exc:  # noqa: BLE001 — never abort
                    _append_audit(settings.audit_path, {
                        "timestamp": _utc_timestamp(),
                        "error": f"{type(exc).__name__}: {str(exc)[:200]}",
                    })
                await asyncio.sleep(settings.poll_interval_seconds)
        finally:
            if spot_client is not None:
                try:
                    await spot_client.close()
                except Exception:  # noqa: BLE001
                    pass


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Listing sniper: poll exchangeInfo, fire on new TRADING symbols."
    )
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--plan-only", action="store_const", const="plan_only", dest="mode")
    group.add_argument("--paper", action="store_const", const="paper", dest="mode")
    group.add_argument("--submit-live", action="store_const", const="live", dest="mode")
    parser.add_argument("--daemon", action="store_true",
                        help="loop forever every poll_interval_seconds")
    parser.set_defaults(mode="plan_only")
    args = parser.parse_args(argv)

    async def _once() -> dict[str, Any]:
        async with aiohttp.ClientSession() as session:
            settings = ListingSniperSettings.from_env()
            state = load_state(settings.state_path)
            spot_client: Any | None = None
            try:
                if args.mode == "live":
                    spot_client = await _build_spot_client(settings)
                return await run_cycle(
                    settings=settings, state=state, mode=args.mode,
                    session=session, spot_client=spot_client,
                )
            finally:
                if spot_client is not None:
                    try:
                        await spot_client.close()
                    except Exception:  # noqa: BLE001
                        pass

    try:
        if args.daemon:
            settings = ListingSniperSettings.from_env()
            asyncio.run(run_daemon(settings, mode=args.mode))
            return 0
        result = asyncio.run(_once())
    except ListingSniperError as exc:
        print(f"listing-sniper failed: {exc}")
        return 1
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())
