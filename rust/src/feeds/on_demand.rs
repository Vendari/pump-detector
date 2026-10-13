//! On-demand price feeds: connect to an exchange's public WebSocket only for
//! the symbols someone watches, subscribing and unsubscribing as watchers
//! come and go, and close after a minute with nothing to watch.
//!
//! Feeds (checked live 2026-10-12):
//! - Bybit `wss://stream.bybit.com/v5/public/linear`, `tickers.<SYM>` (deltas
//!   carry lastPrice only when it changed; max 10 args per request).
//! - KuCoin: token from `POST /api/v1/bullet-public`, wait for `welcome`,
//!   then `/contractMarket/execution:<SYM>` (trades; tickers send nothing).
//! - Aster `wss://fstream.asterdex.com/ws`, `<sym>@aggTrade`.
//! - Hyperliquid `wss://api.hyperliquid.xyz/ws`, `trades` per coin.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use chrono::Utc;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

use super::{num, Alive, Reconnect, Sink};
use crate::model::PriceUpdate;

const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const LINGER: Duration = Duration::from_secs(60);
const APP_PING_EVERY: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Venue {
    Bybit,
    Kucoin,
    Aster,
    Hyperliquid,
}

pub fn usdt(symbol: &str) -> String {
    let s = symbol.to_uppercase();
    if s.ends_with("USDT") {
        s
    } else {
        format!("{s}USDT")
    }
}

impl Venue {
    pub const ALL: [Venue; 4] = [
        Venue::Bybit,
        Venue::Kucoin,
        Venue::Aster,
        Venue::Hyperliquid,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Venue::Bybit => "bybit_futures",
            Venue::Kucoin => "kucoin_futures",
            Venue::Aster => "aster_futures",
            Venue::Hyperliquid => "hyperliquid",
        }
    }

    pub fn from_name(name: &str) -> Option<Venue> {
        Venue::ALL.into_iter().find(|v| v.name() == name)
    }

    /// Our symbol (XUSDT) to the venue's.
    pub fn venue_symbol(self, symbol: &str) -> String {
        let s = usdt(symbol);
        match self {
            Venue::Kucoin => {
                let base = &s[..s.len() - 4];
                format!("{}USDTM", if base == "BTC" { "XBT" } else { base })
            }
            Venue::Hyperliquid => s[..s.len() - 4].to_string(),
            _ => s,
        }
    }

    pub fn subscription(self, syms: &[String], subscribe: bool, msg_id: &mut u64) -> Vec<String> {
        if syms.is_empty() {
            return Vec::new();
        }
        match self {
            Venue::Bybit => syms
                .chunks(10)
                .map(|c| {
                    json!({"op": if subscribe {"subscribe"} else {"unsubscribe"},
                           "args": c.iter().map(|s| format!("tickers.{s}")).collect::<Vec<_>>()})
                    .to_string()
                })
                .collect(),
            Venue::Kucoin => syms
                .chunks(100)
                .map(|c| {
                    *msg_id += 1;
                    json!({"id": msg_id.to_string(), "type": if subscribe {"subscribe"} else {"unsubscribe"},
                           "topic": format!("/contractMarket/execution:{}", c.join(",")),
                           "privateChannel": false, "response": true})
                    .to_string()
                })
                .collect(),
            Venue::Aster => {
                *msg_id += 1;
                vec![json!({"method": if subscribe {"SUBSCRIBE"} else {"UNSUBSCRIBE"},
                            "params": syms.iter().map(|s| format!("{}@aggTrade", s.to_lowercase())).collect::<Vec<_>>(),
                            "id": *msg_id})
                .to_string()]
            }
            Venue::Hyperliquid => syms
                .iter()
                .map(|c| {
                    json!({"method": if subscribe {"subscribe"} else {"unsubscribe"},
                           "subscription": {"type": "trades", "coin": c}})
                    .to_string()
                })
                .collect(),
        }
    }

    /// (venue symbol, last price) pairs in one frame.
    pub fn parse(self, text: &str) -> Vec<(String, f64)> {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return Vec::new();
        };
        let one = |sym: Option<&Value>, price: Option<&Value>| -> Vec<(String, f64)> {
            match (sym.and_then(Value::as_str), num(price)) {
                (Some(s), Some(p)) => vec![(s.to_uppercase(), p)],
                _ => Vec::new(),
            }
        };
        match self {
            Venue::Bybit => {
                if !v
                    .get("topic")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .starts_with("tickers.")
                {
                    return Vec::new();
                }
                let d = v.get("data");
                one(
                    d.and_then(|d| d.get("symbol")),
                    d.and_then(|d| d.get("lastPrice")),
                )
            }
            Venue::Kucoin => {
                if v.get("subject").and_then(Value::as_str) != Some("match") {
                    return Vec::new();
                }
                let d = v.get("data");
                one(
                    d.and_then(|d| d.get("symbol")),
                    d.and_then(|d| d.get("price")),
                )
            }
            Venue::Aster => {
                if v.get("e").and_then(Value::as_str) != Some("aggTrade") {
                    return Vec::new();
                }
                one(v.get("s"), v.get("p"))
            }
            Venue::Hyperliquid => {
                if v.get("channel").and_then(Value::as_str) != Some("trades") {
                    return Vec::new();
                }
                v.get("data")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .flat_map(|t| one(t.get("coin"), t.get("px")))
                    .collect()
            }
        }
    }

    fn ping_message(self, msg_id: &mut u64) -> Option<String> {
        match self {
            Venue::Bybit => Some(json!({"op": "ping"}).to_string()),
            Venue::Kucoin => {
                *msg_id += 1;
                Some(json!({"id": msg_id.to_string(), "type": "ping"}).to_string())
            }
            Venue::Aster => None,
            Venue::Hyperliquid => Some(json!({"method": "ping"}).to_string()),
        }
    }

    async fn ws_url(self, http: &reqwest::Client) -> Result<String, String> {
        match self {
            Venue::Bybit => Ok("wss://stream.bybit.com/v5/public/linear".into()),
            Venue::Aster => Ok("wss://fstream.asterdex.com/ws".into()),
            Venue::Hyperliquid => Ok("wss://api.hyperliquid.xyz/ws".into()),
            Venue::Kucoin => {
                let body: Value = http
                    .post("https://api-futures.kucoin.com/api/v1/bullet-public")
                    .send()
                    .await
                    .and_then(|r| r.error_for_status())
                    .map_err(|e| e.to_string())?
                    .json()
                    .await
                    .map_err(|e| e.to_string())?;
                let d = &body["data"];
                let endpoint = d["instanceServers"][0]["endpoint"]
                    .as_str()
                    .ok_or("no endpoint")?;
                let token = d["token"].as_str().ok_or("no token")?;
                Ok(format!(
                    "{endpoint}?token={token}&connectId=pd{}",
                    Utc::now().timestamp_millis()
                ))
            }
        }
    }
}

/// Runs one venue for as long as the sink lives.
pub async fn run(
    venue: Venue,
    sink: Sink,
    alive: Alive,
    mut wanted: watch::Receiver<BTreeSet<String>>,
) {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("http client");
    let mut pace = Reconnect::new(venue.name());
    loop {
        if wanted.borrow_and_update().is_empty() {
            alive.set(false);
            if wanted.changed().await.is_err() {
                return;
            }
            continue;
        }
        match session(venue, &sink, &alive, &mut wanted, &http).await {
            // Closed because nothing was watched.
            Ok(()) => continue,
            Err((got, e)) => {
                alive.set(false);
                if sink.is_closed() {
                    return;
                }
                tracing::warn!("{} price stream: {e}", venue.name());
                let wait = pace.after(got);
                tracing::info!("{}: reconnecting in {}s", venue.name(), wait.as_secs());
                tokio::time::sleep(wait).await;
            }
        }
    }
}

async fn session(
    venue: Venue,
    sink: &Sink,
    alive: &Alive,
    wanted: &mut watch::Receiver<BTreeSet<String>>,
    http: &reqwest::Client,
) -> Result<(), (bool, String)> {
    let url = venue.ws_url(http).await.map_err(|e| (false, e))?;
    let (ws, _) = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async(url.as_str()),
    )
    .await
    .map_err(|_| (false, "connect timed out".to_string()))?
    .map_err(|e| (false, e.to_string()))?;
    let (mut tx, mut rx) = ws.split();
    if venue == Venue::Kucoin {
        // KuCoin drops subscriptions sent before its welcome message.
        let welcome = async {
            while let Some(Ok(m)) = rx.next().await {
                if let Message::Text(t) = m {
                    if t.as_str().contains(r#""type":"welcome""#) {
                        return true;
                    }
                }
            }
            false
        };
        match tokio::time::timeout(Duration::from_secs(10), welcome).await {
            Ok(true) => {}
            _ => return Err((false, "no welcome".into())),
        }
    }
    tracing::info!("{} price stream connected", venue.name());
    alive.set(true);

    let mut msg_id = 0u64;
    let mut subscribed: HashMap<String, String> = HashMap::new(); // venue symbol -> symbol
    let mut got = false;
    let mut last_frame = Instant::now();
    let mut empty_since: Option<Instant> = None;
    let mut sync = true;
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut app_ping = tokio::time::interval(APP_PING_EVERY);
    app_ping.reset();
    let mut ws_ping = tokio::time::interval(Duration::from_secs(20));
    ws_ping.reset();
    let send_err = |e: tokio_tungstenite::tungstenite::Error, got: bool| (got, e.to_string());
    loop {
        if sync {
            sync = false;
            let want: HashMap<String, String> = wanted
                .borrow_and_update()
                .iter()
                .map(|s| (venue.venue_symbol(s), usdt(s)))
                .collect();
            let drop: Vec<String> = subscribed
                .keys()
                .filter(|v| !want.contains_key(*v))
                .cloned()
                .collect();
            let add: Vec<String> = want
                .keys()
                .filter(|v| !subscribed.contains_key(*v))
                .cloned()
                .collect();
            for m in venue.subscription(&drop, false, &mut msg_id) {
                tx.send(Message::text(m))
                    .await
                    .map_err(|e| send_err(e, got))?;
            }
            for m in venue.subscription(&add, true, &mut msg_id) {
                tx.send(Message::text(m))
                    .await
                    .map_err(|e| send_err(e, got))?;
            }
            subscribed = want;
            if subscribed.is_empty() {
                empty_since.get_or_insert_with(Instant::now);
            } else {
                empty_since = None;
            }
        }
        tokio::select! {
            changed = wanted.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                sync = true;
            }
            frame = rx.next() => {
                let msg = match frame {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err((got, e.to_string())),
                    None => return Err((got, "closed by server".into())),
                };
                last_frame = Instant::now();
                match msg {
                    Message::Text(t) => {
                        let now = Utc::now();
                        let updates: Vec<PriceUpdate> = venue
                            .parse(t.as_str())
                            .into_iter()
                            .filter_map(|(vs, price)| {
                                subscribed.get(&vs).map(|symbol| PriceUpdate {
                                    symbol: symbol.clone(),
                                    exchange: venue.name(),
                                    price,
                                    timestamp: now,
                                    quote_volume_24h: None,
                                })
                            })
                            .collect();
                        if !updates.is_empty() {
                            got = true;
                            if sink.send(updates).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Message::Ping(p) => { let _ = tx.send(Message::Pong(p)).await; }
                    Message::Close(_) => return Err((got, "closed by server".into())),
                    _ => {}
                }
            }
            _ = tick.tick() => {
                if empty_since.is_some_and(|t| t.elapsed() > LINGER) {
                    tracing::info!("{} price stream closed (nothing to watch)", venue.name());
                    return Ok(());
                }
                if last_frame.elapsed() > IDLE_TIMEOUT {
                    return Err((got, "no data for 60 s".into()));
                }
            }
            _ = app_ping.tick() => {
                if let Some(m) = venue.ping_message(&mut msg_id) {
                    tx.send(Message::text(m)).await.map_err(|e| send_err(e, got))?;
                }
            }
            _ = ws_ping.tick() => {
                tx.send(Message::Ping(Vec::new())).await.map_err(|e| send_err(e, got))?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venue_symbols() {
        assert_eq!(Venue::Kucoin.venue_symbol("BTCUSDT"), "XBTUSDTM");
        assert_eq!(Venue::Kucoin.venue_symbol("zest"), "ZESTUSDTM");
        assert_eq!(Venue::Hyperliquid.venue_symbol("ETHUSDT"), "ETH");
        assert_eq!(Venue::Bybit.venue_symbol("ethusdt"), "ETHUSDT");
    }

    #[test]
    fn parses_each_venue() {
        assert_eq!(
            Venue::Bybit.parse(
                r#"{"topic":"tickers.BTCUSDT","data":{"symbol":"BTCUSDT","lastPrice":"83124.5"}}"#
            ),
            vec![("BTCUSDT".to_string(), 83124.5)]
        );
        assert!(Venue::Bybit
            .parse(r#"{"topic":"tickers.BTCUSDT","data":{"symbol":"BTCUSDT"}}"#)
            .is_empty());
        assert_eq!(
            Venue::Kucoin
                .parse(r#"{"subject":"match","data":{"symbol":"XBTUSDTM","price":"100.5"}}"#),
            vec![("XBTUSDTM".to_string(), 100.5)]
        );
        assert_eq!(
            Venue::Aster.parse(r#"{"e":"aggTrade","s":"btcusdt","p":"7"}"#),
            vec![("BTCUSDT".to_string(), 7.0)]
        );
        assert_eq!(
            Venue::Hyperliquid.parse(
                r#"{"channel":"trades","data":[{"coin":"eth","px":"2.5"},{"coin":"X","px":"0"}]}"#
            ),
            vec![("ETH".to_string(), 2.5)]
        );
    }

    #[test]
    fn subscription_messages() {
        let mut id = 0;
        let syms: Vec<String> = (0..12).map(|i| format!("S{i}USDT")).collect();
        assert_eq!(Venue::Bybit.subscription(&syms, true, &mut id).len(), 2);
        let k = Venue::Kucoin.subscription(&syms[..2], false, &mut id);
        assert!(k[0].contains(r#""topic":"/contractMarket/execution:S0USDT,S1USDT""#));
        assert!(k[0].contains(r#""type":"unsubscribe""#));
        let a = Venue::Aster.subscription(&syms[..1], true, &mut id);
        assert!(a[0].contains("s0usdt@aggTrade"));
        assert!(Venue::Hyperliquid
            .subscription(&[], true, &mut id)
            .is_empty());
    }
}
