//! The `cpu` counters, in `count` mode, over `perf_event_open`.
//!
//! Counting is not a cheaper sampling. A sampled counter answers "where did
//! this happen", with a sample every N events and a ring buffer that can
//! overflow; a counted one answers "how many happened", exactly, with no ring
//! and nothing to lose. That is why `cpu.instructions` is the one source in the
//! catalog with `determinism: high`: the number is the hardware's own, read
//! twice, and the only thing between the two reads is the program.
//!
//! Three things here decide whether the number means what it says.
//!
//! The counters of one source are opened as a **group**, so the kernel puts
//! them on the hardware together or not at all. `cpu.ipc` is instructions over
//! cycles; if the two had been scheduled in different slices of time the ratio
//! would be of two different windows and would describe neither.
//!
//! The group **follows the threads and children** the target starts, because a
//! counting event needs no ring buffer and can therefore be inherited. A
//! program that does its work in a thread pool is counted, rather than reported
//! as having done almost nothing.
//!
//! And the group is **read after it is stopped**. The leader's `DISABLE`
//! freezes every member at once, so two counters read a microsecond apart still
//! describe the same window.

use std::path::Path;
use std::time::Duration as StdDuration;

use perf_event_open_sys::bindings as perf;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::perf::{Counter, CounterGroup, Reading, Scope};
use crate::request::{Mode, Target};

/// How often the collector is asked for events.
///
/// It has none to give until the window closes: a counter is read once, at the
/// end. The tick exists so that the run notices a target that exited, and so
/// that the observed window is not rounded up to a whole second for a program
/// that lived ten milliseconds.
const TICK: StdDuration = StdDuration::from_millis(10);

const CYCLES: Counter = Counter::hardware("cycles", perf::PERF_COUNT_HW_CPU_CYCLES);
const INSTRUCTIONS: Counter = Counter::hardware("instructions", perf::PERF_COUNT_HW_INSTRUCTIONS);
const BRANCH_MISSES: Counter =
    Counter::hardware("branch_misses", perf::PERF_COUNT_HW_BRANCH_MISSES);
const CACHE_MISSES: Counter = Counter::hardware("cache_misses", perf::PERF_COUNT_HW_CACHE_MISSES);
const CONTEXT_SWITCHES: Counter =
    Counter::software("context_switches", perf::PERF_COUNT_SW_CONTEXT_SWITCHES);
const MIGRATIONS: Counter = Counter::software("migrations", perf::PERF_COUNT_SW_CPU_MIGRATIONS);

/// What a source's value is, once the counters have been read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value {
    /// The counter itself, under the name `count`.
    Count,
    /// The first counter divided by the second, under the name of the source's
    /// own metric. A derived source states what it is derived from rather than
    /// pretending to be a counter of its own.
    Ratio { name: &'static str },
}

/// One catalog source, as a set of counters and a way to read a value from them.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub source: &'static str,
    pub backend: &'static str,
    pub counters: &'static [Counter],
    pub value: Value,
}

impl Spec {
    /// The attribute that holds this source's measured value.
    pub fn value_attr(&self) -> &'static str {
        match self.value {
            Value::Count => "count",
            Value::Ratio { name } => name,
        }
    }
}

/// Every `cpu` source that is one or more counters read at the end of a window.
pub const SPECS: &[Spec] = &[
    Spec {
        source: "cpu.cycles",
        backend: "pmu",
        counters: &[CYCLES],
        value: Value::Count,
    },
    Spec {
        source: "cpu.instructions",
        backend: "pmu",
        counters: &[INSTRUCTIONS],
        value: Value::Count,
    },
    Spec {
        source: "cpu.branch_misses",
        backend: "pmu",
        counters: &[BRANCH_MISSES],
        value: Value::Count,
    },
    Spec {
        source: "cpu.cache_misses",
        backend: "pmu",
        counters: &[CACHE_MISSES],
        value: Value::Count,
    },
    Spec {
        source: "cpu.context_switches",
        backend: "pmu:sw",
        counters: &[CONTEXT_SWITCHES],
        value: Value::Count,
    },
    Spec {
        source: "cpu.migrations",
        backend: "pmu:sw",
        counters: &[MIGRATIONS],
        value: Value::Count,
    },
    // The order matters: the value is the first counter over the second.
    Spec {
        source: "cpu.ipc",
        backend: "pmu",
        counters: &[INSTRUCTIONS, CYCLES],
        value: Value::Ratio { name: "ipc" },
    },
];

/// The source and backend a counting spec serves, or nothing when the pair is
/// not one of them.
pub fn spec_for(source: &str, backend: &str) -> Option<&'static Spec> {
    SPECS
        .iter()
        .find(|spec| spec.source == source && spec.backend == backend)
}

/// Holds the source until the target exists.
#[derive(Debug)]
pub struct Prepare(&'static Spec);

impl Prepare {
    /// Refuses a mode this collector cannot serve before anything is launched.
    ///
    /// The catalog declares `sample` for most of these, and sampling a counter
    /// is a different mechanism -- a ring buffer, a period, and stacks to make
    /// the samples worth having. Saying so here, rather than counting and
    /// calling it a sample, is the difference between a refusal and a wrong
    /// number.
    pub fn new(spec: &'static Spec, mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Count => Ok(Self(spec)),
            other => Err(format!(
                "`{}` in `{other}` mode needs the sampling path, which lands with stacks; \
                 `--mode count` reads the same counter exactly",
                spec.source
            )),
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        // A counter is opened over a task. Counting a cpu or the whole machine
        // means one event per cpu -- `Scope::Cpu` in `src/perf.rs` -- and the
        // privileges for it, which is a different thing to build and not one to
        // approximate with a per-process number.
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "`{}` counts a task, and a {} target names none: counting every cpu needs \
                 one event per cpu and the privileges for it, which is not built yet",
                self.0.source,
                target.kind()
            )
        })?;
        Ok(Box::new(CounterCollector::new(pid, self.0)))
    }
}

/// Reads one source's counters over one window.
pub struct CounterCollector {
    id: CollectorId,
    pid: i32,
    spec: &'static Spec,
    group: Option<CounterGroup>,
    losses: Losses,
    notes: Vec<String>,
}

impl CounterCollector {
    pub fn new(pid: i32, spec: &'static Spec) -> Self {
        Self {
            id: CollectorId::new(format!("{}/{}", spec.source, spec.backend)),
            pid,
            spec,
            group: None,
            losses: Losses::default(),
            notes: Vec::new(),
        }
    }

    /// Turns the readings into the single event this run produces.
    ///
    /// One event, not one per tick: a counted window has one value, and
    /// inventing intermediate points would suggest a resolution the mode does
    /// not have.
    fn event(&mut self, readings: &[Reading], seq: u64) -> Option<Event> {
        let window = readings.first()?;
        if window.never_ran() {
            self.losses.failed_reads += 1;
            self.losses.note(format!(
                "`{}` was never scheduled onto a cpu during the window -- the target did not \
                 run -- so there is no count, which is not the same as a count of zero",
                self.spec.source
            ));
            return None;
        }

        let mut attrs = Vec::with_capacity(readings.len() + 4);
        match self.spec.value {
            Value::Count => {
                attrs.push(Attr::uint("count", window.value));
                attrs.push(Attr::text("counter", window.name));
            }
            Value::Ratio { name } => {
                let denominator = readings.get(1)?;
                if denominator.value == 0 {
                    self.losses.failed_reads += 1;
                    self.losses.note(format!(
                        "`{}` counted zero, so `{}` has no value",
                        denominator.name, self.spec.source
                    ));
                    return None;
                }
                attrs.push(Attr::float(
                    name,
                    window.value as f64 / denominator.value as f64,
                ));
                // Both terms travel with the ratio: a ratio whose terms are
                // invisible cannot be checked against anything.
                for reading in readings {
                    attrs.push(Attr::uint(reading.name, reading.value));
                }
            }
        }

        // What was actually counted, whenever the reported value is an
        // extrapolation from it.
        for reading in readings {
            if reading.was_multiplexed() {
                attrs.push(Attr::uint(format!("{}_raw", reading.name), reading.raw));
            }
        }

        // The window the counter itself measured, which is the honest
        // denominator for a rate: the wall clock also contains the time between
        // the last tick and the moment the run was noticed to be over.
        attrs.push(Attr::uint("enabled_ns", window.enabled_ns));
        attrs.push(Attr::uint("running_ns", window.running_ns));

        Some(Event {
            ts_ns: monotonic_ns(),
            ts_err_ns: 0,
            kind: self.spec.source.to_owned(),
            keys: vec![KeyValue::task(self.pid)],
            attrs,
            collector_id: self.id.clone(),
            seq,
        })
    }
}

impl Collector for CounterCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        self.spec.value_attr()
    }

    fn start(&mut self) -> Result<(), String> {
        let group =
            CounterGroup::open(self.spec.counters, Scope::Process(self.pid)).map_err(|error| {
                format!(
                    "perf_event_open for `{}` on pid {} was refused ({error}); \
                     check /proc/sys/kernel/perf_event_paranoid, and whether this cpu \
                     exposes the counter at all",
                    self.spec.source, self.pid
                )
            })?;
        if group.kernel_excluded() {
            self.notes.push(format!(
                "this machine would only open `{}` for user mode, so kernel-mode events \
                 are not in this count",
                self.spec.source
            ));
        }
        group.enable().map_err(|error| error.to_string())?;
        self.group = Some(group);
        Ok(())
    }

    fn sample(&mut self, _seq: u64) -> Result<Vec<Event>, SampleError> {
        // A counted window has one value and it is read at the end. The only
        // thing to do on a tick is notice that the thing being counted is gone.
        if self.pid > 0 && !Path::new(&format!("/proc/{}", self.pid)).exists() {
            return Err(SampleError::TargetGone(format!(
                "process {} exited",
                self.pid
            )));
        }
        Ok(Vec::new())
    }

    fn stop(&mut self) -> Vec<Event> {
        let Some(group) = self.group.take() else {
            return Vec::new();
        };
        let readings = match group.read_final() {
            Ok(readings) => readings,
            Err(error) => {
                self.losses.failed_reads += 1;
                self.losses
                    .note(format!("reading the counters failed: {error}"));
                return Vec::new();
            }
        };

        for reading in &readings {
            if reading.was_multiplexed() && !reading.never_ran() {
                self.notes.push(format!(
                    "`{}` shared the hardware with other counters and was on for {:.0} % of \
                     the window: its value is scaled from what was counted, not counted",
                    reading.name,
                    reading.duty() * 100.0
                ));
            }
        }

        self.event(&readings, 0).into_iter().collect()
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(TICK)
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

    fn spec(source: &str) -> &'static Spec {
        SPECS
            .iter()
            .find(|spec| spec.source == source)
            .expect("the source is in the table")
    }

    /// Counting is exact, so a collector that produced a number must have
    /// produced one for a window it actually observed.
    #[test]
    fn a_counted_window_reports_the_window_it_counted() {
        let mut collector = CounterCollector::new(0, spec("cpu.instructions"));
        if collector.start().is_err() {
            eprintln!("skipped: this machine refuses unprivileged perf counting");
            return;
        }
        let mut total = 0u64;
        for index in 0..100_000u64 {
            total = total.wrapping_add(index);
        }
        std::hint::black_box(total);

        let events = collector.stop();
        assert_eq!(events.len(), 1, "a counted window produces one value");
        let event = &events[0];
        let count = event.attr("count").unwrap().as_f64().unwrap();
        assert!(
            count > 100_000.0,
            "a loop of 100k iterations counted {count}"
        );
        let enabled = event.attr("enabled_ns").unwrap().as_f64().unwrap();
        assert!(enabled > 0.0, "the counter reports no enabled window");
        assert!(collector.losses().is_clean(), "{:?}", collector.losses());
    }

    #[test]
    fn a_ratio_carries_both_of_its_terms() {
        let mut collector = CounterCollector::new(0, spec("cpu.ipc"));
        if collector.start().is_err() {
            eprintln!("skipped: this machine refuses unprivileged perf counting");
            return;
        }
        let mut total = 0u64;
        for index in 0..1_000_000u64 {
            total = total.wrapping_add(index);
        }
        std::hint::black_box(total);

        let events = collector.stop();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        let ipc = event.attr("ipc").unwrap().as_f64().unwrap();
        let instructions = event.attr("instructions").unwrap().as_f64().unwrap();
        let cycles = event.attr("cycles").unwrap().as_f64().unwrap();
        // The point of the group: the ratio is of these two numbers, and both
        // are on the event so it can be checked rather than believed.
        assert!((ipc - instructions / cycles).abs() < 1e-9);
        assert!(
            (0.01..20.0).contains(&ipc),
            "an ipc of {ipc} is outside anything a cpu can do"
        );
    }

    /// The ground truth of the whole family: a known number of machine
    /// instructions, counted.
    ///
    /// Two instructions per iteration, fifty million iterations, so a hundred
    /// million retired instructions plus whatever runs between the enable and
    /// the disable -- a few thousand at most.
    ///
    /// The counter is opened user-mode only, and that is not a convenience. A
    /// hundred million is a count of *this loop's* instructions, and an
    /// unrestricted counter also counts every interrupt handler, timer tick and
    /// page fault that landed on this thread while it ran. Comparing the first
    /// number against the second compares two different things, and on a busy
    /// machine the difference is over a percent -- which is how this test
    /// failed one full-suite run in five before the two sides were made to
    /// measure the same thing.
    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn instructions_match_a_loop_whose_instruction_count_is_known() {
        const ITERATIONS: u64 = 50_000_000;
        const PER_ITERATION: u64 = 2;
        let expected = (ITERATIONS * PER_ITERATION) as f64;

        let mut counted = Vec::new();
        for _ in 0..5 {
            let Ok(group) =
                crate::perf::CounterGroup::open_user_mode(&[INSTRUCTIONS], Scope::Process(0))
            else {
                eprintln!("skipped: this machine refuses unprivileged perf counting");
                return;
            };
            group.enable().expect("enable");
            known_loop(ITERATIONS);
            let readings = group.read_final().expect("read");
            counted.push(readings[0].value as f64);
        }

        for count in &counted {
            let error = (count - expected).abs() / expected;
            assert!(
                error < 0.01,
                "counted {count} user-mode instructions where the loop retires {expected}:                  {:.2} % off",
                error * 100.0
            );
        }

        // `determinism: high` in the catalog is a claim about run-to-run
        // spread, and this is the check that keeps it honest. A coefficient of
        // variation above a percent means the environment moved -- frequency,
        // SMT, another tenant -- and no measurement taken here is comparable to
        // another.
        let mean = counted.iter().sum::<f64>() / counted.len() as f64;
        let variance = counted
            .iter()
            .map(|count| (count - mean).powi(2))
            .sum::<f64>()
            / counted.len() as f64;
        let cv = variance.sqrt() / mean;
        assert!(
            cv < 0.01,
            "the counter varies by {:.2} % between identical runs ({counted:?}), so this              machine cannot resolve the differences this family exists to measure",
            cv * 100.0
        );
    }

    /// The counter has to follow the threads the program starts, or a program
    /// that does its work in a thread pool reads as having done nothing.
    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn the_count_includes_a_thread_started_after_the_window_opened() {
        const ITERATIONS: u64 = 50_000_000;
        let expected = (ITERATIONS * 2) as f64;

        let mut collector = CounterCollector::new(0, spec("cpu.instructions"));
        if collector.start().is_err() {
            eprintln!("skipped: this machine refuses unprivileged perf counting");
            return;
        }
        std::thread::spawn(|| known_loop(ITERATIONS))
            .join()
            .expect("the worker thread finished");
        let events = collector.stop();

        let count = events[0].attr("count").unwrap().as_f64().unwrap();
        let error = (count - expected).abs() / expected;
        assert!(
            error < 0.05,
            "the work ran in a thread and retired {expected} instructions, but {count}              were counted: the event is not following the threads it should"
        );
    }

    /// The kernel keeps its own count of context switches per task, so this
    /// one can be checked against something that was not measured by this code.
    ///
    /// `/proc/thread-self` is the calling thread, not the process: a test runs
    /// on a thread cargo started, and the process leader's counters describe a
    /// thread that is doing nothing at all.
    #[test]
    fn context_switches_agree_with_the_kernels_own_count() {
        fn kernel_switches() -> u64 {
            let status = std::fs::read_to_string("/proc/thread-self/status")
                .expect("this thread's status is readable");
            status
                .lines()
                .filter(|line| {
                    line.starts_with("voluntary_ctxt_switches")
                        || line.starts_with("nonvoluntary_ctxt_switches")
                })
                .filter_map(|line| line.split_whitespace().nth(1))
                .filter_map(|value| value.parse::<u64>().ok())
                .sum()
        }

        let mut collector = CounterCollector::new(0, spec("cpu.context_switches"));
        if collector.start().is_err() {
            eprintln!("skipped: this machine refuses unprivileged perf counting");
            return;
        }
        let before = kernel_switches();
        // Every sleep is one switch off the cpu and one back on.
        for _ in 0..50 {
            std::thread::sleep(StdDuration::from_micros(200));
        }
        let kernel = kernel_switches() - before;
        let events = collector.stop();

        let counted = events[0].attr("count").unwrap().as_f64().unwrap();
        assert!(kernel >= 50, "the kernel saw only {kernel} switches");
        let ratio = counted / kernel as f64;
        assert!(
            (0.8..1.25).contains(&ratio),
            "the counter says {counted} context switches where the kernel's own counter              says {kernel}"
        );
    }

    /// A reading, as the kernel would have produced it. The failure paths below
    /// depend on hardware being oversubscribed or a process staying asleep, so
    /// they are built here instead of being waited for.
    fn reading(name: &'static str, raw: u64, enabled_ns: u64, running_ns: u64) -> Reading {
        Reading {
            name,
            raw,
            value: if running_ns == 0 {
                0
            } else {
                ((raw as u128 * enabled_ns as u128) / running_ns as u128) as u64
            },
            enabled_ns,
            running_ns,
        }
    }

    #[test]
    fn a_window_the_counter_never_ran_in_produces_no_number_and_says_why() {
        let mut collector = CounterCollector::new(0, spec("cpu.instructions"));
        let event = collector.event(&[reading("instructions", 0, 5_000_000, 0)], 0);

        assert!(event.is_none(), "a counter that never ran reported a value");
        let losses = collector.losses();
        assert_eq!(losses.failed_reads, 1);
        let reason = losses.reasons.keys().next().expect("a reason").clone();
        assert!(reason.contains("never scheduled"), "{reason}");
        // The distinction this whole path exists for: an idle target has no
        // count, and a count of zero is a different statement.
        assert!(
            reason.contains("not the same as a count of zero"),
            "{reason}"
        );
    }

    #[test]
    fn a_ratio_whose_denominator_counted_nothing_is_not_reported() {
        let mut collector = CounterCollector::new(0, spec("cpu.ipc"));
        let event = collector.event(
            &[
                reading("instructions", 1_000, 5_000_000, 5_000_000),
                reading("cycles", 0, 5_000_000, 5_000_000),
            ],
            0,
        );

        assert!(event.is_none(), "a ratio was reported over zero cycles");
        let losses = collector.losses();
        assert_eq!(losses.failed_reads, 1);
        let reason = losses.reasons.keys().next().expect("a reason").clone();
        assert!(reason.contains("cycles"), "{reason}");
    }

    #[test]
    fn a_multiplexed_counter_publishes_what_was_actually_counted_beside_the_estimate() {
        let mut collector = CounterCollector::new(0, spec("cpu.cycles"));
        // On for a quarter of the window, so the reported value is four times
        // what the hardware saw.
        let event = collector
            .event(&[reading("cycles", 1_000, 4_000_000, 1_000_000)], 0)
            .expect("an estimate is still a number");

        assert_eq!(event.attr("count").unwrap().as_f64().unwrap(), 4_000.0);
        assert_eq!(
            event.attr("cycles_raw").unwrap().as_f64().unwrap(),
            1_000.0,
            "the estimate was published without what it was extrapolated from"
        );
        assert_eq!(
            event.attr("running_ns").unwrap().as_f64().unwrap(),
            1_000_000.0
        );
        assert_eq!(
            event.attr("enabled_ns").unwrap().as_f64().unwrap(),
            4_000_000.0
        );
    }

    #[test]
    fn a_counter_that_was_on_throughout_publishes_no_raw_column() {
        let mut collector = CounterCollector::new(0, spec("cpu.cycles"));
        let event = collector
            .event(&[reading("cycles", 1_000, 4_000_000, 4_000_000)], 0)
            .expect("a full window is a measurement");
        assert_eq!(event.attr("count").unwrap().as_f64().unwrap(), 1_000.0);
        // Nothing was extrapolated, so there is nothing to disclose. A `_raw`
        // column on every event would make the ones that matter invisible.
        assert!(event.attr("cycles_raw").is_none());
        assert_eq!(event.kind, "cpu.cycles");
        assert_eq!(event.seq, 0);
    }

    /// A process asleep for the whole window never reaches a cpu, so there is
    /// nothing to count -- and reporting zero instructions for it would be a
    /// measurement nobody took.
    #[test]
    fn a_target_that_never_runs_reports_no_count_rather_than_zero() {
        let Ok(mut child) = std::process::Command::new("/bin/sleep").arg("5").spawn() else {
            eprintln!("skipped: /bin/sleep is not available");
            return;
        };
        // Let it reach its nanosleep before the window opens.
        std::thread::sleep(StdDuration::from_millis(50));

        let mut collector = CounterCollector::new(child.id() as i32, spec("cpu.instructions"));
        if collector.start().is_err() {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("skipped: this machine refuses unprivileged perf counting");
            return;
        }
        std::thread::sleep(StdDuration::from_millis(150));
        let events = collector.stop();
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            events.is_empty(),
            "a sleeping process was reported as having retired instructions"
        );
        let losses = collector.losses();
        assert_eq!(losses.failed_reads, 1, "{losses:?}");
    }

    #[test]
    fn nothing_is_reported_until_the_window_closes() {
        let mut collector = CounterCollector::new(0, spec("cpu.instructions"));
        if collector.start().is_err() {
            eprintln!("skipped: this machine refuses unprivileged perf counting");
            return;
        }
        for seq in 0..3 {
            // A counted window has one value, at the end. Emitting per tick
            // would suggest a resolution the mode does not have.
            assert!(collector.sample(seq).unwrap().is_empty());
        }
        let events = collector.stop();
        assert_eq!(events.len(), 1);
        // And nothing is left to hand back a second time.
        assert!(collector.stop().is_empty());
    }

    #[test]
    fn a_mode_this_collector_cannot_serve_is_refused_by_name() {
        let error = Prepare::new(
            spec("cpu.cycles"),
            Mode::Sample {
                freq: Mode::DEFAULT_SAMPLE_FREQ,
            },
        )
        .expect_err("sampling a counter is a different mechanism");
        assert!(error.contains("sampling path"), "{error}");
        assert!(error.contains("--mode count"), "{error}");
    }

    /// A loop whose retired-instruction count is known by construction: two
    /// instructions per iteration, a decrement and a conditional branch.
    ///
    /// This is the only kind of ground truth available for a counter. Comparing
    /// it against another tool would only prove the two agree, and comparing it
    /// against itself proves nothing at all.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn known_loop(iterations: u64) {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: the loop only decrements a register it owns and branches on
        // the flag that decrement sets.
        unsafe {
            std::arch::asm!(
                "2:",
                "dec {n}",
                "jnz 2b",
                n = inout(reg) iterations => _,
                options(nostack),
            );
        }
        #[cfg(target_arch = "aarch64")]
        // SAFETY: as above.
        unsafe {
            std::arch::asm!(
                "2:",
                "subs {n}, {n}, #1",
                "b.ne 2b",
                n = inout(reg) iterations => _,
                options(nostack),
            );
        }
    }

    #[test]
    fn every_spec_names_its_value_attribute() {
        for spec in SPECS {
            assert!(!spec.value_attr().is_empty(), "{}", spec.source);
        }
    }
}
