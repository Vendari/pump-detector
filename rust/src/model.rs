//! Prices, candle intervals and candles.

use chrono::{DateTime, Utc};

/// One price from an exchange feed.
///
/// `price` is the market's main price: the one /ws/price, trailing take
/// profit, candles and alerts have always used (Gate: mark, falling back to
/// last; every other venue: last trade). `last` and `mark` are kept apart
/// for /v1 clients. A mark-only update (`primary == false`) carries just a
/// mark price and leaves the main price, candles and trailing alone.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceUpdate {
    pub symbol: String,
    pub exchange: &'static str,
    pub price: f64,
    /// Exchange event time when the feed has one, else receive time.
    pub timestamp: DateTime<Utc>,
    /// Rolling 24 h quote volume (USDT); candles use the deltas between ticks.
    pub quote_volume_24h: Option<f64>,
    /// Quote volume of this tick itself (a trade), added to the open candles.
    /// None means the tick is a once-a-second ticker snapshot, not a trade.
    pub volume: Option<f64>,
    /// Rolling 24 h high and low of the last price, from ticker feeds. A
    /// rise (fall) since the previous tick means a trade printed there, so
    /// candles take it as a wick the snapshots missed.
    pub high_24h: Option<f64>,
    pub low_24h: Option<f64>,
    pub last: Option<f64>,
    pub mark: Option<f64>,
    pub primary: bool,
    /// Funding fields the frame carried (futures tickers and mark streams).
    pub funding: Option<Funding>,
}

/// A perpetual's funding: the current (predicted) rate per funding period
/// as a fraction (0.0001 = 0.01%), when the next payment is, and how often
/// it is paid. Feeds fill what their frames carry; contract lists fill the
/// schedule some streams lack.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Funding {
    pub rate: Option<f64>,
    /// Unix ms of the next funding payment.
    pub next_ms: Option<i64>,
    pub interval_hours: Option<f64>,
}

impl Funding {
    pub fn is_empty(&self) -> bool {
        self.rate.is_none() && self.next_ms.is_none() && self.interval_hours.is_none()
    }

    /// Fields `newer` has replace ours; the rest are kept (Bybit deltas
    /// carry only what changed).
    pub fn merge(&mut self, newer: &Funding) {
        if newer.rate.is_some() {
            self.rate = newer.rate;
        }
        if newer.next_ms.is_some() {
            self.next_ms = newer.next_ms;
        }
        if newer.interval_hours.is_some() {
            self.interval_hours = newer.interval_hours;
        }
    }

    /// Ours, with gaps filled from `other` (a contract list).
    pub fn or(self, other: Option<Funding>) -> Funding {
        let Some(o) = other else { return self };
        Funding {
            rate: self.rate.or(o.rate),
            next_ms: self.next_ms.or(o.next_ms),
            interval_hours: self.interval_hours.or(o.interval_hours),
        }
    }

    /// The next payment at or after `now_ms`: a time already passed moves on
    /// by whole intervals; with no time but an interval, the next multiple
    /// of the interval since 00:00 UTC (how the venues schedule it).
    pub fn next_after(&self, now_ms: i64) -> Option<i64> {
        let step = self
            .interval_hours
            .filter(|h| h.is_finite() && *h > 0.0)
            .map(|h| (h * 3_600_000.0).round() as i64);
        match (self.next_ms, step) {
            (Some(t), _) if t > now_ms => Some(t),
            (Some(t), Some(step)) => Some(t + ((now_ms - t) / step + 1) * step),
            (None, Some(step)) => Some((now_ms / step + 1) * step),
            _ => None,
        }
    }
}

/// A funding rate field: a number or numeric string, any sign.
pub fn rate(v: Option<&serde_json::Value>) -> Option<f64> {
    let r = match v? {
        serde_json::Value::String(s) if !s.is_empty() => s.parse::<f64>().ok()?,
        serde_json::Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    r.is_finite().then_some(r)
}

/// A timestamp field (number or numeric string), unix ms; seconds are
/// scaled up.
pub fn unix_ms(v: Option<&serde_json::Value>) -> Option<i64> {
    let t = match v? {
        serde_json::Value::String(s) => s.parse::<i64>().ok()?,
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64))?,
        _ => return None,
    };
    match t {
        t if t <= 0 => None,
        t if t < 100_000_000_000 => Some(t * 1000),
        t => Some(t),
    }
}

impl PriceUpdate {
    /// A main-price update whose price is the last trade.
    pub fn last_trade(
        symbol: String,
        exchange: &'static str,
        price: f64,
        timestamp: DateTime<Utc>,
    ) -> Self {
        Self {
            symbol,
            exchange,
            price,
            timestamp,
            quote_volume_24h: None,
            volume: None,
            high_24h: None,
            low_24h: None,
            last: Some(price),
            mark: None,
            primary: true,
            funding: None,
        }
    }

    /// A mark price only.
    pub fn mark_only(
        symbol: String,
        exchange: &'static str,
        mark: f64,
        timestamp: DateTime<Utc>,
    ) -> Self {
        Self {
            symbol,
            exchange,
            price: mark,
            timestamp,
            quote_volume_24h: None,
            volume: None,
            high_24h: None,
            low_24h: None,
            last: None,
            mark: Some(mark),
            primary: false,
            funding: None,
        }
    }

    /// Funding fields only: they update the quote, nothing else.
    pub fn funding_only(
        symbol: String,
        exchange: &'static str,
        funding: Funding,
        timestamp: DateTime<Utc>,
    ) -> Self {
        Self {
            symbol,
            exchange,
            price: 0.0,
            timestamp,
            quote_volume_24h: None,
            volume: None,
            high_24h: None,
            low_24h: None,
            last: None,
            mark: None,
            primary: false,
            funding: Some(funding),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Interval {
    S10,
    S30,
    M1,
    M2,
    M3,
    M4,
    M5,
    M10,
    M15,
    M30,
    M45,
    H1,
    H2,
    H3,
    H4,
    H6,
    H12,
    D1,
    W1,
}

impl Interval {
    /// Every interval the API accepts.
    pub const ALL: [Interval; 19] = [
        Interval::S10,
        Interval::S30,
        Interval::M1,
        Interval::M2,
        Interval::M3,
        Interval::M4,
        Interval::M5,
        Interval::M10,
        Interval::M15,
        Interval::M30,
        Interval::M45,
        Interval::H1,
        Interval::H2,
        Interval::H3,
        Interval::H4,
        Interval::H6,
        Interval::H12,
        Interval::D1,
        Interval::W1,
    ];

    /// Intervals we build and store (the Python service's aggregation chain;
    /// 45m, 2h and 4h were never stored).
    pub const BUILT: [Interval; 16] = [
        Interval::S10,
        Interval::S30,
        Interval::M1,
        Interval::M2,
        Interval::M3,
        Interval::M4,
        Interval::M5,
        Interval::M10,
        Interval::M15,
        Interval::M30,
        Interval::H1,
        Interval::H3,
        Interval::H6,
        Interval::H12,
        Interval::D1,
        Interval::W1,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Interval::S10 => "10s",
            Interval::S30 => "30s",
            Interval::M1 => "1m",
            Interval::M2 => "2m",
            Interval::M3 => "3m",
            Interval::M4 => "4m",
            Interval::M5 => "5m",
            Interval::M10 => "10m",
            Interval::M15 => "15m",
            Interval::M30 => "30m",
            Interval::M45 => "45m",
            Interval::H1 => "1h",
            Interval::H2 => "2h",
            Interval::H3 => "3h",
            Interval::H4 => "4h",
            Interval::H6 => "6h",
            Interval::H12 => "12h",
            Interval::D1 => "1D",
            Interval::W1 => "1W",
        }
    }

    pub fn seconds(self) -> i64 {
        match self {
            Interval::S10 => 10,
            Interval::S30 => 30,
            Interval::M1 => 60,
            Interval::M2 => 120,
            Interval::M3 => 180,
            Interval::M4 => 240,
            Interval::M5 => 300,
            Interval::M10 => 600,
            Interval::M15 => 900,
            Interval::M30 => 1800,
            Interval::M45 => 2700,
            Interval::H1 => 3600,
            Interval::H2 => 7200,
            Interval::H3 => 10800,
            Interval::H4 => 14400,
            Interval::H6 => 21600,
            Interval::H12 => 43200,
            Interval::D1 => 86400,
            Interval::W1 => 604800,
        }
    }

    /// "24h" is accepted as 1D.
    pub fn parse(s: &str) -> Option<Interval> {
        if s == "24h" {
            return Some(Interval::D1);
        }
        Interval::ALL.into_iter().find(|i| i.as_str() == s)
    }

    /// Start of the bucket holding `ts`, counted from the Unix epoch.
    pub fn bucket_start(self, ts: DateTime<Utc>) -> i64 {
        let secs = ts.timestamp();
        secs.div_euclid(self.seconds()) * self.seconds()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candle {
    pub symbol: String,
    pub exchange: &'static str,
    pub interval: Interval,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// Bucket start, Unix seconds.
    pub start: i64,
    pub volume: f64,
}

impl Candle {
    pub fn change_percent(&self) -> f64 {
        if self.open > 0.0 {
            (self.close - self.open) / self.open * 100.0
        } else {
            0.0
        }
    }
}

/// Naive ISO time like Python's `datetime.isoformat()` on a naive UTC value.
pub fn iso_naive(ts: DateTime<Utc>) -> String {
    let s = ts.naive_utc().format("%Y-%m-%dT%H:%M:%S%.6f").to_string();
    // Python omits the fraction when it is zero.
    s.strip_suffix(".000000").map(str::to_string).unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn buckets_align_to_epoch() {
        let t = Utc.with_ymd_and_hms(2026, 10, 13, 12, 34, 56).unwrap();
        assert_eq!(
            Interval::M5.bucket_start(t),
            Utc.with_ymd_and_hms(2026, 10, 13, 12, 30, 0)
                .unwrap()
                .timestamp()
        );
        assert_eq!(Interval::S10.bucket_start(t), t.timestamp() - 6);
        assert_eq!(Interval::parse("24h"), Some(Interval::D1));
        assert_eq!(Interval::parse("45m"), Some(Interval::M45));
        assert_eq!(Interval::parse("7m"), None);
    }

    #[test]
    fn iso_like_python() {
        let t = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        assert_eq!(iso_naive(t), "2026-10-13T12:00:00");
        let t = t + chrono::Duration::milliseconds(250);
        assert_eq!(iso_naive(t), "2026-10-13T12:00:00.250000");
    }

    #[test]
    fn funding_schedule() {
        let h = 3_600_000;
        let f = Funding {
            rate: Some(0.0001),
            next_ms: Some(10 * h),
            interval_hours: Some(4.0),
        };
        assert_eq!(f.next_after(9 * h), Some(10 * h));
        // Passed: moves on by whole intervals.
        assert_eq!(f.next_after(10 * h), Some(14 * h));
        assert_eq!(f.next_after(15 * h), Some(18 * h));
        // No time, hourly: the top of the next hour.
        let hl = Funding {
            interval_hours: Some(1.0),
            ..Funding::default()
        };
        assert_eq!(hl.next_after(5 * h + 1), Some(6 * h));
        // Passed with no interval: unknown.
        let b = Funding {
            next_ms: Some(h),
            ..Funding::default()
        };
        assert_eq!(b.next_after(2 * h), None);

        let mut m = f;
        m.merge(&Funding {
            rate: Some(-0.0002),
            ..Funding::default()
        });
        assert_eq!((m.rate, m.next_ms), (Some(-0.0002), Some(10 * h)));
        let filled = Funding {
            rate: Some(0.001),
            ..Funding::default()
        }
        .or(Some(f));
        assert_eq!(
            (filled.rate, filled.interval_hours),
            (Some(0.001), Some(4.0))
        );

        assert_eq!(rate(Some(&serde_json::json!("-0.00012"))), Some(-0.00012));
        assert_eq!(rate(Some(&serde_json::json!(0))), Some(0.0));
        assert_eq!(rate(Some(&serde_json::json!(""))), None);
        assert_eq!(
            unix_ms(Some(&serde_json::json!("1791820800000"))),
            Some(1791820800000)
        );
        assert_eq!(
            unix_ms(Some(&serde_json::json!(1791820800))),
            Some(1791820800000)
        );
    }
}
