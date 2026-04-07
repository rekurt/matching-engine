use crate::Clock;

/// RDTSC-based clock for x86_64.
///
/// Uses the CPU's Time Stamp Counter (TSC) instruction for ultra-low-latency
/// timestamps (~5 ns). Calibrated at construction time against the wall clock.
///
/// On non-x86_64 architectures, this falls back to [`crate::CoarseClock`].
///
/// # Accuracy
///
/// TSC frequency is measured once at startup. On modern CPUs with invariant TSC
/// (Intel Nehalem+, AMD Zen+), drift is minimal. On older CPUs or VMs with
/// unstable TSC, timestamps may drift over hours.
#[cfg(target_arch = "x86_64")]
pub struct RdtscClock {
    tsc_base: u64,
    nanos_base: u64,
    /// Nanoseconds per TSC tick (precomputed reciprocal).
    ns_per_tick: f64,
}

#[cfg(target_arch = "x86_64")]
impl RdtscClock {
    /// Create a new RDTSC clock, calibrating against the wall clock.
    ///
    /// Calibration takes ~10 ms (sleeps to measure TSC frequency).
    pub fn new() -> Self {
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        // Record baseline
        let tsc_start = Self::rdtsc();
        let wall_start = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        // Sleep to measure TSC frequency
        std::thread::sleep(Duration::from_millis(10));

        let tsc_end = Self::rdtsc();
        let wall_end = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        let tsc_delta = tsc_end - tsc_start;
        let wall_delta = wall_end - wall_start;

        let ns_per_tick = wall_delta as f64 / tsc_delta as f64;

        Self {
            tsc_base: tsc_start,
            nanos_base: wall_start,
            ns_per_tick,
        }
    }

    #[inline]
    fn rdtsc() -> u64 {
        unsafe { core::arch::x86_64::_rdtsc() }
    }
}

#[cfg(target_arch = "x86_64")]
impl Clock for RdtscClock {
    #[inline]
    fn now_ns(&self) -> u64 {
        let tsc_now = Self::rdtsc();
        let tsc_delta = tsc_now.wrapping_sub(self.tsc_base);
        self.nanos_base + (tsc_delta as f64 * self.ns_per_tick) as u64
    }
}

#[cfg(target_arch = "x86_64")]
impl Default for RdtscClock {
    fn default() -> Self {
        Self::new()
    }
}

// --- Fallback for non-x86_64 (ARM, etc.) ---

#[cfg(not(target_arch = "x86_64"))]
pub struct RdtscClock {
    inner: crate::CoarseClock,
}

#[cfg(not(target_arch = "x86_64"))]
impl RdtscClock {
    pub fn new() -> Self {
        Self {
            inner: crate::CoarseClock::new(),
        }
    }
}

#[cfg(not(target_arch = "x86_64"))]
impl Clock for RdtscClock {
    #[inline]
    fn now_ns(&self) -> u64 {
        self.inner.now_ns()
    }
}

#[cfg(not(target_arch = "x86_64"))]
impl Default for RdtscClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rdtsc_clock_returns_nonzero() {
        let clock = RdtscClock::new();
        let t = clock.now_ns();
        assert!(t > 0);
    }

    #[test]
    fn rdtsc_clock_monotonic() {
        let clock = RdtscClock::new();
        let t1 = clock.now_ns();
        let t2 = clock.now_ns();
        assert!(t2 >= t1);
    }

    #[test]
    fn rdtsc_clock_accuracy() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let clock = RdtscClock::new();
        std::thread::sleep(std::time::Duration::from_millis(10));

        let rdtsc_ns = clock.now_ns();
        let system_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        let diff = rdtsc_ns.abs_diff(system_ns);
        // Should be within 5ms (generous for test stability)
        assert!(
            diff < 5_000_000,
            "RDTSC too far from system: diff={diff} ns"
        );
    }
}
