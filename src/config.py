import os
from pydantic_settings import BaseSettings, SettingsConfigDict
from typing import Optional

class Settings(BaseSettings):
    BINANCE_API_KEY: str
    BINANCE_SECRET_KEY: str
    TRADING_SYMBOL: str = "BTC/USDT"
    ORDER_SIZE: float = 0.001
    LOG_LEVEL: str = "INFO"

    model_config = SettingsConfigDict(
        env_file=".env",
        env_file_encoding="utf-8",
        extra="ignore"
    )

settings = Settings(_env_file="/home/user/projects/binance-bot/.env")
