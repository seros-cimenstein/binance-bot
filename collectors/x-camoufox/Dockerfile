# syntax=docker/dockerfile:1
#
# Phase 43 read-only X collector (Camoufox). Separate from the `bb` image: no
# Binance credentials, no order path. Builds on amd64 and arm64 (Pi 5).

FROM python:3.12-slim-bookworm

COPY --from=ghcr.io/astral-sh/uv:0.11.9 /uv /uvx /usr/local/bin/

ENV PYTHONUNBUFFERED=1 \
    UV_COMPILE_BYTECODE=1 \
    UV_LINK_MODE=copy \
    UV_PYTHON_DOWNLOADS=never \
    UV_PROJECT_ENVIRONMENT=/opt/x-collector/.venv

# Same uid as the `bot` user of the bb image, so both write the shared data dir.
RUN useradd --create-home --uid 1000 --shell /usr/sbin/nologin collector \
 && mkdir -p /opt/x-collector /app/data \
 && chown -R collector:collector /opt/x-collector /app

WORKDIR /opt/x-collector

# Dependencies (camoufox pins playwright) into a venv owned by the runtime user:
# `camoufox fetch` writes its fingerprint model next to the installed package.
COPY --chown=collector:collector pyproject.toml uv.lock README.md ./
USER collector
RUN uv sync --frozen --no-dev --no-install-project

# Firefox runtime libraries (Playwright's list for this distro/arch), Xvfb for
# X_COLLECTOR_HEADLESS=virtual, and tini as PID 1.
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends tini xvfb \
 && /opt/x-collector/.venv/bin/playwright install-deps firefox \
 && rm -rf /var/lib/apt/lists/*

# The Camoufox browser build paired with camoufox==0.5.8, GeoIP db and fingerprint model.
# `camoufox fetch` exits 0 even when GitHub's API refuses it (seen on the Pi 2026-10-19:
# 403 rate limit, nothing installed), so the build checks the browser is really there.
USER collector
RUN /opt/x-collector/.venv/bin/python -m camoufox fetch \
 && /opt/x-collector/.venv/bin/python -c "from camoufox.pkgman import camoufox_path; print('camoufox browser:', camoufox_path(download_if_missing=False))"

COPY --chown=collector:collector src ./src
RUN uv sync --frozen --no-dev

ENV PATH=/opt/x-collector/.venv/bin:$PATH \
    X_COLLECTOR_DATA_DIR=/app/data

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["python", "-m", "x_collector"]
