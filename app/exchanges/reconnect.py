"""
Reconnect pacing for exchange connections. After MAX_FAILURES failed
attempts in a row the next attempt waits a full day, so a feed that keeps
failing (e.g. an IP ban or a changed endpoint) never hammers the exchange
into a rate limit. A connection that delivers any price resets the count.
"""

import logging

logger = logging.getLogger(__name__)

MAX_FAILURES = 10
RETRY_SECONDS = 5.0
LONG_PAUSE_SECONDS = 24 * 3600.0


class ReconnectPolicy:
    def __init__(self, name: str) -> None:
        self.name = name
        self.failures = 0

    def success(self) -> None:
        if self.failures:
            logger.info(f"{self.name}: connected again after {self.failures} failure(s)")
        self.failures = 0

    def failure(self) -> float:
        """Record a failed attempt; returns how long to wait before the next one."""
        self.failures += 1
        if self.failures >= MAX_FAILURES:
            logger.error(
                f"{self.name}: {self.failures} failed attempts in a row; next attempt in 24 h"
            )
            return LONG_PAUSE_SECONDS
        return RETRY_SECONDS
