# Contributing

Branch from `master`. Describe the behavior changed, the reason, and the checks run. Add regression tests for matching, protocol or recovery changes; keep unrelated cleanup separate.

## Local checks

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build --release --workspace --all-features --locked
cargo build -p me-server --features tls --locked
ME_TEST_TLS=1 python3 scripts/runtime_smoke.py
python3 -m unittest discover -s examples/python
cargo test --manifest-path examples/rust/Cargo.toml --locked
(cd clients/go/meclient && go test -race ./...)
(cd examples/go && go test ./...)
(cd examples/typescript && npm ci --ignore-scripts && npm test)
cargo audit
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked
mdbook build
```

Use Rust 1.96+, Go 1.22+, Python 3.10+, Node.js 22+, OpenSSL and mdBook 0.4.40. `make setup` installs the repository's optional pre-commit hook.

## Conventions

- Use rustfmt and existing type conventions. Keep matching arithmetic in fixed-point integers.
- Explain invariants, safety requirements and tradeoffs in comments; avoid narrating individual statements.
- Document changes to wire layouts, request identity, replay semantics and storage compatibility in the [handbook](docs/src/SUMMARY.md).
- Treat session ownership, event ordering, retry behavior and crash recovery as observable contracts.
- Never remove a failing assertion or widen an acceptance condition merely to pass CI.

## Fuzzing and benchmarks

Codec fuzz targets live in `crates/me-io/fuzz`. Run `make fuzz-codec-install`, then `make fuzz-codec` or `make fuzz-codec-handshaked`; each target runs for 60 seconds.

See [benchmarks](docs/src/benchmarks.md) for reproducible engine measurements. Preserve proptest regression seeds.
