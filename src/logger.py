import sys
from loguru import logger
from .config import settings

def setup_logger():
    logger.remove()
    logger.add(
        sys.stderr,
        level=settings.LOG_LEVEL,
        format="<green>{time:YYYY-MM-DD HH:mm:ss}</green> | <level>{level: <8}</level> | <cyan>{name}</cyan>:<cyan>{function}</cyan>:<cyan>{line}</cyan> - <level>{message}</level>"
    )
    logger.add(
        "logs/bot.log",
        rotation="10 MB",
        retention="1 week",
        level=settings.LOG_LEVEL
    )

setup_logger()
export_logger = logger
