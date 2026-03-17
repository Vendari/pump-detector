import logging
from typing import Optional
from datetime import datetime

from app.config import settings
from app.models.price import PriceUpdate, PriceAlert
from app.database.mongodb import mongodb_service

logger = logging.getLogger(__name__)


class SpikeDetector:
    """
    Detects price spikes (pumps) and drops (dumps) based on historical data.
    
    This service:
    - Compares current price with historical prices in a time window
    - Detects when price change exceeds configured thresholds
    - Generates alerts for significant price movements
    """
    
    def __init__(self):
        """Initialize spike detector with configuration."""
        self.spike_threshold = settings.spike_threshold_percent
        self.drop_threshold = settings.drop_threshold_percent
        self.detection_window = settings.detection_window_minutes
        
        logger.info(
            f"Spike detector initialized: "
            f"spike={self.spike_threshold}%, drop={self.drop_threshold}%, "
            f"window={self.detection_window}min"
        )
    
    async def check_price_movement(self, price_update: PriceUpdate) -> Optional[PriceAlert]:
        """
        Check if a price update represents a significant spike or drop.
        
        This method:
        1. Gets historical prices from the detection window
        2. Compares current price with the earliest price in the window
        3. Calculates percentage change
        4. Returns alert if threshold exceeded
        
        Args:
            price_update: Current price update to check
            
        Returns:
            PriceAlert if spike/drop detected, None otherwise
        """
        try:
            # Get historical prices for this symbol
            history = await mongodb_service.get_price_history(
                symbol=price_update.symbol,
                exchange=price_update.exchange,
                minutes=self.detection_window
            )
            
            # Need at least some historical data to detect movement
            if len(history) < 2:
                logger.debug(f"Insufficient history for {price_update.symbol}, need more data")
                return None
            
            # Get the oldest price in the window (baseline)
            start_price = history[0]["price"]
            current_price = price_update.price
            
            # Calculate percentage change
            change_percent = ((current_price - start_price) / start_price) * 100
            
            # Check for spike (positive change)
            if change_percent >= self.spike_threshold:
                return PriceAlert(
                    symbol=price_update.symbol,
                    exchange=price_update.exchange,
                    alert_type="spike",
                    start_price=start_price,
                    current_price=current_price,
                    change_percent=change_percent,
                    window_minutes=self.detection_window,
                    timestamp=price_update.timestamp
                )
            
            # Check for drop (negative change)
            elif change_percent <= -self.drop_threshold:
                return PriceAlert(
                    symbol=price_update.symbol,
                    exchange=price_update.exchange,
                    alert_type="drop",
                    start_price=start_price,
                    current_price=current_price,
                    change_percent=change_percent,
                    window_minutes=self.detection_window,
                    timestamp=price_update.timestamp
                )
            
            # No significant movement
            return None
            
        except Exception as e:
            logger.error(f"Error checking price movement: {e}")
            return None
    
    def calculate_change_percent(self, old_price: float, new_price: float) -> float:
        """
        Calculate percentage change between two prices.
        
        Args:
            old_price: Previous price
            new_price: Current price
            
        Returns:
            Percentage change (positive for increase, negative for decrease)
        """
        if old_price == 0:
            return 0.0
        
        return ((new_price - old_price) / old_price) * 100
