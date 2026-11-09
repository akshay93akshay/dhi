# ---------------------------------------------------------------------------
# Web UI: build the static Next.js export that dhi-server serves same-origin.
# ---------------------------------------------------------------------------
FROM node:22-alpine AS web

WORKDIR /app/web

COPY web/package.json web/package-lock.json web/.npmrc ./
RUN npm ci

COPY web/ ./
RUN rm -rf out && NEXT_EXPORT=1 npm run build

# ---------------------------------------------------------------------------
# Rust: build the release server binary.
# ---------------------------------------------------------------------------
# cargo-chef splits dependency compilation from source compilation. Docker
# caches the dependency layer until Cargo.toml or Cargo.lock changes, so a
# normal source edit only rebuilds the DHI crates.
FROM rust:1.99.1-bookworm AS chef

# aws-lc-sys (pulled in by rustls) builds C sources and needs cmake; onig_sys
# and other native dependencies probe with pkg-config.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        cmake \
        perl \
        pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && cargo install cargo-chef --locked

WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
COPY rust-toolchain.toml ./
RUN cargo chef cook --release --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
RUN cargo build --release -p dhi-server

# ---------------------------------------------------------------------------
# Docker CLI: the server starts one task container per task through the host
# daemon socket. The image needs the client, not a daemon.
# ---------------------------------------------------------------------------
FROM docker:27-cli AS dockercli

# ---------------------------------------------------------------------------
# Runtime: server binary, static web export, ops prompts/guard rules, and a
# pinned chess MCP server.
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        passwd \
        python3 \
        python3-venv \
        util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m venv /opt/chess \
    && /opt/chess/bin/pip install --no-cache-dir chess-mcp-server==0.1.17

RUN groupadd --gid 10001 dhi \
    && useradd --uid 10001 --gid 10001 --create-home --shell /usr/sbin/nologin dhi

COPY --from=builder /app/target/release/dhi-server /usr/local/bin/dhi-server
COPY --from=dockercli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=web /app/web/out /app/web/out
COPY ops/ /app/ops/
COPY ops/mcp.docker.json /app/ops/mcp.json
COPY docker/entrypoint.sh /usr/local/bin/dhi-entrypoint

RUN chmod 0755 /usr/local/bin/dhi-entrypoint \
    && mkdir -p /app/workspace \
    && chown -R dhi:dhi /app

ENV HOME=/app \
    DHI_WEB_DIR=/app/web/out \
    DHI_PROMPTS_DIR=/app/ops/prompts \
    DHI_MCP_CONFIG=/app/ops/mcp.json

WORKDIR /app

EXPOSE 8080 8090

HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/api/health || exit 1

# The entrypoint starts as root only to make the compose bind mount writable,
# then drops to the unprivileged dhi user before exec'ing the server.
ENTRYPOINT ["/usr/local/bin/dhi-entrypoint"]
CMD ["dhi-server"]
