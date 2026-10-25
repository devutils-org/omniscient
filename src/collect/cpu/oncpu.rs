//! `cpu.oncpu_time` over `/proc/<pid>/stat`.
//!
//! The cheapest source left in this family, and the denominator half the others
//! need: an instruction count says how much work was done, and this says how
//! much of the machine it took to do it.
//!
//! The kernel keeps two totals per task, `utime` and `stime`, in ticks of
//! `USER_HZ` -- a hundredth of a second on every machine this runs on. That
//! granularity is the whole of what the catalog means by `degraded: tick
//! granularity`: a task that ran for three milliseconds is credited with either
//! zero ticks or one, and which of the two is decided by where the timer
//! interrupt fell. Over a window of any length the error stays one tick, so a
//! poll at 100 ms carries up to ten per cent of error and a poll at one second
//! one per cent. The interval is the request's to choose, and what it costs is
//! said on the event rather than left to be worked out.
//!
//! What this is *not*: the `bpf:raw_tp` backend above it in the chain
//! (`oncpu_bpf.rs`) adds up the scheduler's own nanoseconds per thread. This is
//! the fallback that needs no privileges at all, and it reports a process's
//! total rather than its threads.

use std::path::PathBuf;
use std::time::Duration as StdDuration;

use crate::clock::monotonic_ns;
use crate::collect::poll::{CounterPoll, Cumulative, Interval, Reading};
use crate::collect::task::Comm;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

/// What the kernel counts cpu time in. `sysconf(_SC_CLK_TCK)` rather than a
/// literal hundred: the two have agreed on every machine for decades and the
/// day they stop, a literal turns every number here into a wrong one.
pub(super) fn ticks_per_second() -> u64 {
    // SAFETY: a read-only query of a configuration value.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 {
        ticks as u64
    } else {
        100
    }
}

/// The two totals the kernel keeps for a task, in its own ticks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Times {
    user: u64,
    system: u64,
}

impl Cumulative for Times {
    fn since(&self, earlier: &Self) -> Option<Self> {
        Some(Times {
            user: self.user.checked_sub(earlier.user)?,
            system: self.system.checked_sub(earlier.system)?,
        })
    }
}

impl Times {
    fn total(&self) -> u64 {
        self.user + self.system
    }

    /// Reads fields 14 and 15 of a `stat` line.
    ///
    /// Everything before them is skipped by finding the closing parenthesis of
    /// the second field rather than by splitting on spaces: a program called
    /// `my program (2)` puts spaces and parentheses inside a field that is not
    /// quoted, and splitting on whitespace reads its name as several fields and
    /// every number after it as the wrong one.
    fn parse(line: &str) -> Option<Self> {
        let after_comm = &line[line.rfind(')')? + 1..];
        let mut fields = after_comm.split_whitespace();
        // `state` is the first field after the name, and `utime` is the
        // eleventh after that.
        let user = fields.nth(11)?.parse().ok()?;
        let system = fields.next()?.parse().ok()?;
        Some(Self { user, system })
    }
}

/// Holds the mode until the target exists.
pub struct Prepare(pub Mode);

impl Prepare {
    /// Refuses a mode a total cannot serve before anything is launched: a
    /// factory's refusal is the build's answer to the request as written, and
    /// reached with nothing started. At attach it came after the launch --
    /// P7 of `docs/portability.md`.
    pub fn new(mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } => Ok(Self(mode)),
            other => Err(refusal(other)),
        }
    }
}

/// `trace` is the other mode the catalog declares, and it belongs to the
/// backend above this one: watching `sched_switch` is what turns a total into
/// a series of slices with the stack that ran. Counting ticks and calling it a
/// trace would answer the shallower question under the deeper name.
fn refusal(mode: Mode) -> String {
    format!(
        "`cpu.oncpu_time` through `/proc` re-reads a total, which is `--mode poll`; `{mode}` \
         needs the `bpf:tp` backend, which follows sched_switch and is not built yet"
    )
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let interval = match self.0 {
            Mode::Poll { interval } => interval,
            other => return Err(refusal(other)),
        };
        let (pid, path) = match target {
            Target::Tid(tids) => {
                let tid = *tids.first().ok_or("no thread named")?;
                // A thread's own totals live under the process that owns it,
                // and `/proc/<tid>/stat` is that file: the kernel publishes a
                // task directory for every thread. What it does *not* do is
                // make `/proc/<tid>` a process -- which is why the pid is kept
                // as given and the key says `task`.
                (tid, PathBuf::from(format!("/proc/{tid}/stat")))
            }
            other => {
                let pid = other.first_pid().ok_or_else(|| {
                    format!(
                        "cpu time is consumed by a task, and a {} target names none: the \
                         cgroup backend is what answers that scope and is not built yet",
                        other.kind()
                    )
                })?;
                (pid, PathBuf::from(format!("/proc/{pid}/stat")))
            }
        };
        Ok(Box::new(OnCpuTime::new(pid, path, interval)))
    }
}

/// Re-reads a task's cpu totals and reports what it consumed between reads.
pub struct OnCpuTime {
    pid: i32,
    path: PathBuf,
    /// The poll interval, carried only to state the error the tick granularity
    /// costs at this resolution.
    interval: crate::scalar::Duration,
    poll: CounterPoll<(), Times>,
    /// From the baseline until the task's end took a reading away.
    open: bool,
    comm: Comm,
    losses: Losses,
    notes: Vec<String>,
}

impl OnCpuTime {
    pub fn new(pid: i32, path: PathBuf, interval: crate::scalar::Duration) -> Self {
        Self {
            pid,
            path,
            interval,
            poll: CounterPoll::new(Mode::Poll { interval }),
            open: false,
            comm: Comm::of(pid),
            losses: Losses::default(),
            notes: Vec::new(),
        }
    }

    fn reading(&self) -> Result<Reading<(), Times>, SampleError> {
        let now = self.read()?;
        Ok(Reading {
            ts_ns: monotonic_ns(),
            counters: [((), now)].into(),
        })
    }

    fn read(&self) -> Result<Times, SampleError> {
        let text = std::fs::read_to_string(&self.path).map_err(|error| {
            // A task that ended is the end of the run, not a read to retry:
            // every further read would fail the same way.
            if error.kind() == std::io::ErrorKind::NotFound {
                SampleError::TargetGone(format!("task {} exited", self.pid))
            } else {
                SampleError::Transient(format!("{}: {error}", self.path.display()))
            }
        })?;
        Times::parse(&text).ok_or_else(|| {
            SampleError::Transient(format!(
                "{} is not the shape this reads",
                self.path.display()
            ))
        })
    }

    /// Counts the reading the task's end took away.
    ///
    /// A launched program is reaped before the run's closing round, so the
    /// read that would have closed the last interval finds no `/proc/<pid>`:
    /// whatever the task ran since the reading before it, up to one interval,
    /// is in no event. That is a loss of this run's, counted where the run's
    /// losses are, not only a sentence in the catalog. Counted once: the
    /// baseline is dropped with it, so a later read finds nothing to close.
    fn gone(&mut self) {
        if !std::mem::take(&mut self.open) {
            return;
        }
        self.losses.failed_reads += 1;
        *self
            .losses
            .reasons
            .entry(
                "the task ended before the closing read: its cpu time since the last reading, \
                 up to one interval, is in no event (a launched program's exit carries the \
                 whole as `cpu_ns`)"
                    .to_owned(),
            )
            .or_default() += 1;
    }

    /// Turns an interval into an event.
    ///
    /// Separate from `sample` so the arithmetic can be checked against readings
    /// chosen for the cases that matter -- a window in which the task never
    /// ran -- rather than against whatever this machine happened to be doing.
    /// A task's times never go backwards; an interval where they did is none.
    fn event_from(&mut self, interval: Interval<(), Times>, seq: u64) -> Vec<Event> {
        let per_tick_ns = 1_000_000_000 / ticks_per_second();
        let ts_ns = interval.ts_ns;
        let Some(&((), delta)) = interval.moved.first() else {
            return Vec::new();
        };

        let mut attrs = vec![
            Attr::uint("ns", delta.total() * per_tick_ns),
            Attr::uint("user_ns", delta.user * per_tick_ns),
            Attr::uint("system_ns", delta.system * per_tick_ns),
            // The resolution the number actually has. One tick is the whole of
            // the error, whatever the interval, so a reader can tell a series
            // that means something from one that is mostly rounding.
            Attr::uint("resolution_ns", per_tick_ns),
        ];
        if let Some(comm) = self.comm.get() {
            attrs.push(Attr::text("comm", comm));
        }

        vec![Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "cpu.oncpu_time".to_owned(),
            keys: vec![KeyValue::task(self.pid)],
            attrs,
            collector_id: CollectorId::unstamped(),
            seq,
        }]
    }
}

impl Collector for OnCpuTime {
    fn value_attr(&self) -> &'static str {
        "ns"
    }

    fn start(&mut self) -> Result<(), String> {
        // The baseline, before the task is released: the first poll opens
        // the window (`collect::poll`), and the interval after it is measured
        // from here, not from everything since the task began -- which
        // reported as a point would be the first point of every series wrong
        // by however long the program had already been running.
        let mut poll = std::mem::replace(&mut self.poll, CounterPoll::new(Mode::Snapshot));
        let started = poll.start(|| self.reading());
        self.poll = poll;
        started.map_err(|error| match error {
            SampleError::TargetGone(reason) | SampleError::Transient(reason) => reason,
        })?;
        self.open = true;

        let per_tick_ns = 1_000_000_000 / ticks_per_second();
        let interval_ns = self.interval.as_nanos();
        if interval_ns < per_tick_ns * 10 {
            self.notes.push(format!(
                "the kernel credits cpu time in ticks of {:.0} ms and this polls every {:.0} ms, \
                 so each reading is one tick -- up to {:.0} % -- away from what was consumed; \
                 the `bpf:raw_tp` backend is the one that resolves below a tick",
                per_tick_ns as f64 / 1e6,
                interval_ns as f64 / 1e6,
                100.0 * per_tick_ns as f64 / interval_ns as f64,
            ));
        }
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut poll = std::mem::replace(&mut self.poll, CounterPoll::new(Mode::Snapshot));
        let ticked = poll.tick(|| self.reading());
        self.poll = poll;
        match ticked {
            Ok(interval) => Ok(interval
                .map(|interval| self.event_from(interval, seq))
                .unwrap_or_default()),
            Err(SampleError::TargetGone(reason)) => {
                self.gone();
                Err(SampleError::TargetGone(reason))
            }
            Err(error) => Err(error),
        }
    }

    fn tick(&self) -> Option<StdDuration> {
        None
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        self.notes.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};

    fn poll(interval_ms: u64) -> crate::scalar::Duration {
        crate::scalar::Duration::from_nanos(interval_ms * 1_000_000)
    }

    /// The kernel's own nanosecond total for a task, from `schedstat`.
    ///
    /// This is the independently known quantity the collector is checked
    /// against: a different file, a different unit, and an accounting the
    /// scheduler keeps for itself rather than the tick-rounded one that
    /// `/proc/<pid>/stat` publishes.
    fn schedstat_ns(pid: i32) -> Option<u64> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/schedstat")).ok()?;
        text.split_whitespace().next()?.parse().ok()
    }

    /// A shell loop of `iterations` turns: a process that is on a cpu for
    /// essentially the whole of its life.
    ///
    /// Arithmetic the shell does itself, with nothing forked. A loop that calls
    /// `date` on every turn spends its life waiting for children, and a child's
    /// cpu time does not land in its parent's `utime` at all -- which is how a
    /// first version of this test measured a program that was mostly blocked
    /// and concluded the collector under-reported.
    fn spin(iterations: u32) -> Option<Child> {
        let script = format!("i=0; while [ $i -lt {iterations} ]; do i=$((i+1)); done");
        Command::new("/bin/sh").args(["-c", &script]).spawn().ok()
    }

    /// Follows a child to its end, polling the collector, and returns what the
    /// collector reported, what the scheduler's own counter said, and how long
    /// the whole thing took.
    fn follow(mut child: Child, interval_ms: u64) -> (f64, Option<u64>, f64) {
        let pid = child.id() as i32;
        let mut collector = OnCpuTime::new(
            pid,
            PathBuf::from(format!("/proc/{pid}/stat")),
            poll(interval_ms),
        );
        collector.start().expect("the task exists");
        // The runner's first tick, as the window opens.
        collector.sample(0).expect("the opening tick");
        let before = schedstat_ns(pid).unwrap_or(0);
        let started = std::time::Instant::now();

        let mut reported = 0f64;
        // Read while the task is still there, and keep the last one: after it
        // exits there is no `/proc/<pid>` left to compare against.
        let mut kernel = schedstat_ns(pid);
        let mut seq = 1;
        while child.try_wait().ok().flatten().is_none() {
            std::thread::sleep(StdDuration::from_millis(interval_ms));
            if let Some(now) = schedstat_ns(pid) {
                kernel = Some(now);
            }
            match collector.sample(seq) {
                Ok(events) => {
                    for event in events {
                        reported += event.attr("ns").unwrap().as_f64().unwrap();
                    }
                }
                // The task ended between the wait and the read.
                Err(SampleError::TargetGone(_)) => break,
                Err(other) => panic!("{other:?}"),
            }
            seq += 1;
        }
        let wall = started.elapsed().as_nanos() as f64;
        let _ = child.wait();
        (
            reported,
            kernel.map(|last| last.saturating_sub(before)),
            wall,
        )
    }

    /// A task reaped before the closing read leaves its last interval in no
    /// event, and the run says so in its losses rather than only in the
    /// catalog's caveat: one failed read, with the reason, and only once
    /// however often the run asks again.
    #[test]
    fn a_task_reaped_before_the_closing_read_is_a_counted_loss() {
        let mut child = Command::new("sleep").arg("30").spawn().expect("sleep runs");
        let pid = child.id() as i32;
        let mut collector =
            OnCpuTime::new(pid, PathBuf::from(format!("/proc/{pid}/stat")), poll(100));
        collector.start().expect("the task exists");
        collector.sample(0).expect("the task is still there");
        assert_eq!(collector.losses().failed_reads, 0);

        child.kill().expect("kill");
        child.wait().expect("reap");
        for seq in 1..3 {
            assert!(matches!(
                collector.sample(seq),
                Err(SampleError::TargetGone(_))
            ));
        }
        let losses = collector.losses();
        assert_eq!(losses.failed_reads, 1, "{losses:?}");
        assert!(
            losses
                .reasons
                .keys()
                .any(|reason| reason.contains("closing read") && reason.contains("cpu_ns")),
            "{losses:?}"
        );
    }

    #[test]
    fn the_name_of_a_process_with_spaces_in_it_does_not_shift_every_field() {
        // The shape of a real line, with a name chosen to break the naive split.
        let line = "42 (my program (2)) S 1 42 42 0 -1 4194304 900 0 0 0 731 219 0 0 20 0 1 \
                    0 12345 0 0 0";
        let times = Times::parse(line).expect("the line parses");
        assert_eq!(times.user, 731);
        assert_eq!(times.system, 219);
        assert_eq!(times.total(), 950);
    }

    /// The first read is a baseline, not a measurement: reporting it would
    /// credit this window with everything the task had already done.
    #[test]
    fn the_first_read_produces_no_point() {
        let mut collector = OnCpuTime::new(0, PathBuf::from("/proc/self/stat"), poll(100));
        let reading = |ts_ns, user, system| {
            Ok(Reading {
                ts_ns,
                counters: [((), Times { user, system })].into(),
            })
        };
        let mut poll = CounterPoll::new(Mode::Poll {
            interval: poll(100),
        });
        assert!(poll.tick(|| reading(1_000, 500, 100)).unwrap().is_none());
        let interval = poll
            .tick(|| reading(2_000, 530, 110))
            .unwrap()
            .expect("an interval");
        let events = collector.event_from(interval, 1);
        assert_eq!(events.len(), 1);
        let ns = events[0].attr("ns").unwrap().as_f64().unwrap();
        // Forty ticks of ten milliseconds.
        assert_eq!(ns, 40.0 * (1_000_000_000 / ticks_per_second()) as f64);
        assert_eq!(
            events[0].attr("user_ns").unwrap().as_f64().unwrap(),
            30.0 * (1_000_000_000 / ticks_per_second()) as f64
        );
    }

    /// Ground truth: a program that spends its life on a cpu is reported as
    /// having spent it, and one that spends the same time asleep is not.
    ///
    /// The pair is what makes this a measurement rather than a plausible
    /// number. A collector reporting the wall clock passes the first half and
    /// fails the second; one reporting a constant fails both. What the busy
    /// one spent is `/proc/<pid>/schedstat`, the scheduler's own nanosecond
    /// accounting of the same task in a different file, in a different unit,
    /// that this code never reads to produce its answer -- and not its wall
    /// clock, which on a loaded machine is several times its cpu (a life of
    /// 2140 ms reported as 780 ms of cpu on the CI's four vcpus, beside the
    /// rest of the suite).
    #[test]
    fn a_program_that_burns_cpu_is_reported_as_having_burnt_it_and_one_that_sleeps_is_not() {
        let Some(busy) = spin(400_000) else {
            eprintln!("skipped: /bin/sh is not available");
            return;
        };
        let (reported, kernel, _) = follow(busy, 50);
        let kernel = kernel.expect("the scheduler counts this task while it lives");
        assert!(
            kernel > 100_000_000,
            "a shell loop of 400 000 turns was on a cpu for {:.1} ms by the scheduler's \
             account, too little to be the busy program this test needs",
            kernel as f64 / 1e6
        );
        let ratio = reported / kernel.max(1) as f64;
        assert!(
            (0.6..1.5).contains(&ratio),
            "this reports {:.1} ms of cpu time where the scheduler's own counter for the same \
             task says {:.1} ms",
            reported / 1e6,
            kernel as f64 / 1e6
        );

        let Some(idle) = Command::new("sleep").arg("0.4").spawn().ok() else {
            eprintln!("skipped: no `sleep` on this machine's PATH");
            return;
        };
        let (reported, kernel, wall) = follow(idle, 50);
        assert!(
            reported < wall * 0.2,
            "a sleep of {:.0} ms was reported as {:.0} ms of cpu time, which is the wall clock \
             wearing the name of a measurement",
            wall / 1e6,
            reported / 1e6
        );
        // And the scheduler agrees that it consumed nearly nothing, so the
        // small number is the task's and not this collector's rounding.
        assert!(
            kernel.unwrap_or(0) < (wall * 0.2) as u64,
            "the scheduler credits a sleeping task with {:?} ns",
            kernel
        );
    }

    /// A poll close to the kernel's own tick is mostly rounding, and the run
    /// has to say so rather than let a reader take a series of tick-sized steps
    /// for a resolution it does not have.
    #[test]
    fn polling_faster_than_the_kernel_accounts_says_what_that_costs() {
        let mut collector = OnCpuTime::new(0, PathBuf::from("/proc/self/stat"), poll(10));
        collector.start().expect("this process exists");
        let note = collector
            .notes()
            .into_iter()
            .next()
            .expect("a poll at one tick is worth a note");
        assert!(note.contains("ticks"), "{note}");
        assert!(note.contains("bpf:raw_tp"), "{note}");

        let mut slow = OnCpuTime::new(0, PathBuf::from("/proc/self/stat"), poll(1_000));
        slow.start().expect("this process exists");
        assert!(
            slow.notes().is_empty(),
            "a poll a hundred ticks wide needs no warning: {:?}",
            slow.notes()
        );
    }

    /// The mode this backend cannot serve is refused by name, with the one that
    /// could: a total counted in ticks is not a trace of when the task ran.
    #[test]
    fn a_mode_this_backend_cannot_serve_names_the_backend_that_could() {
        let Err(error) = Box::new(Prepare(Mode::Trace)).attach(&Target::Pid(vec![1])) else {
            panic!("a trace is a different mechanism and has to be refused as one");
        };
        assert!(error.contains("bpf:tp"), "{error}");
        assert!(error.contains("--mode poll"), "{error}");
    }

    /// A target with no task in it is refused with what would answer it, rather
    /// than with a number about some other task.
    #[test]
    fn a_target_that_names_no_task_is_refused_with_what_would_answer_it() {
        let Err(error) = Box::new(Prepare(Mode::Poll {
            interval: poll(100),
        }))
        .attach(&Target::System) else {
            panic!("a machine has no cpu time of its own to read here");
        };
        assert!(error.contains("cgroup"), "{error}");
    }
}
