//! Trailing take profit, checked on every price tick.
//!
//! The Trigger worker sends the full list of open trailing positions every
//! second (`PUT /trailing/positions`); positions that leave the list are
//! dropped. When price falls the trailing distance from the best price
//! since the open, we ask the api to close the position
//! (`POST /api/internal/position-trailing/close`). The api claims the row
//! first, so a repeated request never closes twice; a failed request may be
//! repeated after 5 s. The best price goes back to the worker in each sync
//! answer so the database keeps it.
//!
//! Rule (same as `packages/common/src/utils/trailing-take-profit.ts`): no
//! TP order on the exchange; once price moved `activation%` in the trade's
//! favour, close when it falls `distance%` from the best price.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const DEFAULT_ACTIVATION_PERCENT: f64 = 40.0;
pub const DEFAULT_DISTANCE_PERCENT: f64 = 30.0;
/// The first price must sit within this ratio of the entry price, or the
/// symbol is probably a different instrument on that venue.
const FIRST_PRICE_MAX_RATIO: f64 = 2.0;
/// After a failed close request, ask again after this long.
pub const CLOSE_RETRY_AFTER: Duration = Duration::from_secs(5);
const EPS: f64 = 1e-9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    Long,
    Short,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Long => "LONG",
            Side::Short => "SHORT",
        }
    }
}

/// One position as the worker sends it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchRequest {
    pub id: String,
    pub user_id: String,
    pub symbol: String,
    /// pump-detector exchange name (binance_futures, gate_futures, ...).
    pub exchange: String,
    pub side: Side,
    pub entry_price: f64,
    #[serde(default)]
    pub activation_percent: Option<f64>,
    #[serde(default)]
    pub distance_percent: Option<f64>,
    #[serde(default)]
    pub peak_price: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Settings {
    pub activation_percent: f64,
    pub distance_percent: f64,
}

impl Settings {
    pub fn resolve(activation: Option<f64>, distance: Option<f64>) -> Self {
        Self {
            activation_percent: activation
                .filter(|a| a.is_finite() && *a >= 0.0)
                .unwrap_or(DEFAULT_ACTIVATION_PERCENT),
            distance_percent: distance
                .filter(|d| d.is_finite() && *d > 0.0 && *d < 100.0)
                .unwrap_or(DEFAULT_DISTANCE_PERCENT),
        }
    }
}

fn positive(x: f64) -> bool {
    x.is_finite() && x > 0.0
}

/// Price that closes the position; None until the trail is active.
pub fn stop_price(entry: f64, best: f64, side: Side, s: Settings) -> Option<f64> {
    if !positive(entry) || !positive(best) {
        return None;
    }
    let favourable = match side {
        Side::Long => (best / entry - 1.0) * 100.0,
        Side::Short => (1.0 - best / entry) * 100.0,
    };
    if favourable + EPS < s.activation_percent {
        return None;
    }
    Some(match side {
        Side::Long => best * (1.0 - s.distance_percent / 100.0),
        Side::Short => best * (1.0 + s.distance_percent / 100.0),
    })
}

/// What the api's close endpoint takes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseOrder {
    pub user_id: String,
    pub symbol: String,
    pub position_id: String,
    pub position_type: &'static str,
    pub entry_price: f64,
    pub peak_price: f64,
    pub stop_price: f64,
    pub last_price: f64,
    pub activation_percent: f64,
    pub distance_percent: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// No price yet for this market.
    WaitingForPrice,
    Watching,
    /// Close requested; waits for the position to leave the list.
    Closing,
    /// First price was far from entry (wrong instrument?); never closed here.
    Skipped,
}

/// One row of the sync answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub id: String,
    pub state: State,
    pub peak_price: f64,
    pub last_price: Option<f64>,
}

#[derive(Debug, Clone)]
struct Watched {
    req: WatchRequest,
    settings: Settings,
    best: f64,
    last: Option<f64>,
    priced: bool,
    state: State,
    /// Set when a close request failed: may close again after this.
    retry_at: Option<Instant>,
}

/// Market key: (pump exchange name, symbol).
pub type Market = (String, String);

/// Drop every position when the worker has not synced for this long (it is
/// down or moved away); while it is merely restarting we keep watching.
pub const SYNC_STALE_AFTER: Duration = Duration::from_secs(600);

#[derive(Default)]
pub struct Trailing {
    positions: HashMap<String, Watched>,
    by_market: HashMap<Market, HashSet<String>>,
    last_sync: Option<Instant>,
}

impl Trailing {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.positions.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    pub fn markets(&self) -> Vec<Market> {
        self.by_market.keys().cloned().collect()
    }

    /// Replace the watched set. `latest` gives the last known price of a
    /// market, used to price new positions at once. Returns closes to send.
    pub fn sync(
        &mut self,
        list: Vec<WatchRequest>,
        latest: impl Fn(&str, &str) -> Option<f64>,
    ) -> Vec<CloseOrder> {
        self.last_sync = Some(Instant::now());
        let keep: HashSet<String> = list.iter().map(|r| r.id.clone()).collect();
        self.positions.retain(|id, _| keep.contains(id));
        let mut new_ids = Vec::new();
        for mut req in list {
            req.symbol = req.symbol.to_uppercase();
            match self.positions.get_mut(&req.id) {
                Some(w) => {
                    // Settings are snapshotted on the position; a stored peak
                    // can only move the best further in the trade's favour.
                    if let Some(p) = req.peak_price.filter(|p| *p > 0.0) {
                        w.best = match w.req.side {
                            Side::Long => w.best.max(p),
                            Side::Short => w.best.min(p),
                        };
                    }
                }
                None => {
                    if !positive(req.entry_price) {
                        continue;
                    }
                    let settings = Settings::resolve(req.activation_percent, req.distance_percent);
                    let best = match req.peak_price.filter(|p| *p > 0.0) {
                        Some(b) => match req.side {
                            Side::Long => b.max(req.entry_price),
                            Side::Short => b.min(req.entry_price),
                        },
                        None => req.entry_price,
                    };
                    new_ids.push(req.id.clone());
                    self.positions.insert(
                        req.id.clone(),
                        Watched {
                            req,
                            settings,
                            best,
                            last: None,
                            priced: false,
                            state: State::WaitingForPrice,
                            retry_at: None,
                        },
                    );
                }
            }
        }
        self.by_market.clear();
        for (id, w) in &self.positions {
            self.by_market
                .entry((w.req.exchange.clone(), w.req.symbol.clone()))
                .or_default()
                .insert(id.clone());
        }

        let mut closes = Vec::new();
        let now = Instant::now();
        for id in new_ids {
            let (ex, sym) = {
                let w = &self.positions[&id];
                (w.req.exchange.clone(), w.req.symbol.clone())
            };
            if let Some(p) = latest(&ex, &sym) {
                if let Some(c) = self.step(&id, p, now) {
                    closes.push(c);
                }
            }
        }
        closes
    }

    /// Forget everything when the last sync is older than SYNC_STALE_AFTER.
    /// Returns how many positions were dropped.
    pub fn expire(&mut self, now: Instant) -> usize {
        let stale = self
            .last_sync
            .is_some_and(|t| now.duration_since(t) >= SYNC_STALE_AFTER);
        if !stale || self.positions.is_empty() {
            return 0;
        }
        let n = self.positions.len();
        self.positions.clear();
        self.by_market.clear();
        n
    }

    /// A tick for one market; returns closes to send.
    pub fn on_price(
        &mut self,
        exchange: &str,
        symbol: &str,
        price: f64,
        now: Instant,
    ) -> Vec<CloseOrder> {
        let Some(ids) = self
            .by_market
            .get(&(exchange.to_string(), symbol.to_string()))
        else {
            return Vec::new();
        };
        let ids: Vec<String> = ids.iter().cloned().collect();
        ids.iter()
            .filter_map(|id| self.step(id, price, now))
            .collect()
    }

    /// The close request for this position failed: allow another after a pause.
    pub fn close_failed(&mut self, id: &str, now: Instant) {
        if let Some(w) = self.positions.get_mut(id) {
            if w.state == State::Closing {
                w.state = State::Watching;
                w.retry_at = Some(now + CLOSE_RETRY_AFTER);
            }
        }
    }

    pub fn statuses(&self) -> Vec<Status> {
        let mut out: Vec<Status> = self
            .positions
            .iter()
            .map(|(id, w)| Status {
                id: id.clone(),
                state: w.state,
                peak_price: w.best,
                last_price: w.last,
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    fn step(&mut self, id: &str, price: f64, now: Instant) -> Option<CloseOrder> {
        let w = self.positions.get_mut(id)?;
        if !positive(price) || matches!(w.state, State::Closing | State::Skipped) {
            return None;
        }
        w.last = Some(price);
        if !w.priced {
            let ratio = price / w.req.entry_price;
            if !(1.0 / FIRST_PRICE_MAX_RATIO..=FIRST_PRICE_MAX_RATIO).contains(&ratio) {
                tracing::warn!(
                    "trailing {}: price {price} on {} is far from entry {}; wrong instrument? not watching it",
                    id,
                    w.req.exchange,
                    w.req.entry_price
                );
                w.state = State::Skipped;
                return None;
            }
            w.priced = true;
            w.state = State::Watching;
        }
        if let Some(stop) = stop_price(w.req.entry_price, w.best, w.req.side, w.settings) {
            let hit = match w.req.side {
                Side::Long => price <= stop,
                Side::Short => price >= stop,
            };
            if hit && w.retry_at.is_none_or(|t| now >= t) {
                w.state = State::Closing;
                w.retry_at = None;
                return Some(CloseOrder {
                    user_id: w.req.user_id.clone(),
                    symbol: w.req.symbol.clone(),
                    position_id: id.to_string(),
                    position_type: w.req.side.as_str(),
                    entry_price: w.req.entry_price,
                    peak_price: w.best,
                    stop_price: stop,
                    last_price: price,
                    activation_percent: w.settings.activation_percent,
                    distance_percent: w.settings.distance_percent,
                });
            }
            if hit {
                return None;
            }
        }
        let better = match w.req.side {
            Side::Long => price > w.best,
            Side::Short => price < w.best,
        };
        if better {
            w.best = price;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(
        id: &str,
        side: Side,
        activation: f64,
        distance: f64,
        peak: Option<f64>,
    ) -> WatchRequest {
        WatchRequest {
            id: id.into(),
            user_id: "u1".into(),
            symbol: "zestusdt".into(),
            exchange: "gate_futures".into(),
            side,
            entry_price: 100.0,
            activation_percent: Some(activation),
            distance_percent: Some(distance),
            peak_price: peak,
        }
    }

    fn none(_: &str, _: &str) -> Option<f64> {
        None
    }

    #[test]
    fn settings_defaults() {
        let d = Settings::resolve(None, None);
        assert_eq!((d.activation_percent, d.distance_percent), (40.0, 30.0));
        assert_eq!(Settings::resolve(Some(-1.0), Some(0.0)), d);
        assert_eq!(Settings::resolve(Some(f64::NAN), Some(100.0)), d);
        let s = Settings::resolve(Some(0.0), Some(10.0));
        assert_eq!((s.activation_percent, s.distance_percent), (0.0, 10.0));
    }

    #[test]
    fn long_closes_after_drop_from_high() {
        let mut t = Trailing::new();
        t.sync(vec![req("p1", Side::Long, 40.0, 30.0, None)], none);
        let now = Instant::now();
        assert!(t
            .on_price("gate_futures", "ZESTUSDT", 120.0, now)
            .is_empty());
        // Not active yet: a deep drop does nothing (the exchange SL handles it).
        assert!(t.on_price("gate_futures", "ZESTUSDT", 85.0, now).is_empty());
        assert!(t
            .on_price("gate_futures", "ZESTUSDT", 200.0, now)
            .is_empty());
        assert!(t
            .on_price("gate_futures", "ZESTUSDT", 141.0, now)
            .is_empty());
        let c = t.on_price("gate_futures", "ZESTUSDT", 139.9, now);
        assert_eq!(c.len(), 1);
        assert!((c[0].stop_price - 140.0).abs() < 1e-9);
        assert_eq!(c[0].peak_price, 200.0);
        assert_eq!(c[0].position_type, "LONG");
        // Closing: no second request on the next tick.
        assert!(t
            .on_price("gate_futures", "ZESTUSDT", 100.0, now)
            .is_empty());
        assert_eq!(t.statuses()[0].state, State::Closing);
    }

    #[test]
    fn short_mirrors_long() {
        let mut t = Trailing::new();
        t.sync(vec![req("p1", Side::Short, 10.0, 5.0, None)], none);
        let now = Instant::now();
        assert!(t.on_price("gate_futures", "ZESTUSDT", 80.0, now).is_empty());
        assert!(t.on_price("gate_futures", "ZESTUSDT", 83.9, now).is_empty());
        assert_eq!(t.on_price("gate_futures", "ZESTUSDT", 84.0, now).len(), 1);
    }

    #[test]
    fn stored_peak_resumes_and_never_worse_than_entry() {
        let mut t = Trailing::new();
        t.sync(vec![req("p1", Side::Long, 40.0, 30.0, Some(150.0))], none);
        let now = Instant::now();
        // Stop from the stored best is 105: a tick at 106 holds, 105 closes.
        assert!(t
            .on_price("gate_futures", "ZESTUSDT", 106.0, now)
            .is_empty());
        assert_eq!(t.on_price("gate_futures", "ZESTUSDT", 105.0, now).len(), 1);

        let mut t = Trailing::new();
        t.sync(vec![req("p1", Side::Long, 40.0, 30.0, Some(90.0))], none);
        assert_eq!(t.statuses()[0].peak_price, 100.0);
    }

    #[test]
    fn failed_close_retries_after_pause() {
        let mut t = Trailing::new();
        t.sync(vec![req("p1", Side::Long, 10.0, 5.0, Some(120.0))], none);
        let now = Instant::now();
        assert_eq!(t.on_price("gate_futures", "ZESTUSDT", 110.0, now).len(), 1);
        t.close_failed("p1", now);
        assert!(t
            .on_price(
                "gate_futures",
                "ZESTUSDT",
                110.0,
                now + Duration::from_secs(1)
            )
            .is_empty());
        assert_eq!(
            t.on_price("gate_futures", "ZESTUSDT", 110.0, now + CLOSE_RETRY_AFTER)
                .len(),
            1
        );
    }

    #[test]
    fn far_first_price_is_skipped_and_sync_drops_positions() {
        let mut t = Trailing::new();
        t.sync(vec![req("p1", Side::Long, 10.0, 5.0, None)], none);
        let now = Instant::now();
        assert!(t
            .on_price("gate_futures", "ZESTUSDT", 300.0, now)
            .is_empty());
        assert_eq!(t.statuses()[0].state, State::Skipped);
        assert!(t.on_price("gate_futures", "ZESTUSDT", 1.0, now).is_empty());
        t.sync(vec![], none);
        assert!(t.is_empty());
        assert!(t.markets().is_empty());
    }

    #[test]
    fn new_position_is_priced_from_latest_at_once() {
        let mut t = Trailing::new();
        let c = t.sync(
            vec![req("p1", Side::Long, 10.0, 5.0, Some(130.0))],
            |_, _| Some(120.0),
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].symbol, "ZESTUSDT");
    }

    #[test]
    fn peak_from_worker_only_moves_in_favour() {
        let mut t = Trailing::new();
        t.sync(vec![req("p1", Side::Long, 40.0, 30.0, None)], none);
        t.on_price("gate_futures", "ZESTUSDT", 120.0, Instant::now());
        t.sync(vec![req("p1", Side::Long, 40.0, 30.0, Some(110.0))], none);
        assert_eq!(t.statuses()[0].peak_price, 120.0);
        t.sync(vec![req("p1", Side::Long, 40.0, 30.0, Some(125.0))], none);
        assert_eq!(t.statuses()[0].peak_price, 125.0);
    }

    #[test]
    fn stale_list_is_dropped() {
        let mut t = Trailing::new();
        t.sync(vec![req("p1", Side::Long, 40.0, 30.0, None)], none);
        let now = Instant::now();
        assert_eq!(t.expire(now), 0);
        assert_eq!(t.expire(now + SYNC_STALE_AFTER), 1);
        assert!(t.markets().is_empty());
    }

    #[test]
    fn close_body_matches_the_api() {
        let o = CloseOrder {
            user_id: "u1".into(),
            symbol: "ZESTUSDT".into(),
            position_id: "p1".into(),
            position_type: "LONG",
            entry_price: 1.0,
            peak_price: 2.0,
            stop_price: 1.4,
            last_price: 1.39,
            activation_percent: 40.0,
            distance_percent: 30.0,
        };
        assert_eq!(
            serde_json::to_value(&o).unwrap(),
            serde_json::json!({
                "userId": "u1", "symbol": "ZESTUSDT", "positionId": "p1", "positionType": "LONG",
                "entryPrice": 1.0, "peakPrice": 2.0, "stopPrice": 1.4, "lastPrice": 1.39,
                "activationPercent": 40.0, "distancePercent": 30.0
            })
        );
    }
}
