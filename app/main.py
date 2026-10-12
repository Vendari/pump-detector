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

import hmac

from fastapi import FastAPI, HTTPException, Request, WebSocket, WebSocketDisconnect
from fastapi.responses import JSONResponse

from app.config import settings
from app.database.mongodb import mongodb_service
from app.exchanges.binance_futures import BinanceFuturesExchange
from app.exchanges.gate_futures import GateFuturesExchange
from app.exchanges.on_demand import (
    AsterFuturesExchange,
    BybitFuturesExchange,
    HyperliquidExchange,
    KucoinFuturesExchange,
)
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
        gate_futures = GateFuturesExchange()
        price_monitor.add_exchange(gate_futures)
        # Live prices only, connected while someone watches a symbol.
        for on_demand in (
            BybitFuturesExchange(),
            KucoinFuturesExchange(),
            AsterFuturesExchange(),
            HyperliquidExchange(),
        ):
            price_monitor.add_exchange(on_demand)

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


# --- Auth ---

OPEN_PATHS = {"/health"}


def _key_ok(given: str | None) -> bool:
    if not settings.api_key:
        return True
    return given is not None and hmac.compare_digest(given, settings.api_key)


@app.middleware("http")
async def require_api_key(request: Request, call_next):
    if request.url.path not in OPEN_PATHS and not _key_ok(request.headers.get("x-api-key")):
        return JSONResponse(status_code=401, content={"detail": "invalid api key"})
    return await call_next(request)


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
    WebSocket for live prices. Auth: `x-api-key` header or `?key=`.

    Send {"action": "subscribe", "symbol": "BTCUSDT", "exchange": "binance_futures"}
    to add a market (several per connection; the latest known price is sent
    at once). Send {"action": "unsubscribe", "symbol": ..., "exchange": ...}
    to drop one, or {"action": "unsubscribe"} to drop all.
    Exchanges: binance_futures, gate_futures (every market), bybit_futures,
    kucoin_futures, aster_futures, hyperliquid (connected on demand).
    Prices arrive as {"symbol", "exchange", "price", "timestamp"}.
    """
    if not _key_ok(websocket.headers.get("x-api-key") or websocket.query_params.get("key")):
        await websocket.close(code=1008)
        return
    await websocket.accept()
    queue: asyncio.Queue = asyncio.Queue(maxsize=1000)
    subscriptions: set[tuple[str, str]] = set()

    def drop(key: tuple[str, str]) -> None:
        price_broadcaster.unsubscribe(key[0], key[1], queue)
        price_monitor.unwatch(key[0], key[1])
        subscriptions.discard(key)

    def reply(payload: dict) -> None:
        # Everything goes out through the one sender task.
        try:
            queue.put_nowait(payload)
        except asyncio.QueueFull:
            pass

    async def handle(raw: str) -> None:
        try:
            msg = json.loads(raw)
        except json.JSONDecodeError:
            reply({"error": "invalid json"})
            return
        action = msg.get("action")
        sym = str(msg.get("symbol") or "").upper()
        ex = msg.get("exchange") or "binance_futures"
        if action == "subscribe":
            if not sym:
                reply({"error": "symbol required"})
                return
            key = (sym, ex)
            if key not in subscriptions:
                if not price_monitor.watch(sym, ex):
                    reply({"error": "unknown exchange", "exchange": ex})
                    return
                price_broadcaster.subscribe(sym, ex, queue)
                subscriptions.add(key)
            reply({"status": "subscribed", "symbol": sym, "exchange": ex})
            latest = price_broadcaster.get_latest(sym, ex)
            if latest:
                reply(latest)
        elif action == "unsubscribe":
            targets = [(sym, ex)] if sym else list(subscriptions)
            for key in targets:
                if key in subscriptions:
                    drop(key)
            reply({"status": "unsubscribed"})

    async def forward_prices() -> None:
        while True:
            await websocket.send_json(await queue.get())

    sender = asyncio.create_task(forward_prices())
    try:
        while True:
            await handle(await websocket.receive_text())
    except WebSocketDisconnect:
        pass
    except Exception as e:
        logger.debug(f"/ws/price client error: {e}")
    finally:
        sender.cancel()
        for key in list(subscriptions):
            drop(key)


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
    return {"exchanges": exchanges or [e.exchange_name for e in price_monitor.exchanges]}


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
