"""
Candle aggregation service.

Builds OHLC candles from price updates and stores them in the database.
Supports multiple intervals: 10s, 30s, 1m, 2m, 3m, 4m, 5m, 10m, 15m, 30m, 45m, 1h, 2h, 3h, 4h, 1D, 1W.
Max 100 candles per symbol/exchange/interval - oldest are removed when limit exceeded.
"""

import logging
from collections import defaultdict
from dataclasses import dataclass
from datetime import datetime, timedelta
from typing import Dict, List, Optional

from app.models.candle import (
    Candle,
    CandleInterval,
    INTERVAL_ORDER,
    INTERVAL_SECONDS,
    MAX_CANDLES_PER_INTERVAL,
)
from app.models.price import PriceUpdate
from app.database.mongodb import mongodb_service

logger = logging.getLogger(__name__)


@dataclass
class PriceBucket:
    """Accumulated prices for a single time bucket."""

    open: float = 0.0
    high: float = 0.0
    low: float = float("inf")
    close: float = 0.0
    count: int = 0
    volume: float = 0.0

    def add_price(self, price: float, volume_delta: float = 0.0) -> None:
        """Add a price to the bucket; volume_delta is quote volume since last update (if known)."""
        if self.count == 0:
            self.open = self.high = self.low = self.close = price
        else:
            self.high = max(self.high, price)
            self.low = min(self.low, price)
            self.close = price
        self.count += 1
        if volume_delta > 0:
            self.volume += volume_delta

    def to_candle(
        self, symbol: str, exchange: str, interval: str, timestamp: datetime
    ) -> Candle:
        """Convert bucket to Candle model."""
        return Candle(
            symbol=symbol,
            exchange=exchange,
            interval=interval,
            open=self.open,
            high=self.high,
            low=self.low,
            close=self.close,
            timestamp=timestamp,
            volume=self.volume,
        )


def truncate_to_interval(ts: datetime, interval: CandleInterval) -> datetime:
    """Truncate timestamp to interval boundary using epoch-based calculation."""
    seconds = INTERVAL_SECONDS.get(interval, 60)
    epoch = datetime(1970, 1, 1, tzinfo=ts.tzinfo)
    total_secs = int((ts - epoch).total_seconds())
    bucket_secs = (total_secs // seconds) * seconds
    return epoch + timedelta(seconds=bucket_secs)


class CandleService:
    """
    Builds and stores OHLC candles from price updates.

    - 10s candles: built from raw price updates
    - Larger intervals: built by aggregating smaller candles
    - Enforces max 100 candles per symbol/exchange/interval
    """

    def __init__(self) -> None:
        """Initialize candle service."""
        # Buffer for 10s candles: (symbol, exchange) -> {bucket_ts: PriceBucket}
        self._price_buckets: Dict[tuple[str, str], Dict[datetime, PriceBucket]] = (
            defaultdict(dict)
        )
        # Buffer for larger intervals: (symbol, exchange, interval) -> {bucket_ts: List[Candle]}
        self._candle_buffers: Dict[
            tuple[str, str, CandleInterval], Dict[datetime, List[Candle]]
        ] = defaultdict(dict)
        self._lock_buckets: Dict[tuple[str, str], datetime] = {}
        # Last seen cumulative 24h quote volume per (symbol, exchange) — delta → candle volume
        self._last_quote_volume_24h: Dict[tuple[str, str], float] = {}

    async def add_price_update(self, update: PriceUpdate) -> List[Candle]:
        """
        Process a price update and return any newly completed candles.

        Returns list of candles that were completed and stored (caller may use for spike detection).
        """
        symbol = update.symbol
        exchange = update.exchange
        key = (symbol, exchange)

        volume_delta = 0.0
        if update.quote_volume_24h is not None:
            cum = float(update.quote_volume_24h)
            last = self._last_quote_volume_24h.get(key)
            if last is not None and cum >= last:
                volume_delta = cum - last
            self._last_quote_volume_24h[key] = cum

        # Current bucket for 10s
        bucket_ts = truncate_to_interval(update.timestamp, CandleInterval.S10)
        if key not in self._price_buckets:
            self._price_buckets[key] = {}

        buckets = self._price_buckets[key]
        if bucket_ts not in buckets:
            buckets[bucket_ts] = PriceBucket()
        buckets[bucket_ts].add_price(update.price, volume_delta)

        # Check if we've moved to a new 10s bucket - finalize previous
        completed: List[Candle] = []
        to_remove: List[datetime] = []

        for ts, bucket in list(buckets.items()):
            # If this bucket is before the current one (we've moved past it), finalize
            if ts < bucket_ts and bucket.count > 0:
                candle = bucket.to_candle(
                    symbol, exchange, CandleInterval.S10.value, ts
                )
                completed.append(candle)
                to_remove.append(ts)

        for ts in to_remove:
            del buckets[ts]

        # Store completed 10s candles and aggregate upward
        all_stored: List[Candle] = []
        for candle in completed:
            await mongodb_service.upsert_candle(candle)
            await mongodb_service.trim_candles(
                symbol, exchange, CandleInterval.S10.value, MAX_CANDLES_PER_INTERVAL
            )
            all_stored.append(candle)
            # Aggregate into larger intervals
            await self._aggregate_candle_upward(candle, all_stored)

        return all_stored

    async def _aggregate_candle_upward(
        self, candle: Candle, all_stored: List[Candle]
    ) -> None:
        """Aggregate a candle into larger intervals and store when complete."""
        symbol = candle.symbol
        exchange = candle.exchange
        interval_str = candle.interval

        try:
            interval = CandleInterval(interval_str)
        except ValueError:
            return

        idx = INTERVAL_ORDER.index(interval)
        if idx + 1 >= len(INTERVAL_ORDER):
            return

        next_interval = INTERVAL_ORDER[idx + 1]
        next_seconds = INTERVAL_SECONDS[next_interval]
        candles_needed = next_seconds // INTERVAL_SECONDS[interval]

        buffer_key = (symbol, exchange, next_interval)
        if buffer_key not in self._candle_buffers:
            self._candle_buffers[buffer_key] = {}

        buf = self._candle_buffers[buffer_key]
        bucket_ts = truncate_to_interval(candle.timestamp, next_interval)
        if bucket_ts not in buf:
            buf[bucket_ts] = []
        buf[bucket_ts].append(candle)

        # When we have enough, aggregate
        if len(buf[bucket_ts]) >= candles_needed:
            small_candles = buf[bucket_ts][:candles_needed]
            agg = self._aggregate_candles(small_candles, next_interval, bucket_ts)
            if agg:
                await mongodb_service.upsert_candle(agg)
                await mongodb_service.trim_candles(
                    symbol, exchange, next_interval.value, MAX_CANDLES_PER_INTERVAL
                )
                all_stored.append(agg)
                del buf[bucket_ts]
                # Recurse for next level
                await self._aggregate_candle_upward(agg, all_stored)

    def _aggregate_candles(
        self, candles: List[Candle], interval: CandleInterval, bucket_ts: datetime
    ) -> Optional[Candle]:
        """Aggregate multiple smaller candles into one larger candle."""
        if not candles:
            return None
        open_price = candles[0].open
        high_price = max(c.high for c in candles)
        low_price = min(c.low for c in candles)
        close_price = candles[-1].close
        volume = sum(c.volume for c in candles)

        return Candle(
            symbol=candles[0].symbol,
            exchange=candles[0].exchange,
            interval=interval.value,
            open=open_price,
            high=high_price,
            low=low_price,
            close=close_price,
            timestamp=bucket_ts,
            volume=volume,
        )


candle_service = CandleService()