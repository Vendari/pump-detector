# pump-detector (Rust)

Rewrite of the Python service in `app/` (2026-10-13). Same URL, routes, WebSocket protocol, Mongo collections and env names; much less memory and CPU (on the Hetzner box: ~20 MB and ~3% CPU against the Python service's growing heap and ~85% CPU).

## What it does
- **Prices** of every USDT perpetual on six exchanges (`src/feeds/`), all the time: Binance (`!miniTicker@arr` + `!markPrice@arr@1s`), Gate (`futures.tickers`, mark and last), Bybit (`tickers`), KuCoin (`execution` + `instrument`), Aster (`aggTrade` + `!markPrice@arr@1s`) and Hyperliquid (`trades` + `activeAssetCtx`). Market lists come from each venue's REST contract list every 5 minutes (`feeds/markets.rs`); a symbol someone watches that the list lacks is streamed anyway, so a new listing is priced as soon as it is asked for. Bybit, KuCoin, Aster and Hyperliquid spread their markets over 4/5/4/2 connections. Ten failed connects in a row pause a connection for a day.
- **Spot** (since 2026-10-15): every USDT spot pair on Binance (`<sym>@miniTicker`), Bybit (`tickers`), Gate (`spot.tickers`) and KuCoin (`/market/ticker`), all the time, over 2/2/5/3 connections, with candles and backfill like futures. Exchange names `binance_spot`, `bybit_spot`, `gate_spot`, `kucoin_spot`; last price only (no mark). Spot never feeds order sizing (`/v1/marks` uses futures only) or alerts.
- **Main price vs last and mark.** Each market keeps its main price (what /ws/price, trailing, candles and alerts use: Gate's mark price, every other venue's last trade, unchanged from before) and, separately, the last and mark price for /v1 clients. Mark-only updates never move the main price.
- **Candles** for `CANDLE_EXCHANGES` (default all ten: six futures venues and four spot), every interval built straight from ticks (`src/candles.rs`). Written to Mongo `candles` in the background; kept for `CANDLE_RETENTION` intervals (default 1,000) from 1m up, an hour of 10s and two hours of 30s.
- **Backfill** (`src/backfill.rs`): when a chart asks for more candles than we have, the exchange's public kline REST fills the gap (at most once a minute per chart, 4 at a time; a chart waits up to 10 s for a free slot); candles we built ourselves win. Intervals a venue lacks are built from smaller ones.
- **Alerts** on completed 5m/15m/30m/1h/6h/12h/1D candles of `ALERT_EXCHANGES` (default Binance and Gate) (`src/alerts.rs`): a 5% move is stored in `detected_alerts`; 10% is stored and POSTed to `WEBHOOK_URL` with `x-api-key: WEBHOOK_API_KEY`. The api DMs users and can start price-alert autotrades from these, so add a venue to `ALERT_EXCHANGES` deliberately. Custom alerts from `POST /alerts` (`spike_alerts`) use their own threshold and webhook.
- **Trailing take profit** (`src/trailing.rs`): the Trigger worker PUTs the full list of open trailing positions to `/trailing/positions` every second. Each main-price tick of a watched market runs the rule (no TP order; once price moved `activationPercent` in the trade's favour, close when it falls `distancePercent` from the best price). A close is a POST to `TRAILING_CLOSE_URL` (the api's `/api/internal/position-trailing/close`, `x-api-key: TRAILING_API_KEY`, default `WEBHOOK_API_KEY`); the api claims the row first, so a repeat never closes twice, and a failed request may be repeated after 5 s. The answer to each sync carries every position's state and best price, which the worker saves. Without a sync for 10 minutes the list is dropped. Without `TRAILING_CLOSE_URL` closes are only logged (dry run).
- **/v1 for the Trigger api and apps** (`src/v1.rs`): see below.
- **/ui**: operator dashboard (feed health per exchange, every market with last/mark/spread/age, candle charts, recent alerts).

## /v1 (read-only)
- `GET /v1/markets[?exchange=…]` listed symbols per venue; `?symbol=BTC` the venues listing it, with `priced`.
- `GET /v1/prices?m=bybit_futures:BTCUSDT,gate_futures:ETHUSDT` (up to 200) or `?symbol=BTCUSDT` (every venue that has it): `{exchange, symbol, price, last, mark, ts (ms), age_seconds}`.
- `GET /v1/candles?exchange&symbol&interval=1m&limit=300[&end=<unix s>][&backfill=false]`: up to 1,000 candles `{time, open, high, low, close, volume}`, oldest first, the last one still open.
- `GET /v1/stream` WebSocket. Send `{"op":"subscribe","channel":"prices","markets":[{"exchange":"bybit_futures","symbol":"BTCUSDT"}]}` or `{"op":"subscribe","channel":"candles","exchange":"…","symbol":"…","interval":"1m"}` (same with `unsubscribe`; `{"op":"ping"}`). Pushes `{"type":"price",…}` / `{"type":"candle",…,"candle":{…}}` at most every 250 ms per subscription and only on change. At most 300 subscriptions per socket. A browser token's socket gets `{"type":"expired"}` and is closed when the token expires; reconnect with a fresh one.
- **Auth:** `x-api-key: API_KEY` (servers, no rate limit) or a browser token: `Authorization: Bearer <token>` (or `?token=` on the WebSocket). Tokens are `base64url(payload).base64url(HMAC-SHA256(PRICES_TOKEN_SECRET, base64url(payload)))`, payload `{"sub":"<user id>","exp":<unix s>,"scope":"prices"}`, at most 24 h; the Trigger api signs them (`src/token.rs`). Local test token: `PRICES_TOKEN_SECRET=… pump-detector mint-token <user> [seconds]`.
- **Browsers** must come from `CORS_ORIGINS` (default `https://app.trigger.fun,https://trigger.fun,http://localhost:5173`); a WebSocket with another Origin is refused. Per IP (last `X-Forwarded-For` entry, set by Caddy): 10 requests/s (burst 40) and 10 open sockets.
- No fallback: a market we have no price for is missing from the answer; nothing here fetches live prices from an exchange on demand.

## Routes
`/health` (open; `status` is healthy while Binance or Gate streams, `exchanges` lists every connection), `/stats`, `/ws/price` (`x-api-key` header or `?key=`), `/candles`, `/price/{symbol}`, `/alerts` (GET/POST), `/alerts/{id}` (DELETE), `/alerts/detected`, `/webhook/test`, `/exchanges`, `/symbols/{exchange}`, `/trailing/positions` (PUT/GET), `/v1/*` (above), `/ui` and `/ui/api/*` (HTTP Basic `UI_USERNAME`/`UI_PASSWORD`). Everything else needs `x-api-key: API_KEY`.

## Env
`PORT` (8000), `MONGODB_URL`, `MONGODB_DB_NAME`, `API_KEY`, `UI_USERNAME`, `UI_PASSWORD`, `CANDLE_EXCHANGES` (all ten), `ALERT_EXCHANGES` (Binance, Gate), `CANDLE_RETENTION` (1000), `PRICES_TOKEN_SECRET`, `CORS_ORIGINS`, `WEBHOOK_URL`, `WEBHOOK_API_KEY`, `WEBHOOK_TIMEOUT_SECONDS`, `TRAILING_CLOSE_URL`, `TRAILING_API_KEY`, `LOG_LEVEL` or `RUST_LOG`.

## Develop
```
cd rust
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```
Run locally against a throwaway Mongo (`docker run --rm -d -p 27099:27017 mongo:7`) with `MONGODB_URL=mongodb://localhost:27099 PORT=8099 cargo run`.

## Deploy (Hetzner)
Shadow next to the Python app (own database, no webhooks, closes only logged, `127.0.0.1:8001`):
`docker compose -f docker-compose.yml -f docker-compose.shadow.yml up -d --build app-rs`.
