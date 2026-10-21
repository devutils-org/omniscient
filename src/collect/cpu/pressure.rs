//! `cpu.pressure` over `/proc/pressure/cpu`.
//!
//! PSI answers a question utilisation cannot: a machine at 100 % busy may be
//! perfectly sized, and a machine at 60 % busy may have half its tasks waiting
//! for a cpu. The `some` line counts the time at least one runnable task was
//! waiting, in microseconds since boot, so the share of a window spent stalled
//! is a difference of two readings over the time between them.
//!
//! The file is read by the one PSI reader the families share
//! (`crate::collect::psi`), so this source names its attributes the way
//! `mem.pressure` and `io.pressure` do -- `stall_ratio`, `stalled_ns`,
//! `interval_ns`, `some_avg10`, and the `full_` half where the kernel writes
//! one -- rather than by names of its own (gap 3 of
//! `docs/fixes/w22-sysstat.md`).
//!
//! The first read of a poll is its baseline and produces no event. The runner
//! takes its first sample immediately after `start`, so a baseline taken in
//! `start` was a few microseconds to a few milliseconds old when the first
//! share was computed over it, and that share entered the summary's `min` and
//! `mean` beside the full windows (defect 2 of the same handoff).

use std::path::PathBuf;
use std::time::Duration as StdDuration;

use crate::clock::monotonic_ns;
use crate::collect::psi::{Reading, POLL_BASIS, SNAPSHOT_BASIS};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event};
use crate::request::{Mode, Target};

const PATH: &str = "/proc/pressure/cpu";

/// Holds the mode until the target exists.
pub struct Prepare(pub Mode);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match target {
            Target::System => Ok(Box::new(PressureCollector::new(
                self.0,
                PathBuf::from(PATH),
            ))),
            other => Err(format!(
                "`{PATH}` is the machine's pressure, and a {} target is a different scope: \
                 the cgroup backend reads `cpu.pressure` in the cgroup and is not built yet",
                other.kind()
            )),
        }
    }
}

/// Reads how much of a window was spent waiting for a cpu.
pub struct PressureCollector {
    mode: Mode,
    path: PathBuf,
    /// The previous reading, which is what turns a microsecond counter into a
    /// share of a window.
    previous: Option<Reading>,
    losses: Losses,
}

impl PressureCollector {
    pub fn new(mode: Mode, path: PathBuf) -> Self {
        Self {
            mode,
            path,
            previous: None,
            losses: Losses::default(),
        }
    }

    fn read(&self) -> Result<Reading, SampleError> {
        let text = std::fs::read_to_string(&self.path)
            .map_err(|error| SampleError::Transient(format!("{}: {error}", self.path.display())))?;
        Reading::parse(&text, monotonic_ns()).ok_or_else(|| {
            SampleError::Transient(format!(
                "{} did not hold a `some ... total=N` line",
                self.path.display()
            ))
        })
    }

    /// The share of one tick that was spent stalled, or a counted loss when the
    /// two reads measure no window.
    fn stall_event(&mut self, before: &Reading, after: &Reading, seq: u64) -> Option<Event> {
        let mut attrs = match after.stall_attrs(before) {
            Ok(attrs) => attrs,
            Err(unmeasured) => {
                self.losses.undecoded += 1;
                self.losses.note_times(unmeasured.explain(PATH), 1);
                return None;
            }
        };
        attrs.push(Attr::text("basis", POLL_BASIS));
        Some(self.event(after.ts_ns, attrs, seq))
    }

    /// What one read can say: the kernel's own averages, which describe a
    /// window this run did not choose.
    fn snapshot_event(&self, reading: &Reading, seq: u64) -> Event {
        let mut attrs = reading.snapshot_attrs();
        attrs.push(Attr::text("basis", SNAPSHOT_BASIS));
        self.event(reading.ts_ns, attrs, seq)
    }

    fn event(&self, ts_ns: u64, attrs: Vec<Attr>, seq: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "cpu.pressure".to_owned(),
            keys: Vec::new(),
            attrs,
            collector_id: CollectorId::unstamped(),
            seq,
        }
    }
}

impl Collector for PressureCollector {
    /// A poll measures its own window; a snapshot can only repeat the kernel's.
    /// They are different numbers, so they are not given the same name -- the
    /// names `mem.pressure` and `io.pressure` give them.
    fn value_attr(&self) -> &'static str {
        match self.mode {
            Mode::Snapshot => "some_avg10",
            _ => "stall_ratio",
        }
    }

    /// The file is read once to refuse a kernel without PSI before anything is
    /// launched, and the reading is thrown away: the poll's baseline is its
    /// first sample, taken by the runner as the window opens.
    fn start(&mut self) -> Result<(), String> {
        self.read().map(|_| ()).map_err(|error| match error {
            SampleError::Transient(reason) | SampleError::TargetGone(reason) => reason,
        })
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let reading = self.read()?;
        if self.mode == Mode::Snapshot {
            return Ok(vec![self.snapshot_event(&reading, seq)]);
        }
        let before = self.previous.replace(reading.clone());
        let event = before.and_then(|before| self.stall_event(&before, &reading, seq));
        Ok(event.into_iter().collect())
    }

    fn tick(&self) -> Option<StdDuration> {
        None
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

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "omn-cpu-pressure-{}-{}-{name}",
            std::process::id(),
            monotonic_ns()
        ))
    }

    fn write_totals(path: &std::path::Path, some_us: u64, full_us: Option<u64>) {
        let mut text = format!("some avg10=50.00 avg60=25.00 avg300=12.50 total={some_us}\n");
        if let Some(full_us) = full_us {
            text.push_str(&format!(
                "full avg10=10.00 avg60=5.00 avg300=1.00 total={full_us}\n"
            ));
        }
        std::fs::write(path, text).expect("writing the fake pressure file");
    }

    /// Defect 2: the first read of a poll is a baseline, and a baseline
    /// measures nothing. Before, `start` read the file and the runner's first
    /// sample a few microseconds later computed a share over that.
    #[test]
    fn the_first_read_of_a_poll_is_a_baseline_and_emits_nothing() {
        let path = scratch("baseline");
        write_totals(&path, 1_000_000, None);
        let mut collector = PressureCollector::new(poll(), path.clone());
        collector.start().expect("the fake file reads");
        assert!(collector.sample(0).expect("the first read").is_empty());
        write_totals(&path, 1_050_000, None);
        std::thread::sleep(StdDuration::from_millis(20));
        let events = collector.sample(1).expect("a second read");
        assert_eq!(events.len(), 1);
        std::fs::remove_file(&path).ok();
    }

    /// Gap 3: the attributes are its siblings', and the share is the counter's
    /// increase over the window.
    #[test]
    fn the_share_is_named_as_its_siblings_and_is_the_counters_increase() {
        let path = scratch("names");
        write_totals(&path, 1_000_000, Some(400_000));
        let mut collector = PressureCollector::new(poll(), path.clone());
        collector.start().expect("the fake file reads");
        collector.sample(0).expect("the baseline");
        write_totals(&path, 1_250_000, Some(500_000));
        std::thread::sleep(StdDuration::from_millis(50));
        let events = collector.sample(1).expect("a second read");

        let event = &events[0];
        assert_eq!(collector.value_attr(), "stall_ratio");
        assert_eq!(event.attr("stalled_ns").unwrap().as_f64(), Some(250e6));
        assert_eq!(event.attr("full_stalled_ns").unwrap().as_f64(), Some(100e6));
        let interval = event.attr("interval_ns").unwrap().as_f64().unwrap();
        let ratio = event.attr("stall_ratio").unwrap().as_f64().unwrap();
        assert!((ratio - 250e6 / interval).abs() < 1e-9);
        let full = event.attr("full_stall_ratio").unwrap().as_f64().unwrap();
        assert!((full - 100e6 / interval).abs() < 1e-9);
        assert_eq!(event.attr("some_avg10").unwrap().as_f64(), Some(0.5));
        // The old names are gone, so no reader can mix the two.
        for old in ["some", "avg10", "total_us", "full_total_us", "window"] {
            assert!(event.attr(old).is_none(), "`{old}` is still carried");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_snapshot_reports_the_kernels_averages_as_ratios_and_says_so() {
        let path = scratch("snapshot");
        write_totals(&path, 7, None);
        let mut collector = PressureCollector::new(Mode::Snapshot, path.clone());
        collector.start().expect("the fake file reads");
        let events = collector.sample(0).expect("one read");

        assert_eq!(collector.value_attr(), "some_avg10");
        let event = &events[0];
        assert_eq!(event.attr("some_avg10").unwrap().as_f64(), Some(0.5));
        assert_eq!(event.attr("some_avg60").unwrap().as_f64(), Some(0.25));
        assert_eq!(event.attr("some_avg300").unwrap().as_f64(), Some(0.125));
        assert_eq!(event.attr("some_total_ns").unwrap().as_f64(), Some(7000.0));
        assert!(event.attr("stall_ratio").is_none());
        assert!(
            event.attr("full_avg10").is_none(),
            "a full line was invented"
        );
        assert!(event
            .attr("basis")
            .unwrap()
            .to_string()
            .contains("observes no window"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_counter_that_goes_backwards_is_a_counted_loss() {
        let path = scratch("backwards");
        write_totals(&path, 900, None);
        let mut collector = PressureCollector::new(poll(), path.clone());
        collector.start().expect("the fake file reads");
        collector.sample(0).expect("the baseline read");
        write_totals(&path, 100, None);
        assert!(collector.sample(1).expect("a read").is_empty());
        assert_eq!(collector.losses().undecoded, 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_file_without_psi_is_refused_at_start() {
        let path = scratch("absent");
        let mut collector = PressureCollector::new(poll(), path);
        assert!(collector.start().is_err());
    }

    #[test]
    fn a_cgroup_target_is_refused_by_the_backend_that_reads_the_machine() {
        let attached = Box::new(Prepare(Mode::Snapshot))
            .attach(&Target::Cgroup(std::path::PathBuf::from("/user.slice")));
        let refusal = match attached {
            Ok(_) => panic!("the machine's PSI file was accepted as a cgroup's"),
            Err(refusal) => refusal,
        };
        assert!(refusal.contains("cgroup"), "{refusal}");
        assert!(refusal.contains("not built yet"), "{refusal}");
    }

    /// Ground truth: stall time appears when, and only when, tasks are made to
    /// wait for a cpu. Twice as many spinning threads as there are cpus
    /// guarantees a run queue; the counter has to move.
    #[test]
    fn oversubscribing_the_machine_shows_up_as_stall_time() {
        let cpus = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1);
        let mut collector = PressureCollector::new(poll(), PathBuf::from(PATH));
        if collector.start().is_err() {
            eprintln!("skipped: this kernel does not report PSI at {PATH}");
            return;
        }
        collector.sample(0).expect("the baseline read");

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let threads: Vec<_> = (0..cpus * 2)
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
        std::thread::sleep(StdDuration::from_millis(300));
        let events = collector.sample(1).expect("a second read");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for thread in threads {
            thread.join().expect("a spinner finished");
        }

        let share = events[0].attr("stall_ratio").unwrap().as_f64().unwrap();
        assert!(
            share > 0.0,
            "twice as many spinning threads as cpus produced no stall time at all"
        );
        assert!(
            share <= 1.05,
            "a share of a window cannot exceed it, yet {share} was reported"
        );
    }
}
