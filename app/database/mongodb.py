"""MongoDB service for candles and spike alerts."""

import logging
from datetime import datetime
from typing import List, Optional

from motor.motor_asyncio import AsyncIOMotorClient, AsyncIOMotorDatabase, AsyncIOMotorCollection
from pymongo import IndexModel, ASCENDING, DESCENDING

from app.config import settings
from app.models.candle import Candle, INTERVAL_ORDER, MAX_CANDLES_PER_INTERVAL
from app.models.alert import SpikeAlertConfig
from app.models.price import PriceAlert

logger = logging.getLogger(__name__)


class MongoDBService:
    """
    MongoDB service for candle storage and spike alert configuration.

    Collections:
    - candles: OHLC data per symbol/exchange/interval (max 100 per type)
    - spike_alerts: Custom webhook alert configurations
    """

    def __init__(self) -> None:
        """Initialize MongoDB service."""
        self.client: Optional[AsyncIOMotorClient] = None
        self.db: Optional[AsyncIOMotorDatabase] = None
        self.candles_collection: Optional[AsyncIOMotorCollection] = None
        self.spike_alerts_collection: Optional[AsyncIOMotorCollection] = None
        self.detected_alerts_collection: Optional[AsyncIOMotorCollection] = None

    async def connect(self) -> None:
        """Establish connection to MongoDB and setup collections."""
        try:
            logger.info(f"Connecting to MongoDB at {settings.mongodb_url}")

            self.client = AsyncIOMotorClient(settings.mongodb_url)
            self.db = self.client[settings.mongodb_db_name]

            self.candles_collection = self.db["candles"]
            self.spike_alerts_collection = self.db["spike_alerts"]
            self.detected_alerts_collection = self.db["detected_alerts"]

            await self._setup_indexes()

            logger.info("Successfully connected to MongoDB")

        except Exception as e:
            logger.error(f"Failed to connect to MongoDB: {e}")
            raise

    async def disconnect(self) -> None:
        """Close MongoDB connection."""
        if self.client:
            self.client.close()
            logger.info("Disconnected from MongoDB")

    async def _setup_indexes(self) -> None:
        """Create indexes for optimal query performance."""
        try:
            candle_indexes = [
                IndexModel(
                    [
                        ("symbol", ASCENDING),
                        ("exchange", ASCENDING),
                        ("interval", ASCENDING),
                        ("timestamp", DESCENDING),
                    ],
                    name="symbol_exchange_interval_timestamp_idx",
                ),
                IndexModel(
                    [
                        ("symbol", ASCENDING),
                        ("exchange", ASCENDING),
                        ("interval", ASCENDING),
                    ],
                    name="symbol_exchange_interval_idx",
                ),
            ]
            await self.candles_collection.create_indexes(candle_indexes)

            alert_indexes = [
                IndexModel(
                    [
                        ("exchange", ASCENDING),
                        ("symbol", ASCENDING),
                        ("interval", ASCENDING),
                    ],
                    name="alert_exchange_symbol_interval_idx",
                ),
            ]
            await self.spike_alerts_collection.create_indexes(alert_indexes)

            detected_indexes = [
                IndexModel(
                    [("timestamp", DESCENDING)],
                    name="detected_timestamp_idx",
                ),
                IndexModel(
                    [
                        ("symbol", ASCENDING),
                        ("exchange", ASCENDING),
                        ("interval", ASCENDING),
                    ],
                    name="detected_symbol_exchange_interval_idx",
                ),
            ]
            await self.detected_alerts_collection.create_indexes(detected_indexes)

            logger.info("MongoDB indexes created successfully")

        except Exception as e:
            logger.error(f"Error creating indexes: {e}")
            raise

    async def upsert_candle(self, candle: Candle) -> bool:
        """
        Insert or update a candle. Uses composite key (symbol, exchange, interval, timestamp).
        """
        try:
            document = {
                "symbol": candle.symbol,
                "exchange": candle.exchange,
                "interval": candle.interval,
                "open": candle.open,
                "high": candle.high,
                "low": candle.low,
                "close": candle.close,
                "timestamp": candle.timestamp,
                "volume": candle.volume,
            }

            await self.candles_collection.update_one(
                {
                    "symbol": candle.symbol,
                    "exchange": candle.exchange,
                    "interval": candle.interval,
                    "timestamp": candle.timestamp,
                },
                {"$set": document},
                upsert=True,
            )
            return True

        except Exception as e:
            logger.error(f"Error upserting candle: {e}")
            return False

    async def trim_candles(
        self, symbol: str, exchange: str, interval: str, max_count: int
    ) -> int:
        """
        Remove oldest candles when count exceeds max_count.
        Returns number of documents deleted.
        """
        try:
            cursor = self.candles_collection.find(
                {"symbol": symbol, "exchange": exchange, "interval": interval},
                {"_id": 1, "timestamp": 1},
            ).sort("timestamp", ASCENDING)

            docs = await cursor.to_list(length=max_count + 1)
            if len(docs) <= max_count:
                return 0

            to_delete = len(docs) - max_count
            oldest_timestamps = [doc["timestamp"] for doc in docs[:to_delete]]

            result = await self.candles_collection.delete_many(
                {
                    "symbol": symbol,
                    "exchange": exchange,
                    "interval": interval,
                    "timestamp": {"$in": oldest_timestamps},
                }
            )
            return result.deleted_count

        except Exception as e:
            logger.error(f"Error trimming candles: {e}")
            return 0

    async def get_candles(
        self,
        symbol: str,
        exchange: str,
        interval: str,
        limit: int = 100,
    ) -> List[dict]:
        """
        Get candles for a symbol/exchange/interval, newest first.
        """
        try:
            cursor = (
                self.candles_collection.find(
                    {
                        "symbol": symbol,
                        "exchange": exchange,
                        "interval": interval,
                    },
                    {"_id": 0},
                )
                .sort("timestamp", DESCENDING)
                .limit(limit)
            )
            docs = await cursor.to_list(length=limit)
            return list(reversed(docs))  # Return oldest first for chronological order

        except Exception as e:
            logger.error(f"Error getting candles: {e}")
            return []

    async def get_candles_snapshot_by_symbol_exchange(
        self,
        symbol: str,
        exchange: str,
        max_per_interval: int = MAX_CANDLES_PER_INTERVAL,
    ) -> dict[str, list[dict]]:
        """
        Full snapshot of all candles we currently store for this symbol/exchange,
        keyed by interval (each list is oldest-first, capped at max_per_interval).

        Used when persisting detected alerts for downstream ML / LLM training.
        """
        try:
            intervals = await self.candles_collection.distinct(
                "interval",
                {"symbol": symbol, "exchange": exchange},
            )
            if not intervals:
                return {}

            order_map = {iv.value: idx for idx, iv in enumerate(INTERVAL_ORDER)}

            def _interval_sort_key(iv: str) -> tuple[int, str]:
                return (order_map.get(iv, len(INTERVAL_ORDER)), iv)

            out: dict[str, list[dict]] = {}
            for interval in sorted(intervals, key=_interval_sort_key):
                rows = await self.get_candles(
                    symbol, exchange, interval, limit=max_per_interval
                )
                out[interval] = [
                    {k: v for k, v in row.items() if k != "_id"} for row in rows
                ]
            return out
        except Exception as e:
            logger.error(f"Error building candles snapshot: {e}")
            return {}

    async def get_latest_price(self, symbol: str, exchange: str) -> Optional[dict]:
        """
        Get the latest price (close of most recent 10s candle or any candle).
        """
        try:
            doc = await self.candles_collection.find_one(
                {"symbol": symbol, "exchange": exchange},
                {"_id": 0, "close": 1, "timestamp": 1, "interval": 1},
                sort=[("timestamp", DESCENDING)],
            )
            return doc

        except Exception as e:
            logger.error(f"Error getting latest price: {e}")
            return None

    async def get_available_exchanges(self) -> List[str]:
        """Get list of exchanges that have candle data."""
        try:
            return await self.candles_collection.distinct("exchange")
        except Exception as e:
            logger.error(f"Error getting exchanges: {e}")
            return []

    async def get_available_symbols(self, exchange: str) -> List[str]:
        """Get list of symbols for an exchange."""
        try:
            return await self.candles_collection.distinct(
                "symbol", {"exchange": exchange}
            )
        except Exception as e:
            logger.error(f"Error getting symbols: {e}")
            return []

    # --- Spike Alert Configuration ---

    async def create_spike_alert(self, config: SpikeAlertConfig) -> str:
        """Create a spike alert config. Returns alert_id."""
        import uuid

        alert_id = str(uuid.uuid4())
        document = config.model_dump()
        document["alert_id"] = alert_id
        document["created_at"] = datetime.utcnow()
        await self.spike_alerts_collection.insert_one(document)
        return alert_id

    async def get_spike_alerts(
        self,
        exchange: Optional[str] = None,
        symbol: Optional[str] = None,
    ) -> List[dict]:
        """Get spike alert configs, optionally filtered."""
        query: dict = {}
        if exchange:
            query["exchange"] = exchange
        if symbol:
            query["$or"] = [{"symbol": symbol}, {"symbol": None}]
        cursor = self.spike_alerts_collection.find(query, {"_id": 0})
        return await cursor.to_list(length=None)

    async def delete_spike_alert(self, alert_id: str) -> bool:
        """Delete a spike alert by ID."""
        result = await self.spike_alerts_collection.delete_one({"alert_id": alert_id})
        return result.deleted_count > 0

    async def get_alerts_for_exchange_symbol(
        self, exchange: str, symbol: str, interval: str
    ) -> List[dict]:
        """Get alert configs that match this exchange/symbol/interval."""
        cursor = self.spike_alerts_collection.find(
            {
                "exchange": exchange,
                "interval": interval,
                "$or": [{"symbol": symbol}, {"symbol": None}],
            },
            {"_id": 0},
        )
        return await cursor.to_list(length=None)

    # --- Detected Alerts (history of all triggered alerts) ---

    async def store_detected_alert(self, alert: PriceAlert) -> bool:
        """Store a detected alert to database for history (includes full candle snapshot)."""
        try:
            candles = await self.get_candles_snapshot_by_symbol_exchange(
                alert.symbol,
                alert.exchange,
                max_per_interval=MAX_CANDLES_PER_INTERVAL,
            )
            document = {
                "symbol": alert.symbol,
                "exchange": alert.exchange,
                "alert_type": alert.alert_type,
                "start_price": alert.start_price,
                "current_price": alert.current_price,
                "change_percent": alert.change_percent,
                "interval": alert.interval,
                "timestamp": alert.timestamp,
                "webhook_url": alert.webhook_url,
                "alert_id": alert.alert_id,
                "candles": candles,
            }
            await self.detected_alerts_collection.insert_one(document)
            return True
        except Exception as e:
            logger.error(f"Error storing detected alert: {e}")
            return False

    async def get_detected_alerts(
        self,
        symbol: str | None = None,
        exchange: str | None = None,
        limit: int = 100,
    ) -> List[dict]:
        """Get detected alert history."""
        try:
            query: dict = {}
            if symbol:
                query["symbol"] = symbol
            if exchange:
                query["exchange"] = exchange
            cursor = (
                self.detected_alerts_collection.find(query, {"_id": 0})
                .sort("timestamp", DESCENDING)
                .limit(limit)
            )
            return await cursor.to_list(length=limit)
        except Exception as e:
            logger.error(f"Error getting detected alerts: {e}")
            return []


# Global instance
mongodb_service = MongoDBService()
