"""
On-demand price feeds: connect to an exchange's public WebSocket only for the
symbols someone is watching (a /ws/price client), subscribing and
unsubscribing as watchers come and go. Used for exchanges where we don't need
every market for candles and alerts, only live prices (e.g. for Trigger's
trailing take profit): Bybit, KuCoin, Aster, Hyperliquid.

Feeds checked live 2026-10-12:
- Bybit `wss://stream.bybit.com/v5/public/linear`, `tickers.<SYM>` (deltas
  carry lastPrice only when it changed; max 10 args per request).
- KuCoin: token from `POST /api/v1/bullet-public`, wait for `welcome`, then
  `/contractMarket/execution:<SYM>` (trades; the ticker topic sends nothing).
- Aster `wss://fstream.asterdex.com/ws`, `<sym>@aggTrade` (its mini tickers
  only come every few seconds).
- Hyperliquid `wss://api.hyperliquid.xyz/ws`, `trades` per coin (allMids
  only pushes every few seconds).
"""

import asyncio
import json
import logging
import time
from datetime import datetime
from typing import AsyncIterator

import aiohttp
import websockets

from app.exchanges.base import BaseExchange
from app.models.price import PriceUpdate

logger = logging.getLogger(__name__)

IDLE_TIMEOUT_SECONDS = 60
LINGER_SECONDS = 60
PING_EVERY_SECONDS = 15


def _usdt(symbol: str) -> str:
    s = symbol.upper()
    return s if s.endswith("USDT") else f"{s}USDT"


def _num(v) -> float | None:
    try:
        p = float(v)
    except (TypeError, ValueError):
        return None
    return p if p > 0 else None


class OnDemandExchange(BaseExchange):
    """Subclasses describe the venue; this class runs the connection."""

    def __init__(self, exchange_name: str):
        super().__init__(exchange_name=exchange_name)
        self._wanted: dict[str, int] = {}  # symbol -> watcher count
        self._changed = asyncio.Event()
        self._is_running = True  # the connection is managed internally

    # --- venue description -------------------------------------------------
    async def ws_url(self) -> str:
        raise NotImplementedError

    def venue_symbol(self, symbol: str) -> str:
        return _usdt(symbol)

    def subscription(self, venue_symbols: list[str], subscribe: bool) -> list[str]:
        raise NotImplementedError

    def parse(self, text: str) -> list[tuple[str, float]]:
        """(venue symbol, last price) pairs in one frame."""
        raise NotImplementedError

    def ping_message(self) -> str | None:
        return None

    async def after_connect(self, ws) -> None:
        return None

    # --- watchers ----------------------------------------------------------
    def watch(self, symbol: str) -> None:
        symbol = _usdt(symbol)
        self._wanted[symbol] = self._wanted.get(symbol, 0) + 1
        self._changed.set()

    def unwatch(self, symbol: str) -> None:
        symbol = _usdt(symbol)
        n = self._wanted.get(symbol, 0) - 1
        if n <= 0:
            self._wanted.pop(symbol, None)
        else:
            self._wanted[symbol] = n
        self._changed.set()

    @property
    def watched(self) -> set[str]:
        return set(self._wanted)

    # --- BaseExchange ------------------------------------------------------
    async def connect(self) -> None:
        self._is_running = True

    async def disconnect(self) -> None:
        self._is_running = False
        self._changed.set()

    async def subscribe_all_tickers(self) -> AsyncIterator[PriceUpdate]:
        backoff = 1.0
        while self._is_running:
            if not self._wanted:
                self._changed.clear()
                await self._changed.wait()
                continue
            try:
                async for update in self._session():
                    backoff = 1.0
                    yield update
            except asyncio.CancelledError:
                raise
            except Exception as e:
                logger.warning(f"{self.exchange_name} price stream: {e}; reconnecting in {backoff:.0f}s")
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 30.0)

    async def _session(self) -> AsyncIterator[PriceUpdate]:
        url = await self.ws_url()
        async with websockets.connect(url, ping_interval=20, ping_timeout=20, close_timeout=5) as ws:
            await self.after_connect(ws)
            logger.info(f"{self.exchange_name} price stream connected")
            subscribed: dict[str, str] = {}  # venue symbol -> symbol
            last_frame = time.monotonic()
            last_ping = time.monotonic()
            empty_since: float | None = None
            while self._is_running:
                # Sync subscriptions with watchers.
                want = {self.venue_symbol(s): s for s in self._wanted}
                drop = [v for v in subscribed if v not in want]
                add = [v for v in want if v not in subscribed]
                for msg in self.subscription(drop, False):
                    await ws.send(msg)
                for msg in self.subscription(add, True):
                    await ws.send(msg)
                subscribed = want
                self._changed.clear()
                if not subscribed:
                    empty_since = empty_since or time.monotonic()
                    if time.monotonic() - empty_since > LINGER_SECONDS:
                        logger.info(f"{self.exchange_name} price stream closed (nothing to watch)")
                        return
                else:
                    empty_since = None

                recv = asyncio.ensure_future(ws.recv())
                changed = asyncio.ensure_future(self._changed.wait())
                done, _ = await asyncio.wait({recv, changed}, timeout=5, return_when=asyncio.FIRST_COMPLETED)
                if recv in done:
                    changed.cancel()
                    frame = recv.result()
                    last_frame = time.monotonic()
                    if isinstance(frame, bytes):
                        frame = frame.decode("utf-8", "ignore")
                    now = datetime.utcnow()
                    for venue_sym, price in self.parse(frame):
                        symbol = subscribed.get(venue_sym)
                        if symbol:
                            yield PriceUpdate(symbol=symbol, exchange=self.exchange_name, price=price, timestamp=now)
                else:
                    recv.cancel()
                    changed.cancel()
                    try:
                        await recv
                    except (asyncio.CancelledError, Exception):
                        pass
                if time.monotonic() - last_frame > IDLE_TIMEOUT_SECONDS:
                    raise RuntimeError("no data for 60 s")
                if time.monotonic() - last_ping > PING_EVERY_SECONDS:
                    msg = self.ping_message()
                    if msg:
                        await ws.send(msg)
                    last_ping = time.monotonic()


class BybitFuturesExchange(OnDemandExchange):
    def __init__(self):
        super().__init__("bybit_futures")

    async def ws_url(self) -> str:
        return "wss://stream.bybit.com/v5/public/linear"

    def subscription(self, venue_symbols, subscribe):
        op = "subscribe" if subscribe else "unsubscribe"
        return [
            json.dumps({"op": op, "args": [f"tickers.{s}" for s in venue_symbols[i : i + 10]]})
            for i in range(0, len(venue_symbols), 10)
        ]

    def parse(self, text):
        try:
            v = json.loads(text)
        except json.JSONDecodeError:
            return []
        if not str(v.get("topic", "")).startswith("tickers."):
            return []
        d = v.get("data") or {}
        price = _num(d.get("lastPrice"))
        sym = d.get("symbol")
        return [(sym.upper(), price)] if sym and price else []

    def ping_message(self):
        return json.dumps({"op": "ping"})


class KucoinFuturesExchange(OnDemandExchange):
    BULLET_URL = "https://api-futures.kucoin.com/api/v1/bullet-public"

    def __init__(self):
        super().__init__("kucoin_futures")
        self._msg_id = 0

    async def ws_url(self) -> str:
        async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=10)) as s:
            async with s.post(self.BULLET_URL) as resp:
                resp.raise_for_status()
                body = await resp.json()
        data = body["data"]
        endpoint = data["instanceServers"][0]["endpoint"]
        return f"{endpoint}?token={data['token']}&connectId=pd{int(time.time() * 1000)}"

    async def after_connect(self, ws) -> None:
        # KuCoin drops subscriptions sent before its welcome message.
        async def welcome():
            while True:
                frame = await ws.recv()
                if '"type":"welcome"' in str(frame):
                    return
        await asyncio.wait_for(welcome(), timeout=10)

    def venue_symbol(self, symbol):
        base = _usdt(symbol)[:-4]
        return f"{'XBT' if base == 'BTC' else base}USDTM"

    def subscription(self, venue_symbols, subscribe):
        out = []
        for i in range(0, len(venue_symbols), 100):
            self._msg_id += 1
            out.append(
                json.dumps(
                    {
                        "id": str(self._msg_id),
                        "type": "subscribe" if subscribe else "unsubscribe",
                        "topic": "/contractMarket/execution:" + ",".join(venue_symbols[i : i + 100]),
                        "privateChannel": False,
                        "response": True,
                    }
                )
            )
        return out

    def parse(self, text):
        try:
            v = json.loads(text)
        except json.JSONDecodeError:
            return []
        if v.get("subject") != "match":
            return []
        d = v.get("data") or {}
        price = _num(d.get("price"))
        sym = d.get("symbol")
        return [(sym.upper(), price)] if sym and price else []

    def ping_message(self):
        self._msg_id += 1
        return json.dumps({"id": str(self._msg_id), "type": "ping"})


class AsterFuturesExchange(OnDemandExchange):
    def __init__(self):
        super().__init__("aster_futures")
        self._msg_id = 0

    async def ws_url(self) -> str:
        return "wss://fstream.asterdex.com/ws"

    def subscription(self, venue_symbols, subscribe):
        if not venue_symbols:
            return []
        self._msg_id += 1
        return [
            json.dumps(
                {
                    "method": "SUBSCRIBE" if subscribe else "UNSUBSCRIBE",
                    "params": [f"{s.lower()}@aggTrade" for s in venue_symbols],
                    "id": self._msg_id,
                }
            )
        ]

    def parse(self, text):
        try:
            v = json.loads(text)
        except json.JSONDecodeError:
            return []
        if not isinstance(v, dict) or v.get("e") != "aggTrade":
            return []
        price = _num(v.get("p"))
        sym = v.get("s")
        return [(sym.upper(), price)] if sym and price else []


class HyperliquidExchange(OnDemandExchange):
    """Symbols are `<COIN>USDT` here so they match the other exchanges."""

    def __init__(self):
        super().__init__("hyperliquid")

    async def ws_url(self) -> str:
        return "wss://api.hyperliquid.xyz/ws"

    def venue_symbol(self, symbol):
        return _usdt(symbol)[:-4]

    def subscription(self, venue_symbols, subscribe):
        method = "subscribe" if subscribe else "unsubscribe"
        return [
            json.dumps({"method": method, "subscription": {"type": "trades", "coin": c}})
            for c in venue_symbols
        ]

    def parse(self, text):
        try:
            v = json.loads(text)
        except json.JSONDecodeError:
            return []
        if not isinstance(v, dict) or v.get("channel") != "trades":
            return []
        out = []
        for t in v.get("data") or []:
            price = _num(t.get("px"))
            coin = t.get("coin")
            if coin and price:
                out.append((coin.upper(), price))
        return out

    def ping_message(self):
        return json.dumps({"method": "ping"})
