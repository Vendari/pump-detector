# Guide: Adding New Exchanges

This guide walks you through adding support for new cryptocurrency exchanges to the Pump Detector system.

## Overview

The system uses a modular architecture where each exchange is a separate connector that implements a standard interface. This makes it easy to add support for new exchanges without modifying core logic.

## Step-by-Step Guide

### 1. Understand the BaseExchange Interface

All exchanges must inherit from `BaseExchange` and implement three key methods:

```python
from app.exchanges.base import BaseExchange
from app.models.price import PriceUpdate
from typing import AsyncIterator

class YourExchange(BaseExchange):
    async def connect(self) -> None:
        """Establish WebSocket connection"""
        pass
    
    async def disconnect(self) -> None:
        """Close connection and cleanup"""
        pass
    
    async def subscribe_all_tickers(self) -> AsyncIterator[PriceUpdate]:
        """Yield price updates as they arrive"""
        pass
```

### 2. Create Your Exchange Connector

Create a new file: `app/exchanges/your_exchange.py`

#### Example: Bybit Spot Exchange

```python
import json
import logging
from datetime import datetime
from typing import AsyncIterator
import websockets
from websockets.client import WebSocketClientProtocol

from app.exchanges.base import BaseExchange
from app.models.price import PriceUpdate

logger = logging.getLogger(__name__)


class BybitSpotExchange(BaseExchange):
    """
    Bybit Spot exchange connector.
    
    Connects to Bybit Spot WebSocket API and streams ticker prices.
    Reference: https://bybit-exchange.github.io/docs/v5/websocket/public/ticker
    """
    
    WS_URL = "wss://stream.bybit.com/v5/public/spot"
    
    def __init__(self):
        super().__init__(exchange_name="bybit_spot")
        self._websocket: WebSocketClientProtocol | None = None
    
    async def connect(self) -> None:
        """Connect to Bybit WebSocket."""
        try:
            logger.info(f"Connecting to Bybit Spot: {self.WS_URL}")
            
            self._websocket = await websockets.connect(
                self.WS_URL,
                ping_interval=20,
                ping_timeout=10
            )
            
            # Subscribe to all tickers
            subscribe_message = {
                "op": "subscribe",
                "args": ["tickers.*"]
            }
            await self._websocket.send(json.dumps(subscribe_message))
            
            self._is_running = True
            logger.info("Connected to Bybit Spot")
            
        except Exception as e:
            logger.error(f"Failed to connect to Bybit: {e}")
            self._is_running = False
            raise
    
    async def disconnect(self) -> None:
        """Disconnect from Bybit WebSocket."""
        self._is_running = False
        
        if self._websocket:
            try:
                await self._websocket.close()
                logger.info("Disconnected from Bybit Spot")
            except Exception as e:
                logger.error(f"Error during disconnect: {e}")
            finally:
                self._websocket = None
    
    async def subscribe_all_tickers(self) -> AsyncIterator[PriceUpdate]:
        """
        Subscribe to all ticker updates from Bybit Spot.
        
        Yields:
            PriceUpdate: Price update for each trading pair
        """
        if not self._websocket:
            raise RuntimeError("WebSocket not connected")
        
        logger.info("Starting to stream from Bybit Spot")
        
        try:
            async for message in self._websocket:
                try:
                    data = json.loads(message)
                    
                    # Skip non-data messages (subscriptions, pings, etc.)
                    if data.get("topic") != "tickers":
                        continue
                    
                    # Parse the price update
                    price_update = self._parse_price_update(data.get("data", {}))
                    if price_update:
                        yield price_update
                
                except json.JSONDecodeError as e:
                    logger.error(f"JSON decode error: {e}")
                    continue
                except Exception as e:
                    logger.error(f"Error processing message: {e}")
                    continue
        
        except websockets.exceptions.ConnectionClosed:
            logger.warning("Bybit WebSocket connection closed")
            self._is_running = False
        except Exception as e:
            logger.error(f"Error in price stream: {e}")
            self._is_running = False
            raise
    
    def _parse_price_update(self, data: dict) -> PriceUpdate | None:
        """
        Parse Bybit ticker data into PriceUpdate.
        
        Bybit ticker format:
        {
            "symbol": "BTCUSDT",
            "lastPrice": "45000.50",
            "bid1Price": "44999.00",
            "ask1Price": "45001.00",
            ...
        }
        """
        try:
            symbol = data.get("symbol")
            last_price = float(data.get("lastPrice", 0))
            
            if not symbol or last_price <= 0:
                return None
            
            return PriceUpdate(
                symbol=symbol,
                exchange=self.exchange_name,
                price=last_price,
                timestamp=datetime.utcnow()
            )
        
        except (ValueError, KeyError) as e:
            logger.error(f"Error parsing price: {e}, data: {data}")
            return None
```

### 3. Register Your Exchange

Edit `app/main.py` and add your exchange to the `lifespan` function:

```python
from app.exchanges.bybit_spot import BybitSpotExchange

@asynccontextmanager
async def lifespan(app: FastAPI):
    # Startup
    logger.info("Starting Crypto Pump Detector application")
    
    try:
        await mongodb_service.connect()
        
        # Add your exchange here
        binance_futures = BinanceFuturesExchange()
        price_monitor.add_exchange(binance_futures)
        
        bybit_spot = BybitSpotExchange()  # NEW
        price_monitor.add_exchange(bybit_spot)  # NEW
        
        monitor_task = asyncio.create_task(price_monitor.start())
        
        logger.info("Application started successfully")
        yield
        
    # ... rest of the code
```

### 4. Test Your Exchange

1. Start the application:
```bash
python run.py
```

2. Check if your exchange is active:
```bash
curl http://localhost:8000/health
```

Expected response:
```json
{
  "status": "healthy",
  "active_exchanges": ["binance_futures", "bybit_spot"],
  "total_exchanges": 2
}
```

3. Monitor stats:
```bash
curl http://localhost:8000/stats
```

4. View tickers claimed by your exchange:
```bash
curl http://localhost:8000/tickers/bybit_spot
```

## Common WebSocket Patterns

### Pattern 1: Single Stream with All Tickers (like Binance)

```python
async def connect(self) -> None:
    self._websocket = await websockets.connect(
        "wss://exchange.com/stream/all_tickers"
    )
```

### Pattern 2: Subscribe After Connection (like Bybit)

```python
async def connect(self) -> None:
    self._websocket = await websockets.connect("wss://exchange.com")
    
    # Send subscription message
    subscribe_msg = {"op": "subscribe", "args": ["tickers"]}
    await self._websocket.send(json.dumps(subscribe_msg))
```

### Pattern 3: Multiple Streams (if exchange doesn't support all tickers)

```python
async def subscribe_all_tickers(self) -> AsyncIterator[PriceUpdate]:
    # Get list of symbols first
    symbols = await self._get_all_symbols()
    
    # Create multiple WebSocket connections
    tasks = [
        self._subscribe_symbol(symbol)
        for symbol in symbols
    ]
    
    # Merge all streams
    async for price_update in merge_async_iterators(tasks):
        yield price_update
```

## Exchange-Specific Considerations

### Futures vs Spot

Treat futures and spot as separate exchanges:
- `binance_futures` - Binance USDⓈ-M Futures
- `binance_spot` - Binance Spot Market
- `bybit_futures` - Bybit USDT Perpetual
- `bybit_spot` - Bybit Spot Market

### Rate Limits

If an exchange has strict rate limits:

```python
class RateLimitedExchange(BaseExchange):
    def __init__(self):
        super().__init__(exchange_name="rate_limited")
        self._rate_limiter = asyncio.Semaphore(10)  # Max 10 concurrent
    
    async def _with_rate_limit(self, coro):
        async with self._rate_limiter:
            return await coro
```

### Authentication (if required)

```python
class AuthenticatedExchange(BaseExchange):
    def __init__(self, api_key: str, api_secret: str):
        super().__init__(exchange_name="authenticated")
        self.api_key = api_key
        self.api_secret = api_secret
    
    def _generate_signature(self, message: str) -> str:
        import hmac
        import hashlib
        return hmac.new(
            self.api_secret.encode(),
            message.encode(),
            hashlib.sha256
        ).hexdigest()
```

## Debugging Tips

### 1. Enable Debug Logging

In `.env`:
```
LOG_LEVEL=DEBUG
```

### 2. Test WebSocket Manually

```python
import asyncio
import websockets
import json

async def test():
    async with websockets.connect("wss://...") as ws:
        async for msg in ws:
            print(json.dumps(json.loads(msg), indent=2))

asyncio.run(test())
```

### 3. Check Exchange Documentation

- Binance: https://developers.binance.com/
- Bybit: https://bybit-exchange.github.io/docs/
- Gate.io: https://www.gate.io/docs/developers/apiv4/ws/
- HTX: https://www.htx.com/en-us/opend/newApiPages/

## Best Practices

1. **Error Handling**: Always catch and log exceptions
2. **Connection Recovery**: Implement reconnection logic for production
3. **Data Validation**: Validate all fields before creating PriceUpdate
4. **Logging**: Use appropriate log levels (DEBUG for data, INFO for events, ERROR for failures)
5. **Type Hints**: Use proper type hints for better code maintainability
6. **Documentation**: Document the exchange API reference URL

## Need Help?

If you run into issues:
1. Check the exchange's WebSocket documentation
2. Enable DEBUG logging to see raw messages
3. Compare with the Binance Futures implementation
4. Open an issue on GitHub with details

Happy coding! 🚀
