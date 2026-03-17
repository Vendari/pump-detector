#!/usr/bin/env python3
"""
Simple webhook receiver example.

This script demonstrates how to receive and process price alerts
from the Crypto Pump Detector.

Run this on a separate port (e.g., 9000) and configure the main app
to send webhooks to http://localhost:9000/alert
"""

from fastapi import FastAPI, Request
from datetime import datetime
import uvicorn

app = FastAPI(title="Webhook Receiver Example")


@app.post("/alert")
async def receive_alert(request: Request):
    """
    Receive and process price alerts.
    
    This is where you would implement your custom logic:
    - Send Telegram notifications
    - Store alerts in a database
    - Trigger trading bots
    - Send email notifications
    - etc.
    """
    alert = await request.json()
    
    print("\n" + "=" * 70)
    print(f"🚨 ALERT RECEIVED at {datetime.now().strftime('%Y-%m-%d %H:%M:%S')}")
    print("=" * 70)
    print(f"Symbol:       {alert.get('symbol')}")
    print(f"Exchange:     {alert.get('exchange')}")
    print(f"Type:         {alert.get('alert_type').upper()}")
    print(f"Start Price:  ${alert.get('start_price'):,.2f}")
    print(f"Current:      ${alert.get('current_price'):,.2f}")
    print(f"Change:       {alert.get('change_percent'):+.2f}%")
    print(f"Window:       {alert.get('window_minutes')} minutes")
    print(f"Timestamp:    {alert.get('timestamp')}")
    print("=" * 70 + "\n")
    
    # Example: Send Telegram notification (pseudo-code)
    # await send_telegram_message(
    #     f"🚨 {alert['alert_type'].upper()} Alert!\n"
    #     f"{alert['symbol']} on {alert['exchange']}\n"
    #     f"Change: {alert['change_percent']:+.2f}%"
    # )
    
    return {"status": "received", "alert_id": alert.get('symbol')}


@app.get("/")
async def root():
    """Health check endpoint."""
    return {
        "service": "Webhook Receiver",
        "status": "running",
        "timestamp": datetime.now().isoformat()
    }


if __name__ == "__main__":
    print("\n" + "=" * 70)
    print("Starting Webhook Receiver on http://localhost:9000")
    print("Configure WEBHOOK_URL=http://localhost:9000/alert in .env")
    print("=" * 70 + "\n")
    
    uvicorn.run(
        app,
        host="0.0.0.0",
        port=9000,
        log_level="info"
    )
