# System Architecture

## Overview

The Crypto Pump Detector is a high-performance, modular system designed to monitor cryptocurrency prices across multiple exchanges in real-time and detect significant price movements (pumps and dumps).

## Architecture Diagram

```
┌─────────────────────────────────────────────────────────────────┐
│                         FastAPI Application                      │
│                            (main.py)                             │
└────────────────┬────────────────────────────────┬────────────────┘
                 │                                │
                 │                                │
        ┌────────▼────────┐              ┌────────▼─────────┐
        │  HTTP Endpoints │              │  Lifespan Manager│
        │                 │              │                  │
        │  /health        │              │  - Startup       │
        │  /stats         │              │  - Shutdown      │
        │  /price/{sym}   │              │  - Cleanup       │
        │  /webhook/test  │              │                  │
        └─────────────────┘              └──────────────────┘
                                                  │
                                         ┌────────▼─────────┐
                                         │  Price Monitor   │
                                         │ (price_monitor)  │
                                         └──────┬───────────┘
                                                │
                          ┌─────────────────────┼─────────────────────┐
                          │                     │                     │
                 ┌────────▼─────────┐  ┌────────▼────────┐  ┌────────▼─────────┐
                 │ Binance Futures  │  │  Bybit Spot     │  │   Future         │
                 │   (WebSocket)    │  │  (WebSocket)    │  │  Exchanges...    │
                 └────────┬─────────┘  └────────┬────────┘  └──────────────────┘
                          │                     │
                          └─────────┬───────────┘
                                    │
                           ┌────────▼─────────┐
                           │  Price Updates   │
                           │  (async stream)  │
                           └────────┬─────────┘
                                    │
                     ┌──────────────┼──────────────┐
                     │              │              │
            ┌────────▼────────┐    │     ┌────────▼─────────┐
            │ Ticker Ownership│    │     │  Spike Detector  │
            │   (first-come)  │    │     │  (background)    │
            └────────┬────────┘    │     └────────┬─────────┘
                     │              │              │
                     │     ┌────────▼────────┐     │
                     │     │ Batch Processor │     │
                     │     │  (buffer 100)   │     │
                     │     └────────┬────────┘     │
                     │              │              │
                     └──────────────┼──────────────┘
                                    │
                          ┌─────────▼──────────┐
                          │   MongoDB Service  │
                          │                    │
                          │  Collections:      │
                          │  - prices          │
                          │  - ticker_ownership│
                          └─────────┬──────────┘
                                    │
                          ┌─────────▼──────────┐
                          │  MongoDB Database  │
                          │  (with TTL index)  │
                          └────────────────────┘

                                    │
                           ┌────────▼─────────┐
                           │ Webhook Service  │
                           │  (async HTTP)    │
                           └────────┬─────────┘
                                    │
                          ┌─────────▼──────────┐
                          │  External Webhook  │
                          │   (your endpoint)  │
                          └────────────────────┘
```

## Component Details

### 1. FastAPI Application (`app/main.py`)

**Purpose:** Main application entry point and HTTP API

**Responsibilities:**
- Expose REST API endpoints
- Manage application lifecycle (startup/shutdown)
- Initialize all services and exchanges
- Handle health checks and monitoring

**Key Features:**
- Async context manager for clean startup/shutdown
- Background task for price monitoring
- RESTful API for querying data

### 2. Price Monitor (`app/services/price_monitor.py`)

**Purpose:** Coordinates price monitoring across all exchanges

**Responsibilities:**
- Manage multiple exchange connections
- Implement ticker ownership logic (first-come-first-serve)
- Batch price updates for efficient DB writes
- Trigger spike detection
- Maintain statistics

**Key Features:**
- **Batch Processing:** Groups updates (default: 100 or 1 second)
- **Ownership Cache:** In-memory cache to avoid DB lookups
- **Concurrent Processing:** Monitors all exchanges in parallel
- **Non-blocking:** Spike detection runs in background

**Performance Optimizations:**
```python
# Batching reduces DB writes by 100x
batch_size = 100  # Write 100 updates at once
batch_timeout = 1.0  # Or after 1 second

# Caching reduces DB queries by ~99%
ownership_cache: Dict[str, str]  # symbol -> exchange
```

### 3. Exchange Connectors (`app/exchanges/`)

**Purpose:** Connect to exchange WebSocket APIs and stream prices

**Architecture:**
- **BaseExchange:** Abstract interface defining standard methods
- **BinanceFuturesExchange:** Binance USDⓈ-M Futures implementation
- **Future exchanges:** Easy to add by inheriting from BaseExchange

**Interface:**
```python
class BaseExchange(ABC):
    async def connect() -> None
    async def disconnect() -> None
    async def subscribe_all_tickers() -> AsyncIterator[PriceUpdate]
```

**Key Features:**
- Async generators for streaming data
- Connection state management
- Ping/pong keepalive
- Error handling and logging

### 4. MongoDB Service (`app/database/mongodb.py`)

**Purpose:** Database operations and data persistence

**Collections:**

#### prices
```javascript
{
  symbol: "BTCUSDT",
  exchange: "binance_futures",
  price: 45000.50,
  mark_price: 45001.20,
  index_price: 45002.00,
  timestamp: ISODate("2026-01-27T12:00:00Z")
}
```

**Indexes:**
- `(symbol, exchange, timestamp)` - Compound index for queries
- `(timestamp)` - TTL index for automatic cleanup

#### ticker_ownership
```javascript
{
  symbol: "BTCUSDT",
  exchange: "binance_futures",
  claimed_at: ISODate("2026-01-27T10:00:00Z"),
  last_update: ISODate("2026-01-27T12:00:00Z")
}
```

**Indexes:**
- `(symbol)` - Unique index for ownership claims
- `(exchange)` - For querying owned tickers

**Key Features:**
- Bulk insert operations for performance
- Automatic data retention via TTL (default: 12 hours)
- Ownership claim with race condition protection
- Efficient time-range queries

### 5. Spike Detector (`app/services/spike_detector.py`)

**Purpose:** Detect significant price movements

**Algorithm:**
```python
# 1. Get historical prices for detection window (e.g., 5 minutes)
history = get_price_history(symbol, exchange, minutes=5)

# 2. Calculate percentage change
start_price = history[0].price
current_price = latest_update.price
change_percent = ((current_price - start_price) / start_price) * 100

# 3. Check thresholds
if change_percent >= spike_threshold:  # e.g., +5%
    emit_alert("spike")
elif change_percent <= -drop_threshold:  # e.g., -5%
    emit_alert("drop")
```

**Configuration:**
- `SPIKE_THRESHOLD_PERCENT`: Minimum % increase to trigger spike alert
- `DROP_THRESHOLD_PERCENT`: Minimum % decrease to trigger drop alert
- `DETECTION_WINDOW_MINUTES`: Time window to compare prices

### 6. Webhook Service (`app/services/webhook_service.py`)

**Purpose:** Send HTTP notifications for price alerts

**Features:**
- Async HTTP client (aiohttp)
- Configurable timeout
- Error handling and retry logic
- Test endpoint

**Alert Payload:**
```json
{
  "symbol": "BTCUSDT",
  "exchange": "binance_futures",
  "alert_type": "spike",
  "start_price": 45000.00,
  "current_price": 47500.00,
  "change_percent": 5.56,
  "window_minutes": 5,
  "timestamp": "2026-01-27T12:05:00Z"
}
```

## Data Flow

### Normal Operation (Price Update)

```
1. Exchange WebSocket → Price Update received
2. Price Monitor → Check ownership (cache first)
3. If not owned → Try to claim (DB transaction)
4. If owned by us → Add to batch
5. Batch full/timeout → Bulk write to MongoDB
6. Background task → Spike detection
7. If spike detected → Send webhook
```

### Ticker Ownership (First-Come-First-Serve)

```
1. Price update arrives for symbol "BTCUSDT"
2. Check ownership cache
   ├─ If in cache → Allow if owned by this exchange
   └─ If not in cache → Check database
3. If not in database → Try to claim
   ├─ INSERT with unique constraint
   ├─ Success → This exchange owns it now
   └─ Failure → Another exchange owns it
4. Update cache with result
5. Accept or reject price update
```

### Spike Detection Flow

```
1. Price update stored in DB
2. Background task launched (non-blocking)
3. Query: Get last N minutes of prices for this symbol
4. Calculate: % change from oldest to newest
5. If threshold exceeded:
   ├─ Create PriceAlert
   ├─ Log to console
   └─ Send webhook notification
```

## Performance Characteristics

### Throughput

| Metric | Value | Notes |
|--------|-------|-------|
| Updates/sec | ~1,000+ | Per exchange connection |
| DB writes/sec | ~10-20 | Due to batching (100 updates) |
| Spike checks/sec | ~1,000+ | Non-blocking, async |
| Webhook calls | As needed | With timeout protection |

### Memory Usage

| Component | Memory | Notes |
|-----------|--------|-------|
| Ownership cache | ~50KB | 500 tickers × ~100 bytes |
| Price batch | ~10KB | 100 updates × ~100 bytes |
| WebSocket buffers | ~1MB | Per connection |
| Total (1 exchange) | ~10-20MB | Excluding Python runtime |

### Database

| Operation | Index Used | Performance |
|-----------|------------|-------------|
| Claim ownership | `(symbol)` unique | O(log n) |
| Check ownership | `(symbol)` unique | O(log n) |
| Store price | `(timestamp)` TTL | O(log n) |
| Query history | `(symbol, exchange, timestamp)` | O(log n + k) |
| TTL cleanup | `(timestamp)` | Automatic, O(1) per doc |

## Scalability Considerations

### Horizontal Scaling

To scale across multiple instances:

1. **Use distributed locking for ownership**
   - Replace MongoDB unique index with Redis locks
   - Or use distributed consensus (etcd, Consul)

2. **Load balance WebSocket connections**
   - Different instances handle different exchanges
   - Or shard by symbol (A-M on instance 1, N-Z on instance 2)

3. **Centralized state**
   - All instances connect to same MongoDB
   - Ownership cache synced via Redis

### Vertical Scaling

To handle more exchanges/throughput:

1. **Increase batch size** (100 → 500)
2. **Add more workers** (multiple asyncio tasks per exchange)
3. **Optimize spike detection** (sample instead of all data points)
4. **Use faster MongoDB instance** (more RAM, SSD)

## Configuration Tuning

### High Throughput Scenario

```env
# Larger batches, less frequent writes
BATCH_SIZE=500
BATCH_TIMEOUT=2.0

# Less sensitive spike detection
SPIKE_THRESHOLD_PERCENT=10.0
DETECTION_WINDOW_MINUTES=10

# Shorter retention
PRICE_RETENTION_HOURS=6
```

### Low Latency / High Sensitivity

```env
# Smaller batches, more frequent writes
BATCH_SIZE=50
BATCH_TIMEOUT=0.5

# More sensitive detection
SPIKE_THRESHOLD_PERCENT=2.0
DETECTION_WINDOW_MINUTES=2

# Longer retention for analysis
PRICE_RETENTION_HOURS=24
```

## Security Considerations

1. **WebSocket Connections:** No authentication required for public streams
2. **MongoDB:** Should be behind firewall or use authentication
3. **Webhook URL:** Validate SSL certificates, use secrets for auth
4. **API Endpoints:** Add authentication middleware for production
5. **Rate Limiting:** Add rate limits to prevent abuse

## Monitoring & Observability

### Metrics to Track

- Updates received/processed/rejected per second
- Batch flush frequency and size
- WebSocket connection uptime
- MongoDB query latency
- Webhook success/failure rate
- Spike detection frequency

### Logging Levels

- **DEBUG:** Individual price updates, cache hits/misses
- **INFO:** Connections, ownership claims, batch flushes
- **WARNING:** Connection issues, failed webhooks, spikes detected
- **ERROR:** Unhandled exceptions, critical failures

## Future Enhancements

1. **Redis Integration:** For distributed ownership and caching
2. **Metrics Dashboard:** Prometheus + Grafana
3. **Alert Deduplication:** Prevent spam for same symbol
4. **Dynamic Thresholds:** ML-based anomaly detection
5. **Historical Analysis:** Backtesting and pattern recognition
6. **Circuit Breaker:** Auto-disable failing webhooks
7. **Multi-region:** Deploy in multiple regions for redundancy

## References

- [FastAPI Documentation](https://fastapi.tiangolo.com/)
- [MongoDB Motor Driver](https://motor.readthedocs.io/)
- [WebSockets Library](https://websockets.readthedocs.io/)
- [Binance API Docs](https://developers.binance.com/)
