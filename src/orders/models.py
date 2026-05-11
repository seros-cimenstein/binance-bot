from dataclasses import dataclass, field
from datetime import datetime
from typing import Optional, List
from enum import Enum

class OrderStatus(Enum):
    PENDING = "pending"
    OPEN = "open"
    CLOSED = "closed"
    CANCELED = "canceled"
    EXPIRED = "expired"
    REJECTED = "rejected"

class OrderSide(Enum):
    BUY = "buy"
    SELL = "sell"

class OrderType(Enum):
    LIMIT = "limit"
    MARKET = "market"

@dataclass(frozen=True)
class Fill:
    id: str
    order_id: str
    symbol: str
    side: OrderSide
    price: float
    amount: float
    fee: float
    fee_currency: str
    timestamp: datetime

@dataclass
class Order:
    id: str
    symbol: str
    type: OrderType
    side: OrderSide
    amount: float
    price: Optional[float] = None
    status: OrderStatus = OrderStatus.PENDING
    filled_amount: float = 0.0
    remaining_amount: float = 0.0
    avg_fill_price: Optional[float] = None
    fills: List[Fill] = field(default_factory=list)
    timestamp: datetime = field(default_factory=datetime.utcnow)

@dataclass
class Position:
    symbol: str
    amount: float
    entry_price: float
    unrealized_pnl: float = 0.0
    leverage: float = 1.0
    timestamp: datetime = field(default_factory=datetime.utcnow)
