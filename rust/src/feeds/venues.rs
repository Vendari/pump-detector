//! Bybit, KuCoin, Aster and Hyperliquid: every USDT perpetual, all the time.
//!
//! Each venue's market list comes from its REST contract list (refreshed
//! every 5 minutes, `markets.rs`), plus any symbol someone watches that the
//! list does not have yet (a /ws/price or /v1 client, or a trailing
//! position), so a brand-new listing is priced as soon as it is asked for.
//! Markets are spread over a fixed number of connections per venue (by a
//! hash of the symbol, so a refresh only moves new and removed markets) to
//! stay well inside each venue's per-connection topic limits.
//!
//! Streams (checked live 2026-10-12 and 2026-10-14):
//! - Bybit `wss://stream.bybit.com/v5/public/linear`, `tickers.<SYM>`: last,
//!   mark and 24 h turnover (deltas carry only the fields that changed; max
//!   10 args per request).
//! - KuCoin: token from `POST /api/v1/bullet-public`, wait for `welcome`,
//!   then `/contractMarket/execution:<SYM>` (trades) and
//!   `/contract/instrument:<SYM>` (mark price). Tickers send nothing.
//! - Aster `wss://fstream.asterdex.com/ws`, `<sym>@aggTrade` (its mini
//!   ticker only pushes every 3-4 s); mark prices come from the all-market
//!   `!markPrice@arr@1s` stream in `binance.rs`.
//! - Hyperliquid `wss://api.hyperliquid.xyz/ws`, `trades` and
//!   `activeAssetCtx` (mark) per coin, one subscription per message.
//!
//! The main price (`PriceUpdate::price`) is the last trade, as before.
//!
//! Spot (Binance, Bybit, Gate, KuCoin) streams every USDT pair the same way
//! (since 2026-10-15; we build spot candles too, for spot-only tokens now
//! and spot trading later). Spot is never used for order sizing. Streams: Binance `wss://stream.binance.com:9443/ws` `<sym>@miniTicker`;
//! Bybit `/v5/public/spot` `tickers.<SYM>`; Gate `wss://api.gateio.ws/ws/v4/`
//! `spot.tickers`; KuCoin spot bullet token, `/market/ticker:<BASE-USDT>`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use chrono::Utc;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

use super::markets::{self, MarketInfo, Registry, VenueMarkets};
use super::{num, Alive, Reconnect, Sink};
use crate::model::{rate, unix_ms, Funding, PriceUpdate};

const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const APP_PING_EVERY: Duration = Duration::from_secs(15);
/// An on-demand (spot) connection with nothing to watch closes after this.
const LINGER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Venue {
    Bybit,
    Kucoin,
    Aster,
    Hyperliquid,
    BinanceSpot,
    BybitSpot,
    GateSpot,
    KucoinSpot,
}

pub fn usdt(symbol: &str) -> String {
    let s = symbol.to_uppercase();
    if s.ends_with("USDT") {
        s
    } else {
        format!("{s}USDT")
    }
}

/// One market a connection streams: venue id -> our symbol and lot size.
#[derive(Debug, Clone, PartialEq)]
pub struct Sub {
    pub symbol: String,
    pub multiplier: f64,
}

pub type ShardSet = BTreeMap<String, Sub>; // venue symbol -> market

/// What one frame said about one market.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tick {
    pub venue_symbol: String,
    pub last: Option<f64>,
    pub mark: Option<f64>,
    /// Traded size in contracts (times the lot size = base units).
    pub size: Option<f64>,
    pub quote_volume_24h: Option<f64>,
    /// Rolling 24 h high/low of the last price (ticker feeds only).
    pub high_24h: Option<f64>,
    pub low_24h: Option<f64>,
    pub funding: Option<Funding>,
}

fn funding(rate: Option<f64>, next_ms: Option<i64>) -> Option<Funding> {
    let f = Funding {
        rate,
        next_ms,
        interval_hours: None,
    };
    (!f.is_empty()).then_some(f)
}

impl Venue {
    pub const ALL: [Venue; 8] = [
        Venue::Bybit,
        Venue::Kucoin,
        Venue::Aster,
        Venue::Hyperliquid,
        Venue::BinanceSpot,
        Venue::BybitSpot,
        Venue::GateSpot,
        Venue::KucoinSpot,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Venue::Bybit => "bybit_futures",
            Venue::Kucoin => "kucoin_futures",
            Venue::Aster => "aster_futures",
            Venue::Hyperliquid => "hyperliquid",
            Venue::BinanceSpot => "binance_spot",
            Venue::BybitSpot => "bybit_spot",
            Venue::GateSpot => "gate_spot",
            Venue::KucoinSpot => "kucoin_spot",
        }
    }

    fn is_kucoin(self) -> bool {
        matches!(self, Venue::Kucoin | Venue::KucoinSpot)
    }

    pub fn from_name(name: &str) -> Option<Venue> {
        Venue::ALL.into_iter().find(|v| v.name() == name)
    }

    /// Connections per venue. Bybit ~800 markets (args limit), KuCoin ~690
    /// with two topics each (400 topics per connection), Aster ~580
    /// (200 streams per connection), Hyperliquid ~180 with two
    /// subscriptions each.
    pub fn shards(self) -> usize {
        match self {
            Venue::Bybit => 4,
            Venue::Kucoin => 5,
            Venue::Aster => 4,
            Venue::Hyperliquid => 2,
            // Spot: Binance ~510 pairs (5 messages/s, 1024 streams per
            // connection), Bybit ~390, Gate ~2050, KuCoin ~810 (400 topics).
            Venue::BinanceSpot => 2,
            Venue::BybitSpot => 2,
            Venue::GateSpot => 5,
            Venue::KucoinSpot => 3,
        }
    }

    /// Our symbol (XUSDT) to the venue's, for a watched symbol the contract
    /// list does not have (yet).
    pub fn venue_symbol(self, symbol: &str) -> String {
        let s = usdt(symbol);
        match self {
            Venue::Kucoin => {
                let base = &s[..s.len() - 4];
                format!("{}USDTM", if base == "BTC" { "XBT" } else { base })
            }
            Venue::Hyperliquid => s[..s.len() - 4].to_string(),
            Venue::GateSpot => format!("{}_USDT", &s[..s.len() - 4]),
            Venue::KucoinSpot => format!("{}-USDT", &s[..s.len() - 4]),
            _ => s,
        }
    }

    pub fn subscription(self, syms: &[String], subscribe: bool, msg_id: &mut u64) -> Vec<String> {
        if syms.is_empty() {
            return Vec::new();
        }
        match self {
            Venue::Bybit | Venue::BybitSpot => syms
                .chunks(10)
                .map(|c| {
                    json!({"op": if subscribe {"subscribe"} else {"unsubscribe"},
                           "args": c.iter().map(|s| format!("tickers.{s}")).collect::<Vec<_>>()})
                    .to_string()
                })
                .collect(),
            Venue::Kucoin => {
                let mut out = Vec::new();
                for topic in ["/contractMarket/execution", "/contract/instrument"] {
                    for c in syms.chunks(90) {
                        *msg_id += 1;
                        out.push(
                            json!({"id": msg_id.to_string(), "type": if subscribe {"subscribe"} else {"unsubscribe"},
                                   "topic": format!("{topic}:{}", c.join(",")),
                                   "privateChannel": false, "response": true})
                            .to_string(),
                        );
                    }
                }
                out
            }
            Venue::GateSpot => syms
                .chunks(50)
                .map(|c| {
                    json!({"time": Utc::now().timestamp(), "channel": "spot.tickers",
                           "event": if subscribe {"subscribe"} else {"unsubscribe"}, "payload": c})
                    .to_string()
                })
                .collect(),
            Venue::KucoinSpot => syms
                .chunks(90)
                .map(|c| {
                    *msg_id += 1;
                    json!({"id": msg_id.to_string(), "type": if subscribe {"subscribe"} else {"unsubscribe"},
                           "topic": format!("/market/ticker:{}", c.join(",")),
                           "privateChannel": false, "response": true})
                    .to_string()
                })
                .collect(),
            Venue::Aster | Venue::BinanceSpot => syms
                .chunks(100)
                .map(|c| {
                    *msg_id += 1;
                    // Binance spot: the per-symbol mini ticker pushes every
                    // second, so a quiet pair still gets a first price fast.
                    let stream = if self == Venue::Aster { "aggTrade" } else { "miniTicker" };
                    json!({"method": if subscribe {"SUBSCRIBE"} else {"UNSUBSCRIBE"},
                           "params": c.iter().map(|s| format!("{}@{stream}", s.to_lowercase())).collect::<Vec<_>>(),
                           "id": *msg_id})
                    .to_string()
                })
                .collect(),
            Venue::Hyperliquid => syms
                .iter()
                .flat_map(|c| {
                    ["trades", "activeAssetCtx"].map(|t| {
                        json!({"method": if subscribe {"subscribe"} else {"unsubscribe"},
                               "subscription": {"type": t, "coin": c}})
                        .to_string()
                    })
                })
                .collect(),
        }
    }

    /// What one frame says. Venue symbols come back as the venue sends them
    /// (Hyperliquid coins keep their case).
    pub fn parse(self, text: &str) -> Vec<Tick> {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return Vec::new();
        };
        let s = |x: Option<&Value>| x.and_then(Value::as_str).map(str::to_string);
        match self {
            Venue::Bybit | Venue::BybitSpot => {
                if !v
                    .get("topic")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .starts_with("tickers.")
                {
                    return Vec::new();
                }
                let Some(d) = v.get("data") else {
                    return Vec::new();
                };
                let Some(sym) = s(d.get("symbol")) else {
                    return Vec::new();
                };
                let t = Tick {
                    venue_symbol: sym.to_uppercase(),
                    last: num(d.get("lastPrice")),
                    mark: num(d.get("markPrice")),
                    size: None,
                    quote_volume_24h: num(d.get("turnover24h")),
                    high_24h: num(d.get("highPrice24h")),
                    low_24h: num(d.get("lowPrice24h")),
                    funding: funding(
                        rate(d.get("fundingRate")),
                        unix_ms(d.get("nextFundingTime")),
                    ),
                };
                if t.last.is_none() && t.mark.is_none() && t.funding.is_none() {
                    return Vec::new();
                }
                vec![t]
            }
            Venue::Kucoin => match v.get("subject").and_then(Value::as_str) {
                Some("match") => {
                    let d = v.get("data");
                    match (
                        s(d.and_then(|d| d.get("symbol"))),
                        num(d.and_then(|d| d.get("price"))),
                    ) {
                        (Some(sym), Some(p)) => vec![Tick {
                            venue_symbol: sym.to_uppercase(),
                            last: Some(p),
                            size: num(d.and_then(|d| d.get("size"))),
                            ..Tick::default()
                        }],
                        _ => Vec::new(),
                    }
                }
                Some("mark.index.price") => {
                    let sym = v
                        .get("topic")
                        .and_then(Value::as_str)
                        .and_then(|t| t.rsplit_once(':'))
                        .map(|(_, s)| s.to_uppercase());
                    match (sym, num(v.pointer("/data/markPrice"))) {
                        (Some(sym), Some(m)) => vec![Tick {
                            venue_symbol: sym,
                            mark: Some(m),
                            ..Tick::default()
                        }],
                        _ => Vec::new(),
                    }
                }
                // Same topic, about once a minute: the predicted rate.
                Some("funding.rate") => {
                    let sym = v
                        .get("topic")
                        .and_then(Value::as_str)
                        .and_then(|t| t.rsplit_once(':'))
                        .map(|(_, s)| s.to_uppercase());
                    match (sym, rate(v.pointer("/data/fundingRate"))) {
                        (Some(sym), Some(r)) => vec![Tick {
                            venue_symbol: sym,
                            funding: funding(Some(r), None),
                            ..Tick::default()
                        }],
                        _ => Vec::new(),
                    }
                }
                _ => Vec::new(),
            },
            Venue::GateSpot => {
                if v.get("channel").and_then(Value::as_str) != Some("spot.tickers")
                    || v.get("event").and_then(Value::as_str) != Some("update")
                {
                    return Vec::new();
                }
                let r = v.get("result");
                match (
                    s(r.and_then(|r| r.get("currency_pair"))),
                    num(r.and_then(|r| r.get("last"))),
                ) {
                    (Some(sym), Some(p)) => vec![Tick {
                        venue_symbol: sym.to_uppercase(),
                        last: Some(p),
                        quote_volume_24h: num(r.and_then(|r| r.get("quote_volume"))),
                        high_24h: num(r.and_then(|r| r.get("high_24h"))),
                        low_24h: num(r.and_then(|r| r.get("low_24h"))),
                        ..Tick::default()
                    }],
                    _ => Vec::new(),
                }
            }
            Venue::KucoinSpot => {
                if v.get("subject").and_then(Value::as_str) != Some("trade.ticker") {
                    return Vec::new();
                }
                let sym = v
                    .get("topic")
                    .and_then(Value::as_str)
                    .and_then(|t| t.rsplit_once(':'))
                    .map(|(_, s)| s.to_uppercase());
                match (sym, num(v.pointer("/data/price"))) {
                    (Some(sym), Some(p)) => vec![Tick {
                        venue_symbol: sym,
                        last: Some(p),
                        size: num(v.pointer("/data/size")),
                        ..Tick::default()
                    }],
                    _ => Vec::new(),
                }
            }
            Venue::BinanceSpot => {
                if v.get("e").and_then(Value::as_str) != Some("24hrMiniTicker") {
                    return Vec::new();
                }
                match (s(v.get("s")), num(v.get("c"))) {
                    (Some(sym), Some(p)) => vec![Tick {
                        venue_symbol: sym.to_uppercase(),
                        last: Some(p),
                        quote_volume_24h: num(v.get("q")),
                        high_24h: num(v.get("h")),
                        low_24h: num(v.get("l")),
                        ..Tick::default()
                    }],
                    _ => Vec::new(),
                }
            }
            Venue::Aster => {
                if v.get("e").and_then(Value::as_str) != Some("aggTrade") {
                    return Vec::new();
                }
                match (s(v.get("s")), num(v.get("p"))) {
                    (Some(sym), Some(p)) => vec![Tick {
                        venue_symbol: sym.to_uppercase(),
                        last: Some(p),
                        size: num(v.get("q")),
                        ..Tick::default()
                    }],
                    _ => Vec::new(),
                }
            }
            Venue::Hyperliquid => match v.get("channel").and_then(Value::as_str) {
                Some("trades") => v
                    .get("data")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|t| {
                        Some(Tick {
                            venue_symbol: s(t.get("coin"))?,
                            last: Some(num(t.get("px"))?),
                            size: num(t.get("sz")),
                            ..Tick::default()
                        })
                    })
                    .collect(),
                Some("activeAssetCtx") => {
                    match (
                        s(v.pointer("/data/coin")),
                        num(v.pointer("/data/ctx/markPx")),
                    ) {
                        (Some(coin), Some(m)) => vec![Tick {
                            venue_symbol: coin,
                            mark: Some(m),
                            // Hourly rate; paid on the hour.
                            funding: funding(rate(v.pointer("/data/ctx/funding")), None),
                            ..Tick::default()
                        }],
                        _ => Vec::new(),
                    }
                }
                _ => Vec::new(),
            },
        }
    }

    fn ping_message(self, msg_id: &mut u64) -> Option<String> {
        match self {
            Venue::Bybit | Venue::BybitSpot => Some(json!({"op": "ping"}).to_string()),
            Venue::Kucoin | Venue::KucoinSpot => {
                *msg_id += 1;
                Some(json!({"id": msg_id.to_string(), "type": "ping"}).to_string())
            }
            Venue::GateSpot => {
                Some(json!({"time": Utc::now().timestamp(), "channel": "spot.ping"}).to_string())
            }
            Venue::Aster | Venue::BinanceSpot => None,
            Venue::Hyperliquid => Some(json!({"method": "ping"}).to_string()),
        }
    }

    /// Pause between subscription messages (Hyperliquid sends one per
    /// subscription; KuCoin allows 100 messages per 10 s).
    fn send_gap(self) -> Duration {
        match self {
            Venue::Hyperliquid => Duration::from_millis(15),
            Venue::Kucoin | Venue::KucoinSpot => Duration::from_millis(120),
            // Binance spot accepts 5 incoming messages per second.
            Venue::BinanceSpot => Duration::from_millis(250),
            Venue::GateSpot => Duration::from_millis(50),
            _ => Duration::from_millis(5),
        }
    }

    async fn ws_url(self, http: &reqwest::Client) -> Result<String, String> {
        match self {
            Venue::Bybit => Ok("wss://stream.bybit.com/v5/public/linear".into()),
            Venue::Aster => Ok("wss://fstream.asterdex.com/ws".into()),
            Venue::Hyperliquid => Ok("wss://api.hyperliquid.xyz/ws".into()),
            Venue::BinanceSpot => Ok("wss://stream.binance.com:9443/ws".into()),
            Venue::BybitSpot => Ok("wss://stream.bybit.com/v5/public/spot".into()),
            Venue::GateSpot => Ok("wss://api.gateio.ws/ws/v4/".into()),
            Venue::Kucoin | Venue::KucoinSpot => {
                let bullet = if self == Venue::Kucoin {
                    "https://api-futures.kucoin.com/api/v1/bullet-public"
                } else {
                    "https://api.kucoin.com/api/v1/bullet-public"
                };
                let body: Value = http
                    .post(bullet)
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
                    Utc::now().timestamp_nanos_opt().unwrap_or_default()
                ))
            }
        }
    }

    /// Price updates for the ticks of markets this connection streams.
    pub fn updates(self, ticks: Vec<Tick>, subs: &ShardSet) -> Vec<PriceUpdate> {
        let now = Utc::now();
        let mut out = Vec::new();
        for t in ticks {
            let key = if self == Venue::Hyperliquid {
                t.venue_symbol.clone()
            } else {
                t.venue_symbol.to_uppercase()
            };
            let Some(sub) = subs.get(&key) else {
                continue;
            };
            let u = match t.last {
                Some(last) => {
                    let mut u = PriceUpdate::last_trade(sub.symbol.clone(), self.name(), last, now);
                    u.mark = t.mark;
                    u.quote_volume_24h = t.quote_volume_24h;
                    u.volume = t.size.map(|q| q * sub.multiplier * last);
                    u.high_24h = t.high_24h;
                    u.low_24h = t.low_24h;
                    u
                }
                None => match (t.mark, t.funding) {
                    (Some(m), _) => PriceUpdate::mark_only(sub.symbol.clone(), self.name(), m, now),
                    (None, Some(f)) => {
                        PriceUpdate::funding_only(sub.symbol.clone(), self.name(), f, now)
                    }
                    (None, None) => continue,
                },
            };
            let mut u = u;
            u.funding = t.funding;
            out.push(u);
        }
        out
    }
}

/// Stable shard for a symbol (FNV-1a), so a refresh never reshuffles.
pub fn shard_of(symbol: &str, shards: usize) -> usize {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in symbol.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h % shards as u64) as usize
}

/// Split listed and watched markets over the venue's connections.
pub fn split(venue: Venue, listed: &VenueMarkets, watched: &BTreeSet<String>) -> Vec<ShardSet> {
    let n = venue.shards();
    let mut out = vec![ShardSet::new(); n];
    let mut add = |symbol: &str, info: MarketInfo| {
        let key = if venue == Venue::Hyperliquid {
            info.venue_symbol.clone()
        } else {
            info.venue_symbol.to_uppercase()
        };
        out[shard_of(symbol, n)].insert(
            key,
            Sub {
                symbol: symbol.to_string(),
                multiplier: info.multiplier,
            },
        );
    };
    for (sym, info) in listed {
        add(sym, info.clone());
    }
    for sym in watched {
        if !listed.contains_key(sym) {
            add(
                sym,
                MarketInfo {
                    venue_symbol: venue.venue_symbol(sym),
                    multiplier: 1.0,
                    funding: None,
                },
            );
        }
    }
    out
}

/// Runs one venue: keeps its market list fresh and its connections fed.
pub async fn run(
    venue: Venue,
    sink: Sink,
    alive: Vec<Alive>,
    registry: Registry,
    mut watched: watch::Receiver<BTreeSet<String>>,
) {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("http client");
    let mut shard_tx = Vec::new();
    for (i, a) in alive.into_iter().enumerate() {
        let (tx, rx) = watch::channel(ShardSet::new());
        shard_tx.push(tx);
        tokio::spawn(run_shard(venue, i, sink.clone(), a, rx));
    }
    let mut refresh = tokio::time::interval(markets::REFRESH);
    loop {
        tokio::select! {
            _ = refresh.tick() => {
                match markets::fetch(&http, venue.name()).await {
                    Ok(m) => {
                        let before = registry.count(venue.name());
                        if before != m.len() {
                            tracing::info!("{}: {} market(s) listed", venue.name(), m.len());
                        }
                        registry.set(venue.name(), m);
                    }
                    Err(e) => {
                        tracing::warn!("{}: market list failed: {e}", venue.name());
                        if registry.count(venue.name()) == 0 {
                            // Nothing yet: ask again soon instead of in 5 minutes.
                            refresh.reset_after(Duration::from_secs(30));
                        }
                    }
                }
            }
            changed = watched.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
        if sink.is_closed() {
            return;
        }
        let sets = split(
            venue,
            &registry.venue(venue.name()),
            &watched.borrow_and_update(),
        );
        for (tx, set) in shard_tx.iter().zip(sets) {
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
}

/// One connection of a venue, for as long as the sink lives.
async fn run_shard(
    venue: Venue,
    index: usize,
    sink: Sink,
    alive: Alive,
    mut wanted: watch::Receiver<ShardSet>,
) {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("http client");
    let label = format!("{} #{}", venue.name(), index + 1);
    let mut pace = Reconnect::new(venue.name());
    loop {
        if wanted.borrow_and_update().is_empty() {
            alive.set(false);
            if wanted.changed().await.is_err() {
                return;
            }
            continue;
        }
        match session(venue, &label, &sink, &alive, &mut wanted, &http).await {
            // Idle (nothing watched for a minute), or shutting down.
            Ok(()) => {
                alive.set(false);
                if sink.is_closed() || wanted.has_changed().is_err() {
                    return;
                }
            }
            Err((got, e)) => {
                alive.set(false);
                if sink.is_closed() {
                    return;
                }
                tracing::warn!("{label} price stream: {e}");
                let wait = pace.after(got);
                tracing::info!("{label}: reconnecting in {}s", wait.as_secs());
                tokio::time::sleep(wait).await;
            }
        }
    }
}

async fn session(
    venue: Venue,
    label: &str,
    sink: &Sink,
    alive: &Alive,
    wanted: &mut watch::Receiver<ShardSet>,
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
    if venue.is_kucoin() {
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
    tracing::info!("{label} price stream connected");

    let mut msg_id = 0u64;
    let mut subscribed = ShardSet::new();
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
            let want = wanted.borrow_and_update().clone();
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
            let gap = venue.send_gap();
            for m in venue
                .subscription(&drop, false, &mut msg_id)
                .into_iter()
                .chain(venue.subscription(&add, true, &mut msg_id))
            {
                tx.send(Message::text(m))
                    .await
                    .map_err(|e| send_err(e, got))?;
                tokio::time::sleep(gap).await;
            }
            if !add.is_empty() || !drop.is_empty() {
                tracing::info!(
                    "{label}: streaming {} market(s) (+{} -{})",
                    want.len(),
                    add.len(),
                    drop.len()
                );
            }
            subscribed = want;
            alive.set(!subscribed.is_empty());
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
                        let updates = venue.updates(venue.parse(t.as_str()), &subscribed);
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
                    tracing::info!("{label} price stream closed (nothing to watch)");
                    return Ok(());
                }
                if empty_since.is_none() && last_frame.elapsed() > IDLE_TIMEOUT {
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
        let b = Venue::Bybit.parse(
            r#"{"topic":"tickers.BTCUSDT","data":{"symbol":"BTCUSDT","lastPrice":"83124.5","markPrice":"83120","turnover24h":"1000"}}"#,
        );
        assert_eq!(b[0].last, Some(83124.5));
        assert_eq!(b[0].mark, Some(83120.0));
        assert_eq!(b[0].quote_volume_24h, Some(1000.0));
        // A delta with only the mark.
        let b = Venue::Bybit
            .parse(r#"{"topic":"tickers.BTCUSDT","data":{"symbol":"BTCUSDT","markPrice":"5"}}"#);
        assert_eq!((b[0].last, b[0].mark), (None, Some(5.0)));
        assert!(Venue::Bybit
            .parse(r#"{"topic":"tickers.BTCUSDT","data":{"symbol":"BTCUSDT"}}"#)
            .is_empty());
        let b = Venue::Bybit.parse(
            r#"{"topic":"tickers.BTCUSDT","data":{"symbol":"BTCUSDT","fundingRate":"-0.0005","nextFundingTime":"1791820800000"}}"#,
        );
        let f = b[0].funding.unwrap();
        assert_eq!((f.rate, f.next_ms), (Some(-0.0005), Some(1791820800000)));
        let k = Venue::Kucoin.parse(
            r#"{"topic":"/contract/instrument:XBTUSDTM","subject":"funding.rate","data":{"granularity":60000,"fundingRate":0.0001,"timestamp":1}}"#,
        );
        assert_eq!(k[0].funding.unwrap().rate, Some(0.0001));

        let k = Venue::Kucoin
            .parse(r#"{"subject":"match","data":{"symbol":"XBTUSDTM","price":"100.5","size":3}}"#);
        assert_eq!(
            (k[0].venue_symbol.as_str(), k[0].last, k[0].size),
            ("XBTUSDTM", Some(100.5), Some(3.0))
        );
        let k = Venue::Kucoin.parse(
            r#"{"topic":"/contract/instrument:XBTUSDTM","subject":"mark.index.price","data":{"markPrice":101.25,"indexPrice":101.2}}"#,
        );
        assert_eq!(
            (k[0].venue_symbol.as_str(), k[0].mark),
            ("XBTUSDTM", Some(101.25))
        );

        let a = Venue::Aster.parse(r#"{"e":"aggTrade","s":"btcusdt","p":"7","q":"2"}"#);
        assert_eq!(
            (a[0].venue_symbol.as_str(), a[0].last, a[0].size),
            ("BTCUSDT", Some(7.0), Some(2.0))
        );

        let h = Venue::Hyperliquid.parse(
            r#"{"channel":"trades","data":[{"coin":"kPEPE","px":"2.5","sz":"4"},{"coin":"X","px":"0"}]}"#,
        );
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].venue_symbol, "kPEPE");
        let h = Venue::Hyperliquid.parse(
            r#"{"channel":"activeAssetCtx","data":{"coin":"BTC","ctx":{"markPx":"64000.5","midPx":"64001","funding":"0.0000125"}}}"#,
        );
        assert_eq!(h[0].mark, Some(64000.5));
        assert_eq!(h[0].funding.unwrap().rate, Some(0.0000125));
    }

    #[test]
    fn updates_keep_last_as_main_price() {
        let mut subs = ShardSet::new();
        subs.insert(
            "XBTUSDTM".into(),
            Sub {
                symbol: "BTCUSDT".into(),
                multiplier: 0.001,
            },
        );
        let u = Venue::Kucoin.updates(
            vec![
                Tick {
                    venue_symbol: "XBTUSDTM".into(),
                    last: Some(100.0),
                    size: Some(5.0),
                    ..Tick::default()
                },
                Tick {
                    venue_symbol: "XBTUSDTM".into(),
                    mark: Some(99.0),
                    ..Tick::default()
                },
                Tick {
                    venue_symbol: "OTHERUSDTM".into(),
                    last: Some(1.0),
                    ..Tick::default()
                },
            ],
            &subs,
        );
        assert_eq!(u.len(), 2);
        assert!(u[0].primary);
        assert_eq!((u[0].symbol.as_str(), u[0].price), ("BTCUSDT", 100.0));
        assert!((u[0].volume.unwrap() - 0.5).abs() < 1e-9);
        assert!(!u[1].primary);
        assert_eq!(u[1].mark, Some(99.0));

        let f = Venue::Kucoin.updates(
            vec![Tick {
                venue_symbol: "XBTUSDTM".into(),
                funding: funding(Some(0.0002), None),
                ..Tick::default()
            }],
            &subs,
        );
        assert_eq!(f.len(), 1);
        assert!(!f[0].primary && f[0].mark.is_none() && f[0].last.is_none());
        assert_eq!(f[0].funding.unwrap().rate, Some(0.0002));
    }

    #[test]
    fn parses_spot() {
        let g = Venue::GateSpot.parse(
            r#"{"time":1,"channel":"spot.tickers","event":"update","result":{"currency_pair":"BTC_USDT","last":"64000.1","quote_volume":"5"}}"#,
        );
        assert_eq!(
            (g[0].venue_symbol.as_str(), g[0].last),
            ("BTC_USDT", Some(64000.1))
        );
        let k = Venue::KucoinSpot.parse(
            r#"{"type":"message","topic":"/market/ticker:BTC-USDT","subject":"trade.ticker","data":{"price":"64001","size":"0.01"}}"#,
        );
        assert_eq!(
            (k[0].venue_symbol.as_str(), k[0].last),
            ("BTC-USDT", Some(64001.0))
        );
        let b = Venue::BinanceSpot
            .parse(r#"{"e":"24hrMiniTicker","s":"BTCUSDT","c":"64002","q":"1000"}"#);
        assert_eq!(b[0].last, Some(64002.0));
        assert_eq!(Venue::GateSpot.venue_symbol("btc"), "BTC_USDT");
        assert_eq!(Venue::KucoinSpot.venue_symbol("BTCUSDT"), "BTC-USDT");
        let mut id = 0;
        assert!(
            Venue::GateSpot.subscription(&["BTC_USDT".into()], true, &mut id)[0]
                .contains(r#""channel":"spot.tickers""#)
        );
    }

    #[test]
    fn spot_streams_every_listed_market() {
        let mut listed = VenueMarkets::new();
        for s in ["BTCUSDT", "ETHUSDT"] {
            listed.insert(
                s.into(),
                MarketInfo {
                    venue_symbol: format!("{}_USDT", &s[..3]),
                    multiplier: 1.0,
                    funding: None,
                },
            );
        }
        let all: BTreeMap<String, Sub> = split(Venue::GateSpot, &listed, &BTreeSet::new())
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(all.keys().collect::<Vec<_>>(), vec!["BTC_USDT", "ETH_USDT"]);
    }

    #[test]
    fn split_is_stable_and_adds_watched() {
        let mut listed = VenueMarkets::new();
        for s in ["BTCUSDT", "ETHUSDT", "SOLUSDT", "KPEPEUSDT"] {
            listed.insert(
                s.into(),
                MarketInfo {
                    venue_symbol: if s == "KPEPEUSDT" {
                        "kPEPE".into()
                    } else {
                        s[..s.len() - 4].into()
                    },
                    multiplier: 1.0,
                    funding: None,
                },
            );
        }
        let watched: BTreeSet<String> = ["NEWUSDT".to_string()].into();
        let sets = split(Venue::Hyperliquid, &listed, &watched);
        assert_eq!(sets.len(), Venue::Hyperliquid.shards());
        let all: BTreeMap<String, Sub> = sets.into_iter().flatten().collect();
        assert_eq!(all.len(), 5);
        assert_eq!(all["kPEPE"].symbol, "KPEPEUSDT");
        assert_eq!(all["NEW"].symbol, "NEWUSDT");
        assert_eq!(shard_of("BTCUSDT", 4), shard_of("BTCUSDT", 4));
    }

    #[test]
    fn subscription_messages() {
        let mut id = 0;
        let syms: Vec<String> = (0..12).map(|i| format!("S{i}USDT")).collect();
        assert_eq!(Venue::Bybit.subscription(&syms, true, &mut id).len(), 2);
        let k = Venue::Kucoin.subscription(&syms[..2], false, &mut id);
        assert_eq!(k.len(), 2);
        assert!(k[0].contains(r#""topic":"/contractMarket/execution:S0USDT,S1USDT""#));
        assert!(k[1].contains(r#""topic":"/contract/instrument:S0USDT,S1USDT""#));
        assert!(k[0].contains(r#""type":"unsubscribe""#));
        let a = Venue::Aster.subscription(&syms[..1], true, &mut id);
        assert!(a[0].contains("s0usdt@aggTrade"));
        assert_eq!(
            Venue::Hyperliquid
                .subscription(&syms[..2], true, &mut id)
                .len(),
            4
        );
        assert!(Venue::Hyperliquid
            .subscription(&[], true, &mut id)
            .is_empty());
    }
}
