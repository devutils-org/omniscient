//! `cpu.topdown`: where the issue slots went.
//!
//! Every other counting source in this family is a number. This one is a
//! *method*. The pipeline offers a fixed number of issue slots per cycle, and
//! each of them either retires a useful operation or is lost to one of three
//! things: work thrown away after a mispredict, a front end that could not
//! deliver an instruction, or a back end that could not accept one. The four
//! account for all of them, so they sum to one, and the largest is the answer
//! to "what is this program actually waiting on" -- which no individual counter
//! gives, however many of them are read.
//!
//! Three things decide whether the four numbers mean anything.
//!
//! **The events are the kernel's, not this file's.** The catalog says to map
//! per cpu family and never hard-code event names, and the way to do that
//! without carrying a table that is wrong on the next machine is to read the
//! one the kernel publishes: `slots` and the four `topdown-*` events under
//! `/sys/bus/event_source/devices/cpu/events`. A machine that publishes them
//! can be measured, whoever made the cpu; a machine that does not is told which
//! ones are missing.
//!
//! **They are read in one group, with `slots` as the leader.** Four fractions
//! of a denominator taken from a different slice of time are four numbers that
//! describe nothing, and on the hardware that implements this the kernel
//! requires the grouping anyway.
//!
//! **A multiplexed group is scaled before it is divided.** Five counters do not
//! always fit, and this is exactly where an unscaled ratio stops being noisy
//! and becomes wrong: the numerator and the denominator would have been on the
//! hardware for different fractions of the window. `Reading::value` is the
//! scaled number and is what the arithmetic below uses.

use std::time::Duration as StdDuration;

use crate::clock::monotonic_ns;
use crate::collect::task::Comm;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::perf::{Counter, PerCpuGroups, Reading, ThreadCounters};
use crate::request::{Mode, Target};

const TICK: StdDuration = StdDuration::from_millis(10);

/// The performance unit that publishes them. `cpu` on both vendors that
/// implement the method.
const PMU: &str = "cpu";

/// How many slots there were, which every fraction is out of.
const SLOTS: (&str, &str) = ("slots", "slots");

/// The four categories, as this tool names them and as the kernel does.
///
/// The order is the order the method is read in: what got through first, then
/// the three ways a slot is lost, from the cheapest to diagnose to the least.
const CATEGORIES: [(&str, &str); 4] = [
    ("retiring", "topdown-retiring"),
    ("bad_speculation", "topdown-bad-spec"),
    ("frontend_bound", "topdown-fe-bound"),
    ("backend_bound", "topdown-be-bound"),
];

/// The counters this method needs, or the names of the ones this machine does
/// not publish.
///
/// The refusal lists every missing event rather than the first: a reader who
/// learns `slots` is absent and then that `topdown-retiring` is too has been
/// told twice what one message could have said, and the set is what says
/// whether this is a cpu without the method or a kernel too old to expose it.
pub fn counters() -> Result<Vec<Counter>, String> {
    let wanted = std::iter::once(SLOTS).chain(CATEGORIES);
    let mut counters = Vec::new();
    let mut missing = Vec::new();
    for (name, event) in wanted {
        match Counter::from_sysfs(name, PMU, event) {
            Some(counter) => counters.push(counter),
            None => missing.push(event),
        }
    }
    if missing.is_empty() {
        return Ok(counters);
    }
    Err(format!(
        "this cpu publishes no {} under {}/{PMU}/events, so the top-down method has no \
         denominator here: it needs a pipeline that counts issue slots, which is Intel from \
         Icelake and AMD from Zen 4. Nothing else measures the same thing, so nothing is \
         substituted",
        missing.join(", "),
        crate::perf::PMU_DEVICES
    ))
}

/// Turns one group's readings into the four fractions and what they came from.
///
/// Fails rather than reports when there were no slots at all: a window in which
/// the pipeline issued nothing has no distribution to describe, and four zeroes
/// would read as a program that was perfectly stalled in every way at once.
fn fractions(readings: &[Reading]) -> Result<(Vec<Attr>, Option<String>), String> {
    let slots = readings
        .first()
        .ok_or_else(|| "the group came back empty".to_owned())?;
    if slots.never_ran() {
        return Err("the counters were never scheduled onto the hardware".to_owned());
    }
    if slots.value == 0 {
        return Err(
            "the pipeline issued no slots in this window, so there is no distribution to \
             report -- which is not the same as every category being zero"
                .to_owned(),
        );
    }

    let mut attrs = Vec::with_capacity(readings.len() * 2 + 2);
    let mut sum = 0.0;
    for (index, (name, _)) in CATEGORIES.iter().enumerate() {
        let category = readings
            .get(index + 1)
            .ok_or_else(|| format!("`{name}` is missing from the group"))?;
        let fraction = category.value as f64 / slots.value as f64;
        sum += fraction;
        attrs.push(Attr::float(*name, fraction));
    }
    // The counts the fractions are of, so the arithmetic can be checked from
    // the event rather than believed.
    for reading in readings {
        attrs.push(Attr::uint(reading.name, reading.value));
    }
    attrs.push(Attr::float("accounted", sum));

    // A partition of the slots that does not add up to all of them is the one
    // failure this method cannot hide: it means the five counters did not
    // describe the same window, or that this cpu's events are not the ones the
    // method assumes. Reported rather than normalised away, because scaling
    // four numbers to sum to one would make a broken measurement look perfect.
    let note = ((sum - 1.0).abs() > 0.02).then(|| {
        format!(
            "the four categories account for {:.1} % of the issue slots rather than 100 %: \
             the counters did not describe the same window, or this cpu's topdown events do \
             not partition its slots. The fractions are reported as counted, not rescaled",
            sum * 100.0
        )
    });
    Ok((attrs, note))
}

/// Holds the mode until the target exists.
pub struct Prepare(pub Mode);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.0 != Mode::Count {
            return Err(format!(
                "`cpu.topdown` is a distribution over a whole window, which is `--mode count`; \
                 `{}` would ask where each slot went, and the hardware counts slots without \
                 saying where they were",
                self.0
            ));
        }
        let scope = match target {
            Target::Cpu(set) => Scoped::Cpus(set.cpus().to_vec()),
            Target::System => Scoped::Cpus(
                crate::perf::online_cpus()
                    .map_err(|error| format!("cannot tell which cpus are online: {error}"))?,
            ),
            // The counting sources' cgroup shape (`cgroup.rs`): a group per
            // cpu, each filtered to the cgroup, the slots summed.
            Target::Cgroup(path) => Scoped::Cgroup(
                super::cgroup::Cgroup::resolve(path)?,
                crate::perf::online_cpus()
                    .map_err(|error| format!("cannot tell which cpus are online: {error}"))?,
            ),
            other => Scoped::Task(other.first_pid().ok_or_else(|| {
                format!(
                    "`cpu.topdown` counts a task, a cpu or a cgroup, and a {} target is none of \
                     them",
                    other.kind()
                )
            })?),
        };
        let mut collector = Topdown::new(scope);
        // A `--tid` target is that task and what it starts; every other target
        // that names a process is every thread it has.
        collector.whole_process = !matches!(target, Target::Tid(_));
        Ok(Box::new(collector))
    }
}

/// What the counters follow.
enum Scoped {
    Task(i32),
    Cpus(Vec<u32>),
    Cgroup(super::cgroup::Cgroup, Vec<u32>),
}

/// Opened groups, of whichever shape the target asked for.
enum Opened {
    /// One group per thread the task had at the open, each inheriting. See
    /// [`ThreadCounters`].
    Task(ThreadCounters),
    Cpus(PerCpuGroups),
    /// One group per cpu over one cgroup, read as their sum.
    Cgroup(PerCpuGroups),
}

pub struct Topdown {
    id: CollectorId,
    scope: Scoped,
    counters: Vec<Counter>,
    opened: Option<Opened>,
    /// Whether a task scope follows every thread of the process.
    whole_process: bool,
    comm: Comm,
    losses: Losses,
    notes: Vec<String>,
}

impl Topdown {
    fn new(scope: Scoped) -> Self {
        let comm = match scope {
            Scoped::Task(pid) => Comm::of(pid),
            Scoped::Cpus(_) | Scoped::Cgroup(..) => Comm::of(0),
        };
        Self {
            id: CollectorId::new("cpu.topdown/pmu"),
            scope,
            counters: Vec::new(),
            opened: None,
            whole_process: true,
            comm,
            losses: Losses::default(),
            notes: Vec::new(),
        }
    }

    /// One reading set, as the event it describes -- or nothing, with the
    /// reason counted as a failed read.
    fn event(&mut self, readings: &[Reading], keys: Vec<KeyValue>, seq: u64) -> Option<Event> {
        let (mut attrs, note) = match fractions(readings) {
            Ok(both) => both,
            Err(reason) => {
                self.losses.failed_reads += 1;
                self.losses.note(reason);
                return None;
            }
        };
        if let Some(note) = note {
            self.notes.push(note);
        }
        self.notes.extend(super::counters::multiplexing_notes(
            readings,
            &match &self.scope {
                Scoped::Task(pid) => format!("task {pid}"),
                Scoped::Cgroup(cgroup, _) => format!("the cgroup {}", cgroup.directory.display()),
                Scoped::Cpus(_) => keys
                    .first()
                    .map(KeyValue::to_string)
                    .unwrap_or_else(|| "this cpu".to_owned()),
            },
        ));
        if let KeyValue::Cpu { cpu } = keys.first()? {
            attrs.push(Attr::uint("cpu", u64::from(*cpu)));
        }
        if let Some(comm) = self.comm.get() {
            attrs.push(Attr::text("comm", comm));
        }
        Some(Event {
            ts_ns: monotonic_ns(),
            ts_err_ns: 0,
            kind: "cpu.topdown".to_owned(),
            keys,
            attrs,
            collector_id: self.id.clone(),
            seq,
        })
    }
}

impl Collector for Topdown {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    /// The fraction of slots that did useful work. The other three say where
    /// the rest went and travel on the same event; this is the one a series is
    /// built from, because it is the only one of the four that is better when
    /// it is larger.
    fn value_attr(&self) -> &'static str {
        "retiring"
    }

    fn start(&mut self) -> Result<(), String> {
        self.counters = counters()?;
        self.opened = Some(match &self.scope {
            Scoped::Task(pid) => {
                let opened = if self.whole_process {
                    ThreadCounters::open(&self.counters, *pid)
                } else {
                    ThreadCounters::open_task(&self.counters, *pid)
                };
                let group = opened.map_err(|error| {
                    format!(
                        "opening the five top-down counters as one group on pid {pid} was \
                         refused ({error}); they have to be one group, because four \
                         fractions of a denominator from another window describe nothing"
                    )
                })?;
                // Each thread's group is one window of its own, so the sums
                // are of slots that were counted together, thread by thread.
                if group.threads().len() > 1 {
                    self.notes.push(format!(
                        "process {pid} was already running {} threads when the counters were \
                         opened, so a group was opened on each and the slots are their sum",
                        group.threads().len()
                    ));
                }
                if group.unlisted() > 0 {
                    self.notes.push(format!(
                        "{} thread{} of process {pid} appeared while the groups were being \
                         opened; each is counted if the thread that started it already had its \
                         group, and not otherwise",
                        group.unlisted(),
                        if group.unlisted() == 1 { "" } else { "s" },
                    ));
                }
                group.enable().map_err(|error| error.to_string())?;
                Opened::Task(group)
            }
            Scoped::Cpus(cpus) => {
                let groups = PerCpuGroups::open(&self.counters, cpus)
                    .map_err(|reason| format!("`cpu.topdown`: {reason}"))?;
                groups.enable()?;
                Opened::Cpus(groups)
            }
            Scoped::Cgroup(cgroup, cpus) => {
                let groups = PerCpuGroups::open_cgroup(&self.counters, &cgroup.directory, cpus)
                    .map_err(|reason| {
                        format!(
                            "`cpu.topdown` over the cgroup {}: {reason}",
                            cgroup.directory.display()
                        )
                    })?;
                groups.enable()?;
                Opened::Cgroup(groups)
            }
        });
        Ok(())
    }

    fn sample(&mut self, _seq: u64) -> Result<Vec<Event>, SampleError> {
        self.comm.get();
        Ok(Vec::new())
    }

    fn stop(&mut self) -> Vec<Event> {
        match self.opened.take() {
            None => Vec::new(),
            Some(Opened::Task(group)) => {
                let pid = match self.scope {
                    Scoped::Task(pid) => pid,
                    _ => unreachable!("a task group comes from a task scope"),
                };
                match group.read_final() {
                    Ok(readings) => self
                        .event(&readings, vec![KeyValue::task(pid)], 0)
                        .into_iter()
                        .collect(),
                    Err(error) => {
                        self.losses.failed_reads += 1;
                        self.losses
                            .note(format!("reading the top-down counters failed: {error}"));
                        Vec::new()
                    }
                }
            }
            Some(Opened::Cpus(groups)) => {
                let mut events = Vec::new();
                for (cpu, reading) in groups.read_final() {
                    match reading {
                        Ok(readings) => {
                            let seq = events.len() as u64;
                            if let Some(event) =
                                self.event(&readings, vec![KeyValue::cpu(cpu)], seq)
                            {
                                events.push(event);
                            }
                        }
                        Err(error) => {
                            self.losses.failed_reads += 1;
                            self.losses.note(format!(
                                "reading the top-down counters on cpu {cpu} failed: {error}"
                            ));
                        }
                    }
                }
                events
            }
            Some(Opened::Cgroup(groups)) => {
                let (total, failed) = groups.read_total();
                for (cpu, error) in failed {
                    self.losses.failed_reads += 1;
                    self.losses.note(format!(
                        "reading the top-down counters on cpu {cpu} failed ({error}), so the \
                         cgroup's slots there are not in the fractions"
                    ));
                }
                let key = match &self.scope {
                    Scoped::Cgroup(cgroup, _) => cgroup.key(),
                    _ => unreachable!("a cgroup group comes from a cgroup scope"),
                };
                total
                    .and_then(|readings| self.event(&readings, vec![key], 0))
                    .into_iter()
                    .collect()
            }
        }
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

    fn reading(name: &'static str, value: u64) -> Reading {
        Reading {
            name,
            raw: value,
            value,
            enabled_ns: 1_000_000,
            running_ns: 1_000_000,
        }
    }

    /// The one claim the method makes: the four categories are a partition of
    /// the issue slots, so whatever the workload, they sum to one.
    ///
    /// On a machine that publishes the events this is measured. On one that
    /// does not, the refusal is what is checked -- and it has to name the
    /// events rather than the syscall, because "this cpu is too old for the
    /// method" and "this tool cannot build the config" send a reader to
    /// entirely different places.
    #[test]
    fn the_four_categories_account_for_every_issue_slot() {
        match counters() {
            Err(reason) => {
                assert!(reason.contains("slots"), "{reason}");
                assert!(reason.contains("Icelake"), "{reason}");
                // And it must not offer a substitute, because there is none:
                // four fractions from counters that do not partition the slots
                // would be four numbers that describe nothing.
                assert!(reason.contains("nothing is substituted"), "{reason}");
                eprintln!("skipped, and the refusal checked: {reason}");
            }
            Ok(counters) => {
                assert_eq!(counters.len(), 1 + CATEGORIES.len());
                let mut collector = Topdown::new(Scoped::Task(0));
                collector.start().expect("the events are published here");
                let mut total = 0u64;
                for index in 0..20_000_000u64 {
                    total = total.wrapping_add(index.wrapping_mul(7));
                }
                std::hint::black_box(total);
                let events = collector.stop();

                let event = &events[0];
                let accounted = event.attr("accounted").unwrap().as_f64().unwrap();
                assert!(
                    (accounted - 1.0).abs() < 0.02,
                    "the four categories account for {accounted} of the issue slots"
                );
                for (name, _) in CATEGORIES {
                    let fraction = event.attr(name).unwrap().as_f64().unwrap();
                    assert!(
                        (0.0..=1.0).contains(&fraction),
                        "`{name}` is {fraction}, which is not a share of anything"
                    );
                }
                // A loop of dependent arithmetic retires a great deal and is
                // bound by nothing in particular; whatever else it is, it is
                // not mostly front-end bound.
                let retiring = event.attr("retiring").unwrap().as_f64().unwrap();
                let frontend = event.attr("frontend_bound").unwrap().as_f64().unwrap();
                assert!(
                    retiring > frontend,
                    "a tight arithmetic loop is reported as more front-end bound ({frontend}) \
                     than retiring ({retiring})"
                );
            }
        }
    }

    /// The arithmetic, against readings chosen for the cases that matter. A
    /// machine with the events cannot be relied upon to produce a window where
    /// the group was time-shared, and that is the case where an unscaled ratio
    /// stops being noisy and becomes wrong.
    #[test]
    fn the_fractions_are_of_the_scaled_counts_and_carry_what_they_came_from() {
        let (attrs, note) = fractions(&[
            reading("slots", 1_000),
            reading("retiring", 400),
            reading("bad_speculation", 100),
            reading("frontend_bound", 200),
            reading("backend_bound", 300),
        ])
        .expect("a full window is a measurement");
        let named = |name: &str| {
            attrs
                .iter()
                .find(|attr| attr.name == name)
                .unwrap_or_else(|| panic!("`{name}` is on the event"))
                .value
                .as_f64()
                .expect("numeric")
        };
        assert_eq!(named("retiring"), 0.4);
        assert_eq!(named("backend_bound"), 0.3);
        assert_eq!(named("accounted"), 1.0);
        // The counts as well as the fractions: a derived number whose terms are
        // invisible cannot be checked against anything.
        assert_eq!(named("slots"), 1_000.0);
        assert_eq!(named("retiring"), 0.4);
        assert!(note.is_none(), "{note:?}");
    }

    /// A time-shared group reports counts the kernel extrapolated, and the
    /// fractions have to be of those. Dividing a scaled numerator by an
    /// unscaled denominator is the failure this checks for: here every counter
    /// was on for a quarter of the window, and the fractions are unchanged by
    /// that because both halves were scaled the same way.
    #[test]
    fn a_time_shared_group_still_partitions_its_slots() {
        let shared = |name: &'static str, raw: u64| Reading {
            name,
            raw,
            value: raw * 4,
            enabled_ns: 4_000_000,
            running_ns: 1_000_000,
        };
        let (attrs, note) = fractions(&[
            shared("slots", 1_000),
            shared("retiring", 400),
            shared("bad_speculation", 100),
            shared("frontend_bound", 200),
            shared("backend_bound", 300),
        ])
        .expect("an estimate is still a distribution");
        let accounted = attrs
            .iter()
            .find(|attr| attr.name == "accounted")
            .unwrap()
            .value
            .as_f64()
            .unwrap();
        assert_eq!(accounted, 1.0);
        assert!(note.is_none(), "{note:?}");

        // And a set that does not partition the slots is said so rather than
        // rescaled into looking perfect.
        let (_, note) = fractions(&[
            reading("slots", 1_000),
            reading("retiring", 400),
            reading("bad_speculation", 100),
            reading("frontend_bound", 100),
            reading("backend_bound", 100),
        ])
        .expect("a partial account is still reported");
        let note = note.expect("70 % of the slots is not a partition of them");
        assert!(note.contains("70.0 %"), "{note}");
        assert!(note.contains("not rescaled"), "{note}");
    }

    /// A window in which the pipeline issued nothing has no distribution, and
    /// four zeroes would read as a program stalled in every way at once.
    #[test]
    fn a_window_with_no_slots_reports_no_distribution_rather_than_four_zeroes() {
        let error = fractions(&[
            reading("slots", 0),
            reading("retiring", 0),
            reading("bad_speculation", 0),
            reading("frontend_bound", 0),
            reading("backend_bound", 0),
        ])
        .expect_err("no slots is not a distribution");
        assert!(error.contains("no distribution"), "{error}");
        assert!(error.contains("not the same as"), "{error}");

        let never_ran = fractions(&[Reading {
            name: "slots",
            raw: 0,
            value: 0,
            enabled_ns: 4_000_000,
            running_ns: 0,
        }])
        .expect_err("a counter that never ran has no value");
        assert!(never_ran.contains("never scheduled"), "{never_ran}");
    }

    /// Sampling is the question this method cannot answer: the hardware counts
    /// slots without recording where any of them went.
    #[test]
    fn a_mode_this_method_cannot_serve_is_refused_with_why() {
        let Err(error) = Box::new(Prepare(Mode::Sample {
            freq: Mode::DEFAULT_SAMPLE_FREQ,
        }))
        .attach(&Target::Pid(vec![1])) else {
            panic!("sampling a distribution over a window is not a thing to do");
        };
        assert!(error.contains("--mode count"), "{error}");
        assert!(error.contains("without saying where"), "{error}");
    }
}
