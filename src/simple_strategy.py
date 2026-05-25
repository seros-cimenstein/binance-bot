"""Legacy threshold strategy kept for simple examples and old tests."""

from loguru import logger

class SimpleStrategy:
    def __init__(self, symbol: str, order_size: float):
        self.symbol = symbol
        self.order_size = order_size
        self.last_price = None

    def decide(self, current_price: float):
        if self.last_price is None:
            self.last_price = current_price
            return None

        # Simple logic: if price drops by 1%, buy. If it rises by 1%, sell.
        # This is just a placeholder strategy.
        change = (current_price - self.last_price) / self.last_price
        
        decision = None
        if change <= -0.01:
            decision = 'buy'
        elif change >= 0.01:
            decision = 'sell'
        
        if decision:
            logger.info(f"Strategy decision: {decision} at price {current_price} (change: {change:.2%})")
            self.last_price = current_price
            
        return decision
