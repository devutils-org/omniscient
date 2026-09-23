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
    /// One task, only while it runs on one cpu.
    ///
    /// This is the shape that can follow a program's threads. The kernel
    /// refuses a ring for an inherited event on every cpu at once, and accepts
    /// one for an inherited event pinned to a cpu: so a task and everything it
    /// starts is sampled by one such event per cpu, each with its own ring.
    TaskOnCpu { pid: i32, cpu: u32 },
    /// Every task of a cgroup v2 directory, while it runs on one cpu.
    ///
    /// `fd` is the directory, opened by the caller and held until the event is
    /// open; the kernel resolves it to the cgroup at `perf_event_open` and does
    /// not need it afterwards.
    CgroupOnCpu { fd: RawFd, cpu: u32 },
}

impl Scope {
    /// The `pid` and `cpu` arguments of `perf_event_open`, and its flags.
    fn arguments(self) -> (i32, i32, u64) {
        let flags = sys::bindings::PERF_FLAG_FD_CLOEXEC as u64;
        match self {
            Scope::Process(pid) => (pid, -1, flags),
            Scope::Cpu(cpu) => (-1, cpu as i32, flags),
            Scope::TaskOnCpu { pid, cpu } => (pid, cpu as i32, flags),
            Scope::CgroupOnCpu { fd, cpu } => (
                fd,
                cpu as i32,
                flags | sys::bindings::PERF_FLAG_PID_CGROUP as u64,
            ),
        }
    }

    /// The cpu a per-cpu scope is pinned to.
    pub fn cpu(self) -> Option<u32> {
        match self {
            Scope::Process(_) => None,
            Scope::Cpu(cpu) | Scope::TaskOnCpu { cpu, .. } | Scope::CgroupOnCpu { cpu, .. } => {
                Some(cpu)
            }
        }
    }
}

/// How often a sampling event fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rate {
    /// Once every so many occurrences of whatever the event counts.
    Period(u64),
    /// About this many times a second; the kernel adjusts the period to get
    /// there, and the adjustment is its own and not visible from here.
    Hz(u64),
}

/// What a sampling event counts, and what each of its samples carries.
///
/// The software events a fault sampler or a cpu-clock profile open are one
/// point in this space; a hardware unit with a type number the kernel assigned
/// at boot, such as AMD's IBS, is another. Everything that decides the record
/// layout is here, because the decoder reads it back from the same place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleSpec {
    /// `perf_event_attr.type`: `PERF_TYPE_SOFTWARE`, or a dynamic unit's type
    /// read from its `type` file under [`PMU_DEVICES`].
    pub kind: u32,
    pub config: u64,
    /// `config2`, which some units use for filters that are not events.
    pub config2: u64,
    pub rate: Rate,
    /// Samples taken in kernel mode are not delivered.
    pub exclude_kernel: bool,
    /// Tasks the target starts afterwards are sampled too. Only accepted by the
    /// kernel with a ring for a [`Scope::TaskOnCpu`] event.
    pub inherit: bool,
    pub callchain: Callchain,
    /// Each sample carries the cpu it was taken on, the access's data source
    /// and its weights: see [`MemoryAccess`].
    pub memory: bool,
}

impl SampleSpec {
    /// A software event, the shape every sampler here had before IBS.
    pub fn software(config: u64, period: u64, callchain: Callchain) -> Self {
        Self {
            kind: sys::bindings::PERF_TYPE_SOFTWARE,
            config,
            config2: 0,
            rate: Rate::Period(period),
            exclude_kernel: false,
            inherit: false,
            callchain,
            memory: false,
        }
    }

    /// The sample fields this spec asks for, which is also the layout the
    /// decoder reads: the kernel writes them in the order of their bits.
    fn sample_type(&self) -> u64 {
        let mut sample_type = SAMPLE_TYPE;
        if self.callchain == Callchain::FramePointers {
            sample_type |= sys::bindings::PERF_SAMPLE_CALLCHAIN as u64;
        }
        if self.memory {
            sample_type |= (sys::bindings::PERF_SAMPLE_CPU
                | sys::bindings::PERF_SAMPLE_WEIGHT_STRUCT
                | sys::bindings::PERF_SAMPLE_DATA_SRC) as u64;
        }
        sample_type
    }
}

/// What the hardware said about one memory access it sampled.
///
/// The kernel decodes the unit's own registers into these two words, so a
/// collector reads the same fields from IBS on AMD as from PEBS on Intel. What
/// each vendor fills in differs, and [`MemoryAccess::latency_cycles`] is the
/// field where that matters most.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryAccess {
    pub source: DataSource,
    /// Cycles from the load's issue until its data arrived, when the hardware
    /// measured it; zero when it did not.
    ///
    /// Zero is not "fast". AMD family 17h counts this only for a load that
    /// missed the L1 data cache, so every L1 hit carries zero, and a histogram
    /// that bucketed those zeros would be a histogram of what the hardware did
    /// not measure.
    pub latency_cycles: u32,
    /// The second weight: on IBS, cycles from the op's tagging to its
    /// retirement, which covers more than the memory access.
    pub retire_cycles: u16,
}

impl MemoryAccess {
    fn from_words(weight: u64, data_src: u64) -> Self {
        Self {
            source: DataSource(data_src),
            // `perf_sample_weight`: var1_dw in the low half, var2_w above it.
            latency_cycles: (weight & 0xffff_ffff) as u32,
            retire_cycles: ((weight >> 32) & 0xffff) as u16,
        }
    }
}

/// `perf_mem_data_src`, the kernel's vendor-neutral description of where an
/// access went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataSource(pub u64);

/// What kind of operation the sampled op was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemOp {
    Load,
    Store,
    Prefetch,
    Exec,
    /// Not a memory operation, or one the hardware did not describe: most of
    /// the ops an IBS op sample tags are arithmetic and branches.
    None,
}

/// Which level of the hierarchy answered the access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemLevel {
    L1,
    /// The miss buffer: the line was already on its way for an earlier miss.
    Lfb,
    L2,
    L3,
    /// A cache of this node that is not this core's own. On AMD family 17h
    /// this is how an L3 hit and a hit in a sibling core's cache are both
    /// reported, because the unit's data-source field does not tell them apart.
    Peer,
    Ram,
    /// A cache on another node.
    RemoteCache,
    /// Memory on another node.
    RemoteRam,
    Io,
    /// The hardware reported nothing usable.
    Unknown,
}

impl MemLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            MemLevel::L1 => "l1",
            MemLevel::Lfb => "lfb",
            MemLevel::L2 => "l2",
            MemLevel::L3 => "l3",
            MemLevel::Peer => "peer",
            MemLevel::Ram => "ram",
            MemLevel::RemoteCache => "remote_cache",
            MemLevel::RemoteRam => "remote_ram",
            MemLevel::Io => "io",
            MemLevel::Unknown => "unknown",
        }
    }
}

impl DataSource {
    fn field(self, shift: u32, bits: u32) -> u64 {
        (self.0 >> shift) & ((1u64 << bits) - 1)
    }

    pub fn op(self) -> MemOp {
        use sys::bindings as b;
        let op = self.field(b::PERF_MEM_OP_SHIFT, 5) as u32;
        if op & b::PERF_MEM_OP_LOAD != 0 {
            MemOp::Load
        } else if op & b::PERF_MEM_OP_STORE != 0 {
            MemOp::Store
        } else if op & b::PERF_MEM_OP_PFETCH != 0 {
            MemOp::Prefetch
        } else if op & b::PERF_MEM_OP_EXEC != 0 {
            MemOp::Exec
        } else {
            MemOp::None
        }
    }

    fn remote(self) -> bool {
        use sys::bindings as b;
        self.field(b::PERF_MEM_REMOTE_SHIFT, 1) as u32 & b::PERF_MEM_REMOTE_REMOTE != 0
    }

    /// The level, read from the numbered field the kernel fills since 4.14 and
    /// from the older bit mask when the numbered one says nothing.
    pub fn level(self) -> MemLevel {
        use sys::bindings as b;
        let remote = self.remote();
        match self.field(b::PERF_MEM_LVLNUM_SHIFT, 4) as u32 {
            b::PERF_MEM_LVLNUM_L1 => return MemLevel::L1,
            b::PERF_MEM_LVLNUM_LFB => return MemLevel::Lfb,
            b::PERF_MEM_LVLNUM_L2 => return MemLevel::L2,
            b::PERF_MEM_LVLNUM_L3 if remote => return MemLevel::RemoteCache,
            b::PERF_MEM_LVLNUM_L3 => return MemLevel::L3,
            b::PERF_MEM_LVLNUM_ANY_CACHE if remote => return MemLevel::RemoteCache,
            b::PERF_MEM_LVLNUM_ANY_CACHE => return MemLevel::Peer,
            b::PERF_MEM_LVLNUM_RAM if remote => return MemLevel::RemoteRam,
            b::PERF_MEM_LVLNUM_RAM => return MemLevel::Ram,
            b::PERF_MEM_LVLNUM_IO => return MemLevel::Io,
            _ => {}
        }
        let mask = self.field(b::PERF_MEM_LVL_SHIFT, 14) as u32;
        if mask & b::PERF_MEM_LVL_HIT == 0 {
            return MemLevel::Unknown;
        }
        if mask & b::PERF_MEM_LVL_L1 != 0 {
            MemLevel::L1
        } else if mask & b::PERF_MEM_LVL_LFB != 0 {
            MemLevel::Lfb
        } else if mask & b::PERF_MEM_LVL_L2 != 0 {
            MemLevel::L2
        } else if mask & b::PERF_MEM_LVL_L3 != 0 {
            MemLevel::L3
        } else if mask & b::PERF_MEM_LVL_LOC_RAM != 0 {
            MemLevel::Ram
        } else if mask & (b::PERF_MEM_LVL_REM_RAM1 | b::PERF_MEM_LVL_REM_RAM2) != 0 {
            MemLevel::RemoteRam
        } else if mask & (b::PERF_MEM_LVL_REM_CCE1 | b::PERF_MEM_LVL_REM_CCE2) != 0 {
            MemLevel::RemoteCache
        } else if mask & b::PERF_MEM_LVL_IO != 0 {
            MemLevel::Io
        } else {
            MemLevel::Unknown
        }
    }

    /// The line was modified in the cache that answered: another core had
    /// written it, and this access had to take it away. This is the signal
    /// false sharing leaves, and the one a miss count cannot show.
    pub fn hitm(self) -> bool {
        use sys::bindings as b;
        self.field(b::PERF_MEM_SNOOP_SHIFT, 5) as u32 & b::PERF_MEM_SNOOP_HITM != 0
    }

    /// The access missed the first-level data TLB.
    pub fn tlb_miss(self) -> bool {
        use sys::bindings as b;
        self.field(b::PERF_MEM_TLB_SHIFT, 7) as u32 & b::PERF_MEM_TLB_MISS != 0
    }
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
    /// The cpu the sample was taken on, when the event asked for it.
    pub cpu: Option<u32>,
    /// Where the sampled access went and how long it waited, when the event
    /// asked for it. See [`SampleSpec::memory`].
    pub access: Option<MemoryAccess>,
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
#[cfg(test)]
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

/// A sampled event and the ring buffer its samples arrive in.
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
    sample_type: u64,
    /// Further events on the same cpu whose samples the kernel writes into this
    /// event's ring rather than into rings of their own. See
    /// [`SampledEvent::follow`].
    followers: Vec<RawFd>,
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
        Self::open(
            &SampleSpec::software(config, period, callchain),
            scope,
            ring_pages,
        )
    }

    /// Opens any sampled event, shrinking the ring until the machine allows it.
    pub fn open(spec: &SampleSpec, scope: Scope, ring_pages: usize) -> io::Result<Self> {
        let mut pages = ring_pages.next_power_of_two();
        loop {
            match Self::open_with_ring(spec, scope, pages) {
                Ok(event) => return Ok(event),
                Err(error) if pages > MIN_RING_PAGES && is_ring_too_large(&error) => {
                    pages /= 2;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Opens a second event on this event's cpu and sends its samples into
    /// this event's ring.
    ///
    /// Two threads of a running process are two events on every cpu, and a
    /// ring per event per cpu would be a ring per thread per cpu. The kernel
    /// lets events on one cpu share a ring (`PERF_EVENT_IOC_SET_OUTPUT`), and
    /// refuses to let events on two different cpus share one, which is why this
    /// asks for the scope's cpu to be this event's.
    ///
    /// The follower must ask for the same sample fields: the ring has one
    /// layout, and it is this event's.
    pub fn follow(&mut self, spec: &SampleSpec, scope: Scope) -> io::Result<()> {
        if spec.sample_type() != self.sample_type {
            return Err(io::Error::other(
                "an event that shares a ring must ask for the same sample fields",
            ));
        }
        let fd = open_fd(spec, scope)?;
        // SAFETY: both descriptors are perf events this process opened.
        let result = unsafe { sys::ioctls::SET_OUTPUT(fd, self.fd) };
        if result < 0 {
            let error = io::Error::other(format!(
                "redirecting a sampled event into a shared ring: {}",
                io::Error::last_os_error()
            ));
            unsafe { libc::close(fd) };
            return Err(error);
        }
        self.followers.push(fd);
        Ok(())
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

    fn open_with_ring(spec: &SampleSpec, scope: Scope, ring_pages: usize) -> io::Result<Self> {
        assert!(
            ring_pages.is_power_of_two(),
            "the ring must be a power of two pages"
        );
        let fd = open_fd(spec, scope)?;

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
            callchain: spec.callchain,
            sample_type: spec.sample_type(),
            followers: Vec::new(),
        })
    }

    /// Enables this event and every event that follows it into its ring.
    pub fn enable(&self) -> io::Result<()> {
        for fd in std::iter::once(self.fd).chain(self.followers.iter().copied()) {
            self.ioctl(unsafe { sys::ioctls::ENABLE(fd, 0) })?;
        }
        Ok(())
    }

    pub fn disable(&self) -> io::Result<()> {
        for fd in std::iter::once(self.fd).chain(self.followers.iter().copied()) {
            self.ioctl(unsafe { sys::ioctls::DISABLE(fd, 0) })?;
        }
        Ok(())
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
        match kind {
            sys::bindings::PERF_RECORD_SAMPLE => {
                Record::Sample(decode_sample(&body, misc, self.sample_type))
            }
            sys::bindings::PERF_RECORD_LOST => Record::Lost {
                count: word_at(&body, 1).unwrap_or(0),
            },
            sys::bindings::PERF_RECORD_THROTTLE => Record::Throttle { throttled: true },
            sys::bindings::PERF_RECORD_UNTHROTTLE => Record::Throttle { throttled: false },
            other => Record::Other { kind: other },
        }
    }
}

/// The eight-byte word at `index` after the record header.
fn word_at(body: &[u8], index: usize) -> Option<u64> {
    let start = 8 + index * 8;
    body.get(start..start + 8)
        .map(|bytes| u64::from_ne_bytes(bytes.try_into().expect("8 bytes")))
}

/// Reads a sample record in the layout its event's `sample_type` gave it.
///
/// The kernel writes the fields in the order of their bits and skips the ones
/// that were not asked for, so the position of `DATA_SRC` depends on whether a
/// callchain came before it. Walking the bits in that order is the only
/// reading that stays right when a field is added. A field the record is too
/// short to hold reads as zero rather than as the next record's bytes.
fn decode_sample(body: &[u8], misc: u16, sample_type: u64) -> Sample {
    use sys::bindings as b;
    let asked = |bit: u32| sample_type & bit as u64 != 0;
    let mut index = 0;
    let mut next = || {
        let word = word_at(body, index).unwrap_or(0);
        index += 1;
        word
    };
    let ip = if asked(b::PERF_SAMPLE_IP) { next() } else { 0 };
    let tid_field = if asked(b::PERF_SAMPLE_TID) { next() } else { 0 };
    let time_ns = if asked(b::PERF_SAMPLE_TIME) {
        next()
    } else {
        0
    };
    let addr = if asked(b::PERF_SAMPLE_ADDR) {
        next()
    } else {
        0
    };
    // `cpu` then a reserved half-word, in one word.
    let cpu = asked(b::PERF_SAMPLE_CPU).then(|| (next() & 0xffff_ffff) as u32);
    let callchain = asked(b::PERF_SAMPLE_CALLCHAIN).then(|| {
        let chain = callchain_of(body, index);
        index += 1 + chain.len();
        chain
    });
    let weight = asked(b::PERF_SAMPLE_WEIGHT_STRUCT).then(|| {
        let word = word_at(body, index).unwrap_or(0);
        index += 1;
        word
    });
    let data_src = asked(b::PERF_SAMPLE_DATA_SRC).then(|| {
        let word = word_at(body, index).unwrap_or(0);
        index += 1;
        word
    });
    Sample {
        ip,
        pid: (tid_field & 0xffff_ffff) as u32,
        tid: (tid_field >> 32) as u32,
        time_ns,
        addr,
        cpu_mode: CpuMode::from_misc(misc),
        callchain,
        cpu,
        access: match (weight, data_src) {
            (Some(weight), Some(data_src)) => Some(MemoryAccess::from_words(weight, data_src)),
            _ => None,
        },
    }
}

impl Drop for SampledEvent {
    fn drop(&mut self) {
        unsafe {
            // Followers first: each writes into this event's ring.
            for fd in &self.followers {
                libc::close(*fd);
            }
            libc::munmap(self.base as *mut libc::c_void, self.map_len);
            libc::close(self.fd);
        }
    }
}

/// Opens the event a spec describes, without a ring.
fn open_fd(spec: &SampleSpec, scope: Scope) -> io::Result<RawFd> {
    let mut attr: sys::bindings::perf_event_attr = unsafe { std::mem::zeroed() };
    attr.type_ = spec.kind;
    attr.size = std::mem::size_of::<sys::bindings::perf_event_attr>() as u32;
    attr.config = spec.config;
    attr.__bindgen_anon_4.config2 = spec.config2;
    match spec.rate {
        Rate::Period(period) => attr.__bindgen_anon_1.sample_period = period,
        Rate::Hz(hz) => {
            attr.set_freq(1);
            attr.__bindgen_anon_1.sample_freq = hz;
        }
    }
    attr.sample_type = spec.sample_type();
    if spec.callchain == Callchain::FramePointers {
        // Both halves are asked for. A kernel chain without a user chain
        // says a task is in `read`, and never which of its own functions
        // called it; a user chain without a kernel one cannot tell work
        // from waiting. `crate::stack` keeps them apart afterwards.
        attr.sample_max_stack = max_stack_depth();
    }
    if spec.exclude_kernel {
        attr.set_exclude_kernel(1);
        attr.set_exclude_hv(1);
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
    // `inherit` is off unless the spec asks for it, and the kernel only accepts
    // it here for an event pinned to a cpu: it refuses to map a ring buffer for
    // an inherited per-task event on every cpu (`perf_mmap` returns EINVAL when
    // `cpu == -1 && attr.inherit`), because each inheriting task would need a
    // buffer of its own. So a software sampler over `Scope::Process` samples the
    // task it was given, and `sampled_threads` states the limit rather than
    // leaving it to be discovered in a missing number.
    if spec.inherit {
        attr.set_inherit(1);
    }

    let (pid, cpu, flags) = scope.arguments();
    let fd = unsafe { sys::perf_event_open(&mut attr, pid, cpu, -1, flags) };
    if fd < 0 {
        return Err(io::Error::other(format!(
            "perf_event_open: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(fd)
}

/// The callchain of a sample record whose event asked for one.
///
/// It is a count at word `at` and then that many addresses. The count is the
/// kernel's, but the bound checked here is the record's own length: a decoder
/// that trusted the count would walk off the end of a truncated record and hand
/// back whatever the ring held next as frames.
///
/// Split out as a free function because, like the seam, it is a piece of this
/// module whose correctness does not need a kernel to check.
fn callchain_of(body: &[u8], at: usize) -> Vec<u64> {
    let Some(count) = word_at(body, at) else {
        return Vec::new();
    };
    let available = body.len().saturating_sub(8 + (at + 1) * 8) / 8;
    let depth = (count as usize).min(available);
    (0..depth)
        .filter_map(|index| word_at(body, at + 1 + index))
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

    /// Builds one of the cache descriptors, such as `LLC-load-misses`.
    ///
    /// `PERF_TYPE_HW_CACHE` is the one event type whose config is not a number
    /// the kernel publishes but three small fields packed into one word: which
    /// cache, which operation, and whether the count is of accesses or of
    /// misses. Packing them wrongly does not fail -- the open succeeds and the
    /// counter counts something else -- so the three are named here rather than
    /// written as a literal at each call site.
    pub const fn cache(name: &'static str, cache: u32, operation: u32, result: u32) -> Self {
        Self {
            name,
            kind: sys::bindings::PERF_TYPE_HW_CACHE,
            config: (cache as u64) | ((operation as u64) << 8) | ((result as u64) << 16),
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

/// Which word of the attribute a `format` term lives in, and at which bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatBit {
    Config(u32),
    Config2(u32),
}

/// Reads a one-bit term of a unit's `format` directory, as `config:19`.
///
/// The layout of a unit's configuration is the kernel's to describe, and it
/// describes it here; a bit written into a config from memory of a datasheet
/// is the kind of error that makes an open succeed and measure something else.
/// A term that spans several bits is not a flag, and is not answered.
pub fn format_bit(pmu: &str, term: &str) -> Option<FormatBit> {
    parse_format_bit(&read_trimmed(&format!(
        "{PMU_DEVICES}/{pmu}/format/{term}"
    ))?)
}

fn parse_format_bit(text: &str) -> Option<FormatBit> {
    let (word, bit) = text.trim().split_once(':')?;
    if bit.contains('-') || bit.contains(',') {
        return None;
    }
    let bit: u32 = bit.parse().ok()?;
    if bit >= 64 {
        return None;
    }
    match word {
        "config" => Some(FormatBit::Config(bit)),
        "config2" => Some(FormatBit::Config2(bit)),
        _ => None,
    }
}

/// AMD's instruction-based sampling of ops, as this machine publishes it.
///
/// The unit tags one micro-op every so often and follows it to retirement, so
/// what comes back is not a counter overflowing at some instruction nearby but
/// the op itself: the address it loaded from, which level answered, and how
/// many cycles the answer took. That is the one thing a counting PMU cannot
/// say, and the reason `mem.latency` and `mem.sharing` go through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IbsOp {
    /// The type number the kernel assigned this unit at boot.
    pub kind: u32,
    /// `cnt_ctl`: count dispatched ops rather than cycles between two tags.
    pub count_ops: Option<FormatBit>,
    /// `swfilt`: let the kernel discard samples by privilege level. Without it
    /// the unit refuses any `exclude_*` bit outright.
    pub swfilt: Option<FormatBit>,
}

impl IbsOp {
    pub const PMU: &'static str = "ibs_op";

    /// The unit, when the machine has one.
    pub fn probe() -> Option<Self> {
        let kind = read_trimmed(&format!("{PMU_DEVICES}/{}/type", Self::PMU))?
            .parse()
            .ok()?;
        Some(Self {
            kind,
            count_ops: format_bit(Self::PMU, "cnt_ctl"),
            swfilt: format_bit(Self::PMU, "swfilt"),
        })
    }

    /// Whether this unit can leave the kernel's own accesses out.
    pub fn can_exclude_kernel(&self) -> bool {
        self.swfilt.is_some()
    }

    /// A sampling event over this unit.
    ///
    /// Ops rather than cycles are counted between two tags whenever the unit
    /// allows it. Counting cycles tags whatever op is dispatched when the
    /// counter runs out, and a core stalled on a miss dispatches nothing, so
    /// the ops around a long stall are under-represented in exactly the
    /// measurement meant to find them. Counting ops tags one op in every so
    /// many, whatever they cost.
    pub fn spec(&self, rate: Rate, exclude_kernel: bool, inherit: bool) -> SampleSpec {
        let mut config = 0u64;
        let mut config2 = 0u64;
        let mut set = |bit: FormatBit| match bit {
            FormatBit::Config(bit) => config |= 1 << bit,
            FormatBit::Config2(bit) => config2 |= 1 << bit,
        };
        if let Some(bit) = self.count_ops {
            set(bit);
        }
        let exclude_kernel = exclude_kernel && self.can_exclude_kernel();
        if exclude_kernel {
            set(self.swfilt.expect("checked above"));
        }
        SampleSpec {
            kind: self.kind,
            config,
            config2,
            rate,
            exclude_kernel,
            inherit,
            callchain: Callchain::Off,
            memory: true,
        }
    }

    /// Whether `count_ops` was honoured, for the record that travels with the
    /// numbers.
    pub fn counts_ops(&self) -> bool {
        self.count_ops.is_some()
    }
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
            let (pid, cpu, flags) = scope.arguments();
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
            if matches!(scope, Scope::Process(_) | Scope::TaskOnCpu { .. }) {
                attr.set_inherit(1);
            }
            let leader = group.fds.first().copied().unwrap_or(-1);
            let fd = unsafe { sys::perf_event_open(&mut attr, pid, cpu, leader, flags) };
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

/// Where the kernel lists the cpus it currently has.
pub const ONLINE_CPUS: &str = "/sys/devices/system/cpu/online";

/// The cpus this machine has right now, as the kernel numbers them.
///
/// Read rather than derived from a core count: the numbering has holes on a
/// machine where a cpu was taken offline, and `nproc` would hand back a count
/// that indexes cpus which do not exist. A per-cpu event opened on one of those
/// fails with `ENODEV`, which reads like a permission problem and is not.
pub fn online_cpus() -> io::Result<Vec<u32>> {
    let list = std::fs::read_to_string(ONLINE_CPUS)?;
    parse_cpu_list(&list).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{ONLINE_CPUS} does not read as a cpu list: `{}`",
                list.trim()
            ),
        )
    })
}

/// `0-3,8,10-11`, which is how the kernel writes every cpu list it publishes.
pub fn parse_cpu_list(list: &str) -> Option<Vec<u32>> {
    let mut cpus = Vec::new();
    for part in list
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        match part.split_once('-') {
            Some((first, last)) => {
                let first: u32 = first.parse().ok()?;
                let last: u32 = last.parse().ok()?;
                if last < first {
                    return None;
                }
                cpus.extend(first..=last);
            }
            None => cpus.push(part.parse().ok()?),
        }
    }
    Some(cpus)
}

/// A cpu list as a person would say it, for a message about one that is not in
/// it.
fn describe_cpu_list(cpus: &[u32]) -> String {
    match (cpus.first(), cpus.last()) {
        (Some(first), Some(last)) if cpus.len() as u32 == last - first + 1 => {
            format!("cpus {first} to {last}")
        }
        _ => format!(
            "cpus {}",
            cpus.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// What `/proc/sys/kernel/perf_event_paranoid` says, when it can be read.
fn paranoid() -> Option<i32> {
    std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Why a per-cpu event was refused, in terms of the two things that grant it.
///
/// Counting one cpu means counting every task on it, including tasks of other
/// users, so the kernel asks for more than counting your own process does:
/// `perf_event_paranoid` at zero or below, or `CAP_PERFMON` on this binary.
/// Naming which one is missing is the difference between a message someone can
/// act on and one that sends them to the wrong file -- and the alternative,
/// falling back to a per-task count, would answer a different question under
/// the same name.
pub fn per_cpu_refusal(cpu: u32, error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EPERM) => {
            let level = match paranoid() {
                Some(level) => format!("it is {level} here"),
                None => "it could not be read here".to_owned(),
            };
            format!(
                "counting cpu {cpu} observes every task on it, which needs \
                 /proc/sys/kernel/perf_event_paranoid at 0 or below ({level}) or CAP_PERFMON on \
                 this binary; a per-task count is not the same measurement and is not \
                 substituted"
            )
        }
        _ => format!(
            "perf_event_open on cpu {cpu} was refused ({error}); this cpu is online, so it is \
             the counter rather than the core that is not there"
        ),
    }
}

/// One counter group per cpu, opened and released together.
///
/// A counter follows either a task or a cpu, never both, so counting a machine
/// is not one event but one per cpu. They are kept apart rather than added up:
/// a sum over twenty-four cores describes no core, and one core saturated among
/// twenty-three idle ones reads exactly like a machine at four percent.
/// Aggregating a series is something the emit layer does on request; recovering
/// a cpu from a sum is not something anything can do.
pub struct PerCpuGroups {
    groups: Vec<(u32, CounterGroup)>,
}

impl PerCpuGroups {
    /// Opens the counters on every named cpu.
    ///
    /// Every cpu has to open, and the first refusal is the whole answer: a set
    /// of cpus where some counted and some did not is not the measurement that
    /// was asked for, and reporting the ones that worked would hide the ones
    /// that did not behind a plausible number.
    pub fn open(counters: &[Counter], cpus: &[u32]) -> Result<Self, String> {
        if cpus.is_empty() {
            return Err("no cpu to count".to_owned());
        }
        // Asked before opening anything, because the kernel answers `EINVAL`
        // for a cpu that does not exist -- the same errno it gives for an
        // attribute it cannot honour. Reading the list the kernel publishes
        // tells the two apart, and they send the reader to different places:
        // one is a cpu number, the other is a counter this hardware lacks.
        if let Ok(online) = online_cpus() {
            if let Some(offline) = cpus.iter().find(|cpu| !online.contains(cpu)) {
                return Err(format!(
                    "cpu {offline} is not online, so there is no counter to open on it; this \
                     machine has {}",
                    describe_cpu_list(&online)
                ));
            }
        }
        let mut groups = Vec::with_capacity(cpus.len());
        for &cpu in cpus {
            match CounterGroup::open(counters, Scope::Cpu(cpu)) {
                Ok(group) => groups.push((cpu, group)),
                Err(error) => return Err(per_cpu_refusal(cpu, &error)),
            }
        }
        Ok(Self { groups })
    }

    /// True when the machine would only open these counters for user mode, so
    /// what they count is a share of what was asked for.
    pub fn kernel_excluded(&self) -> bool {
        self.groups.iter().any(|(_, group)| group.kernel_excluded())
    }

    pub fn cpus(&self) -> Vec<u32> {
        self.groups.iter().map(|(cpu, _)| *cpu).collect()
    }

    /// Starts every cpu's group.
    ///
    /// The groups are enabled one after another, so the windows differ by the
    /// few microseconds between two ioctls. Each group reports the window it
    /// was itself enabled for, which is what a rate has to be divided by --
    /// using the wall clock instead would credit one cpu with a window it never
    /// had.
    pub fn enable(&self) -> Result<(), String> {
        for (cpu, group) in &self.groups {
            group
                .enable()
                .map_err(|error| format!("enabling the counters on cpu {cpu} failed: {error}"))?;
        }
        Ok(())
    }

    /// Stops and reads every cpu, keeping each cpu's answer with its number.
    ///
    /// A cpu that went offline mid-run is a read that failed, not a run that
    /// failed: the others counted a real window and saying so beside the
    /// failure is more useful than discarding them.
    pub fn read_final(&self) -> Vec<(u32, io::Result<Vec<Reading>>)> {
        // Every group is stopped before any is read, so the cpus describe one
        // window rather than a staircase of them.
        for (_, group) in &self.groups {
            let _ = group.disable();
        }
        self.groups
            .iter()
            .map(|(cpu, group)| (*cpu, group.read()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE_FAULTS: u64 = sys::bindings::PERF_COUNT_SW_PAGE_FAULTS as u64;

    fn callchain_of_fixed(body: &[u8]) -> Vec<u64> {
        callchain_of(body, FIXED_FIELDS)
    }

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
            callchain_of_fixed(&record(3, &[0xa, 0xb, 0xc])),
            vec![0xa, 0xb, 0xc]
        );
        // A count larger than the record can hold yields what the record holds.
        assert_eq!(callchain_of_fixed(&record(99, &[0xa, 0xb])), vec![0xa, 0xb]);
        assert_eq!(callchain_of_fixed(&record(0, &[])), Vec::<u64>::new());
        // A record too short to even hold the count is not a callchain.
        assert_eq!(callchain_of_fixed(&[0u8; 16]), Vec::<u64>::new());
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

    /// The kernel writes every cpu list it publishes this way, and a list read
    /// wrongly opens counters on cpus that are not there.
    #[test]
    fn a_cpu_list_reads_the_way_the_kernel_writes_one() {
        assert_eq!(parse_cpu_list("0-3"), Some(vec![0, 1, 2, 3]));
        assert_eq!(parse_cpu_list("0"), Some(vec![0]));
        // Holes are the whole reason this is read rather than counted: a
        // machine with cpu 2 offline numbers the rest as it always did.
        assert_eq!(parse_cpu_list("0-1,3,5-6"), Some(vec![0, 1, 3, 5, 6]));
        assert_eq!(parse_cpu_list("0-3\n"), Some(vec![0, 1, 2, 3]));
        assert_eq!(parse_cpu_list(""), Some(Vec::new()));
        assert_eq!(parse_cpu_list("3-1"), None);
        assert_eq!(parse_cpu_list("all"), None);
    }

    /// Every cpu the kernel says is online has to have a directory, or the list
    /// being read is not the list of cpus.
    #[test]
    fn the_online_cpus_are_the_ones_this_machine_has() {
        let cpus = online_cpus().expect("this kernel publishes its online cpus");
        assert!(!cpus.is_empty(), "no cpu is online, which cannot be");
        for cpu in &cpus {
            assert!(
                std::path::Path::new(&format!("/sys/devices/system/cpu/cpu{cpu}")).exists(),
                "cpu{cpu} is listed as online and has no directory"
            );
        }
        // The kernel's own count of possible cpus is the upper bound.
        assert!(cpus.len() <= num_cpus_from_sysconf());
    }

    fn num_cpus_from_sysconf() -> usize {
        // SAFETY: a read-only query of a configuration value.
        let count = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
        count.max(1) as usize
    }

    /// The refusal a per-cpu event gets is a privilege answer, and it has to
    /// name the two things that grant it rather than the syscall that failed.
    #[test]
    fn a_refused_per_cpu_event_names_what_would_grant_it() {
        let denied = io::Error::from_raw_os_error(libc::EACCES);
        let message = per_cpu_refusal(3, &denied);
        assert!(message.contains("perf_event_paranoid"), "{message}");
        assert!(message.contains("CAP_PERFMON"), "{message}");
        // And says what it will not do instead, because a per-task count under
        // a per-cpu name is the failure this whole path exists to avoid.
        assert!(
            message.contains("per-task count is not the same"),
            "{message}"
        );

        // Everything else is about the counter, not about permission, and must
        // not send the reader to a sysctl that was never in the way.
        let invalid = io::Error::from_raw_os_error(libc::EINVAL);
        let message = per_cpu_refusal(3, &invalid);
        assert!(!message.contains("CAP_PERFMON"), "{message}");
        assert!(message.contains("this cpu is online"), "{message}");
    }

    /// A cpu that does not exist and a counter that does not exist come back
    /// from the kernel as the same `EINVAL`. Only the published list of online
    /// cpus tells them apart, and they send a reader to different places.
    #[test]
    fn a_cpu_that_is_not_online_is_refused_as_absent_rather_than_as_forbidden() {
        let beyond = online_cpus()
            .expect("online cpus")
            .iter()
            .max()
            .copied()
            .expect("a cpu")
            + 1;
        let Err(error) = PerCpuGroups::open(&[INSTRUCTIONS], &[beyond]) else {
            panic!("a cpu this machine does not have cannot be counted");
        };
        assert!(
            error.contains(&format!("cpu {beyond} is not online")),
            "{error}"
        );
        assert!(error.contains("this machine has cpus"), "{error}");
        assert!(!error.contains("perf_event_paranoid"), "{error}");
    }

    /// A per-cpu group counts a cpu, not a task: the work below happens on one
    /// cpu of many, and the group still has to come back with a reading for
    /// every cpu it was opened on.
    #[test]
    fn a_group_per_cpu_reports_one_reading_per_cpu() {
        let cpus = online_cpus().expect("online cpus");
        let Ok(groups) = PerCpuGroups::open(&[INSTRUCTIONS], &cpus) else {
            eprintln!("skipped: this machine will not open a per-cpu counter");
            return;
        };
        assert_eq!(groups.cpus(), cpus);
        groups.enable().expect("enable");
        busy();
        let readings = groups.read_final();

        assert_eq!(readings.len(), cpus.len());
        let mut counted = 0u64;
        for (cpu, reading) in &readings {
            let reading = reading.as_ref().expect("a cpu that is online reads");
            assert_eq!(reading.len(), 1);
            assert!(
                reading[0].enabled_ns > 0,
                "cpu {cpu} reports a window of no length"
            );
            counted += reading[0].value;
        }
        // The work above ran somewhere, and a set covering every cpu contains
        // wherever that was.
        assert!(
            counted > 1_000_000,
            "every cpu of this machine together retired {counted} instructions while a \
             two-million-iteration loop ran"
        );
    }

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

    /// The position of every field after the callchain depends on the
    /// callchain's length, so the layout is decoded field by field from the
    /// event's own `sample_type`. A decoder with fixed offsets reads the data
    /// source out of a frame.
    #[test]
    fn a_memory_sample_is_read_in_the_layout_its_fields_give_it() {
        use sys::bindings as b;
        let data_src = (u64::from(b::PERF_MEM_OP_LOAD) << b::PERF_MEM_OP_SHIFT)
            | (u64::from(b::PERF_MEM_LVLNUM_RAM) << b::PERF_MEM_LVLNUM_SHIFT);
        let weight = 309u64 | (512u64 << 32);
        let record = |frames: &[u64]| {
            let mut body = vec![0u8; 8];
            let mut words = vec![0x401000u64, (7u64 << 32) | 6, 1_000, 0xdead_0040, 5];
            words.push(frames.len() as u64);
            words.extend_from_slice(frames);
            words.extend_from_slice(&[weight, data_src]);
            for word in words {
                body.extend_from_slice(&word.to_ne_bytes());
            }
            body
        };
        let spec = SampleSpec {
            callchain: Callchain::FramePointers,
            memory: true,
            ..SampleSpec::software(0, 1, Callchain::Off)
        };
        for frames in [&[][..], &[0xa, 0xb, 0xc][..]] {
            let sample = decode_sample(&record(frames), 0, spec.sample_type());
            assert_eq!((sample.pid, sample.tid), (6, 7));
            assert_eq!(sample.addr, 0xdead_0040);
            assert_eq!(sample.cpu, Some(5));
            assert_eq!(sample.callchain.as_deref(), Some(frames));
            let access = sample.access.expect("the record carries an access");
            assert_eq!(access.latency_cycles, 309);
            assert_eq!(access.retire_cycles, 512);
            assert_eq!(access.source.op(), MemOp::Load);
            assert_eq!(access.source.level(), MemLevel::Ram);
        }

        // Without the memory fields, nothing is read past the fixed four.
        let plain = decode_sample(&record(&[]), 0, SAMPLE_TYPE);
        assert_eq!((plain.cpu, plain.access), (None, None));
    }

    /// Each value here is one the kernel's IBS decoder writes on AMD family
    /// 17h, taken apart the way `perf mem report` takes it apart.
    #[test]
    fn a_data_source_names_the_level_that_answered() {
        use sys::bindings as b;
        let source = |op: u32, lvlnum: u32, remote: bool, snoop: u32| {
            DataSource(
                (u64::from(op) << b::PERF_MEM_OP_SHIFT)
                    | (u64::from(lvlnum) << b::PERF_MEM_LVLNUM_SHIFT)
                    | (u64::from(remote) << b::PERF_MEM_REMOTE_SHIFT)
                    | (u64::from(snoop) << b::PERF_MEM_SNOOP_SHIFT),
            )
        };
        let l1 = source(b::PERF_MEM_OP_LOAD, b::PERF_MEM_LVLNUM_L1, false, 0);
        assert_eq!(
            (l1.op(), l1.level(), l1.hitm()),
            (MemOp::Load, MemLevel::L1, false)
        );
        let peer = source(
            b::PERF_MEM_OP_LOAD,
            b::PERF_MEM_LVLNUM_ANY_CACHE,
            false,
            b::PERF_MEM_SNOOP_HITM,
        );
        assert_eq!((peer.level(), peer.hitm()), (MemLevel::Peer, true));
        let far = source(b::PERF_MEM_OP_LOAD, b::PERF_MEM_LVLNUM_RAM, true, 0);
        assert_eq!(far.level(), MemLevel::RemoteRam);
        // An op the unit did not describe is not a load that hit somewhere.
        let none = source(b::PERF_MEM_OP_NA, b::PERF_MEM_LVLNUM_NA, false, 0);
        assert_eq!((none.op(), none.level()), (MemOp::None, MemLevel::Unknown));
        // The older mask, for a kernel that fills only that.
        let old = DataSource(
            u64::from(b::PERF_MEM_LVL_HIT | b::PERF_MEM_LVL_L2) << b::PERF_MEM_LVL_SHIFT,
        );
        assert_eq!(old.level(), MemLevel::L2);
        let missed = DataSource(
            u64::from(b::PERF_MEM_LVL_MISS | b::PERF_MEM_LVL_L1) << b::PERF_MEM_LVL_SHIFT,
        );
        assert_eq!(missed.level(), MemLevel::Unknown);
    }

    #[test]
    fn a_format_term_is_a_word_and_a_bit() {
        assert_eq!(parse_format_bit("config:19"), Some(FormatBit::Config(19)));
        assert_eq!(parse_format_bit("config2:0\n"), Some(FormatBit::Config2(0)));
        // A field, not a flag: setting its first bit would be a guess.
        assert_eq!(parse_format_bit("config:0-7"), None);
        assert_eq!(parse_format_bit("config1:3"), None);
        assert_eq!(parse_format_bit("config:64"), None);
    }

    /// A shuffled chain of pointers, one per cache line, so that every load
    /// depends on the one before it and no prefetcher can guess the next line.
    fn chain(bytes: usize) -> Vec<usize> {
        let slots = bytes / 64;
        let mut chain = vec![0usize; slots * 8];
        let mut order: Vec<usize> = (0..slots).collect();
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for index in (1..slots).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            order.swap(index, (state % (index as u64 + 1)) as usize);
        }
        for window in 0..slots {
            chain[order[window] * 8] = order[(window + 1) % slots] * 8;
        }
        chain
    }

    fn chase(chain: &[usize], steps: u64) {
        let mut at = 0usize;
        for _ in 0..steps {
            at = unsafe { std::ptr::read_volatile(chain.as_ptr().add(at)) };
        }
        std::hint::black_box(at);
    }

    /// The loads this thread made inside `range`, by the level that answered.
    fn levels_of(event: &mut SampledEvent, range: (u64, u64)) -> Vec<(MemLevel, u32)> {
        let mut seen = Vec::new();
        event.drain(|record| {
            if let Record::Sample(sample) = record {
                let Some(access) = sample.access else { return };
                if access.source.op() == MemOp::Load && (range.0..range.1).contains(&sample.addr) {
                    seen.push((access.source.level(), access.latency_cycles));
                }
            }
        });
        seen
    }

    /// Ground truth that needs no reference counter: a chain that fits in L1
    /// and one four times larger than this machine's whole L3, walked by the
    /// same code in the same thread. Which level answers is decided by the
    /// working set, not by the tool, so the hardware has to say L1 for most of
    /// one and memory for most of the other -- and a latency only for the
    /// second, since family 17h measures one only for a load that missed.
    ///
    /// Asserted as a separation, not a threshold: other tests share this
    /// machine's caches, and a quiet machine and a loaded one differ in how
    /// far apart the two walks land, not in which side each lands on.
    #[test]
    fn an_ibs_sample_says_which_level_answered_the_load() {
        let Some(ibs) = IbsOp::probe() else {
            eprintln!("skipped: this machine publishes no `ibs_op` unit (not AMD, or too old)");
            return;
        };
        let tid = unsafe { libc::gettid() };
        let spec = ibs.spec(Rate::Hz(20_000), true, false);
        let mut event = match SampledEvent::open(&spec, Scope::Process(tid), 64) {
            Ok(event) => event,
            Err(error) => {
                eprintln!("skipped: IBS op sampling of this thread was refused: {error}");
                return;
            }
        };

        // Built before the window opens: building a chain is itself a quarter
        // of a million stores, and they would fill the ring before the walk
        // began.
        let mut walk = |bytes: usize| {
            let chain = chain(bytes);
            let base = chain.as_ptr() as u64;
            let range = (base, base + (chain.len() * 8) as u64);
            let mut loads = Vec::new();
            // Enabled once, for a fixed time, and drained while it runs, the
            // way a collector drains on its tick. Re-enabling restarts the
            // kernel's adjustment of the period to the frequency, and in a
            // debug build only about one op in a hundred is the chain's own
            // load -- the rest is the loop around it.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
            event.enable().expect("enable");
            while std::time::Instant::now() < deadline {
                chase(&chain, 20_000);
                loads.extend(levels_of(&mut event, range));
            }
            event.disable().expect("disable");
            loads.extend(levels_of(&mut event, range));
            loads
        };
        let small = walk(4096);
        let large = walk(256 << 20);
        assert_eq!(
            event.lost(),
            0,
            "the ring overflowed, so the shares below are of a subset"
        );

        let share = |loads: &[(MemLevel, u32)], keep: fn(MemLevel) -> bool| {
            loads.iter().filter(|(at, _)| keep(*at)).count() as f64 / loads.len().max(1) as f64
        };
        let in_l1 = |level: MemLevel| level == MemLevel::L1;
        let beyond_l1 = |level: MemLevel| !matches!(level, MemLevel::L1 | MemLevel::Unknown);
        let measured = |loads: &[(MemLevel, u32)]| {
            loads.iter().filter(|(_, latency)| *latency > 0).count() as f64
                / loads.len().max(1) as f64
        };
        eprintln!(
            "4 KiB: {} loads, {:.2} in L1, {:.2} with a latency; 256 MiB: {} loads, {:.2} beyond \
             L1, {:.2} with a latency",
            small.len(),
            share(&small, in_l1),
            measured(&small),
            large.len(),
            share(&large, beyond_l1),
            measured(&large),
        );
        assert!(
            small.len() >= 20 && large.len() >= 20,
            "too few samples to say anything"
        );
        // Which level beyond L1 is not asserted: this thread's walk has been
        // seen answered by memory and by L3 in two runs on a loaded machine,
        // and the level a miss lands in is the machine's business. That it
        // missed is the working set's.
        assert!(
            share(&small, in_l1) > 0.5,
            "a 4 KiB chain is answered by L1, and the hardware said so for only {:.2} of it",
            share(&small, in_l1)
        );
        assert!(
            share(&large, beyond_l1) > 0.5,
            "a 256 MiB chain misses L1, and the hardware said so for only {:.2} of it",
            share(&large, beyond_l1)
        );
        assert!(
            measured(&large) > measured(&small) + 0.4,
            "a load that misses carries a latency and one that hits does not"
        );
    }
}
