# Examples

This directory contains example scripts demonstrating various use cases of the Crypto Pump Detector system.

## Scripts

### 1. Simple Webhook Receiver (`simple_webhook_receiver.py`)

A basic webhook receiver that demonstrates how to process price alerts from the Crypto Pump Detector.

**Usage:**
```bash
python examples/simple_webhook_receiver.py
```

Then configure the main app to send webhooks to this receiver:
```bash
# In .env
WEBHOOK_URL=http://localhost:9000/alert
```

**Customize it for:**
- Sending Telegram notifications
- Storing alerts in a database
- Triggering trading bots
- Sending email/SMS notifications

### 2. Real-time Stats Monitor (`monitor_stats.py`)

A terminal-based dashboard that displays live statistics from the running application.

**Usage:**
```bash
# Monitor local instance (default)
python examples/monitor_stats.py

# Monitor remote instance
python examples/monitor_stats.py http://remote-host:8000

# Custom refresh interval (default: 2 seconds)
python examples/monitor_stats.py http://localhost:8000 5
```

**Features:**
- Real-time system health status
- Monitoring statistics (updates, ownership, cache)
- Ticker counts by exchange
- Acceptance rate calculation
- Auto-refresh display

## Creating Your Own Examples

Feel free to create custom scripts for your specific use cases:

1. **Price Alerts to Telegram Bot**
```python
import aiohttp
from telegram import Bot

async def send_to_telegram(alert):
    bot = Bot(token="YOUR_BOT_TOKEN")
    message = f"🚨 {alert['alert_type'].upper()} Alert!\n"
    message += f"{alert['symbol']}: {alert['change_percent']:+.2f}%"
    await bot.send_message(chat_id="YOUR_CHAT_ID", text=message)
```

2. **Store Alerts in PostgreSQL**
```python
import asyncpg

async def store_alert(alert):
    conn = await asyncpg.connect('postgresql://...')
    await conn.execute('''
        INSERT INTO alerts (symbol, exchange, alert_type, change_percent, timestamp)
        VALUES ($1, $2, $3, $4, $5)
    ''', alert['symbol'], alert['exchange'], alert['alert_type'], 
       alert['change_percent'], alert['timestamp'])
    await conn.close()
```

3. **Trading Bot Integration**
```python
async def handle_spike_alert(alert):
    if alert['alert_type'] == 'spike' and alert['change_percent'] > 10:
        # Execute your trading logic
        await place_order(
            symbol=alert['symbol'],
            side='BUY',
            quantity=calculate_quantity(alert['change_percent'])
        )
```

## Need Help?

Check the main README.md for more information or open an issue on GitHub.
