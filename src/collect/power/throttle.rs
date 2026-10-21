//! `power.throttle` through the per-cpu `thermal_throttle` counters.
//!
//! This is the guardrail of the family. A run that throttled is not a slower
//! run, it is a different run: the part changed its operating point in the
//! middle of the measurement, and averaging such a run in with the others
//! produces a number that describes neither. The counters here are what let a
//! run be marked unusable instead.
//!
//! Which makes the failure mode of this source unusually important. The
//! interface is x86-and-mostly-Intel: `thermal_throttle` appears for cpus whose
//! driver registers a thermal interrupt handler, and on a machine without one
//! -- an AMD desktop, an ARM board, a virtual machine -- the directory is
//! simply not there. Reporting zero throttling events in that case would be a
//! measurement nobody took, and it would let a throttled run pass as clean. So
//! the collector refuses to start, by name, and the resolver refuses earlier
//! still: the catalog requires the file, and a machine without it is told that
//! this source is out of reach here.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

use super::sysfs;

const CPU_ROOT: &str = "/sys/devices/system/cpu";

#[derive(Debug)]
pub struct Prepare {
    mode: Mode,
}

impl Prepare {
    pub fn new(mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } => Ok(Self { mode }),
            Mode::Trace => Err(
                "each throttling event with its moment comes from the `thermal:` tracepoints; \
                 sysfs keeps counts, so poll them"
                    .to_owned(),
            ),
            other => Err(format!(
                "throttling is counted, and `{}` gives the count since boot rather than during \
                 the run; poll it",
                other.kind()
            )),
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let root = PathBuf::from(CPU_ROOT);
        let cpus = match target {
            Target::System => sysfs::online_cpus(&root)
                .map_err(|error| format!("cannot tell which cpus are online: {error}"))?,
            Target::Cpu(set) => set.cpus().to_vec(),
            other => {
                return Err(format!(
                    "throttling happens to a core, not to a {}; use --cpu or --system",
                    other.kind()
                ))
            }
        };
        Ok(Box::new(Throttling::new(self.mode, root, cpus)))
    }
}

/// Throttle counts summed over the cpus being watched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Reading {
    ts_ns: u64,
    /// Per cpu, not summed. A machine-wide throttle count says a core was held
    /// back somewhere; which core is the question, because a single core
    /// throttling under an AVX-512 licence and a package throttling under a
    /// thermal limit are different problems with the same total. `cpu` is also
    /// a declared filter field here, which a sum cannot carry.
    per_cpu: BTreeMap<u32, (u64, u64)>,
}

pub struct Throttling {
    #[allow(dead_code)]
    mode: Mode,
    root: PathBuf,
    cpus: Vec<u32>,
    previous: Option<Reading>,
    losses: Losses,
}

impl Throttling {
    pub fn new(mode: Mode, root: PathBuf, cpus: Vec<u32>) -> Self {
        Self {
            mode,
            root,
            cpus,
            previous: None,
            losses: Losses::default(),
        }
    }

    fn directory(&self, cpu: u32) -> PathBuf {
        self.root.join(format!("cpu{cpu}")).join("thermal_throttle")
    }

    /// How many cpus expose the interface at all.
    fn covered(&self) -> usize {
        self.cpus
            .iter()
            .filter(|&&cpu| self.directory(cpu).exists())
            .count()
    }

    fn read(&mut self) -> Result<Reading, SampleError> {
        let mut reading = Reading {
            ts_ns: 0,
            per_cpu: BTreeMap::new(),
        };
        let mut read_any = false;
        for &cpu in &self.cpus {
            let directory = self.directory(cpu);
            if !directory.exists() {
                continue;
            }
            // A cpu that has the directory but not one of the counters is not
            // the same as a cpu with no counters: the package count is absent
            // on parts that report throttling per core only.
            let mut counts = (0u64, 0u64);
            let mut seen = false;
            if let Ok(core) = sysfs::number(&directory.join("core_throttle_count")) {
                counts.0 = core;
                seen = true;
            }
            if let Ok(package) = sysfs::number(&directory.join("package_throttle_count")) {
                counts.1 = package;
                seen = true;
            }
            if seen {
                reading.per_cpu.insert(cpu, counts);
                read_any = true;
            }
        }
        if !read_any {
            return Err(SampleError::Transient(
                "no cpu reported a throttle count".to_owned(),
            ));
        }
        reading.ts_ns = monotonic_ns();
        Ok(reading)
    }
}

impl Collector for Throttling {
    fn value_attr(&self) -> &'static str {
        "throttles"
    }

    /// Refuses, by name, a machine where throttling cannot be seen.
    ///
    /// The alternative is a series of zeros, which reads exactly like a run
    /// that did not throttle. That is the one mistake this source must not
    /// make.
    fn start(&mut self) -> Result<(), String> {
        if self.covered() == 0 {
            return Err(format!(
                "no cpu under {} exposes a `thermal_throttle` directory. It is an x86 thermal \
                 interrupt interface, absent on AMD parts, on ARM and in most virtual machines. \
                 Reporting zero throttling events here would claim a measurement that was never \
                 taken, so this backend refuses: on such a machine, whether a run throttled has \
                 to be established another way",
                self.root.display()
            ));
        }
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let after = self.read()?;
        let Some(before) = self.previous.replace(after.clone()) else {
            // The first read is a baseline: the counts it holds are from boot.
            return Ok(Vec::new());
        };
        let interval_ns = after.ts_ns.saturating_sub(before.ts_ns);
        let mut events = Vec::new();
        for (cpu, (core_after, package_after)) in &after.per_cpu {
            let (core_before, package_before) = before.per_cpu.get(cpu).copied().unwrap_or((0, 0));
            let (Some(core), Some(package)) = (
                core_after.checked_sub(core_before),
                package_after.checked_sub(package_before),
            ) else {
                self.losses.undecoded += 1;
                self.losses.note_times(
                    format!(
                        "the throttle counter for cpu{cpu} went backwards, which means it went \
                     offline and came back; this interval cannot be attributed"
                    ),
                    1,
                );
                continue;
            };
            events.push(Event {
                ts_ns: after.ts_ns,
                ts_err_ns: 0,
                kind: "power.throttle".to_owned(),
                keys: vec![KeyValue::cpu(*cpu)],
                attrs: vec![
                    Attr::uint("throttles", core + package),
                    Attr::uint("core_throttles", core),
                    Attr::uint("package_throttles", package),
                    Attr::uint("cpu", u64::from(*cpu)),
                    Attr::uint("interval_ns", interval_ns),
                ],
                collector_id: CollectorId::unstamped(),
                seq: seq + events.len() as u64,
            });
        }
        Ok(events)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::power::sysfs::scratch;

    fn poll() -> Mode {
        Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        }
    }

    fn throttle_tree(name: &str, core: u64, package: u64) -> PathBuf {
        let root = scratch::tree(name);
        for cpu in 0..2 {
            scratch::write(
                &root,
                &format!("cpu{cpu}/thermal_throttle/core_throttle_count"),
                &core.to_string(),
            );
            scratch::write(
                &root,
                &format!("cpu{cpu}/thermal_throttle/package_throttle_count"),
                &package.to_string(),
            );
        }
        root
    }

    #[test]
    fn what_is_reported_is_the_throttling_during_the_tick_not_since_boot() {
        let root = throttle_tree("throttle", 100, 7);
        let mut collector = Throttling::new(poll(), root.clone(), vec![0, 1]);
        collector.start().expect("the counters are there");
        assert!(
            collector.sample(0).expect("the first read").is_empty(),
            "the first read is a baseline: its counts are from boot"
        );

        for cpu in 0..2 {
            scratch::write(
                &root,
                &format!("cpu{cpu}/thermal_throttle/core_throttle_count"),
                "103",
            );
        }
        let events = collector.sample(1).expect("a second read");

        assert_eq!(events.len(), 2, "one event per cpu");
        // Three more on each of the two cpus, reported against each of them
        // rather than as a machine total of six that names no core.
        for (offset, event) in events.iter().enumerate() {
            assert_eq!(event.keys[0].to_string(), format!("cpu:{offset}"));
            assert_eq!(event.attr("throttles").unwrap().as_f64(), Some(3.0));
            assert_eq!(event.attr("core_throttles").unwrap().as_f64(), Some(3.0));
            assert_eq!(event.attr("package_throttles").unwrap().as_f64(), Some(0.0));
            assert_eq!(event.attr("cpu").unwrap().as_f64(), Some(offset as f64));
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// The failure that matters. On a machine with no such counter, zero
    /// throttling events reads exactly like a run that did not throttle -- and
    /// this is the source whose whole job is to say that a run is unusable.
    #[test]
    fn a_machine_that_cannot_see_throttling_refuses_instead_of_reporting_none() {
        let root = scratch::tree("throttle-none");
        let mut collector = Throttling::new(poll(), root.clone(), vec![0, 1]);
        let refused = collector.start().expect_err("there is nothing to read");
        assert!(refused.contains("AMD"), "{refused}");
        assert!(
            refused.contains("never taken"),
            "the refusal has to say why zero would be a lie: {refused}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A cpu that went offline and came back restarts its counters; the
    /// interval that straddles it is void for that cpu rather than enormous.
    ///
    /// Both cpus are reset here, so both intervals are void and the count of
    /// losses is two -- one per cpu, which is the granularity the reading now
    /// has. Resetting one would leave the other measurable, which is the point
    /// of keeping them apart.
    #[test]
    fn a_counter_that_goes_backwards_is_a_counted_loss() {
        let root = throttle_tree("throttle-backwards", 50, 4);
        let mut collector = Throttling::new(poll(), root.clone(), vec![0, 1]);
        collector.start().expect("the counters are there");
        collector.sample(0).expect("the baseline read");

        for cpu in 0..2 {
            scratch::write(
                &root,
                &format!("cpu{cpu}/thermal_throttle/core_throttle_count"),
                "0",
            );
        }
        assert!(collector.sample(1).expect("a read").is_empty());
        assert_eq!(collector.losses().undecoded, 2);
        assert!(collector
            .losses()
            .reasons
            .keys()
            .any(|reason| reason.contains("went backwards")));
        std::fs::remove_dir_all(&root).ok();
    }

    /// Throttling happens to a core. A target that names a process is refused,
    /// because the machine's throttle count is not that process's.
    #[test]
    fn a_process_target_is_refused_rather_than_answered_for_the_machine() {
        let prepared = Box::new(Prepare::new(poll()).expect("poll is servable"));
        let refused = prepared
            .attach(&Target::Pid(vec![1]))
            .err()
            .expect("a pid does not throttle, a core does");
        assert!(refused.contains("--cpu"), "{refused}");
    }

    /// And on this machine, whichever it is, the collector and the filesystem
    /// have to agree about whether throttling can be seen at all.
    #[test]
    fn this_machine_is_answered_the_same_way_by_the_collector_and_by_sysfs() {
        let root = PathBuf::from(CPU_ROOT);
        let Ok(cpus) = sysfs::online_cpus(&root) else {
            return;
        };
        let exposed = cpus
            .iter()
            .any(|cpu| root.join(format!("cpu{cpu}/thermal_throttle")).exists());
        let mut collector = Throttling::new(poll(), root, cpus);
        assert_eq!(
            collector.start().is_ok(),
            exposed,
            "the collector and sysfs disagree about whether throttling is visible here"
        );
    }
}
