import json

from app.exchanges.binance_futures import BinanceFuturesExchange
from app.exchanges.on_demand import (
    AsterFuturesExchange,
    BybitFuturesExchange,
    HyperliquidExchange,
    KucoinFuturesExchange,
)

# Frames are trimmed copies of what each venue sent on 2026-10-12.


def test_binance_mini_ticker_array():
    ex = BinanceFuturesExchange()
    frame = json.dumps(
        [
            {"e": "24hrMiniTicker", "E": 1791807529762, "s": "CHIPUSDT", "c": "0.0635700", "q": "9512345.6"},
            {"e": "24hrMiniTicker", "E": 1791807529762, "s": "BADUSDT", "c": "0"},
            {"e": "markPriceUpdate", "s": "BTCUSDT", "p": "1"},
        ]
    )
    updates = ex.parse_message(frame)
    assert [(u.symbol, u.price, u.quote_volume_24h) for u in updates] == [("CHIPUSDT", 0.06357, 9512345.6)]
    assert ex.parse_message('{"result":null,"id":1}') == []
    assert "/market/ws/" in ex.WS_URL


def test_bybit():
    ex = BybitFuturesExchange()
    assert ex.parse('{"topic":"tickers.BTCUSDT","type":"snapshot","data":{"symbol":"BTCUSDT","lastPrice":"83390.50"}}') == [("BTCUSDT", 83390.5)]
    assert ex.parse('{"topic":"tickers.BTCUSDT","type":"delta","data":{"symbol":"BTCUSDT","bid1Price":"1"}}') == []
    subs = ex.subscription([f"T{i}USDT" for i in range(11)], True)
    assert len(subs) == 2 and json.loads(subs[0])["args"][0] == "tickers.T0USDT"


def test_kucoin():
    ex = KucoinFuturesExchange()
    assert ex.venue_symbol("BTCUSDT") == "XBTUSDTM"
    assert ex.venue_symbol("zest") == "ZESTUSDTM"
    sub = json.loads(ex.subscription(["XBTUSDTM", "ZESTUSDTM"], True)[0])
    assert sub["topic"] == "/contractMarket/execution:XBTUSDTM,ZESTUSDTM"
    frame = '{"topic":"/contractMarket/execution:XBTUSDTM","type":"message","subject":"match","data":{"symbol":"XBTUSDTM","price":"83394.3"}}'
    assert ex.parse(frame) == [("XBTUSDTM", 83394.3)]
    assert ex.parse('{"id":"1","type":"ack"}') == []


def test_aster():
    ex = AsterFuturesExchange()
    sub = json.loads(ex.subscription(["ZESTUSDT"], True)[0])
    assert sub["method"] == "SUBSCRIBE" and sub["params"] == ["zestusdt@aggTrade"]
    assert ex.parse('{"e":"aggTrade","s":"BTCUSDT","p":"83398.8"}') == [("BTCUSDT", 83398.8)]
    assert ex.subscription([], True) == []


def test_hyperliquid():
    ex = HyperliquidExchange()
    assert ex.venue_symbol("ZESTUSDT") == "ZEST"
    assert json.loads(ex.subscription(["ZEST"], False)[0]) == {"method": "unsubscribe", "subscription": {"type": "trades", "coin": "ZEST"}}
    frame = '{"channel":"trades","data":[{"coin":"BTC","px":"83410.0"},{"coin":"BTC","px":"83406.0"}]}'
    assert ex.parse(frame) == [("BTC", 83410.0), ("BTC", 83406.0)]


def test_watch_counts():
    ex = BybitFuturesExchange()
    ex.watch("zest")
    ex.watch("ZESTUSDT")
    ex.unwatch("ZESTUSDT")
    assert ex.watched == {"ZESTUSDT"}
    ex.unwatch("ZESTUSDT")
    assert ex.watched == set()
