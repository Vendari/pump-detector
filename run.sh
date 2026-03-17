#!/bin/bash

# Crypto Pump Detector - Run Script

# Check if Python is installed
if ! command -v python3 &> /dev/null; then
    echo "Python 3 is not installed. Please install Python 3.8 or higher."
    exit 1
fi

# Check if .env file exists
if [ ! -f .env ]; then
    echo ".env file not found. Creating from .env.example..."
    cp .env.example .env
    echo "Please configure .env file with your settings before running again."
    exit 1
fi

# Check if virtual environment exists
if [ ! -d "venv" ]; then
    echo "Creating virtual environment..."
    python3 -m venv venv
fi

# Activate virtual environment
source venv/bin/activate

# Install/upgrade dependencies
echo "Installing dependencies..."
pip install -r requirements.txt

# Check if MongoDB is running
echo "Checking MongoDB connection..."
python3 -c "
from pymongo import MongoClient
import os
from dotenv import load_dotenv

load_dotenv()
mongodb_url = os.getenv('MONGODB_URL', 'mongodb://localhost:27017')

try:
    client = MongoClient(mongodb_url, serverSelectionTimeoutMS=2000)
    client.server_info()
    print('✓ MongoDB is running')
except Exception as e:
    print('✗ MongoDB is not running or not accessible')
    print(f'  Error: {e}')
    print('  Please start MongoDB before running the application.')
    exit(1)
"

if [ $? -ne 0 ]; then
    exit 1
fi

# Run the application
echo ""
echo "Starting Crypto Pump Detector..."
echo "================================"
python3 -m app.main
