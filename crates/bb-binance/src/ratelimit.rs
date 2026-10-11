//! Client-side request-weight limiter and server-reported usage tracking.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use reqwest::header::HeaderMap;
use tokio::time::Instant;

use crate::error::{BinanceError, Result};

/// FIFO sliding-window limiter for Binance request weights (port of
/// `AsyncWeightedRateLimiter`). Waiters are served strictly in arrival order:
/// the fair `tokio::sync::Mutex` gate is held while a waiter sleeps.
#[derive(Debug)]
pub struct WeightLimiter {
    limit: u32,
    period: Duration,
    gate: tokio::sync::Mutex<()>,
    window: Mutex<Window>,
}

#[derive(Debug, Default)]
struct Window {
    events: VecDeque<(Instant, u32)>,
    used: u32,
}

impl Window {
    fn prune(&mut self, now: Instant, period: Duration) {
        while let Some(&(at, weight)) = self.events.front() {
            if now.duration_since(at) < period {
                break;
            }
            self.events.pop_front();
            self.used = self.used.saturating_sub(weight);
        }
    }
}

impl WeightLimiter {
    /// Budget `limit` weight per rolling `period`.
    pub fn new(limit: u32, period: Duration) -> Self {
        Self {
            limit: limit.max(1),
            period,
            gate: tokio::sync::Mutex::new(()),
            window: Mutex::new(Window::default()),
        }
    }

    /// Reserve `weight` before sending a request, sleeping until it fits.
    pub async fn acquire(&self, weight: u32) -> Result<()> {
        if weight == 0 {
            return Ok(());
        }
        if weight > self.limit {
            return Err(BinanceError::InvalidRequest(
                "request weight exceeds configured limit".into(),
            ));
        }
        let _turn = self.gate.lock().await;
        loop {
            let sleep_for = {
                let mut window = self
                    .window
                    .lock()
                    .map_err(|_| BinanceError::InvalidRequest("rate limiter poisoned".into()))?;
                let now = Instant::now();
                window.prune(now, self.period);
                if window.used + weight <= self.limit {
                    window.events.push_back((now, weight));
                    window.used += weight;
                    return Ok(());
                }
                window
                    .events
                    .front()
                    .map(|&(at, _)| (at + self.period).saturating_duration_since(now))
                    .unwrap_or_default()
                    .max(Duration::from_millis(50))
            };
            tracing::debug!(
                request_weight = weight,
                weight_limit = self.limit,
                sleep_ms = sleep_for.as_millis() as u64,
                "Binance request weight throttled"
            );
            tokio::time::sleep(sleep_for).await;
        }
    }

    /// Weight reserved in the current window.
    pub fn used(&self) -> u32 {
        self.window
            .lock()
            .map(|mut w| {
                w.prune(Instant::now(), self.period);
                w.used
            })
            .unwrap_or(0)
    }
}

/// Server-reported usage from the last response of one API family.
///
/// Keys are lower-case header suffixes, e.g. `used-weight-1m`, `order-count-10s`,
/// `order-count-1d`, `sapi-used-ip-weight-1m`, `sapi-used-uid-weight-1m`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RateLimitUsage {
    pub counters: BTreeMap<String, u64>,
    pub updated_at: Option<Instant>,
}

impl RateLimitUsage {
    /// `X-MBX-USED-WEIGHT-1M`.
    pub fn used_weight_1m(&self) -> Option<u64> {
        self.counters.get("used-weight-1m").copied()
    }

    /// `X-MBX-ORDER-COUNT-10S`.
    pub fn order_count_10s(&self) -> Option<u64> {
        self.counters.get("order-count-10s").copied()
    }

    /// `X-MBX-ORDER-COUNT-1D` (spot).
    pub fn order_count_1d(&self) -> Option<u64> {
        self.counters.get("order-count-1d").copied()
    }

    /// `X-MBX-ORDER-COUNT-1M` (futures).
    pub fn order_count_1m(&self) -> Option<u64> {
        self.counters.get("order-count-1m").copied()
    }

    /// Merge usage headers from a response; returns whether any were present.
    pub(crate) fn update_from_headers(&mut self, headers: &HeaderMap) -> bool {
        let mut seen = false;
        for (name, value) in headers {
            let name = name.as_str();
            let key = if let Some(rest) = name.strip_prefix("x-mbx-") {
                if !(rest.starts_with("used-weight-") || rest.starts_with("order-count-")) {
                    continue;
                }
                rest.to_owned()
            } else if let Some(rest) = name.strip_prefix("x-sapi-") {
                format!("sapi-{rest}")
            } else {
                continue;
            };
            if let Some(v) = value
                .to_str()
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
            {
                self.counters.insert(key, v);
                seen = true;
            }
        }
        if seen {
            self.updated_at = Some(Instant::now());
        }
        seen
    }
}

/// Exponential backoff with up to 25 % jitter (port of `_retry_sleep_seconds`).
pub fn backoff_delay(attempt: u32, base: Duration, max: Duration, jitter: f64) -> Duration {
    let factor = 2u32.saturating_pow(attempt.saturating_sub(1));
    let capped = base.saturating_mul(factor).min(max);
    capped + capped.mul_f64(jitter.clamp(0.0, 1.0) * 0.25)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[tokio::test(start_paused = true)]
    async fn limiter_waits_for_window_to_expire() -> Result<()> {
        let limiter = WeightLimiter::new(10, Duration::from_secs(60));
        let start = Instant::now();
        limiter.acquire(6).await?;
        limiter.acquire(4).await?;
        assert_eq!(limiter.used(), 10);
        limiter.acquire(1).await?;
        assert!(start.elapsed() >= Duration::from_secs(60));
        assert_eq!(limiter.used(), 1);
        assert!(limiter.acquire(11).await.is_err());
        Ok(())
    }

    #[test]
    fn backoff_matches_python_schedule_without_jitter() {
        let base = Duration::from_millis(250);
        let max = Duration::from_secs(1);
        assert_eq!(backoff_delay(1, base, max, 0.0), Duration::from_millis(250));
        assert_eq!(backoff_delay(2, base, max, 0.0), Duration::from_millis(500));
        assert_eq!(backoff_delay(4, base, max, 0.0), Duration::from_secs(1));
        assert_eq!(
            backoff_delay(1, base, max, 1.0),
            Duration::from_micros(312_500)
        );
    }

    #[test]
    fn usage_headers_are_tracked() {
        let mut headers = HeaderMap::new();
        headers.insert("x-mbx-used-weight-1m", HeaderValue::from_static("12"));
        headers.insert("x-mbx-order-count-10s", HeaderValue::from_static("3"));
        headers.insert("x-sapi-used-ip-weight-1m", HeaderValue::from_static("40"));
        headers.insert("x-mbx-uuid", HeaderValue::from_static("abc"));
        let mut usage = RateLimitUsage::default();
        assert!(usage.update_from_headers(&headers));
        assert_eq!(usage.used_weight_1m(), Some(12));
        assert_eq!(usage.order_count_10s(), Some(3));
        assert_eq!(usage.counters.get("sapi-used-ip-weight-1m"), Some(&40));
        assert_eq!(usage.counters.len(), 3);
    }
}
