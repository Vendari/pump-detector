//! Exchange price feeds. Binance and Gate stream every market (they build
//! candles and alerts); Bybit, KuCoin, Aster and Hyperliquid connect only for
//! the symbols someone watches (a /ws/price client or a trailing position).

pub mod binance;
pub mod gate;
pub mod on_demand;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::model::PriceUpdate;

pub type Sink = mpsc::Sender<Vec<PriceUpdate>>;

/// Reconnect pacing: 5 s between attempts; after 10 failed attempts in a
/// row the next one waits a day, so a feed that keeps failing (an IP ban,
/// a changed endpoint) never hammers the exchange into a rate limit. A
/// connection that delivered any price resets the count.
pub struct Reconnect {
    name: &'static str,
    failures: u32,
}

pub const RETRY: Duration = Duration::from_secs(5);
const MAX_FAILURES: u32 = 10;
const LONG_PAUSE: Duration = Duration::from_secs(24 * 3600);

impl Reconnect {
    pub fn new(name: &'static str) -> Self {
        Self { name, failures: 0 }
    }

    pub fn success(&mut self) {
        if self.failures > 0 {
            tracing::info!(
                "{}: connected again after {} failure(s)",
                self.name,
                self.failures
            );
        }
        self.failures = 0;
    }

    /// Delay before the next attempt.
    pub fn after(&mut self, got_data: bool) -> Duration {
        if got_data {
            self.success();
            return RETRY;
        }
        self.failures += 1;
        if self.failures >= MAX_FAILURES {
            tracing::error!(
                "{}: {} failed attempts in a row; next attempt in 24 h",
                self.name,
                self.failures
            );
            LONG_PAUSE
        } else {
            RETRY
        }
    }
}

/// Whether a feed is connected (for /health).
#[derive(Clone, Default)]
pub struct Alive(Arc<AtomicBool>);

impl Alive {
    pub fn set(&self, v: bool) {
        self.0.store(v, Ordering::Relaxed);
    }
    pub fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

pub fn num(v: Option<&serde_json::Value>) -> Option<f64> {
    let v = v?;
    let p = match v {
        serde_json::Value::String(s) => s.parse::<f64>().ok()?,
        serde_json::Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    (p.is_finite() && p > 0.0).then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_pauses_after_ten_failures() {
        let mut r = Reconnect::new("x");
        for _ in 0..9 {
            assert_eq!(r.after(false), RETRY);
        }
        assert_eq!(r.after(false), LONG_PAUSE);
        assert_eq!(r.after(true), RETRY);
        assert_eq!(r.after(false), RETRY);
    }
}
