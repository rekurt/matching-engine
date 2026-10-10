# RFC 0001: Typed session and server sequence identities

## Status

Implemented. This note records the design rationale; the Rust API documentation describes current methods.

## Problem

An untyped 16-byte session ID can be confused with other byte arrays, and a raw outbound sequence counter makes reserved sequence zero easy to assign accidentally. Repeated manual slicing in logs can also expose a full bearer credential. Session-ID collisions must not overwrite existing state.

## Decisions

- `SessionId` is a transparent newtype in `me-core`, re-exported by `me-io`. Core orders can store ownership without depending on the network crate.
- `SessionId::NONE` names the all-zero sentinel used before a connection has a session. Random session generation excludes that value.
- Explicit byte constructors/accessors support wire encoding. Debug formatting uses a short prefix, not the full credential.
- `ServerFrameSeq` wraps the outbound counter. It starts at one; reseeding rejects zero. Saturating advancement prevents integer wrap, while session admission/delivery must still handle sequence exhaustion.
- Session creation checks for collisions under the selected shard lock and retries with a fresh ID. It does not replace another session.

## Compatibility

The wire session ID remains 16 opaque bytes, and frame sequences remain little-endian `u64`. These types distinguish identities in code without changing the protocol layout. Sequence zero is reserved for out-of-band control; positive sequences belong to the per-session replay stream.

Explicit construction alone does not validate session admission or ownership. The session store and engine enforce those runtime checks. A session ID is a bearer credential, not a financial account identity.

## Validation

Regression tests cover the sentinel, byte round trips, prefix-only Debug output, nonzero sequence reseeding, sequence progression and collision-safe creation. Protocol tests verify the wire layout and resume behavior. These tests are retained in `crates/me-io/tests` and `crates/me-core`.
