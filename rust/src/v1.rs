//! Read-only /v1 endpoints for the Trigger api and apps.
//!
//! - `GET /v1/markets[?exchange=…][&symbol=…]`: listed markets per venue;
//!   without `symbol`, also `volume_24h` per token (futures, USDT).
//! - `GET /v1/prices?m=bybit_futures:BTCUSDT,…` or `?symbol=BTCUSDT`: last
//!   and mark price with their age (up to 200 markets). Futures venues come
//!   first; spot venues (`*_spot`, last price only, also with candles) are
//!   the fallback for a token with no futures market.
//! - `GET /v1/candles?exchange&symbol&interval=1m&limit=300[&end=<unix s>]`:
//!   our candles, gaps filled from the exchange's history (at most once a
//!   minute per chart), plus the open candle.
//! - `GET /v1/stream` (WebSocket): prices and live candles, pushed at most
//!   four times a second per market, only when something changed.
//! - `GET /v1/marks` (WebSocket, server key only): one reference price per
//!   token for the order services, pushed when it moved past a threshold;
//!   see marks.rs.
//!
//! Callers: the server API key (`x-api-key`), or a browser token from the
//! Trigger api (`Authorization: Bearer`, or `?token=` on the WebSocket; see
//! token.rs). Browsers must come from CORS_ORIGINS. Every IP is rate limited
//! (requests and open sockets) and a socket holds at most 300
//! subscriptions. There is no fallback: a market we cannot price is
//! reported as missing, never fetched live from an exchange.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::app::App;
use crate::backfill;
use crate::feeds::venues::{usdt, Venue};
use crate::feeds::EXCHANGES;
use crate::hub::{Latest, Quote};
use crate::model::Interval;
use crate::store::{retention, Bar};
use crate::token;

pub const MAX_LIMIT: i64 = 1000;
const MAX_SUBS: usize = 300;
const MAX_PRICES: usize = 200;
const PUSH_EVERY: Duration = Duration::from_millis(250);
const BACKFILL_EVERY: Duration = Duration::from_secs(60);
/// How long a chart request waits for one of the backfill slots.
const BACKFILL_WAIT: Duration = Duration::from_secs(10);
/// A chart the exchange could not fill is not asked again for about this
/// long (stored as a future time in `backfilled`, kept until a minute after).
const QUIET_BACKFILL: Duration = Duration::from_secs(29 * 60);
/// Browser limits per signed-in user: requests per second, burst, open
/// sockets. Per IP the limits are much looser, because phones on one mobile
/// carrier often share an address (carrier NAT).
const USER_RATE: f64 = 10.0;
const USER_BURST: f64 = 40.0;
const SOCKETS_PER_USER: usize = 5;
const IP_RATE: f64 = 100.0;
const IP_BURST: f64 = 400.0;
const SOCKETS_PER_IP: usize = 200;
/// The server pings every socket this often and drops one silent for
/// `IDLE_DROP` (a phone that went offline frees its slot).
const SERVER_PING: Duration = Duration::from_secs(20);
const IDLE_DROP: Duration = Duration::from_secs(60);

type AppState = State<Arc<App>>;

// --- callers and limits ---

#[derive(Debug, Clone, PartialEq)]
pub enum Caller {
    /// The server API key: no rate limit.
    Server,
    /// A browser token for user `sub` until `exp`.
    User { sub: String, exp: i64 },
}

fn err(status: StatusCode, detail: impl Into<String>) -> Response {
    (status, Json(json!({"detail": detail.into()}))).into_response()
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

pub fn caller(
    app: &App,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Result<Caller, &'static str> {
    if let (Some(k), Some(given)) = (
        &app.cfg.api_key,
        headers.get("x-api-key").and_then(|v| v.to_str().ok()),
    ) {
        if crate::http::eq(given, k) {
            return Ok(Caller::Server);
        }
        return Err("invalid api key");
    }
    if app.cfg.api_key.is_none() && headers.get("x-api-key").is_some() {
        // Local runs without a key.
        return Ok(Caller::Server);
    }
    let Some(t) = bearer(headers).or(query_token) else {
        return Err("token required");
    };
    let Some(secret) = &app.cfg.prices_token_secret else {
        return Err("browser tokens are not enabled");
    };
    token::verify(secret, t, Utc::now().timestamp()).map(|c| Caller::User {
        sub: c.sub,
        exp: c.exp,
    })
}

/// Client IP: the last X-Forwarded-For entry (added by our Caddy), else
/// "direct".
pub fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "direct".into())
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

/// Token buckets and socket counts per key (`u:<user>` or `ip:<address>`).
#[derive(Default)]
pub struct Limits {
    buckets: Mutex<HashMap<String, Bucket>>,
    sockets: Mutex<HashMap<String, usize>>,
}

impl Limits {
    pub fn allow(&self, key: &str, rate: f64, burst: f64, now: Instant) -> bool {
        let mut b = self.buckets.lock();
        if b.len() > 50_000 {
            b.retain(|_, x| now.duration_since(x.at) < Duration::from_secs(60));
        }
        let x = b.entry(key.to_string()).or_insert(Bucket {
            tokens: burst,
            at: now,
        });
        x.tokens = (x.tokens + now.duration_since(x.at).as_secs_f64() * rate).min(burst);
        x.at = now;
        if x.tokens >= 1.0 {
            x.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    pub fn open_socket(&self, key: &str, max: usize) -> bool {
        let mut s = self.sockets.lock();
        let n = s.entry(key.to_string()).or_default();
        if *n >= max {
            return false;
        }
        *n += 1;
        true
    }

    pub fn close_socket(&self, key: &str) {
        let mut s = self.sockets.lock();
        if let Some(n) = s.get_mut(key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                s.remove(key);
            }
        }
    }

    /// A browser request: within both its user's and its address's budget.
    pub fn allow_user(&self, sub: &str, ip: &str, now: Instant) -> bool {
        self.allow(&format!("ip:{ip}"), IP_RATE, IP_BURST, now)
            && self.allow(&format!("u:{sub}"), USER_RATE, USER_BURST, now)
    }

    /// Opens a browser socket for a user and address, or neither.
    pub fn open_user_socket(&self, sub: &str, ip: &str) -> bool {
        let (u, i) = (format!("u:{sub}"), format!("ip:{ip}"));
        if !self.open_socket(&i, SOCKETS_PER_IP) {
            return false;
        }
        if !self.open_socket(&u, SOCKETS_PER_USER) {
            self.close_socket(&i);
            return false;
        }
        true
    }

    pub fn close_user_socket(&self, sub: &str, ip: &str) {
        self.close_socket(&format!("u:{sub}"));
        self.close_socket(&format!("ip:{ip}"));
    }
}

async fn guard(State(app): AppState, req: Request, next: Next) -> Response {
    if req.method() == Method::OPTIONS {
        return next.run(req).await;
    }
    let token_q = req.uri().query().and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("token="))
            .map(str::to_string)
    });
    let who = match caller(&app, req.headers(), token_q.as_deref()) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::UNAUTHORIZED, e),
    };
    if let Caller::User { sub, .. } = &who {
        if !app
            .limits
            .allow_user(sub, &client_ip(req.headers()), Instant::now())
        {
            return err(StatusCode::TOO_MANY_REQUESTS, "slow down");
        }
    }
    next.run(req).await
}

pub fn router(app: Arc<App>) -> Router<Arc<App>> {
    let origins: Vec<HeaderValue> = app
        .cfg
        .cors_origins
        .iter()
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([Method::GET])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
        .max_age(Duration::from_secs(3600));
    Router::new()
        .route("/v1/markets", get(markets))
        .route("/v1/prices", get(prices))
        .route("/v1/candles", get(candles))
        .route("/v1/stream", get(stream))
        .route("/v1/marks", get(crate::marks::handler))
        .layer(middleware::from_fn_with_state(app, guard))
        .layer(cors)
}

// --- shapes ---

fn known(ex: &str) -> bool {
    EXCHANGES.contains(&ex)
}

fn ms(t: DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

/// One market's price as /v1 serves it.
/// Prices older than this are served as missing (a halted or delisted
/// market keeps its last tick forever otherwise).
const STALE_AFTER_SECS: i64 = 600;

pub fn price_value(ex: &str, sym: &str, l: Option<Latest>, q: Quote, now: DateTime<Utc>) -> Value {
    let fresh = |t: DateTime<Utc>| (now - t).num_seconds() <= STALE_AFTER_SECS;
    let l = l.filter(|l| fresh(l.timestamp));
    let q = Quote {
        last: q.last.filter(|x| fresh(x.1)),
        mark: q.mark.filter(|x| fresh(x.1)),
    };
    let newest = [
        l.map(|l| l.timestamp),
        q.last.map(|x| x.1),
        q.mark.map(|x| x.1),
    ]
    .into_iter()
    .flatten()
    .max();
    json!({
        "exchange": ex,
        "symbol": sym,
        "market": if crate::feeds::is_spot(ex) { "spot" } else { "futures" },
        "price": l.map(|l| l.price),
        "last": q.last.map(|x| x.0),
        "mark": q.mark.map(|x| x.0),
        "ts": newest.map(ms),
        "age_seconds": newest.map(|t| ((now - t).num_milliseconds().max(0) as f64 / 100.0).round() / 10.0),
    })
}

/// "BTC" or "btcusdt" -> "BTCUSDT".
fn norm(sym: &str) -> String {
    usdt(sym.trim())
}

// --- /v1/markets ---

#[derive(Deserialize)]
struct MarketsQuery {
    exchange: Option<String>,
    symbol: Option<String>,
}

async fn markets(State(app): AppState, Query(q): Query<MarketsQuery>) -> Response {
    if let Some(sym) = q.symbol.as_deref() {
        let sym = norm(sym);
        let hub = app.hub.lock();
        let rows: Vec<Value> = app
            .markets
            .exchanges_for(&sym)
            .into_iter()
            .map(|(ex, info)| {
                let priced = hub.priced(ex, std::iter::once(&sym), 300) > 0;
                json!({"exchange": ex, "symbol": sym, "venue_symbol": info.venue_symbol, "priced": priced,
                       "market": if crate::feeds::is_spot(ex) { "spot" } else { "futures" }})
            })
            .collect();
        return Json(json!({"symbol": sym, "markets": rows})).into_response();
    }
    let list: Vec<&str> = match q.exchange.as_deref() {
        Some(e) if known(e) => vec![EXCHANGES.iter().copied().find(|x| *x == e).unwrap_or(e)],
        Some(_) => return err(StatusCode::BAD_REQUEST, "unknown exchange"),
        None => EXCHANGES.to_vec(),
    };
    let out: BTreeMap<&str, Vec<String>> = list
        .into_iter()
        .map(|ex| (ex, app.markets.venue(ex).into_keys().collect()))
        .collect();
    let volumes = futures_volumes(&app.hub.lock(), &out);
    Json(json!({"exchanges": out, "volume_24h": volumes})).into_response()
}

/// Per token, the 24 h quote volume (whole USDT) summed over the futures
/// venues in `listed` that report one, so pickers can rank tokens. Spot
/// volume is left out: trading is futures only.
fn futures_volumes(
    hub: &crate::hub::Hub,
    listed: &BTreeMap<&str, Vec<String>>,
) -> BTreeMap<String, u64> {
    let mut out: BTreeMap<String, u64> = BTreeMap::new();
    for (ex, symbols) in listed {
        if crate::feeds::is_spot(ex) {
            continue;
        }
        for sym in symbols {
            if let Some(v) = hub.volume_24h(ex, sym) {
                *out.entry(sym.clone()).or_default() += v.round() as u64;
            }
        }
    }
    out
}

// --- /v1/prices ---

#[derive(Deserialize)]
struct PricesQuery {
    m: Option<String>,
    symbol: Option<String>,
}

async fn prices(State(app): AppState, Query(q): Query<PricesQuery>) -> Response {
    let wanted: Vec<(String, String)> = if let Some(m) = q.m.as_deref() {
        let mut v = Vec::new();
        for part in m.split(',').filter(|p| !p.is_empty()) {
            let Some((ex, sym)) = part.split_once(':') else {
                return err(StatusCode::BAD_REQUEST, "m is exchange:SYMBOL,…");
            };
            if !known(ex) {
                return err(StatusCode::BAD_REQUEST, format!("unknown exchange {ex}"));
            }
            v.push((ex.to_string(), norm(sym)));
        }
        v
    } else if let Some(sym) = q.symbol.as_deref() {
        let sym = norm(sym);
        EXCHANGES
            .iter()
            .map(|e| (e.to_string(), sym.clone()))
            .collect()
    } else {
        return err(StatusCode::BAD_REQUEST, "m or symbol required");
    };
    if wanted.len() > MAX_PRICES {
        return err(
            StatusCode::BAD_REQUEST,
            format!("at most {MAX_PRICES} markets"),
        );
    }
    let only_priced = q.m.is_none();
    let now = Utc::now();
    let hub = app.hub.lock();
    let rows: Vec<Value> = wanted
        .iter()
        .filter_map(|(ex, sym)| {
            let l = hub.latest(ex, sym);
            let quote = hub.quote(ex, sym);
            if only_priced && l.is_none() && quote.is_none() {
                return None;
            }
            Some(price_value(ex, sym, l, quote.unwrap_or_default(), now))
        })
        .collect();
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"prices": rows})),
    )
        .into_response()
}

// --- /v1/candles ---

#[derive(Deserialize)]
pub struct CandlesQuery {
    pub exchange: String,
    pub symbol: String,
    #[serde(default = "one_minute")]
    pub interval: String,
    #[serde(default = "three_hundred")]
    pub limit: i64,
    pub end: Option<i64>,
    #[serde(default = "yes")]
    pub backfill: bool,
}

fn one_minute() -> String {
    "1m".into()
}
fn three_hundred() -> i64 {
    300
}
fn yes() -> bool {
    true
}

/// The venue's id for one of our symbols.
pub fn venue_symbol(app: &App, ex: &str, sym: &str) -> String {
    if let Some(i) = app.markets.get(ex, sym) {
        return i.venue_symbol;
    }
    match (ex, Venue::from_name(ex)) {
        ("gate_futures", _) => format!("{}_USDT", sym.trim_end_matches("USDT")),
        (_, Some(v)) => v.venue_symbol(sym),
        _ => sym.to_string(),
    }
}

/// Candles for a chart, oldest first, plus where they came from.
/// Our candles plus the exchange's for the same chart. The exchange's closed
/// candles win: ours can be partial (built from the first tick after a
/// restart or reconnect).
fn merge_fetched(bars: Vec<Bar>, fetched: Vec<Bar>, open_bucket: i64) -> Vec<Bar> {
    let mut by_start: BTreeMap<i64, Bar> = bars.into_iter().map(|b| (b.start, b)).collect();
    for b in fetched {
        if b.start < open_bucket {
            by_start.insert(b.start, b);
        } else {
            by_start.entry(b.start).or_insert(b);
        }
    }
    by_start.into_values().collect()
}

/// The built interval 45m, 2h and 4h are made from.
fn base_of(interval: Interval) -> Option<Interval> {
    match interval {
        Interval::M45 => Some(Interval::M15),
        Interval::H2 | Interval::H4 => Some(Interval::H1),
        _ => None,
    }
}

/// Bars of `base` into `interval` buckets; a leading bucket we only have
/// part of is dropped.
fn aggregate_bars(base: &[Bar], base_secs: i64, interval: Interval) -> Vec<Bar> {
    let mut out: Vec<Bar> = Vec::new();
    let secs = interval.seconds();
    for b in base {
        let start = b.start.div_euclid(secs) * secs;
        match out.last_mut() {
            Some(o) if o.start == start => {
                o.high = o.high.max(b.high);
                o.low = o.low.min(b.low);
                o.close = b.close;
                o.volume += b.volume;
            }
            _ => out.push(Bar { start, ..*b }),
        }
    }
    if let (Some(first), Some(b0)) = (out.first(), base.first()) {
        if b0.start > first.start && b0.start - first.start >= base_secs {
            out.remove(0);
        }
    }
    out
}

/// Candles for a chart, oldest first, plus whether the exchange filled gaps.
/// 45m, 2h and 4h (not built from ticks) come from 15m or 1h candles.
pub async fn candle_series(
    app: &Arc<App>,
    ex: &str,
    sym: &str,
    interval: Interval,
    limit: i64,
    end: Option<i64>,
    allow_backfill: bool,
) -> (Vec<Bar>, bool) {
    let Some(base) = base_of(interval) else {
        return built_series(app, ex, sym, interval, limit, end, allow_backfill).await;
    };
    let factor = interval.seconds() / base.seconds();
    let (bars, backfilled) = built_series(
        app,
        ex,
        sym,
        base,
        (limit.clamp(1, MAX_LIMIT) * factor + factor).min(MAX_LIMIT),
        end,
        allow_backfill,
    )
    .await;
    let mut out = aggregate_bars(&bars, base.seconds(), interval);
    let skip = out.len().saturating_sub(limit.clamp(1, MAX_LIMIT) as usize);
    (out.split_off(skip), backfilled)
}

async fn built_series(
    app: &Arc<App>,
    ex: &str,
    sym: &str,
    interval: Interval,
    limit: i64,
    end: Option<i64>,
    allow_backfill: bool,
) -> (Vec<Bar>, bool) {
    let now = Utc::now().timestamp();
    let limit = limit.clamp(1, MAX_LIMIT);
    let end_given = end.is_some_and(|e| e < now);
    let end = end.unwrap_or(now).min(now);
    let secs = interval.seconds();
    let built = Interval::BUILT.contains(&interval);
    let mut bars = if built {
        app.store.bars(sym, ex, interval.as_str(), end, limit).await
    } else {
        Vec::new()
    };

    let mut backfilled = false;
    let from = interval.bucket_start(DateTime::from_timestamp(end, 0).unwrap_or_default())
        - (limit - 1) * secs;
    let thin = (bars.len() as f64) < 0.9 * limit as f64;
    if allow_backfill && thin {
        let page = if end_given {
            end.div_euclid(secs * 50)
        } else {
            -1
        };
        let key = (ex.to_string(), sym.to_string(), interval.as_str(), page);
        let due = {
            let mut recent = app.backfilled.lock();
            let t = Instant::now();
            recent.retain(|_, at| t.duration_since(*at) < BACKFILL_EVERY);
            match recent.entry(key.clone()) {
                std::collections::hash_map::Entry::Occupied(_) => false,
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(t);
                    true
                }
            }
        };
        if due {
            // Wait a little for a free slot rather than serving a thin chart.
            let slot = tokio::time::timeout(BACKFILL_WAIT, app.backfill_slots.acquire()).await;
            if let Ok(Ok(_slot)) = slot {
                let vs = venue_symbol(app, ex, sym);
                match backfill::fetch(&app.http, ex, &vs, interval, from, end, limit as usize).await
                {
                    Ok(raw) if !raw.is_empty() => {
                        backfilled = true;
                        let fetched: Vec<Bar> = raw
                            .iter()
                            .map(|r| Bar {
                                start: r.start,
                                open: r.open,
                                high: r.high,
                                low: r.low,
                                close: r.close,
                                volume: r.volume,
                            })
                            .collect();
                        app.remember_fetched(key.clone(), fetched.clone());
                        let open_bucket = interval.bucket_start(Utc::now());
                        let keep_from = now - retention(interval, app.cfg.candle_retention) * secs;
                        if built {
                            let to_store: Vec<Bar> = fetched
                                .iter()
                                .copied()
                                .filter(|b| b.start >= keep_from && b.start < open_bucket)
                                .collect();
                            if !to_store.is_empty() {
                                app.store.insert_missing(sym, ex, interval, &to_store).await;
                            }
                        }
                        bars = merge_fetched(bars, fetched, open_bucket);
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("backfill {ex} {sym} {}: {e}", interval.as_str()),
                }
                // Still thin after asking: the exchange has nothing older (a
                // new listing, a gappy venue). Don't ask again for a while.
                if (bars.len() as f64) < 0.9 * limit as f64 {
                    app.backfilled
                        .lock()
                        .insert(key.clone(), Instant::now() + QUIET_BACKFILL);
                }
            } else {
                // No slot in time: let the next request for this chart try.
                app.backfilled.lock().remove(&key);
            }
        } else {
            // Asked recently (maybe by this same chart a moment ago, or still
            // in flight): serve what that fetch returned instead of nothing.
            let deadline = Instant::now() + BACKFILL_WAIT;
            loop {
                if let Some(fetched) = app.fetched_for(&key) {
                    backfilled = true;
                    let open_bucket = interval.bucket_start(Utc::now());
                    let fetched = fetched
                        .into_iter()
                        .filter(|b| b.start >= from && b.start <= end)
                        .collect();
                    bars = merge_fetched(bars, fetched, open_bucket);
                    break;
                }
                let in_flight = app.backfill_slots.available_permits() < 4;
                if !in_flight || Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }

    if built && end >= now - secs {
        if let Some(c) = app.book.lock().open_candle(ex, sym, interval) {
            let open = Bar {
                start: c.start,
                open: c.open,
                high: c.high,
                low: c.low,
                close: c.close,
                volume: c.volume,
            };
            match bars.last() {
                Some(b) if b.start == open.start => {
                    let n = bars.len();
                    bars[n - 1] = open;
                }
                Some(b) if b.start > open.start => {}
                _ => bars.push(open),
            }
        }
    }
    let skip = bars.len().saturating_sub(limit as usize);
    (bars.split_off(skip), backfilled)
}

async fn candles(State(app): AppState, Query(q): Query<CandlesQuery>) -> Response {
    if !known(&q.exchange) {
        return err(StatusCode::BAD_REQUEST, "unknown exchange");
    }
    let Some(interval) = Interval::parse(&q.interval) else {
        return err(StatusCode::BAD_REQUEST, "unknown interval");
    };
    let sym = norm(&q.symbol);
    let (bars, backfilled) = candle_series(
        &app,
        &q.exchange,
        &sym,
        interval,
        q.limit,
        q.end,
        q.backfill,
    )
    .await;
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "exchange": q.exchange,
            "symbol": sym,
            "interval": interval.as_str(),
            "backfilled": backfilled,
            "candles": bars.iter().map(Bar::json).collect::<Vec<_>>(),
        })),
    )
        .into_response()
}

// --- /v1/stream ---

#[derive(Deserialize)]
struct StreamQuery {
    token: Option<String>,
}

async fn stream(
    State(app): AppState,
    headers: HeaderMap,
    Query(q): Query<StreamQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    // Browsers always send Origin; servers and scripts usually don't.
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if !app.cfg.cors_origins.iter().any(|o| o == origin) {
            return err(StatusCode::FORBIDDEN, "origin not allowed");
        }
    }
    let who = match caller(&app, &headers, q.token.as_deref()) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::UNAUTHORIZED, e),
    };
    let ip = client_ip(&headers);
    let user = match &who {
        Caller::User { sub, .. } => Some(sub.clone()),
        Caller::Server => None,
    };
    if let Some(sub) = &user {
        if !app.limits.open_user_socket(sub, &ip) {
            return err(StatusCode::TOO_MANY_REQUESTS, "too many connections");
        }
    }
    let release = {
        let (app, ip, user) = (app.clone(), ip.clone(), user.clone());
        move || {
            if let Some(sub) = &user {
                app.limits.close_user_socket(sub, &ip);
            }
        }
    };
    let on_fail = release.clone();
    ws.on_failed_upgrade(move |_| on_fail())
        .on_upgrade(move |socket| async move {
            client(app.clone(), socket, who.clone()).await;
            release();
        })
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Sub {
    Price(String, String),
    Candle(String, String, Interval),
}

impl Sub {
    fn market(&self) -> (String, String) {
        match self {
            Sub::Price(e, s) | Sub::Candle(e, s, _) => (e.clone(), s.clone()),
        }
    }
}

/// Parse one subscribe/unsubscribe message into subscriptions.
fn parse_subs(v: &Value) -> Result<Vec<Sub>, String> {
    let channel = v.get("channel").and_then(Value::as_str).unwrap_or("prices");
    let one = |m: &Value| -> Result<(String, String), String> {
        let ex = m
            .get("exchange")
            .and_then(Value::as_str)
            .ok_or("exchange required")?;
        if !known(ex) {
            return Err(format!("unknown exchange {ex}"));
        }
        let sym = m
            .get("symbol")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or("symbol required")?;
        Ok((ex.to_string(), norm(sym)))
    };
    match channel {
        "prices" => {
            let list: Vec<&Value> = match v.get("markets").and_then(Value::as_array) {
                Some(a) => a.iter().collect(),
                None => vec![v],
            };
            list.into_iter()
                .map(|m| one(m).map(|(e, s)| Sub::Price(e, s)))
                .collect()
        }
        "candles" => {
            let (e, s) = one(v)?;
            let i = v
                .get("interval")
                .and_then(Value::as_str)
                .and_then(Interval::parse)
                .filter(|i| Interval::BUILT.contains(i))
                .ok_or("interval must be one we build")?;
            Ok(vec![Sub::Candle(e, s, i)])
        }
        other => Err(format!("unknown channel {other}")),
    }
}

async fn client(app: Arc<App>, socket: WebSocket, who: Caller) {
    use std::sync::atomic::Ordering::Relaxed;
    app.stats.stream_clients.fetch_add(1, Relaxed);
    let (mut sink, mut stream) = socket.split();
    let mut subs: HashSet<Sub> = HashSet::new();
    // What was last sent per subscription, to push only changes.
    let mut sent: HashMap<Sub, Value> = HashMap::new();
    let mut push = tokio::time::interval(PUSH_EVERY);
    let mut ping = tokio::time::interval(SERVER_PING);
    ping.reset();
    let mut heard = Instant::now();
    let server = who == Caller::Server;
    let mut out: Vec<String> = Vec::new();
    'conn: loop {
        tokio::select! {
            _ = ping.tick() => {
                if heard.elapsed() > IDLE_DROP {
                    break 'conn;
                }
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break 'conn;
                }
            }
            frame = stream.next() => {
                let Some(Ok(msg)) = frame else { break };
                heard = Instant::now();
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    out.push(json!({"type": "error", "message": "invalid json"}).to_string());
                    continue;
                };
                match v.get("op").and_then(Value::as_str).unwrap_or("") {
                    "ping" => out.push(json!({"type": "pong"}).to_string()),
                    op @ ("subscribe" | "unsubscribe") => match parse_subs(&v) {
                        Err(e) => out.push(json!({"type": "error", "message": e}).to_string()),
                        Ok(list) => {
                            let mut hub = app.hub.lock();
                            let mut done = Vec::new();
                            for s in list {
                                if op == "subscribe" {
                                    if subs.len() >= MAX_SUBS {
                                        out.push(json!({"type": "error", "message": format!("at most {MAX_SUBS} subscriptions")}).to_string());
                                        break;
                                    }
                                    // Only servers may make a venue stream a
                                    // market its list doesn't have yet;
                                    // browsers read listed markets only.
                                    if subs.insert(s.clone()) && server {
                                        hub.watch(&s.market());
                                    }
                                } else if subs.remove(&s) {
                                    if server {
                                        hub.unwatch(&s.market());
                                    }
                                    sent.remove(&s);
                                }
                                done.push(s);
                            }
                            drop(hub);
                            for s in done {
                                let (ex, sym) = s.market();
                                let listed = app.markets.get(&ex, &sym).is_some();
                                let mut m = json!({"type": if op == "subscribe" {"subscribed"} else {"unsubscribed"},
                                                   "exchange": ex, "symbol": sym, "listed": listed});
                                if let Sub::Candle(_, _, i) = &s {
                                    m["channel"] = json!("candles");
                                    m["interval"] = json!(i.as_str());
                                } else {
                                    m["channel"] = json!("prices");
                                }
                                out.push(m.to_string());
                            }
                        }
                    },
                    _ => out.push(json!({"type": "error", "message": "op must be subscribe, unsubscribe or ping"}).to_string()),
                }
            }
            _ = push.tick() => {
                if let Caller::User { exp, .. } = &who {
                    if Utc::now().timestamp() >= *exp {
                        let _ = sink.send(Message::text(json!({"type": "expired"}).to_string())).await;
                        break 'conn;
                    }
                }
                let now = Utc::now();
                {
                    let hub = app.hub.lock();
                    let book = app.book.lock();
                    for s in &subs {
                        let msg = match s {
                            Sub::Price(ex, sym) => {
                                let l = hub.latest(ex, sym);
                                let Some(q) = hub.quote(ex, sym) else { continue };
                                let mut v = price_value(ex, sym, l, q, now);
                                v["type"] = json!("price");
                                if let Some(o) = v.as_object_mut() {
                                    o.remove("age_seconds");
                                }
                                v
                            }
                            Sub::Candle(ex, sym, i) => {
                                let Some(c) = book.open_candle(ex, sym, *i) else { continue };
                                json!({"type": "candle", "exchange": ex, "symbol": sym, "interval": i.as_str(),
                                       "candle": Bar { start: c.start, open: c.open, high: c.high, low: c.low,
                                                       close: c.close, volume: c.volume }.json()})
                            }
                        };
                        if sent.get(s) != Some(&msg) {
                            out.push(msg.to_string());
                            sent.insert(s.clone(), msg);
                        }
                    }
                }
            }
        }
        for m in out.drain(..) {
            if sink.send(Message::text(m)).await.is_err() {
                break 'conn;
            }
        }
    }
    if server {
        let mut hub = app.hub.lock();
        for s in &subs {
            hub.unwatch(&s.market());
        }
    }
    app.stats.stream_clients.fetch_sub(1, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn futures_volumes_sum_futures_only() {
        let (mut hub, _rx) = crate::hub::Hub::new();
        let mut put = |ex: &'static str, sym: &str, v: f64| {
            let mut u = crate::model::PriceUpdate::last_trade(sym.into(), ex, 1.0, Utc::now());
            u.quote_volume_24h = Some(v);
            hub.publish(&u);
        };
        put("bybit_futures", "BTCUSDT", 100.4);
        put("gate_futures", "BTCUSDT", 50.0);
        put("binance_spot", "BTCUSDT", 1e9);
        let listed: BTreeMap<&str, Vec<String>> = [
            (
                "bybit_futures",
                vec!["BTCUSDT".to_string(), "ETHUSDT".to_string()],
            ),
            ("gate_futures", vec!["BTCUSDT".to_string()]),
            ("binance_spot", vec!["BTCUSDT".to_string()]),
        ]
        .into_iter()
        .collect();
        let v = futures_volumes(&hub, &listed);
        assert_eq!(v.get("BTCUSDT"), Some(&150));
        assert_eq!(v.get("ETHUSDT"), None);
    }

    #[test]
    fn rate_limit_refills() {
        let l = Limits::default();
        let t = Instant::now();
        for _ in 0..USER_BURST as usize {
            assert!(l.allow_user("u1", "1.2.3.4", t));
        }
        assert!(!l.allow_user("u1", "1.2.3.4", t));
        // Another user behind the same carrier address is not blocked.
        assert!(l.allow_user("u2", "1.2.3.4", t));
        assert!(l.allow_user("u1", "1.2.3.4", t + Duration::from_millis(200)));
    }

    #[test]
    fn sockets_per_ip() {
        let l = Limits::default();
        for _ in 0..SOCKETS_PER_USER {
            assert!(l.open_user_socket("u1", "ip"));
        }
        assert!(!l.open_user_socket("u1", "ip"));
        assert!(l.open_user_socket("u2", "ip"));
        l.close_user_socket("u1", "ip");
        assert!(l.open_user_socket("u1", "ip"));
    }

    #[test]
    fn forwarded_ip_is_the_proxy_added_one() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "6.6.6.6, 1.2.3.4".parse().unwrap());
        assert_eq!(client_ip(&h), "1.2.3.4");
        assert_eq!(client_ip(&HeaderMap::new()), "direct");
    }

    #[test]
    fn subscription_messages() {
        let v: Value = serde_json::from_str(
            r#"{"op":"subscribe","channel":"prices","markets":[{"exchange":"bybit_futures","symbol":"btc"},{"exchange":"hyperliquid","symbol":"ETHUSDT"}]}"#,
        )
        .unwrap();
        assert_eq!(
            parse_subs(&v).unwrap(),
            vec![
                Sub::Price("bybit_futures".into(), "BTCUSDT".into()),
                Sub::Price("hyperliquid".into(), "ETHUSDT".into())
            ]
        );
        let v: Value = serde_json::from_str(
            r#"{"op":"subscribe","channel":"candles","exchange":"gate_futures","symbol":"SOLUSDT","interval":"5m"}"#,
        )
        .unwrap();
        assert_eq!(
            parse_subs(&v).unwrap(),
            vec![Sub::Candle(
                "gate_futures".into(),
                "SOLUSDT".into(),
                Interval::M5
            )]
        );
        let bad: Value = serde_json::from_str(r#"{"exchange":"nope","symbol":"X"}"#).unwrap();
        assert!(parse_subs(&bad).is_err());
        let bad: Value = serde_json::from_str(
            r#"{"channel":"candles","exchange":"gate_futures","symbol":"X","interval":"4h"}"#,
        )
        .unwrap();
        assert!(parse_subs(&bad).is_err());
    }

    #[test]
    fn four_hours_from_hourly_bars() {
        let h = 3600;
        let t0 = 1_791_000_000 - 1_791_000_000 % (4 * h);
        // Starts one hour into a 4h bucket: that partial bucket is dropped.
        let base: Vec<Bar> = (1..9)
            .map(|i| Bar {
                start: t0 + i * h,
                open: i as f64,
                high: i as f64 + 0.5,
                low: i as f64 - 0.5,
                close: i as f64 + 0.1,
                volume: 1.0,
            })
            .collect();
        let out = aggregate_bars(&base, h, Interval::H4);
        // The newest bucket (hour 8) may still be open, so it stays.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].start, t0 + 4 * h);
        assert_eq!(
            (
                out[0].open,
                out[0].high,
                out[0].low,
                out[0].close,
                out[0].volume
            ),
            (4.0, 7.5, 3.5, 7.1, 4.0)
        );
        assert_eq!(base_of(Interval::M45), Some(Interval::M15));
        assert_eq!(base_of(Interval::H1), None);
    }

    #[test]
    fn stale_prices_are_missing() {
        let now = Utc::now();
        let old = now - chrono::Duration::seconds(STALE_AFTER_SECS + 5);
        let q = Quote {
            last: Some((2.0, old)),
            mark: Some((1.9, now)),
        };
        let v = price_value("bybit_futures", "XUSDT", None, q, now);
        assert_eq!(v["last"], Value::Null);
        assert_eq!(v["mark"], json!(1.9));
    }

    #[test]
    fn price_shape() {
        let now = Utc::now();
        let q = Quote {
            last: Some((2.0, now)),
            mark: Some((1.9, now - chrono::Duration::seconds(1))),
        };
        let v = price_value("bybit_futures", "XUSDT", None, q, now);
        assert_eq!(v["last"], json!(2.0));
        assert_eq!(v["mark"], json!(1.9));
        assert_eq!(v["price"], Value::Null);
        assert_eq!(v["ts"], json!(now.timestamp_millis()));
    }
}
