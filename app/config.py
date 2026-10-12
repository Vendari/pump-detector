from pydantic_settings import BaseSettings
from typing import Optional


class Settings(BaseSettings):
    """Application configuration."""

    # MongoDB
    mongodb_url: str = "mongodb://localhost:27017"
    mongodb_db_name: str = "crypto_pump_detector"

    # Webhook (timeout for alert webhooks)
    webhook_timeout_seconds: int = 10

    # Webhook for 10% tier alerts (5% tier = DB only)
    webhook_url: Optional[str] = None
    webhook_api_key: Optional[str] = None

    # Application
    log_level: str = "INFO"

    # Shared key for every endpoint except /health: HTTP `x-api-key` header,
    # WebSocket `x-api-key` header or `?key=`. Unset = open (local only).
    api_key: Optional[str] = None

    # Exchanges that build candles and run spike alerts (heavy on Mongo).
    # Other exchanges only serve live prices on /ws/price and /price.
    candle_exchanges: str = "binance_futures,gate_futures"

    @property
    def candle_exchange_set(self) -> set[str]:
        return {e.strip() for e in self.candle_exchanges.split(",") if e.strip()}
    
    class Config:
        env_file = ".env"
        case_sensitive = False
        extra = "ignore"  # Ignore deprecated env vars


settings = Settings()
