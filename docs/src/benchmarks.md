# Benchmarks

Criterion suites measure the matching engine in isolation. They do not include TLS, network delivery or the durable server's synchronous journal writes.

```sh
cargo bench --locked -p me-bench --bench latency
cargo bench --locked -p me-bench --bench throughput
cargo bench --locked -p me-bench --bench e2e
cargo bench --locked -p me-bench --bench stress
cargo bench --locked -p me-bench --bench realistic
```

Record the Git revision, Rust version, CPU, operating system, compiler options and workload. Run on an otherwise idle machine with a consistent power policy. Criterion writes reports under `target/criterion`; compare repeated runs rather than a single best value.

`me-datagen` creates reproducible order streams with `--seed`. Inspect its options with `cargo run --locked -p me-datagen -- --help`. Record symbol count, order mix, book depth and seed with any results.

For service measurements, include transport, journal sync, client event application and restart/replay cost. Test both steady load and bursts on the intended filesystem. Report percentile latency, throughput, errors, memory and disk growth. No benchmark number in this repository is a deployment SLA.
