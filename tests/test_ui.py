import base64
from datetime import datetime, timedelta

import pytest
from fastapi.testclient import TestClient

from app.config import settings
from app.database.mongodb import mongodb_service
from app.exchanges.on_demand import BybitFuturesExchange
from app.main import app
from app.models.price import PriceUpdate
from app.services.price_broadcaster import price_broadcaster
from app.services.price_monitor import price_monitor


def basic(user: str, password: str) -> dict:
    token = base64.b64encode(f"{user}:{password}".encode()).decode()
    return {"Authorization": f"Basic {token}"}


ADMIN = basic("admin", "admin")


@pytest.fixture
def client(monkeypatch):
    monkeypatch.setattr(settings, "api_key", "k1")
    monkeypatch.setattr(settings, "ui_username", "admin")
    monkeypatch.setattr(settings, "ui_password", "admin")
    monkeypatch.setattr(settings, "candle_exchanges", "binance_futures,gate_futures")
    monkeypatch.setattr(price_monitor, "exchanges", [BybitFuturesExchange()])
    monkeypatch.setattr(price_broadcaster, "_latest_prices", {})
    # No lifespan: no Mongo, no exchange connections.
    return TestClient(app)


@pytest.mark.parametrize("path", ["/ui", "/ui/api/prices", "/ui/api/exchanges"])
def test_ui_requires_basic_auth(client, path):
    r = client.get(path)
    assert r.status_code == 401
    assert r.headers["www-authenticate"] == 'Basic realm="pump-detector"'
    assert client.get(path, headers=basic("admin", "wrong")).status_code == 401
    assert client.get(path, headers=basic("root", "admin")).status_code == 401
    assert client.get(path, headers={"Authorization": "Basic !!!"}).status_code == 401
    assert client.get(path, headers=ADMIN).status_code == 200


def test_api_key_alone_does_not_open_ui(client):
    assert client.get("/ui", headers={"x-api-key": "k1"}).status_code == 401
    assert client.get("/ui/api/prices", headers={"x-api-key": "k1"}).status_code == 401


def test_ui_auth_does_not_open_other_endpoints(client):
    assert client.get("/stats", headers=ADMIN).status_code == 401
    assert client.get("/stats", headers={"x-api-key": "k1"}).status_code == 200
    assert client.get("/uix", headers=ADMIN).status_code == 401
    assert client.get("/health").status_code == 200


def test_ui_page_has_no_secrets(client, monkeypatch):
    r = client.get("/ui", headers=ADMIN)
    assert r.headers["content-type"].startswith("text/html")
    assert "lightweight-charts@4.2.0" in r.text
    assert "k1" not in r.text


def test_ui_credentials_configurable(client, monkeypatch):
    monkeypatch.setattr(settings, "ui_password", "s3cret")
    assert client.get("/ui", headers=ADMIN).status_code == 401
    assert client.get("/ui", headers=basic("admin", "s3cret")).status_code == 200


def test_ui_prices_lists_latest_prices(client):
    ts = datetime.utcnow() - timedelta(seconds=5)
    price_broadcaster._latest_prices[("BTCUSDT", "binance_futures")] = PriceUpdate(
        symbol="BTCUSDT", exchange="binance_futures", price=65000.5, timestamp=ts
    )
    price_broadcaster._latest_prices[("ZESTUSDT", "bybit_futures")] = PriceUpdate(
        symbol="ZESTUSDT", exchange="bybit_futures", price=0.19
    )
    rows = client.get("/ui/api/prices", headers=ADMIN).json()
    by_key = {(r["symbol"], r["exchange"]): r for r in rows}
    assert set(by_key) == {("BTCUSDT", "binance_futures"), ("ZESTUSDT", "bybit_futures")}
    btc = by_key[("BTCUSDT", "binance_futures")]
    assert btc["price"] == 65000.5
    assert 4 <= btc["age_seconds"] < 60
    assert set(btc) == {"symbol", "exchange", "price", "timestamp", "age_seconds"}


def test_ui_exchanges(client):
    data = client.get("/ui/api/exchanges", headers=ADMIN).json()
    assert data["exchanges"] == [{"name": "bybit_futures", "candles": False}]
    assert data["candle_exchanges"] == ["binance_futures", "gate_futures"]
    assert "1m" in data["intervals"]


def test_ui_candles_uses_mongodb(client, monkeypatch):
    calls = []

    async def fake_get_candles(symbol, exchange, interval, limit=100):
        calls.append((symbol, exchange, interval, limit))
        return [
            {
                "symbol": symbol, "exchange": exchange, "interval": interval,
                "open": 1.0, "high": 2.0, "low": 0.5, "close": 1.5, "volume": 10.0,
                "timestamp": datetime(2026, 1, 1, 0, 1),
            }
        ]

    monkeypatch.setattr(mongodb_service, "get_candles", fake_get_candles)
    r = client.get(
        "/ui/api/candles?symbol=btcusdt&exchange=binance_futures&interval=5m&limit=500",
        headers=ADMIN,
    )
    assert r.status_code == 200
    assert calls == [("BTCUSDT", "binance_futures", "5m", 100)]
    data = r.json()
    assert data["candles_stored"] is True
    assert data["candles"] == [
        {"time": 1767225660, "open": 1.0, "high": 2.0, "low": 0.5, "close": 1.5, "volume": 10.0}
    ]


def test_ui_candles_other_exchange_and_bad_interval(client, monkeypatch):
    async def boom(*a, **k):
        raise AssertionError("should not query Mongo")

    monkeypatch.setattr(mongodb_service, "get_candles", boom)
    r = client.get("/ui/api/candles?symbol=BTCUSDT&exchange=bybit_futures", headers=ADMIN)
    assert r.json()["candles_stored"] is False
    assert r.json()["candles"] == []
    r = client.get(
        "/ui/api/candles?symbol=BTCUSDT&exchange=binance_futures&interval=7m", headers=ADMIN
    )
    assert r.status_code == 400
    assert client.get("/ui/api/candles?symbol=BTCUSDT&exchange=binance_futures").status_code == 401
