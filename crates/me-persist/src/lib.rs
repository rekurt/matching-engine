//! # me-persist — Write-Ahead Log and Snapshots
//!
//! Provides crash-recovery persistence for the matching engine via:
//!
//! - **WAL** ([`wal::WalWriter`] / [`wal::WalReader`]) — Sequential binary log of all
//!   commands with CRC32 checksums per entry. Format: `[len:u32][crc:u32][seq:u64][data]`.
//! - **Snapshots** ([`snapshot`]) — Atomic point-in-time captures of the full order book
//!   state, written via temp-file + rename for crash safety.
//!
//! ## Recovery
//!
//! 1. Load the latest snapshot (if any)
//! 2. Replay WAL entries from the snapshot's sequence number forward
//! 3. Engine state is fully restored — deterministic replay guaranteed

pub mod codec;
pub mod snapshot;
pub mod wal;
