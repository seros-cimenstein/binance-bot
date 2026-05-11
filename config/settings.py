from pydantic_settings import BaseSettings, SettingsConfigDict
from pydantic import Field

class Settings(BaseSettings):
    binance_api_key: str = Field(..., validation_alias="BINANCE_API_KEY")
    binance_api_secret: str = Field(..., validation_alias="BINANCE_API_SECRET")
    
    trading_symbol: str = Field("BTC/USDT", validation_alias="TRADING_SYMBOL")
    trade_quantity: float = Field(0.001, validation_alias="TRADE_QUANTITY")
    log_level: str = Field("INFO", validation_alias="LOG_LEVEL")
    use_testnet: bool = Field(True, validation_alias="USE_TESTNET")

    model_config = SettingsConfigDict(env_file=".env", env_file_encoding="utf-8", extra="ignore")

settings = Settings()
