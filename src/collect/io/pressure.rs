//! `io.pressure` over `/proc/pressure/io`.
//!
//! Pressure stall information answers a question the throughput counters
//! cannot: not how much I/O happened, but how much time was *lost* waiting for
//! it. A device at half its throughput with nothing waiting is idle capacity; a
//! device at half its throughput with every thread blocked is the bottleneck,
//! and `/proc/diskstats` reports the same number in both cases.
//!
//! The file gives two things, and they are not the same measurement:
//!
//! - `total`, a counter of microseconds of stall since boot. Its increase over
//!   a tick, divided by the tick, is the fraction of that window that was lost.
//!   That is what a poll reports, and it is a fact about the run.
//! - `avg10`, `avg60`, `avg300`, the kernel's own exponentially decayed
//!   averages. They describe the last ten seconds to five minutes, which
//!   overlaps the run without being about it.
//!
//! A poll therefore reports the ratio computed from the counter, and a snapshot
//! -- which observes no window at all -- reports `avg10` and says so. Both are
//! ratios in `[0, 1]`: the kernel writes percentages and the file's own unit is
//! per cent, so the division by a hundred is the only conversion here.
//!
//! `some` is time when at least one task was stalled on I/O, `full` time when
//! every runnable task was. `some` is the headline because it is the one that
//! means work was delayed; `full` is carried beside it because the gap between
//! them says whether the machine had anything else to get on with.

use std::path::PathBuf;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, AttrValue, CollectorId, Event};
use crate::request::{Mode, Target};

/// Holds the mode until the run starts.
pub struct Prepare {
    mode: Mode,
}

impl Prepare {
    pub fn new(mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } | Mode::Snapshot => Ok(Self { mode }),
            other => Err(format!(
                "pressure is a counter that is read, not a stream of events: `{}` cannot be \
                 served from /proc/pressure/io; poll it, or take one snapshot",
                other.kind()
            )),
        }
    }
}

impl Prepared for Prepare {
    /// `/proc/pressure/io` is one file for the machine. The same measurement
    /// per cgroup is a different file and a different backend, so a target this
    /// one cannot narrow to is refused rather than answered machine-wide.
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match target {
            Target::System => Ok(Box::new(Pressure::new(
                self.mode,
                PathBuf::from("/proc/pressure/io"),
            ))),
            other => Err(format!(
                "/proc/pressure/io is the machine's stall counter; a {} target needs                  `io.pressure` under that cgroup, which is the `fs:cgroup` backend",
                other.kind()
            )),
        }
    }
}

pub struct Pressure {
    id: CollectorId,
    mode: Mode,
    path: PathBuf,
    previous: Option<Reading>,
    losses: Losses,
}

impl Pressure {
    pub fn new(mode: Mode, path: PathBuf) -> Self {
        Self {
            id: CollectorId::new("io.pressure/fs:proc"),
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

    /// The share of one tick that was spent stalled.
    fn stall_event(&mut self, before: &Reading, after: &Reading, seq: u64) -> Option<Event> {
        let interval_ns = after.ts_ns.saturating_sub(before.ts_ns);
        if interval_ns == 0 {
            self.losses.undecoded += 1;
            self.losses.note(
                "two reads of /proc/pressure/io landed in the same nanosecond, so there is no \
                 window to divide by"
                    .to_owned(),
            );
            return None;
        }
        let Some(some_ns) = stalled_ns(after.some.total_us, before.some.total_us) else {
            self.losses.undecoded += 1;
            self.losses.note(
                "the stall counter in /proc/pressure/io went backwards, so this interval cannot \
                 be attributed"
                    .to_owned(),
            );
            return None;
        };

        let mut attrs = vec![
            Attr {
                name: "stall_ratio".to_owned(),
                value: AttrValue::Float(some_ns as f64 / interval_ns as f64),
            },
            Attr::uint("stalled_ns", some_ns),
            Attr::uint("interval_ns", interval_ns),
        ];
        if let Some(full_ns) = after
            .full
            .as_ref()
            .zip(before.full.as_ref())
            .and_then(|(after, before)| stalled_ns(after.total_us, before.total_us))
        {
            attrs.push(Attr {
                name: "full_stall_ratio".to_owned(),
                value: AttrValue::Float(full_ns as f64 / interval_ns as f64),
            });
            attrs.push(Attr::uint("full_stalled_ns", full_ns));
        }
        attrs.extend(after.average_attrs());
        attrs.push(Attr::text(
            "basis",
            "the stall counter's increase over this tick",
        ));

        Some(Event {
            ts_ns: after.ts_ns,
            ts_err_ns: 0,
            kind: "io.pressure".to_owned(),
            keys: Vec::new(),
            attrs,
            collector_id: self.id.clone(),
            seq,
        })
    }

    /// What can be said from a single read: the kernel's own averages, which
    /// describe a window this run did not choose.
    fn snapshot_event(&self, reading: &Reading, seq: u64) -> Event {
        let mut attrs = reading.average_attrs();
        attrs.push(Attr::uint("some_total_ns", reading.some.total_us * 1_000));
        if let Some(full) = &reading.full {
            attrs.push(Attr::uint("full_total_ns", full.total_us * 1_000));
        }
        attrs.push(Attr::text(
            "basis",
            "the kernel's decayed averages: a snapshot observes no window of its own",
        ));
        Event {
            ts_ns: reading.ts_ns,
            ts_err_ns: 0,
            kind: "io.pressure".to_owned(),
            keys: Vec::new(),
            attrs,
            collector_id: self.id.clone(),
            seq,
        }
    }
}

impl Collector for Pressure {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    /// A poll measures its own window; a snapshot can only repeat the kernel's.
    /// They are different numbers, so they are not given the same name.
    fn value_attr(&self) -> &'static str {
        match self.mode {
            Mode::Snapshot => "some_avg10",
            _ => "stall_ratio",
        }
    }

    /// Nothing is read here. The runner takes its first sample immediately
    /// after starting, so a baseline taken now would be a few microseconds old
    /// and the first ratio in the series would be noise divided by almost
    /// nothing. The first read is the baseline, and it produces no event.
    fn start(&mut self) -> Result<(), String> {
        Ok(())
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

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

/// Microseconds of stall turned into nanoseconds, or `None` when the counter
/// moved backwards.
fn stalled_ns(after_us: u64, before_us: u64) -> Option<u64> {
    after_us.checked_sub(before_us).map(|micros| micros * 1_000)
}

/// One read of the file.
#[derive(Debug, Clone, PartialEq)]
struct Reading {
    ts_ns: u64,
    some: Psi,
    /// Absent on kernels or cgroups that do not write a `full` line. Absent is
    /// not zero: zero would claim nothing was ever fully stalled.
    full: Option<Psi>,
}

/// One `some`/`full` line: three decayed averages and a stall counter.
#[derive(Debug, Clone, PartialEq)]
struct Psi {
    avg10: f64,
    avg60: f64,
    avg300: f64,
    total_us: u64,
}

impl Reading {
    fn parse(text: &str, ts_ns: u64) -> Option<Self> {
        let mut some = None;
        let mut full = None;
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let kind = fields.next()?;
            let mut line = Psi {
                avg10: 0.0,
                avg60: 0.0,
                avg300: 0.0,
                total_us: 0,
            };
            for field in fields {
                let (name, value) = field.split_once('=')?;
                match name {
                    "avg10" => line.avg10 = value.parse().ok()?,
                    "avg60" => line.avg60 = value.parse().ok()?,
                    "avg300" => line.avg300 = value.parse().ok()?,
                    "total" => line.total_us = value.parse().ok()?,
                    // A field this reader does not know is not a reason to
                    // discard a line that holds what it came for.
                    _ => {}
                }
            }
            match kind {
                "some" => some = Some(line),
                "full" => full = Some(line),
                _ => {}
            }
        }
        Some(Self {
            ts_ns,
            some: some?,
            full,
        })
    }

    /// The kernel's averages, as ratios rather than the per cent the file
    /// writes, so every ratio this collector emits is on one scale.
    fn average_attrs(&self) -> Vec<Attr> {
        let ratio = |name: &str, value: f64| Attr {
            name: name.to_owned(),
            value: AttrValue::Float(value / 100.0),
        };
        let mut attrs = vec![
            ratio("some_avg10", self.some.avg10),
            ratio("some_avg60", self.some.avg60),
            ratio("some_avg300", self.some.avg300),
        ];
        if let Some(full) = &self.full {
            attrs.push(ratio("full_avg10", full.avg10));
            attrs.push(ratio("full_avg60", full.avg60));
            attrs.push(ratio("full_avg300", full.avg300));
        }
        attrs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;

    /// The file as this machine writes it.
    const SYSTEM: &str = "\
some avg10=30.96 avg60=37.29 avg300=46.82 total=9276004371
full avg10=29.77 avg60=36.24 avg300=45.91 total=9194047541
";

    fn poll() -> Mode {
        Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "omn-pressure-{}-{}-{name}",
            std::process::id(),
            monotonic_ns()
        ));
        std::fs::write(&path, SYSTEM).expect("a scratch file");
        path
    }

    fn write_totals(path: &Path, some_us: u64, full_us: u64) {
        std::fs::write(
            path,
            format!(
                "some avg10=1.00 avg60=2.00 avg300=3.00 total={some_us}\n\
                 full avg10=0.50 avg60=1.00 avg300=1.50 total={full_us}\n"
            ),
        )
        .expect("writing the fake pressure file");
    }

    #[test]
    fn the_file_is_read_the_way_the_kernel_writes_it() {
        let reading = Reading::parse(SYSTEM, 42).expect("a `some` line");
        assert_eq!(reading.ts_ns, 42);
        assert_eq!(reading.some.total_us, 9_276_004_371);
        assert_eq!(reading.some.avg10, 30.96);
        assert_eq!(reading.full.as_ref().unwrap().avg300, 45.91);
    }

    /// A cgroup on an older kernel writes no `full` line. Reporting zero there
    /// would claim nothing was ever fully stalled, which is a measurement
    /// nobody took.
    #[test]
    fn a_missing_full_line_is_absent_rather_than_zero() {
        let reading = Reading::parse("some avg10=0.00 avg60=0.00 avg300=0.00 total=17\n", 0)
            .expect("a `some` line");
        assert!(reading.full.is_none());
        assert!(reading
            .average_attrs()
            .iter()
            .all(|attr| !attr.name.starts_with("full")));
    }

    /// The percentages the kernel writes come out as ratios, on one scale with
    /// the ratio computed from the counter.
    #[test]
    fn the_kernels_averages_are_reported_as_ratios() {
        let reading = Reading::parse(SYSTEM, 0).expect("a `some` line");
        let attrs = reading.average_attrs();
        let avg10 = attrs.iter().find(|attr| attr.name == "some_avg10").unwrap();
        assert_eq!(avg10.value.as_f64(), Some(0.3096));
    }

    /// The share of a window that was stalled is the counter's increase over
    /// that window, and nothing else.
    #[test]
    fn the_stalled_share_is_the_counters_increase_over_the_tick() {
        let path = scratch("stall");
        write_totals(&path, 1_000_000, 500_000);

        let mut collector = Pressure::new(poll(), path.clone());
        collector.start().expect("the fake file reads");
        assert!(
            collector.sample(0).expect("the first read").is_empty(),
            "the first read is a baseline, and a baseline measures nothing"
        );
        // A quarter of a second more stall, over a window this test bounds
        // below by sleeping for less than it.
        write_totals(&path, 1_250_000, 600_000);
        std::thread::sleep(std::time::Duration::from_millis(50));
        let events = collector.sample(1).expect("a second read");

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.attr("stalled_ns").unwrap().as_f64(), Some(250e6));
        assert_eq!(event.attr("full_stalled_ns").unwrap().as_f64(), Some(100e6));
        let interval = event.attr("interval_ns").unwrap().as_f64().unwrap();
        let ratio = event.attr("stall_ratio").unwrap().as_f64().unwrap();
        assert!((ratio - 250e6 / interval).abs() < 1e-9);
        assert_eq!(
            event.attr("basis").unwrap().to_string(),
            "the stall counter's increase over this tick"
        );
        std::fs::remove_file(&path).ok();
    }

    /// A snapshot observes no window, so it reports the kernel's own averages
    /// under their own name instead of a ratio it did not measure.
    #[test]
    fn a_snapshot_reports_the_kernels_averages_and_says_so() {
        let path = scratch("snapshot");
        let mut collector = Pressure::new(Mode::Snapshot, path.clone());
        collector.start().expect("nothing to do");
        let events = collector.sample(0).expect("one read");

        assert_eq!(collector.value_attr(), "some_avg10");
        assert_eq!(events[0].attr("some_avg10").unwrap().as_f64(), Some(0.3096));
        assert!(events[0].attr("stall_ratio").is_none());
        assert!(events[0]
            .attr("basis")
            .unwrap()
            .to_string()
            .contains("observes no window"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_counter_that_goes_backwards_is_a_counted_loss() {
        let path = scratch("backwards");
        write_totals(&path, 900, 800);
        let mut collector = Pressure::new(poll(), path.clone());
        collector.start().expect("the fake file reads");
        collector.sample(0).expect("the baseline read");
        write_totals(&path, 100, 50);

        assert!(collector.sample(1).expect("a read").is_empty());
        assert_eq!(collector.losses().undecoded, 1);
        std::fs::remove_file(&path).ok();
    }

    /// Ground truth against the kernel's own counter: the stall this collector
    /// reports for a window must be at most the stall the kernel counted over a
    /// window that contains it, and no share of a window can exceed all of it.
    #[test]
    fn the_reported_stall_is_bounded_by_the_kernels_own_counter() {
        let system = PathBuf::from("/proc/pressure/io");
        // PSI is a kernel option, and a kernel built without it has nothing to
        // check against.
        if !system.exists() {
            return;
        }
        let before = read_total_us(&system);
        let mut collector = Pressure::new(poll(), system.clone());
        collector.start().expect("/proc/pressure/io reads");
        collector.sample(0).expect("the baseline read");

        // Something for the device to do, so the window is not necessarily
        // empty. Whether it stalls at all is the machine's business; the bounds
        // hold either way.
        let scratch_file = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("target/psi-{}.bin", std::process::id()));
        if let Ok(mut file) = std::fs::File::create(&scratch_file) {
            let chunk = [0u8; 256 * 1024];
            for _ in 0..32 {
                let _ = file.write_all(&chunk);
            }
            let _ = file.sync_all();
        }
        std::fs::remove_file(&scratch_file).ok();

        let events = collector.sample(1).expect("a second read");
        let after = read_total_us(&system);

        assert_eq!(events.len(), 1);
        let event = &events[0];
        let stalled = event.attr("stalled_ns").unwrap().as_f64().unwrap();
        let interval = event.attr("interval_ns").unwrap().as_f64().unwrap();
        let ratio = event.attr("stall_ratio").unwrap().as_f64().unwrap();
        assert!(
            stalled <= (after - before) as f64 * 1_000.0,
            "reported {stalled} ns of stall inside a window where the kernel counted less"
        );
        assert!(
            ratio <= 1.0,
            "a window cannot be stalled for more than its own length: {stalled} ns of {interval}"
        );
    }

    fn read_total_us(path: &Path) -> u64 {
        let text = std::fs::read_to_string(path).expect("/proc/pressure/io reads");
        Reading::parse(&text, 0)
            .expect("a `some` line")
            .some
            .total_us
    }
}
