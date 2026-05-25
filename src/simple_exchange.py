"""Legacy minimal Binance client kept for simple examples and old imports.

The production runtime uses :mod:`src.exchange.client` instead.
"""

import ccxt.async_support as ccxt
from .config import Settings
from loguru import logger

class BinanceClient:
    def __init__(self, settings: Settings | None = None):
        settings = settings or Settings.from_env()
        self.exchange = ccxt.binance({
            'apiKey': settings.api_key,
            'secret': settings.api_secret,
            'enableRateLimit': True,
            'options': {
                'defaultType': 'spot'
            }
        })
        if settings.use_testnet:
            self.exchange.set_sandbox_mode(True)

    async def fetch_balance(self):
        try:
            balance = await self.exchange.fetch_balance()
            return balance
        except Exception as e:
            logger.error(f"Error fetching balance: {e}")
            return None

    async def fetch_ticker(self, symbol: str):
        try:
            ticker = await self.exchange.fetch_ticker(symbol)
            return ticker
        except Exception as e:
            logger.error(f"Error fetching ticker for {symbol}: {e}")
            return None

    async def create_order(self, symbol: str, side: str, amount: float, price: float = None):
        try:
            if side == 'buy':
                order = await self.exchange.create_market_buy_order(symbol, amount)
            else:
                order = await self.exchange.create_market_sell_order(symbol, amount)
            return order
        except Exception as e:
            logger.error(f"Error creating {side} order for {symbol}: {e}")
            return None

    async def close(self):
        await self.exchange.close()
