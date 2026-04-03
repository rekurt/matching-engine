# syntax=docker/dockerfile:1.7
# ─────────────────────────────────────────────────────────────────────────────
# Stage 1 — cargo-chef planner
#   Computes a "recipe" (dependency manifest) so the next stage can cache deps
#   independently from source code changes.
# ─────────────────────────────────────────────────────────────────────────────
FROM rust:1.88-slim-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /build

# ─────────────────────────────────────────────────────────────────────────────
# Stage 2 — dependency cache
#   Only re-runs when Cargo.toml / Cargo.lock changes, not on src edits.
# ─────────────────────────────────────────────────────────────────────────────
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS cacher
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json -p me-server

# ─────────────────────────────────────────────────────────────────────────────
# Stage 3 — full build
# ─────────────────────────────────────────────────────────────────────────────
FROM chef AS builder
COPY . .

# Reuse pre-built deps from cacher stage
COPY --from=cacher /build/target target
RUN cargo build --release -p me-server

# ─────────────────────────────────────────────────────────────────────────────
# Stage 4 — minimal runtime image
# ─────────────────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# Install only what's needed at runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 meuser \
    && useradd  --uid 10001 --gid meuser --no-create-home --shell /sbin/nologin meuser

COPY --from=builder /build/target/release/me-server /usr/local/bin/me-server

# OCI image metadata
ARG BUILD_DATE
ARG GIT_SHA
LABEL org.opencontainers.image.title="matching-engine" \
      org.opencontainers.image.description="High-performance HFT cryptocurrency spot matching engine" \
      org.opencontainers.image.source="https://github.com/rekurt/matching-engine" \
      org.opencontainers.image.revision="${GIT_SHA}" \
      org.opencontainers.image.created="${BUILD_DATE}" \
      org.opencontainers.image.licenses="MIT"

USER meuser

# TCP order gateway | HTTP metrics+admin
EXPOSE 9090 8080

HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

ENTRYPOINT ["/usr/local/bin/me-server"]
CMD ["--tcp-addr", "0.0.0.0:9090", "--metrics-addr", "0.0.0.0:8080"]
