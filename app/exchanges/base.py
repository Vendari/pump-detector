from abc import ABC, abstractmethod
from typing import AsyncIterator, Optional
from app.models.price import PriceUpdate


class BaseExchange(ABC):
    """
    Base interface for all exchange implementations.
    
    This abstract class defines the standard interface that all exchange
    connectors must implement, ensuring modularity and easy extensibility.
    """
    
    def __init__(self, exchange_name: str):
        """
        Initialize the exchange connector.
        
        Args:
            exchange_name: Unique identifier for this exchange (e.g., 'binance_futures')
        """
        self.exchange_name = exchange_name
        self._is_running = False
    
    @abstractmethod
    async def connect(self) -> None:
        """
        Establish connection to the exchange's WebSocket API.
        
        This method should handle:
        - WebSocket connection setup
        - Authentication if required
        - Initial subscription to price streams
        """
        pass
    
    @abstractmethod
    async def disconnect(self) -> None:
        """
        Close connection to the exchange's WebSocket API.
        
        This method should handle:
        - Graceful WebSocket closure
        - Cleanup of resources
        """
        pass
    
    @abstractmethod
    async def subscribe_all_tickers(self) -> AsyncIterator[PriceUpdate]:
        """
        Subscribe to price updates for all available trading pairs.
        
        This is an async generator that yields PriceUpdate objects as they
        arrive from the exchange's WebSocket stream.
        
        Yields:
            PriceUpdate: Price update for a trading pair
            
        Example:
            async for price_update in exchange.subscribe_all_tickers():
                print(f"{price_update.symbol}: {price_update.price}")
        """
        pass
    
    @property
    def is_running(self) -> bool:
        """Check if the exchange connection is active."""
        return self._is_running
    
    def __repr__(self) -> str:
        return f"<{self.__class__.__name__}(exchange={self.exchange_name}, running={self.is_running})>"
