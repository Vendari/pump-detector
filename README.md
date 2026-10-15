# Crypto Pump Detector

A high-performance cryptocurrency price monitoring system that detects price spikes and drops across multiple exchanges in real-time.

## Features

- **Candle-based storage**: OHLC candles (10s, 30s, 1m ... 1W), max 100 per type - efficient DB usage
- **Modular exchange support**: Easy to add new exchanges
- **Live price WebSocket**: Subscribe to real-time price updates for any symbol
- **REST API for candles**: Get historical candles by symbol, exchange, interval
- **Custom spike alerts**: Register webhooks with exchange, interval, and threshold
- **Per-exchange data**: No ticker ownership - each exchange stores its own prices

## Quick Start

```bash
# Start MongoDB
docker run -d -p 27017:27017 --name mongodb mongo:latest

# Configure
cp .env.example .env

# Run
python run.py
```

API docs: http://localhost:8000/docs

## API Overview

### Live Price (WebSocket)

```
wss://<host>/ws/price   (x-api-key header or ?key=)
```

Send `{"action": "subscribe", "symbol": "BTCUSDT", "exchange": "bybit_futures"}` once per market (several per connection; the latest known price comes back at once). `{"action": "unsubscribe", "symbol": ..., "exchange": ...}` drops one, `{"action": "unsubscribe"}` drops all.
Receive: `{"symbol": "BTCUSDT", "exchange": "bybit_futures", "price": 45000.5, "timestamp": "..."}`

Exchanges:
- `binance_futures` (all markets, last price from `!miniTicker@arr` on `/market/ws`), `gate_futures` (all markets): also build candles and spike alerts (`CANDLE_EXCHANGES`).
- `bybit_futures`, `kucoin_futures`, `aster_futures`, `hyperliquid`: live price only, connected on demand while someone watches the symbol (trade or ticker streams, sub-second).

Trigger's trail-rs (trailing take profit) uses this as its primary price source.

### Reconnects

A dropped exchange connection is retried every 5 s. After 10 failed attempts in a row the next attempt waits 24 h, so a broken feed never gets the server rate limited; any received price resets the count.

### Auth

`GET /v1/marks` (WebSocket, `API_KEY` only) serves the Trigger order services one reference price per token: a snapshot on connect, then a token only when its price moved `MARKS_PUSH_THRESHOLD_PCT` (default 2 %) since the last value sent, plus a heartbeat every 5 s. The reference is the highest fresh futures price across venues whose feed is alive; when the live venues are more than 25 % apart (one ticker for two projects, a frozen mark, a bad print, or one venue far ahead on a listing) each venue's own price is sent as `<exchange>:<token>` instead (e.g. `bybit_futures:PUFFERUSDT`). Clients look up `<exchange>:<token>` first, then `<token>`.

Every endpoint except `/health` needs `API_KEY` when it is set (HTTP `x-api-key` header). In production Caddy terminates TLS for `DOMAIN` and only ports 80/443 are open (`docker compose up -d --build`).

### Web UI

`https://<host>/ui` (e.g. https://prices.trigger.fun/ui) shows every monitored price (search by symbol, filter by exchange, sort, refreshes every 2 s) and, on click, a candlestick chart of our own stored candles with an interval picker (refreshes every 5 s). Candles exist only for `CANDLE_EXCHANGES`.

Everything under `/ui` (the page and its `/ui/api/prices`, `/ui/api/candles`, `/ui/api/exchanges` endpoints) uses HTTP Basic auth with `UI_USERNAME` / `UI_PASSWORD` (default `admin` / `admin`) instead of the API key. The API key does not open `/ui`, and the UI login does not open any other endpoint.

### Candles (REST)

```
GET /candles?symbol=BTCUSDT&exchange=binance_futures&interval=1m&limit=100
```

Intervals: `10s`, `30s`, `1m`, `2m`, `3m`, `4m`, `5m`, `10m`, `15m`, `30m`, `45m`, `1h`, `2h`, `3h`, `4h`, `1D`, `1W`

### Latest Price (REST)

```
GET /price/BTCUSDT?exchange=binance_futures
```

### Spike Alerts (REST)

Register a custom alert - bot calls with webhook URL, exchange, interval, threshold:

```
POST /alerts
{
  "webhook_url": "https://your-bot.com/webhook",
  "exchange": "binance_futures",
  "interval": "10m",
  "threshold_percent": 10.0,
  "symbol": "BTCUSDT"  // optional - omit for all symbols
}
```

- **Positive threshold** = spike (pump) alert
- **Negative threshold** = drop (dump) alert
- **Interval** = time period (e.g. 10% in 10 minutes, or 5% in 1 hour)

```
GET /alerts
DELETE /alerts/{alert_id}
```

## Configuration

| Variable | Description | Default |
|----------|-------------|---------|
| `MONGODB_URL` | MongoDB connection | `mongodb://localhost:27017` |
| `MONGODB_DB_NAME` | Database name | `crypto_pump_detector` |
| `WEBHOOK_TIMEOUT_SECONDS` | Webhook request timeout | `10` |
| `LOG_LEVEL` | Logging level | `INFO` |
| `UI_USERNAME` | Basic auth user for `/ui` | `admin` |
| `UI_PASSWORD` | Basic auth password for `/ui` | `admin` |

## Examples

```bash
# Test candles API (app must be running)
python examples/test_candles.py

# Test WebSocket live price
python examples/test_websocket_price.py

# Test spike alerts (run webhook receiver first: python examples/simple_webhook_receiver.py)
python examples/test_spike_alerts.py
```

## Architecture

- **Price updates** → Candle aggregation (10s base) → Larger intervals (30s, 1m, ... 1W)
- **Max 100 candles** per symbol/exchange/interval - oldest auto-removed
- **Custom alerts** stored in DB - checked when candles complete
- **WebSocket** broadcasts live prices to subscribers

## License

MIT
