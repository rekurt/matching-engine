# Contributing / Участие в разработке

Thank you for your interest in contributing to the Matching Engine project!

Спасибо за интерес к проекту Matching Engine!

## Getting Started / Начало работы

```bash
# Clone the repository / Клонировать репозиторий
git clone https://github.com/rekurt/matching-engine.git
cd matching-engine

# Build / Собрать
cargo build

# Run tests / Запустить тесты
cargo test --workspace

# Run benchmarks / Запустить бенчмарки
cargo bench -p me-bench

# Generate documentation / Сгенерировать документацию
cargo doc --workspace --no-deps --open
```

## Project Structure / Структура проекта

| Crate | Purpose |
|-------|---------|
| `me-core` | Foundational types (FixedPrice, Order, Trade, MatchEvent) |
| `me-book` | Order book (BTreeMap + Slab), matching algorithm |
| `me-engine` | Engine per trading pair, Router, ShardedRouterV2 |
| `me-persist` | WAL with CRC32, atomic snapshots |
| `me-io` | Binary wire protocol, TCP/UDS servers |
| `me-server` | Server binary, config, admin API |
| `me-metrics` | Prometheus metrics |
| `me-bench` | Criterion benchmarks |
| `me-datagen` | Order flow generator |
| `me-risk` | Risk management |
| `me-audit` | Audit logging |

## Development Workflow / Рабочий процесс

1. **Fork and branch** — Create a feature branch from `master`
2. **Write code** — Follow existing code style and patterns
3. **Add tests** — All new functionality must have tests
4. **Run checks:**
   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace -- -D warnings
   cargo test --workspace
   cargo doc --workspace --no-deps -D warnings
   ```
5. **Submit PR** — Describe the changes and link related issues

## Code Style / Стиль кода

- Follow `rustfmt` defaults (run `cargo fmt`)
- No `unsafe` code without detailed safety comments
- Use `expect("context")` instead of `unwrap()` in non-test code
- Add `/// ` doc comments to all public items
- Keep the hot path allocation-free where possible
- Fixed-point arithmetic only (no `f32`/`f64` in matching logic)

## Testing / Тестирование

- **Unit tests** — In the same file as the code (`#[cfg(test)]`)
- **Integration tests** — In `crates/me-engine/tests/`
- **Property tests** — Using `proptest` for invariant verification
- **Benchmarks** — Using `criterion` in `crates/me-bench/benches/`

Run specific test suites:
```bash
cargo test -p me-core
cargo test -p me-book
cargo test -p me-engine
cargo test -p me-persist
cargo test -p me-io
```

## Performance / Производительность

When modifying the hot path (matching algorithm, order book, codec):
- Run benchmarks before and after: `cargo bench -p me-bench`
- Avoid heap allocations in the matching loop
- Prefer `Slab` over `Vec` for order storage
- Use `BTreeMap` for price-level indexing

## Documentation / Документация

- All public APIs must have `///` doc comments
- Architecture docs are bilingual (English + Russian) in `docs/`
- Wire protocol spec: `docs/PROTOCOL.md`
- Keep README.md in sync with actual capabilities

## Commit Messages / Сообщения коммитов

Follow conventional commits:
```
feat: add new order type
fix: correct matching priority for FOK orders
perf: reduce allocations in price level iteration
docs: update wire protocol specification
test: add property tests for order book invariants
refactor: extract config parsing into separate module
```

## License / Лицензия

By contributing, you agree that your contributions will be licensed under the MIT License.
