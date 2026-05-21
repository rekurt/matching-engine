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
/// v2: encoded order layout gained a `flags` byte (see codec::ORDER_ENCODED_SIZE).
const SNAPSHOT_VERSION: u16 = 2;

/// Manager for saving and loading order book snapshots.
pub struct SnapshotManager {
    snapshot_dir: PathBuf,
}

impl SnapshotManager {
    pub fn new(snapshot_dir: impl AsRef<Path>) -> io::Result<Self> {
        let dir = snapshot_dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
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
        let mut file = File::create(&tmp_path)?;
        file.write_all(&data)?;
        file.sync_all()?;
        fs::rename(&tmp_path, &path)?;
        // Best-effort durability of the rename: fsync the containing directory.
        if let Ok(dir) = File::open(&self.snapshot_dir) {
            let _ = dir.sync_all();
        }

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
        File::open(path)?.read_to_end(&mut data)?;

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

        let mut offset = 0;

        // Magic
        let magic = u32::from_le_bytes(payload[offset..offset + 4].try_into().unwrap());
        offset += 4;
        if magic != SNAPSHOT_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid snapshot magic",
            ));
        }

        // Version
        let version = u16::from_le_bytes(payload[offset..offset + 2].try_into().unwrap());
        offset += 2;
        if version != SNAPSHOT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unsupported snapshot version: {version}"),
            ));
        }

        // Symbol
        let symbol = SymbolId(u16::from_le_bytes(
            payload[offset..offset + 2].try_into().unwrap(),
        ));
        offset += 2;

        // Sequence
        let sequence = u64::from_le_bytes(payload[offset..offset + 8].try_into().unwrap());
        offset += 8;

        // Trade ID gen
        let trade_id_gen = u64::from_le_bytes(payload[offset..offset + 8].try_into().unwrap());
        offset += 8;

        // WAL sequence watermark
        let wal_sequence = u64::from_le_bytes(payload[offset..offset + 8].try_into().unwrap());
        offset += 8;

        // Resting orders
        let resting_count =
            u32::from_le_bytes(payload[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        let mut resting_orders = Vec::with_capacity(resting_count);
        for _ in 0..resting_count {
            let order = codec::decode_order_from(&payload[offset..])
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            offset += codec::ORDER_ENCODED_SIZE;
            resting_orders.push(order);
        }

        // Stop orders
        let stop_count =
            u32::from_le_bytes(payload[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        let mut stop_orders = Vec::with_capacity(stop_count);
        for _ in 0..stop_count {
            let order = codec::decode_order_from(&payload[offset..])
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            offset += codec::ORDER_ENCODED_SIZE;
            stop_orders.push(order);
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
}
