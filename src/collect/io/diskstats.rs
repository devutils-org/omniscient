//! The block-layer counters of `/proc/diskstats`.
//!
//! One file answers four sources: volume, operation rate, merges and queue
//! depth. It is the cheapest thing in the family and the least attributable:
//! the kernel keeps these counters per device, so nothing here says which
//! process, file or request the I/O belonged to. That is what the eBPF backends
//! are for, and it is why every one of these backends declares itself degraded.
//!
//! Three properties of the file decide the shape of this collector.
//!
//! **The counters are cumulative.** A value read at one instant says how much
//! the device has done since boot, which is a number about the machine's
//! lifetime and not about the run. So an event carries the *increase* over the
//! tick that produced it, together with the length of that tick. Summing the
//! increases gives the run's total; dividing by the window gives its rate. Had
//! the events carried the running total, both would have been nonsense.
//!
//! **Adding up every line double counts.** A partition's I/O is counted again
//! on the disk that holds it, and a device-mapper or md device's I/O is counted
//! again on each member below it. This machine, for instance, reports
//! `nvme0n1` and eight partitions of it, and `nvme0n1p6` alone carries almost
//! all of the traffic. So the sum runs over the devices nothing is stacked on,
//! and every event names the set it summed.
//!
//! **A sector here is 512 bytes**, whatever the device's logical block size --
//! the kernel normalizes it before writing the file. Bytes are therefore
//! `sectors x 512` exactly, not a conversion that has to be justified.
//!
//! What the numbers mean is the family's standing warning: this is what the
//! *device* did. Readahead, filesystem metadata and journal writes are in it,
//! page-cache hits never reach it, and the gap between this and what the
//! application asked for is the diagnosis rather than an error.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event};
use crate::request::{Mode, Target};

/// `/proc/diskstats` counts in 512-byte sectors whatever the device's own
/// block size is, so this factor is a fact about the file, not an assumption
/// about the hardware.
const SECTOR_BYTES: u64 = 512;

/// Which number a request wants out of the shared file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Bytes,
    Iops,
    Merges,
    Inflight,
}

impl Metric {
    fn source_id(self) -> &'static str {
        match self {
            Metric::Bytes => "io.bytes",
            Metric::Iops => "io.iops",
            Metric::Merges => "io.merges",
            Metric::Inflight => "io.inflight",
        }
    }

    /// The attribute holding the measurement, named for what it is rather than
    /// left to be guessed from the unit.
    fn value_attr(self) -> &'static str {
        match self {
            Metric::Bytes => "bytes",
            Metric::Iops => "ops",
            Metric::Merges => "merges",
            Metric::Inflight => "inflight",
        }
    }

    /// Queue depth is a gauge: what is outstanding at the instant of the read.
    /// The other three are counters, and only their increase means anything.
    fn is_gauge(self) -> bool {
        self == Metric::Inflight
    }
}

/// Holds the choice of metric until the run starts.
#[derive(Debug)]
pub struct Prepare {
    metric: Metric,
    mode: Mode,
}

impl Prepare {
    /// Refuses a mode this backend cannot serve, with what it would take to
    /// serve it, rather than returning a number of a different kind.
    pub fn new(metric: Metric, mode: Mode) -> Result<Self, String> {
        match (mode, metric.is_gauge()) {
            (Mode::Poll { .. }, _) => Ok(Self { metric, mode }),
            (Mode::Count, false) => Ok(Self { metric, mode }),
            (Mode::Snapshot, true) => Ok(Self { metric, mode }),
            (Mode::Count, true) => Err(format!(
                "`{}` is a queue depth: it is read at an instant, so it counts over no window. \
                 Poll it, or take one snapshot",
                metric.source_id()
            )),
            (Mode::Snapshot, false) => Err(format!(
                "one read of `/proc/diskstats` gives a counter that has been rising since boot, \
                 not what `{}` did during a run; poll it, or count it over a duration",
                metric.source_id()
            )),
            (Mode::Trace | Mode::Sample { .. }, _) => Err(format!(
                "`/proc/diskstats` holds the block layer's counters, not its events: `{}` per \
                 request needs the tracepoint backend, which arrives with the eBPF layer",
                metric.source_id()
            )),
        }
    }
}

impl Prepared for Prepare {
    /// These counters are kept per device for the whole machine, and the block
    /// layer records nothing about who asked for the I/O. So a target that
    /// names a process is refused by name rather than answered with the
    /// machine's total, which would be attribution that was never measured.
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match target {
            Target::System => Ok(Box::new(Diskstats::new(
                self.metric,
                self.mode,
                PathBuf::from("/proc/diskstats"),
            ))),
            other => Err(format!(
                "/proc/diskstats counts per device for the whole machine, and a {} target asks                  for something it does not record; use --system, or the eBPF backend once it                  lands",
                other.kind()
            )),
        }
    }
}

/// Reads one line per block device and keeps the ones that stand on their own.
pub struct Diskstats {
    id: CollectorId,
    metric: Metric,
    mode: Mode,
    path: PathBuf,
    /// What each device name turned out to be, cached: the classification
    /// reads `/sys`, and the answer only changes when hardware appears.
    roles: BTreeMap<String, Role>,
    /// The reading the next delta is measured against.
    previous: Option<Reading>,
    /// The reading a `count` request measures its total against: taken before
    /// the observed program starts, so the total covers the whole run.
    baseline: Option<Reading>,
    losses: Losses,
}

impl Diskstats {
    pub fn new(metric: Metric, mode: Mode, path: PathBuf) -> Self {
        Self {
            id: CollectorId::new(format!("{}/fs:proc", metric.source_id())),
            metric,
            mode,
            path,
            roles: BTreeMap::new(),
            previous: None,
            baseline: None,
            losses: Losses::default(),
        }
    }

    /// Reads the file and sums the devices whose counters are their own.
    fn read(&mut self) -> Result<Reading, SampleError> {
        let text = std::fs::read_to_string(&self.path)
            .map_err(|error| SampleError::Transient(format!("{}: {error}", self.path.display())))?;

        let mut totals = Totals::default();
        let mut devices = Vec::new();
        let mut undecoded = Vec::new();
        for line in text.lines() {
            let Some((name, numbers)) = split_line(line) else {
                undecoded.push(format!("`{}` is not a diskstats line", line.trim()));
                continue;
            };
            let role = match self.roles.get(name) {
                Some(role) => *role,
                None => {
                    let role = classify(name);
                    self.roles.insert(name.to_owned(), role);
                    role
                }
            };
            match role {
                // Counted again on the device below it; adding it here would
                // report the same I/O twice.
                Role::Derived => continue,
                Role::Unknown => {
                    undecoded.push(format!(
                        "`{name}` has no entry under /sys/class/block, so whether its counters \
                         are its own could not be checked; it is left out of the total"
                    ));
                    continue;
                }
                Role::Leaf => {}
            }
            match DeviceCounters::parse(&numbers) {
                Some(counters) => {
                    totals.add(&counters);
                    devices.push(name.to_owned());
                }
                None => undecoded.push(format!(
                    "`{name}` reports {} counters, fewer than the eleven every kernel since 2.6 \
                     writes",
                    numbers.len()
                )),
            }
        }

        for reason in undecoded {
            self.losses.undecoded += 1;
            self.losses.note(reason);
        }

        Ok(Reading {
            ts_ns: monotonic_ns(),
            totals,
            devices,
        })
    }

    /// Builds the event for the increase between two readings, or explains why
    /// there is none to build.
    fn delta_event(&mut self, before: &Reading, after: &Reading, seq: u64) -> Option<Event> {
        if before.devices != after.devices {
            self.losses.undecoded += 1;
            self.losses.note(format!(
                "the set of devices changed between two reads ({} then {}), so the increase \
                 between them is not one machine's worth of I/O",
                before.devices.join(","),
                after.devices.join(",")
            ));
            return None;
        }
        let Some(delta) = after.totals.since(&before.totals) else {
            self.losses.undecoded += 1;
            self.losses.note(
                "a counter in /proc/diskstats went backwards, which happens when a device is \
                 removed and re-added; this interval cannot be attributed"
                    .to_owned(),
            );
            return None;
        };

        let interval_ns = after.ts_ns.saturating_sub(before.ts_ns);
        let mut attrs = match self.metric {
            Metric::Bytes => vec![
                Attr::uint("bytes", delta.bytes()),
                Attr::uint("read_bytes", delta.sectors_read * SECTOR_BYTES),
                Attr::uint("write_bytes", delta.sectors_written * SECTOR_BYTES),
                Attr::uint("sector_bytes", SECTOR_BYTES),
            ],
            Metric::Iops => vec![
                Attr::uint("ops", delta.reads + delta.writes),
                Attr::uint("reads", delta.reads),
                Attr::uint("writes", delta.writes),
            ],
            Metric::Merges => vec![
                Attr::uint("merges", delta.reads_merged + delta.writes_merged),
                Attr::uint("read_merges", delta.reads_merged),
                Attr::uint("write_merges", delta.writes_merged),
            ],
            // A gauge has no increase to report.
            Metric::Inflight => return None,
        };
        // Absent on kernels that do not write these columns, and absent is not
        // zero: zero would claim the device discarded nothing.
        if self.metric == Metric::Bytes {
            if let Some(sectors) = delta.sectors_discarded {
                attrs.push(Attr::uint("discarded_bytes", sectors * SECTOR_BYTES));
            }
        }
        if self.metric == Metric::Iops {
            if let Some(discards) = delta.discards {
                attrs.push(Attr::uint("discards", discards));
            }
            if let Some(flushes) = delta.flushes {
                attrs.push(Attr::uint("flushes", flushes));
            }
        }
        attrs.push(Attr::uint("interval_ns", interval_ns));
        attrs.push(Attr::text("devices", after.devices.join(",")));

        Some(Event {
            ts_ns: after.ts_ns,
            ts_err_ns: 0,
            kind: self.metric.source_id().to_owned(),
            keys: Vec::new(),
            attrs,
            collector_id: self.id.clone(),
            seq,
        })
    }

    fn gauge_event(&self, reading: &Reading, seq: u64) -> Event {
        Event {
            ts_ns: reading.ts_ns,
            ts_err_ns: 0,
            kind: self.metric.source_id().to_owned(),
            keys: Vec::new(),
            attrs: vec![
                Attr::uint("inflight", reading.totals.in_flight),
                Attr::text("devices", reading.devices.join(",")),
            ],
            collector_id: self.id.clone(),
            seq,
        }
    }
}

impl Collector for Diskstats {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        self.metric.value_attr()
    }

    /// A counted run takes its baseline before the observed program is
    /// released, so its one total covers the whole run.
    ///
    /// A polled run does not: the runner takes its first sample immediately
    /// after starting, and an interval of a few microseconds would put a
    /// meaningless ratio at the head of every series. Its first read is the
    /// baseline instead, and produces no event, because a baseline is not a
    /// measurement of anything.
    fn start(&mut self) -> Result<(), String> {
        if self.mode != Mode::Count {
            return Ok(());
        }
        let reading = self
            .read()
            .map_err(|error| format!("cannot read {}: {error:?}", self.path.display()))?;
        self.baseline = Some(reading);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if self.metric.is_gauge() {
            let reading = self.read()?;
            let event = self.gauge_event(&reading, seq);
            self.previous = Some(reading);
            return Ok(vec![event]);
        }
        // A counted run reports one total at the end; reading the file every
        // tick would cost something and answer nothing.
        if self.mode == Mode::Count {
            return Ok(Vec::new());
        }
        let after = self.read()?;
        let before = self.previous.replace(after.clone());
        let event = before.and_then(|before| self.delta_event(&before, &after, seq));
        Ok(event.into_iter().collect())
    }

    /// A counted run reports its one total here, measured from the baseline
    /// taken before the observed program started.
    ///
    /// A polled run reports nothing here on purpose. The runner samples every
    /// collector and *then* decides to stop, so a closing read would cover the
    /// few microseconds since that last tick: a point in the series with a
    /// window near zero, which says nothing and drags the summary with it.
    fn stop(&mut self) -> Vec<Event> {
        if self.mode != Mode::Count {
            return Vec::new();
        }
        let Some(before) = self.baseline.clone() else {
            return Vec::new();
        };
        match self.read() {
            Ok(after) => self
                .delta_event(&before, &after, u64::MAX / 2)
                .into_iter()
                .collect(),
            Err(error) => {
                self.losses.failed_reads += 1;
                self.losses.note(format!(
                    "the closing read failed ({error:?}), so the last interval of this run is                      missing"
                ));
                Vec::new()
            }
        }
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

/// What one reading of the file adds up to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reading {
    ts_ns: u64,
    totals: Totals,
    /// The devices that were summed, in the order the file lists them, so an
    /// event says what it covers instead of leaving it to be assumed.
    devices: Vec<String>,
}

/// The counters of every device that stands on its own, added together.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Totals {
    reads: u64,
    reads_merged: u64,
    sectors_read: u64,
    writes: u64,
    writes_merged: u64,
    sectors_written: u64,
    in_flight: u64,
    discards: Option<u64>,
    sectors_discarded: Option<u64>,
    flushes: Option<u64>,
}

impl Totals {
    fn add(&mut self, device: &DeviceCounters) {
        self.reads += device.reads;
        self.reads_merged += device.reads_merged;
        self.sectors_read += device.sectors_read;
        self.writes += device.writes;
        self.writes_merged += device.writes_merged;
        self.sectors_written += device.sectors_written;
        self.in_flight += device.in_flight;
        self.discards = sum_known(self.discards, device.discards);
        self.sectors_discarded = sum_known(self.sectors_discarded, device.sectors_discarded);
        self.flushes = sum_known(self.flushes, device.flushes);
    }

    fn bytes(&self) -> u64 {
        (self.sectors_read + self.sectors_written) * SECTOR_BYTES
    }

    /// The increase since an earlier reading, or `None` when a counter went
    /// backwards and the interval is therefore not measurable.
    fn since(&self, before: &Totals) -> Option<Totals> {
        Some(Totals {
            reads: self.reads.checked_sub(before.reads)?,
            reads_merged: self.reads_merged.checked_sub(before.reads_merged)?,
            sectors_read: self.sectors_read.checked_sub(before.sectors_read)?,
            writes: self.writes.checked_sub(before.writes)?,
            writes_merged: self.writes_merged.checked_sub(before.writes_merged)?,
            sectors_written: self.sectors_written.checked_sub(before.sectors_written)?,
            // A gauge has no meaningful difference; the later value is kept so
            // the field says what it said.
            in_flight: self.in_flight,
            discards: difference(self.discards, before.discards),
            sectors_discarded: difference(self.sectors_discarded, before.sectors_discarded),
            flushes: difference(self.flushes, before.flushes),
        })
    }
}

/// Adds a column only while every device reports it. One kernel writes it and
/// another does not, and a partial sum would be a number about neither.
fn sum_known(running: Option<u64>, device: Option<u64>) -> Option<u64> {
    match (running, device) {
        (None, value) => value,
        (Some(running), Some(value)) => Some(running + value),
        (Some(_), None) => None,
    }
}

fn difference(after: Option<u64>, before: Option<u64>) -> Option<u64> {
    match (after, before) {
        (Some(after), Some(before)) => after.checked_sub(before),
        _ => None,
    }
}

/// Whether a device's counters are its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Nothing is stacked on it: its counters are counted nowhere else.
    Leaf,
    /// A partition, or a device-mapper or md device built on others. Its I/O
    /// appears again on the device below it.
    Derived,
    /// `/sys/class/block` said nothing about it. Left out rather than guessed
    /// at, and said so.
    Unknown,
}

/// Asks `/sys` what a device is, rather than reading its name.
///
/// Names are a trap here: `nvme0n1p6` looks like a partition and is one, but
/// `dm-0` looks like a disk and is stacked on whatever is below it, while
/// `zram0` looks synthetic and is genuinely its own device.
fn classify(name: &str) -> Role {
    classify_under(Path::new("/sys/class/block"), name)
}

fn classify_under(root: &Path, name: &str) -> Role {
    let device = root.join(name);
    if !device.exists() {
        return Role::Unknown;
    }
    if device.join("partition").exists() {
        return Role::Derived;
    }
    match std::fs::read_dir(device.join("slaves")) {
        // No `slaves` directory, or an empty one: nothing is underneath.
        Ok(mut entries) => match entries.next() {
            Some(_) => Role::Derived,
            None => Role::Leaf,
        },
        Err(_) => Role::Leaf,
    }
}

/// Splits `  259  0 nvme0n1 88820 1663 ...` into the name and its counters.
fn split_line(line: &str) -> Option<(&str, Vec<u64>)> {
    let mut fields = line.split_whitespace();
    let _major: u64 = fields.next()?.parse().ok()?;
    let _minor: u64 = fields.next()?.parse().ok()?;
    let name = fields.next()?;
    let numbers = fields
        .map(|field| field.parse().ok())
        .collect::<Option<_>>()?;
    Some((name, numbers))
}

/// One device's line, in the order the kernel writes the columns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DeviceCounters {
    reads: u64,
    reads_merged: u64,
    sectors_read: u64,
    writes: u64,
    writes_merged: u64,
    sectors_written: u64,
    in_flight: u64,
    /// Discards arrived in 4.18 and flushes in 5.5. `None` means this kernel
    /// does not report them.
    discards: Option<u64>,
    sectors_discarded: Option<u64>,
    flushes: Option<u64>,
}

impl DeviceCounters {
    /// The eleven classic columns are required; the discard and flush columns
    /// are taken when the kernel writes them.
    fn parse(numbers: &[u64]) -> Option<Self> {
        if numbers.len() < 11 {
            return None;
        }
        Some(Self {
            reads: numbers[0],
            reads_merged: numbers[1],
            sectors_read: numbers[2],
            writes: numbers[4],
            writes_merged: numbers[5],
            sectors_written: numbers[6],
            in_flight: numbers[8],
            discards: numbers.get(11).copied(),
            sectors_discarded: numbers.get(13).copied(),
            flushes: numbers.get(15).copied(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A line as this machine's kernel writes it, with the twenty columns of
    /// 5.5 and later.
    const NVME_LINE: &str =
        " 259       0 nvme0n1 88820 1663 10828804 52651 407865 2784 33649410 737959 0 58254 \
         799183 16568 0 31757872 4559 9322 4011";

    fn poll() -> Mode {
        Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        }
    }

    /// A device on this machine whose counters are its own, so a synthetic
    /// file written for a test is classified the way a real one would be.
    fn a_leaf_device() -> Option<String> {
        let text = std::fs::read_to_string("/proc/diskstats").ok()?;
        text.lines()
            .filter_map(|line| split_line(line).map(|(name, _)| name.to_owned()))
            .find(|name| classify(name) == Role::Leaf)
    }

    /// A private file to stand in for `/proc/diskstats`.
    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "omn-io-{}-{}-{name}",
            std::process::id(),
            monotonic_ns()
        ));
        std::fs::write(&path, "").expect("a scratch file");
        path
    }

    fn write_stats(path: &Path, lines: &[String]) {
        std::fs::write(path, lines.join("\n") + "\n").expect("writing the fake diskstats");
    }

    /// One device line with the counters a test cares about.
    fn line(
        name: &str,
        reads: u64,
        sectors_read: u64,
        writes: u64,
        sectors_written: u64,
    ) -> String {
        format!(
            "   8       0 {name} {reads} 0 {sectors_read} 0 {writes} 0 {sectors_written} 0 3 0 0"
        )
    }

    #[test]
    fn a_line_is_read_the_way_the_kernel_writes_it() {
        let (name, numbers) = split_line(NVME_LINE).expect("a diskstats line");
        assert_eq!(name, "nvme0n1");
        let counters = DeviceCounters::parse(&numbers).expect("eleven columns at least");
        assert_eq!(counters.reads, 88_820);
        assert_eq!(counters.sectors_read, 10_828_804);
        assert_eq!(counters.writes, 407_865);
        assert_eq!(counters.sectors_written, 33_649_410);
        assert_eq!(counters.in_flight, 0);
        assert_eq!(counters.discards, Some(16_568));
        assert_eq!(counters.flushes, Some(9_322));
    }

    /// The columns that arrived in 4.18 and 5.5 are absent on older kernels,
    /// and absent is not zero: zero would say the device discarded nothing.
    #[test]
    fn a_kernel_that_does_not_report_discards_says_nothing_rather_than_zero() {
        let (_, numbers) = split_line(&line("sda", 1, 2, 3, 4)).expect("a line");
        let counters = DeviceCounters::parse(&numbers).expect("eleven columns");
        assert_eq!(counters.discards, None);
        assert_eq!(counters.flushes, None);
        assert_eq!(counters.in_flight, 3);
    }

    /// Adding every line of the file would count a partition's I/O twice, once
    /// on the partition and once on the disk that holds it.
    #[test]
    fn what_a_device_is_comes_from_sysfs_rather_than_from_its_name() {
        let root = scratch("sysfs");
        std::fs::remove_file(&root).expect("replacing the file with a directory");
        let disk = root.join("nvme0n1");
        let partition = root.join("nvme0n1p6");
        let mapped = root.join("dm-0");
        std::fs::create_dir_all(&disk).unwrap();
        std::fs::create_dir_all(&partition).unwrap();
        std::fs::create_dir_all(mapped.join("slaves").join("nvme0n1p6")).unwrap();
        std::fs::write(partition.join("partition"), "6\n").unwrap();

        assert_eq!(classify_under(&root, "nvme0n1"), Role::Leaf);
        assert_eq!(classify_under(&root, "nvme0n1p6"), Role::Derived);
        assert_eq!(classify_under(&root, "dm-0"), Role::Derived);
        assert_eq!(classify_under(&root, "sdz"), Role::Unknown);
        std::fs::remove_dir_all(&root).ok();
    }

    /// The same, against the machine the test runs on: a partition here must be
    /// recognised as one, or every byte it moves is counted twice.
    #[test]
    fn this_machines_partitions_are_not_mistaken_for_disks() {
        let text = match std::fs::read_to_string("/proc/diskstats") {
            Ok(text) => text,
            Err(_) => return,
        };
        for line in text.lines() {
            let Some((name, _)) = split_line(line) else {
                continue;
            };
            let sysfs = Path::new("/sys/class/block").join(name);
            if !sysfs.exists() {
                continue;
            }
            let expected = if sysfs.join("partition").exists() {
                Role::Derived
            } else {
                classify(name)
            };
            assert_eq!(classify(name), expected, "`{name}` was classified wrongly");
        }
    }

    /// Ground truth against the kernel's own arithmetic: the increase reported
    /// for an interval is exactly the increase the file shows over it.
    #[test]
    fn the_reported_volume_is_the_increase_between_two_reads() {
        let Some(device) = a_leaf_device() else {
            return;
        };
        let path = scratch("bytes");
        write_stats(&path, &[line(&device, 10, 100, 20, 200)]);

        let mut collector = Diskstats::new(Metric::Bytes, poll(), path.clone());
        collector.start().expect("the fake file reads");
        assert!(
            collector.sample(0).expect("the first read").is_empty(),
            "the first read is a baseline, and a baseline measures nothing"
        );
        write_stats(&path, &[line(&device, 12, 108, 23, 216)]);
        let events = collector.sample(1).expect("a second read");

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.attr("bytes").unwrap().as_f64(), Some(24.0 * 512.0));
        assert_eq!(
            event.attr("read_bytes").unwrap().as_f64(),
            Some(8.0 * 512.0)
        );
        assert_eq!(
            event.attr("write_bytes").unwrap().as_f64(),
            Some(16.0 * 512.0)
        );
        assert_eq!(event.attr("devices").unwrap().to_string(), device);
        assert!(event.attr("interval_ns").unwrap().as_f64().unwrap() > 0.0);
        assert!(collector.losses().is_clean());
        std::fs::remove_file(&path).ok();
    }

    /// A counted run reports one total for the whole window, measured from
    /// before the observed program started.
    #[test]
    fn a_counted_run_reports_one_total_over_the_whole_window() {
        let Some(device) = a_leaf_device() else {
            return;
        };
        let path = scratch("count");
        write_stats(&path, &[line(&device, 10, 100, 20, 200)]);

        let mut collector = Diskstats::new(Metric::Iops, Mode::Count, path.clone());
        collector.start().expect("the fake file reads");
        write_stats(&path, &[line(&device, 15, 140, 21, 208)]);
        assert!(
            collector.sample(0).expect("a tick").is_empty(),
            "a counted run has nothing to say until it ends"
        );
        write_stats(&path, &[line(&device, 40, 400, 60, 600)]);

        let events = collector.stop();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].attr("ops").unwrap().as_f64(), Some(70.0));
        assert_eq!(events[0].attr("reads").unwrap().as_f64(), Some(30.0));
        assert_eq!(events[0].attr("writes").unwrap().as_f64(), Some(40.0));
        std::fs::remove_file(&path).ok();
    }

    /// A device that is removed and re-added restarts its counters at zero. The
    /// interval that straddles it is not measurable, and saying so is the only
    /// honest answer: a wrapped subtraction would report an enormous number.
    #[test]
    fn a_counter_that_goes_backwards_is_a_counted_loss_not_a_negative_number() {
        let Some(device) = a_leaf_device() else {
            return;
        };
        let path = scratch("backwards");
        write_stats(&path, &[line(&device, 500, 5_000, 600, 6_000)]);

        let mut collector = Diskstats::new(Metric::Bytes, poll(), path.clone());
        collector.start().expect("the fake file reads");
        collector.sample(0).expect("the baseline read");
        write_stats(&path, &[line(&device, 1, 10, 2, 20)]);

        assert!(collector.sample(1).expect("a read").is_empty());
        let losses = collector.losses();
        assert_eq!(losses.undecoded, 1);
        assert!(!losses.is_clean());
        assert!(losses
            .reasons
            .keys()
            .any(|reason| reason.contains("went backwards")));
        std::fs::remove_file(&path).ok();
    }

    /// Two readings over different sets of devices do not subtract: the
    /// difference would be part I/O and part hotplug.
    #[test]
    fn a_device_appearing_mid_run_voids_the_interval_rather_than_inflating_it() {
        let Some(device) = a_leaf_device() else {
            return;
        };
        let Some(second) = std::fs::read_to_string("/proc/diskstats")
            .ok()
            .and_then(|text| {
                text.lines()
                    .filter_map(|line| split_line(line).map(|(name, _)| name.to_owned()))
                    .find(|name| classify(name) == Role::Leaf && *name != device)
            })
        else {
            return;
        };
        let path = scratch("hotplug");
        write_stats(&path, &[line(&device, 10, 100, 20, 200)]);

        let mut collector = Diskstats::new(Metric::Bytes, poll(), path.clone());
        collector.start().expect("the fake file reads");
        collector.sample(0).expect("the baseline read");
        write_stats(
            &path,
            &[line(&device, 12, 108, 23, 216), line(&second, 1, 8, 1, 8)],
        );

        assert!(collector.sample(1).expect("a read").is_empty());
        assert!(collector
            .losses()
            .reasons
            .keys()
            .any(|reason| reason.contains("set of devices changed")));
        std::fs::remove_file(&path).ok();
    }

    /// Queue depth is what is outstanding at the instant of the read, so it is
    /// reported as it stands rather than as an increase.
    #[test]
    fn queue_depth_is_reported_as_it_stands() {
        let Some(device) = a_leaf_device() else {
            return;
        };
        let path = scratch("inflight");
        write_stats(&path, &[line(&device, 10, 100, 20, 200)]);

        let mut collector = Diskstats::new(Metric::Inflight, Mode::Snapshot, path.clone());
        collector.start().expect("the fake file reads");
        let events = collector.sample(0).expect("a read");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].attr("inflight").unwrap().as_f64(), Some(3.0));
        std::fs::remove_file(&path).ok();
    }

    /// `/proc/diskstats` counts what the device did, not what an event stream
    /// says; a mode it cannot serve is refused with what would serve it.
    #[test]
    fn a_mode_this_backend_cannot_serve_is_refused_with_the_reason() {
        let refused = Prepare::new(Metric::Bytes, Mode::Trace).expect_err("trace is not servable");
        assert!(refused.contains("eBPF"), "{refused}");
        let refused =
            Prepare::new(Metric::Bytes, Mode::Snapshot).expect_err("a counter needs a window");
        assert!(refused.contains("since boot"), "{refused}");
        assert!(Prepare::new(Metric::Inflight, Mode::Snapshot).is_ok());
    }

    /// Ground truth against something known independently of this code: bytes
    /// that were written and flushed to a real device must appear in the
    /// device's counters, and the run's own increase cannot exceed the increase
    /// the kernel reports over a window that contains it.
    #[test]
    fn bytes_flushed_to_a_disk_show_up_in_its_counters() {
        const SIZE: usize = 32 * 1024 * 1024;
        const CHUNK: usize = 256 * 1024;

        let scratch_file = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("target/io-ground-truth-{}.bin", std::process::id()));
        std::fs::create_dir_all(scratch_file.parent().expect("target/")).unwrap();
        // A file on a tmpfs never reaches a block device, and neither does one
        // on an overlay in a container. Without a real device below it there is
        // no ground truth to check against, so the test says nothing rather
        // than failing for the wrong reason.
        if !backed_by_a_block_device(scratch_file.parent().unwrap()) {
            return;
        }

        let stats = PathBuf::from("/proc/diskstats");
        let before = independent_sectors_written(&stats);
        let mut collector = Diskstats::new(Metric::Bytes, poll(), stats.clone());
        collector.start().expect("/proc/diskstats reads");
        collector.sample(0).expect("the baseline read");

        // Incompressible, so a filesystem that compresses cannot make the
        // device write less than was asked of it. Written a chunk at a time:
        // holding the whole file in memory would leave a freed arena behind
        // that the tests running after this one inherit.
        let chunk = incompressible(CHUNK);
        let mut file = std::fs::File::create(&scratch_file).expect("a scratch file");
        for _ in 0..SIZE / CHUNK {
            file.write_all(&chunk).expect("writing");
        }
        file.sync_all().expect("fsync");
        drop(file);

        let events = collector.sample(1).expect("a second read");
        let after = independent_sectors_written(&stats);
        std::fs::remove_file(&scratch_file).ok();

        assert_eq!(events.len(), 1, "one interval, one event");
        let written = events[0].attr("write_bytes").unwrap().as_f64().unwrap();
        assert!(
            written >= SIZE as f64,
            "{SIZE} bytes were flushed to the device but only {written} were reported"
        );
        let independent = (after - before) as f64 * 512.0;
        assert!(
            written <= independent,
            "the collector reported {written} bytes over a window inside one where the kernel \
             itself counted {independent}"
        );
    }

    /// The sum of every line would be larger than the truth wherever a disk has
    /// partitions doing I/O, which is the ordinary case.
    #[test]
    fn summing_every_line_would_count_the_same_bytes_twice() {
        let Ok(text) = std::fs::read_to_string("/proc/diskstats") else {
            return;
        };
        let mut naive = 0u64;
        let mut leaves = 0u64;
        for line in text.lines() {
            let Some((name, numbers)) = split_line(line) else {
                continue;
            };
            let Some(counters) = DeviceCounters::parse(&numbers) else {
                continue;
            };
            naive += counters.sectors_read + counters.sectors_written;
            if classify(name) == Role::Leaf {
                leaves += counters.sectors_read + counters.sectors_written;
            }
        }
        assert!(
            leaves <= naive,
            "the devices that stand on their own cannot move more than every line together"
        );
    }

    /// Whether a path sits on a real block device.
    ///
    /// `stat` is not enough: btrfs, and any filesystem that gives its
    /// subvolumes an anonymous device, reports a `st_dev` with major 0 that
    /// appears nowhere in `/sys/dev/block`. What the mount was made from is in
    /// `/proc/self/mountinfo`, so that is what is asked.
    fn backed_by_a_block_device(path: &Path) -> bool {
        mount_source(path).is_some_and(|source| source.starts_with("/dev/"))
    }

    /// The device the longest matching mount in `/proc/self/mountinfo` was made
    /// from. The longest match is the one that matters: `/home` is mounted over
    /// `/`, and the file is on whichever is deeper.
    fn mount_source(path: &Path) -> Option<String> {
        let path = path.canonicalize().ok()?;
        let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
        let mut best: Option<(usize, String)> = None;
        for line in text.lines() {
            let (before, after) = line.split_once(" - ")?;
            let mount_point = before.split_whitespace().nth(4)?;
            let source = after.split_whitespace().nth(1)?;
            if !path.starts_with(mount_point) {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|(len, _)| *len < mount_point.len())
            {
                best = Some((mount_point.len(), source.to_owned()));
            }
        }
        best.map(|(_, source)| source)
    }

    /// The same sum, written again in the test so the collector is checked
    /// against something other than itself.
    fn independent_sectors_written(path: &Path) -> u64 {
        let text = std::fs::read_to_string(path).expect("/proc/diskstats reads");
        let mut total = 0;
        for line in text.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 14 {
                continue;
            }
            if classify(fields[2]) != Role::Leaf {
                continue;
            }
            total += fields[9].parse::<u64>().unwrap_or(0);
        }
        total
    }

    /// Bytes no filesystem can compress away, from a generator with no
    /// dependencies.
    fn incompressible(size: usize) -> Vec<u8> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut bytes = Vec::with_capacity(size);
        while bytes.len() < size {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.extend_from_slice(&state.to_le_bytes());
        }
        bytes.truncate(size);
        bytes
    }
}
