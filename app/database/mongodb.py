import logging
from datetime import datetime, timedelta
from typing import List, Optional
from motor.motor_asyncio import AsyncIOMotorClient, AsyncIOMotorDatabase, AsyncIOMotorCollection
from pymongo import IndexModel, ASCENDING, DESCENDING
from pymongo.errors import DuplicateKeyError

from app.config import settings
from app.models.price import PriceUpdate, TickerOwnership

logger = logging.getLogger(__name__)


class MongoDBService:
    """
    MongoDB service for storing price data and managing ticker ownership.
    
    This service handles:
    - Price data storage with automatic TTL (time-to-live) based on retention hours
    - Ticker ownership tracking (first-come-first-serve)
    - Efficient querying for spike detection
    - Automatic cleanup of old data
    """
    
    def __init__(self):
        """Initialize MongoDB service."""
        self.client: Optional[AsyncIOMotorClient] = None
        self.db: Optional[AsyncIOMotorDatabase] = None
        self.prices_collection: Optional[AsyncIOMotorCollection] = None
        self.ownership_collection: Optional[AsyncIOMotorCollection] = None
    
    async def connect(self) -> None:
        """Establish connection to MongoDB and setup collections."""
        try:
            logger.info(f"Connecting to MongoDB at {settings.mongodb_url}")
            
            # Create async MongoDB client
            self.client = AsyncIOMotorClient(settings.mongodb_url)
            self.db = self.client[settings.mongodb_db_name]
            
            # Initialize collections
            self.prices_collection = self.db["prices"]
            self.ownership_collection = self.db["ticker_ownership"]
            
            # Setup indexes
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
        """
        Create indexes for optimal query performance.
        
        Indexes:
        - prices: (symbol, exchange, timestamp) for efficient time-range queries
        - prices: (timestamp) with TTL for automatic data cleanup
        - ticker_ownership: (symbol) unique to prevent duplicate ownership
        """
        try:
            # Price collection indexes
            price_indexes = [
                # Compound index for querying specific symbol's price history
                IndexModel(
                    [("symbol", ASCENDING), ("exchange", ASCENDING), ("timestamp", DESCENDING)],
                    name="symbol_exchange_timestamp_idx"
                ),
                # TTL index - automatically delete documents after retention period
                IndexModel(
                    [("timestamp", ASCENDING)],
                    name="timestamp_ttl_idx",
                    expireAfterSeconds=settings.price_retention_hours * 3600
                ),
            ]
            await self.prices_collection.create_indexes(price_indexes)
            
            # Ownership collection indexes
            ownership_indexes = [
                # Unique index on symbol to ensure single ownership
                IndexModel(
                    [("symbol", ASCENDING)],
                    name="symbol_unique_idx",
                    unique=True
                ),
                # Index on exchange for querying owned symbols
                IndexModel(
                    [("exchange", ASCENDING)],
                    name="exchange_idx"
                ),
            ]
            await self.ownership_collection.create_indexes(ownership_indexes)
            
            logger.info("MongoDB indexes created successfully")
            
        except Exception as e:
            logger.error(f"Error creating indexes: {e}")
            raise
    
    async def store_price_update(self, price_update: PriceUpdate) -> bool:
        """
        Store a price update in the database.
        
        Args:
            price_update: Price update to store
            
        Returns:
            bool: True if stored successfully, False otherwise
        """
        try:
            document = {
                "symbol": price_update.symbol,
                "exchange": price_update.exchange,
                "price": price_update.price,
                "timestamp": price_update.timestamp,
                "mark_price": price_update.mark_price,
                "index_price": price_update.index_price,
            }
            
            await self.prices_collection.insert_one(document)
            return True
            
        except Exception as e:
            logger.error(f"Error storing price update: {e}, data: {price_update}")
            return False
    
    async def bulk_store_price_updates(self, price_updates: List[PriceUpdate]) -> int:
        """
        Store multiple price updates in a single batch operation.
        
        This is more efficient than storing updates one by one.
        
        Args:
            price_updates: List of price updates to store
            
        Returns:
            int: Number of documents inserted
        """
        if not price_updates:
            return 0
        
        try:
            documents = [
                {
                    "symbol": pu.symbol,
                    "exchange": pu.exchange,
                    "price": pu.price,
                    "timestamp": pu.timestamp,
                    "mark_price": pu.mark_price,
                    "index_price": pu.index_price,
                }
                for pu in price_updates
            ]
            
            result = await self.prices_collection.insert_many(documents, ordered=False)
            return len(result.inserted_ids)
            
        except Exception as e:
            logger.error(f"Error in bulk store: {e}")
            return 0
    
    async def get_price_history(
        self,
        symbol: str,
        exchange: str,
        minutes: int
    ) -> List[dict]:
        """
        Get price history for a symbol within the specified time window.
        
        Args:
            symbol: Trading pair symbol
            exchange: Exchange name
            minutes: Number of minutes to look back
            
        Returns:
            List of price documents sorted by timestamp (oldest first)
        """
        try:
            start_time = datetime.utcnow() - timedelta(minutes=minutes)
            
            cursor = self.prices_collection.find(
                {
                    "symbol": symbol,
                    "exchange": exchange,
                    "timestamp": {"$gte": start_time}
                },
                {"_id": 0}  # Exclude MongoDB _id field
            ).sort("timestamp", ASCENDING)
            
            return await cursor.to_list(length=None)
            
        except Exception as e:
            logger.error(f"Error getting price history: {e}")
            return []
    
    async def claim_ticker_ownership(self, symbol: str, exchange: str) -> bool:
        """
        Attempt to claim ownership of a ticker for an exchange.
        
        Uses first-come-first-serve logic: if no exchange owns the ticker yet,
        the ownership is claimed. If another exchange already owns it, the
        claim is rejected.
        
        Args:
            symbol: Trading pair symbol
            exchange: Exchange attempting to claim ownership
            
        Returns:
            bool: True if ownership claimed successfully, False if already owned
        """
        try:
            ownership = {
                "symbol": symbol,
                "exchange": exchange,
                "claimed_at": datetime.utcnow(),
                "last_update": datetime.utcnow(),
            }
            
            await self.ownership_collection.insert_one(ownership)
            logger.info(f"Ticker ownership claimed: {symbol} by {exchange}")
            return True
            
        except DuplicateKeyError:
            # Symbol already owned by another exchange
            logger.debug(f"Ticker {symbol} already owned by another exchange")
            return False
        except Exception as e:
            logger.error(f"Error claiming ownership: {e}")
            return False
    
    async def check_ticker_ownership(self, symbol: str, exchange: str) -> bool:
        """
        Check if an exchange owns a specific ticker.
        
        Args:
            symbol: Trading pair symbol
            exchange: Exchange name
            
        Returns:
            bool: True if the exchange owns the ticker, False otherwise
        """
        try:
            ownership = await self.ownership_collection.find_one(
                {"symbol": symbol, "exchange": exchange}
            )
            return ownership is not None
            
        except Exception as e:
            logger.error(f"Error checking ownership: {e}")
            return False
    
    async def update_ticker_last_seen(self, symbol: str, exchange: str) -> bool:
        """
        Update the last_update timestamp for a ticker ownership.
        
        Args:
            symbol: Trading pair symbol
            exchange: Exchange name
            
        Returns:
            bool: True if updated successfully
        """
        try:
            result = await self.ownership_collection.update_one(
                {"symbol": symbol, "exchange": exchange},
                {"$set": {"last_update": datetime.utcnow()}}
            )
            return result.modified_count > 0
            
        except Exception as e:
            logger.error(f"Error updating last seen: {e}")
            return False
    
    async def get_owned_tickers(self, exchange: str) -> List[str]:
        """
        Get all tickers owned by a specific exchange.
        
        Args:
            exchange: Exchange name
            
        Returns:
            List of ticker symbols
        """
        try:
            cursor = self.ownership_collection.find(
                {"exchange": exchange},
                {"symbol": 1, "_id": 0}
            )
            results = await cursor.to_list(length=None)
            return [doc["symbol"] for doc in results]
            
        except Exception as e:
            logger.error(f"Error getting owned tickers: {e}")
            return []
    
    async def get_ticker_count_by_exchange(self) -> dict:
        """
        Get count of tickers owned by each exchange.
        
        Returns:
            Dictionary mapping exchange names to ticker counts
        """
        try:
            pipeline = [
                {"$group": {"_id": "$exchange", "count": {"$sum": 1}}}
            ]
            
            cursor = self.ownership_collection.aggregate(pipeline)
            results = await cursor.to_list(length=None)
            
            return {doc["_id"]: doc["count"] for doc in results}
            
        except Exception as e:
            logger.error(f"Error getting ticker counts: {e}")
            return {}


# Global instance
mongodb_service = MongoDBService()
