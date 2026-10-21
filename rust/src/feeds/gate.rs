//! Gate USDT perpetual futures: `futures.tickers` per contract (there is no
//! wildcard subscribe). Contracts come from REST, are subscribed in batches,
//! and are re-fetched every 15 minutes to pick up new listings. The price is
//! the last trade price (mark when last is missing), like every other venue:
//! Gate's mark trails trades by seconds on a fast move, so mark-built candles
//! put a listing pump a minute late (AT, 2026-10-21).
//! Tickers carry the funding rate; the next funding time and interval come
//! from the contract list.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use super::markets::Registry;
use super::{num, Alive, Reconnect, Sink};
use crate::model::{rate, unix_ms, Funding, PriceUpdate};

pub const NAME: &str = "gate_futures";
const WS_URL: &str = "wss://fx-ws.gateio.ws/v4/ws/usdt";
const REST_CONTRACTS: &str = "https://api.gateio.ws/api/v4/futures/usdt/contracts";
const BATCH: usize = 80;
const REFRESH: Duration = Duration::from_secs(15 * 60);

/// BTC_USDT -> BTCUSDT.
pub fn symbol_of(contract: &str) -> String {
    contract.replace('_', "").to_uppercase()
}

/// Trading, not delisting, contracts from the REST answer, by name, with
/// their funding (rate, next time, interval).
pub fn trading_contracts(rows: &Value) -> Vec<(String, Option<Funding>)> {
    let mut out: Vec<(String, Option<Funding>)> = rows
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r.get("in_delisting").and_then(Value::as_bool) != Some(true))
        .filter(|r| r.get("status").and_then(Value::as_str) == Some("trading"))
        .filter_map(|r| {
            let name = r.get("name")?.as_str()?.to_string();
            let f = Funding {
                rate: rate(r.get("funding_rate")),
                next_ms: unix_ms(r.get("funding_next_apply")),
                interval_hours: r
                    .get("funding_interval")
                    .and_then(Value::as_f64)
                    .filter(|s| *s > 0.0)
                    .map(|s| s / 3600.0),
            };
            Some((name, (!f.is_empty()).then_some(f)))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
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
            let price = last.or(mark)?;
            let qv = num(item.get("volume_24h_quote")).or_else(|| num(item.get("volume_24h")));
            let funding = rate(item.get("funding_rate")).map(|r| Funding {
                rate: Some(r),
                ..Funding::default()
            });
            Some(PriceUpdate {
                symbol: symbol_of(contract),
                exchange: NAME,
                price,
                timestamp: ts,
                quote_volume_24h: qv,
                volume: None,
                high_24h: num(item.get("high_24h")),
                low_24h: num(item.get("low_24h")),
                last,
                mark,
                primary: true,
                funding,
            })
        })
        .collect()
}

fn listed(contracts: &[(String, Option<Funding>)]) -> super::markets::VenueMarkets {
    contracts
        .iter()
        .map(|(c, funding)| {
            (
                symbol_of(c),
                super::markets::MarketInfo {
                    venue_symbol: c.clone(),
                    multiplier: 1.0,
                    funding: *funding,
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

async fn fetch_contracts(http: &reqwest::Client) -> Result<Vec<(String, Option<Funding>)>, String> {
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
    // The contract list first: it can take a minute from some networks, and
    // Gate drops a socket that stays quiet that long.
    let contracts = match fetch_contracts(http).await {
        Ok(c) if !c.is_empty() => {
            registry.set(NAME, listed(&c));
            c.into_iter().map(|(name, _)| name).collect()
        }
        other => {
            match other {
                Ok(_) => tracing::error!("Gate futures: no trading contracts from REST"),
                Err(e) => tracing::error!("Gate futures: contract list failed: {e}"),
            }
            let known: Vec<String> = registry
                .venue(NAME)
                .into_values()
                .map(|i| i.venue_symbol)
                .collect();
            if known.is_empty() {
                return false;
            }
            tracing::warn!(
                "Gate futures: using the last known {} contract(s)",
                known.len()
            );
            known
        }
    };
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

    tracing::info!(
        "Gate futures: initial subscribe for {} contract(s)",
        contracts.len()
    );
    for batch in contracts.chunks(BATCH) {
        if tx.send(Message::text(subscribe_msg(batch))).await.is_err() {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut subscribed: HashSet<String> = contracts.into_iter().collect();
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
                        let new: Vec<String> = contracts.into_iter().map(|(c, _)| c).filter(|c| !subscribed.contains(c)).collect();
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
    fn parses_tickers_last_first() {
        let u = parse(
            r#"{"time":1,"time_ms":1791807529762,"channel":"futures.tickers","event":"update","result":[
                {"contract":"BTC_USDT","last":"100","mark_price":"101","volume_24h_quote":"5000","funding_rate":"0.0001","high_24h":"110","low_24h":"90"},
                {"contract":"ETH_USDT","last":"10","mark_price":""},
                {"contract":"BAD_USDT","last":"0"},
                {"contract":"NEW_USDT","last":"","mark_price":"2"}]}"#,
        );
        assert_eq!(u.len(), 3);
        assert_eq!((u[2].symbol.as_str(), u[2].price), ("NEWUSDT", 2.0));
        assert_eq!(
            (u[0].symbol.as_str(), u[0].price, u[0].quote_volume_24h),
            ("BTCUSDT", 100.0, Some(5000.0))
        );
        assert_eq!((u[0].high_24h, u[0].low_24h), (Some(110.0), Some(90.0)));
        assert_eq!((u[1].symbol.as_str(), u[1].price), ("ETHUSDT", 10.0));
        assert_eq!((u[0].last, u[0].mark), (Some(100.0), Some(101.0)));
        assert_eq!(u[1].mark, None);
        assert_eq!(u[0].funding.unwrap().rate, Some(0.0001));
        assert!(u[1].funding.is_none());
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
        let names: Vec<String> = trading_contracts(&rows).into_iter().map(|c| c.0).collect();
        assert_eq!(names, vec!["A_USDT", "B_USDT"]);
        let rows: Value = serde_json::from_str(
            r#"[{"name":"A_USDT","status":"trading","funding_rate":"-0.000412","funding_next_apply":1791820800,"funding_interval":14400}]"#,
        )
        .unwrap();
        let f = trading_contracts(&rows)[0].1.unwrap();
        assert_eq!(
            (f.rate, f.next_ms, f.interval_hours),
            (Some(-0.000412), Some(1791820800000), Some(4.0))
        );
    }
}
