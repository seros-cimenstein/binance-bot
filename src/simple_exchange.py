import ccxt.async_support as ccxt
from .config import settings
from .logger import export_logger as logger

class BinanceClient:
    def __init__(self):
        self.exchange = ccxt.binance({
            'apiKey': settings.BINANCE_API_KEY,
            'secret': settings.BINANCE_SECRET_KEY,
            'enableRateLimit': True,
            'options': {
                'defaultType': 'spot'
            }
        })

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
