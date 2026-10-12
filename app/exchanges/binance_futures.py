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
    Binance USDⓈ-M Futures: last price for every market from the all-market
    mini ticker stream (`!miniTicker@arr`, one array per second with the
    symbols that changed).

    Binance moved market streams to `/market/ws` (checked 2026-10-12); the old
    `/ws/!markPrice@arr@1s` URL connects but no longer sends anything.
    """

    WS_URL = "wss://fstream.binance.com/market/ws/!miniTicker@arr"
    EXCHANGE_NAME = "binance_futures"

    def __init__(self):
        super().__init__(exchange_name=self.EXCHANGE_NAME)
        self._websocket: WebSocketClientProtocol | None = None

    async def connect(self) -> None:
        try:
            logger.info(f"Connecting to {self.exchange_name}: {self.WS_URL}")
            self._websocket = await websockets.connect(
                self.WS_URL,
                ping_interval=20,
                ping_timeout=10,
                close_timeout=10,
            )
            self._is_running = True
            logger.info(f"Connected to {self.exchange_name}")
        except Exception as e:
            logger.error(f"Failed to connect to {self.exchange_name}: {e}")
            self._is_running = False
            raise

    async def disconnect(self) -> None:
        self._is_running = False
        if self._websocket:
            try:
                await self._websocket.close()
            except Exception as e:
                logger.error(f"Error during disconnect: {e}")
            finally:
                self._websocket = None

    async def subscribe_all_tickers(self) -> AsyncIterator[PriceUpdate]:
        if not self._websocket:
            raise RuntimeError("WebSocket not connected. Call connect() first.")
        try:
            async for message in self._websocket:
                for update in self.parse_message(message):
                    yield update
        except websockets.exceptions.ConnectionClosed:
            logger.warning(f"{self.exchange_name} WebSocket connection closed")
            self._is_running = False
        except Exception as e:
            logger.error(f"Error in {self.exchange_name} price stream: {e}")
            self._is_running = False
            raise

    def parse_message(self, message: str) -> list[PriceUpdate]:
        """
        One frame of `!miniTicker@arr`:
        [{"e":"24hrMiniTicker","E":1791807529762,"s":"CHIPUSDT","c":"0.06357",
          "o":"0.06664","h":"0.07088","l":"0.06217","v":"144088466","q":"9512345.6"}, ...]
        """
        try:
            data = json.loads(message)
        except json.JSONDecodeError:
            return []
        if not isinstance(data, list):
            return []
        out: list[PriceUpdate] = []
        for item in data:
            if not isinstance(item, dict) or item.get("e") != "24hrMiniTicker":
                continue
            try:
                symbol = item.get("s")
                price = float(item.get("c", 0))
                if not symbol or price <= 0:
                    continue
                quote_volume = item.get("q")
                out.append(
                    PriceUpdate(
                        symbol=symbol,
                        exchange=self.exchange_name,
                        price=price,
                        timestamp=datetime.utcfromtimestamp(int(item.get("E", 0)) / 1000.0),
                        quote_volume_24h=float(quote_volume) if quote_volume is not None else None,
                    )
                )
            except (TypeError, ValueError):
                continue
        return out

    async def reconnect(self) -> bool:
        # Pacing (and the day-long pause after ten failures) is PriceMonitor's.
        try:
            await self.disconnect()
            await self.connect()
            return True
        except Exception as e:
            logger.error(f"{self.exchange_name} reconnection failed: {e}")
            return False
