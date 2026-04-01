# Stage 1: Build
FROM rust:latest AS builder

WORKDIR /usr/src/matching-engine

# Copy manifests first for better layer caching
COPY Cargo.toml Cargo.lock ./
COPY crates/me-core/Cargo.toml crates/me-core/Cargo.toml
COPY crates/me-book/Cargo.toml crates/me-book/Cargo.toml
COPY crates/me-engine/Cargo.toml crates/me-engine/Cargo.toml
COPY crates/me-persist/Cargo.toml crates/me-persist/Cargo.toml
COPY crates/me-io/Cargo.toml crates/me-io/Cargo.toml
COPY crates/me-bench/Cargo.toml crates/me-bench/Cargo.toml
COPY crates/me-datagen/Cargo.toml crates/me-datagen/Cargo.toml
COPY crates/me-server/Cargo.toml crates/me-server/Cargo.toml

# Create stub sources so cargo can resolve the workspace
RUN mkdir -p crates/me-core/src && echo "" > crates/me-core/src/lib.rs && \
    mkdir -p crates/me-book/src && echo "" > crates/me-book/src/lib.rs && \
    mkdir -p crates/me-engine/src && echo "" > crates/me-engine/src/lib.rs && \
    mkdir -p crates/me-persist/src && echo "" > crates/me-persist/src/lib.rs && \
    mkdir -p crates/me-io/src && echo "" > crates/me-io/src/lib.rs && \
    mkdir -p crates/me-bench/src && echo "" > crates/me-bench/src/lib.rs && \
    mkdir -p crates/me-datagen/src && echo "fn main() {}" > crates/me-datagen/src/main.rs && \
    mkdir -p crates/me-server/src && echo "fn main() {}" > crates/me-server/src/main.rs

# Pre-build dependencies
RUN cargo build --release -p me-server 2>/dev/null || true

# Copy real sources
COPY crates/ crates/

# Build the actual binary
RUN cargo build --release -p me-server

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/matching-engine/target/release/me-server /usr/local/bin/me-server

EXPOSE 9090

CMD ["me-server", "--tcp-addr", "0.0.0.0:9090"]
