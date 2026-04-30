"""
Spike detector - tiered detection + custom alert configs.

Built-in (intervals 5m, 15m, 30m, 1h, 6h, 12h, 24h):
- >= 5%: save to DB only
- >= 10%: save to DB + send webhook (if WEBHOOK_URL set)

Custom alerts from API (POST /alerts) use their own thresholds and webhooks.
"""

import logging
from typing import List

from app.config import settings
from app.database.mongodb import mongodb_service
from app.models.candle import Candle
from app.models.price import PriceUpdate, PriceAlert

logger = logging.getLogger(__name__)

# Built-in detection intervals (24h = 1D in candle storage)
BUILTIN_INTERVALS = frozenset(("5m", "15m", "30m", "1h", "6h", "12h", "24h"))

# Tiers: (threshold_pct, send_webhook)
SPIKE_TIER_5 = 5.0   # save only
SPIKE_TIER_10 = 10.0  # save + webhook
DROP_TIER_5 = -5.0
DROP_TIER_10 = -10.0


class SpikeDetector:
    """
    Tiered spike/drop detection:
    - 5%: record to DB
    - 10%: record to DB + webhook
    Intervals: 5m, 15m, 30m, 1h, 6h, 12h, 24h (1D)
    """

    def __init__(self) -> None:
        """Initialize spike detector."""
        logger.info(
            "Spike detector: 5%% tier (DB only), 10%% tier (DB + webhook), "
            "intervals=%s",
            list(BUILTIN_INTERVALS),
        )

    async def check_custom_alerts(
        self,
        _price_update: PriceUpdate,
        completed_candles: List[Candle],
    ) -> List[PriceAlert]:
        """
        Check completed candles: built-in tiers + custom API configs.

        Returns list of PriceAlert (all saved to DB; webhook sent when tier=10 and URL set).
        """
        alerts: List[PriceAlert] = []

        for candle in completed_candles:
            change_pct = (
                (candle.close - candle.open) / candle.open * 100
                if candle.open > 0
                else 0
            )

            # Resolve 24h -> 1D for candle interval
            candle_interval = candle.interval
            if candle_interval == "1D":
                check_interval = "24h"
            else:
                check_interval = candle_interval

            if check_interval not in BUILTIN_INTERVALS:
                # Not a built-in interval - only check custom configs
                config_alerts = await self._check_custom_configs(
                    candle, change_pct
                )
                alerts.extend(config_alerts)
                continue

            # Built-in tiered detection
            builtin = self._check_builtin_tiers(candle, change_pct)
            alerts.extend(builtin)

            # Custom configs (API-registered)
            config_alerts = await self._check_custom_configs(candle, change_pct)
            alerts.extend(config_alerts)

        return alerts

    def _check_builtin_tiers(
        self, candle: Candle, change_pct: float
    ) -> List[PriceAlert]:
        """Check 5% and 10% tiers. 5% = DB only, 10% = DB + webhook."""
        result: List[PriceAlert] = []

        # Spike (positive)
        if change_pct >= SPIKE_TIER_10:
            result.append(
                PriceAlert(
                    symbol=candle.symbol,
                    exchange=candle.exchange,
                    alert_type="spike",
                    start_price=candle.open,
                    current_price=candle.close,
                    change_percent=change_pct,
                    interval=candle.interval,
                    webhook_url=settings.webhook_url,
                    alert_id="builtin_10",
                )
            )
        elif change_pct >= SPIKE_TIER_5:
            result.append(
                PriceAlert(
                    symbol=candle.symbol,
                    exchange=candle.exchange,
                    alert_type="spike",
                    start_price=candle.open,
                    current_price=candle.close,
                    change_percent=change_pct,
                    interval=candle.interval,
                    webhook_url=settings.webhook_url,
                    alert_id="builtin_5",
                )
            )

        # Drop (negative)
        if change_pct <= DROP_TIER_10:
            result.append(
                PriceAlert(
                    symbol=candle.symbol,
                    exchange=candle.exchange,
                    alert_type="drop",
                    start_price=candle.open,
                    current_price=candle.close,
                    change_percent=change_pct,
                    interval=candle.interval,
                    webhook_url=settings.webhook_url,
                    alert_id="builtin_10",
                )
            )
        elif change_pct <= DROP_TIER_5:
            result.append(
                PriceAlert(
                    symbol=candle.symbol,
                    exchange=candle.exchange,
                    alert_type="drop",
                    start_price=candle.open,
                    current_price=candle.close,
                    change_percent=change_pct,
                    interval=candle.interval,
                    webhook_url=settings.webhook_url,
                    alert_id="builtin_5",
                )
            )

        return result

    async def _check_custom_configs(
        self, candle: Candle, change_pct: float
    ) -> List[PriceAlert]:
        """Check API-registered custom alert configs."""
        alerts: List[PriceAlert] = []
        configs = await mongodb_service.get_alerts_for_exchange_symbol(
            exchange=candle.exchange,
            symbol=candle.symbol,
            interval=candle.interval,
        )

        for config in configs:
            if config.get("symbol") and config["symbol"] != candle.symbol:
                continue

            threshold = config.get("threshold_percent", 0)
            webhook_url = config.get("webhook_url")
            alert_id = config.get("alert_id")

            triggered = False
            alert_type = "spike"
            if threshold > 0 and change_pct >= threshold:
                triggered = True
                alert_type = "spike"
            elif threshold < 0 and change_pct <= threshold:
                triggered = True
                alert_type = "drop"

            if triggered and webhook_url:
                alerts.append(
                    PriceAlert(
                        symbol=candle.symbol,
                        exchange=candle.exchange,
                        alert_type=alert_type,
                        start_price=candle.open,
                        current_price=candle.close,
                        change_percent=change_pct,
                        interval=candle.interval,
                        webhook_url=webhook_url,
                        alert_id=alert_id,
                    )
                )

        return alerts
