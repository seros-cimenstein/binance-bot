import pytest
import asyncio
from unittest.mock import MagicMock

@pytest.fixture
def mock_binance_client():
    client = MagicMock()
    # Mock common methods as needed
    return client

@pytest.fixture(scope="session")
def event_loop():
    loop = asyncio.get_event_loop_policy().new_event_loop()
    yield loop
    loop.close()
