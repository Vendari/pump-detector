import asyncio
import logging
from contextlib import asynccontextmanager

from fastapi import FastAPI, HTTPException
from fastapi.responses import JSONResponse

from app.config import settings
from app.database.mongodb import mongodb_service
from app.exchanges.binance_futures import BinanceFuturesExchange
from app.services.price_monitor import price_monitor
from app.services.webhook_service import webhook_service

# Configure logging
logging.basicConfig(
    level=getattr(logging, settings.log_level.upper()),
    format='%(asctime)s - %(name)s - %(levelname)s - %(message)s'
)
logger = logging.getLogger(__name__)


@asynccontextmanager
async def lifespan(app: FastAPI):
    """
    Application lifespan manager.
    
    Handles startup and shutdown events:
    - Startup: Connect to MongoDB, initialize exchanges, start monitoring
    - Shutdown: Stop monitoring, disconnect from exchanges, close connections
    """
    # Startup
    logger.info("Starting Crypto Pump Detector application")
    
    try:
        # Connect to MongoDB
        await mongodb_service.connect()
        
        # Initialize exchanges
        binance_futures = BinanceFuturesExchange()
        price_monitor.add_exchange(binance_futures)
        
        # Start price monitoring in background
        monitor_task = asyncio.create_task(price_monitor.start())
        
        logger.info("Application started successfully")
        
        yield  # Application is running
        
    except Exception as e:
        logger.error(f"Error during startup: {e}")
        raise
    
    finally:
        # Shutdown
        logger.info("Shutting down application")
        
        try:
            # Stop price monitoring
            await price_monitor.stop()
            
            # Close webhook service
            await webhook_service.close()
            
            # Disconnect from MongoDB
            await mongodb_service.disconnect()
            
            logger.info("Application shutdown complete")
            
        except Exception as e:
            logger.error(f"Error during shutdown: {e}")


# Create FastAPI application
app = FastAPI(
    title="Crypto Pump Detector",
    description="Real-time cryptocurrency price monitoring and spike detection system",
    version="1.0.0",
    lifespan=lifespan
)


@app.get("/")
async def root():
    """Root endpoint with application information."""
    return {
        "name": "Crypto Pump Detector",
        "version": "1.0.0",
        "status": "running",
        "exchanges": [exchange.exchange_name for exchange in price_monitor.exchanges],
    }


@app.get("/health")
async def health_check():
    """Health check endpoint."""
    try:
        # Check if exchanges are running
        active_exchanges = [
            e.exchange_name for e in price_monitor.exchanges if e.is_running
        ]
        
        return {
            "status": "healthy" if active_exchanges else "degraded",
            "active_exchanges": active_exchanges,
            "total_exchanges": len(price_monitor.exchanges),
        }
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@app.get("/stats")
async def get_stats():
    """Get monitoring statistics."""
    try:
        # Get price monitor stats
        monitor_stats = price_monitor.get_stats()
        
        # Get ticker ownership counts by exchange
        ownership_counts = await mongodb_service.get_ticker_count_by_exchange()
        
        return {
            "monitoring": monitor_stats,
            "ownership": ownership_counts,
        }
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@app.get("/tickers/{exchange}")
async def get_exchange_tickers(exchange: str):
    """
    Get all tickers owned by a specific exchange.
    
    Args:
        exchange: Exchange name (e.g., 'binance_futures')
    """
    try:
        tickers = await mongodb_service.get_owned_tickers(exchange)
        return {
            "exchange": exchange,
            "ticker_count": len(tickers),
            "tickers": tickers,
        }
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@app.get("/price/{symbol}")
async def get_price_history(symbol: str, exchange: str = "binance_futures", minutes: int = 60):
    """
    Get price history for a specific symbol.
    
    Args:
        symbol: Trading pair symbol (e.g., 'BTCUSDT')
        exchange: Exchange name (default: 'binance_futures')
        minutes: Number of minutes of history to retrieve (default: 60)
    """
    try:
        if minutes > settings.price_retention_hours * 60:
            raise HTTPException(
                status_code=400,
                detail=f"Requested time range exceeds retention period of {settings.price_retention_hours} hours"
            )
        
        history = await mongodb_service.get_price_history(symbol, exchange, minutes)
        
        return {
            "symbol": symbol,
            "exchange": exchange,
            "window_minutes": minutes,
            "data_points": len(history),
            "history": history,
        }
    except HTTPException:
        raise
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@app.post("/webhook/test")
async def test_webhook():
    """Test the webhook endpoint configuration."""
    try:
        if not settings.webhook_url:
            raise HTTPException(
                status_code=400,
                detail="Webhook URL not configured in environment"
            )
        
        success = await webhook_service.test_webhook()
        
        if success:
            return {"status": "success", "message": "Webhook test sent successfully"}
        else:
            raise HTTPException(
                status_code=500,
                detail="Failed to send test webhook"
            )
    except HTTPException:
        raise
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@app.post("/webhook/alert")
async def receive_webhook_alert(payload: dict):
    """
    Example webhook endpoint that can receive alerts.
    
    This is a sample endpoint that demonstrates what the webhook
    receiver should implement. In production, this would be replaced
    by your own webhook handler.
    """
    logger.info(f"Received webhook alert: {payload}")
    return {"status": "received", "payload": payload}


if __name__ == "__main__":
    import uvicorn
    
    uvicorn.run(
        "app.main:app",
        host="0.0.0.0",
        port=8000,
        reload=False,  # Disable reload for production
        log_level=settings.log_level.lower()
    )
