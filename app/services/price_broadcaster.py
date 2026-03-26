"""
Live price broadcaster for WebSocket subscribers.

Stores latest price per (symbol, exchange) and broadcasts to WebSocket clients.
"""

import asyncio
import logging
from typing import Dict, Optional, Set, Tuple

from app.models.price import PriceUpdate

logger = logging.getLogger(__name__)


class PriceBroadcaster:
    """
    Broadcasts live price updates to WebSocket subscribers.

    Subscriptions: (symbol, exchange) - clients subscribe to specific pairs.
    """

    def __init__(self) -> None:
        self._subscribers: Dict[Tuple[str, str], Set[asyncio.Queue]] = {}
        self._latest_prices: Dict[Tuple[str, str], PriceUpdate] = {}
        self._lock = asyncio.Lock()

    def subscribe(self, symbol: str, exchange: str) -> asyncio.Queue:
        """Create a subscription queue for symbol/exchange."""
        key = (symbol.upper(), exchange)
        queue: asyncio.Queue = asyncio.Queue(maxsize=100)
        if key not in self._subscribers:
            self._subscribers[key] = set()
        self._subscribers[key].add(queue)
        return queue

    def unsubscribe(self, symbol: str, exchange: str, queue: asyncio.Queue) -> None:
        """Remove a subscription."""
        key = (symbol.upper(), exchange)
        if key in self._subscribers:
            self._subscribers[key].discard(queue)
            if not self._subscribers[key]:
                del self._subscribers[key]

    async def broadcast(self, update: PriceUpdate) -> None:
        """Broadcast price update to subscribers."""
        key = (update.symbol.upper(), update.exchange)
        self._latest_prices[key] = update

        async with self._lock:
            queues = list(self._subscribers.get(key, set()))

        payload = {
            "symbol": update.symbol,
            "exchange": update.exchange,
            "price": update.price,
            "timestamp": update.timestamp.isoformat(),
        }

        for q in queues:
            try:
                q.put_nowait(payload)
            except asyncio.QueueFull:
                pass

    def get_latest(self, symbol: str, exchange: str) -> Optional[dict]:
        """Get latest price for symbol/exchange."""
        key = (symbol.upper(), exchange)
        update = self._latest_prices.get(key)
        if update:
            return {
                "symbol": update.symbol,
                "exchange": update.exchange,
                "price": update.price,
                "timestamp": update.timestamp.isoformat(),
            }
        return None


price_broadcaster = PriceBroadcaster()
