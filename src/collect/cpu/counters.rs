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
use crate::collect::task::Comm;
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
const CACHE_REFERENCES: Counter =
    Counter::hardware("cache_references", perf::PERF_COUNT_HW_CACHE_REFERENCES);
const CONTEXT_SWITCHES: Counter =
    Counter::software("context_switches", perf::PERF_COUNT_SW_CONTEXT_SWITCHES);
const MIGRATIONS: Counter = Counter::software("migrations", perf::PERF_COUNT_SW_CPU_MIGRATIONS);

// The cache descriptors. These are the `mem.cache_*` sources, and they are
// here rather than under `src/collect/mem/` because the mechanism decides who
// writes a source: they are rows in this table, read through this group, with
// nothing of their own but a config. A second counters table beside this one,
// for the sake of the first word of an id, would be two copies of the
// multiplexing and window arithmetic that only one of them would be tested
// against.
const L1D_LOAD_MISSES: Counter = Counter::cache(
    "l1d_load_misses",
    perf::PERF_COUNT_HW_CACHE_L1D,
    perf::PERF_COUNT_HW_CACHE_OP_READ,
    perf::PERF_COUNT_HW_CACHE_RESULT_MISS,
);
const L1D_LOADS: Counter = Counter::cache(
    "l1d_loads",
    perf::PERF_COUNT_HW_CACHE_L1D,
    perf::PERF_COUNT_HW_CACHE_OP_READ,
    perf::PERF_COUNT_HW_CACHE_RESULT_ACCESS,
);
const LLC_LOAD_MISSES: Counter = Counter::cache(
    "llc_load_misses",
    perf::PERF_COUNT_HW_CACHE_LL,
    perf::PERF_COUNT_HW_CACHE_OP_READ,
    perf::PERF_COUNT_HW_CACHE_RESULT_MISS,
);
const LLC_LOADS: Counter = Counter::cache(
    "llc_loads",
    perf::PERF_COUNT_HW_CACHE_LL,
    perf::PERF_COUNT_HW_CACHE_OP_READ,
    perf::PERF_COUNT_HW_CACHE_RESULT_ACCESS,
);
const DTLB_LOAD_MISSES: Counter = Counter::cache(
    "dtlb_load_misses",
    perf::PERF_COUNT_HW_CACHE_DTLB,
    perf::PERF_COUNT_HW_CACHE_OP_READ,
    perf::PERF_COUNT_HW_CACHE_RESULT_MISS,
);
const ITLB_LOAD_MISSES: Counter = Counter::cache(
    "itlb_load_misses",
    perf::PERF_COUNT_HW_CACHE_ITLB,
    perf::PERF_COUNT_HW_CACHE_OP_READ,
    perf::PERF_COUNT_HW_CACHE_RESULT_MISS,
);

/// What a source's value is, once the counters have been read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value {
    /// The counter itself, under the name `count`.
    Count,
    /// The first counter divided by the second, under the name of the source's
    /// own metric. A derived source states what it is derived from rather than
    /// pretending to be a counter of its own.
    Ratio { name: &'static str },
    /// Every counter added together, under the name `count`, with each term
    /// published beside it.
    ///
    /// For the one shape where a source is genuinely two counters of the same
    /// thing -- a TLB has an instruction side and a data side, and the catalog
    /// entry names both. The terms travel with the total for the same reason
    /// they travel with a ratio: a number nobody can decompose cannot be
    /// checked, and "which of the two moved" is the first question anyone asks
    /// of it.
    Total,
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
            Value::Count | Value::Total => "count",
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
    // A miss count alone says nothing: a program that does twice the work
    // misses twice as often and is no worse for it. The accesses are counted in
    // the same group so the rate can be had from the event rather than from a
    // second run.
    Spec {
        source: "mem.cache_l1",
        backend: "pmu",
        counters: &[L1D_LOAD_MISSES, L1D_LOADS],
        value: Value::Count,
    },
    Spec {
        source: "mem.cache_llc",
        backend: "pmu",
        counters: &[LLC_LOAD_MISSES, LLC_LOADS],
        value: Value::Count,
    },
    // The architectural cache-miss counter, which is a different measurement
    // and says so in the catalog: it counts every last-level access that
    // missed, not loads only, and what the vendor puts in it varies.
    Spec {
        source: "mem.cache_llc",
        backend: "pmu:generic",
        counters: &[CACHE_MISSES, CACHE_REFERENCES],
        value: Value::Count,
    },
    // The one source whose value is genuinely two counters: a TLB miss on the
    // instruction side and one on the data side are the same event to the
    // program and different events to the hardware.
    Spec {
        source: "mem.cache_tlb",
        backend: "pmu",
        counters: &[DTLB_LOAD_MISSES, ITLB_LOAD_MISSES],
        value: Value::Total,
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
        // A counter follows either a task or a cpu, and the kernel offers no
        // third shape. Which one a target means is decided here, once, so that
        // neither collector has to hold a target it cannot serve.
        match target {
            Target::Cpu(set) => Ok(Box::new(super::percpu::PerCpuCounters::new(
                set.cpus().to_vec(),
                self.0,
            ))),
            Target::System => {
                let cpus = crate::perf::online_cpus()
                    .map_err(|error| format!("cannot tell which cpus are online: {error}"))?;
                Ok(Box::new(super::percpu::PerCpuCounters::new(cpus, self.0)))
            }
            other => {
                let pid = other.first_pid().ok_or_else(|| {
                    format!(
                        "`{}` counts a task or a cpu, and a {} target is neither: name a \
                         process with --pid or --tid, a core with --cpu, or the machine with \
                         --system",
                        self.0.source,
                        other.kind()
                    )
                })?;
                Ok(Box::new(CounterCollector::new(pid, self.0)))
            }
        }
    }
}

/// Turns one cpu's or one task's readings into the attributes of an event, or
/// into nothing with the reason recorded.
///
/// Shared by the two collectors deliberately. A per-cpu count and a per-task
/// count are the same measurement over a different scope, and the parts that
/// decide what the number *means* -- a counter that never ran, a ratio over
/// zero, a value the kernel extrapolated -- have to be the same in both or one
/// of them is only as honest as its own tests.
pub(super) fn measured(
    spec: &Spec,
    readings: &[Reading],
    losses: &mut Losses,
) -> Option<Vec<Attr>> {
    let window = readings.first()?;
    if window.never_ran() {
        losses.failed_reads += 1;
        // Two different things produce `running_ns == 0`, and the pair is told
        // apart by `enabled_ns`: for a per-task event both clocks advance only
        // while the task is on a cpu, so a zero there is a target that never
        // ran, and a non-zero one is a target that ran while the counter did
        // not -- the pmu was full. A group of two, which is what a ratio opens
        // so that both terms cover one window, is the first thing crowded out.
        //
        // Guessing the first and printing it was the defect: a reader whose
        // program was plainly working was told it had not run.
        losses.note(if window.enabled_ns == 0 {
            format!(
                "`{}` was enabled for no time at all: the target did not run during the \
                 window, so there is no count, which is not the same as a count of zero",
                spec.source
            )
        } else {
            format!(
                "`{}` was never scheduled onto a cpu during the window, although the target \
                 ran for {:.2} ms of it: its counters could not fit on the pmu beside what \
                 else is open, and a group is crowded out whole. There is no count, which is \
                 not the same as a count of zero",
                spec.source,
                window.enabled_ns as f64 / 1e6,
            )
        });
        return None;
    }

    let mut attrs = Vec::with_capacity(readings.len() + 4);
    match spec.value {
        Value::Count => {
            attrs.push(Attr::uint("count", window.value));
            attrs.push(Attr::text("counter", window.name));
            // Anything counted beside the value is what the value is read
            // against: a miss count without its accesses is not a rate, and
            // asking for the second in another run would measure another
            // window.
            for reading in &readings[1..] {
                attrs.push(Attr::uint(reading.name, reading.value));
            }
        }
        Value::Ratio { name } => {
            let denominator = readings.get(1)?;
            if denominator.value == 0 {
                losses.failed_reads += 1;
                losses.note(format!(
                    "`{}` counted zero, so `{}` has no value",
                    denominator.name, spec.source
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
        Value::Total => {
            attrs.push(Attr::uint(
                "count",
                readings.iter().map(|reading| reading.value).sum(),
            ));
            attrs.push(Attr::text(
                "counter",
                readings
                    .iter()
                    .map(|reading| reading.name)
                    .collect::<Vec<_>>()
                    .join(" + "),
            ));
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

    // The window the counter itself measured, which is the honest denominator
    // for a rate: the wall clock also contains the time between the last tick
    // and the moment the run was noticed to be over.
    attrs.push(Attr::uint("enabled_ns", window.enabled_ns));
    attrs.push(Attr::uint("running_ns", window.running_ns));
    Some(attrs)
}

/// What the readings had to give up, in words that reach the user.
///
/// A counter that shared the hardware reports a value it did not count, and
/// `where` says which cpu or task that happened on: on a busy machine it
/// happens to some and not others, and one note for the whole run would leave
/// the reader guessing which numbers are estimates.
pub(super) fn multiplexing_notes(readings: &[Reading], whose: &str) -> Vec<String> {
    readings
        .iter()
        .filter(|reading| reading.was_multiplexed() && !reading.never_ran())
        .map(|reading| {
            format!(
                "`{}` on {whose} shared the hardware with other counters and was on for \
                 {:.0} % of the window: its value is scaled from what was counted, not counted",
                reading.name,
                reading.duty() * 100.0
            )
        })
        .collect()
}

/// Reads one source's counters over one window.
pub struct CounterCollector {
    id: CollectorId,
    pid: i32,
    spec: &'static Spec,
    group: Option<CounterGroup>,
    /// The catalog declares `comm` as a filter field on every counting source
    /// of this family, and a filter field nothing emits is false everywhere.
    comm: Comm,
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
            comm: Comm::of(pid),
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
        let mut attrs = measured(self.spec, readings, &mut self.losses)?;
        if let Some(comm) = self.comm.get() {
            attrs.push(Attr::text("comm", comm));
        }

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
        // A counted window has one value and it is read at the end. Two things
        // are worth doing on a tick: noticing that the thing being counted is
        // gone, and keeping the task's name, which is read here rather than at
        // `start` because at `start` a launched program has not execed yet and
        // still carries this tool's own name.
        self.comm.get();
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

        self.notes
            .extend(multiplexing_notes(&readings, &format!("task {}", self.pid)));

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

    /// A mapping of `bytes`, on pages the kernel has been told not to collapse.
    ///
    /// The `MADV_NOHUGEPAGE` is what makes the TLB half of the test below mean
    /// something on any machine rather than on a busy one. Transparent huge
    /// pages turn sixty-four megabytes into thirty-two entries, which every TLB
    /// holds without effort, so the same walk misses a hundred times less on a
    /// quiet machine -- where the kernel can find 2 MiB of contiguous memory --
    /// than on a loaded one, where it cannot. That is a real property of the
    /// hardware and the reason `mem.cache_tlb`'s catalog entry says to read it
    /// beside `mem.thp`; it is also a variable this test has no business
    /// leaving to chance, so it is pinned rather than tolerated.
    struct Pages {
        base: *mut libc::c_void,
        len: usize,
    }

    impl Pages {
        fn new(len: usize) -> Self {
            // SAFETY: an anonymous private mapping of our own, unmapped in Drop.
            let base = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(base, libc::MAP_FAILED, "the mapping must succeed");
            // SAFETY: advising the kernel about the mapping just made. A kernel
            // built without transparent huge pages answers EINVAL, which is the
            // state this asks for anyway.
            unsafe { libc::madvise(base, len, libc::MADV_NOHUGEPAGE) };
            // Touched, so every page is resident before anything is counted.
            // SAFETY: writing inside the mapping.
            unsafe { std::ptr::write_bytes(base.cast::<u8>(), 1, len) };
            Self { base, len }
        }

        fn as_slice(&self) -> &[u8] {
            // SAFETY: the mapping is readable and lives as long as `self`.
            unsafe { std::slice::from_raw_parts(self.base.cast::<u8>(), self.len) }
        }
    }

    impl Drop for Pages {
        fn drop(&mut self) {
            // SAFETY: unmapping what `new` mapped.
            unsafe { libc::munmap(self.base, self.len) };
        }
    }

    /// A buffer of `bytes` and an order in which to visit its cache lines that
    /// the hardware cannot predict.
    ///
    /// The shuffle is what makes the walk below a memory test rather than a
    /// bandwidth test: a sequential walk is prefetched and misses almost
    /// nothing however large the array is, so a sequential version of this
    /// would report a cache that never misses and would pass against a counter
    /// that counts nothing at all.
    ///
    /// Built before the counters are opened, deliberately. Filling sixty-four
    /// megabytes and shuffling a million indices is itself a great deal of
    /// memory traffic, and counting it would mean the large walk was measured
    /// partly on its own setup.
    fn shuffled_lines(bytes: usize) -> (Vec<usize>, Pages) {
        const LINE: usize = 64;
        let lines = bytes / LINE;
        let mut order: Vec<usize> = (0..lines).collect();
        // A fixed seed, so a failure can be reproduced, and xorshift rather
        // than a dependency.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for index in (1..lines).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            order.swap(index, (state % (index as u64 + 1)) as usize);
        }
        (order, Pages::new(bytes))
    }

    /// Touches one byte of every line, in the order given, `passes` times.
    fn walk(order: &[usize], buffer: &Pages, passes: usize) -> usize {
        let buffer = buffer.as_slice();
        let mut total = 0u64;
        for _ in 0..passes {
            for &line in order {
                total += u64::from(buffer[line * 64]);
            }
        }
        std::hint::black_box(total);
        order.len() * passes
    }

    /// What one spec's counters read over `work`, on this thread alone.
    ///
    /// This thread and not this process: `cargo test` runs its tests in
    /// parallel threads of one process, and a process-wide counter would be
    /// counting the other tests' memory traffic as well as this one's.
    fn counted_on_this_thread(spec: &Spec, work: impl FnOnce()) -> Option<Vec<Reading>> {
        // SAFETY: a read-only query of this thread's own id.
        let tid = unsafe { libc::gettid() };
        let group = CounterGroup::open(spec.counters, Scope::Process(tid)).ok()?;
        group.enable().ok()?;
        work();
        group.read_final().ok()
    }

    /// The misses a spec counted, whichever shape its value takes.
    fn misses(spec: &Spec, readings: &[Reading]) -> f64 {
        match spec.value {
            Value::Total => readings.iter().map(|reading| reading.value).sum::<u64>() as f64,
            _ => readings[0].value as f64,
        }
    }

    /// The three `PERF_TYPE_HW_CACHE` fields, unpacked from what each
    /// descriptor packed, and checked against the name it was given.
    ///
    /// This is the deterministic half of the guard. Packing the fields wrongly
    /// does not fail: the open succeeds and the counter counts something else,
    /// so the failure is a plausible number rather than an error. The walk
    /// below catches it on a quiet machine and skips on a loaded one, because
    /// it needs a cache it does not share; this one runs everywhere and costs
    /// nothing, and it is what catches the realistic bug -- two arguments of a
    /// `Counter::cache` call transposed at one of six call sites.
    #[test]
    fn every_cache_descriptor_packs_the_fields_its_name_claims() {
        for (counter, cache, operation, result) in [
            (
                L1D_LOAD_MISSES,
                perf::PERF_COUNT_HW_CACHE_L1D,
                perf::PERF_COUNT_HW_CACHE_OP_READ,
                perf::PERF_COUNT_HW_CACHE_RESULT_MISS,
            ),
            (
                L1D_LOADS,
                perf::PERF_COUNT_HW_CACHE_L1D,
                perf::PERF_COUNT_HW_CACHE_OP_READ,
                perf::PERF_COUNT_HW_CACHE_RESULT_ACCESS,
            ),
            (
                LLC_LOAD_MISSES,
                perf::PERF_COUNT_HW_CACHE_LL,
                perf::PERF_COUNT_HW_CACHE_OP_READ,
                perf::PERF_COUNT_HW_CACHE_RESULT_MISS,
            ),
            (
                LLC_LOADS,
                perf::PERF_COUNT_HW_CACHE_LL,
                perf::PERF_COUNT_HW_CACHE_OP_READ,
                perf::PERF_COUNT_HW_CACHE_RESULT_ACCESS,
            ),
            (
                DTLB_LOAD_MISSES,
                perf::PERF_COUNT_HW_CACHE_DTLB,
                perf::PERF_COUNT_HW_CACHE_OP_READ,
                perf::PERF_COUNT_HW_CACHE_RESULT_MISS,
            ),
            (
                ITLB_LOAD_MISSES,
                perf::PERF_COUNT_HW_CACHE_ITLB,
                perf::PERF_COUNT_HW_CACHE_OP_READ,
                perf::PERF_COUNT_HW_CACHE_RESULT_MISS,
            ),
        ] {
            let name = counter.name;
            let config = counter.config();
            assert_eq!(
                counter.kind(),
                perf::PERF_TYPE_HW_CACHE,
                "`{name}` is not a cache descriptor at all"
            );
            // The kernel's own layout: cache id in the low byte, operation in
            // the next, result in the one above that, and nothing else set.
            assert_eq!(config & 0xff, u64::from(cache), "`{name}`: wrong cache");
            assert_eq!(
                (config >> 8) & 0xff,
                u64::from(operation),
                "`{name}`: wrong operation"
            );
            assert_eq!(
                (config >> 16) & 0xff,
                u64::from(result),
                "`{name}`: wrong result"
            );
            assert_eq!(
                config >> 24,
                0,
                "`{name}` sets bits the kernel does not read"
            );

            // And the name says the same thing the packing does, which is what
            // makes a transposed call site visible rather than merely wrong.
            let is_miss = result == perf::PERF_COUNT_HW_CACHE_RESULT_MISS;
            assert_eq!(
                name.ends_with("_misses"),
                is_miss,
                "`{name}` is named for a {} and packs the other",
                if is_miss { "miss" } else { "access" }
            );
        }
    }

    /// The ground truth of the cache counters: a walk over an array far larger
    /// than any cache misses on nearly every access, and a walk over four
    /// kilobytes misses on almost none.
    ///
    /// The check is the ratio between the two, not either count. An absolute
    /// number depends on this machine's cache sizes, its prefetchers and its
    /// page size; the ratio depends only on the counter counting the thing it
    /// is named after. A counter with the three `PERF_TYPE_HW_CACHE` fields
    /// packed wrongly opens, runs and returns a plausible number -- this is
    /// what catches it, because whatever else it counts does not care how big
    /// the array was.
    ///
    /// The denominator is the number of accesses the walk made, which the test
    /// chose rather than measured.
    ///
    /// **Best of several attempts**, because the cache is the one piece of
    /// hardware this test cannot have to itself. Measured on this machine: the
    /// ratio is 1200 to 123000 on a quiet one and falls to 104, and once to 27,
    /// while the other six hundred tests of this suite run beside it -- and it
    /// falls from both ends, the far walk missing four times *less* as well as
    /// the near one missing three times more. Lowering the bar to survive the
    /// worst sample would weaken the thing the test exists to catch, and a
    /// counter with its three `PERF_TYPE_HW_CACHE` fields packed wrongly counts
    /// something that does not care how big the array was -- so it produces no
    /// good sample, not an unlucky one. Every attempt is reported when they all
    /// fail.
    #[test]
    fn a_walk_that_cannot_fit_in_cache_misses_where_one_that_fits_does_not() {
        // Larger than the 16 MiB a thread of this class of machine sees, by
        // enough that a bigger part would change nothing.
        const LARGE: usize = 64 * 1024 * 1024;
        const SMALL: usize = 4096;
        const ACCESSES: usize = 2 * 1024 * 1024;

        let (far_order, far_buffer) = shuffled_lines(LARGE);
        let (near_order, near_buffer) = shuffled_lines(SMALL);
        let far_passes = ACCESSES / far_order.len();
        let near_passes = ACCESSES / near_order.len();

        // How many times a spec may be measured before the ratio is called
        // absent. Three is enough for one of them to land in a quiet moment,
        // and cheap: the whole test is under a second.
        const ATTEMPTS: usize = 3;
        // Above this, the control walk is not a control: see where it is used.
        const CONTROL_CEILING: f64 = 0.005;

        let mut served = 0;
        // Measured here at roughly one miss per access on the far walk for all
        // three, against a few thousandths on the near one: ratios of four
        // figures on a quiet machine and around a hundred under this suite.
        // The bars below are set well under the worst of those rather than at
        // the best.
        for (source, backend, floor) in [
            ("mem.cache_l1", "pmu", 0.3),
            ("mem.cache_llc", "pmu", 0.3),
            ("mem.cache_llc", "pmu:generic", 0.3),
            // The TLB is the one that huge pages would otherwise decide, which
            // is why the mapping above refuses them; with them, this same walk
            // misses a hundred times less on a quiet machine than on a busy one.
            ("mem.cache_tlb", "pmu", 0.1),
        ] {
            let Some(spec) = spec_for(source, backend) else {
                panic!("`{source}` on `{backend}` is not in the table");
            };
            let mut attempts: Vec<String> = Vec::new();
            let mut opened = false;
            let mut passed = false;
            // Attempts in which the far walk missed often enough to be a
            // measurement of anything.
            let mut controls = 0usize;
            for _ in 0..ATTEMPTS {
                let Some(far) = counted_on_this_thread(spec, || {
                    assert_eq!(walk(&far_order, &far_buffer, far_passes), ACCESSES);
                }) else {
                    // This machine has no such descriptor -- AMD family 17h
                    // publishes no last-level cache event at all, which is why
                    // the chain has a second backend. That is a machine's
                    // limit, and the backend below it is asserted in the same
                    // loop.
                    break;
                };
                let near = counted_on_this_thread(spec, || {
                    assert_eq!(walk(&near_order, &near_buffer, near_passes), ACCESSES);
                })
                .expect("the same counters opened a moment ago");
                opened = true;

                // A multiplexed reading is an extrapolation, not a count, and
                // two of them make a ratio of two estimates. Reported rather
                // than dropped: a machine where every attempt was time-shared
                // is a fact about the machine and belongs in the failure.
                let shared = far.iter().chain(near.iter()).any(Reading::was_multiplexed);
                let per_access = |readings: &[Reading]| misses(spec, readings) / ACCESSES as f64;
                let (loose, tight) = (per_access(&far), per_access(&near));
                // Both bars, together. An attempt with a fine ratio between two
                // tiny numbers is noise with a direction, and one that misses
                // often on both walks is a counter that is not about caches --
                // so neither number alone decides, and an attempt that clears
                // one and not the other has not cleared anything.
                let cleared = loose > tight * 50.0 && loose > floor;
                // Whether the measurement was possible at all, which is a
                // different question from whether it passed.
                //
                // A shuffled walk over sixty-four megabytes must miss on nearly
                // every access -- measured at 1.15 per intended access on a
                // quiet machine. Under the full suite of this project it reads
                // between 0.03 and 0.35, and the near walk simultaneously
                // rises from 0.0008 to 0.03: six hundred other tests share this
                // machine's caches and its SMT siblings, and both walks stop
                // being about how big their arrays were. There is no bar low
                // enough to survive that and high enough to mean anything, so
                // an attempt that could not see the far walk is reported and
                // not counted.
                //
                // What this gives up is covered deterministically above, by the
                // descriptor test that needs no cache of its own.
                // Two conditions, because the contention shows at both ends.
                // The far walk must have missed like a walk over sixty-four
                // megabytes -- measured at 1.15 per intended access on a quiet
                // machine, and between 0.03 and 0.35 under the full suite of
                // this project. And the near walk must still be the control it
                // is: four kilobytes is sixty-four lines and fits in any L1
                // ever built, so more than one miss in two hundred accesses
                // means the L1 belonged to something else -- another of the six
                // hundred tests beside this one, or whatever shares this core's
                // SMT sibling. Quiet, it reads 0.0008.
                //
                // An attempt failing either was not a measurement of how big
                // its arrays were, and there is no bar low enough to survive
                // that and high enough to mean anything. What this gives up is
                // covered deterministically above, by the descriptor test that
                // needs no cache of its own.
                let measurable = loose > floor && tight <= CONTROL_CEILING;
                attempts.push(format!(
                    "far {loose:.5}, near {tight:.5}, ratio {:.0}{}",
                    loose / tight.max(f64::MIN_POSITIVE),
                    if shared { ", time-shared" } else { "" }
                ));
                if measurable {
                    controls += 1;
                }
                // An attempt that cleared both bars has shown the difference a
                // cache makes, whatever the control was doing: the control
                // decides whether a *failure* means anything, not whether a
                // success counts.
                if cleared {
                    passed = true;
                    break;
                }
            }

            if !opened {
                eprintln!("skipped: {source}/{backend} does not open on this machine");
                continue;
            }
            served += 1;
            if !passed && controls == 0 {
                // Loudly, and with the numbers: this is the machine refusing to
                // serve the measurement, not the measurement meaning nothing.
                // The same test run on its own is the one that has an answer.
                eprintln!(
                    "skipped: {source}/{backend} -- in none of {ATTEMPTS} attempts did a walk \
                     over {LARGE} bytes miss more than {floor} times per access while one over \
                     {SMALL} stayed under {CONTROL_CEILING}, so this machine's caches were not \
                     this thread's and neither walk says anything about how big its array was. \
                     Attempts: {}",
                    attempts.join("; ")
                );
                continue;
            }
            assert!(
                passed,
                "{source} via {backend}: no attempt out of {ATTEMPTS} showed the difference a \
                 cache makes -- a walk over {LARGE} bytes must miss more than {floor} times per \
                 access and more than fifty times as often as one over {SMALL}. Attempts: {}",
                attempts.join("; ")
            );
        }
        assert!(
            served >= 3,
            "only {served} of the four cache backends opened here, so the three sources are \
             not all covered on this machine"
        );
    }

    /// A miss count without its denominator is not a rate, and the whole point
    /// of counting the accesses in the same group is that the denominator
    /// describes the same window.
    #[test]
    fn a_cache_source_publishes_the_accesses_its_misses_are_out_of() {
        let spec = spec_for("mem.cache_l1", "pmu").expect("in the table");
        let mut collector = CounterCollector::new(0, spec);
        if collector.start().is_err() {
            eprintln!("skipped: this machine refuses unprivileged perf counting");
            return;
        }
        let (order, buffer) = shuffled_lines(1024 * 1024);
        walk(&order, &buffer, 4);
        let events = collector.stop();

        let event = &events[0];
        let misses = event.attr("count").unwrap().as_f64().unwrap();
        let loads = event.attr("l1d_loads").unwrap().as_f64().unwrap();
        assert_eq!(
            event.attr("counter").unwrap().to_string(),
            "l1d_load_misses"
        );
        assert!(
            loads > misses,
            "the cache missed {misses} times in {loads} loads, which is more misses than \
             accesses: the two counters are not what they are labelled"
        );
    }

    /// A total of two counters has to be that total, and both terms have to be
    /// on the event: "the data side or the instruction side" is the first
    /// question asked of a TLB number.
    #[test]
    fn a_total_is_the_sum_of_its_terms_and_carries_them() {
        let spec = spec_for("mem.cache_tlb", "pmu").expect("in the table");
        let mut losses = Losses::default();
        let attrs = measured(
            spec,
            &[
                reading("dtlb_load_misses", 700, 1_000, 1_000),
                reading("itlb_load_misses", 30, 1_000, 1_000),
            ],
            &mut losses,
        )
        .expect("a full window is a measurement");

        let named = |name: &str| {
            attrs
                .iter()
                .find(|attr| attr.name == name)
                .unwrap_or_else(|| panic!("`{name}` is on the event"))
        };
        assert_eq!(named("count").value.as_f64(), Some(730.0));
        assert_eq!(named("dtlb_load_misses").value.as_f64(), Some(700.0));
        assert_eq!(named("itlb_load_misses").value.as_f64(), Some(30.0));
        assert_eq!(
            named("counter").value.to_string(),
            "dtlb_load_misses + itlb_load_misses"
        );
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

    /// Two different things produce `running_ns == 0`, and the message has to
    /// say which. For a per-task event both clocks advance only while the task
    /// is on a cpu, so `enabled_ns` is the discriminator: zero is a target that
    /// never ran, and non-zero is a target that ran while its counters could
    /// not fit on the pmu -- a group is crowded out whole, which is why a ratio
    /// meets this where a single counter is only time-shared.
    ///
    /// Picking the first and printing it whichever had happened is the defect
    /// `docs/scenarios/cpu.md` records as cpu 9: a reader whose program was
    /// plainly working was told it had not run.
    #[test]
    fn a_window_the_counter_never_ran_in_produces_no_number_and_says_why() {
        let mut collector = CounterCollector::new(0, spec("cpu.instructions"));
        let event = collector.event(&[reading("instructions", 0, 0, 0)], 0);

        assert!(event.is_none(), "a counter that never ran reported a value");
        let losses = collector.losses();
        assert_eq!(losses.failed_reads, 1);
        let reason = losses.reasons.keys().next().expect("a reason").clone();
        assert!(reason.contains("the target did not run"), "{reason}");
        assert!(
            !reason.contains("pmu"),
            "a target that never got a cpu is being blamed on the hardware: {reason}"
        );
        // The distinction this whole path exists for: an idle target has no
        // count, and a count of zero is a different statement.
        assert!(
            reason.contains("not the same as a count of zero"),
            "{reason}"
        );
    }

    /// The other half of the same fork: the target ran for five milliseconds of
    /// the window and the counter was on for none of it.
    #[test]
    fn a_counter_crowded_off_the_pmu_blames_the_pmu_and_not_the_target() {
        let mut collector = CounterCollector::new(0, spec("cpu.instructions"));
        let event = collector.event(&[reading("instructions", 0, 5_000_000, 0)], 0);

        assert!(event.is_none(), "a counter that never ran reported a value");
        let reason = collector
            .losses()
            .reasons
            .keys()
            .next()
            .expect("a reason")
            .clone();
        assert!(reason.contains("could not fit on the pmu"), "{reason}");
        assert!(
            reason.contains("5.00 ms"),
            "the window the target did run for is what tells the reader which of the two \
             happened, and it is not in the message: {reason}"
        );
        assert!(
            !reason.contains("the target did not run"),
            "a target that ran is being reported as one that did not: {reason}"
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
