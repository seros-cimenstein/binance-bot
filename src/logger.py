import sys
from loguru import logger

def setup_logger(log_level="INFO", log_file="logs/bot.log"):
    """Configure loguru to write to stderr and a rotation file."""
    logger.remove()
    logger.add(
        sys.stderr,
        level=log_level,
        format="<green>{time:YYYY-MM-DD HH:mm:ss}</green> | <level>{level: <8}</level> | <cyan>{name}</cyan>:<cyan>{function}</cyan>:<cyan>{line}</cyan> - <level>{message}</level>"
    )
    if log_file:
        logger.add(
            log_file,
            rotation="10 MB",
            retention="1 week",
            level=log_level
        )

# Pre-configure with defaults for simple imports if needed
# But main.py should call setup_logger() explicitly.
