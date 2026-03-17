# Crypto Pump Detector

A high-performance cryptocurrency price monitoring system that detects price spikes and drops across multiple exchanges in real-time.

## Features

- **Modular Exchange Support**: Easy to add new exchanges with standardized interface
- **Real-time Price Monitoring**: WebSocket connections for instant price updates
- **Spike Detection**: Configurable thresholds for detecting pumps and dumps
- **Ticker Ownership**: First-come-first-serve mechanism ensures single source per ticker
- **Efficient Storage**: MongoDB with automatic TTL-based data retention (default: 12 hours)
- **Batch Processing**: Optimized bulk writes for high-throughput scenarios
- **Webhook Alerts**: Instant HTTP notifications when price movements detected
- **RESTful API**: Query endpoints for statistics, price history, and monitoring

## Quick Start

### Option 1: Docker Compose (Recommended)

```bash
# Start MongoDB and the application
docker-compose up -d

# View logs
docker-compose logs -f app
```

### Option 2: Local Development

1. **Install dependencies:**
```bash
pip install -r requirements.txt
```

2. **Configure environment:**
```bash
cp .env.example .env
# Edit .env with your configuration
```

3. **Start MongoDB:**
```bash
# Using Docker
docker run -d -p 27017:27017 --name mongodb mongo:latest

# Or use local MongoDB installation
systemctl start mongodb  # Linux
brew services start mongodb-community  # macOS
```

4. **Run the application:**
```bash
# Using Python runner (cross-platform)
python run.py

# Or using shell script (Unix-like systems)
chmod +x run.sh
./run.sh

# Or directly
python -m app.main
```

5. **Access the API:**
- Web UI: http://localhost:8000/docs
- Health check: http://localhost:8000/health
- Statistics: http://localhost:8000/stats

## Architecture

```
pump-detector/
├── app/
│   ├── main.py              # FastAPI app entry point
│   ├── config.py            # Configuration management
│   ├── exchanges/           # Exchange connectors
│   │   ├── base.py          # Base exchange interface
│   │   ├── binance_futures.py  # Binance Futures implementation
│   │   └── types.py         # Type definitions
│   ├── services/            # Business logic
│   │   ├── price_monitor.py    # Coordinates monitoring & batching
│   │   ├── spike_detector.py   # Detects price spikes/drops
│   │   └── webhook_service.py  # Sends webhook notifications
│   ├── database/            # Data persistence
│   │   └── mongodb.py       # MongoDB operations & indexes
│   └── models/              # Data models
│       └── price.py         # PriceUpdate, PriceAlert, etc.
├── requirements.txt         # Python dependencies
├── .env.example            # Configuration template
└── README.md               # This file
```

## API Endpoints

### Monitoring

- `GET /` - Application information
- `GET /health` - Health check with exchange status
- `GET /stats` - Monitoring statistics and ownership counts

### Price Data

- `GET /price/{symbol}?exchange=binance_futures&minutes=60` - Price history for a symbol
- `GET /tickers/{exchange}` - All tickers owned by an exchange

### Webhooks

- `POST /webhook/test` - Test webhook configuration
- `POST /webhook/alert` - Example webhook receiver endpoint

## Configuration

All configuration is managed through environment variables in `.env`:

| Variable | Description | Default |
|----------|-------------|---------|
| `MONGODB_URL` | MongoDB connection string | `mongodb://localhost:27017` |
| `MONGODB_DB_NAME` | Database name | `crypto_pump_detector` |
| `PRICE_RETENTION_HOURS` | Hours to retain price data | `12` |
| `SPIKE_THRESHOLD_PERCENT` | Spike detection threshold | `5.0` |
| `DROP_THRESHOLD_PERCENT` | Drop detection threshold | `5.0` |
| `DETECTION_WINDOW_MINUTES` | Time window for detection | `5` |
| `WEBHOOK_URL` | Webhook URL for alerts | `None` |
| `WEBHOOK_TIMEOUT_SECONDS` | Webhook request timeout | `10` |
| `LOG_LEVEL` | Logging level | `INFO` |

## Adding New Exchanges

The system is designed for easy extensibility. Here's how to add a new exchange:

### Step 1: Create Exchange Connector

Create a new file in `app/exchanges/` (e.g., `bybit_spot.py`):

```python
from app.exchanges.base import BaseExchange
from app.models.price import PriceUpdate

class BybitSpotExchange(BaseExchange):
    def __init__(self):
        super().__init__(exchange_name="bybit_spot")
    
    async def connect(self) -> None:
        # Implement WebSocket connection
        pass
    
    async def disconnect(self) -> None:
        # Implement cleanup
        pass
    
    async def subscribe_all_tickers(self) -> AsyncIterator[PriceUpdate]:
        # Yield price updates
        pass
```

### Step 2: Register in Main App

In `app/main.py`, add to the `lifespan` function:

```python
from app.exchanges.bybit_spot import BybitSpotExchange

# In lifespan function, after binance_futures:
bybit_spot = BybitSpotExchange()
price_monitor.add_exchange(bybit_spot)
```

That's it! The ticker ownership and spike detection will automatically work with the new exchange.

## Performance Optimizations

The application is optimized for high-throughput scenarios:

1. **Batch Processing**: Price updates are batched (default: 100 updates or 1 second) before writing to MongoDB
2. **Ownership Caching**: Ticker ownership is cached in memory to avoid DB lookups
3. **Compound Indexes**: MongoDB indexes optimized for time-range queries
4. **TTL Indexes**: Automatic data cleanup using MongoDB TTL indexes
5. **Async I/O**: All I/O operations are asynchronous for maximum concurrency
6. **Non-blocking Webhooks**: Webhook calls don't block price processing

## Monitoring & Statistics

View real-time statistics:

```bash
curl http://localhost:8000/stats
```

Example response:
```json
{
  "monitoring": {
    "total_updates": 125000,
    "stored_updates": 120000,
    "rejected_updates": 5000,
    "ownership_claims": 450,
    "batch_size": 23,
    "cached_tickers": 450,
    "active_exchanges": 1
  },
  "ownership": {
    "binance_futures": 450
  }
}
```

## Webhook Payload Format

When a price spike or drop is detected, a POST request is sent to the configured webhook URL:

```json
{
  "symbol": "BTCUSDT",
  "exchange": "binance_futures",
  "alert_type": "spike",
  "start_price": 45000.00,
  "current_price": 47500.00,
  "change_percent": 5.56,
  "window_minutes": 5,
  "timestamp": "2026-01-27T12:05:00"
}
```

## Development

### Running Tests

```bash
pytest tests/
```

### Code Style

```bash
black app/
flake8 app/
mypy app/
```

## Troubleshooting

### MongoDB Connection Failed

Ensure MongoDB is running and accessible:
```bash
docker ps | grep mongo
# or
systemctl status mongodb
```

### No Price Updates

Check exchange connectivity:
```bash
curl http://localhost:8000/health
```

### Webhook Not Sending

Test webhook configuration:
```bash
curl -X POST http://localhost:8000/webhook/test
```

## License

MIT License - See LICENSE file for details

## Support

For issues and questions, please open an issue on GitHub.
