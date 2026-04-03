//! Session store: per-session outbound frame buffer enabling reconnect replay.
//!
//! After a pod restart or network drop, a client can reconnect, present its
//! `session_id` and the index of the last frame it received, and the server will
//! replay any frames the client missed.
//!
//! Design:
//! - Sessions are keyed by a 16-byte random ID issued in HandshakeAck.
//! - Each session keeps a fixed-capacity ring buffer of raw outbound frame bytes.
//! - Frames are identified by a monotonic `server_seq` counter per connection.
//! - Expired sessions are cleaned up lazily on the next `gc()` call.

use std::collections::VecDeque;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use rand::Rng;

pub type SessionId = [u8; 16];

// ─── Session entry ───────────────────────────────────────────────────────────

struct SessionEntry {
    /// Ring buffer of (server_seq, raw_frame_bytes).
    frames: VecDeque<(u64, Bytes)>,
    /// Capacity limit (oldest frames evicted when full).
    capacity: usize,
    /// When session was created or last resumed (for TTL).
    last_access: Instant,
    /// How long before an idle session expires.
    ttl: Duration,
}

impl SessionEntry {
    fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            frames: VecDeque::with_capacity(capacity.min(256)),
            capacity,
            last_access: Instant::now(),
            ttl,
        }
    }

    fn push(&mut self, seq: u64, frame: Bytes) {
        if self.frames.len() >= self.capacity {
            self.frames.pop_front();
        }
        self.frames.push_back((seq, frame));
    }

    fn is_expired(&self) -> bool {
        self.last_access.elapsed() > self.ttl
    }

    /// Return frames with seq > last_received, in order.
    fn frames_after(&self, last_received: u64) -> Vec<Bytes> {
        self.frames
            .iter()
            .filter(|(seq, _)| *seq > last_received)
            .map(|(_, frame)| frame.clone())
            .collect()
    }
}

// ─── SessionStore ────────────────────────────────────────────────────────────

pub struct SessionStore {
    inner: Arc<Mutex<SessionStoreInner>>,
    capacity: usize,
    ttl: Duration,
}

struct SessionStoreInner {
    sessions: HashMap<SessionId, SessionEntry>,
}

impl SessionStore {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SessionStoreInner {
                sessions: HashMap::new(),
            })),
            capacity,
            ttl,
        }
    }

    /// Create and register a new session. Returns the new session ID.
    pub fn create(&self) -> SessionId {
        let mut id = [0u8; 16];
        rand::rng().fill(&mut id);

        let entry = SessionEntry::new(self.capacity, self.ttl);
        let mut inner = self.inner.lock().expect("session store lock");
        inner.sessions.insert(id, entry);
        id
    }

    /// Push an outbound frame to the session's ring buffer.
    ///
    /// `server_seq` is the connection-local monotonic counter; the client will
    /// send this back in `ResumeSession.last_recv_seq`.
    pub fn push(&self, id: &SessionId, server_seq: u64, frame: Bytes) {
        let mut inner = self.inner.lock().expect("session store lock");
        if let Some(entry) = inner.sessions.get_mut(id) {
            entry.push(server_seq, frame);
        }
    }

    /// Resume a session: return frames the client hasn't received yet.
    ///
    /// Returns `None` if the session is not found or has expired.
    pub fn resume(&self, id: &SessionId, last_recv_seq: u64) -> Option<Vec<Bytes>> {
        let mut inner = self.inner.lock().expect("session store lock");
        let entry = inner.sessions.get_mut(id)?;
        if entry.is_expired() {
            inner.sessions.remove(id);
            return None;
        }
        entry.last_access = Instant::now();
        Some(entry.frames_after(last_recv_seq))
    }

    /// Remove expired sessions. Call periodically (e.g., in a background task).
    pub fn gc(&self) {
        let mut inner = self.inner.lock().expect("session store lock");
        inner.sessions.retain(|_, entry| !entry.is_expired());
    }
}

impl Clone for SessionStore {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            capacity: self.capacity,
            ttl: self.ttl,
        }
    }
}
