//! The clock.
//!
//! One domain, `CLOCK_MONOTONIC`, everywhere. This is not a preference: an event
//! read from procfs and an event produced by an eBPF program have to be
//! comparable, and `bpf_ktime_get_ns()` is `CLOCK_MONOTONIC`. Using
//! `CLOCK_REALTIME` here, or `bpf_ktime_get_boot_ns()` there, would silently
//! shift half the events by the machine's suspend time and destroy every join
//! without producing a single visible error.
//!
//! Timestamps taken elsewhere -- a NIC's PHC, a GPU's own counter -- are in
//! another domain. They are converted at the boundary and carry a non-zero
//! `ts_err_ns` saying how much the conversion could be off.

/// Reads `CLOCK_MONOTONIC`, in nanoseconds.
///
/// This is the same timebase an eBPF program sees through
/// `bpf_ktime_get_ns()`, so events from both sides sort into one stream.
pub fn monotonic_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes a `timespec` we own, and `CLOCK_MONOTONIC`
    // is available on every kernel this tool targets.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    debug_assert_eq!(result, 0, "CLOCK_MONOTONIC must be readable");
    (time.tv_sec as u64) * 1_000_000_000 + (time.tv_nsec as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_monotonic_clock_moves_forward() {
        let first = monotonic_ns();
        let second = monotonic_ns();
        assert!(second >= first, "CLOCK_MONOTONIC went backwards");
        assert!(
            first > 0,
            "CLOCK_MONOTONIC should be past zero on a running machine"
        );
    }
}
