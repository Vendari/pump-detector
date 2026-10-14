//! Historical candles from exchange public REST, to fill chart history the
//! service did not record itself.
//!
//! Endpoints (checked live 2026-10-14):
//! - Binance `GET fapi.binance.com/fapi/v1/klines` and Aster (same API) `GET
//!   fapi.asterdex.com/fapi/v1/klines`: oldest first, up to 1500, quote
//!   volume at index 7. An unknown symbol is HTTP 400 with code -1121.
//! - Gate `GET api.gateio.ws/api/v4/futures/usdt/candlesticks` on
//!   `mark_<CONTRACT>` (our Gate price is the mark price): oldest first, up
//!   to 2000, no volume, only the latest 10000 points of any interval. An
//!   unknown contract is HTTP 400 `CONTRACT_NOT_FOUND`.
//! - Bybit `GET api.bybit.com/v5/market/kline?category=linear`: newest first,
//!   up to 1000, turnover (quote volume) at index 6. An unknown symbol is
//!   retCode 10001; an unsupported interval answers an empty list.
//! - KuCoin `GET api-futures.kucoin.com/api/v1/kline/query`: oldest first,
//!   at most 200 per call (counted from `from`), turnover at index 6. Minutes
//!   without trades have no candle. An unknown symbol answers empty data.
//! - Hyperliquid `POST api.hyperliquid.xyz/info` `candleSnapshot`: oldest
//!   first, only the latest ~5000 candles; volume is in coins. An unknown
//!   coin is HTTP 500 with body `null`.
//!
//! Intervals a venue lacks are built from the largest native interval that
//! divides them, into our epoch-aligned buckets (1W always from 1D, since
//! our weeks start on Thursday).

// Callers come with the chart integration.
#![allow(dead_code)]

use std::time::Duration;

use chrono::{TimeZone, Utc};
use serde_json::{json, Value};

use crate::feeds::num;
use crate::model::Interval;

const MAX_PAGES: usize = 4;
const PAGE_PAUSE: Duration = Duration::from_millis(100);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, PartialEq)]
pub struct RawCandle {
    /// Bucket start, Unix seconds.
    pub start: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// Quote (USDT) volume.
    pub volume: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Venue {
    Binance,
    Aster,
    Gate,
    Bybit,
    Kucoin,
    Hyperliquid,
}

/// Native intervals in seconds with the venue's name for them. Only
/// epoch-aligned ones (weekly and longer candles are not).
const BINANCE_INTERVALS: &[(i64, &str)] = &[
    (60, "1m"),
    (180, "3m"),
    (300, "5m"),
    (900, "15m"),
    (1800, "30m"),
    (3600, "1h"),
    (7200, "2h"),
    (14400, "4h"),
    (21600, "6h"),
    (28800, "8h"),
    (43200, "12h"),
    (86400, "1d"),
];
const GATE_INTERVALS: &[(i64, &str)] = &[
    (10, "10s"),
    (30, "30s"),
    (60, "1m"),
    (180, "3m"),
    (300, "5m"),
    (900, "15m"),
    (1800, "30m"),
    (3600, "1h"),
    (7200, "2h"),
    (14400, "4h"),
    (21600, "6h"),
    (28800, "8h"),
    (43200, "12h"),
    (86400, "1d"),
];
const BYBIT_INTERVALS: &[(i64, &str)] = &[
    (60, "1"),
    (180, "3"),
    (300, "5"),
    (900, "15"),
    (1800, "30"),
    (3600, "60"),
    (7200, "120"),
    (14400, "240"),
    (21600, "360"),
    (43200, "720"),
    (86400, "D"),
];
const KUCOIN_INTERVALS: &[(i64, &str)] = &[
    (60, "1"),
    (180, "3"),
    (300, "5"),
    (900, "15"),
    (1800, "30"),
    (3600, "60"),
    (7200, "120"),
    (14400, "240"),
    (28800, "480"),
    (43200, "720"),
    (86400, "1440"),
];
const HYPERLIQUID_INTERVALS: &[(i64, &str)] = &[
    (60, "1m"),
    (180, "3m"),
    (300, "5m"),
    (900, "15m"),
    (1800, "30m"),
    (3600, "1h"),
    (7200, "2h"),
    (14400, "4h"),
    (28800, "8h"),
    (43200, "12h"),
    (86400, "1d"),
];

impl Venue {
    fn from_name(name: &str) -> Option<Venue> {
        Some(match name {
            "binance_futures" => Venue::Binance,
            "aster_futures" => Venue::Aster,
            "gate_futures" => Venue::Gate,
            "bybit_futures" => Venue::Bybit,
            "kucoin_futures" => Venue::Kucoin,
            "hyperliquid" => Venue::Hyperliquid,
            _ => return None,
        })
    }

    fn intervals(self) -> &'static [(i64, &'static str)] {
        match self {
            Venue::Binance | Venue::Aster => BINANCE_INTERVALS,
            Venue::Gate => GATE_INTERVALS,
            Venue::Bybit => BYBIT_INTERVALS,
            Venue::Kucoin => KUCOIN_INTERVALS,
            Venue::Hyperliquid => HYPERLIQUID_INTERVALS,
        }
    }

    /// Candles one request returns at most.
    fn page_size(self) -> i64 {
        match self {
            Venue::Binance | Venue::Aster => 1500,
            Venue::Gate => 2000,
            Venue::Bybit => 1000,
            Venue::Kucoin => 200,
            Venue::Hyperliquid => 5000,
        }
    }

    /// How many of the latest candles the venue keeps, when limited.
    fn history(self) -> Option<i64> {
        match self {
            // 10000, less a margin for the clock moving between requests.
            Venue::Gate => Some(9990),
            Venue::Hyperliquid => Some(5000),
            _ => None,
        }
    }

    /// Largest native interval dividing ours: (seconds, venue name).
    fn native_for(self, interval: Interval) -> Option<(i64, &'static str)> {
        let secs = interval.seconds();
        self.intervals()
            .iter()
            .rev()
            .find(|(n, _)| secs % n == 0)
            .copied()
    }

    fn parse(self, status: u16, body: &str) -> Result<Vec<RawCandle>, String> {
        match self {
            Venue::Binance | Venue::Aster => parse_binance(status, body),
            Venue::Gate => parse_gate(status, body),
            Venue::Bybit => parse_bybit(status, body),
            Venue::Kucoin => parse_kucoin(status, body),
            Venue::Hyperliquid => parse_hyperliquid(status, body),
        }
    }
}

/// Candles with start in [from, to] (unix seconds, inclusive), oldest first, at most `max` (newest kept when trimming).
/// `exchange` is one of: binance_futures, gate_futures, bybit_futures, kucoin_futures, aster_futures, hyperliquid.
/// `venue_symbol` is the exchange's own instrument id, already mapped by the caller:
///   binance/bybit/aster: "BTCUSDT"; gate: "BTC_USDT"; kucoin: "XBTUSDTM"; hyperliquid: coin as listed, case-sensitive, e.g. "BTC", "kPEPE".
pub async fn fetch(
    http: &reqwest::Client,
    exchange: &str,
    venue_symbol: &str,
    interval: Interval,
    from: i64,
    to: i64,
    max: usize,
) -> Result<Vec<RawCandle>, String> {
    let venue = Venue::from_name(exchange).ok_or_else(|| format!("unknown exchange {exchange}"))?;
    let Some((native, native_name)) = venue.native_for(interval) else {
        return Ok(Vec::new());
    };
    let now = Utc::now().timestamp();
    let Some((first, last)) = window(venue, interval, native, from, to, now, max) else {
        return Ok(Vec::new());
    };
    let mut natives = Vec::new();
    for (i, (start, end)) in pages(first, last, native, venue.page_size())
        .into_iter()
        .enumerate()
    {
        if i > 0 {
            tokio::time::sleep(PAGE_PAUSE).await;
        }
        let (status, body) = request(http, venue, venue_symbol, native_name, start, end)
            .await
            .map_err(|e| format!("{exchange}: {e}"))?;
        let page = venue
            .parse(status, &body)
            .map_err(|e| format!("{exchange}: {e}"))?;
        if page.is_empty() {
            // Nothing older: an unknown symbol or the start of its history.
            break;
        }
        natives.extend(
            page.into_iter()
                .filter(|c| c.start >= start && c.start <= end),
        );
    }
    let mut out: Vec<RawCandle> = aggregate(natives, interval, first)
        .into_iter()
        .filter(|c| c.start >= from && c.start <= to)
        .collect();
    if out.len() > max {
        out.drain(..out.len() - max);
    }
    Ok(out)
}

/// First and last native candle start to fetch, both inclusive. Leading
/// buckets are only included whole; the last bucket is fetched to its end.
fn window(
    venue: Venue,
    interval: Interval,
    native: i64,
    from: i64,
    to: i64,
    now: i64,
    max: usize,
) -> Option<(i64, i64)> {
    let iv = interval.seconds();
    let to = to.min(now);
    if max == 0 || to < from {
        return None;
    }
    let max = i64::try_from(max).unwrap_or(i64::MAX / iv);
    let last_bucket = to.div_euclid(iv) * iv;
    let mut first = ceil_to(from, iv).max(last_bucket.saturating_sub((max - 1).saturating_mul(iv)));
    let last = (last_bucket + iv - native).min(now.div_euclid(native) * native);
    let mut earliest = last - (MAX_PAGES as i64 * venue.page_size() - 1) * native;
    if let Some(points) = venue.history() {
        earliest = earliest.max(now.div_euclid(native) * native - (points - 1) * native);
    }
    first = first.max(ceil_to(earliest, iv));
    (first <= last).then_some((first, last))
}

fn ceil_to(ts: i64, step: i64) -> i64 {
    -((-ts).div_euclid(step) * step)
}

/// Request ranges (inclusive), newest first, at most `MAX_PAGES`.
fn pages(first: i64, last: i64, native: i64, page_size: i64) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    let mut end = last;
    while end >= first && out.len() < MAX_PAGES {
        let start = (end - (page_size - 1) * native).max(first);
        out.push((start, end));
        end = start - native;
    }
    out
}

async fn request(
    http: &reqwest::Client,
    venue: Venue,
    symbol: &str,
    interval: &str,
    start: i64,
    end: i64,
) -> Result<(u16, String), String> {
    let (start_ms, end_ms) = (start * 1000, end * 1000);
    let req = match venue {
        Venue::Binance | Venue::Aster => {
            let host = if venue == Venue::Binance {
                "https://fapi.binance.com"
            } else {
                "https://fapi.asterdex.com"
            };
            http.get(format!("{host}/fapi/v1/klines")).query(&[
                ("symbol", symbol.to_string()),
                ("interval", interval.to_string()),
                ("startTime", start_ms.to_string()),
                ("endTime", end_ms.to_string()),
                ("limit", venue.page_size().to_string()),
            ])
        }
        Venue::Gate => http
            .get("https://api.gateio.ws/api/v4/futures/usdt/candlesticks")
            .query(&[
                ("contract", format!("mark_{symbol}")),
                ("interval", interval.to_string()),
                ("from", start.to_string()),
                ("to", end.to_string()),
            ]),
        Venue::Bybit => http.get("https://api.bybit.com/v5/market/kline").query(&[
            ("category", "linear".to_string()),
            ("symbol", symbol.to_string()),
            ("interval", interval.to_string()),
            ("start", start_ms.to_string()),
            ("end", end_ms.to_string()),
            ("limit", venue.page_size().to_string()),
        ]),
        Venue::Kucoin => http
            .get("https://api-futures.kucoin.com/api/v1/kline/query")
            .query(&[
                ("symbol", symbol.to_string()),
                ("granularity", interval.to_string()),
                ("from", start_ms.to_string()),
                ("to", end_ms.to_string()),
            ]),
        Venue::Hyperliquid => http.post("https://api.hyperliquid.xyz/info").json(&json!({
            "type": "candleSnapshot",
            "req": {"coin": symbol, "interval": interval, "startTime": start_ms, "endTime": end_ms},
        })),
    };
    let resp = req
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let body = resp.text().await.map_err(|e| e.to_string())?;
    Ok((status, body))
}

/// Our buckets from native candles in any order. Buckets starting before
/// `first` are dropped (their earlier parts were not fetched).
fn aggregate(mut natives: Vec<RawCandle>, interval: Interval, first: i64) -> Vec<RawCandle> {
    natives.sort_by_key(|c| c.start);
    natives.dedup_by_key(|c| c.start);
    let mut out: Vec<RawCandle> = Vec::new();
    for c in natives {
        let Some(ts) = Utc.timestamp_opt(c.start, 0).single() else {
            continue;
        };
        let bucket = interval.bucket_start(ts);
        if bucket < first {
            continue;
        }
        match out.last_mut() {
            Some(b) if b.start == bucket => {
                b.high = b.high.max(c.high);
                b.low = b.low.min(c.low);
                b.close = c.close;
                b.volume += c.volume;
            }
            _ => out.push(RawCandle { start: bucket, ..c }),
        }
    }
    out
}

/// A volume field: zero is valid, missing or bad counts as zero.
fn volume(v: Option<&Value>) -> f64 {
    let x = match v {
        Some(Value::String(s)) => s.parse::<f64>().unwrap_or(0.0),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    };
    if x.is_finite() && x > 0.0 {
        x
    } else {
        0.0
    }
}

fn ms_to_s(v: Option<&Value>) -> Option<i64> {
    let ms = match v? {
        Value::String(s) => s.parse::<i64>().ok()?,
        Value::Number(n) => n.as_i64()?,
        _ => return None,
    };
    Some(ms.div_euclid(1000))
}

fn candle(
    start: Option<i64>,
    o: Option<&Value>,
    h: Option<&Value>,
    l: Option<&Value>,
    c: Option<&Value>,
    volume: f64,
) -> Option<RawCandle> {
    Some(RawCandle {
        start: start?,
        open: num(o)?,
        high: num(h)?,
        low: num(l)?,
        close: num(c)?,
        volume,
    })
}

fn json_body(status: u16, body: &str) -> Result<Value, String> {
    serde_json::from_str(body).map_err(|_| format!("HTTP {status}: {}", short(body)))
}

fn short(body: &str) -> String {
    body.chars().take(200).collect()
}

fn rows(v: &Value, status: u16, body: &str) -> Result<Vec<Value>, String> {
    v.as_array()
        .cloned()
        .ok_or_else(|| format!("HTTP {status}: {}", short(body)))
}

/// Binance and Aster klines: `[openTime, o, h, l, c, vol, closeTime, quoteVol, ...]`.
fn parse_binance(status: u16, body: &str) -> Result<Vec<RawCandle>, String> {
    let v = json_body(status, body)?;
    if v.get("code").and_then(Value::as_i64) == Some(-1121) {
        return Ok(Vec::new());
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {}", short(body)));
    }
    Ok(rows(&v, status, body)?
        .iter()
        .filter_map(|r| {
            candle(
                ms_to_s(r.get(0)),
                r.get(1),
                r.get(2),
                r.get(3),
                r.get(4),
                volume(r.get(7)),
            )
        })
        .collect())
}

/// Gate mark-price candlesticks: `{t, o, h, l, c, v, sum}`; volume is zero.
fn parse_gate(status: u16, body: &str) -> Result<Vec<RawCandle>, String> {
    let v = json_body(status, body)?;
    if v.get("label").and_then(Value::as_str) == Some("CONTRACT_NOT_FOUND") {
        return Ok(Vec::new());
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {}", short(body)));
    }
    Ok(rows(&v, status, body)?
        .iter()
        .filter_map(|r| {
            candle(
                r.get("t").and_then(Value::as_i64),
                r.get("o"),
                r.get("h"),
                r.get("l"),
                r.get("c"),
                0.0,
            )
        })
        .collect())
}

/// Bybit v5 kline: `[start, o, h, l, c, volume, turnover]`, newest first.
fn parse_bybit(status: u16, body: &str) -> Result<Vec<RawCandle>, String> {
    let v = json_body(status, body)?;
    match v.get("retCode").and_then(Value::as_i64) {
        Some(0) if (200..300).contains(&status) => {}
        Some(10001)
            if v.get("retMsg")
                .and_then(Value::as_str)
                .is_some_and(|m| m.contains("Symbol Is Invalid")) =>
        {
            return Ok(Vec::new())
        }
        _ => return Err(format!("HTTP {status}: {}", short(body))),
    }
    Ok(rows(&v["result"]["list"], status, body)?
        .iter()
        .filter_map(|r| {
            candle(
                ms_to_s(r.get(0)),
                r.get(1),
                r.get(2),
                r.get(3),
                r.get(4),
                volume(r.get(6)),
            )
        })
        .collect())
}

/// KuCoin futures kline: `[time, o, h, l, c, volume (lots), turnover]`.
fn parse_kucoin(status: u16, body: &str) -> Result<Vec<RawCandle>, String> {
    let v = json_body(status, body)?;
    if v.get("code").and_then(Value::as_str) != Some("200000") || !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {}", short(body)));
    }
    Ok(rows(&v["data"], status, body)?
        .iter()
        .filter_map(|r| {
            candle(
                ms_to_s(r.get(0)),
                r.get(1),
                r.get(2),
                r.get(3),
                r.get(4),
                volume(r.get(6)),
            )
        })
        .collect())
}

/// Hyperliquid candleSnapshot: `{t, o, h, l, c, v (coins)}`; quote volume
/// is approximated as v * close.
fn parse_hyperliquid(status: u16, body: &str) -> Result<Vec<RawCandle>, String> {
    if status == 500 && body.trim() == "null" {
        return Ok(Vec::new());
    }
    let v = json_body(status, body)?;
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {}", short(body)));
    }
    Ok(rows(&v, status, body)?
        .iter()
        .filter_map(|r| {
            let mut c = candle(
                ms_to_s(r.get("t")),
                r.get("o"),
                r.get("h"),
                r.get("l"),
                r.get("c"),
                0.0,
            )?;
            c.volume = volume(r.get("v")) * c.close;
            Some(c)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc(start: i64, o: f64, h: f64, l: f64, c: f64, v: f64) -> RawCandle {
        RawCandle {
            start,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: v,
        }
    }

    #[test]
    fn parses_binance_and_aster() {
        let body = r#"[[1791964920000,"83648.50","83693.30","83648.40","83680.10","130.160",1791964979999,"10891096.20240",2941,"68.763","5753684.04030","0"],[1791964980000,"83680.10","83706.00","83680.00","83706.00","66.449",1791965039999,"5561494.77780",758,"61.777","5170437.59960","0"]]"#;
        let got = parse_binance(200, body).unwrap();
        assert_eq!(
            got[0],
            rc(
                1791964920,
                83648.5,
                83693.3,
                83648.4,
                83680.1,
                10891096.2024
            )
        );
        assert_eq!(got[1].start, 1791964980);
        let unknown = r#"{"code":-1121,"msg":"Invalid symbol."}"#;
        assert_eq!(parse_binance(400, unknown).unwrap(), vec![]);
        let bad = r#"{"code":-1120,"msg":"Invalid interval."}"#;
        assert!(parse_binance(400, bad).is_err());
        assert!(parse_binance(502, "<html>").is_err());
    }

    #[test]
    fn parses_gate_mark() {
        let body = r#"[{"o":"83654.9","v":0,"t":1791964920,"c":"83683.83","l":"83654.9","h":"83692.3","sum":"0"},{"o":"83684.7","v":0,"t":1791964980,"c":"83738.47","l":"83684.7","h":"83742.2","sum":"0"}]"#;
        let got = parse_gate(200, body).unwrap();
        assert_eq!(
            got[0],
            rc(1791964920, 83654.9, 83692.3, 83654.9, 83683.83, 0.0)
        );
        assert_eq!(got.len(), 2);
        assert_eq!(
            parse_gate(400, r#"{"label":"CONTRACT_NOT_FOUND"}"#).unwrap(),
            vec![]
        );
        let old = r#"{"label":"INVALID_PARAM_VALUE","message":"Candlestick too long ago. Maximum 10000 points recently are allowed"}"#;
        assert!(parse_gate(400, old).is_err());
    }

    #[test]
    fn parses_bybit() {
        let body = r#"{"retCode":0,"retMsg":"OK","result":{"symbol":"BTCUSDT","category":"linear","list":[["1791965040000","83671.7","83734.9","83653.3","83719","39.719","3324646.4004"],["1791964980000","83678.6","83784.2","83671.7","83671.7","205.487","17205549.7775"]]},"retExtInfo":{},"time":1791965099568}"#;
        let got = parse_bybit(200, body).unwrap();
        assert_eq!(
            got[1],
            rc(
                1791964980,
                83678.6,
                83784.2,
                83671.7,
                83671.7,
                17205549.7775
            )
        );
        let unknown = r#"{"retCode":10001,"retMsg":"params error: Symbol Is Invalid","result":{},"retExtInfo":{},"time":1791965099813}"#;
        assert_eq!(parse_bybit(200, unknown).unwrap(), vec![]);
        let other = r#"{"retCode":10006,"retMsg":"Too many visits!","result":{}}"#;
        assert!(parse_bybit(200, other).is_err());
    }

    #[test]
    fn parses_kucoin() {
        let body = r#"{"code":"200000","data":[[1791964980000,83681.9,83771.2,83680.3,83680.3,3612,302430.9539],[1791965040000,83680.3,83727.6,83680.3,83711.6,623,52153.4454]]}"#;
        let got = parse_kucoin(200, body).unwrap();
        assert_eq!(
            got[0],
            rc(1791964980, 83681.9, 83771.2, 83680.3, 83680.3, 302430.9539)
        );
        assert_eq!(
            parse_kucoin(200, r#"{"code":"200000","data":[]}"#).unwrap(),
            vec![]
        );
        // Without turnover the volume is zero.
        let short = r#"{"code":"200000","data":[[1791964980000,1.0,2.0,0.5,1.5,10]]}"#;
        assert_eq!(parse_kucoin(200, short).unwrap()[0].volume, 0.0);
        let bad = r#"{"msg":"Unsupported granularity","code":"300000"}"#;
        assert!(parse_kucoin(200, bad).is_err());
    }

    #[test]
    fn parses_hyperliquid() {
        let body = r#"[{"t":1791964980000,"T":1791965039999,"s":"BTC","i":"1m","o":"83689.0","c":"83670.0","h":"83771.0","l":"83670.0","v":"2.0","n":626}]"#;
        let got = parse_hyperliquid(200, body).unwrap();
        assert_eq!(
            got[0],
            rc(1791964980, 83689.0, 83771.0, 83670.0, 83670.0, 167340.0)
        );
        assert_eq!(parse_hyperliquid(500, "null").unwrap(), vec![]);
        let bad = "Failed to deserialize the JSON body into the target type";
        assert!(parse_hyperliquid(422, bad).is_err());
    }

    #[test]
    fn picks_largest_dividing_native() {
        let n = |v: Venue, i: Interval| v.native_for(i);
        assert_eq!(n(Venue::Binance, Interval::M45), Some((900, "15m")));
        assert_eq!(n(Venue::Binance, Interval::M4), Some((60, "1m")));
        assert_eq!(n(Venue::Binance, Interval::M10), Some((300, "5m")));
        assert_eq!(n(Venue::Binance, Interval::H3), Some((3600, "1h")));
        assert_eq!(n(Venue::Binance, Interval::W1), Some((86400, "1d")));
        assert_eq!(n(Venue::Binance, Interval::S10), None);
        assert_eq!(n(Venue::Gate, Interval::S10), Some((10, "10s")));
        assert_eq!(n(Venue::Gate, Interval::S30), Some((30, "30s")));
        assert_eq!(n(Venue::Gate, Interval::M2), Some((60, "1m")));
        assert_eq!(n(Venue::Kucoin, Interval::H6), Some((7200, "120")));
        assert_eq!(n(Venue::Hyperliquid, Interval::H6), Some((7200, "2h")));
        assert_eq!(n(Venue::Bybit, Interval::H6), Some((21600, "360")));
        assert_eq!(n(Venue::Bybit, Interval::D1), Some((86400, "D")));
        assert_eq!(n(Venue::Hyperliquid, Interval::S30), None);
    }

    #[test]
    fn aggregates_into_epoch_buckets() {
        // 1m candles 12:02..12:09 into 4m: 12:00 is partial (starts before
        // the fetched range) and dropped; 12:04 is whole; 12:08 is open.
        let base = 1791964800; // multiple of 240
        let natives: Vec<RawCandle> = (2..10)
            .rev()
            .map(|i| {
                let p = i as f64;
                rc(base + i * 60, p, p + 0.5, p - 0.5, p + 0.1, 1.0)
            })
            .collect();
        let got = aggregate(natives, Interval::M4, base + 120);
        assert_eq!(
            got,
            vec![
                rc(base + 240, 4.0, 7.5, 3.5, 7.1, 4.0),
                rc(base + 480, 8.0, 9.5, 7.5, 9.1, 2.0),
            ]
        );
    }

    #[test]
    fn weeks_start_on_thursday() {
        // 1970-01-01 was a Thursday, so epoch-aligned weeks start on one.
        let thu = 1791417600;
        assert_eq!(thu % 604800, 0);
        let days: Vec<RawCandle> = (-3..7)
            .map(|d| rc(thu + d * 86400, 10.0, 11.0 + d as f64, 9.0, 10.5, 1.0))
            .collect();
        let got = aggregate(days, Interval::W1, thu);
        assert_eq!(got, vec![rc(thu, 10.0, 17.0, 9.0, 10.5, 7.0)]);
    }

    #[test]
    fn window_and_pages() {
        let now = 1791965130; // 12:05:30 within a 1m bucket
                              // Last 10 x 4m: whole buckets, fetched to the current minute.
        let (first, last) = window(Venue::Binance, Interval::M4, 60, 0, now, now, 10).unwrap();
        assert_eq!(first % 240, 0);
        assert_eq!(last, now / 60 * 60);
        assert_eq!((now / 240 * 240 - first) / 240, 9);
        // `from` inside a bucket starts at the next whole one.
        let (first, _) =
            window(Venue::Binance, Interval::M4, 60, now - 1000, now, now, 100).unwrap();
        assert_eq!(first, (now - 1000) / 240 * 240 + 240);
        // KuCoin pages are capped at 4 x 200.
        let (first, last) = window(Venue::Kucoin, Interval::M1, 60, 0, now, now, 5000).unwrap();
        assert_eq!((last - first) / 60 + 1, 800);
        let p = pages(first, last, 60, 200);
        assert_eq!(p.len(), 4);
        assert_eq!(p[0].1, last);
        assert_eq!(p[3].0, first);
        assert_eq!(p[0].0 - p[1].1, 60);
        // Gate keeps 10000 points only.
        let (first, _) = window(Venue::Gate, Interval::D1, 86400, 0, now, now, 1_000_000).unwrap();
        assert!(first >= now - 10000 * 86400);
        // A past `to` fetches its whole last bucket.
        let (_, last) =
            window(Venue::Bybit, Interval::H1, 900, 0, now - 7200 + 60, now, 5).unwrap();
        assert_eq!(last, (now - 7200 + 60) / 3600 * 3600 + 2700);
        assert_eq!(
            window(Venue::Bybit, Interval::H1, 900, now, now - 10, now, 5),
            None
        );
        assert_eq!(
            window(Venue::Bybit, Interval::H1, 900, 0, now, now, 0),
            None
        );
    }

    fn sane(ex: &str, c: &[RawCandle], step: i64, volume: bool) {
        assert!(!c.is_empty());
        // KuCoin has no candle for a minute without trades, even on BTC.
        let gaps_ok = ex.starts_with("kucoin");
        for w in c.windows(2) {
            let d = w[1].start - w[0].start;
            if gaps_ok {
                assert!(d > 0 && d % step == 0, "{ex}: bad step at {}", w[0].start);
            } else {
                assert_eq!(d, step, "{ex}: gap at {}", w[0].start);
            }
        }
        for k in c {
            assert_eq!(k.start % step, 0);
            assert!(k.low > 0.0 && k.low <= k.open.min(k.close) && k.high >= k.open.max(k.close));
            assert!(k.volume >= 0.0);
        }
        if volume {
            assert!(c.iter().map(|k| k.volume).sum::<f64>() > 0.0);
        }
    }

    #[tokio::test]
    #[ignore]
    async fn live_btc_on_every_venue() {
        let http = reqwest::Client::new();
        let now = Utc::now().timestamp();
        for (ex, sym) in [
            ("binance_futures", "BTCUSDT"),
            ("aster_futures", "BTCUSDT"),
            ("gate_futures", "BTC_USDT"),
            ("bybit_futures", "BTCUSDT"),
            ("kucoin_futures", "XBTUSDTM"),
            ("hyperliquid", "BTC"),
        ] {
            let c = fetch(&http, ex, sym, Interval::M1, now - 3600, now, 60)
                .await
                .unwrap_or_else(|e| panic!("{ex}: {e}"));
            assert!(c.len() >= 55, "{ex}: {} candles", c.len());
            assert!(c.len() <= 60);
            sane(ex, &c, 60, ex != "gate_futures");
            assert!(now - c[c.len() - 1].start < 180, "{ex}: stale");
        }
        // Paging (KuCoin: 200 per call) and aggregation.
        let c = fetch(
            &http,
            "kucoin_futures",
            "XBTUSDTM",
            Interval::M1,
            0,
            now,
            500,
        )
        .await
        .unwrap();
        assert!(
            c.len() >= 495 && c.len() <= 500,
            "kucoin paging: {}",
            c.len()
        );
        sane("kucoin 1m", &c, 60, true);
        let c = fetch(
            &http,
            "binance_futures",
            "BTCUSDT",
            Interval::M45,
            0,
            now,
            40,
        )
        .await
        .unwrap();
        assert_eq!(c.len(), 40);
        sane("binance 45m", &c, 2700, true);
        let c = fetch(&http, "gate_futures", "BTC_USDT", Interval::W1, 0, now, 5)
            .await
            .unwrap();
        assert_eq!(c.len(), 5);
        sane("gate 1W", &c, 604800, false);
        let c = fetch(
            &http,
            "gate_futures",
            "BTC_USDT",
            Interval::S10,
            now - 600,
            now,
            30,
        )
        .await
        .unwrap();
        assert!(c.len() >= 28 && c.len() <= 30, "gate 10s: {}", c.len());
        sane("gate 10s", &c, 10, false);
        assert!(
            fetch(&http, "bybit_futures", "BTCUSDT", Interval::S10, 0, now, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(fetch(
            &http,
            "binance_futures",
            "NOPEXYZUSDT",
            Interval::M1,
            0,
            now,
            10
        )
        .await
        .unwrap()
        .is_empty());
    }
}
