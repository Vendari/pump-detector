"""Models for spike alert configuration."""

from datetime import datetime
from typing import Optional

from pydantic import BaseModel, Field


class SpikeAlertConfig(BaseModel):
    """
    Custom spike/drop alert configuration.

    Bot registers via API with webhook URL, exchange, interval, and threshold.
    Positive percentage = spike (pump), negative = drop (dump).
    """

    webhook_url: str = Field(..., description="URL to POST when alert triggers")
    exchange: str = Field(..., description="Exchange to monitor (e.g., binance_futures)")
    interval: str = Field(
        ...,
        description="Time period for spike detection (e.g., 1m, 10m, 1h, 1D)",
    )
    threshold_percent: float = Field(
        ...,
        description="Percentage change threshold. Positive=spike, negative=drop",
    )
    symbol: Optional[str] = Field(
        None,
        description="Specific symbol to monitor, or None for all symbols",
    )
    created_at: datetime = Field(default_factory=datetime.utcnow)
    alert_id: Optional[str] = Field(None, description="Unique alert identifier")

    class Config:
        json_schema_extra = {
            "example": {
                "webhook_url": "https://my-bot.com/webhook/alert",
                "exchange": "binance_futures",
                "interval": "10m",
                "threshold_percent": 10.0,
                "symbol": "BTCUSDT",
            }
        }
