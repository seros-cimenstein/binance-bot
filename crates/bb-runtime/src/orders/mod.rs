//! Order models, lifecycle management, payload parsing and sizing
//! (port of `src/orders/`).

pub mod manager;
pub mod models;
pub mod payload;
pub mod sizing;

pub use manager::{OrderManager, OrderManagerError, OrderStateMachine, PlaceOrder};
pub use models::{Fill, Order, OrderSide, OrderStatus, OrderType, Position};
pub use payload::OrderPayload;
pub use sizing::{OrderSizer, OrderSizingConfig, OrderSizingError, OrderSizingRequest, SizedOrder};
