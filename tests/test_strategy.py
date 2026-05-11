import unittest
from src.simple_strategy import SimpleStrategy

class TestSimpleStrategy(unittest.TestCase):
    def test_strategy_buy(self):
        strategy = SimpleStrategy("BTC/USDT", 0.001)
        strategy.decide(100.0)
        decision = strategy.decide(98.0) # 2% drop
        self.assertEqual(decision, 'buy')

    def test_strategy_sell(self):
        strategy = SimpleStrategy("BTC/USDT", 0.001)
        strategy.decide(100.0)
        decision = strategy.decide(102.0) # 2% rise
        self.assertEqual(decision, 'sell')

    def test_strategy_none(self):
        strategy = SimpleStrategy("BTC/USDT", 0.001)
        strategy.decide(100.0)
        decision = strategy.decide(100.5) # 0.5% rise
        self.assertIsNone(decision)

if __name__ == '__main__':
    unittest.main()
