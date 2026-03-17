# Quick Start Guide

Get the Crypto Pump Detector up and running in 5 minutes!

## Prerequisites

- Python 3.8 or higher
- MongoDB (or Docker to run MongoDB)
- 2GB RAM minimum
- Internet connection for WebSocket streams

## Step 1: Install Dependencies

```bash
# Clone or navigate to the project directory
cd pump-detector

# Create virtual environment (recommended)
python3 -m venv venv
source venv/bin/activate  # On Windows: venv\Scripts\activate

# Install dependencies
pip install -r requirements.txt
```

## Step 2: Start MongoDB

### Option A: Using Docker (Recommended)

```bash
docker run -d -p 27017:27017 --name mongodb mongo:latest
```

### Option B: Using docker-compose

```bash
docker-compose up -d mongodb
```

### Option C: Local MongoDB Installation

**Linux:**
```bash
sudo systemctl start mongodb
```

**macOS:**
```bash
brew services start mongodb-community
```

**Windows:**
```powershell
net start MongoDB
```

## Step 3: Configure Environment

```bash
# Copy example config
cp .env.example .env

# Edit .env (optional - defaults work for local development)
nano .env  # or your favorite editor
```

**Minimal .env for testing:**
```env
MONGODB_URL=mongodb://localhost:27017
MONGODB_DB_NAME=crypto_pump_detector
WEBHOOK_URL=http://localhost:9000/alert  # Optional
```

## Step 4: Run the Application

### Option A: Using the Python Runner (Recommended)

```bash
python run.py
```

This will:
- Check if all dependencies are installed
- Verify MongoDB connection
- Start the application

### Option B: Using the Shell Script

```bash
chmod +x run.sh
./run.sh
```

### Option C: Direct Python

```bash
python -m app.main
```

## Step 5: Verify It's Working

### Check Application Status

```bash
# In a new terminal
curl http://localhost:8000/health
```

Expected response:
```json
{
  "status": "healthy",
  "active_exchanges": ["binance_futures"],
  "total_exchanges": 1
}
```

### View Live Statistics

```bash
# Terminal dashboard
python examples/monitor_stats.py
```

You should see:
- System health status
- Price updates being processed
- Ticker ownership claims
- Real-time statistics

### Check the API Documentation

Open your browser to: http://localhost:8000/docs

You'll see the interactive Swagger UI with all available endpoints.

## Step 6: Set Up Webhook Receiver (Optional)

To receive price alerts, run the example webhook receiver:

```bash
# In a new terminal
python examples/simple_webhook_receiver.py
```

This starts a webhook receiver on port 9000. When price spikes or drops are detected, you'll see alerts in this terminal.

## What's Happening?

Once started, the application is:

1. **Connecting** to Binance Futures WebSocket
2. **Receiving** price updates for all trading pairs (~500+ symbols)
3. **Claiming** ticker ownership (first-come-first-serve)
4. **Storing** prices in MongoDB (automatically expires after 12 hours)
5. **Detecting** price spikes/drops every second
6. **Sending** webhook notifications when alerts trigger

## Testing Spike Detection

To test the spike detection without waiting for real market movements:

1. **Lower the detection threshold:**
```env
# In .env
SPIKE_THRESHOLD_PERCENT=0.5  # Trigger on 0.5% change
DETECTION_WINDOW_MINUTES=1   # Check last 1 minute
```

2. **Restart the application:**
```bash
python run.py
```

3. **Wait for alerts:**
Watch the webhook receiver terminal for alerts (usually within 1-2 minutes)

## Common Commands

### View All Tickers Owned by Binance Futures
```bash
curl http://localhost:8000/tickers/binance_futures
```

### Get Price History for Bitcoin
```bash
curl "http://localhost:8000/price/BTCUSDT?exchange=binance_futures&minutes=60"
```

### View Statistics
```bash
curl http://localhost:8000/stats
```

### Test Webhook Configuration
```bash
curl -X POST http://localhost:8000/webhook/test
```

## Monitoring

### Real-time Dashboard
```bash
python examples/monitor_stats.py
```

### View Application Logs

The application outputs logs to stdout. To save logs:

```bash
python -m app.main 2>&1 | tee app.log
```

### Log Levels

Control verbosity in `.env`:

```env
LOG_LEVEL=INFO   # Default: INFO, DEBUG, WARNING, ERROR
```

## Stopping the Application

Press `Ctrl+C` in the terminal running the application.

The application will:
- Gracefully close all WebSocket connections
- Flush remaining batched updates to database
- Close database connection
- Clean up resources

## Troubleshooting

### "MongoDB connection failed"

**Solution:** Make sure MongoDB is running:
```bash
docker ps | grep mongo
# or
systemctl status mongodb
```

### "No price updates received"

**Possible causes:**
1. Check internet connection
2. Binance API might be down (check https://www.binance.com/en/support/announcement)
3. Check logs for WebSocket errors

### "ModuleNotFoundError"

**Solution:** Install dependencies:
```bash
pip install -r requirements.txt
```

### Application starts but no tickers claimed

**Cause:** Another instance might be running

**Solution:** 
1. Stop other instances
2. Clear ownership collection:
```bash
docker exec -it mongodb mongosh
use crypto_pump_detector
db.ticker_ownership.deleteMany({})
exit
```

## Next Steps

Now that you have the basic system running:

1. **Add more exchanges** - See [ADDING_EXCHANGES.md](ADDING_EXCHANGES.md)
2. **Customize spike detection** - Adjust thresholds in `.env`
3. **Integrate webhooks** - Connect to Telegram, Discord, email, etc.
4. **Monitor performance** - Use the stats dashboard
5. **Scale up** - See [ARCHITECTURE.md](ARCHITECTURE.md) for scaling tips

## Production Deployment

For production use:

1. **Use environment variables** instead of `.env` file
2. **Enable authentication** on MongoDB
3. **Add API authentication** to FastAPI endpoints
4. **Use process manager** (systemd, supervisor, PM2)
5. **Set up monitoring** (Prometheus, Grafana)
6. **Configure alerting** (PagerDuty, OpsGenie)
7. **Use docker-compose** for easier deployment

Example production start:
```bash
# Using docker-compose
docker-compose up -d

# Or with systemd
sudo systemctl enable crypto-pump-detector
sudo systemctl start crypto-pump-detector
```

## Support

- **Documentation:** Check README.md and ARCHITECTURE.md
- **Examples:** See the `examples/` directory
- **Issues:** Open an issue on GitHub

Happy monitoring! 🚀
