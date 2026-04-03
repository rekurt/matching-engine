.PHONY: build build-release build-tls test test-all bench bench-latency bench-throughput bench-e2e bench-stress lint fmt fmt-check doc clean run run-release docker docker-push help

# ─── Configuration ────────────────────────────────────────────────────────────

BINARY        := me-server
DOCKER_IMAGE  ?= matching-engine
DOCKER_TAG    ?= latest
TCP_ADDR      ?= 0.0.0.0:9090
CONFIG        ?= config.toml

# ─── Build ────────────────────────────────────────────────────────────────────

build:
	cargo build -p $(BINARY)

build-release:
	cargo build --release -p $(BINARY)

build-tls:
	cargo build --release -p $(BINARY) --features me-io/tls

build-all:
	cargo build --workspace

# ─── Test ─────────────────────────────────────────────────────────────────────

test:
	cargo test --workspace

test-io:
	cargo test -p me-io

test-engine:
	cargo test -p me-engine

test-book:
	cargo test -p me-book

test-persist:
	cargo test -p me-persist

# ─── Benchmarks ───────────────────────────────────────────────────────────────

bench:
	cargo bench -p me-bench

bench-latency:
	cargo bench -p me-bench --bench latency

bench-throughput:
	cargo bench -p me-bench --bench throughput

bench-e2e:
	cargo bench -p me-bench --bench e2e

bench-stress:
	cargo bench -p me-bench --bench stress

# ─── Code quality ─────────────────────────────────────────────────────────────

lint:
	cargo clippy --workspace -- -D warnings

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

doc:
	cargo doc --workspace --no-deps --open

# ─── Run ──────────────────────────────────────────────────────────────────────

run:
	cargo run -p $(BINARY) -- --config $(CONFIG) --tcp-addr $(TCP_ADDR)

run-release:
	cargo run --release -p $(BINARY) -- --config $(CONFIG) --tcp-addr $(TCP_ADDR)

# ─── Docker ───────────────────────────────────────────────────────────────────

docker:
	docker build -t $(DOCKER_IMAGE):$(DOCKER_TAG) .

docker-push:
	docker push $(DOCKER_IMAGE):$(DOCKER_TAG)

# ─── Misc ─────────────────────────────────────────────────────────────────────

clean:
	cargo clean

help:
	@echo "Usage: make <target>"
	@echo ""
	@echo "Build:"
	@echo "  build            Dev build (me-server)"
	@echo "  build-release    Release build (me-server)"
	@echo "  build-tls        Release build with TLS support"
	@echo "  build-all        Dev build (all crates)"
	@echo ""
	@echo "Test:"
	@echo "  test             Run all workspace tests"
	@echo "  test-io          Run me-io tests"
	@echo "  test-engine      Run me-engine tests"
	@echo "  test-book        Run me-book tests"
	@echo "  test-persist     Run me-persist tests"
	@echo ""
	@echo "Benchmarks:"
	@echo "  bench            Run all benchmarks"
	@echo "  bench-latency    Run latency benchmark"
	@echo "  bench-throughput Run throughput benchmark"
	@echo "  bench-e2e        Run end-to-end benchmark"
	@echo "  bench-stress     Run stress benchmark"
	@echo ""
	@echo "Quality:"
	@echo "  lint             Clippy (warnings as errors)"
	@echo "  fmt              Format all code"
	@echo "  fmt-check        Check formatting (CI)"
	@echo "  doc              Build and open docs"
	@echo ""
	@echo "Run:"
	@echo "  run              Dev run (TCP_ADDR=$(TCP_ADDR), CONFIG=$(CONFIG))"
	@echo "  run-release      Release run"
	@echo ""
	@echo "Docker:"
	@echo "  docker           Build Docker image (DOCKER_IMAGE=$(DOCKER_IMAGE):$(DOCKER_TAG))"
	@echo "  docker-push      Push Docker image"
	@echo ""
	@echo "  clean            Remove build artifacts"
