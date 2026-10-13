//! Pump/dump detection on completed candles.
//!
//! Built-in tiers on 5m, 15m, 30m, 1h, 6h, 12h and 1D: a move of 5% is
//! stored only; 10% is stored and sent to WEBHOOK_URL. Custom configs
//! (POST /alerts) use their own threshold and webhook on any interval.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{Candle, Interval};

pub const BUILTIN: [Interval; 7] = [
    Interval::M5,
    Interval::M15,
    Interval::M30,
    Interval::H1,
    Interval::H6,
    Interval::H12,
    Interval::D1,
];

#[derive(Debug, Clone, PartialEq)]
pub struct PriceAlert {
    pub symbol: String,
    pub exchange: &'static str,
    /// "spike" or "drop".
    pub alert_type: &'static str,
    pub start_price: f64,
    pub current_price: f64,
    pub change_percent: f64,
    pub interval: Interval,
    pub timestamp: DateTime<Utc>,
    pub webhook_url: Option<String>,
    pub alert_id: Option<String>,
}

/// A custom alert registered through POST /alerts (Mongo `spike_alerts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpikeAlertConfig {
    pub webhook_url: String,
    pub exchange: String,
    pub interval: String,
    pub threshold_percent: f64,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub alert_id: Option<String>,
}

fn alert(
    c: &Candle,
    kind: &'static str,
    now: DateTime<Utc>,
    webhook: Option<String>,
    id: Option<String>,
) -> PriceAlert {
    PriceAlert {
        symbol: c.symbol.clone(),
        exchange: c.exchange,
        alert_type: kind,
        start_price: c.open,
        current_price: c.close,
        change_percent: c.change_percent(),
        interval: c.interval,
        timestamp: now,
        webhook_url: webhook,
        alert_id: id,
    }
}

/// Alerts for one completed candle.
pub fn check(
    c: &Candle,
    webhook_url: Option<&str>,
    configs: &[SpikeAlertConfig],
    now: DateTime<Utc>,
) -> Vec<PriceAlert> {
    let pct = c.change_percent();
    let mut out = Vec::new();
    if BUILTIN.contains(&c.interval) {
        let hook = webhook_url.map(str::to_string);
        if pct >= 10.0 {
            out.push(alert(
                c,
                "spike",
                now,
                hook.clone(),
                Some("builtin_10".into()),
            ));
        } else if pct >= 5.0 {
            out.push(alert(c, "spike", now, None, Some("builtin_5".into())));
        }
        if pct <= -10.0 {
            out.push(alert(c, "drop", now, hook, Some("builtin_10".into())));
        } else if pct <= -5.0 {
            out.push(alert(c, "drop", now, None, Some("builtin_5".into())));
        }
    }
    for cfg in configs {
        if cfg.exchange != c.exchange || Interval::parse(&cfg.interval) != Some(c.interval) {
            continue;
        }
        if cfg.symbol.as_deref().is_some_and(|s| s != c.symbol) {
            continue;
        }
        let t = cfg.threshold_percent;
        let kind = if t > 0.0 && pct >= t {
            "spike"
        } else if t < 0.0 && pct <= t {
            "drop"
        } else {
            continue;
        };
        if !cfg.webhook_url.is_empty() {
            out.push(alert(
                c,
                kind,
                now,
                Some(cfg.webhook_url.clone()),
                cfg.alert_id.clone(),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candle(interval: Interval, open: f64, close: f64) -> Candle {
        Candle {
            symbol: "XUSDT".into(),
            exchange: "binance_futures",
            interval,
            open,
            high: open.max(close),
            low: open.min(close),
            close,
            start: 0,
            volume: 0.0,
        }
    }

    #[test]
    fn tiers() {
        let now = Utc::now();
        let a = check(
            &candle(Interval::M5, 1.0, 1.12),
            Some("http://hook"),
            &[],
            now,
        );
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].alert_type, "spike");
        assert_eq!(a[0].webhook_url.as_deref(), Some("http://hook"));
        let a = check(
            &candle(Interval::M5, 1.0, 0.94),
            Some("http://hook"),
            &[],
            now,
        );
        assert_eq!((a[0].alert_type, a[0].webhook_url.clone()), ("drop", None));
        assert!(check(&candle(Interval::M5, 1.0, 1.04), Some("h"), &[], now).is_empty());
        // Not a built-in interval.
        assert!(check(&candle(Interval::M1, 1.0, 2.0), Some("h"), &[], now).is_empty());
    }

    #[test]
    fn custom_configs() {
        let now = Utc::now();
        let cfg = SpikeAlertConfig {
            webhook_url: "http://bot".into(),
            exchange: "binance_futures".into(),
            interval: "1m".into(),
            threshold_percent: -3.0,
            symbol: None,
            alert_id: Some("a1".into()),
        };
        let a = check(
            &candle(Interval::M1, 1.0, 0.96),
            None,
            std::slice::from_ref(&cfg),
            now,
        );
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].alert_id.as_deref(), Some("a1"));
        let mut other = cfg;
        other.symbol = Some("YUSDT".into());
        assert!(check(&candle(Interval::M1, 1.0, 0.96), None, &[other], now).is_empty());
    }
}
