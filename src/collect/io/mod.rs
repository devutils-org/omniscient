//! Collectors for the `io` family.
//!
//! The family spans three layers that must never be reported as one another:
//! what the application asked for (the syscall), what the filesystem did with
//! it (journal, locks, allocation), and what the device actually performed (the
//! block layer). The gap between them is the diagnosis -- a read that takes
//! 40 ms at the syscall and 2 ms at the device was not slow because of the
//! disk -- so each layer keeps its own sources.
//!
//! Two mechanisms live here and they answer different questions. The procfs
//! collectors read the block layer's counters: free, system-wide, exact, and
//! silent about who caused the I/O. The eBPF collectors watch the block layer's
//! own tracepoints and see one request at a time -- which device, which task,
//! how long it queued and how long the device took.
//!
//! The two are not a fallback for each other and the catalog does not treat
//! them as one. `/proc/diskstats` cannot say which request was slow and the
//! tracepoints cannot see a device nobody is using, so where a source has both
//! they are ordered by which answers its question better and the entry says
//! what the cheaper one gives up.

mod block;
use block::histogram_is_enough;
mod diskstats;
mod fsync;
pub(super) mod histogram;
mod pagecache;
mod pressure;
pub(super) mod vfs;
mod writeback;

/// Whether this request's collector builds a log2 histogram in the kernel and
/// publishes no record per request, so that its rows are `(device, bucket)`
/// and carry no key but `dev`.
///
/// Only `io.latency` does: `io.queue` and `io.size` stream every request
/// whatever they emit, and so does every other family's source. The validator
/// reads this to refuse a join that would match nothing
/// (`validate::check_joins`), and must not refuse one that works.
pub(crate) fn builds_histogram_in_kernel(request: &crate::request::Request) -> bool {
    request.source.as_str() == "io.latency" && histogram_is_enough(request)
}

use std::collections::BTreeMap;

use super::Factory;
use crate::bpf::attach::io_vfs::Layer;
use block::Measure;
use diskstats::Metric;

/// The name the kernel gives each `major:minor`, read from `/proc/diskstats`.
///
/// The eBPF collectors get a packed `dev_t` out of the kernel and a reader
/// wants a name, and `dev` is a text attribute everywhere else in this family
/// -- so the two mechanisms have to agree about what a device is called.
/// `/sys/dev/block/<major>:<minor>` would answer the same question one device
/// at a time; this answers it for every device in one read, which is what a
/// drain of a few thousand records wants.
///
/// The pair is what an event's key carries and the name is what a reader sees,
/// deliberately: a device name is a symlink away from meaning something else.
fn device_names() -> BTreeMap<(u32, u32), String> {
    let Ok(text) = std::fs::read_to_string("/proc/diskstats") else {
        return BTreeMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let major = fields.next()?.parse().ok()?;
            let minor = fields.next()?.parse().ok()?;
            Some(((major, minor), fields.next()?.to_owned()))
        })
        .collect()
}

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
        // The block layer's own tracepoints. These three share one program
        // shape and are the only sources in the project that produce
        // `blk:rq@open_ts`, which is the key `scoped_eq` joins on.
        Factory::new("io.latency", "bpf:raw_tp", |resolved| {
            Ok(Box::new(block::Prepare {
                measure: Measure::Latency,
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
                in_kernel_histogram: block::histogram_is_enough(&resolved.request),
            }))
        }),
        Factory::new("io.queue", "bpf:raw_tp", |resolved| {
            Ok(Box::new(block::Prepare {
                measure: Measure::Queue,
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
                in_kernel_histogram: false,
            }))
        }),
        Factory::new("io.size", "bpf:raw_tp", |resolved| {
            Ok(Box::new(block::Prepare {
                measure: Measure::Size,
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
                in_kernel_histogram: false,
            }))
        }),
        // The two layers above the block one, over fentry and bucketed in the
        // kernel: the call the application made, and the part of it the VFS
        // and the filesystem spent on a regular file.
        Factory::new("io.syscall", "bpf:fentry", |resolved| {
            Ok(Box::new(vfs::Prepare {
                layer: Layer::Syscall,
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("io.fs", "bpf:fentry", |resolved| {
            Ok(Box::new(vfs::Prepare {
                layer: Layer::Fs,
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("io.fsync", "bpf:fentry", |resolved| {
            Ok(Box::new(fsync::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        // The page cache, from the two tracepoints on either side of a read:
        // what it looked up and what the cache had to fill. One program pair,
        // read two ways.
        Factory::new("io.pagecache", "bpf:raw_tp", |resolved| {
            Ok(Box::new(pagecache::Prepare {
                reading: pagecache::Reading::Misses,
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("io.readahead", "bpf:raw_tp", |resolved| {
            Ok(Box::new(pagecache::Prepare {
                reading: pagecache::Reading::Ahead,
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        // The flusher, which is the other half of a write and belongs to no
        // process at all.
        Factory::new("io.writeback", "bpf:raw_tp", |resolved| {
            Ok(Box::new(writeback::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
    ]
}
