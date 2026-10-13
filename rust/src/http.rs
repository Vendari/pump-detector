//! HTTP API, /ws/price and the /ui page (same routes as the Python
//! service), plus `PUT /trailing/positions` for the Trigger worker.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use base64::Engine;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;

use crate::alerts::SpikeAlertConfig;
use crate::app::App;
use crate::feeds::on_demand::Venue;
use crate::hub::price_json;
use crate::model::{iso_naive, Interval};
use crate::store::{to_json, MAX_CANDLES_PER_INTERVAL};
use crate::trailing::WatchRequest;

const UI_HTML: &str = include_str!("../static/ui.html");

type AppState = State<Arc<App>>;

fn eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

fn key_ok(app: &App, given: Option<&str>) -> bool {
    match &app.cfg.api_key {
        None => true,
        Some(k) => given.is_some_and(|g| eq(g, k)),
    }
}

fn basic_ok(app: &App, headers: &HeaderMap) -> bool {
    let Some(h) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(b64) = h
        .strip_prefix("Basic ")
        .or_else(|| h.strip_prefix("basic "))
    else {
        return false;
    };
    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
        return false;
    };
    let Ok(s) = String::from_utf8(raw) else {
        return false;
    };
    let Some((u, p)) = s.split_once(':') else {
        return false;
    };
    // Evaluate both, so timing does not tell which one was wrong.
    let user = eq(u, &app.cfg.ui_username);
    let pass = eq(p, &app.cfg.ui_password);
    user & pass
}

fn err(status: StatusCode, detail: impl Into<String>) -> Response {
    (status, Json(json!({"detail": detail.into()}))).into_response()
}

async fn auth(State(app): AppState, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if path == "/ui" || path.starts_with("/ui/") {
        if !basic_ok(&app, req.headers()) {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Basic realm=\"pump-detector\"")],
                Json(json!({"detail": "authentication required"})),
            )
                .into_response();
        }
        return next.run(req).await;
    }
    // /health is open; /ws/price checks its own key (header or ?key=).
    if path == "/health" || path == "/ws/price" {
        return next.run(req).await;
    }
    let given = req.headers().get("x-api-key").and_then(|v| v.to_str().ok());
    if !key_ok(&app, given) {
        return err(StatusCode::UNAUTHORIZED, "invalid api key");
    }
    next.run(req).await
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/stats", get(stats))
        .route("/ws/price", get(ws_price))
        .route("/candles", get(candles))
        .route("/price/{symbol}", get(price))
        .route("/alerts", post(create_alert).get(list_alerts))
        .route("/alerts/detected", get(detected))
        .route("/alerts/{alert_id}", delete(delete_alert))
        .route("/webhook/test", post(webhook_test))
        .route("/exchanges", get(exchanges))
        .route("/symbols/{exchange}", get(symbols))
        .route("/trailing/positions", put(trailing_sync).get(trailing_list))
        .route("/ui", get(ui))
        .route("/ui/api/prices", get(ui_prices))
        .route("/ui/api/exchanges", get(ui_exchanges))
        .route("/ui/api/candles", get(ui_candles))
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .with_state(app)
}

fn exchange_names(app: &App) -> Vec<&'static str> {
    app.feeds.iter().map(|(n, _)| *n).collect()
}

async fn root(State(app): AppState) -> Json<Value> {
    Json(json!({
        "name": "Crypto Pump Detector",
        "version": env!("CARGO_PKG_VERSION"),
        "status": "running",
        "exchanges": exchange_names(&app),
    }))
}

async fn health(State(app): AppState) -> Json<Value> {
    // On-demand feeds are idle when nothing is watched; count them as up.
    let active: Vec<&str> = app
        .feeds
        .iter()
        .filter(|(n, a)| a.get() || Venue::from_name(n).is_some())
        .map(|(n, _)| *n)
        .collect();
    let streaming = app
        .feeds
        .iter()
        .filter(|(n, a)| Venue::from_name(n).is_none() && a.get())
        .count();
    Json(json!({
        "status": if streaming > 0 { "healthy" } else { "degraded" },
        "active_exchanges": active,
        "total_exchanges": app.feeds.len(),
    }))
}

async fn stats(State(app): AppState) -> Json<Value> {
    use std::sync::atomic::Ordering::Relaxed;
    let s = &app.stats;
    Json(json!({"monitoring": {
        "total_updates": s.total_updates.load(Relaxed),
        "candles_created": s.candles_created.load(Relaxed),
        "alerts_sent": s.alerts_sent.load(Relaxed),
        "active_exchanges": app.feeds.iter().filter(|(_, a)| a.get()).count(),
        "trailing_positions": app.trailing.lock().len(),
        "trailing_closes": s.trailing_closes.load(Relaxed),
    }}))
}

// --- /ws/price ---

#[derive(Deserialize)]
struct WsQuery {
    key: Option<String>,
}

async fn ws_price(
    State(app): AppState,
    headers: HeaderMap,
    Query(q): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let given = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or(q.key);
    if !key_ok(&app, given.as_deref()) {
        // Python closed with 1008 after the handshake; refusing it is clearer.
        return err(StatusCode::UNAUTHORIZED, "invalid api key");
    }
    ws.on_upgrade(move |socket| ws_client(app, socket))
}

fn known_exchange(app: &App, ex: &str) -> bool {
    app.feeds.iter().any(|(n, _)| *n == ex)
}

async fn ws_client(app: Arc<App>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<String>(1000);
    let client = app.hub.lock().new_client();
    let mut subs: HashSet<(String, String)> = HashSet::new();

    let sender = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if sink.send(Message::text(m)).await.is_err() {
                break;
            }
        }
    });
    let reply = |v: Value| {
        let _ = tx.try_send(v.to_string());
    };

    while let Some(Ok(msg)) = stream.next().await {
        let Message::Text(text) = msg else {
            if matches!(msg, Message::Close(_)) {
                break;
            }
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else {
            reply(json!({"error": "invalid json"}));
            continue;
        };
        let action = v.get("action").and_then(Value::as_str).unwrap_or("");
        let sym = v
            .get("symbol")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_uppercase();
        let ex = v
            .get("exchange")
            .and_then(Value::as_str)
            .unwrap_or("binance_futures")
            .to_string();
        match action {
            "subscribe" => {
                if sym.is_empty() {
                    reply(json!({"error": "symbol required"}));
                    continue;
                }
                if !known_exchange(&app, &ex) {
                    reply(json!({"error": "unknown exchange", "exchange": ex}));
                    continue;
                }
                let key = (ex.clone(), sym.clone());
                let latest = {
                    let mut hub = app.hub.lock();
                    if subs.insert(key.clone()) {
                        hub.subscribe(client, key, tx.clone());
                    }
                    hub.latest(&ex, &sym)
                };
                reply(json!({"status": "subscribed", "symbol": sym, "exchange": ex}));
                if let Some(l) = latest {
                    let _ = tx.try_send(price_json(&sym, &ex, &l));
                }
            }
            "unsubscribe" => {
                let targets: Vec<(String, String)> = if sym.is_empty() {
                    subs.iter().cloned().collect()
                } else {
                    vec![(ex, sym)]
                };
                let mut hub = app.hub.lock();
                for k in targets {
                    if subs.remove(&k) {
                        hub.unsubscribe(client, &k);
                    }
                }
                drop(hub);
                reply(json!({"status": "unsubscribed"}));
            }
            _ => {}
        }
    }
    {
        let mut hub = app.hub.lock();
        for k in &subs {
            hub.unsubscribe(client, k);
        }
    }
    drop(tx);
    sender.abort();
}

// --- candles and prices ---

#[derive(Deserialize)]
struct CandleQuery {
    symbol: String,
    #[serde(default = "default_exchange")]
    exchange: String,
    #[serde(default = "default_interval")]
    interval: String,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_exchange() -> String {
    "binance_futures".into()
}
fn default_interval() -> String {
    "1m".into()
}
fn default_limit() -> i64 {
    100
}

fn valid_intervals() -> Vec<&'static str> {
    Interval::ALL.iter().map(|i| i.as_str()).collect()
}

fn bad_interval() -> Response {
    err(
        StatusCode::BAD_REQUEST,
        format!("Invalid interval. Valid: {:?}", valid_intervals()),
    )
}

/// "24h" is stored as 1D.
fn stored_interval(s: &str) -> Option<&'static str> {
    Interval::parse(s).map(Interval::as_str)
}

async fn candles(State(app): AppState, Query(q): Query<CandleQuery>) -> Response {
    let Some(interval) = stored_interval(&q.interval) else {
        return bad_interval();
    };
    let limit = q.limit.clamp(1, MAX_CANDLES_PER_INTERVAL);
    let rows = app
        .store
        .get_candles(&q.symbol, &q.exchange, interval, limit)
        .await;
    Json(json!({
        "symbol": q.symbol,
        "exchange": q.exchange,
        "interval": q.interval,
        "candles": rows.iter().map(to_json).collect::<Vec<_>>(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ExchangeQuery {
    #[serde(default = "default_exchange")]
    exchange: String,
}

async fn price(
    State(app): AppState,
    Path(symbol): Path<String>,
    Query(q): Query<ExchangeQuery>,
) -> Response {
    let latest = app.hub.lock().latest(&q.exchange, &symbol);
    if let Some(l) = latest {
        let v: Value =
            serde_json::from_str(&price_json(&symbol, &q.exchange, &l)).unwrap_or_default();
        return Json(v).into_response();
    }
    match app.store.latest_close(&symbol, &q.exchange).await {
        Some((close, ts)) => Json(json!({
            "symbol": symbol, "exchange": q.exchange, "price": close, "timestamp": iso_naive(ts.to_chrono()),
        }))
        .into_response(),
        None => err(StatusCode::NOT_FOUND, "No price data for symbol"),
    }
}

// --- custom alerts ---

async fn create_alert(State(app): AppState, Json(cfg): Json<SpikeAlertConfig>) -> Response {
    if Interval::parse(&cfg.interval).is_none() {
        return bad_interval();
    }
    match app.store.create_spike_alert(&cfg).await {
        Ok(id) => {
            app.reload_configs().await;
            Json(json!({"status": "created", "alert_id": id})).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct AlertQuery {
    exchange: Option<String>,
    symbol: Option<String>,
    #[serde(default = "default_limit")]
    limit: i64,
}

async fn list_alerts(State(app): AppState, Query(q): Query<AlertQuery>) -> Json<Value> {
    Json(
        json!({"alerts": app.store.list_spike_alerts(q.exchange.as_deref(), q.symbol.as_deref()).await}),
    )
}

async fn delete_alert(State(app): AppState, Path(id): Path<String>) -> Response {
    if !app.store.delete_spike_alert(&id).await {
        return err(StatusCode::NOT_FOUND, "Alert not found");
    }
    app.reload_configs().await;
    Json(json!({"status": "deleted"})).into_response()
}

async fn detected(State(app): AppState, Query(q): Query<AlertQuery>) -> Json<Value> {
    Json(
        json!({"alerts": app.store.detected_alerts(q.symbol.as_deref(), q.exchange.as_deref(), q.limit).await}),
    )
}

#[derive(Deserialize)]
struct UrlQuery {
    url: String,
}

async fn webhook_test(State(app): AppState, Query(q): Query<UrlQuery>) -> Response {
    if !q.url.starts_with("http") {
        return err(StatusCode::BAD_REQUEST, "Valid url required");
    }
    let payload = json!({
        "symbol": "TESTUSDT", "exchange": "test_exchange", "alert_type": "spike",
        "start_price": 100.0, "current_price": 105.0, "change_percent": 5.0, "interval": "1m",
        "timestamp": iso_naive(chrono::Utc::now()), "alert_id": null,
    });
    let mut req = app
        .http
        .post(&q.url)
        .json(&payload)
        .timeout(app.cfg.webhook_timeout);
    if let Some(k) = &app.cfg.webhook_api_key {
        req = req.header("x-api-key", k);
    }
    match req.send().await {
        Ok(r) if r.status().is_success() => {
            Json(json!({"status": "success", "message": "Webhook test sent"})).into_response()
        }
        _ => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to send test webhook",
        ),
    }
}

async fn exchanges(State(app): AppState) -> Json<Value> {
    let mut ex = app.store.distinct_exchanges().await;
    if ex.is_empty() {
        ex = exchange_names(&app)
            .into_iter()
            .map(str::to_string)
            .collect();
    }
    Json(json!({"exchanges": ex}))
}

async fn symbols(State(app): AppState, Path(exchange): Path<String>) -> Json<Value> {
    let s = app.store.distinct_symbols(&exchange).await;
    Json(json!({"exchange": exchange, "symbols": s}))
}

// --- trailing ---

#[derive(Deserialize)]
struct TrailingSync {
    positions: Vec<WatchRequest>,
}

/// The worker's full list of open trailing positions. Answers each one's
/// state and best price so far.
async fn trailing_sync(State(app): AppState, Json(body): Json<TrailingSync>) -> Json<Value> {
    let statuses = app.sync_trailing(body.positions);
    Json(json!({"positions": statuses}))
}

async fn trailing_list(State(app): AppState) -> Json<Value> {
    Json(json!({"positions": app.trailing.lock().statuses()}))
}

// --- /ui ---

async fn ui() -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Html(UI_HTML)).into_response()
}

async fn ui_prices(State(app): AppState) -> Response {
    let now = chrono::Utc::now();
    let rows: Vec<Value> = app
        .hub
        .lock()
        .all_latest()
        .into_iter()
        .map(|((ex, sym), l)| {
            let ts = l.timestamp.timestamp_millis() as f64 / 1000.0;
            let age = ((now - l.timestamp).num_milliseconds().max(0) as f64 / 100.0).round() / 10.0;
            json!({"symbol": sym, "exchange": ex, "price": l.price, "timestamp": ts, "age_seconds": age})
        })
        .collect();
    ([(header::CACHE_CONTROL, "no-store")], Json(rows)).into_response()
}

fn candle_exchanges_sorted(app: &App) -> Vec<String> {
    let mut v: Vec<String> = app.cfg.candle_exchanges.iter().cloned().collect();
    v.sort();
    v
}

async fn ui_exchanges(State(app): AppState) -> Json<Value> {
    let ex: Vec<Value> = app
        .feeds
        .iter()
        .map(|(n, _)| json!({"name": n, "candles": app.cfg.candle_exchanges.contains(*n)}))
        .collect();
    Json(json!({
        "exchanges": ex,
        "candle_exchanges": candle_exchanges_sorted(&app),
        "intervals": valid_intervals(),
    }))
}

async fn ui_candles(State(app): AppState, Query(q): Query<HashMap<String, String>>) -> Response {
    let (Some(symbol), Some(exchange)) = (q.get("symbol"), q.get("exchange")) else {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "symbol and exchange required",
        );
    };
    let interval_s = q.get("interval").map(String::as_str).unwrap_or("1m");
    let Some(interval) = stored_interval(interval_s) else {
        return bad_interval();
    };
    let symbol = symbol.to_uppercase();
    let limit = q
        .get("limit")
        .and_then(|l| l.parse::<i64>().ok())
        .unwrap_or(100)
        .clamp(1, MAX_CANDLES_PER_INTERVAL);
    let stored = app.cfg.candle_exchanges.contains(exchange.as_str());
    let mut out = Vec::new();
    if stored {
        for c in app
            .store
            .get_candles(&symbol, exchange, interval, limit)
            .await
        {
            let Ok(ts) = c.get_datetime("timestamp") else {
                continue;
            };
            out.push(json!({
                "time": ts.timestamp_millis() / 1000,
                "open": c.get_f64("open").ok(),
                "high": c.get_f64("high").ok(),
                "low": c.get_f64("low").ok(),
                "close": c.get_f64("close").ok(),
                "volume": c.get_f64("volume").unwrap_or(0.0),
            }));
        }
    }
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "symbol": symbol,
            "exchange": exchange,
            "interval": interval_s,
            "candles_stored": stored,
            "candle_exchanges": candle_exchanges_sorted(&app),
            "candles": out,
        })),
    )
        .into_response()
}
