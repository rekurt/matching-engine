# Repository notes

Rust workspace with 12 crates. Read the [architecture](docs/src/architecture.md) and [contribution guide](CONTRIBUTING.md) before changing behavior.

Keep wire protocol v2, session ownership, durable request identity and event-equivalence recovery compatible. Update the handbook when these contracts change. Financial account logic belongs outside this service.

Use graph discovery when available, then verify exact source and coverage. Explain invariants rather than restating code. Add meaningful regression tests for behavior changes and run the checks listed in CONTRIBUTING.md. Keep generated output and review logs outside the repository.
