#!/usr/bin/env python3
"""
Test WebSocket live price subscription.

Usage:
    # Start the app first: python run.py
    python examples/test_websocket_price.py
"""

import asyncio
import json
import websockets


async def main():
    uri = "ws://localhost:8000/ws/price"

    async with websockets.connect(uri) as ws:
        # Subscribe to BTCUSDT
        await ws.send(json.dumps({
            "action": "subscribe",
            "symbol": "BTCUSDT",
            "exchange": "binance_futures",
        }))

        # Wait for confirmation
        msg = await ws.recv()
        print(f"Server: {msg}")

        # Receive 5 price updates
        print("Receiving live prices (Ctrl+C to stop)...")
        for _ in range(5):
            msg = await ws.recv()
            data = json.loads(msg)
            if "price" in data:
                print(f"  {data['symbol']}: ${data['price']:.2f} @ {data['timestamp']}")
            else:
                print(f"  {msg}")


if __name__ == "__main__":
    asyncio.run(main())
