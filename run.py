#!/usr/bin/env python3
"""
Simple Python runner for the Crypto Pump Detector.

This script provides a cross-platform way to run the application
without needing shell scripts.
"""

import sys
import os
import subprocess


def check_dependencies():
    """Check if required dependencies are installed."""
    try:
        import fastapi
        import motor
        import websockets
        print("✓ All dependencies installed")
        return True
    except ImportError as e:
        print(f"✗ Missing dependencies: {e}")
        print("\nPlease install dependencies:")
        print("  pip install -r requirements.txt")
        return False


def check_env_file():
    """Check if .env file exists."""
    if not os.path.exists(".env"):
        print("✗ .env file not found")
        if os.path.exists(".env.example"):
            print("\nCopying .env.example to .env...")
            with open(".env.example", "r") as src:
                with open(".env", "w") as dst:
                    dst.write(src.read())
            print("✓ Created .env file")
            print("\nPlease configure .env with your settings before running again.")
            return False
        else:
            print("Please create a .env file with your configuration.")
            return False
    return True


def check_mongodb():
    """Check if MongoDB is accessible."""
    try:
        from pymongo import MongoClient
        from dotenv import load_dotenv
        
        load_dotenv()
        mongodb_url = os.getenv("MONGODB_URL", "mongodb://localhost:27017")
        
        client = MongoClient(mongodb_url, serverSelectionTimeoutMS=2000)
        client.server_info()
        print("✓ MongoDB is running")
        return True
    except Exception as e:
        print(f"✗ MongoDB connection failed: {e}")
        print("\nPlease ensure MongoDB is running:")
        print("  Docker: docker run -d -p 27017:27017 --name mongodb mongo:latest")
        print("  Local:  systemctl start mongodb  (Linux)")
        print("          brew services start mongodb-community  (macOS)")
        return False


def run_application():
    """Run the FastAPI application."""
    print("\nStarting Crypto Pump Detector...")
    print("=" * 50)
    
    try:
        import uvicorn
        from app.config import settings
        
        uvicorn.run(
            "app.main:app",
            host="0.0.0.0",
            port=8000,
            reload=False,
            log_level=settings.log_level.lower()
        )
    except KeyboardInterrupt:
        print("\n\nApplication stopped by user")
    except Exception as e:
        print(f"\n✗ Error running application: {e}")
        sys.exit(1)


def main():
    """Main entry point."""
    print("Crypto Pump Detector - Startup Checks")
    print("=" * 50)
    
    # Run all checks
    checks = [
        ("Environment file", check_env_file),
        ("Dependencies", check_dependencies),
        ("MongoDB", check_mongodb),
    ]
    
    for check_name, check_func in checks:
        print(f"\nChecking {check_name}...")
        if not check_func():
            print(f"\n✗ {check_name} check failed. Please fix the issue and try again.")
            sys.exit(1)
    
    print("\n✓ All checks passed!")
    
    # Run the application
    run_application()


if __name__ == "__main__":
    main()
