import sys
from pathlib import Path
from loguru import logger

def setup_logging(log_dir: str = "logs"):
    """
    Sets up logging using loguru.
    - Stdout for real-time monitoring.
    - logs/app.log for general application logs (rotating).
    - logs/trades.log for trade audit logs (rotating).
    """
    log_path = Path(log_dir)
    log_path.mkdir(parents=True, exist_ok=True)

    # Remove default handler
    logger.remove()

    # General log format
    log_format = (
        "<green>{time:YYYY-MM-DD HH:mm:ss.SSS}</green> | "
        "<level>{level: <8}</level> | "
        "<cyan>{name}</cyan>:<cyan>{function}</cyan>:<cyan>{line}</cyan> - "
        "<level>{message}</level>"
    )

    # 1. Stdout Logger
    logger.add(
        sys.stdout,
        format=log_format,
        level="INFO",
        colorize=True
    )

    # 2. General File Logger (Rotating)
    logger.add(
        log_path / "app.log",
        rotation="10 MB",
        retention="1 month",
        compression="zip",
        format=log_format,
        level="DEBUG",
        enqueue=True
    )

    # 3. Trade Audit Logger (Specialized)
    # Filter for logs that specifically mention "TRADE" or are sent to a specific logger name if needed.
    # For simplicity, we can use a filter function or a separate logger if loguru allowed easy multiple instances.
    # Here we use a filter to separate trade logs.
    logger.add(
        log_path / "trades.log",
        rotation="5 MB",
        retention="1 year",
        compression="zip",
        format="{time:YYYY-MM-DD HH:mm:ss.SSS} | {message}",
        level="INFO",
        filter=lambda record: "trade_audit" in record["extra"],
        enqueue=True
    )

    logger.info("Logging system initialized.")

# Helper to log trades specifically
def log_trade(message: str):
    logger.bind(trade_audit=True).info(message)

if __name__ == "__main__":
    setup_logging()
    logger.debug("Debug message for general log")
    logger.info("Info message for general log and stdout")
    log_trade("AUDIT: Bought 0.01 BTC at 50000 USDT")
