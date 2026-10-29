//! Paged kline backfill (port of `src/data/history.py::HistoryFetcher`).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::error::{BinanceError, Result};
use crate::models::{Kline, Ohlcv};
use crate::rest::BinanceClient;
use crate::symbols::exchange_id;

/// Rows per page requested from Binance.
pub const PAGE_LIMIT: u32 = 1_000;

/// Fetches complete kline history by walking `startTime` forward page by page.
#[derive(Debug, Clone)]
pub struct HistoryFetcher {
    client: BinanceClient,
    limit: u32,
}

impl HistoryFetcher {
    /// Spot klines (`/api/v3/klines`).
    pub fn spot(client: BinanceClient) -> Self {
        Self {
            client,
            limit: PAGE_LIMIT,
        }
    }

    /// Fetch every kline whose open time lies in `[start, end]` (or `[start, now]`),
    /// de-duplicated by open time and sorted ascending.
    ///
    /// Pagination: next `startTime` = last open time of the page + 1; stops on an
    /// empty page, a short page (`< limit`), or once past `end`. A page that does
    /// not advance is an error.
    pub async fn fetch_klines(
        &self,
        symbol: &str,
        interval: &str,
        start: DateTime<Utc>,
        end: Option<DateTime<Utc>>,
    ) -> Result<Vec<Kline>> {
        let rest_symbol = exchange_id(symbol)?;
        let interval = interval.trim();
        if interval.is_empty() {
            return Err(BinanceError::InvalidRequest(
                "interval must not be blank".into(),
            ));
        }
        let start_ms = start.timestamp_millis();
        let end_ms = end.map(|e| e.timestamp_millis());
        if end_ms.is_some_and(|e| start_ms > e) {
            return Err(BinanceError::InvalidRequest(
                "start_time must be before or equal to end_time".into(),
            ));
        }
        tracing::info!(symbol = %rest_symbol, interval, start_time_ms = start_ms, end_time_ms = ?end_ms, "Starting Binance REST kline history fetch");

        let mut by_open_time: BTreeMap<i64, Kline> = BTreeMap::new();
        let mut next_start = start_ms;
        let mut page_number = 0u32;
        while end_ms.is_none_or(|e| next_start <= e) {
            page_number += 1;
            let mut page = self
                .client
                .klines(
                    &rest_symbol,
                    interval,
                    Some(next_start),
                    end_ms,
                    Some(self.limit),
                )
                .await?;
            if page.is_empty() {
                tracing::info!(symbol = %rest_symbol, page_number, total_klines = by_open_time.len(), "No klines returned; stopping history fetch");
                break;
            }
            let page_len = page.len();
            page.sort_by_key(|k| k.open_time);
            let last_open = page.last().map_or(next_start, |k| k.open_time);
            for kline in page {
                if kline.open_time < start_ms || end_ms.is_some_and(|e| kline.open_time > e) {
                    continue;
                }
                by_open_time.insert(kline.open_time, kline);
            }
            let next_page_start = last_open + 1;
            tracing::info!(
                symbol = %rest_symbol,
                page_number,
                page_size = page_len,
                total_klines = by_open_time.len(),
                next_start_ms = next_page_start,
                "Fetched Binance kline page"
            );
            if next_page_start <= next_start {
                return Err(BinanceError::History(
                    "Binance kline pagination did not advance".into(),
                ));
            }
            next_start = next_page_start;
            if page_len < self.limit as usize {
                break;
            }
        }
        tracing::info!(symbol = %rest_symbol, total_klines = by_open_time.len(), "Completed Binance REST kline history fetch");
        Ok(by_open_time.into_values().collect())
    }

    /// [`Self::fetch_klines`] reshaped to ccxt OHLCV rows.
    pub async fn fetch_ohlcv(
        &self,
        symbol: &str,
        timeframe: &str,
        start: DateTime<Utc>,
        end: Option<DateTime<Utc>>,
    ) -> Result<Vec<Ohlcv>> {
        Ok(self
            .fetch_klines(symbol, timeframe, start, end)
            .await?
            .iter()
            .map(Kline::ohlcv)
            .collect())
    }
}
