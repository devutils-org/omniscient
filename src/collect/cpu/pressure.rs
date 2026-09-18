//! `cpu.pressure` over `/proc/pressure/cpu`.
//!
//! PSI answers a question utilisation cannot: a machine at 100 % busy may be
//! perfectly sized, and a machine at 60 % busy may have half its tasks waiting
//! for a cpu. The `some` line counts the time at least one runnable task was
//! waiting, in microseconds since boot, so the share of a window spent stalled
//! is a difference of two readings over the time between them.
//!
//! The file also carries the kernel's own ten, sixty and three-hundred second
//! averages. Those are percentages in the file and ratios here: one event, one
//! unit, or the first reader to average two of these columns together gets a
//! number that is off by a hundred.

use std::time::Duration as StdDuration;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event};
use crate::request::{Mode, Target};

const PATH: &str = "/proc/pressure/cpu";

/// One `some` or `full` line.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Line {
    avg10: f64,
    avg60: f64,
    avg300: f64,
    total_us: u64,
}

impl Line {
    fn parse(line: &str) -> Option<(String, Line)> {
        let mut fields = line.split_whitespace();
        let kind = fields.next()?.to_owned();
        let mut parsed = Line::default();
        for field in fields {
            let (name, value) = field.split_once('=')?;
            match name {
                "avg10" => parsed.avg10 = value.parse().ok()?,
                "avg60" => parsed.avg60 = value.parse().ok()?,
                "avg300" => parsed.avg300 = value.parse().ok()?,
                "total" => parsed.total_us = value.parse().ok()?,
                // An unknown column is not a reason to discard the line, but it
                // is also not something to guess the meaning of.
                _ => {}
            }
        }
        Some((kind, parsed))
    }
}

/// Holds the mode until the target exists.
pub struct Prepare(pub Mode);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match target {
            Target::System => Ok(Box::new(PressureCollector::new(self.0))),
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
    id: CollectorId,
    mode: Mode,
    /// The previous reading and when it was taken, which is what turns a
    /// microsecond counter into a share of a window.
    previous: Option<(u64, u64)>,
}

impl PressureCollector {
    pub fn new(mode: Mode) -> Self {
        Self {
            id: CollectorId::new("cpu.pressure/fs:proc"),
            mode,
            previous: None,
        }
    }

    fn read(&self) -> Result<(Line, Option<Line>), SampleError> {
        let text = std::fs::read_to_string(PATH)
            .map_err(|error| SampleError::Transient(format!("{PATH}: {error}")))?;
        let mut some = None;
        let mut full = None;
        for line in text.lines() {
            match Line::parse(line) {
                Some((kind, parsed)) if kind == "some" => some = Some(parsed),
                Some((kind, parsed)) if kind == "full" => full = Some(parsed),
                _ => {}
            }
        }
        let some =
            some.ok_or_else(|| SampleError::Transient(format!("no `some` line in {PATH}")))?;
        Ok((some, full))
    }
}

impl Collector for PressureCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "some"
    }

    fn start(&mut self) -> Result<(), String> {
        if self.mode != Mode::Snapshot {
            let (some, _) = self.read().map_err(|error| format!("{error:?}"))?;
            self.previous = Some((monotonic_ns(), some.total_us));
        }
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let (some, full) = self.read()?;
        Ok(self.event_from(some, full, monotonic_ns(), seq))
    }

    fn tick(&self) -> Option<StdDuration> {
        None
    }
}

impl PressureCollector {
    /// Turns a reading into an event, against whatever was read before it.
    ///
    /// Separate from `sample` so the share can be checked against a stall time
    /// and a window chosen for the purpose, instead of against whatever the
    /// machine was doing while the test ran.
    fn event_from(&mut self, some: Line, full: Option<Line>, ts_ns: u64, seq: u64) -> Vec<Event> {
        // A single read cannot be a share of anything: the counter is
        // cumulative. What it does carry is the kernel's own ten-second
        // average, which is a share, and saying which of the two is being
        // reported is the whole point.
        let (window, share) = match self.previous.replace((ts_ns, some.total_us)) {
            Some((then_ns, then_us)) => {
                let elapsed_us = ts_ns.saturating_sub(then_ns) / 1_000;
                if elapsed_us == 0 {
                    // Two reads inside one microsecond. There is no window to
                    // take a share of, and zero would read as a machine where
                    // nothing waited.
                    return Vec::new();
                }
                let stalled_us = some.total_us.saturating_sub(then_us);
                (
                    "since the previous read",
                    stalled_us as f64 / elapsed_us as f64,
                )
            }
            None => ("the kernel's 10-second average", some.avg10 / 100.0),
        };

        let mut attrs = vec![
            Attr::float("some", share),
            Attr::float("avg10", some.avg10 / 100.0),
            Attr::float("avg60", some.avg60 / 100.0),
            Attr::float("avg300", some.avg300 / 100.0),
            Attr::uint("total_us", some.total_us),
            Attr::text("window", window),
        ];
        if let Some(full) = full {
            // `full` means every task was stalled at once. The kernel does not
            // define it for cpu at machine scope, and reports it inside a
            // cgroup; it is carried when it is there and never invented.
            attrs.push(Attr::float("full_avg10", full.avg10 / 100.0));
            attrs.push(Attr::uint("full_total_us", full.total_us));
        }

        vec![Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "cpu.pressure".to_owned(),
            keys: Vec::new(),
            attrs,
            collector_id: self.id.clone(),
            seq,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pressure_line_parses_into_averages_and_a_total() {
        let (kind, line) = Line::parse("some avg10=0.00 avg60=0.02 avg300=1.50 total=8942667")
            .expect("a psi line");
        assert_eq!(kind, "some");
        assert_eq!(line.avg300, 1.5);
        assert_eq!(line.total_us, 8_942_667);
    }

    fn line(avg10: f64, total_us: u64) -> Line {
        Line {
            avg10,
            avg60: 0.0,
            avg300: 0.0,
            total_us,
        }
    }

    fn polling() -> PressureCollector {
        PressureCollector::new(Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        })
    }

    #[test]
    fn a_malformed_line_is_refused_rather_than_guessed_at() {
        // A column without a value is a file this reader does not understand,
        // and a zero would be indistinguishable from an idle machine.
        assert!(Line::parse("some avg10 0.00 total=5").is_none());
        // A column it has no meaning for is skipped, not fatal: the kernel is
        // free to add one.
        let (_, parsed) = Line::parse("some avg10=1.00 avg600=2.00 total=5").expect("a psi line");
        assert_eq!((parsed.avg10, parsed.total_us), (1.0, 5));
    }

    #[test]
    fn the_share_is_the_stall_time_over_the_window_it_was_measured_in() {
        let mut collector = polling();
        collector.previous = Some((0, 1_000_000));
        // Fifty milliseconds of stall in a hundred milliseconds of window.
        let events = collector.event_from(line(0.0, 1_050_000), None, 100_000_000, 0);

        let event = &events[0];
        assert_eq!(event.attr("some").unwrap().as_f64().unwrap(), 0.5);
        assert_eq!(
            event.attr("window").unwrap().to_string(),
            "since the previous read"
        );
        assert_eq!(
            event.attr("total_us").unwrap().as_f64().unwrap(),
            1_050_000.0
        );
    }

    #[test]
    fn two_reads_in_the_same_instant_report_nothing() {
        let mut collector = polling();
        collector.previous = Some((100_000_000, 1_000_000));
        assert!(collector
            .event_from(line(0.0, 1_000_000), None, 100_000_500, 0)
            .is_empty());
    }

    #[test]
    fn the_kernels_percentages_are_published_as_ratios() {
        let mut collector = PressureCollector::new(Mode::Snapshot);
        let events = collector.event_from(
            Line {
                avg10: 50.0,
                avg60: 25.0,
                avg300: 12.5,
                total_us: 7,
            },
            None,
            1,
            0,
        );

        let event = &events[0];
        // The file writes percentages. One event, one unit, or the first reader
        // to put two of these columns in the same average is out by a hundred.
        assert_eq!(event.attr("avg10").unwrap().as_f64().unwrap(), 0.5);
        assert_eq!(event.attr("avg60").unwrap().as_f64().unwrap(), 0.25);
        assert_eq!(event.attr("avg300").unwrap().as_f64().unwrap(), 0.125);
        assert_eq!(event.attr("some").unwrap().as_f64().unwrap(), 0.5);
        assert_eq!(
            event.attr("window").unwrap().to_string(),
            "the kernel's 10-second average"
        );
    }

    #[test]
    fn a_full_line_is_carried_when_the_kernel_writes_one_and_never_invented() {
        let mut collector = PressureCollector::new(Mode::Snapshot);
        let without = collector.event_from(line(0.0, 0), None, 1, 0);
        assert!(without[0].attr("full_avg10").is_none());

        let mut collector = PressureCollector::new(Mode::Snapshot);
        let with = collector.event_from(line(0.0, 0), Some(line(10.0, 42)), 1, 0);
        assert_eq!(with[0].attr("full_avg10").unwrap().as_f64().unwrap(), 0.1);
        assert_eq!(
            with[0].attr("full_total_us").unwrap().as_f64().unwrap(),
            42.0
        );
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
        let mut collector = PressureCollector::new(Mode::Poll {
            interval: crate::request::Mode::DEFAULT_POLL_INTERVAL,
        });
        if collector.start().is_err() {
            eprintln!("skipped: this kernel does not report PSI at {PATH}");
            return;
        }

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
        let events = collector.sample(0).expect("a second read");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for thread in threads {
            thread.join().expect("a spinner finished");
        }

        let some = events[0].attr("some").unwrap().as_f64().unwrap();
        assert!(
            some > 0.0,
            "twice as many spinning threads as cpus produced no stall time at all"
        );
        assert!(
            some <= 1.05,
            "a share of a window cannot exceed it, yet {some} was reported"
        );
    }

    #[test]
    fn a_single_read_reports_the_kernels_own_average_and_says_so() {
        let mut collector = PressureCollector::new(Mode::Snapshot);
        let Ok(events) = collector.sample(0) else {
            eprintln!("skipped: this kernel does not report PSI at {PATH}");
            return;
        };
        assert_eq!(
            events[0].attr("window").unwrap().to_string(),
            "the kernel's 10-second average"
        );
        let some = events[0].attr("some").unwrap().as_f64().unwrap();
        let avg10 = events[0].attr("avg10").unwrap().as_f64().unwrap();
        assert_eq!(some, avg10);
    }
}
