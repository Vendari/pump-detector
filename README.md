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
ws://localhost:8000/ws/price
```

Send: `{"action": "subscribe", "symbol": "BTCUSDT", "exchange": "binance_futures"}`
Receive: `{"symbol": "BTCUSDT", "exchange": "binance_futures", "price": 45000.5, "timestamp": "..."}`

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
