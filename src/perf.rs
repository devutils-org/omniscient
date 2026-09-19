//! `perf_event_open` and its ring buffer.
//!
//! This is the syscall, not the `perf` tool: a request is an attribute struct
//! and a file descriptor, and the events arrive in a shared memory ring the
//! kernel writes and this process reads. Shelling out to `perf record` would
//! make the tool a scraper of someone else's output format and would put a
//! second clock domain in the middle of the pipeline.
//!
//! Two shapes live here. A *sampled* event hands back one record per
//! occurrence through the ring; a *counted* event hands back one number per
//! read and needs no ring at all. The second is not a cheaper version of the
//! first: counting is what makes `cpu.instructions` exact to a fraction of a
//! percent, and it is the only shape that can follow the threads and children a
//! program starts, because the kernel refuses to map a ring buffer for an
//! inherited per-task event.
//!
//! Two things here are not incidental. The clock is pinned explicitly with
//! `use_clockid` and `CLOCK_MONOTONIC`, so a sample and an event read from
//! procfs sort into the same stream. And the ring reports its own overflows
//! through `PERF_RECORD_LOST`, which is counted and surfaced: a sampler that
//! silently drops a third of its samples still produces a plausible-looking
//! profile.

use std::io;
use std::os::fd::RawFd;

use perf_event_open_sys as sys;

/// What a sampling event is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// One process, on every cpu.
    Process(i32),
    /// Every process, on one cpu. Needs privileges on most machines.
    Cpu(u32),
}

/// One record read out of the ring.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Sample(Sample),
    /// The kernel had nowhere to put `count` events. They are gone; the only
    /// honest thing left to do is say how many.
    Lost {
        count: u64,
    },
    /// The kernel throttled the event because it was costing too much.
    Throttle {
        throttled: bool,
    },
    /// A record this reader does not decode. Counted, never silently dropped.
    Other {
        kind: u32,
    },
}

/// A sample, with the fields this tool asks for.
///
/// Not `Copy`: a sample taken with a callchain owns the frames the kernel wrote
/// for it. A sample taken without one carries `None` and costs no allocation,
/// which is what keeps the fault sampler as cheap as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// Instruction pointer at the time of the event.
    pub ip: u64,
    pub pid: u32,
    pub tid: u32,
    /// `CLOCK_MONOTONIC`, because the attribute asked for it.
    pub time_ns: u64,
    /// The address the event is about. For a page fault, the address that
    /// faulted, which is what makes an interval join possible.
    pub addr: u64,
    /// Where the sampled instruction itself was running.
    ///
    /// The kernel puts this in the record header, and it is what the frames
    /// before the first context marker belong to. Without it a callchain that
    /// begins in the kernel and a callchain that begins in userspace are
    /// indistinguishable, and every kernel frame would be filed as user code.
    pub cpu_mode: CpuMode,
    /// The callchain exactly as the kernel wrote it, context markers included.
    ///
    /// `None` when the event did not ask for one, which is not the same as
    /// `Some(empty)`: the second means the unwinder was asked and came back
    /// with nothing. [`crate::stack`] is what turns this into frames.
    pub callchain: Option<Vec<u64>>,
}

/// Which privilege level the sampled instruction was running at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CpuMode {
    Kernel,
    User,
    /// Anything else the header reported: hypervisor, guest, or a mode this
    /// build does not know. Named rather than folded into `User`, because
    /// guessing here misattributes a whole stack.
    #[default]
    Unknown,
}

impl CpuMode {
    fn from_misc(misc: u16) -> Self {
        match u32::from(misc) & sys::bindings::PERF_RECORD_MISC_CPUMODE_MASK {
            sys::bindings::PERF_RECORD_MISC_KERNEL => CpuMode::Kernel,
            sys::bindings::PERF_RECORD_MISC_USER => CpuMode::User,
            _ => CpuMode::Unknown,
        }
    }
}

/// Whether the kernel is asked to walk the stack at every sample.
///
/// This is a request to the kernel, not a post-processing choice: the walk
/// happens in the interrupt handler, with the sampled task's registers still
/// live, and nothing afterwards can recover a stack that was not taken then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Callchain {
    /// No stack. A sample is an instruction pointer and nothing else.
    #[default]
    Off,
    /// The kernel walks frame pointers, in kernel mode and then in the target's
    /// own mode, and writes both chains with a marker between them.
    ///
    /// Cheap -- a few dozen loads per sample -- and worth exactly what the
    /// target's build is worth: a program compiled without frame pointers has
    /// no chain to walk and yields its leaf frame alone.
    FramePointers,
}

/// The sample fields this reader requests, in the order the kernel writes them.
const SAMPLE_TYPE: u64 = (sys::bindings::PERF_SAMPLE_IP
    | sys::bindings::PERF_SAMPLE_TID
    | sys::bindings::PERF_SAMPLE_TIME
    | sys::bindings::PERF_SAMPLE_ADDR) as u64;

/// The fixed fields above, counted in eight-byte words: `ip`, `pid`+`tid`,
/// `time`, `addr`. The callchain's length follows them.
const FIXED_FIELDS: usize = 4;

/// What the kernel will let a callchain be, in frames.
///
/// Read rather than assumed: `sample_max_stack` above the machine's own
/// `perf_event_max_stack` is refused with `EINVAL`, and a depth limit invented
/// here would make the open fail on exactly the machines that restrict it.
fn max_stack_depth() -> u16 {
    read_trimmed("/proc/sys/kernel/perf_event_max_stack")
        .and_then(|text| text.parse().ok())
        .unwrap_or(127)
}

/// The smallest ring worth trying. Below this, a burst of faults would be
/// mostly loss, and a mostly-lost measurement is worse than a refusal.
const MIN_RING_PAGES: usize = 8;

/// Whether an error means "that ring is bigger than you are allowed to map".
fn is_ring_too_large(error: &io::Error) -> bool {
    let text = error.to_string();
    text.contains("mmap")
        && (text.contains("Operation not permitted") || text.contains("Cannot allocate memory"))
}

/// A software event sampled into a ring buffer.
pub struct SampledEvent {
    fd: RawFd,
    base: *mut u8,
    map_len: usize,
    data_size: u64,
    /// Ring overflows, accumulated across drains.
    lost: u64,
    undecoded: u64,
    /// What was asked for at open time, because it decides the record layout:
    /// a decoder that reads a callchain the kernel was never asked to write
    /// reads whatever follows the record instead.
    callchain: Callchain,
}

// The mapping belongs to this struct alone and is only read through `&mut self`.
unsafe impl Send for SampledEvent {}

impl SampledEvent {
    /// Opens a software event, shrinking the ring until the machine allows it.
    ///
    /// `perf_event_mlock_kb` caps what an unprivileged process may map, and the
    /// cap is small by default. Halving down to a floor gets a working ring on
    /// a stock machine; the size that was accepted is reported, because a
    /// smaller ring means a higher chance of dropping samples under a burst.
    pub fn open_software(
        config: u64,
        period: u64,
        scope: Scope,
        ring_pages: usize,
    ) -> io::Result<Self> {
        Self::open_software_with(config, period, scope, ring_pages, Callchain::Off)
    }

    /// The same, asking the kernel for a callchain with every sample.
    ///
    /// Separate from [`SampledEvent::open_software`] rather than a default
    /// argument, because a callchain is not free: every sample grows by a word
    /// per frame, so a ring that held a burst of bare samples holds a fraction
    /// of that many once stacks are on. A caller that does not want stacks must
    /// not pay for them by accident.
    pub fn open_software_with(
        config: u64,
        period: u64,
        scope: Scope,
        ring_pages: usize,
        callchain: Callchain,
    ) -> io::Result<Self> {
        let mut pages = ring_pages.next_power_of_two();
        loop {
            match Self::open_with_ring(config, period, scope, pages, callchain) {
                Ok(event) => return Ok(event),
                Err(error) if pages > MIN_RING_PAGES && is_ring_too_large(&error) => {
                    pages /= 2;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Whether this machine will hand this user a callchain with a sample.
    ///
    /// Answered by opening the event a profile opens and throwing it away,
    /// because every cheaper answer is a guess: the sysctl, the capability and
    /// the kernel's build options all have a veto, and only the kernel holds
    /// all three. The event is over the calling process, so the probe measures
    /// nothing and disturbs nothing.
    pub fn callchain_supported() -> io::Result<()> {
        Self::open_software_with(
            sys::bindings::PERF_COUNT_SW_CPU_CLOCK as u64,
            // Far longer than the probe lives: it is never enabled, and a
            // period this long would not fire if it were.
            u64::from(u32::MAX),
            Scope::Process(0),
            MIN_RING_PAGES,
            Callchain::FramePointers,
        )
        .map(drop)
    }

    /// Whether this event asks the kernel to unwind.
    pub fn callchain(&self) -> Callchain {
        self.callchain
    }

    /// The data pages this event's ring actually got.
    pub fn ring_pages(&self) -> usize {
        (self.data_size as usize) / page_size()
    }

    /// What this event does and does not see, in one line, for the record that
    /// travels with the numbers.
    pub fn sampled_threads(&self) -> &'static str {
        "the target task only: threads it starts afterwards are not sampled"
    }

    fn open_with_ring(
        config: u64,
        period: u64,
        scope: Scope,
        ring_pages: usize,
        callchain: Callchain,
    ) -> io::Result<Self> {
        assert!(
            ring_pages.is_power_of_two(),
            "the ring must be a power of two pages"
        );

        let mut attr: sys::bindings::perf_event_attr = unsafe { std::mem::zeroed() };
        attr.type_ = sys::bindings::PERF_TYPE_SOFTWARE;
        attr.size = std::mem::size_of::<sys::bindings::perf_event_attr>() as u32;
        attr.config = config;
        attr.__bindgen_anon_1.sample_period = period;
        attr.sample_type = SAMPLE_TYPE;
        if callchain == Callchain::FramePointers {
            attr.sample_type |= sys::bindings::PERF_SAMPLE_CALLCHAIN as u64;
            // Both halves are asked for. A kernel chain without a user chain
            // says a task is in `read`, and never which of its own functions
            // called it; a user chain without a kernel one cannot tell work
            // from waiting. `crate::stack` keeps them apart afterwards.
            attr.sample_max_stack = max_stack_depth();
        }
        attr.__bindgen_anon_2.wakeup_events = 1;
        // Start stopped: the caller decides when the window opens, which is how
        // a launched program can be sampled from its first instruction.
        attr.set_disabled(1);
        // One clock domain for the whole tool. Without this the samples would
        // be on the kernel's default clock and every join against them would be
        // off by an amount nobody would notice.
        attr.set_use_clockid(1);
        attr.clockid = libc::CLOCK_MONOTONIC;
        // `inherit` is deliberately left off. The kernel refuses to map a ring
        // buffer for an inherited per-task event (`perf_mmap` returns EINVAL
        // when `cpu == -1 && attr.inherit`), because each inheriting task would
        // need a buffer of its own. Following every thread means per-cpu events,
        // which need privileges most machines do not grant. So this samples the
        // task it was given, and `sampled_threads` below states the limit rather
        // than leaving it to be discovered in a missing number.

        let (pid, cpu) = match scope {
            Scope::Process(pid) => (pid, -1),
            Scope::Cpu(cpu) => (-1, cpu as i32),
        };
        let fd = unsafe {
            sys::perf_event_open(
                &mut attr,
                pid,
                cpu,
                -1,
                sys::bindings::PERF_FLAG_FD_CLOEXEC as u64,
            )
        };
        if fd < 0 {
            return Err(io::Error::other(format!(
                "perf_event_open: {}",
                io::Error::last_os_error()
            )));
        }

        let page = page_size();
        let map_len = (ring_pages + 1) * page;
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            let error = io::Error::other(format!(
                "mmap of {ring_pages} ring pages: {}",
                io::Error::last_os_error()
            ));
            unsafe { libc::close(fd) };
            return Err(error);
        }

        Ok(Self {
            fd,
            base: base as *mut u8,
            map_len,
            data_size: (ring_pages * page) as u64,
            lost: 0,
            undecoded: 0,
            callchain,
        })
    }

    pub fn enable(&self) -> io::Result<()> {
        self.ioctl(unsafe { sys::ioctls::ENABLE(self.fd, 0) })
    }

    pub fn disable(&self) -> io::Result<()> {
        self.ioctl(unsafe { sys::ioctls::DISABLE(self.fd, 0) })
    }

    fn ioctl(&self, result: i32) -> io::Result<()> {
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Events the ring had to throw away since this event was opened.
    pub fn lost(&self) -> u64 {
        self.lost
    }

    /// Records this reader did not know how to decode.
    pub fn undecoded(&self) -> u64 {
        self.undecoded
    }

    /// Reads everything the kernel has published, calling `handle` per record.
    ///
    /// The tail is only published back to the kernel once the batch has been
    /// copied out, so a record is never overwritten while it is being read.
    pub fn drain(&mut self, mut handle: impl FnMut(Record)) {
        let header = self.base as *mut sys::bindings::perf_event_mmap_page;
        // Acquire: everything the kernel wrote before publishing this head must
        // be visible before it is read.
        let head = unsafe { std::ptr::read_volatile(&(*header).data_head) };
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        let mut tail = unsafe { std::ptr::read_volatile(&(*header).data_tail) };

        while tail < head {
            let (kind, misc, size) = self.read_header(tail);
            if size == 0 {
                // A zero-length record would loop forever; treat the rest of
                // the batch as unreadable rather than spin.
                self.undecoded += 1;
                break;
            }
            let record = self.decode(kind, misc, tail, size);
            match &record {
                Record::Lost { count } => self.lost += count,
                Record::Other { .. } => self.undecoded += 1,
                _ => {}
            }
            handle(record);
            tail += size;
        }

        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        unsafe { std::ptr::write_volatile(&mut (*header).data_tail, tail) };
    }

    fn data(&self) -> *const u8 {
        unsafe { self.base.add(page_size()) }
    }

    /// Copies `len` bytes starting at ring offset `offset`, following the wrap.
    fn copy_out(&self, offset: u64, len: usize) -> Vec<u8> {
        // SAFETY: the mapping is `data_size` bytes long and `read_wrapped`
        // never reads past it; the kernel writes there and this process only
        // reads.
        let ring = unsafe { std::slice::from_raw_parts(self.data(), self.data_size as usize) };
        read_wrapped(ring, offset, len)
    }

    /// The record's type, its `misc` bits and its length.
    ///
    /// `misc` is read here rather than ignored because it is the only place the
    /// kernel says which mode the sampled instruction was in, and a callchain
    /// cannot be split into its kernel and user halves without knowing where it
    /// started.
    fn read_header(&self, offset: u64) -> (u32, u16, u64) {
        let bytes = self.copy_out(offset, 8);
        let kind = u32::from_ne_bytes(bytes[0..4].try_into().expect("4 bytes"));
        let misc = u16::from_ne_bytes(bytes[4..6].try_into().expect("2 bytes"));
        let size = u16::from_ne_bytes(bytes[6..8].try_into().expect("2 bytes")) as u64;
        (kind, misc, size)
    }

    fn decode(&self, kind: u32, misc: u16, offset: u64, size: u64) -> Record {
        let body = self.copy_out(offset, size as usize);
        let field = |index: usize| -> u64 {
            let start = 8 + index * 8;
            body.get(start..start + 8)
                .map(|bytes| u64::from_ne_bytes(bytes.try_into().expect("8 bytes")))
                .unwrap_or(0)
        };
        match kind {
            sys::bindings::PERF_RECORD_SAMPLE => {
                let tid_field = field(1);
                Record::Sample(Sample {
                    ip: field(0),
                    pid: (tid_field & 0xffff_ffff) as u32,
                    tid: (tid_field >> 32) as u32,
                    time_ns: field(2),
                    addr: field(3),
                    cpu_mode: CpuMode::from_misc(misc),
                    callchain: match self.callchain {
                        Callchain::Off => None,
                        Callchain::FramePointers => Some(callchain_of(&body)),
                    },
                })
            }
            sys::bindings::PERF_RECORD_LOST => Record::Lost { count: field(1) },
            sys::bindings::PERF_RECORD_THROTTLE => Record::Throttle { throttled: true },
            sys::bindings::PERF_RECORD_UNTHROTTLE => Record::Throttle { throttled: false },
            other => Record::Other { kind: other },
        }
    }
}

impl Drop for SampledEvent {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.map_len);
            libc::close(self.fd);
        }
    }
}

/// The callchain of a sample record whose event asked for one.
///
/// It follows the fixed fields as a count and then that many addresses. The
/// count is the kernel's, but the bound checked here is the record's own
/// length: a decoder that trusted the count would walk off the end of a
/// truncated record and hand back whatever the ring held next as frames.
///
/// Split out as a free function because, like the seam, it is a piece of this
/// module whose correctness does not need a kernel to check.
fn callchain_of(body: &[u8]) -> Vec<u64> {
    let word = |index: usize| -> Option<u64> {
        let start = 8 + index * 8;
        body.get(start..start + 8)
            .map(|bytes| u64::from_ne_bytes(bytes.try_into().expect("8 bytes")))
    };
    let Some(count) = word(FIXED_FIELDS) else {
        return Vec::new();
    };
    let available = body.len().saturating_sub(8 + (FIXED_FIELDS + 1) * 8) / 8;
    let depth = (count as usize).min(available);
    (0..depth)
        .filter_map(|index| word(FIXED_FIELDS + 1 + index))
        .collect()
}

/// Copies `len` bytes from ring offset `offset`, following the wrap.
///
/// A ring buffer has no end, only a seam, and a record that straddles it is
/// perfectly ordinary. Split out as a free function because that seam is the
/// one piece of this module whose correctness does not need a kernel to check.
fn read_wrapped(ring: &[u8], offset: u64, len: usize) -> Vec<u8> {
    if ring.is_empty() {
        return vec![0; len];
    }
    let size = ring.len();
    let start = (offset % size as u64) as usize;
    let mut bytes = Vec::with_capacity(len);
    let first = len.min(size - start);
    bytes.extend_from_slice(&ring[start..start + first]);
    let mut remaining = len - first;
    // A read longer than the ring itself would wrap more than once. It cannot
    // happen for a record, but going around rather than panicking keeps the
    // seam logic total.
    while remaining > 0 {
        let take = remaining.min(size);
        bytes.extend_from_slice(&ring[..take]);
        remaining -= take;
    }
    bytes
}

fn page_size() -> usize {
    // SAFETY: `sysconf` takes an integer and returns one.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as usize
    } else {
        4096
    }
}

/// Whether this machine lets an unprivileged process sample its own page
/// faults, and why not when it does not.
pub fn self_sampling_error() -> Option<String> {
    match SampledEvent::open_software(
        sys::bindings::PERF_COUNT_SW_PAGE_FAULTS as u64,
        1,
        Scope::Process(0),
        4,
    ) {
        Ok(_) => None,
        Err(error) => Some(format!(
            "perf_event_open was refused ({error}); check /proc/sys/kernel/perf_event_paranoid"
        )),
    }
}

/// A counter the kernel knows by type and config.
///
/// The name travels with the counter because it is what a reading is labelled
/// with, and because a counter that could not be opened has to be able to say
/// which one it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counter {
    pub name: &'static str,
    kind: u32,
    config: u64,
}

impl Counter {
    pub const fn hardware(name: &'static str, config: u32) -> Self {
        Self {
            name,
            kind: sys::bindings::PERF_TYPE_HARDWARE,
            config: config as u64,
        }
    }

    pub const fn software(name: &'static str, config: u32) -> Self {
        Self {
            name,
            kind: sys::bindings::PERF_TYPE_SOFTWARE,
            config: config as u64,
        }
    }

    /// Builds a counter from a performance unit the kernel describes itself.
    ///
    /// Everything beyond the architectural events -- RAPL's energy domains, the
    /// uncore's memory controllers, the `msr` unit's `aperf` and `mperf` --
    /// publishes its own type number and the config of each event it offers
    /// under `/sys/bus/event_source/devices`. Reading that is how a counter for
    /// them gets built without a table of magic numbers that would be wrong on
    /// the next machine.
    pub fn from_sysfs(name: &'static str, pmu: &str, event: &str) -> Option<Self> {
        let (kind, config) = sysfs_event(pmu, event)?;
        Some(Self { name, kind, config })
    }

    pub fn kind(&self) -> u32 {
        self.kind
    }

    pub fn config(&self) -> u64 {
        self.config
    }
}

/// The directory the kernel describes its performance units in.
pub const PMU_DEVICES: &str = "/sys/bus/event_source/devices";

/// The type and config of an event a performance unit publishes.
///
/// Only the `event=` and `umask=` terms are read. The rest of a config's layout
/// lives in the unit's `format` directory and is left alone rather than
/// guessed at: a config built wrong makes the open fail, so the error is always
/// in the direction of reporting something unavailable that works, never the
/// reverse.
pub fn sysfs_event(pmu: &str, event: &str) -> Option<(u32, u64)> {
    let kind: u32 = read_trimmed(&format!("{PMU_DEVICES}/{pmu}/type"))?
        .parse()
        .ok()?;
    let terms = read_trimmed(&format!("{PMU_DEVICES}/{pmu}/events/{event}"))?;

    let mut config = 0u64;
    for term in terms.split(',') {
        let Some((key, value)) = term.split_once('=') else {
            continue;
        };
        let Ok(value) = u64::from_str_radix(value.trim().trim_start_matches("0x"), 16) else {
            continue;
        };
        match key.trim() {
            "event" => config |= value,
            "umask" => config |= value << 8,
            _ => {}
        }
    }
    Some((kind, config))
}

/// What one tick of a unit's counter is worth, and in what.
///
/// RAPL counts in units of 2^-32 joules and says so in the `.scale` file beside
/// the event. A collector that assumed microjoules would be wrong by ten orders
/// of magnitude, which is why this is read rather than known.
pub fn sysfs_scale(pmu: &str, event: &str) -> Option<(f64, String)> {
    let scale = read_trimmed(&format!("{PMU_DEVICES}/{pmu}/events/{event}.scale"))?
        .parse()
        .ok()?;
    let unit = read_trimmed(&format!("{PMU_DEVICES}/{pmu}/events/{event}.unit"))
        .unwrap_or_else(|| "count".to_owned());
    Some((scale, unit))
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
}

/// One counter's value at the moment the group was read.
///
/// `raw` and `value` are both kept. They differ when the kernel had more events
/// open than the hardware has counters and time-shared them, and the difference
/// is the size of the estimate: reporting the scaled number alone would present
/// an extrapolation as a measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub name: &'static str,
    /// What the kernel returned, counted only while the counter was scheduled.
    pub raw: u64,
    /// `raw` extrapolated to the whole enabled window. Equal to `raw` whenever
    /// the counter was never multiplexed, which is the normal case.
    pub value: u64,
    /// How long the counter was enabled, in nanoseconds of the same domain the
    /// rest of the tool uses.
    pub enabled_ns: u64,
    /// How long it was actually scheduled onto hardware.
    pub running_ns: u64,
}

impl Reading {
    /// True when the counter shared hardware with others, so `value` is an
    /// extrapolation rather than a count.
    pub fn was_multiplexed(&self) -> bool {
        self.running_ns < self.enabled_ns
    }

    /// True when the counter never got onto hardware at all: there is no
    /// number here, and zero would be a lie.
    pub fn never_ran(&self) -> bool {
        self.running_ns == 0
    }

    /// The share of the enabled window the counter was actually on.
    pub fn duty(&self) -> f64 {
        if self.enabled_ns == 0 {
            return 1.0;
        }
        self.running_ns as f64 / self.enabled_ns as f64
    }
}

/// Extrapolates a partial count to the window it was meant to cover.
///
/// This is the kernel's own convention for a multiplexed counter: it was on for
/// `running_ns` of an `enabled_ns` window, so what it would have counted
/// throughout is `raw * enabled / running`. The result is an estimate, and
/// `Reading::was_multiplexed` is how a caller knows to say so rather than
/// present it as a count.
///
/// A counter that never ran has no value at all. Zero is returned because a
/// number is needed, and `Reading::never_ran` is what distinguishes it from a
/// counter that ran and counted nothing -- a difference an idle process makes
/// every time.
fn scaled(raw: u64, enabled_ns: u64, running_ns: u64) -> u64 {
    if running_ns == 0 {
        0
    } else if running_ns >= enabled_ns {
        raw
    } else {
        // In 128 bits, because a cycle counter over a long window times a
        // nanosecond count overflows 64 bits long before either does.
        ((raw as u128 * enabled_ns as u128) / running_ns as u128) as u64
    }
}

/// Counters opened together and scheduled together.
///
/// Group membership is not a convenience here. A ratio of two counters that
/// were not co-scheduled is a ratio of two different windows: instructions from
/// one slice of time over cycles from another says nothing about either. The
/// kernel guarantees that the members of a group are on the hardware at the
/// same time or not at all, which is what makes `cpu.ipc` a number rather than
/// a coincidence.
///
/// The group is read after it is disabled, and the leader's `DISABLE` stops
/// every member at once. So the readings are of one instant, without a
/// `PERF_FORMAT_GROUP` read that the kernel refuses for inherited events on
/// several kernels.
pub struct CounterGroup {
    /// The leader first; every other member was opened against it.
    fds: Vec<RawFd>,
    counters: Vec<Counter>,
    kernel_excluded: bool,
}

impl CounterGroup {
    /// Opens the counters against one target, following the threads and
    /// children it starts.
    ///
    /// Kernel-mode events are counted when the machine allows it. When
    /// `perf_event_paranoid` forbids it the group is reopened counting user
    /// mode only, and `kernel_excluded` says so: a count of user-mode
    /// instructions is a different measurement from a count of all of them, and
    /// which one was taken has to reach the output.
    pub fn open(counters: &[Counter], scope: Scope) -> io::Result<Self> {
        assert!(!counters.is_empty(), "a group needs at least one counter");
        match Self::open_with(counters, scope, false) {
            Ok(group) => Ok(group),
            Err(error)
                if matches!(error.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) =>
            {
                Self::open_with(counters, scope, true)
            }
            Err(error) => Err(error),
        }
    }

    /// Opens counters that count user-mode instructions only.
    ///
    /// Wanted whenever the number is being compared against an expectation
    /// about a program's own work. An unrestricted counter also counts every
    /// interrupt handler, timer tick and page fault that landed on the task,
    /// and on a busy machine that is a percent or more -- enough to make a
    /// comparison against a known instruction count meaningless.
    pub fn open_user_mode(counters: &[Counter], scope: Scope) -> io::Result<Self> {
        Self::open_with(counters, scope, true)
    }

    fn open_with(counters: &[Counter], scope: Scope, exclude_kernel: bool) -> io::Result<Self> {
        let mut group = Self {
            fds: Vec::with_capacity(counters.len()),
            counters: counters.to_vec(),
            kernel_excluded: exclude_kernel,
        };
        for counter in counters {
            let mut attr: sys::bindings::perf_event_attr = unsafe { std::mem::zeroed() };
            attr.type_ = counter.kind;
            attr.size = std::mem::size_of::<sys::bindings::perf_event_attr>() as u32;
            attr.config = counter.config;
            // Both times come back with every read, which is the only way to
            // know whether the value is a count or an extrapolation.
            attr.read_format = (sys::bindings::PERF_FORMAT_TOTAL_TIME_ENABLED
                | sys::bindings::PERF_FORMAT_TOTAL_TIME_RUNNING)
                as u64;
            if exclude_kernel {
                attr.set_exclude_kernel(1);
                attr.set_exclude_hv(1);
            }
            // The whole group starts stopped and is released by the leader, so
            // a launched program can be counted from its first instruction.
            if group.fds.is_empty() {
                attr.set_disabled(1);
            }
            let (pid, cpu) = match scope {
                Scope::Process(pid) => (pid, -1),
                Scope::Cpu(cpu) => (-1, cpu as i32),
            };
            // A counting event needs no ring buffer, which is what makes
            // `inherit` usable here: the count follows the threads and children
            // the target starts, and a program whose work happens in a thread
            // pool is counted rather than reported as idle. The sampling path
            // in this file cannot do the same, because the kernel refuses to
            // map a ring for an inherited per-task event.
            //
            // It is equally refused for an event that follows a cpu rather than
            // a task: there is nothing to inherit into. A package counter --
            // RAPL, the uncore -- is exactly that, and setting the bit made
            // every one of them fail with EINVAL.
            if matches!(scope, Scope::Process(_)) {
                attr.set_inherit(1);
            }
            let leader = group.fds.first().copied().unwrap_or(-1);
            let fd = unsafe {
                sys::perf_event_open(
                    &mut attr,
                    pid,
                    cpu,
                    leader,
                    sys::bindings::PERF_FLAG_FD_CLOEXEC as u64,
                )
            };
            if fd < 0 {
                let errno = io::Error::last_os_error();
                return Err(io::Error::from_raw_os_error(
                    errno.raw_os_error().unwrap_or(libc::EINVAL),
                ));
            }
            group.fds.push(fd);
        }
        Ok(group)
    }

    /// Whether kernel-mode events had to be left out, and why.
    pub fn kernel_excluded(&self) -> bool {
        self.kernel_excluded
    }

    pub fn enable(&self) -> io::Result<()> {
        // The leader carries the whole group, so one ioctl starts them all at
        // the same instant.
        let result = unsafe { sys::ioctls::ENABLE(self.leader(), 0) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn disable(&self) -> io::Result<()> {
        let result = unsafe { sys::ioctls::DISABLE(self.leader(), 0) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn leader(&self) -> RawFd {
        *self.fds.first().expect("a group always has a leader")
    }

    /// Reads every counter in the group.
    ///
    /// Read the group while it is disabled when the values have to describe one
    /// instant, which is what [`CounterGroup::read_final`] does.
    pub fn read(&self) -> io::Result<Vec<Reading>> {
        let mut readings = Vec::with_capacity(self.fds.len());
        for (fd, counter) in self.fds.iter().zip(&self.counters) {
            let mut buffer = [0u64; 3];
            let read = unsafe {
                libc::read(
                    *fd,
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    std::mem::size_of_val(&buffer),
                )
            };
            if read != std::mem::size_of_val(&buffer) as isize {
                return Err(io::Error::last_os_error());
            }
            let (raw, enabled_ns, running_ns) = (buffer[0], buffer[1], buffer[2]);
            readings.push(Reading {
                name: counter.name,
                raw,
                value: scaled(raw, enabled_ns, running_ns),
                enabled_ns,
                running_ns,
            });
        }
        Ok(readings)
    }

    /// Stops the group and reads it.
    ///
    /// Stopping first is what makes the readings simultaneous: the leader's
    /// `DISABLE` freezes every member, so two counters read a microsecond apart
    /// still describe the same window.
    pub fn read_final(&self) -> io::Result<Vec<Reading>> {
        self.disable()?;
        self.read()
    }
}

impl Drop for CounterGroup {
    fn drop(&mut self) {
        for fd in self.fds.drain(..) {
            unsafe { libc::close(fd) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE_FAULTS: u64 = sys::bindings::PERF_COUNT_SW_PAGE_FAULTS as u64;

    #[test]
    fn a_record_that_straddles_the_seam_is_read_whole() {
        // Sixteen bytes of ring, numbered so any misread is visible.
        let ring: Vec<u8> = (0..16).collect();

        // Wholly before the seam.
        assert_eq!(read_wrapped(&ring, 4, 4), vec![4, 5, 6, 7]);
        // Ending exactly on it.
        assert_eq!(read_wrapped(&ring, 12, 4), vec![12, 13, 14, 15]);
        // Straddling it: the tail of the ring, then the head.
        assert_eq!(read_wrapped(&ring, 14, 4), vec![14, 15, 0, 1]);
        // An offset past the end is the same position, one lap later, which is
        // what the kernel's ever-growing head and tail mean.
        assert_eq!(read_wrapped(&ring, 16 + 14, 4), read_wrapped(&ring, 14, 4));
        assert_eq!(read_wrapped(&ring, 16 * 1000 + 3, 2), vec![3, 4]);
    }

    /// A record is length-prefixed and the count inside it is the kernel's.
    /// Trusting the count over the length is how a decoder walks off the end of
    /// a truncated record and reports the next record's bytes as frames.
    #[test]
    fn a_callchain_is_bounded_by_the_record_and_not_by_its_own_count() {
        // header, ip, tid, time, addr, nr, then nr addresses.
        let record = |depth: u64, frames: &[u64]| {
            let mut body = vec![0u8; 8];
            for word in [1u64, 2, 3, 4, depth].iter().chain(frames) {
                body.extend_from_slice(&word.to_ne_bytes());
            }
            body
        };

        assert_eq!(
            callchain_of(&record(3, &[0xa, 0xb, 0xc])),
            vec![0xa, 0xb, 0xc]
        );
        // A count larger than the record can hold yields what the record holds.
        assert_eq!(callchain_of(&record(99, &[0xa, 0xb])), vec![0xa, 0xb]);
        assert_eq!(callchain_of(&record(0, &[])), Vec::<u64>::new());
        // A record too short to even hold the count is not a callchain.
        assert_eq!(callchain_of(&[0u8; 16]), Vec::<u64>::new());
    }

    /// The header says which mode the sampled instruction was in, and it is the
    /// only thing that does. Reading it wrongly files kernel addresses as user
    /// code, which symbolizes them through the wrong tables entirely.
    #[test]
    fn the_record_header_says_which_side_the_sample_was_taken_on() {
        assert_eq!(
            CpuMode::from_misc(sys::bindings::PERF_RECORD_MISC_KERNEL as u16),
            CpuMode::Kernel
        );
        assert_eq!(
            CpuMode::from_misc(sys::bindings::PERF_RECORD_MISC_USER as u16),
            CpuMode::User
        );
        // The mode is the low three bits; the rest of `misc` is other flags and
        // must not change the answer.
        assert_eq!(
            CpuMode::from_misc(0xff00 | sys::bindings::PERF_RECORD_MISC_USER as u16),
            CpuMode::User
        );
        assert_eq!(CpuMode::from_misc(0), CpuMode::Unknown);
    }

    /// An event that did not ask for stacks must not come back with an empty
    /// one: "no callchain" and "a callchain with nothing in it" are different
    /// answers, and only the second says the unwinder ran.
    #[test]
    fn a_sample_from_an_event_that_asked_for_no_stack_carries_no_callchain() {
        let Ok(mut event) = SampledEvent::open_software(PAGE_FAULTS, 1, Scope::Process(0), 16)
        else {
            eprintln!("skipped: this machine refuses unprivileged perf sampling");
            return;
        };
        assert_eq!(event.callchain(), Callchain::Off);
        event.enable().expect("enable");
        // Mapped rather than allocated: pages the allocator already holds are
        // resident, and a third identical `Vec` in this binary faults nothing.
        const PAGES: usize = 64;
        let size = PAGES * page_size();
        let block = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(block, libc::MAP_FAILED, "the test needs a fresh mapping");
        for index in 0..PAGES {
            unsafe {
                (block as *mut u8)
                    .add(index * page_size())
                    .write_volatile(1)
            };
        }
        event.disable().expect("disable");

        let mut seen = 0;
        event.drain(|record| {
            if let Record::Sample(sample) = record {
                seen += 1;
                assert_eq!(sample.callchain, None);
            }
        });
        unsafe { libc::munmap(block, size) };
        assert!(seen > 0, "no sample arrived to check");
    }

    #[test]
    fn a_read_longer_than_the_ring_goes_around_rather_than_panicking() {
        let ring: Vec<u8> = (0..4).collect();
        assert_eq!(
            read_wrapped(&ring, 2, 10),
            vec![2, 3, 0, 1, 2, 3, 0, 1, 2, 3]
        );
        assert_eq!(read_wrapped(&[], 0, 3), vec![0, 0, 0]);
    }

    #[test]
    fn a_page_fault_sample_carries_the_address_that_faulted() {
        let Ok(mut event) = SampledEvent::open_software(PAGE_FAULTS, 1, Scope::Process(0), 64)
        else {
            eprintln!("skipped: this machine refuses unprivileged perf sampling");
            return;
        };
        event.enable().expect("enable");

        // Mapped, not allocated. `free()` returns nothing to the kernel, so an
        // allocator hands back pages that are already resident and touching
        // them faults nothing: measured here, a third identical `Vec` produced
        // zero faults, which is why this test used to fail only inside a full
        // suite run and pass on its own. A fresh anonymous mapping is the only
        // way to know the pages are not there yet.
        const PAGES: usize = 64;
        let page = page_size();
        let block = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PAGES * page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(block, libc::MAP_FAILED, "the test needs a fresh mapping");
        let base = block as u64;
        for index in 0..PAGES {
            unsafe { std::ptr::write_volatile((block as *mut u8).add(index * page), 1) };
        }
        event.disable().expect("disable");

        let (mut samples, mut inside) = (0, 0);
        event.drain(|record| {
            if let Record::Sample(sample) = record {
                samples += 1;
                if (base..base + (PAGES * page) as u64).contains(&sample.addr) {
                    inside += 1;
                }
            }
        });

        // Touching N pages does not produce N faults. On a kernel with
        // multi-size transparent huge pages one fault installs a whole folio,
        // which on this family is up to sixteen base pages, so the count is a
        // fraction of the pages touched and varies run to run. Asserting the
        // page count was a test encoding an expectation the kernel never made.
        //
        // What is worth asserting is that the addresses are real: the block
        // sits at an address the kernel chose for this mapping, so samples
        // landing inside it cannot be coincidence.
        assert!(samples > 0, "no page-fault sample arrived at all");
        assert!(
            inside >= PAGES / 16,
            "only {inside} of {samples} samples landed in the {PAGES} pages that were touched, which is below even one fault per sixteen-page folio"
        );
        assert!(
            inside <= PAGES,
            "{inside} samples landed in a block of {PAGES} pages, so an address is being reported that was never touched"
        );
        unsafe { libc::munmap(block, PAGES * page) };
    }

    const CYCLES: Counter = Counter::hardware("cycles", sys::bindings::PERF_COUNT_HW_CPU_CYCLES);
    const INSTRUCTIONS: Counter =
        Counter::hardware("instructions", sys::bindings::PERF_COUNT_HW_INSTRUCTIONS);

    /// Something to count that the compiler cannot delete.
    fn busy() {
        let mut total = 0u64;
        for index in 0..2_000_000u64 {
            total = total.wrapping_add(index * 3);
        }
        std::hint::black_box(total);
    }

    #[test]
    fn every_counter_of_a_group_describes_the_same_window() {
        let Ok(group) = CounterGroup::open(&[INSTRUCTIONS, CYCLES], Scope::Process(0)) else {
            eprintln!("skipped: this machine refuses unprivileged perf counting");
            return;
        };
        group.enable().expect("enable");
        busy();
        let readings = group.read_final().expect("read");

        assert_eq!(readings.len(), 2);
        assert!(readings.iter().all(|reading| reading.value > 0));
        // The whole point of a group: the two counters were on the hardware
        // together, so a ratio of them is a ratio of one window.
        let spread = readings[0].enabled_ns.abs_diff(readings[1].enabled_ns);
        assert!(
            spread < 1_000_000,
            "the two counters describe windows {spread}ns apart"
        );
    }

    /// A counter that shared the hardware has to say so. The scaled value is an
    /// extrapolation, and a tool that presented it as a count would report a
    /// number nobody could tell apart from a measured one.
    #[test]
    fn a_time_shared_counter_reports_that_its_value_is_scaled() {
        let groups: Vec<CounterGroup> = (0..16)
            .filter_map(|_| CounterGroup::open(&[CYCLES], Scope::Process(0)).ok())
            .collect();
        if groups.len() < 16 {
            eprintln!("skipped: this machine refuses unprivileged perf counting");
            return;
        }
        for group in &groups {
            group.enable().expect("enable");
        }
        busy();
        let readings: Vec<Reading> = groups
            .iter()
            .filter_map(|group| group.read_final().ok())
            .flatten()
            .collect();

        let shared: Vec<&Reading> = readings
            .iter()
            .filter(|reading| reading.was_multiplexed())
            .collect();
        if shared.is_empty() {
            eprintln!("skipped: this cpu scheduled sixteen counters at once");
            return;
        }
        for reading in shared {
            assert!(reading.duty() < 1.0);
            assert!(
                reading.value >= reading.raw,
                "a scaled value is an extrapolation upwards, never downwards"
            );
        }
    }

    #[test]
    fn a_counter_that_was_on_throughout_is_reported_as_it_was_counted() {
        // The common case, and the one that must never be touched: an
        // unmultiplexed counter's value is the hardware's own number.
        assert_eq!(scaled(1_000, 5_000_000, 5_000_000), 1_000);
        // The kernel occasionally reports a running window a hair longer than
        // the enabled one; extrapolating downwards from that would invent a
        // shortfall.
        assert_eq!(scaled(1_000, 5_000_000, 5_000_001), 1_000);
    }

    #[test]
    fn a_counter_that_was_on_for_part_of_the_window_is_extrapolated_to_all_of_it() {
        // On for a quarter of the window, so four times what it counted.
        assert_eq!(scaled(1_000, 4_000_000, 1_000_000), 4_000);
        // The arithmetic is done in 128 bits: a cycle counter over a minute
        // times a nanosecond window passes 64 bits without difficulty.
        let cycles = 4_000_000_000u64 * 60;
        assert_eq!(
            scaled(cycles, 60_000_000_000, 30_000_000_000),
            cycles * 2,
            "the product overflowed instead of being computed"
        );
    }

    #[test]
    fn a_counter_that_never_ran_is_not_a_counter_that_counted_nothing() {
        let never = Reading {
            name: "cycles",
            raw: 0,
            value: scaled(0, 5_000_000, 0),
            enabled_ns: 5_000_000,
            running_ns: 0,
        };
        assert!(never.never_ran());
        assert_eq!(never.value, 0);

        let idle = Reading {
            name: "cycles",
            raw: 0,
            value: scaled(0, 5_000_000, 5_000_000),
            enabled_ns: 5_000_000,
            running_ns: 5_000_000,
        };
        // Both hold zero. Only one of them is a measurement, and telling them
        // apart is what stops an idle process reading as a zero count.
        assert!(!idle.never_ran());
        assert!(!idle.was_multiplexed());
        assert_eq!(idle.duty(), 1.0);
    }

    /// Counting is exact, which means the window's edges have to be exact too:
    /// nothing that happens while the group is stopped may appear in it.
    #[test]
    fn work_done_outside_the_window_is_not_counted() {
        let Ok(group) = CounterGroup::open(&[INSTRUCTIONS], Scope::Process(0)) else {
            eprintln!("skipped: this machine refuses unprivileged perf counting");
            return;
        };
        group.enable().expect("enable");
        busy();
        let inside = group.read_final().expect("read")[0].value;

        // The group is stopped now. This work belongs to no window.
        for _ in 0..4 {
            busy();
        }
        let after = group.read().expect("read")[0].value;

        assert_eq!(
            inside, after,
            "{} instructions were retired while the group was disabled and {} of them              were counted anyway",
            after - inside,
            after - inside
        );
        assert!(inside > 0, "nothing at all was counted inside the window");
    }

    /// A group that cannot fit on the hardware is never scheduled, and the
    /// kernel then has nothing to report. What must not happen is that nothing
    /// arrives as zero.
    #[test]
    fn a_group_too_large_for_the_hardware_reports_no_value_rather_than_zero() {
        // How many counters a group may even contain is itself a limit, and it
        // differs between vendors: the largest group this machine will open is
        // found rather than assumed, and it only has to exceed the number of
        // counters the hardware can schedule at once.
        let opened = (2..=16).rev().find_map(|size| {
            let counters: Vec<Counter> = (0..size).map(|_| CYCLES).collect();
            CounterGroup::open(&counters, Scope::Process(0)).ok()
        });
        let Some(group) = opened else {
            eprintln!("skipped: this machine refuses unprivileged perf counting");
            return;
        };
        group.enable().expect("enable");
        busy();
        let readings = group.read_final().expect("read");

        if !readings[0].never_ran() {
            eprintln!(
                "skipped: this machine scheduled {} counters at once",
                readings.len()
            );
            return;
        }
        assert!(
            readings.iter().all(|reading| reading.never_ran()),
            "part of a group ran without the rest, which a group is meant to prevent"
        );
        assert!(
            readings.iter().all(|reading| reading.value == 0),
            "a counter that never ran reported a value"
        );
    }

    /// Every group holds file descriptors, and a session opens one per source
    /// per run. A leak here would end a long run with EMFILE and no explanation.
    #[test]
    fn closing_a_group_gives_its_descriptors_back() {
        // Only this process's perf events are counted, and generously: the
        // other tests in this binary open and close their own while this runs.
        // The signature of a leak is a hundred and twenty-eight descriptors,
        // which no amount of concurrent noise resembles.
        fn perf_descriptors() -> usize {
            std::fs::read_dir("/proc/self/fd")
                .into_iter()
                .flatten()
                .flatten()
                .filter(|entry| {
                    std::fs::read_link(entry.path())
                        .map(|target| target.to_string_lossy().contains("perf_event"))
                        .unwrap_or(false)
                })
                .count()
        }

        const GROUPS: usize = 64;
        let Ok(group) = CounterGroup::open(&[INSTRUCTIONS, CYCLES], Scope::Process(0)) else {
            eprintln!("skipped: this machine refuses unprivileged perf counting");
            return;
        };
        drop(group);

        let before = perf_descriptors();
        for _ in 0..GROUPS {
            let group = CounterGroup::open(&[INSTRUCTIONS, CYCLES], Scope::Process(0))
                .expect("the first one opened, so these do too");
            drop(group);
        }
        let after = perf_descriptors();
        assert!(
            after < before + GROUPS,
            "{GROUPS} groups of two counters left {} perf descriptors behind",
            after.saturating_sub(before)
        );
    }

    /// On a machine where `perf_event_paranoid` forbids counting kernel mode,
    /// the group is reopened without it -- and then the count means something
    /// different. This checks the restricted shape opens and admits what it is;
    /// the retry that reaches it can only be triggered on a machine that
    /// refuses the unrestricted one.
    #[test]
    fn a_user_mode_only_group_says_that_it_is_one() {
        let Ok(group) = CounterGroup::open_with(&[INSTRUCTIONS], Scope::Process(0), true) else {
            eprintln!("skipped: this machine refuses unprivileged perf counting");
            return;
        };
        assert!(group.kernel_excluded());
        group.enable().expect("enable");
        busy();
        let readings = group.read_final().expect("read");
        assert!(readings[0].value > 0);

        let Ok(everything) = CounterGroup::open(&[INSTRUCTIONS], Scope::Process(0)) else {
            return;
        };
        if everything.kernel_excluded() {
            // This machine has no unrestricted shape to compare against.
            return;
        }
        everything.enable().expect("enable");
        busy();
        let full = everything.read_final().expect("read");
        assert!(
            full[0].value > readings[0].value,
            "counting kernel mode as well cannot yield fewer instructions              ({} against {})",
            full[0].value,
            readings[0].value
        );
    }

    #[test]
    fn sample_timestamps_share_the_tools_clock_domain() {
        let Ok(mut event) = SampledEvent::open_software(PAGE_FAULTS, 1, Scope::Process(0), 16)
        else {
            return;
        };
        event.enable().expect("enable");
        // Enough pages that a folio of any size still yields several faults,
        // and mapped rather than allocated: a block from the allocator's arena
        // can come back on pages another test in this binary already made
        // resident, and a page that is already there does not fault. The test
        // would then be waiting for events that had no reason to happen.
        const PAGES: usize = 256;
        let size = PAGES * page_size();
        // SAFETY: an anonymous mapping of our own, unmapped below.
        let block = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(block, libc::MAP_FAILED, "the mapping must succeed");
        for index in 0..PAGES {
            // SAFETY: writing one byte per page inside that mapping.
            unsafe {
                block
                    .cast::<u8>()
                    .add(index * page_size())
                    .write_volatile(1)
            };
        }
        event.disable().expect("disable");
        // SAFETY: unmapping what was mapped above.
        unsafe { libc::munmap(block, size) };
        let after = crate::clock::monotonic_ns();

        let mut seen = None;
        event.drain(|record| {
            if let Record::Sample(sample) = record {
                seen.get_or_insert(sample.time_ns);
            }
        });

        let time = seen.expect("at least one sample");
        assert!(
            time <= after,
            "a sample cannot be newer than the clock read after it"
        );
        assert!(
            after - time < 5_000_000_000,
            "sample clock and CLOCK_MONOTONIC are {}ns apart, so they are different clocks",
            after - time
        );
    }
}
