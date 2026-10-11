//! Bounded single-consumer queue that drops the **oldest** item when full,
//! preserving freshness for market data (Python `asyncio.Queue` +
//! "drop oldest on QueueFull" semantics). Capacity 0 means unbounded.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Notify;

/// Drop-oldest bounded queue.
#[derive(Debug)]
pub struct DropOldestQueue<T> {
    items: Mutex<VecDeque<T>>,
    capacity: usize,
    notify: Notify,
    dropped: AtomicU64,
}

impl<T> DropOldestQueue<T> {
    /// New queue; `capacity == 0` is unbounded.
    pub fn new(capacity: usize) -> Self {
        Self {
            items: Mutex::new(VecDeque::new()),
            capacity,
            notify: Notify::new(),
            dropped: AtomicU64::new(0),
        }
    }

    /// Enqueue, evicting the oldest item when full. Returns `true` if an item was dropped.
    pub fn push(&self, item: T) -> bool {
        let mut dropped = false;
        if let Ok(mut items) = self.items.lock() {
            if self.capacity > 0 && items.len() >= self.capacity {
                items.pop_front();
                dropped = true;
            }
            items.push_back(item);
        }
        if dropped {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.notify.notify_one();
        dropped
    }

    /// Dequeue without waiting.
    pub fn try_pop(&self) -> Option<T> {
        self.items
            .lock()
            .ok()
            .and_then(|mut items| items.pop_front())
    }

    /// Wait for the next item.
    pub async fn pop(&self) -> T {
        loop {
            if let Some(item) = self.try_pop() {
                return item;
            }
            self.notify.notified().await;
        }
    }

    /// Items currently queued.
    pub fn len(&self) -> usize {
        self.items.lock().map(|items| items.len()).unwrap_or(0)
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Configured capacity (0 = unbounded).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Items evicted so far because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drops_oldest_when_full_and_wakes_consumer() {
        let queue = std::sync::Arc::new(DropOldestQueue::new(2));
        assert!(!queue.push(1));
        assert!(!queue.push(2));
        assert!(queue.push(3));
        assert_eq!(queue.dropped(), 1);
        assert_eq!(queue.pop().await, 2);
        assert_eq!(queue.pop().await, 3);
        let consumer = {
            let queue = queue.clone();
            tokio::spawn(async move { queue.pop().await })
        };
        tokio::task::yield_now().await;
        queue.push(9);
        assert_eq!(consumer.await.ok(), Some(9));
        let unbounded = DropOldestQueue::new(0);
        for i in 0..5000 {
            unbounded.push(i);
        }
        assert_eq!(unbounded.len(), 5000);
    }
}
