# RFC: Type-Safety Newtypes for Session & Sequence Invariants

## Status

Proposed — follow-up to `feat/protocol-v2-exactly-once` (commit `5862f0d`).
Not urgent; implementation deferred until after protocol-v2 bake-in.

## Overview

Protocol v2 introduced several invariants that are currently enforced by
convention and documentation rather than by the type system:

1. `SessionId` is a raw `[u8; 16]` type alias — no distinction from any
   other 16-byte array; `[0u8; 16]` sentinel encoded only in comments.
2. `server_frame_seq: u64` — the "seq=0 is reserved" invariant lives in
   `ConnectionCodecState::new()` initializing to 1; nothing stops a caller
   from writing `conn.server_frame_seq = 0`.
3. `Order.client_session_id` is `pub` and directly mutated in tcp_server,
   bypassing the `with_session_id` builder.
4. `SessionStore::create()` does not guard against session-id collisions
   (astronomically unlikely, but silently overwrites on `map.insert`).

Type-design review (2026-04-05) flagged #1 and #2 as "do before merge"
for maximum leverage. We deferred them to land protocol v2 quickly; this
RFC captures the plan to revisit.

## Context

- Related files:
  - `crates/me-io/src/session_store.rs` — `SessionId = [u8; 16]` alias,
    `SessionStore::create()`
  - `crates/me-io/src/codec.rs` — `ConnectionCodecState.server_frame_seq`
  - `crates/me-core/src/order.rs` — `Order.client_session_id` field
  - `crates/me-engine/src/router.rs` — consumers of session_id in snapshot
  - `crates/me-io/src/tcp_server.rs` + `uds_server.rs` — session_id flow
- Existing patterns: `SymbolId(u16)`, `OrderId` (u64 alias — similar weakness)
- Dependencies: none new

## Motivation

| Invariant | Current Enforcement | Failure Mode |
|-----------|--------------------|--------------|
| session_id is random, not arbitrary bytes | Convention | Passing wrong 16 bytes silently accepted |
| `[0u8;16]` = "no session" | Comments | No `is_none()` / `NONE` constant |
| Log-safe prefix is first 4 bytes | 13+ ad-hoc `&id[..4]` call sites | Easy to drift/inconsistent |
| server_frame_seq >= 1 (seq=0 reserved) | `::new()` + comments | Direct field assignment breaks invariant |
| Frame seq advances monotonically | `server_frame_seq += count` by each caller | Any caller can reset to 0 |

## Proposal

### A. `SessionId` newtype (high-leverage)

```rust
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct SessionId([u8; 16]);

impl SessionId {
    /// Sentinel meaning "no session bound".
    pub const NONE: Self = Self([0; 16]);

    /// Generate a cryptographically random session id.
    pub fn random() -> Self { /* rand::rng().fill(&mut [0u8;16]) */ }

    pub fn is_none(&self) -> bool { self.0 == [0; 16] }

    /// Log-safe 4-byte prefix, hex-encoded.
    pub fn log_prefix(&self) -> String { hex::encode(&self.0[..4]) }

    pub fn as_bytes(&self) -> &[u8; 16] { &self.0 }
}

impl fmt::Debug for SessionId { /* hex-encoded short form */ }
```

**Replaces**:
- `SessionId = [u8; 16]` alias
- All `&id[..4]` logging call sites → `id.log_prefix()`
- `[0u8;16]` sentinel → `SessionId::NONE` / `.is_none()`
- `rand::rng().fill(&mut id)` → `SessionId::random()`

**Touches**: ~10-12 files (session_store, codec, tcp_server, uds_server,
order, router, tests).

### B. `ServerFrameSeq` newtype (medium-leverage)

```rust
pub struct ServerFrameSeq(u64);

impl ServerFrameSeq {
    /// First seq assigned to outbound frames (seq=0 is reserved).
    pub const FIRST: Self = Self(1);

    /// Return current seq and advance by 1.
    pub fn next(&mut self) -> u64 { let s = self.0; self.0 += 1; s }

    /// Advance by `n` frames after a batch push.
    pub fn advance_by(&mut self, n: u64) { self.0 += n; }

    /// Peek at the next seq without consuming.
    pub fn peek(&self) -> u64 { self.0 }

    /// Used by ResumeSession handler to re-seed from session_store.
    pub fn reset_to(&mut self, next_seq: u64) { self.0 = next_seq; }
}
```

**Replaces**: `ConnectionCodecState.server_frame_seq: u64` and
`conn.server_frame_seq += frames_pushed` call sites.

**Invariant**: the type cannot represent seq=0 — `FIRST` is 1, `next()`
only increments. The only way to set a value externally is `reset_to()`,
which should only be called from the ResumeSession handler after
`SessionStore::next_outbound_seq()`.

### C. `SessionStore::create()` collision retry

```rust
pub fn create(&self) -> SessionId {
    loop {
        let id = SessionId::random();
        let shard = self.shard_for(&id);
        let mut map = Self::lock_shard(shard, &id, "create");
        if !map.contains_key(&id) {
            map.insert(id, SessionEntry::new(...));
            return id;
        }
        // Collision (astronomical): retry with fresh random id.
        tracing::warn!(?id, "SessionId collision on create — retrying");
    }
}
```

**Justification**: 128-bit random id collision is ~1 in 2^64 even after
billions of sessions, but when it hits on a long-running server, silent
state overwrite is worse than the cost of a `contains_key` check.

## Development Approach

- Testing: TDD strict (red-green) for each newtype method
- Complete A → B → C; each task lands in its own commit
- Tests must pass between commits (no "WIP" state)

## Implementation Steps

### Task A1: Introduce `SessionId` newtype (constructors, accessors, Debug)

**Files:**
- Modify: `crates/me-io/src/session_store.rs` — replace type alias
- Create: inline `impl SessionId` block with NONE, random, is_none, log_prefix, as_bytes, Debug/Display
- Create: `crates/me-io/tests/session_id_newtype_tests.rs` with unit tests

- [ ] Write failing test: `SessionId::NONE.is_none()` returns true
- [ ] Write failing test: `SessionId::random()` returns non-NONE
- [ ] Write failing test: `log_prefix` returns 8-char hex
- [ ] Write failing test: Debug format is short hex
- [ ] Implement `SessionId` struct with constructors
- [ ] Run session_id_newtype_tests.rs → green
- [ ] Run `cargo build --workspace` — compilation errors in consumers are expected

### Task A2: Propagate `SessionId` through session_store API

**Files:**
- Modify: `crates/me-io/src/session_store.rs` — all `&SessionId` params
- Modify: `crates/me-io/src/codec.rs` — `ConnectionCodecState.session_id`
- Modify: all 13+ `lock_shard(shard, id, "...")` call sites

- [ ] Replace `[0u8;16]` literals with `SessionId::NONE`
- [ ] Replace `rand::rng().fill(&mut id)` with `SessionId::random()`
- [ ] Replace `&id[..4]` logging with `id.log_prefix()`
- [ ] Run `cargo test -p me-io` — all tests green

### Task A3: Propagate `SessionId` through tcp_server/uds_server/router/order

**Files:**
- Modify: `crates/me-core/src/order.rs` — `client_session_id` field type
- Modify: `crates/me-engine/src/router.rs` — `get_open_orders_by_session` signature
- Modify: `crates/me-io/src/tcp_server.rs` + `uds_server.rs` — conn.session_id usage
- Modify: `crates/me-book/src/book.rs` — `scan_by_session_id` signature

- [ ] Update Order::client_session_id to SessionId, default SessionId::NONE
- [ ] Compile-time assert still fires: size_of::<Order>() == 88
- [ ] Update `scan_by_session_id(session_id: &SessionId)`
- [ ] Run `cargo test --workspace` — all tests green
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings` — 0 warnings

### Task B1: Introduce `ServerFrameSeq` newtype

**Files:**
- Create: `ServerFrameSeq` struct in `crates/me-io/src/codec.rs` or new module
- Create: `crates/me-io/tests/server_frame_seq_newtype_tests.rs`

- [ ] Write failing test: `FIRST.peek() == 1`
- [ ] Write failing test: `next()` returns current, advances to +1
- [ ] Write failing test: `advance_by(5)` from FIRST → peek == 6
- [ ] Write failing test: `reset_to(100)` → next() returns 100
- [ ] Write failing test: cannot construct with seq=0 (no public constructor other than FIRST)
- [ ] Implement struct
- [ ] Run tests → green

### Task B2: Replace raw u64 with `ServerFrameSeq` in ConnectionCodecState

**Files:**
- Modify: `crates/me-io/src/codec.rs` — `ConnectionCodecState.server_frame_seq`
- Modify: `crates/me-io/src/tcp_server.rs` + `uds_server.rs` — usage sites
- Modify: `crates/me-io/tests/e2e_exactly_once_tests.rs` — test helpers

- [ ] Replace `pub server_frame_seq: u64` with `pub server_frame_seq: ServerFrameSeq`
- [ ] Replace `conn.server_frame_seq += frames_pushed` with `.advance_by(frames_pushed)`
- [ ] Replace `conn.server_frame_seq =` in ResumeSession handler with `.reset_to(...)`
- [ ] Update tests that directly manipulate server_frame_seq
- [ ] Run `cargo test --workspace` — green
- [ ] Run `cargo clippy` — green

### Task C1: Collision-safe `SessionStore::create()`

**Files:**
- Modify: `crates/me-io/src/session_store.rs` — `create()`
- Modify: `crates/me-io/tests/session_store_v2_tests.rs` — add collision test

- [ ] Write test: mock PRNG or inject fixed-id generator, assert retry-on-collision
  (may need a `create_with_rng` test hook)
- [ ] Implement retry loop with tracing::warn! on collision
- [ ] Run tests → green

## Verification

1. `cargo test --workspace` — all 420+ tests pass
2. `cargo clippy --workspace --all-targets -- -D warnings` — 0 warnings
3. `cargo bench -p me-bench` — no regression in matching hot path
4. `grep -rn "\[0u8; *16\]" crates/` — zero matches (all replaced with NONE)
5. `grep -rn "\.server_frame_seq = " crates/me-io/src/` — only reset_to() in handlers
6. Run `reconnect_workflow_preserves_seq_monotonicity` — still green

## Risks & Trade-offs

- **Diff size**: ~500 LOC touched across 10+ files. Easy to merge-conflict
  with parallel protocol work.
- **`Order` struct size**: Changing type alias to newtype may alter layout
  if `[u8;16]` has different alignment than `SessionId([u8;16])`. The
  compile-time assert catches this.
- **PRNG in `SessionId::random`**: `rand::rng()` is thread-local; no
  contention. No change from current code.
- **Debug format churn**: existing log output changes from raw bytes to
  hex-prefix. Downstream log parsers (if any) need updating.

## Out of Scope

- `OrderId` newtype — similar issue but orthogonal; separate RFC.
- `ServerFrameSeq` persistence across restarts (current behavior: reset
  on process start; tolerable because clients do not persist seqs across
  TCP reconnects).
- Removing `Order.client_session_id` from `pub` to enforce builder use —
  would break WAL codec; separate design discussion.

## References

- Original type-design-review: agent report `a99c0df` in
  `feat/protocol-v2-exactly-once` PR discussion
- Current ResumeSession fix: `crates/me-io/src/tcp_server.rs:334-375`
  using `SessionStore::next_outbound_seq()`
- Compile-time Order size assert: `crates/me-core/src/order.rs:87-93`
