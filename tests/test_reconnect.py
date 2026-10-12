import asyncio

import pytest

from app.exchanges import reconnect
from app.exchanges.on_demand import BybitFuturesExchange
from app.exchanges.reconnect import LONG_PAUSE_SECONDS, RETRY_SECONDS, ReconnectPolicy
from app.services.price_monitor import PriceMonitor


def test_day_pause_after_ten_failures_and_reset_on_success():
    p = ReconnectPolicy("x")
    assert [p.failure() for _ in range(9)] == [RETRY_SECONDS] * 9
    assert p.failure() == LONG_PAUSE_SECONDS
    # Still failing after the day: wait another day.
    assert p.failure() == LONG_PAUSE_SECONDS
    p.success()
    assert p.failure() == RETRY_SECONDS


class _AlwaysFails:
    exchange_name = "dead"
    is_running = False

    def __init__(self):
        self.attempts = 0

    async def reconnect(self):
        self.attempts += 1
        return False


@pytest.mark.asyncio
async def test_monitor_pauses_a_failing_exchange_for_a_day(monkeypatch):
    sleeps: list[float] = []
    ex = _AlwaysFails()
    monitor = PriceMonitor()

    async def fake_sleep(seconds):
        sleeps.append(seconds)
        if len(sleeps) >= 11:
            monitor._stopping = True

    monkeypatch.setattr(asyncio, "sleep", fake_sleep)
    await monitor._monitor_exchange(ex)
    assert ex.attempts == 11
    assert sleeps[:9] == [RETRY_SECONDS] * 9
    assert sleeps[9] == LONG_PAUSE_SECONDS


@pytest.mark.asyncio
async def test_on_demand_feed_pauses_for_a_day(monkeypatch):
    ex = BybitFuturesExchange()
    ex.watch("BTCUSDT")
    sleeps: list[float] = []

    async def broken_session():
        raise RuntimeError("refused")
        yield  # pragma: no cover

    async def fake_sleep(seconds):
        sleeps.append(seconds)
        if len(sleeps) >= 10:
            ex._is_running = False

    monkeypatch.setattr(ex, "_session", broken_session)
    monkeypatch.setattr(asyncio, "sleep", fake_sleep)
    async for _ in ex.subscribe_all_tickers():
        pass
    assert sleeps[:9] == [5.0] * 9
    assert sleeps[9] == reconnect.LONG_PAUSE_SECONDS
