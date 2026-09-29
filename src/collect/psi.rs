//! Pressure stall information, read once for every family that reads it.
//!
//! `/proc/pressure/{cpu,io,memory,irq}` and a cgroup's `*.pressure` files all
//! write the same two lines -- `some` and `full`, three decayed averages and a
//! microsecond counter each -- and `io.pressure` and `mem.pressure` each had a
//! reader of their own that agreed with the other by convention only (finding
//! 23 in `docs/open-findings.md`). This is the one reader, and the one place
//! the arithmetic a poll does over two reads is written:
//!
//! - the counter's increase over a window, divided by the window, is the share
//!   of *that window* that was lost -- a fact about the run;
//! - the averages describe the last ten seconds to five minutes, which overlaps
//!   the run without being about it, so a snapshot reports them under their
//!   own names and never as a ratio it measured.
//!
//! The kernel writes percentages; every ratio here is in `[0, 1]`, on one scale
//! with the ratio computed from the counter.
//!
//! Absent is not zero. A cgroup on an older kernel writes no `full` line, and
//! zero there would claim nothing was ever fully stalled.
//!
//! **`/proc/pressure/irq` is not read by this**, and it is the one file that
//! differs: it writes a `full` line and no `some` -- an interrupt stalls the
//! cpu it lands on, and there is no "some tasks" half of that -- measured on
//! the development machine. No source reads it yet; the one that does wants a
//! reading whose `some` is optional, and its headline is `full`.

use crate::event::Attr;

/// One `some`/`full` line: three decayed averages and a stall counter.
#[derive(Debug, Clone, PartialEq)]
pub struct Psi {
    /// The kernel's averages, in the per cent the file writes.
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    /// Microseconds of stall since boot, or since the cgroup was created.
    pub total_us: u64,
}

/// One read of a pressure file.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// When it was read, on `CLOCK_MONOTONIC`.
    pub ts_ns: u64,
    pub some: Psi,
    /// Absent where the kernel writes no `full` line.
    pub full: Option<Psi>,
}

/// Why two reads measure no window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unmeasured {
    /// Both reads landed in the same nanosecond: nothing to divide by.
    SameInstant,
    /// The `some` counter went backwards: the file was replaced, or the cgroup
    /// was recreated under the same path.
    Backwards,
}

impl Unmeasured {
    /// The sentence a run's losses carry, naming the file.
    pub fn explain(self, file: &str) -> String {
        match self {
            Unmeasured::SameInstant => format!(
                "two reads of {file} landed in the same nanosecond, so there is no window to \
                 divide by"
            ),
            Unmeasured::Backwards => format!(
                "the stall counter in {file} went backwards, so this interval cannot be \
                 attributed"
            ),
        }
    }
}

impl Reading {
    /// Parses the file as the kernel writes it, or `None` when it holds no
    /// `some ... total=N` line.
    pub fn parse(text: &str, ts_ns: u64) -> Option<Self> {
        let mut some = None;
        let mut full = None;
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let Some(kind) = fields.next() else {
                continue;
            };
            let mut psi = Psi {
                avg10: 0.0,
                avg60: 0.0,
                avg300: 0.0,
                total_us: 0,
            };
            for field in fields {
                let (name, value) = field.split_once('=')?;
                match name {
                    "avg10" => psi.avg10 = value.parse().ok()?,
                    "avg60" => psi.avg60 = value.parse().ok()?,
                    "avg300" => psi.avg300 = value.parse().ok()?,
                    "total" => psi.total_us = value.parse().ok()?,
                    // A field this reader does not know is not a reason to
                    // discard a line that holds what it came for.
                    _ => {}
                }
            }
            match kind {
                "some" => some = Some(psi),
                "full" => full = Some(psi),
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
    /// writes.
    pub fn average_attrs(&self) -> Vec<Attr> {
        let mut attrs = vec![
            Attr::float("some_avg10", self.some.avg10 / 100.0),
            Attr::float("some_avg60", self.some.avg60 / 100.0),
            Attr::float("some_avg300", self.some.avg300 / 100.0),
        ];
        if let Some(full) = &self.full {
            attrs.push(Attr::float("full_avg10", full.avg10 / 100.0));
            attrs.push(Attr::float("full_avg60", full.avg60 / 100.0));
            attrs.push(Attr::float("full_avg300", full.avg300 / 100.0));
        }
        attrs
    }

    /// What one read can say: the kernel's averages and its two counters, and
    /// the basis that says a snapshot observed no window of its own.
    pub fn snapshot_attrs(&self) -> Vec<Attr> {
        let mut attrs = self.average_attrs();
        attrs.push(Attr::uint("some_total_ns", self.some.total_us * 1_000));
        if let Some(full) = &self.full {
            attrs.push(Attr::uint("full_total_ns", full.total_us * 1_000));
        }
        attrs
    }

    /// The share of the window since `before` that was stalled, from the
    /// counters, with the averages of this read beside it.
    pub fn stall_attrs(&self, before: &Reading) -> Result<Vec<Attr>, Unmeasured> {
        let interval_ns = self.ts_ns.saturating_sub(before.ts_ns);
        if interval_ns == 0 {
            return Err(Unmeasured::SameInstant);
        }
        let some_ns =
            stalled_ns(self.some.total_us, before.some.total_us).ok_or(Unmeasured::Backwards)?;
        let mut attrs = vec![
            Attr::float("stall_ratio", some_ns as f64 / interval_ns as f64),
            Attr::uint("stalled_ns", some_ns),
            Attr::uint("interval_ns", interval_ns),
        ];
        if let Some(full_ns) = self
            .full
            .as_ref()
            .zip(before.full.as_ref())
            .and_then(|(after, before)| stalled_ns(after.total_us, before.total_us))
        {
            attrs.push(Attr::float(
                "full_stall_ratio",
                full_ns as f64 / interval_ns as f64,
            ));
            attrs.push(Attr::uint("full_stalled_ns", full_ns));
        }
        attrs.extend(self.average_attrs());
        Ok(attrs)
    }
}

/// What the `basis` attribute says a poll's value is.
pub const POLL_BASIS: &str = "the stall counter's increase over this tick";
/// What it says a snapshot's is.
pub const SNAPSHOT_BASIS: &str =
    "the kernel's decayed averages: a snapshot observes no window of its own";

/// Microseconds of stall turned into nanoseconds, or `None` when the counter
/// moved backwards.
fn stalled_ns(after_us: u64, before_us: u64) -> Option<u64> {
    after_us.checked_sub(before_us).map(|micros| micros * 1_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file as this machine writes it.
    const SYSTEM: &str = "\
some avg10=30.96 avg60=37.29 avg300=46.82 total=9276004371
full avg10=29.77 avg60=36.24 avg300=45.91 total=9194047541
";

    fn reading(ts_ns: u64, some_us: u64, full_us: Option<u64>) -> Reading {
        let mut text = format!("some avg10=1.00 avg60=2.00 avg300=3.00 total={some_us}\n");
        if let Some(full_us) = full_us {
            text.push_str(&format!(
                "full avg10=0.50 avg60=1.00 avg300=1.50 total={full_us}\n"
            ));
        }
        Reading::parse(&text, ts_ns).expect("a `some` line")
    }

    #[test]
    fn the_file_is_read_the_way_the_kernel_writes_it() {
        let reading = Reading::parse(SYSTEM, 42).expect("a `some` line");
        assert_eq!(reading.ts_ns, 42);
        assert_eq!(reading.some.total_us, 9_276_004_371);
        assert_eq!(reading.some.avg10, 30.96);
        assert_eq!(reading.full.as_ref().unwrap().avg300, 45.91);
    }

    /// A file with no `some` line is not a reading of zero stall.
    #[test]
    fn a_file_without_a_some_line_is_no_reading() {
        assert!(Reading::parse("full avg10=0.00 avg60=0.00 avg300=0.00 total=1\n", 0).is_none());
        assert!(Reading::parse("", 0).is_none());
    }

    /// A cgroup on an older kernel writes no `full` line. Reporting zero there
    /// would claim nothing was ever fully stalled.
    #[test]
    fn a_missing_full_line_is_absent_rather_than_zero() {
        let reading = reading(0, 17, None);
        assert!(reading.full.is_none());
        assert!(reading
            .average_attrs()
            .iter()
            .chain(&reading.snapshot_attrs())
            .all(|attr| !attr.name.starts_with("full")));
        let later = self::reading(1_000, 20, None);
        assert!(later
            .stall_attrs(&reading)
            .expect("a window")
            .iter()
            .all(|attr| !attr.name.starts_with("full")));
    }

    /// The percentages the kernel writes come out as ratios.
    #[test]
    fn the_kernels_averages_are_reported_as_ratios() {
        let reading = Reading::parse(SYSTEM, 0).expect("a `some` line");
        let attrs = reading.average_attrs();
        let avg10 = attrs.iter().find(|attr| attr.name == "some_avg10").unwrap();
        assert_eq!(avg10.value.as_f64(), Some(0.3096));
    }

    /// The share of a window that was stalled is the counters' increase over
    /// that window, and nothing else.
    #[test]
    fn the_stalled_share_is_the_counters_increase_over_the_window() {
        let before = reading(1_000_000_000, 1_000_000, Some(500_000));
        let after = reading(2_000_000_000, 1_250_000, Some(600_000));
        let attrs = after.stall_attrs(&before).expect("a window");
        let value = |name: &str| {
            attrs
                .iter()
                .find(|attr| attr.name == name)
                .and_then(|attr| attr.value.as_f64())
                .unwrap_or_else(|| panic!("no `{name}`"))
        };
        assert_eq!(value("stalled_ns"), 250e6);
        assert_eq!(value("full_stalled_ns"), 100e6);
        assert_eq!(value("interval_ns"), 1e9);
        assert_eq!(value("stall_ratio"), 0.25);
        assert_eq!(value("full_stall_ratio"), 0.1);
    }

    #[test]
    fn a_window_that_measures_nothing_says_why() {
        let before = reading(5, 900, None);
        assert_eq!(
            reading(5, 1_000, None).stall_attrs(&before),
            Err(Unmeasured::SameInstant)
        );
        assert_eq!(
            reading(6, 100, None).stall_attrs(&before),
            Err(Unmeasured::Backwards)
        );
        assert!(Unmeasured::Backwards
            .explain("/proc/pressure/io")
            .contains("/proc/pressure/io"));
    }

    /// Ground truth against the kernel's own files: two reads of each of the
    /// three pressure files a source reads parse, and the share between them is
    /// a share. `irq` writes `full` alone, and is no reading here rather than a
    /// reading of zero.
    #[test]
    fn every_pressure_file_a_source_reads_is_read() {
        if let Ok(irq) = std::fs::read_to_string("/proc/pressure/irq") {
            if !irq.lines().any(|line| line.starts_with("some")) {
                assert!(Reading::parse(&irq, 0).is_none(), "no `some` is no reading");
            }
        }
        let mut read = 0;
        for resource in ["cpu", "io", "memory"] {
            let path = format!("/proc/pressure/{resource}");
            let Ok(first) = std::fs::read_to_string(&path) else {
                continue;
            };
            let before = Reading::parse(&first, crate::clock::monotonic_ns())
                .unwrap_or_else(|| panic!("{path}: {first}"));
            std::thread::sleep(std::time::Duration::from_millis(5));
            let second = std::fs::read_to_string(&path).expect("read a second time");
            let after = Reading::parse(&second, crate::clock::monotonic_ns()).expect("parses");
            let attrs = after.stall_attrs(&before).expect("a window");
            let ratio = attrs[0].value.as_f64().expect("a ratio");
            assert!((0.0..=1.0).contains(&ratio), "{path}: {ratio}");
            read += 1;
        }
        if read == 0 {
            eprintln!("skipped: this kernel was built without CONFIG_PSI");
        }
    }
}
