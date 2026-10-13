# pump-detector (Rust)

Rewrite of the Python service in `app/` (2026-10-13). Same URL, routes, WebSocket protocol, Mongo collections and env names; much less memory and CPU (on the Hetzner box: ~20 MB and ~3% CPU against the Python service's growing heap and ~85% CPU).

## What it does
- **Prices** from six futures exchanges (`src/feeds/`). Binance (`!miniTicker@arr`) and Gate (`futures.tickers` per contract, mark price) stream every market. Bybit, KuCoin, Aster and Hyperliquid connect only while someone watches a symbol (a `/ws/price` client or a trailing position) and close a minute after the last one leaves. Ten failed connects in a row pause a feed for a day.
- **Candles** for `CANDLE_EXCHANGES` (default Binance and Gate), every interval built straight from ticks (`src/candles.rs`): one open candle per market and interval in memory, completed when the first tick of a later bucket arrives. Written to Mongo `candles` in the background; candles older than 100 intervals are deleted every 10 minutes.
- **Alerts** on completed 5m/15m/30m/1h/6h/12h/1D candles (`src/alerts.rs`): a 5% move is stored in `detected_alerts`; 10% is stored and POSTed to `WEBHOOK_URL` with `x-api-key: WEBHOOK_API_KEY`. Custom alerts from `POST /alerts` (`spike_alerts`) use their own threshold and webhook.
- **Trailing take profit** (`src/trailing.rs`): the Trigger worker PUTs the full list of open trailing positions to `/trailing/positions` every second. Each price tick of a watched market runs the rule (no TP order; once price moved `activationPercent` in the trade's favour, close when it falls `distancePercent` from the best price). A close is a POST to `TRAILING_CLOSE_URL` (the api's `/api/internal/position-trailing/close`, `x-api-key: TRAILING_API_KEY`, default `WEBHOOK_API_KEY`); the api claims the row first, so a repeat never closes twice, and a failed request may be repeated after 5 s. The answer to each sync carries every position's state and best price, which the worker saves. Without a sync for 10 minutes the list is dropped. Without `TRAILING_CLOSE_URL` closes are only logged (dry run).

## Routes
`/health` (open), `/stats`, `/ws/price` (`x-api-key` header or `?key=`), `/candles`, `/price/{symbol}`, `/alerts` (GET/POST), `/alerts/{id}` (DELETE), `/alerts/detected`, `/webhook/test`, `/exchanges`, `/symbols/{exchange}`, `/trailing/positions` (PUT/GET), `/ui` and `/ui/api/*` (HTTP Basic `UI_USERNAME`/`UI_PASSWORD`). Everything except `/health` and `/ui` needs `x-api-key: API_KEY`.

## Env
`PORT` (8000), `MONGODB_URL`, `MONGODB_DB_NAME`, `API_KEY`, `UI_USERNAME`, `UI_PASSWORD`, `CANDLE_EXCHANGES`, `WEBHOOK_URL`, `WEBHOOK_API_KEY`, `WEBHOOK_TIMEOUT_SECONDS`, `TRAILING_CLOSE_URL`, `TRAILING_API_KEY`, `LOG_LEVEL` or `RUST_LOG`.

## Develop
```
cd rust
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```
Run locally against a throwaway Mongo (`docker run --rm -d -p 27099:27017 mongo:7`) with `MONGODB_URL=mongodb://localhost:27099 PORT=8099 cargo run`.

## Deploy (Hetzner)
Shadow next to the Python app (own database, no webhooks, closes only logged, `127.0.0.1:8001`):
`docker compose -f docker-compose.yml -f docker-compose.shadow.yml up -d --build app-rs`.
