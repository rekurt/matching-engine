use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::Clock;

/// High-performance coarse clock.
///
/// A background thread updates an `AtomicU64` every ~100 µs with the current
/// wall-clock time. Reading the clock is a single `Relaxed` atomic load (~1 ns).
///
/// The refresh target is 100 µs; scheduling delays can make cached values older.
/// Latency:  ~1-2 ns (atomic load, no syscall).
pub struct CoarseClock {
    nanos: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
}

impl CoarseClock {
    /// Create and start the coarse clock.
    /// Spawns a background thread that updates the timestamp every ~100 µs.
    pub fn new() -> Self {
        let nanos = Arc::new(AtomicU64::new(Self::wall_clock_ns()));
        let running = Arc::new(AtomicBool::new(true));

        let nanos_clone = Arc::clone(&nanos);
        let running_clone = Arc::clone(&running);

        thread::Builder::new()
            .name("me-clock-coarse".into())
            .spawn(move || {
                while running_clone.load(Ordering::Relaxed) {
                    nanos_clone.store(Self::wall_clock_ns(), Ordering::Relaxed);
                    thread::sleep(Duration::from_micros(100));
                }
            })
            .expect("failed to spawn coarse clock thread");

        Self { nanos, running }
    }

    #[inline]
    fn wall_clock_ns() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
    }
}

impl Clock for CoarseClock {
    #[inline]
    fn now_ns(&self) -> u64 {
        self.nanos.load(Ordering::Relaxed)
    }
}

impl Default for CoarseClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CoarseClock {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coarse_clock_returns_nonzero() {
        let clock = CoarseClock::new();
        let t = clock.now_ns();
        assert!(t > 0);
    }

    #[test]
    fn coarse_clock_updates_from_wall_time() {
        let before = CoarseClock::wall_clock_ns();
        let clock = CoarseClock::new();
        let initial = clock.now_ns();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let updated = loop {
            let current = clock.now_ns();
            if current > initial {
                break current;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "background clock did not update"
            );
            thread::sleep(Duration::from_micros(100));
        };
        let after = CoarseClock::wall_clock_ns();
        assert!(
            updated >= before && updated <= after,
            "clock must report sampled wall time"
        );
    }
}
