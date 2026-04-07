//! Session store v2: sharded, with dedup table and cumulative ACK support.
//!
//! This module implements the server-side state required for **exactly-once**
//! delivery semantics:
//!
//! 1. **Replay buffer** — per-session ring of outbound frames, indexed by
//!    monotonic `server_seq`. Clients request replay via `ResumeSession`.
//!
//! 2. **Dedup table** — per-session `request_id → cached response` map.
//!    When a client retries a command with the same `request_id` (e.g. after
//!    reconnect during mid-request), the server returns the cached response
//!    instead of re-executing the command. This prevents double-processing.
//!
//! 3. **Cumulative ACK** — clients periodically tell the server
//!    `last_processed_server_seq` via `Ping` piggyback or explicit `Acknowledge`.
//!    The server purges replay buffer entries up to that sequence, bounding
//!    memory usage.
//!
//! ## Sharding
//!
//! At scale (10k+ concurrent sessions, 1M+ ops/s) a single `Mutex<HashMap>`
//! becomes a contention bottleneck. We shard the map across a fixed 64
//! `Mutex`es (see `NUM_SHARDS` below) keyed by the first byte of the
//! session_id. Sharding is deliberately decoupled from core count so sessions
//! hash to the same shard across processes/machines. Since session IDs are
//! random, this gives roughly uniform distribution.

use std::collections::VecDeque;
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
/// See RFC-0001 Task B (`docs/rfcs/0001-type-safety-newtypes.md`).
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
        self.0 += 1;
        s
    }

    /// Advance the counter by `n` after a batch push of `n` frames.
    #[inline]
    pub fn advance_by(&mut self, n: u64) {
        self.0 += n;
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

// ─── Session entry ───────────────────────────────────────────────────────────

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

    /// Max dedup entries — when exceeded, oldest entries are evicted FIFO.
    dedup_max: usize,
    /// FIFO queue of (request_id) for dedup eviction ordering.
    dedup_order: VecDeque<u64>,

    /// Wall-clock of last session activity; used for TTL expiration.
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
        let gap_boundary = self
            .highest_evicted_seq
            .max(self.last_acked_server_seq);
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

    /// Insert a response into the dedup table (with FIFO eviction if full).
    #[allow(clippy::map_entry)] // Entry API makes the two-phase (update vs insert+FIFO) logic less clear.
    fn dedup_insert(&mut self, request_id: u64, response: Bytes) {
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

// ─── Sharded SessionStore ────────────────────────────────────────────────────

type Shard = Mutex<FxHashMap<SessionId, SessionEntry>>;

pub struct SessionStore {
    shards: Arc<[Shard; NUM_SHARDS]>,
    buffer_min: usize,
    buffer_max: usize,
    dedup_max: usize,
    ttl: Duration,
}

impl SessionStore {
    /// Create a new store with the given MAX replay buffer capacity and TTL.
    ///
    /// Each session's buffer starts at `min(capacity, DEFAULT_BUFFER_MIN=1024)`
    /// and grows adaptively up to `capacity`. Once `capacity` is reached,
    /// oldest frames are evicted to make room (slow-consumer behavior).
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self::new_adaptive(capacity.min(DEFAULT_BUFFER_MIN), capacity, DEFAULT_DEDUP_MAX, ttl)
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
        assert!(
            buffer_min <= buffer_max,
            "buffer_min must be <= buffer_max"
        );
        let shards: [Shard; NUM_SHARDS] =
            std::array::from_fn(|_| Mutex::new(FxHashMap::default()));
        Self {
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
        let shard = self.shard_for(id);
        let mut map = Self::lock_shard(shard, id, "resume");
        let Some(entry) = map.get_mut(id) else {
            return ReplayResult::SessionNotFound;
        };
        if entry.is_expired() {
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
        // Defensive hardening: `next_outbound_seq()` computes `highest + 1`.
        // If a malicious client sends ACK=u64::MAX and we stored it verbatim,
        // that addition would overflow and could crash resume handling.
        let sanitized_last_acked = if last_acked_server_seq == u64::MAX {
            tracing::warn!(
                session = ?id,
                "ignoring impossible ACK=u64::MAX to preserve seq>=1 invariant"
            );
            u64::MAX - 1
        } else {
            last_acked_server_seq
        };
        let before = entry.frames.len();
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
    /// **Must be called after `resume()` returns Ok/Gap** to re-synchronize
    /// the connection's `server_frame_seq` counter. Without this, a new
    /// connection after reconnect would restart seq numbering and collide
    /// with frames already present in the replay buffer. Seq=0 is reserved
    /// as the "unsent" sentinel, so the minimum returned value is 1.
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
        for (idx, shard) in self.shards.iter().enumerate() {
            let mut map = Self::lock_shard_by_idx(shard, idx, "gc");
            map.retain(|_, entry| !entry.is_expired());
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
            shards: Arc::clone(&self.shards),
            buffer_min: self.buffer_min,
            buffer_max: self.buffer_max,
            dedup_max: self.dedup_max,
            ttl: self.ttl,
        }
    }
}
