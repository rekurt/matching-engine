use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use me_core::order::Order;
use me_core::types::SymbolId;

use crate::codec;

/// A snapshot of a single order book's state.
#[derive(Clone, Debug)]
pub struct BookSnapshot {
    pub symbol: SymbolId,
    pub sequence: u64,
    pub trade_id_gen: u64,
    /// Shard-wide WAL append sequence at the moment this snapshot was taken.
    /// On recovery, only WAL records with a strictly greater sequence are
    /// replayed for this symbol, making replay idempotent regardless of when
    /// the crash occurred relative to WAL truncation.
    pub wal_sequence: u64,
    pub resting_orders: Vec<Order>,
    pub stop_orders: Vec<Order>,
}

/// Snapshot file format:
/// [magic: u32]["MESN"]
/// [version: u16]
/// [symbol_id: u16]
/// [sequence: u64]
/// [trade_id_gen: u64]
/// [wal_sequence: u64]
/// [resting_count: u32][resting_orders...]
/// [stop_count: u32][stop_orders...]
/// [crc: u32] (of everything above)
const SNAPSHOT_MAGIC: u32 = 0x4D45534E; // "MESN"
/// v2: encoded order gained a `flags` byte.
/// v3: encoded order gained `expire_at`, `trail_offset`, `display_qty`
///     (GTD / trailing-stop / iceberg). Older v2 snapshots are rejected by
///     `load_file` rather than mis-decoded against the wider record layout.
const SNAPSHOT_VERSION: u16 = 3;

/// Manager for saving and loading order book snapshots.
pub struct SnapshotManager {
    snapshot_dir: PathBuf,
}

impl SnapshotManager {
    pub fn new(snapshot_dir: impl AsRef<Path>) -> io::Result<Self> {
        let dir = snapshot_dir.as_ref().to_path_buf();
        crate::create_dir_all_durable(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                if entry.file_type()?.is_file()
                    && entry.file_name().to_string_lossy().starts_with("snapshot_")
                {
                    fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o600))?;
                }
            }
        }
        Ok(Self { snapshot_dir: dir })
    }

    /// Save a snapshot for a symbol.
    pub fn save(&self, snapshot: &BookSnapshot) -> io::Result<PathBuf> {
        let filename = format!(
            "snapshot_{:04}_{:016}.bin",
            snapshot.symbol.0, snapshot.sequence
        );
        let path = self.snapshot_dir.join(&filename);

        let mut data = Vec::with_capacity(4096);

        // Header
        data.extend_from_slice(&SNAPSHOT_MAGIC.to_le_bytes());
        data.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
        data.extend_from_slice(&snapshot.symbol.0.to_le_bytes());
        data.extend_from_slice(&snapshot.sequence.to_le_bytes());
        data.extend_from_slice(&snapshot.trade_id_gen.to_le_bytes());
        data.extend_from_slice(&snapshot.wal_sequence.to_le_bytes());

        // Resting orders
        let resting_count = snapshot.resting_orders.len() as u32;
        data.extend_from_slice(&resting_count.to_le_bytes());
        for order in &snapshot.resting_orders {
            codec::encode_order_to(order, &mut data);
        }

        // Stop orders
        let stop_count = snapshot.stop_orders.len() as u32;
        data.extend_from_slice(&stop_count.to_le_bytes());
        for order in &snapshot.stop_orders {
            codec::encode_order_to(order, &mut data);
        }

        // CRC of everything
        let crc = crc32fast::hash(&data);
        data.extend_from_slice(&crc.to_le_bytes());

        // Write atomically: write to temp, fsync the temp file, rename, then
        // fsync the directory so the rename itself is durable. Without the
        // directory fsync a crash can leave the new name unpersisted while the
        // WAL has already been advanced/truncated against this snapshot.
        let tmp_path = path.with_extension("tmp");
        let mut options = fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&data)?;
        file.sync_all()?;
        fs::rename(&tmp_path, &path)?;
        File::open(&self.snapshot_dir)?.sync_all()?;

        Ok(path)
    }

    /// Load the latest snapshot for a symbol.
    pub fn load_latest(&self, symbol: SymbolId) -> io::Result<Option<BookSnapshot>> {
        let prefix = format!("snapshot_{:04}_", symbol.0);
        let mut snapshots: Vec<_> = fs::read_dir(&self.snapshot_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".bin"))
            })
            .collect();

        snapshots.sort_by_key(|b| std::cmp::Reverse(b.file_name()));

        match snapshots.first() {
            Some(entry) => {
                let snapshot = self.load_file(entry.path())?;
                Ok(Some(snapshot))
            }
            None => Ok(None),
        }
    }

    /// Load a snapshot from a specific file.
    pub fn load_file(&self, path: impl AsRef<Path>) -> io::Result<BookSnapshot> {
        let mut data = Vec::new();
        let mut file = File::open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.read_to_end(&mut data)?;

        // Verify CRC (last 4 bytes)
        if data.len() < 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Snapshot too short",
            ));
        }
        let payload = &data[..data.len() - 4];
        let stored_crc = u32::from_le_bytes(data[data.len() - 4..].try_into().unwrap());
        let computed_crc = crc32fast::hash(payload);
        if stored_crc != computed_crc {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Snapshot CRC mismatch",
            ));
        }

        // Sanity cap on the declared order counts. A CRC pass does not prove
        // the payload is well-formed (disk bit-rot or an external overwrite can
        // produce a CRC-valid-but-corrupt file), so a wild `resting_count` must
        // not drive a multi-GB `Vec::with_capacity` OOM before the read loop
        // even runs. 10M orders/book is already far beyond any real deployment.
        const MAX_SNAPSHOT_ORDERS: usize = 10_000_000;

        let mut offset = 0usize;

        // Bounds-checked fixed-width readers over `payload`. Every field read
        // goes through these so a truncated/corrupt payload returns InvalidData
        // instead of panicking on an out-of-range slice index in the shard
        // worker during startup recovery.
        let truncated = || io::Error::new(io::ErrorKind::InvalidData, "Snapshot truncated");
        let read_u16 = |off: usize| -> io::Result<u16> {
            payload
                .get(off..off + 2)
                .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
                .ok_or_else(truncated)
        };
        let read_u32 = |off: usize| -> io::Result<u32> {
            payload
                .get(off..off + 4)
                .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
                .ok_or_else(truncated)
        };
        let read_u64 = |off: usize| -> io::Result<u64> {
            payload
                .get(off..off + 8)
                .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
                .ok_or_else(truncated)
        };

        // Magic
        let magic = read_u32(offset)?;
        offset += 4;
        if magic != SNAPSHOT_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid snapshot magic",
            ));
        }

        // Version
        let version = read_u16(offset)?;
        offset += 2;
        if version != SNAPSHOT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unsupported snapshot version: {version}"),
            ));
        }

        // Symbol
        let symbol = SymbolId(read_u16(offset)?);
        offset += 2;

        // Sequence
        let sequence = read_u64(offset)?;
        offset += 8;

        // Trade ID gen
        let trade_id_gen = read_u64(offset)?;
        offset += 8;

        // WAL sequence watermark
        let wal_sequence = read_u64(offset)?;
        offset += 8;

        // Decode `count` consecutive fixed-size orders starting at `offset`,
        // validating that the payload actually holds that many bytes.
        let decode_orders =
            |offset: &mut usize, count: usize| -> io::Result<Vec<me_core::order::Order>> {
                if count > MAX_SNAPSHOT_ORDERS {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Snapshot order count {count} exceeds cap {MAX_SNAPSHOT_ORDERS}"),
                    ));
                }
                if count > payload.len().saturating_sub(*offset) / codec::ORDER_ENCODED_SIZE {
                    return Err(truncated());
                }
                let mut orders = Vec::with_capacity(count);
                for _ in 0..count {
                    if payload.len().saturating_sub(*offset) < codec::ORDER_ENCODED_SIZE {
                        return Err(truncated());
                    }
                    let order = codec::decode_order_from(&payload[*offset..])
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    *offset += codec::ORDER_ENCODED_SIZE;
                    orders.push(order);
                }
                Ok(orders)
            };

        // Resting orders
        let resting_count = read_u32(offset)? as usize;
        offset += 4;
        let resting_orders = decode_orders(&mut offset, resting_count)?;

        // Stop orders
        let stop_count = read_u32(offset)? as usize;
        offset += 4;
        let stop_orders = decode_orders(&mut offset, stop_count)?;

        if offset != payload.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot trailing bytes",
            ));
        }
        Ok(BookSnapshot {
            symbol,
            sequence,
            trade_id_gen,
            wal_sequence,
            resting_orders,
            stop_orders,
        })
    }

    /// Clean up old snapshots, keeping only the N most recent for each symbol.
    pub fn cleanup(&self, symbol: SymbolId, keep: usize) -> io::Result<()> {
        let prefix = format!("snapshot_{:04}_", symbol.0);
        let mut snapshots: Vec<_> = fs::read_dir(&self.snapshot_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".bin"))
            })
            .collect();

        snapshots.sort_by_key(|b| std::cmp::Reverse(b.file_name()));

        for old in snapshots.iter().skip(keep) {
            fs::remove_file(old.path())?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::order::{Order, TimeInForce};
    use me_core::types::*;
    use std::fs;

    fn temp_snapshot_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("me_snapshot_tests").join(name);
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn test_save_and_load() {
        let dir = temp_snapshot_dir("save_load");
        let mgr = SnapshotManager::new(&dir).unwrap();

        let order1 = Order::new_limit(
            1,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(100.0),
            FixedQty::from_f64(1.0),
            1000,
        );
        let order2 = Order::new_stop_limit(
            2,
            SymbolId(0),
            Side::Sell,
            TimeInForce::GTC,
            FixedPrice::from_f64(95.0),
            FixedPrice::from_f64(90.0),
            FixedQty::from_f64(2.0),
            2000,
        );

        let snapshot = BookSnapshot {
            symbol: SymbolId(0),
            sequence: 100,
            trade_id_gen: 50,
            wal_sequence: 4242,
            resting_orders: vec![order1],
            stop_orders: vec![order2],
        };

        let path = mgr.save(&snapshot).unwrap();
        assert!(path.exists());

        let loaded = mgr.load_latest(SymbolId(0)).unwrap().unwrap();
        assert_eq!(loaded.symbol, SymbolId(0));
        assert_eq!(loaded.sequence, 100);
        assert_eq!(loaded.trade_id_gen, 50);
        assert_eq!(loaded.wal_sequence, 4242);
        assert_eq!(loaded.resting_orders.len(), 1);
        assert_eq!(loaded.resting_orders[0].id, 1);
        assert_eq!(loaded.stop_orders.len(), 1);
        assert_eq!(loaded.stop_orders[0].id, 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_empty_snapshot() {
        let dir = temp_snapshot_dir("empty");
        let mgr = SnapshotManager::new(&dir).unwrap();

        let snapshot = BookSnapshot {
            symbol: SymbolId(0),
            sequence: 0,
            trade_id_gen: 0,
            wal_sequence: 0,
            resting_orders: vec![],
            stop_orders: vec![],
        };

        mgr.save(&snapshot).unwrap();

        let loaded = mgr.load_latest(SymbolId(0)).unwrap().unwrap();
        assert!(loaded.resting_orders.is_empty());
        assert!(loaded.stop_orders.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_no_snapshot_returns_none() {
        let dir = temp_snapshot_dir("none");
        let mgr = SnapshotManager::new(&dir).unwrap();

        let result = mgr.load_latest(SymbolId(0)).unwrap();
        assert!(result.is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    /// Append a valid CRC over `payload` and write `payload || crc` to a file.
    fn write_with_crc(dir: &PathBuf, name: &str, payload: &[u8]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let crc = crc32fast::hash(payload);
        let mut data = payload.to_vec();
        data.extend_from_slice(&crc.to_le_bytes());
        let path = dir.join(name);
        fs::write(&path, &data).unwrap();
        path
    }

    #[test]
    fn load_file_truncated_payload_errors_not_panics() {
        // A CRC-valid file whose payload ends right after the magic must return
        // InvalidData when the loader reaches the next (missing) field — not
        // panic on an out-of-range slice in the recovery path.
        let dir = temp_snapshot_dir("truncated");
        let payload = SNAPSHOT_MAGIC.to_le_bytes().to_vec(); // magic only
        let path = write_with_crc(&dir, "snapshot_0000_truncated.bin", &payload);

        let mgr = SnapshotManager::new(&dir).unwrap();
        let r = mgr.load_file(&path);
        assert!(r.is_err(), "truncated snapshot must error, got {r:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_absurd_order_count_errors_not_ooms() {
        // A CRC-valid header declaring u32::MAX resting orders must be rejected
        // by the order-count cap BEFORE any multi-GB Vec::with_capacity.
        let dir = temp_snapshot_dir("absurd_count");
        let mut payload = Vec::new();
        payload.extend_from_slice(&SNAPSHOT_MAGIC.to_le_bytes());
        payload.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes()); // symbol
        payload.extend_from_slice(&0u64.to_le_bytes()); // sequence
        payload.extend_from_slice(&0u64.to_le_bytes()); // trade_id_gen
        payload.extend_from_slice(&0u64.to_le_bytes()); // wal_sequence
        payload.extend_from_slice(&u32::MAX.to_le_bytes()); // resting_count
        let path = write_with_crc(&dir, "snapshot_0000_absurd.bin", &payload);

        let mgr = SnapshotManager::new(&dir).unwrap();
        let r = mgr.load_file(&path);
        assert!(r.is_err(), "absurd order count must error, got {r:?}");

        let _ = fs::remove_dir_all(&dir);
    }
}
