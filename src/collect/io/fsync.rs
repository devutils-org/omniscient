//! `io.fsync` over fentry: one event per sync, keyed by the file it made
//! durable.
//!
//! `src/bpf/io_fsync.rs` has the program and why it streams. What this file
//! decides is what the event says. The file key is `file:<dev>+<ino>` with the
//! superblock's device in `stat`'s encoding -- the same key `io.fs` produces
//! for the reads and writes of that file, so the two join on it -- and the
//! call is named by what it was: `fdatasync` when the kernel was told the
//! metadata could wait, `fsync` otherwise.

use std::time::Duration as StdDuration;

use super::vfs::Mounts;
use crate::bpf::fentry::Narrowing;
use crate::bpf::io_fsync::{Fsync, FsyncProgram, Via};
use crate::bpf::io_vfs;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the ring is drained. A sync is rare, but a thread that syncs a
/// clean file gets its answer in a microsecond or two, and many of them at once
/// fill a ring fast.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// `S_IFDIR` and `S_IFREG`, for naming what was synced.
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;

pub struct Prepare {
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot measure how long a sync took: each one is the gap between the \
                 two halves of one call, and `io.fsync` reports them one at a time",
                self.mode
            ));
        }
        let narrowing = super::vfs::narrowing("io.fsync", target)?;
        Ok(Box::new(FsyncCollector {
            narrowing,
            program: None,
            losses: Losses::default(),
            predicate: self.filter,
            mounts: None,
            dropped_pending: 0,
            dropped_ring: 0,
            orphaned: 0,
            missed: 0,
            unfollowed_said: false,
            seen: 0,
            failed: 0,
            filtered_out: 0,
            still_pending: 0,
            unnamed: 0,
            by_descriptor: true,
        }))
    }
}

pub struct FsyncCollector {
    narrowing: Narrowing,
    program: Option<FsyncProgram>,
    losses: Losses,
    predicate: Option<FilterExpr>,
    mounts: Option<Mounts>,
    dropped_pending: u64,
    dropped_ring: u64,
    orphaned: u64,
    missed: u64,
    /// Whether the run has said that the descendants map overflowed.
    unfollowed_said: bool,
    seen: u64,
    failed: u64,
    filtered_out: u64,
    still_pending: usize,
    /// Syncs whose file the descriptor walk could not find.
    unnamed: u64,
    /// Whether this kernel had `do_fsync` to time.
    by_descriptor: bool,
}

impl FsyncCollector {
    fn event(&mut self, record: &Fsync, seq: u64) -> Event {
        let dev = io_vfs::major_minor(record.dev);
        let (fs, _, device) = self.mounts.get_or_insert_with(Mounts::read).lookup(dev);
        let call = if record.datasync != 0 {
            "fdatasync"
        } else {
            "fsync"
        };
        let kind = match record.mode {
            S_IFREG => "file",
            S_IFDIR => "directory",
            _ => "other",
        };
        let file = KeyValue::File {
            dev: io_vfs::stat_dev(record.dev),
            ino: record.ino,
        };
        let mut attrs = vec![
            Attr::uint("lat_ns", record.duration_ns),
            Attr::text("call", call),
            Attr::uint("ino", record.ino),
            Attr::text("dev", device),
            Attr::uint("sb_dev", io_vfs::stat_dev(record.dev)),
            Attr::text("fs", fs),
            Attr::text("type", kind),
            Attr::int("start", record.start),
            Attr::int("end", record.end),
            // `fsync(2)` asks for the whole file, which the kernel writes as
            // `0..LLONG_MAX`; anything else came from `msync` or a filesystem.
            Attr::text(
                "range",
                if record.start == 0 && record.end == i64::MAX {
                    "whole"
                } else {
                    "partial"
                },
            ),
            Attr::int("ret", i64::from(record.ret)),
            Attr::uint("tid", u64::from(record.tid)),
            Attr::uint("pid", u64::from(record.tgid)),
            Attr::text("comm", record.comm.clone()),
        ];
        if record.ret < 0 {
            attrs.push(Attr::int("errno", i64::from(-record.ret)));
        }
        if record.fd >= 0 {
            attrs.push(Attr::int("fd", i64::from(record.fd)));
        }
        // The devices the sync's writes land on, as `io.bytes` keys them.
        if let Some(under) = self
            .mounts
            .as_mut()
            .and_then(|mounts| mounts.block_devices(dev))
        {
            attrs.push(Attr::text("blk_devs", under));
        }
        // Which function the sync was timed at: `do_fsync` found the file
        // through the descriptor, the other two were handed it.
        attrs.push(Attr::text(
            "via",
            record.via.map(Via::function).unwrap_or("unknown"),
        ));
        if record.ino == 0 {
            self.unnamed += 1;
        }
        Event {
            ts_ns: record.ts_ns,
            ts_err_ns: 0,
            kind: "io.fsync".to_owned(),
            keys: if record.ino == 0 {
                vec![KeyValue::task(record.tid as i32)]
            } else {
                vec![file, KeyValue::task(record.tid as i32)]
            },
            attrs,
            collector_id: CollectorId::unstamped(),
            seq,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_mut() else {
            return;
        };
        let mut records = Vec::new();
        let mut undecoded = 0;
        program.drain(|record| records.push(record), &mut undecoded);
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the fsync ring was shorter than one sync");
        }
        for (index, record) in records.iter().enumerate() {
            self.seen += 1;
            if record.ret < 0 {
                self.failed += 1;
            }
            let event = self.event(record, seq + index as u64);
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += 1;
                    continue;
                }
            }
            events.push(event);
        }
    }

    fn absorb_counters(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let (pending, ring, orphaned) = program.dropped();
        if orphaned > self.orphaned {
            self.losses.dropped += orphaned - self.orphaned;
            self.losses
                .note_times(super::histogram::ORPHANED, orphaned - self.orphaned);
            self.orphaned = orphaned;
        }
        if pending > self.dropped_pending {
            self.losses.dropped += pending - self.dropped_pending;
            self.losses.note_times(
                "more threads were inside a sync at once than the pending map holds, so their \
                 syncs were never timed",
                pending - self.dropped_pending,
            );
            self.dropped_pending = pending;
        }
        if ring > self.dropped_ring {
            self.losses.dropped += ring - self.dropped_ring;
            self.losses.note_times(
                "the fsync ring was full: syncs returned faster than they were drained, so this \
                 run's count of them is short",
                ring - self.dropped_ring,
            );
            self.dropped_ring = ring;
        }
        let unfollowed = program.unfollowed();
        if unfollowed > 0 && !self.unfollowed_said {
            self.losses.note(format!(
                "{unfollowed} process{} the target started {} not followed: the map of the \
                 processes it started was full, so {} syncs are in no event",
                if unfollowed == 1 { "" } else { "es" },
                if unfollowed == 1 { "was" } else { "were" },
                if unfollowed == 1 { "its" } else { "their" },
            ));
            self.unfollowed_said = true;
        }
        let missed = program.missed();
        if missed > self.missed {
            self.losses.dropped += missed - self.missed;
            self.losses.note_times(
                "the kernel skipped a run of these programs because one was already running on \
                 that cpu, preempted halfway, so a sync went untimed",
                missed - self.missed,
            );
            self.missed = missed;
        }
    }
}

impl Collector for FsyncCollector {
    fn value_attr(&self) -> &'static str {
        "lat_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        self.mounts = Some(Mounts::read());
        let program = FsyncProgram::load(self.narrowing)?;
        self.by_descriptor = program.by_descriptor;
        self.program = Some(program);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        self.absorb_counters();
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        self.absorb_counters();
        if let Some(program) = self.program.as_ref() {
            let (open, orphaned) = program.still_pending();
            self.still_pending = open;
            if orphaned > 0 {
                self.losses.dropped += orphaned as u64;
                self.losses
                    .note_times(super::histogram::ORPHANED, orphaned as u64);
            }
        }
        // The programs stay attached until the collector is dropped, after
        // the run has closed its accounts. Unlinking a trampoline walks every
        // ftrace record in the kernel, and six of them cost about 69 ms of
        // cpu: dropped here, inside the window, that was the whole of the
        // backends bench's 142 % and 144 % against a 25 ms target (perf:
        // `bpf_trampoline_unlink_prog` 12.95 % of the run's samples, beside
        // the link's 13.22 % at the attach, which falls before the window).
        // Kept, the window's cpu is 1 to 1.7 ms. The attach is said as cpu
        // before the window opened; the unlink is after it closed, and
        // nothing reads it (`docs/fixes/w24-io.md`, a request to stream run).
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = vec![
            "timed at do_fsync, vfs_fsync and vfs_fsync_range, whichever a sync reaches first: \
             fsync, fdatasync and msync(MS_SYNC) are here, and sync(2), syncfs(2) and \
             sync_file_range(2) are not"
                .to_owned(),
        ];
        if !self.by_descriptor {
            notes.push(
                "this kernel has no do_fsync in its BTF, so fsync and fdatasync are timed where \
                 they reach vfs_fsync; if that is inlined too, they are not timed at all"
                    .to_owned(),
            );
        }
        if self.failed > 0 {
            notes.push(format!(
                "{} of {} syncs returned an error, and each carries its errno: a sync that \
                 failed did not make anything durable, and its duration is not a commit's",
                self.failed, self.seen
            ));
        }
        if self.unnamed > 0 {
            notes.push(format!(
                "{} sync{} could not be named: the descriptor was closed or the table replaced \
                 while the sync ran, and the event carries no file rather than file zero",
                self.unnamed,
                if self.unnamed == 1 { "" } else { "s" }
            ));
        }
        if self.still_pending > 0 {
            notes.push(format!(
                "{} thread{} inside a sync when the run ended, and in no event",
                self.still_pending,
                if self.still_pending == 1 {
                    " was"
                } else {
                    "s were"
                }
            ));
        }
        if self.predicate.is_some() {
            notes.push(format!(
                "the filter removed {} sync{}; it ran here, over the drained records, not in the \
                 kernel",
                self.filtered_out,
                if self.filtered_out == 1 { "" } else { "s" }
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(datasync: u32, ret: i32) -> Fsync {
        Fsync {
            ts_ns: 1,
            duration_ns: 2_000_000,
            ino: 4242,
            start: 0,
            end: i64::MAX,
            dev: 28,
            datasync,
            ret,
            tid: 7,
            tgid: 7,
            mode: S_IFREG,
            comm: "db".to_owned(),
            fd: 3,
            via: Some(Via::Descriptor),
        }
    }

    fn collector() -> FsyncCollector {
        FsyncCollector {
            narrowing: Narrowing::default(),
            program: None,
            losses: Losses::default(),
            predicate: None,
            mounts: None,
            dropped_pending: 0,
            dropped_ring: 0,
            orphaned: 0,
            missed: 0,
            unfollowed_said: false,
            seen: 0,
            failed: 0,
            filtered_out: 0,
            still_pending: 0,
            unnamed: 0,
            by_descriptor: true,
        }
    }

    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let event = collector().event(&record(0, 0), 0);
        for field in &catalog
            .get(&"io.fsync".parse().expect("a source id"))
            .expect("io.fsync is in the catalog")
            .filter_fields
        {
            assert!(
                event.attrs.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    /// The file comes first: it is the series, and it is the key `io.fs` puts
    /// on the reads and writes of the same file.
    #[test]
    fn the_file_is_the_series_and_the_call_is_named() {
        let event = collector().event(&record(1, 0), 0);
        assert_eq!(event.keys[0].to_string(), "file:28+4242");
        assert_eq!(
            event.attr("call").map(ToString::to_string),
            Some("fdatasync".to_owned())
        );
        let failed = collector().event(&record(0, -5), 0);
        assert_eq!(failed.attr("errno").and_then(|v| v.as_f64()), Some(5.0));
    }

    #[test]
    fn only_trace_is_served_and_a_count_is_refused_by_name() {
        let refused = Box::new(Prepare {
            mode: Mode::Count,
            filter: None,
        })
        .attach(&Target::System);
        let Err(reason) = refused else {
            panic!("a counted fsync is not served")
        };
        assert!(reason.contains("count"), "{reason}");
    }
}
