# syntax=docker/dockerfile:1
#
# Binance bot fleet image: one static-ish `bb` binary, the read-only `cryptobot-gui`
# dashboard, plus the `omp` agent CLI (bot decisions for bot-spot and the capital
# allocator run through omp RPC).

# ---------- build ----------
FROM rust:1-bookworm AS build

# aws-lc-rs (rustls crypto provider) needs cmake + clang; rusqlite bundles SQLite via cc.
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

ENV TZ=UTC \
    BUN_INSTALL=/opt/bun \
    PATH=/opt/bun/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

ARG TARGETARCH
ARG SUPERCRONIC_VERSION=v0.2.30
ARG BUN_VERSION=bun-v1.3.14
ARG OMP_VERSION=18.8.9

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        sqlite3 ca-certificates curl unzip tini tzdata \
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

# omp (oh-my-pi coding agent) on Bun, pinned. Used only in RPC mode with --no-tools.
RUN set -eux; \
    curl -fsSL https://bun.sh/install | bash -s "${BUN_VERSION}"; \
    bun add -g "@oh-my-pi/pi-coding-agent@${OMP_VERSION}"; \
    ln -s /opt/bun/bin/omp /usr/local/bin/omp; \
    omp --version

COPY --from=build /usr/local/bin/bb /usr/local/bin/cryptobot-gui /usr/local/bin/

WORKDIR /app
COPY docker ./docker
COPY scripts ./scripts

RUN useradd --create-home --uid 1000 --shell /bin/bash bot \
 && mkdir -p /app/data /app/backups /app/logs \
 && chown -R bot:bot /app
USER bot

# omp keeps credentials, settings and sessions under PI_CODING_AGENT_DIR: put it on the
# persistent data volume so a one-time `omp` login (or ANTHROPIC_API_KEY) survives recreates.
ENV BINANCE_BOT_BACKUP_DIR=/app/backups \
    PI_CODING_AGENT_DIR=/app/data/omp-agent

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["bb", "--help"]
