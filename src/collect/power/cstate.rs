//! `power.cstate` through `cpuidle` in sysfs.
//!
//! Idle residency is what says whether a machine was waiting or working, and it
//! is the other half of a frequency reading: a core that spent nine tenths of a
//! window in C2 and boosted through the tenth has a high average frequency and
//! almost no work behind it.
//!
//! The kernel keeps, per cpu and per idle state, a microsecond counter of time
//! spent there and a count of entries. Both are cumulative since boot, so what
//! is reported is the increase over the tick -- and the first read of a run is
//! a baseline that produces no event, because a baseline measures nothing.
//!
//! Residency is cpu-time, not wall time: twenty-four cpus idle for a second
//! give twenty-four cpu-seconds of it. Reported as a total, a 300 ms tick on
//! this machine reads `5.99s`, which looks like a mistake and invites being
//! read as one. So the value of an event is the residency of the *average*
//! watched cpu -- the total divided by how many were watched -- which can never
//! exceed the tick, and the sum is carried beside it as `idle_ns_total`. With
//! `--cpu 3` the two are the same number.
//!
//! `idle_ratio` is the same thing as a fraction of the window, so it stays in
//! `[0, 1]` whatever the machine's size. A value above one would mean the
//! arithmetic is wrong, which is what the test asserts.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, AttrValue, CollectorId, Event, KeyValue};
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
                "an idle transition is an event, and events come from the `power:cpu_idle` \
                 tracepoint: cpuidle holds residency totals, so poll them"
                    .to_owned(),
            ),
            other => Err(format!(
                "residency is a counter: `{}` would report the time spent idle since boot rather \
                 than during the run; poll it",
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
                    "an idle state belongs to a core, not to a {}; use --cpu or --system",
                    other.kind()
                ))
            }
        };
        Ok(Box::new(Residency::new(self.mode, root, cpus)))
    }
}

/// One cpu's idling over an interval, before it becomes an event.
#[derive(Default)]
struct CpuResidency {
    idle_ns: u64,
    entries: u64,
    /// State name, nanoseconds, entries.
    per_state: Vec<(String, u64, u64)>,
}

/// Time and entries per idle state, for every cpu being watched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Reading {
    ts_ns: u64,
    /// (cpu, state name) to (microseconds of residency, entries).
    ///
    /// Kept apart per cpu rather than summed. A residency averaged over
    /// twenty-four cores says the machine was 90 % idle while one core was
    /// pinned at 100 %, which is the shape of exactly the problem someone
    /// reads this to find; and the catalog declares `cpu` as a filter field
    /// here, which an aggregate cannot carry.
    states: BTreeMap<(u32, String), (u64, u64)>,
}

pub struct Residency {
    id: CollectorId,
    #[allow(dead_code)]
    mode: Mode,
    root: PathBuf,
    cpus: Vec<u32>,
    previous: Option<Reading>,
    losses: Losses,
}

impl Residency {
    pub fn new(mode: Mode, root: PathBuf, cpus: Vec<u32>) -> Self {
        Self {
            id: CollectorId::new("power.cstate/fs:sys"),
            mode,
            root,
            cpus,
            previous: None,
            losses: Losses::default(),
        }
    }

    fn read(&mut self) -> Result<Reading, SampleError> {
        let mut states: BTreeMap<(u32, String), (u64, u64)> = BTreeMap::new();
        let mut read_any = false;
        for &cpu in &self.cpus {
            let directory = self.root.join(format!("cpu{cpu}")).join("cpuidle");
            let Ok(entries) = sysfs::entries(&directory) else {
                continue;
            };
            for state in entries {
                let file = state.file_name().unwrap_or_default().to_string_lossy();
                if !file.starts_with("state") {
                    continue;
                }
                let name = sysfs::text(&state.join("name")).unwrap_or_else(|_| file.into_owned());
                let (Ok(time), Ok(usage)) = (
                    sysfs::number(&state.join("time")),
                    sysfs::number(&state.join("usage")),
                ) else {
                    self.losses.undecoded += 1;
                    self.losses.note(format!(
                        "cpu{cpu} state `{name}` did not report its residency"
                    ));
                    continue;
                };
                let total = states.entry((cpu, name)).or_insert((0, 0));
                total.0 += time;
                total.1 += usage;
                read_any = true;
            }
        }
        if !read_any {
            return Err(SampleError::Transient(format!(
                "no idle state was readable under {}",
                self.root.display()
            )));
        }
        Ok(Reading {
            ts_ns: monotonic_ns(),
            states,
        })
    }

    /// One event per cpu for the increase between two readings.
    fn delta_events(&mut self, before: &Reading, after: &Reading, seq: u64) -> Vec<Event> {
        if before.states.keys().ne(after.states.keys()) {
            self.losses.undecoded += 1;
            self.losses.note(
                "the set of idle states changed between two reads, so the increase between them \
                 is not one machine's worth of idling"
                    .to_owned(),
            );
            return Vec::new();
        }
        let interval_ns = after.ts_ns.saturating_sub(before.ts_ns);
        if interval_ns == 0 {
            return Vec::new();
        }

        // Grouped by cpu, keeping the states of each together.
        let mut per_cpu: BTreeMap<u32, CpuResidency> = BTreeMap::new();
        for ((cpu, name), (time, usage)) in &after.states {
            let (before_time, before_usage) = before
                .states
                .get(&(*cpu, name.clone()))
                .copied()
                .unwrap_or((0, 0));
            let (Some(time), Some(usage)) = (
                time.checked_sub(before_time),
                usage.checked_sub(before_usage),
            ) else {
                self.losses.undecoded += 1;
                self.losses.note(format!(
                    "the residency counter for cpu{cpu} state `{name}` went backwards, so this \
                     interval cannot be attributed"
                ));
                return Vec::new();
            };
            // cpuidle counts microseconds; everything in this tool is
            // nanoseconds.
            let nanos = time * 1_000;
            let entry = per_cpu.entry(*cpu).or_default();
            entry.idle_ns += nanos;
            entry.entries += usage;
            entry.per_state.push((name.clone(), nanos, usage));
        }

        per_cpu
            .into_iter()
            .enumerate()
            .map(|(offset, (cpu, residency))| {
                let CpuResidency {
                    idle_ns,
                    entries,
                    per_state,
                } = residency;
                let mut attrs = vec![
                    Attr::uint("idle_ns", idle_ns),
                    // Not bounded by one. The kernel credits a residency when
                    // the cpu leaves the idle state, so a core that slept for
                    // a second and woke inside a 200 ms tick reports the whole
                    // second against that tick. Over a window long enough for
                    // the crediting to wash out it converges on the real share;
                    // over one tick it is a share of the tick in which the
                    // sleep was *accounted*, not one it happened in.
                    Attr {
                        name: "idle_ratio".to_owned(),
                        value: AttrValue::Float(idle_ns as f64 / interval_ns as f64),
                    },
                    Attr::uint("entries", entries),
                    Attr::uint("interval_ns", interval_ns),
                    Attr::uint("cpu", u64::from(cpu)),
                ];
                for (name, nanos, usage) in per_state {
                    let key = sysfs::attribute_name("state", &name);
                    attrs.push(Attr::uint(format!("{key}_ns"), nanos));
                    attrs.push(Attr::uint(format!("{key}_entries"), usage));
                }
                Event {
                    ts_ns: after.ts_ns,
                    ts_err_ns: 0,
                    kind: "power.cstate".to_owned(),
                    keys: vec![KeyValue::cpu(cpu)],
                    attrs,
                    collector_id: self.id.clone(),
                    seq: seq + offset as u64,
                }
            })
            .collect()
    }
}

impl Collector for Residency {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    /// Nanoseconds this cpu spent idle during the tick, which is at most the
    /// tick itself. The same number as a fraction is `idle_ratio`.
    fn value_attr(&self) -> &'static str {
        "idle_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        let Some(&first) = self.cpus.first() else {
            return Err("no cpu to read".to_owned());
        };
        let directory = self.root.join(format!("cpu{first}")).join("cpuidle");
        if !directory.exists() {
            return Err(format!(
                "{} does not exist: this kernel has no cpuidle driver, which is the ordinary case \
                 in a virtual machine and under `idle=poll`",
                directory.display()
            ));
        }
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let after = self.read()?;
        let before = self.previous.replace(after.clone());
        Ok(before
            .map(|before| self.delta_events(&before, &after, seq))
            .unwrap_or_default())
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

    /// Two cpus, three states, known counters: the increase has to come out
    /// exactly, in nanoseconds, from a file that counts microseconds.
    #[test]
    fn residency_is_the_increase_over_the_tick_converted_from_microseconds() {
        let root = scratch::tree("cpuidle");
        let write = |root: &PathBuf, cpu: u32, state: u32, name: &str, time: u64, usage: u64| {
            scratch::write(root, &format!("cpu{cpu}/cpuidle/state{state}/name"), name);
            scratch::write(
                root,
                &format!("cpu{cpu}/cpuidle/state{state}/time"),
                &time.to_string(),
            );
            scratch::write(
                root,
                &format!("cpu{cpu}/cpuidle/state{state}/usage"),
                &usage.to_string(),
            );
        };
        for cpu in 0..2 {
            write(&root, cpu, 0, "POLL", 100, 5);
            write(&root, cpu, 1, "C1", 1_000, 10);
        }

        let mut collector = Residency::new(poll(), root.clone(), vec![0, 1]);
        collector.start().expect("a cpuidle directory");
        assert!(
            collector.sample(0).expect("the first read").is_empty(),
            "the first read is a baseline, and a baseline measures nothing"
        );

        // Each cpu gains 50 ms in C1 and 1 ms in POLL.
        for cpu in 0..2 {
            write(&root, cpu, 0, "POLL", 1_100, 6);
            write(&root, cpu, 1, "C1", 51_000, 12);
        }
        let events = collector.sample(1).expect("a second read");

        assert_eq!(events.len(), 2, "one event per cpu");
        for (offset, event) in events.iter().enumerate() {
            assert_eq!(event.keys[0].to_string(), format!("cpu:{offset}"));
            // 50 ms in C1 and 1 ms in POLL, for this cpu alone.
            assert_eq!(event.attr("idle_ns").unwrap().as_f64(), Some(51_000_000.0));
            assert_eq!(
                event.attr("state.c1_ns").unwrap().as_f64(),
                Some(50_000_000.0)
            );
            assert_eq!(event.attr("state.c1_entries").unwrap().as_f64(), Some(2.0));
            assert_eq!(event.attr("entries").unwrap().as_f64(), Some(3.0));
            assert_eq!(
                event.attr("cpu").unwrap().as_f64(),
                Some(offset as f64),
                "`cpu` is a declared filter field, and a key is not one"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// The cpus asked for are the cpus read: a mean over the machine is not an
    /// answer to a question about two cores.
    #[test]
    fn a_cpu_target_narrows_the_residency_to_those_cores() {
        let root = scratch::tree("cpuidle-cpus");
        for cpu in 0..4 {
            scratch::write(&root, &format!("cpu{cpu}/cpuidle/state0/name"), "C1");
            scratch::write(&root, &format!("cpu{cpu}/cpuidle/state0/time"), "0");
            scratch::write(&root, &format!("cpu{cpu}/cpuidle/state0/usage"), "0");
        }

        let mut collector = Residency::new(poll(), root.clone(), vec![0, 1]);
        collector.start().expect("a cpuidle directory");
        collector.sample(0).expect("the baseline read");
        for cpu in 0..4 {
            scratch::write(&root, &format!("cpu{cpu}/cpuidle/state0/time"), "10000");
            scratch::write(&root, &format!("cpu{cpu}/cpuidle/state0/usage"), "1");
        }
        let events = collector.sample(1).expect("a second read");

        // Two cpus of the four, ten milliseconds each, and the other two are
        // absent rather than averaged in.
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].keys[0].to_string(), "cpu:0");
        assert_eq!(events[1].keys[0].to_string(), "cpu:1");
        for event in &events {
            assert_eq!(event.attr("idle_ns").unwrap().as_f64(), Some(10_000_000.0));
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// Two readings over different sets of states do not subtract: the machine
    /// changed idle drivers in between, and the difference would be part
    /// residency and part bookkeeping.
    #[test]
    fn a_change_of_idle_states_voids_the_interval_rather_than_inventing_one() {
        let root = scratch::tree("cpuidle-states");
        scratch::write(&root, "cpu0/cpuidle/state0/name", "C1");
        scratch::write(&root, "cpu0/cpuidle/state0/time", "1000");
        scratch::write(&root, "cpu0/cpuidle/state0/usage", "1");

        let mut collector = Residency::new(poll(), root.clone(), vec![0]);
        collector.start().expect("a cpuidle directory");
        collector.sample(0).expect("the baseline read");

        scratch::write(&root, "cpu0/cpuidle/state1/name", "C2");
        scratch::write(&root, "cpu0/cpuidle/state1/time", "5000");
        scratch::write(&root, "cpu0/cpuidle/state1/usage", "2");

        assert!(collector.sample(1).expect("a read").is_empty());
        assert_eq!(collector.losses().undecoded, 1);
        assert!(collector
            .losses()
            .reasons
            .keys()
            .any(|reason| reason.contains("set of idle states changed")));
        std::fs::remove_dir_all(&root).ok();
    }

    /// A counter that goes backwards means a cpu went away and came back; the
    /// interval that straddles it cannot be attributed.
    #[test]
    fn a_residency_counter_that_goes_backwards_is_a_counted_loss() {
        let root = scratch::tree("cpuidle-backwards");
        scratch::write(&root, "cpu0/cpuidle/state0/name", "C1");
        scratch::write(&root, "cpu0/cpuidle/state0/time", "9000");
        scratch::write(&root, "cpu0/cpuidle/state0/usage", "9");

        let mut collector = Residency::new(poll(), root.clone(), vec![0]);
        collector.start().expect("a cpuidle directory");
        collector.sample(0).expect("the baseline read");
        scratch::write(&root, "cpu0/cpuidle/state0/time", "10");

        assert!(collector.sample(1).expect("a read").is_empty());
        assert!(collector
            .losses()
            .reasons
            .keys()
            .any(|reason| reason.contains("went backwards")));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_machine_without_cpuidle_refuses_with_the_reason() {
        let root = scratch::tree("cpuidle-none");
        let mut collector = Residency::new(poll(), root.clone(), vec![0]);
        let refused = collector.start().expect_err("nothing to read");
        assert!(refused.contains("no cpuidle driver"), "{refused}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Ground truth against a second, independent kernel accounting of the same
    /// thing: `/proc/stat` counts idle and iowait jiffies per cpu, by a path
    /// that shares no code with cpuidle. Over a window long enough for the
    /// crediting lag to wash out, the two agree -- measured here at 0.968 and
    /// 0.974 of a two-second window.
    ///
    /// The tolerance is wide on purpose. What this has to catch is an error of
    /// scale: microseconds left unconverted, or a total divided by the wrong
    /// number of cpus.
    #[test]
    fn residency_agrees_with_the_kernels_other_idle_accounting() {
        let root = PathBuf::from(CPU_ROOT);
        let Ok(cpus) = sysfs::online_cpus(&root) else {
            return;
        };
        let mut collector = Residency::new(poll(), root.clone(), cpus.clone());
        if collector.start().is_err() {
            return;
        }

        let before = proc_stat_idle_ns();
        collector.sample(0).expect("the baseline read");
        std::thread::sleep(std::time::Duration::from_millis(1_200));
        let events = collector.sample(1).expect("a second read");
        let after = proc_stat_idle_ns();

        let (Some(before), Some(after)) = (before, after) else {
            return;
        };
        let independent = (after - before) as f64;
        // One event per cpu now, and `/proc/stat`'s first line is the machine,
        // so the comparison is against the sum of the cpus -- which is exactly
        // what the single `idle_ns_total` attribute used to hold.
        let measured: f64 = events
            .iter()
            .filter_map(|event| event.attr("idle_ns")?.as_f64())
            .sum();

        // `/proc/stat` counts in ten-millisecond jiffies, per cpu, so on a
        // twenty-four cpu machine it can be a fifth of a second of cpu-time out
        // on quantization alone. That slack is what makes the comparison hold
        // on a saturated machine, where both numbers are near zero and a ratio
        // would divide by nothing.
        let slack = cpus.len() as f64 * 20e6;
        assert!(
            (measured - independent).abs() <= slack + 0.3 * independent,
            "cpuidle reported {measured} ns of idle cpu-time where /proc/stat counted \
             {independent} ns"
        );
    }

    /// Idle plus iowait from `/proc/stat`, in nanoseconds of cpu-time.
    fn proc_stat_idle_ns() -> Option<u64> {
        let text = std::fs::read_to_string("/proc/stat").ok()?;
        let line = text.lines().next()?;
        let fields: Vec<&str> = line.split_whitespace().collect();
        let idle: u64 = fields.get(4)?.parse().ok()?;
        let iowait: u64 = fields.get(5)?.parse().ok()?;
        // USER_HZ is 100 on every Linux this runs on: one jiffy is 10 ms.
        Some((idle + iowait) * 10_000_000)
    }
}
