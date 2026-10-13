//! Settings from the environment (same names as the Python service, plus
//! the trailing ones).

use std::collections::HashSet;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub mongodb_url: String,
    pub mongodb_db_name: String,
    /// Where 10% alerts go (the Trigger api); unset = alerts stored only.
    pub webhook_url: Option<String>,
    pub webhook_api_key: Option<String>,
    pub webhook_timeout: Duration,
    /// Shared key for every endpoint except /health (`x-api-key`, or `?key=`
    /// on the WebSocket). Unset = open (local only).
    pub api_key: Option<String>,
    pub ui_username: String,
    pub ui_password: String,
    /// Exchanges that build candles and run alerts.
    pub candle_exchanges: HashSet<String>,
    /// The api's trailing close endpoint; unset = log closes only (dry run).
    pub trailing_close_url: Option<String>,
    /// x-api-key for the close endpoint (defaults to WEBHOOK_API_KEY).
    pub trailing_api_key: Option<String>,
}

fn var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl Config {
    pub fn from_env() -> Self {
        let webhook_api_key = var("WEBHOOK_API_KEY");
        Self {
            port: var("PORT").and_then(|p| p.parse().ok()).unwrap_or(8000),
            mongodb_url: var("MONGODB_URL").unwrap_or_else(|| "mongodb://localhost:27017".into()),
            mongodb_db_name: var("MONGODB_DB_NAME")
                .unwrap_or_else(|| "crypto_pump_detector".into()),
            webhook_url: var("WEBHOOK_URL"),
            webhook_timeout: Duration::from_secs(
                var("WEBHOOK_TIMEOUT_SECONDS")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(10),
            ),
            api_key: var("API_KEY"),
            ui_username: var("UI_USERNAME").unwrap_or_else(|| "admin".into()),
            ui_password: var("UI_PASSWORD").unwrap_or_else(|| "admin".into()),
            candle_exchanges: var("CANDLE_EXCHANGES")
                .unwrap_or_else(|| "binance_futures,gate_futures".into())
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            trailing_close_url: var("TRAILING_CLOSE_URL"),
            trailing_api_key: var("TRAILING_API_KEY").or_else(|| webhook_api_key.clone()),
            webhook_api_key,
        }
    }
}
