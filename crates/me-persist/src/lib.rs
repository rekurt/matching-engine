#![forbid(unsafe_code)]
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
//! The router validates and replays the complete decision journal, comparing
//! recovered events with recorded outcomes. Snapshots are checkpoints; they do
//! not replace decision history or the session journal.

pub mod codec;
pub mod snapshot;
pub mod wal;

/// Persist newly created directory entries before publishing durable files inside them.
pub fn create_dir_all_durable(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let path = path.as_ref();
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut missing = Vec::new();
    for directory in absolute.ancestors() {
        if directory.is_dir() {
            break;
        }
        missing.push(directory.to_path_buf());
    }
    std::fs::create_dir_all(&absolute)?;
    for directory in missing {
        std::fs::File::open(&directory)?.sync_all()?;
        if let Some(parent) = directory.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
    }
    Ok(())
}
