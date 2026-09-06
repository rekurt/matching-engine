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
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum WalCommand {
    Decision {
        command: Box<WalCommand>,
        session_id: me_core::SessionId,
        request_id: Option<u64>,
        item_index: u32,
        events: Vec<me_core::events::MatchEvent>,
        expirations: Vec<me_core::events::MatchEvent>,
        expiration_owners: Vec<(OrderId, me_core::SessionId)>,
        owners: Vec<(OrderId, me_core::SessionId)>,
        processing_timestamp: u64,
        apply: bool,
    },
    TradingState {
        symbol: SymbolId,
        halted: bool,
    },
    SymbolConfig {
        symbol: SymbolId,
        fingerprint: String,
    },
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

impl WalCommand {
    /// Symbol this command targets — used to route replayed records to the
    /// correct engine during recovery.
    pub fn symbol(&self) -> SymbolId {
        match self {
            WalCommand::Decision { command, .. } => command.symbol(),
            WalCommand::TradingState { symbol, .. } | WalCommand::SymbolConfig { symbol, .. } => {
                *symbol
            }
            WalCommand::PlaceOrder(order) => order.symbol,
            WalCommand::CancelOrder { symbol, .. } => *symbol,
            WalCommand::AmendOrder { symbol, .. } => *symbol,
        }
    }
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
    failed: bool,
}

impl WalWriter {
    /// Open or create a WAL file.
    pub fn open(path: impl AsRef<Path>, sync_interval: u64) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        file.try_lock().map_err(io::Error::other)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.sync_all()?;
        File::open(
            path.parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?
        .sync_all()?;

        let sequence = WalReader::open(&path)?
            .read_all()?
            .last()
            .map_or(0, |entry| entry.sequence);
        Ok(Self {
            writer: BufWriter::new(file),
            sequence,
            path,
            sync_interval,
            writes_since_sync: 0,
            failed: false,
        })
    }

    /// Set the starting sequence number (e.g., after loading a snapshot).
    pub fn set_sequence(&mut self, seq: u64) {
        self.sequence = seq;
    }

    fn ensure_healthy(&self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other(
                "WAL writer failed; reopen and recover before writing",
            ));
        }
        Ok(())
    }

    /// Append, synchronizing at the configured interval.
    pub fn append(&mut self, command: &WalCommand) -> io::Result<u64> {
        let seq = self.append_nosync(command)?;
        if self.sync_interval == 0 || self.writes_since_sync >= self.sync_interval {
            self.sync()?;
        }
        Ok(seq)
    }

    /// Buffer a record. Call `sync` before acknowledging its outcome.
    pub fn append_nosync(&mut self, command: &WalCommand) -> io::Result<u64> {
        self.ensure_healthy()?;
        let seq = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("WAL sequence exhausted"))?;
        let cmd_bytes = codec::encode_wal_command(command);
        if cmd_bytes.len() > MAX_WAL_RECORD_LEN {
            return Err(io::Error::other("WAL record too large"));
        }
        let mut checksum = crc32fast::Hasher::new();
        checksum.update(&seq.to_le_bytes());
        checksum.update(&cmd_bytes);
        let crc = checksum.finalize();
        let len = (cmd_bytes.len() as u32) | 0x8000_0000;
        let result = (|| {
            self.writer.write_all(&len.to_le_bytes())?;
            self.writer.write_all(&crc.to_le_bytes())?;
            self.writer.write_all(&seq.to_le_bytes())?;
            self.writer.write_all(&cmd_bytes)
        })();
        if result.is_err() {
            self.failed = true;
        }
        result?;
        self.sequence = seq;
        self.writes_since_sync = self.writes_since_sync.saturating_add(1);
        Ok(seq)
    }

    /// Flush the buffer AND fsync to physical storage.
    ///
    /// `BufWriter::flush` alone only pushes bytes into the OS page cache; a
    /// power loss before the kernel writes back loses "journaled" records.
    /// `sync_data` forces the data (not necessarily metadata) to disk, which
    /// is what the Pre-mode durability contract actually requires.
    pub fn sync(&mut self) -> io::Result<()> {
        self.ensure_healthy()?;
        let result = self
            .writer
            .flush()
            .and_then(|_| self.writer.get_ref().sync_data());
        if result.is_err() {
            self.failed = true;
        }
        result?;
        self.writes_since_sync = 0;
        Ok(())
    }

    /// Flush and sync the WAL to disk. Durable: kept as the shutdown path.
    pub fn flush(&mut self) -> io::Result<()> {
        self.sync()
    }

    /// Low-level truncation for callers with their own complete recovery protocol.
    /// Keeps the writer sequence counter. The server never calls this: truncating
    /// its authoritative decision journal invalidates recovery and request history.
    pub fn rotate_truncate(&mut self) -> io::Result<()> {
        self.ensure_healthy()?;
        let result = (|| {
            self.writer.flush()?;
            self.writer.get_ref().set_len(0)?;
            self.writer.get_ref().sync_all()
        })();
        if result.is_err() {
            self.failed = true;
        }
        result?;
        self.writes_since_sync = 0;
        Ok(())
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

/// Hard cap on a single WAL record's payload length. A larger declared
/// length means a torn write or corruption, NOT a real record — refuse to
/// allocate gigabytes from a garbage `len` field.
pub const MAX_WAL_RECORD_LEN: usize = 16 * 1024 * 1024;

/// Per-record framing overhead: len(u32) + crc(u32) + sequence(u64).
const RECORD_HEADER_LEN: u64 = 4 + 4 + 8;

/// Outcome of a crash-tolerant WAL scan.
#[derive(Debug)]
pub struct RecoveryResult {
    /// Entries successfully read before any torn/corrupt tail.
    pub entries: Vec<WalEntry>,
    /// Byte offset just past the last fully-valid record. The caller should
    /// truncate the WAL file to this length before reopening it for append,
    /// so new records are not written after a corrupt gap.
    pub valid_bytes: u64,
    /// `Some(reason)` if the scan stopped early on a torn/corrupt tail.
    pub truncated: Option<String>,
}

/// Reads and replays WAL entries from a file.
pub struct WalReader {
    reader: BufReader<File>,
    /// Byte offset of the next record to read (== bytes consumed so far).
    offset: u64,
}

impl WalReader {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = File::open(path)?;
        Ok(Self {
            reader: BufReader::new(file),
            offset: 0,
        })
    }

    /// Read the next entry from the WAL.
    /// Returns None at clean EOF, Err on corruption or a torn tail record.
    pub fn next_entry(&mut self) -> io::Result<Option<WalEntry>> {
        // Read len
        let mut len_buf = [0u8; 4];
        if self.reader.read(&mut len_buf[..1])? == 0 {
            return Ok(None);
        }
        self.reader.read_exact(&mut len_buf[1..])?;
        let raw_len = u32::from_le_bytes(len_buf);
        let protected_sequence = raw_len & 0x8000_0000 != 0;
        let len = (raw_len & 0x7fff_ffff) as usize;

        // Reject implausible lengths BEFORE allocating.
        if len > MAX_WAL_RECORD_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("WAL record len {len} exceeds cap {MAX_WAL_RECORD_LEN} (corrupt/torn)"),
            ));
        }

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
        let mut checksum = crc32fast::Hasher::new();
        if protected_sequence {
            checksum.update(&seq_buf);
        }
        checksum.update(&cmd_bytes);
        let computed_crc = checksum.finalize();
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

        self.offset += RECORD_HEADER_LEN + len as u64;
        Ok(Some(WalEntry { sequence, command }))
    }

    /// Read all remaining entries (strict: errors on any corruption).
    pub fn read_all(&mut self) -> io::Result<Vec<WalEntry>> {
        let mut entries = Vec::new();
        while let Some(entry) = self.next_entry()? {
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Read entries starting AFTER a given sequence number (exclusive).
    ///
    /// `from_sequence` is the last sequence already captured by a snapshot,
    /// so it must NOT be replayed again — applying it would double-apply the
    /// command baked into the snapshot.
    pub fn read_from(&mut self, from_sequence: u64) -> io::Result<Vec<WalEntry>> {
        let mut entries = Vec::new();
        while let Some(entry) = self.next_entry()? {
            if entry.sequence > from_sequence {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    /// Crash-tolerant scan: read every fully-valid record, stopping cleanly at
    /// the first torn or corrupt record (the expected state of the last record
    /// after a crash mid-append). Returns the good entries plus the byte offset
    /// the file should be truncated to.
    pub fn read_recoverable(&mut self) -> RecoveryResult {
        let mut entries = Vec::new();
        loop {
            match self.next_entry() {
                Ok(Some(entry)) => entries.push(entry),
                Ok(None) => {
                    return RecoveryResult {
                        entries,
                        valid_bytes: self.offset,
                        truncated: None,
                    }
                }
                Err(e) => {
                    return RecoveryResult {
                        entries,
                        valid_bytes: self.offset,
                        truncated: Some(e.to_string()),
                    }
                }
            }
        }
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
            writer.append(&WalCommand::PlaceOrder(order)).unwrap();
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

    fn write_two_records(path: &Path) -> u64 {
        let mut writer = WalWriter::open(path, 1).unwrap();
        let order = Order::new_limit(
            1,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(100.0),
            FixedQty::from_f64(1.0),
            1000,
        );
        writer.append(&WalCommand::PlaceOrder(order)).unwrap();
        writer
            .append(&WalCommand::CancelOrder {
                order_id: 1,
                symbol: SymbolId(0),
                timestamp: 2000,
            })
            .unwrap();
        writer.flush().unwrap();
        fs::metadata(path).unwrap().len()
    }

    #[test]
    fn test_recoverable_clean_tail() {
        let path = temp_wal_path("test_recover_clean.wal");
        let _ = fs::remove_file(&path);
        let size = write_two_records(&path);

        let mut reader = WalReader::open(&path).unwrap();
        let result = reader.read_recoverable();
        assert_eq!(result.entries.len(), 2);
        assert!(result.truncated.is_none());
        assert_eq!(result.valid_bytes, size, "clean scan consumes whole file");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_recoverable_torn_tail() {
        use std::io::Write as _;
        let path = temp_wal_path("test_recover_torn.wal");
        let _ = fs::remove_file(&path);
        let good_size = write_two_records(&path);

        // Simulate a crash mid-append: a len header with no body.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&1000u32.to_le_bytes()).unwrap(); // claims 1000 bytes, none follow
        f.flush().unwrap();

        let mut reader = WalReader::open(&path).unwrap();
        let result = reader.read_recoverable();
        assert_eq!(result.entries.len(), 2, "good records recovered");
        assert!(result.truncated.is_some(), "torn tail reported");
        assert_eq!(
            result.valid_bytes, good_size,
            "truncation point is end of last good record"
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_recoverable_oversized_len_does_not_allocate() {
        use std::io::Write as _;
        let path = temp_wal_path("test_recover_huge.wal");
        let _ = fs::remove_file(&path);
        let good_size = write_two_records(&path);

        // A garbage len of u32::MAX must be rejected by the cap, not allocated.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&u32::MAX.to_le_bytes()).unwrap();
        f.flush().unwrap();

        let mut reader = WalReader::open(&path).unwrap();
        let result = reader.read_recoverable();
        assert_eq!(result.entries.len(), 2);
        assert!(result.truncated.is_some());
        assert_eq!(result.valid_bytes, good_size);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn sequence_corruption_is_detected() {
        let path = temp_wal_path("sequence_corruption.wal");
        let _ = fs::remove_file(&path);
        write_two_records(&path);
        let mut bytes = fs::read(&path).unwrap();
        bytes[8] ^= 1;
        fs::write(&path, bytes).unwrap();
        assert!(WalReader::open(&path).unwrap().read_all().is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn partial_length_header_is_not_clean_eof() {
        for length in 1..=3 {
            let path = temp_wal_path(&format!("partial_header_{length}.wal"));
            fs::write(&path, vec![0u8; length]).unwrap();
            assert!(WalReader::open(&path).unwrap().read_all().is_err());
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn bare_relative_wal_path_is_supported() {
        let path = PathBuf::from(format!(
            "me-relative-{}-{}.wal",
            std::process::id(),
            me_core::SessionId::random().log_prefix()
        ));
        let result = WalWriter::open(&path, 1);
        let _ = fs::remove_file(&path);
        assert!(
            result.is_ok(),
            "relative WAL path failed: {:?}",
            result.err()
        );
    }

    #[test]
    fn oversized_record_does_not_advance_sequence() {
        let path = temp_wal_path("oversized_sequence.wal");
        let _ = fs::remove_file(&path);
        let mut writer = WalWriter::open(&path, 1).unwrap();
        let oversized = WalCommand::SymbolConfig {
            symbol: SymbolId(0),
            fingerprint: "x".repeat(MAX_WAL_RECORD_LEN),
        };
        assert!(writer.append_nosync(&oversized).is_err());
        assert_eq!(writer.sequence(), 0);
        assert_eq!(
            writer
                .append(&WalCommand::TradingState {
                    symbol: SymbolId(0),
                    halted: true
                })
                .unwrap(),
            1
        );
        drop(writer);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn failed_sync_poison_prevents_further_writes() {
        let path = temp_wal_path("failed_sync_poison.wal");
        let _ = fs::remove_file(&path);
        let mut writer = WalWriter::open(&path, 100).unwrap();
        writer.writer = BufWriter::new(File::open(&path).unwrap());
        let command = WalCommand::TradingState {
            symbol: SymbolId(0),
            halted: true,
        };
        writer.append_nosync(&command).unwrap();
        assert!(writer.sync().is_err());
        assert!(
            writer.append_nosync(&command).is_err(),
            "writer accepted new bytes after failed fsync"
        );
        drop(writer);
        fs::remove_file(path).unwrap();
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
