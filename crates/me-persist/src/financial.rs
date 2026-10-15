//! Versioned financial journal. Legacy WAL and snapshot readers cannot open it.
use serde::{de::DeserializeOwned, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    marker::PhantomData,
    path::Path,
};
const MAGIC: &[u8; 8] = b"MEFIN001";
const MAX_RECORD: usize = 16 * 1024 * 1024;
pub struct FinancialJournal<T> {
    file: File,
    sequence: u64,
    failed: bool,
    marker: PhantomData<T>,
}
impl<T: Serialize + DeserializeOwned> FinancialJournal<T> {
    pub fn open(path: impl AsRef<Path>) -> io::Result<(Self, Vec<T>)> {
        let path = path.as_ref();
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.try_lock().map_err(io::Error::other)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        if file.metadata()?.len() == 0 {
            file.write_all(MAGIC)?;
            file.sync_all()?;
            File::open(
                path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            )?
            .sync_all()?;
        }
        file.seek(SeekFrom::Start(0))?;
        let mut magic = [0; 8];
        file.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::other("unsupported financial journal version"));
        }
        let size = file.metadata()?.len();
        let mut offset = 8u64;
        let mut sequence = 0u64;
        let mut records = Vec::new();
        while offset < size {
            if size - offset < 20 {
                break;
            }
            let mut header = [0; 20];
            file.read_exact(&mut header)?;
            let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
            let crc = u32::from_le_bytes(header[4..8].try_into().unwrap());
            let seq = u64::from_le_bytes(header[8..16].try_into().unwrap());
            if crc32fast::hash(&header[..16])
                != u32::from_le_bytes(header[16..20].try_into().unwrap())
            {
                return Err(io::Error::other("corrupt financial header checksum"));
            }
            if len > MAX_RECORD || len == 0 || sequence.checked_add(1) != Some(seq) {
                return Err(io::Error::other("corrupt financial record header"));
            }
            if size - offset - 20 < len as u64 {
                break;
            }
            let mut bytes = vec![0; len];
            file.read_exact(&mut bytes)?;
            let mut hash = crc32fast::Hasher::new();
            hash.update(&seq.to_le_bytes());
            hash.update(&bytes);
            if hash.finalize() != crc {
                return Err(io::Error::other("corrupt financial record checksum"));
            }
            records.push(serde_json::from_slice(&bytes).map_err(io::Error::other)?);
            sequence = seq;
            offset += 20 + len as u64;
        }
        // Only an incomplete final write is discarded; complete corrupt records fail closed.
        if offset < size {
            file.set_len(offset)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::End(0))?;
        Ok((
            Self {
                file,
                sequence,
                failed: false,
                marker: PhantomData,
            },
            records,
        ))
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn append(&mut self, record: &T) -> io::Result<u64> {
        if self.failed {
            return Err(io::Error::other(
                "financial writer failed; restart required",
            ));
        }
        let seq = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("financial sequence exhausted"))?;
        let bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
        if bytes.len() > MAX_RECORD {
            return Err(io::Error::other("financial record too large"));
        }
        let mut hash = crc32fast::Hasher::new();
        hash.update(&seq.to_le_bytes());
        hash.update(&bytes);
        let result = (|| {
            let mut header = Vec::with_capacity(20);
            header.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            header.extend_from_slice(&hash.finalize().to_le_bytes());
            header.extend_from_slice(&seq.to_le_bytes());
            let header_crc = crc32fast::hash(&header);
            header.extend_from_slice(&header_crc.to_le_bytes());
            self.file.write_all(&header)?;
            self.file.write_all(&bytes)?;
            self.file.sync_data()
        })();
        if result.is_err() {
            self.failed = true;
        }
        result?;
        self.sequence = seq;
        Ok(seq)
    }
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }
}
