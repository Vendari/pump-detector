"""
Crypto Pump Detector - FastAPI application.

Features:
- Live price subscription via WebSocket
- REST API for candles
- Custom spike alerts with webhook registration
"""

import asyncio
import json
import logging
from contextlib import asynccontextmanager

from fastapi import FastAPI, HTTPException, WebSocket, WebSocketDisconnect
from fastapi.responses import JSONResponse

from app.config import settings
from app.database.mongodb import mongodb_service
from app.exchanges.binance_futures import BinanceFuturesExchange
from app.models.alert import SpikeAlertConfig
from app.models.candle import CandleInterval, parse_interval
from app.services.price_broadcaster import price_broadcaster
from app.services.price_monitor import price_monitor
from app.services.webhook_service import webhook_service

# Configure logging
logging.basicConfig(
    level=getattr(logging, settings.log_level.upper()),
    format="%(asctime)s - %(name)s - %(levelname)s - %(message)s",
)
logger = logging.getLogger(__name__)

VALID_INTERVALS = [i.value for i in CandleInterval]


@asynccontextmanager
async def lifespan(app: FastAPI):
    """Application lifespan: startup and shutdown."""
    logger.info("Starting Crypto Pump Detector application")

    try:
        await mongodb_service.connect()

        binance_futures = BinanceFuturesExchange()
        price_monitor.add_exchange(binance_futures)

        monitor_task = asyncio.create_task(price_monitor.start())

        logger.info("Application started successfully")

        yield

    except Exception as e:
        logger.error(f"Error during startup: {e}")
        raise

    finally:
        logger.info("Shutting down application")
        try:
            await price_monitor.stop()
            await webhook_service.close()
            await mongodb_service.disconnect()
            logger.info("Application shutdown complete")
        except Exception as e:
            logger.error(f"Error during shutdown: {e}")


app = FastAPI(
    title="Crypto Pump Detector",
    description="Real-time cryptocurrency price monitoring, candles, and spike alerts",
    version="2.0.0",
    lifespan=lifespan,
)


# --- Health & Info ---

@app.get("/")
async def root():
    """Application info."""
    return {
        "name": "Crypto Pump Detector",
        "version": "2.0.0",
        "status": "running",
        "exchanges": [e.exchange_name for e in price_monitor.exchanges],
    }


@app.get("/health")
async def health_check():
    """Health check."""
    active = [e.exchange_name for e in price_monitor.exchanges if e.is_running]
    return {
        "status": "healthy" if active else "degraded",
        "active_exchanges": active,
        "total_exchanges": len(price_monitor.exchanges),
    }


@app.get("/stats")
async def get_stats():
    """Monitoring statistics."""
    return {"monitoring": price_monitor.get_stats()}


# --- Live Price WebSocket ---

@app.websocket("/ws/price")
async def websocket_live_price(websocket: WebSocket):
    """
    WebSocket for live price updates.

    Send JSON: {"action": "subscribe", "symbol": "BTCUSDT", "exchange": "binance_futures"}
    Send JSON: {"action": "unsubscribe"}
    """
    await websocket.accept()
    queue: asyncio.Queue | None = None
    symbol: str | None = None
    exchange = "binance_futures"
    inbound: asyncio.Queue = asyncio.Queue()

    async def receive_loop():
        try:
            while True:
                data = await websocket.receive_text()
                inbound.put_nowait(data)
        except (WebSocketDisconnect, Exception):
            inbound.put_nowait(None)  # Signal disconnect

    recv_task = asyncio.create_task(receive_loop())

    try:
        while True:
            # Wait for price update (if subscribed) or inbound message
            wait_tasks = [asyncio.create_task(inbound.get())]
            if queue:
                wait_tasks.append(asyncio.create_task(asyncio.wait_for(queue.get(), timeout=0.2)))

            done, pending = await asyncio.wait(wait_tasks, return_when=asyncio.FIRST_COMPLETED)
            for t in pending:
                t.cancel()

            for t in done:
                try:
                    result = t.result()
                    if result is None:
                        return
                    if isinstance(result, dict):
                        await websocket.send_json(result)
                    else:
                        msg = json.loads(result)
                        action = msg.get("action")
                        if action == "subscribe":
                            sym = msg.get("symbol", "").upper()
                            ex = msg.get("exchange", "binance_futures")
                            if not sym:
                                await websocket.send_json({"error": "symbol required"})
                                continue
                            if queue and symbol and exchange:
                                price_broadcaster.unsubscribe(symbol, exchange, queue)
                            symbol, exchange = sym, ex
                            queue = price_broadcaster.subscribe(symbol, exchange)
                            await websocket.send_json({"status": "subscribed", "symbol": symbol, "exchange": exchange})
                        elif action == "unsubscribe":
                            if queue and symbol and exchange:
                                price_broadcaster.unsubscribe(symbol, exchange, queue)
                                queue = None
                            await websocket.send_json({"status": "unsubscribed"})
                except (json.JSONDecodeError, asyncio.CancelledError, asyncio.TimeoutError):
                    pass
    except WebSocketDisconnect:
        pass
    finally:
        recv_task.cancel()
        try:
            await recv_task
        except asyncio.CancelledError:
            pass
        if queue and symbol and exchange:
            price_broadcaster.unsubscribe(symbol, exchange, queue)


# --- REST: Candles ---

@app.get("/candles")
async def get_candles(
    symbol: str,
    exchange: str = "binance_futures",
    interval: str = "1m",
    limit: int = 100,
):
    """
    Get candles for a symbol.

    Intervals: 10s, 30s, 1m, 2m, 3m, 4m, 5m, 10m, 15m, 30m, 45m, 1h, 2h, 3h, 4h, 1D, 1W
    """
    if parse_interval(interval) is None:
        raise HTTPException(
            status_code=400,
            detail=f"Invalid interval. Valid: {VALID_INTERVALS}",
        )
    if limit > 100:
        limit = 100

    candles = await mongodb_service.get_candles(symbol, exchange, interval, limit)
    return {
        "symbol": symbol,
        "exchange": exchange,
        "interval": interval,
        "candles": candles,
    }


# --- REST: Latest Price ---

@app.get("/price/{symbol}")
async def get_latest_price(symbol: str, exchange: str = "binance_futures"):
    """Get latest price for a symbol (from live stream or last candle)."""
    price = price_broadcaster.get_latest(symbol, exchange)
    if price:
        return price
    doc = await mongodb_service.get_latest_price(symbol, exchange)
    if doc:
        return {
            "symbol": symbol,
            "exchange": exchange,
            "price": doc["close"],
            "timestamp": doc["timestamp"].isoformat(),
        }
    raise HTTPException(status_code=404, detail="No price data for symbol")


# --- REST: Spike Alerts ---

@app.post("/alerts")
async def create_spike_alert(config: SpikeAlertConfig):
    """
    Register a custom spike alert.

    - webhook_url: URL to POST when alert triggers
    - exchange: Exchange to monitor
    - interval: Time period (e.g. 10m, 1h, 1D)
    - threshold_percent: Positive = spike, negative = drop
    - symbol: Optional - specific symbol or all
    """
    if parse_interval(config.interval) is None:
        raise HTTPException(
            status_code=400,
            detail=f"Invalid interval. Valid: {VALID_INTERVALS}",
        )
    alert_id = await mongodb_service.create_spike_alert(config)
    return {"status": "created", "alert_id": alert_id}


@app.get("/alerts")
async def list_spike_alerts(exchange: str | None = None, symbol: str | None = None):
    """List registered spike alerts."""
    alerts = await mongodb_service.get_spike_alerts(exchange=exchange, symbol=symbol)
    return {"alerts": alerts}


@app.delete("/alerts/{alert_id}")
async def delete_spike_alert(alert_id: str):
    """Delete a spike alert."""
    ok = await mongodb_service.delete_spike_alert(alert_id)
    if not ok:
        raise HTTPException(status_code=404, detail="Alert not found")
    return {"status": "deleted"}


@app.get("/alerts/detected")
async def list_detected_alerts(
    symbol: str | None = None,
    exchange: str | None = None,
    limit: int = 100,
):
    """List detected alert history (all stored alerts)."""
    alerts = await mongodb_service.get_detected_alerts(
        symbol=symbol, exchange=exchange, limit=limit
    )
    return {"alerts": alerts}


# --- Webhook Test ---

@app.post("/webhook/test")
async def test_webhook(url: str):
    """Test a webhook URL with a sample alert. Pass url as query param: ?url=https://..."""
    if not url or not url.startswith("http"):
        raise HTTPException(status_code=400, detail="Valid url required")
    success = await webhook_service.test_webhook(url)
    if success:
        return {"status": "success", "message": "Webhook test sent"}
    raise HTTPException(status_code=500, detail="Failed to send test webhook")


# --- Exchanges & Symbols ---

@app.get("/exchanges")
async def list_exchanges():
    """List exchanges with data."""
    exchanges = await mongodb_service.get_available_exchanges()
    return {"exchanges": exchanges or ["binance_futures"]}


@app.get("/symbols/{exchange}")
async def list_symbols(exchange: str):
    """List symbols for an exchange."""
    symbols = await mongodb_service.get_available_symbols(exchange)
    return {"exchange": exchange, "symbols": symbols}


if __name__ == "__main__":
    import uvicorn

    uvicorn.run(
        "app.main:app",
        host="0.0.0.0",
        port=8000,
        reload=False,
        log_level=settings.log_level.lower(),
    )
