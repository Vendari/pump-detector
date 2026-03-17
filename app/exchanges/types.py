"""
Type definitions for exchange implementations.

This module provides type hints and protocol definitions to ensure
type safety across exchange implementations.
"""

from typing import Protocol, AsyncIterator
from app.models.price import PriceUpdate


class ExchangeProtocol(Protocol):
    """
    Protocol defining the interface that all exchanges must implement.
    
    This can be used for type checking to ensure exchange implementations
    follow the correct interface.
    """
    
    exchange_name: str
    
    async def connect(self) -> None:
        """Establish connection to the exchange."""
        ...
    
    async def disconnect(self) -> None:
        """Close connection to the exchange."""
        ...
    
    async def subscribe_all_tickers(self) -> AsyncIterator[PriceUpdate]:
        """Subscribe to all ticker price updates."""
        ...
    
    @property
    def is_running(self) -> bool:
        """Check if the exchange connection is active."""
        ...
