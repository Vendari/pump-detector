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
    pub volume: Option<f64>,
    pub last: Option<f64>,
    pub mark: Option<f64>,
    pub primary: bool,
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
            last: Some(price),
            mark: None,
            primary: true,
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
            last: None,
            mark: Some(mark),
            primary: false,
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
}
