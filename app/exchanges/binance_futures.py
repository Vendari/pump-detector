import json
import logging
from datetime import datetime
from typing import AsyncIterator
import websockets
from websockets.client import WebSocketClientProtocol

from app.exchanges.base import BaseExchange
from app.models.price import PriceUpdate

logger = logging.getLogger(__name__)


class BinanceFuturesExchange(BaseExchange):
    """
    Binance USDⓈ-M Futures exchange connector.
    
    Connects to Binance Futures WebSocket API and streams mark prices
    for all trading pairs using the !markPrice@arr stream.
    
    Reference: https://developers.binance.com/docs/derivatives/usds-margined-futures/websocket-market-streams/Mark-Price-Stream-for-All-market
    """
    
    # Binance Futures WebSocket base URL
    WS_BASE_URL = "wss://fstream.binance.com"
    
    # Stream name for all market mark prices (updates every 1 second)
    STREAM_NAME = "!markPrice@arr@1s"
    
    def __init__(self):
        """Initialize Binance Futures exchange connector."""
        super().__init__(exchange_name="binance_futures")
        self._websocket: WebSocketClientProtocol | None = None
        self._reconnect_attempts = 0
        self._max_reconnect_attempts = 10
    
    async def connect(self) -> None:
        """Establish WebSocket connection to Binance Futures."""
        try:
            ws_url = f"{self.WS_BASE_URL}/ws/{self.STREAM_NAME}"
            logger.info(f"Connecting to Binance Futures: {ws_url}")
            
            self._websocket = await websockets.connect(
                ws_url,
                ping_interval=20,
                ping_timeout=10,
                close_timeout=10
            )
            
            self._is_running = True
            self._reconnect_attempts = 0
            logger.info("Successfully connected to Binance Futures WebSocket")
            
        except Exception as e:
            logger.error(f"Failed to connect to Binance Futures: {e}")
            self._is_running = False
            raise
    
    async def disconnect(self) -> None:
        """Close WebSocket connection."""
        self._is_running = False
        
        if self._websocket:
            try:
                await self._websocket.close()
                logger.info("Disconnected from Binance Futures WebSocket")
            except Exception as e:
                logger.error(f"Error during disconnect: {e}")
            finally:
                self._websocket = None
    
    async def subscribe_all_tickers(self) -> AsyncIterator[PriceUpdate]:
        """
        Subscribe to all ticker price updates from Binance Futures.
        
        The stream provides mark price updates for all symbols every 1 second.
        Each message contains an array of price updates for all trading pairs.
        
        Yields:
            PriceUpdate: Price update for each trading pair
        """
        if not self._websocket:
            raise RuntimeError("WebSocket not connected. Call connect() first.")
        
        logger.info("Starting to stream price updates from Binance Futures")
        
        try:
            async for message in self._websocket:
                try:
                    # Parse the JSON message
                    data = json.loads(message)
                    
                    # The message is an array of price updates
                    if not isinstance(data, list):
                        logger.warning(f"Unexpected message format: {type(data)}")
                        continue
                    
                    # Process each symbol's price update
                    for item in data:
                        try:
                            price_update = self._parse_price_update(item)
                            if price_update:
                                yield price_update
                        except Exception as e:
                            logger.error(f"Error parsing price update: {e}, data: {item}")
                            continue
                    
                except json.JSONDecodeError as e:
                    logger.error(f"Failed to decode JSON message: {e}")
                    continue
                except Exception as e:
                    logger.error(f"Error processing message: {e}")
                    continue
        
        except websockets.exceptions.ConnectionClosed:
            logger.warning("WebSocket connection closed")
            self._is_running = False
        except Exception as e:
            logger.error(f"Error in price stream: {e}")
            self._is_running = False
            raise
    
    def _parse_price_update(self, data: dict) -> PriceUpdate | None:
        """
        Parse a single price update from Binance Futures message.
        
        Message format:
        {
            "e": "markPriceUpdate",
            "E": 1562305380000,  # Event time
            "s": "BTCUSDT",      # Symbol
            "p": "11185.87786614",  # Mark price
            "i": "11784.62659091",  # Index price
            "P": "11784.25641265",  # Estimated settle price
            "r": "0.00030000",   # Funding rate
            "T": 1562306400000   # Next funding time
        }
        
        Args:
            data: Raw price update dictionary from WebSocket
            
        Returns:
            PriceUpdate object or None if parsing fails
        """
        try:
            # Skip if not a mark price update event
            if data.get("e") != "markPriceUpdate":
                return None
            
            symbol = data.get("s")
            mark_price = float(data.get("p", 0))
            index_price = float(data.get("i", 0))
            event_time = int(data.get("E", 0))
            
            # Validate required fields
            if not symbol or mark_price <= 0:
                logger.debug(f"Invalid price data: {data}")
                return None
            
            # Convert event time from milliseconds to datetime
            timestamp = datetime.utcfromtimestamp(event_time / 1000.0)
            
            return PriceUpdate(
                symbol=symbol,
                exchange=self.exchange_name,
                price=mark_price,
                mark_price=mark_price,
                index_price=index_price,
                timestamp=timestamp
            )
            
        except (ValueError, KeyError) as e:
            logger.error(f"Error parsing price update: {e}, data: {data}")
            return None
    
    async def reconnect(self) -> bool:
        """
        Attempt to reconnect to the WebSocket.
        
        Returns:
            bool: True if reconnection successful, False otherwise
        """
        if self._reconnect_attempts >= self._max_reconnect_attempts:
            logger.error(f"Max reconnection attempts ({self._max_reconnect_attempts}) reached")
            return False
        
        self._reconnect_attempts += 1
        logger.info(f"Attempting reconnection (attempt {self._reconnect_attempts}/{self._max_reconnect_attempts})")
        
        try:
            await self.disconnect()
            await self.connect()
            return True
        except Exception as e:
            logger.error(f"Reconnection failed: {e}")
            return False
