import asyncio
import logging
from typing import List, Dict
from collections import deque

from app.exchanges.base import BaseExchange
from app.models.price import PriceUpdate
from app.database.mongodb import mongodb_service
from app.services.spike_detector import SpikeDetector
from app.services.webhook_service import webhook_service

logger = logging.getLogger(__name__)


class PriceMonitor:
    """
    Coordinates price monitoring across multiple exchanges.
    
    This service:
    - Manages connections to multiple exchanges
    - Implements ticker ownership (first-come-first-serve)
    - Batches price updates for efficient database writes
    - Triggers spike detection on price updates
    """
    
    def __init__(self, batch_size: int = 100, batch_timeout: float = 1.0):
        """
        Initialize the price monitor.
        
        Args:
            batch_size: Number of price updates to batch before writing to DB
            batch_timeout: Maximum seconds to wait before flushing batch
        """
        self.exchanges: List[BaseExchange] = []
        self.spike_detector = SpikeDetector()
        self.batch_size = batch_size
        self.batch_timeout = batch_timeout
        
        # Batching mechanism for efficient DB writes
        self._price_batch: deque[PriceUpdate] = deque()
        self._batch_lock = asyncio.Lock()
        self._batch_task: asyncio.Task | None = None
        
        # Ticker ownership cache (symbol -> exchange_name)
        # This avoids DB lookups for every price update
        self._ownership_cache: Dict[str, str] = {}
        self._cache_lock = asyncio.Lock()
        
        # Statistics
        self.stats = {
            "total_updates": 0,
            "stored_updates": 0,
            "rejected_updates": 0,
            "ownership_claims": 0,
        }
    
    def add_exchange(self, exchange: BaseExchange) -> None:
        """
        Add an exchange to monitor.
        
        Args:
            exchange: Exchange instance to add
        """
        self.exchanges.append(exchange)
        logger.info(f"Added exchange: {exchange.exchange_name}")
    
    async def start(self) -> None:
        """
        Start monitoring all registered exchanges.
        
        This will:
        1. Connect to all exchanges
        2. Start consuming price updates
        3. Start batch flush task
        """
        if not self.exchanges:
            raise RuntimeError("No exchanges registered. Add at least one exchange before starting.")
        
        logger.info(f"Starting price monitor with {len(self.exchanges)} exchange(s)")
        
        # Start batch flush task
        self._batch_task = asyncio.create_task(self._batch_flush_loop())
        
        # Start monitoring each exchange in parallel
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
        """Stop monitoring and cleanup resources."""
        logger.info("Stopping price monitor")
        
        # Cancel batch task
        if self._batch_task:
            self._batch_task.cancel()
            try:
                await self._batch_task
            except asyncio.CancelledError:
                pass
        
        # Flush remaining batch
        await self._flush_batch()
        
        # Disconnect all exchanges
        for exchange in self.exchanges:
            try:
                await exchange.disconnect()
            except Exception as e:
                logger.error(f"Error disconnecting {exchange.exchange_name}: {e}")
        
        logger.info("Price monitor stopped")
    
    async def _monitor_exchange(self, exchange: BaseExchange) -> None:
        """
        Monitor a single exchange and process its price updates.
        
        Args:
            exchange: Exchange to monitor
        """
        try:
            # Connect to exchange
            await exchange.connect()
            
            # Subscribe to all tickers and process updates
            async for price_update in exchange.subscribe_all_tickers():
                await self._process_price_update(price_update)
                
        except Exception as e:
            logger.error(f"Error monitoring {exchange.exchange_name}: {e}")
            raise
    
    async def _process_price_update(self, price_update: PriceUpdate) -> None:
        """
        Process a single price update with ownership checking.
        
        Args:
            price_update: Price update to process
        """
        self.stats["total_updates"] += 1
        
        # Log every 1000 updates
        if self.stats["total_updates"] % 1000 == 0:
            logger.info(f"Processed {self.stats['total_updates']} total updates")
        
        # Check/claim ownership
        if not await self._handle_ticker_ownership(price_update):
            self.stats["rejected_updates"] += 1
            return
        
        # Add to batch for DB storage
        await self._add_to_batch(price_update)
        
        # Trigger spike detection (async, non-blocking)
        asyncio.create_task(self._check_for_spike(price_update))
        
        self.stats["stored_updates"] += 1
    
    async def _handle_ticker_ownership(self, price_update: PriceUpdate) -> bool:
        """
        Handle ticker ownership using first-come-first-serve logic.
        
        If the ticker is not owned by any exchange, claim it.
        If it's owned by this exchange, allow it.
        If it's owned by another exchange, reject it.
        
        Args:
            price_update: Price update to check
            
        Returns:
            bool: True if this exchange owns/claimed the ticker, False otherwise
        """
        symbol = price_update.symbol
        exchange = price_update.exchange
        
        # Check cache first (fast path)
        async with self._cache_lock:
            if symbol in self._ownership_cache:
                return self._ownership_cache[symbol] == exchange
        
        # Not in cache, check database
        owns_ticker = await mongodb_service.check_ticker_ownership(symbol, exchange)
        
        if owns_ticker:
            # Already owned by this exchange
            async with self._cache_lock:
                self._ownership_cache[symbol] = exchange
            await mongodb_service.update_ticker_last_seen(symbol, exchange)
            return True
        
        # Try to claim ownership (first-come-first-serve)
        claimed = await mongodb_service.claim_ticker_ownership(symbol, exchange)
        
        if claimed:
            async with self._cache_lock:
                self._ownership_cache[symbol] = exchange
            self.stats["ownership_claims"] += 1
            logger.info(f"Claimed ownership: {symbol} -> {exchange}")
            return True
        else:
            # Another exchange claimed it first
            # Find out who owns it and cache it
            ownership = await mongodb_service.ownership_collection.find_one({"symbol": symbol})
            if ownership:
                async with self._cache_lock:
                    self._ownership_cache[symbol] = ownership["exchange"]
            return False
    
    async def _add_to_batch(self, price_update: PriceUpdate) -> None:
        """
        Add a price update to the batch for bulk writing.
        
        Args:
            price_update: Price update to add
        """
        should_flush = False
        async with self._batch_lock:
            self._price_batch.append(price_update)
            
            # Check if batch is full
            if len(self._price_batch) >= self.batch_size:
                should_flush = True
        
        # Flush outside the lock to avoid deadlock
        if should_flush:
            await self._flush_batch()
    
    async def _flush_batch(self) -> None:
        """Write all batched price updates to database."""
        async with self._batch_lock:
            if not self._price_batch:
                return
            
            # Convert deque to list and clear
            updates = list(self._price_batch)
            self._price_batch.clear()
        
        # Write to database (outside the lock)
        try:
            count = await mongodb_service.bulk_store_price_updates(updates)
            logger.info(f"Flushed {count} price updates to database")
        except Exception as e:
            logger.error(f"Error flushing batch: {e}")
    
    async def _batch_flush_loop(self) -> None:
        """Periodically flush the batch based on timeout."""
        try:
            while True:
                await asyncio.sleep(self.batch_timeout)
                await self._flush_batch()
        except asyncio.CancelledError:
            logger.debug("Batch flush loop cancelled")
            raise
    
    async def _check_for_spike(self, price_update: PriceUpdate) -> None:
        """
        Check if the price update represents a spike or drop.
        
        Args:
            price_update: Price update to check
        """
        try:
            alert = await self.spike_detector.check_price_movement(price_update)
            
            if alert:
                logger.warning(
                    f"ALERT: {alert.alert_type.upper()} detected for {alert.symbol} "
                    f"on {alert.exchange}: {alert.change_percent:.2f}% "
                    f"({alert.start_price} -> {alert.current_price})"
                )
                
                # Send webhook notification
                await webhook_service.send_alert(alert)
                
        except Exception as e:
            logger.error(f"Error checking for spike: {e}")
    
    def get_stats(self) -> dict:
        """Get monitoring statistics."""
        return {
            **self.stats,
            "batch_size": len(self._price_batch),
            "cached_tickers": len(self._ownership_cache),
            "active_exchanges": len([e for e in self.exchanges if e.is_running]),
        }


# Global instance
price_monitor = PriceMonitor()
