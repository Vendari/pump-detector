# System Architecture

## Overview

Crypto Pump Detector v2 uses **candle-based storage** instead of raw price updates. Each exchange streams independently (no ticker ownership). Custom spike alerts are registered via API with webhook URL, exchange, interval, and threshold.

## Data Flow

```
Exchange WebSocket → Price Updates
       ↓
  Price Broadcaster (WebSocket subscribers)
       ↓
  Candle Service (aggregate to 10s, 30s, 1m ... 1W)
       ↓
  MongoDB (candles collection, max 100 per type)
       ↓
  Spike Detector (check against registered alerts)
       ↓
  Webhook Service (POST to alert's webhook_url)
```

## Storage

### Candles Collection

- **Key**: (symbol, exchange, interval, timestamp)
- **Intervals**: 10s, 30s, 1m, 2m, 3m, 4m, 5m, 10m, 15m, 30m, 45m, 1h, 2h, 3h, 4h, 1D, 1W
- **Limit**: 100 candles per symbol/exchange/interval
- **Aggregation**: 10s from raw prices; larger intervals from smaller candles

### Spike Alerts Collection

- **webhook_url**: Where to POST when alert triggers
- **exchange**: Which exchange to monitor
- **interval**: Time period (e.g. 10m, 1h, 1D)
- **threshold_percent**: Positive = spike, negative = drop
- **symbol**: Optional - specific symbol or all

## Components

- **PriceMonitor**: Coordinates exchange streams, calls candle service, triggers spike detection
- **CandleService**: Builds OHLC from price updates, aggregates to larger intervals
- **PriceBroadcaster**: Broadcasts live prices to WebSocket subscribers
- **SpikeDetector**: Checks completed candles against alert configs
- **WebhookService**: Sends alerts to configured URLs

## Bot Integration

1. **Live price**: Connect to `ws://host/ws/price`, send subscribe with symbol/exchange
2. **Candles**: `GET /candles?symbol=X&exchange=Y&interval=1m`
3. **Alerts**: `POST /alerts` with webhook_url, exchange, interval, threshold_percent
