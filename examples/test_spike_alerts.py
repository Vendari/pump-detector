#!/usr/bin/env python3
"""
Test spike alert registration and webhook receiver.

Usage:
    # Terminal 1: Start webhook receiver
    python examples/simple_webhook_receiver.py

    # Terminal 2: Start the app
    python run.py

    # Terminal 3: Run this script
    python examples/test_spike_alerts.py
"""

import asyncio
import aiohttp


async def main():
    base = "http://localhost:8000"
    webhook_url = "http://localhost:9000/alert"

    async with aiohttp.ClientSession() as session:
        # Create spike alert: 10% move in 10m on binance_futures
        print("Creating spike alert (10% in 10m, webhook to localhost:9000)...")
        async with session.post(
            f"{base}/alerts",
            json={
                "webhook_url": webhook_url,
                "exchange": "binance_futures",
                "interval": "10m",
                "threshold_percent": 10.0,
                "symbol": "BTCUSDT",
            },
        ) as r:
            if r.status == 200:
                data = await r.json()
                alert_id = data["alert_id"]
                print(f"  Created alert: {alert_id}")
            else:
                print(f"  Error: {r.status} {await r.text()}")
                return

        # Create drop alert: -5% in 5m
        print("\nCreating drop alert (-5% in 5m)...")
        async with session.post(
            f"{base}/alerts",
            json={
                "webhook_url": webhook_url,
                "exchange": "binance_futures",
                "interval": "5m",
                "threshold_percent": -5.0,
                "symbol": None,
            },
        ) as r:
            if r.status == 200:
                data = await r.json()
                print(f"  Created alert: {data['alert_id']}")
            else:
                print(f"  Error: {r.status}")

        # List alerts
        print("\nListing alerts...")
        async with session.get(f"{base}/alerts") as r:
            if r.status == 200:
                data = await r.json()
                print(f"  Total: {len(data['alerts'])} alerts")
                for a in data["alerts"]:
                    print(f"    - {a.get('alert_id', '')[:8]}... {a.get('exchange')} {a.get('interval')} {a.get('threshold_percent')}%")

        # Test webhook
        print("\nTesting webhook...")
        async with session.post(f"{base}/webhook/test?url={webhook_url}") as r:
            if r.status == 200:
                print("  Webhook test sent successfully")
            else:
                print(f"  Error: {r.status}")

        # Delete first alert
        print(f"\nDeleting alert {alert_id}...")
        async with session.delete(f"{base}/alerts/{alert_id}") as r:
            if r.status == 200:
                print("  Deleted")
            else:
                print(f"  Error: {r.status}")


if __name__ == "__main__":
    asyncio.run(main())
