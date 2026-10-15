//! Which markets each venue lists: USDT-margined perpetuals that are
//! trading, from each venue's public REST contract list. Feeds stream all of
//! them and refresh the list every few minutes so new listings join quickly.
//!
//! Our symbol is always `<BASE>USDT` in upper case; the venue id is what the
//! venue's own streams and REST use (Gate `BTC_USDT`, KuCoin `XBTUSDTM`,
//! Hyperliquid `BTC` or `kPEPE`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use serde::Serialize;
use serde_json::{json, Value};

/// How often contract lists are re-read.
pub const REFRESH: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MarketInfo {
    /// The venue's own id.
    pub venue_symbol: String,
    /// Contract size in base units (KuCoin lots), 1 elsewhere.
    #[serde(skip)]
    pub multiplier: f64,
}

pub type VenueMarkets = BTreeMap<String, MarketInfo>; // our symbol -> info

/// Shared registry: exchange -> our symbol -> venue market.
#[derive(Clone, Default)]
pub struct Registry(Arc<RwLock<HashMap<&'static str, VenueMarkets>>>);

impl Registry {
    pub fn set(&self, exchange: &'static str, markets: VenueMarkets) {
        self.0.write().insert(exchange, markets);
    }

    pub fn venue(&self, exchange: &str) -> VenueMarkets {
        self.0.read().get(exchange).cloned().unwrap_or_default()
    }

    pub fn get(&self, exchange: &str, symbol: &str) -> Option<MarketInfo> {
        self.0.read().get(exchange)?.get(symbol).cloned()
    }

    pub fn count(&self, exchange: &str) -> usize {
        self.0.read().get(exchange).map_or(0, BTreeMap::len)
    }

    /// Exchanges listing `symbol`.
    pub fn exchanges_for(&self, symbol: &str) -> Vec<(&'static str, MarketInfo)> {
        let r = self.0.read();
        let mut v: Vec<_> = r
            .iter()
            .filter_map(|(ex, m)| m.get(symbol).map(|i| (*ex, i.clone())))
            .collect();
        v.sort_by_key(|(ex, _)| *ex);
        v
    }
}

fn info(venue_symbol: &str) -> MarketInfo {
    MarketInfo {
        venue_symbol: venue_symbol.to_string(),
        multiplier: 1.0,
    }
}

pub fn binance_like(body: &Value) -> VenueMarkets {
    binance_list(body, true)
}

/// Binance-style exchangeInfo: USDT perpetuals, or USDT spot pairs.
pub fn binance_list(body: &Value, perpetual: bool) -> VenueMarkets {
    body.get("symbols")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| s.get("status").and_then(Value::as_str) == Some("TRADING"))
        .filter(|s| s.get("quoteAsset").and_then(Value::as_str) == Some("USDT"))
        .filter(|s| {
            !perpetual || s.get("contractType").and_then(Value::as_str) == Some("PERPETUAL")
        })
        .filter_map(|s| s.get("symbol")?.as_str())
        .map(|s| (s.to_uppercase(), info(s)))
        .collect()
}

pub fn bybit(body: &Value) -> VenueMarkets {
    bybit_list(body, true)
}

/// Bybit instruments: linear USDT perpetuals, or USDT spot pairs.
pub fn bybit_list(body: &Value, perpetual: bool) -> VenueMarkets {
    body.pointer("/result/list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| s.get("status").and_then(Value::as_str) == Some("Trading"))
        .filter(|s| s.get("quoteCoin").and_then(Value::as_str) == Some("USDT"))
        .filter(|s| {
            !perpetual || s.get("contractType").and_then(Value::as_str) == Some("LinearPerpetual")
        })
        .filter_map(|s| s.get("symbol")?.as_str())
        .map(|s| (s.to_uppercase(), info(s)))
        .collect()
}

pub fn kucoin(body: &Value) -> VenueMarkets {
    body.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| s.get("status").and_then(Value::as_str) == Some("Open"))
        .filter(|s| s.get("quoteCurrency").and_then(Value::as_str) == Some("USDT"))
        .filter(|s| s.get("type").and_then(Value::as_str) == Some("FFWCSX"))
        .filter(|s| s.get("isInverse").and_then(Value::as_bool) != Some(true))
        .filter_map(|s| {
            let id = s.get("symbol")?.as_str()?;
            let base = s.get("baseCurrency")?.as_str()?.to_uppercase();
            let base = if base == "XBT" {
                "BTC".to_string()
            } else {
                base
            };
            let multiplier = super::num(s.get("multiplier")).unwrap_or(1.0);
            Some((
                format!("{base}USDT"),
                MarketInfo {
                    venue_symbol: id.to_string(),
                    multiplier,
                },
            ))
        })
        .collect()
}

pub fn gate_spot(rows: &Value) -> VenueMarkets {
    rows.as_array()
        .into_iter()
        .flatten()
        .filter(|r| r.get("trade_status").and_then(Value::as_str) == Some("tradable"))
        .filter(|r| r.get("quote").and_then(Value::as_str) == Some("USDT"))
        .filter_map(|r| {
            let id = r.get("id")?.as_str()?;
            let base = r.get("base")?.as_str()?.to_uppercase();
            Some((format!("{base}USDT"), info(id)))
        })
        .collect()
}

pub fn kucoin_spot(body: &Value) -> VenueMarkets {
    body.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| s.get("enableTrading").and_then(Value::as_bool) == Some(true))
        .filter(|s| s.get("quoteCurrency").and_then(Value::as_str) == Some("USDT"))
        .filter_map(|s| {
            let id = s.get("symbol")?.as_str()?;
            let base = s.get("baseCurrency")?.as_str()?.to_uppercase();
            Some((format!("{base}USDT"), info(id)))
        })
        .collect()
}

pub fn hyperliquid(body: &Value) -> VenueMarkets {
    body.get("universe")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|a| a.get("isDelisted").and_then(Value::as_bool) != Some(true))
        .filter_map(|a| a.get("name")?.as_str())
        // Builder-deployed markets look like "dex:COIN"; main perps only.
        .filter(|n| !n.contains(':'))
        .map(|n| (format!("{}USDT", n.to_uppercase()), info(n)))
        .collect()
}

async fn get_json(http: &reqwest::Client, url: &str) -> Result<Value, String> {
    http.get(url)
        .timeout(Duration::from_secs(90))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.without_url().to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

/// The venue's current list. An empty list is an error (never wipe a
/// venue's markets because of a bad answer).
pub async fn fetch(http: &reqwest::Client, exchange: &str) -> Result<VenueMarkets, String> {
    let m = match exchange {
        "binance_futures" => {
            binance_like(&get_json(http, "https://fapi.binance.com/fapi/v1/exchangeInfo").await?)
        }
        "aster_futures" => {
            binance_like(&get_json(http, "https://fapi.asterdex.com/fapi/v1/exchangeInfo").await?)
        }
        "bybit_futures" => bybit(
            &get_json(
                http,
                "https://api.bybit.com/v5/market/instruments-info?category=linear&limit=1000",
            )
            .await?,
        ),
        "binance_spot" => binance_list(
            &get_json(
                http,
                "https://api.binance.com/api/v3/exchangeInfo?permissions=SPOT",
            )
            .await?,
            false,
        ),
        "bybit_spot" => bybit_list(
            &get_json(
                http,
                "https://api.bybit.com/v5/market/instruments-info?category=spot&limit=1000",
            )
            .await?,
            false,
        ),
        "gate_spot" => {
            gate_spot(&get_json(http, "https://api.gateio.ws/api/v4/spot/currency_pairs").await?)
        }
        "kucoin_spot" => {
            kucoin_spot(&get_json(http, "https://api.kucoin.com/api/v2/symbols").await?)
        }
        "kucoin_futures" => kucoin(
            &get_json(
                http,
                "https://api-futures.kucoin.com/api/v1/contracts/active",
            )
            .await?,
        ),
        "hyperliquid" => {
            let body = http
                .post("https://api.hyperliquid.xyz/info")
                .json(&json!({"type": "meta"}))
                .timeout(Duration::from_secs(90))
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| e.without_url().to_string())?
                .json::<Value>()
                .await
                .map_err(|e| e.to_string())?;
            hyperliquid(&body)
        }
        other => return Err(format!("unknown exchange {other}")),
    };
    if m.is_empty() {
        return Err("empty market list".into());
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lists() {
        let b: Value = serde_json::from_str(
            r#"{"symbols":[{"symbol":"BTCUSDT","status":"TRADING","quoteAsset":"USDT","contractType":"PERPETUAL"},
            {"symbol":"BTCUSDT_261225","status":"TRADING","quoteAsset":"USDT","contractType":"CURRENT_QUARTER"},
            {"symbol":"XUSDC","status":"TRADING","quoteAsset":"USDC","contractType":"PERPETUAL"},
            {"symbol":"OLDUSDT","status":"SETTLING","quoteAsset":"USDT","contractType":"PERPETUAL"}]}"#,
        )
        .unwrap();
        assert_eq!(binance_like(&b).keys().collect::<Vec<_>>(), vec!["BTCUSDT"]);

        let y: Value = serde_json::from_str(
            r#"{"result":{"list":[{"symbol":"0GUSDT","status":"Trading","quoteCoin":"USDT","contractType":"LinearPerpetual"},
            {"symbol":"BTC-26DEC26","status":"Trading","quoteCoin":"USDT","contractType":"LinearFutures"}]}}"#,
        )
        .unwrap();
        assert_eq!(bybit(&y).keys().collect::<Vec<_>>(), vec!["0GUSDT"]);

        let k: Value = serde_json::from_str(
            r#"{"data":[{"symbol":"XBTUSDTM","status":"Open","quoteCurrency":"USDT","type":"FFWCSX","baseCurrency":"XBT","multiplier":0.001,"isInverse":false},
            {"symbol":"XBTUSDM","status":"Open","quoteCurrency":"USD","type":"FFWCSX","baseCurrency":"XBT","multiplier":-1,"isInverse":true}]}"#,
        )
        .unwrap();
        let km = kucoin(&k);
        assert_eq!(km.len(), 1);
        assert_eq!(km["BTCUSDT"].venue_symbol, "XBTUSDTM");
        assert_eq!(km["BTCUSDT"].multiplier, 0.001);

        let h: Value = serde_json::from_str(
            r#"{"universe":[{"name":"BTC"},{"name":"kPEPE"},{"name":"OLD","isDelisted":true},{"name":"xyz:TSLA"}]}"#,
        )
        .unwrap();
        let hm = hyperliquid(&h);
        assert_eq!(hm.keys().collect::<Vec<_>>(), vec!["BTCUSDT", "KPEPEUSDT"]);
        assert_eq!(hm["KPEPEUSDT"].venue_symbol, "kPEPE");
    }

    #[test]
    fn parses_spot_lists() {
        let g: Value = serde_json::from_str(
            r#"[{"id":"PEPE_USDT","base":"PEPE","quote":"USDT","trade_status":"tradable"},
                {"id":"X_BTC","base":"X","quote":"BTC","trade_status":"tradable"},
                {"id":"Y_USDT","base":"Y","quote":"USDT","trade_status":"untradable"}]"#,
        )
        .unwrap();
        assert_eq!(gate_spot(&g)["PEPEUSDT"].venue_symbol, "PEPE_USDT");
        assert_eq!(gate_spot(&g).len(), 1);
        let k: Value = serde_json::from_str(
            r#"{"data":[{"symbol":"AVA-USDT","baseCurrency":"AVA","quoteCurrency":"USDT","enableTrading":true}]}"#,
        )
        .unwrap();
        assert_eq!(kucoin_spot(&k)["AVAUSDT"].venue_symbol, "AVA-USDT");
        let b: Value = serde_json::from_str(
            r#"{"symbols":[{"symbol":"BTCUSDT","status":"TRADING","quoteAsset":"USDT"}]}"#,
        )
        .unwrap();
        assert_eq!(binance_list(&b, false).len(), 1);
        assert_eq!(binance_list(&b, true).len(), 0);
    }

    #[test]
    fn registry_lookups() {
        let r = Registry::default();
        let mut m = VenueMarkets::new();
        m.insert("BTCUSDT".into(), info("XBTUSDTM"));
        r.set("kucoin_futures", m);
        assert_eq!(r.count("kucoin_futures"), 1);
        assert_eq!(r.exchanges_for("BTCUSDT")[0].0, "kucoin_futures");
        assert!(r.get("kucoin_futures", "ETHUSDT").is_none());
    }
}
