//! Candles built straight from ticks: one open candle per market and
//! interval. A candle is complete when the first tick of a later bucket
//! arrives. Memory is bounded by markets x intervals, gaps included.
//!
//! Ticker feeds (Binance mini ticker, Bybit/Gate tickers) give one price a
//! second, not every trade, so two fixes keep their candles close to the
//! exchange's own without calling it:
//! - A snapshot candle opens at the previous candle's close (the last price
//!   until the next trade), not at its first snapshot. A listing pump that
//!   starts in the first second used to open the candle at the top.
//! - A rise of the feed's rolling 24 h high (fall of its low) since the
//!   previous tick means a trade printed there, so it becomes the candle's
//!   high (low). Wicks between snapshots used to be lost. The trade may
//!   belong to the second before the bucket started; it counts in the new one.
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

    /// Widen to a trade price seen only through the 24 h high/low.
    fn extend(&mut self, high: Option<f64>, low: Option<f64>) {
        if let Some(h) = high {
            self.high = self.high.max(h);
        }
        if let Some(l) = low {
            self.low = self.low.min(l);
        }
    }
}

/// Remember the feed's latest 24 h extreme and return it when it moved past
/// the previous one (a new trade there). The first value seen is only kept:
/// it may be hours old.
fn rise(last: &mut Option<f64>, new: Option<f64>, past: fn(f64, f64) -> bool) -> Option<f64> {
    let new = new.filter(|v| v.is_finite() && *v > 0.0)?;
    let moved = last.is_some_and(|old| past(new, old));
    *last = Some(new);
    moved.then_some(new)
}

#[derive(Default)]
struct Market {
    last_quote_volume: Option<f64>,
    last_high_24h: Option<f64>,
    last_low_24h: Option<f64>,
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
        let mut volume = u.volume.unwrap_or(0.0);
        if let Some(cum) = u.quote_volume_24h.filter(|_| u.volume.is_none()) {
            if let Some(last) = m.last_quote_volume {
                if cum >= last {
                    volume = cum - last;
                }
            }
            m.last_quote_volume = Some(cum);
        }

        let new_high = rise(&mut m.last_high_24h, u.high_24h, |new, old| new > old);
        let new_low = rise(&mut m.last_low_24h, u.low_24h, |new, old| new < old);
        let snapshot = u.volume.is_none();

        let mut done = Vec::new();
        for (i, interval) in Interval::BUILT.into_iter().enumerate() {
            let start = interval.bucket_start(u.timestamp);
            match &mut m.open[i] {
                Some(acc) if acc.start == start => {
                    acc.add(u.price, volume);
                    acc.extend(new_high, new_low);
                }
                // A late tick for a finished bucket: ignore it for this interval.
                Some(acc) if acc.start > start => {}
                slot => {
                    let prev_close = slot.as_ref().map(|a| a.close);
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
                    let mut acc = match prev_close.filter(|_| snapshot) {
                        Some(close) => {
                            let mut a = Acc::new(start, close, volume);
                            a.add(u.price, 0.0);
                            a
                        }
                        None => Acc::new(start, u.price, volume),
                    };
                    acc.extend(new_high, new_low);
                    *slot = Some(acc);
                }
            }
        }
        done
    }

    /// The candle still being built for a market and interval.
    pub fn open_candle(&self, exchange: &str, symbol: &str, interval: Interval) -> Option<Candle> {
        let i = Interval::BUILT.iter().position(|x| *x == interval)?;
        let (key, m) = self
            .markets
            .get_key_value(&(exchange_static(exchange)?, symbol.to_string()))?;
        let acc = m.open[i]?;
        Some(Candle {
            symbol: key.1.clone(),
            exchange: key.0,
            interval,
            open: acc.open,
            high: acc.high,
            low: acc.low,
            close: acc.close,
            start: acc.start,
            volume: acc.volume,
        })
    }
}

/// The feed's static name for an exchange name (map keys hold `&'static str`).
fn exchange_static(name: &str) -> Option<&'static str> {
    crate::feeds::EXCHANGES.iter().copied().find(|e| *e == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn tick(sec: i64, price: f64) -> PriceUpdate {
        PriceUpdate::last_trade(
            "XUSDT".into(),
            "binance_futures",
            price,
            Utc.timestamp_opt(1_791_000_000 - 1_791_000_000 % 604_800 + sec, 0)
                .unwrap(),
        )
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

    /// CTUSDT 2026-10-19 07:40 on Binance: the pump began 1.2 s into the
    /// minute, the first snapshot already read the 15% band cap 0.34771 and
    /// the top (0.435) printed between snapshots. Exchange candle: O 0.3025
    /// H 0.435 L 0.3025 C 0.38888.
    #[test]
    fn snapshot_candle_opens_at_previous_close_and_keeps_wicks() {
        let mut b = CandleBook::new();
        let snap = |sec: i64, price: f64, high: f64, low: f64| {
            let mut t = tick(sec, price);
            t.high_24h = Some(high);
            t.low_24h = Some(low);
            t
        };
        b.add(&snap(50, 0.30215, 0.3135, 0.2961));
        b.add(&snap(59, 0.30245, 0.3135, 0.2961));
        b.add(&snap(61, 0.34771, 0.34771, 0.2961));
        b.add(&snap(70, 0.39, 0.435, 0.2961));
        b.add(&snap(119, 0.38888, 0.435, 0.2961));
        let done = b.add(&snap(121, 0.389, 0.435, 0.2961));
        let m1 = done.iter().find(|c| c.interval == Interval::M1).unwrap();
        assert_eq!(
            (m1.open, m1.high, m1.low, m1.close),
            (0.30245, 0.435, 0.30245, 0.38888)
        );
        // The 10 s candle opening at 1:10 starts where 1:00-1:10 closed.
        let s10 = done.iter().find(|c| c.interval == Interval::S10).unwrap();
        assert_eq!((s10.open, s10.close), (0.39, 0.38888));
    }

    #[test]
    fn first_24h_extreme_is_not_a_wick_and_trades_open_at_their_price() {
        let mut b = CandleBook::new();
        let mut t = tick(0, 1.0);
        // A 24 h high from hours ago must not become this candle's high.
        t.high_24h = Some(5.0);
        b.add(&t);
        b.add(&tick(9, 1.1));
        // A trade tick opens its candle at its own price.
        let mut trade = tick(10, 1.3);
        trade.volume = Some(1.0);
        let done = b.add(&trade);
        assert_eq!(done[0].high, 1.1);
        let c = b
            .open_candle("binance_futures", "XUSDT", Interval::S10)
            .unwrap();
        assert_eq!((c.open, c.low), (1.3, 1.3));
        // A falling 24 h low is a trade below the snapshots.
        let mut t = tick(12, 1.25);
        t.low_24h = Some(0.9);
        b.add(&t);
        t.low_24h = Some(0.8);
        b.add(&t);
        let c = b
            .open_candle("binance_futures", "XUSDT", Interval::S10)
            .unwrap();
        assert_eq!(c.low, 0.8);
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
    fn trade_volume_and_open_candle() {
        let mut b = CandleBook::new();
        let mut t = tick(0, 2.0);
        t.volume = Some(10.0);
        t.quote_volume_24h = Some(1e9);
        b.add(&t);
        b.add(&t);
        let c = b
            .open_candle("binance_futures", "XUSDT", Interval::S10)
            .unwrap();
        assert_eq!((c.close, c.volume), (2.0, 20.0));
        assert!(b
            .open_candle("gate_futures", "XUSDT", Interval::S10)
            .is_none());
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
