from __future__ import annotations

import json
import asyncio
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

import pytest

import src.claude_spot as claude_spot
from src.claude_spot import (
    CLAUDE_MONEY_MAKER_MISSION,
    ClaudeSpotDecision,
    ClaudeSpotError,
    ClaudeSpotSettings,
    HeadlessClaudeSpotAdvisor,
    build_decision_context,
    run_claude_spot_cycle,
)
from src.pool.models import MarketSnapshot, RouteLeg, RouteQuote
from src.social_signal import (
    SOCIAL_LABEL_BULLISH,
    SOCIAL_LABEL_MANIPULATION,
    SocialSignalRecord,
    write_social_audit,
)


def _settings(tmp_path: Path, **overrides: Any) -> ClaudeSpotSettings:
    values = {
        "api_key": "key",
        "api_secret": "secret",
        "use_testnet": False,
        "dry_run": False,
        "confirm_live_trading": "yes",
        "enabled": True,
        "dynamic_scope": "all_eligible_spot",
        "audit_path": str(tmp_path / "claude-spot.jsonl"),
        "lock_path": str(tmp_path / "claude-spot.lock"),
        "emergency_stop_path": str(tmp_path / "claude-spot.stop"),
        "min_quote_volume_usdt": 0.0,
        "candidate_limit": 20,
        "ticker_stale_seconds": 0.0,
        "market_evidence_enabled": False,
        "require_market_evidence_for_live": False,
    }
    values.update(overrides)
    return ClaudeSpotSettings(**values)


def _market(symbol: str, *, price: float, precision: int | float = 0.000001) -> dict[str, Any]:
    base, quote = symbol.split("/")
    step_size = str(precision)
    return {
        "symbol": symbol,
        "base": base,
        "quote": quote,
        "spot": True,
        "active": True,
        "precision": {"amount": precision},
        "limits": {"amount": {"min": precision}, "cost": {"min": 5.0}},
        "info": {
            "filters": [
                {
                    "filterType": "LOT_SIZE",
                    "minQty": step_size,
                    "stepSize": step_size,
                }
            ]
        },
    }


class FakeClaudeSpotClient:
    def __init__(self, *, final_status: str = "closed") -> None:
        self.final_status = final_status
        self.balances = {
            "free": {"BNB": 0.02, "USDT": 0.0, "EDEN": 0.0},
            "total": {"BNB": 0.02, "USDT": 0.0, "EDEN": 0.0},
        }
        self.orders: list[tuple[str, str, str, float]] = []
        self.order_tests: list[tuple[Any, ...]] = []
        self.requests: list[tuple[str, tuple[Any, ...]]] = []

    async def get_balance(self) -> dict[str, dict[str, float]]:
        return {
            group: dict(values)
            for group, values in self.balances.items()
        }

    async def request(self, method: str, *args: Any, **_: Any) -> Any:
        self.requests.append((method, args))
        if method == "privateGetAccount":
            return {
                "canTrade": True,
                "canWithdraw": False,
                "accountType": "SPOT",
                "permissions": ["SPOT"],
            }
        if method == "load_markets":
            return {
                "BNB/USDT": _market("BNB/USDT", price=600.0, precision=0.000001),
                "EDEN/USDT": _market("EDEN/USDT", price=1.0, precision=0.001),
            }
        if method == "fetch_tickers":
            return {
                "BNB/USDT": {
                    "bid": 600.0,
                    "ask": 601.0,
                    "last": 600.0,
                    "quoteVolume": 10_000_000.0,
                    "timestamp": 1,
                },
                "EDEN/USDT": {
                    "bid": 0.998,
                    "ask": 1.0,
                    "last": 1.0,
                    "quoteVolume": 1_000_000.0,
                    "timestamp": 1,
                },
            }
        if method == "privatePostOrderTest":
            self.order_tests.append(args)
            return {}
        if method == "fetch_order":
            return {"id": args[0], "status": self.final_status, "filled": 1.0}
        raise AssertionError(f"unexpected request: {method}")

    async def fetch_ticker(self, symbol: str) -> dict[str, float]:
        if symbol == "BNB/USDT":
            return {"bid": 600.0, "ask": 601.0, "last": 600.0}
        if symbol == "EDEN/USDT":
            return {"bid": 0.998, "ask": 1.0, "last": 1.0}
        raise AssertionError(f"unexpected ticker: {symbol}")

    async def get_commission_rates(self, symbol: str) -> dict[str, Any]:
        return {
            "symbol": symbol.replace("/", ""),
            "standardCommission": {
                "maker": "0.001",
                "taker": "0.001",
                "buyer": "0",
                "seller": "0",
            },
            "discount": {"enabledForAccount": False, "enabledForSymbol": False},
        }

    async def create_order(
        self,
        symbol: str,
        order_type: str,
        side: str,
        amount: float,
    ) -> dict[str, Any]:
        self.orders.append((symbol, order_type, side, amount))
        assert order_type == "market"
        if symbol == "BNB/USDT":
            self.balances["free"]["BNB"] = 0.0
            self.balances["total"]["BNB"] = 0.0
            self.balances["free"]["USDT"] = 11.95
            self.balances["total"]["USDT"] = 11.95
            return {"id": "leg-1", "status": self.final_status, "filled": amount}
        if symbol == "EDEN/USDT":
            self.balances["free"]["USDT"] = 0.0
            self.balances["total"]["USDT"] = 0.0
            self.balances["free"]["EDEN"] = 11.9
            self.balances["total"]["EDEN"] = 11.9
            return {"id": "leg-2", "status": self.final_status, "filled": amount}
        raise AssertionError(f"unexpected order symbol: {symbol}")

    async def close(self) -> None:
        return None


class PriceMovedBuyClient(FakeClaudeSpotClient):
    def __init__(self) -> None:
        super().__init__()
        self.buy_costs: list[float] = []
        self.quote_before_buy: list[float] = []

    async def create_order(
        self,
        symbol: str,
        order_type: str,
        side: str,
        amount: float,
    ) -> dict[str, Any]:
        if symbol != "EDEN/USDT":
            return await super().create_order(symbol, order_type, side, amount)
        self.orders.append((symbol, order_type, side, amount))
        assert order_type == "market"
        quote_free = self.balances["free"]["USDT"]
        fill_price = 1.005
        cost = amount * fill_price
        self.quote_before_buy.append(quote_free)
        if cost > quote_free:
            raise RuntimeError("Account has insufficient balance for requested action")
        self.buy_costs.append(cost)
        self.balances["free"]["USDT"] = quote_free - cost
        self.balances["total"]["USDT"] = quote_free - cost
        self.balances["free"]["EDEN"] = amount * 0.999
        self.balances["total"]["EDEN"] = amount * 0.999
        return {"id": "leg-2", "status": self.final_status, "filled": amount}


class FailingSecondLegClient(FakeClaudeSpotClient):
    async def create_order(
        self,
        symbol: str,
        order_type: str,
        side: str,
        amount: float,
    ) -> dict[str, Any]:
        if symbol == "EDEN/USDT":
            self.orders.append((symbol, order_type, side, amount))
            raise RuntimeError("Account has insufficient balance for requested action")
        return await super().create_order(symbol, order_type, side, amount)


class ExecutionCommissionFailureClient(FakeClaudeSpotClient):
    def __init__(self) -> None:
        super().__init__()
        self.commission_calls: dict[str, int] = {}

    async def get_commission_rates(self, symbol: str) -> dict[str, Any]:
        self.commission_calls[symbol] = self.commission_calls.get(symbol, 0) + 1
        if symbol == "EDEN/USDT" and self.commission_calls[symbol] > 1:
            raise RuntimeError("commission endpoint unavailable")
        return await super().get_commission_rates(symbol)


def _ohlcv_rows(
    *,
    start_close: float,
    end_close: float,
    count: int = 170,
    start_volume: float = 1_000.0,
    end_volume: float = 2_000.0,
    step_ms: int = 3_600_000,
) -> list[list[float]]:
    rows: list[list[float]] = []
    for index in range(count):
        progress = index / (count - 1)
        close = start_close + (end_close - start_close) * progress
        volume = start_volume + (end_volume - start_volume) * progress
        timestamp = index * step_ms
        rows.append([timestamp, close, close, close, close, volume])
    return rows


class EvidenceClaudeSpotClient(FakeClaudeSpotClient):
    def __init__(
        self,
        *,
        eden_start: float = 1.0,
        eden_end: float = 1.25,
        bnb_start: float = 600.0,
        bnb_end: float = 600.0,
        eden_short_start: float | None = None,
        eden_short_end: float | None = None,
        bnb_short_start: float | None = None,
        bnb_short_end: float | None = None,
    ) -> None:
        super().__init__()
        self.eden_start = eden_start
        self.eden_end = eden_end
        self.bnb_start = bnb_start
        self.bnb_end = bnb_end
        self.eden_short_start = eden_start if eden_short_start is None else eden_short_start
        self.eden_short_end = eden_end if eden_short_end is None else eden_short_end
        self.bnb_short_start = bnb_start if bnb_short_start is None else bnb_short_start
        self.bnb_short_end = bnb_end if bnb_short_end is None else bnb_short_end
        self.ohlcv_calls: list[tuple[str, str]] = []

    async def fetch_ohlcv(
        self,
        symbol: str,
        timeframe: str,
        start_time: datetime,
        end_time: datetime,
    ) -> list[list[float]]:
        self.ohlcv_calls.append((symbol, timeframe))
        if symbol == "BNB/USDT":
            if timeframe == "1m":
                return _ohlcv_rows(
                    start_close=self.bnb_short_start,
                    end_close=self.bnb_short_end,
                    count=20,
                    step_ms=60_000,
                )
            return _ohlcv_rows(start_close=self.bnb_start, end_close=self.bnb_end)
        if symbol == "EDEN/USDT":
            if timeframe == "1m":
                return _ohlcv_rows(
                    start_close=self.eden_short_start,
                    end_close=self.eden_short_end,
                    count=20,
                    step_ms=60_000,
                )
            return _ohlcv_rows(start_close=self.eden_start, end_close=self.eden_end)
        return []


class MissingEvidenceClient(EvidenceClaudeSpotClient):
    async def fetch_ohlcv(
        self,
        symbol: str,
        timeframe: str,
        start_time: datetime,
        end_time: datetime,
    ) -> list[list[float]]:
        self.ohlcv_calls.append((symbol, timeframe))
        return []


class HoldingMemoryClient(FakeClaudeSpotClient):
    def __init__(self) -> None:
        super().__init__()
        self.balances = {
            "free": {"BNB": 0.0, "USDT": 0.0, "EDEN": 10.0},
            "total": {"BNB": 0.0, "USDT": 0.0, "EDEN": 10.0},
        }


class SmallMarketClient(FakeClaudeSpotClient):
    async def request(self, method: str, *args: Any, **kwargs: Any) -> Any:
        self.requests.append((method, args))
        if method == "privateGetAccount":
            return {
                "canTrade": True,
                "canWithdraw": False,
                "accountType": "SPOT",
                "permissions": ["SPOT"],
            }
        if method == "load_markets":
            return {
                "BNB/USDT": _market("BNB/USDT", price=600.0, precision=0.000001),
                "MICRO/USDT": _market("MICRO/USDT", price=0.02, precision=1.0),
                "LARGE/USDT": _market("LARGE/USDT", price=10.0, precision=0.001),
            }
        if method == "fetch_tickers":
            return {
                "BNB/USDT": {
                    "bid": 600.0,
                    "ask": 601.0,
                    "last": 600.0,
                    "quoteVolume": 10_000_000.0,
                    "timestamp": 1,
                },
                "MICRO/USDT": {
                    "bid": 0.0199,
                    "ask": 0.02,
                    "last": 0.02,
                    "quoteVolume": 20_000.0,
                    "timestamp": 1,
                },
                "LARGE/USDT": {
                    "bid": 9.99,
                    "ask": 10.0,
                    "last": 10.0,
                    "quoteVolume": 5_000_000.0,
                    "timestamp": 1,
                },
            }
        return await super().request(method, *args, **kwargs)


class IdRRouteClient(FakeClaudeSpotClient):
    def __init__(self) -> None:
        super().__init__()
        self.balances = {
            "free": {"POL": 50.0, "USDT": 0.0, "SUI": 0.0},
            "total": {"POL": 50.0, "USDT": 0.0, "SUI": 0.0},
        }

    async def request(self, method: str, *args: Any, **kwargs: Any) -> Any:
        self.requests.append((method, args))
        if method == "privateGetAccount":
            return {
                "canTrade": True,
                "canWithdraw": False,
                "accountType": "SPOT",
                "permissions": ["SPOT"],
            }
        if method == "load_markets":
            return {
                "POL/IDR": _market("POL/IDR", price=3_000.0, precision=0.1),
                "SUI/IDR": _market("SUI/IDR", price=50_000.0, precision=0.1),
                "USDT/IDR": _market("USDT/IDR", price=16_000.0, precision=0.1),
                "POL/TRY": _market("POL/TRY", price=6.0, precision=0.1),
                "SUI/JPY": _market("SUI/JPY", price=470.0, precision=0.1),
                "USDT/EUR": _market("USDT/EUR", price=0.9, precision=0.1),
                "POL/USDT": _market("POL/USDT", price=0.18, precision=0.1),
                "SUI/USDT": _market("SUI/USDT", price=3.0, precision=0.1),
            }
        if method == "fetch_tickers":
            return {
                "POL/IDR": {
                    "bid": 3_000.0,
                    "ask": 3_002.0,
                    "last": 3_001.0,
                    "quoteVolume": 2_000_000_000.0,
                    "timestamp": 1,
                },
                "SUI/IDR": {
                    "bid": 50_000.0,
                    "ask": 50_050.0,
                    "last": 50_025.0,
                    "quoteVolume": 2_000_000_000.0,
                    "timestamp": 1,
                },
                "USDT/IDR": {
                    "bid": 16_000.0,
                    "ask": 16_010.0,
                    "last": 16_005.0,
                    "quoteVolume": 2_000_000_000.0,
                    "timestamp": 1,
                },
                "POL/TRY": {
                    "bid": 6.0,
                    "ask": 6.01,
                    "last": 6.0,
                    "quoteVolume": 20_000_000.0,
                    "timestamp": 1,
                },
                "SUI/JPY": {
                    "bid": 470.0,
                    "ask": 471.0,
                    "last": 470.0,
                    "quoteVolume": 20_000_000.0,
                    "timestamp": 1,
                },
                "USDT/EUR": {
                    "bid": 0.9,
                    "ask": 0.901,
                    "last": 0.9,
                    "quoteVolume": 20_000_000.0,
                    "timestamp": 1,
                },
                "POL/USDT": {
                    "bid": 0.18,
                    "ask": 0.181,
                    "last": 0.18,
                    "quoteVolume": 100_000.0,
                    "timestamp": 1,
                },
                "SUI/USDT": {
                    "bid": 3.0,
                    "ask": 3.01,
                    "last": 3.0,
                    "quoteVolume": 100_000.0,
                    "timestamp": 1,
                },
            }
        return await super().request(method, *args, **kwargs)


class FakeAlerter:
    def __init__(self) -> None:
        self.messages: list[str] = []

    async def send(self, message: str) -> bool:
        self.messages.append(message)
        return True


class RouteChoosingAdvisor:
    def __init__(
        self,
        destination: str = "EDEN",
        reason: str = "ai_money_maker_route",
    ) -> None:
        self.destination = destination
        self.reason = reason
        self.contexts = []

    def decide(self, context):
        self.contexts.append(context)
        candidate = next(
            item
            for item in context.candidate_routes
            if item.route.destination_asset == self.destination
        )
        return ClaudeSpotDecision(
            action="rebalance",
            route_id=candidate.route_id,
            source_asset=candidate.route.source_asset,
            destination_asset=candidate.route.destination_asset,
            spend_mode="all_available",
            spend_amount=candidate.route.input_amount,
            reason=self.reason,
        )


class TimeoutAdvisor:
    def decide(self, context):
        raise claude_spot.ClaudeSpotDecisionTimeout(
            "Claude decision timed out after 120.0 seconds"
        )


class NoCallAdvisor:
    def decide(self, context):
        raise AssertionError("advisor should not be called")


def _write_audit_row(tmp_path: Path, **payload: Any) -> None:
    path = tmp_path / "claude-spot.jsonl"
    path.parent.mkdir(parents=True, exist_ok=True)
    row = {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "status": "submitted",
        "submission_attempted": True,
        "submitted_legs": [{"symbol": "BNB/USDT"}],
        "decision": {"action": "rebalance", "route_id": "BNB->EDEN:test"},
    }
    row.update(payload)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(row) + "\n")


def _candidate_with_evidence(
    expected_edge_bps: float,
    *,
    gate_pass: bool = False,
    status: str = "ready",
) -> claude_spot.ClaudeRouteCandidate:
    route = RouteQuote(
        source_asset="BNB",
        destination_asset="EDEN",
        input_amount=0.02,
        output_amount=10.0,
        slippage_bps=25.0,
        legs=(),
    )
    return claude_spot.ClaudeRouteCandidate(
        route_id="BNB->EDEN:test",
        route=route,
        estimated_output_usdt=10.0,
        source_value_usdt=12.0,
        immediate_friction_bps=75.0,
        market_evidence={
            "status": status,
            "expected_edge_after_friction_bps": expected_edge_bps,
            "minimum_expected_edge_bps": 50.0,
            "gate_pass": gate_pass,
        },
    )


def _decision_context_for_candidate(
    candidate: claude_spot.ClaudeRouteCandidate,
) -> claude_spot.ClaudeDecisionContext:
    return claude_spot.ClaudeDecisionContext(
        timestamp="now",
        prompt_version="test",
        mission=CLAUDE_MONEY_MAKER_MISSION,
        market_pick_mode="small_market_aggressive",
        upfront_friction_allowance_bps=300.0,
        balances={},
        portfolio_value_usdt=12.0,
        candidate_routes=(candidate,),
        execution_invariants=(),
        route_scope={},
        trade_memory={},
    )


def _rebalance_decision(
    *,
    reason: str = "speculative reversal thesis after oversold capitulation",
) -> ClaudeSpotDecision:
    return ClaudeSpotDecision(
        action="rebalance",
        route_id="BNB->EDEN:test",
        source_asset="BNB",
        destination_asset="EDEN",
        spend_mode="all_available",
        spend_amount=0.02,
        reason=reason,
    )


def _result_for_alert(**overrides: Any) -> claude_spot.ClaudeSpotResult:
    payload: dict[str, Any] = {
        "timestamp": "2026-05-27T00:00:00+00:00",
        "mode": "live",
        "status": "held",
        "prompt_version": "test",
        "context_hash": "hash",
        "decision": {"action": "hold", "reason": "no route"},
        "candidate_route_count": 0,
    }
    payload.update(overrides)
    return claude_spot.ClaudeSpotResult(**payload)


def _alert_message(result: claude_spot.ClaudeSpotResult, reason_chars: int = 450) -> str:
    return claude_spot._format_alert_message(
        result,
        Path("/tmp/claude-spot.jsonl"),
        True,
        reason_chars,
    )


def test_telegram_alert_explains_hold_without_candidates() -> None:
    message = _alert_message(
        _result_for_alert(
            status="held",
            decision={"action": "hold", "reason": "no executable supplied route"},
            candidate_route_count=0,
        )
    )

    assert "ELI5:" in message
    assert "No valid trade route passed the filters" in message
    assert "Candidates: <code>0</code>" in message
    assert "Submitted: <code>no</code>" in message


def test_telegram_alert_explains_evidence_rejection() -> None:
    message = _alert_message(
        _result_for_alert(
            status="held",
            decision={
                "action": "hold",
                "reason": "market evidence gate rejected selected route",
            },
            candidate_route_count=2,
            selected_route={
                "route_id": "BNB->EDEN:test",
                "source_asset": "BNB",
                "destination_asset": "EDEN",
                "market_evidence": {
                    "expected_edge_after_friction_bps": 10.0,
                    "minimum_expected_edge_bps": 50.0,
                },
            },
        )
    )

    assert "Claude considered BNB-&gt;EDEN" in message
    assert "10.00 bps" in message
    assert "required 50.00 bps" in message


def test_telegram_alert_explains_planned_route() -> None:
    message = _alert_message(
        _result_for_alert(
            status="planned",
            decision={
                "action": "rebalance",
                "route_id": "BNB->EDEN:test",
                "source_asset": "BNB",
                "destination_asset": "EDEN",
                "reason": "ai_money_maker_route",
            },
            selected_route={
                "route_id": "BNB->EDEN:test",
                "source_asset": "BNB",
                "destination_asset": "EDEN",
            },
            candidate_route_count=3,
        )
    )

    assert "plan-only" in message
    assert "Route: <code>BNB-&gt;EDEN" in message
    assert "Submitted: <code>no</code>" in message


def test_telegram_alert_explains_submitted_route() -> None:
    message = _alert_message(
        _result_for_alert(
            status="submitted",
            decision={
                "action": "rebalance",
                "route_id": "BNB->EDEN:test",
                "source_asset": "BNB",
                "destination_asset": "EDEN",
                "reason": "ai_money_maker_route",
            },
            selected_route={
                "route_id": "BNB->EDEN:test",
                "source_asset": "BNB",
                "destination_asset": "EDEN",
            },
            candidate_route_count=3,
            submission_attempted=True,
        )
    )

    assert "passed balance, scope, fee/friction, evidence, order-test" in message
    assert "Submitted: <code>yes</code>" in message


def test_telegram_alert_explains_failed_execution() -> None:
    message = _alert_message(
        _result_for_alert(
            status="failed",
            decision={
                "action": "rebalance",
                "route_id": "BNB->EDEN:test",
                "source_asset": "BNB",
                "destination_asset": "EDEN",
                "reason": "try route",
            },
            selected_route={
                "route_id": "BNB->EDEN:test",
                "source_asset": "BNB",
                "destination_asset": "EDEN",
            },
            submission_attempted=True,
            error="route halted",
        )
    )

    assert "execution failed" in message
    assert "uncertain wallet state" in message


def test_telegram_alert_explains_blocked_cycle() -> None:
    message = _alert_message(
        _result_for_alert(
            status="blocked",
            decision={},
            error="Claude Spot live submit requires all_eligible_spot",
        )
    )

    assert "safety or configuration check blocked" in message
    assert "Claude Spot live submit requires all_eligible_spot" in message


def test_telegram_alert_escapes_html_and_truncates_reason() -> None:
    message = _alert_message(
        _result_for_alert(
            decision={
                "action": "hold",
                "reason": "<b>bad</b> & very long reason",
            },
        ),
        reason_chars=18,
    )

    assert "&lt;b&gt;bad&lt;/b&gt; &amp; ve..." in message
    assert "<code><b>bad</b>" not in message


def test_telegram_reason_chars_zero_keeps_full_reason() -> None:
    reason = "full why text " * 80

    message = _alert_message(
        _result_for_alert(decision={"action": "hold", "reason": reason}),
        reason_chars=0,
    )

    assert claude_spot._compact_text(reason) in message


@pytest.mark.asyncio
async def test_telegram_alert_splits_long_full_reason(tmp_path: Path) -> None:
    reason = "after fee edge remains strong " * 450
    alerter = FakeAlerter()

    sent = await claude_spot._send_alert(
        alerter,
        _result_for_alert(decision={"action": "hold", "reason": reason}),
        tmp_path / "claude-spot.jsonl",
        _settings(tmp_path, telegram_reason_chars=0),
    )

    assert sent is True
    assert len(alerter.messages) > 1
    assert all(len(message) <= claude_spot.TELEGRAM_MESSAGE_MAX_CHARS for message in alerter.messages)
    assert any("CLAUDE SPOT WHY" in message for message in alerter.messages[1:])
    assert "after fee edge remains strong" in " ".join(alerter.messages)


def test_claude_decision_validation_requires_route_for_rebalance() -> None:
    with pytest.raises(ClaudeSpotError, match="route_id"):
        ClaudeSpotDecision.from_mapping(
            {
                "action": "rebalance",
                "source_asset": "BNB",
                "destination_asset": "EDEN",
                "spend_mode": "all_available",
                "reason": "missing route",
            }
        )

    decision = ClaudeSpotDecision.from_mapping(
        {"action": "hold", "reason": "no edge"}
    )

    assert decision.action == "hold"


def test_headless_claude_prompt_contains_mission_and_schema(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    calls: list[dict[str, Any]] = []

    class Completed:
        returncode = 0
        stderr = ""
        stdout = json.dumps({"structured_output": {"action": "hold", "reason": "wait"}})

    def fake_run(argv, **kwargs):
        calls.append({"argv": argv, "kwargs": kwargs})
        return Completed()

    monkeypatch.setattr(claude_spot.subprocess, "run", fake_run)
    client = FakeClaudeSpotClient()
    context, _ = asyncio.run(build_decision_context(client, _settings(tmp_path)))

    advisor = HeadlessClaudeSpotAdvisor(command="claude", model="sonnet", timeout_seconds=3)
    decision = advisor.decide(context)

    assert decision.action == "hold"
    argv = calls[0]["argv"]
    assert "--json-schema" in argv
    schema = json.loads(argv[argv.index("--json-schema") + 1])
    assert schema["type"] == "object"
    assert "allOf" not in schema
    assert "--model" in argv
    assert "sonnet" in argv
    system_prompt = argv[argv.index("--system-prompt") + 1]
    user_prompt = argv[-1]
    assert CLAUDE_MONEY_MAKER_MISSION in system_prompt
    assert "AGGRESSIVE risk taker" in system_prompt
    assert "social_signal_evidence" in system_prompt
    assert "after all modeled fees" in user_prompt
    assert "speculative_min_edge_bps" in user_prompt
    assert "concrete speculative thesis" in user_prompt
    assert "short_returns_bps 5m/15m" in user_prompt
    assert calls[0]["kwargs"]["timeout"] == 3


def test_headless_claude_timeout_is_concise(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def fake_run(argv, **kwargs):
        raise claude_spot.subprocess.TimeoutExpired(argv, kwargs["timeout"])

    monkeypatch.setattr(claude_spot.subprocess, "run", fake_run)
    context = claude_spot.ClaudeDecisionContext(
        timestamp="now",
        prompt_version="v",
        mission=CLAUDE_MONEY_MAKER_MISSION,
        market_pick_mode="small_market_aggressive",
        upfront_friction_allowance_bps=500.0,
        balances={},
        portfolio_value_usdt=0.0,
        candidate_routes=(),
        execution_invariants=(),
    )

    advisor = HeadlessClaudeSpotAdvisor(command="claude", timeout_seconds=3)

    with pytest.raises(claude_spot.ClaudeSpotDecisionTimeout, match="3.0 seconds"):
        advisor.decide(context)


def test_from_env_defaults_to_wider_small_market_friction_and_evidence_caps(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("BINANCE_API_KEY", "key")
    monkeypatch.setenv("BINANCE_API_SECRET", "secret")
    monkeypatch.delenv("CLAUDE_SPOT_ALLOW_NEGATIVE_FRICTION_BPS", raising=False)
    monkeypatch.delenv("CLAUDE_SPOT_MARKET_EVIDENCE_MIN_EDGE_BPS", raising=False)
    monkeypatch.delenv("CLAUDE_SPOT_LEG_ROUNDING_RESERVE_BPS", raising=False)
    monkeypatch.delenv("CLAUDE_SPOT_MIN_LEG_QUOTE_RESERVE_BPS", raising=False)
    monkeypatch.delenv("CLAUDE_SPOT_TELEGRAM_REASON_CHARS", raising=False)
    monkeypatch.delenv("CLAUDE_SPOT_SPECULATIVE_MODE_ENABLED", raising=False)
    monkeypatch.delenv("CLAUDE_SPOT_SPECULATIVE_MIN_EDGE_BPS", raising=False)
    monkeypatch.delenv("CLAUDE_SPOT_SPECULATIVE_REQUIRE_THESIS", raising=False)

    settings = ClaudeSpotSettings.from_env()

    assert settings.allow_negative_friction_bps == pytest.approx(300.0)
    assert settings.market_evidence_min_edge_bps == pytest.approx(50.0)
    assert settings.leg_rounding_reserve_bps == pytest.approx(5.0)
    assert settings.min_leg_quote_reserve_bps == pytest.approx(25.0)
    assert settings.telegram_reason_chars == 0
    assert settings.speculative_mode_enabled is False
    assert settings.speculative_min_edge_bps == pytest.approx(-1000.0)
    assert settings.speculative_require_thesis is True


def test_from_env_loads_speculative_mode(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("BINANCE_API_KEY", "key")
    monkeypatch.setenv("BINANCE_API_SECRET", "secret")
    monkeypatch.setenv("CLAUDE_SPOT_SPECULATIVE_MODE_ENABLED", "true")
    monkeypatch.setenv("CLAUDE_SPOT_SPECULATIVE_MIN_EDGE_BPS", "-750")
    monkeypatch.setenv("CLAUDE_SPOT_SPECULATIVE_REQUIRE_THESIS", "false")

    settings = ClaudeSpotSettings.from_env()

    assert settings.speculative_mode_enabled is True
    assert settings.speculative_min_edge_bps == pytest.approx(-750.0)
    assert settings.speculative_require_thesis is False


def test_speculative_min_edge_rejects_non_finite(tmp_path: Path) -> None:
    with pytest.raises(ClaudeSpotError, match="speculative_min_edge_bps must be finite"):
        _settings(tmp_path, speculative_min_edge_bps=float("nan"))


def test_buy_quote_reserve_uses_dynamic_fee_and_floor(tmp_path: Path) -> None:
    settings = _settings(
        tmp_path,
        slippage_buffer_bps=25.0,
        leg_rounding_reserve_bps=5.0,
        min_leg_quote_reserve_bps=25.0,
    )

    assert claude_spot._buy_quote_reserve_bps(0.001, settings) == pytest.approx(40.0)
    assert claude_spot._buy_quote_reserve_bps(0.0001, settings) == pytest.approx(31.0)
    assert claude_spot._buy_quote_reserve_bps(
        0.0,
        _settings(
            tmp_path,
            slippage_buffer_bps=5.0,
            leg_rounding_reserve_bps=5.0,
            min_leg_quote_reserve_bps=25.0,
        ),
    ) == pytest.approx(25.0)


def test_evidence_only_cli_prints_context(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
) -> None:
    async def fake_payload(settings: ClaudeSpotSettings) -> dict[str, Any]:
        assert settings.api_key == "key"
        return {
            "candidate_routes": [],
            "trade_memory": {"status": "empty"},
        }

    monkeypatch.setenv("BINANCE_API_KEY", "key")
    monkeypatch.setenv("BINANCE_API_SECRET", "secret")
    monkeypatch.setattr(claude_spot, "_build_evidence_only_payload", fake_payload)

    assert claude_spot.run(["--evidence-only"]) == 0

    payload = json.loads(capsys.readouterr().out)
    assert payload["trade_memory"]["status"] == "empty"


@pytest.mark.asyncio
async def test_build_context_offers_dynamic_multi_hop_route(tmp_path: Path) -> None:
    context, _ = await build_decision_context(FakeClaudeSpotClient(), _settings(tmp_path))

    destinations = {candidate.route.destination_asset for candidate in context.candidate_routes}
    eden = next(
        candidate
        for candidate in context.candidate_routes
        if candidate.route.destination_asset == "EDEN"
    )

    assert "EDEN" in destinations
    assert len(eden.route.legs) == 2
    assert [leg.symbol for leg in eden.route.legs] == ["BNB/USDT", "EDEN/USDT"]
    assert context.mission == CLAUDE_MONEY_MAKER_MISSION
    assert context.portfolio_value_usdt > 0


@pytest.mark.asyncio
async def test_aggressive_mode_prefers_small_market_candidates(tmp_path: Path) -> None:
    context, _ = await build_decision_context(
        SmallMarketClient(),
        _settings(
            tmp_path,
            min_quote_volume_usdt=100_000.0,
            small_min_quote_volume_usdt=5_000.0,
            small_max_spread_bps=100.0,
            small_candidate_limit=1,
        ),
    )

    assert context.market_pick_mode == "small_market_aggressive"
    assert [candidate.route.destination_asset for candidate in context.candidate_routes] == [
        "MICRO"
    ]
    candidate = context.candidate_routes[0]
    assert candidate.liquidity_tier == "micro"
    assert candidate.quote_volume_usdt == pytest.approx(20_000.0)
    assert candidate.market_pick_score != 0
    assert "immediate_friction" in candidate.market_pick_reason


@pytest.mark.asyncio
async def test_friction_cap_removes_high_friction_candidates(tmp_path: Path) -> None:
    context, _ = await build_decision_context(
        FakeClaudeSpotClient(),
        _settings(
            tmp_path,
            allow_negative_friction_bps=1.0,
            speculative_mode_enabled=True,
            speculative_min_edge_bps=-1000.0,
            small_candidate_limit=10,
        ),
    )

    assert context.route_scope["max_immediate_friction_bps"] == 1.0
    assert context.route_scope["speculative_mode_enabled"] is True
    assert context.route_scope["market_evidence_short_timeframe"] == "1m"
    assert context.route_scope["market_evidence_short_windows"] == ["5m", "15m"]
    assert context.candidate_routes == ()


@pytest.mark.asyncio
async def test_market_evidence_packet_contains_compact_windows(tmp_path: Path) -> None:
    client = EvidenceClaudeSpotClient()
    context, _ = await build_decision_context(
        client,
        _settings(
            tmp_path,
            market_evidence_enabled=True,
            require_market_evidence_for_live=True,
            market_evidence_min_edge_bps=10.0,
        ),
    )

    eden = next(
        candidate
        for candidate in context.candidate_routes
        if candidate.route.destination_asset == "EDEN"
    )
    evidence = eden.market_evidence

    assert evidence["status"] == "ready"
    assert set(evidence["destination"]["returns_bps"]) == {"1h", "4h", "24h", "7d"}
    assert set(evidence["destination"]["short_returns_bps"]) == {"5m", "15m"}
    assert evidence["destination"]["returns_bps"]["7d"] > 0
    assert evidence["destination"]["short_returns_bps"]["15m"] > 0
    assert evidence["source"]["returns_bps"]["24h"] == pytest.approx(0.0)
    assert evidence["source"]["short_returns_bps"]["5m"] == pytest.approx(0.0)
    assert evidence["short_relative_momentum_bps"] > 0
    assert evidence["gate_pass"] is True
    assert evidence["expected_edge_after_friction_bps"] > evidence["minimum_expected_edge_bps"]
    assert evidence["speculative_mode_enabled"] is False
    assert evidence["speculative_min_edge_bps"] == pytest.approx(-1000.0)
    assert evidence["speculative_require_thesis"] is True
    assert ("BNB/USDT", "1m") in client.ohlcv_calls
    assert ("EDEN/USDT", "1m") in client.ohlcv_calls


def test_weighted_relative_momentum_uses_short_tape() -> None:
    source = {
        "returns_bps": {"1h": 0.0, "4h": 0.0, "24h": 0.0, "7d": 0.0},
        "short_returns_bps": {"5m": -200.0, "15m": -300.0},
    }
    destination = {
        "returns_bps": {"1h": 0.0, "4h": 0.0, "24h": 0.0, "7d": 0.0},
        "short_returns_bps": {"5m": 0.0, "15m": 0.0},
    }

    assert claude_spot._weighted_short_relative_momentum_bps(
        source,
        destination,
    ) == pytest.approx(260.0)
    assert claude_spot._weighted_relative_momentum_bps(
        source,
        destination,
    ) == pytest.approx(65.0)


@pytest.mark.asyncio
async def test_social_signal_evidence_is_added_to_candidate_context(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    social_path = tmp_path / "social.jsonl"
    write_social_audit(
        social_path,
        [
            SocialSignalRecord(
                source="telegram",
                message_id="m1",
                timestamp=datetime.now(timezone.utc).isoformat(),
                author_hash="author",
                channel_hash="channel",
                excerpt="EDEN breakout watch",
                urls=(),
                assets=("EDEN",),
                label=SOCIAL_LABEL_BULLISH,
                confidence=0.8,
                manipulation_risk=0.0,
            )
        ],
    )
    monkeypatch.setenv("SOCIAL_SIGNAL_ENABLED", "true")
    monkeypatch.setenv("SOCIAL_SIGNAL_AUDIT_PATH", str(social_path))

    context, _ = await build_decision_context(FakeClaudeSpotClient(), _settings(tmp_path))

    eden = next(
        candidate
        for candidate in context.candidate_routes
        if candidate.route.destination_asset == "EDEN"
    )
    assert eden.social_signal_evidence["mention_count"] == 1
    assert eden.social_signal_evidence["bullish_mentions"] == 1


@pytest.mark.asyncio
async def test_social_signal_manipulation_risk_removes_candidate(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    social_path = tmp_path / "social.jsonl"
    write_social_audit(
        social_path,
        [
            SocialSignalRecord(
                source="telegram",
                message_id="m1",
                timestamp=datetime.now(timezone.utc).isoformat(),
                author_hash="author",
                channel_hash="channel",
                excerpt="EDEN pump group 100x",
                urls=(),
                assets=("EDEN",),
                label=SOCIAL_LABEL_MANIPULATION,
                confidence=0.95,
                manipulation_risk=0.95,
            )
        ],
    )
    monkeypatch.setenv("SOCIAL_SIGNAL_ENABLED", "true")
    monkeypatch.setenv("SOCIAL_SIGNAL_AUDIT_PATH", str(social_path))
    monkeypatch.setenv("SOCIAL_SIGNAL_BLOCK_MANIPULATION_RISK", "true")

    context, _ = await build_decision_context(FakeClaudeSpotClient(), _settings(tmp_path))

    assert all(
        candidate.route.destination_asset != "EDEN"
        for candidate in context.candidate_routes
    )


@pytest.mark.asyncio
async def test_trade_memory_records_current_holding_pnl(tmp_path: Path) -> None:
    _write_audit_row(
        tmp_path,
        decision={
            "action": "rebalance",
            "route_id": "BNB->EDEN:test",
            "source_asset": "BNB",
            "destination_asset": "EDEN",
        },
        selected_route={
            "route_id": "BNB->EDEN:test",
            "source_asset": "BNB",
            "destination_asset": "EDEN",
            "source_value_usdt": 12.0,
            "estimated_output_usdt": 15.0,
            "immediate_friction_bps": 100.0,
        },
    )

    context, _ = await build_decision_context(
        HoldingMemoryClient(),
        _settings(tmp_path),
    )

    memory = context.trade_memory
    assert memory["status"] == "ready"
    assert memory["recent_trade_count"] == 1
    assert memory["current_holding"]["asset"] == "EDEN"
    assert memory["current_holding"]["unrealized_pnl_bps"] < 0
    assert memory["recent_loss_streak"] == 1


@pytest.mark.asyncio
async def test_blocked_quote_scope_removes_idr_route_legs(tmp_path: Path) -> None:
    context, markets = await build_decision_context(
        IdRRouteClient(),
        _settings(
            tmp_path,
            block_fiat_assets=False,
            blocked_quotes=("IDR",),
            small_min_quote_volume_usdt=0.0,
            small_candidate_limit=10,
        ),
    )

    offered_symbols = {
        leg.symbol
        for candidate in context.candidate_routes
        for leg in candidate.route.legs
    }

    assert context.route_scope["blocked_quotes"] == ["IDR"]
    assert all(market.quote != "IDR" for market in markets.values())
    assert "POL/IDR" not in offered_symbols
    assert "SUI/IDR" not in offered_symbols
    assert "USDT/IDR" not in offered_symbols


@pytest.mark.asyncio
async def test_default_scope_blocks_fiat_route_assets(tmp_path: Path) -> None:
    context, markets = await build_decision_context(
        IdRRouteClient(),
        _settings(
            tmp_path,
            blocked_quotes=(),
            small_min_quote_volume_usdt=0.0,
            small_candidate_limit=10,
        ),
    )

    offered_symbols = {
        leg.symbol
        for candidate in context.candidate_routes
        for leg in candidate.route.legs
    }

    assert context.route_scope["block_fiat_assets"] is True
    assert all(
        market.base not in claude_spot.CLAUDE_SPOT_FIAT_ASSETS
        and market.quote not in claude_spot.CLAUDE_SPOT_FIAT_ASSETS
        for market in markets.values()
    )
    assert not {
        "POL/IDR",
        "SUI/IDR",
        "USDT/IDR",
        "POL/TRY",
        "SUI/JPY",
        "USDT/EUR",
    }.intersection(offered_symbols)


def test_route_scope_rejects_blocked_quote_before_submission(tmp_path: Path) -> None:
    route = RouteQuote(
        source_asset="POL",
        destination_asset="SUI",
        input_amount=50.0,
        output_amount=3.0,
        slippage_bps=25.0,
        legs=(
            RouteLeg(
                symbol="POL/IDR",
                side="sell",
                source_asset="POL",
                destination_asset="IDR",
                input_amount=50.0,
                spent_amount=50.0,
                gross_output_amount=150_000.0,
                fee_rate=0.001,
                fee_output_amount=150.0,
                net_output_amount=149_850.0,
            ),
        ),
    )

    with pytest.raises(ClaudeSpotError, match="quote asset IDR"):
        claude_spot._assert_route_scope_allowed(
            route,
            _settings(tmp_path, blocked_quotes=("IDR",)),
        )


@pytest.mark.asyncio
async def test_liquid_mode_keeps_top_volume_candidate_path(tmp_path: Path) -> None:
    context, _ = await build_decision_context(
        SmallMarketClient(),
        _settings(
            tmp_path,
            market_pick_mode="liquid_top_value",
            min_quote_volume_usdt=5_000.0,
            candidate_limit=1,
        ),
    )

    assert context.market_pick_mode == "liquid_top_value"
    assert [candidate.route.destination_asset for candidate in context.candidate_routes] == [
        "USDT"
    ]


@pytest.mark.asyncio
async def test_plan_only_asks_claude_and_writes_audit_without_order(tmp_path: Path) -> None:
    client = FakeClaudeSpotClient()
    advisor = RouteChoosingAdvisor()
    alerter = FakeAlerter()

    result = await run_claude_spot_cycle(
        _settings(tmp_path),
        submit_order=False,
        client=client,
        advisor=advisor,
        alerter=alerter,
    )

    assert result.status == "planned"
    assert result.submission_attempted is False
    assert result.selected_route["destination_asset"] == "EDEN"
    assert client.orders == []
    assert alerter.messages
    payload = json.loads(Path(result.audit_path).read_text(encoding="utf-8").strip())
    assert payload["decision"]["reason"] == "ai_money_maker_route"
    assert payload["market_pick_mode"] == "small_market_aggressive"
    assert payload["candidate_routes"][0]["market_pick_score"] != 0


@pytest.mark.asyncio
async def test_live_cycle_holds_without_order_when_claude_times_out(tmp_path: Path) -> None:
    client = FakeClaudeSpotClient()

    result = await run_claude_spot_cycle(
        _settings(tmp_path),
        submit_order=True,
        client=client,
        advisor=TimeoutAdvisor(),
        alerter=FakeAlerter(),
    )

    assert result.status == "held"
    assert result.submission_attempted is False
    assert result.decision["action"] == "hold"
    assert "timed out" in result.error
    assert client.orders == []
    payload = json.loads(Path(result.audit_path).read_text(encoding="utf-8").strip())
    assert payload["status"] == "held"
    assert payload["submission_attempted"] is False


@pytest.mark.asyncio
async def test_live_cycle_holds_during_trade_cooldown(tmp_path: Path) -> None:
    _write_audit_row(
        tmp_path,
        timestamp=(datetime.now(timezone.utc) - timedelta(minutes=5)).isoformat(),
    )
    client = FakeClaudeSpotClient()

    result = await run_claude_spot_cycle(
        _settings(tmp_path, min_trade_interval_seconds=3600.0),
        submit_order=True,
        client=client,
        advisor=NoCallAdvisor(),
        alerter=FakeAlerter(),
    )

    assert result.status == "held"
    assert result.submission_attempted is False
    assert "trade cooldown active" in result.decision["reason"]
    assert client.orders == []


@pytest.mark.asyncio
async def test_live_cycle_holds_after_daily_trade_cap(tmp_path: Path) -> None:
    _write_audit_row(tmp_path, decision={"route_id": "one"})
    _write_audit_row(tmp_path, decision={"route_id": "two"})
    client = FakeClaudeSpotClient()

    result = await run_claude_spot_cycle(
        _settings(
            tmp_path,
            min_trade_interval_seconds=0.0,
            max_submitted_trades_per_day=2,
        ),
        submit_order=True,
        client=client,
        advisor=NoCallAdvisor(),
        alerter=FakeAlerter(),
    )

    assert result.status == "held"
    assert result.submission_attempted is False
    assert "daily submitted-trade cap reached" in result.decision["reason"]
    assert client.orders == []


@pytest.mark.asyncio
async def test_live_cycle_ignores_fixed_trade_limits_when_disabled(tmp_path: Path) -> None:
    _write_audit_row(
        tmp_path,
        timestamp=(datetime.now(timezone.utc) - timedelta(minutes=1)).isoformat(),
        decision={"route_id": "one"},
    )
    _write_audit_row(
        tmp_path,
        timestamp=datetime.now(timezone.utc).isoformat(),
        decision={"route_id": "two"},
    )
    client = FakeClaudeSpotClient()
    advisor = RouteChoosingAdvisor()

    result = await run_claude_spot_cycle(
        _settings(
            tmp_path,
            min_trade_interval_seconds=0.0,
            max_submitted_trades_per_day=0,
        ),
        submit_order=True,
        client=client,
        advisor=advisor,
        alerter=FakeAlerter(),
    )

    assert result.status == "submitted"
    assert result.submission_attempted is True
    assert len(advisor.contexts) == 1
    assert [order[0] for order in client.orders] == ["BNB/USDT", "EDEN/USDT"]


@pytest.mark.asyncio
async def test_live_cycle_holds_after_failed_submitted_route(tmp_path: Path) -> None:
    _write_audit_row(
        tmp_path,
        status="failed",
        error="route halted",
        submitted_legs=[{"symbol": "TRUMP/FDUSD"}],
    )
    client = FakeClaudeSpotClient()

    result = await run_claude_spot_cycle(
        _settings(
            tmp_path,
            min_trade_interval_seconds=0.0,
            max_submitted_trades_per_day=0,
            halt_after_failed_submission=True,
        ),
        submit_order=True,
        client=client,
        advisor=NoCallAdvisor(),
        alerter=FakeAlerter(),
    )

    assert result.status == "held"
    assert result.submission_attempted is False
    assert "previous live route failed" in result.decision["reason"]
    assert client.orders == []


@pytest.mark.asyncio
async def test_operator_review_marker_clears_failed_submission_hold(
    tmp_path: Path,
) -> None:
    _write_audit_row(
        tmp_path,
        status="failed",
        error="route halted",
        submitted_legs=[{"symbol": "BONK/USDT"}],
    )
    settings = _settings(tmp_path)

    audit_path = claude_spot.mark_failed_route_reviewed(
        settings,
        reason="operator reviewed partial route and approved re-enable",
    )

    assert audit_path == tmp_path / "claude-spot.jsonl"
    client = FakeClaudeSpotClient()
    result = await run_claude_spot_cycle(
        settings,
        submit_order=True,
        client=client,
        advisor=RouteChoosingAdvisor(),
        alerter=FakeAlerter(),
    )

    assert result.status == "submitted"
    assert [order[0] for order in client.orders] == ["BNB/USDT", "EDEN/USDT"]
    rows = [
        json.loads(line)
        for line in (tmp_path / "claude-spot.jsonl").read_text(encoding="utf-8").splitlines()
    ]
    assert rows[1]["status"] == "reviewed"
    assert rows[1]["operator_review"]["failed_submission_reviewed"] is True
    assert rows[-1]["status"] == "submitted"


@pytest.mark.asyncio
async def test_live_cycle_holds_when_market_evidence_missing(tmp_path: Path) -> None:
    client = MissingEvidenceClient()

    result = await run_claude_spot_cycle(
        _settings(
            tmp_path,
            market_evidence_enabled=True,
            require_market_evidence_for_live=True,
            market_evidence_min_edge_bps=10.0,
        ),
        submit_order=True,
        client=client,
        advisor=RouteChoosingAdvisor(),
        alerter=FakeAlerter(),
    )

    assert result.status == "held"
    assert result.submission_attempted is False
    assert "market evidence is insufficient" in result.decision["reason"]
    assert result.selected_route["market_evidence"]["status"] == "insufficient"
    assert client.orders == []


@pytest.mark.asyncio
async def test_live_cycle_holds_when_market_edge_is_too_low(tmp_path: Path) -> None:
    client = EvidenceClaudeSpotClient(eden_start=1.0, eden_end=1.0)

    result = await run_claude_spot_cycle(
        _settings(
            tmp_path,
            market_evidence_enabled=True,
            require_market_evidence_for_live=True,
            market_evidence_min_edge_bps=50.0,
        ),
        submit_order=True,
        client=client,
        advisor=RouteChoosingAdvisor(),
        alerter=FakeAlerter(),
    )

    assert result.status == "held"
    assert result.submission_attempted is False
    assert "market evidence gate rejected" in result.decision["reason"]
    assert client.orders == []


def test_speculative_gate_allows_negative_edge_with_thesis(tmp_path: Path) -> None:
    candidate = _candidate_with_evidence(-900.0)
    decision = _rebalance_decision()
    settings = _settings(
        tmp_path,
        market_evidence_enabled=True,
        require_market_evidence_for_live=True,
        market_evidence_min_edge_bps=50.0,
        speculative_mode_enabled=True,
        speculative_min_edge_bps=-1000.0,
        speculative_require_thesis=True,
    )

    reason = claude_spot._market_evidence_hold_reason(
        settings,
        _decision_context_for_candidate(candidate),
        candidate,
        decision,
    )
    selected = claude_spot._selected_route_context(candidate, settings, decision)

    assert reason is None
    assert selected["speculative_gate"]["allowed"] is True
    assert selected["speculative_gate"]["thesis_present"] is True


def test_speculative_gate_blocks_below_floor(tmp_path: Path) -> None:
    candidate = _candidate_with_evidence(-1001.0)
    decision = _rebalance_decision()
    settings = _settings(
        tmp_path,
        market_evidence_enabled=True,
        require_market_evidence_for_live=True,
        market_evidence_min_edge_bps=50.0,
        speculative_mode_enabled=True,
        speculative_min_edge_bps=-1000.0,
        speculative_require_thesis=True,
    )

    reason = claude_spot._market_evidence_hold_reason(
        settings,
        _decision_context_for_candidate(candidate),
        candidate,
        decision,
    )

    assert reason is not None
    assert "speculative gate rejected" in reason
    assert "below speculative floor" in reason


def test_speculative_gate_requires_thesis(tmp_path: Path) -> None:
    candidate = _candidate_with_evidence(-900.0)
    decision = _rebalance_decision(reason="try this route")
    settings = _settings(
        tmp_path,
        market_evidence_enabled=True,
        require_market_evidence_for_live=True,
        market_evidence_min_edge_bps=50.0,
        speculative_mode_enabled=True,
        speculative_min_edge_bps=-1000.0,
        speculative_require_thesis=True,
    )

    reason = claude_spot._market_evidence_hold_reason(
        settings,
        _decision_context_for_candidate(candidate),
        candidate,
        decision,
    )

    assert reason is not None
    assert "speculative thesis missing" in reason


@pytest.mark.asyncio
async def test_live_cycle_submits_speculative_negative_edge_when_enabled(
    tmp_path: Path,
) -> None:
    client = EvidenceClaudeSpotClient(eden_start=1.0, eden_end=1.0)
    alerter = FakeAlerter()

    result = await run_claude_spot_cycle(
        _settings(
            tmp_path,
            market_evidence_enabled=True,
            require_market_evidence_for_live=True,
            market_evidence_min_edge_bps=50.0,
            speculative_mode_enabled=True,
            speculative_min_edge_bps=-1000.0,
            speculative_require_thesis=True,
        ),
        submit_order=True,
        client=client,
        advisor=RouteChoosingAdvisor(
            reason="speculative reversal thesis after oversold capitulation",
        ),
        alerter=alerter,
    )

    assert result.status == "submitted"
    assert result.submission_attempted is True
    assert result.selected_route["market_evidence"]["gate_pass"] is False
    assert result.selected_route["speculative_gate"]["allowed"] is True
    assert result.selected_route["speculative_gate"]["thesis_present"] is True
    assert "Speculative:" in alerter.messages[0]
    assert [order[0] for order in client.orders] == ["BNB/USDT", "EDEN/USDT"]


@pytest.mark.asyncio
async def test_live_cycle_executes_full_multi_hop_route_serially(tmp_path: Path) -> None:
    client = FakeClaudeSpotClient()
    result = await run_claude_spot_cycle(
        _settings(tmp_path),
        submit_order=True,
        client=client,
        advisor=RouteChoosingAdvisor(),
        alerter=FakeAlerter(),
    )

    assert result.status == "submitted"
    assert result.submission_attempted is True
    assert [order[0] for order in client.orders] == ["BNB/USDT", "EDEN/USDT"]
    assert len(result.submitted_legs) == 2
    assert result.submitted_legs[0]["status"] == "closed"
    assert result.submitted_legs[1]["destination_asset"] == "EDEN"
    assert len(client.order_tests) == 3


@pytest.mark.asyncio
async def test_live_buy_leg_reserves_dynamic_fee_slippage_and_rounding(
    tmp_path: Path,
) -> None:
    client = PriceMovedBuyClient()

    result = await run_claude_spot_cycle(
        _settings(
            tmp_path,
            slippage_buffer_bps=25.0,
            leg_rounding_reserve_bps=5.0,
            min_leg_quote_reserve_bps=25.0,
        ),
        submit_order=True,
        client=client,
        advisor=RouteChoosingAdvisor(),
        alerter=FakeAlerter(),
    )

    assert result.status == "submitted"
    assert [order[0] for order in client.orders] == ["BNB/USDT", "EDEN/USDT"]
    assert client.buy_costs[0] <= client.quote_before_buy[0]
    assert client.orders[1][3] < 11.9


@pytest.mark.asyncio
async def test_live_cycle_submits_when_strict_market_evidence_passes(tmp_path: Path) -> None:
    client = EvidenceClaudeSpotClient()

    result = await run_claude_spot_cycle(
        _settings(
            tmp_path,
            market_evidence_enabled=True,
            require_market_evidence_for_live=True,
            market_evidence_min_edge_bps=10.0,
        ),
        submit_order=True,
        client=client,
        advisor=RouteChoosingAdvisor(),
        alerter=FakeAlerter(),
    )

    assert result.status == "submitted"
    assert result.submission_attempted is True
    assert result.selected_route["market_evidence"]["gate_pass"] is True
    assert [order[0] for order in client.orders] == ["BNB/USDT", "EDEN/USDT"]


def test_claude_order_amount_uses_lot_step_over_precision() -> None:
    market = MarketSnapshot(
        symbol="HBAR/BNB",
        base="HBAR",
        quote="BNB",
        bid=0.000132,
        ask=0.0001325,
        last=0.0001325,
        quote_volume=2.5,
        quote_volume_usdt=20_000.0,
        min_amount=1.0,
        min_notional=0.000001,
        amount_precision=0.1,
        timestamp_ms=1,
        spread_bps=3.0,
        eligible=True,
        amount_step=1.0,
    )

    amount = claude_spot._leg_order_amount(
        market,
        "buy",
        0.017013,
        slippage_bps=0.0,
    )

    assert amount == pytest.approx(128.0)


@pytest.mark.asyncio
async def test_live_cycle_requires_explicit_dynamic_scope(tmp_path: Path) -> None:
    with pytest.raises(ClaudeSpotError, match="all_eligible_spot"):
        await run_claude_spot_cycle(
            _settings(tmp_path, dynamic_scope=""),
            submit_order=True,
            client=FakeClaudeSpotClient(),
            advisor=RouteChoosingAdvisor(),
            alerter=FakeAlerter(),
        )


@pytest.mark.asyncio
async def test_live_cycle_halts_after_unclosed_leg(tmp_path: Path) -> None:
    client = FakeClaudeSpotClient(final_status="open")

    with pytest.raises(ClaudeSpotError, match="route halted"):
        await run_claude_spot_cycle(
            _settings(tmp_path),
            submit_order=True,
            client=client,
            advisor=RouteChoosingAdvisor(),
            alerter=FakeAlerter(),
        )

    assert [order[0] for order in client.orders] == ["BNB/USDT"]
    audit_lines = (tmp_path / "claude-spot.jsonl").read_text(
        encoding="utf-8",
    ).splitlines()
    assert len(audit_lines) == 1
    payload = json.loads(audit_lines[0])
    assert payload["status"] == "failed"
    assert payload["submission_attempted"] is True
    assert payload["submitted_legs"][0]["symbol"] == "BNB/USDT"


@pytest.mark.asyncio
async def test_failed_second_leg_writes_one_audit_with_submitted_leg(
    tmp_path: Path,
) -> None:
    client = FailingSecondLegClient()

    with pytest.raises(ClaudeSpotError, match="route halted"):
        await run_claude_spot_cycle(
            _settings(tmp_path),
            submit_order=True,
            client=client,
            advisor=RouteChoosingAdvisor(),
            alerter=FakeAlerter(),
        )

    lines = (tmp_path / "claude-spot.jsonl").read_text(encoding="utf-8").splitlines()
    assert len(lines) == 1
    payload = json.loads(lines[0])
    assert payload["status"] == "failed"
    assert payload["submission_attempted"] is True
    assert len(payload["submitted_legs"]) == 1
    assert payload["submitted_legs"][0]["symbol"] == "BNB/USDT"


@pytest.mark.asyncio
async def test_second_leg_commission_failure_preserves_submitted_leg(
    tmp_path: Path,
) -> None:
    client = ExecutionCommissionFailureClient()

    with pytest.raises(ClaudeSpotError, match="preflight failed"):
        await run_claude_spot_cycle(
            _settings(tmp_path),
            submit_order=True,
            client=client,
            advisor=RouteChoosingAdvisor(),
            alerter=FakeAlerter(),
        )

    lines = (tmp_path / "claude-spot.jsonl").read_text(encoding="utf-8").splitlines()
    assert len(lines) == 1
    payload = json.loads(lines[0])
    assert payload["status"] == "failed"
    assert payload["submission_attempted"] is True
    assert len(payload["submitted_legs"]) == 1
    assert payload["submitted_legs"][0]["symbol"] == "BNB/USDT"
