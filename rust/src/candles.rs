//! Candles built straight from ticks: one open candle per market and
//! interval. A candle is complete when the first tick of a later bucket
//! arrives. Memory is bounded by markets x intervals, gaps included.
//!
//! The Python service built each interval from the one below and only when
//! every smaller candle arrived, so quiet markets lost candles and leaked
//! memory, and 3m/4m/5m/15m were built from misaligned buckets.

use std::collections::HashMap;

use crate::model::{Candle, Interval, PriceUpdate};

#[derive(Debug, Clone, Copy)]
struct Acc {
    start: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

impl Acc {
    fn new(start: i64, price: f64, volume: f64) -> Self {
        Self {
            start,
            open: price,
            high: price,
            low: price,
            close: price,
            volume,
        }
    }

    fn add(&mut self, price: f64, volume: f64) {
        self.high = self.high.max(price);
        self.low = self.low.min(price);
        self.close = price;
        self.volume += volume;
    }
}

#[derive(Default)]
struct Market {
    last_quote_volume: Option<f64>,
    open: [Option<Acc>; Interval::BUILT.len()],
}

#[derive(Default)]
pub struct CandleBook {
    markets: HashMap<(&'static str, String), Market>,
}

impl CandleBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tick; returns the candles it completed (smallest interval first).
    pub fn add(&mut self, u: &PriceUpdate) -> Vec<Candle> {
        if !(u.price.is_finite() && u.price > 0.0) {
            return Vec::new();
        }
        let m = self
            .markets
            .entry((u.exchange, u.symbol.clone()))
            .or_default();
        let mut volume = 0.0;
        if let Some(cum) = u.quote_volume_24h {
            if let Some(last) = m.last_quote_volume {
                if cum >= last {
                    volume = cum - last;
                }
            }
            m.last_quote_volume = Some(cum);
        }

        let mut done = Vec::new();
        for (i, interval) in Interval::BUILT.into_iter().enumerate() {
            let start = interval.bucket_start(u.timestamp);
            match &mut m.open[i] {
                Some(acc) if acc.start == start => acc.add(u.price, volume),
                // A late tick for a finished bucket: ignore it for this interval.
                Some(acc) if acc.start > start => {}
                slot => {
                    if let Some(acc) = slot.take() {
                        done.push(Candle {
                            symbol: u.symbol.clone(),
                            exchange: u.exchange,
                            interval,
                            open: acc.open,
                            high: acc.high,
                            low: acc.low,
                            close: acc.close,
                            start: acc.start,
                            volume: acc.volume,
                        });
                    }
                    *slot = Some(Acc::new(start, u.price, volume));
                }
            }
        }
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn tick(sec: i64, price: f64) -> PriceUpdate {
        PriceUpdate {
            symbol: "XUSDT".into(),
            exchange: "binance_futures",
            price,
            timestamp: Utc
                .timestamp_opt(1_791_000_000 - 1_791_000_000 % 604_800 + sec, 0)
                .unwrap(),
            quote_volume_24h: None,
        }
    }

    #[test]
    fn completes_on_next_bucket_with_ohlc() {
        let mut b = CandleBook::new();
        assert!(b.add(&tick(0, 1.0)).is_empty());
        b.add(&tick(3, 1.5));
        b.add(&tick(5, 0.8));
        b.add(&tick(9, 1.2));
        let done = b.add(&tick(10, 2.0));
        assert_eq!(done.len(), 1);
        let c = &done[0];
        assert_eq!(c.interval, Interval::S10);
        assert_eq!((c.open, c.high, c.low, c.close), (1.0, 1.5, 0.8, 1.2));
    }

    #[test]
    fn gaps_still_complete_larger_candles() {
        let mut b = CandleBook::new();
        // One tick every 20 s for 6 minutes: every 10 s bucket after a gap.
        let mut fives = Vec::new();
        for i in 0..=18 {
            for c in b.add(&tick(i * 20, 1.0 + i as f64 / 100.0)) {
                if c.interval == Interval::M5 {
                    fives.push(c);
                }
            }
        }
        assert_eq!(fives.len(), 1);
        assert_eq!(fives[0].open, 1.0);
        assert!((fives[0].close - 1.14).abs() < 1e-9);
    }

    #[test]
    fn volume_from_24h_deltas() {
        let mut b = CandleBook::new();
        let mut t = tick(0, 1.0);
        t.quote_volume_24h = Some(100.0);
        b.add(&t);
        t.timestamp += chrono::Duration::seconds(2);
        t.quote_volume_24h = Some(130.0);
        b.add(&t);
        t.timestamp += chrono::Duration::seconds(2);
        // A drop (24 h window rolled): no volume.
        t.quote_volume_24h = Some(120.0);
        b.add(&t);
        let done = b.add(&tick(10, 1.0));
        assert_eq!(done[0].volume, 30.0);
    }

    #[test]
    fn late_ticks_are_ignored() {
        let mut b = CandleBook::new();
        b.add(&tick(15, 1.0));
        assert!(b.add(&tick(5, 9.0)).is_empty());
        let done = b.add(&tick(20, 1.0));
        assert_eq!(done[0].high, 1.0);
    }
}
