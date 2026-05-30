"""Claude-controlled autonomous live Spot routing.

This module is intentionally separate from ``src.pool``. The pool runtime
remains paper-only; Claude Spot owns the live order path for Phase 32.
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import html
import json
import math
import os
import shlex
import statistics
import subprocess
from dataclasses import asdict, dataclass, field, replace
from datetime import datetime, timedelta, timezone
from decimal import Decimal, ROUND_DOWN
from pathlib import Path
from typing import Any, Mapping, Protocol

from dotenv import load_dotenv

from src.api_permissions import (
    ApiPermissionError,
    READING,
    STATUS_OK,
    ensure_spot_order_test_allowed,
    inspect_api_permissions,
)
from src.config import Settings
from src.data.history import HistoryFetcher
from src.exchange.client import BinanceClient
from src.pool.catalog import MarketCatalog
from src.pool.config import PoolSettings
from src.pool.fees import CommissionService
from src.pool.models import MarketSnapshot, RouteLeg, RouteQuote
from src.pool.routes import RouteEngine
from src.social_signal import SocialSignalSettings, social_evidence_for_assets
from src.telegram_alerts import TelegramAlerter


CLAUDE_MONEY_MAKER_MISSION = (
    "You are a disciplined, cost-aware quantitative trader. Idle capital is "
    "strictly preferable to negative-expected-value trades. You compound by "
    "refusing trades whose expected edge after all modeled fees, spread, and "
    "slippage is not materially positive. Hold is the default action; "
    "rebalances must clear an explicit edge bar."
)
CLAUDE_SPOT_PROMPT_VERSION = "claude-spot-v3-calibrated"
CLAUDE_SPOT_MARKET_PICK_LIQUID = "liquid_top_value"
CLAUDE_SPOT_MARKET_PICK_SMALL = "small_market_aggressive"
CLAUDE_SPOT_MARKET_PICK_MODES = {
    CLAUDE_SPOT_MARKET_PICK_LIQUID,
    CLAUDE_SPOT_MARKET_PICK_SMALL,
}
SPECULATIVE_THESIS_KEYWORDS = (
    "breakout",
    "capitulation",
    "exit risk",
    "mean reversion",
    "momentum rotation",
    "oversold",
    "relative strength shift",
    "reversal",
    "speculative",
    "volume anomaly",
)
TELEGRAM_MESSAGE_MAX_CHARS = 3900
CLAUDE_SPOT_SHORT_EVIDENCE_TIMEFRAME = "1m"
CLAUDE_SPOT_SHORT_EVIDENCE_LOOKBACK_MINUTES = 30
CLAUDE_SPOT_SHORT_EVIDENCE_WINDOWS: tuple[tuple[str, int], ...] = (
    ("5m", 5),
    ("15m", 15),
)
CLAUDE_SPOT_EVIDENCE_WINDOWS: tuple[tuple[str, int], ...] = (
    ("1h", 1),
    ("4h", 4),
    ("24h", 24),
    ("7d", 168),
)
CLAUDE_SPOT_SHORT_EVIDENCE_WEIGHTS: dict[str, float] = {
    "5m": 0.10,
    "15m": 0.15,
}
CLAUDE_SPOT_EVIDENCE_WEIGHTS: dict[str, float] = {
    "1h": 0.15,
    "4h": 0.20,
    "24h": 0.25,
    "7d": 0.15,
}
_LOW_UPSIDE_ASSETS = {
    "AUD",
    "BRL",
    "BUSD",
    "DAI",
    "EUR",
    "FDUSD",
    "GBP",
    "JPY",
    "TUSD",
    "TRY",
    "USDC",
    "USDP",
    "USD1",
    "USDT",
}
DEFAULT_CLAUDE_SPOT_BLOCKED_QUOTES = ("IDR",)
DEFAULT_CLAUDE_SPOT_BLOCK_FIAT_ASSETS = True
CLAUDE_SPOT_FIAT_ASSETS = frozenset(
    {
        "AED",
        "AFN",
        "ALL",
        "AMD",
        "ANG",
        "AOA",
        "ARS",
        "AUD",
        "AWG",
        "AZN",
        "BAM",
        "BBD",
        "BDT",
        "BGN",
        "BHD",
        "BIF",
        "BMD",
        "BND",
        "BOB",
        "BRL",
        "BSD",
        "BTN",
        "BWP",
        "BYN",
        "BZD",
        "CAD",
        "CDF",
        "CHF",
        "CLP",
        "CNH",
        "CNY",
        "COP",
        "CRC",
        "CUP",
        "CVE",
        "CZK",
        "DJF",
        "DKK",
        "DOP",
        "DZD",
        "EGP",
        "ERN",
        "ETB",
        "EUR",
        "FJD",
        "FKP",
        "GBP",
        "GEL",
        "GHS",
        "GIP",
        "GMD",
        "GNF",
        "GTQ",
        "GYD",
        "HKD",
        "HNL",
        "HTG",
        "HUF",
        "IDR",
        "ILS",
        "INR",
        "IQD",
        "IRR",
        "ISK",
        "JMD",
        "JOD",
        "JPY",
        "KES",
        "KGS",
        "KHR",
        "KMF",
        "KPW",
        "KRW",
        "KWD",
        "KYD",
        "KZT",
        "LAK",
        "LBP",
        "LKR",
        "LRD",
        "LSL",
        "LYD",
        "MAD",
        "MDL",
        "MGA",
        "MKD",
        "MMK",
        "MNT",
        "MOP",
        "MRU",
        "MUR",
        "MVR",
        "MWK",
        "MXN",
        "MYR",
        "MZN",
        "NAD",
        "NGN",
        "NIO",
        "NOK",
        "NPR",
        "NZD",
        "OMR",
        "PAB",
        "PEN",
        "PGK",
        "PHP",
        "PKR",
        "PLN",
        "PYG",
        "QAR",
        "RON",
        "RSD",
        "RUB",
        "RWF",
        "SAR",
        "SBD",
        "SCR",
        "SDG",
        "SEK",
        "SGD",
        "SHP",
        "SLE",
        "SOS",
        "SRD",
        "SSP",
        "STN",
        "SVC",
        "SYP",
        "SZL",
        "THB",
        "TJS",
        "TMT",
        "TND",
        "TOP",
        "TRY",
        "TTD",
        "TWD",
        "TZS",
        "UAH",
        "UGX",
        "USD",
        "UYU",
        "UZS",
        "VES",
        "VND",
        "VUV",
        "WST",
        "XAF",
        "XCD",
        "XOF",
        "XPF",
        "YER",
        "ZAR",
        "ZMW",
        "ZWL",
    }
)

CLAUDE_SPOT_DECISION_SCHEMA: dict[str, Any] = {
    "type": "object",
    "additionalProperties": False,
    "required": ["action", "reason"],
    "properties": {
        "action": {"type": "string", "enum": ["hold", "rebalance"]},
        "route_id": {"type": "string"},
        "source_asset": {"type": "string"},
        "destination_asset": {"type": "string"},
        "spend_mode": {"type": "string", "enum": ["all_available"]},
        "spend_amount": {"type": "number", "minimum": 0},
        "reason": {"type": "string"},
    },
}

CLAUDE_SPOT_SYSTEM_PROMPT = f"""You are the autonomous Spot trading brain.
Mission: "{CLAUDE_MONEY_MAKER_MISSION}"

You choose one Spot action from the supplied executable route candidates.
Hold is always available and is the default when no candidate clears its
edge bar. Trading just to deploy capital is forbidden; round-trip fees and
spread compound against you faster than discretionary tape-reads recover.

Use the supplied dynamic fee estimates, route_scope.upfront_friction_allowance_bps,
candidate immediate_friction_bps, market_evidence.expected_edge_after_friction_bps,
market_evidence.minimum_expected_edge_bps, trade_memory, and any supplied
social_signal_evidence. Rebalance only when:
  1. The route's expected edge after all modeled costs is materially positive.
  2. The market_evidence packet is ready and its normal gate passes.
  3. Your reason cites the concrete evidence (which signal, what magnitude, over
     what horizon) that justifies paying the immediate friction.

If you are unsure, hold. There is no penalty for holding; there is a real,
measurable penalty for trading without edge.

Non-overridable execution invariants:
- Use only supplied route_id values.
- Return one JSON object matching the schema.
- Do not submit orders, claim orders were submitted, or call tools.
- Do not request Margin, Futures, transfers, withdrawals, or prediction trades.
- There is no bypass flag. You cannot override the friction or edge gates.
"""


class ClaudeSpotError(RuntimeError):
    """Raised when Claude Spot cannot proceed."""


class ClaudeSpotDecisionTimeout(ClaudeSpotError):
    """Raised when Claude does not return a decision before the cycle timeout."""


class ClaudeSpotExecutionError(ClaudeSpotError):
    """Raised when a live route stops after zero or more submitted legs."""

    def __init__(self, message: str, submitted_legs: list["ClaudeSpotLegResult"]) -> None:
        super().__init__(message)
        self.submitted_legs = submitted_legs


@dataclass(frozen=True, slots=True)
class ClaudeSpotSettings:
    """Environment-backed settings for autonomous Claude Spot execution."""

    api_key: str
    api_secret: str
    use_testnet: bool = False
    dry_run: bool = True
    confirm_live_trading: str = "no"
    enabled: bool = False
    dynamic_scope: str = ""
    claude_command: str = "gemini"
    claude_model: str = "gemini-3-flash-preview"
    claude_timeout_seconds: float = 300.0
    claude_max_budget_usd: str = ""
    prompt_version: str = CLAUDE_SPOT_PROMPT_VERSION
    telegram_bot_token: str = ""
    telegram_chat_id: str = ""
    audit_path: str = "data/claude-spot-decisions.jsonl"
    lock_path: str = "data/claude-spot.lock"
    emergency_stop_path: str = "data/claude-spot.stop"
    max_route_legs: int = 3
    max_route_expansions: int = 10_000
    max_candidate_routes: int = 1
    candidate_limit: int = 5
    market_pick_mode: str = CLAUDE_SPOT_MARKET_PICK_LIQUID
    min_quote_volume_usdt: float = 100_000.0
    max_spread_bps: float = 50.0
    small_min_quote_volume_usdt: float = 5_000.0
    small_max_spread_bps: float = 250.0
    small_candidate_limit: int = 10
    allow_negative_friction_bps: float = 40.0
    require_proven_edge: bool = True
    slippage_buffer_bps: float = 25.0
    leg_rounding_reserve_bps: float = 5.0
    min_leg_quote_reserve_bps: float = 25.0
    ticker_stale_seconds: float = 300.0
    fee_cache_seconds: float = 60.0
    min_trade_interval_seconds: float = 0.0
    max_submitted_trades_per_day: int = 0
    halt_after_failed_submission: bool = True
    market_evidence_enabled: bool = True
    require_market_evidence_for_live: bool = True
    market_evidence_min_edge_bps: float = 50.0
    speculative_mode_enabled: bool = False
    speculative_min_edge_bps: float = -1000.0
    speculative_require_thesis: bool = True
    market_evidence_lookback_hours: int = 168
    market_evidence_max_symbols: int = 12
    market_evidence_timeframe: str = "1h"
    trade_memory_limit: int = 8
    max_recent_loss_streak: int = 2
    telegram_explain: bool = True
    telegram_reason_chars: int = 0
    allowed_symbols: tuple[str, ...] = ()
    allowed_assets: tuple[str, ...] = ()
    blocked_symbols: tuple[str, ...] = ()
    blocked_assets: tuple[str, ...] = ()
    blocked_quotes: tuple[str, ...] = DEFAULT_CLAUDE_SPOT_BLOCKED_QUOTES
    block_fiat_assets: bool = DEFAULT_CLAUDE_SPOT_BLOCK_FIAT_ASSETS

    def __post_init__(self) -> None:
        if not self.api_key.strip() or not self.api_secret.strip():
            raise ClaudeSpotError("Claude Spot requires Binance API credentials")
        object.__setattr__(self, "api_key", self.api_key.strip())
        object.__setattr__(self, "api_secret", self.api_secret.strip())
        object.__setattr__(
            self,
            "confirm_live_trading",
            self.confirm_live_trading.strip().lower(),
        )
        object.__setattr__(self, "dynamic_scope", self.dynamic_scope.strip().lower())
        object.__setattr__(self, "claude_command", self.claude_command.strip())
        object.__setattr__(self, "claude_model", self.claude_model.strip())
        object.__setattr__(self, "market_pick_mode", self.market_pick_mode.strip().lower())
        object.__setattr__(
            self,
            "claude_max_budget_usd",
            self.claude_max_budget_usd.strip(),
        )
        object.__setattr__(
            self,
            "market_evidence_timeframe",
            self.market_evidence_timeframe.strip(),
        )
        object.__setattr__(self, "prompt_version", self.prompt_version.strip())
        object.__setattr__(
            self,
            "telegram_bot_token",
            self.telegram_bot_token.strip(),
        )
        object.__setattr__(self, "telegram_chat_id", self.telegram_chat_id.strip())
        object.__setattr__(
            self,
            "allowed_symbols",
            _normalize_symbol_list(self.allowed_symbols, "allowed_symbols"),
        )
        object.__setattr__(
            self,
            "allowed_assets",
            _normalize_asset_list(self.allowed_assets, "allowed_assets"),
        )
        object.__setattr__(
            self,
            "blocked_symbols",
            _normalize_symbol_list(self.blocked_symbols, "blocked_symbols"),
        )
        object.__setattr__(
            self,
            "blocked_assets",
            _normalize_asset_list(self.blocked_assets, "blocked_assets"),
        )
        object.__setattr__(
            self,
            "blocked_quotes",
            _normalize_asset_list(self.blocked_quotes, "blocked_quotes"),
        )
        for path_name in ("audit_path", "lock_path", "emergency_stop_path"):
            if not getattr(self, path_name).strip():
                raise ClaudeSpotError(f"{path_name} must not be blank")
        if not self.claude_command:
            raise ClaudeSpotError("claude_command must not be blank")
        if not self.prompt_version:
            raise ClaudeSpotError("prompt_version must not be blank")
        if self.market_pick_mode not in CLAUDE_SPOT_MARKET_PICK_MODES:
            raise ClaudeSpotError(
                "CLAUDE_SPOT_MARKET_PICK_MODE must be liquid_top_value or "
                "small_market_aggressive"
            )
        if self.max_route_legs <= 0 or self.max_route_legs > 3:
            raise ClaudeSpotError("max_route_legs must be between 1 and 3")
        for name in (
            "max_route_expansions",
            "max_candidate_routes",
            "candidate_limit",
            "small_candidate_limit",
        ):
            if getattr(self, name) < 0:
                raise ClaudeSpotError(f"{name} must be non-negative")
        for name in (
            "claude_timeout_seconds",
            "min_quote_volume_usdt",
            "max_spread_bps",
            "small_min_quote_volume_usdt",
            "small_max_spread_bps",
            "allow_negative_friction_bps",
            "slippage_buffer_bps",
            "leg_rounding_reserve_bps",
            "min_leg_quote_reserve_bps",
            "ticker_stale_seconds",
            "fee_cache_seconds",
            "min_trade_interval_seconds",
            "market_evidence_min_edge_bps",
        ):
            _finite_non_negative(getattr(self, name), name)
        _finite_number(self.speculative_min_edge_bps, "speculative_min_edge_bps")
        for name in (
            "market_evidence_lookback_hours",
            "market_evidence_max_symbols",
            "trade_memory_limit",
            "max_recent_loss_streak",
            "telegram_reason_chars",
        ):
            if getattr(self, name) < 0:
                raise ClaudeSpotError(f"{name} must be non-negative")
        if self.claude_timeout_seconds <= 0:
            raise ClaudeSpotError("claude_timeout_seconds must be greater than zero")
        if not self.market_evidence_timeframe:
            raise ClaudeSpotError("market_evidence_timeframe must not be blank")
        if self.market_evidence_timeframe != "1h":
            raise ClaudeSpotError("market_evidence_timeframe must be 1h")
        if self.max_submitted_trades_per_day < 0:
            raise ClaudeSpotError("max_submitted_trades_per_day must be non-negative")

    @classmethod
    def from_env(cls) -> "ClaudeSpotSettings":
        """Load Claude Spot settings from environment variables."""
        load_dotenv()
        api_secret = os.getenv("BINANCE_API_SECRET") or os.getenv("BINANCE_SECRET_KEY")
        if not api_secret or not api_secret.strip():
            raise ClaudeSpotError(
                "missing required environment variable: BINANCE_API_SECRET or BINANCE_SECRET_KEY"
            )
        return cls(
            api_key=_required("BINANCE_API_KEY"),
            api_secret=api_secret,
            use_testnet=_bool("USE_TESTNET", True),
            dry_run=_bool("DRY_RUN", True),
            confirm_live_trading=os.getenv("CONFIRM_LIVE_TRADING", "no"),
            enabled=_bool("CLAUDE_SPOT_ENABLED", False),
            dynamic_scope=os.getenv("CLAUDE_SPOT_DYNAMIC_SCOPE", ""),
            claude_command=os.getenv("CLAUDE_SPOT_COMMAND", "claude"),
            claude_model=os.getenv("CLAUDE_SPOT_MODEL", "sonnet"),
            claude_timeout_seconds=_float("CLAUDE_SPOT_TIMEOUT_SECONDS", 300.0),
            claude_max_budget_usd=os.getenv("CLAUDE_SPOT_MAX_BUDGET_USD", ""),
            prompt_version=os.getenv(
                "CLAUDE_SPOT_PROMPT_VERSION",
                CLAUDE_SPOT_PROMPT_VERSION,
            ),
            telegram_bot_token=os.getenv("TELEGRAM_BOT_TOKEN", ""),
            telegram_chat_id=os.getenv("TELEGRAM_CHAT_ID", ""),
            audit_path=os.getenv(
                "CLAUDE_SPOT_AUDIT_PATH",
                "data/claude-spot-decisions.jsonl",
            ),
            lock_path=os.getenv("CLAUDE_SPOT_LOCK_PATH", "data/claude-spot.lock"),
            emergency_stop_path=os.getenv(
                "CLAUDE_SPOT_STOP_PATH",
                "data/claude-spot.stop",
            ),
            max_route_legs=_int("CLAUDE_SPOT_MAX_ROUTE_LEGS", 3),
            max_route_expansions=_int("CLAUDE_SPOT_MAX_ROUTE_EXPANSIONS", 10_000),
            max_candidate_routes=_int("CLAUDE_SPOT_MAX_CANDIDATE_ROUTES", 1),
            candidate_limit=_int("CLAUDE_SPOT_CANDIDATE_LIMIT", 5),
            market_pick_mode=os.getenv(
                "CLAUDE_SPOT_MARKET_PICK_MODE",
                CLAUDE_SPOT_MARKET_PICK_LIQUID,
            ),
            min_quote_volume_usdt=_float("CLAUDE_SPOT_MIN_QUOTE_VOLUME_USDT", 100_000.0),
            max_spread_bps=_float("CLAUDE_SPOT_MAX_SPREAD_BPS", 50.0),
            small_min_quote_volume_usdt=_float(
                "CLAUDE_SPOT_SMALL_MIN_QUOTE_VOLUME_USDT",
                5_000.0,
            ),
            small_max_spread_bps=_float(
                "CLAUDE_SPOT_SMALL_MAX_SPREAD_BPS",
                250.0,
            ),
            small_candidate_limit=_int("CLAUDE_SPOT_SMALL_CANDIDATE_LIMIT", 10),
            allow_negative_friction_bps=_float(
                "CLAUDE_SPOT_ALLOW_NEGATIVE_FRICTION_BPS",
                40.0,
            ),
            require_proven_edge=_bool(
                "CLAUDE_SPOT_REQUIRE_PROVEN_EDGE",
                True,
            ),
            slippage_buffer_bps=_float("CLAUDE_SPOT_SLIPPAGE_BUFFER_BPS", 25.0),
            leg_rounding_reserve_bps=_float(
                "CLAUDE_SPOT_LEG_ROUNDING_RESERVE_BPS",
                5.0,
            ),
            min_leg_quote_reserve_bps=_float(
                "CLAUDE_SPOT_MIN_LEG_QUOTE_RESERVE_BPS",
                25.0,
            ),
            ticker_stale_seconds=_float("CLAUDE_SPOT_TICKER_STALE_SECONDS", 300.0),
            fee_cache_seconds=_float("CLAUDE_SPOT_FEE_CACHE_SECONDS", 60.0),
            min_trade_interval_seconds=_float(
                "CLAUDE_SPOT_MIN_TRADE_INTERVAL_SECONDS",
                0.0,
            ),
            max_submitted_trades_per_day=_int(
                "CLAUDE_SPOT_MAX_SUBMITTED_TRADES_PER_DAY",
                0,
            ),
            halt_after_failed_submission=_bool(
                "CLAUDE_SPOT_HALT_AFTER_FAILED_SUBMISSION",
                True,
            ),
            market_evidence_enabled=_bool(
                "CLAUDE_SPOT_MARKET_EVIDENCE_ENABLED",
                True,
            ),
            require_market_evidence_for_live=_bool(
                "CLAUDE_SPOT_REQUIRE_MARKET_EVIDENCE_FOR_LIVE",
                True,
            ),
            market_evidence_min_edge_bps=_float(
                "CLAUDE_SPOT_MARKET_EVIDENCE_MIN_EDGE_BPS",
                50.0,
            ),
            speculative_mode_enabled=_bool(
                "CLAUDE_SPOT_SPECULATIVE_MODE_ENABLED",
                False,
            ),
            speculative_min_edge_bps=_float(
                "CLAUDE_SPOT_SPECULATIVE_MIN_EDGE_BPS",
                -1000.0,
            ),
            speculative_require_thesis=_bool(
                "CLAUDE_SPOT_SPECULATIVE_REQUIRE_THESIS",
                True,
            ),
            market_evidence_lookback_hours=_int(
                "CLAUDE_SPOT_MARKET_EVIDENCE_LOOKBACK_HOURS",
                168,
            ),
            market_evidence_max_symbols=_int(
                "CLAUDE_SPOT_MARKET_EVIDENCE_MAX_SYMBOLS",
                12,
            ),
            market_evidence_timeframe=os.getenv(
                "CLAUDE_SPOT_MARKET_EVIDENCE_TIMEFRAME",
                "1h",
            ),
            trade_memory_limit=_int("CLAUDE_SPOT_TRADE_MEMORY_LIMIT", 8),
            max_recent_loss_streak=_int("CLAUDE_SPOT_MAX_RECENT_LOSS_STREAK", 2),
            telegram_explain=_bool("CLAUDE_SPOT_TELEGRAM_EXPLAIN", True),
            telegram_reason_chars=_int("CLAUDE_SPOT_TELEGRAM_REASON_CHARS", 0),
            allowed_symbols=_csv_env(
                "CLAUDE_SPOT_ALLOWED_SYMBOLS",
                os.getenv("API_SYMBOL_WHITELIST", ""),
            ),
            allowed_assets=_csv_env(
                "CLAUDE_SPOT_ALLOWED_ASSETS",
                os.getenv("API_ASSET_WHITELIST", ""),
            ),
            blocked_symbols=_csv_env("CLAUDE_SPOT_BLOCKED_SYMBOLS", ""),
            blocked_assets=_csv_env("CLAUDE_SPOT_BLOCKED_ASSETS", ""),
            blocked_quotes=_csv_env(
                "CLAUDE_SPOT_BLOCKED_QUOTES",
                ",".join(DEFAULT_CLAUDE_SPOT_BLOCKED_QUOTES),
            ),
            block_fiat_assets=_bool(
                "CLAUDE_SPOT_BLOCK_FIAT_ASSETS",
                DEFAULT_CLAUDE_SPOT_BLOCK_FIAT_ASSETS,
            ),
        )

    def pool_settings(self) -> PoolSettings:
        """Return compatible route/catalog settings for live candidate building."""
        return PoolSettings(
            api_key=self.api_key,
            api_secret=self.api_secret,
            use_testnet=self.use_testnet,
            mode="paper",
            max_route_legs=self.max_route_legs,
            max_route_expansions=self.max_route_expansions,
            max_candidate_routes=max(self.max_candidate_routes, 1),
            max_scoring_assets=(
                self.candidate_limit
                if self.market_pick_mode == CLAUDE_SPOT_MARKET_PICK_LIQUID
                else 0
            ),
            min_quote_volume_usdt=self.effective_min_quote_volume_usdt,
            max_spread_bps=self.effective_max_spread_bps,
            slippage_buffer_bps=self.slippage_buffer_bps,
            ticker_stale_seconds=self.ticker_stale_seconds,
            fee_cache_seconds=self.fee_cache_seconds,
        )

    @property
    def effective_min_quote_volume_usdt(self) -> float:
        """Return the active market-volume gate for Claude Spot cataloging."""
        if self.market_pick_mode == CLAUDE_SPOT_MARKET_PICK_SMALL:
            return self.small_min_quote_volume_usdt
        return self.min_quote_volume_usdt

    @property
    def effective_max_spread_bps(self) -> float:
        """Return the active spread gate for Claude Spot cataloging."""
        if self.market_pick_mode == CLAUDE_SPOT_MARKET_PICK_SMALL:
            return self.small_max_spread_bps
        return self.max_spread_bps

    @property
    def effective_candidate_limit(self) -> int:
        """Return the active candidate count sent to Claude."""
        if self.market_pick_mode == CLAUDE_SPOT_MARKET_PICK_SMALL:
            return self.small_candidate_limit
        return self.candidate_limit

    def legacy_settings(self) -> Settings:
        """Return base Settings for existing permission diagnostics."""
        return Settings(
            api_key=self.api_key,
            api_secret=self.api_secret,
            use_testnet=self.use_testnet,
            dry_run=self.dry_run,
            confirm_live_trading=self.confirm_live_trading,
        )


@dataclass(frozen=True, slots=True)
class ClaudeSpotDecision:
    """One schema-validated Claude trading decision."""

    action: str
    reason: str
    route_id: str = ""
    source_asset: str = ""
    destination_asset: str = ""
    spend_mode: str = ""
    spend_amount: float | None = None

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_mapping(cls, payload: Mapping[str, Any]) -> "ClaudeSpotDecision":
        """Validate and normalize Claude's raw decision payload."""
        action = str(payload.get("action", "")).strip().lower()
        reason = str(payload.get("reason", "")).strip()
        if action not in {"hold", "rebalance"}:
            raise ClaudeSpotError("Claude decision action must be hold or rebalance")
        if not reason:
            raise ClaudeSpotError("Claude decision reason must not be blank")
        if action == "hold":
            return cls(action="hold", reason=reason)
        route_id = str(payload.get("route_id", "")).strip()
        source = str(payload.get("source_asset", "")).strip().upper()
        destination = str(payload.get("destination_asset", "")).strip().upper()
        spend_mode = str(payload.get("spend_mode", "")).strip().lower()
        if not route_id:
            raise ClaudeSpotError("rebalance decision requires route_id")
        if not source or not destination or source == destination:
            raise ClaudeSpotError("rebalance decision requires distinct assets")
        if spend_mode != "all_available":
            raise ClaudeSpotError("Claude Spot v1 requires spend_mode=all_available")
        spend_amount = _optional_positive_float(payload.get("spend_amount"))
        return cls(
            action="rebalance",
            route_id=route_id,
            source_asset=source,
            destination_asset=destination,
            spend_mode=spend_mode,
            spend_amount=spend_amount,
            reason=reason,
        )


@dataclass(frozen=True, slots=True)
class ClaudeRouteCandidate:
    """One executable route offered to Claude."""

    route_id: str
    route: RouteQuote
    estimated_output_usdt: float | None
    source_value_usdt: float | None = None
    immediate_friction_bps: float | None = None
    market_pick_score: float = 0.0
    market_pick_reason: str = ""
    quote_volume_usdt: float | None = None
    spread_bps: float | None = None
    liquidity_tier: str = ""
    market_evidence: dict[str, Any] = field(default_factory=dict)
    social_signal_evidence: dict[str, Any] = field(default_factory=dict)

    def as_context(self) -> dict[str, Any]:
        return {
            "route_id": self.route_id,
            "source_asset": self.route.source_asset,
            "destination_asset": self.route.destination_asset,
            "input_amount": self.route.input_amount,
            "output_amount": self.route.output_amount,
            "estimated_output_usdt": self.estimated_output_usdt,
            "source_value_usdt": self.source_value_usdt,
            "immediate_friction_bps": self.immediate_friction_bps,
            "market_pick_score": self.market_pick_score,
            "market_pick_reason": self.market_pick_reason,
            "quote_volume_usdt": self.quote_volume_usdt,
            "spread_bps": self.spread_bps,
            "liquidity_tier": self.liquidity_tier,
            "market_evidence": self.market_evidence,
            "social_signal_evidence": self.social_signal_evidence,
            "slippage_bps": self.route.slippage_bps,
            "legs": [_leg_context(leg) for leg in self.route.legs],
        }


@dataclass(frozen=True, slots=True)
class ClaudeDecisionContext:
    """Prompt context sent to Claude."""

    timestamp: str
    prompt_version: str
    mission: str
    market_pick_mode: str
    upfront_friction_allowance_bps: float
    balances: dict[str, Any]
    portfolio_value_usdt: float
    candidate_routes: tuple[ClaudeRouteCandidate, ...]
    execution_invariants: tuple[str, ...]
    route_scope: dict[str, Any] = field(default_factory=dict)
    trade_memory: dict[str, Any] = field(default_factory=dict)

    def as_prompt_payload(self) -> dict[str, Any]:
        return {
            "timestamp": self.timestamp,
            "prompt_version": self.prompt_version,
            "mission": self.mission,
            "market_pick_mode": self.market_pick_mode,
            "upfront_friction_allowance_bps": self.upfront_friction_allowance_bps,
            "balances": self.balances,
            "portfolio_value_usdt": self.portfolio_value_usdt,
            "candidate_routes": [
                candidate.as_context() for candidate in self.candidate_routes
            ],
            "execution_invariants": list(self.execution_invariants),
            "route_scope": self.route_scope,
            "trade_memory": self.trade_memory,
        }

    @property
    def context_hash(self) -> str:
        payload = json.dumps(self.as_prompt_payload(), sort_keys=True)
        return hashlib.sha256(payload.encode("utf-8")).hexdigest()


@dataclass(frozen=True, slots=True)
class ClaudeSpotLegResult:
    """Auditable result for one submitted Spot route leg."""

    symbol: str
    side: str
    source_asset: str
    destination_asset: str
    order_amount: float
    source_free_before: float
    destination_free_before: float
    source_free_after: float
    destination_free_after: float
    order: dict[str, Any]
    final_order: dict[str, Any]
    status: str

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class ClaudeSpotResult:
    """One Claude Spot cycle result."""

    timestamp: str
    mode: str
    status: str
    prompt_version: str
    context_hash: str
    decision: dict[str, Any]
    candidate_route_count: int
    market_pick_mode: str = ""
    candidate_routes: list[dict[str, Any]] = field(default_factory=list)
    selected_route: dict[str, Any] = field(default_factory=dict)
    submitted_legs: list[dict[str, Any]] = field(default_factory=list)
    submission_attempted: bool = False
    error: str = ""
    audit_path: str = ""
    telegram_sent: bool = False

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


class ClaudeSpotAdvisor(Protocol):
    """Interface for Claude decision providers."""

    def decide(self, context: ClaudeDecisionContext) -> ClaudeSpotDecision:
        """Return one decision for a prompt context."""


class HeadlessClaudeSpotAdvisor:
    """Call Claude Code headlessly and parse a structured Spot decision."""

    def __init__(
        self,
        *,
        command: str = "claude",
        model: str = "",
        timeout_seconds: float = 240.0,
        max_budget_usd: str = "",
    ) -> None:
        self.command = command
        self.model = model
        self.timeout_seconds = timeout_seconds
        self.max_budget_usd = max_budget_usd

    def decide(self, context: ClaudeDecisionContext) -> ClaudeSpotDecision:
        argv = self._argv(context)
        try:
            completed = subprocess.run(
                argv,
                text=True,
                capture_output=True,
                timeout=max(self.timeout_seconds, 0.1),
                check=False,
            )
        except subprocess.TimeoutExpired as exc:
            raise ClaudeSpotDecisionTimeout(
                f"Claude decision timed out after {max(self.timeout_seconds, 0.1):.1f} seconds"
            ) from exc
        except (OSError, subprocess.SubprocessError, ValueError) as exc:
            raise ClaudeSpotError(f"Claude decision unavailable: {exc}") from exc
        if completed.returncode != 0:
            detail = completed.stderr.strip() or completed.stdout.strip()
            raise ClaudeSpotError(f"Claude decision failed: {detail}")
        payload = _extract_json_object(completed.stdout)
        if payload is None:
            raise ClaudeSpotError("Claude response did not contain a JSON object")
        return ClaudeSpotDecision.from_mapping(payload)

    def _argv(self, context: ClaudeDecisionContext) -> list[str]:
        argv = shlex.split(self.command)
        if not argv:
            raise ClaudeSpotError("Claude command parsed to an empty argv")
            
        user_prompt = _build_user_prompt(context)
        
        if "gemini" in argv[0]:
            prompt_text = (
                f"SYSTEM PROMPT:\n{CLAUDE_SPOT_SYSTEM_PROMPT}\n\n"
                f"OUTPUT SCHEMA:\nYou MUST return ONLY valid JSON matching this schema:\n"
                f"{json.dumps(CLAUDE_SPOT_DECISION_SCHEMA, indent=2)}\n\n"
                f"CONTEXT:\n{user_prompt}"
            )
            argv.extend(["-p", prompt_text])
            if self.model:
                argv.extend(["-m", self.model])
        else:
            argv.extend(
                [
                    "-p",
                    "--no-session-persistence",
                    "--tools",
                    "",
                    "--output-format",
                    "json",
                    "--json-schema",
                    json.dumps(CLAUDE_SPOT_DECISION_SCHEMA, sort_keys=True),
                    "--system-prompt",
                    CLAUDE_SPOT_SYSTEM_PROMPT,
                ]
            )
            if self.model:
                argv.extend(["--model", self.model])
            if self.max_budget_usd:
                argv.extend(["--max-budget-usd", self.max_budget_usd])
            argv.append(user_prompt)
        return argv


async def run_claude_spot_cycle(
    settings: ClaudeSpotSettings,
    *,
    submit_order: bool,
    client: Any | None = None,
    advisor: ClaudeSpotAdvisor | None = None,
    alerter: Any | None = None,
    use_lock: bool = False,
) -> ClaudeSpotResult:
    """Run one autonomous Claude Spot decision cycle."""
    _preflight(settings, submit_order=submit_order)
    owns_client = client is None
    client = client or BinanceClient(
        settings.api_key,
        settings.api_secret,
        testnet=settings.use_testnet,
        max_retries=0,
    )
    advisor = advisor or HeadlessClaudeSpotAdvisor(
        command=settings.claude_command,
        model=settings.claude_model,
        timeout_seconds=settings.claude_timeout_seconds,
        max_budget_usd=settings.claude_max_budget_usd,
    )
    alerter = alerter or TelegramAlerter(
        settings.telegram_bot_token,
        settings.telegram_chat_id,
    )
    if hasattr(alerter, "check_for_go_off_command"):
        await alerter.check_for_go_off_command()
    lock: _CycleLock | None = None
    context: ClaudeDecisionContext | None = None
    decision: ClaudeSpotDecision | None = None
    selected: ClaudeRouteCandidate | None = None
    try:
        if use_lock:
            lock = _CycleLock(Path(settings.lock_path))
            lock.acquire()
        context, markets = await build_decision_context(client, settings)
        if submit_order:
            discipline_reason = _trade_discipline_hold_reason(settings)
            if discipline_reason is not None:
                decision = ClaudeSpotDecision(action="hold", reason=discipline_reason)
                result = ClaudeSpotResult(
                    timestamp=_utc_timestamp(),
                    mode=_mode(settings),
                    status="held",
                    prompt_version=settings.prompt_version,
                    context_hash=context.context_hash,
                    decision=decision.as_dict(),
                    candidate_route_count=len(context.candidate_routes),
                    market_pick_mode=settings.market_pick_mode,
                    candidate_routes=_audit_candidates(context),
                    submission_attempted=False,
                )
                audit_file = _write_audit(Path(settings.audit_path), result)
                telegram_sent = await _send_alert(alerter, result, audit_file, settings)
                return ClaudeSpotResult(
                    **{
                        **result.as_dict(),
                        "audit_path": str(audit_file),
                        "telegram_sent": telegram_sent,
                    }
                )
        try:
            decision = advisor.decide(context)
        except ClaudeSpotDecisionTimeout as exc:
            decision = ClaudeSpotDecision(
                action="hold",
                reason=f"{exc}; no order submitted",
            )
            result = ClaudeSpotResult(
                timestamp=_utc_timestamp(),
                mode=_mode(settings),
                status="held",
                prompt_version=settings.prompt_version,
                context_hash=context.context_hash,
                decision=decision.as_dict(),
                candidate_route_count=len(context.candidate_routes),
                market_pick_mode=settings.market_pick_mode,
                candidate_routes=_audit_candidates(context),
                submission_attempted=False,
                error=str(exc),
            )
            audit_file = _write_audit(Path(settings.audit_path), result)
            telegram_sent = await _send_alert(alerter, result, audit_file, settings)
            return ClaudeSpotResult(
                **{
                    **result.as_dict(),
                    "audit_path": str(audit_file),
                    "telegram_sent": telegram_sent,
                }
            )
        selected = _select_candidate(decision, context.candidate_routes)
        if decision.action == "hold":
            result = ClaudeSpotResult(
                timestamp=_utc_timestamp(),
                mode=_mode(settings),
                status="held",
                prompt_version=settings.prompt_version,
                context_hash=context.context_hash,
                decision=decision.as_dict(),
                candidate_route_count=len(context.candidate_routes),
                market_pick_mode=settings.market_pick_mode,
                candidate_routes=_audit_candidates(context),
                submission_attempted=False,
            )
            audit_file = _write_audit(Path(settings.audit_path), result)
            telegram_sent = await _send_alert(alerter, result, audit_file, settings)
            return ClaudeSpotResult(
                **{**result.as_dict(), "audit_path": str(audit_file), "telegram_sent": telegram_sent}
            )
        if selected is None:
            raise ClaudeSpotError("Claude selected a route that was not offered")
        _validate_rebalance_decision(decision, selected)
        _assert_route_scope_allowed(selected.route, settings)
        selected_context = _selected_route_context(selected, settings, decision)
        if not submit_order:
            result = ClaudeSpotResult(
                timestamp=_utc_timestamp(),
                mode=_mode(settings),
                status="planned",
                prompt_version=settings.prompt_version,
                context_hash=context.context_hash,
                decision=decision.as_dict(),
                candidate_route_count=len(context.candidate_routes),
                market_pick_mode=settings.market_pick_mode,
                candidate_routes=_audit_candidates(context),
                selected_route=selected_context,
                submission_attempted=False,
            )
            audit_file = _write_audit(Path(settings.audit_path), result)
            telegram_sent = await _send_alert(alerter, result, audit_file, settings)
            return ClaudeSpotResult(
                **{**result.as_dict(), "audit_path": str(audit_file), "telegram_sent": telegram_sent}
            )
        evidence_hold_reason = _market_evidence_hold_reason(
            settings,
            context,
            selected,
            decision,
        )
        if evidence_hold_reason is not None:
            decision = ClaudeSpotDecision(action="hold", reason=evidence_hold_reason)
            result = ClaudeSpotResult(
                timestamp=_utc_timestamp(),
                mode=_mode(settings),
                status="held",
                prompt_version=settings.prompt_version,
                context_hash=context.context_hash,
                decision=decision.as_dict(),
                candidate_route_count=len(context.candidate_routes),
                market_pick_mode=settings.market_pick_mode,
                candidate_routes=_audit_candidates(context),
                selected_route=selected_context,
                submission_attempted=False,
            )
            audit_file = _write_audit(Path(settings.audit_path), result)
            telegram_sent = await _send_alert(alerter, result, audit_file, settings)
            return ClaudeSpotResult(
                **{**result.as_dict(), "audit_path": str(audit_file), "telegram_sent": telegram_sent}
            )
        await _ensure_live_permission(client, settings, selected.route, markets)
        try:
            submitted_legs = await _execute_route(client, settings, selected.route, markets)
        except ClaudeSpotExecutionError as exc:
            result = ClaudeSpotResult(
                timestamp=_utc_timestamp(),
                mode=_mode(settings),
                status="failed",
                prompt_version=settings.prompt_version,
                context_hash=context.context_hash,
                decision=decision.as_dict(),
                candidate_route_count=len(context.candidate_routes),
                market_pick_mode=settings.market_pick_mode,
                candidate_routes=_audit_candidates(context),
                selected_route=selected_context,
                submitted_legs=[leg.as_dict() for leg in exc.submitted_legs],
                submission_attempted=bool(exc.submitted_legs),
                error=str(exc),
            )
            audit_file = _write_audit(Path(settings.audit_path), result)
            await _send_alert(alerter, result, audit_file, settings)
            raise
        result = ClaudeSpotResult(
            timestamp=_utc_timestamp(),
            mode=_mode(settings),
            status="submitted",
            prompt_version=settings.prompt_version,
            context_hash=context.context_hash,
            decision=decision.as_dict(),
            candidate_route_count=len(context.candidate_routes),
            market_pick_mode=settings.market_pick_mode,
            candidate_routes=_audit_candidates(context),
            selected_route=selected_context,
            submitted_legs=[leg.as_dict() for leg in submitted_legs],
            submission_attempted=True,
        )
        audit_file = _write_audit(Path(settings.audit_path), result)
        telegram_sent = await _send_alert(alerter, result, audit_file, settings)
        return ClaudeSpotResult(
            **{**result.as_dict(), "audit_path": str(audit_file), "telegram_sent": telegram_sent}
        )
    except ClaudeSpotExecutionError:
        raise
    except Exception as exc:
        result = ClaudeSpotResult(
            timestamp=_utc_timestamp(),
            mode=_mode(settings),
            status="blocked" if not submit_order else "failed",
            prompt_version=settings.prompt_version,
            context_hash=context.context_hash if context is not None else "",
            decision=decision.as_dict() if decision is not None else {},
            candidate_route_count=(
                len(context.candidate_routes) if context is not None else 0
            ),
            market_pick_mode=settings.market_pick_mode,
            candidate_routes=_audit_candidates(context),
            selected_route=(
                _selected_route_context(selected, settings, decision)
                if selected is not None and decision is not None
                else {}
            ),
            submission_attempted=False,
            error=str(exc),
        )
        audit_file = _write_audit(Path(settings.audit_path), result)
        await _send_alert(alerter, result, audit_file, settings)
        raise
    finally:
        if lock is not None:
            lock.release()
        if owns_client:
            await client.close()


async def build_decision_context(
    client: Any,
    settings: ClaudeSpotSettings,
) -> tuple[ClaudeDecisionContext, dict[str, MarketSnapshot]]:
    """Build the current Spot portfolio and executable route context."""
    pool_settings = settings.pool_settings()
    catalog = MarketCatalog(client, pool_settings)
    markets = _scoped_markets(await catalog.refresh(), settings)
    fees = CommissionService(client, cache_seconds=settings.fee_cache_seconds)
    routes = RouteEngine(markets, fees, pool_settings)
    balance = _sanitize_balance(await client.get_balance())
    free_balances = balance.get("free", {})
    candidate_assets = _candidate_assets(markets, free_balances, settings)
    route_candidates = await _build_route_candidates(
        routes,
        markets,
        free_balances,
        candidate_assets,
        settings=settings,
    )
    audit_rows, audit_read_error = _load_audit_rows(Path(settings.audit_path))
    trade_memory = _trade_memory_context(
        audit_rows,
        free_balances,
        routes,
        settings,
        read_error=audit_read_error,
    )
    if settings.market_evidence_enabled and route_candidates:
        route_candidates = await _enrich_route_candidates_with_evidence(
            client,
            route_candidates,
            markets,
            settings,
        )
    social_settings = SocialSignalSettings.from_env()
    if social_settings.enabled and route_candidates:
        social_evidence = social_evidence_for_assets(
            social_settings,
            (candidate.route.destination_asset for candidate in route_candidates),
        )
        route_candidates = [
            replace(
                candidate,
                social_signal_evidence=social_evidence.get(
                    candidate.route.destination_asset,
                    {"status": "empty"},
                ),
            )
            for candidate in route_candidates
            if social_evidence.get(candidate.route.destination_asset, {}).get("block_route")
            is not True
        ]
    portfolio_value = _portfolio_value_usdt(free_balances, routes)
    context = ClaudeDecisionContext(
        timestamp=_utc_timestamp(),
        prompt_version=settings.prompt_version,
        mission=CLAUDE_MONEY_MAKER_MISSION,
        market_pick_mode=settings.market_pick_mode,
        upfront_friction_allowance_bps=settings.allow_negative_friction_bps,
        balances=balance,
        portfolio_value_usdt=portfolio_value,
        candidate_routes=tuple(route_candidates),
        execution_invariants=(
            "only supplied route_id values may be selected",
            "one action may execute at most three serial Spot order legs",
            "each leg must satisfy Binance market rules and order-test preflight",
            "halt after any rejected, ambiguous, partial, or unreconciled leg",
            "route symbols, assets, and quote assets must satisfy local route scope",
            "traditional fiat assets are excluded when fiat blocking is enabled",
            "live rebalances must satisfy configured fixed trade limits when enabled and always satisfy friction cap",
            "live rebalances must satisfy market evidence, break-even, and trade-memory gates",
            "configured social manipulation-risk blocks remove routes before Claude can select them",
            "Spot only; no margin, futures, transfer, withdrawal, or prediction writes",
        ),
        route_scope=_route_scope_context(settings),
        trade_memory=trade_memory,
    )
    return context, markets


async def _build_route_candidates(
    routes: RouteEngine,
    markets: Mapping[str, MarketSnapshot],
    free_balances: Mapping[str, Any],
    candidate_assets: tuple[str, ...],
    *,
    settings: ClaudeSpotSettings,
) -> list[ClaudeRouteCandidate]:
    candidates: list[ClaudeRouteCandidate] = []
    source_assets = [
        asset
        for asset, amount in sorted(free_balances.items())
        if _positive_float(amount) is not None
        and _asset_scope_block_reason(str(asset), settings) is None
    ]
    for source in source_assets:
        amount = _positive_float(free_balances.get(source))
        if amount is None:
            continue
        for destination in candidate_assets:
            if destination == source:
                continue
            route = await routes.quote(
                source,
                destination,
                amount,
                apply_bnb_discount=False,
            )
            if route is None or not route.legs:
                continue
            if _route_scope_block_reason(route, settings) is not None:
                continue
            estimated_output_usdt = None
            if destination == "USDT":
                estimated_output_usdt = route.output_amount
            else:
                usdt_rate = routes.indicative_rate(destination, "USDT")
                if usdt_rate is not None:
                    estimated_output_usdt = route.output_amount * usdt_rate
            source_value_usdt = _source_value_usdt(
                source,
                amount,
                routes,
            )
            immediate_friction_bps = _immediate_friction_bps(
                source_value_usdt,
                estimated_output_usdt,
            )
            if _route_friction_exceeds_allowance(immediate_friction_bps, settings):
                continue
            market = _route_focus_market(route, markets)
            market_pick_score, market_pick_reason = _route_market_pick_score(
                route,
                market,
                source_value_usdt=source_value_usdt,
                immediate_friction_bps=immediate_friction_bps,
                settings=settings,
            )
            candidates.append(
                ClaudeRouteCandidate(
                    route_id=_route_id(route),
                    route=route,
                    estimated_output_usdt=estimated_output_usdt,
                    source_value_usdt=source_value_usdt,
                    immediate_friction_bps=immediate_friction_bps,
                    market_pick_score=market_pick_score,
                    market_pick_reason=market_pick_reason,
                    quote_volume_usdt=(
                        market.quote_volume_usdt if market is not None else None
                    ),
                    spread_bps=market.spread_bps if market is not None else None,
                    liquidity_tier=_liquidity_tier(
                        market.quote_volume_usdt if market is not None else None
                    ),
                )
            )
    if settings.market_pick_mode == CLAUDE_SPOT_MARKET_PICK_SMALL:
        candidates.sort(
            key=lambda candidate: (
                candidate.market_pick_score,
                -(candidate.immediate_friction_bps or 0.0),
                -len(candidate.route.legs),
                candidate.route.destination_asset,
            ),
            reverse=True,
        )
    else:
        candidates.sort(
            key=lambda candidate: (
                candidate.estimated_output_usdt is not None,
                candidate.estimated_output_usdt or 0.0,
                -len(candidate.route.legs),
                candidate.route.destination_asset,
            ),
            reverse=True,
        )
    limit = settings.effective_candidate_limit
    if limit > 0:
        candidates = candidates[:limit]
    return candidates


async def _enrich_route_candidates_with_evidence(
    client: Any,
    candidates: list[ClaudeRouteCandidate],
    markets: Mapping[str, MarketSnapshot],
    settings: ClaudeSpotSettings,
) -> list[ClaudeRouteCandidate]:
    evidence_cache: dict[str, dict[str, Any]] = {}

    async def evidence_for(asset: str) -> dict[str, Any]:
        normalized = asset.strip().upper()
        if normalized in evidence_cache:
            return evidence_cache[normalized]
        if (
            settings.market_evidence_max_symbols > 0
            and len(evidence_cache) >= settings.market_evidence_max_symbols
        ):
            evidence_cache[normalized] = {
                "asset": normalized,
                "symbol": "",
                "status": "unavailable",
                "reason": "market evidence symbol cap reached",
            }
            return evidence_cache[normalized]
        evidence_cache[normalized] = await _asset_market_evidence(
            client,
            normalized,
            markets,
            settings,
        )
        return evidence_cache[normalized]

    enriched: list[ClaudeRouteCandidate] = []
    for candidate in candidates:
        source = await evidence_for(candidate.route.source_asset)
        destination = await evidence_for(candidate.route.destination_asset)
        enriched.append(
            replace(
                candidate,
                market_evidence=_route_market_evidence(
                    candidate,
                    source,
                    destination,
                    settings,
                ),
            )
        )
    return enriched


async def _asset_market_evidence(
    client: Any,
    asset: str,
    markets: Mapping[str, MarketSnapshot],
    settings: ClaudeSpotSettings,
) -> dict[str, Any]:
    if asset in _LOW_UPSIDE_ASSETS:
        return _stable_asset_evidence(asset)
    symbol = _asset_history_symbol(asset, markets)
    if symbol is None:
        return {
            "asset": asset,
            "symbol": "",
            "status": "unavailable",
            "reason": "no eligible history symbol found",
        }
    try:
        rows = await _fetch_ohlcv_rows(
            client,
            symbol,
            settings.market_evidence_timeframe,
            _market_evidence_start_time(settings),
            datetime.now(timezone.utc),
        )
    except Exception as exc:
        return {
            "asset": asset,
            "symbol": symbol,
            "status": "unavailable",
            "reason": f"history fetch failed: {exc}",
        }
    short_rows: list[Any] = []
    short_error = ""
    try:
        end_time = datetime.now(timezone.utc)
        short_rows = await _fetch_ohlcv_rows(
            client,
            symbol,
            CLAUDE_SPOT_SHORT_EVIDENCE_TIMEFRAME,
            end_time - timedelta(minutes=CLAUDE_SPOT_SHORT_EVIDENCE_LOOKBACK_MINUTES),
            end_time,
        )
    except Exception as exc:
        short_error = str(exc)
    return _ohlcv_evidence(
        asset,
        symbol,
        rows,
        short_rows=short_rows,
        short_error=short_error,
    )


async def _fetch_ohlcv_rows(
    client: Any,
    symbol: str,
    timeframe: str,
    start_time: datetime,
    end_time: datetime,
) -> list[Any]:
    fetch_ohlcv = getattr(client, "fetch_ohlcv", None)
    if callable(fetch_ohlcv):
        return await fetch_ohlcv(
            symbol,
            timeframe,
            start_time,
            end_time,
        )
    return await HistoryFetcher(client).fetch_ohlcv(
        symbol,
        timeframe,
        start_time,
        end_time,
    )


def _market_evidence_start_time(settings: ClaudeSpotSettings) -> datetime:
    lookback_hours = max(settings.market_evidence_lookback_hours, 168)
    return datetime.now(timezone.utc) - timedelta(hours=lookback_hours + 2)


def _asset_history_symbol(
    asset: str,
    markets: Mapping[str, MarketSnapshot],
) -> str | None:
    normalized = asset.strip().upper()
    for quote in ("USDT", "FDUSD", "USDC", "USD1", "BTC", "BNB", "ETH"):
        if normalized == quote:
            continue
        symbol = f"{normalized}/{quote}"
        market = markets.get(symbol)
        if market is not None and market.eligible:
            return symbol
    fallback = next(
        (
            market.symbol
            for market in markets.values()
            if market.eligible and market.base.strip().upper() == normalized
        ),
        None,
    )
    return fallback


def _stable_asset_evidence(asset: str) -> dict[str, Any]:
    return {
        "asset": asset,
        "symbol": asset,
        "status": "ready",
        "reason": "stable or low-upside benchmark asset; modeled as flat",
        "candles": 0,
        "returns_bps": {name: 0.0 for name, _ in CLAUDE_SPOT_EVIDENCE_WINDOWS},
        "short_returns_bps": {
            name: 0.0 for name, _ in CLAUDE_SPOT_SHORT_EVIDENCE_WINDOWS
        },
        "short_status": "ready",
        "short_timeframe": CLAUDE_SPOT_SHORT_EVIDENCE_TIMEFRAME,
        "short_candles": 0,
        "hourly_volatility_bps": 0.0,
        "volume_24h": None,
        "previous_volume_24h": None,
        "volume_change_percent": None,
    }


def _ohlcv_evidence(
    asset: str,
    symbol: str,
    rows: list[Any],
    *,
    short_rows: list[Any] | None = None,
    short_error: str = "",
) -> dict[str, Any]:
    candles = _clean_ohlcv_rows(rows)
    short_candles = _clean_ohlcv_rows(short_rows or [])
    short_returns = _short_window_returns_bps(short_candles)
    short_status = "ready" if _short_returns_ready(short_returns) else "insufficient"
    short_reason = (
        "compact minute evidence ready"
        if short_status == "ready"
        else "not enough minute candles for 5m/15m evidence"
    )
    if short_error:
        short_status = "unavailable"
        short_reason = f"short history fetch failed: {short_error}"
    returns: dict[str, float] = {}
    for name, hours in CLAUDE_SPOT_EVIDENCE_WINDOWS:
        value = _window_return_bps(candles, hours)
        if value is not None:
            returns[name] = value
    if set(returns) != {name for name, _ in CLAUDE_SPOT_EVIDENCE_WINDOWS}:
        return {
            "asset": asset,
            "symbol": symbol,
            "status": "insufficient",
            "reason": "not enough hourly candles for compact evidence windows",
            "candles": len(candles),
            "returns_bps": returns,
            "short_returns_bps": short_returns,
            "short_status": short_status,
            "short_reason": short_reason,
            "short_timeframe": CLAUDE_SPOT_SHORT_EVIDENCE_TIMEFRAME,
            "short_candles": len(short_candles),
        }
    hourly_returns = [
        (current["close"] / previous["close"]) - 1
        for previous, current in zip(candles, candles[1:])
        if previous["close"] > 0
    ]
    volume_24h = _sum_recent_volume(candles, 24)
    previous_volume_24h = _sum_previous_volume(candles, 24)
    volume_change_percent = None
    if previous_volume_24h is not None and previous_volume_24h > 0:
        volume_change_percent = ((volume_24h or 0.0) / previous_volume_24h - 1) * 100
    return {
        "asset": asset,
        "symbol": symbol,
        "status": "ready",
        "reason": "compact hourly evidence ready",
        "candles": len(candles),
        "returns_bps": returns,
        "short_returns_bps": short_returns,
        "short_status": short_status,
        "short_reason": short_reason,
        "short_timeframe": CLAUDE_SPOT_SHORT_EVIDENCE_TIMEFRAME,
        "short_candles": len(short_candles),
        "hourly_volatility_bps": (
            statistics.pstdev(hourly_returns) * 10_000
            if len(hourly_returns) > 1
            else 0.0
        ),
        "volume_24h": volume_24h,
        "previous_volume_24h": previous_volume_24h,
        "volume_change_percent": volume_change_percent,
    }


def _clean_ohlcv_rows(rows: list[Any]) -> list[dict[str, float]]:
    candles: list[dict[str, float]] = []
    for row in rows:
        if not isinstance(row, (list, tuple)) or len(row) < 6:
            continue
        try:
            timestamp = float(row[0])
            close = float(row[4])
            volume = float(row[5])
        except (TypeError, ValueError):
            continue
        if not all(math.isfinite(value) for value in (timestamp, close, volume)):
            continue
        if close <= 0 or volume < 0:
            continue
        candles.append({"timestamp": timestamp, "close": close, "volume": volume})
    candles.sort(key=lambda candle: candle["timestamp"])
    return candles


def _window_return_bps(candles: list[dict[str, float]], hours: int) -> float | None:
    if len(candles) < hours + 1:
        return None
    start = candles[-(hours + 1)]["close"]
    end = candles[-1]["close"]
    if start <= 0:
        return None
    return (end / start - 1) * 10_000


def _short_window_returns_bps(candles: list[dict[str, float]]) -> dict[str, float]:
    returns: dict[str, float] = {}
    for name, minutes in CLAUDE_SPOT_SHORT_EVIDENCE_WINDOWS:
        value = _window_return_bps(candles, minutes)
        if value is not None:
            returns[name] = value
    return returns


def _short_returns_ready(returns: Mapping[str, Any]) -> bool:
    return set(returns) == {name for name, _ in CLAUDE_SPOT_SHORT_EVIDENCE_WINDOWS}


def _sum_recent_volume(candles: list[dict[str, float]], hours: int) -> float | None:
    if len(candles) < hours:
        return None
    return sum(candle["volume"] for candle in candles[-hours:])


def _sum_previous_volume(candles: list[dict[str, float]], hours: int) -> float | None:
    if len(candles) < hours * 2:
        return None
    return sum(candle["volume"] for candle in candles[-hours * 2 : -hours])


def _route_market_evidence(
    candidate: ClaudeRouteCandidate,
    source: Mapping[str, Any],
    destination: Mapping[str, Any],
    settings: ClaudeSpotSettings,
) -> dict[str, Any]:
    friction_bps = max(candidate.immediate_friction_bps or 0.0, 0.0)
    base = {
        "source": dict(source),
        "destination": dict(destination),
        "route_friction_bps": friction_bps,
        "minimum_expected_edge_bps": settings.market_evidence_min_edge_bps,
        "required_relative_move_bps": (
            friction_bps + settings.market_evidence_min_edge_bps
        ),
        "speculative_mode_enabled": settings.speculative_mode_enabled,
        "speculative_min_edge_bps": settings.speculative_min_edge_bps,
        "speculative_require_thesis": settings.speculative_require_thesis,
    }
    if source.get("status") != "ready" or destination.get("status") != "ready":
        return {
            **base,
            "status": "insufficient",
            "gate_pass": False,
            "gate_reason": "source or destination market evidence is not ready",
        }
    relative_momentum_bps = _weighted_relative_momentum_bps(source, destination)
    if relative_momentum_bps is None:
        return {
            **base,
            "status": "insufficient",
            "gate_pass": False,
            "gate_reason": "missing return windows for relative momentum",
        }
    short_relative_momentum_bps = _weighted_short_relative_momentum_bps(
        source,
        destination,
    )
    expected_edge_bps = relative_momentum_bps - friction_bps
    gate_pass = expected_edge_bps >= settings.market_evidence_min_edge_bps
    speculative_floor_pass = expected_edge_bps >= settings.speculative_min_edge_bps
    return {
        **base,
        "status": "ready",
        "relative_momentum_bps": relative_momentum_bps,
        "short_relative_momentum_bps": short_relative_momentum_bps,
        "expected_edge_after_friction_bps": expected_edge_bps,
        "gate_pass": gate_pass,
        "speculative_floor_pass": speculative_floor_pass,
        "gate_reason": (
            "expected edge clears strict evidence minimum"
            if gate_pass
            else "expected edge does not clear strict evidence minimum"
        ),
    }


def _weighted_relative_momentum_bps(
    source: Mapping[str, Any],
    destination: Mapping[str, Any],
) -> float | None:
    long_delta = _weighted_returns_delta(
        source,
        destination,
        "returns_bps",
        CLAUDE_SPOT_EVIDENCE_WEIGHTS,
        require_all=True,
    )
    if long_delta is None:
        return None
    short_delta = _weighted_returns_delta(
        source,
        destination,
        "short_returns_bps",
        CLAUDE_SPOT_SHORT_EVIDENCE_WEIGHTS,
        require_all=False,
    )
    weighted_total, weight_total = long_delta
    if short_delta is not None:
        weighted_total += short_delta[0]
        weight_total += short_delta[1]
    if weight_total <= 0:
        return None
    return weighted_total / weight_total


def _weighted_short_relative_momentum_bps(
    source: Mapping[str, Any],
    destination: Mapping[str, Any],
) -> float | None:
    delta = _weighted_returns_delta(
        source,
        destination,
        "short_returns_bps",
        CLAUDE_SPOT_SHORT_EVIDENCE_WEIGHTS,
        require_all=False,
    )
    if delta is None or delta[1] <= 0:
        return None
    return delta[0] / delta[1]


def _weighted_returns_delta(
    source: Mapping[str, Any],
    destination: Mapping[str, Any],
    returns_key: str,
    weights: Mapping[str, float],
    *,
    require_all: bool,
) -> tuple[float, float] | None:
    source_returns = source.get(returns_key)
    destination_returns = destination.get(returns_key)
    if not isinstance(source_returns, Mapping) or not isinstance(destination_returns, Mapping):
        return None if require_all else (0.0, 0.0)
    weighted_total = 0.0
    weight_total = 0.0
    for name, weight in weights.items():
        source_value = _finite_float_or_none(source_returns.get(name))
        destination_value = _finite_float_or_none(destination_returns.get(name))
        if source_value is None or destination_value is None:
            if require_all:
                return None
            continue
        weighted_total += (destination_value - source_value) * weight
        weight_total += weight
    if require_all and weight_total <= 0:
        return None
    return weighted_total, weight_total


async def _execute_route(
    client: Any,
    settings: ClaudeSpotSettings,
    route: RouteQuote,
    markets: Mapping[str, MarketSnapshot],
) -> list[ClaudeSpotLegResult]:
    if len(route.legs) > settings.max_route_legs:
        raise ClaudeSpotError("selected route exceeds configured max_route_legs")
    _assert_route_scope_allowed(route, settings)
    submitted: list[ClaudeSpotLegResult] = []
    fees = CommissionService(client, cache_seconds=settings.fee_cache_seconds)
    for leg in route.legs:
        market = markets.get(leg.symbol)
        if market is None or not market.eligible:
            raise ClaudeSpotError(f"selected route leg is no longer eligible: {leg.symbol}")
        before = _sanitize_balance(await client.get_balance())
        free_before = before.get("free", {})
        source_before = _balance_amount(free_before, leg.source_asset)
        destination_before = _balance_amount(free_before, leg.destination_asset)
        if source_before <= 0:
            raise ClaudeSpotError(f"no free {leg.source_asset} balance for route leg")
        try:
            order_amount = await _live_leg_order_amount(
                fees,
                market,
                leg.side,
                source_before,
                settings=settings,
            )
            await ensure_spot_order_test_allowed(
                client,
                symbol=leg.symbol,
                side=leg.side,
                order_type="market",
                quantity=order_amount,
            )
        except Exception as exc:
            if submitted:
                raise ClaudeSpotExecutionError(
                    f"Claude Spot preflight failed for {leg.symbol}; route halted",
                    submitted,
                ) from exc
            raise
        try:
            order = await client.create_order(
                leg.symbol,
                "market",
                leg.side,
                order_amount,
            )
        except Exception as exc:
            raise ClaudeSpotExecutionError(
                f"Claude Spot order failed for {leg.symbol}; route halted",
                submitted,
            ) from exc
        final_order = await _final_order_status(client, leg.symbol, order)
        status = str(final_order.get("status") or order.get("status") or "").lower()
        after = _sanitize_balance(await client.get_balance())
        free_after = after.get("free", {})
        source_after = _balance_amount(free_after, leg.source_asset)
        destination_after = _balance_amount(free_after, leg.destination_asset)
        result = ClaudeSpotLegResult(
            symbol=leg.symbol,
            side=leg.side,
            source_asset=leg.source_asset,
            destination_asset=leg.destination_asset,
            order_amount=order_amount,
            source_free_before=source_before,
            destination_free_before=destination_before,
            source_free_after=source_after,
            destination_free_after=destination_after,
            order=_sanitize_order(order),
            final_order=_sanitize_order(final_order),
            status=status,
        )
        submitted.append(result)
        if status not in {"closed", "filled"}:
            raise ClaudeSpotExecutionError(
                f"Claude Spot route halted after {leg.symbol}: {status}",
                submitted,
            )
        if destination_after <= destination_before:
            raise ClaudeSpotExecutionError(
                f"Claude Spot route did not increase {leg.destination_asset} after {leg.symbol}",
                submitted,
            )
    return submitted


def _preflight(settings: ClaudeSpotSettings, *, submit_order: bool) -> None:
    _ensure_no_emergency_stop(settings.emergency_stop_path)
    if not submit_order:
        return
    if not settings.enabled:
        raise ClaudeSpotError("CLAUDE_SPOT_ENABLED=true is required for live submission")
    if settings.dynamic_scope != "all_eligible_spot":
        raise ClaudeSpotError(
            "CLAUDE_SPOT_DYNAMIC_SCOPE=all_eligible_spot is required for dynamic live scope"
        )
    if settings.use_testnet:
        raise ClaudeSpotError("Claude Spot live submission requires USE_TESTNET=false")
    if settings.dry_run:
        raise ClaudeSpotError("Claude Spot live submission requires DRY_RUN=false")
    if settings.confirm_live_trading != "yes":
        raise ClaudeSpotError("Claude Spot live submission requires CONFIRM_LIVE_TRADING=yes")
    if settings.require_proven_edge and os.getenv("CLAUDE_SPOT_PROVEN_EDGE", "").strip().lower() != "yes":
        raise ClaudeSpotError(
            "Claude Spot live submission requires CLAUDE_SPOT_PROVEN_EDGE=yes. "
            "Run scripts/claude_spot_backtest.py and only set this when the harness "
            "shows positive risk-adjusted edge net of fees. Until then, live submission "
            "is paper-only."
        )


async def _ensure_live_permission(
    client: Any,
    settings: ClaudeSpotSettings,
    route: RouteQuote,
    markets: Mapping[str, MarketSnapshot],
) -> None:
    try:
        report = await inspect_api_permissions(
            settings.legacy_settings(),
            client=client,
            include_spot_order_test=False,
            feature_names=("claude_spot_autonomous",),
        )
        reading = report.capability(READING)
        if reading.status != STATUS_OK:
            raise ClaudeSpotError(f"account reading is not allowed: {reading.detail}")
        if report.can_trade is not True:
            raise ClaudeSpotError("Binance account canTrade is not true")
        if not route.legs:
            raise ClaudeSpotError("selected route has no executable legs")
        _assert_route_scope_allowed(route, settings)
        first_leg = route.legs[0]
        market = markets.get(first_leg.symbol)
        if market is None or not market.eligible:
            raise ClaudeSpotError(
                f"selected route first leg is no longer eligible: {first_leg.symbol}"
            )
        balance = _sanitize_balance(await client.get_balance())
        source_free = _balance_amount(balance.get("free", {}), first_leg.source_asset)
        fees = CommissionService(client, cache_seconds=settings.fee_cache_seconds)
        order_amount = await _live_leg_order_amount(
            fees,
            market,
            first_leg.side,
            source_free,
            settings=settings,
        )
        await ensure_spot_order_test_allowed(
            client,
            symbol=first_leg.symbol,
            side=first_leg.side,
            order_type="market",
            quantity=order_amount,
        )
    except ApiPermissionError as exc:
        raise ClaudeSpotError(str(exc)) from exc


def _select_candidate(
    decision: ClaudeSpotDecision,
    candidates: tuple[ClaudeRouteCandidate, ...],
) -> ClaudeRouteCandidate | None:
    if decision.action != "rebalance":
        return None
    return next(
        (candidate for candidate in candidates if candidate.route_id == decision.route_id),
        None,
    )


def _validate_rebalance_decision(
    decision: ClaudeSpotDecision,
    candidate: ClaudeRouteCandidate,
) -> None:
    route = candidate.route
    if decision.source_asset != route.source_asset:
        raise ClaudeSpotError("Claude source_asset does not match selected route")
    if decision.destination_asset != route.destination_asset:
        raise ClaudeSpotError("Claude destination_asset does not match selected route")
    if decision.spend_amount is not None and not math.isclose(
        decision.spend_amount,
        route.input_amount,
        rel_tol=1e-9,
        abs_tol=1e-12,
    ):
        raise ClaudeSpotError(
            "Claude Spot v1 only accepts spend_amount equal to route input_amount"
        )


def _route_scope_context(settings: ClaudeSpotSettings) -> dict[str, Any]:
    return {
        "allowed_symbols": list(settings.allowed_symbols),
        "allowed_assets": list(settings.allowed_assets),
        "blocked_symbols": list(settings.blocked_symbols),
        "blocked_assets": list(settings.blocked_assets),
        "blocked_quotes": list(settings.blocked_quotes),
        "block_fiat_assets": settings.block_fiat_assets,
        "max_immediate_friction_bps": settings.allow_negative_friction_bps,
        "min_trade_interval_seconds": settings.min_trade_interval_seconds,
        "max_submitted_trades_per_day": settings.max_submitted_trades_per_day,
        "halt_after_failed_submission": settings.halt_after_failed_submission,
        "market_evidence_enabled": settings.market_evidence_enabled,
        "require_market_evidence_for_live": settings.require_market_evidence_for_live,
        "market_evidence_min_edge_bps": settings.market_evidence_min_edge_bps,
        "market_evidence_short_timeframe": CLAUDE_SPOT_SHORT_EVIDENCE_TIMEFRAME,
        "market_evidence_short_windows": [
            name for name, _ in CLAUDE_SPOT_SHORT_EVIDENCE_WINDOWS
        ],
        "speculative_mode_enabled": settings.speculative_mode_enabled,
        "speculative_min_edge_bps": settings.speculative_min_edge_bps,
        "speculative_require_thesis": settings.speculative_require_thesis,
        "market_evidence_lookback_hours": settings.market_evidence_lookback_hours,
        "market_evidence_max_symbols": settings.market_evidence_max_symbols,
        "market_evidence_timeframe": settings.market_evidence_timeframe,
        "trade_memory_limit": settings.trade_memory_limit,
        "max_recent_loss_streak": settings.max_recent_loss_streak,
    }


def _selected_route_context(
    candidate: ClaudeRouteCandidate,
    settings: ClaudeSpotSettings,
    decision: ClaudeSpotDecision,
) -> dict[str, Any]:
    payload = candidate.as_context()
    payload["speculative_gate"] = _speculative_gate_context(
        settings,
        candidate,
        decision,
    )
    return payload


def _speculative_gate_context(
    settings: ClaudeSpotSettings,
    candidate: ClaudeRouteCandidate,
    decision: ClaudeSpotDecision,
) -> dict[str, Any]:
    evidence = candidate.market_evidence if isinstance(candidate.market_evidence, Mapping) else {}
    edge = _finite_float_or_none(evidence.get("expected_edge_after_friction_bps"))
    required = _finite_float_or_none(evidence.get("minimum_expected_edge_bps"))
    evidence_status = str(evidence.get("status") or "")
    normal_gate_pass = evidence.get("gate_pass") is True
    floor_pass = edge is not None and edge >= settings.speculative_min_edge_bps
    thesis_present = _speculative_thesis_present(decision.reason)
    payload: dict[str, Any] = {
        "enabled": settings.speculative_mode_enabled,
        "min_edge_bps": settings.speculative_min_edge_bps,
        "require_thesis": settings.speculative_require_thesis,
        "normal_gate_pass": normal_gate_pass,
        "evidence_status": evidence_status,
        "expected_edge_after_friction_bps": edge,
        "minimum_expected_edge_bps": required,
        "floor_pass": floor_pass,
        "thesis_present": thesis_present,
        "allowed": False,
        "reason": "",
    }
    if normal_gate_pass:
        payload["reason"] = "normal evidence gate already passed"
        return payload
    if not settings.speculative_mode_enabled:
        payload["reason"] = "speculative gate disabled"
        return payload
    if evidence_status != "ready":
        payload["reason"] = "market evidence is not ready"
        return payload
    if edge is None:
        payload["reason"] = "expected edge is unavailable"
        return payload
    if not floor_pass:
        payload["reason"] = "expected edge is below speculative floor"
        return payload
    if settings.speculative_require_thesis and not thesis_present:
        payload["reason"] = "speculative thesis missing"
        return payload
    payload["allowed"] = True
    payload["reason"] = "speculative gate allows below-normal-edge route"
    return payload


def _speculative_thesis_present(reason: str) -> bool:
    text = _compact_text(reason).lower()
    return any(keyword in text for keyword in SPECULATIVE_THESIS_KEYWORDS)


def _market_evidence_hold_reason(
    settings: ClaudeSpotSettings,
    context: ClaudeDecisionContext,
    candidate: ClaudeRouteCandidate,
    decision: ClaudeSpotDecision,
) -> str | None:
    if not settings.require_market_evidence_for_live:
        return None
    if not settings.market_evidence_enabled:
        return "market evidence gate is required for live submission; no order submitted"
    memory_reason = _trade_memory_live_block_reason(context.trade_memory, settings)
    if memory_reason is not None:
        return memory_reason
    evidence = candidate.market_evidence
    if not evidence:
        return "selected route has no market evidence packet; no order submitted"
    if evidence.get("status") != "ready":
        reason = str(
            evidence.get("gate_reason") or evidence.get("reason") or "not ready"
        )
        return f"selected route market evidence is insufficient: {reason}; no order submitted"
    if evidence.get("gate_pass") is not True:
        speculative_gate = _speculative_gate_context(settings, candidate, decision)
        if speculative_gate.get("allowed") is True:
            return None
        edge = _finite_float_or_none(evidence.get("expected_edge_after_friction_bps"))
        required = _finite_float_or_none(evidence.get("minimum_expected_edge_bps"))
        speculative_reason = ""
        if settings.speculative_mode_enabled:
            speculative_reason = (
                f"; speculative gate rejected: {speculative_gate.get('reason')}"
            )
        if edge is not None and required is not None:
            return (
                "market evidence gate rejected selected route: "
                f"expected edge {edge:.2f} bps is below required {required:.2f} bps; "
                f"no order submitted{speculative_reason}"
            )
        return (
            "market evidence gate rejected selected route; "
            f"no order submitted{speculative_reason}"
        )
    return None


def _trade_memory_live_block_reason(
    trade_memory: Mapping[str, Any],
    settings: ClaudeSpotSettings,
) -> str | None:
    if Path("data/go_off.flag").exists():
        return None
    if trade_memory.get("status") == "unavailable":
        return (
            "could not read Claude Spot trade memory; holding until audit history "
            "is available"
        )
    loss_streak = int(trade_memory.get("recent_loss_streak") or 0)
    if (
        settings.max_recent_loss_streak > 0
        and loss_streak >= settings.max_recent_loss_streak
    ):
        return (
            "recent Claude Spot loss streak reached "
            f"{loss_streak}/{settings.max_recent_loss_streak}; no order submitted"
        )
    return None


def _trade_discipline_hold_reason(settings: ClaudeSpotSettings) -> str | None:
    if Path("data/go_off.flag").exists():
        return None
    rows, read_error = _load_audit_rows(Path(settings.audit_path))
    if read_error:
        return f"could not read Claude Spot audit history: {read_error}; no order submitted"
    if not rows:
        return None
    now = datetime.now(timezone.utc)
    last_mutating_index = next(
        (
            index
            for index in range(len(rows) - 1, -1, -1)
            if _audit_row_attempted_submission(rows[index])
        ),
        None,
    )
    if (
        settings.halt_after_failed_submission
        and last_mutating_index is not None
        and str(rows[last_mutating_index].get("status") or "").lower() == "failed"
        and not any(
            _audit_row_failed_submission_reviewed(row)
            for row in rows[last_mutating_index + 1 :]
        )
    ):
        return (
            "previous live route failed after submitting at least one order; "
            "holding until operator review"
        )
    last_submitted_at = next(
        (
            _audit_row_timestamp(row)
            for row in reversed(rows)
            if str(row.get("status") or "").lower() == "submitted"
            and _audit_row_attempted_submission(row)
        ),
        None,
    )
    if (
        settings.min_trade_interval_seconds > 0
        and last_submitted_at is not None
    ):
        elapsed = max((now - last_submitted_at).total_seconds(), 0.0)
        if elapsed < settings.min_trade_interval_seconds:
            remaining = settings.min_trade_interval_seconds - elapsed
            return (
                "trade cooldown active after the last submitted rebalance; "
                f"{remaining:.0f} seconds remain; no order submitted"
            )
    if settings.max_submitted_trades_per_day > 0:
        submitted_today = 0
        for row in rows:
            if not _audit_row_attempted_submission(row):
                continue
            timestamp = _audit_row_timestamp(row)
            if timestamp is not None and timestamp.date() == now.date():
                submitted_today += 1
        if submitted_today >= settings.max_submitted_trades_per_day:
            return (
                "daily submitted-trade cap reached "
                f"({submitted_today}/{settings.max_submitted_trades_per_day}); "
                "no order submitted"
            )
    return None


def _load_audit_rows(path: Path) -> tuple[list[Mapping[str, Any]], str]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except FileNotFoundError:
        return [], ""
    except OSError as exc:
        return [], str(exc)
    rows: list[Mapping[str, Any]] = []
    for line in lines[-500:]:
        try:
            payload = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(payload, Mapping):
            rows.append(payload)
    return rows, ""


def _trade_memory_context(
    rows: list[Mapping[str, Any]],
    free_balances: Mapping[str, Any],
    routes: RouteEngine,
    settings: ClaudeSpotSettings,
    *,
    read_error: str = "",
) -> dict[str, Any]:
    if read_error:
        return {
            "status": "unavailable",
            "reason": read_error,
            "recent_trade_count": 0,
            "recent_loss_streak": 0,
            "recent_outcomes": [],
        }
    submitted = [
        row
        for row in rows
        if str(row.get("status") or "").lower() == "submitted"
        and _audit_row_attempted_submission(row)
    ]
    if not submitted:
        return {
            "status": "empty",
            "reason": "no submitted Claude Spot routes in audit history",
            "recent_trade_count": 0,
            "recent_loss_streak": 0,
            "recent_outcomes": [],
        }
    outcomes = _trade_memory_outcomes(submitted, free_balances, routes)
    limit = settings.trade_memory_limit
    recent = outcomes if limit <= 0 else outcomes[-limit:]
    return {
        "status": "ready",
        "recent_trade_count": len(submitted),
        "recent_loss_streak": _recent_loss_streak(recent),
        "current_holding": _current_holding_memory(
            submitted[-1],
            free_balances,
            routes,
        ),
        "recent_outcomes": recent,
    }


def _trade_memory_outcomes(
    submitted: list[Mapping[str, Any]],
    free_balances: Mapping[str, Any],
    routes: RouteEngine,
) -> list[dict[str, Any]]:
    outcomes: list[dict[str, Any]] = []
    for index, row in enumerate(submitted):
        destination = _row_destination_asset(row)
        entry_value = _row_estimated_output_usdt(row)
        exit_value = _next_exit_value_usdt(destination, submitted[index + 1 :])
        outcome_status = "unknown"
        pnl_bps = None
        current_value = None
        if entry_value is not None and exit_value is not None:
            outcome_status = "closed"
            pnl_bps = (exit_value / entry_value - 1) * 10_000
        elif entry_value is not None and destination:
            amount = _positive_float(free_balances.get(destination))
            rate = routes.indicative_rate(destination, "USDT") if amount is not None else None
            if amount is not None and rate is not None:
                current_value = amount * rate
                outcome_status = "open"
                pnl_bps = (current_value / entry_value - 1) * 10_000
        outcomes.append(
            {
                "timestamp": row.get("timestamp", ""),
                "route_id": _row_route_id(row),
                "source_asset": _row_source_asset(row),
                "destination_asset": destination,
                "entry_value_usdt": entry_value,
                "exit_value_usdt": exit_value,
                "current_value_usdt": current_value,
                "pnl_bps": pnl_bps,
                "status": outcome_status,
                "route_friction_bps": _row_immediate_friction_bps(row),
            }
        )
    return outcomes


def _current_holding_memory(
    last_submitted: Mapping[str, Any],
    free_balances: Mapping[str, Any],
    routes: RouteEngine,
) -> dict[str, Any]:
    destination = _row_destination_asset(last_submitted)
    if not destination:
        return {"status": "unknown", "reason": "last route destination is unknown"}
    amount = _positive_float(free_balances.get(destination))
    entry_value = _row_estimated_output_usdt(last_submitted)
    if amount is None:
        return {
            "status": "not_current",
            "asset": destination,
            "reason": "last route destination is not a positive free balance",
        }
    rate = routes.indicative_rate(destination, "USDT")
    current_value = amount * rate if rate is not None else None
    pnl_bps = None
    if entry_value is not None and current_value is not None:
        pnl_bps = (current_value / entry_value - 1) * 10_000
    return {
        "status": "open",
        "asset": destination,
        "amount": amount,
        "entry_value_usdt": entry_value,
        "current_value_usdt": current_value,
        "unrealized_pnl_bps": pnl_bps,
        "entered_at": last_submitted.get("timestamp", ""),
        "route_id": _row_route_id(last_submitted),
    }


def _next_exit_value_usdt(
    destination: str,
    later_rows: list[Mapping[str, Any]],
) -> float | None:
    if not destination:
        return None
    for row in later_rows:
        if _row_source_asset(row) == destination:
            return _row_source_value_usdt(row)
    return None


def _recent_loss_streak(outcomes: list[Mapping[str, Any]]) -> int:
    streak = 0
    for outcome in reversed(outcomes):
        pnl_bps = _finite_float_or_none(outcome.get("pnl_bps"))
        if pnl_bps is None:
            break
        if pnl_bps < 0:
            streak += 1
            continue
        break
    return streak


def _row_route_id(row: Mapping[str, Any]) -> str:
    decision = row.get("decision")
    if isinstance(decision, Mapping):
        route_id = decision.get("route_id")
        if isinstance(route_id, str):
            return route_id
    selected = row.get("selected_route")
    if isinstance(selected, Mapping):
        route_id = selected.get("route_id")
        if isinstance(route_id, str):
            return route_id
    return ""


def _row_source_asset(row: Mapping[str, Any]) -> str:
    return _row_asset(row, "source_asset")


def _row_destination_asset(row: Mapping[str, Any]) -> str:
    return _row_asset(row, "destination_asset")


def _row_asset(row: Mapping[str, Any], field_name: str) -> str:
    decision = row.get("decision")
    if isinstance(decision, Mapping):
        value = decision.get(field_name)
        if isinstance(value, str) and value.strip():
            return value.strip().upper()
    selected = row.get("selected_route")
    if isinstance(selected, Mapping):
        value = selected.get(field_name)
        if isinstance(value, str) and value.strip():
            return value.strip().upper()
    return ""


def _row_selected_route(row: Mapping[str, Any]) -> Mapping[str, Any]:
    selected = row.get("selected_route")
    return selected if isinstance(selected, Mapping) else {}


def _row_estimated_output_usdt(row: Mapping[str, Any]) -> float | None:
    return _finite_float_or_none(_row_selected_route(row).get("estimated_output_usdt"))


def _row_source_value_usdt(row: Mapping[str, Any]) -> float | None:
    return _finite_float_or_none(_row_selected_route(row).get("source_value_usdt"))


def _row_immediate_friction_bps(row: Mapping[str, Any]) -> float | None:
    return _finite_float_or_none(_row_selected_route(row).get("immediate_friction_bps"))


def _audit_row_attempted_submission(row: Mapping[str, Any]) -> bool:
    return bool(row.get("submission_attempted")) or bool(row.get("submitted_legs"))


def _audit_row_failed_submission_reviewed(row: Mapping[str, Any]) -> bool:
    if str(row.get("status") or "").lower() not in {"reviewed", "operator_reviewed"}:
        return False
    review = row.get("operator_review")
    return isinstance(review, Mapping) and review.get("failed_submission_reviewed") is True


def _audit_row_timestamp(row: Mapping[str, Any]) -> datetime | None:
    raw = row.get("timestamp")
    if not isinstance(raw, str) or not raw.strip():
        return None
    try:
        timestamp = datetime.fromisoformat(raw.strip().replace("Z", "+00:00"))
    except ValueError:
        return None
    if timestamp.tzinfo is None:
        timestamp = timestamp.replace(tzinfo=timezone.utc)
    return timestamp.astimezone(timezone.utc)


def _scoped_markets(
    markets: Mapping[str, MarketSnapshot],
    settings: ClaudeSpotSettings,
) -> dict[str, MarketSnapshot]:
    return {
        symbol: market
        for symbol, market in markets.items()
        if _market_scope_block_reason(market, settings) is None
    }


def _assert_route_scope_allowed(
    route: RouteQuote,
    settings: ClaudeSpotSettings,
) -> None:
    reason = _route_scope_block_reason(route, settings)
    if reason is not None:
        raise ClaudeSpotError(reason)


def _route_scope_block_reason(
    route: RouteQuote,
    settings: ClaudeSpotSettings,
) -> str | None:
    source_reason = _asset_scope_block_reason(route.source_asset, settings)
    if source_reason is not None:
        return f"selected route source {route.source_asset} blocked: {source_reason}"
    destination_reason = _asset_scope_block_reason(route.destination_asset, settings)
    if destination_reason is not None:
        return (
            f"selected route destination {route.destination_asset} blocked: "
            f"{destination_reason}"
        )
    for leg in route.legs:
        reason = _leg_scope_block_reason(leg, settings)
        if reason is not None:
            return f"selected route leg {leg.symbol} blocked: {reason}"
    return None


def _leg_scope_block_reason(
    leg: RouteLeg,
    settings: ClaudeSpotSettings,
) -> str | None:
    symbol = _normalize_scope_symbol(leg.symbol)
    if _symbol_matches(settings.blocked_symbols, symbol):
        return "symbol is in Claude Spot local blocklist"
    if settings.allowed_symbols and not _symbol_matches(settings.allowed_symbols, symbol):
        return "symbol is outside Claude Spot local allowlist"
    base, quote = _split_symbol(symbol)
    if quote and quote in settings.blocked_quotes:
        return f"quote asset {quote} is in Claude Spot local blocklist"
    for asset in (leg.source_asset, leg.destination_asset, base, quote):
        if not asset:
            continue
        reason = _asset_scope_block_reason(asset, settings)
        if reason is not None:
            return reason
    return None


def _market_scope_block_reason(
    market: MarketSnapshot,
    settings: ClaudeSpotSettings,
) -> str | None:
    symbol = _normalize_scope_symbol(market.symbol)
    if _symbol_matches(settings.blocked_symbols, symbol):
        return "symbol is in Claude Spot local blocklist"
    if settings.allowed_symbols and not _symbol_matches(settings.allowed_symbols, symbol):
        return "symbol is outside Claude Spot local allowlist"
    quote = market.quote.strip().upper()
    if quote in settings.blocked_quotes:
        return f"quote asset {quote} is in Claude Spot local blocklist"
    for asset in (market.base, market.quote):
        reason = _asset_scope_block_reason(asset, settings)
        if reason is not None:
            return reason
    return None


def _asset_scope_block_reason(
    asset: str,
    settings: ClaudeSpotSettings,
) -> str | None:
    normalized = asset.strip().upper()
    if not normalized:
        return "asset is blank"
    if normalized in settings.blocked_assets:
        return f"asset {normalized} is in Claude Spot local blocklist"
    if settings.block_fiat_assets and normalized in CLAUDE_SPOT_FIAT_ASSETS:
        return f"asset {normalized} is blocked as fiat"
    if settings.allowed_assets and normalized not in settings.allowed_assets:
        return f"asset {normalized} is outside Claude Spot local allowlist"
    return None


def _symbol_matches(configured: tuple[str, ...], symbol: str) -> bool:
    if not configured:
        return False
    normalized = _normalize_scope_symbol(symbol)
    compact = normalized.replace("/", "")
    return normalized in configured or compact in configured


def _split_symbol(symbol: str) -> tuple[str, str]:
    normalized = _normalize_scope_symbol(symbol)
    if "/" not in normalized:
        return "", ""
    base, quote = normalized.split("/", 1)
    return base, quote.split(":", 1)[0]


def _candidate_assets(
    markets: Mapping[str, MarketSnapshot],
    free_balances: Mapping[str, Any],
    settings: ClaudeSpotSettings,
) -> tuple[str, ...]:
    if settings.market_pick_mode == CLAUDE_SPOT_MARKET_PICK_SMALL:
        return _small_market_candidate_assets(markets, free_balances, settings)
    sorted_markets = sorted(
        (market for market in markets.values() if market.eligible),
        key=lambda market: market.quote_volume_usdt or 0.0,
        reverse=True,
    )
    assets: list[str] = []
    for market in sorted_markets:
        if (
            market.base not in assets
            and _asset_scope_block_reason(market.base, settings) is None
        ):
            assets.append(market.base)
        if (
            settings.candidate_limit > 0
            and len(assets) >= settings.candidate_limit
        ):
            break
    for asset in free_balances:
        normalized = str(asset).strip().upper()
        if (
            normalized
            and normalized not in assets
            and _asset_scope_block_reason(normalized, settings) is None
        ):
            assets.append(normalized)
    if "USDT" not in assets and _asset_scope_block_reason("USDT", settings) is None:
        assets.append("USDT")
    return tuple(assets)


def _small_market_candidate_assets(
    markets: Mapping[str, MarketSnapshot],
    free_balances: Mapping[str, Any],
    settings: ClaudeSpotSettings,
) -> tuple[str, ...]:
    free_assets = {
        str(asset).strip().upper()
        for asset, amount in free_balances.items()
        if _positive_float(amount) is not None
    }
    best_by_asset: dict[str, tuple[float, MarketSnapshot]] = {}
    for market in markets.values():
        if not market.eligible:
            continue
        asset = market.base.strip().upper()
        if (
            not asset
            or asset in free_assets
            or asset in _LOW_UPSIDE_ASSETS
            or _asset_scope_block_reason(asset, settings) is not None
        ):
            continue
        score = _small_market_asset_score(market, settings)
        current = best_by_asset.get(asset)
        if current is None or score > current[0]:
            best_by_asset[asset] = (score, market)
    ranked = sorted(
        best_by_asset.items(),
        key=lambda item: (
            item[1][0],
            -(item[1][1].spread_bps or 0.0),
            item[0],
        ),
        reverse=True,
    )
    limit = settings.small_candidate_limit
    assets = [
        asset for asset, _ in (ranked if limit <= 0 else ranked[:limit])
    ]
    if "USDT" not in assets and (limit <= 0 or len(assets) < limit):
        assets.append("USDT")
    return tuple(assets)


def _small_market_asset_score(
    market: MarketSnapshot,
    settings: ClaudeSpotSettings,
) -> float:
    volume = market.quote_volume_usdt or 0.0
    if volume <= 0:
        return -math.inf
    small_bonus = 100.0 if volume < settings.min_quote_volume_usdt else 0.0
    volume_score = -math.log10(max(volume, 1.0)) * 5.0
    spread_penalty = min(max(market.spread_bps, 0.0), settings.small_max_spread_bps) * 0.08
    notional_bonus = max(10.0 - market.min_notional, 0.0)
    quote_bonus = 5.0 if market.quote in {"USDT", "FDUSD", "USDC", "BNB", "BTC", "ETH"} else 0.0
    return small_bonus + volume_score + notional_bonus + quote_bonus - spread_penalty


def _portfolio_value_usdt(
    free_balances: Mapping[str, Any],
    routes: RouteEngine,
) -> float:
    total = 0.0
    for asset, raw_amount in free_balances.items():
        amount = _positive_float(raw_amount)
        if amount is None:
            continue
        normalized = str(asset).strip().upper()
        if normalized == "USDT":
            total += amount
            continue
        rate = routes.indicative_rate(normalized, "USDT")
        if rate is not None:
            total += amount * rate
    return total


def _source_value_usdt(
    source_asset: str,
    amount: float,
    routes: RouteEngine,
) -> float | None:
    if source_asset == "USDT":
        return amount
    rate = routes.indicative_rate(source_asset, "USDT")
    return amount * rate if rate is not None else None


def _immediate_friction_bps(
    source_value_usdt: float | None,
    estimated_output_usdt: float | None,
) -> float | None:
    if (
        source_value_usdt is None
        or estimated_output_usdt is None
        or source_value_usdt <= 0
    ):
        return None
    return (source_value_usdt - estimated_output_usdt) / source_value_usdt * 10_000


def _route_friction_exceeds_allowance(
    immediate_friction_bps: float | None,
    settings: ClaudeSpotSettings,
) -> bool:
    return (
        immediate_friction_bps is not None
        and immediate_friction_bps > settings.allow_negative_friction_bps
    )


def _route_focus_market(
    route: RouteQuote,
    markets: Mapping[str, MarketSnapshot],
) -> MarketSnapshot | None:
    for leg in reversed(route.legs):
        market = markets.get(leg.symbol)
        if market is None:
            continue
        if route.destination_asset in {market.base, market.quote}:
            return market
    return markets.get(route.legs[-1].symbol) if route.legs else None


def _route_market_pick_score(
    route: RouteQuote,
    market: MarketSnapshot | None,
    *,
    source_value_usdt: float | None,
    immediate_friction_bps: float | None,
    settings: ClaudeSpotSettings,
) -> tuple[float, str]:
    if market is None:
        return 0.0, "no focus market metadata"
    asset_score = _small_market_asset_score(market, settings)
    friction = max(immediate_friction_bps or 0.0, 0.0)
    friction_penalty = min(friction, 2_000.0) * 0.04
    leg_penalty = max(len(route.legs) - 1, 0) * 4.0
    source_fit_bonus = 0.0
    if source_value_usdt is not None and market.min_notional > 0:
        source_fit_bonus = min(source_value_usdt / market.min_notional, 4.0) * 2.0
    score = asset_score + source_fit_bonus - friction_penalty - leg_penalty
    reason = (
        f"{_liquidity_tier(market.quote_volume_usdt)} market "
        f"volume={_fmt_float(market.quote_volume_usdt)} USDT, "
        f"spread={_fmt_float(market.spread_bps)} bps, "
        f"min_notional={_fmt_float(market.min_notional)}, "
        f"legs={len(route.legs)}, "
        f"immediate_friction={_fmt_float(immediate_friction_bps)} bps"
    )
    return score, reason


def _liquidity_tier(volume_usdt: float | None) -> str:
    if volume_usdt is None:
        return "unknown"
    if volume_usdt < 25_000:
        return "micro"
    if volume_usdt < 100_000:
        return "small"
    if volume_usdt < 1_000_000:
        return "mid"
    return "large"


def _fmt_float(value: float | None) -> str:
    if value is None:
        return "unknown"
    return f"{value:.6g}"


def _audit_candidates(context: ClaudeDecisionContext | None) -> list[dict[str, Any]]:
    if context is None:
        return []
    return [candidate.as_context() for candidate in context.candidate_routes]


def _route_id(route: RouteQuote) -> str:
    raw = "|".join(
        [
            route.source_asset,
            route.destination_asset,
            ",".join(
                f"{leg.symbol}:{leg.side}:{leg.source_asset}>{leg.destination_asset}"
                for leg in route.legs
            ),
        ]
    )
    digest = hashlib.sha256(raw.encode("utf-8")).hexdigest()[:12]
    return f"{route.source_asset}->{route.destination_asset}:{digest}"


def _leg_context(leg: RouteLeg) -> dict[str, Any]:
    return {
        "symbol": leg.symbol,
        "side": leg.side,
        "source_asset": leg.source_asset,
        "destination_asset": leg.destination_asset,
        "input_amount": leg.input_amount,
        "spent_amount": leg.spent_amount,
        "gross_output_amount": leg.gross_output_amount,
        "fee_rate": leg.fee_rate,
        "fee_output_amount": leg.fee_output_amount,
        "net_output_amount": leg.net_output_amount,
        "commission_discount_asset": leg.commission_discount_asset,
    }


async def _live_leg_order_amount(
    fees: CommissionService,
    market: MarketSnapshot,
    side: str,
    input_amount: float,
    *,
    settings: ClaudeSpotSettings,
) -> float:
    spendable_input = input_amount
    if side == "buy":
        commission = await fees.get(market.symbol)
        reserve_bps = _buy_quote_reserve_bps(
            commission.taker_rate(side, apply_discount=False),
            settings,
        )
        if reserve_bps >= 10_000:
            raise ClaudeSpotError("Claude Spot buy reserve consumes full quote balance")
        spendable_input = input_amount * (1 - reserve_bps / 10_000)
    return _leg_order_amount(
        market,
        side,
        spendable_input,
        slippage_bps=settings.slippage_buffer_bps,
    )


def _buy_quote_reserve_bps(
    taker_fee_rate: float,
    settings: ClaudeSpotSettings,
) -> float:
    if not math.isfinite(taker_fee_rate) or taker_fee_rate < 0:
        raise ClaudeSpotError("Claude Spot dynamic fee rate is invalid")
    dynamic_reserve = (
        taker_fee_rate * 10_000
        + settings.slippage_buffer_bps
        + settings.leg_rounding_reserve_bps
    )
    return max(dynamic_reserve, settings.min_leg_quote_reserve_bps)


def _leg_order_amount(
    market: MarketSnapshot,
    side: str,
    input_amount: float,
    *,
    slippage_bps: float,
) -> float:
    slip = slippage_bps / 10_000
    amount_step = market.amount_step if market.amount_step is not None else market.amount_precision
    if side == "sell":
        amount = _round_down(input_amount, amount_step)
        notional = amount * market.bid * (1 - slip)
    else:
        price = market.ask * (1 + slip)
        amount = _round_down(input_amount / price, amount_step)
        notional = amount * price
    if amount <= 0:
        raise ClaudeSpotError("Claude Spot leg order amount rounded to zero")
    if amount < market.min_amount or notional < market.min_notional:
        raise ClaudeSpotError("Claude Spot leg is below Binance minimum amount or notional")
    return amount


async def _final_order_status(
    client: Any,
    symbol: str,
    order: Mapping[str, Any],
) -> Mapping[str, Any]:
    status = str(order.get("status") or "").lower()
    order_id = order.get("id") or order.get("orderId")
    if status in {"closed", "filled", "canceled", "rejected", "expired"} or not order_id:
        return order
    fetched: Mapping[str, Any] = order
    for _ in range(3):
        await asyncio.sleep(1)
        fetched = await client.request("fetch_order", str(order_id), symbol, request_weight=1)
        fetched_status = str(fetched.get("status") or "").lower()
        if fetched_status in {"closed", "filled", "canceled", "rejected", "expired"}:
            return fetched
    return fetched


async def _send_alert(
    alerter: Any,
    result: ClaudeSpotResult,
    audit_path: Path,
    settings: ClaudeSpotSettings | None = None,
) -> bool:
    send = getattr(alerter, "send", None)
    if send is None or not callable(send):
        return False
    explain = True if settings is None else settings.telegram_explain
    reason_chars = 0 if settings is None else settings.telegram_reason_chars
    sent_all = True
    for message in _format_alert_messages(result, audit_path, explain, reason_chars):
        sent_all = bool(await send(message)) and sent_all
    return sent_all


def _format_alert_message(
    result: ClaudeSpotResult,
    audit_path: Path,
    explain: bool,
    reason_chars: int,
) -> str:
    return "\n".join(
        _format_alert_lines(
            result,
            audit_path,
            explain,
            reason_chars,
            include_why=True,
        )
    )


def _format_alert_messages(
    result: ClaudeSpotResult,
    audit_path: Path,
    explain: bool,
    reason_chars: int,
) -> list[str]:
    message = _format_alert_message(result, audit_path, explain, reason_chars)
    if len(message) <= TELEGRAM_MESSAGE_MAX_CHARS:
        return [message]
    if not explain:
        return _split_alert_text(message)
    header = "\n".join(
        _format_alert_lines(
            result,
            audit_path,
            explain,
            reason_chars,
            include_why=False,
        )
    )
    why = _short_text(_decision_reason(result), reason_chars)
    if not why:
        return _split_alert_text(header)
    messages = _split_alert_text(header)
    prefix = "<b>CLAUDE SPOT WHY</b>\n"
    chunk_limit = TELEGRAM_MESSAGE_MAX_CHARS - len(prefix)
    messages.extend(
        prefix + _code(chunk)
        for chunk in _split_code_chunks(why, chunk_limit)
    )
    return messages


def _format_alert_lines(
    result: ClaudeSpotResult,
    audit_path: Path,
    explain: bool,
    reason_chars: int,
    *,
    include_why: bool,
) -> list[str]:
    decision = result.decision or {}
    lines = [
        "<b>CLAUDE SPOT BRAIN</b>",
        f"Status: {_code(result.status)}",
        f"Action: {_code(decision.get('action', ''))}",
        f"Route: {_code(_route_label(result))}",
    ]
    if explain:
        lines.extend(
            [
                f"ELI5: {_code(_decision_eli5(result))}",
                f"Candidates: {_code(str(result.candidate_route_count))}",
                f"Submitted: {_code('yes' if result.submission_attempted else 'no')}",
            ]
        )
        speculative = _speculative_alert_text(result)
        if speculative:
            lines.append(f"Speculative: {_code(speculative)}")
        why = _short_text(_decision_reason(result), reason_chars) if include_why else ""
        if include_why and why:
            lines.append(f"Why: {_code(why)}")
    lines.append(f"Audit: {_code(str(audit_path))}")
    return lines


def _decision_eli5(result: ClaudeSpotResult) -> str:
    decision = result.decision or {}
    status = str(result.status or "").lower()
    action = str(decision.get("action") or "").lower()
    reason = _compact_text(_decision_reason(result)).lower()
    route = _route_label(result)

    if status == "submitted":
        speculative = result.selected_route.get("speculative_gate", {})
        if isinstance(speculative, Mapping) and speculative.get("allowed") is True:
            return (
                f"The bot chose {route} as a speculative trade: normal edge was "
                "below the standard gate but above the speculative floor, and it "
                "still passed balance, scope, fee/friction, order-test, and safety checks."
            )
        return (
            f"The bot chose {route} and submitted it because the route passed "
            "balance, scope, fee/friction, evidence, order-test, and safety checks."
        )
    if status == "planned":
        return (
            f"Claude chose {route}, but this cycle was plan-only, so no real order "
            "was allowed."
        )
    if status == "failed":
        if result.submission_attempted:
            return (
                f"The bot tried {route}, but execution failed, so it stopped instead "
                "of continuing with an uncertain wallet state."
            )
        return "The cycle failed before any order was submitted, so the bot did not trade."
    if status == "blocked":
        return "A safety or configuration check blocked the cycle before trading."
    if status == "held" or action == "hold":
        if "timed out" in reason:
            return "Claude did not answer in time, so the safe move was to hold."
        if "previous live route failed" in reason:
            return (
                "A previous live route failed, so the bot is waiting instead of "
                "compounding an uncertain state."
            )
        if "cooldown" in reason:
            return "A trade was too recent, so the bot held to avoid churn."
        if "daily submitted-trade cap" in reason:
            return "The daily submitted-trade cap was already reached, so no order was sent."
        if "loss streak" in reason:
            return "Recent Claude trades look weak, so the bot held instead of adding risk."
        if "market evidence" in reason or "expected edge" in reason:
            return _evidence_hold_eli5(result)
        if result.candidate_route_count <= 0:
            return (
                "No valid trade route passed the filters, so trading would mean "
                "guessing and paying fees."
            )
        return "Claude judged holding to be better than paying fees for the available trade setup."
    return "The cycle recorded a decision and kept the full details in the audit log."


def _evidence_hold_eli5(result: ClaudeSpotResult) -> str:
    route = _route_label(result)
    evidence = result.selected_route.get("market_evidence", {})
    speculative = result.selected_route.get("speculative_gate", {})
    if isinstance(evidence, Mapping):
        edge = _finite_float_or_none(evidence.get("expected_edge_after_friction_bps"))
        required = _finite_float_or_none(evidence.get("minimum_expected_edge_bps"))
        if edge is not None and required is not None:
            if isinstance(speculative, Mapping) and speculative.get("enabled"):
                reason = _compact_text(speculative.get("reason") or "not allowed")
                return (
                    f"Claude considered {route}, but its expected edge after fees and "
                    f"slippage was {edge:.2f} bps, below the required {required:.2f} bps; "
                    f"the speculative gate also blocked it because {reason}."
                )
            return (
                f"Claude considered {route}, but its expected edge after fees and "
                f"slippage was {edge:.2f} bps, below the required {required:.2f} bps."
            )
    if route:
        return (
            f"Claude considered {route}, but the market evidence was not strong "
            "enough after fees and slippage."
        )
    return "The market evidence was not strong enough after fees and slippage."


def _speculative_alert_text(result: ClaudeSpotResult) -> str:
    speculative = result.selected_route.get("speculative_gate", {})
    if not isinstance(speculative, Mapping) or not speculative.get("enabled"):
        return ""
    state = "allowed" if speculative.get("allowed") is True else "blocked"
    edge = _finite_float_or_none(speculative.get("expected_edge_after_friction_bps"))
    floor = _finite_float_or_none(speculative.get("min_edge_bps"))
    reason = _compact_text(speculative.get("reason", ""))
    parts = [state]
    if edge is not None:
        parts.append(f"edge {edge:.2f} bps")
    if floor is not None:
        parts.append(f"floor {floor:.2f} bps")
    if reason:
        parts.append(reason)
    return "; ".join(parts)


def _decision_reason(result: ClaudeSpotResult) -> str:
    decision = result.decision or {}
    reason = _compact_text(decision.get("reason", ""))
    if reason:
        return reason
    return _compact_text(result.error or result.status)


def _route_label(result: ClaudeSpotResult) -> str:
    decision = result.decision or {}
    selected = result.selected_route or {}
    route_id = str(decision.get("route_id") or selected.get("route_id") or "").strip()
    source = str(decision.get("source_asset") or selected.get("source_asset") or "").strip()
    destination = str(
        decision.get("destination_asset") or selected.get("destination_asset") or ""
    ).strip()
    if source and destination:
        label = f"{source}->{destination}"
        if route_id:
            return f"{label} ({route_id})"
        return label
    return route_id or "n/a"


def _code(value: Any) -> str:
    return f"<code>{html.escape(_compact_text(value), quote=True)}</code>"


def _short_text(value: Any, limit: int) -> str:
    text = _compact_text(value)
    if limit <= 0 or len(text) <= limit:
        return text
    return text[: max(limit - 3, 0)].rstrip() + "..."


def _split_alert_text(message: str) -> list[str]:
    if len(message) <= TELEGRAM_MESSAGE_MAX_CHARS:
        return [message]
    chunks: list[str] = []
    current = ""
    for line in message.splitlines():
        candidate = line if not current else f"{current}\n{line}"
        if len(candidate) <= TELEGRAM_MESSAGE_MAX_CHARS:
            current = candidate
            continue
        if current:
            chunks.append(current)
            current = ""
        if len(line) <= TELEGRAM_MESSAGE_MAX_CHARS:
            current = line
        else:
            chunks.extend(
                line[index : index + TELEGRAM_MESSAGE_MAX_CHARS]
                for index in range(0, len(line), TELEGRAM_MESSAGE_MAX_CHARS)
            )
    if current:
        chunks.append(current)
    return chunks


def _split_code_chunks(text: str, max_wrapped_chars: int) -> list[str]:
    clean = _compact_text(text)
    if not clean:
        return []
    max_wrapped_chars = max(max_wrapped_chars, len("<code>x</code>"))
    chunks: list[str] = []
    remaining = clean
    while remaining:
        if len(_code(remaining)) <= max_wrapped_chars:
            chunks.append(remaining)
            break
        low = 1
        high = len(remaining)
        best = 1
        while low <= high:
            middle = (low + high) // 2
            if len(_code(remaining[:middle].rstrip())) <= max_wrapped_chars:
                best = middle
                low = middle + 1
            else:
                high = middle - 1
        split_at = best
        space_at = remaining.rfind(" ", 0, best)
        if space_at > best // 2:
            split_at = space_at
        chunk = remaining[:split_at].strip()
        if not chunk:
            chunk = remaining[:best].strip() or remaining[0]
            split_at = max(best, 1)
        chunks.append(chunk)
        remaining = remaining[split_at:].strip()
    return chunks


def _compact_text(value: Any) -> str:
    return " ".join(str(value or "").split())


def _write_audit(path: Path, result: ClaudeSpotResult) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(result.as_dict(), sort_keys=True) + "\n")
    return path


def mark_failed_route_reviewed(
    settings: ClaudeSpotSettings,
    *,
    reason: str = "operator reviewed failed submitted route and approved live re-enable",
) -> Path:
    rows, read_error = _load_audit_rows(Path(settings.audit_path))
    if read_error:
        raise ClaudeSpotError(f"could not read Claude Spot audit history: {read_error}")
    last_failed = next(
        (
            row
            for row in reversed(rows)
            if str(row.get("status") or "").lower() == "failed"
            and _audit_row_attempted_submission(row)
        ),
        None,
    )
    if last_failed is None:
        raise ClaudeSpotError("no failed submitted Claude Spot route needs review")
    failed_index = rows.index(last_failed)
    if any(_audit_row_failed_submission_reviewed(row) for row in rows[failed_index + 1 :]):
        raise ClaudeSpotError("latest failed submitted Claude Spot route is already reviewed")
    record = {
        "timestamp": _utc_timestamp(),
        "mode": _mode(settings),
        "status": "reviewed",
        "prompt_version": settings.prompt_version,
        "context_hash": "",
        "decision": {
            "action": "hold",
            "route_id": "",
            "source_asset": "",
            "destination_asset": "",
            "spend_mode": "",
            "spend_amount": None,
            "reason": reason,
        },
        "candidate_route_count": 0,
        "market_pick_mode": settings.market_pick_mode,
        "candidate_routes": [],
        "selected_route": {},
        "submitted_legs": [],
        "submission_attempted": False,
        "error": "",
        "operator_review": {
            "failed_submission_reviewed": True,
            "reviewed_failed_timestamp": last_failed.get("timestamp", ""),
            "reviewed_failed_route_id": _row_route_id(last_failed),
            "reason": reason,
        },
    }
    path = Path(settings.audit_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(record, sort_keys=True) + "\n")
    return path


def _build_user_prompt(context: ClaudeDecisionContext) -> str:
    payload = json.dumps(context.as_prompt_payload(), indent=2, sort_keys=True)
    return f"""Choose the next autonomous Spot action.

Return only JSON matching the schema. For rebalance include route_id,
source_asset, destination_asset, spend_mode=all_available, and spend_amount.
Use one supplied route_id for rebalance.

Default to hold. Rebalance only when a specific candidate route's
market_evidence.expected_edge_after_friction_bps is materially positive and
clears its minimum_expected_edge_bps. Do not trade merely to trade. If holding
is the best after-cost action, hold. Citing 5m/15m short-tape momentum alone
is not sufficient justification to pay round-trip fees and spread.

Use candidate market_evidence, social_signal_evidence, and trade_memory
directly. A live route will be blocked after your decision if its expected
edge after friction does not clear the configured evidence gate. If
route_scope.speculative_mode_enabled is true, you may select a route whose
normal market_evidence.gate_pass is false only when
expected_edge_after_friction_bps is at or above
route_scope.speculative_min_edge_bps and your reason states a concrete
speculative thesis, such as reversal, breakout, volume anomaly, oversold
mean reversion, or current-position exit risk. Otherwise, hold.

Decision context:
{payload}
"""


def _extract_json_object(raw_output: str) -> Mapping[str, Any] | None:
    payload = _parse_json(raw_output)
    if _looks_like_decision(payload):
        return payload
    if isinstance(payload, Mapping):
        structured = payload.get("structured_output")
        if _looks_like_decision(structured):
            return structured
    for text in _nested_text_candidates(payload):
        nested = _parse_json(text)
        if _looks_like_decision(nested):
            return nested
    return None


def _parse_json(value: Any) -> Mapping[str, Any] | None:
    if isinstance(value, Mapping):
        return value
    if not isinstance(value, str):
        return None
    text = value.strip()
    if text.startswith("```"):
        text = text.removeprefix("```json").removeprefix("```").removesuffix("```").strip()
    try:
        payload = json.loads(text)
    except json.JSONDecodeError:
        start = text.find("{")
        end = text.rfind("}")
        if start < 0 or end <= start:
            return None
        try:
            payload = json.loads(text[start : end + 1])
        except json.JSONDecodeError:
            return None
    return payload if isinstance(payload, Mapping) else None


def _nested_text_candidates(payload: Mapping[str, Any] | None) -> list[str]:
    if not isinstance(payload, Mapping):
        return []
    candidates: list[str] = []
    for key in ("result", "content", "text", "message"):
        value = payload.get(key)
        if isinstance(value, str):
            candidates.append(value)
        elif isinstance(value, Mapping):
            candidates.extend(_nested_text_candidates(value))
        elif isinstance(value, list):
            for item in value:
                if isinstance(item, str):
                    candidates.append(item)
                elif isinstance(item, Mapping):
                    text = item.get("text")
                    if isinstance(text, str):
                        candidates.append(text)
                    candidates.extend(_nested_text_candidates(item))
    return candidates


def _looks_like_decision(payload: Any) -> bool:
    return isinstance(payload, Mapping) and isinstance(payload.get("action"), str)


def _sanitize_balance(balance: object) -> dict[str, Any]:
    if not isinstance(balance, Mapping):
        return {}
    sanitized: dict[str, Any] = {}
    for group_name in ("free", "used", "total"):
        group = balance.get(group_name)
        if not isinstance(group, Mapping):
            continue
        values = {
            str(asset).upper(): float(amount)
            for asset, amount in group.items()
            if _positive_float(amount) is not None
        }
        if values:
            sanitized[group_name] = values
    return sanitized


def _sanitize_order(order: Mapping[str, Any]) -> dict[str, Any]:
    allowed = {
        "id",
        "clientOrderId",
        "orderId",
        "symbol",
        "type",
        "side",
        "status",
        "amount",
        "filled",
        "remaining",
        "average",
        "price",
        "cost",
        "fee",
        "fees",
        "timestamp",
        "datetime",
    }
    return {key: order[key] for key in allowed if key in order}


def _balance_amount(group: Mapping[str, Any], asset: str) -> float:
    return float(group.get(asset, 0.0) or 0.0)


def _round_down(amount: float, precision: int | float | None) -> float:
    if precision is None:
        return amount
    quantum = Decimal("1").scaleb(-precision) if isinstance(precision, int) else Decimal(str(precision))
    if quantum <= 0:
        return amount
    decimal_amount = Decimal(str(amount))
    return float((decimal_amount / quantum).to_integral_value(rounding=ROUND_DOWN) * quantum)


def _ensure_no_emergency_stop(path: str) -> None:
    if Path(path).exists():
        raise ClaudeSpotError(f"Claude Spot emergency stop is active: {path}")


class _CycleLock:
    def __init__(self, path: Path) -> None:
        self.path = path
        self._fd: int | None = None

    def acquire(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        try:
            self._fd = os.open(
                self.path,
                os.O_CREAT | os.O_EXCL | os.O_WRONLY,
                0o600,
            )
        except FileExistsError as exc:
            raise ClaudeSpotError(f"Claude Spot cycle lock is active: {self.path}") from exc
        os.write(self._fd, str(os.getpid()).encode("ascii"))

    def release(self) -> None:
        if self._fd is not None:
            os.close(self._fd)
            self._fd = None
        try:
            self.path.unlink()
        except FileNotFoundError:
            pass


def _mode(settings: ClaudeSpotSettings) -> str:
    if settings.dry_run:
        return "DRY_RUN"
    if settings.use_testnet:
        return "testnet"
    return "LIVE"


def _utc_timestamp() -> str:
    return datetime.now(timezone.utc).isoformat()


def _positive_float(value: Any) -> float | None:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return None
    return number if math.isfinite(number) and number > 0 else None


def _optional_positive_float(value: Any) -> float | None:
    if value is None:
        return None
    return _positive_float(value)


def _finite_float_or_none(value: Any) -> float | None:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return None
    return number if math.isfinite(number) else None


def _finite_non_negative(value: float, name: str) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise ClaudeSpotError(f"{name} must be a number") from exc
    if not math.isfinite(number) or number < 0:
        raise ClaudeSpotError(f"{name} must be finite and non-negative")
    return number


def _finite_number(value: float, name: str) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise ClaudeSpotError(f"{name} must be a number") from exc
    if not math.isfinite(number):
        raise ClaudeSpotError(f"{name} must be finite")
    return number


def _required(name: str) -> str:
    value = os.getenv(name)
    if not value or not value.strip():
        raise ClaudeSpotError(f"missing required environment variable: {name}")
    return value.strip()


def _bool(name: str, default: bool) -> bool:
    raw = os.getenv(name)
    if raw is None:
        return default
    normalized = raw.strip().lower()
    if normalized in {"1", "true", "yes", "on"}:
        return True
    if normalized in {"0", "false", "no", "off"}:
        return False
    raise ClaudeSpotError(f"{name} must be a boolean")


def _float(name: str, default: float) -> float:
    raw = os.getenv(name)
    try:
        return float(raw) if raw is not None else default
    except ValueError as exc:
        raise ClaudeSpotError(f"{name} must be a number") from exc


def _int(name: str, default: int) -> int:
    raw = os.getenv(name)
    try:
        return int(raw) if raw is not None else default
    except ValueError as exc:
        raise ClaudeSpotError(f"{name} must be an integer") from exc


def _csv_env(name: str, default: str) -> tuple[str, ...]:
    raw = os.getenv(name)
    text = default if raw is None else raw
    return tuple(part.strip() for part in text.split(",") if part.strip())


def _normalize_symbol_list(values: tuple[str, ...], field_name: str) -> tuple[str, ...]:
    normalized: list[str] = []
    for value in values:
        symbol = _normalize_scope_symbol(value)
        if not symbol:
            raise ClaudeSpotError(f"{field_name} contains a blank symbol")
        if not symbol.replace("/", "").replace(":", "").replace("-", "").isalnum():
            raise ClaudeSpotError(f"{field_name} contains an invalid symbol")
        if symbol not in normalized:
            normalized.append(symbol)
    return tuple(normalized)


def _normalize_asset_list(values: tuple[str, ...], field_name: str) -> tuple[str, ...]:
    normalized: list[str] = []
    for value in values:
        asset = value.strip().upper()
        if not asset:
            raise ClaudeSpotError(f"{field_name} contains a blank asset")
        if not asset.replace("-", "").replace("_", "").isalnum():
            raise ClaudeSpotError(f"{field_name} contains an invalid asset")
        if asset not in normalized:
            normalized.append(asset)
    return tuple(normalized)


def _normalize_scope_symbol(value: str) -> str:
    symbol = value.strip().upper().replace(" ", "")
    if "/" not in symbol:
        return symbol
    base, quote = symbol.split("/", 1)
    return f"{base}/{quote.split(':', 1)[0]}"


async def _build_evidence_only_payload(settings: ClaudeSpotSettings) -> dict[str, Any]:
    _ensure_no_emergency_stop(settings.emergency_stop_path)
    client = BinanceClient(
        settings.api_key,
        settings.api_secret,
        testnet=settings.use_testnet,
        max_retries=0,
    )
    try:
        context, _ = await build_decision_context(client, settings)
        return context.as_prompt_payload()
    finally:
        await client.close()


def run(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Run one Claude-controlled autonomous Spot decision cycle."
    )
    parser.add_argument(
        "--submit-live",
        action="store_true",
        help="submit the selected Claude route when live env gates pass",
    )
    parser.add_argument(
        "--plan-only",
        action="store_true",
        help="ask Claude and audit the selected route without submitting orders",
    )
    parser.add_argument(
        "--evidence-only",
        action="store_true",
        help="print Claude Spot decision context without asking Claude or submitting orders",
    )
    parser.add_argument(
        "--mark-failed-route-reviewed",
        action="store_true",
        help="append an operator-review audit marker that clears the failed-route hold",
    )
    parser.add_argument(
        "--review-reason",
        default="operator reviewed failed submitted route and approved live re-enable",
        help="reason stored with --mark-failed-route-reviewed",
    )
    args = parser.parse_args(argv)
    submit_order = args.submit_live and not args.plan_only
    try:
        if args.mark_failed_route_reviewed:
            audit_path = mark_failed_route_reviewed(
                ClaudeSpotSettings.from_env(),
                reason=args.review_reason,
            )
            print(
                json.dumps(
                    {
                        "status": "reviewed",
                        "audit_path": str(audit_path),
                    },
                    indent=2,
                    sort_keys=True,
                )
            )
            return 0
        if args.evidence_only:
            payload = asyncio.run(
                _build_evidence_only_payload(ClaudeSpotSettings.from_env())
            )
            print(json.dumps(payload, indent=2, sort_keys=True))
            return 0
        result = asyncio.run(
            run_claude_spot_cycle(
                ClaudeSpotSettings.from_env(),
                submit_order=submit_order,
                use_lock=True,
            )
        )
    except Exception as exc:
        print(f"Claude Spot failed: {exc}")
        return 1
    print(json.dumps(result.as_dict(), indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(run())
