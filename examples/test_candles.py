#!/usr/bin/env python3
"""
Test candles REST API.

Usage:
    # Start the app first: python run.py
    python examples/test_candles.py
"""

import asyncio
import aiohttp


async def main():
    base = "http://localhost:8000"

    async with aiohttp.ClientSession() as session:
        # Get candles for TREEUSDT
        print("Fetching 1m candles for TREEUSDT...")
        async with session.get(
            f"{base}/candles?symbol=TREEUSDT&exchange=binance_futures&interval=5m&limit=10"
        ) as r:
            if r.status == 200:
                data = await r.json()
                print(f"  Got {len(data['candles'])} candles")
                for c in data["candles"][:3]:
                    print(f"    {c['timestamp']}: O={c['open']} H={c['high']} L={c['low']} C={c['close']}")
            else:
                print(f"  Error: {r.status}")

        # Get latest price
        print("\nFetching latest price for TREEUSDT...")
        async with session.get(f"{base}/price/TREEUSDT?exchange=binance_futures") as r:
            if r.status == 200:
                data = await r.json()
                print(f"  Price: {data['price']} at {data['timestamp']}")
            else:
                print(f"  Error: {r.status} (may need to wait for data)")


if __name__ == "__main__":
    asyncio.run(main())
