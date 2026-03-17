#!/usr/bin/env python3
"""
Real-time statistics monitor.

This script continuously displays statistics from the running
Crypto Pump Detector application.

Usage:
    python examples/monitor_stats.py
"""

import asyncio
import aiohttp
import os
from datetime import datetime


class StatsMonitor:
    """Monitor and display real-time statistics."""
    
    def __init__(self, api_url: str = "http://localhost:8000"):
        self.api_url = api_url
        self.running = True
    
    async def fetch_stats(self) -> dict:
        """Fetch statistics from the API."""
        async with aiohttp.ClientSession() as session:
            async with session.get(f"{self.api_url}/stats") as response:
                if response.status == 200:
                    return await response.json()
                else:
                    return {}
    
    async def fetch_health(self) -> dict:
        """Fetch health status from the API."""
        async with aiohttp.ClientSession() as session:
            async with session.get(f"{self.api_url}/health") as response:
                if response.status == 200:
                    return await response.json()
                else:
                    return {}
    
    def clear_screen(self):
        """Clear the terminal screen."""
        os.system('clear' if os.name != 'nt' else 'cls')
    
    def format_number(self, num: int) -> str:
        """Format number with thousand separators."""
        return f"{num:,}"
    
    def display_stats(self, stats: dict, health: dict):
        """Display statistics in a formatted way."""
        self.clear_screen()
        
        print("\n" + "=" * 70)
        print(f"  CRYPTO PUMP DETECTOR - LIVE STATS")
        print(f"  {datetime.now().strftime('%Y-%m-%d %H:%M:%S')}")
        print("=" * 70)
        
        # Health Status
        print("\n📊 SYSTEM HEALTH")
        print("-" * 70)
        status = health.get("status", "unknown")
        status_emoji = "✅" if status == "healthy" else "⚠️"
        print(f"  Status: {status_emoji} {status.upper()}")
        print(f"  Active Exchanges: {len(health.get('active_exchanges', []))}")
        for exchange in health.get('active_exchanges', []):
            print(f"    • {exchange}")
        
        # Monitoring Stats
        monitoring = stats.get("monitoring", {})
        print("\n📈 MONITORING STATISTICS")
        print("-" * 70)
        print(f"  Total Updates:     {self.format_number(monitoring.get('total_updates', 0))}")
        print(f"  Stored Updates:    {self.format_number(monitoring.get('stored_updates', 0))}")
        print(f"  Rejected Updates:  {self.format_number(monitoring.get('rejected_updates', 0))}")
        print(f"  Ownership Claims:  {self.format_number(monitoring.get('ownership_claims', 0))}")
        print(f"  Batch Size:        {monitoring.get('batch_size', 0)}")
        print(f"  Cached Tickers:    {self.format_number(monitoring.get('cached_tickers', 0))}")
        
        # Calculate acceptance rate
        total = monitoring.get('total_updates', 0)
        if total > 0:
            acceptance_rate = (monitoring.get('stored_updates', 0) / total) * 100
            print(f"  Acceptance Rate:   {acceptance_rate:.1f}%")
        
        # Ownership by Exchange
        ownership = stats.get("ownership", {})
        if ownership:
            print("\n🏢 TICKER OWNERSHIP")
            print("-" * 70)
            for exchange, count in ownership.items():
                print(f"  {exchange}: {self.format_number(count)} tickers")
        
        print("\n" + "=" * 70)
        print("  Press Ctrl+C to exit")
        print("=" * 70 + "\n")
    
    async def run(self, refresh_interval: int = 2):
        """Run the stats monitor with periodic updates."""
        print("Starting stats monitor...")
        print(f"Connecting to {self.api_url}")
        
        try:
            while self.running:
                try:
                    stats = await self.fetch_stats()
                    health = await self.fetch_health()
                    
                    if stats or health:
                        self.display_stats(stats, health)
                    else:
                        print(f"Unable to connect to {self.api_url}")
                        print("Make sure the Crypto Pump Detector is running.")
                    
                    await asyncio.sleep(refresh_interval)
                
                except aiohttp.ClientError as e:
                    print(f"\nConnection error: {e}")
                    print("Retrying in 5 seconds...")
                    await asyncio.sleep(5)
        
        except KeyboardInterrupt:
            print("\n\nStopping stats monitor...")
            self.running = False


async def main():
    """Main entry point."""
    import sys
    
    # Parse command line arguments
    api_url = sys.argv[1] if len(sys.argv) > 1 else "http://localhost:8000"
    refresh_interval = int(sys.argv[2]) if len(sys.argv) > 2 else 2
    
    monitor = StatsMonitor(api_url=api_url)
    await monitor.run(refresh_interval=refresh_interval)


if __name__ == "__main__":
    asyncio.run(main())
