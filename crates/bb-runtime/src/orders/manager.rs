//! Port of `src/orders/manager.py`: order lifecycle management.
//!
//! The manager keeps every known order in memory for historical lookup,
//! tracks active order ids separately, and derives open positions from
//! monotonically increasing filled quantities. State lives behind a mutex
//! that is never held across an exchange call.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use parking_lot::{Mutex, MutexGuard};

use bb_core::Decimal;
use rust_decimal_macros::dec;

use super::models::{Fill, Order, OrderSide, OrderStatus, OrderType, Position, utc_now};
use super::payload::{OrderPayload, format_g};
use crate::db::{OrderDb, OrderDbError};
use crate::exchange::{Exchange, ExchangeError, ExchangeOrderRequest};

const EPSILON: Decimal = dec!(0.000000000001);

#[derive(Debug, thiserror::Error)]
pub enum OrderManagerError {
    /// `OrderValidationError`.
    #[error("{0}")]
    Validation(String),
    /// `OrderNotFoundError`.
    #[error("order not found: {0}")]
    NotFound(String),
    /// `InvalidOrderTransitionError`.
    #[error("invalid order transition {order_id}: {current} -> {target}")]
    InvalidTransition {
        order_id: String,
        current: OrderStatus,
        target: OrderStatus,
    },
    #[error(transparent)]
    Exchange(#[from] ExchangeError),
    #[error(transparent)]
    Db(#[from] OrderDbError),
}

fn validation(message: impl Into<String>) -> OrderManagerError {
    OrderManagerError::Validation(message.into())
}

type Result<T, E = OrderManagerError> = std::result::Result<T, E>;

/// Validates order lifecycle transitions (`OrderStateMachine`).
pub struct OrderStateMachine;

impl OrderStateMachine {
    pub fn can_transition(current: OrderStatus, target: OrderStatus) -> bool {
        current == target
            || match current {
                OrderStatus::Pending => target != OrderStatus::Pending,
                OrderStatus::Open => !matches!(target, OrderStatus::Pending | OrderStatus::Open),
                _ => false,
            }
    }

    pub fn transition(order: &mut Order, target: OrderStatus) -> Result<()> {
        if order.status == target {
            return Ok(());
        }
        if !Self::can_transition(order.status, target) {
            return Err(OrderManagerError::InvalidTransition {
                order_id: order.id.clone(),
                current: order.status,
                target,
            });
        }
        order.status = target;
        Ok(())
    }
}

/// `place_order` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceOrder {
    pub symbol: String,
    pub order_type: OrderType,
    pub side: OrderSide,
    pub amount: Decimal,
    pub price: Option<Decimal>,
    pub client_order_id: Option<String>,
}

impl PlaceOrder {
    pub fn market(symbol: impl Into<String>, side: OrderSide, amount: Decimal) -> Self {
        Self {
            symbol: symbol.into(),
            order_type: OrderType::Market,
            side,
            amount,
            price: None,
            client_order_id: None,
        }
    }

    pub fn limit(
        symbol: impl Into<String>,
        side: OrderSide,
        amount: Decimal,
        price: Decimal,
    ) -> Self {
        Self {
            price: Some(price),
            order_type: OrderType::Limit,
            ..Self::market(symbol, side, amount)
        }
    }
}

struct Entry {
    order: Order,
    /// Insertion sequence (Python dict order) for stable timestamp sorting.
    seq: u64,
}

#[derive(Default)]
struct State {
    orders: HashMap<String, Entry>,
    ids_by_symbol: HashMap<String, HashSet<String>>,
    active_ids: HashSet<String>,
    positions: BTreeMap<String, Position>,
    applied: HashMap<String, Decimal>,
    next_seq: u64,
}

/// Coordinates exchange order calls and maintains local order state.
pub struct OrderManager {
    exchange: Arc<dyn Exchange>,
    db: Option<Arc<OrderDb>>,
    state: Mutex<State>,
}

impl std::fmt::Debug for OrderManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrderManager")
            .field("db", &self.db.is_some())
            .finish_non_exhaustive()
    }
}

impl OrderManager {
    pub fn new(exchange: Arc<dyn Exchange>, db: Option<Arc<OrderDb>>) -> Self {
        Self {
            exchange,
            db,
            state: Mutex::new(State::default()),
        }
    }

    pub fn exchange(&self) -> &Arc<dyn Exchange> {
        &self.exchange
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock()
    }

    /// Rebuild in-memory state from persisted orders on startup.
    pub fn restore_from_db(&self) -> Result<()> {
        let Some(db) = &self.db else {
            return Ok(());
        };
        let orders = db.load_orders()?;
        let mut st = self.lock();
        *st = State::default();
        for mut order in orders {
            st.ids_by_symbol
                .entry(order.symbol.clone())
                .or_default()
                .insert(order.id.clone());
            sync_active(&mut st, &order);
            let filled = order.filled_amount;
            apply_position_delta(&mut st, &mut order, filled, None)?;
            st.applied.insert(order.id.clone(), filled);
            let seq = bump(&mut st);
            st.orders.insert(order.id.clone(), Entry { order, seq });
        }
        Ok(())
    }

    /// Refresh locally active orders from the exchange after startup restore.
    pub async fn reconcile_active_orders(&self) -> Result<Vec<Order>> {
        let mut reconciled = Vec::new();
        for order in self.list_active_orders(None)? {
            reconciled.push(self.refresh_order(&order.id, Some(&order.symbol)).await?);
        }
        Ok(reconciled)
    }

    /// Place an order and reconcile the exchange response into local state.
    pub async fn place_order(&self, request: PlaceOrder) -> Result<Order> {
        let symbol = normalize_symbol(&request.symbol)?;
        let amount = positive(request.amount, "amount")?;
        let price = request.price.map(|p| positive(p, "price")).transpose()?;
        if request.order_type == OrderType::Limit && price.is_none() {
            return Err(validation("price is required for limit orders"));
        }
        let local_id = request
            .client_order_id
            .as_deref()
            .and_then(normalize_optional_id)
            .unwrap_or_else(local_order_id);
        let mut order = Order::new(
            local_id.clone(),
            symbol.clone(),
            request.order_type,
            request.side,
            amount,
        );
        order.price = price;
        order.remaining_amount = amount;
        {
            let mut st = self.lock();
            if st.orders.contains_key(&local_id) {
                return Err(validation(format!("duplicate local order id: {local_id}")));
            }
            self.store(&mut st, order)?;
        }

        let exchange_request = ExchangeOrderRequest {
            symbol,
            order_type: request.order_type,
            side: request.side,
            amount,
            price,
        };
        let payload = match self.exchange.create_order(&exchange_request).await {
            Ok(payload) => payload,
            Err(err) => {
                let mut st = self.lock();
                if let Some(entry) = st.orders.get_mut(&local_id) {
                    OrderStateMachine::transition(&mut entry.order, OrderStatus::Rejected)?;
                    let order = entry.order.clone();
                    sync_active(&mut st, &order);
                }
                return Err(err.into());
            }
        };

        let mut st = self.lock();
        let order = self.reconcile(&mut st, &local_id, &payload, Some(OrderStatus::Open))?;
        Ok(order)
    }

    /// Cancel an active order and update local state with the response.
    pub async fn cancel_order(&self, order_id: &str, symbol: Option<&str>) -> Result<Order> {
        let order_id = normalize_id(order_id)?;
        let symbol = {
            let st = self.lock();
            match st.orders.get(&order_id) {
                None => match symbol {
                    None => return Err(OrderManagerError::NotFound(order_id)),
                    Some(s) => normalize_symbol(s)?,
                },
                Some(entry) => {
                    if entry.order.status.is_terminal() {
                        return Err(OrderManagerError::InvalidTransition {
                            order_id: entry.order.id.clone(),
                            current: entry.order.status,
                            target: OrderStatus::Canceled,
                        });
                    }
                    normalize_symbol(symbol.unwrap_or(&entry.order.symbol))?
                }
            }
        };

        let payload = self.exchange.cancel_order(&order_id, &symbol).await?;
        let mut st = self.lock();
        if st.orders.contains_key(&order_id) {
            self.reconcile(&mut st, &order_id, &payload, Some(OrderStatus::Canceled))
        } else {
            let order = order_from_payload(
                &mut st,
                &payload,
                Some(&order_id),
                Some(&symbol),
                Some(OrderStatus::Canceled),
            )?;
            self.store(&mut st, order.clone())?;
            Ok(order)
        }
    }

    /// Fetch an order from the exchange and reconcile local state.
    pub async fn refresh_order(&self, order_id: &str, symbol: Option<&str>) -> Result<Order> {
        let order_id = normalize_id(order_id)?;
        let symbol = {
            let st = self.lock();
            match (symbol, st.orders.get(&order_id)) {
                (Some(s), _) => Some(normalize_symbol(s)?),
                (None, Some(entry)) => Some(normalize_symbol(&entry.order.symbol)?),
                (None, None) => None,
            }
        };

        let payload = self
            .exchange
            .fetch_order(&order_id, symbol.as_deref())
            .await?;
        let mut st = self.lock();
        if st.orders.contains_key(&order_id) {
            self.reconcile(&mut st, &order_id, &payload, None)
        } else {
            let order =
                order_from_payload(&mut st, &payload, Some(&order_id), symbol.as_deref(), None)?;
            self.store(&mut st, order.clone())?;
            Ok(order)
        }
    }

    /// Local order snapshot, optionally refreshed from the exchange.
    pub async fn get_order(
        &self,
        order_id: &str,
        symbol: Option<&str>,
        refresh: bool,
    ) -> Result<Order> {
        if refresh {
            return self.refresh_order(order_id, symbol).await;
        }
        let order_id = normalize_id(order_id)?;
        self.lock()
            .orders
            .get(&order_id)
            .map(|e| e.order.clone())
            .ok_or(OrderManagerError::NotFound(order_id))
    }

    pub async fn get_order_status(
        &self,
        order_id: &str,
        symbol: Option<&str>,
        refresh: bool,
    ) -> Result<OrderStatus> {
        Ok(self.get_order(order_id, symbol, refresh).await?.status)
    }

    /// Apply a REST or WebSocket order payload to local state.
    pub fn apply_order_update(&self, payload: &OrderPayload) -> Result<Order> {
        let mut st = self.lock();
        let order_id = extract_order_id(payload, None)?;
        if st.orders.contains_key(&order_id) {
            self.reconcile(&mut st, &order_id, payload, None)
        } else {
            let order = order_from_payload(&mut st, payload, Some(&order_id), None, None)?;
            self.store(&mut st, order.clone())?;
            Ok(order)
        }
    }

    /// Snapshots of every known order, sorted by timestamp.
    pub fn list_orders(&self, symbol: Option<&str>) -> Result<Vec<Order>> {
        let symbol = symbol.map(normalize_symbol).transpose()?;
        let st = self.lock();
        let mut entries: Vec<&Entry> = match &symbol {
            None => st.orders.values().collect(),
            Some(s) => st
                .ids_by_symbol
                .get(s)
                .into_iter()
                .flatten()
                .filter_map(|id| st.orders.get(id))
                .collect(),
        };
        entries.sort_by_key(|e| (e.order.timestamp, e.seq));
        Ok(entries.into_iter().map(|e| e.order.clone()).collect())
    }

    /// Snapshots of pending and open orders, sorted by timestamp.
    pub fn list_active_orders(&self, symbol: Option<&str>) -> Result<Vec<Order>> {
        let symbol = symbol.map(normalize_symbol).transpose()?;
        let st = self.lock();
        let mut entries: Vec<&Entry> = st
            .active_ids
            .iter()
            .filter_map(|id| st.orders.get(id))
            .filter(|e| symbol.as_ref().is_none_or(|s| &e.order.symbol == s))
            .collect();
        entries.sort_by_key(|e| (e.order.timestamp, e.seq));
        Ok(entries.into_iter().map(|e| e.order.clone()).collect())
    }

    /// Alias for [`Self::list_active_orders`].
    pub fn list_open_orders(&self, symbol: Option<&str>) -> Result<Vec<Order>> {
        self.list_active_orders(symbol)
    }

    /// Current open position for a symbol, if any.
    pub fn get_position(&self, symbol: &str) -> Result<Option<Position>> {
        let symbol = normalize_symbol(symbol)?;
        Ok(self.lock().positions.get(&symbol).cloned())
    }

    /// All non-flat positions, sorted by symbol.
    pub fn list_positions(&self) -> Vec<Position> {
        self.lock().positions.values().cloned().collect()
    }

    /// Insert or replace an order in every index and persist it.
    fn store(&self, st: &mut State, order: Order) -> Result<()> {
        if let Some(previous) = st.orders.get(&order.id)
            && previous.order.symbol != order.symbol
        {
            let previous_symbol = previous.order.symbol.clone();
            remove_symbol_id(st, &previous_symbol, &order.id);
        }
        st.ids_by_symbol
            .entry(order.symbol.clone())
            .or_default()
            .insert(order.id.clone());
        sync_active(st, &order);
        if let Some(db) = &self.db {
            db.upsert_order(&order)?;
        }
        let seq = match st.orders.get(&order.id) {
            Some(entry) => entry.seq,
            None => bump(st),
        };
        st.orders.insert(order.id.clone(), Entry { order, seq });
        Ok(())
    }

    /// Replace a local temporary id with the exchange order id.
    fn rekey(&self, st: &mut State, mut order: Order, target_id: &str) -> Result<Order> {
        if order.id == target_id {
            return Ok(order);
        }
        if st.orders.contains_key(target_id) {
            return Err(validation(format!(
                "duplicate order id from exchange: {target_id}"
            )));
        }
        let previous_id = order.id.clone();
        st.orders.remove(&previous_id);
        st.active_ids.remove(&previous_id);
        if let Some(ids) = st.ids_by_symbol.get_mut(&order.symbol) {
            ids.remove(&previous_id);
        }
        if let Some(db) = &self.db {
            db.delete_order(&previous_id)?;
        }
        if let Some(applied) = st.applied.remove(&previous_id)
            && !applied.is_zero()
        {
            st.applied.insert(target_id.to_owned(), applied);
        }
        order.id = target_id.to_owned();
        self.store(st, order.clone())?;
        Ok(order)
    }

    /// Merge an exchange payload into the known order `order_id`.
    fn reconcile(
        &self,
        st: &mut State,
        order_id: &str,
        payload: &OrderPayload,
        default_status: Option<OrderStatus>,
    ) -> Result<Order> {
        let current = st
            .orders
            .get(order_id)
            .map(|e| e.order.clone())
            .ok_or_else(|| OrderManagerError::NotFound(order_id.to_owned()))?;

        let exchange_id = extract_order_id(payload, Some(&current.id))?;
        let new_symbol = extract_symbol(payload, Some(&current.symbol))?;
        let new_type = extract_order_type(payload, Some(current.order_type))?;
        let new_side = extract_order_side(payload, Some(current.side))?;
        let new_amount = extract_amount(payload, Some(current.amount))?;
        let new_price = extract_price(payload, current.price);
        let new_filled = extract_filled(payload, current.filled_amount);
        let new_remaining = extract_remaining(payload, new_amount, new_filled);
        let previous_average = current.avg_fill_price;
        let new_average = extract_average(payload, new_filled, current.avg_fill_price);
        let target_status = extract_status(
            payload,
            Some(default_status.unwrap_or(current.status)),
            new_amount,
            new_filled,
        )?;
        let incoming_fills = extract_fills(payload, &exchange_id, &new_symbol, new_side)?;

        if !OrderStateMachine::can_transition(current.status, target_status) {
            return Err(OrderManagerError::InvalidTransition {
                order_id: current.id,
                current: current.status,
                target: target_status,
            });
        }

        let mut order = self.rekey(st, current, &exchange_id)?;

        let previous_symbol = std::mem::replace(&mut order.symbol, new_symbol.clone());
        if previous_symbol != new_symbol {
            remove_symbol_id(st, &previous_symbol, &order.id);
            st.ids_by_symbol
                .entry(new_symbol)
                .or_default()
                .insert(order.id.clone());
        }
        order.order_type = new_type;
        order.side = new_side;
        order.amount = new_amount;
        order.price = new_price;
        order.remaining_amount = new_remaining;
        order.avg_fill_price = new_average;
        OrderStateMachine::transition(&mut order, target_status)?;

        let incoming_total: Decimal = incoming_fills.iter().map(|f| f.amount).sum();
        let mut existing = std::mem::take(&mut order.fills);
        if !incoming_fills.is_empty() && incoming_total >= new_filled - EPSILON {
            let synthetic_prefix = format!("{}:synthetic:", order.id);
            existing.retain(|f| !f.id.starts_with(&synthetic_prefix));
        }
        order.fills = merge_fills(existing, incoming_fills);
        apply_position_delta(st, &mut order, new_filled, previous_average)?;
        order.filled_amount = new_filled;
        sync_active(st, &order);
        if let Some(db) = &self.db {
            db.upsert_order(&order)?;
        }
        let seq = match st.orders.get(&order.id) {
            Some(entry) => entry.seq,
            None => bump(st),
        };
        st.orders.insert(
            order.id.clone(),
            Entry {
                order: order.clone(),
                seq,
            },
        );
        Ok(order)
    }
}

fn bump(st: &mut State) -> u64 {
    st.next_seq += 1;
    st.next_seq
}

fn remove_symbol_id(st: &mut State, symbol: &str, order_id: &str) {
    if let Some(ids) = st.ids_by_symbol.get_mut(symbol) {
        ids.remove(order_id);
        if ids.is_empty() {
            st.ids_by_symbol.remove(symbol);
        }
    }
}

fn sync_active(st: &mut State, order: &Order) {
    if order.status.is_active() {
        st.active_ids.insert(order.id.clone());
    } else {
        st.active_ids.remove(&order.id);
    }
}

/// Build an order from a payload not already known locally (applies its fills
/// to the position, like Python's `_order_from_payload_unlocked`).
fn order_from_payload(
    st: &mut State,
    payload: &OrderPayload,
    fallback_id: Option<&str>,
    fallback_symbol: Option<&str>,
    default_status: Option<OrderStatus>,
) -> Result<Order> {
    let order_id = extract_order_id(payload, fallback_id)?;
    let symbol = extract_symbol(payload, fallback_symbol)?;
    let order_type = extract_order_type(payload, None)?;
    let side = extract_order_side(payload, None)?;
    let amount = extract_amount(payload, None)?;
    let filled = extract_filled(payload, Decimal::ZERO);
    let remaining = extract_remaining(payload, amount, filled);
    let status = extract_status(payload, default_status, amount, filled)?;
    let fills = extract_fills(payload, &order_id, &symbol, side)?;
    let mut order = Order {
        id: order_id,
        symbol,
        order_type,
        side,
        amount,
        price: extract_price(payload, None),
        status,
        filled_amount: filled,
        remaining_amount: remaining,
        avg_fill_price: extract_average(payload, filled, None),
        fills,
        timestamp: payload.timestamp.unwrap_or_else(utc_now),
    };
    apply_position_delta(st, &mut order, filled, None)?;
    Ok(order)
}

/// Apply only newly filled quantity to the symbol position.
fn apply_position_delta(
    st: &mut State,
    order: &mut Order,
    target_filled: Decimal,
    previous_average: Option<Decimal>,
) -> Result<()> {
    let already_applied = st.applied.get(&order.id).copied().unwrap_or_default();
    let delta = target_filled - already_applied;
    if delta <= EPSILON {
        return Ok(());
    }
    let price = position_fill_price(
        order,
        delta,
        target_filled,
        already_applied,
        previous_average,
    )?;
    apply_position_fill(st, &order.symbol, order.side, delta, price);
    st.applied.insert(order.id.clone(), target_filled);
    if order.fills.is_empty() {
        order.fills.push(Fill {
            id: format!("{}:synthetic:{}", order.id, format_g(target_filled)),
            order_id: order.id.clone(),
            symbol: order.symbol.clone(),
            side: order.side,
            price,
            amount: delta,
            fee: Decimal::ZERO,
            fee_currency: String::new(),
            timestamp: utc_now(),
        });
    }
    Ok(())
}

/// Update the net position for a single fill.
fn apply_position_fill(
    st: &mut State,
    symbol: &str,
    side: OrderSide,
    amount: Decimal,
    price: Decimal,
) {
    let signed = if side == OrderSide::Buy {
        amount
    } else {
        -amount
    };
    let Some(existing) = st.positions.get_mut(symbol) else {
        if signed.abs() > EPSILON {
            st.positions
                .insert(symbol.to_owned(), Position::new(symbol, signed, price));
        }
        return;
    };
    let old_amount = existing.amount;
    let new_amount = old_amount + signed;
    if new_amount.abs() <= EPSILON {
        st.positions.remove(symbol);
        return;
    }
    if same_sign(old_amount, signed) {
        let old_notional = old_amount.abs() * existing.entry_price;
        let fill_notional = signed.abs() * price;
        existing.entry_price = (old_notional + fill_notional) / new_amount.abs();
    } else if !same_sign(old_amount, new_amount) {
        existing.entry_price = price;
    }
    existing.amount = new_amount;
    existing.timestamp = utc_now();
}

fn same_sign(left: Decimal, right: Decimal) -> bool {
    (left >= Decimal::ZERO && right >= Decimal::ZERO)
        || (left < Decimal::ZERO && right < Decimal::ZERO)
}

/// Best available price for a newly applied fill delta.
fn position_fill_price(
    order: &Order,
    delta: Decimal,
    target_filled: Decimal,
    already_applied: Decimal,
    previous_average: Option<Decimal>,
) -> Result<Decimal> {
    if let Some(price) = price_from_fills(&order.fills, delta, target_filled, already_applied) {
        return Ok(price);
    }
    if let Some(avg) = order.avg_fill_price.filter(|p| *p > Decimal::ZERO) {
        if let Some(prev) = previous_average.filter(|p| *p > Decimal::ZERO)
            && already_applied > EPSILON
        {
            let delta_cost = target_filled * avg - already_applied * prev;
            if delta_cost > EPSILON {
                return Ok(delta_cost / delta);
            }
        }
        return Ok(avg);
    }
    if let Some(price) = order.price.filter(|p| *p > Decimal::ZERO) {
        return Ok(price);
    }
    Err(validation("filled order is missing a usable fill price"))
}

/// Weighted delta price from detailed fill records.
fn price_from_fills(
    fills: &[Fill],
    delta: Decimal,
    target_filled: Decimal,
    already_applied: Decimal,
) -> Option<Decimal> {
    if fills.is_empty() {
        return None;
    }
    let total: Decimal = fills.iter().map(|f| f.amount).sum();
    if total <= EPSILON {
        return None;
    }
    if total <= delta + EPSILON {
        return Some(fills.iter().map(|f| f.amount * f.price).sum::<Decimal>() / total);
    }
    if total + EPSILON < target_filled {
        return None;
    }
    let mut skip = already_applied;
    let mut take = delta;
    let mut cost = Decimal::ZERO;
    let mut taken_total = Decimal::ZERO;
    for fill in fills {
        let mut amount = fill.amount;
        if skip > EPSILON {
            let skipped = skip.min(amount);
            amount -= skipped;
            skip -= skipped;
        }
        if amount <= EPSILON {
            continue;
        }
        let taken = take.min(amount);
        cost += taken * fill.price;
        taken_total += taken;
        take -= taken;
        if take <= EPSILON {
            break;
        }
    }
    (taken_total > EPSILON).then(|| cost / taken_total)
}

fn merge_fills(mut existing: Vec<Fill>, incoming: Vec<Fill>) -> Vec<Fill> {
    let mut known: HashSet<String> = existing.iter().map(|f| f.id.clone()).collect();
    for fill in incoming {
        if known.insert(fill.id.clone()) {
            existing.push(fill);
        }
    }
    existing
}

fn normalize_symbol(symbol: &str) -> Result<String> {
    let trimmed = symbol.trim();
    if trimmed.is_empty() {
        return Err(validation("symbol must not be blank"));
    }
    Ok(trimmed.to_owned())
}

fn normalize_optional_id(id: &str) -> Option<String> {
    let trimmed = id.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn normalize_id(id: &str) -> Result<String> {
    normalize_optional_id(id).ok_or_else(|| validation("order_id must not be blank"))
}

fn local_order_id() -> String {
    format!("local-{}", uuid::Uuid::new_v4().simple())
}

fn positive(value: Decimal, field: &str) -> Result<Decimal> {
    if value <= Decimal::ZERO {
        return Err(validation(format!("{field} must be greater than zero")));
    }
    Ok(value)
}

fn extract_order_id(payload: &OrderPayload, fallback: Option<&str>) -> Result<String> {
    payload
        .id
        .as_deref()
        .and_then(normalize_optional_id)
        .or_else(|| fallback.and_then(normalize_optional_id))
        .ok_or_else(|| validation("order payload is missing an order id"))
}

fn extract_symbol(payload: &OrderPayload, fallback: Option<&str>) -> Result<String> {
    match payload.symbol.as_deref().or(fallback) {
        Some(symbol) => normalize_symbol(symbol),
        None => Err(validation("order payload is missing a symbol")),
    }
}

fn extract_order_type(payload: &OrderPayload, fallback: Option<OrderType>) -> Result<OrderType> {
    match payload.order_type.as_deref() {
        Some(raw) => {
            let normalized = raw.trim().to_lowercase();
            let normalized = if matches!(normalized.as_str(), "limit_maker" | "limit maker") {
                "limit".to_owned()
            } else {
                normalized
            };
            normalized
                .parse()
                .map_err(|_| validation("order payload has unsupported order type"))
        }
        None => fallback.ok_or_else(|| validation("order payload is missing an order type")),
    }
}

fn extract_order_side(payload: &OrderPayload, fallback: Option<OrderSide>) -> Result<OrderSide> {
    match payload.side.as_deref() {
        Some(raw) => raw
            .parse()
            .map_err(|_| validation("order payload has unsupported order side")),
        None => fallback.ok_or_else(|| validation("order payload is missing an order side")),
    }
}

fn extract_amount(payload: &OrderPayload, fallback: Option<Decimal>) -> Result<Decimal> {
    match payload.amount.or(fallback) {
        Some(amount) if amount <= Decimal::ZERO => {
            Err(validation("order amount must be greater than zero"))
        }
        Some(amount) => Ok(amount),
        None => Err(validation("order payload is missing an amount")),
    }
}

fn extract_price(payload: &OrderPayload, fallback: Option<Decimal>) -> Option<Decimal> {
    payload
        .price
        .filter(|p| *p > Decimal::ZERO)
        .or(fallback.filter(|p| *p > Decimal::ZERO))
}

fn extract_filled(payload: &OrderPayload, fallback: Decimal) -> Decimal {
    if let Some(filled) = payload.filled {
        return filled.max(Decimal::ZERO);
    }
    if !payload.fills.is_empty() {
        return payload.fills.iter().filter_map(|f| f.amount).sum();
    }
    fallback
}

fn extract_remaining(payload: &OrderPayload, amount: Decimal, filled: Decimal) -> Decimal {
    match payload.remaining {
        Some(remaining) => remaining.max(Decimal::ZERO),
        None => (amount - filled).max(Decimal::ZERO),
    }
}

fn extract_average(
    payload: &OrderPayload,
    filled: Decimal,
    fallback: Option<Decimal>,
) -> Option<Decimal> {
    if let Some(avg) = payload.average.filter(|p| *p > Decimal::ZERO) {
        return Some(avg);
    }
    if let Some(cost) = payload.cost.filter(|c| !c.is_zero())
        && filled > EPSILON
    {
        return Some(cost / filled);
    }
    fallback
}

fn extract_status(
    payload: &OrderPayload,
    default_status: Option<OrderStatus>,
    amount: Decimal,
    filled: Decimal,
) -> Result<OrderStatus> {
    if let Some(raw) = payload.status.as_deref().filter(|s| !s.is_empty()) {
        return normalize_order_status(raw);
    }
    if filled >= amount - EPSILON {
        return Ok(OrderStatus::Closed);
    }
    Ok(default_status.unwrap_or(OrderStatus::Pending))
}

/// Map ccxt and raw Binance statuses to local lifecycle statuses.
pub fn normalize_order_status(raw: &str) -> Result<OrderStatus> {
    Ok(match raw.trim().to_lowercase().as_str() {
        "pending" => OrderStatus::Pending,
        "new" | "open" | "partially_filled" | "partially filled" => OrderStatus::Open,
        "filled" | "closed" | "done" => OrderStatus::Closed,
        "canceled" | "cancelled" => OrderStatus::Canceled,
        "expired" => OrderStatus::Expired,
        "rejected" => OrderStatus::Rejected,
        _ => return Err(validation(format!("unsupported order status: {raw}"))),
    })
}

fn extract_fills(
    payload: &OrderPayload,
    order_id: &str,
    symbol: &str,
    side: OrderSide,
) -> Result<Vec<Fill>> {
    let mut fills = Vec::new();
    for (index, raw) in payload.fills.iter().enumerate() {
        let (Some(price), Some(amount)) = (raw.price.filter(|p| *p > Decimal::ZERO), raw.amount)
        else {
            continue;
        };
        let id = raw
            .id
            .as_deref()
            .and_then(normalize_optional_id)
            .unwrap_or_else(|| {
                format!(
                    "{order_id}:fill:{index}:{}@{}",
                    format_g(amount),
                    format_g(price)
                )
            });
        let fill_symbol = match raw.symbol.as_deref() {
            Some(s) => normalize_symbol(s)?,
            None => symbol.to_owned(),
        };
        let fill_side = match raw.side.as_deref() {
            Some(s) => s.parse().map_err(OrderManagerError::Validation)?,
            None => side,
        };
        fills.push(Fill {
            id,
            order_id: order_id.to_owned(),
            symbol: fill_symbol,
            side: fill_side,
            price,
            amount: amount.max(Decimal::ZERO),
            fee: raw.fee,
            fee_currency: raw.fee_currency.clone(),
            timestamp: raw.timestamp.unwrap_or_else(utc_now),
        });
    }
    Ok(fills)
}
