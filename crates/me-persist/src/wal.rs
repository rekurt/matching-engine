use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use me_core::order::Order;
use me_core::types::{FixedPrice, FixedQty, OrderId, SymbolId};

use crate::codec;

// ─── WAL Entry ───────────────────────────────────────────────────────────────

/// A single entry in the write-ahead log.
#[derive(Clone, Debug)]
pub struct WalEntry {
    pub sequence: u64,
    pub command: WalCommand,
}

/// Commands that can be written to the WAL.
/// Mirrors me_engine::command::Command but is self-contained for persistence.
#[derive(Clone, Debug)]
pub enum WalCommand {
    PlaceOrder(Order),
    CancelOrder {
        order_id: OrderId,
        symbol: SymbolId,
        timestamp: u64,
    },
    AmendOrder {
        order_id: OrderId,
        symbol: SymbolId,
        new_price: Option<FixedPrice>,
        new_qty: Option<FixedQty>,
        timestamp: u64,
    },
}

// ─── WAL Writer ──────────────────────────────────────────────────────────────

/// Write-ahead log writer. Appends commands to a file with CRC32 checksums.
///
/// File format per entry:
/// [len: u32][crc: u32][sequence: u64][command_bytes: [u8; len]]
pub struct WalWriter {
    writer: BufWriter<File>,
    sequence: u64,
    path: PathBuf,
    sync_interval: u64,
    writes_since_sync: u64,
}

impl WalWriter {
    /// Open or create a WAL file.
    pub fn open(path: impl AsRef<Path>, sync_interval: u64) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;

        Ok(Self {
            writer: BufWriter::new(file),
            sequence: 0,
            path,
            sync_interval,
            writes_since_sync: 0,
        })
    }

    /// Set the starting sequence number (e.g., after loading a snapshot).
    pub fn set_sequence(&mut self, seq: u64) {
        self.sequence = seq;
    }

    /// Append a command to the WAL.
    pub fn append(&mut self, command: &WalCommand) -> io::Result<u64> {
        self.sequence += 1;
        let seq = self.sequence;

        let cmd_bytes = codec::encode_wal_command(command);
        let crc = crc32fast::hash(&cmd_bytes);
        let len = cmd_bytes.len() as u32;

        self.writer.write_all(&len.to_le_bytes())?;
        self.writer.write_all(&crc.to_le_bytes())?;
        self.writer.write_all(&seq.to_le_bytes())?;
        self.writer.write_all(&cmd_bytes)?;

        self.writes_since_sync += 1;
        if self.writes_since_sync >= self.sync_interval {
            self.writer.flush()?;
            self.writes_since_sync = 0;
        }

        Ok(seq)
    }

    /// Flush and sync the WAL to disk.
    pub fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }

    /// Current sequence number.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Path to the WAL file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ─── WAL Reader ──────────────────────────────────────────────────────────────

/// Reads and replays WAL entries from a file.
pub struct WalReader {
    reader: BufReader<File>,
}

impl WalReader {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = File::open(path)?;
        Ok(Self {
            reader: BufReader::new(file),
        })
    }

    /// Read the next entry from the WAL.
    /// Returns None at EOF, Err on corruption.
    pub fn next_entry(&mut self) -> io::Result<Option<WalEntry>> {
        // Read len
        let mut len_buf = [0u8; 4];
        match self.reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let len = u32::from_le_bytes(len_buf) as usize;

        // Read CRC
        let mut crc_buf = [0u8; 4];
        self.reader.read_exact(&mut crc_buf)?;
        let stored_crc = u32::from_le_bytes(crc_buf);

        // Read sequence
        let mut seq_buf = [0u8; 8];
        self.reader.read_exact(&mut seq_buf)?;
        let sequence = u64::from_le_bytes(seq_buf);

        // Read command bytes
        let mut cmd_bytes = vec![0u8; len];
        self.reader.read_exact(&mut cmd_bytes)?;

        // Verify CRC
        let computed_crc = crc32fast::hash(&cmd_bytes);
        if computed_crc != stored_crc {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "WAL CRC mismatch at sequence {sequence}: stored={stored_crc}, computed={computed_crc}"
                ),
            ));
        }

        let command = codec::decode_wal_command(&cmd_bytes).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("WAL decode error: {e}"))
        })?;

        Ok(Some(WalEntry { sequence, command }))
    }

    /// Read all remaining entries.
    pub fn read_all(&mut self) -> io::Result<Vec<WalEntry>> {
        let mut entries = Vec::new();
        while let Some(entry) = self.next_entry()? {
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Read entries starting from a given sequence number.
    pub fn read_from(&mut self, from_sequence: u64) -> io::Result<Vec<WalEntry>> {
        let mut entries = Vec::new();
        while let Some(entry) = self.next_entry()? {
            if entry.sequence >= from_sequence {
                entries.push(entry);
            }
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::order::{Order, TimeInForce};
    use me_core::types::*;
    use std::fs;

    fn temp_wal_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("me_wal_tests");
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn test_write_and_read() {
        let path = temp_wal_path("test_write_read.wal");
        let _ = fs::remove_file(&path);

        {
            let mut writer = WalWriter::open(&path, 1).unwrap();
            let order = Order::new_limit(
                1,
                SymbolId(0),
                Side::Buy,
                TimeInForce::GTC,
                FixedPrice::from_f64(100.0),
                FixedQty::from_f64(1.0),
                1000,
            );
            writer
                .append(&WalCommand::PlaceOrder(order))
                .unwrap();
            writer
                .append(&WalCommand::CancelOrder {
                    order_id: 1,
                    symbol: SymbolId(0),
                    timestamp: 2000,
                })
                .unwrap();
            writer.flush().unwrap();
        }

        {
            let mut reader = WalReader::open(&path).unwrap();
            let entries = reader.read_all().unwrap();
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].sequence, 1);
            assert_eq!(entries[1].sequence, 2);
            match &entries[0].command {
                WalCommand::PlaceOrder(o) => assert_eq!(o.id, 1),
                _ => panic!("Expected PlaceOrder"),
            }
            match &entries[1].command {
                WalCommand::CancelOrder { order_id, .. } => assert_eq!(*order_id, 1),
                _ => panic!("Expected CancelOrder"),
            }
        }

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_empty_wal() {
        let path = temp_wal_path("test_empty.wal");
        let _ = fs::remove_file(&path);

        {
            let mut writer = WalWriter::open(&path, 1).unwrap();
            writer.flush().unwrap();
        }

        {
            let mut reader = WalReader::open(&path).unwrap();
            let entries = reader.read_all().unwrap();
            assert!(entries.is_empty());
        }

        let _ = fs::remove_file(&path);
    }
}
