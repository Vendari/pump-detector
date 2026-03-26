"""Candle and interval models for OHLC data."""

from datetime import datetime
from enum import Enum
from typing import List, Tuple

from pydantic import BaseModel, Field


class CandleInterval(str, Enum):
    """Supported candle intervals with their duration in seconds."""

    S10 = "10s"
    S30 = "30s"
    M1 = "1m"
    M2 = "2m"
    M3 = "3m"
    M4 = "4m"
    M5 = "5m"
    M10 = "10m"
    M15 = "15m"
    M30 = "30m"
    M45 = "45m"
    H1 = "1h"
    H2 = "2h"
    H3 = "3h"
    H4 = "4h"
    H6 = "6h"
    H12 = "12h"
    D1 = "1D"
    W1 = "1W"


# Interval duration in seconds (for aggregation logic)
INTERVAL_SECONDS: dict[CandleInterval, int] = {
    CandleInterval.S10: 10,
    CandleInterval.S30: 30,
    CandleInterval.M1: 60,
    CandleInterval.M2: 120,
    CandleInterval.M3: 180,
    CandleInterval.M4: 240,
    CandleInterval.M5: 300,
    CandleInterval.M10: 600,
    CandleInterval.M15: 900,
    CandleInterval.M30: 1800,
    CandleInterval.M45: 2700,
    CandleInterval.H1: 3600,
    CandleInterval.H2: 7200,
    CandleInterval.H3: 10800,
    CandleInterval.H4: 14400,
    CandleInterval.H6: 21600,
    CandleInterval.H12: 43200,
    CandleInterval.D1: 86400,
    CandleInterval.W1: 604800,
}

# Intervals ordered for aggregation (next_seconds/prev_seconds must be int)
# Path: 30m->1h(2), 1h->3h(3), 3h->6h(2), 6h->12h(2), 12h->1D(2)
INTERVAL_ORDER: List[CandleInterval] = [
    CandleInterval.S10,
    CandleInterval.S30,
    CandleInterval.M1,
    CandleInterval.M2,
    CandleInterval.M3,
    CandleInterval.M4,
    CandleInterval.M5,
    CandleInterval.M10,
    CandleInterval.M15,
    CandleInterval.M30,
    CandleInterval.H1,
    CandleInterval.H3,
    CandleInterval.H6,
    CandleInterval.H12,
    CandleInterval.D1,
    CandleInterval.W1,
]

# Max candles to retain per symbol/exchange/interval
MAX_CANDLES_PER_INTERVAL = 100


class Candle(BaseModel):
    """OHLC candle data."""

    symbol: str = Field(..., description="Trading pair symbol")
    exchange: str = Field(..., description="Exchange name")
    interval: str = Field(..., description="Candle interval (e.g., 1m, 1h)")
    open: float = Field(..., description="Opening price")
    high: float = Field(..., description="Highest price")
    low: float = Field(..., description="Lowest price")
    close: float = Field(..., description="Closing price")
    timestamp: datetime = Field(..., description="Candle start timestamp (aligned to interval)")
    volume: float = Field(default=0.0, description="Volume (if available)")

    class Config:
        json_schema_extra = {
            "example": {
                "symbol": "BTCUSDT",
                "exchange": "binance_futures",
                "interval": "1m",
                "open": 45000.0,
                "high": 45100.0,
                "low": 44950.0,
                "close": 45050.0,
                "timestamp": "2026-01-27T12:00:00Z",
                "volume": 100.5,
            }
        }


def parse_interval(interval_str: str) -> CandleInterval | None:
    """Parse interval string to CandleInterval enum. Accepts 24h as alias for 1D."""
    if interval_str == "24h":
        return CandleInterval.D1
    try:
        return CandleInterval(interval_str)
    except ValueError:
        return None


def get_interval_seconds(interval: CandleInterval) -> int:
    """Get duration in seconds for an interval."""
    return INTERVAL_SECONDS.get(interval, 0)


def get_parent_interval(interval: CandleInterval) -> CandleInterval | None:
    """Get the next larger interval for aggregation."""
    idx = INTERVAL_ORDER.index(interval)
    if idx + 1 < len(INTERVAL_ORDER):
        return INTERVAL_ORDER[idx + 1]
    return None
