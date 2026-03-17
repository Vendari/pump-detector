import logging
from typing import Optional
import aiohttp

from app.config import settings
from app.models.price import PriceAlert

logger = logging.getLogger(__name__)


class WebhookService:
    """
    Service for sending webhook notifications when price alerts are detected.
    
    This service:
    - Sends HTTP POST requests to configured webhook URL
    - Handles timeouts and errors gracefully
    - Provides retry logic (optional)
    """
    
    def __init__(self):
        """Initialize webhook service."""
        self.webhook_url = settings.webhook_url
        self.timeout = settings.webhook_timeout_seconds
        self._session: Optional[aiohttp.ClientSession] = None
        
        if self.webhook_url:
            logger.info(f"Webhook service initialized: {self.webhook_url}")
        else:
            logger.warning("Webhook URL not configured, alerts will be logged only")
    
    async def _get_session(self) -> aiohttp.ClientSession:
        """Get or create aiohttp session."""
        if self._session is None or self._session.closed:
            self._session = aiohttp.ClientSession()
        return self._session
    
    async def close(self) -> None:
        """Close the aiohttp session."""
        if self._session and not self._session.closed:
            await self._session.close()
    
    async def send_alert(self, alert: PriceAlert) -> bool:
        """
        Send a price alert to the configured webhook URL.
        
        Args:
            alert: Price alert to send
            
        Returns:
            bool: True if webhook sent successfully, False otherwise
        """
        if not self.webhook_url:
            logger.debug("Webhook URL not configured, skipping")
            return False
        
        try:
            # Prepare payload
            payload = {
                "symbol": alert.symbol,
                "exchange": alert.exchange,
                "alert_type": alert.alert_type,
                "start_price": alert.start_price,
                "current_price": alert.current_price,
                "change_percent": round(alert.change_percent, 2),
                "window_minutes": alert.window_minutes,
                "timestamp": alert.timestamp.isoformat(),
            }
            
            # Send webhook
            session = await self._get_session()
            timeout = aiohttp.ClientTimeout(total=self.timeout)
            
            async with session.post(
                self.webhook_url,
                json=payload,
                timeout=timeout
            ) as response:
                if response.status == 200:
                    logger.info(f"Webhook sent successfully for {alert.symbol}")
                    return True
                else:
                    logger.warning(
                        f"Webhook returned status {response.status} for {alert.symbol}"
                    )
                    return False
                    
        except aiohttp.ClientError as e:
            logger.error(f"HTTP error sending webhook: {e}")
            return False
        except Exception as e:
            logger.error(f"Unexpected error sending webhook: {e}")
            return False
    
    async def test_webhook(self) -> bool:
        """
        Test the webhook endpoint with a sample alert.
        
        Returns:
            bool: True if webhook endpoint is reachable
        """
        if not self.webhook_url:
            logger.error("Webhook URL not configured")
            return False
        
        test_alert = PriceAlert(
            symbol="TESTUSDT",
            exchange="test_exchange",
            alert_type="spike",
            start_price=100.0,
            current_price=105.0,
            change_percent=5.0,
            window_minutes=5
        )
        
        return await self.send_alert(test_alert)


# Global instance
webhook_service = WebhookService()
