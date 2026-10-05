//! `io.vfs`: every call into the VFS, counted by function, over fentry.
//!
//! What `vfsstat` reports: how many reads, writes, opens, creates and syncs
//! the machine (or a target) made per second, of any file type. `io.fs` times
//! the reads and writes of regular files; this counts every call and times
//! none, so its cost is one atomic add per call.
//!
//! **One event per interval**, its value the calls of every function summed
//! and each function's calls beside it under the function's name: the
//! aggregating emits answer with the total, a filter on `op` (`--filter 'op
//! == "vfs_open"'`) keeps one function, and `raw` is the split.

use std::time::Duration;

use crate::bpf::attach::io_vfs::VfsCalls;
use crate::bpf::Whose;
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

pub struct Prepare {
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(self.mode, Mode::Count | Mode::Poll { .. }) {
            return Err(format!(
                "`{}` mode cannot serve `io.vfs`: the calls are counted in the kernel and read \
                 as totals, over the run (`count`) or per interval (`poll`)",
                self.mode
            ));
        }
        let whose = crate::collect::sched::whose(target, "a VFS call is counted per task")?;
        Ok(Box::new(VfsCount::new(whose, self.mode, self.filter)))
    }
}

pub struct VfsCount {
    id: CollectorId,
    whose: Whose,
    mode: Mode,
    predicate: Option<FilterExpr>,
    program: Option<VfsCalls>,
    /// The counts at the last read, and when it was taken.
    previous: Vec<(&'static str, u64)>,
    previous_ns: u64,
    losses: Losses,
    missed: u64,
    unfollowed: u64,
    unguarded: u64,
    filtered_out: u64,
}

impl VfsCount {
    pub fn new(whose: Whose, mode: Mode, predicate: Option<FilterExpr>) -> Self {
        Self {
            id: CollectorId::new("io.vfs/bpf:fentry"),
            whose,
            mode,
            predicate,
            program: None,
            previous: Vec::new(),
            previous_ns: 0,
            losses: Losses::default(),
            missed: 0,
            unfollowed: 0,
            unguarded: 0,
            filtered_out: 0,
        }
    }

    /// One event for the interval since the previous read: the calls of
    /// every function the filter keeps, summed into `calls`, and each beside
    /// it under its own name.
    fn interval(&mut self, seq: u64) -> Vec<Event> {
        let Some(program) = self.program.as_ref() else {
            return Vec::new();
        };
        let now = program.read();
        let now_ns = monotonic_ns();
        let interval_ns = now_ns.saturating_sub(self.previous_ns);
        let mut total = 0;
        let mut split = Vec::new();
        for (op, calls) in &now {
            let before = self
                .previous
                .iter()
                .find(|(name, _)| name == op)
                .map_or(0, |(_, calls)| *calls);
            let calls = calls.saturating_sub(before);
            // The filter is asked of each function as an event of its own,
            // so `op == "vfs_open"` keeps that function and nothing else.
            if let Some(predicate) = &self.predicate {
                let probe = Event {
                    ts_ns: now_ns,
                    ts_err_ns: 0,
                    kind: "io.vfs".to_owned(),
                    keys: Vec::new(),
                    attrs: vec![Attr::uint("calls", calls), Attr::text("op", *op)],
                    collector_id: self.id.clone(),
                    seq,
                };
                if !crate::filter::matches(predicate, &probe) {
                    self.filtered_out += 1;
                    continue;
                }
            }
            total += calls;
            split.push(Attr::uint(*op, calls));
        }
        self.previous = now;
        self.previous_ns = now_ns;
        self.absorb();
        let mut attrs = vec![
            Attr::uint("calls", total),
            Attr::uint("interval_ns", interval_ns),
        ];
        attrs.extend(split);
        vec![Event {
            ts_ns: now_ns,
            ts_err_ns: 0,
            kind: "io.vfs".to_owned(),
            keys: Vec::new(),
            attrs,
            collector_id: self.id.clone(),
            seq,
        }]
    }

    fn absorb(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let missed = program.missed();
        if missed > self.missed {
            self.losses.dropped += missed - self.missed;
            self.losses.note(
                "the kernel skipped a run of these programs because one was already running on \
                 that cpu, preempted halfway, so a call went uncounted",
            );
            self.missed = missed;
        }
        let unfollowed = program.unfollowed();
        if unfollowed > self.unfollowed {
            self.losses.note(format!(
                "{unfollowed} process{} the target started {} not followed: the map of the \
                 processes it started was full, so {} calls are in no count",
                if unfollowed == 1 { "" } else { "es" },
                if unfollowed == 1 { "was" } else { "were" },
                if unfollowed == 1 { "its" } else { "their" },
            ));
            self.unfollowed = unfollowed;
        }
        let unguarded = program.unguarded();
        if unguarded > self.unguarded {
            self.losses.note(format!(
                "{unguarded} `do_fsync` call{} could not mark {} thread, the map of threads \
                 inside a sync being full, so on a kernel that does not inline \
                 `vfs_fsync_range` into `do_fsync` {} counted under both and `calls` is an \
                 upper bound",
                if unguarded == 1 { "" } else { "s" },
                if unguarded == 1 { "its" } else { "their" },
                if unguarded == 1 {
                    "that sync is"
                } else {
                    "those syncs are"
                },
            ));
            self.unguarded = unguarded;
        }
    }
}

impl Collector for VfsCount {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "calls"
    }

    fn start(&mut self) -> Result<(), String> {
        let program = VfsCalls::load(self.whose)?;
        self.previous = program.read();
        self.previous_ns = monotonic_ns();
        self.program = Some(program);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if self.mode == Mode::Count {
            self.absorb();
            return Ok(Vec::new());
        }
        Ok(self.interval(seq))
    }

    /// A counted run's one total per function. A polled run adds nothing
    /// here: a closing read would cover the few microseconds since the last
    /// tick, a point with a window near zero.
    fn stop(&mut self) -> Vec<Event> {
        if self.mode != Mode::Count {
            self.absorb();
            return Vec::new();
        }
        self.interval(u64::MAX / 2)
    }

    fn tick(&self) -> Option<Duration> {
        match self.mode {
            // Read once a second to bring the losses in; the counts are the
            // kernel's and lose nothing between reads.
            Mode::Count => Some(Duration::from_secs(1)),
            _ => None,
        }
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = vec![
            "every call into the VFS entry points named in `op`, of any file type -- a pipe's \
             or a socket's read is a `vfs_read` as a file's is -- counted on entry, timed never; \
             a read through io_uring, sendfile, splice or a mapping's fault calls none of them"
                .to_owned(),
        ];
        if let Some(program) = &self.program {
            if !program.absent.is_empty() {
                notes.push(format!(
                    "not counted on this kernel: {}",
                    program.absent.join("; ")
                ));
            }
        }
        if self.filtered_out > 0 {
            notes.push(format!(
                "the filter left out {} (function, interval) counts; it ran here, over each \
                 function's count",
                self.filtered_out
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_that_is_not_a_count_is_refused_by_name() {
        let prepare = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
        });
        let refused = match prepare.attach(&Target::System) {
            Ok(_) => panic!("a trace of counted calls"),
            Err(reason) => reason,
        };
        assert!(refused.contains("count"), "{refused}");
    }
}
