//! Shared state and the price pipeline: every tick updates the hub (latest
//! prices, /ws/price clients), builds candles and alerts for the candle
//! exchanges, and drives trailing positions. Database writes, webhooks and
//! close requests run in their own tasks so a slow one never delays prices.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc;

use crate::alerts::{self, PriceAlert, SpikeAlertConfig};
use crate::candles::CandleBook;
use crate::config::Config;
use crate::feeds::Alive;
use crate::hub::Hub;
use crate::model::{Candle, PriceUpdate};
use crate::store::{webhook_payload, Store};
use crate::trailing::{CloseOrder, Trailing, WatchRequest};

/// Above the api's worst case for a close: 45 s exchange wait plus reads.
const CLOSE_HTTP_TIMEOUT: Duration = Duration::from_secs(110);

#[derive(Default)]
pub struct Stats {
    pub total_updates: AtomicU64,
    pub candles_created: AtomicU64,
    pub alerts_sent: AtomicU64,
    pub trailing_closes: AtomicU64,
}

pub struct App {
    pub cfg: Config,
    pub hub: Mutex<Hub>,
    pub trailing: Mutex<Trailing>,
    pub store: Store,
    pub configs: RwLock<Vec<SpikeAlertConfig>>,
    pub stats: Stats,
    /// (exchange name, connected)
    pub feeds: Vec<(&'static str, Alive)>,
    pub http: reqwest::Client,
    candle_tx: mpsc::Sender<Candle>,
    alert_tx: mpsc::Sender<PriceAlert>,
    close_tx: mpsc::UnboundedSender<CloseOrder>,
}

pub struct Channels {
    pub candles: mpsc::Receiver<Candle>,
    pub alerts: mpsc::Receiver<PriceAlert>,
    pub closes: mpsc::UnboundedReceiver<CloseOrder>,
}

impl App {
    pub fn new(
        cfg: Config,
        hub: Hub,
        store: Store,
        feeds: Vec<(&'static str, Alive)>,
    ) -> (Arc<Self>, Channels) {
        let (candle_tx, candles) = mpsc::channel(200_000);
        let (alert_tx, alerts) = mpsc::channel(10_000);
        let (close_tx, closes) = mpsc::unbounded_channel();
        let app = Arc::new(Self {
            cfg,
            hub: Mutex::new(hub),
            trailing: Mutex::new(Trailing::new()),
            store,
            configs: RwLock::new(Vec::new()),
            stats: Stats::default(),
            feeds,
            http: reqwest::Client::builder().build().expect("http client"),
            candle_tx,
            alert_tx,
            close_tx,
        });
        (
            app,
            Channels {
                candles,
                alerts,
                closes,
            },
        )
    }

    pub async fn reload_configs(&self) {
        let c = self.store.spike_configs().await;
        *self.configs.write() = c;
    }

    /// Replace the trailing set; returns the statuses for the answer.
    pub fn sync_trailing(&self, list: Vec<WatchRequest>) -> Vec<crate::trailing::Status> {
        let latest: HashMap<(String, String), f64> = {
            let hub = self.hub.lock();
            list.iter()
                .filter_map(|r| {
                    let sym = r.symbol.to_uppercase();
                    let l = hub.latest(&r.exchange, &sym)?;
                    // Only a recent price may price a new position.
                    let fresh = (chrono::Utc::now() - l.timestamp).num_seconds() <= 120;
                    fresh.then(|| ((r.exchange.clone(), sym), l.price))
                })
                .collect()
        };
        let (closes, markets, statuses) = {
            let mut t = self.trailing.lock();
            let closes = t.sync(list, |ex, sym| {
                latest.get(&(ex.to_string(), sym.to_string())).copied()
            });
            (closes, t.markets(), t.statuses())
        };
        self.hub.lock().set_trailing_markets(markets);
        for c in closes {
            let _ = self.close_tx.send(c);
        }
        statuses
    }

    fn on_update(&self, u: &PriceUpdate, book: &mut CandleBook) {
        self.hub.lock().publish(u);

        let closes = self
            .trailing
            .lock()
            .on_price(u.exchange, &u.symbol, u.price, Instant::now());
        for c in closes {
            let _ = self.close_tx.send(c);
        }

        if !self.cfg.candle_exchanges.contains(u.exchange) {
            return;
        }
        let done = book.add(u);
        if done.is_empty() {
            return;
        }
        self.stats
            .candles_created
            .fetch_add(done.len() as u64, Ordering::Relaxed);
        let now = chrono::Utc::now();
        let configs = self.configs.read();
        for c in done {
            for a in alerts::check(&c, self.cfg.webhook_url.as_deref(), &configs, now) {
                if self.alert_tx.try_send(a).is_err() {
                    tracing::warn!("alert queue full; dropping an alert");
                }
            }
            if self.candle_tx.try_send(c).is_err() {
                tracing::warn!("candle write queue full; dropping a candle");
            }
        }
    }

    /// The price pipeline. Runs until every feed is gone.
    pub async fn process(self: Arc<Self>, mut rx: mpsc::Receiver<Vec<PriceUpdate>>) {
        let mut book = CandleBook::new();
        let mut batches = Vec::new();
        while rx.recv_many(&mut batches, 64).await > 0 {
            for batch in batches.drain(..) {
                self.stats
                    .total_updates
                    .fetch_add(batch.len() as u64, Ordering::Relaxed);
                for u in &batch {
                    self.on_update(u, &mut book);
                }
            }
        }
    }

    /// Stores every alert and sends those with a webhook URL.
    pub async fn alert_sender(self: Arc<Self>, mut rx: mpsc::Receiver<PriceAlert>) {
        while let Some(a) = rx.recv().await {
            self.stats.alerts_sent.fetch_add(1, Ordering::Relaxed);
            if let Some(url) = a.webhook_url.clone() {
                let app = self.clone();
                let payload = webhook_payload(&a);
                let symbol = a.symbol.clone();
                tokio::spawn(async move {
                    let mut req = app
                        .http
                        .post(&url)
                        .json(&payload)
                        .timeout(app.cfg.webhook_timeout);
                    if let Some(k) = &app.cfg.webhook_api_key {
                        req = req.header("x-api-key", k);
                    }
                    match req.send().await {
                        Ok(r) if r.status().is_success() => {
                            tracing::info!("Webhook sent for {symbol}")
                        }
                        Ok(r) => {
                            tracing::warn!("Webhook returned {} for {symbol}", r.status().as_u16())
                        }
                        Err(e) => tracing::error!(
                            "HTTP error sending webhook for {symbol}: {}",
                            e.without_url()
                        ),
                    }
                });
            }
            let store = self.store.clone();
            tokio::spawn(async move { store.store_detected(&a).await });
        }
    }

    /// Sends trailing closes to the api (one request each; a failure lets the
    /// position close again after a pause).
    pub async fn closer(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<CloseOrder>) {
        while let Some(o) = rx.recv().await {
            self.stats.trailing_closes.fetch_add(1, Ordering::Relaxed);
            let summary = format!(
                "{} {} {} (entry {}, high {}, stop {}, last {})",
                o.position_id,
                o.symbol,
                o.position_type,
                o.entry_price,
                o.peak_price,
                o.stop_price,
                o.last_price
            );
            let Some(url) = self.cfg.trailing_close_url.clone() else {
                tracing::info!("DRY RUN would close {summary}");
                continue;
            };
            tracing::info!("Asking the api to close {summary}");
            let app = self.clone();
            tokio::spawn(async move {
                let mut req = app.http.post(&url).json(&o).timeout(CLOSE_HTTP_TIMEOUT);
                if let Some(k) = &app.cfg.trailing_api_key {
                    req = req.header("x-api-key", k);
                }
                let failed = match req.send().await {
                    Ok(r) if r.status().is_success() => {
                        let body = r.text().await.unwrap_or_default();
                        tracing::info!(
                            "Api close answer for {}: {}",
                            o.position_id,
                            &body[..body.len().min(500)]
                        );
                        false
                    }
                    Ok(r) => {
                        tracing::error!(
                            "Close request for {} failed: api answered {}",
                            o.position_id,
                            r.status().as_u16()
                        );
                        true
                    }
                    Err(e) => {
                        tracing::error!(
                            "Close request for {} failed: {}",
                            o.position_id,
                            e.without_url()
                        );
                        true
                    }
                };
                if failed {
                    app.trailing
                        .lock()
                        .close_failed(&o.position_id, Instant::now());
                }
            });
        }
    }

    /// Drops the trailing list when the worker stopped syncing.
    pub async fn expire_trailing(self: Arc<Self>) {
        let mut every = tokio::time::interval(Duration::from_secs(30));
        loop {
            every.tick().await;
            let dropped = self.trailing.lock().expire(Instant::now());
            if dropped > 0 {
                tracing::warn!(
                    "No trailing sync for 10 minutes: stopped watching {dropped} position(s)"
                );
                self.hub.lock().set_trailing_markets(Vec::new());
            }
        }
    }

    /// One log line every 5 minutes.
    pub async fn report(self: Arc<Self>) {
        let mut every = tokio::time::interval(Duration::from_secs(300));
        every.tick().await;
        loop {
            every.tick().await;
            let s = &self.stats;
            tracing::info!(
                "updates {}, candles {}, alerts {}, trailing positions {}, trailing closes {}",
                s.total_updates.load(Ordering::Relaxed),
                s.candles_created.load(Ordering::Relaxed),
                s.alerts_sent.load(Ordering::Relaxed),
                self.trailing.lock().len(),
                s.trailing_closes.load(Ordering::Relaxed),
            );
        }
    }
}
