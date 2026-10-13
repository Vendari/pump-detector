import asyncio
from datetime import datetime, timedelta, timezone

from app.models.candle import CandleInterval
from app.models.price import PriceUpdate
from app.services import candle_service as cs


class _FakeMongo:
    async def upsert_candle(self, candle):
        pass

    async def trim_candles(self, *args):
        return 0


def test_gaps_do_not_pile_up_in_aggregation_buffers(monkeypatch):
    monkeypatch.setattr(cs, "mongodb_service", _FakeMongo())
    service = cs.CandleService()
    start = datetime(2026, 10, 13, 0, 0, 0, tzinfo=timezone.utc)

    async def run():
        # A quiet market: one price every 20 s, so every 30 s bucket misses
        # 10 s candles and never completes. Two hours of that.
        for i in range(360):
            await service.add_price_update(
                PriceUpdate(
                    symbol="QUIETUSDT",
                    exchange="binance_futures",
                    price=1.0 + i * 1e-4,
                    timestamp=start + timedelta(seconds=20 * i),
                )
            )

    asyncio.run(run())
    for key, buf in service._candle_buffers.items():
        assert len(buf) <= 1, (key, len(buf))
    assert (
        "QUIETUSDT",
        "binance_futures",
        CandleInterval.S30,
    ) in service._candle_buffers
