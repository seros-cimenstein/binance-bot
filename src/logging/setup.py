import sys
from pathlib import Path
from loguru import logger

def setup_logging(log_dir: str = "logs", log_level: str = "INFO"):
    """
    Configures loguru to log to stdout and rotating files.
    Also creates a separate audit log for trades.
    """
    log_path = Path(log_dir)
    log_path.mkdir(parents=True, exist_ok=True)

    # Remove default handler
    logger.remove()

    # Main log file - rotating
    logger.add(
        log_path / "app.log",
        rotation="10 MB",
        retention="10 days",
        level=log_level,
        format="{time:YYYY-MM-DD HH:mm:ss} | {level: <8} | {name}:{function}:{line} - {message}"
    )

    # Console output
    logger.add(
        sys.stdout,
        level=log_level,
        format="<green>{time:YYYY-MM-DD HH:mm:ss}</green> | <level>{level: <8}</level> | <cyan>{name}</cyan>:<cyan>{function}</cyan>:<cyan>{line}</cyan> - <level>{message}</level>"
    )

    # Trade audit log - specifically for buy/sell events
    logger.add(
        log_path / "trades.log",
        rotation="5 MB",
        retention="30 days",
        level="INFO",
        filter=lambda record: "audit" in record["extra"],
        format="{time:YYYY-MM-DD HH:mm:ss} | AUDIT | {message}"
    )

    logger.info("Logging initialized.")

def get_audit_logger():
    """Returns a logger bound with 'audit' extra to trigger the audit log filter."""
    return logger.bind(audit=True)
