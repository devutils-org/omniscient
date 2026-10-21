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
use crate::collect::poll::{CounterPoll, Cumulative, Interval};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Direction, Event, KeyValue};
use crate::request::{KeyKind, Mode, Target};

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
    /// The share of the window the device had a request outstanding: the
    /// io-ticks column, what `iostat -x` prints as `%util`.
    Util,
}

impl Metric {
    /// The attributes this metric reads out of one device's increase.
    ///
    /// `None` for a gauge, which has no increase to report: a queue depth is
    /// read at an instant and the difference between two depths is not a depth.
    fn attrs(self, delta: &DeviceCounters, interval_ns: u64) -> Option<Vec<Attr>> {
        let mut attrs = match self {
            Metric::Bytes => {
                let mut attrs = vec![
                    Attr::uint("bytes", delta.bytes()),
                    Attr::uint("read_bytes", delta.sectors_read * SECTOR_BYTES),
                    Attr::uint("write_bytes", delta.sectors_written * SECTOR_BYTES),
                    Attr::uint("sector_bytes", SECTOR_BYTES),
                ];
                // Absent on kernels that do not write this column, and absent
                // is not zero: zero would claim the device discarded nothing.
                if let Some(sectors) = delta.sectors_discarded {
                    attrs.push(Attr::uint("discarded_bytes", sectors * SECTOR_BYTES));
                }
                attrs
            }
            Metric::Iops => {
                let mut attrs = vec![
                    Attr::uint("ops", delta.reads + delta.writes),
                    Attr::uint("reads", delta.reads),
                    Attr::uint("writes", delta.writes),
                ];
                if let Some(discards) = delta.discards {
                    attrs.push(Attr::uint("discards", discards));
                }
                if let Some(flushes) = delta.flushes {
                    attrs.push(Attr::uint("flushes", flushes));
                }
                attrs
            }
            Metric::Merges => vec![
                Attr::uint("merges", delta.reads_merged + delta.writes_merged),
                Attr::uint("read_merges", delta.reads_merged),
                Attr::uint("write_merges", delta.writes_merged),
            ],
            // The kernel counts the time in milliseconds of jiffies -- a
            // request outstanding at a tick charges the whole tick -- so the
            // share is as coarse as that, which is iostat's coarseness too.
            Metric::Util => {
                let busy_ns = delta.io_ticks_ms * 1_000_000;
                let weighted_ns = delta.weighted_ms * 1_000_000;
                let share = |ns: u64| {
                    if interval_ns == 0 {
                        0.0
                    } else {
                        ns as f64 / interval_ns as f64
                    }
                };
                vec![
                    Attr::float("util", share(busy_ns).min(1.0)),
                    Attr::uint("busy_ns", busy_ns),
                    // The time every request spent outstanding, summed: over
                    // the window, the average queue `iostat` calls `aqu-sz`.
                    Attr::float("queue", share(weighted_ns)),
                ]
            }
            Metric::Inflight => return None,
        };
        attrs.shrink_to_fit();
        Some(attrs)
    }

    /// The same increase split by direction, for a request keyed on `dir`:
    /// each direction the kernel counts, and only those it counts -- a kernel
    /// that writes no discard column gives no `dir:discard`, not a zero.
    fn directions(self, delta: &DeviceCounters) -> Vec<(Direction, u64)> {
        let mut split = match self {
            Metric::Bytes => vec![
                (Direction::Read, delta.sectors_read * SECTOR_BYTES),
                (Direction::Write, delta.sectors_written * SECTOR_BYTES),
            ],
            Metric::Iops => vec![
                (Direction::Read, delta.reads),
                (Direction::Write, delta.writes),
            ],
            Metric::Merges => vec![
                (Direction::Read, delta.reads_merged),
                (Direction::Write, delta.writes_merged),
            ],
            Metric::Inflight | Metric::Util => return Vec::new(),
        };
        match self {
            Metric::Bytes => split.extend(
                delta
                    .sectors_discarded
                    .map(|sectors| (Direction::Discard, sectors * SECTOR_BYTES)),
            ),
            Metric::Iops => {
                split.extend(delta.discards.map(|ops| (Direction::Discard, ops)));
                split.extend(delta.flushes.map(|ops| (Direction::Flush, ops)));
            }
            _ => {}
        }
        split
    }

    fn source_id(self) -> &'static str {
        match self {
            Metric::Bytes => "io.bytes",
            Metric::Iops => "io.iops",
            Metric::Merges => "io.merges",
            Metric::Inflight => "io.inflight",
            Metric::Util => "io.util",
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
            Metric::Util => "util",
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
    by_direction: bool,
}

impl Prepare {
    /// Refuses a mode this backend cannot serve, with what it would take to
    /// serve it, rather than returning a number of a different kind.
    pub fn new(metric: Metric, mode: Mode) -> Result<Self, String> {
        let ready = Self {
            metric,
            mode,
            by_direction: false,
        };
        match (mode, metric.is_gauge()) {
            (Mode::Poll { .. }, _) => Ok(ready),
            (Mode::Count, false) => Ok(ready),
            (Mode::Snapshot, true) => Ok(ready),
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

    /// One event per device and direction rather than one per device, when
    /// the request is keyed on `dir`. Without it the events are what every
    /// reader of a device's series has always read.
    pub fn keyed(mut self, keys: &[KeyKind]) -> Self {
        self.by_direction = keys.contains(&KeyKind::Dir);
        self
    }
}

impl Prepared for Prepare {
    /// These counters are kept per device for the whole machine, and the block
    /// layer records nothing about who asked for the I/O. So a target that
    /// names a process is refused by name rather than answered with the
    /// machine's total, which would be attribution that was never measured.
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match target {
            Target::System => Ok(Box::new(
                Diskstats::new(self.metric, self.mode, PathBuf::from("/proc/diskstats"))
                    .by_direction(self.by_direction),
            )),
            other => Err(format!(
                "/proc/diskstats counts per device for the whole machine, and a {} target asks \
                 for something it does not record; use --system, or the eBPF backend once it \
                 lands",
                other.kind()
            )),
        }
    }
}

/// Reads one line per block device and keeps the ones that stand on their own.
pub struct Diskstats {
    metric: Metric,
    mode: Mode,
    path: PathBuf,
    /// Where each device's own `inflight` file is, for the gauge: see
    /// [`Diskstats::gauge_events`].
    sys_block: PathBuf,
    /// The devices whose `inflight` file could not be read, and why: their
    /// gauge is the diskstats column alone, which is the very case where a
    /// kernel like the CI's reads 0, so the run says so in its notes.
    unread_sysfs: BTreeMap<String, String>,
    /// What each device name turned out to be, cached: the classification
    /// reads `/sys`, and the answer only changes when hardware appears.
    roles: BTreeMap<String, Role>,
    /// The baseline and each interval's reading (`collect::poll`).
    poll: CounterPoll<DeviceId, DeviceCounters>,
    /// One event per direction, keyed `dir`, rather than one per device.
    by_direction: bool,
    losses: Losses,
}

impl Diskstats {
    pub fn new(metric: Metric, mode: Mode, path: PathBuf) -> Self {
        Self {
            metric,
            mode,
            path,
            sys_block: PathBuf::from("/sys/block"),
            unread_sysfs: BTreeMap::new(),
            roles: BTreeMap::new(),
            poll: CounterPoll::new(mode),
            by_direction: false,
            losses: Losses::default(),
        }
    }

    pub fn by_direction(mut self, by_direction: bool) -> Self {
        self.by_direction = by_direction;
        self
    }

    /// Reads the file and sums the devices whose counters are their own.
    fn read(&mut self) -> Result<Reading, SampleError> {
        let text = std::fs::read_to_string(&self.path)
            .map_err(|error| SampleError::Transient(format!("{}: {error}", self.path.display())))?;

        let mut devices: BTreeMap<DeviceId, DeviceCounters> = BTreeMap::new();
        let mut undecoded = Vec::new();
        for line in text.lines() {
            let Some((major, minor, name, numbers)) = split_line(line) else {
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
                    devices.insert(
                        DeviceId {
                            major,
                            minor,
                            name: name.to_owned(),
                        },
                        counters,
                    );
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
            self.losses.note_times(reason, 1);
        }

        Ok(Reading {
            ts_ns: monotonic_ns(),
            counters: devices,
        })
    }

    /// Builds one event per device for the increase between two readings.
    ///
    /// A device that appears mid-run has no earlier reading to measure against
    /// and contributes nothing to this interval rather than its counters since
    /// boot; one that disappears contributes nothing either, because the
    /// interval it would report has no end. Both used to void the whole
    /// interval for every device, on the grounds that the sum was no longer
    /// one machine's worth of I/O -- which was right about the sum and is not a
    /// reason to drop the devices that were there throughout.
    fn delta_events(
        &mut self,
        interval: Interval<DeviceId, DeviceCounters>,
        seq: u64,
    ) -> Vec<Event> {
        let interval_ns = interval.interval_ns;
        let mut events = Vec::new();
        let mut index = seq;
        for device in &interval.appeared {
            // Counted, not only noted. An interval of this device's I/O is
            // missing from the run, and a reason without a count leaves
            // `Losses::is_clean` true -- so the run would print `no loss`
            // while a device's first interval was quietly absent. The
            // category is the one the line below uses for the same shape of
            // gap: a reading the collector could not turn into an interval.
            self.losses.undecoded += 1;
            self.losses.note_times(
                format!(
                    "`{}` appeared during the run, so this interval has nothing to measure its \
                     counters against",
                    device.name
                ),
                1,
            );
        }
        for device in &interval.went_back {
            self.losses.undecoded += 1;
            self.losses.note_times(
                format!(
                    "a counter for `{}` in /proc/diskstats went backwards, which happens when a \
                     device is removed and re-added; this interval cannot be attributed",
                    device.name
                ),
                1,
            );
        }
        for (device, delta) in &interval.moved {
            if self.by_direction {
                for (direction, value) in self.metric.directions(delta) {
                    let mut attrs = vec![
                        Attr::uint(self.metric.value_attr(), value),
                        Attr::uint("interval_ns", interval_ns),
                        Attr::text("dev", device.name.clone()),
                    ];
                    if self.metric == Metric::Bytes {
                        attrs.push(Attr::uint("sector_bytes", SECTOR_BYTES));
                    }
                    events.push(Event {
                        ts_ns: interval.ts_ns,
                        ts_err_ns: 0,
                        kind: self.metric.source_id().to_owned(),
                        keys: vec![
                            KeyValue::Dev {
                                major: device.major,
                                minor: device.minor,
                            },
                            KeyValue::dir(direction),
                        ],
                        attrs,
                        collector_id: CollectorId::unstamped(),
                        seq: index,
                    });
                    index += 1;
                }
                continue;
            }
            let Some(mut attrs) = self.metric.attrs(delta, interval_ns) else {
                return Vec::new();
            };
            attrs.push(Attr::uint("interval_ns", interval_ns));
            attrs.push(Attr::text("dev", device.name.clone()));
            events.push(Event {
                ts_ns: interval.ts_ns,
                ts_err_ns: 0,
                kind: self.metric.source_id().to_owned(),
                keys: vec![KeyValue::Dev {
                    major: device.major,
                    minor: device.minor,
                }],
                attrs,
                collector_id: CollectorId::unstamped(),
                seq: index,
            });
            index += 1;
        }
        events
    }

    /// One event per device with what it held outstanding at the read.
    ///
    /// **The kernel keeps two accounts of that, and neither is the whole of it
    /// on every kernel.** `/proc/diskstats`' ninth column is the block
    /// layer's I/O accounting, counted from the moment a request is accounted,
    /// scheduler queue included. On the CI's 6.17 kernel the disk's line read
    /// 0 at every one of 101 polls while `/sys/block/nvme0n1/inflight` read up
    /// to 199 (`docs/scenarios/io.md`, defect 20). The kernel's source says
    /// why: from the per-cpu `in_flight` counters' first use for multi-queue
    /// devices until 7.2, `blk_account_io_start` counted a request on the
    /// partition it came through and never on the disk, whose line therefore
    /// read 0 under any queue a partition made. 7.2 counts it on both
    /// (upstream `5bdb8ec58b54`, "block: propagate in_flight to whole disk on
    /// partition I/O"). `/sys/block/<dev>/inflight`
    /// is the other account, which on a multi-queue device walks the
    /// requests the driver holds, whichever partition they came through, and
    /// leaves out the ones still in the scheduler. Each is a count of requests
    /// really outstanding, so the larger of the two is still never more than
    /// the device held; the gauge is that, and the event carries both, so a
    /// kernel where they part is visible in the output rather than averaged
    /// away.
    fn gauge_events(&mut self, reading: &Reading, seq: u64) -> Vec<Event> {
        reading
            .counters
            .iter()
            .enumerate()
            .map(|(offset, (device, counters))| {
                let path = self.sys_block.join(&device.name).join("inflight");
                let driver = match read_inflight(&path) {
                    Ok(driver) => Some(driver),
                    Err(reason) => {
                        self.unread_sysfs
                            .entry(device.name.clone())
                            .or_insert_with(|| format!("{}: {reason}", path.display()));
                        None
                    }
                };
                let mut attrs = vec![
                    Attr::uint("inflight", counters.in_flight.max(driver.unwrap_or(0))),
                    Attr::text("dev", device.name.clone()),
                    Attr::uint("diskstats_inflight", counters.in_flight),
                ];
                if let Some(driver) = driver {
                    attrs.push(Attr::uint("sysfs_inflight", driver));
                }
                Event {
                    ts_ns: reading.ts_ns,
                    ts_err_ns: 0,
                    kind: self.metric.source_id().to_owned(),
                    keys: vec![KeyValue::Dev {
                        major: device.major,
                        minor: device.minor,
                    }],
                    attrs,
                    collector_id: CollectorId::unstamped(),
                    seq: seq + offset as u64,
                }
            })
            .collect()
    }
}

impl Collector for Diskstats {
    fn value_attr(&self) -> &'static str {
        self.metric.value_attr()
    }

    /// Every run that measures an increase takes its baseline here, before
    /// the observed program is released, so what it does from its first
    /// instruction is in the first interval.
    ///
    /// A counted run measures its one total against it. A polled run measures
    /// its first interval against it, at its *second* tick: the runner takes
    /// the first as soon as the program is released, and an interval of a few
    /// microseconds would put a meaningless ratio at the head of every series.
    /// That tick produces no event and moves nothing, so the bytes between
    /// the release and it are in the next interval rather than lost.
    fn start(&mut self) -> Result<(), String> {
        if self.metric.is_gauge() {
            return Ok(());
        }
        let path = self.path.display().to_string();
        let mut poll = std::mem::replace(&mut self.poll, CounterPoll::new(self.mode));
        let started = poll.start(|| self.read());
        self.poll = poll;
        started.map_err(|error| format!("cannot read {path}: {error:?}"))
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if self.metric.is_gauge() {
            let reading = self.read()?;
            return Ok(self.gauge_events(&reading, seq));
        }
        // A counted run reports one total at the end, and a polled run's first
        // tick opens its window (`collect::poll`).
        let mut poll = std::mem::replace(&mut self.poll, CounterPoll::new(self.mode));
        let ticked = poll.tick(|| self.read());
        self.poll = poll;
        Ok(ticked?
            .map(|interval| self.delta_events(interval, seq))
            .unwrap_or_default())
    }

    /// A counted run reports its one total here, measured from the baseline
    /// taken before the observed program started; a polled run, nothing.
    fn stop(&mut self) -> Vec<Event> {
        let mut poll = std::mem::replace(&mut self.poll, CounterPoll::new(self.mode));
        let stopped = poll.stop(|| self.read());
        self.poll = poll;
        match stopped {
            Ok(Some(total)) => self.delta_events(total, u64::MAX / 2),
            Ok(None) => Vec::new(),
            Err(error) => {
                self.losses.failed_reads += 1;
                self.losses.note_times(
                    format!(
                        "the closing read failed ({error:?}), so the last interval of this run is \
                     missing"
                    ),
                    1,
                );
                Vec::new()
            }
        }
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    /// The devices whose queue depth is the diskstats column alone, because
    /// their `/sys/block/<dev>/inflight` could not be read at some poll.
    fn notes(&self) -> Vec<String> {
        self.unread_sysfs
            .iter()
            .map(|(device, reason)| {
                format!(
                    "{device}'s queue depth is /proc/diskstats' account alone at one poll or \
                     more, which can read 0 under a real queue: {reason}"
                )
            })
            .collect()
    }
}

/// What one reading of the file holds, per device.
///
/// The counters were summed into one total until it became clear that the sum
/// answers a question nobody asks. `/proc/diskstats` keeps them per device; the
/// catalog declares `dev` as both a key and a filter field on every source
/// here, and `io.bytes --emit topk` is the question "which device" -- which a
/// single machine-wide number cannot answer at all. Keeping them apart is also
/// what makes the sum available, since adding up series is something the emit
/// layer can do and splitting one apart is not.
type Reading = crate::collect::poll::Reading<DeviceId, DeviceCounters>;

/// A block device as `/proc/diskstats` identifies it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DeviceId {
    major: u32,
    minor: u32,
    name: String,
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

/// The reads and writes `/sys/block/<dev>/inflight` holds (`"       0      199"`),
/// added; the reason when the file is not there or not two numbers.
fn read_inflight(path: &Path) -> Result<u64, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let numbers: Vec<u64> = text
        .split_whitespace()
        .map(|field| field.parse::<u64>())
        .collect::<Result<_, _>>()
        .map_err(|_| format!("`{}` is not two counts", text.trim()))?;
    match numbers[..] {
        [reads, writes] => Ok(reads + writes),
        _ => Err(format!("`{}` is not two counts", text.trim())),
    }
}

/// Splits `  259  0 nvme0n1 88820 1663 ...` into the name and its counters.
fn split_line(line: &str) -> Option<(u32, u32, &str, Vec<u64>)> {
    let mut fields = line.split_whitespace();
    let major: u32 = fields.next()?.parse().ok()?;
    let minor: u32 = fields.next()?.parse().ok()?;
    let name = fields.next()?;
    let numbers = fields
        .map(|field| field.parse().ok())
        .collect::<Option<_>>()?;
    Some((major, minor, name, numbers))
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
    /// Milliseconds with at least one request outstanding, and milliseconds
    /// of every request's time outstanding summed (columns 10 and 11).
    io_ticks_ms: u64,
    weighted_ms: u64,
    /// Discards arrived in 4.18 and flushes in 5.5. `None` means this kernel
    /// does not report them.
    discards: Option<u64>,
    sectors_discarded: Option<u64>,
    flushes: Option<u64>,
}

impl Cumulative for DeviceCounters {
    fn since(&self, earlier: &Self) -> Option<Self> {
        DeviceCounters::since(self, earlier)
    }
}

impl DeviceCounters {
    fn bytes(&self) -> u64 {
        (self.sectors_read + self.sectors_written) * SECTOR_BYTES
    }

    /// The increase since an earlier reading of the same device, or `None` when
    /// a counter went backwards -- which happens when a device is removed and
    /// re-added under the same numbers, and makes the interval unmeasurable
    /// rather than zero.
    fn since(&self, before: &DeviceCounters) -> Option<DeviceCounters> {
        Some(DeviceCounters {
            reads: self.reads.checked_sub(before.reads)?,
            reads_merged: self.reads_merged.checked_sub(before.reads_merged)?,
            sectors_read: self.sectors_read.checked_sub(before.sectors_read)?,
            writes: self.writes.checked_sub(before.writes)?,
            writes_merged: self.writes_merged.checked_sub(before.writes_merged)?,
            sectors_written: self.sectors_written.checked_sub(before.sectors_written)?,
            // A gauge has no meaningful difference; the later value is kept so
            // the field says what it said.
            in_flight: self.in_flight,
            io_ticks_ms: self.io_ticks_ms.checked_sub(before.io_ticks_ms)?,
            weighted_ms: self.weighted_ms.checked_sub(before.weighted_ms)?,
            discards: difference(self.discards, before.discards),
            sectors_discarded: difference(self.sectors_discarded, before.sectors_discarded),
            flushes: difference(self.flushes, before.flushes),
        })
    }

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
            io_ticks_ms: numbers[9],
            weighted_ms: numbers[10],
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
            .filter_map(|line| split_line(line).map(|(_, _, name, _)| name.to_owned()))
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
        let (_, _, name, numbers) = split_line(NVME_LINE).expect("a diskstats line");
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
        let (_, _, _, numbers) = split_line(&line("sda", 1, 2, 3, 4)).expect("a line");
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
            let Some((_, _, name, _)) = split_line(line) else {
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
        assert_eq!(event.attr("dev").unwrap().to_string(), device);
        assert!(event.attr("interval_ns").unwrap().as_f64().unwrap() > 0.0);
        assert!(collector.losses().is_clean());
        std::fs::remove_file(&path).ok();
    }

    /// A polled run's baseline is taken at the start, before the program is
    /// released, and not at the first tick: what the device did between the
    /// two is in the first interval the run reports, not lost.
    #[test]
    fn a_polled_run_measures_from_its_start_and_not_from_its_first_tick() {
        let Some(device) = a_leaf_device() else {
            return;
        };
        let path = scratch("poll-baseline");
        write_stats(&path, &[line(&device, 10, 100, 20, 200)]);
        let mut collector = Diskstats::new(Metric::Bytes, poll(), path.clone());
        collector.start().expect("the fake file reads");
        // The program's first I/O, between its release and the first tick.
        write_stats(&path, &[line(&device, 11, 104, 21, 204)]);
        assert!(
            collector.sample(0).expect("the first tick").is_empty(),
            "the first tick, microseconds after the release, is no interval"
        );
        write_stats(&path, &[line(&device, 12, 108, 23, 216)]);
        let events = collector.sample(1).expect("the second tick");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].attr("bytes").unwrap().as_f64(),
            Some(24.0 * 512.0),
            "the whole increase since the start, the release's included"
        );
        std::fs::remove_file(&path).ok();
    }

    /// A baseline older than an interval at the first tick was taken while
    /// the run was still attaching, and what the device did since is from
    /// before the window: the first interval is measured from the first tick
    /// instead (top, finding 2: 833 MB at offset zero of a live `omn top`).
    #[test]
    fn a_baseline_older_than_an_interval_is_retaken_at_the_window_s_opening() {
        let Some(device) = a_leaf_device() else {
            return;
        };
        let path = scratch("stale-baseline");
        write_stats(&path, &[line(&device, 10, 100, 20, 200)]);
        let mut collector = Diskstats::new(
            Metric::Bytes,
            Mode::Poll {
                interval: "10ms".parse().unwrap(),
            },
            path.clone(),
        );
        collector.start().expect("the fake file reads");
        // Startup: other collectors attaching, the device busy meanwhile.
        write_stats(&path, &[line(&device, 1000, 100_000, 2000, 200_000)]);
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(collector.sample(0).expect("the first tick").is_empty());
        write_stats(&path, &[line(&device, 1001, 100_004, 2001, 200_004)]);
        let events = collector.sample(1).expect("the second tick");
        assert_eq!(
            events[0].attr("bytes").unwrap().as_f64(),
            Some(8.0 * 512.0),
            "only what the device did inside the window"
        );
        std::fs::remove_file(&path).ok();
    }

    /// Keyed on `dir`, a device's increase is one event per direction, each
    /// carrying that direction's share under the value attribute, and the
    /// directions add up to the device's total.
    #[test]
    fn a_request_keyed_on_direction_gets_one_series_per_direction() {
        let Some(device) = a_leaf_device() else {
            return;
        };
        let path = scratch("by-direction");
        write_stats(&path, &[line(&device, 10, 100, 20, 200)]);
        let mut collector = Diskstats::new(Metric::Bytes, poll(), path.clone()).by_direction(true);
        assert!(collector.sample(0).expect("the baseline").is_empty());
        write_stats(&path, &[line(&device, 12, 108, 23, 216)]);
        let events = collector.sample(1).expect("a second read");
        let split: Vec<(String, f64)> = events
            .iter()
            .filter(|event| event.keys[1] != KeyValue::dir(Direction::Discard))
            .map(|event| {
                assert!(matches!(event.keys[0], KeyValue::Dev { .. }));
                (
                    event.keys[1].to_string(),
                    event.attr("bytes").unwrap().as_f64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            split,
            [
                ("dir:read".to_owned(), 8.0 * 512.0),
                ("dir:write".to_owned(), 16.0 * 512.0)
            ]
        );
        let series: std::collections::BTreeSet<String> = events
            .iter()
            .map(|event| crate::emit::SeriesKey::by(event, &[KeyKind::Dir]).to_string())
            .collect();
        assert!(series.contains("dir:read") && series.contains("dir:write"));
        assert!(
            Prepare::new(Metric::Bytes, poll())
                .unwrap()
                .keyed(&[KeyKind::Dev, KeyKind::Dir])
                .by_direction
        );
        assert!(
            !Prepare::new(Metric::Bytes, poll())
                .unwrap()
                .keyed(&[KeyKind::Dev])
                .by_direction
        );
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

    /// `%util` is the io-ticks column's increase over the window, as `iostat
    /// -x` computes it, and `queue` the weighted column's: `aqu-sz`.
    #[test]
    fn utilization_is_the_busy_time_over_the_window() {
        let Some(device) = a_leaf_device() else {
            return;
        };
        let path = scratch("util");
        let busy = |ticks: u64, weighted: u64| {
            format!("   8       0 {device} 1 0 8 0 1 0 8 0 0 {ticks} {weighted}")
        };
        write_stats(&path, &[busy(1_000, 5_000)]);
        let mut collector = Diskstats::new(Metric::Util, Mode::Count, path.clone());
        collector.start().expect("the fake file reads");
        let opened = std::time::Instant::now();
        // A quarter of a second of busy time over at least as long a window,
        // and four requests outstanding on average over that quarter.
        write_stats(&path, &[busy(1_250, 6_000)]);
        std::thread::sleep(std::time::Duration::from_millis(500));
        let events = collector.stop();
        let window = opened.elapsed().as_nanos() as f64;
        assert_eq!(events.len(), 1);
        let event = &events[0];
        let interval = event.attr("interval_ns").unwrap().as_f64().unwrap();
        assert!(interval >= 500e6 && interval <= window + 50e6, "{interval}");
        assert_eq!(event.attr("busy_ns").unwrap().as_f64(), Some(250e6));
        let util = event.attr("util").unwrap().as_f64().unwrap();
        assert!((util - 250e6 / interval).abs() < 1e-9, "{util}");
        let queue = event.attr("queue").unwrap().as_f64().unwrap();
        assert!((queue - 1_000e6 / interval).abs() < 1e-9, "{queue}");
        assert!(Prepare::new(Metric::Util, Mode::Snapshot).is_err());
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

    /// A device that appears mid-run costs that device's first interval and
    /// nothing else.
    ///
    /// This used to void the whole interval, for every device, on the grounds
    /// that the difference between two sets was part I/O and part hotplug.
    /// That was right about the *sum*, which no longer exists: the counters are
    /// now reported per device, so a device that was there throughout has a
    /// measurable interval whatever appeared beside it. Dropping it too would
    /// lose real I/O to an event that did not involve it.
    #[test]
    fn a_device_appearing_mid_run_costs_only_its_own_first_interval() {
        let Some(device) = a_leaf_device() else {
            return;
        };
        let Some(second) = std::fs::read_to_string("/proc/diskstats")
            .ok()
            .and_then(|text| {
                text.lines()
                    .filter_map(|line| split_line(line).map(|(_, _, name, _)| name.to_owned()))
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

        let events = collector.sample(1).expect("a read");
        assert_eq!(
            events.len(),
            1,
            "the device that was there throughout is still measurable"
        );
        assert_eq!(
            events[0].attr("dev").map(ToString::to_string),
            Some(device.clone())
        );
        // 8 sectors read and 16 written, at 512 bytes each.
        assert_eq!(
            events[0].attr("bytes").map(ToString::to_string),
            Some(((8 + 16) * SECTOR_BYTES).to_string())
        );
        assert!(
            collector.losses().reasons.keys().any(
                |reason| reason.contains(&second) && reason.contains("appeared during the run")
            ),
            "the device that could not be measured is named: {:?}",
            collector.losses().reasons
        );
        assert_eq!(
            collector.losses().undecoded,
            1,
            "a named reason with nothing counted behind it leaves the run calling itself \
             complete: {:?}",
            collector.losses()
        );
        assert!(
            !collector.losses().is_clean(),
            "an interval of this device's I/O is missing, so the run is not complete"
        );
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
        collector.sys_block = path.join("no-sysfs-here");
        collector.start().expect("the fake file reads");
        let events = collector.sample(0).expect("a read");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].attr("inflight").unwrap().as_f64(), Some(3.0));
        assert!(
            events[0].attr("sysfs_inflight").is_none(),
            "a sysfs file that is not there is absent, not zero"
        );
        let notes = collector.notes();
        assert!(
            notes.len() == 1
                && notes[0].contains(&device)
                && notes[0].contains("/proc/diskstats' account alone"),
            "a gauge read from one account alone says so, and of which device: {notes:?}"
        );
        std::fs::remove_file(&path).ok();
    }

    /// The CI's shape: the disk's diskstats line holding nothing while the
    /// driver held 199 requests that came through a partition. The gauge is
    /// the larger account, both ways round, and carries each.
    #[test]
    fn a_queue_one_account_misses_is_read_from_the_other() {
        let Some(device) = a_leaf_device() else {
            eprintln!(
                "skipped: no device in /proc/diskstats is a leaf, so none is classified as one"
            );
            return;
        };
        let sys_block = scratch("sys-block");
        std::fs::remove_file(&sys_block).expect("replacing the file with a directory");
        std::fs::create_dir_all(sys_block.join(&device)).unwrap();
        let path = scratch("inflight-two-accounts");
        let depth = |diskstats: u64, sysfs: &str| {
            write_stats(
                &path,
                &[format!(
                    "   8       0 {device} 1 0 2 0 3 0 4 0 {diskstats} 0 0"
                )],
            );
            std::fs::write(sys_block.join(&device).join("inflight"), sysfs).unwrap();
            let mut collector = Diskstats::new(Metric::Inflight, Mode::Snapshot, path.clone());
            collector.sys_block = sys_block.clone();
            let events = collector.sample(0).expect("a read");
            assert_eq!(events.len(), 1);
            let value = |name: &str| events[0].attr(name).and_then(|attr| attr.as_f64());
            (
                value("inflight"),
                value("diskstats_inflight"),
                value("sysfs_inflight"),
            )
        };
        assert_eq!(
            depth(0, "       0      199\n"),
            (Some(199.0), Some(0.0), Some(199.0))
        );
        assert_eq!(
            depth(12, "       2        3\n"),
            (Some(12.0), Some(12.0), Some(5.0))
        );
        // A file that is not two numbers is no reading at all.
        assert_eq!(depth(4, "garbage\n"), (Some(4.0), Some(4.0), None));
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir_all(&sys_block).ok();
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

        // One event per device now, and the reference below counts every
        // device that stands on its own, so the two sides are summed the same
        // way. Which device the file landed on is not the subject.
        assert!(!events.is_empty(), "one interval, at least one device");
        let written: f64 = events
            .iter()
            .filter_map(|event| event.attr("write_bytes")?.as_f64())
            .sum();
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

    /// Merges are counted per direction, because a read merge and a write merge
    /// say different things: one is sequential reading being coalesced, the
    /// other is a writeback pattern the filesystem produced.
    #[test]
    fn merges_are_reported_per_direction_and_summed() {
        let Some(device) = a_leaf_device() else {
            return;
        };
        let path = scratch("merges");
        // `line` writes zeroes into the merge columns, so they are set here.
        let stats = |reads_merged: u64, writes_merged: u64| {
            vec![format!(
                "   8       0 {device} 10 {reads_merged} 100 0 20 {writes_merged} 200 0 0 0 0"
            )]
        };
        write_stats(&path, &stats(5, 7));

        let mut collector = Diskstats::new(Metric::Merges, Mode::Count, path.clone());
        collector.start().expect("the fake file reads");
        write_stats(&path, &stats(9, 30));
        let events = collector.stop();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].attr("read_merges").unwrap().as_f64(), Some(4.0));
        assert_eq!(events[0].attr("write_merges").unwrap().as_f64(), Some(23.0));
        assert_eq!(events[0].attr("merges").unwrap().as_f64(), Some(27.0));
        std::fs::remove_file(&path).ok();
    }

    /// The runner samples every collector and only then decides to stop, so a
    /// closing read would cover microseconds. A polled run therefore ends
    /// without adding a point that says nothing.
    #[test]
    fn a_polled_run_adds_no_point_at_its_stop() {
        let Some(device) = a_leaf_device() else {
            return;
        };
        let path = scratch("stop");
        write_stats(&path, &[line(&device, 10, 100, 20, 200)]);

        let mut collector = Diskstats::new(Metric::Bytes, poll(), path.clone());
        collector.start().expect("the fake file reads");
        collector.sample(0).expect("the baseline read");
        write_stats(&path, &[line(&device, 12, 108, 23, 216)]);
        assert_eq!(collector.sample(1).expect("a tick").len(), 1);

        assert!(
            collector.stop().is_empty(),
            "a polled run has already measured everything it can"
        );
        std::fs::remove_file(&path).ok();
    }

    /// The block layer records no process, so a target that names one is
    /// refused by name rather than answered with the machine's total.
    #[test]
    fn a_target_that_names_a_process_is_refused_rather_than_answered() {
        let prepared = Box::new(Prepare::new(Metric::Bytes, poll()).expect("poll is servable"));
        let refused = prepared
            .attach(&Target::Pid(vec![1]))
            .err()
            .expect("a pid has no diskstats");
        assert!(refused.contains("per device"), "{refused}");
        assert!(refused.contains("--system"), "{refused}");

        let prepared = Box::new(Prepare::new(Metric::Bytes, poll()).expect("poll is servable"));
        assert!(prepared.attach(&Target::System).is_ok());
    }

    /// The devices an event says it summed must all be devices whose counters
    /// are their own. A partition in that list would mean its bytes were
    /// counted twice.
    #[test]
    fn the_devices_an_event_names_are_all_devices_that_stand_alone() {
        let mut collector = Diskstats::new(
            Metric::Bytes,
            Mode::Snapshot,
            PathBuf::from("/proc/diskstats"),
        );
        // Snapshot is refused for a counter, so the reading is taken directly:
        // what is under test is which devices the sum covers.
        let Ok(reading) = collector.read() else {
            return;
        };
        assert!(
            !reading.counters.is_empty(),
            "this machine has block devices"
        );
        for device in reading.counters.keys() {
            let name = &device.name;
            let sysfs = Path::new("/sys/class/block").join(name);
            assert!(
                !sysfs.join("partition").exists(),
                "`{name}` is a partition and its I/O is counted again on its disk"
            );
            let stacked = std::fs::read_dir(sysfs.join("slaves"))
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false);
            assert!(
                !stacked,
                "`{name}` is stacked on other devices and its I/O is counted again below it"
            );
        }
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
            let Some((_, _, name, numbers)) = split_line(line) else {
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
