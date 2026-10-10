//! Session ownership, retained request outcomes and bounded replay buffers.
//!
//! Mutation retries reuse their original outcomes; query responses can be
//! evicted from a bounded cache. Cumulative ACKs retire replay frames, not the
//! durable mutation history. Persistence is enabled by the server's WAL config.
//!
//! A fixed set of 64 locks distributes session access independently of engine
//! shard topology and host CPU count.

use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use rustc_hash::FxHashMap;

// Re-export from me-core so existing me-io callers keep working.
pub use me_core::SessionId;

/// Monotonic outbound-frame sequence counter for a connection.
///
/// Encodes the "seq=0 is the 'unsent' sentinel" invariant at the type
/// level: `FIRST` is 1, `next()` only increments, and `reset_to()` is
/// the only way to externally rewrite the counter (used after ResumeSession
/// to continue from `SessionStore::next_outbound_seq()`).
///
/// See `docs/rfcs/0001-type-safety-newtypes.md` for the sequence invariant.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ServerFrameSeq(u64);

impl ServerFrameSeq {
    /// The first seq to assign (seq=0 is reserved as "unsent").
    pub const FIRST: Self = Self(1);

    /// Return the current seq and advance to +1. This is the
    /// per-frame assignment primitive. Named `next` by analogy with
    /// Iterator, but ServerFrameSeq deliberately does NOT implement
    /// Iterator — it's a mutable counter, not a stream.
    #[inline]
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u64 {
        let s = self.0;
        self.0 = self.0.saturating_add(1);
        s
    }

    /// Advance the counter by `n` after a batch push of `n` frames.
    #[inline]
    pub fn advance_by(&mut self, n: u64) {
        self.0 = self.0.saturating_add(n);
    }

    /// Peek at the next-to-assign seq without consuming it.
    #[inline]
    pub fn peek(&self) -> u64 {
        self.0
    }

    /// Re-seed from `SessionStore::next_outbound_seq()` after ResumeSession.
    /// This is the ONLY way to rewrite the counter from outside.
    #[inline]
    pub fn reset_to(&mut self, next_seq: u64) {
        assert!(
            next_seq > 0,
            "ServerFrameSeq::reset_to(0) violates seq>=1 invariant"
        );
        self.0 = next_seq;
    }
}

impl Default for ServerFrameSeq {
    fn default() -> Self {
        Self::FIRST
    }
}

/// Number of lock shards. Must be a power of 2 so we can use bit-masking for
/// shard selection. 64 shards gives us good contention reduction without
/// excessive overhead for small deployments.
const NUM_SHARDS: usize = 64;
const SHARD_MASK: usize = NUM_SHARDS - 1;

/// Default maximum number of dedup entries per session.
const DEFAULT_DEDUP_MAX: usize = 10_000;
const MAX_QUERY_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const MAX_JOURNAL_RECORD_BYTES: usize = 64 * 1024 * 1024;

/// Soft limit threshold for FlowControlWarning (80% of buffer_max).
const SOFT_LIMIT_PCT: usize = 80;

/// Adaptive buffer default minimum (initial capacity for new sessions).
const DEFAULT_BUFFER_MIN: usize = 1024;

/// Adaptive buffer default maximum (hard ceiling above which eviction starts).
#[allow(dead_code)]
pub const DEFAULT_BUFFER_MAX: usize = 65536;

/// Result of a resume() operation.
///
/// `Ok(frames)` means replay succeeded; frames may be empty if client is
/// already caught up.
///
/// `Gap` means the client requested a seq that is no longer buffered — either
/// hard-limit-evicted (slow consumer) OR ACK-purged (see H1 fix: gap boundary
/// = max(highest_evicted_seq, last_acked_server_seq)). The client MUST request
/// a full state snapshot via `GetOpenOrders` to resync. Range is inclusive on
/// both ends: `[missing_from..=missing_to]`.
#[derive(Debug, PartialEq)]
pub enum ReplayResult {
    Ok(Vec<Bytes>),
    Gap {
        /// First seq the client needs but that is no longer replayable
        /// (inclusive, i.e. `last_received + 1`).
        missing_from: u64,
        /// Highest seq that is no longer replayable: `max(highest_evicted_seq,
        /// last_acked_server_seq)`. NOT the same as "first in buffer - 1"
        /// when the gap is caused by an ACK purge (the watermark can outrun
        /// what's still in the buffer).
        missing_to: u64,
    },
    /// Session not found or expired.
    SessionNotFound,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionEntry {
    /// Ring of (server_seq, raw_frame_bytes) for replay on reconnect.
    frames: VecDeque<(u64, Bytes)>,

    /// Current replay buffer capacity (adaptive between min..max).
    capacity: usize,

    /// Upper bound for adaptive buffer growth.
    buffer_max: usize,

    /// Highest server_seq ever evicted from this buffer (0 = nothing evicted).
    /// Used for gap detection: if client requests seq <= highest_evicted_seq,
    /// the frames are gone and client must resync.
    highest_evicted_seq: u64,

    /// True when consumer has fallen behind and buffer hit hard limit.
    /// Set by push() when eviction happens at buffer_max; cleared by apply_ack().
    slow_consumer: bool,

    /// Whether a soft-limit FlowControlWarning has been sent since last ACK.
    /// Prevents spam: only one warning per buffer fill cycle.
    soft_warning_sent: bool,

    /// Cumulative ACK: client has processed all frames with seq <= this value.
    /// Used by `apply_ack()` to purge old entries.
    last_acked_server_seq: u64,

    /// Dedup table: request_id → cached response frame bytes.
    /// Populated on first execution, consulted on retries.
    dedup: FxHashMap<u64, Bytes>,
    #[serde(default)]
    request_payloads: FxHashMap<u64, Vec<u8>>,
    #[serde(default)]
    request_bytes: usize,
    #[serde(default)]
    placement_units: usize,
    #[serde(default)]
    cancel_units: usize,
    #[serde(default)]
    query_payloads: FxHashMap<u64, Vec<u8>>,
    #[serde(default)]
    query_order: VecDeque<u64>,
    #[serde(default)]
    query_response_bytes: usize,
    #[serde(default)]
    batch_intents: FxHashMap<u64, Vec<me_engine::command::Command>>,
    #[serde(default)]
    batch_results: FxHashMap<u64, FxHashMap<u32, Bytes>>,

    /// Max dedup entries — when exceeded, oldest entries are evicted FIFO.
    dedup_max: usize,
    /// FIFO queue of (request_id) for dedup eviction ordering.
    dedup_order: VecDeque<u64>,

    /// Wall-clock of last session activity; used for TTL expiration.
    #[serde(skip, default = "Instant::now")]
    last_access: Instant,

    /// Session idle timeout.
    ttl: Duration,
}

impl SessionEntry {
    fn new(buffer_min: usize, buffer_max: usize, dedup_max: usize, ttl: Duration) -> Self {
        Self {
            frames: VecDeque::with_capacity(buffer_min.min(256)),
            capacity: buffer_min,
            buffer_max,
            highest_evicted_seq: 0,
            slow_consumer: false,
            soft_warning_sent: false,
            last_acked_server_seq: 0,
            dedup: FxHashMap::default(),
            request_payloads: FxHashMap::default(),
            request_bytes: 0,
            placement_units: 0,
            cancel_units: 0,
            query_payloads: FxHashMap::default(),
            query_order: VecDeque::new(),
            query_response_bytes: 0,
            batch_intents: FxHashMap::default(),
            batch_results: FxHashMap::default(),
            dedup_max,
            dedup_order: VecDeque::with_capacity(dedup_max.min(256)),
            last_access: Instant::now(),
            ttl,
        }
    }

    /// Push a frame into the replay buffer with adaptive growth.
    ///
    /// Growth policy:
    /// - If buffer is full and can grow (capacity < buffer_max): double capacity
    /// - If at buffer_max: evict oldest (hard limit reached → slow consumer)
    fn push_frame(&mut self, seq: u64, frame: Bytes) {
        if self.frames.len() >= self.capacity {
            if self.capacity < self.buffer_max {
                // Adaptive growth: double capacity up to buffer_max
                self.capacity = (self.capacity * 2).min(self.buffer_max);
            } else {
                // At hard limit — evict oldest and mark as slow consumer
                if let Some((evicted_seq, _)) = self.frames.pop_front() {
                    self.highest_evicted_seq = evicted_seq;
                }
                self.slow_consumer = true;
            }
        }
        self.frames.push_back((seq, frame));
    }

    fn is_expired(&self) -> bool {
        self.last_access.elapsed() > self.ttl
    }

    /// Check replay buffer state for a given client request.
    ///
    /// Returns:
    /// - `Ok(frames)` if all frames with seq > last_received are still buffered
    /// - `Gap { missing_from, missing_to }` if some/all requested frames are
    ///   no longer replayable — either hard-limit-evicted OR ACK-purged
    ///   (gap_boundary = max(highest_evicted_seq, last_acked_server_seq))
    fn check_replay(&self, last_received: u64) -> ReplayResult {
        // Gap boundary = the highest seq that was REMOVED from the buffer,
        // regardless of cause (hard-limit eviction OR ACK-driven purge).
        //
        // A well-behaved client shouldn't request frames it already ACKed,
        // but a client that crashes BEFORE sending an ACK may reconnect
        // with a last_recv_seq lower than last_acked_server_seq. We still
        // detect that gap defensively — frames that were ACKed-then-purged
        // are gone, and the client needs to resync via GetOpenOrders.
        let gap_boundary = self.highest_evicted_seq.max(self.last_acked_server_seq);
        if gap_boundary > 0 && last_received < gap_boundary {
            return ReplayResult::Gap {
                missing_from: last_received + 1,
                missing_to: gap_boundary,
            };
        }

        let frames = self
            .frames
            .iter()
            .filter(|(seq, _)| *seq > last_received)
            .map(|(_, frame)| frame.clone())
            .collect();
        ReplayResult::Ok(frames)
    }

    /// Purge replay buffer entries with seq <= last_acked.
    /// Called when client sends cumulative ACK. Also clears slow_consumer
    /// flag when the buffer drops back below the soft limit.
    fn apply_ack(&mut self, last_acked: u64) {
        // Only advance the ACK; never regress
        if last_acked <= self.last_acked_server_seq {
            return;
        }
        self.last_acked_server_seq = last_acked;

        while let Some(&(seq, _)) = self.frames.front() {
            if seq <= last_acked {
                self.frames.pop_front();
            } else {
                break;
            }
        }

        // If buffer is now below soft limit, clear slow_consumer flag
        // and allow another warning cycle.
        if self.usage_pct() < SOFT_LIMIT_PCT {
            self.slow_consumer = false;
            self.soft_warning_sent = false;
        }
    }

    /// Current buffer usage as percentage of max capacity (0..=100).
    fn usage_pct(&self) -> usize {
        if self.buffer_max == 0 {
            return 0;
        }
        (self.frames.len() * 100) / self.buffer_max
    }

    /// Check dedup table for a cached response.
    fn dedup_lookup(&self, request_id: u64) -> Option<&Bytes> {
        self.dedup.get(&request_id)
    }

    fn evict_oldest_query(&mut self) {
        if let Some(oldest) = self.query_order.pop_front() {
            self.query_payloads.remove(&oldest);
            if let Some(response) = self.dedup.remove(&oldest) {
                self.query_response_bytes =
                    self.query_response_bytes.saturating_sub(response.len());
            }
        }
    }

    /// Insert a response into the dedup table (with FIFO eviction if full).
    #[allow(clippy::map_entry)] // Entry API makes the two-phase (update vs insert+FIFO) logic less clear.
    fn dedup_insert(&mut self, request_id: u64, response: Bytes) {
        if self.query_payloads.contains_key(&request_id) {
            if let Some(old) = self.dedup.remove(&request_id) {
                self.query_response_bytes = self.query_response_bytes.saturating_sub(old.len());
            }
            self.query_order.retain(|request| *request != request_id);
            self.query_order.push_back(request_id);
            while self.query_response_bytes.saturating_add(response.len()) > MAX_QUERY_CACHE_BYTES
                && !self.query_order.is_empty()
            {
                self.evict_oldest_query();
            }
            if self.query_payloads.contains_key(&request_id) {
                self.query_response_bytes += response.len();
                self.dedup.insert(request_id, response);
            }
            return;
        }

        // If already present, don't shift eviction order — just update.
        if self.dedup.contains_key(&request_id) {
            self.dedup.insert(request_id, response);
            return;
        }
        // Evict oldest if at capacity
        if self.dedup.len() >= self.dedup_max {
            if let Some(oldest) = self.dedup_order.pop_front() {
                self.dedup.remove(&oldest);
            }
        }
        self.dedup.insert(request_id, response);
        self.dedup_order.push_back(request_id);
    }
}

type Shard = Mutex<FxHashMap<SessionId, SessionEntry>>;

#[derive(Serialize, Deserialize)]
enum JournalRecord {
    Create(SessionId),
    Remove(SessionId),
    Push(SessionId, u64, Bytes),
    Ack(SessionId, u64),
    Dedup(SessionId, u64, Bytes),
    Snapshot(SessionId, u64, Bytes),
    Admit(SessionId, u64, Vec<u8>),
    BatchBegin(SessionId, u64, Vec<me_engine::command::Command>),
    BatchDone(SessionId, u64, Bytes),
    Commit {
        session: SessionId,
        request: u64,
        item: u32,
        events: Vec<me_core::MatchEvent>,
        owners: Vec<(me_core::OrderId, SessionId)>,
        expiration_count: usize,
        expiration_owners: Vec<(me_core::OrderId, SessionId)>,
    },
}

pub struct SessionStore {
    execution_gate: Arc<[Arc<tokio::sync::Mutex<()>>; NUM_SHARDS]>,
    path: Option<PathBuf>,
    journal: Option<Arc<Mutex<std::fs::File>>>,
    failed: Arc<std::sync::atomic::AtomicBool>,
    checkpoint_lock: Arc<Mutex<()>>,
    active: Arc<Mutex<HashSet<SessionId>>>,
    shards: Arc<[Shard; NUM_SHARDS]>,
    buffer_min: usize,
    buffer_max: usize,
    dedup_max: usize,
    ttl: Duration,
}

impl SessionStore {
    pub async fn execution_guards(
        &self,
        symbols: Vec<u16>,
    ) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
        let mut indices: Vec<_> = symbols
            .into_iter()
            .map(|s| s as usize % NUM_SHARDS)
            .collect();
        indices.sort_unstable();
        indices.dedup();
        let mut guards = Vec::new();
        for index in indices {
            guards.push(Arc::clone(&self.execution_gate[index]).lock_owned().await);
        }
        guards
    }

    pub fn open(path: impl AsRef<Path>, capacity: usize, ttl: Duration) -> std::io::Result<Self> {
        use std::io::{Read, Seek, SeekFrom};
        let path = path.as_ref();
        let mut store = Self::new(capacity, ttl);
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.sync_all()?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        let total = file.metadata()?.len();
        let mut offset = 0u64;
        loop {
            let mut header = [0u8; 12];
            match file.read_exact(&mut header) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(error),
            }
            let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
            let header_crc = u32::from_le_bytes(header[4..8].try_into().unwrap());
            if crc32fast::hash(&header[..4]) != header_crc || len > MAX_JOURNAL_RECORD_BYTES {
                return Err(std::io::Error::other("invalid session journal header"));
            }
            let end = offset + 12 + len as u64;
            if end > total {
                break;
            }
            let mut payload = vec![0; len];
            file.read_exact(&mut payload)?;
            if crc32fast::hash(&payload) != u32::from_le_bytes(header[8..12].try_into().unwrap()) {
                return Err(std::io::Error::other("session journal checksum mismatch"));
            }
            let record: JournalRecord =
                serde_json::from_slice(&payload).map_err(std::io::Error::other)?;
            store.replay_record(record)?;
            offset = end;
        }
        if offset != total {
            file.set_len(offset)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::End(0))?;
        store.path = Some(path.to_path_buf());
        store.journal = Some(Arc::new(Mutex::new(file)));
        Ok(store)
    }

    pub fn checkpoint(&self) -> std::io::Result<()> {
        self.ensure_healthy()
    }

    fn append(&self, record: &JournalRecord) -> std::io::Result<()> {
        use std::io::Write;
        self.ensure_healthy()?;
        let Some(journal) = &self.journal else {
            return Ok(());
        };
        let payload = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        if payload.len() > MAX_JOURNAL_RECORD_BYTES {
            self.failed
                .store(true, std::sync::atomic::Ordering::Release);
            return Err(std::io::Error::other(
                "session journal record exceeds recovery limit",
            ));
        }
        let mut bytes = (u32::try_from(payload.len()).map_err(std::io::Error::other)?)
            .to_le_bytes()
            .to_vec();
        bytes.extend_from_slice(&crc32fast::hash(&bytes).to_le_bytes());
        bytes.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
        bytes.extend_from_slice(&payload);
        let result = (|| {
            let mut file = journal.lock().unwrap();
            file.write_all(&bytes)?;
            file.sync_data()
        })();
        if result.is_err() {
            self.failed
                .store(true, std::sync::atomic::Ordering::Release);
        }
        result
    }

    fn replay_record(&self, record: JournalRecord) -> std::io::Result<()> {
        match record {
            JournalRecord::Remove(id) => {
                self.shard_for(&id).lock().unwrap().remove(&id);
            }
            JournalRecord::Create(id) => {
                if id.is_none() || self.shard_for(&id).lock().unwrap().contains_key(&id) {
                    return Err(std::io::Error::other("invalid duplicate session creation"));
                }
                self.create_with_id_source(|| id);
            }
            JournalRecord::Push(id, seq, bytes) => {
                if seq == 0
                    || seq == u64::MAX
                    || seq != self.next_outbound_seq(&id)
                    || bytes.len() < crate::protocol::FRAME_HEADER_SIZE
                    || u64::from_le_bytes(bytes[6..14].try_into().unwrap()) != seq
                {
                    return Err(std::io::Error::other("invalid session frame sequence"));
                }
                if !self.push(&id, seq, bytes) {
                    return Err(std::io::Error::other("unknown journal session"));
                }
            }
            JournalRecord::Ack(id, seq) => {
                self.apply_ack(&id, seq);
            }
            JournalRecord::Dedup(id, request, bytes) => self.dedup_insert(&id, request, bytes),
            JournalRecord::Snapshot(id, request, bytes) => {
                self.commit_snapshot(&id, request, bytes)?;
            }
            JournalRecord::Admit(id, request, payload) => {
                self.admit_request(id, request, payload)?
            }
            JournalRecord::BatchBegin(id, request, commands) => {
                self.shard_for(&id)
                    .lock()
                    .unwrap()
                    .get_mut(&id)
                    .ok_or_else(|| std::io::Error::other("unknown batch session"))?
                    .batch_intents
                    .insert(request, commands);
            }
            JournalRecord::BatchDone(id, request, bytes) => {
                let mut shard = self.shard_for(&id).lock().unwrap();
                let entry = shard
                    .get_mut(&id)
                    .ok_or_else(|| std::io::Error::other("unknown batch session"))?;
                entry.dedup.insert(request, bytes);
                entry.batch_intents.remove(&request);
            }
            JournalRecord::Commit {
                session,
                request,
                item,
                events,
                owners,
                expiration_count,
                expiration_owners,
            } => {
                self.commit_request(me_engine::router::DurableRequest {
                    session_id: session,
                    request_id: request,
                    item_index: item,
                    events,
                    owners,
                    expiration_count,
                    expiration_owners,
                })?;
            }
        }
        Ok(())
    }

    pub fn ensure_healthy(&self) -> std::io::Result<()> {
        if self.failed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(std::io::Error::other(
                "session persistence failed; restart required",
            ));
        }
        Ok(())
    }

    pub fn reconcile(
        &self,
        records: Vec<me_engine::router::DurableRequest>,
    ) -> std::io::Result<()> {
        for record in records {
            if self.path.is_some() {
                let shard = self.shard_for(&record.session_id).lock().unwrap();
                let entry = shard.get(&record.session_id).ok_or_else(|| {
                    std::io::Error::other("durable request session missing from session journal")
                })?;
                if !entry.request_payloads.contains_key(&record.request_id)
                    && !entry.dedup.contains_key(&record.request_id)
                    && !entry.batch_intents.contains_key(&record.request_id)
                {
                    return Err(std::io::Error::other(
                        "durable request admission missing from session journal",
                    ));
                }
            }
            self.commit_request(record)?;
        }
        Ok(())
    }

    pub fn commit_request(
        &self,
        record: me_engine::router::DurableRequest,
    ) -> std::io::Result<Bytes> {
        self.ensure_healthy()?;
        use me_core::MatchEvent;
        let _lock = self.checkpoint_lock.lock().unwrap();
        let mut guards: Vec<_> = self.shards.iter().map(|s| s.lock().unwrap()).collect();
        let requester = record.session_id;
        let idx = Self::shard_idx(&requester);
        if !guards[idx].contains_key(&requester) {
            return Err(std::io::Error::other("requester session missing"));
        }
        if let Some(cached) = guards[idx].get(&requester).and_then(|e| {
            if record.item_index == 0 {
                e.dedup.get(&record.request_id)
            } else {
                e.batch_results
                    .get(&record.request_id)
                    .and_then(|items| items.get(&record.item_index))
            }
        }) {
            return Ok(cached.clone());
        }
        let journal_record = JournalRecord::Commit {
            session: record.session_id,
            request: record.request_id,
            item: record.item_index,
            events: record.events.clone(),
            owners: record.owners.clone(),
            expiration_count: record.expiration_count,
            expiration_owners: record.expiration_owners.clone(),
        };
        let triggered: HashSet<_> = record
            .events
            .iter()
            .filter_map(|event| {
                if let MatchEvent::StopTriggered { order_id, .. } = event {
                    Some(*order_id)
                } else {
                    None
                }
            })
            .collect();
        let owners: FxHashMap<_, _> = record.owners.into_iter().collect();
        let mut recipients: FxHashMap<SessionId, Vec<MatchEvent>> = FxHashMap::default();
        recipients.entry(requester).or_default();
        let expiration_owners: FxHashMap<_, _> = record.expiration_owners.into_iter().collect();
        for (event_index, event) in record.events.into_iter().enumerate() {
            let owners = if event_index < record.expiration_count {
                &expiration_owners
            } else {
                &owners
            };
            let ids = match &event {
                MatchEvent::Trade(t) => vec![t.maker_order_id, t.taker_order_id],
                MatchEvent::OrderAccepted { order_id, .. }
                | MatchEvent::OrderRejected { order_id, .. }
                | MatchEvent::OrderPartiallyFilled { order_id, .. }
                | MatchEvent::OrderFilled { order_id, .. }
                | MatchEvent::OrderCancelled { order_id, .. }
                | MatchEvent::OrderAmended { order_id, .. }
                | MatchEvent::StopTriggered { order_id, .. }
                | MatchEvent::OrderStatus { order_id, .. }
                | MatchEvent::OrderNotFound { order_id } => vec![*order_id],
            };
            let targets: HashSet<_> = ids
                .iter()
                .map(|id| {
                    owners
                        .get(id)
                        .copied()
                        .filter(|s| !s.is_none())
                        .unwrap_or(requester)
                })
                .collect();
            if matches!(
                event,
                MatchEvent::OrderRejected { .. }
                    | MatchEvent::OrderNotFound { .. }
                    | MatchEvent::OrderAccepted { .. }
            ) && !ids.iter().any(|id| triggered.contains(id))
            {
                recipients.entry(requester).or_default().push(event);
            } else {
                for target in targets {
                    recipients.entry(target).or_default().push(event.clone());
                }
            }
        }
        let mut response = Bytes::new();
        for (recipient, events) in recipients {
            let entry = guards[Self::shard_idx(&recipient)]
                .entry(recipient)
                .or_insert_with(|| {
                    SessionEntry::new(self.buffer_min, self.buffer_max, usize::MAX, self.ttl)
                });
            let mut block = bytes::BytesMut::new();
            crate::codec::encode_events(
                &events,
                0,
                if recipient == requester {
                    record.request_id
                } else {
                    0
                },
                &mut block,
            );
            let mut seq = entry
                .frames
                .back()
                .map(|(s, _)| *s)
                .unwrap_or(0)
                .max(entry.highest_evicted_seq)
                .max(entry.last_acked_server_seq)
                + 1;
            let mut offset = 0;
            while offset < block.len() {
                let end = offset
                    + crate::protocol::FRAME_HEADER_SIZE
                    + u32::from_le_bytes(block[offset..offset + 4].try_into().unwrap()) as usize;
                block[offset + 6..offset + 14].copy_from_slice(&seq.to_le_bytes());
                entry.push_frame(seq, Bytes::copy_from_slice(&block[offset..end]));
                offset = end;
                seq += 1;
            }
            if recipient == requester {
                response = block.freeze();
            }
        }
        let entry = guards[idx].get_mut(&requester).unwrap();
        if record.item_index == 0 {
            entry.dedup.insert(record.request_id, response.clone());
        } else {
            entry
                .batch_results
                .entry(record.request_id)
                .or_default()
                .insert(record.item_index, response.clone());
        }
        self.append(&journal_record)?;
        Ok(response)
    }

    pub fn cache_open_orders(
        &self,
        session: &SessionId,
        request: u64,
        orders: &[me_core::Order],
    ) -> std::io::Result<Bytes> {
        if orders
            .len()
            .checked_mul(82)
            .and_then(|bytes| bytes.checked_add(64))
            .is_none_or(|bytes| bytes > MAX_SNAPSHOT_BYTES)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "snapshot exceeds response byte limit",
            ));
        }
        {
            let shard = self.shard_for(session).lock().unwrap();
            if shard
                .get(session)
                .and_then(|entry| entry.query_payloads.get(&request))
                .is_none_or(|payload| payload.first() != Some(&9))
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "snapshot requires query admission",
                ));
            }
        }
        let snapshot_seq = self.next_outbound_seq(session);
        if snapshot_seq
            .checked_add(orders.len() as u64 + 2)
            .is_none_or(|next| next == u64::MAX)
        {
            return Err(std::io::Error::other("snapshot sequence exhausted"));
        }
        let mut response = bytes::BytesMut::with_capacity(64 + 82 * orders.len());
        let mut frame = bytes::BytesMut::with_capacity(82);
        crate::codec::encode_open_orders_begin(
            snapshot_seq,
            request,
            orders.len() as u32,
            snapshot_seq,
            &mut frame,
        );
        response.extend_from_slice(&frame);
        for (index, order) in orders.iter().enumerate() {
            frame.clear();
            crate::codec::encode_open_order(
                snapshot_seq + index as u64 + 1,
                request,
                order,
                &mut frame,
            );
            response.extend_from_slice(&frame);
        }
        frame.clear();
        crate::codec::encode_open_orders_end(
            snapshot_seq + orders.len() as u64 + 1,
            request,
            snapshot_seq,
            &mut frame,
        );
        response.extend_from_slice(&frame);
        self.commit_snapshot(session, request, response.freeze())
    }

    fn commit_snapshot(
        &self,
        session: &SessionId,
        request: u64,
        response: Bytes,
    ) -> std::io::Result<Bytes> {
        self.ensure_healthy()?;
        let invalid = || {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid snapshot journal response",
            )
        };
        if response.len() < 64 || response.len() > MAX_SNAPSHOT_BYTES {
            return Err(invalid());
        }
        let count = u32::from_le_bytes(response[22..26].try_into().unwrap()) as usize;
        if count.checked_mul(82).and_then(|n| n.checked_add(64)) != Some(response.len()) {
            return Err(invalid());
        }
        let mut shard = self.shard_for(session).lock().unwrap();
        let entry = shard.get_mut(session).ok_or_else(invalid)?;
        if entry
            .query_payloads
            .get(&request)
            .is_none_or(|payload| payload.first() != Some(&9))
        {
            return Err(invalid());
        }
        let first = entry
            .frames
            .back()
            .map(|(seq, _)| *seq)
            .unwrap_or(0)
            .max(entry.highest_evicted_seq)
            .max(entry.last_acked_server_seq)
            .checked_add(1)
            .ok_or_else(invalid)?;
        if first
            .checked_add(count as u64 + 2)
            .is_none_or(|next| next == u64::MAX)
        {
            return Err(invalid());
        }
        let mut offset = 0;
        for index in 0..count + 2 {
            let (kind, payload_len) = if index == 0 {
                (0x1f, 12)
            } else if index == count + 1 {
                (0x22, 8)
            } else {
                (0x21, 60)
            };
            let header = &response[offset..offset + crate::protocol::FRAME_HEADER_SIZE];
            if u32::from_le_bytes(header[..4].try_into().unwrap()) as usize != payload_len
                || header[4] != crate::protocol::PROTOCOL_VERSION
                || header[5] != kind
                || u64::from_le_bytes(header[6..14].try_into().unwrap()) != first + index as u64
                || u64::from_le_bytes(header[14..22].try_into().unwrap()) != request
            {
                return Err(invalid());
            }
            offset += crate::protocol::FRAME_HEADER_SIZE + payload_len;
        }
        if u64::from_le_bytes(response[26..34].try_into().unwrap()) != first
            || u64::from_le_bytes(response[response.len() - 8..].try_into().unwrap()) != first
        {
            return Err(invalid());
        }
        self.append(&JournalRecord::Snapshot(
            *session,
            request,
            response.clone(),
        ))?;
        let mut offset = 0;
        for index in 0..count + 2 {
            let end = offset
                + crate::protocol::FRAME_HEADER_SIZE
                + u32::from_le_bytes(response[offset..offset + 4].try_into().unwrap()) as usize;
            entry.push_frame(
                first + index as u64,
                Bytes::copy_from_slice(&response[offset..end]),
            );
            offset = end;
        }
        entry.dedup_insert(request, response.clone());
        entry.last_access = Instant::now();
        Ok(response)
    }

    pub fn process_batch(
        &self,
        router: &me_engine::router::Router,
        session: SessionId,
        request: u64,
        mut commands: Vec<me_engine::command::Command>,
    ) -> std::io::Result<Bytes> {
        if let Some(cached) = self.dedup_lookup(&session, request) {
            return Ok(cached);
        }
        {
            let mut shard = self.shard_for(&session).lock().unwrap();
            let entry = shard
                .get_mut(&session)
                .ok_or_else(|| std::io::Error::other("session missing"))?;
            for command in &mut commands {
                if let me_engine::command::Command::PlaceOrder(order) = command {
                    order.client_session_id = session;
                }
            }
            commands = entry
                .batch_intents
                .entry(request)
                .or_insert(commands)
                .clone();
        }
        self.append(&JournalRecord::BatchBegin(
            session,
            request,
            commands.clone(),
        ))?;
        for (index, command) in commands.into_iter().enumerate() {
            let item_index = u32::try_from(index + 1).map_err(std::io::Error::other)?;
            router.process_batch_item(command, session, request, item_index);
            let record = router
                .request_record_item(session, request, item_index)
                .map_err(std::io::Error::other)?
                .ok_or_else(|| std::io::Error::other("durable batch outcome missing"))?;
            self.commit_request(record)?;
        }
        let result = {
            let mut shard = self.shard_for(&session).lock().unwrap();
            let entry = shard.get_mut(&session).unwrap();
            let mut result = bytes::BytesMut::new();
            let count = entry.batch_intents[&request].len();
            for index in 1..=count {
                result.extend_from_slice(&entry.batch_results[&request][&(index as u32)]);
            }
            let result = result.freeze();
            entry.dedup.insert(request, result.clone());
            entry.batch_intents.remove(&request);
            result
        };
        self.append(&JournalRecord::BatchDone(session, request, result.clone()))?;
        Ok(result)
    }

    pub fn recover_batches(&self, router: &me_engine::router::Router) -> std::io::Result<()> {
        let mut pending = Vec::new();
        for shard in self.shards.iter() {
            for (session, entry) in shard.lock().unwrap().iter() {
                for (request, commands) in &entry.batch_intents {
                    pending.push((*session, *request, commands.clone()));
                }
            }
        }
        for (session, request, commands) in pending {
            self.process_batch(router, session, request, commands)?;
        }
        Ok(())
    }

    pub fn admit_request(
        &self,
        id: SessionId,
        request: u64,
        payload: Vec<u8>,
    ) -> std::io::Result<()> {
        self.ensure_healthy()?;
        {
            let mut shard = self.shard_for(&id).lock().unwrap();
            let entry = shard
                .get_mut(&id)
                .ok_or_else(|| std::io::Error::other("session missing"))?;
            if let Some(previous) = entry
                .request_payloads
                .get(&request)
                .or_else(|| entry.query_payloads.get(&request))
            {
                if previous != &payload {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "request_id reused with different payload",
                    ));
                }
                return Ok(());
            }
            let kind = payload.first().copied().unwrap_or(0);
            if matches!(kind, 7 | 9) {
                if entry.query_order.len() >= self.dedup_max {
                    entry.evict_oldest_query();
                }
                self.append(&JournalRecord::Admit(id, request, payload.clone()))?;
                entry.query_order.push_back(request);
                entry.query_payloads.insert(request, payload);
                return Ok(());
            }
            let units = if matches!(kind, 11 | 12) {
                payload.get(1).copied().unwrap_or(0) as usize
            } else {
                1
            };
            if matches!(kind, 2 | 12) {
                if entry.cancel_units.saturating_add(units) > 100_000 {
                    return Err(std::io::Error::other(
                        "session cancellation budget exhausted",
                    ));
                }
                entry.cancel_units += units;
            } else {
                if entry.placement_units.saturating_add(units) > 100_000
                    || entry.request_bytes.saturating_add(payload.len()) > 16 * 1024 * 1024
                {
                    return Err(std::io::Error::other("session placement budget exhausted; cancellation and queries remain available"));
                }
                entry.placement_units += units;
            }
            if !matches!(kind, 2 | 12) {
                entry.request_bytes = entry.request_bytes.saturating_add(payload.len());
            }
            self.append(&JournalRecord::Admit(id, request, payload.clone()))?;
            entry.request_payloads.insert(request, payload);
        }
        self.checkpoint()
    }

    pub fn try_create(&self) -> std::io::Result<SessionId> {
        self.ensure_healthy()?;
        let _guard = self.checkpoint_lock.lock().unwrap();
        if self.len() >= 10_000 {
            return Err(std::io::Error::other("session capacity exhausted"));
        }
        Ok(self.create())
    }

    pub fn delivery_snapshot(
        &self,
        id: &SessionId,
        last_received: u64,
    ) -> std::io::Result<(ReplayResult, u64)> {
        self.ensure_healthy()?;
        let active = self.active.lock().unwrap();
        let mut map = Self::lock_shard(self.shard_for(id), id, "delivery_snapshot");
        let Some(entry) = map.get_mut(id) else {
            return Ok((ReplayResult::SessionNotFound, 1));
        };
        if self.path.is_none() && entry.is_expired() && !active.contains(id) {
            map.remove(id);
            return Ok((ReplayResult::SessionNotFound, 1));
        }
        let highest = entry
            .frames
            .back()
            .map(|(seq, _)| *seq)
            .unwrap_or(0)
            .max(entry.highest_evicted_seq)
            .max(entry.last_acked_server_seq);
        if last_received > highest {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "resume cursor exceeds the session sequence",
            ));
        }
        let next = highest
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("session sequence exhausted"))?;
        entry.last_access = Instant::now();
        Ok((entry.check_replay(last_received), next))
    }

    pub fn pending(
        &self,
        id: &SessionId,
        next: u64,
        output: &mut bytes::BytesMut,
    ) -> std::io::Result<u64> {
        self.ensure_healthy()?;
        output.clear();
        let last_received = next.checked_sub(1).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid zero delivery cursor",
            )
        })?;
        let (replay, snapshot_next) = self.delivery_snapshot(id, last_received)?;
        match replay {
            ReplayResult::Ok(frames) => {
                crate::codec::encode_replay_frames(&frames, output);
                Ok(next_sequence_in_block(output).unwrap_or(next))
            }
            ReplayResult::Gap {
                missing_from,
                missing_to,
            } => {
                crate::codec::encode_replay_gap(0, missing_from, missing_to, output);
                Ok(snapshot_next)
            }
            ReplayResult::SessionNotFound => Err(std::io::Error::other("active session expired")),
        }
    }

    pub fn claim(&self, id: SessionId) -> std::io::Result<SessionLease> {
        self.ensure_healthy()?;
        let mut active = self.active.lock().unwrap();
        if active.contains(&id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "session already connected",
            ));
        }
        let mut shard = Self::lock_shard(self.shard_for(&id), &id, "claim");
        let entry = shard.get_mut(&id).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "session not found")
        })?;
        entry.last_access = Instant::now();
        active.insert(id);
        Ok(SessionLease {
            id,
            active: Arc::clone(&self.active),
        })
    }

    /// Create a new store with the given MAX replay buffer capacity and TTL.
    ///
    /// Each session's buffer starts at `min(capacity, DEFAULT_BUFFER_MIN=1024)`
    /// and grows adaptively up to `capacity`. Once `capacity` is reached,
    /// oldest frames are evicted to make room (slow-consumer behavior).
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self::new_adaptive(
            capacity.min(DEFAULT_BUFFER_MIN),
            capacity,
            DEFAULT_DEDUP_MAX,
            ttl,
        )
    }

    /// Create a new store with custom MAX capacity and dedup table size.
    ///
    /// - `capacity`: MAX per-session replay buffer size (adaptive: starts at
    ///   min(capacity, 1024) and grows up to `capacity`)
    /// - `dedup_max`: max entries in each session's dedup table
    pub fn new_with_dedup(capacity: usize, dedup_max: usize, ttl: Duration) -> Self {
        Self::new_adaptive(capacity.min(DEFAULT_BUFFER_MIN), capacity, dedup_max, ttl)
    }

    /// Create a store with full control over adaptive buffer bounds.
    ///
    /// - `buffer_min`: starting capacity for each session's replay buffer
    /// - `buffer_max`: hard limit; if reached, oldest frames are evicted
    /// - `dedup_max`: max entries in per-session dedup table
    pub fn new_adaptive(
        buffer_min: usize,
        buffer_max: usize,
        dedup_max: usize,
        ttl: Duration,
    ) -> Self {
        assert!(buffer_min <= buffer_max, "buffer_min must be <= buffer_max");
        let shards: [Shard; NUM_SHARDS] = std::array::from_fn(|_| Mutex::new(FxHashMap::default()));
        Self {
            execution_gate: Arc::new(std::array::from_fn(|_| {
                Arc::new(tokio::sync::Mutex::new(()))
            })),
            path: None,
            journal: None,
            failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            checkpoint_lock: Arc::new(Mutex::new(())),
            active: Arc::new(Mutex::new(HashSet::new())),
            shards: Arc::new(shards),
            buffer_min,
            buffer_max,
            dedup_max,
            ttl,
        }
    }

    /// Pick a shard for the given session ID using its first byte.
    /// Since session_id is random, this gives uniform distribution.
    #[inline]
    fn shard_for(&self, id: &SessionId) -> &Shard {
        let idx = (id.as_bytes()[0] as usize) & SHARD_MASK;
        &self.shards[idx]
    }

    /// Compute the shard index for a session ID (for diagnostics).
    #[inline]
    fn shard_idx(id: &SessionId) -> usize {
        (id.as_bytes()[0] as usize) & SHARD_MASK
    }

    /// Lock a shard, recovering from poisoning with a loud log. A poisoned
    /// shard means a prior thread panicked while holding this lock — state
    /// MAY be inconsistent, but the best we can do is log loudly and
    /// continue rather than propagating the panic.
    #[inline]
    fn lock_shard<'a>(
        shard: &'a Shard,
        id: &SessionId,
        op: &'static str,
    ) -> std::sync::MutexGuard<'a, FxHashMap<SessionId, SessionEntry>> {
        shard.lock().unwrap_or_else(|poisoned| {
            tracing::error!(
                shard_idx = Self::shard_idx(id),
                session_id = %id.log_prefix(),
                operation = op,
                "SESSION STORE SHARD POISONED — recovering inner state"
            );
            poisoned.into_inner()
        })
    }

    /// Lock a shard by index (for gc/len — bulk ops with no specific session).
    #[inline]
    fn lock_shard_by_idx<'a>(
        shard: &'a Shard,
        shard_idx: usize,
        op: &'static str,
    ) -> std::sync::MutexGuard<'a, FxHashMap<SessionId, SessionEntry>> {
        shard.lock().unwrap_or_else(|poisoned| {
            tracing::error!(
                shard_idx,
                operation = op,
                "SESSION STORE SHARD POISONED — recovering inner state"
            );
            poisoned.into_inner()
        })
    }

    /// Create and register a new session with a random id. Returns the id.
    ///
    /// Collision-safe: retries until an unused id is drawn. Collision
    /// probability is 2^-128 per attempt, but over a long server lifetime
    /// with billions of sessions the expected overhead is negligible while
    /// the safety benefit (no silent session overwrite) is absolute.
    pub fn create(&self) -> SessionId {
        self.create_with_id_source(SessionId::random)
    }

    /// Create and register a new session, using the caller-supplied closure
    /// to generate candidate ids. If the drawn id collides with an existing
    /// session, logs a WARN and calls the closure again.
    ///
    /// Primarily exists for deterministic collision-retry tests; production
    /// code should use `create()`.
    pub fn create_with_id_source<F>(&self, mut id_source: F) -> SessionId
    where
        F: FnMut() -> SessionId,
    {
        loop {
            let id = id_source();
            if id.is_none() {
                tracing::warn!("SessionStore::create drew SessionId::NONE — retrying");
                continue;
            }
            let entry =
                SessionEntry::new(self.buffer_min, self.buffer_max, self.dedup_max, self.ttl);
            let shard = self.shard_for(&id);
            let mut map = Self::lock_shard(shard, &id, "create");
            if map.contains_key(&id) {
                // Astronomical collision — log loudly and retry. If this
                // EVER fires in production, something is wrong with the RNG.
                tracing::warn!(
                    session_id = %id.log_prefix(),
                    "SessionId collision on create — retrying with fresh id"
                );
                continue;
            }
            if let Err(error) = self.append(&JournalRecord::Create(id)) {
                tracing::error!(%error, "session creation persistence failed");
            }
            map.insert(id, entry);
            return id;
        }
    }

    /// Push an outbound frame to the session's ring buffer.
    ///
    /// `server_seq` is the connection-local monotonic counter; the client
    /// sends it back in `ResumeSession.last_recv_seq` and in ACKs.
    ///
    /// Returns `true` if the session exists and the frame was pushed;
    /// `false` if the session has already been evicted (TTL / GC) and the
    /// caller should treat the frame as non-replayable.
    pub fn push(&self, id: &SessionId, server_seq: u64, frame: Bytes) -> bool {
        let shard = self.shard_for(id);
        let mut map = Self::lock_shard(shard, id, "push");
        if let Some(entry) = map.get_mut(id) {
            if self
                .append(&JournalRecord::Push(*id, server_seq, frame.clone()))
                .is_err()
            {
                return false;
            }
            entry.push_frame(server_seq, frame);
            entry.last_access = Instant::now();
            true
        } else {
            false
        }
    }

    /// Split a contiguous block of wire frames by their length prefixes,
    /// OVERWRITE each frame's header.seq with a monotonically-assigned
    /// `starting_seq, starting_seq+1, ...`, and push each frame individually
    /// into the replay ring.
    ///
    /// Header.seq (u64 LE at offset 6..14) is rewritten in-place in the
    /// provided mutable block so the bytes sent over the wire carry the same
    /// server_seq values stored in the replay buffer. This is the single
    /// source of truth for outbound sequencing.
    ///
    /// Returns the number of frames successfully parsed and pushed. Caller
    /// should advance their `server_frame_seq` counter by this amount
    /// BEFORE transmitting the (mutated) block over the wire.
    pub fn push_frames_with_seq_rewrite(
        &self,
        id: &SessionId,
        starting_seq: u64,
        block: &mut [u8],
    ) -> u64 {
        use crate::protocol::FRAME_HEADER_SIZE;

        let shard = self.shard_for(id);
        let mut map = Self::lock_shard(shard, id, "push_frames_with_seq_rewrite");
        let Some(entry) = map.get_mut(id) else {
            // Session already evicted (TTL/GC) — the caller's wire frames
            // will still transmit but they cannot be replayed on reconnect.
            // Log at WARN so operators can correlate with replay gaps.
            tracing::warn!(
                session_id = %id.log_prefix(),
                "push to missing/expired session — frames will not be replayable"
            );
            return 0;
        };

        let starting_seq = starting_seq.max(
            entry
                .frames
                .back()
                .map(|(s, _)| *s)
                .unwrap_or(0)
                .max(entry.highest_evicted_seq)
                .max(entry.last_acked_server_seq)
                .saturating_add(1),
        );
        let mut offset = 0;
        let mut count = 0u64;
        while offset + FRAME_HEADER_SIZE <= block.len() {
            let payload_len = u32::from_le_bytes([
                block[offset],
                block[offset + 1],
                block[offset + 2],
                block[offset + 3],
            ]) as usize;
            let frame_end = offset + FRAME_HEADER_SIZE + payload_len;
            if frame_end > block.len() {
                break; // truncated frame — stop splitting
            }
            // Overwrite header.seq (bytes 6..14) with assigned server_seq.
            let seq = starting_seq + count;
            block[offset + 6..offset + 14].copy_from_slice(&seq.to_le_bytes());
            let frame = Bytes::copy_from_slice(&block[offset..frame_end]);
            if self
                .append(&JournalRecord::Push(*id, seq, frame.clone()))
                .is_err()
            {
                return 0;
            }
            entry.push_frame(seq, frame);
            offset = frame_end;
            count += 1;
        }
        entry.last_access = Instant::now();
        count
    }

    /// Resume a session with gap detection.
    ///
    /// Returns `ReplayResult::Ok(frames)` if all requested frames are still
    /// buffered, `ReplayResult::Gap` if some were evicted (client must resync
    /// via GetOpenOrders), or `ReplayResult::SessionNotFound` if the session
    /// is missing or expired.
    pub fn resume(&self, id: &SessionId, last_recv_seq: u64) -> ReplayResult {
        let active = self.active.lock().unwrap();
        let shard = self.shard_for(id);
        let mut map = Self::lock_shard(shard, id, "resume");
        let Some(entry) = map.get_mut(id) else {
            return ReplayResult::SessionNotFound;
        };
        if self.path.is_none() && entry.is_expired() && !active.contains(id) {
            map.remove(id);
            return ReplayResult::SessionNotFound;
        }
        entry.last_access = Instant::now();
        entry.check_replay(last_recv_seq)
    }

    /// Check buffer pressure for flow control decisions.
    ///
    /// Returns `(usage_pct, is_hard_limit)`:
    /// - `usage_pct`: 0..=100 — current buffer fill level as % of max capacity
    /// - `is_hard_limit`: true when slow_consumer flag is set (eviction happened)
    ///
    /// Returns `(0, false)` for missing sessions.
    pub fn buffer_pressure(&self, id: &SessionId) -> (u8, bool) {
        let shard = self.shard_for(id);
        let map = Self::lock_shard(shard, id, "buffer_pressure");
        let Some(entry) = map.get(id) else {
            return (0, false);
        };
        (entry.usage_pct() as u8, entry.slow_consumer)
    }

    /// Should we emit a FlowControlWarning right now?
    ///
    /// Returns `Some((usage_pct, is_hard))` if a warning should be sent:
    /// - soft limit (≥80%) reached AND no warning sent yet since buffer last
    ///   drained back below 80% via ACK (i.e., once per fill cycle), OR
    /// - hard limit reached (slow_consumer flag is set by buffer eviction) —
    ///   emits EVERY time until an ACK clears the flag.
    ///
    /// Warning rhythm by scenario:
    /// - Fill to 85% → ACK drains to 70% → fill to 85%: 2 soft warnings
    /// - Fill to 85% and stay there (partial ACKs): 1 soft warning only
    /// - Fill to 100% (hard): emitted EVERY call until buffer drains
    ///
    /// Returns `None` otherwise. Marks soft_warning_sent=true as a side effect.
    pub fn check_flow_control_warning(&self, id: &SessionId) -> Option<(u8, bool)> {
        let shard = self.shard_for(id);
        let mut map = Self::lock_shard(shard, id, "check_flow_control_warning");
        let entry = map.get_mut(id)?;
        let pct = entry.usage_pct();

        // Hard limit: slow consumer flag is set → emit every time until ACK clears it.
        if entry.slow_consumer {
            return Some((pct as u8, true));
        }

        // Soft limit: emit once per fill cycle.
        if pct >= SOFT_LIMIT_PCT && !entry.soft_warning_sent {
            entry.soft_warning_sent = true;
            return Some((pct as u8, false));
        }

        None
    }

    /// Process a cumulative ACK: purge replay buffer entries up to `last_acked`.
    /// Returns the number of frames evicted.
    pub fn apply_ack(&self, id: &SessionId, last_acked_server_seq: u64) -> usize {
        let shard = self.shard_for(id);
        let mut map = Self::lock_shard(shard, id, "apply_ack");
        let Some(entry) = map.get_mut(id) else {
            return 0;
        };
        // Defensive hardening: cap the ACK value to the highest seq the server
        // has actually sent.  A malicious client could send an arbitrarily large
        // ACK (e.g. u64::MAX) which, when stored as `last_acked_server_seq`,
        // would cause `next_outbound_seq()` to return a near-overflow value and
        // eventually panic or wrap on the next `ServerFrameSeq::next()` call.
        let buf_highest = entry.frames.back().map(|(s, _)| *s).unwrap_or(0);
        let highest_sent = buf_highest.max(entry.highest_evicted_seq);
        let sanitized_last_acked = if last_acked_server_seq > highest_sent {
            if last_acked_server_seq > highest_sent.saturating_add(1) {
                tracing::warn!(
                    session = ?id,
                    ack = last_acked_server_seq,
                    highest_sent,
                    "clamping out-of-range ACK to highest sent seq"
                );
            }
            highest_sent
        } else {
            last_acked_server_seq
        };
        let before = entry.frames.len();
        if self
            .append(&JournalRecord::Ack(*id, sanitized_last_acked))
            .is_err()
        {
            return 0;
        }
        entry.apply_ack(sanitized_last_acked);
        entry.last_access = Instant::now();
        before.saturating_sub(entry.frames.len())
    }

    /// Check dedup table for a cached response. Returns cloned Bytes if found.
    pub fn dedup_lookup(&self, id: &SessionId, request_id: u64) -> Option<Bytes> {
        let shard = self.shard_for(id);
        // Mutable lock: a cache hit counts as session activity and MUST
        // refresh last_access, otherwise a retry-heavy client (dedup cache
        // hitting every time) could TTL-expire mid-retry while being
        // actively exercised.
        let mut map = Self::lock_shard(shard, id, "dedup_lookup");
        let entry = map.get_mut(id)?;
        let hit = entry.dedup_lookup(request_id).cloned();
        if hit.is_some() {
            entry.last_access = Instant::now();
        }
        hit
    }

    /// Insert a response into the dedup table for future retries.
    pub fn dedup_insert(&self, id: &SessionId, request_id: u64, response: Bytes) {
        let shard = self.shard_for(id);
        let mut map = Self::lock_shard(shard, id, "dedup_insert");
        if let Some(entry) = map.get_mut(id) {
            if self
                .append(&JournalRecord::Dedup(*id, request_id, response.clone()))
                .is_err()
            {
                return;
            }
            entry.dedup_insert(request_id, response);
            entry.last_access = Instant::now();
        }
    }

    /// Get current replay buffer size (for metrics/tests).
    pub fn buffer_len(&self, id: &SessionId) -> usize {
        let shard = self.shard_for(id);
        let map = Self::lock_shard(shard, id, "buffer_len");
        map.get(id).map(|e| e.frames.len()).unwrap_or(0)
    }

    /// The seq value that the NEXT outbound frame on this session must carry.
    ///
    /// Equals `1 + max(highest seq in buffer, highest_evicted_seq, last_acked_server_seq)`.
    /// Returns 1 for fresh / missing sessions.
    ///
    /// This is an instantaneous observation, not a delivery cursor. Network
    /// handlers must use `delivery_snapshot()` to select replay and its next
    /// cursor atomically; a separate call here can skip concurrent deliveries.
    /// Seq=0 is reserved, so the minimum returned value is 1.
    pub fn next_outbound_seq(&self, id: &SessionId) -> u64 {
        let shard = self.shard_for(id);
        let map = Self::lock_shard(shard, id, "next_outbound_seq");
        let Some(entry) = map.get(id) else {
            return 1;
        };
        let buf_highest = entry.frames.back().map(|(s, _)| *s).unwrap_or(0);
        let highest = buf_highest
            .max(entry.highest_evicted_seq)
            .max(entry.last_acked_server_seq);
        highest.saturating_add(1)
    }

    /// Get current dedup table size (for metrics/tests).
    pub fn dedup_len(&self, id: &SessionId) -> usize {
        let shard = self.shard_for(id);
        let map = Self::lock_shard(shard, id, "dedup_len");
        map.get(id).map(|e| e.dedup.len()).unwrap_or(0)
    }

    /// Remove expired sessions from all shards. Call periodically.
    pub fn gc(&self) {
        let active = self.active.lock().unwrap();
        for (idx, shard) in self.shards.iter().enumerate() {
            let mut map = Self::lock_shard_by_idx(shard, idx, "gc");
            let expired: Vec<_> = map
                .iter()
                .filter(|(id, entry)| {
                    entry.is_expired()
                        && !active.contains(id)
                        && (self.path.is_none()
                            || (entry.request_payloads.is_empty()
                                && entry.dedup.is_empty()
                                && entry.batch_intents.is_empty()))
                })
                .map(|(id, _)| *id)
                .collect();
            for id in expired {
                if self.append(&JournalRecord::Remove(id)).is_ok() {
                    map.remove(&id);
                }
            }
        }
    }

    /// Total number of active sessions across all shards.
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .enumerate()
            .map(|(idx, s)| Self::lock_shard_by_idx(s, idx, "len").len())
            .sum()
    }

    /// True when no active sessions exist.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Clone for SessionStore {
    fn clone(&self) -> Self {
        Self {
            execution_gate: Arc::clone(&self.execution_gate),
            path: self.path.clone(),
            journal: self.journal.clone(),
            failed: Arc::clone(&self.failed),
            checkpoint_lock: Arc::clone(&self.checkpoint_lock),
            active: Arc::clone(&self.active),
            shards: Arc::clone(&self.shards),
            buffer_min: self.buffer_min,
            buffer_max: self.buffer_max,
            dedup_max: self.dedup_max,
            ttl: self.ttl,
        }
    }
}

pub struct SessionLease {
    id: SessionId,
    active: Arc<Mutex<HashSet<SessionId>>>,
}
impl Drop for SessionLease {
    fn drop(&mut self) {
        self.active.lock().unwrap().remove(&self.id);
    }
}

pub(crate) fn next_sequence_in_block(block: &[u8]) -> Option<u64> {
    let mut offset = 0;
    let mut highest = 0;
    while offset + crate::protocol::FRAME_HEADER_SIZE <= block.len() {
        let size = u32::from_le_bytes(block[offset..offset + 4].try_into().unwrap()) as usize;
        let end = offset + crate::protocol::FRAME_HEADER_SIZE + size;
        if end > block.len() {
            break;
        }
        highest = highest.max(u64::from_le_bytes(
            block[offset + 6..offset + 14].try_into().unwrap(),
        ));
        offset = end;
    }
    if highest > 0 {
        highest.checked_add(1)
    } else {
        None
    }
}

#[cfg(test)]
mod durability_fault_tests {
    use super::*;
    #[test]
    fn interrupted_batch_replays_committed_item_and_finishes_remaining_item() {
        use me_core::{FixedPrice, FixedQty, Order, Side, SymbolConfig, SymbolId, TimeInForce};
        use me_engine::{command::Command, router::Router};
        let path = std::env::temp_dir().join(format!(
            "me-session-batch-{}",
            SessionId::random().log_prefix()
        ));
        let store = SessionStore::open(&path, 32, Duration::from_secs(60)).unwrap();
        let session = store.create();
        let mut router = Router::new(1);
        router.add_symbol(SymbolConfig::default_btc_usdt());
        let commands: Vec<_> = [1, 2]
            .into_iter()
            .map(|id| {
                let mut order = Order::new_limit(
                    id,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_f64(100.0),
                    FixedQty::from_f64(1.0),
                    1,
                );
                order.client_session_id = session;
                Command::PlaceOrder(order)
            })
            .collect();
        store
            .append(&JournalRecord::BatchBegin(session, 10, commands.clone()))
            .unwrap();
        router.process_batch_item(commands[0].clone(), session, 10, 1);
        drop(store);
        let store = SessionStore::open(&path, 32, Duration::from_secs(60)).unwrap();
        store
            .reconcile(router.recovered_requests().unwrap())
            .unwrap();
        store.recover_batches(&router).unwrap();
        assert_eq!(router.get_open_orders_by_session(session).unwrap().len(), 2);
        let response = store.dedup_lookup(&session, 10).unwrap();
        assert_eq!(
            store.process_batch(&router, session, 10, commands).unwrap(),
            response
        );
        assert_eq!(router.get_open_orders_by_session(session).unwrap().len(), 2);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn placement_limit_keeps_cancellation_and_queries_available() {
        let store = SessionStore::new(32, Duration::from_secs(60));
        let id = store.create();
        {
            let mut shard = store.shard_for(&id).lock().unwrap();
            let entry = shard.get_mut(&id).unwrap();
            entry.placement_units = 100_000;
            entry.request_bytes = 16 * 1024 * 1024;
        }
        assert!(store.admit_request(id, 1, vec![1]).is_err());
        assert!(store.admit_request(id, 2, vec![2]).is_ok());
        assert!(store.admit_request(id, 3, vec![9]).is_ok());
    }

    #[test]
    fn unused_expired_session_removal_survives_restart() {
        let path = std::env::temp_dir().join(format!(
            "me-session-gc-{}",
            SessionId::random().log_prefix()
        ));
        let store = SessionStore::open(&path, 32, Duration::ZERO).unwrap();
        let id = store.create();
        store.gc();
        assert_eq!(store.len(), 0);
        drop(store);
        let store = SessionStore::open(&path, 32, Duration::ZERO).unwrap();
        assert_eq!(store.resume(&id, 0), ReplayResult::SessionNotFound);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn append_failure_blocks_cached_success_until_restart() {
        let path = std::env::temp_dir().join(format!(
            "me-session-fault-{}",
            SessionId::random().log_prefix()
        ));
        let mut store = SessionStore::open(&path, 32, Duration::from_secs(60)).unwrap();
        let id = store.create();
        store.journal = Some(Arc::new(Mutex::new(std::fs::File::open(&path).unwrap())));
        let request = || me_engine::router::DurableRequest {
            session_id: id,
            request_id: 1,
            item_index: 0,
            events: vec![me_core::MatchEvent::OrderAccepted {
                order_id: 1,
                timestamp: 1,
            }],
            owners: vec![(1, id)],
            expiration_count: 0,
            expiration_owners: Vec::new(),
        };
        assert!(store.commit_request(request()).is_err());
        assert!(store.commit_request(request()).is_err());
        assert!(store.checkpoint().is_err());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn query_cache_byte_eviction_preserves_mutation_decisions_and_replays() {
        let path = std::env::temp_dir().join(format!(
            "me-query-budget-{}",
            SessionId::random().log_prefix()
        ));
        let store = SessionStore::open(&path, 32, Duration::from_secs(60)).unwrap();
        let id = store.create();
        store.admit_request(id, 1, vec![1]).unwrap();
        let mutation = store
            .commit_request(me_engine::router::DurableRequest {
                session_id: id,
                request_id: 1,
                item_index: 0,
                events: vec![me_core::MatchEvent::OrderAccepted {
                    order_id: 1,
                    timestamp: 1,
                }],
                owners: vec![(1, id)],
                expiration_count: 0,
                expiration_owners: Vec::new(),
            })
            .unwrap();
        for request in 2..=4 {
            store.admit_request(id, request, vec![9]).unwrap();
            store.dedup_insert(&id, request, Bytes::from(vec![1; 6 * 1024 * 1024]));
        }
        for reopened in [false, true] {
            let restored;
            let current = if reopened {
                restored = SessionStore::open(&path, 32, Duration::from_secs(60)).unwrap();
                &restored
            } else {
                &store
            };
            assert!(
                current.dedup_lookup(&id, 2).is_none(),
                "oldest query must be evicted by byte budget"
            );
            assert!(current.dedup_lookup(&id, 3).is_some());
            assert!(current.dedup_lookup(&id, 4).is_some());
            assert_eq!(current.dedup_lookup(&id, 1), Some(mutation.clone()));
            assert!(
                current.admit_request(id, 2, vec![7]).is_ok(),
                "evicted query payload must not outlive its response"
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn oversized_snapshot_is_rejected_before_any_frame_is_pushed() {
        let store = SessionStore::new(32, Duration::from_secs(60));
        let id = store.create();
        store.admit_request(id, 1, vec![9]).unwrap();
        let order = me_core::Order::new_limit(
            1,
            me_core::SymbolId(0),
            me_core::Side::Buy,
            me_core::TimeInForce::GTC,
            me_core::FixedPrice::from_f64(100.0),
            me_core::FixedQty::from_f64(1.0),
            1,
        );
        let oversized = vec![order; 103_000];
        assert!(store.cache_open_orders(&id, 1, &oversized).is_err());
        assert_eq!(store.next_outbound_seq(&id), 1);
        assert!(store.dedup_lookup(&id, 1).is_none());
        assert_eq!(
            store
                .cache_open_orders(&id, 1, &oversized[..100_000])
                .unwrap()
                .len(),
            8_200_064
        );
    }

    #[test]
    fn oversized_journal_record_never_reaches_disk() {
        let path = std::env::temp_dir().join(format!(
            "me-journal-limit-{}",
            SessionId::random().log_prefix()
        ));
        let store = SessionStore::open(&path, 32, Duration::from_secs(60)).unwrap();
        let id = store.create();
        let length = std::fs::metadata(&path).unwrap().len();
        assert!(store
            .append(&JournalRecord::Dedup(
                id,
                1,
                Bytes::from(vec![255; 17 * 1024 * 1024])
            ))
            .is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), length);
        drop(store);
        assert!(SessionStore::open(&path, 32, Duration::from_secs(60)).is_ok());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn durable_snapshot_is_atomic_across_torn_append_and_retry() {
        let path = std::env::temp_dir().join(format!(
            "me-atomic-snapshot-{}",
            SessionId::random().log_prefix()
        ));
        let store = SessionStore::open(&path, 32, Duration::from_secs(60)).unwrap();
        let id = store.create();
        store.admit_request(id, 1, vec![9]).unwrap();
        let before = std::fs::metadata(&path).unwrap().len();
        let order = me_core::Order::new_limit(
            1,
            me_core::SymbolId(0),
            me_core::Side::Buy,
            me_core::TimeInForce::GTC,
            me_core::FixedPrice::from_f64(100.0),
            me_core::FixedQty::from_f64(1.0),
            1,
        );
        let orders = vec![order; 3200];
        let original = store.cache_open_orders(&id, 1, &orders).unwrap();
        let after = std::fs::metadata(&path).unwrap().len();
        drop(store);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(before + (after - before) / 2)
            .unwrap();
        let store = SessionStore::open(&path, 32, Duration::from_secs(60)).unwrap();
        assert_eq!(
            store.next_outbound_seq(&id),
            1,
            "torn snapshot must not persist a partial response"
        );
        assert!(store.dedup_lookup(&id, 1).is_none());
        assert_eq!(store.cache_open_orders(&id, 1, &orders).unwrap(), original);
        drop(store);
        let store = SessionStore::open(&path, 32, Duration::from_secs(60)).unwrap();
        assert_eq!(store.dedup_lookup(&id, 1), Some(original));
        assert_eq!(store.next_outbound_seq(&id), 3203);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn replacing_query_response_does_not_double_count_bytes() {
        let store = SessionStore::new(32, Duration::from_secs(60));
        let id = store.create();
        for request in [1, 2, 1] {
            store.admit_request(id, request, vec![9]).unwrap();
            store.dedup_insert(&id, request, Bytes::from(vec![1; 6 * 1024 * 1024]));
        }
        assert!(store.dedup_lookup(&id, 1).is_some());
        assert!(store.dedup_lookup(&id, 2).is_some());
    }

    #[test]
    fn snapshot_requires_query_admission_without_mutating_replay() {
        let store = SessionStore::new(32, Duration::from_secs(60));
        let id = store.create();
        assert!(store.cache_open_orders(&id, 1, &[]).is_err());
        assert_eq!(store.next_outbound_seq(&id), 1);
    }
}
