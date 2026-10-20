//! `GET /v1/marks` (WebSocket, server API key only): one reference mark price
//! per token for the Trigger order services, which size orders from it instead
//! of asking the exchange first.
//!
//! On connect the client gets every token's price (`{"type":"snap",...}`);
//! after that a token is pushed only when its price moved at least
//! `MARKS_PUSH_THRESHOLD_PCT` (default 2 %) from the last value sent on that
//! connection, so a quiet token costs nothing and a pumping one is pushed at
//! every step. A heartbeat every 5 s tells the client the feed is alive, so
//! silence means "within the threshold of what you have", never "dead".
//!
//! One price per token: token prices barely differ between exchanges, so the
//! reference is the highest price among venues whose feed ticked in the last
//! 30 s and whose mark (or last trade, when the venue has no mark) is at most
//! 5 minutes old. Tokens whose venues are more than 25 % apart are sent per
//! exchange (`<exchange>:<token>`) instead; see `reference_prices`. Clients
//! look up `<exchange>:<token>` first, then `<token>`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use serde_json::json;

use crate::app::App;
use crate::hub::Quote;

const TICK: Duration = Duration::from_millis(250);
const HEARTBEAT: Duration = Duration::from_secs(5);
/// A venue whose feed has not ticked for this long is skipped.
const FEED_ALIVE_SECS: i64 = 30;
/// A quote older than this is not used as a reference.
const QUOTE_MAX_AGE_SECS: i64 = 300;
/// Farther than this from the other venues is a bad print or another project.
const OUTLIER: f64 = 0.25;

/// Reference prices keyed by token (`ACUUSDT`): the highest fresh price
/// among the venues that are alive. Max, not a venue priority: when one venue
/// pumps first (ACU on Binance, +12 % in ~100 ms) a reference below it would
/// oversize the order there; with the max the error only undersizes laggards.
///
/// When the venues are more than 25 % apart (one ticker naming two projects,
/// a frozen mark on a delisted contract, a garbage print, or one venue far
/// ahead on a listing) there is no token price; each venue's own price is
/// sent instead under `<exchange>:<token>` (`bybit_futures:PUFFERUSDT`), so
/// every order is sized from the exchange it goes to.
pub fn reference_prices(
    quotes: &[((String, String), Quote)],
    last_tick: &HashMap<String, DateTime<Utc>>,
    now: DateTime<Utc>,
) -> HashMap<String, f64> {
    let alive = |ex: &str| {
        last_tick
            .get(ex)
            .is_some_and(|t| (now - *t).num_seconds() <= FEED_ALIVE_SECS)
    };
    let mut per_token: HashMap<&str, Vec<(&str, f64)>> = HashMap::new();
    for ((ex, sym), q) in quotes {
        if !crate::feeds::FUTURES.contains(&ex.as_str()) || !alive(ex) {
            continue;
        }
        let Some((price, at)) = q.mark.or(q.last) else {
            continue;
        };
        if !price.is_finite() || price <= 0.0 || (now - at).num_seconds() > QUOTE_MAX_AGE_SECS {
            continue;
        }
        per_token
            .entry(sym.as_str())
            .or_default()
            .push((ex.as_str(), price));
    }
    let mut out = HashMap::new();
    for (sym, v) in per_token {
        let max = v.iter().map(|(_, p)| *p).fold(f64::MIN, f64::max);
        let min = v.iter().map(|(_, p)| *p).fold(f64::MAX, f64::min);
        if min < max * (1.0 - OUTLIER) {
            for (ex, p) in v {
                out.insert(format!("{ex}:{sym}"), p);
            }
        } else {
            out.insert(sym.to_string(), max);
        }
    }
    out
}

/// What one connection was last sent, and which tokens moved enough to send.
pub struct Pusher {
    threshold: f64,
    last_sent: HashMap<String, f64>,
}

impl Pusher {
    pub fn new(threshold_pct: f64) -> Self {
        Self {
            threshold: threshold_pct / 100.0,
            last_sent: HashMap::new(),
        }
    }

    /// Every token, remembered as sent (the snapshot).
    pub fn snapshot(&mut self, refs: &HashMap<String, f64>) -> Vec<(String, f64)> {
        let mut out: Vec<(String, f64)> = refs.iter().map(|(s, p)| (s.clone(), *p)).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        self.last_sent = refs.clone();
        out
    }

    /// Tokens that are new or moved at least the threshold (either way) since
    /// the value last sent; those become the new last-sent values.
    pub fn moved(&mut self, refs: &HashMap<String, f64>) -> Vec<(String, Option<f64>)> {
        let mut out: Vec<(String, Option<f64>)> = Vec::new();
        // A token with no reference any more is sent as null and forgotten.
        let gone: Vec<String> = self
            .last_sent
            .keys()
            .filter(|s| !refs.contains_key(*s))
            .cloned()
            .collect();
        for sym in gone {
            self.last_sent.remove(&sym);
            out.push((sym, None));
        }
        for (sym, p) in refs {
            let send = match self.last_sent.get(sym) {
                None => true,
                Some(prev) => (p / prev - 1.0).abs() >= self.threshold,
            };
            if send {
                self.last_sent.insert(sym.clone(), *p);
                out.push((sym.clone(), Some(*p)));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

fn current(app: &App) -> HashMap<String, f64> {
    let hub = app.hub.lock();
    let quotes: Vec<_> = hub
        .all_quotes()
        .into_iter()
        .map(|(m, _, q)| (m, q))
        .collect();
    let ticks: HashMap<String, DateTime<Utc>> = crate::feeds::FUTURES
        .iter()
        .filter_map(|ex| hub.last_tick(ex).map(|t| (ex.to_string(), t)))
        .collect();
    drop(hub);
    reference_prices(&quotes, &ticks, Utc::now())
}

fn rows<P: serde::Serialize>(list: &[(String, P)]) -> serde_json::Value {
    json!(list.iter().map(|(s, p)| json!([s, p])).collect::<Vec<_>>())
}

pub async fn handler(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    match crate::v1::caller(&app, &headers, None) {
        Ok(crate::v1::Caller::Server) => {}
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(json!({"detail": "server api key required"})),
            )
                .into_response()
        }
    }
    ws.on_upgrade(move |socket| client(app, socket))
}

async fn client(app: Arc<App>, socket: WebSocket) {
    use std::sync::atomic::Ordering::Relaxed;
    app.stats.marks_clients.fetch_add(1, Relaxed);
    let (mut sink, mut stream) = socket.split();
    let mut pusher = Pusher::new(app.cfg.marks_threshold_pct);
    let snap = pusher.snapshot(&current(&app));
    let first = json!({"type": "snap", "ts": Utc::now().timestamp_millis(),
                       "thresholdPct": app.cfg.marks_threshold_pct, "d": rows(&snap)});
    if sink.send(Message::text(first.to_string())).await.is_ok() {
        let mut tick = tokio::time::interval(TICK);
        let mut beat = tokio::time::interval(HEARTBEAT);
        loop {
            let msg = tokio::select! {
                frame = stream.next() => match frame {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => continue,
                },
                _ = tick.tick() => {
                    let moved = pusher.moved(&current(&app));
                    if moved.is_empty() {
                        continue;
                    }
                    app.stats.marks_pushed.fetch_add(moved.len() as u64, Relaxed);
                    json!({"type": "m", "ts": Utc::now().timestamp_millis(), "d": rows(&moved)})
                }
                _ = beat.tick() => json!({"type": "hb", "ts": Utc::now().timestamp_millis()}),
            };
            if sink.send(Message::text(msg.to_string())).await.is_err() {
                break;
            }
        }
    }
    app.stats.marks_clients.fetch_sub(1, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(mark: Option<f64>, last: Option<f64>, at: DateTime<Utc>) -> Quote {
        Quote {
            last: last.map(|p| (p, at)),
            mark: mark.map(|p| (p, at)),
            ..Quote::default()
        }
    }

    #[test]
    fn reference_is_the_highest_live_fresh_price_or_per_exchange_when_venues_disagree() {
        let now = Utc::now();
        let ticks: HashMap<String, DateTime<Utc>> = [
            (
                "binance_futures".to_string(),
                now - chrono::Duration::seconds(60),
            ),
            ("gate_futures".to_string(), now),
            ("bybit_futures".to_string(), now),
            ("kucoin_futures".to_string(), now),
        ]
        .into();
        let quotes = vec![
            // Binance feed is dead (no tick for 60 s): skipped even though highest.
            (
                ("binance_futures".into(), "ACUUSDT".into()),
                q(Some(0.20), None, now),
            ),
            (
                ("bybit_futures".into(), "ACUUSDT".into()),
                q(Some(0.11), None, now),
            ),
            (
                ("gate_futures".into(), "ACUUSDT".into()),
                q(Some(0.10), None, now),
            ),
            // Venues far apart: each venue's own price instead.
            (
                ("gate_futures".into(), "BADUSDT".into()),
                q(Some(1.0), None, now),
            ),
            (
                ("bybit_futures".into(), "BADUSDT".into()),
                q(Some(1.02), None, now),
            ),
            (
                ("kucoin_futures".into(), "BADUSDT".into()),
                q(Some(5.0), None, now),
            ),
            // Two venues 5x apart: per exchange too.
            (
                ("gate_futures".into(), "TWOUSDT".into()),
                q(Some(1.0), None, now),
            ),
            (
                ("kucoin_futures".into(), "TWOUSDT".into()),
                q(Some(5.0), None, now),
            ),
            // Only a last trade, no mark: still a price.
            (
                ("bybit_futures".into(), "XUSDT".into()),
                q(None, Some(2.0), now),
            ),
            // Stale quote: dropped.
            (
                ("gate_futures".into(), "OLDUSDT".into()),
                q(Some(1.0), None, now - chrono::Duration::seconds(600)),
            ),
            // Spot is never a reference.
            (
                ("binance_spot".into(), "SPOTUSDT".into()),
                q(None, Some(1.0), now),
            ),
        ];
        let r = reference_prices(&quotes, &ticks, now);
        assert_eq!(r.get("ACUUSDT"), Some(&0.11));
        assert!(!r.contains_key("BADUSDT"));
        assert_eq!(r.get("gate_futures:BADUSDT"), Some(&1.0));
        assert_eq!(r.get("bybit_futures:BADUSDT"), Some(&1.02));
        assert_eq!(r.get("kucoin_futures:BADUSDT"), Some(&5.0));
        assert_eq!(r.get("XUSDT"), Some(&2.0));
        assert!(!r.contains_key("TWOUSDT"));
        assert_eq!(r.get("kucoin_futures:TWOUSDT"), Some(&5.0));
        // Agreeing tokens get no per-exchange rows.
        assert!(!r.contains_key("bybit_futures:ACUUSDT"));
        assert!(!r.contains_key("OLDUSDT"));
        assert!(!r.contains_key("SPOTUSDT"));
    }

    #[test]
    fn pushes_only_moves_past_the_threshold_from_the_last_sent_value() {
        let mut p = Pusher::new(2.0);
        let mut refs: HashMap<String, f64> = [("ACUUSDT".to_string(), 100.0)].into();
        assert_eq!(p.snapshot(&refs), vec![("ACUUSDT".to_string(), 100.0)]);
        refs.insert("ACUUSDT".into(), 101.9);
        assert!(p.moved(&refs).is_empty());
        // Small steps add up against the last value sent, not the last tick.
        refs.insert("ACUUSDT".into(), 102.0);
        assert_eq!(p.moved(&refs), vec![("ACUUSDT".to_string(), Some(102.0))]);
        refs.insert("ACUUSDT".into(), 99.9);
        assert_eq!(p.moved(&refs), vec![("ACUUSDT".to_string(), Some(99.9))]);
        // A token that appears later is sent on first sight.
        refs.insert("NEWUSDT".into(), 1.0);
        assert_eq!(p.moved(&refs), vec![("NEWUSDT".to_string(), Some(1.0))]);
        // A token that loses its reference is sent as null.
        refs.remove("NEWUSDT");
        assert_eq!(p.moved(&refs), vec![("NEWUSDT".to_string(), None)]);
    }
}
