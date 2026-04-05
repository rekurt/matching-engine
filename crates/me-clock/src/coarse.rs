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
/// Accuracy: ±100 µs (sufficient for order timestamps).
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
    fn coarse_clock_updates() {
        let clock = CoarseClock::new();
        let t1 = clock.now_ns();
        // Wait longer than the update interval (100µs)
        thread::sleep(Duration::from_millis(2));
        let t2 = clock.now_ns();
        assert!(t2 > t1, "clock should advance: t1={t1}, t2={t2}");
    }

    #[test]
    fn coarse_clock_accuracy() {
        let clock = CoarseClock::new();
        thread::sleep(Duration::from_millis(1)); // let updater run once
        let coarse = clock.now_ns();
        let system = CoarseClock::wall_clock_ns();
        let diff = coarse.abs_diff(system);
        // Should be within 1ms (generous, actual is ±100µs)
        assert!(
            diff < 1_000_000,
            "coarse clock too far from system: diff={diff} ns"
        );
    }
}
