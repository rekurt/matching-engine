//! Session identifier — 128-bit opaque token used across the protocol stack.
//!
//! Originally a `[u8; 16]` type alias in `me-io`, promoted to `me-core` so
//! `Order` can embed it without a reverse crate dependency. See RFC-0001
//! (`docs/rfcs/0001-type-safety-newtypes.md`) for the design rationale.
//!
//! Enforces by construction:
//! - `NONE` sentinel ("no session bound") has a name, not magic bytes
//! - random generation goes through a single `random()` constructor
//! - logs use `log_prefix()` instead of ad-hoc `&id[..4]` slicing
//! - compile-time distinction from any other `[u8; 16]` (e.g. symbol bytes)

use rand::Rng;
use serde::{Deserialize, Serialize};

/// 128-bit session identifier.
///
/// `#[repr(transparent)]` guarantees identical ABI/layout to the inner
/// `[u8; 16]` — safe to reinterpret when reading/writing wire frames.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct SessionId([u8; 16]);

impl SessionId {
    /// Sentinel meaning "no session bound" (all-zero bytes). Used for:
    /// - fresh `Order`s before codec injects the real session_id
    /// - `ConnectionCodecState::new()` before handshake completes
    pub const NONE: Self = Self([0u8; 16]);

    /// Generate a cryptographically-random session id from the thread-local
    /// RNG.
    ///
    /// Never returns [`SessionId::NONE`]. Even though hitting all-zero bytes
    /// is astronomically unlikely (2^-128), reserving `NONE` as a strict
    /// sentinel keeps invariants crisp across logs, replay, and snapshots.
    pub fn random() -> Self {
        loop {
            let mut bytes = [0u8; 16];
            rand::rng().fill(&mut bytes);
            let id = Self(bytes);
            if !id.is_none() {
                return id;
            }
        }
    }

    /// Construct from a raw 16-byte array (wire decode, test fixtures).
    #[inline]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Is this the "no session" sentinel?
    #[inline]
    pub fn is_none(&self) -> bool {
        self.0 == [0u8; 16]
    }

    /// Borrow the underlying 16 bytes (e.g. for codec writes).
    #[inline]
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// 8-character lowercase hex of the first 4 bytes — log-safe short
    /// identifier. 32 bits is enough to correlate across logs without
    /// printing the full 128-bit id.
    pub fn log_prefix(&self) -> String {
        format!(
            "{:02x}{:02x}{:02x}{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3]
        )
    }
}

impl std::fmt::Debug for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SessionId({})", self.log_prefix())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::NONE
    }
}
