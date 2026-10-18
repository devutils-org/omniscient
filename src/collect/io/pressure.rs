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
use crate::collect::psi::{Reading, POLL_BASIS, SNAPSHOT_BASIS};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event};
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
                "/proc/pressure/io is the machine's stall counter; a {} target needs \
                 `io.pressure` under that cgroup, which is the `fs:cgroup` backend",
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
        let mut attrs = match after.stall_attrs(before) {
            Ok(attrs) => attrs,
            Err(unmeasured) => {
                self.losses.undecoded += 1;
                self.losses
                    .note_times(unmeasured.explain("/proc/pressure/io"), 1);
                return None;
            }
        };
        attrs.push(Attr::text("basis", POLL_BASIS));
        Some(self.event(after.ts_ns, attrs, seq))
    }

    /// What can be said from a single read: the kernel's own averages, which
    /// describe a window this run did not choose.
    fn snapshot_event(&self, reading: &Reading, seq: u64) -> Event {
        let mut attrs = reading.snapshot_attrs();
        attrs.push(Attr::text("basis", SNAPSHOT_BASIS));
        self.event(reading.ts_ns, attrs, seq)
    }

    fn event(&self, ts_ns: u64, attrs: Vec<Attr>, seq: u64) -> Event {
        Event {
            ts_ns,
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

    /// The same measurement per cgroup is a different file and a different
    /// backend; answering a cgroup with the machine's stall counter would be
    /// attribution nobody measured.
    #[test]
    fn a_cgroup_target_is_sent_to_the_backend_that_can_serve_it() {
        let prepared = Box::new(Prepare::new(poll()).expect("poll is servable"));
        let refused = prepared
            .attach(&Target::Cgroup(PathBuf::from("/sys/fs/cgroup/user.slice")))
            .err()
            .expect("the machine-wide file cannot narrow to a cgroup");
        assert!(refused.contains("fs:cgroup"), "{refused}");
    }

    /// A poll measures its own window and a snapshot repeats the kernel's.
    /// They are different numbers, so nothing downstream may find them under
    /// the same name.
    #[test]
    fn the_two_modes_do_not_publish_their_value_under_one_name() {
        let path = scratch("names");
        let polled = Pressure::new(poll(), path.clone());
        let snapshot = Pressure::new(Mode::Snapshot, path.clone());
        assert_ne!(polled.value_attr(), snapshot.value_attr());
        assert_eq!(polled.value_attr(), "stall_ratio");
        assert_eq!(snapshot.value_attr(), "some_avg10");
        std::fs::remove_file(&path).ok();
    }

    fn read_total_us(path: &Path) -> u64 {
        let text = std::fs::read_to_string(path).expect("/proc/pressure/io reads");
        Reading::parse(&text, 0)
            .expect("a `some` line")
            .some
            .total_us
    }
}
