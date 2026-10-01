FROM python:3.12-slim AS base

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1 \
    PIP_NO_CACHE_DIR=1 \
    TZ=UTC

ARG TARGETARCH
ARG SUPERCRONIC_VERSION=v0.2.30

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        sqlite3 \
        ca-certificates \
        curl \
        tini \
        tzdata \
 && rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64)  SC_ARCH=amd64 ;; \
      arm64)  SC_ARCH=arm64 ;; \
      arm)    SC_ARCH=arm ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /usr/local/bin/supercronic \
        "https://github.com/aptible/supercronic/releases/download/${SUPERCRONIC_VERSION}/supercronic-linux-${SC_ARCH}"; \
    chmod +x /usr/local/bin/supercronic; \
    /usr/local/bin/supercronic -test /dev/null

WORKDIR /app

COPY requirements.txt ./
RUN pip install -r requirements.txt

COPY . .

RUN mkdir -p /app/backups /app/logs

RUN useradd --create-home --uid 1000 --shell /bin/bash bot \
 && chown -R bot:bot /app
USER bot

ENV BINANCE_BOT_BACKUP_DIR=/app/backups

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["python", "-m", "src.main"]
