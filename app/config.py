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
    
    class Config:
        env_file = ".env"
        case_sensitive = False
        extra = "ignore"  # Ignore deprecated env vars


settings = Settings()
