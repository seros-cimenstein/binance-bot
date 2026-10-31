# syntax=docker/dockerfile:1
#
# Binance Bot Spot image: one static-ish `bb` binary (the algorithm trader) and the
# read-only `cryptobot-gui` dashboard, plus supercronic for the scheduler.

# ---------- build ----------
FROM rust:1-bookworm AS build

# aws-lc-rs (rustls crypto provider) needs cmake + clang.
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake clang \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p bb-cli -p cryptobot-gui \
 && install -m 0755 target/release/bb target/release/cryptobot-gui /usr/local/bin/

# ---------- runtime ----------
FROM debian:bookworm-slim AS runtime

ENV TZ=UTC

ARG TARGETARCH
ARG SUPERCRONIC_VERSION=v0.2.30

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates curl tini tzdata \
 && rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) SC_ARCH=amd64 ;; \
      arm64) SC_ARCH=arm64 ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /usr/local/bin/supercronic \
        "https://github.com/aptible/supercronic/releases/download/${SUPERCRONIC_VERSION}/supercronic-linux-${SC_ARCH}"; \
    chmod +x /usr/local/bin/supercronic; \
    /usr/local/bin/supercronic -test /dev/null

COPY --from=build /usr/local/bin/bb /usr/local/bin/cryptobot-gui /usr/local/bin/

WORKDIR /app
COPY docker ./docker
COPY scripts ./scripts
COPY config ./config

RUN useradd --create-home --uid 1000 --shell /bin/bash bot \
 && mkdir -p /app/data /app/backups /app/logs \
 && chown -R bot:bot /app
USER bot

ENV BINANCE_BOT_BACKUP_DIR=/app/backups

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["bb", "--help"]
