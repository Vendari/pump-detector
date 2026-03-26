"""
Price monitor - coordinates price streaming from exchanges and candle building.

No ticker ownership: all exchanges stream and store their own data.
Price updates are aggregated into candles (10s, 30s, 1m, ... 1W) and stored.
Custom spike alerts are checked against registered webhook configs.
"""

import asyncio
import logging
from typing import Dict, List, Set

from app.exchanges.base import BaseExchange
from app.models.price import PriceUpdate
from app.database.mongodb import mongodb_service
from app.services.candle_service import candle_service
from app.services.spike_detector import SpikeDetector
from app.services.webhook_service import webhook_service
from app.services.price_broadcaster import price_broadcaster

logger = logging.getLogger(__name__)


class PriceMonitor:
    """
    Coordinates price monitoring across multiple exchanges.

    - All exchanges stream independently (no ownership)
    - Price updates -> candles via CandleService
    - Spike detection uses candles and custom alert configs
    """

    def __init__(self, batch_size: int = 100, batch_timeout: float = 1.0):
        self.exchanges: List[BaseExchange] = []
        self.spike_detector = SpikeDetector()
        self.batch_size = batch_size
        self.batch_timeout = batch_timeout

        # Live price subscribers for WebSocket (symbol: Set[exchange])
        self._live_subscribers: Dict[str, Set[str]] = {}  # symbol -> set of exchanges

        self.stats = {
            "total_updates": 0,
            "candles_created": 0,
            "alerts_sent": 0,
        }
        self._stopping = False

    def add_exchange(self, exchange: BaseExchange) -> None:
        """Add an exchange to monitor."""
        self.exchanges.append(exchange)
        logger.info(f"Added exchange: {exchange.exchange_name}")

    def subscribe_live(self, symbol: str, exchange: str) -> None:
        """Register a live price subscription for symbol on exchange."""
        if symbol not in self._live_subscribers:
            self._live_subscribers[symbol] = set()
        self._live_subscribers[symbol].add(exchange)

    def unsubscribe_live(self, symbol: str, exchange: str) -> None:
        """Remove a live price subscription."""
        if symbol in self._live_subscribers:
            self._live_subscribers[symbol].discard(exchange)

    def get_live_subscribers(self, symbol: str, exchange: str) -> bool:
        """Check if there are subscribers for this symbol/exchange."""
        return (
            symbol in self._live_subscribers
            and exchange in self._live_subscribers[symbol]
        )

    async def start(self) -> None:
        """Start monitoring all registered exchanges."""
        if not self.exchanges:
            raise RuntimeError("No exchanges registered.")
        self._stopping = False
        logger.info(f"Starting price monitor with {len(self.exchanges)} exchange(s)")

        tasks = [
            asyncio.create_task(self._monitor_exchange(exchange))
            for exchange in self.exchanges
        ]

        try:
            await asyncio.gather(*tasks)
        except Exception as e:
            logger.error(f"Error in price monitor: {e}")
            raise

    async def stop(self) -> None:
        """Stop monitoring and cleanup."""
        logger.info("Stopping price monitor")
        self._stopping = True
        for exchange in self.exchanges:
            try:
                await exchange.disconnect()
            except Exception as e:
                logger.error(f"Error disconnecting {exchange.exchange_name}: {e}")
        logger.info("Price monitor stopped")

    async def _monitor_exchange(self, exchange: BaseExchange) -> None:
        """Monitor a single exchange with auto-reconnection on disconnect."""
        reconnect_delay = 5.0
        while not self._stopping:
            try:
                if not exchange.is_running:
                    if hasattr(exchange, "reconnect"):
                        reconnected = await exchange.reconnect()
                        if not reconnected:
                            logger.warning(f"Reconnect failed, retrying in {reconnect_delay}s...")
                            await asyncio.sleep(reconnect_delay)
                            continue
                    else:
                        await exchange.connect()
                async for price_update in exchange.subscribe_all_tickers():
                    await self._process_price_update(price_update)
            except asyncio.CancelledError:
                raise
            except Exception as e:
                logger.error(f"Error monitoring {exchange.exchange_name}: {e}")
            if not exchange.is_running:
                logger.info(f"Connection closed for {exchange.exchange_name}, reconnecting in {reconnect_delay}s...")
            await asyncio.sleep(reconnect_delay)

    async def _process_price_update(self, price_update: PriceUpdate) -> None:
        """Process a price update: build candles, check alerts."""
        self.stats["total_updates"] += 1

        if self.stats["total_updates"] % 1000 == 0:
            logger.info(f"Processed {self.stats['total_updates']} total updates")

        # Broadcast for WebSocket subscribers
        await price_broadcaster.broadcast(price_update)

        # Build candles from price update
        completed_candles = await candle_service.add_price_update(price_update)
        self.stats["candles_created"] += len(completed_candles)

        # Check custom spike alerts
        alerts = await self.spike_detector.check_custom_alerts(
            price_update, completed_candles
        )
        for alert in alerts:
            self.stats["alerts_sent"] += 1
            await mongodb_service.store_detected_alert(alert)
            await webhook_service.send_alert(alert)

    def get_stats(self) -> dict:
        """Get monitoring statistics."""
        return {
            **self.stats,
            "active_exchanges": len([e for e in self.exchanges if e.is_running]),
        }


price_monitor = PriceMonitor()
