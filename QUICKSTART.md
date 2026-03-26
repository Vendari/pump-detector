# Quick Start Guide

Get the Crypto Pump Detector up and running in 5 minutes!

## Prerequisites

- Python 3.8+
- MongoDB (or Docker)
- Internet connection for WebSocket streams

## Step 1: Install & Configure

```bash
cd pump-detector
pip install -r requirements.txt
cp .env.example .env
```

## Step 2: Start MongoDB

```bash
docker run -d -p 27017:27017 --name mongodb mongo:latest
```

## Step 3: Run the Application

```bash
python run.py
```

## Step 4: Test the API

### Live Price (WebSocket)

```bash
python examples/test_websocket_price.py
```

### Candles (REST)

```bash
curl "http://localhost:8000/candles?symbol=BTCUSDT&exchange=binance_futures&interval=1m&limit=10"
```

### Spike Alerts

1. Start webhook receiver: `python examples/simple_webhook_receiver.py`
2. Register alert: `python examples/test_spike_alerts.py`

## API Documentation

Open http://localhost:8000/docs for interactive Swagger UI.

## Common Endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /health` | Health check |
| `GET /price/{symbol}` | Latest price |
| `GET /candles?symbol=&exchange=&interval=` | Historical candles |
| `POST /alerts` | Register spike alert |
| `WS /ws/price` | Live price stream |
