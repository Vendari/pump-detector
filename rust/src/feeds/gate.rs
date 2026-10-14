//! Gate USDT perpetual futures: `futures.tickers` per contract (there is no
//! wildcard subscribe). Contracts come from REST, are subscribed in batches,
//! and are re-fetched every 15 minutes to pick up new listings. The price is
//! the mark price (last price when mark is missing), as in the Python service.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use super::markets::Registry;
use super::{num, Alive, Reconnect, Sink};
use crate::model::PriceUpdate;

pub const NAME: &str = "gate_futures";
const WS_URL: &str = "wss://fx-ws.gateio.ws/v4/ws/usdt";
const REST_CONTRACTS: &str = "https://api.gateio.ws/api/v4/futures/usdt/contracts";
const BATCH: usize = 80;
const REFRESH: Duration = Duration::from_secs(15 * 60);

/// BTC_USDT -> BTCUSDT.
pub fn symbol_of(contract: &str) -> String {
    contract.replace('_', "").to_uppercase()
}

/// Names of trading, not delisting, contracts from the REST answer.
pub fn trading_contracts(rows: &Value) -> Vec<String> {
    let mut out: Vec<String> = rows
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r.get("in_delisting").and_then(Value::as_bool) != Some(true))
        .filter(|r| r.get("status").and_then(Value::as_str) == Some("trading"))
        .filter_map(|r| r.get("name")?.as_str().map(str::to_string))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// One WS frame; only `futures.tickers` updates give prices.
pub fn parse(text: &str) -> Vec<PriceUpdate> {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        tracing::warn!("Gate futures channel error: {err}");
        return Vec::new();
    }
    if v.get("channel").and_then(Value::as_str) != Some("futures.tickers")
        || v.get("event").and_then(Value::as_str) != Some("update")
    {
        return Vec::new();
    }
    let ts = v
        .get("time_ms")
        .and_then(Value::as_i64)
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
        .unwrap_or_else(Utc::now);
    v.get("result")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let contract = item.get("contract")?.as_str()?;
            let mark = num(item.get("mark_price"));
            let last = num(item.get("last"));
            let price = mark.or(last)?;
            let qv = num(item.get("volume_24h_quote")).or_else(|| num(item.get("volume_24h")));
            Some(PriceUpdate {
                symbol: symbol_of(contract),
                exchange: NAME,
                price,
                timestamp: ts,
                quote_volume_24h: qv,
                volume: None,
                last,
                mark,
                primary: true,
            })
        })
        .collect()
}

fn listed(contracts: &[String]) -> super::markets::VenueMarkets {
    contracts
        .iter()
        .map(|c| {
            (
                symbol_of(c),
                super::markets::MarketInfo {
                    venue_symbol: c.clone(),
                    multiplier: 1.0,
                },
            )
        })
        .collect()
}

fn subscribe_msg(batch: &[String]) -> String {
    json!({
        "time": Utc::now().timestamp(),
        "channel": "futures.tickers",
        "event": "subscribe",
        "payload": batch,
    })
    .to_string()
}

async fn fetch_contracts(http: &reqwest::Client) -> Result<Vec<String>, String> {
    let rows: Value = http
        .get(REST_CONTRACTS)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    Ok(trading_contracts(&rows))
}

pub async fn run(sink: Sink, alive: Alive, registry: Registry) {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .build()
        .expect("http client");
    let mut pace = Reconnect::new(NAME);
    loop {
        let got = session(&sink, &alive, &http, &registry).await;
        alive.set(false);
        if sink.is_closed() {
            return;
        }
        let wait = pace.after(got);
        tracing::info!("{NAME}: reconnecting in {}s", wait.as_secs());
        tokio::time::sleep(wait).await;
    }
}

async fn session(sink: &Sink, alive: &Alive, http: &reqwest::Client, registry: &Registry) -> bool {
    tracing::info!("Connecting to Gate futures: {WS_URL}");
    let mut req = WS_URL.into_client_request().expect("url");
    req.headers_mut()
        .insert("X-Gate-Size-Decimal", "1".parse().expect("header"));
    let ws = match tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async(req),
    )
    .await
    {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(e)) => {
            tracing::error!("Failed to connect to Gate futures: {e}");
            return false;
        }
        Err(_) => {
            tracing::error!("Gate futures connect timed out");
            return false;
        }
    };
    tracing::info!("Connected to Gate futures WebSocket");
    let (mut tx, mut rx) = ws.split();

    let mut subscribed: HashSet<String> = HashSet::new();
    match fetch_contracts(http).await {
        Ok(contracts) if !contracts.is_empty() => {
            tracing::info!(
                "Gate futures: initial subscribe for {} contract(s)",
                contracts.len()
            );
            registry.set(NAME, listed(&contracts));
            for batch in contracts.chunks(BATCH) {
                if tx.send(Message::text(subscribe_msg(batch))).await.is_err() {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            subscribed.extend(contracts);
        }
        Ok(_) => tracing::error!("Gate futures: no trading contracts from REST"),
        Err(e) => {
            tracing::error!("Gate futures: contract list failed: {e}");
            return false;
        }
    }
    alive.set(true);

    let mut got = false;
    let mut refresh = tokio::time::interval(REFRESH);
    refresh.reset();
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    ping.reset();
    loop {
        tokio::select! {
            frame = tokio::time::timeout(Duration::from_secs(120), rx.next()) => {
                let msg = match frame {
                    Ok(Some(Ok(m))) => m,
                    Ok(Some(Err(e))) => { tracing::warn!("Gate futures: {e}"); return got; }
                    Ok(None) => { tracing::warn!("Gate futures WebSocket closed"); return got; }
                    Err(_) => { tracing::warn!("Gate futures: no data for 120 s"); return got; }
                };
                match msg {
                    Message::Text(t) => {
                        let updates = parse(t.as_str());
                        if !updates.is_empty() {
                            got = true;
                            if sink.send(updates).await.is_err() {
                                return got;
                            }
                        }
                    }
                    Message::Ping(p) => { let _ = tx.send(Message::Pong(p)).await; }
                    Message::Close(_) => return got,
                    _ => {}
                }
            }
            _ = refresh.tick() => {
                match fetch_contracts(http).await {
                    Ok(contracts) => {
                        if !contracts.is_empty() {
                            registry.set(NAME, listed(&contracts));
                        }
                        let new: Vec<String> = contracts.into_iter().filter(|c| !subscribed.contains(c)).collect();
                        if !new.is_empty() {
                            tracing::info!("Gate futures: subscribing to {} new contract(s): {:?}", new.len(), &new[..new.len().min(5)]);
                            for batch in new.chunks(BATCH) {
                                if tx.send(Message::text(subscribe_msg(batch))).await.is_err() {
                                    return got;
                                }
                            }
                            subscribed.extend(new);
                        }
                    }
                    Err(e) => tracing::warn!("Gate futures contract refresh failed: {e}"),
                }
            }
            _ = ping.tick() => {
                if tx.send(Message::Ping(Vec::new())).await.is_err() {
                    return got;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tickers_mark_first() {
        let u = parse(
            r#"{"time":1,"time_ms":1791807529762,"channel":"futures.tickers","event":"update","result":[
                {"contract":"BTC_USDT","last":"100","mark_price":"101","volume_24h_quote":"5000"},
                {"contract":"ETH_USDT","last":"10","mark_price":""},
                {"contract":"BAD_USDT","last":"0"}]}"#,
        );
        assert_eq!(u.len(), 2);
        assert_eq!(
            (u[0].symbol.as_str(), u[0].price, u[0].quote_volume_24h),
            ("BTCUSDT", 101.0, Some(5000.0))
        );
        assert_eq!((u[1].symbol.as_str(), u[1].price), ("ETHUSDT", 10.0));
        assert_eq!((u[0].last, u[0].mark), (Some(100.0), Some(101.0)));
        assert_eq!(u[1].mark, None);
        assert!(parse(
            r#"{"channel":"futures.tickers","event":"subscribe","result":{"status":"success"}}"#
        )
        .is_empty());
    }

    #[test]
    fn contract_filter() {
        let rows: Value = serde_json::from_str(
            r#"[{"name":"B_USDT","status":"trading"},{"name":"A_USDT","status":"trading"},
                {"name":"C_USDT","status":"trading","in_delisting":true},{"name":"D_USDT","status":"delisted"}]"#,
        )
        .unwrap();
        assert_eq!(trading_contracts(&rows), vec!["A_USDT", "B_USDT"]);
    }
}
