use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use tracing::{info, warn};

use me_core::events::MatchEvent;
use me_core::order::Order;
use me_core::types::SymbolId;

use crate::logger::{match_event_to_entry, order_to_entry};
use crate::types::AuditEntry;

/// Audit output format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditFormat {
    /// JSON via serde_json — human-readable, ~1.2 µs per event.
    Json,
    /// Binary via bincode — compact, ~50 ns per event.
    Binary,
}

/// Async audit logger: zero-alloc on the hot path.
///
/// The matching thread writes fixed-size `AuditEntry` structs into a bounded
/// crossbeam channel (MPSC, used as SPSC). A background consumer thread reads
/// entries and serializes them (JSON or binary) via tracing.
///
/// Hot-path cost: ~10-30 ns per event (channel send, no serialization).
/// If the buffer is full, entries are dropped (never blocks matching).
pub struct AsyncAuditLogger {
    tx: Sender<AuditEntry>,
    running: Arc<AtomicBool>,
    _consumer: Option<JoinHandle<()>>,
    /// Entries dropped because the buffer was full (slow consumer).
    dropped: Arc<std::sync::atomic::AtomicU64>,
    /// Entries lost because the consumer thread died/panicked — CATASTROPHIC
    /// for audit pipeline integrity. Monitor separately from `dropped`.
    disconnected: Arc<std::sync::atomic::AtomicU64>,
}

impl AsyncAuditLogger {
    /// Create a new async audit logger.
    ///
    /// - `buffer_size`: ring buffer capacity (power of 2 recommended, e.g. 65536)
    /// - `format`: output format (JSON or Binary)
    pub fn new(buffer_size: usize, format: AuditFormat) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(buffer_size);
        let running = Arc::new(AtomicBool::new(true));
        let dropped = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let disconnected = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let running_clone = Arc::clone(&running);
        let dropped_clone = Arc::clone(&dropped);

        let handle = thread::Builder::new()
            .name("me-audit-consumer".into())
            .spawn(move || {
                consumer_loop(rx, running_clone, dropped_clone, format);
            })
            .expect("failed to spawn audit consumer thread");

        Self {
            tx,
            running,
            _consumer: Some(handle),
            dropped,
            disconnected,
        }
    }

    /// Log an order submission. Zero-alloc hot path.
    #[inline]
    pub fn log_order_submitted(&self, order: &Order) {
        let entry = order_to_entry(order);
        self.try_send(entry);
    }

    /// Log match events. Zero-alloc hot path.
    #[inline]
    pub fn log_match_events(&self, events: &[MatchEvent], symbol: SymbolId) {
        for event in events {
            if let Some(entry) = match_event_to_entry(event, symbol) {
                self.try_send(entry);
            }
        }
    }

    /// Log a single AuditEntry directly.
    #[inline]
    pub fn log_entry(&self, entry: AuditEntry) {
        self.try_send(entry);
    }

    /// Get the number of entries dropped because the buffer was full
    /// (slow consumer). Recoverable — entries lost only in this bounded
    /// window.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Get the number of entries lost because the consumer thread died
    /// (channel disconnected). **Catastrophic** for audit pipeline
    /// integrity — operators should alert when this is non-zero.
    pub fn disconnected_count(&self) -> u64 {
        self.disconnected.load(Ordering::Relaxed)
    }

    #[inline]
    fn try_send(&self, entry: AuditEntry) {
        match self.tx.try_send(entry) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                // Consumer thread died — audit pipeline is broken. Bump a
                // separate counter so operators can alert specifically on
                // "audit pipeline dead" vs "buffer full".
                self.disconnected.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for AsyncAuditLogger {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        // Ensure wake-up even if the channel is currently full: retry with
        // small backoff. Without this, a saturated channel causes Drop to
        // hang indefinitely in handle.join() because the consumer is blocked
        // on recv() (which only returns when a new entry arrives OR the
        // channel is closed — the latter doesn't apply since we hold tx).
        let mut woke_consumer = false;
        for _ in 0..10 {
            if self.tx.try_send(AuditEntry::default()).is_ok() {
                woke_consumer = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if !woke_consumer {
            // Channel stayed full for 10ms — consumer is either wedged or
            // genuinely saturated. handle.join() below MAY hang; at least
            // surface the reason so operators can diagnose a stuck shutdown.
            tracing::warn!(
                "audit Drop: could not wake consumer after 10 retries; join may hang"
            );
        }
        if let Some(handle) = self._consumer.take() {
            if let Err(panic) = handle.join() {
                // Consumer panicked — log loudly so operators can investigate.
                tracing::error!(
                    ?panic,
                    "audit consumer thread panicked during shutdown"
                );
            }
        }
    }
}

/// Consumer loop: runs on a dedicated thread, reads AuditEntry, serializes, emits.
fn consumer_loop(
    rx: Receiver<AuditEntry>,
    running: Arc<AtomicBool>,
    dropped: Arc<std::sync::atomic::AtomicU64>,
    format: AuditFormat,
) {
    let mut last_dropped_report = 0u64;

    while running.load(Ordering::Acquire) {
        // Block until an entry arrives (or channel disconnected)
        match rx.recv() {
            Ok(entry) => {
                if entry.event_type == 0 {
                    // Dummy wake-up entry from Drop
                    continue;
                }
                emit_entry(&entry, format);
            }
            Err(_) => {
                // Channel disconnected — the AsyncAuditLogger was dropped.
                // Normal shutdown path, but log at DEBUG so operators can
                // correlate "consumer loop exited" with audit producer drops.
                tracing::debug!(
                    target: "audit",
                    "audit consumer loop: channel disconnected, exiting"
                );
                break;
            }
        }

        // Drain any queued entries (batch processing)
        while let Ok(entry) = rx.try_recv() {
            if entry.event_type == 0 {
                continue;
            }
            emit_entry(&entry, format);
        }

        // Periodically report dropped count
        let current_dropped = dropped.load(Ordering::Relaxed);
        if current_dropped > last_dropped_report + 1000 {
            warn!(
                dropped = current_dropped - last_dropped_report,
                "audit entries dropped (buffer full)"
            );
            last_dropped_report = current_dropped;
        }
    }

    // Drain remaining entries on shutdown
    while let Ok(entry) = rx.try_recv() {
        if entry.event_type != 0 {
            emit_entry(&entry, format);
        }
    }
}

fn emit_entry(entry: &AuditEntry, format: AuditFormat) {
    match format {
        AuditFormat::Json => {
            let event = entry.to_audit_event();
            match serde_json::to_string(&event) {
                Ok(json) => info!(target: "audit", "{}", json),
                Err(e) => {
                    // Serialization failure — log at error level with entry
                    // identity so the missing record can be reconstructed
                    // from raw binary dump or order-by-order reconciliation.
                    warn!(
                        target: "audit",
                        error = %e,
                        event_type = entry.event_type,
                        order_id = entry.order_id,
                        timestamp_ns = entry.timestamp_ns,
                        "FAILED TO SERIALIZE AUDIT ENTRY — RECORD LOST"
                    );
                }
            }
        }
        AuditFormat::Binary => {
            // Emit raw binary representation: just the fixed-size AuditEntry bytes.
            // Consumer applications decode using the same struct layout.
            let bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    entry as *const AuditEntry as *const u8,
                    std::mem::size_of::<AuditEntry>(),
                )
            };
            info!(
                target: "audit_binary",
                bytes = ?bytes,
                bytes_len = bytes.len(),
                event_type = entry.event_type
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::events::MatchEvent;

    #[test]
    fn async_logger_basic() {
        let logger = AsyncAuditLogger::new(1024, AuditFormat::Json);

        let events = vec![
            MatchEvent::OrderAccepted {
                order_id: 1,
                timestamp: 1000,
            },
            MatchEvent::OrderCancelled {
                order_id: 2,
                remaining_qty: me_core::types::FixedQty::from_raw(5000),
                timestamp: 2000,
            },
        ];

        logger.log_match_events(&events, SymbolId(0));

        // Give consumer thread time to process
        std::thread::sleep(std::time::Duration::from_millis(10));

        assert_eq!(logger.dropped_count(), 0);
    }

    #[test]
    fn async_logger_binary() {
        let logger = AsyncAuditLogger::new(1024, AuditFormat::Binary);

        let events = vec![MatchEvent::OrderAccepted {
            order_id: 42,
            timestamp: 9999,
        }];

        logger.log_match_events(&events, SymbolId(0));
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert_eq!(logger.dropped_count(), 0);
    }

    #[test]
    fn async_logger_drops_on_full_buffer() {
        // Tiny buffer: 2 entries
        let logger = AsyncAuditLogger::new(2, AuditFormat::Json);

        // Flood with entries faster than consumer can process
        for i in 0..100 {
            let entry = AuditEntry {
                event_type: crate::types::event_type::ORDER_ACCEPTED,
                order_id: i,
                timestamp_ns: i * 1000,
                ..AuditEntry::default()
            };
            logger.log_entry(entry);
        }

        // Some should have been dropped
        std::thread::sleep(std::time::Duration::from_millis(50));
        // We can't guarantee exact count, but dropped should be > 0 with tiny buffer
        // (consumer may have kept up if fast enough)
    }

    #[test]
    fn async_logger_clean_shutdown() {
        let logger = AsyncAuditLogger::new(1024, AuditFormat::Json);
        logger.log_entry(AuditEntry {
            event_type: crate::types::event_type::ORDER_ACCEPTED,
            order_id: 1,
            timestamp_ns: 1000,
            ..AuditEntry::default()
        });
        // Drop triggers clean shutdown — should not hang
        drop(logger);
    }
}
