# me-io fuzz harness

Coverage-guided fuzzing of `me_io::codec::decode_frame` against arbitrary
adversarial byte inputs. The invariants we enforce:

1. `decode_frame` MUST NOT panic on any input.
2. Successful decodes MUST consume strictly positive and bounded bytes
   (`0 < consumed <= input.len()`).

Targets:

- `decode_frame` — decoder starting in `HandshakePending` state
- `decode_frame_handshaked` — decoder starting in `Active` state (exercises
  command parsing paths gated on a completed handshake)

## Running

Requires nightly + `cargo-fuzz`:

```sh
# From repo root:
make fuzz-codec-install   # once
make fuzz-codec           # run decode_frame for 60s
make fuzz-codec-handshaked
```

For longer runs (CI nightly job), bump `-max_total_time=` in the Makefile
targets, or run directly:

```sh
cd crates/me-io/fuzz
cargo +nightly fuzz run decode_frame -- -max_total_time=3600
```

Crashes land in `artifacts/decode_frame/` and are minimized automatically.
Copy the minimized reproducer bytes into `tests/decode_frame_smoke.rs` as
a regression guard before fixing the underlying decoder bug.

## Stable-Rust smoke test

For every-CI panic-freedom checks that don't need nightly, see
`crates/me-io/tests/decode_frame_smoke.rs` — it runs the same harness
against a frozen adversarial corpus and byte-walk.
