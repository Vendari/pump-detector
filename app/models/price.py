from datetime import datetime
from typing import Optional
from pydantic import BaseModel, Field


class PriceUpdate(BaseModel):
    """Represents a price update from an exchange."""
    
    symbol: str = Field(..., description="Trading pair symbol (e.g., BTCUSDT)")
    exchange: str = Field(..., description="Exchange name (e.g., binance_futures)")
    price: float = Field(..., description="Current price")
    timestamp: datetime = Field(default_factory=datetime.utcnow, description="Update timestamp")
    mark_price: Optional[float] = Field(None, description="Mark price (for futures)")
    index_price: Optional[float] = Field(None, description="Index price (for futures)")
    
    class Config:
        json_schema_extra = {
            "example": {
                "symbol": "BTCUSDT",
                "exchange": "binance_futures",
                "price": 45000.50,
                "timestamp": "2026-01-27T12:00:00Z",
                "mark_price": 45001.20,
                "index_price": 45002.00
            }
        }


class TickerOwnership(BaseModel):
    """Tracks which exchange owns a particular ticker."""
    
    symbol: str = Field(..., description="Trading pair symbol")
    exchange: str = Field(..., description="Exchange that owns this ticker")
    claimed_at: datetime = Field(default_factory=datetime.utcnow, description="When ownership was claimed")
    last_update: datetime = Field(default_factory=datetime.utcnow, description="Last price update timestamp")


class PriceAlert(BaseModel):
    """Represents a price spike or drop alert."""
    
    symbol: str
    exchange: str
    alert_type: str = Field(..., description="'spike' or 'drop'")
    start_price: float
    current_price: float
    change_percent: float
    window_minutes: int
    timestamp: datetime = Field(default_factory=datetime.utcnow)
    
    class Config:
        json_schema_extra = {
            "example": {
                "symbol": "BTCUSDT",
                "exchange": "binance_futures",
                "alert_type": "spike",
                "start_price": 45000.00,
                "current_price": 47500.00,
                "change_percent": 5.56,
                "window_minutes": 5,
                "timestamp": "2026-01-27T12:05:00Z"
            }
        }
