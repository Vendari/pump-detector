//! Binance USDⓈ-M futures: last price of every market from the all-market
//! mini ticker stream (one array per second with the symbols that changed).
//! Market streams moved to `/market/ws` (checked 2026-10-12).

use std::time::Duration;

use chrono::{TimeZone, Utc};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

use super::{num, Alive, Reconnect, Sink};
use crate::model::PriceUpdate;

pub const NAME: &str = "binance_futures";
const URL: &str = "wss://fstream.binance.com/market/ws/!miniTicker@arr";

/// One `!miniTicker@arr` frame.
pub fn parse(text: &str) -> Vec<PriceUpdate> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|i| i.get("e").and_then(Value::as_str) == Some("24hrMiniTicker"))
        .filter_map(|i| {
            let symbol = i.get("s")?.as_str()?;
            let price = num(i.get("c"))?;
            let ms = i.get("E").and_then(Value::as_i64).unwrap_or(0);
            Some(PriceUpdate {
                symbol: symbol.to_string(),
                exchange: NAME,
                price,
                timestamp: Utc
                    .timestamp_millis_opt(ms)
                    .single()
                    .unwrap_or_else(Utc::now),
                quote_volume_24h: i.get("q").and_then(|q| q.as_str()?.parse().ok()),
            })
        })
        .collect()
}

pub async fn run(sink: Sink, alive: Alive) {
    let mut pace = Reconnect::new(NAME);
    loop {
        let got = session(&sink, &alive).await;
        alive.set(false);
        if sink.is_closed() {
            return;
        }
        let wait = pace.after(got);
        tracing::info!("{NAME}: reconnecting in {}s", wait.as_secs());
        tokio::time::sleep(wait).await;
    }
}

/// True when the session delivered any price.
async fn session(sink: &Sink, alive: &Alive) -> bool {
    tracing::info!("Connecting to {NAME}: {URL}");
    let ws = match tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async(URL),
    )
    .await
    {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(e)) => {
            tracing::error!("{NAME}: connect failed: {e}");
            return false;
        }
        Err(_) => {
            tracing::error!("{NAME}: connect timed out");
            return false;
        }
    };
    tracing::info!("Connected to {NAME}");
    alive.set(true);
    let (mut tx, mut rx) = ws.split();
    let mut got = false;
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    ping.reset();
    loop {
        tokio::select! {
            frame = tokio::time::timeout(Duration::from_secs(60), rx.next()) => {
                let msg = match frame {
                    Ok(Some(Ok(m))) => m,
                    Ok(Some(Err(e))) => { tracing::warn!("{NAME}: {e}"); return got; }
                    Ok(None) => { tracing::warn!("{NAME} WebSocket connection closed"); return got; }
                    Err(_) => { tracing::warn!("{NAME}: no data for 60 s"); return got; }
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
    fn parses_mini_tickers() {
        let u = parse(
            r#"[{"e":"24hrMiniTicker","E":1791807529762,"s":"CHIPUSDT","c":"0.06357","o":"0.06664","h":"0.07088","l":"0.06217","v":"144088466","q":"9512345.6"},
                {"e":"24hrMiniTicker","E":1791807529762,"s":"BADUSDT","c":"0"},
                {"e":"other","s":"X","c":"1"}]"#,
        );
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].symbol, "CHIPUSDT");
        assert_eq!(u[0].price, 0.06357);
        assert_eq!(u[0].quote_volume_24h, Some(9512345.6));
        assert_eq!(u[0].timestamp.timestamp_millis(), 1791807529762);
        assert!(parse("{}").is_empty());
    }
}
