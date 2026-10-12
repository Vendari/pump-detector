import pytest
from fastapi.testclient import TestClient
from starlette.websockets import WebSocketDisconnect

from app.config import settings
from app.exchanges.on_demand import BybitFuturesExchange
from app.main import app
from app.models.price import PriceUpdate
from app.services.price_broadcaster import price_broadcaster
from app.services.price_monitor import price_monitor


@pytest.fixture
def client(monkeypatch):
    monkeypatch.setattr(settings, "api_key", "k1")
    bybit = BybitFuturesExchange()
    monkeypatch.setattr(price_monitor, "exchanges", [bybit])
    # No lifespan: no Mongo, no exchange connections.
    return TestClient(app), bybit


def test_http_needs_key_except_health(client):
    c, _ = client
    assert c.get("/health").status_code == 200
    assert c.get("/stats").status_code == 401
    assert c.get("/stats", headers={"x-api-key": "k1"}).status_code == 200


def test_ws_rejects_without_key(client):
    c, _ = client
    with pytest.raises(WebSocketDisconnect):
        with c.websocket_connect("/ws/price") as ws:
            ws.receive_json()


def test_ws_multi_subscribe_latest_price_and_watch(client):
    c, bybit = client
    price_broadcaster._latest_prices[("ZESTUSDT", "bybit_futures")] = PriceUpdate(
        symbol="ZESTUSDT", exchange="bybit_futures", price=0.19
    )
    with c.websocket_connect("/ws/price?key=k1") as ws:
        ws.send_json({"action": "subscribe", "symbol": "zestusdt", "exchange": "bybit_futures"})
        assert ws.receive_json()["status"] == "subscribed"
        assert ws.receive_json()["price"] == 0.19
        assert bybit.watched == {"ZESTUSDT"}
        ws.send_json({"action": "subscribe", "symbol": "BTCUSDT", "exchange": "bybit_futures"})
        assert ws.receive_json()["symbol"] == "BTCUSDT"
        assert bybit.watched == {"ZESTUSDT", "BTCUSDT"}
        ws.send_json({"action": "subscribe", "symbol": "BTCUSDT", "exchange": "nope"})
        assert ws.receive_json()["error"] == "unknown exchange"
        ws.send_json({"action": "unsubscribe", "symbol": "BTCUSDT", "exchange": "bybit_futures"})
        assert ws.receive_json()["status"] == "unsubscribed"
        assert bybit.watched == {"ZESTUSDT"}
    # Disconnect releases everything.
    import time

    for _ in range(50):
        if not bybit.watched:
            break
        time.sleep(0.02)
    assert bybit.watched == set()
