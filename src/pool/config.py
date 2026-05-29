"""Environment configuration for the modular paper asset-pool runtime."""

from __future__ import annotations

import math
import os
from dataclasses import dataclass

from dotenv import load_dotenv


@dataclass(frozen=True, slots=True)
class PoolSettings:
    """Validated paper-only pool settings.

    Pool evaluation intentionally has no live-execution switch. A future live
    allocator requires a separate approved design and entrypoint.
    """

    api_key: str
    api_secret: str
    use_testnet: bool = False
    mode: str = "paper"
    db_path: str = "data/pool-paper.db"
    benchmarks: tuple[str, ...] = ("USDT", "BTC", "PAXG", "XAUT")
    benchmark_weights: tuple[float, ...] = (0.25, 0.25, 0.25, 0.25)
    max_route_legs: int = 3
    max_route_expansions: int = 10_000
    max_candidate_routes: int = 8
    max_scoring_assets: int = 0
    min_quote_volume_usdt: float = 100_000.0
    max_spread_bps: float = 50.0
    slippage_buffer_bps: float = 25.0
    ticker_stale_seconds: float = 300.0
    fee_cache_seconds: float = 60.0
    bnb_fee_buffer_multiplier: float = 3.0
    min_bnb_fee_reserve: float = 0.00005
    scorers: tuple[str, ...] = ("simple_momentum", "risk_adjusted_momentum")
    momentum_observations: int = 96
    cycle_seconds: float = 900.0
    max_allocation_percent: float = 25.0
    max_reallocation_percent: float = 10.0
    min_improvement_bps: float = 50.0
    max_drawdown_percent: float = 10.0
    decision_cooldown_seconds: float = 3600.0

    def __post_init__(self) -> None:
        if not self.api_key.strip() or not self.api_secret.strip():
            raise RuntimeError("pool runtime requires Binance API credentials")
        object.__setattr__(self, "api_key", self.api_key.strip())
        object.__setattr__(self, "api_secret", self.api_secret.strip())
        if self.mode.strip().lower() != "paper":
            raise RuntimeError("POOL_MODE must be paper; live pool execution is disabled")
        object.__setattr__(self, "mode", "paper")
        if not self.db_path.strip():
            raise RuntimeError("POOL_DB_PATH must not be blank")
        object.__setattr__(self, "db_path", self.db_path.strip())
        benchmarks = tuple(asset.strip().upper() for asset in self.benchmarks if asset.strip())
        if not benchmarks or len(set(benchmarks)) != len(benchmarks):
            raise RuntimeError("POOL_BENCHMARKS must contain unique assets")
        if len(benchmarks) != len(self.benchmark_weights):
            raise RuntimeError("POOL_BENCHMARK_WEIGHTS must match POOL_BENCHMARKS")
        weights = tuple(_finite_positive(value, "POOL_BENCHMARK_WEIGHTS") for value in self.benchmark_weights)
        weight_sum = sum(weights)
        if not math.isclose(weight_sum, 1.0, rel_tol=0.0, abs_tol=1e-8):
            raise RuntimeError("POOL_BENCHMARK_WEIGHTS must sum to 1")
        object.__setattr__(self, "benchmarks", benchmarks)
        object.__setattr__(self, "benchmark_weights", weights)
        for name in (
            "max_route_legs",
            "max_route_expansions",
            "max_candidate_routes",
            "momentum_observations",
        ):
            if getattr(self, name) <= 0:
                raise RuntimeError(f"{name.upper()} must be a positive integer")
        if self.max_scoring_assets < 0:
            raise RuntimeError("POOL_MAX_SCORING_ASSETS must be non-negative")
        if self.max_route_legs > 3:
            raise RuntimeError("POOL_MAX_ROUTE_LEGS must not exceed 3")
        for name in (
            "min_quote_volume_usdt",
            "max_spread_bps",
            "slippage_buffer_bps",
            "ticker_stale_seconds",
            "fee_cache_seconds",
            "bnb_fee_buffer_multiplier",
            "min_bnb_fee_reserve",
            "cycle_seconds",
            "max_allocation_percent",
            "max_reallocation_percent",
            "min_improvement_bps",
            "max_drawdown_percent",
            "decision_cooldown_seconds",
        ):
            _finite_non_negative(getattr(self, name), name.upper())
        if self.bnb_fee_buffer_multiplier < 1:
            raise RuntimeError("BNB_FEE_BUFFER_MULTIPLIER must be at least 1")
        scorers = tuple(name.strip().lower() for name in self.scorers if name.strip())
        supported = {"simple_momentum", "risk_adjusted_momentum"}
        if not scorers or not set(scorers).issubset(supported):
            raise RuntimeError("POOL_SCORERS contains an unsupported scorer")
        object.__setattr__(self, "scorers", scorers)

    @classmethod
    def from_env(cls) -> "PoolSettings":
        """Load the isolated paper-pool runtime from environment variables."""
        load_dotenv()
        api_key = _required("BINANCE_API_KEY")
        api_secret = os.getenv("BINANCE_API_SECRET") or os.getenv("BINANCE_SECRET_KEY")
        if not api_secret or not api_secret.strip():
            raise RuntimeError(
                "missing required environment variable: BINANCE_API_SECRET or BINANCE_SECRET_KEY"
            )
        benchmarks = _csv("POOL_BENCHMARKS", "USDT,BTC,PAXG,XAUT")
        weights = tuple(
            float(value) for value in _csv("POOL_BENCHMARK_WEIGHTS", "0.25,0.25,0.25,0.25")
        )
        return cls(
            api_key=api_key,
            api_secret=api_secret,
            use_testnet=_bool("USE_TESTNET", False),
            mode=os.getenv("POOL_MODE", "paper"),
            db_path=os.getenv("POOL_DB_PATH", "data/pool-paper.db"),
            benchmarks=benchmarks,
            benchmark_weights=weights,
            max_route_legs=_int("POOL_MAX_ROUTE_LEGS", 3),
            max_route_expansions=_int("POOL_MAX_ROUTE_EXPANSIONS", 10_000),
            max_candidate_routes=_int("POOL_MAX_CANDIDATE_ROUTES", 8),
            max_scoring_assets=_int("POOL_MAX_SCORING_ASSETS", 0),
            min_quote_volume_usdt=_float("POOL_MIN_QUOTE_VOLUME_USDT", 100_000.0),
            max_spread_bps=_float("POOL_MAX_SPREAD_BPS", 50.0),
            slippage_buffer_bps=_float("POOL_SLIPPAGE_BUFFER_BPS", 25.0),
            ticker_stale_seconds=_float("POOL_TICKER_STALE_SECONDS", 300.0),
            fee_cache_seconds=_float("POOL_FEE_CACHE_SECONDS", 60.0),
            bnb_fee_buffer_multiplier=_float("POOL_BNB_FEE_BUFFER_MULTIPLIER", 3.0),
            min_bnb_fee_reserve=_float("POOL_MIN_BNB_FEE_RESERVE", 0.00005),
            scorers=_csv("POOL_SCORERS", "simple_momentum,risk_adjusted_momentum"),
            momentum_observations=_int("POOL_MOMENTUM_OBSERVATIONS", 96),
            cycle_seconds=_float("POOL_CYCLE_SECONDS", 900.0),
            max_allocation_percent=_float("POOL_MAX_ALLOCATION_PERCENT", 25.0),
            max_reallocation_percent=_float("POOL_MAX_REALLOCATION_PERCENT", 10.0),
            min_improvement_bps=_float("POOL_MIN_IMPROVEMENT_BPS", 50.0),
            max_drawdown_percent=_float("POOL_MAX_DRAWDOWN_PERCENT", 10.0),
            decision_cooldown_seconds=_float("POOL_DECISION_COOLDOWN_SECONDS", 3600.0),
        )


def _required(name: str) -> str:
    value = os.getenv(name)
    if not value or not value.strip():
        raise RuntimeError(f"missing required environment variable: {name}")
    return value.strip()


def _csv(name: str, default: str) -> tuple[str, ...]:
    return tuple(value.strip() for value in os.getenv(name, default).split(",") if value.strip())


def _float(name: str, default: float) -> float:
    raw = os.getenv(name)
    try:
        return float(raw) if raw is not None else default
    except ValueError as exc:
        raise RuntimeError(f"{name} must be a number") from exc


def _int(name: str, default: int) -> int:
    raw = os.getenv(name)
    try:
        return int(raw) if raw is not None else default
    except ValueError as exc:
        raise RuntimeError(f"{name} must be an integer") from exc


def _bool(name: str, default: bool) -> bool:
    raw = os.getenv(name)
    if raw is None:
        return default
    normalized = raw.strip().lower()
    if normalized in {"1", "true", "yes", "on"}:
        return True
    if normalized in {"0", "false", "no", "off"}:
        return False
    raise RuntimeError(f"{name} must be a boolean")


def _finite_non_negative(value: float, name: str) -> float:
    if not math.isfinite(float(value)) or float(value) < 0:
        raise RuntimeError(f"{name} must be a finite non-negative number")
    return float(value)


def _finite_positive(value: float, name: str) -> float:
    if not math.isfinite(float(value)) or float(value) <= 0:
        raise RuntimeError(f"{name} must contain positive finite numbers")
    return float(value)
