//! Price gaps between exchanges for the same token (`/ui/gaps`): for every
//! token listed on two or more futures venues, the cheapest and the dearest
//! venue and how far apart they are. Built only from the prices the feeds
//! already hold; nothing here asks an exchange for anything.
//!
//! Venues quote some small-priced tokens per 1,000 or per million units
//! (`1000PEPEUSDT`, `1MBABYDOGEUSDT`, Hyperliquid `kPEPE`), so prices are
//! compared per single token. A gap of `max_pct` or more is almost always two
//! projects sharing a ticker (or a dead contract), not a price difference, so
//! those tokens are hidden and only counted.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::hub::Quote;

/// A venue whose feed has not ticked for this long is skipped.
const FEED_ALIVE_SECS: i64 = 30;
/// A last trade older than this gives way to a fresher mark.
const LAST_TRADE_MAX_AGE_SECS: i64 = 15;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Source {
    /// Last trade; the mark when the last trade is older than
    /// `LAST_TRADE_MAX_AGE_SECS` (a quiet market's last trade drifts away
    /// from the others and shows a gap that is not there).
    Last,
    /// Mark price, or the last trade when the venue has no fresh mark.
    Mark,
}

#[derive(Debug, Clone)]
pub struct Opts {
    /// Only these exchanges (all futures venues when empty).
    pub exchanges: Vec<String>,
    /// Gaps at or above this (percent) are hidden.
    pub max_pct: f64,
    pub source: Source,
    /// A price older than this (seconds) is not compared.
    pub max_age_secs: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct VenuePrice {
    pub exchange: String,
    /// The market's own symbol on that venue (`1000PEPEUSDT`).
    pub symbol: String,
    /// Price of one token.
    pub price: f64,
    pub age_seconds: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Gap {
    /// The token (`PEPE`).
    pub token: String,
    /// How far the dearest venue is above the cheapest, in percent.
    pub gap_pct: f64,
    pub low: VenuePrice,
    pub high: VenuePrice,
    /// Every compared venue, cheapest first.
    pub venues: Vec<VenuePrice>,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub rows: Vec<Gap>,
    /// Tokens priced on two or more venues.
    pub compared: usize,
    /// Tokens hidden because their gap is at or above `max_pct`.
    pub hidden: usize,
}

/// The token and how many tokens one contract price covers:
/// `1000PEPEUSDT` is (`PEPE`, 1000), Hyperliquid `kPEPE` (`KPEPEUSDT`) too.
pub fn token_and_scale(exchange: &str, symbol: &str, venue_symbol: Option<&str>) -> (String, f64) {
    let base = symbol.strip_suffix("USDT").unwrap_or(symbol);
    if exchange == "hyperliquid" {
        if let Some(rest) = venue_symbol.and_then(|v| v.strip_prefix('k')) {
            if rest.starts_with(|c: char| c.is_ascii_uppercase()) {
                return (rest.to_string(), 1000.0);
            }
        }
    }
    for (prefix, scale) in [
        ("1000000", 1e6),
        ("100000", 1e5),
        ("10000", 1e4),
        ("1000", 1e3),
        ("1M", 1e6),
    ] {
        if let Some(rest) = base.strip_prefix(prefix) {
            if rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
                return (rest.to_string(), scale);
            }
        }
    }
    (base.to_string(), 1.0)
}

fn pick(q: &Quote, source: Source, now: DateTime<Utc>, max_age: i64) -> Option<(f64, f64)> {
    let fresh = |x: Option<(f64, DateTime<Utc>)>| {
        x.filter(|(p, at)| p.is_finite() && *p > 0.0 && (now - *at).num_seconds() <= max_age)
    };
    let (p, at) = match source {
        Source::Last => q
            .last
            .filter(|(_, at)| (now - *at).num_seconds() <= LAST_TRADE_MAX_AGE_SECS)
            .and_then(|l| fresh(Some(l)))
            .or(fresh(q.mark))
            .or(fresh(q.last)),
        Source::Mark => fresh(q.mark).or(fresh(q.last)),
    }?;
    let age = (now - at).num_milliseconds().max(0) as f64 / 1000.0;
    Some((p, (age * 10.0).round() / 10.0))
}

/// Gaps for every token on two or more live venues, largest first.
/// `venue_symbol(exchange, symbol)` gives the venue's own id (for `kPEPE`).
pub fn gaps(
    quotes: &[((String, String), Quote)],
    last_tick: &HashMap<String, DateTime<Utc>>,
    venue_symbol: impl Fn(&str, &str) -> Option<String>,
    now: DateTime<Utc>,
    opts: &Opts,
) -> Report {
    let alive = |ex: &str| {
        last_tick
            .get(ex)
            .is_some_and(|t| (now - *t).num_seconds() <= FEED_ALIVE_SECS)
    };
    let mut per_token: HashMap<String, Vec<VenuePrice>> = HashMap::new();
    for ((ex, sym), q) in quotes {
        if !crate::feeds::FUTURES.contains(&ex.as_str()) || !alive(ex) {
            continue;
        }
        if !opts.exchanges.is_empty() && !opts.exchanges.iter().any(|e| e == ex) {
            continue;
        }
        let Some((price, age)) = pick(q, opts.source, now, opts.max_age_secs) else {
            continue;
        };
        let (token, scale) = token_and_scale(ex, sym, venue_symbol(ex, sym).as_deref());
        let list = per_token.entry(token).or_default();
        // Two contracts of one token on a venue (PEPEUSDT and 1000PEPEUSDT):
        // keep the first, a venue is one side of a trade.
        if list.iter().any(|v| v.exchange == *ex) {
            continue;
        }
        list.push(VenuePrice {
            exchange: ex.clone(),
            symbol: sym.clone(),
            price: price / scale,
            age_seconds: age,
        });
    }
    let mut report = Report::default();
    for (token, mut venues) in per_token {
        if venues.len() < 2 {
            continue;
        }
        report.compared += 1;
        venues.sort_by(|a, b| a.price.total_cmp(&b.price));
        let low = venues[0].clone();
        let high = venues[venues.len() - 1].clone();
        let gap_pct = (high.price / low.price - 1.0) * 100.0;
        if gap_pct >= opts.max_pct {
            report.hidden += 1;
            continue;
        }
        report.rows.push(Gap {
            token,
            gap_pct,
            low,
            high,
            venues,
        });
    }
    report.rows.sort_by(|a, b| {
        b.gap_pct
            .total_cmp(&a.gap_pct)
            .then_with(|| a.token.cmp(&b.token))
    });
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(last: Option<f64>, mark: Option<f64>, at: DateTime<Utc>) -> Quote {
        Quote {
            last: last.map(|p| (p, at)),
            mark: mark.map(|p| (p, at)),
        }
    }

    fn opts() -> Opts {
        Opts {
            exchanges: vec![],
            max_pct: 10.0,
            source: Source::Last,
            max_age_secs: 60,
        }
    }

    #[test]
    fn scaled_contracts_are_priced_per_token() {
        assert_eq!(
            token_and_scale("binance_futures", "1000PEPEUSDT", None),
            ("PEPE".into(), 1e3)
        );
        assert_eq!(
            token_and_scale("binance_futures", "1MBABYDOGEUSDT", None),
            ("BABYDOGE".into(), 1e6)
        );
        assert_eq!(
            token_and_scale("bybit_futures", "1000000MOGUSDT", None),
            ("MOG".into(), 1e6)
        );
        assert_eq!(
            token_and_scale("hyperliquid", "KPEPEUSDT", Some("kPEPE")),
            ("PEPE".into(), 1e3)
        );
        // Not a scale prefix.
        assert_eq!(
            token_and_scale("hyperliquid", "KAITOUSDT", Some("KAITO")),
            ("KAITO".into(), 1.0)
        );
        assert_eq!(
            token_and_scale("gate_futures", "1INCHUSDT", None),
            ("1INCH".into(), 1.0)
        );
        assert_eq!(
            token_and_scale("gate_futures", "1000USDT", None),
            ("1000".into(), 1.0)
        );
    }

    #[test]
    fn largest_gaps_first_and_same_name_projects_hidden() {
        let now = Utc::now();
        let ticks: HashMap<String, DateTime<Utc>> = [
            "binance_futures",
            "gate_futures",
            "bybit_futures",
            "hyperliquid",
        ]
        .into_iter()
        .map(|e| (e.to_string(), now))
        .chain([(
            "kucoin_futures".to_string(),
            now - chrono::Duration::seconds(120),
        )])
        .collect();
        let old = now - chrono::Duration::seconds(300);
        let m = |ex: &str, sym: &str| (ex.to_string(), sym.to_string());
        let quotes = vec![
            (m("binance_futures", "AAAUSDT"), q(Some(1.00), None, now)),
            (m("gate_futures", "AAAUSDT"), q(Some(1.02), None, now)),
            (m("bybit_futures", "AAAUSDT"), q(Some(1.01), None, now)),
            // Dead feed: ignored even though it would be the widest gap.
            (m("kucoin_futures", "AAAUSDT"), q(Some(1.09), None, now)),
            // 1000PEPE at 0.0105 per 1,000 vs kPEPE at 0.0100 per 1,000: 5 %.
            (
                m("binance_futures", "1000PEPEUSDT"),
                q(Some(0.0105), None, now),
            ),
            (m("hyperliquid", "KPEPEUSDT"), q(Some(0.0100), None, now)),
            // Same name, other project: hidden.
            (m("binance_futures", "TWOUSDT"), q(Some(1.0), None, now)),
            (m("gate_futures", "TWOUSDT"), q(Some(3.0), None, now)),
            // Old last trade falls back to a fresh mark.
            (m("binance_futures", "MRKUSDT"), q(Some(1.0), None, now)),
            (
                m("bybit_futures", "MRKUSDT"),
                Quote {
                    last: Some((2.0, old)),
                    mark: Some((1.001, now)),
                },
            ),
            // A quiet market's 20 s old last trade gives way to its mark.
            (m("gate_futures", "QUIUSDT"), q(Some(1.0), None, now)),
            (
                m("bybit_futures", "QUIUSDT"),
                Quote {
                    last: Some((1.08, now - chrono::Duration::seconds(20))),
                    mark: Some((1.002, now)),
                },
            ),
            // One venue only: not compared.
            (m("gate_futures", "ONEUSDT"), q(Some(1.0), None, now)),
            // Spot never counts.
            (m("binance_spot", "ONEUSDT"), q(Some(1.05), None, now)),
        ];
        let venue = |ex: &str, sym: &str| {
            (ex == "hyperliquid" && sym == "KPEPEUSDT").then(|| "kPEPE".to_string())
        };
        let r = gaps(&quotes, &ticks, venue, now, &opts());
        let tokens: Vec<&str> = r.rows.iter().map(|g| g.token.as_str()).collect();
        assert_eq!(tokens, vec!["PEPE", "AAA", "QUI", "MRK"]);
        assert_eq!((r.compared, r.hidden), (5, 1));
        let pepe = &r.rows[0];
        assert!((pepe.gap_pct - 5.0).abs() < 1e-9);
        assert_eq!(pepe.low.exchange, "hyperliquid");
        assert_eq!(pepe.high.symbol, "1000PEPEUSDT");
        assert!((pepe.high.price - 0.0000105).abs() < 1e-15);
        let aaa = &r.rows[1];
        assert_eq!(aaa.venues.len(), 3);
        assert_eq!(
            (aaa.low.exchange.as_str(), aaa.high.exchange.as_str()),
            ("binance_futures", "gate_futures")
        );
        assert!((r.rows[2].gap_pct - 0.2).abs() < 1e-9);
        assert!((r.rows[3].gap_pct - 0.1).abs() < 1e-9);

        // Only two exchanges: AAA is Binance vs Bybit now.
        let mut o = opts();
        o.exchanges = vec!["binance_futures".into(), "bybit_futures".into()];
        let r = gaps(&quotes, &ticks, |_: &str, _: &str| None, now, &o);
        let aaa = r.rows.iter().find(|g| g.token == "AAA").unwrap();
        assert!((aaa.gap_pct - 1.0).abs() < 1e-9);
        assert!(r.rows.iter().all(|g| g.token != "PEPE"));
    }
}
