from pydantic_settings import BaseSettings
from typing import Optional


class Settings(BaseSettings):
    """Application configuration."""
    
    # MongoDB
    mongodb_url: str = "mongodb://localhost:27017"
    mongodb_db_name: str = "crypto_pump_detector"
    
    # Price History
    price_retention_hours: int = 12
    
    # Spike Detection
    spike_threshold_percent: float = 5.0
    drop_threshold_percent: float = 5.0
    detection_window_minutes: int = 5
    
    # Webhook
    webhook_url: Optional[str] = None
    webhook_timeout_seconds: int = 10
    
    # Application
    log_level: str = "INFO"
    
    class Config:
        env_file = ".env"
        case_sensitive = False


settings = Settings()
