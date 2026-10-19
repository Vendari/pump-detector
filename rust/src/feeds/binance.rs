//! Binance USDⓈ-M futures: last price of every market from the all-market
//! mini ticker stream (one array per second with the symbols that changed).
//! Market streams moved to `/market/ws` (checked 2026-10-12).
//!
//! Mark prices of every market come from the all-market `!markPrice@arr@1s`
//! stream, which Aster (a Binance-style venue) offers too.

use std::time::Duration;

use chrono::{TimeZone, Utc};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

use super::{num, Alive, Reconnect, Sink};
use crate::model::PriceUpdate;

pub const NAME: &str = "binance_futures";
const URL: &str = "wss://fstream.binance.com/market/ws/!miniTicker@arr";
pub const MARK_URL: &str = "wss://fstream.binance.com/market/ws/!markPrice@arr@1s";
pub const ASTER_MARK_URL: &str = "wss://fstream.asterdex.com/ws/!markPrice@arr@1s";

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
            let mut u = PriceUpdate::last_trade(
                symbol.to_string(),
                NAME,
                price,
                Utc.timestamp_millis_opt(ms)
                    .single()
                    .unwrap_or_else(Utc::now),
            );
            u.quote_volume_24h = i.get("q").and_then(|q| q.as_str()?.parse().ok());
            u.high_24h = num(i.get("h"));
            u.low_24h = num(i.get("l"));
            Some(u)
        })
        .collect()
}

/// One `!markPrice@arr@1s` frame (Binance or Aster).
pub fn parse_marks(text: &str, exchange: &'static str) -> Vec<PriceUpdate> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|i| i.get("e").and_then(Value::as_str) == Some("markPriceUpdate"))
        .filter_map(|i| {
            let symbol = i.get("s")?.as_str()?.to_uppercase();
            let mark = num(i.get("p"))?;
            let ms = i.get("E").and_then(Value::as_i64).unwrap_or(0);
            Some(PriceUpdate::mark_only(
                symbol,
                exchange,
                mark,
                Utc.timestamp_millis_opt(ms)
                    .single()
                    .unwrap_or_else(Utc::now),
            ))
        })
        .collect()
}

/// Last prices of every Binance market.
pub async fn run(sink: Sink, alive: Alive) {
    run_stream(NAME, URL, parse, sink, alive).await
}

/// Mark prices of every market of a Binance-style venue.
pub async fn run_marks(exchange: &'static str, url: &'static str, sink: Sink, alive: Alive) {
    run_stream(
        exchange,
        url,
        move |t: &str| parse_marks(t, exchange),
        sink,
        alive,
    )
    .await
}

async fn run_stream<F>(name: &'static str, url: &'static str, parse: F, sink: Sink, alive: Alive)
where
    F: Fn(&str) -> Vec<PriceUpdate> + Send + Sync,
{
    let mut pace = Reconnect::new(name);
    loop {
        let got = session(name, url, &parse, &sink, &alive).await;
        alive.set(false);
        if sink.is_closed() {
            return;
        }
        let wait = pace.after(got);
        tracing::info!("{name} ({url}): reconnecting in {}s", wait.as_secs());
        tokio::time::sleep(wait).await;
    }
}

/// True when the session delivered any price.
async fn session<F>(
    name: &'static str,
    url: &'static str,
    parse: &F,
    sink: &Sink,
    alive: &Alive,
) -> bool
where
    F: Fn(&str) -> Vec<PriceUpdate>,
{
    tracing::info!("Connecting to {name}: {url}");
    let ws = match tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async(url),
    )
    .await
    {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(e)) => {
            tracing::error!("{name}: connect failed: {e}");
            return false;
        }
        Err(_) => {
            tracing::error!("{name}: connect timed out");
            return false;
        }
    };
    tracing::info!("Connected to {name}: {url}");
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
                    Ok(Some(Err(e))) => { tracing::warn!("{name}: {e}"); return got; }
                    Ok(None) => { tracing::warn!("{name} WebSocket connection closed"); return got; }
                    Err(_) => { tracing::warn!("{name}: no data for 60 s"); return got; }
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
        assert_eq!(
            (u[0].high_24h, u[0].low_24h),
            (Some(0.07088), Some(0.06217))
        );
        assert_eq!(u[0].timestamp.timestamp_millis(), 1791807529762);
        assert!(parse("{}").is_empty());
        assert_eq!(u[0].last, Some(0.06357));
        assert!(u[0].primary);
    }

    #[test]
    fn parses_mark_prices() {
        let u = parse_marks(
            r#"[{"e":"markPriceUpdate","E":1791807529000,"s":"BTCUSDT","p":"64000.10","i":"64001"},{"e":"markPriceUpdate","s":"Z","p":"0"}]"#,
            "aster_futures",
        );
        assert_eq!(u.len(), 1);
        assert_eq!(
            (u[0].exchange, u[0].mark, u[0].primary),
            ("aster_futures", Some(64000.1), false)
        );
    }
}
