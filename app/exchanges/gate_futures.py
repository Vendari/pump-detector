import asyncio
import json
import logging
import time
from datetime import datetime
from typing import AsyncIterator

import aiohttp
import websockets
from websockets.client import WebSocketClientProtocol

from app.exchanges.base import BaseExchange
from app.models.price import PriceUpdate

logger = logging.getLogger(__name__)

# Gate Futures WebSocket v4 — tickers are per-contract; there is no documented
# "!all" / wildcard subscribe for futures.tickers (unlike e.g. futures.orders).
# See: https://www.gate.com/docs/developers/futures/ws/en/#tickers-subscription
GATE_FUTURES_WS_USDT = "wss://fx-ws.gateio.ws/v4/ws/usdt"
GATE_REST_CONTRACTS = "https://api.gateio.ws/api/v4/futures/usdt/contracts"

# Subscribe payload size (not documented; keep conservative)
TICKER_SUBSCRIBE_BATCH = 80
CONTRACT_REFRESH_SECONDS = 15 * 60


def gate_contract_to_symbol(contract: str) -> str:
    """Normalize BTC_USDT -> BTCUSDT for parity with Binance-style symbols."""
    return contract.replace("_", "").upper()


class GateFuturesExchange(BaseExchange):
    """
    Gate.io USDT-settled perpetual futures — public ticker stream per contract.

    Discovers contracts via REST, subscribes to futures.tickers in batches,
    and periodically re-fetches contracts to subscribe to newly listed markets.
    """

    def __init__(self) -> None:
        super().__init__(exchange_name="gate_futures")
        self._websocket: WebSocketClientProtocol | None = None
        self._reconnect_attempts = 0
        self._max_reconnect_attempts = 10
        self._subscribed_contracts: set[str] = set()
        self._refresh_task: asyncio.Task[None] | None = None
        self._http_session: aiohttp.ClientSession | None = None

    async def _get_session(self) -> aiohttp.ClientSession:
        if self._http_session is None or self._http_session.closed:
            self._http_session = aiohttp.ClientSession(
                timeout=aiohttp.ClientTimeout(total=30)
            )
        return self._http_session

    async def fetch_trading_contracts(self) -> list[str]:
        """List active USDT perpetual contract names from REST."""
        session = await self._get_session()
        async with session.get(GATE_REST_CONTRACTS) as resp:
            resp.raise_for_status()
            rows = await resp.json()
        out: list[str] = []
        for row in rows:
            if not isinstance(row, dict):
                continue
            name = row.get("name")
            if not name or not isinstance(name, str):
                continue
            if row.get("in_delisting") is True:
                continue
            if row.get("status") != "trading":
                continue
            out.append(name)
        return sorted(set(out))

    def _ws_request(self, channel: str, event: str, payload: list[str]) -> str:
        return json.dumps(
            {
                "time": int(time.time()),
                "channel": channel,
                "event": event,
                "payload": payload,
            }
        )

    async def _subscribe_ticker_batches(self, contracts: list[str]) -> None:
        if not self._websocket:
            raise RuntimeError("WebSocket not connected")
        for i in range(0, len(contracts), TICKER_SUBSCRIBE_BATCH):
            batch = contracts[i : i + TICKER_SUBSCRIBE_BATCH]
            msg = self._ws_request("futures.tickers", "subscribe", batch)
            await self._websocket.send(msg)
            # Allow server to process; avoids bursting hundreds of subs at once
            await asyncio.sleep(0.05)

    async def _subscribe_new_contracts(self, names: list[str]) -> None:
        new = [n for n in names if n not in self._subscribed_contracts]
        if not new:
            return
        logger.info(
            "Gate futures: subscribing to %d new contract(s): %s%s",
            len(new),
            new[:5],
            "..." if len(new) > 5 else "",
        )
        await self._subscribe_ticker_batches(new)
        self._subscribed_contracts.update(new)

    async def _subscription_refresh_loop(self) -> None:
        while self._is_running and self._websocket:
            try:
                await asyncio.sleep(CONTRACT_REFRESH_SECONDS)
                if not self._is_running or not self._websocket:
                    break
                contracts = await self.fetch_trading_contracts()
                await self._subscribe_new_contracts(contracts)
            except asyncio.CancelledError:
                raise
            except Exception as e:
                logger.warning("Gate futures contract refresh failed: %s", e)

    async def _pump_raw_messages(
        self, out: asyncio.Queue[str | None]
    ) -> None:
        """Read WebSocket frames as fast as possible; parsing happens in subscribe_all_tickers."""
        if not self._websocket:
            await out.put(None)
            return
        try:
            async for raw in self._websocket:
                if not self._is_running:
                    break
                await out.put(raw)
        except websockets.exceptions.ConnectionClosed:
            logger.warning("Gate futures WebSocket closed")
            self._is_running = False
        except asyncio.CancelledError:
            raise
        except Exception as e:
            logger.error("Gate futures read loop error: %s", e)
            self._is_running = False
        finally:
            await out.put(None)

    def _raw_message_to_updates(self, raw: str) -> list[PriceUpdate]:
        """Turn one WS text frame into ticker updates (may be empty)."""
        try:
            data = json.loads(raw)
        except json.JSONDecodeError as e:
            logger.debug("Gate futures JSON error: %s", e)
            return []

        err = data.get("error")
        if err:
            logger.warning("Gate futures channel error: %s", err)
            return []

        channel = data.get("channel")
        event = data.get("event")
        if channel != "futures.tickers" or event != "update":
            return []

        result = data.get("result")
        if not isinstance(result, list):
            return []

        time_ms = data.get("time_ms")
        ts_base = (
            datetime.utcfromtimestamp(time_ms / 1000.0)
            if isinstance(time_ms, int)
            else datetime.utcnow()
        )

        out: list[PriceUpdate] = []
        for item in result:
            if not isinstance(item, dict):
                continue
            update = self._parse_ticker(item, ts_base)
            if update:
                out.append(update)
        return out

    async def connect(self) -> None:
        try:
            extra_headers = [("X-Gate-Size-Decimal", "1")]
            logger.info("Connecting to Gate futures: %s", GATE_FUTURES_WS_USDT)
            # High contract count → bursty traffic; slow downstream work must not stall reads
            # or keepalive ping/pong can time out. Longer ping_timeout is a backup; the queue
            # pump below is the main fix.
            self._websocket = await websockets.connect(
                GATE_FUTURES_WS_USDT,
                extra_headers=extra_headers,
                ping_interval=30,
                ping_timeout=120,
                close_timeout=10,
            )
            self._is_running = True
            self._reconnect_attempts = 0
            self._subscribed_contracts.clear()
            logger.info("Connected to Gate futures WebSocket")
        except Exception as e:
            logger.error("Failed to connect to Gate futures: %s", e)
            self._is_running = False
            raise

    async def disconnect(self) -> None:
        self._is_running = False
        if self._refresh_task:
            self._refresh_task.cancel()
            try:
                await self._refresh_task
            except asyncio.CancelledError:
                pass
            self._refresh_task = None

        if self._websocket:
            try:
                await self._websocket.close()
                logger.info("Disconnected from Gate futures WebSocket")
            except Exception as e:
                logger.error("Error during Gate disconnect: %s", e)
            finally:
                self._websocket = None

        if self._http_session and not self._http_session.closed:
            await self._http_session.close()
            self._http_session = None

        self._subscribed_contracts.clear()

    async def subscribe_all_tickers(self) -> AsyncIterator[PriceUpdate]:
        if not self._websocket:
            raise RuntimeError("WebSocket not connected. Call connect() first.")

        contracts = await self.fetch_trading_contracts()
        if not contracts:
            logger.error("Gate futures: no trading contracts from REST")
        else:
            logger.info(
                "Gate futures: initial subscribe for %d contract(s)",
                len(contracts),
            )
            await self._subscribe_ticker_batches(contracts)
            self._subscribed_contracts = set(contracts)

        self._refresh_task = asyncio.create_task(self._subscription_refresh_loop())

        raw_queue: asyncio.Queue[str | None] = asyncio.Queue()
        pump_task = asyncio.create_task(self._pump_raw_messages(raw_queue))

        try:
            while True:
                raw = await raw_queue.get()
                if raw is None:
                    break
                if not self._is_running:
                    break
                for update in self._raw_message_to_updates(raw):
                    yield update
        finally:
            pump_task.cancel()
            try:
                await pump_task
            except asyncio.CancelledError:
                pass
            if self._refresh_task:
                self._refresh_task.cancel()
                try:
                    await self._refresh_task
                except asyncio.CancelledError:
                    pass
                self._refresh_task = None

    def _parse_ticker(self, item: dict, ts: datetime) -> PriceUpdate | None:
        contract = item.get("contract")
        if not contract or not isinstance(contract, str):
            return None

        def _f(key: str) -> float:
            v = item.get(key)
            if v is None or v == "":
                return 0.0
            try:
                return float(v)
            except (TypeError, ValueError):
                return 0.0

        mark = _f("mark_price")
        last = _f("last")
        index_p = _f("index_price")
        price = mark if mark > 0 else last
        if price <= 0:
            return None

        # Cumulative 24h quote volume (USDT for USDT-margined); candle layer uses deltas.
        quote_vol_24h = _f("volume_24h_quote")
        if quote_vol_24h <= 0:
            quote_vol_24h = _f("volume_24h")

        symbol = gate_contract_to_symbol(contract)
        return PriceUpdate(
            symbol=symbol,
            exchange=self.exchange_name,
            price=price,
            mark_price=mark if mark > 0 else None,
            index_price=index_p if index_p > 0 else None,
            quote_volume_24h=quote_vol_24h if quote_vol_24h > 0 else None,
            timestamp=ts,
        )

    async def reconnect(self) -> bool:
        if self._reconnect_attempts >= self._max_reconnect_attempts:
            logger.error(
                "Gate futures: max reconnect attempts (%s) reached",
                self._max_reconnect_attempts,
            )
            return False
        self._reconnect_attempts += 1
        logger.info(
            "Gate futures reconnect attempt %s/%s",
            self._reconnect_attempts,
            self._max_reconnect_attempts,
        )
        try:
            await self.disconnect()
            await self.connect()
            return True
        except Exception as e:
            logger.error("Gate futures reconnection failed: %s", e)
            return False
