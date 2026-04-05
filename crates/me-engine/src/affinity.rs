//! Thread affinity and priority utilities for HFT workloads.
//!
//! On Linux, pins threads to specific CPU cores (NUMA-aware) and optionally
//! sets real-time scheduling priority. On other platforms, these are no-ops.

use tracing::debug;

#[cfg(target_os = "linux")]
use tracing::warn;

/// Pin the current thread to a specific CPU core.
///
/// On Linux: uses `sched_setaffinity` syscall.
/// On macOS/Windows: no-op (logged as debug).
///
/// # Arguments
/// - `core_id`: logical CPU core index (0-based)
pub fn pin_to_core(core_id: usize) {
    #[cfg(target_os = "linux")]
    {
        unsafe {
            let mut cpuset: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_SET(core_id, &mut cpuset);
            let result = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &cpuset);
            if result == 0 {
                debug!(core_id, "thread pinned to core");
            } else {
                warn!(core_id, error = %std::io::Error::last_os_error(), "failed to pin thread");
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        debug!(core_id, "thread pinning not supported on this platform (no-op)");
    }
}

/// Set real-time scheduling priority for the current thread.
///
/// On Linux: uses `sched_setscheduler` with `SCHED_FIFO` and priority 1.
/// Requires root or `CAP_SYS_NICE` capability.
/// On other platforms: no-op.
///
/// Returns `Ok(())` on success or if not supported, `Err` on failure.
pub fn set_realtime_priority() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        unsafe {
            let param = libc::sched_param { sched_priority: 1 };
            let result = libc::sched_setscheduler(0, libc::SCHED_FIFO, &param);
            if result == 0 {
                debug!("thread set to SCHED_FIFO priority 1");
                Ok(())
            } else {
                let err = std::io::Error::last_os_error();
                warn!(error = %err, "failed to set real-time priority (requires root or CAP_SYS_NICE)");
                Err(format!("sched_setscheduler failed: {err}"))
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        debug!("real-time priority not supported on this platform (no-op)");
        Ok(())
    }
}

/// Get the number of available CPU cores.
pub fn available_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pin_to_core_does_not_panic() {
        // Should not panic on any platform (no-op on non-Linux)
        pin_to_core(0);
    }

    #[test]
    fn test_pin_to_core_high_id() {
        // High core ID — may fail on Linux if core doesn't exist, but should not panic
        pin_to_core(9999);
    }

    #[test]
    fn test_set_realtime_priority_returns_result() {
        // On non-Linux: Ok(()). On Linux without root: Err.
        let result = set_realtime_priority();
        // We don't assert Ok/Err because it depends on permissions
        let _ = result;
    }

    #[test]
    fn test_available_cores() {
        let cores = available_cores();
        assert!(cores >= 1);
    }
}
