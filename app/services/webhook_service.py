"""
Webhook service - sends spike/drop alerts to configured URLs.

Supports per-alert webhook URLs (from custom alert configs).
"""

import logging
from typing import Optional

import aiohttp

from app.config import settings
from app.models.price import PriceAlert

logger = logging.getLogger(__name__)


class WebhookService:
    """
    Sends HTTP POST requests to webhook URLs when alerts trigger.

    Uses webhook_url from each PriceAlert (custom alerts).
    """

    def __init__(self, timeout: int = 10):
        """Initialize webhook service."""
        self.timeout = timeout
        self._session: Optional[aiohttp.ClientSession] = None

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
        Send a price alert to the alert's webhook_url.

        Returns True if sent successfully.
        """
        webhook_url = alert.webhook_url
        if not webhook_url:
            logger.debug("No webhook URL in alert, skipping")
            return False

        try:
            payload = {
                "symbol": alert.symbol,
                "exchange": alert.exchange,
                "alert_type": alert.alert_type,
                "start_price": alert.start_price,
                "current_price": alert.current_price,
                "change_percent": round(alert.change_percent, 2),
                "interval": alert.interval,
                "timestamp": alert.timestamp.isoformat(),
                "alert_id": alert.alert_id,
            }

            session = await self._get_session()
            timeout = aiohttp.ClientTimeout(total=self.timeout)

            async with session.post(
                webhook_url,
                json=payload,
                timeout=timeout,
            ) as response:
                if response.status == 200:
                    logger.info(f"Webhook sent to {webhook_url} for {alert.symbol}")
                    return True
                else:
                    logger.warning(
                        f"Webhook returned {response.status} for {alert.symbol}"
                    )
                    return False

        except aiohttp.ClientError as e:
            logger.error(f"HTTP error sending webhook: {e}")
            return False
        except Exception as e:
            logger.error(f"Unexpected error sending webhook: {e}")
            return False

    async def test_webhook(self, url: str) -> bool:
        """Test a webhook URL with a sample alert."""
        test_alert = PriceAlert(
            symbol="TESTUSDT",
            exchange="test_exchange",
            alert_type="spike",
            start_price=100.0,
            current_price=105.0,
            change_percent=5.0,
            interval="1m",
            webhook_url=url,
        )
        return await self.send_alert(test_alert)


# Global instance
webhook_service = WebhookService(timeout=settings.webhook_timeout_seconds)
