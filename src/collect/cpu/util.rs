//! `cpu.util` over `/proc/stat`.
//!
//! The file counts ticks spent in each state since boot, per cpu and for the
//! machine. A utilisation is a difference of two such readings divided by the
//! ticks that elapsed between them, which is why this collector keeps the
//! previous reading and why the first one is taken when the window opens rather
//! than on the first tick: otherwise the first number would describe the
//! machine's whole uptime and would be plotted next to numbers that describe a
//! second.
//!
//! `busy` here excludes `iowait`. A cpu waiting for a disk is not doing work,
//! and reporting it as busy is how a machine that is stalled on IO comes to
//! look saturated. The state is still reported on its own, next to the others.

use std::time::Duration as StdDuration;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event};
use crate::request::{Mode, Target};

const PATH: &str = "/proc/stat";

/// The states `/proc/stat` counts, in the order the file writes them.
///
/// `guest` and `guest_nice` are deliberately absent: the kernel already counts
/// them inside `user` and `nice`, and adding them would inflate the total and
/// deflate every share computed from it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Times {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
    steal: u64,
}

impl Times {
    fn total(&self) -> u64 {
        self.user
            + self.nice
            + self.system
            + self.idle
            + self.iowait
            + self.irq
            + self.softirq
            + self.steal
    }

    /// Parses one `cpu...` line, or nothing when the line is something else.
    fn parse(line: &str) -> Option<(String, Times)> {
        let mut fields = line.split_whitespace();
        let label = fields.next()?;
        if !label.starts_with("cpu") {
            return None;
        }
        let mut values = [0u64; 8];
        for slot in values.iter_mut() {
            // A short line is not a parse failure: older kernels stop at
            // `steal`, and the missing states are zero, not unknown.
            match fields.next() {
                Some(field) => *slot = field.parse().ok()?,
                None => break,
            }
        }
        Some((
            label.to_owned(),
            Times {
                user: values[0],
                nice: values[1],
                system: values[2],
                idle: values[3],
                iowait: values[4],
                irq: values[5],
                softirq: values[6],
                steal: values[7],
            },
        ))
    }

    fn since(&self, previous: &Times) -> Times {
        // A counter that went backwards means the file was re-read across a cpu
        // going offline and coming back. Saturating rather than wrapping keeps
        // the share in range; the total then lands at zero and the read is
        // reported as producing nothing.
        Times {
            user: self.user.saturating_sub(previous.user),
            nice: self.nice.saturating_sub(previous.nice),
            system: self.system.saturating_sub(previous.system),
            idle: self.idle.saturating_sub(previous.idle),
            iowait: self.iowait.saturating_sub(previous.iowait),
            irq: self.irq.saturating_sub(previous.irq),
            softirq: self.softirq.saturating_sub(previous.softirq),
            steal: self.steal.saturating_sub(previous.steal),
        }
    }
}

/// Holds the mode until the target exists.
pub struct Prepare(pub Mode);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match target {
            Target::System => Ok(Box::new(UtilCollector::new(self.0))),
            // Every cpu's line is in the same file, but the events would all
            // land in one series: the event model has no `cpu` correlation key
            // to separate them by, and a mean across cpus is a number that
            // describes no cpu.
            Target::Cpu(_) => Err(
                "per-cpu utilisation needs a `cpu` correlation key, which the event model \
                 does not have yet; `--system` reports the machine"
                    .to_owned(),
            ),
            other => Err(format!(
                "`/proc/stat` counts the machine, and a {} target is a different scope: \
                 the cgroup backend is the one that answers it, and it is not built yet",
                other.kind()
            )),
        }
    }
}

/// Reads the machine's cpu states, and reports what changed between reads.
pub struct UtilCollector {
    id: CollectorId,
    mode: Mode,
    previous: Option<Times>,
}

impl UtilCollector {
    pub fn new(mode: Mode) -> Self {
        Self {
            id: CollectorId::new("cpu.util/fs:proc"),
            mode,
            previous: None,
        }
    }

    fn read(&self) -> Result<Times, SampleError> {
        let text = std::fs::read_to_string(PATH)
            .map_err(|error| SampleError::Transient(format!("{PATH}: {error}")))?;
        text.lines()
            .find_map(|line| match Times::parse(line) {
                Some((label, times)) if label == "cpu" => Some(times),
                _ => None,
            })
            .ok_or_else(|| SampleError::Transient(format!("no aggregate cpu line in {PATH}")))
    }
}

impl Collector for UtilCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "busy"
    }

    fn start(&mut self) -> Result<(), String> {
        // The baseline is taken as the window opens, so the first poll already
        // describes an interval and not the machine's uptime.
        if self.mode != Mode::Snapshot {
            self.previous = Some(self.read().map_err(|error| format!("{error:?}"))?);
        }
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let now = self.read()?;
        Ok(self.event_from(now, monotonic_ns(), seq))
    }

    fn tick(&self) -> Option<StdDuration> {
        None
    }
}

impl UtilCollector {
    /// Turns a reading into an event, against whatever was read before it.
    ///
    /// Separate from `sample` so that the arithmetic can be checked against
    /// readings chosen for the cases that matter -- a cpu that went offline, a
    /// window where nothing moved -- rather than against whatever the machine
    /// happened to be doing.
    fn event_from(&mut self, now: Times, ts_ns: u64, seq: u64) -> Vec<Event> {
        // A snapshot of this file is a statement about the whole uptime, and
        // saying which window a share covers is the difference between a
        // reading and a guess.
        let (window, delta) = match self.previous.replace(now) {
            Some(previous) => ("since the previous read", now.since(&previous)),
            None => ("since boot", now),
        };

        let total = delta.total();
        if total == 0 {
            // Two reads inside one tick of the kernel's own clock, or a cpu
            // that went offline and took its counters with it. Nothing moved,
            // so there is no share to report -- and zero would read as a
            // perfectly idle machine.
            return Vec::new();
        }
        let share = |ticks: u64| ticks as f64 / total as f64;

        let mut attrs = vec![
            Attr::float("busy", 1.0 - share(delta.idle) - share(delta.iowait)),
            Attr::float("user", share(delta.user)),
            Attr::float("nice", share(delta.nice)),
            Attr::float("system", share(delta.system)),
            Attr::float("idle", share(delta.idle)),
            Attr::float("iowait", share(delta.iowait)),
            Attr::float("irq", share(delta.irq)),
            Attr::float("softirq", share(delta.softirq)),
            Attr::float("steal", share(delta.steal)),
            Attr::text("window", window),
        ];
        // Steal time is the machine being taken away from underneath the
        // measurement. It is not a state of this workload, and a run with any
        // of it is not comparable to one without.
        if delta.steal > 0 {
            attrs.push(Attr::uint("steal_ticks", delta.steal));
        }

        vec![Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "cpu.util".to_owned(),
            keys: Vec::new(),
            attrs,
            collector_id: self.id.clone(),
            seq,
        }]
    }
}

/// The states a `/proc/stat` line carries, for tests in this crate.
#[cfg(test)]
fn parse_line(line: &str) -> Option<(String, Times)> {
    Times::parse(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_parses_into_the_states_the_kernel_writes() {
        let (label, times) =
            parse_line("cpu  296316 815 104014 28906570 2519292 40473 7897 0 0 0").unwrap();
        assert_eq!(label, "cpu");
        assert_eq!(times.user, 296_316);
        assert_eq!(times.steal, 0);
        // guest and guest_nice are already inside user and nice; counting them
        // again would inflate every share.
        assert_eq!(
            times.total(),
            296_316 + 815 + 104_014 + 28_906_570 + 2_519_292 + 40_473 + 7_897
        );
    }

    #[test]
    fn a_short_line_from_an_older_kernel_leaves_the_missing_states_at_zero() {
        let (_, times) = parse_line("cpu 1 2 3 4").unwrap();
        assert_eq!((times.idle, times.iowait, times.steal), (4, 0, 0));
    }

    #[test]
    fn a_non_cpu_line_is_not_a_cpu_line() {
        assert!(parse_line("intr 12345 0 0").is_none());
        assert!(parse_line("ctxt 999").is_none());
    }

    /// A reading, in the order `/proc/stat` writes the states.
    fn times(fields: [u64; 8]) -> Times {
        Times {
            user: fields[0],
            nice: fields[1],
            system: fields[2],
            idle: fields[3],
            iowait: fields[4],
            irq: fields[5],
            softirq: fields[6],
            steal: fields[7],
        }
    }

    fn polling() -> UtilCollector {
        UtilCollector::new(Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        })
    }

    #[test]
    fn the_shares_are_of_the_interval_and_not_of_the_uptime() {
        let mut collector = polling();
        // A machine that has been up a long while, mostly idle.
        collector.previous = Some(times([1_000, 0, 0, 9_000, 0, 0, 0, 0]));
        // And a busy second: fifty ticks of work, fifty of idle.
        let events = collector.event_from(times([1_050, 0, 0, 9_050, 0, 0, 0, 0]), 1, 0);

        let event = &events[0];
        assert_eq!(event.attr("busy").unwrap().as_f64().unwrap(), 0.5);
        assert_eq!(event.attr("user").unwrap().as_f64().unwrap(), 0.5);
        assert_eq!(
            event.attr("window").unwrap().to_string(),
            "since the previous read",
            "an interval was reported as if it were the uptime"
        );
    }

    #[test]
    fn a_cpu_waiting_for_a_disk_is_not_reported_as_busy() {
        let mut collector = polling();
        collector.previous = Some(Times::default());
        // Half the window working, half of it waiting for IO and none of it
        // idle. A machine stalled on IO must not read as saturated.
        let events = collector.event_from(times([50, 0, 0, 0, 50, 0, 0, 0]), 1, 0);

        let event = &events[0];
        assert_eq!(event.attr("busy").unwrap().as_f64().unwrap(), 0.5);
        assert_eq!(event.attr("iowait").unwrap().as_f64().unwrap(), 0.5);
        assert_eq!(event.attr("idle").unwrap().as_f64().unwrap(), 0.0);
    }

    #[test]
    fn a_window_in_which_nothing_moved_reports_nothing() {
        let mut collector = polling();
        let reading = times([1_000, 0, 0, 9_000, 0, 0, 0, 0]);
        collector.previous = Some(reading);
        // Two reads inside one tick of the kernel's clock. Zero would read as a
        // perfectly idle machine, which is a claim nobody measured.
        assert!(collector.event_from(reading, 1, 0).is_empty());
    }

    #[test]
    fn a_counter_that_went_backwards_does_not_become_an_enormous_share() {
        let mut collector = polling();
        collector.previous = Some(times([1_000, 0, 0, 9_000, 0, 0, 0, 0]));
        // A cpu went offline and came back with its counters reset. Wrapping
        // would turn this into billions of ticks of work.
        let events = collector.event_from(times([10, 0, 0, 90, 0, 0, 0, 0]), 1, 0);
        assert!(events.is_empty());
    }

    #[test]
    fn stolen_time_is_called_out_rather_than_left_in_a_column() {
        let mut collector = polling();
        collector.previous = Some(Times::default());
        let events = collector.event_from(times([50, 0, 0, 40, 0, 0, 0, 10]), 1, 0);

        let event = &events[0];
        // The machine was taken away for a tenth of the window. That is not a
        // state of this workload, and a run with any of it is not comparable to
        // one without.
        assert_eq!(event.attr("steal").unwrap().as_f64().unwrap(), 0.1);
        assert_eq!(event.attr("steal_ticks").unwrap().as_f64().unwrap(), 10.0);
    }

    #[test]
    fn a_machine_with_no_steal_time_carries_no_steal_column() {
        let mut collector = polling();
        collector.previous = Some(Times::default());
        let events = collector.event_from(times([50, 0, 0, 50, 0, 0, 0, 0]), 1, 0);
        assert!(events[0].attr("steal_ticks").is_none());
    }

    #[test]
    fn a_cgroup_target_is_refused_by_the_backend_that_reads_the_machine() {
        let attached = Box::new(Prepare(Mode::Snapshot))
            .attach(&Target::Cgroup(std::path::PathBuf::from("/user.slice")));
        let refusal = match attached {
            Ok(_) => panic!("`/proc/stat` was accepted as a cgroup's file"),
            Err(refusal) => refusal,
        };
        assert!(refusal.contains("cgroup backend"), "{refusal}");
    }

    /// Ground truth: burn a known share of the machine and see it appear.
    ///
    /// The independent quantity is the work this test does -- half the cpus,
    /// spinning for a known time -- measured by a clock rather than by
    /// `/proc/stat`. Utilisation below that share means the collector is not
    /// reading what it claims.
    #[test]
    fn utilisation_reflects_a_known_amount_of_burned_cpu() {
        let cpus = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1);
        if cpus < 2 {
            eprintln!("skipped: one cpu, so there is no share to burn");
            return;
        }
        let burners = cpus / 2;

        let mut collector = UtilCollector::new(Mode::Poll {
            interval: crate::request::Mode::DEFAULT_POLL_INTERVAL,
        });
        collector.start().expect("/proc/stat is readable");

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let threads: Vec<_> = (0..burners)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut total = 0u64;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        total = total.wrapping_add(1);
                    }
                    std::hint::black_box(total);
                })
            })
            .collect();
        std::thread::sleep(StdDuration::from_millis(400));
        let events = collector.sample(0).expect("a second read");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for thread in threads {
            thread.join().expect("a burner finished");
        }

        let busy = events[0].attr("busy").unwrap().as_f64().unwrap();
        let expected = burners as f64 / cpus as f64;
        assert!(
            busy >= 0.8 * expected,
            "{burners} of {cpus} cpus were spinning, which is {expected:.2} of the machine, \
             yet only {busy:.2} was reported busy"
        );
        assert!(
            busy <= 1.0,
            "a machine cannot be more than fully busy, yet {busy:.2} was reported"
        );
        assert_eq!(
            events[0].attr("window").unwrap().to_string(),
            "since the previous read"
        );
    }

    #[test]
    fn the_shares_of_one_read_add_up_to_the_whole() {
        let mut collector = UtilCollector::new(Mode::Snapshot);
        let events = collector.sample(0).expect("/proc/stat is readable");
        let event = &events[0];
        assert_eq!(event.attr("window").unwrap().to_string(), "since boot");
        let sum: f64 = [
            "user", "nice", "system", "idle", "iowait", "irq", "softirq", "steal",
        ]
        .iter()
        .map(|state| event.attr(state).unwrap().as_f64().unwrap())
        .sum();
        assert!((sum - 1.0).abs() < 1e-9, "the states sum to {sum}");
    }
}
