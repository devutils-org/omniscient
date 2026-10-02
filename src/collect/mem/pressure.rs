//! `mem.pressure` over `/proc/pressure/memory` and a cgroup's
//! `memory.pressure`.
//!
//! Every other source in this family answers "how much memory is there". This
//! one answers "how much time was lost because there was not enough", and the
//! two are not related by any constant: a machine with a gigabyte free and a
//! machine at its limit both report a footprint, and only one of them is
//! stopping to reclaim.
//!
//! The file gives two things, and they are not two precisions of one number:
//!
//! - `some`, the share of the window during which **at least one** task was
//!   stalled on memory -- waiting on reclaim, on a refault, on compaction, on
//!   swap-in. That is the one that means work was delayed.
//! - `full`, the share during which **every** runnable task was. That is the
//!   one that means the machine stopped: there was nothing left for a cpu to
//!   get on with.
//!
//! Both are carried, because the gap between them is the diagnosis. `some` high
//! and `full` near zero is a workload that reclaims while other work proceeds;
//! the two together is thrashing.
//!
//! As in `io.pressure`, a poll reports the ratio it computed from the counter's
//! increase over its own window -- a fact about this run -- and a snapshot,
//! which observes no window at all, reports the kernel's decayed averages under
//! their own names. They are different measurements, so they do not share a
//! name.
//!
//! Absent is not zero. A kernel built without `CONFIG_PSI`, and a machine under
//! no memory pressure at all, are different answers; only the second is a
//! number, and the first is a missing requirement the resolver reports before
//! anything attaches.

use std::path::PathBuf;

use super::cgroup::Cgroup;
use crate::clock::monotonic_ns;
use crate::collect::psi::{Reading, POLL_BASIS, SNAPSHOT_BASIS};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

const SYSTEM_FILE: &str = "/proc/pressure/memory";

/// Which file the stall counter is read from, and therefore what it is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `/proc/pressure/memory`: every task on the machine.
    Machine,
    /// `<cgroup>/memory.pressure`: the tasks charged to one cgroup.
    Cgroup,
}

impl Scope {
    fn backend(self) -> &'static str {
        match self {
            Scope::Machine => "fs:proc",
            Scope::Cgroup => "fs:cgroup",
        }
    }

    fn read_from(self) -> &'static str {
        match self {
            Scope::Machine => "/proc/pressure/memory",
            Scope::Cgroup => "memory.pressure",
        }
    }
}

/// Holds the mode and the scope until the target is known.
pub struct Prepare {
    mode: Mode,
    scope: Scope,
}

impl Prepare {
    pub fn new(scope: Scope, mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } | Mode::Snapshot => Ok(Self { mode, scope }),
            other => Err(format!(
                "pressure is a counter that is read, not a stream of events: `{}` cannot be \
                 served from {}; poll it, or take one snapshot",
                other.kind(),
                scope.read_from()
            )),
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match self.scope {
            Scope::Machine => match target {
                Target::System => Ok(Box::new(Pressure::new(
                    self.mode,
                    Scope::Machine,
                    PathBuf::from(SYSTEM_FILE),
                    Vec::new(),
                ))),
                other => Err(format!(
                    "{SYSTEM_FILE} is the machine's memory stall counter, and a {} target is \
                     not the machine: the same measurement for one cgroup is `memory.pressure` \
                     under it, which is the `fs:cgroup` backend",
                    other.kind()
                )),
            },
            Scope::Cgroup => {
                let cgroup = Cgroup::resolve(target)?;
                let file = cgroup.file("memory.pressure");
                if !file.exists() {
                    return Err(format!(
                        "{} does not exist: pressure accounting per cgroup needs a kernel built \
                         with CONFIG_PSI and the memory controller enabled on this cgroup",
                        file.display()
                    ));
                }
                let keys = vec![cgroup.key()];
                Ok(Box::new(Pressure::new(
                    self.mode,
                    Scope::Cgroup,
                    file,
                    keys,
                )))
            }
        }
    }
}

/// Reads one pressure file.
pub struct Pressure {
    id: CollectorId,
    mode: Mode,
    scope: Scope,
    path: PathBuf,
    keys: Vec<KeyValue>,
    previous: Option<Reading>,
    losses: Losses,
}

impl Pressure {
    pub fn new(mode: Mode, scope: Scope, path: PathBuf, keys: Vec<KeyValue>) -> Self {
        Self {
            id: CollectorId::new(format!("mem.pressure/{}", scope.backend())),
            mode,
            scope,
            path,
            keys,
            previous: None,
            losses: Losses::default(),
        }
    }

    fn read(&self) -> Result<Reading, SampleError> {
        let text = std::fs::read_to_string(&self.path).map_err(|error| {
            // A cgroup is removed when its last process leaves, which for a
            // launched program is the end of the run rather than a lost read.
            if error.kind() == std::io::ErrorKind::NotFound {
                SampleError::TargetGone(format!("{} is gone", self.path.display()))
            } else {
                SampleError::Transient(format!("{}: {error}", self.path.display()))
            }
        })?;
        Reading::parse(&text, monotonic_ns()).ok_or_else(|| {
            SampleError::Transient(format!(
                "{} did not hold a `some ... total=N` line",
                self.path.display()
            ))
        })
    }

    /// The share of one tick that was spent stalled on memory.
    fn stall_event(&mut self, before: &Reading, after: &Reading, seq: u64) -> Option<Event> {
        let mut attrs = match after.stall_attrs(before) {
            Ok(attrs) => attrs,
            Err(unmeasured) => {
                self.losses.undecoded += 1;
                self.losses
                    .note(unmeasured.explain(&self.path.display().to_string()));
                return None;
            }
        };
        attrs.push(Attr::text("read_from", self.scope.read_from()));
        attrs.push(Attr::text("basis", POLL_BASIS));
        Some(self.event(after.ts_ns, attrs, seq))
    }

    /// What a single read can say: the kernel's own averages, over a window
    /// this run did not choose.
    fn snapshot_event(&self, reading: &Reading, seq: u64) -> Event {
        let mut attrs = reading.snapshot_attrs();
        attrs.push(Attr::text("read_from", self.scope.read_from()));
        attrs.push(Attr::text("basis", SNAPSHOT_BASIS));
        self.event(reading.ts_ns, attrs, seq)
    }

    fn event(&self, ts_ns: u64, attrs: Vec<Attr>, seq: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "mem.pressure".to_owned(),
            keys: self.keys.clone(),
            attrs,
            collector_id: self.id.clone(),
            seq,
        }
    }
}

impl Collector for Pressure {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    /// A poll measures its own window; a snapshot can only repeat the
    /// kernel's. Different numbers, so different names.
    fn value_attr(&self) -> &'static str {
        match self.mode {
            Mode::Snapshot => "some_avg10",
            _ => "stall_ratio",
        }
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let reading = self.read()?;
        if self.mode == Mode::Snapshot {
            return Ok(vec![self.snapshot_event(&reading, seq)]);
        }
        // The first read is the baseline and produces nothing: a ratio needs
        // two reads and the window between them.
        let before = self.previous.replace(reading.clone());
        let event = before.and_then(|before| self.stall_event(&before, &reading, seq));
        Ok(event.into_iter().collect())
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::mem::testing::Scratch;
    use std::time::Duration;

    const MEMORY: &str = "\
some avg10=1.23 avg60=2.34 avg300=3.45 total=1069440
full avg10=0.98 avg60=1.87 avg300=2.76 total=903210
";

    fn poll() -> Mode {
        Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        }
    }

    fn scratch_file(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "omn-mem-pressure-{}-{}-{name}",
            std::process::id(),
            monotonic_ns()
        ));
        std::fs::write(&path, MEMORY).expect("a scratch file");
        path
    }

    fn write_totals(path: &std::path::Path, some_us: u64, full_us: u64) {
        std::fs::write(
            path,
            format!(
                "some avg10=1.00 avg60=2.00 avg300=3.00 total={some_us}\n\
                 full avg10=0.50 avg60=1.00 avg300=1.50 total={full_us}\n"
            ),
        )
        .expect("writing the fake pressure file");
    }

    #[test]
    fn the_stalled_share_is_the_counters_increase_over_the_tick() {
        let path = scratch_file("stall");
        write_totals(&path, 2_000_000, 400_000);
        let mut collector = Pressure::new(poll(), Scope::Machine, path.clone(), Vec::new());
        assert!(
            collector.sample(0).expect("the first read").is_empty(),
            "the first read is a baseline, and a baseline measures nothing"
        );

        write_totals(&path, 2_300_000, 500_000);
        std::thread::sleep(Duration::from_millis(50));
        let events = collector.sample(1).expect("a second read");

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.attr("stalled_ns").unwrap().as_f64(), Some(300e6));
        assert_eq!(event.attr("full_stalled_ns").unwrap().as_f64(), Some(100e6));
        let interval = event.attr("interval_ns").unwrap().as_f64().unwrap();
        let ratio = event.attr("stall_ratio").unwrap().as_f64().unwrap();
        assert!((ratio - 300e6 / interval).abs() < 1e-9);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_snapshot_reports_the_kernels_averages_and_says_so() {
        let path = scratch_file("snapshot");
        let mut collector = Pressure::new(Mode::Snapshot, Scope::Machine, path.clone(), Vec::new());
        let events = collector.sample(0).expect("one read");
        assert_eq!(collector.value_attr(), "some_avg10");
        assert_eq!(events[0].attr("some_avg10").unwrap().as_f64(), Some(0.0123));
        assert!(events[0].attr("stall_ratio").is_none());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_counter_that_goes_backwards_is_a_counted_loss() {
        let path = scratch_file("backwards");
        write_totals(&path, 900, 800);
        let mut collector = Pressure::new(poll(), Scope::Machine, path.clone(), Vec::new());
        collector.sample(0).expect("the baseline read");
        write_totals(&path, 100, 50);
        assert!(collector.sample(1).expect("a read").is_empty());
        assert_eq!(collector.losses().undecoded, 1);
        std::fs::remove_file(&path).ok();
    }

    /// A cgroup is a different file and a different backend. Answering a
    /// cgroup with the machine's stall counter would be attribution nobody
    /// measured.
    #[test]
    fn a_cgroup_target_is_sent_to_the_backend_that_can_serve_it() {
        let refused = Box::new(Prepare::new(Scope::Machine, poll()).expect("poll is servable"))
            .attach(&Target::Cgroup(PathBuf::from("/user.slice")))
            .err()
            .expect("the machine file cannot answer for a cgroup");
        assert!(refused.contains("fs:cgroup"), "{refused}");
    }

    /// Ground truth: memory pressure is *caused* and then measured.
    ///
    /// A cgroup is held (`memory.high`) to a fraction of what a process inside
    /// it is about to touch, so the kernel has to reclaim continuously to let
    /// it run. That
    /// is memory stall by construction -- the tasks are waiting for pages that
    /// have to be found before they can have them -- and the same collector
    /// reading the same file for an unconstrained cgroup a moment earlier
    /// reports nothing of the sort.
    ///
    /// Without the comparison this would prove only that a number came out.
    /// With it, the number tracks the one thing the test changed.
    #[test]
    fn a_cgroup_forced_to_reclaim_stalls_and_an_idle_one_does_not() {
        const LIMIT: u64 = 24 * 1024 * 1024;
        const TOUCHED: u64 = 192 * 1024 * 1024;

        let Some(scratch) = Scratch::cgroup("pressure") else {
            eprintln!("skipped: this user cannot create a cgroup under its own slice");
            return;
        };
        if !std::path::Path::new(&scratch.path().join("memory.pressure")).exists() {
            eprintln!("skipped: this kernel was built without CONFIG_PSI");
            return;
        }

        let mut collector = Pressure::new(
            poll(),
            Scope::Cgroup,
            scratch.path().join("memory.pressure"),
            Vec::new(),
        );

        // An empty cgroup, over a window of the same length: the baseline
        // against which "pressure" means anything at all.
        collector.sample(0).expect("the baseline read");
        std::thread::sleep(Duration::from_millis(200));
        let idle = collector.sample(1).expect("a read of an idle cgroup");
        let idle_ratio = idle
            .first()
            .and_then(|event| event.attr("stall_ratio"))
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0);

        if let Some(reason) = scratch.no_swap_room(2 * TOUCHED.saturating_sub(LIMIT)) {
            eprintln!("skipped: {reason}");
            return;
        }
        // `memory.high`, not `memory.max`: both make the kernel reclaim from
        // the cgroup as it grows, and the time its tasks wait for that is the
        // stall this measures, but only `max` calls the OOM killer when reclaim
        // falls behind. On the CI's `defaults` cell -- a swap file on a cloud
        // disk, four cpus shared with the rest of the suite -- `max` killed
        // the child halfway (`memory.events`: `max 1631 oom 1 oom_kill 1`),
        // and the test said nothing about pressure. Over `high`, a task that
        // outruns reclaim is throttled, and the throttling is itself counted
        // as memory stall -- which is why the child is not required to have
        // finished: on a swap device too slow for reclaim to keep up, the
        // over-high penalty sleeps it for up to two seconds per charge batch,
        // and thirty seconds of that is thirty seconds of the stall measured
        // here. Eight times the limit, not twenty: enough reclaim to stall on
        // any device, and a third of the swap the earlier half gibibyte asked
        // of a cloud disk.
        assert!(scratch.write("memory.high", &LIMIT.to_string()));
        collector.sample(2).expect("a read before the work starts");
        let mut child = scratch.spawn_throttled_toucher(TOUCHED, Duration::from_secs(30));
        let finished = child.finished_touching();
        let stalled = collector.sample(3).expect("a read after the work");
        child.stop();
        if !finished {
            eprintln!(
                "the child was still touching its {TOUCHED} bytes after thirty seconds over a \
                 {LIMIT}-byte memory.high: throttled, which is the stall this reads"
            );
        }

        let event = stalled.first().expect("a second read produces a point");
        let ratio = event.attr("stall_ratio").unwrap().as_f64().unwrap();
        assert!(
            ratio > 0.0,
            "a cgroup limited to {LIMIT} bytes while a process touched {TOUCHED} has to stall; \
             stall_ratio was {ratio}"
        );
        assert!(
            ratio > idle_ratio,
            "the same cgroup stalled no more under forced reclaim ({ratio}) than while empty \
             ({idle_ratio})"
        );
        assert!(
            (0.0..=1.0).contains(&ratio),
            "a window cannot be stalled for more than its own length: {ratio}"
        );
        if let Some(full) = event
            .attr("full_stall_ratio")
            .and_then(|value| value.as_f64())
        {
            assert!(
                full <= ratio + 1e-9,
                "`full` counts a subset of what `some` counts: {full} against {ratio}"
            );
        }
    }
}
