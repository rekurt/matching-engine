//! High-performance clock implementations for the matching engine.
//!
//! Provides a `Clock` trait with three backends:
//! - [`SystemClock`] — wraps `SystemTime::now()`, nanosecond accuracy, ~25 ns
//! - [`CoarseClock`] — atomic load from a background-updated counter, ~1 ns, ±100µs accuracy
//! - [`RdtscClock`] — x86_64 `RDTSC` instruction, ~5 ns, ~10 ns accuracy (x86_64 only)

mod coarse;
mod rdtsc;

pub use coarse::CoarseClock;
pub use rdtsc::RdtscClock;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Clock trait for timestamping in the matching engine hot path.
///
/// All implementations return nanoseconds since UNIX epoch.
/// Designed for zero-alloc, zero-syscall operation on the hot path.
pub trait Clock: Send + Sync + 'static {
    /// Current time in nanoseconds since UNIX epoch.
    fn now_ns(&self) -> u64;
}

/// Blanket impl: `Arc<dyn Clock>` is itself a `Clock`.
impl Clock for Arc<dyn Clock> {
    #[inline]
    fn now_ns(&self) -> u64 {
        (**self).now_ns()
    }
}

/// Wall-clock via `SystemTime::now()`. Accurate but slow (~25 ns syscall).
/// Use for tests and non-latency-sensitive paths.
pub struct SystemClock;

impl Clock for SystemClock {
    #[inline]
    fn now_ns(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
    }
}

/// Create a clock from a configuration string.
///
/// - `"system"` → [`SystemClock`]
/// - `"coarse"` → [`CoarseClock`] (default)
/// - `"rdtsc"` → [`RdtscClock`] on x86_64, falls back to `CoarseClock` on other architectures
pub fn create_clock(kind: &str) -> Arc<dyn Clock> {
    match kind {
        "system" => Arc::new(SystemClock),
        "rdtsc" => {
            #[cfg(target_arch = "x86_64")]
            {
                Arc::new(RdtscClock::new())
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                Arc::new(CoarseClock::new())
            }
        }
        _ => Arc::new(CoarseClock::new()), // "coarse" and any unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_returns_nonzero() {
        let clock = SystemClock;
        let t = clock.now_ns();
        assert!(t > 0);
    }

    #[test]
    fn system_clock_monotonic_ish() {
        let clock = SystemClock;
        let t1 = clock.now_ns();
        let t2 = clock.now_ns();
        assert!(t2 >= t1);
    }

    #[test]
    fn create_clock_system() {
        let clock = create_clock("system");
        assert!(clock.now_ns() > 0);
    }

    #[test]
    fn create_clock_coarse() {
        let clock = create_clock("coarse");
        assert!(clock.now_ns() > 0);
    }

    #[test]
    fn create_clock_rdtsc() {
        let clock = create_clock("rdtsc");
        assert!(clock.now_ns() > 0);
    }

    #[test]
    fn create_clock_default() {
        let clock = create_clock("unknown");
        assert!(clock.now_ns() > 0);
    }
}
