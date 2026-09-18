//! Collectors for the `io` family.
//!
//! The family spans three layers that must never be reported as one another:
//! what the application asked for (the syscall), what the filesystem did with
//! it (journal, locks, allocation), and what the device actually performed (the
//! block layer). The gap between them is the diagnosis -- a read that takes
//! 40 ms at the syscall and 2 ms at the device was not slow because of the
//! disk -- so each layer keeps its own sources.
//!
//! What is here reads the block layer's counters out of procfs. It is free,
//! it is system-wide, and it says nothing about who caused the I/O; the
//! per-request, per-task view is the eBPF backends' job and arrives with that
//! layer.

mod diskstats;
mod pressure;

use super::Factory;
use diskstats::Metric;

/// The collectors this family provides.
pub fn factories() -> Vec<Factory> {
    vec![
        Factory::new("io.bytes", "fs:proc", |resolved| {
            Ok(Box::new(diskstats::Prepare::new(
                Metric::Bytes,
                resolved.request.mode,
            )?))
        }),
        Factory::new("io.iops", "fs:proc", |resolved| {
            Ok(Box::new(diskstats::Prepare::new(
                Metric::Iops,
                resolved.request.mode,
            )?))
        }),
        Factory::new("io.merges", "fs:proc", |resolved| {
            Ok(Box::new(diskstats::Prepare::new(
                Metric::Merges,
                resolved.request.mode,
            )?))
        }),
        Factory::new("io.inflight", "fs:proc", |resolved| {
            Ok(Box::new(diskstats::Prepare::new(
                Metric::Inflight,
                resolved.request.mode,
            )?))
        }),
        Factory::new("io.pressure", "fs:proc", |resolved| {
            Ok(Box::new(pressure::Prepare::new(resolved.request.mode)?))
        }),
    ]
}
