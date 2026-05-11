"""Unit tests for the Binance trading bot risk guard."""

from __future__ import annotations

import asyncio
import importlib
import sys
import types
from pathlib import Path
from typing import Any
from unittest.mock import Mock

import pytest
from pydantic import BaseModel


PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))


class FakeLoguruLogger:
    """Small loguru-compatible logger that records bound log events."""

    def __init__(
        self,
        *,
        events: list[dict[str, Any]] | None = None,
        bind_calls: list[dict[str, Any]] | None = None,
        context: dict[str, Any] | None = None,
    ) -> None:
        self.events = events if events is not None else []
        self.bind_calls = bind_calls if bind_calls is not None else []
        self.context = context or {}
        self.info = Mock(side_effect=self._record_info)
        self.warning = Mock(side_effect=self._record_warning)
        self.error = Mock(side_effect=self._record_error)
        self.critical = Mock(side_effect=self._record_critical)
        self.debug = Mock(side_effect=self._record_debug)

    def bind(self, **context: Any) -> "FakeLoguruLogger":
        bound_context = {**self.context, **context}
        self.bind_calls.append(bound_context)
        return FakeLoguruLogger(
            events=self.events,
            bind_calls=self.bind_calls,
            context=bound_context,
        )

    def opt(self, **_: Any) -> "FakeLoguruLogger":
        return self

    def _record_info(self, message: str, *_: Any, **__: Any) -> None:
        self._record("info", message)

    def _record_warning(self, message: str, *_: Any, **__: Any) -> None:
        self._record("warning", message)

    def _record_error(self, message: str, *_: Any, **__: Any) -> None:
        self._record("error", message)

    def _record_critical(self, message: str, *_: Any, **__: Any) -> None:
        self._record("critical", message)

    def _record_debug(self, message: str, *_: Any, **__: Any) -> None:
        self._record("debug", message)

    def _record(self, level: str, message: str) -> None:
        self.events.append(
            {
                "level": level,
                "message": message,
                "context": dict(self.context),
            }
        )


@pytest.fixture()
def risk_module(monkeypatch: pytest.MonkeyPatch) -> tuple[Any, FakeLoguruLogger]:
    """Import the risk guard with loguru replaced by a recording mock."""
    fake_logger = FakeLoguruLogger()
    loguru = types.ModuleType("loguru")
    loguru.logger = fake_logger
    monkeypatch.setitem(sys.modules, "loguru", loguru)

    sys.modules.pop("src.risk.guard", None)
    module = importlib.import_module("src.risk.guard")
    yield module, fake_logger
    sys.modules.pop("src.risk.guard", None)


def _base_config(module: Any) -> Any:
    return module.RiskConfig(
        max_position_size_usd=1_000.0,
        daily_loss_limit_usd=1_000.0,
        duplicate_window_seconds=60.0,
        max_total_exposure_usd=5_000.0,
        daily_drawdown_limit_pct=5.0,
        min_balance_usdt=100.0,
        allowed_leverage=5.0,
        max_open_orders=5,
        max_price_deviation_pct=2.0,
    )


def _build_structured_context(
    module: Any,
    *,
    config_overrides: dict[str, Any] | None = None,
    order_overrides: dict[str, Any] | None = None,
    portfolio_overrides: dict[str, Any] | None = None,
    market_overrides: dict[str, Any] | None = None,
) -> tuple[Any, Any, Any, Any]:
    config_values = _model_dump(_base_config(module))
    config_values.update(config_overrides or {})
    order_values = {
        "symbol": "BTC/USDT",
        "side": "buy",
        "amount": 0.01,
        "price": 50_000.0,
        "leverage": 5.0,
        "order_type": "limit",
    }
    order_values.update(order_overrides or {})
    portfolio_values = {
        "total_equity_usdt": 10_000.0,
        "available_balance_usdt": 500.0,
        "realized_pnl_today_usdt": -100.0,
        "unrealized_pnl_today_usdt": -100.0,
        "positions": {"ETH/USDT": 500.0},
        "total_exposure_usdt": 500.0,
        "open_orders_count": 1,
    }
    portfolio_values.update(portfolio_overrides or {})
    market_values = {
        "symbol": order_values["symbol"],
        "mark_price": 50_000.0,
    }
    market_values.update(market_overrides or {})

    return (
        module.RiskGuard(module.RiskConfig(**config_values)),
        module.OrderRequest(**order_values),
        module.PortfolioState(**portfolio_values),
        module.MarketData(**market_values),
    )


def _model_dump(model: BaseModel) -> dict[str, Any]:
    if hasattr(model, "model_dump"):
        return model.model_dump()
    return model.dict()


def _run(coro: Any) -> Any:
    return asyncio.run(coro)


def _assert_logged(
    fake_logger: FakeLoguruLogger,
    *,
    level: str,
    message: str,
) -> dict[str, Any]:
    for event in fake_logger.events:
        if event["level"] == level and event["message"] == message:
            return event
    raise AssertionError(f"{level!r} log message {message!r} was not emitted")


def _assert_not_logged(
    fake_logger: FakeLoguruLogger,
    *,
    level: str,
    message: str,
) -> None:
    assert not any(
        event["level"] == level and event["message"] == message
        for event in fake_logger.events
    )


@pytest.mark.parametrize(
    (
        "config_overrides",
        "order_overrides",
        "portfolio_overrides",
        "market_overrides",
        "expected_error",
        "expected_violation_log",
        "expected_violation_level",
    ),
    [
        pytest.param(
            {},
            {},
            {},
            {},
            None,
            None,
            None,
            id="happy_path_order_allowed",
        ),
        pytest.param(
            {},
            {},
            {"positions": {"BTC/USDT": 500.0}, "total_exposure_usdt": 500.0},
            {},
            None,
            None,
            None,
            id="max_position_size_exact_limit_allowed",
        ),
        pytest.param(
            {},
            {},
            {"positions": {"BTC/USDT": 500.01}, "total_exposure_usdt": 500.01},
            {},
            "maximum position size breached",
            "Maximum position size breached",
            "error",
            id="max_position_size_exceeded_rejected",
        ),
        pytest.param(
            {},
            {"side": "sell", "amount": 0.005},
            {"positions": {"BTC/USDT": 750.0}, "total_exposure_usdt": 750.0},
            {},
            None,
            None,
            None,
            id="sell_reduces_long_position_allowed",
        ),
        pytest.param(
            {},
            {"side": "sell", "amount": 0.005},
            {"positions": {"BTC/USDT": -750.01}, "total_exposure_usdt": 750.01},
            {},
            "maximum position size breached",
            "Maximum position size breached",
            "error",
            id="sell_increases_short_exposure_rejected",
        ),
        pytest.param(
            {},
            {},
            {"positions": {"ETH/USDT": 4_500.0}, "total_exposure_usdt": 4_500.0},
            {},
            None,
            None,
            None,
            id="total_exposure_exact_limit_allowed",
        ),
        pytest.param(
            {},
            {},
            {"positions": {"ETH/USDT": 4_500.01}, "total_exposure_usdt": 4_500.01},
            {},
            "maximum total exposure breached",
            "Maximum total exposure breached",
            "error",
            id="total_exposure_exceeded_rejected",
        ),
        pytest.param(
            {},
            {},
            {
                "realized_pnl_today_usdt": -250.0,
                "unrealized_pnl_today_usdt": -250.0,
            },
            {},
            None,
            None,
            None,
            id="daily_drawdown_exact_limit_allowed",
        ),
        pytest.param(
            {"daily_drawdown_limit_pct": None},
            {},
            {
                "realized_pnl_today_usdt": -1_000.0,
                "unrealized_pnl_today_usdt": -1_000.0,
            },
            {},
            None,
            None,
            None,
            id="daily_drawdown_guard_disabled_allows_large_loss",
        ),
        pytest.param(
            {},
            {},
            {
                "realized_pnl_today_usdt": 600.0,
                "unrealized_pnl_today_usdt": -200.0,
            },
            {},
            None,
            None,
            None,
            id="positive_daily_pnl_has_no_drawdown",
        ),
        pytest.param(
            {},
            {},
            {
                "realized_pnl_today_usdt": -250.01,
                "unrealized_pnl_today_usdt": -250.0,
            },
            {},
            "daily drawdown breached",
            "Daily drawdown breached",
            "critical",
            id="daily_drawdown_exceeded_rejected",
        ),
        pytest.param(
            {},
            {},
            {"available_balance_usdt": 200.0},
            {},
            None,
            None,
            None,
            id="minimum_balance_exact_buffer_allowed",
        ),
        pytest.param(
            {},
            {},
            {"available_balance_usdt": 199.99},
            {},
            "minimum balance breached",
            "Minimum balance breached",
            "error",
            id="minimum_balance_buffer_breached_rejected",
        ),
        pytest.param(
            {},
            {"leverage": 5.0},
            {},
            {},
            None,
            None,
            None,
            id="leverage_exact_match_allowed",
        ),
        pytest.param(
            {},
            {"leverage": 10.0},
            {},
            {},
            "leverage check breached",
            "Leverage check breached",
            "error",
            id="leverage_mismatch_rejected",
        ),
        pytest.param(
            {},
            {},
            {"open_orders_count": 4},
            {},
            None,
            None,
            None,
            id="max_open_orders_exact_result_allowed",
        ),
        pytest.param(
            {},
            {},
            {"open_orders_count": 5},
            {},
            "maximum open orders breached",
            "Maximum open orders breached",
            "error",
            id="max_open_orders_exceeded_rejected",
        ),
        pytest.param(
            {},
            {"price": 51_000.0},
            {},
            {"mark_price": 50_000.0},
            None,
            None,
            None,
            id="price_deviation_exact_limit_allowed",
        ),
        pytest.param(
            {},
            {"price": 51_001.0},
            {},
            {"mark_price": 50_000.0},
            "price deviation breached",
            "Price deviation breached",
            "error",
            id="price_deviation_exceeded_rejected",
        ),
    ],
)
def test_structured_risk_guard_conditions_are_table_driven(
    risk_module: tuple[Any, FakeLoguruLogger],
    config_overrides: dict[str, Any],
    order_overrides: dict[str, Any],
    portfolio_overrides: dict[str, Any],
    market_overrides: dict[str, Any],
    expected_error: str | None,
    expected_violation_log: str | None,
    expected_violation_level: str | None,
) -> None:
    module, fake_logger = risk_module
    guard, order, portfolio, market_data = _build_structured_context(
        module,
        config_overrides=config_overrides,
        order_overrides=order_overrides,
        portfolio_overrides=portfolio_overrides,
        market_overrides=market_overrides,
    )

    if expected_error is None:
        _run(guard.validate_order(order, portfolio, market_data))

        _assert_logged(fake_logger, level="info", message="Risk validation passed")
        _assert_not_logged(
            fake_logger,
            level="error",
            message="Risk validation failed",
        )
        return

    with pytest.raises(module.RiskBreachError, match=expected_error) as exc_info:
        _run(guard.validate_order(order, portfolio, market_data))

    assert exc_info.value.context
    _assert_logged(
        fake_logger,
        level=expected_violation_level,
        message=expected_violation_log,
    )
    failure_log = _assert_logged(
        fake_logger,
        level="error",
        message="Risk validation failed",
    )
    assert failure_log["context"]["risk_context"] == exc_info.value.context


@pytest.mark.parametrize(
    (
        "config_overrides",
        "order_overrides",
        "portfolio_overrides",
        "market_overrides",
    ),
    [
        pytest.param(
            {},
            {"amount": 0.005},
            {"positions": {"BTC/USDT": 250.0}, "total_exposure_usdt": 250.0},
            {},
            id="keyword_structured_validation_path",
        )
    ],
)
def test_structured_validation_accepts_keyword_arguments(
    risk_module: tuple[Any, FakeLoguruLogger],
    config_overrides: dict[str, Any],
    order_overrides: dict[str, Any],
    portfolio_overrides: dict[str, Any],
    market_overrides: dict[str, Any],
) -> None:
    module, fake_logger = risk_module
    guard, order, portfolio, market_data = _build_structured_context(
        module,
        config_overrides=config_overrides,
        order_overrides=order_overrides,
        portfolio_overrides=portfolio_overrides,
        market_overrides=market_overrides,
    )

    _run(
        guard.validate_order(
            order=order,
            portfolio=portfolio,
            market_data=market_data,
        )
    )

    _assert_logged(fake_logger, level="info", message="Risk validation passed")


@pytest.mark.parametrize(
    (
        "setup_duplicate",
        "expire_duplicate",
        "expected_error",
        "expected_violation_log",
    ),
    [
        pytest.param(
            False,
            False,
            None,
            None,
            id="legacy_duplicate_window_first_order_allowed",
        ),
        pytest.param(
            True,
            False,
            "duplicate order breached",
            "Duplicate order breached",
            id="legacy_duplicate_within_window_rejected",
        ),
        pytest.param(
            True,
            True,
            None,
            None,
            id="legacy_duplicate_after_window_allowed",
        ),
    ],
)
def test_legacy_duplicate_order_window_is_enforced(
    risk_module: tuple[Any, FakeLoguruLogger],
    setup_duplicate: bool,
    expire_duplicate: bool,
    expected_error: str | None,
    expected_violation_log: str | None,
) -> None:
    module, fake_logger = risk_module
    config = module.RiskConfig(
        max_position_size_usd=1_000.0,
        daily_loss_limit_usd=1_000.0,
        duplicate_window_seconds=10.0,
    )
    guard = module.RiskGuard(config)

    if setup_duplicate:
        _run(
            guard.validate_order(
                symbol="BTC/USDT",
                side="buy",
                amount=0.01,
                price=50_000.0,
                current_position=0.0,
            )
        )
    if expire_duplicate:
        stale_timestamp = (
            module.time.monotonic() - config.duplicate_window_seconds - 1.0
        )
        duplicate_key = ("BTC/USDT", "buy", 0.01)
        with guard._lock:
            guard._duplicate_orders[duplicate_key] = stale_timestamp
            guard._duplicate_order_events.clear()
            guard._duplicate_order_events.append((stale_timestamp, duplicate_key))

    if expected_error is None:
        _run(
            guard.validate_order(
                symbol="BTC/USDT",
                side="buy",
                amount=0.01,
                price=50_000.0,
                current_position=0.0,
            )
        )
        _assert_logged(fake_logger, level="info", message="Risk validation passed")
        return

    with pytest.raises(module.RiskBreachError, match=expected_error):
        _run(
            guard.validate_order(
                symbol="BTC/USDT",
                side="buy",
                amount=0.01,
                price=50_000.0,
                current_position=0.0,
            )
        )
    _assert_logged(fake_logger, level="error", message=expected_violation_log)
    _assert_logged(fake_logger, level="error", message="Risk validation failed")


@pytest.mark.parametrize(
    (
        "daily_loss_increment",
        "order_kwargs",
        "expected_error",
        "expected_violation_log",
        "expected_violation_level",
    ),
    [
        pytest.param(
            0.0,
            {
                "symbol": "BTC/USDT",
                "side": "buy",
                "amount": 0.02,
                "price": 50_000.0,
                "current_position": 0.0,
            },
            None,
            None,
            None,
            id="legacy_max_position_exact_limit_allowed",
        ),
        pytest.param(
            0.0,
            {
                "symbol": "BTC/USDT",
                "side": "sell",
                "amount": 0.01,
                "price": 50_000.0,
                "current_position": 0.02,
            },
            None,
            None,
            None,
            id="legacy_sell_reducing_position_allowed",
        ),
        pytest.param(
            0.0,
            {
                "symbol": "BTC/USDT",
                "side": "sell",
                "amount": 0.01,
                "price": 50_000.0,
                "current_position": -0.01,
            },
            None,
            None,
            None,
            id="legacy_short_exact_limit_allowed",
        ),
        pytest.param(
            0.0,
            {
                "symbol": "BTC/USDT",
                "side": "sell",
                "amount": 0.01,
                "price": 50_000.0,
                "current_position": -0.02,
            },
            "maximum position size breached",
            "Maximum position size breached",
            "error",
            id="legacy_short_increase_exposure_rejected",
        ),
        pytest.param(
            0.0,
            {
                "symbol": "BTC/USDT",
                "side": "buy",
                "amount": 0.020001,
                "price": 50_000.0,
                "current_position": 0.0,
            },
            "maximum position size breached",
            "Maximum position size breached",
            "error",
            id="legacy_max_position_exceeded_rejected",
        ),
        pytest.param(
            999.99,
            {
                "symbol": "ETH/USDT",
                "side": "buy",
                "amount": 0.02,
                "price": 1_000.0,
                "current_position": 0.0,
            },
            None,
            None,
            None,
            id="legacy_daily_loss_below_limit_allowed",
        ),
        pytest.param(
            1_000.0,
            {
                "symbol": "ETH/USDT",
                "side": "buy",
                "amount": 0.02,
                "price": 1_000.0,
                "current_position": 0.0,
            },
            "daily loss limit breached",
            "Daily loss limit breached",
            "critical",
            id="legacy_daily_loss_exact_limit_rejected",
        ),
        pytest.param(
            1_000.01,
            {
                "symbol": "ETH/USDT",
                "side": "buy",
                "amount": 0.02,
                "price": 1_000.0,
                "current_position": 0.0,
            },
            "daily loss limit breached",
            "Daily loss limit breached",
            "critical",
            id="legacy_daily_loss_above_limit_rejected",
        ),
    ],
)
def test_legacy_position_and_daily_loss_guards_are_preserved(
    risk_module: tuple[Any, FakeLoguruLogger],
    daily_loss_increment: float,
    order_kwargs: dict[str, Any],
    expected_error: str | None,
    expected_violation_log: str | None,
    expected_violation_level: str | None,
) -> None:
    module, fake_logger = risk_module
    guard = module.RiskGuard(
        module.RiskConfig(
            max_position_size_usd=1_000.0,
            daily_loss_limit_usd=1_000.0,
            duplicate_window_seconds=60.0,
        )
    )
    if daily_loss_increment:
        guard.update_realized_pnl(-daily_loss_increment)

    if expected_error is None:
        _run(guard.validate_order(**order_kwargs))
        _assert_logged(fake_logger, level="info", message="Risk validation passed")
        return

    with pytest.raises(module.RiskBreachError, match=expected_error):
        _run(guard.validate_order(**order_kwargs))
    _assert_logged(
        fake_logger,
        level=expected_violation_level,
        message=expected_violation_log,
    )
    _assert_logged(fake_logger, level="error", message="Risk validation failed")


@pytest.mark.parametrize(
    ("order_kwargs", "expected_error"),
    [
        pytest.param(
            {
                "symbol": " ",
                "side": "buy",
                "amount": 0.01,
                "price": 50_000.0,
                "current_position": 0.0,
            },
            "symbol must not be blank",
            id="blank_symbol_rejected",
        ),
        pytest.param(
            {
                "symbol": "BTC/USDT",
                "side": "hold",
                "amount": 0.01,
                "price": 50_000.0,
                "current_position": 0.0,
            },
            "side must be 'buy' or 'sell'",
            id="invalid_side_rejected",
        ),
        pytest.param(
            {
                "symbol": "BTC/USDT",
                "side": "buy",
                "amount": 0.0,
                "price": 50_000.0,
                "current_position": 0.0,
            },
            "amount must be a finite value greater than zero",
            id="zero_amount_rejected",
        ),
        pytest.param(
            {
                "symbol": "BTC/USDT",
                "side": "buy",
                "amount": 0.01,
                "price": 0.0,
                "current_position": 0.0,
            },
            "price must be a finite value greater than zero",
            id="zero_price_rejected",
        ),
        pytest.param(
            {
                "symbol": "BTC/USDT",
                "side": "buy",
                "amount": 0.01,
                "price": 50_000.0,
                "current_position": float("nan"),
            },
            "current_position must be a finite number",
            id="non_finite_current_position_rejected",
        ),
    ],
)
def test_legacy_order_input_validation_rejects_invalid_requests(
    risk_module: tuple[Any, FakeLoguruLogger],
    order_kwargs: dict[str, Any],
    expected_error: str,
) -> None:
    module, fake_logger = risk_module
    guard = module.RiskGuard(
        module.RiskConfig(
            max_position_size_usd=1_000.0,
            daily_loss_limit_usd=1_000.0,
            duplicate_window_seconds=60.0,
        )
    )

    with pytest.raises(module.RiskBreachError, match=expected_error):
        _run(guard.validate_order(**order_kwargs))

    _assert_logged(fake_logger, level="error", message="Risk validation failed")


@pytest.mark.parametrize(
    ("pnl_values", "expected_daily_loss", "expected_final_log_level"),
    [
        pytest.param([250.0], 0.0, "info", id="profits_do_not_reduce_daily_loss"),
        pytest.param([-250.0, 100.0], 250.0, "info", id="losses_accumulate_only"),
        pytest.param([-1_000.0], 1_000.0, "warning", id="limit_loss_logs_warning"),
    ],
)
def test_realized_pnl_updates_daily_loss_counter(
    risk_module: tuple[Any, FakeLoguruLogger],
    pnl_values: list[float],
    expected_daily_loss: float,
    expected_final_log_level: str,
) -> None:
    module, fake_logger = risk_module
    guard = module.RiskGuard(
        module.RiskConfig(
            max_position_size_usd=1_000.0,
            daily_loss_limit_usd=1_000.0,
        )
    )

    for pnl in pnl_values:
        guard.update_realized_pnl(pnl)

    assert guard.current_daily_loss == expected_daily_loss
    assert fake_logger.events[-1]["level"] == expected_final_log_level
    assert fake_logger.events[-1]["message"] == "Realized PnL applied to risk guard"


@pytest.mark.parametrize(
    ("config_kwargs", "expected_error"),
    [
        pytest.param(
            {"max_position_size_usd": True, "daily_loss_limit_usd": 1_000.0},
            "risk threshold must be a finite number",
            id="boolean_threshold_rejected",
        ),
        pytest.param(
            {"max_position_size_usd": 1_000.0, "daily_loss_limit_usd": float("inf")},
            "risk threshold must be a finite number",
            id="infinite_threshold_rejected",
        ),
        pytest.param(
            {
                "max_position_size_usd": 1_000.0,
                "daily_loss_limit_usd": 1_000.0,
                "max_price_deviation_pct": float("nan"),
            },
            "risk threshold must be a finite number",
            id="nan_optional_threshold_rejected",
        ),
    ],
)
def test_risk_config_rejects_non_finite_thresholds(
    risk_module: tuple[Any, FakeLoguruLogger],
    config_kwargs: dict[str, Any],
    expected_error: str,
) -> None:
    module, _ = risk_module

    with pytest.raises(ValueError, match=expected_error):
        module.RiskConfig(**config_kwargs)
