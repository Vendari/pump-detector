//! Latest price per market, /ws/price subscribers, and which symbols the
//! on-demand feeds must stream (watched by a client or a trailing position).

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use serde_json::json;
use tokio::sync::{mpsc, watch};

use crate::feeds::on_demand::{usdt, Venue};
use crate::model::{iso_naive, PriceUpdate};

pub type Market = (String, String); // (exchange, symbol)

#[derive(Debug, Clone, Copy)]
pub struct Latest {
    pub price: f64,
    pub timestamp: DateTime<Utc>,
}

pub fn price_json(symbol: &str, exchange: &str, l: &Latest) -> String {
    json!({"symbol": symbol, "exchange": exchange, "price": l.price, "timestamp": iso_naive(l.timestamp)}).to_string()
}

pub struct Hub {
    latest: HashMap<Market, Latest>,
    subscribers: HashMap<Market, HashMap<u64, mpsc::Sender<String>>>,
    client_watch: HashMap<Market, usize>,
    trailing_watch: BTreeSet<Market>,
    venues: HashMap<&'static str, watch::Sender<BTreeSet<String>>>,
    next_client: u64,
}

impl Hub {
    pub fn new() -> (Self, HashMap<Venue, watch::Receiver<BTreeSet<String>>>) {
        let mut venues = HashMap::new();
        let mut receivers = HashMap::new();
        for v in Venue::ALL {
            let (tx, rx) = watch::channel(BTreeSet::new());
            venues.insert(v.name(), tx);
            receivers.insert(v, rx);
        }
        (
            Self {
                latest: HashMap::new(),
                subscribers: HashMap::new(),
                client_watch: HashMap::new(),
                trailing_watch: BTreeSet::new(),
                venues,
                next_client: 0,
            },
            receivers,
        )
    }

    pub fn new_client(&mut self) -> u64 {
        self.next_client += 1;
        self.next_client
    }

    /// Record a price and fan it out to subscribers (slow clients drop prices).
    pub fn publish(&mut self, u: &PriceUpdate) {
        let key = (u.exchange.to_string(), u.symbol.clone());
        let l = Latest {
            price: u.price,
            timestamp: u.timestamp,
        };
        if let Some(subs) = self.subscribers.get(&key) {
            let msg = price_json(&u.symbol, u.exchange, &l);
            for tx in subs.values() {
                let _ = tx.try_send(msg.clone());
            }
        }
        self.latest.insert(key, l);
    }

    pub fn latest(&self, exchange: &str, symbol: &str) -> Option<Latest> {
        self.latest
            .get(&(exchange.to_string(), symbol.to_string()))
            .copied()
    }

    pub fn all_latest(&self) -> Vec<(Market, Latest)> {
        self.latest.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    pub fn subscribe(&mut self, client: u64, market: Market, tx: mpsc::Sender<String>) {
        let subs = self.subscribers.entry(market.clone()).or_default();
        if subs.insert(client, tx).is_none() {
            *self.client_watch.entry(market.clone()).or_default() += 1;
            self.publish_venue(&market.0);
        }
    }

    pub fn unsubscribe(&mut self, client: u64, market: &Market) {
        let Some(subs) = self.subscribers.get_mut(market) else {
            return;
        };
        if subs.remove(&client).is_some() {
            if subs.is_empty() {
                self.subscribers.remove(market);
            }
            if let Some(n) = self.client_watch.get_mut(market) {
                *n -= 1;
                if *n == 0 {
                    self.client_watch.remove(market);
                }
            }
            self.publish_venue(&market.0);
        }
    }

    /// Markets the trailing engine needs prices for.
    pub fn set_trailing_markets(&mut self, markets: Vec<Market>) {
        let next: BTreeSet<Market> = markets.into_iter().collect();
        if next == self.trailing_watch {
            return;
        }
        let touched: BTreeSet<String> = next
            .symmetric_difference(&self.trailing_watch)
            .map(|m| m.0.clone())
            .collect();
        self.trailing_watch = next;
        for ex in touched {
            self.publish_venue(&ex);
        }
    }

    fn publish_venue(&self, exchange: &str) {
        let Some(tx) = self.venues.get(exchange) else {
            return;
        };
        let set: BTreeSet<String> = self
            .client_watch
            .keys()
            .chain(self.trailing_watch.iter())
            .filter(|m| m.0 == exchange)
            .map(|m| usdt(&m.1))
            .collect();
        tx.send_if_modified(|cur| {
            if *cur == set {
                false
            } else {
                *cur = set;
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upd(price: f64) -> PriceUpdate {
        PriceUpdate {
            symbol: "BTCUSDT".into(),
            exchange: "bybit_futures",
            price,
            timestamp: Utc::now(),
            quote_volume_24h: None,
        }
    }

    #[tokio::test]
    async fn subscribers_get_prices_and_venues_follow_watchers() {
        let (mut hub, rx) = Hub::new();
        let bybit = &rx[&Venue::Bybit];
        let m: Market = ("bybit_futures".into(), "BTCUSDT".into());
        let (tx, mut prices) = mpsc::channel(10);
        let c = hub.new_client();
        hub.subscribe(c, m.clone(), tx);
        assert!(bybit.borrow().contains("BTCUSDT"));
        hub.publish(&upd(5.0));
        assert!(prices.recv().await.unwrap().contains(r#""price":5.0"#));
        assert_eq!(hub.latest("bybit_futures", "BTCUSDT").unwrap().price, 5.0);

        hub.set_trailing_markets(vec![m.clone()]);
        hub.unsubscribe(c, &m);
        assert!(
            bybit.borrow().contains("BTCUSDT"),
            "trailing still needs it"
        );
        hub.set_trailing_markets(vec![]);
        assert!(bybit.borrow().is_empty());
    }
}
