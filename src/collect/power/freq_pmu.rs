//! `power.freq` through the `msr` performance unit: APERF, MPERF and the TSC.
//!
//! The three counters turbostat reads, opened as perf events instead of
//! through `/dev/cpu/*/msr`: the kernel's `msr` unit publishes them, and opens
//! them for any user `perf_event_paranoid` (or `CAP_PERFMON`) allows, where the
//! device files are root's. Over one interval, per cpu:
//!
//! - MPERF counts at a fixed reference rate, the TSC's, while the cpu is in
//!   C0; so `busy = dMPERF / dTSC` is the share of the interval the cpu was
//!   not idle -- turbostat's `Busy%` -- and `idle` is the rest.
//! - APERF counts at the rate the core actually ran, also only in C0; so
//!   `hz = tsc_hz * dAPERF / dMPERF` is the frequency while it ran --
//!   turbostat's `Bzy_MHz` -- and `avg_hz = dAPERF / dt` is that frequency
//!   spread over the whole interval, idle included -- `Avg_MHz`.
//!
//! Unlike `cpuinfo_avg_freq`, which is the last scheduler tick's ratio and a
//! setpoint on an idle cpu, every number here covers the interval between two
//! reads, and an idle cpu's busy frequency is still the one it ran at when it
//! woke. A cpu that never left idle over an interval (MPERF did not move) is
//! still an event -- busy 0, idle 1, `measures=idle` -- without `hz`, since it
//! ran at no clock. Two reads that cannot be divided (a counter read lower,
//! or no time passed) are a failed read, counted with the cpu named. The read
//! at window open is the baseline and produces no event.

use std::collections::BTreeMap;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::perf::{Counter, CounterGroup, Scope};
use crate::request::{Mode, Target};

use super::sysfs;

const PMU: &str = "msr";
const CPU_ROOT: &str = "/sys/devices/system/cpu";

#[derive(Debug)]
pub struct Prepare;

impl Prepare {
    pub fn new(mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } => Ok(Self),
            other => Err(format!(
                "a frequency through APERF and MPERF is a ratio of two differences over an \
                 interval: `{}` has no interval to divide by; poll it",
                other.kind()
            )),
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let cpus = match target {
            Target::System => sysfs::online_cpus(std::path::Path::new(CPU_ROOT))
                .map_err(|error| format!("cannot tell which cpus are online: {error}"))?,
            Target::Cpu(set) => set.cpus().to_vec(),
            other => {
                return Err(format!(
                    "a frequency belongs to a core, not to a {}: the cores a process ran on are \
                     what `sched` reports; use --cpu or --system",
                    other.kind()
                ))
            }
        };
        Ok(Box::new(Clocks {
            id: CollectorId::new("power.freq/pmu"),
            cpus,
            groups: Vec::new(),
            previous: BTreeMap::new(),
            losses: Losses::default(),
        }))
    }
}

/// One cpu's three counters at one instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Counts {
    pub aperf: u64,
    pub mperf: u64,
    pub tsc: u64,
    pub ts_ns: u64,
}

/// What one interval of one cpu measures.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Interval {
    /// The frequency while the cpu was in C0 (turbostat's `Bzy_MHz`), or
    /// nothing when it never was: no MPERF tick, so no clock it ran at.
    pub hz: Option<f64>,
    /// The cpu's cycles over the whole interval (`Avg_MHz`).
    pub avg_hz: f64,
    /// The share of the interval in C0 (`Busy%`).
    pub busy: f64,
    pub idle: f64,
    /// The reference rate MPERF and the TSC count at.
    pub tsc_hz: f64,
    pub interval_ns: u64,
}

/// Why two reads of one cpu measure no interval: each is a fault of the
/// measurement, and counted as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Undivided {
    /// A counter read lower than before: a reset, a wrap or a migrated
    /// counter, and no difference that means anything.
    Backwards,
    /// No time passed between the two reads, by the clock or by the TSC.
    NoTime,
}

impl Undivided {
    fn reason(self, cpu: u32) -> String {
        match self {
            Undivided::Backwards => format!(
                "cpu{cpu}: APERF, MPERF or the TSC read lower than at the previous read, so \
                 that interval measured nothing"
            ),
            Undivided::NoTime => format!(
                "cpu{cpu}: no time passed between two reads (by the clock or the TSC), so \
                 that interval measured nothing"
            ),
        }
    }
}

impl Interval {
    /// The interval between two reads. A cpu that never left idle is a
    /// measurement -- busy 0, idle 1, no frequency; a counter that went
    /// backwards or no time passing is not.
    pub fn between(before: Counts, after: Counts) -> Result<Interval, Undivided> {
        let (Some(aperf), Some(mperf), Some(tsc)) = (
            after.aperf.checked_sub(before.aperf),
            after.mperf.checked_sub(before.mperf),
            after.tsc.checked_sub(before.tsc),
        ) else {
            return Err(Undivided::Backwards);
        };
        let interval_ns = after
            .ts_ns
            .checked_sub(before.ts_ns)
            .ok_or(Undivided::Backwards)?;
        if tsc == 0 || interval_ns == 0 {
            return Err(Undivided::NoTime);
        }
        let seconds = interval_ns as f64 / 1e9;
        let tsc_hz = tsc as f64 / seconds;
        let busy = (mperf as f64 / tsc as f64).min(1.0);
        Ok(Interval {
            hz: (mperf > 0).then(|| tsc_hz * aperf as f64 / mperf as f64),
            avg_hz: aperf as f64 / seconds,
            busy,
            idle: 1.0 - busy,
            tsc_hz,
            interval_ns,
        })
    }
}

pub struct Clocks {
    id: CollectorId,
    cpus: Vec<u32>,
    groups: Vec<(u32, CounterGroup)>,
    previous: BTreeMap<u32, Counts>,
    losses: Losses,
}

impl Clocks {
    fn read(&mut self) -> BTreeMap<u32, Counts> {
        let mut counts = BTreeMap::new();
        for (cpu, group) in &self.groups {
            let values = match group.read() {
                Ok(values) if values.len() == 3 && values.iter().all(|v| !v.never_ran()) => values,
                Ok(_) => {
                    self.losses.failed_reads += 1;
                    self.losses
                        .note(format!("cpu{cpu}: APERF, MPERF or the TSC never ran"));
                    continue;
                }
                Err(error) => {
                    self.losses.failed_reads += 1;
                    self.losses.note_times(format!("cpu{cpu}: {error}"), 1);
                    continue;
                }
            };
            counts.insert(
                *cpu,
                Counts {
                    aperf: values[0].value,
                    mperf: values[1].value,
                    tsc: values[2].value,
                    ts_ns: monotonic_ns(),
                },
            );
        }
        counts
    }
}

impl Collector for Clocks {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "hz"
    }

    fn start(&mut self) -> Result<(), String> {
        let counters = ["aperf", "mperf", "tsc"]
            .into_iter()
            .map(|event| {
                Counter::from_sysfs(event, PMU, event).ok_or_else(|| {
                    format!("the `{PMU}` performance unit publishes no `{event}` event here")
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        for &cpu in &self.cpus {
            let group = CounterGroup::open(&counters, Scope::Cpu(cpu)).map_err(|error| {
                format!(
                    "cannot open APERF, MPERF and the TSC on cpu{cpu}: {error}; check \
                     /proc/sys/kernel/perf_event_paranoid or grant CAP_PERFMON"
                )
            })?;
            group
                .enable()
                .map_err(|error| format!("cannot start the counters on cpu{cpu}: {error}"))?;
            self.groups.push((cpu, group));
        }
        if self.groups.is_empty() {
            return Err("no cpu to read".to_owned());
        }
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let counts = self.read();
        if counts.is_empty() {
            return Err(SampleError::TargetGone(
                "no cpu's counters could be read any more".to_owned(),
            ));
        }
        let mut events = Vec::new();
        for (cpu, after) in counts {
            // The read at window open is the baseline the first interval is
            // measured from: it describes the machine before the run.
            let Some(before) = self.previous.insert(cpu, after) else {
                continue;
            };
            let interval = match Interval::between(before, after) {
                Ok(interval) => interval,
                Err(undivided) => {
                    self.losses.failed_reads += 1;
                    self.losses.note_times(undivided.reason(cpu), 1);
                    continue;
                }
            };
            let mut attrs = Vec::with_capacity(9);
            // No hz when the cpu was never in C0: there is no clock it ran
            // at, and the emits count the event as unmeasured.
            if let Some(hz) = interval.hz {
                attrs.push(Attr::float("hz", hz));
            }
            attrs.extend([
                Attr::uint("cpu", u64::from(cpu)),
                Attr::float("avg_hz", interval.avg_hz),
                Attr::float("busy", interval.busy),
                Attr::float("idle", interval.idle),
                Attr::float("tsc_hz", interval.tsc_hz),
                Attr::uint("interval_ns", interval.interval_ns),
            ]);
            attrs.push(match interval.hz {
                Some(_) => Attr::text(
                    "basis",
                    "APERF/MPERF over the interval: hz is the frequency while in C0 \
                     (turbostat's Bzy_MHz), avg_hz the cycles over the whole interval \
                     (Avg_MHz), busy MPERF over the TSC (Busy%)",
                ),
                None => Attr::text(
                    "basis",
                    "APERF/MPERF over the interval: MPERF did not move, so the cpu never \
                     left idle; busy 0, idle 1, and no hz, since it ran at no clock",
                ),
            });
            attrs.push(Attr::text(
                "measures",
                if interval.hz.is_some() {
                    "effective"
                } else {
                    "idle"
                },
            ));
            events.push(Event {
                ts_ns: after.ts_ns,
                ts_err_ns: 0,
                kind: "power.freq".to_owned(),
                keys: vec![KeyValue::cpu(cpu)],
                attrs,
                collector_id: self.id.clone(),
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

    fn poll() -> Mode {
        Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        }
    }

    /// The arithmetic turbostat does, on numbers worked by hand: a 3.8 GHz
    /// reference clock, a second, a cpu in C0 for a quarter of it at 4.56 GHz.
    #[test]
    fn busy_share_and_both_frequencies_come_from_three_differences() {
        let before = Counts {
            aperf: 1_000,
            mperf: 2_000,
            tsc: 3_000,
            ts_ns: 1_000_000_000,
        };
        let after = Counts {
            aperf: 1_000 + 1_140_000_000,
            mperf: 2_000 + 950_000_000,
            tsc: 3_000 + 3_800_000_000,
            ts_ns: 2_000_000_000,
        };
        let interval = Interval::between(before, after).expect("a dividable interval");
        assert!((interval.tsc_hz - 3.8e9).abs() < 1.0);
        assert!((interval.busy - 0.25).abs() < 1e-12);
        assert!((interval.idle - 0.75).abs() < 1e-12);
        let hz = interval.hz.expect("a cpu in C0 has a frequency");
        assert!((hz - 4.56e9).abs() < 1.0, "{hz}");
        assert!((interval.avg_hz - 1.14e9).abs() < 1.0);
    }

    /// A cpu that never left idle is measured -- busy 0, idle 1 -- and ran
    /// at no frequency; a counter that went backwards or no time passing
    /// measured no interval.
    #[test]
    fn an_idle_cpu_is_a_measurement_and_a_backwards_counter_is_a_fault() {
        let at = |aperf, mperf, tsc, ts_ns| Counts {
            aperf,
            mperf,
            tsc,
            ts_ns,
        };
        let idle = Interval::between(at(5, 5, 5, 1), at(5, 5, 100, 2)).expect("idle is measured");
        assert_eq!(idle.hz, None);
        assert_eq!(idle.busy, 0.0);
        assert_eq!(idle.idle, 1.0);
        assert_eq!(
            Interval::between(at(5, 5, 5, 1), at(4, 6, 100, 2)),
            Err(Undivided::Backwards)
        );
        assert_eq!(
            Interval::between(at(5, 5, 5, 1), at(9, 9, 9, 1)),
            Err(Undivided::NoTime)
        );
    }

    #[test]
    fn a_process_target_and_a_counted_window_are_refused() {
        let prepared = Box::new(Prepare::new(poll()).expect("poll is servable"));
        let refused = prepared
            .attach(&Target::Pid(vec![1]))
            .err()
            .expect("a pid has no frequency of its own");
        assert!(refused.contains("--cpu"), "{refused}");
        assert!(Prepare::new(Mode::Count).is_err());
    }

    /// On this machine, against something these counters did not compute: a
    /// busy frequency inside the range cpufreq declares, and a busy share that
    /// is a share. Getting `hz` wrong by the TSC's scale fails by orders of
    /// magnitude.
    #[test]
    fn this_machines_cpus_read_inside_the_range_cpufreq_declares() {
        let prepared = Box::new(Prepare::new(poll()).unwrap());
        let Ok(mut collector) = prepared.attach(&Target::System) else {
            return;
        };
        if let Err(reason) = collector.start() {
            eprintln!("skipped: {reason}");
            return;
        }
        assert!(collector.sample(0).expect("the baseline").is_empty());
        std::thread::sleep(std::time::Duration::from_millis(200));
        let events = collector.sample(1).expect("a read");
        assert!(!events.is_empty(), "no cpu woke in 200 ms");
        for event in &events {
            let cpu = event.attr("cpu").unwrap().as_f64().unwrap() as u32;
            let hz = event.attr("hz").unwrap().as_f64().unwrap();
            let busy = event.attr("busy").unwrap().as_f64().unwrap();
            assert!((0.0..=1.0).contains(&busy), "cpu{cpu} busy {busy}");
            let directory = std::path::PathBuf::from(format!("{CPU_ROOT}/cpu{cpu}/cpufreq"));
            let (Ok(min), Ok(max)) = (
                sysfs::number(&directory.join("cpuinfo_min_freq")),
                sysfs::number(&directory.join("cpuinfo_max_freq")),
            ) else {
                continue;
            };
            assert!(
                hz >= min as f64 * 1e3 * 0.9 && hz <= max as f64 * 1e3 * 1.15,
                "cpu{cpu} ran at {hz} Hz, outside the declared {min}..{max} kHz"
            );
        }
    }
}
