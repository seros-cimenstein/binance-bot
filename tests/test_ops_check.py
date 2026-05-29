from __future__ import annotations

import pytest

from src.config import Settings
from src.ops_check import OperationsCheckError, run_checks


class FakeClient:
    def __init__(self, *, ticker: dict[str, float] | None = None) -> None:
        self.ticker = ticker or {"last": 42.5}
        self.closed = False
        self.balance_requests = 0

    async def fetch_ticker(self, symbol: str) -> dict[str, float]:
        assert symbol == "BTC/USDT"
        return self.ticker

    async def fetch_balance(self) -> dict[str, object]:
        self.balance_requests += 1
        return {}

    async def close(self) -> None:
        self.closed = True


class FakeAlerter:
    def __init__(self, delivered: bool) -> None:
        self.delivered = delivered
        self.messages: list[tuple[str, str]] = []

    async def alert_operations_test(self, mode: str, symbol: str) -> bool:
        self.messages.append((mode, symbol))
        return self.delivered


@pytest.mark.asyncio
async def test_ops_check_is_read_only_and_can_confirm_telegram_delivery() -> None:
    client = FakeClient()
    alerter = FakeAlerter(True)

    results = await run_checks(
        Settings(
            api_key="key",
            api_secret="secret",
            use_testnet=False,
            confirm_live_trading="yes",
            telegram_bot_token="token",
            telegram_chat_id="chat",
        ),
        telegram_test=True,
        client_factory=lambda _: client,
        alerter_factory=lambda *_: alerter,
    )

    assert results == [
        "Mode: LIVE",
        "Exchange ticker OK: BTC/USDT last=42.5",
        "Account access OK: balance request succeeded",
        "Telegram delivery OK: verification message sent",
    ]
    assert client.balance_requests == 1
    assert alerter.messages == [("LIVE", "BTC/USDT")]
    assert client.closed is True


@pytest.mark.asyncio
async def test_ops_check_fails_when_telegram_verification_is_not_delivered() -> None:
    client = FakeClient()

    with pytest.raises(OperationsCheckError, match="Telegram verification"):
        await run_checks(
            Settings(api_key="key", api_secret="secret"),
            telegram_test=True,
            client_factory=lambda _: client,
            alerter_factory=lambda *_: FakeAlerter(False),
        )

    assert client.closed is True


@pytest.mark.asyncio
async def test_ops_check_rejects_unconfirmed_live_configuration() -> None:
    with pytest.raises(OperationsCheckError, match="CONFIRM_LIVE_TRADING"):
        await run_checks(
            Settings(api_key="key", api_secret="secret", use_testnet=False),
            client_factory=lambda _: FakeClient(),
        )
