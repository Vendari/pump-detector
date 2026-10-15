//! MongoDB: candles (by default the last 1,000 per market and interval from
//! 1m up, an hour of 10s and two hours of 30s), detected alerts (with a
//! candle snapshot for later training) and custom alert configs. Same
//! database, collections and document shapes as the Python service.

use std::time::Duration;

use bson::{doc, Bson, DateTime as BsonDateTime, Document};
use chrono::{DateTime, Utc};
use futures::{stream, StreamExt, TryStreamExt};
use mongodb::options::IndexOptions;
use mongodb::{Client, Collection, IndexModel};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::alerts::{PriceAlert, SpikeAlertConfig};
use crate::model::{Candle, Interval};

/// Candles per interval in a detected alert's snapshot and the legacy
/// `/candles` default.
pub const MAX_CANDLES_PER_INTERVAL: i64 = 100;
const WRITE_CONCURRENCY: usize = 16;

/// Candles kept per market for an interval.
pub fn retention(i: Interval, configured: i64) -> i64 {
    match i {
        Interval::S10 => 360,
        Interval::S30 => 240,
        _ => configured,
    }
}

/// One candle as the API serves it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bar {
    /// Bucket start, Unix seconds.
    pub start: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

impl Bar {
    pub fn json(&self) -> Value {
        json!({"time": self.start, "open": self.open, "high": self.high, "low": self.low,
               "close": self.close, "volume": self.volume})
    }
}

fn bar_of(d: &Document) -> Option<Bar> {
    Some(Bar {
        start: d.get_datetime("timestamp").ok()?.timestamp_millis() / 1000,
        open: d.get_f64("open").ok()?,
        high: d.get_f64("high").ok()?,
        low: d.get_f64("low").ok()?,
        close: d.get_f64("close").ok()?,
        volume: d.get_f64("volume").unwrap_or(0.0),
    })
}

#[derive(Clone)]
pub struct Store {
    candles: Collection<Document>,
    spike_alerts: Collection<Document>,
    detected: Collection<Document>,
}

fn bdt(ts: DateTime<Utc>) -> BsonDateTime {
    BsonDateTime::from_millis(ts.timestamp_millis())
}

/// Naive ISO time for JSON, like FastAPI renders Python's naive datetimes.
fn iso(b: &Bson) -> Value {
    match b {
        Bson::DateTime(d) => Value::String(crate::model::iso_naive(d.to_chrono())),
        other => other.clone().into_relaxed_extjson(),
    }
}

/// A Mongo document as API JSON, without `_id`, dates as naive ISO strings.
pub fn to_json(d: &Document) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in d {
        if k == "_id" {
            continue;
        }
        let jv = match v {
            Bson::DateTime(_) => iso(v),
            Bson::Document(inner) => to_json(inner),
            Bson::Array(a) => Value::Array(
                a.iter()
                    .map(|x| match x {
                        Bson::Document(inner) => to_json(inner),
                        Bson::DateTime(_) => iso(x),
                        other => other.clone().into_relaxed_extjson(),
                    })
                    .collect(),
            ),
            other => other.clone().into_relaxed_extjson(),
        };
        m.insert(k.clone(), jv);
    }
    Value::Object(m)
}

pub fn candle_doc(c: &Candle) -> Document {
    doc! {
        "symbol": &c.symbol,
        "exchange": c.exchange,
        "interval": c.interval.as_str(),
        "open": c.open,
        "high": c.high,
        "low": c.low,
        "close": c.close,
        "timestamp": BsonDateTime::from_millis(c.start * 1000),
        "volume": c.volume,
    }
}

impl Store {
    pub async fn connect(url: &str, db_name: &str) -> mongodb::error::Result<Self> {
        let client = Client::with_uri_str(url).await?;
        let db = client.database(db_name);
        let s = Self {
            candles: db.collection("candles"),
            spike_alerts: db.collection("spike_alerts"),
            detected: db.collection("detected_alerts"),
        };
        db.run_command(doc! {"ping": 1}).await?;
        s.indexes().await?;
        Ok(s)
    }

    async fn indexes(&self) -> mongodb::error::Result<()> {
        let named = |keys: Document, name: &str| {
            IndexModel::builder()
                .keys(keys)
                .options(IndexOptions::builder().name(name.to_string()).build())
                .build()
        };
        self.candles
            .create_indexes(vec![
                named(
                    doc! {"symbol": 1, "exchange": 1, "interval": 1, "timestamp": -1},
                    "symbol_exchange_interval_timestamp_idx",
                ),
                named(
                    doc! {"interval": 1, "timestamp": 1},
                    "interval_timestamp_idx",
                ),
            ])
            .await?;
        // A prefix of the index above, so it only cost memory and disk.
        let _ = self
            .candles
            .drop_index("symbol_exchange_interval_idx")
            .await;
        self.spike_alerts
            .create_indexes(vec![named(
                doc! {"exchange": 1, "symbol": 1, "interval": 1},
                "alert_exchange_symbol_interval_idx",
            )])
            .await?;
        self.detected
            .create_indexes(vec![
                named(doc! {"timestamp": -1}, "detected_timestamp_idx"),
                named(
                    doc! {"symbol": 1, "exchange": 1, "interval": 1},
                    "detected_symbol_exchange_interval_idx",
                ),
            ])
            .await?;
        Ok(())
    }

    async fn upsert_candle(&self, c: &Candle) -> mongodb::error::Result<()> {
        let d = candle_doc(c);
        self.candles
            .update_one(
                doc! {"symbol": &c.symbol, "exchange": c.exchange, "interval": c.interval.as_str(),
                "timestamp": BsonDateTime::from_millis(c.start * 1000)},
                doc! {"$set": d},
            )
            .upsert(true)
            .await?;
        Ok(())
    }

    /// Writes completed candles as they come, a few at a time.
    pub async fn candle_writer(self, mut rx: mpsc::Receiver<Candle>) {
        let mut errors = 0u64;
        loop {
            let mut batch = Vec::with_capacity(256);
            if rx.recv_many(&mut batch, 1024).await == 0 {
                return;
            }
            let results: Vec<_> = stream::iter(batch)
                .map(|c| {
                    let s = self.clone();
                    async move { s.upsert_candle(&c).await }
                })
                .buffer_unordered(WRITE_CONCURRENCY)
                .collect()
                .await;
            for r in results {
                if let Err(e) = r {
                    errors += 1;
                    if errors % 100 == 1 {
                        tracing::error!("Error upserting candle ({errors} so far): {e}");
                    }
                }
            }
        }
    }

    /// Delete candles older than `retention` intervals.
    pub async fn trim_loop(self, configured: i64) {
        let mut every = tokio::time::interval(Duration::from_secs(600));
        loop {
            every.tick().await;
            let now = Utc::now().timestamp();
            let mut deleted = 0;
            for i in Interval::ALL {
                let cutoff = BsonDateTime::from_millis(
                    (now - retention(i, configured) * i.seconds()) * 1000,
                );
                match self
                    .candles
                    .delete_many(doc! {"interval": i.as_str(), "timestamp": {"$lt": cutoff}})
                    .await
                {
                    Ok(r) => deleted += r.deleted_count,
                    Err(e) => tracing::error!("Error trimming {} candles: {e}", i.as_str()),
                }
            }
            if deleted > 0 {
                tracing::info!("Trimmed {deleted} old candles");
            }
        }
    }

    /// Candles oldest first (the newest `limit`).
    pub async fn get_candles(
        &self,
        symbol: &str,
        exchange: &str,
        interval: &str,
        limit: i64,
    ) -> Vec<Document> {
        let r = self
            .candles
            .find(doc! {"symbol": symbol, "exchange": exchange, "interval": interval})
            .projection(doc! {"_id": 0})
            .sort(doc! {"timestamp": -1})
            .limit(limit)
            .await;
        match r {
            Ok(cur) => {
                let mut v: Vec<Document> = cur.try_collect().await.unwrap_or_default();
                v.reverse();
                v
            }
            Err(e) => {
                tracing::error!("Error getting candles: {e}");
                Vec::new()
            }
        }
    }

    /// Up to `limit` candles starting at or before `end` (Unix seconds),
    /// oldest first.
    pub async fn bars(
        &self,
        symbol: &str,
        exchange: &str,
        interval: &str,
        end: i64,
        limit: i64,
    ) -> Vec<Bar> {
        let r = self
            .candles
            .find(doc! {"symbol": symbol, "exchange": exchange, "interval": interval,
                        "timestamp": {"$lte": BsonDateTime::from_millis(end * 1000)}})
            .projection(doc! {"_id": 0, "timestamp": 1, "open": 1, "high": 1, "low": 1, "close": 1, "volume": 1})
            .sort(doc! {"timestamp": -1})
            .limit(limit)
            .await;
        match r {
            Ok(cur) => {
                let docs: Vec<Document> = cur.try_collect().await.unwrap_or_default();
                let mut v: Vec<Bar> = docs.iter().filter_map(bar_of).collect();
                v.reverse();
                v
            }
            Err(e) => {
                tracing::error!("Error getting candles: {e}");
                Vec::new()
            }
        }
    }

    /// Store closed candles fetched from an exchange. They replace ours for
    /// the same bucket: ours can be partial after a restart or reconnect.
    pub async fn insert_missing(
        &self,
        symbol: &str,
        exchange: &str,
        interval: Interval,
        bars: &[Bar],
    ) {
        let results: Vec<_> = stream::iter(bars.iter().copied())
            .map(|b| async move {
                let ts = BsonDateTime::from_millis(b.start * 1000);
                self.candles
                    .update_one(
                        doc! {"symbol": symbol, "exchange": exchange, "interval": interval.as_str(), "timestamp": ts},
                        doc! {"$set": {"symbol": symbol, "exchange": exchange, "interval": interval.as_str(),
                              "open": b.open, "high": b.high, "low": b.low, "close": b.close,
                              "timestamp": ts, "volume": b.volume}},
                    )
                    .upsert(true)
                    .await
            })
            .buffer_unordered(WRITE_CONCURRENCY)
            .collect()
            .await;
        if let Some(Err(e)) = results.into_iter().find(Result::is_err) {
            tracing::warn!("Storing backfilled candles for {exchange} {symbol}: {e}");
        }
    }

    /// Newest detected alerts without their candle snapshots.
    pub async fn recent_detected(&self, limit: i64) -> Vec<Value> {
        match self
            .detected
            .find(doc! {})
            .projection(doc! {"_id": 0, "candles": 0})
            .sort(doc! {"timestamp": -1})
            .limit(limit)
            .await
        {
            Ok(cur) => cur
                .try_collect::<Vec<Document>>()
                .await
                .unwrap_or_default()
                .iter()
                .map(to_json)
                .collect(),
            Err(e) => {
                tracing::error!("Error getting detected alerts: {e}");
                Vec::new()
            }
        }
    }

    pub async fn latest_close(&self, symbol: &str, exchange: &str) -> Option<(f64, BsonDateTime)> {
        let d = self
            .candles
            .find_one(doc! {"symbol": symbol, "exchange": exchange})
            .projection(doc! {"_id": 0, "close": 1, "timestamp": 1})
            .sort(doc! {"timestamp": -1})
            .await
            .ok()??;
        Some((d.get_f64("close").ok()?, *d.get_datetime("timestamp").ok()?))
    }

    pub async fn distinct_exchanges(&self) -> Vec<String> {
        self.candles
            .distinct("exchange", doc! {})
            .await
            .map(|v| {
                v.into_iter()
                    .filter_map(|b| b.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub async fn distinct_symbols(&self, exchange: &str) -> Vec<String> {
        self.candles
            .distinct("symbol", doc! {"exchange": exchange})
            .await
            .map(|v| {
                v.into_iter()
                    .filter_map(|b| b.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    // --- custom alert configs ---

    pub async fn spike_configs(&self) -> Vec<SpikeAlertConfig> {
        match self.spike_alerts.find(doc! {}).await {
            Ok(cur) => cur
                .try_collect::<Vec<Document>>()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter_map(|d| serde_json::from_value(to_json(&d)).ok())
                .collect(),
            Err(e) => {
                tracing::error!("Error loading alert configs: {e}");
                Vec::new()
            }
        }
    }

    pub async fn list_spike_alerts(
        &self,
        exchange: Option<&str>,
        symbol: Option<&str>,
    ) -> Vec<Value> {
        let mut q = doc! {};
        if let Some(e) = exchange {
            q.insert("exchange", e);
        }
        if let Some(s) = symbol {
            q.insert("$or", vec![doc! {"symbol": s}, doc! {"symbol": Bson::Null}]);
        }
        match self.spike_alerts.find(q).projection(doc! {"_id": 0}).await {
            Ok(cur) => cur
                .try_collect::<Vec<Document>>()
                .await
                .unwrap_or_default()
                .iter()
                .map(to_json)
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    pub async fn create_spike_alert(
        &self,
        cfg: &SpikeAlertConfig,
    ) -> mongodb::error::Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.spike_alerts
            .insert_one(doc! {
                "webhook_url": &cfg.webhook_url,
                "exchange": &cfg.exchange,
                "interval": &cfg.interval,
                "threshold_percent": cfg.threshold_percent,
                "symbol": cfg.symbol.clone().map(Bson::String).unwrap_or(Bson::Null),
                "created_at": bdt(Utc::now()),
                "alert_id": &id,
            })
            .await?;
        Ok(id)
    }

    pub async fn delete_spike_alert(&self, id: &str) -> bool {
        self.spike_alerts
            .delete_one(doc! {"alert_id": id})
            .await
            .map(|r| r.deleted_count > 0)
            .unwrap_or(false)
    }

    // --- detected alerts ---

    /// Every stored candle of this market, by interval (oldest first).
    async fn snapshot(&self, symbol: &str, exchange: &str) -> Document {
        let mut out = Document::new();
        for i in Interval::BUILT {
            let rows = self
                .get_candles(symbol, exchange, i.as_str(), MAX_CANDLES_PER_INTERVAL)
                .await;
            if !rows.is_empty() {
                out.insert(i.as_str(), rows);
            }
        }
        out
    }

    pub async fn store_detected(&self, a: &PriceAlert) {
        let candles = self.snapshot(&a.symbol, a.exchange).await;
        let d = doc! {
            "symbol": &a.symbol,
            "exchange": a.exchange,
            "alert_type": a.alert_type,
            "start_price": a.start_price,
            "current_price": a.current_price,
            "change_percent": a.change_percent,
            "interval": a.interval.as_str(),
            "timestamp": bdt(a.timestamp),
            "webhook_url": a.webhook_url.clone().map(Bson::String).unwrap_or(Bson::Null),
            "alert_id": a.alert_id.clone().map(Bson::String).unwrap_or(Bson::Null),
            "candles": candles,
        };
        if let Err(e) = self.detected.insert_one(d).await {
            tracing::error!("Error storing detected alert: {e}");
        }
    }

    pub async fn detected_alerts(
        &self,
        symbol: Option<&str>,
        exchange: Option<&str>,
        limit: i64,
    ) -> Vec<Value> {
        let mut q = doc! {};
        if let Some(s) = symbol {
            q.insert("symbol", s);
        }
        if let Some(e) = exchange {
            q.insert("exchange", e);
        }
        match self
            .detected
            .find(q)
            .projection(doc! {"_id": 0})
            .sort(doc! {"timestamp": -1})
            .limit(limit)
            .await
        {
            Ok(cur) => cur
                .try_collect::<Vec<Document>>()
                .await
                .unwrap_or_default()
                .iter()
                .map(to_json)
                .collect(),
            Err(e) => {
                tracing::error!("Error getting detected alerts: {e}");
                Vec::new()
            }
        }
    }
}

/// Webhook payload (same as the Python service).
pub fn webhook_payload(a: &PriceAlert) -> Value {
    json!({
        "symbol": a.symbol,
        "exchange": a.exchange,
        "alert_type": a.alert_type,
        "start_price": a.start_price,
        "current_price": a.current_price,
        "change_percent": (a.change_percent * 100.0).round() / 100.0,
        "interval": a.interval.as_str(),
        "timestamp": crate::model::iso_naive(a.timestamp),
        "alert_id": a.alert_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_drops_id_and_renders_dates() {
        let d = doc! {"_id": 1, "close": 1.5, "timestamp": BsonDateTime::from_millis(1_791_000_000_000), "n": Bson::Null};
        let j = to_json(&d);
        assert_eq!(
            j,
            json!({"close": 1.5, "timestamp": "2026-10-03T04:00:00", "n": null})
        );
    }

    #[test]
    fn webhook_payload_rounds_change() {
        let a = PriceAlert {
            symbol: "X".into(),
            exchange: "gate_futures",
            alert_type: "drop",
            start_price: 1.0,
            current_price: 0.87654,
            change_percent: -12.346,
            interval: Interval::M5,
            timestamp: chrono::TimeZone::timestamp_opt(&Utc, 1_791_000_000, 0).unwrap(),
            webhook_url: Some("u".into()),
            alert_id: Some("builtin_10".into()),
        };
        let p = webhook_payload(&a);
        assert_eq!(p["change_percent"], json!(-12.35));
        assert_eq!(p["interval"], json!("5m"));
        assert_eq!(p["timestamp"], json!("2026-10-03T04:00:00"));
    }
}
