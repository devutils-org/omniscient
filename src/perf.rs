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
        if self.callchain.asks_callchain() {
            sample_type |= sys::bindings::PERF_SAMPLE_CALLCHAIN as u64;
        }
        if self.callchain.user_regs() {
            sample_type |= (sys::bindings::PERF_SAMPLE_REGS_USER
                | sys::bindings::PERF_SAMPLE_STACK_USER) as u64;
        }
        if self.memory {
            sample_type |= (sys::bindings::PERF_SAMPLE_CPU
                | sys::bindings::PERF_SAMPLE_WEIGHT_STRUCT
                | sys::bindings::PERF_SAMPLE_DATA_SRC) as u64;
        }
        // In frequency mode the kernel adjusts each sample's period as it
        // goes, and a sample stands for that many events: perf weighs its
        // shares by it, and so does a profile that reads it.
        if matches!(self.rate, Rate::Hz(_)) {
            sample_type |= sys::bindings::PERF_SAMPLE_PERIOD as u64;
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
    /// The task's user registers at the sample, when the event asked for
    /// them. `None` for a sample of a kernel thread, which has none.
    pub user_regs: Option<UserRegs>,
    /// The top of the task's user stack, from the stack pointer up, when the
    /// event asked for it: as many bytes as the kernel could copy, which is
    /// fewer than asked where the stack is shallower.
    pub user_stack: Option<Vec<u8>>,
    /// The cpu the sample was taken on, when the event asked for it.
    pub cpu: Option<u32>,
    /// Where the sampled access went and how long it waited, when the event
    /// asked for it. See [`SampleSpec::memory`].
    pub access: Option<MemoryAccess>,
    /// How many occurrences of the event this sample stands for, when the
    /// event asked for it: in frequency mode, the period the kernel chose
    /// for this sample, which changes from one to the next.
    pub period: Option<u64>,
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

/// What the kernel is asked to hand over at every sample so that its stack can
/// be unwound.
///
/// This is a request to the kernel, not a post-processing choice: the walk
/// happens in the interrupt handler, with the sampled task's registers still
/// live, and nothing afterwards can recover a stack that was not taken then.
/// Each unwind mode of [`crate::request::StackMode`] is one of these shapes,
/// and [`crate::stack::choose`] is what picks it.
///
/// `kernel` is whether the kernel half of the stack is asked for. It is false
/// where `perf_event_paranoid` refuses kernel profiling to this user (2, the
/// kernel's default): the event is then opened with `exclude_kernel`, so a
/// sample taken while the task ran in the kernel is not delivered at all, and
/// the stacks that are delivered hold only the task's own frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Callchain {
    /// No stack. A sample is an instruction pointer and nothing else.
    #[default]
    Off,
    /// The kernel walks frame pointers, in kernel mode and then in the target's
    /// own mode, and writes both chains with a marker between them.
    ///
    /// Cheap -- a word per frame -- and worth exactly what the target's build is
    /// worth: a program compiled without frame pointers has no chain to walk
    /// and yields its leaf frame alone.
    FramePointers { kernel: bool },
    /// The frame-pointer chain, plus the user registers and the top
    /// [`LEAF_STACK_BYTES`] of the user stack: enough to find, from the leaf's
    /// call frame information, the return address a frameless leaf left there
    /// and which the frame-pointer walk steps over.
    FramePointersAndLeaf { kernel: bool },
    /// The user registers and [`COPIED_STACK_BYTES`] of the user stack, for an
    /// unwinder in this process to walk from `.eh_frame`. The kernel still
    /// walks its own half, when `kernel`, and not the user one.
    CopiedStack { kernel: bool },
}

/// How much of the user stack `fp+leaf` copies with each sample.
///
/// A frameless leaf keeps its return address at the stack pointer plus what
/// the leaf itself pushed or reserved, which for the functions that go
/// frameless -- small, register-only -- is a few words. 256 bytes is 32 of
/// them; a leaf that reserved more is reported as not recovered, not guessed.
pub const LEAF_STACK_BYTES: u32 = 256;

/// How much of the user stack `dwarf` copies with each sample: what `perf
/// record --call-graph dwarf` copies by default. A stack deeper than this is
/// unwound as far as the copy reaches, and said to be cut there.
pub const COPIED_STACK_BYTES: u32 = 8192;

impl Callchain {
    /// Both halves of the frame-pointer walk, the shape every profile had
    /// before the unwind modes.
    pub const BOTH_HALVES: Callchain = Callchain::FramePointers { kernel: true };

    /// Whether the kernel half is asked for.
    pub fn kernel(self) -> bool {
        match self {
            Callchain::Off => false,
            Callchain::FramePointers { kernel }
            | Callchain::FramePointersAndLeaf { kernel }
            | Callchain::CopiedStack { kernel } => kernel,
        }
    }

    /// The same shape with the kernel half dropped.
    pub fn user_only(self) -> Callchain {
        match self {
            Callchain::Off => Callchain::Off,
            Callchain::FramePointers { .. } => Callchain::FramePointers { kernel: false },
            Callchain::FramePointersAndLeaf { .. } => {
                Callchain::FramePointersAndLeaf { kernel: false }
            }
            Callchain::CopiedStack { .. } => Callchain::CopiedStack { kernel: false },
        }
    }

    /// Whether the event is opened with `exclude_kernel`, because the kernel
    /// half was not asked for.
    fn excludes_kernel(self) -> bool {
        self != Callchain::Off && !self.kernel()
    }

    /// Whether the kernel writes a `PERF_SAMPLE_CALLCHAIN` at all.
    fn asks_callchain(self) -> bool {
        match self {
            Callchain::Off => false,
            Callchain::FramePointers { .. } | Callchain::FramePointersAndLeaf { .. } => true,
            Callchain::CopiedStack { kernel } => kernel,
        }
    }

    /// The bytes of user stack copied with each sample, zero for none.
    pub fn user_stack_bytes(self) -> u32 {
        match self {
            Callchain::Off | Callchain::FramePointers { .. } => 0,
            Callchain::FramePointersAndLeaf { .. } => LEAF_STACK_BYTES,
            Callchain::CopiedStack { .. } => COPIED_STACK_BYTES,
        }
    }

    /// Whether the user registers come with each sample.
    pub fn user_regs(self) -> bool {
        self.user_stack_bytes() > 0
    }
}

/// The user registers an unwinder starts from: the instruction pointer, the
/// stack pointer and the frame pointer, and on aarch64 the link register.
///
/// Only these, because they are the only ones a return address is computed
/// from in practice (`framehop`'s own reasoning, and `perf`'s).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UserRegs {
    pub ip: u64,
    pub sp: u64,
    pub fp: u64,
    /// aarch64's link register; zero on x86_64, which has none.
    pub lr: u64,
}

/// `perf_event_attr.sample_regs_user` for [`UserRegs`], and the order the
/// kernel writes them in: by bit, lowest first.
#[cfg(target_arch = "x86_64")]
const USER_REGS_MASK: u64 = (1 << 6) | (1 << 7) | (1 << 8); // bp, sp, ip
#[cfg(target_arch = "aarch64")]
const USER_REGS_MASK: u64 = (1 << 29) | (1 << 30) | (1 << 31) | (1 << 32); // x29, lr, sp, pc
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const USER_REGS_MASK: u64 = 0;

impl UserRegs {
    fn from_words(words: &[u64]) -> Option<Self> {
        #[cfg(target_arch = "x86_64")]
        {
            let [bp, sp, ip] = words.try_into().ok()?;
            Some(Self {
                ip,
                sp,
                fp: bp,
                lr: 0,
            })
        }
        #[cfg(target_arch = "aarch64")]
        {
            let [fp, lr, sp, ip] = words.try_into().ok()?;
            Some(Self { ip, sp, fp, lr })
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            let _ = words;
            None
        }
    }
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
pub const MIN_RING_PAGES: usize = 8;

/// Whether an error means "that ring is bigger than you are allowed to map".
pub fn is_ring_too_large(error: &io::Error) -> bool {
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
    /// Throttled periods, accumulated across drains. See
    /// [`SampledEvent::throttled`].
    throttled: u64,
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
    ///
    /// Yes when either shape opens: both halves, or the user half alone. At
    /// `perf_event_paranoid` 2 -- the kernel's default -- only the second does,
    /// and a profile of the user's own program is still a profile;
    /// [`kernel_callchain_allowed`] is what tells the two apart, and
    /// [`crate::stack::choose`] is where the choice is made and written down.
    pub fn callchain_supported() -> io::Result<()> {
        if kernel_callchain_allowed() {
            return Ok(());
        }
        probe_open(Callchain::FramePointers { kernel: false })
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
            throttled: 0,
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

    /// Throttled periods seen since the event was opened, accumulated across
    /// drains the way [`SampledEvent::lost`] is.
    ///
    /// Kept here rather than by each collector, so that a sampling collector
    /// reports throttling by default rather than by remembering to: one that
    /// matched `Record::Throttle { .. } => {}` read as complete while the
    /// kernel skipped samples. `PERF_RECORD_UNTHROTTLE` closes a period and is
    /// not a second one.
    pub fn throttled(&self) -> u64 {
        self.throttled
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
                Record::Throttle { throttled: true } => self.throttled += 1,
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
    // After `cpu` and before `read` and the callchain, in bit order.
    let period = asked(b::PERF_SAMPLE_PERIOD).then(&mut next);
    let callchain = asked(b::PERF_SAMPLE_CALLCHAIN).then(|| {
        let chain = callchain_of(body, index);
        index += 1 + chain.len();
        chain
    });
    // `abi`, then one word per register in the mask, lowest bit first; no
    // registers at all for a task with no user half.
    let user_regs = if asked(b::PERF_SAMPLE_REGS_USER) {
        let abi = word_at(body, index).unwrap_or(0);
        index += 1;
        if abi == 0 {
            None
        } else {
            let count = USER_REGS_MASK.count_ones() as usize;
            let words: Vec<u64> = (0..count)
                .map(|offset| word_at(body, index + offset).unwrap_or(0))
                .collect();
            index += count;
            UserRegs::from_words(&words)
        }
    } else {
        None
    };
    // `size`, `size` bytes, and -- only when `size` is not zero -- `dyn_size`,
    // the part of them the kernel actually filled.
    let user_stack = if asked(b::PERF_SAMPLE_STACK_USER) {
        let size = word_at(body, index).unwrap_or(0) as usize;
        index += 1;
        let start = 8 + index * 8;
        let data = body
            .get(start..start + size)
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        index += size.div_ceil(8);
        if size > 0 {
            let filled = word_at(body, index).unwrap_or(0) as usize;
            index += 1;
            Some(data[..filled.min(data.len())].to_vec())
        } else {
            Some(Vec::new())
        }
    } else {
        None
    };
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
        user_regs,
        user_stack,
        cpu,
        access: match (weight, data_src) {
            (Some(weight), Some(data_src)) => Some(MemoryAccess::from_words(weight, data_src)),
            _ => None,
        },
        period,
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

/// Opens a never-enabled cpu-clock event over this process with the given
/// stack shape, and closes it: the one question only the kernel can answer.
fn probe_open(callchain: Callchain) -> io::Result<()> {
    SampledEvent::open_software_with(
        sys::bindings::PERF_COUNT_SW_CPU_CLOCK as u64,
        // Far longer than the probe lives: it is never enabled, and a period
        // this long would not fire if it were.
        u64::from(u32::MAX),
        Scope::Process(0),
        MIN_RING_PAGES,
        callchain,
    )
    .map(drop)
}

/// Whether this user may have the kernel half of a stack, which is whether it
/// may profile the kernel at all: `perf_event_paranoid` below 2, or
/// `CAP_PERFMON`.
///
/// Asked once per process and kept: the answer is the machine's, and every
/// profile of a run asks it.
pub fn kernel_callchain_allowed() -> bool {
    static ANSWER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ANSWER.get_or_init(|| probe_open(Callchain::BOTH_HALVES).is_ok())
}

/// `perf_event_paranoid is N`, as a test's skip line names the machine's
/// reason: the CI's cells are told apart by it (`.github/expected-skips*.txt`).
#[cfg(test)]
pub(crate) fn paranoid_said() -> String {
    match std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid") {
        Ok(text) => format!("perf_event_paranoid is {}", text.trim()),
        Err(error) => format!("perf_event_paranoid is unreadable ({error})"),
    }
}

/// Whether the kernel will copy the user registers and a slice of the user
/// stack with each sample, which `fp+leaf` and `dwarf` both need.
///
/// `PERF_SAMPLE_REGS_USER` and `PERF_SAMPLE_STACK_USER` are per-architecture
/// (`HAVE_PERF_USER_STACK_DUMP`), and a kernel that lacks them refuses the
/// open with `EINVAL` rather than quietly leaving them out.
pub fn user_stack_supported() -> io::Result<()> {
    static ANSWER: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    ANSWER
        .get_or_init(|| {
            if USER_REGS_MASK == 0 {
                return Err(format!(
                    "this build names no user registers for {}",
                    std::env::consts::ARCH
                ));
            }
            probe_open(Callchain::CopiedStack { kernel: false }).map_err(|error| error.to_string())
        })
        .clone()
        .map_err(io::Error::other)
}

/// Whether the cpu keeps a call stack in its branch records that this user
/// may sample: `PERF_SAMPLE_BRANCH_CALL_STACK`, Intel's LBR call-stack mode
/// since Haswell.
///
/// Asked by opening a cycles event that samples it, over this process and user
/// mode only. A cpu with no branch records at all, one whose records have no
/// call-stack mode, and a virtual machine that hides them, all refuse it.
pub fn branch_call_stack_supported() -> io::Result<()> {
    let mut attr: sys::bindings::perf_event_attr = unsafe { std::mem::zeroed() };
    attr.type_ = sys::bindings::PERF_TYPE_HARDWARE;
    attr.size = std::mem::size_of::<sys::bindings::perf_event_attr>() as u32;
    attr.config = sys::bindings::PERF_COUNT_HW_CPU_CYCLES as u64;
    attr.__bindgen_anon_1.sample_period = u64::from(u32::MAX);
    attr.sample_type =
        (sys::bindings::PERF_SAMPLE_IP | sys::bindings::PERF_SAMPLE_BRANCH_STACK) as u64;
    attr.branch_sample_type = (sys::bindings::PERF_SAMPLE_BRANCH_USER
        | sys::bindings::PERF_SAMPLE_BRANCH_CALL_STACK) as u64;
    attr.set_exclude_kernel(1);
    attr.set_exclude_hv(1);
    attr.set_disabled(1);
    let fd = unsafe { sys::perf_event_open(&mut attr, 0, -1, -1, 0) };
    if fd < 0 {
        let os = io::Error::last_os_error();
        return Err(io::Error::new(os.kind(), format!("perf_event_open: {os}")));
    }
    unsafe { libc::close(fd) };
    Ok(())
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
    if spec.callchain.asks_callchain() {
        // Both halves are asked for where the machine allows it. A kernel
        // chain without a user chain says a task is in `read`, and never which
        // of its own functions called it; a user chain without a kernel one
        // cannot tell work from waiting. `crate::stack` keeps them apart
        // afterwards.
        attr.sample_max_stack = max_stack_depth();
        if !spec.callchain.kernel() {
            attr.set_exclude_callchain_kernel(1);
        }
    }
    if let Callchain::CopiedStack { .. } = spec.callchain {
        // The user half is unwound here, from the copy; the kernel's own walk
        // of it would be a second, worse answer to the same question.
        attr.set_exclude_callchain_user(1);
    }
    if spec.callchain.user_regs() {
        attr.sample_regs_user = USER_REGS_MASK;
        attr.sample_stack_user = spec.callchain.user_stack_bytes();
    }
    if spec.exclude_kernel || spec.callchain.excludes_kernel() {
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
        // The kind is kept, so that a caller can still tell a refusal from a
        // missing counter: `per_cpu_refusal` reads it.
        let os = io::Error::last_os_error();
        return Err(io::Error::new(os.kind(), format!("perf_event_open: {os}")));
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

/// The threads a process has right now, from `/proc/<pid>/task`, or the pid
/// alone when that cannot be listed.
///
/// A launched program at its gate has one thread, so this is `[pid]` there and
/// the answer only matters for a process that is already running -- which is
/// every target of a late attach. A pid whose directory cannot be read is
/// answered with itself rather than an error: `perf_event_open` on it is what
/// decides whether it can be followed, and says why when it cannot.
pub fn threads_of(pid: i32) -> Vec<i32> {
    let listed = std::fs::read_dir(format!("/proc/{pid}/task")).map(|entries| {
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect::<Vec<i32>>()
    });
    match listed {
        Ok(mut tids) if pid > 0 && !tids.is_empty() => {
            tids.sort_unstable();
            tids
        }
        _ => vec![pid],
    }
}

/// Whether an open failed because the task had exited in between.
///
/// A thread that left between the listing and the open has nothing left to
/// count or sample, and skipping it is right; every other refusal is the run's.
fn is_gone(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ESRCH) || error.to_string().contains("No such process")
}

/// Threads of `pid` that exist now and are not in `opened`.
///
/// Asked once every event is open. `inherit` follows what a task starts *after*
/// its event exists, so a thread started between the listing and the moment
/// its creator's event was opened is followed by nothing; one started after
/// that moment is followed by its creator's. From here the two cannot be told
/// apart, and opening an event on every such thread would count the second
/// kind twice -- so they are counted, and the caller says how many.
fn started_while_opening(pid: i32, opened: &[i32]) -> usize {
    if opened.len() == 1 && opened[0] == pid {
        // Nothing was listed beyond the target itself, so nothing can have
        // been missed between a listing and an open.
        return 0;
    }
    threads_of(pid)
        .into_iter()
        .filter(|tid| !opened.contains(tid))
        .count()
}

/// Counters over every thread a process has, each following what it starts.
///
/// A per-task counter follows a process by inheritance, and inheritance is
/// from the open: opened at the launch gate, on a program with one thread, it
/// follows every thread the program will ever start; opened on a program that
/// already runs a second thread, it counts the thread it was opened on and
/// nothing else. That second case measured 106,206 instructions for a phase
/// that retired two thousand million, with no loss and no note. So a group is
/// opened on each entry of `/proc/<pid>/task`, each inheriting, and the
/// readings are their sum.
///
/// At the gate this is exactly one group, and nothing changes.
pub struct ThreadCounters {
    groups: Vec<CounterGroup>,
    tids: Vec<i32>,
    kernel_excluded: bool,
    unlisted: usize,
}

impl ThreadCounters {
    /// Opens the counters on every thread `pid` has now.
    ///
    /// Kernel mode is dropped for all of them or none: a sum of a count with
    /// kernel mode and one without is neither measurement.
    pub fn open(counters: &[Counter], pid: i32) -> io::Result<Self> {
        let mut opened = Self::open_tasks(counters, &threads_of(pid))?;
        opened.unlisted = started_while_opening(pid, &opened.tids);
        Ok(opened)
    }

    /// Opens the counters on one task, and what it starts: the `--tid` shape,
    /// where the other threads of its process are not what was asked about.
    pub fn open_task(counters: &[Counter], tid: i32) -> io::Result<Self> {
        Self::open_tasks(counters, &[tid])
    }

    fn open_tasks(counters: &[Counter], tasks: &[i32]) -> io::Result<Self> {
        let mut groups = Vec::new();
        let mut tids = Vec::new();
        let mut kernel_excluded = None;
        let mut first_error = None;
        for &tid in tasks {
            let opened = match kernel_excluded {
                None => CounterGroup::open(counters, Scope::Process(tid)),
                Some(excluded) => CounterGroup::open_with(counters, Scope::Process(tid), excluded),
            };
            match opened {
                Ok(group) => {
                    kernel_excluded.get_or_insert(group.kernel_excluded());
                    groups.push(group);
                    tids.push(tid);
                }
                Err(error) if is_gone(&error) => {
                    first_error.get_or_insert(error);
                }
                Err(error) => return Err(error),
            }
        }
        if groups.is_empty() {
            return Err(first_error.unwrap_or_else(|| io::Error::from_raw_os_error(libc::ESRCH)));
        }
        Ok(Self {
            groups,
            tids,
            kernel_excluded: kernel_excluded.unwrap_or(false),
            unlisted: 0,
        })
    }

    pub fn kernel_excluded(&self) -> bool {
        self.kernel_excluded
    }

    /// The threads a counter was opened on.
    pub fn threads(&self) -> &[i32] {
        &self.tids
    }

    /// Threads that appeared while the counters were being opened, each
    /// counted only if its creator's counter was already open. See
    /// [`started_while_opening`].
    pub fn unlisted(&self) -> usize {
        self.unlisted
    }

    pub fn enable(&self) -> io::Result<()> {
        self.groups.iter().try_for_each(CounterGroup::enable)
    }

    /// Stops every group, then reads them, and adds the readings up.
    ///
    /// Every group is stopped before any is read, so the sum is of one window
    /// to within the few microseconds the stops take, rather than to within
    /// the reads as well.
    pub fn read_final(&self) -> io::Result<Vec<Reading>> {
        for group in &self.groups {
            group.disable()?;
        }
        self.read()
    }

    /// Reads every thread's group while they run, and adds them: a poll's
    /// reading, whose difference from the previous one is a tick's count.
    pub fn read(&self) -> io::Result<Vec<Reading>> {
        let mut total: Option<Vec<Reading>> = None;
        for group in &self.groups {
            let readings = group.read()?;
            total = Some(match total {
                None => readings,
                Some(sum) => add_readings(&sum, &readings),
            });
        }
        Ok(total.unwrap_or_default())
    }
}

/// What the counters counted between two readings of one group: a tick's
/// share of the window, with the tick's own enabled and running times.
///
/// Each value was scaled over the whole window up to its reading, so a
/// difference of two scaled values is the tick's estimate; a tick the counter
/// was never scheduled in has `running_ns` 0 and says so the way a window
/// does.
pub fn delta_readings(now: &[Reading], before: &[Reading]) -> Vec<Reading> {
    now.iter()
        .zip(before)
        .map(|(now, before)| Reading {
            name: now.name,
            raw: now.raw.saturating_sub(before.raw),
            value: now.value.saturating_sub(before.value),
            enabled_ns: now.enabled_ns.saturating_sub(before.enabled_ns),
            running_ns: now.running_ns.saturating_sub(before.running_ns),
        })
        .collect()
}

/// Two groups' readings of the same counters, added.
///
/// The times add as well, which is the kernel's own convention for an
/// inherited counter: the reading of one that followed a thread into its
/// children carries their enabled and running times summed with its own. Each
/// value was scaled over its own window before being added, so a group that
/// was multiplexed and one that was not are each right before they are summed.
fn add_readings(left: &[Reading], right: &[Reading]) -> Vec<Reading> {
    left.iter()
        .zip(right)
        .map(|(a, b)| Reading {
            name: a.name,
            raw: a.raw + b.raw,
            value: a.value + b.value,
            enabled_ns: a.enabled_ns + b.enabled_ns,
            running_ns: a.running_ns + b.running_ns,
        })
        .collect()
}

/// A sampled event over tasks and everything they go on to start.
///
/// The kernel refuses to map a ring for an inherited event over every cpu at
/// once (`perf_mmap` returns EINVAL for `cpu == -1 && inherit`), because each
/// inheriting task would need a buffer of its own, and it accepts one for an
/// inherited event pinned to a cpu. So each task is sampled by one inherited
/// event per cpu, and the events of one cpu share that cpu's ring
/// ([`SampledEvent::follow`]): a ring per cpu, not a ring per task per cpu.
///
/// Each task listed is opened, not only the first, because `inherit` follows
/// what a task starts after its event exists: a program already running a
/// thread pool is followed whole only if every thread of the pool has an event
/// of its own.
pub struct ThreadSampler {
    rings: Vec<SampledEvent>,
    /// The cpu each ring is pinned to, in the order of `rings`.
    ring_cpus: Vec<u32>,
    tids: Vec<i32>,
    unlisted: usize,
}

impl ThreadSampler {
    /// Opens `spec`, inherited, on every thread `pid` has now, on every online
    /// cpu, with a ring of `ring_pages` per cpu.
    pub fn open_process(spec: &SampleSpec, pid: i32, ring_pages: usize) -> io::Result<Self> {
        let cpus = online_cpus()?;
        let mut sampler = Self::open(spec, &threads_of(pid), &cpus, ring_pages)?;
        sampler.unlisted = started_while_opening(pid, &sampler.tids);
        Ok(sampler)
    }

    /// Opens `spec`, inherited, on each of `tasks` on each of `cpus`.
    pub fn open(
        spec: &SampleSpec,
        tasks: &[i32],
        cpus: &[u32],
        ring_pages: usize,
    ) -> io::Result<Self> {
        let spec = SampleSpec {
            inherit: true,
            ..*spec
        };
        let mut rings = Vec::with_capacity(cpus.len());
        let mut ring_cpus = Vec::with_capacity(cpus.len());
        let mut followed = std::collections::BTreeSet::new();
        let mut first_error = None;
        for &cpu in cpus {
            let mut ring: Option<SampledEvent> = None;
            for &pid in tasks {
                let scope = Scope::TaskOnCpu { pid, cpu };
                let opened = match ring.as_mut() {
                    None => SampledEvent::open(&spec, scope, ring_pages).map(|event| {
                        ring = Some(event);
                    }),
                    Some(ring) => ring.follow(&spec, scope),
                };
                match opened {
                    Ok(()) => {
                        followed.insert(pid);
                    }
                    Err(error) if is_gone(&error) => {
                        first_error.get_or_insert(error);
                    }
                    Err(error) => return Err(error),
                }
            }
            if let Some(ring) = ring {
                rings.push(ring);
                ring_cpus.push(cpu);
            }
        }
        if rings.is_empty() {
            return Err(first_error.unwrap_or_else(|| {
                io::Error::other("there was no task and no cpu to open a sampled event on")
            }));
        }
        Ok(Self {
            rings,
            ring_cpus,
            tids: followed.into_iter().collect(),
            unlisted: 0,
        })
    }

    /// Opens `spec` over every task on each of `cpus`: one event and one ring
    /// per cpu, which is what sampling a machine or a set of cores is.
    ///
    /// Not inherited: an event that observes a cpu already sees every task
    /// that runs there, including the ones started after it. Every cpu has to
    /// open, and the first refusal is the whole answer, in the words of
    /// [`per_cpu_refusal`] -- a profile of the cores that happened to open
    /// would describe a machine with fewer cores, and a per-task event in
    /// their place would answer a different question under the same name.
    ///
    /// Every ring gets the same size. The kernel charges the rings of one user
    /// against `perf_event_mlock_kb` per online cpu, and beyond that against
    /// `RLIMIT_MEMLOCK`; shrinking each ring on its own until it maps let the
    /// first cpus take the whole budget and left the last one nothing, which
    /// is P11 of `docs/portability.md` -- *"perf_event_open on cpu 3 was
    /// refused (mmap of 8 ring pages: Operation not permitted)"* on a 5.15
    /// kernel whose default `RLIMIT_MEMLOCK` is 64 KiB. So a size one cpu is
    /// refused is given up for every cpu, and the whole set is opened again at
    /// half of it.
    pub fn open_cpus(spec: &SampleSpec, cpus: &[u32], ring_pages: usize) -> Result<Self, String> {
        Self::open_per_cpu(spec, cpus, ring_pages, Scope::Cpu)
    }

    /// Opens `spec` over the tasks of one cgroup v2 directory, on each of
    /// `cpus`: one event and one ring per cpu, each counting only while a
    /// task of that cgroup (or of a cgroup below it) runs there.
    ///
    /// The directory is opened here and closed once every event is open; the
    /// kernel resolves it to its cgroup at `perf_event_open`. The privilege
    /// asked is a per-cpu event's -- the kernel's check is `perf_allow_cpu`,
    /// because a cgroup's tasks may belong to anybody -- and a refusal says so
    /// in [`per_cpu_refusal`]'s words.
    pub fn open_cgroup(
        spec: &SampleSpec,
        directory: &std::path::Path,
        cpus: &[u32],
        ring_pages: usize,
    ) -> Result<Self, String> {
        let cgroup = open_cgroup_directory(directory)?;
        let fd = std::os::fd::AsRawFd::as_raw_fd(&cgroup);
        Self::open_per_cpu(spec, cpus, ring_pages, |cpu| Scope::CgroupOnCpu { fd, cpu })
    }

    fn open_per_cpu(
        spec: &SampleSpec,
        cpus: &[u32],
        ring_pages: usize,
        scope: impl Fn(u32) -> Scope,
    ) -> Result<Self, String> {
        if cpus.is_empty() {
            return Err("no cpu to sample".to_owned());
        }
        let spec = SampleSpec {
            inherit: false,
            ..*spec
        };
        let mut pages = ring_pages.next_power_of_two().max(MIN_RING_PAGES);
        loop {
            match open_rings(&spec, cpus, pages, &scope) {
                Ok(rings) => {
                    return Ok(Self {
                        rings,
                        ring_cpus: cpus.to_vec(),
                        tids: Vec::new(),
                        unlisted: 0,
                    })
                }
                Err((_, error)) if pages > MIN_RING_PAGES && is_ring_too_large(&error) => {
                    pages /= 2;
                }
                Err((cpu, error)) if is_ring_too_large(&error) => {
                    return Err(ring_budget_refusal(Some(cpu), cpus.len(), pages, &error))
                }
                Err((cpu, error)) => return Err(per_cpu_refusal(cpu, &error)),
            }
        }
    }

    /// The threads an event was opened on.
    pub fn threads(&self) -> &[i32] {
        &self.tids
    }

    /// Threads that appeared while the events were being opened. See
    /// [`started_while_opening`].
    pub fn unlisted(&self) -> usize {
        self.unlisted
    }

    /// The cpus a ring was mapped on.
    pub fn cpus(&self) -> usize {
        self.rings.len()
    }

    pub fn callchain(&self) -> Callchain {
        self.rings
            .first()
            .map_or(Callchain::Off, SampledEvent::callchain)
    }

    pub fn enable(&self) -> io::Result<()> {
        self.rings.iter().try_for_each(SampledEvent::enable)
    }

    pub fn disable(&self) -> io::Result<()> {
        self.rings.iter().try_for_each(SampledEvent::disable)
    }

    pub fn lost(&self) -> u64 {
        self.rings.iter().map(SampledEvent::lost).sum()
    }

    /// What each ring lost, by the cpu it is pinned to, for the rings that
    /// lost anything: the part of [`ThreadSampler::lost`] each cpu's ring owns.
    pub fn lost_by_cpu(&self) -> Vec<(u32, u64)> {
        self.rings
            .iter()
            .zip(&self.ring_cpus)
            .map(|(ring, &cpu)| (cpu, ring.lost()))
            .filter(|(_, lost)| *lost > 0)
            .collect()
    }

    pub fn undecoded(&self) -> u64 {
        self.rings.iter().map(SampledEvent::undecoded).sum()
    }

    /// Throttled periods over every ring. See [`SampledEvent::throttled`].
    pub fn throttled(&self) -> u64 {
        self.rings.iter().map(SampledEvent::throttled).sum()
    }

    /// Throttled periods by the cpu each ring is pinned to, for the rings the
    /// kernel throttled: the part of [`ThreadSampler::throttled`] each cpu's
    /// ring owns, the way [`ThreadSampler::lost_by_cpu`] splits the drops.
    pub fn throttled_by_cpu(&self) -> Vec<(u32, u64)> {
        self.rings
            .iter()
            .zip(&self.ring_cpus)
            .map(|(ring, &cpu)| (cpu, ring.throttled()))
            .filter(|(_, throttled)| *throttled > 0)
            .collect()
    }

    /// Reads every ring, handing over the samples in time order.
    ///
    /// Each ring is in order and the rings are per cpu, so a thread that
    /// migrated has its samples in two of them; the stream a collector builds
    /// events from is sorted by time, which is what every other source's is.
    /// The records that are not samples -- a loss, a throttle -- come first,
    /// since they carry no time of their own.
    pub fn drain(&mut self, mut handle: impl FnMut(Record)) {
        let mut samples = Vec::new();
        for (ring, &cpu) in self.rings.iter_mut().zip(&self.ring_cpus) {
            ring.drain(|record| match record {
                // The ring is pinned, so the cpu is known without asking the
                // kernel to write it into every record.
                Record::Sample(mut sample) => {
                    sample.cpu.get_or_insert(cpu);
                    samples.push(sample)
                }
                other => handle(other),
            });
        }
        samples.sort_by_key(|sample| sample.time_ns);
        for sample in samples {
            handle(Record::Sample(sample));
        }
    }
}

/// A task found by [`TaskSampler::discover`]: its process, and itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    pub tgid: i32,
    pub tid: i32,
}

/// A task an event was opened on after the open, with the cpu it had used
/// when its event began sampling: its `schedstat`, read right after the
/// enable, so the two never overlap -- what the samples cover starts there.
/// And its name and its process's parent, read then too: a short process is
/// gone by the drain that decodes its samples, and its samples kept no name
/// and no parent (`make`'s `as`, 280 ms, outside every job).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub task: Found,
    pub cpu_before_ns: u64,
    pub comm: Option<String>,
    pub ppid: Option<u64>,
}

impl Discovered {
    fn now(task: Found) -> Self {
        Self {
            task,
            cpu_before_ns: cpu_before(task),
            comm: std::fs::read_to_string(format!("/proc/{}/task/{}/comm", task.tgid, task.tid))
                .ok()
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty()),
            ppid: parent_of(task.tgid as u32),
        }
    }
}

/// A process's parent, the fourth field of its `/proc/<pid>/stat` (after the
/// command, which may hold spaces and parentheses, so read past the last `)`).
/// `None` once the process is gone, or for the kernel's own (`ppid` 0).
pub fn parent_of(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parent_in_stat(&stat)
}

pub fn parent_in_stat(stat: &str) -> Option<u64> {
    let after = &stat[stat.rfind(')')? + 1..];
    after
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
        .filter(|ppid| *ppid > 0)
}

/// The cpu time a task had used, from the first field of its `schedstat`:
/// nanoseconds on a cpu, the kernel's own account. 0 once it is gone.
fn cpu_before(task: Found) -> u64 {
    std::fs::read_to_string(format!("/proc/{}/task/{}/schedstat", task.tgid, task.tid))
        .ok()
        .and_then(|line| line.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

/// How often the discovery thread looks for tasks the target started.
const DISCOVERY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// The thread that finds the tasks a target starts and opens their events,
/// apart from the drain: a drain that symbolizes a large program ran 260 ms
/// a round in a debug build, up to a second, and a task found only on the
/// next round lost what it ran meanwhile -- a 330 ms `as` that started and
/// ended inside one round was in no row at all (`cases/make-sqlite`).
struct Discovery {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    found: std::sync::Arc<std::sync::Mutex<Vec<(Discovered, SampledEvent)>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Discovery {
    fn stop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    fn take(&self) -> Vec<(Discovered, SampledEvent)> {
        std::mem::take(&mut *self.found.lock().expect("the discovery queue"))
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The processes `root` has started and not yet reaped, and root itself, each
/// with its threads.
///
/// Read from `/proc/<pid>/task/<tid>/children`, which lists the children each
/// thread forked. A kernel built without it answers with the root's threads
/// alone, which is what the caller then follows.
fn tasks_under(root: i32) -> Vec<Found> {
    let mut found = Vec::new();
    let mut processes = vec![root];
    let mut seen = std::collections::BTreeSet::new();
    while let Some(tgid) = processes.pop() {
        if !seen.insert(tgid) {
            continue;
        }
        for tid in threads_of(tgid) {
            found.push(Found { tgid, tid });
            let children = std::fs::read_to_string(format!("/proc/{tgid}/task/{tid}/children"))
                .unwrap_or_default();
            processes.extend(
                children
                    .split_whitespace()
                    .filter_map(|pid| pid.parse::<i32>().ok()),
            );
        }
    }
    found
}

/// A sampled event per task, each on every cpu and carrying its own period
/// across them, with the tasks a target starts found as it starts them.
///
/// The other way to follow a program, [`ThreadSampler`], inherits: complete
/// from each task's first instruction, but an inherited event can only be
/// mapped pinned to a cpu, so a task is sampled by one event per cpu, and each
/// of those keeps the part of a period it had counted when the task left that
/// cpu. A task that ran on twelve cpus loses up to twelve periods of samples
/// rather than one -- measured at five to nine milliseconds on a 400 ms program
/// at 997 Hz, against a decomposition that states one period as the on-cpu
/// row's resolution. Where the samples are a quantity of time, that is an
/// over-statement of precision, so this shape keeps each task's period whole:
/// one event per task, `cpu == -1`, not inherited, a ring each.
///
/// What it gives up is the start of a task it did not have at the open: a
/// thread or a process the target starts is found by the next
/// [`TaskSampler::discover`], and what it did before then is not sampled. The
/// caller is handed each task it finds, to read what it had already done --
/// `/proc/<pid>/task/<tid>/schedstat` holds that exactly -- and to say so.
pub struct TaskSampler {
    spec: SampleSpec,
    root: i32,
    follows: bool,
    /// The ring of every task but the root's.
    ring_pages: usize,
    /// The root task's ring: the one task every profile has, and the one a
    /// single-threaded program puts every sample into.
    root_ring_pages: usize,
    rings: std::collections::BTreeMap<i32, SampledEvent>,
    enabled: bool,
    at_open: usize,
    /// The thread that opens the events of tasks the target starts, once
    /// [`TaskSampler::discover_in_background`] has started it.
    discovery: Option<Discovery>,
    /// What the rings of tasks that have exited could not decode, kept when
    /// their rings are released.
    retired_undecoded: u64,
    /// The throttled periods of those rings, by task, kept the same way: a
    /// thread the kernel throttled and that exited was still throttled.
    retired_throttled: std::collections::BTreeMap<i32, u64>,
    /// What those rings lost, by task, kept the same way: a thread that
    /// overflowed its ring and exited still lost those samples.
    retired_lost: std::collections::BTreeMap<i32, u64>,
}

impl TaskSampler {
    /// Opens `spec` on `pid` and, when `follows`, on every task under it now:
    /// `pid` with a ring of `root_ring_pages`, every other task with one of
    /// `ring_pages`.
    pub fn open(
        spec: &SampleSpec,
        pid: i32,
        follows: bool,
        root_ring_pages: usize,
        ring_pages: usize,
    ) -> io::Result<Self> {
        let mut sampler = Self {
            spec: SampleSpec {
                inherit: false,
                ..*spec
            },
            root: pid,
            follows,
            ring_pages,
            root_ring_pages,
            rings: std::collections::BTreeMap::new(),
            enabled: false,
            at_open: 0,
            discovery: None,
            retired_undecoded: 0,
            retired_throttled: std::collections::BTreeMap::new(),
            retired_lost: std::collections::BTreeMap::new(),
        };
        let tasks = if follows {
            tasks_under(pid)
        } else {
            vec![Found {
                tgid: pid,
                tid: pid,
            }]
        };
        let mut first_error = None;
        for task in tasks {
            let pages = if task.tid == pid {
                root_ring_pages
            } else {
                ring_pages
            };
            match SampledEvent::open(&sampler.spec, Scope::Process(task.tid), pages) {
                Ok(event) => {
                    sampler.rings.insert(task.tid, event);
                }
                Err(error) if is_gone(&error) => {
                    first_error.get_or_insert(error);
                }
                Err(error) => return Err(error),
            }
        }
        if sampler.rings.is_empty() {
            return Err(first_error.unwrap_or_else(|| io::Error::from_raw_os_error(libc::ESRCH)));
        }
        sampler.at_open = sampler.rings.len();
        Ok(sampler)
    }

    /// Opens an event on every task under the root that has none yet, and
    /// returns them, each with the cpu it had used when its event began. A
    /// task that exited before its event could be opened is passed over: it
    /// has nothing left to sample. With the discovery thread running, the
    /// tasks it found since the last call, their events already sampling.
    pub fn discover(&mut self) -> Vec<Discovered> {
        if let Some(discovery) = &self.discovery {
            let mut found = Vec::new();
            for (discovered, event) in discovery.take() {
                self.rings.insert(discovered.task.tid, event);
                found.push(discovered);
            }
            return found;
        }
        if !self.follows {
            return Vec::new();
        }
        let mut found = Vec::new();
        for task in tasks_under(self.root) {
            if self.rings.contains_key(&task.tid) {
                continue;
            }
            let Ok(event) = SampledEvent::open(
                &self.spec,
                Scope::Process(task.tid),
                if task.tid == self.root {
                    self.root_ring_pages
                } else {
                    self.ring_pages
                },
            ) else {
                continue;
            };
            if self.enabled && event.enable().is_err() {
                continue;
            }
            self.rings.insert(task.tid, event);
            found.push(Discovered::now(task));
        }
        found
    }

    /// Finds the tasks the target starts on a thread of its own, every
    /// [`DISCOVERY_INTERVAL`], whatever the drain is doing: each new task's
    /// event is opened and enabled there and handed over at the next
    /// [`TaskSampler::discover`]. Called once the events are enabled; nothing
    /// when the sampler follows the root alone.
    pub fn discover_in_background(&mut self) {
        if !self.follows || self.discovery.is_some() {
            return;
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let found = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut known: std::collections::BTreeSet<i32> = self.rings.keys().copied().collect();
        let (spec, root, pages) = (self.spec, self.root, self.ring_pages);
        let thread = {
            let (stop, found) = (stop.clone(), found.clone());
            std::thread::Builder::new()
                .name("omn-discovery".into())
                .spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                        for task in tasks_under(root) {
                            if known.contains(&task.tid) {
                                continue;
                            }
                            let opened = SampledEvent::open(&spec, Scope::Process(task.tid), pages)
                                .and_then(|event| event.enable().map(|()| event));
                            // Gone already, or refused: not looked at again.
                            known.insert(task.tid);
                            let Ok(event) = opened else { continue };
                            let discovered = Discovered::now(task);
                            found
                                .lock()
                                .expect("the discovery queue")
                                .push((discovered, event));
                        }
                        std::thread::sleep(DISCOVERY_INTERVAL);
                    }
                })
        };
        if let Ok(thread) = thread {
            self.discovery = Some(Discovery {
                stop,
                found,
                thread: Some(thread),
            });
        }
    }

    /// The tasks an event was opened on at the open.
    pub fn at_open(&self) -> usize {
        self.at_open
    }

    pub fn callchain(&self) -> Callchain {
        self.rings
            .values()
            .next()
            .map_or(Callchain::Off, SampledEvent::callchain)
    }

    pub fn enable(&mut self) -> io::Result<()> {
        self.rings.values().try_for_each(SampledEvent::enable)?;
        self.enabled = true;
        Ok(())
    }

    pub fn disable(&mut self) -> io::Result<()> {
        // The thread stops first, and what it found is kept: a task found in
        // the last round is sampled until here like the others.
        if let Some(mut discovery) = self.discovery.take() {
            discovery.stop();
            for (discovered, event) in discovery.take() {
                self.rings.insert(discovered.task.tid, event);
            }
        }
        self.enabled = false;
        // A task that has exited refuses nothing here: its event is still
        // open and is simply not running.
        self.rings.values().try_for_each(SampledEvent::disable)
    }

    pub fn undecoded(&self) -> u64 {
        self.retired_undecoded
            + self
                .rings
                .values()
                .map(SampledEvent::undecoded)
                .sum::<u64>()
    }

    /// What each task's ring lost, for the tasks whose ring lost anything,
    /// including tasks that have exited: each ring is one task's, so its drops
    /// are that task's and no other's.
    pub fn lost_by_task(&self) -> Vec<(i32, u64)> {
        let mut lost = self.retired_lost.clone();
        for (tid, ring) in &self.rings {
            if ring.lost() > 0 {
                *lost.entry(*tid).or_default() += ring.lost();
            }
        }
        lost.into_iter().collect()
    }

    /// Throttled periods by task, for the tasks whose ring the kernel
    /// throttled, including tasks that have exited: the part of
    /// [`TaskSampler::throttled`] each task's ring owns, so that a profile's
    /// throttles are placed on the task they happened to and on no other.
    pub fn throttled_by_task(&self) -> Vec<(i32, u64)> {
        let mut throttled = self.retired_throttled.clone();
        for (tid, ring) in &self.rings {
            if ring.throttled() > 0 {
                *throttled.entry(*tid).or_default() += ring.throttled();
            }
        }
        throttled.into_iter().collect()
    }

    /// Throttled periods over every ring, including those of tasks that have
    /// exited. See [`SampledEvent::throttled`].
    pub fn throttled(&self) -> u64 {
        self.retired_throttled.values().sum::<u64>()
            + self
                .rings
                .values()
                .map(SampledEvent::throttled)
                .sum::<u64>()
    }

    /// Reads every ring, handing over the samples in time order and the
    /// records that are not samples first. See [`ThreadSampler::drain`].
    ///
    /// A task that has exited has written its last sample before it left, so
    /// once its ring is drained it is released: a shell that runs a thousand
    /// short programs does not hold a thousand rings to the end of the run.
    pub fn drain(&mut self, mut handle: impl FnMut(Record)) {
        let mut samples = Vec::new();
        let mut exited = Vec::new();
        for (tid, ring) in &mut self.rings {
            let gone = !std::path::Path::new(&format!("/proc/{tid}")).exists();
            ring.drain(|record| match record {
                Record::Sample(sample) => samples.push(sample),
                other => handle(other),
            });
            if gone {
                exited.push(*tid);
            }
        }
        for tid in exited {
            if let Some(ring) = self.rings.remove(&tid) {
                self.retired_undecoded += ring.undecoded();
                if ring.throttled() > 0 {
                    *self.retired_throttled.entry(tid).or_default() += ring.throttled();
                }
                if ring.lost() > 0 {
                    *self.retired_lost.entry(tid).or_default() += ring.lost();
                }
            }
        }
        samples.sort_by_key(|sample| sample.time_ns);
        for sample in samples {
            handle(Record::Sample(sample));
        }
    }
}

// The cpu list the kernel publishes has one reader, in `capabilities.rs`,
// which answers "what does this machine have" for everything else; finding 26
// of `docs/open-findings.md` counted four copies. The names stay here for the
// callers that know them as perf's.
pub use crate::capabilities::{online_cpus, parse_cpu_list, ONLINE_CPUS};

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
    let refused = matches!(error.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM))
        || error.kind() == io::ErrorKind::PermissionDenied;
    if !refused {
        return format!(
            "perf_event_open on cpu {cpu} was refused ({error}); this cpu is online, so it is \
             the counter rather than the core that is not there"
        );
    }
    let level = match paranoid() {
        Some(level) => format!("it is {level} here"),
        None => "it could not be read here".to_owned(),
    };
    format!(
        "counting cpu {cpu} observes every task on it, which needs \
         /proc/sys/kernel/perf_event_paranoid at 0 or below ({level}{}) or CAP_PERFMON on \
         this binary; a per-task count is not the same measurement and is not substituted",
        crate::capabilities::paranoid_beyond_capabilities()
    )
}

/// Opens one ring of exactly `pages` on each cpu, or says which cpu refused.
fn open_rings(
    spec: &SampleSpec,
    cpus: &[u32],
    pages: usize,
    scope: &impl Fn(u32) -> Scope,
) -> Result<Vec<SampledEvent>, (u32, io::Error)> {
    cpus.iter()
        .map(|&cpu| SampledEvent::open_with_ring(spec, scope(cpu), pages).map_err(|e| (cpu, e)))
        .collect()
}

/// Opens a cgroup v2 directory for `PERF_FLAG_PID_CGROUP`.
pub fn open_cgroup_directory(directory: &std::path::Path) -> Result<std::fs::File, String> {
    std::fs::File::open(directory).map_err(|error| {
        format!(
            "cannot open the cgroup {}: {error}; a cgroup target is a directory of the cgroup \
             v2 hierarchy",
            directory.display()
        )
    })
}

/// A number read from one of `/proc/sys/kernel`'s files.
fn kernel_sysctl(name: &str) -> Option<i64> {
    read_trimmed(&format!("/proc/sys/kernel/{name}"))?
        .parse()
        .ok()
}

/// This process's `RLIMIT_MEMLOCK`, in KiB, or `None` when it is unlimited.
fn memlock_kib() -> Option<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes the struct it is given and nothing else.
    if unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut limit) } != 0
        || limit.rlim_cur == libc::RLIM_INFINITY
    {
        return None;
    }
    Some(limit.rlim_cur / 1024)
}

/// Why rings on every cpu could not be mapped even at the smallest size, in
/// terms of the two budgets the kernel charges them to.
///
/// `perf_mmap` lets one user lock `perf_event_mlock_kb` per online cpu for
/// its rings, and charges what goes past that to `RLIMIT_MEMLOCK` -- unless
/// `perf_event_paranoid` is -1 or the process holds `CAP_IPC_LOCK`, neither of
/// which `CAP_PERFMON` implies. The kernel's default `RLIMIT_MEMLOCK` was 64
/// KiB until 5.16 and is 8 MiB since, so the same rings map on 6.1 and are
/// refused on 5.15. The rings of this user's other perf sessions count against
/// the same budget, which is why the refusal says what the budget is rather
/// than predicting it.
///
/// `cpu` is the one refused, where the opener knows it; a sampler that opens
/// its rings in one call does not.
pub fn ring_budget_refusal(
    cpu: Option<u32>,
    cpus: usize,
    pages: usize,
    error: &io::Error,
) -> String {
    let refused = match cpu {
        Some(cpu) => format!("cpu {cpu} was refused"),
        None => "one was refused".to_owned(),
    };
    let page_kib = page_size() / 1024;
    let asked_kib = cpus * (pages + 1) * page_kib;
    let per_cpu = match kernel_sysctl("perf_event_mlock_kb") {
        Some(kib) => format!("{kib} KiB"),
        None => "an unreadable amount".to_owned(),
    };
    let memlock = match memlock_kib() {
        Some(kib) => format!("{kib} KiB"),
        None => "unlimited".to_owned(),
    };
    format!(
        "the sampling rings of {cpus} cpus do not fit what this user may lock: {refused} a \
         ring of {pages} pages ({error}), after the others took theirs, {asked_kib} \
         KiB in all at that size. The kernel lets a user lock \
         /proc/sys/kernel/perf_event_mlock_kb per online cpu ({per_cpu} here) and charges the \
         rest to RLIMIT_MEMLOCK ({memlock} here; `ulimit -l`), unless perf_event_paranoid is \
         -1 or the process holds CAP_IPC_LOCK -- CAP_PERFMON does not lift it. Raise either \
         limit, or close this user's other perf sessions"
    )
}

/// Whether this process may open an event that observes a whole cpu.
///
/// The question a system or core target asks of every `pmu:` backend, and
/// not the one the catalog's `pmu:` probe answers: that probe opens the event
/// over this process, which `perf_event_paranoid` at 2 allows and a cpu event
/// it does not. The kernel's check (`perf_allow_cpu`) is the same whichever
/// event is opened, so a disabled software clock on the first online cpu is
/// the whole answer, opened and closed at once. The refusal is
/// [`per_cpu_refusal`]'s, which names the sysctl and the capability that grant
/// it.
pub fn per_cpu_support() -> Result<(), String> {
    let cpu = online_cpus()
        .ok()
        .and_then(|cpus| cpus.first().copied())
        .unwrap_or(0);
    let spec = SampleSpec::software(
        sys::bindings::PERF_COUNT_SW_CPU_CLOCK as u64,
        1_000_000,
        Callchain::Off,
    );
    let fd = open_fd(&spec, Scope::Cpu(cpu)).map_err(|error| per_cpu_refusal(cpu, &error))?;
    // SAFETY: a descriptor this function opened and nothing else holds.
    unsafe { libc::close(fd) };
    Ok(())
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
        // The online list is asked before opening anything (in `open_scoped`),
        // because the kernel answers `EINVAL` for a cpu that does not exist --
        // the same errno it gives for an attribute it cannot honour. Reading
        // the list the kernel publishes tells the two apart, and they send the
        // reader to different places: one is a cpu number, the other is a
        // counter this hardware lacks.
        Self::open_scoped(counters, cpus, Scope::Cpu)
    }

    /// Opens the counters over the tasks of one cgroup v2 directory, on every
    /// named cpu.
    ///
    /// A counter follows a task or a cpu, and a cgroup is the second shape
    /// with a filter: each cpu's group counts only while a task of that cgroup
    /// -- or of one below it -- runs there. So a cgroup is one group per cpu,
    /// like a machine, and the privilege asked is a cpu's (`perf_allow_cpu`):
    /// the cgroup's tasks may be anybody's.
    pub fn open_cgroup(
        counters: &[Counter],
        directory: &std::path::Path,
        cpus: &[u32],
    ) -> Result<Self, String> {
        let cgroup = open_cgroup_directory(directory)?;
        let fd = std::os::fd::AsRawFd::as_raw_fd(&cgroup);
        Self::open_scoped(counters, cpus, |cpu| Scope::CgroupOnCpu { fd, cpu })
    }

    fn open_scoped(
        counters: &[Counter],
        cpus: &[u32],
        scope: impl Fn(u32) -> Scope,
    ) -> Result<Self, String> {
        if cpus.is_empty() {
            return Err("no cpu to count".to_owned());
        }
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
            match CounterGroup::open(counters, scope(cpu)) {
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
        self.read()
    }

    /// Reads every cpu while the groups run: a poll's reading.
    pub fn read(&self) -> Vec<(u32, io::Result<Vec<Reading>>)> {
        self.groups
            .iter()
            .map(|(cpu, group)| (*cpu, group.read()))
            .collect()
    }

    /// Stops every cpu and adds their readings into one, keeping the cpus
    /// whose read failed apart.
    ///
    /// For a cgroup, where the cpus are where its tasks happened to run and
    /// not what was asked about: the cgroup's count is the sum, with the times
    /// summed the way [`ThreadCounters::read_final`] sums a process's threads.
    /// Each cpu's value is scaled over its own window before it is added.
    pub fn read_total(&self) -> (Option<Vec<Reading>>, Vec<(u32, io::Error)>) {
        for (_, group) in &self.groups {
            let _ = group.disable();
        }
        self.read_sum()
    }

    /// The same sum while the groups run: a poll's reading of a cgroup.
    pub fn read_sum(&self) -> (Option<Vec<Reading>>, Vec<(u32, io::Error)>) {
        let mut total: Option<Vec<Reading>> = None;
        let mut failed = Vec::new();
        for (cpu, reading) in self.read() {
            match reading {
                Ok(readings) => {
                    total = Some(match total {
                        None => readings,
                        Some(sum) => add_readings(&sum, &readings),
                    })
                }
                Err(error) => failed.push((cpu, error)),
            }
        }
        (total, failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE_FAULTS: u64 = sys::bindings::PERF_COUNT_SW_PAGE_FAULTS as u64;

    /// In frequency mode a sample carries its period, between `cpu` and the
    /// fields after it; a fixed period asks for none.
    #[test]
    fn a_frequency_mode_sample_carries_its_period_and_the_fields_after_it_line_up() {
        use sys::bindings as b;
        let spec = SampleSpec {
            memory: true,
            rate: Rate::Hz(99),
            ..SampleSpec::software(0, 1, Callchain::Off)
        };
        assert!(spec.sample_type() & u64::from(b::PERF_SAMPLE_PERIOD) != 0);
        let fixed = SampleSpec::software(0, 1, Callchain::Off);
        assert!(fixed.sample_type() & u64::from(b::PERF_SAMPLE_PERIOD) == 0);
        let weight = 309u64;
        let data_src = (u64::from(b::PERF_MEM_OP_LOAD) << b::PERF_MEM_OP_SHIFT)
            | (u64::from(b::PERF_MEM_LVLNUM_RAM) << b::PERF_MEM_LVLNUM_SHIFT);
        let mut body = vec![0u8; 8];
        for word in [
            0x401000u64,
            (7u64 << 32) | 6,
            1_000,
            0,
            5,
            123_457,
            weight,
            data_src,
        ] {
            body.extend_from_slice(&word.to_ne_bytes());
        }
        let sample = decode_sample(&body, 0, spec.sample_type());
        assert_eq!(sample.cpu, Some(5));
        assert_eq!(sample.period, Some(123_457));
        let access = sample.access.expect("the weight after the period");
        assert_eq!(access.latency_cycles, 309);
        assert_eq!(decode_sample(&body, 0, fixed.sample_type()).period, None);
    }

    /// The user registers and the copied stack sit between the callchain and
    /// the weight, and the stack is as long as the kernel filled it, not as
    /// long as it reserved: `dyn_size` after the bytes says how much of them
    /// are real. A decoder that read `size` as the data, or forgot `dyn_size`,
    /// would read the weight out of the stack.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn a_copied_stack_is_read_to_what_the_kernel_filled_and_the_fields_after_it_still_line_up() {
        use sys::bindings as b;
        let spec = SampleSpec {
            memory: true,
            ..SampleSpec::software(0, 1, Callchain::FramePointersAndLeaf { kernel: true })
        };
        let mut words = vec![0x401000u64, (7u64 << 32) | 6, 1_000, 0, 5];
        words.extend([2, 0x401000, 0x402000]); // callchain: two frames
        words.extend([2, 0x7ffd_0010, 0x7ffd_0000, 0x401000]); // abi, bp, sp, ip
        let reserved = LEAF_STACK_BYTES as usize;
        words.push(reserved as u64);
        let mut stack = vec![0u8; reserved];
        stack[..8].copy_from_slice(&0x402abcu64.to_ne_bytes());
        let filled = 40u64;
        let weight = 309u64 | (512u64 << 32);
        let data_src = (u64::from(b::PERF_MEM_OP_LOAD) << b::PERF_MEM_OP_SHIFT)
            | (u64::from(b::PERF_MEM_LVLNUM_RAM) << b::PERF_MEM_LVLNUM_SHIFT);
        let mut body = vec![0u8; 8];
        for word in &words {
            body.extend_from_slice(&word.to_ne_bytes());
        }
        body.extend_from_slice(&stack);
        for word in [filled, weight, data_src] {
            body.extend_from_slice(&word.to_ne_bytes());
        }
        let sample = decode_sample(&body, 0, spec.sample_type());
        assert_eq!(sample.callchain.as_deref(), Some(&[0x401000, 0x402000][..]));
        assert_eq!(
            sample.user_regs,
            Some(UserRegs {
                ip: 0x401000,
                sp: 0x7ffd_0000,
                fp: 0x7ffd_0010,
                lr: 0
            })
        );
        let copied = sample.user_stack.expect("the record carries a stack");
        assert_eq!(copied.len(), filled as usize);
        assert_eq!(copied[..8], 0x402abcu64.to_ne_bytes());
        let access = sample
            .access
            .expect("the fields after the stack still line up");
        assert_eq!(access.latency_cycles, 309);
        assert_eq!(access.source.level(), MemLevel::Ram);
    }

    /// A copied stack asks for no user callchain from the kernel, and a
    /// user-only shape opens nothing of the kernel's.
    #[test]
    fn each_stack_shape_asks_the_kernel_for_what_its_mode_needs() {
        use sys::bindings as b;
        let asks = |callchain: Callchain, bit: u32| {
            SampleSpec::software(0, 1, callchain).sample_type() & u64::from(bit) != 0
        };
        assert!(!asks(Callchain::Off, b::PERF_SAMPLE_CALLCHAIN));
        assert!(asks(Callchain::BOTH_HALVES, b::PERF_SAMPLE_CALLCHAIN));
        assert!(!asks(Callchain::BOTH_HALVES, b::PERF_SAMPLE_STACK_USER));
        let leaf = Callchain::FramePointersAndLeaf { kernel: false };
        assert!(asks(leaf, b::PERF_SAMPLE_CALLCHAIN) && asks(leaf, b::PERF_SAMPLE_STACK_USER));
        assert!(!asks(
            Callchain::CopiedStack { kernel: false },
            b::PERF_SAMPLE_CALLCHAIN
        ));
        assert!(asks(
            Callchain::CopiedStack { kernel: true },
            b::PERF_SAMPLE_CALLCHAIN
        ));
        assert!(asks(
            Callchain::CopiedStack { kernel: false },
            b::PERF_SAMPLE_REGS_USER
        ));
        assert_eq!(
            Callchain::BOTH_HALVES.user_only(),
            Callchain::FramePointers { kernel: false }
        );
        assert!(Callchain::FramePointers { kernel: false }.excludes_kernel());
        assert!(!Callchain::Off.excludes_kernel());
    }

    fn callchain_of_fixed(body: &[u8]) -> Vec<u64> {
        callchain_of(body, FIXED_FIELDS)
    }

    /// P11's refusal names the two budgets and the one capability that lift
    /// it, with this machine's numbers, and does not send the reader to
    /// `CAP_PERFMON`, which does not.
    #[test]
    fn a_ring_that_does_not_fit_names_what_the_kernel_charged_it_to() {
        let error = io::Error::other("mmap of 8 ring pages: Operation not permitted (os error 1)");
        let refusal = ring_budget_refusal(Some(3), 4, 8, &error);
        for words in [
            "cpu 3",
            "4 cpus",
            "8 pages",
            "144 KiB",
            "perf_event_mlock_kb",
            "RLIMIT_MEMLOCK",
            "CAP_IPC_LOCK",
            "CAP_PERFMON does not lift it",
        ] {
            assert!(refusal.contains(words), "`{words}` is not in: {refusal}");
        }
        let limit = kernel_sysctl("perf_event_mlock_kb").expect("the sysctl is readable");
        assert!(
            refusal.contains(&format!("({limit} KiB here)")),
            "{refusal}"
        );
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
            inside,
            after,
            "{} instructions were retired while the group was disabled and {} of them \
             were counted anyway",
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

        // The descriptor table is the process's, and the other tests of this
        // binary open perf events of their own on other threads meanwhile: a
        // per-cpu group over twenty-four cpus is more than a round's margin.
        // A leak is left behind by every round; a neighbour's descriptors are
        // not, so the least any of three rounds left is the measurement.
        let left = (0..3)
            .map(|_| {
                let before = perf_descriptors();
                for _ in 0..GROUPS {
                    let group = CounterGroup::open(&[INSTRUCTIONS, CYCLES], Scope::Process(0))
                        .expect("the first one opened, so these do too");
                    drop(group);
                }
                perf_descriptors().saturating_sub(before)
            })
            .min()
            .expect("three rounds");
        assert!(
            left < GROUPS,
            "{GROUPS} groups of two counters left {left} perf descriptors behind in each of \
             three rounds"
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
            "counting kernel mode as well cannot yield fewer instructions ({} against {})",
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
            callchain: Callchain::BOTH_HALVES,
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

#[cfg(test)]
mod following_tests {
    use super::*;

    fn reading(name: &'static str, raw: u64, enabled_ns: u64, running_ns: u64) -> Reading {
        Reading {
            name,
            raw,
            value: scaled(raw, enabled_ns, running_ns),
            enabled_ns,
            running_ns,
        }
    }

    /// Each group is scaled over its own window before the sum, so a thread
    /// that was multiplexed and one that was not are each right.
    #[test]
    fn readings_of_two_threads_add_value_by_value_and_time_by_time() {
        let whole = [reading("instructions", 100, 10, 10)];
        let halved = [reading("instructions", 50, 10, 5)];
        let sum = add_readings(&whole, &halved);
        assert_eq!(sum[0].raw, 150);
        assert_eq!(sum[0].value, 200, "100 counted, and 50 over half a window");
        assert_eq!((sum[0].enabled_ns, sum[0].running_ns), (20, 15));
        assert!(sum[0].was_multiplexed());
    }

    /// The listing a late attach opens one event per entry of, with the
    /// calling thread and a thread started before the listing in it.
    #[test]
    fn the_threads_of_this_process_include_one_started_before_the_listing() {
        let (tid_sender, tid) = std::sync::mpsc::channel();
        let (stop, parked) = std::sync::mpsc::channel::<()>();
        let helper = std::thread::spawn(move || {
            // SAFETY: a read-only query of this thread's own id.
            tid_sender.send(unsafe { libc::gettid() }).unwrap();
            let _ = parked.recv();
        });
        let helper_tid = tid.recv().unwrap();
        let listed = threads_of(std::process::id() as i32);
        drop(stop);
        helper.join().unwrap();
        assert!(listed.contains(&helper_tid), "{listed:?}");
        // SAFETY: as above.
        assert!(listed.contains(&unsafe { libc::gettid() }), "{listed:?}");
        // The calling task, and a pid with no directory, answer themselves.
        assert_eq!(threads_of(0), vec![0]);
        assert_eq!(threads_of(i32::MAX), vec![i32::MAX]);
    }

    /// A thread started after the open is found by `discover`, and sampled
    /// from then on for the cpu it burns; the one that opened is sampled with
    /// its period whole, which is why this shape exists beside the inherited
    /// one.
    /// With the discovery thread, a task that starts and ends between two
    /// calls of `discover` is still found, named, and sampled: the drain
    /// that calls it can be a second late (a debug build symbolizing a large
    /// program), and `make`'s 330 ms `as` came and went inside one.
    #[test]
    fn a_task_that_comes_and_goes_between_two_discovers_is_found_by_the_thread() {
        const HZ: u64 = 1000;
        const BURN_NS: u64 = 100_000_000;
        let spec = SampleSpec::software(
            sys::bindings::PERF_COUNT_SW_CPU_CLOCK as u64,
            1_000_000_000 / HZ,
            Callchain::Off,
        );
        let mut sampler = match TaskSampler::open(&spec, std::process::id() as i32, true, 8, 8) {
            Ok(sampler) => sampler,
            Err(error) => {
                eprintln!("skipped: this machine refuses unprivileged perf sampling: {error}");
                return;
            }
        };
        sampler.enable().unwrap();
        sampler.discover_in_background();
        let worker = std::thread::Builder::new()
            .name("omn-shortlived".into())
            .spawn(|| {
                // Long enough for the thread's rounds to meet it.
                std::thread::sleep(std::time::Duration::from_millis(50));
                let clock = || {
                    let mut now = libc::timespec {
                        tv_sec: 0,
                        tv_nsec: 0,
                    };
                    // SAFETY: `now` is a valid timespec for the kernel to write.
                    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
                    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
                };
                let until = clock() + BURN_NS;
                while clock() < until {
                    std::hint::spin_loop();
                }
                // SAFETY: a read-only query of this thread's own id.
                unsafe { libc::gettid() }
            })
            .unwrap();
        let worker_tid = worker.join().unwrap();
        // Only now, the task gone: what the thread found meanwhile.
        let found = sampler.discover();
        let worker = found
            .iter()
            .find(|found| found.task.tid == worker_tid)
            .unwrap_or_else(|| panic!("the short-lived worker was not found: {found:?}"));
        assert_eq!(worker.comm.as_deref(), Some("omn-shortlived"));
        assert!(worker.cpu_before_ns < BURN_NS / 2, "{worker:?}");
        sampler.disable().unwrap();
        let mut samples = 0u64;
        sampler.drain(|record| {
            if matches!(record, Record::Sample(ref sample) if sample.tid == worker_tid as u32) {
                samples += 1;
            }
        });
        let expected = BURN_NS * HZ / 1_000_000_000;
        assert!(
            samples >= expected / 2,
            "{samples} samples of the worker's {expected} periods"
        );
    }

    #[test]
    fn a_task_sampler_finds_a_thread_started_after_the_open_and_samples_it() {
        const HZ: u64 = 1000;
        const BURN_NS: u64 = 200_000_000;
        let spec = SampleSpec::software(
            sys::bindings::PERF_COUNT_SW_CPU_CLOCK as u64,
            1_000_000_000 / HZ,
            Callchain::Off,
        );
        let mut sampler = match TaskSampler::open(&spec, std::process::id() as i32, true, 8, 8) {
            Ok(sampler) => sampler,
            Err(error) => {
                eprintln!("skipped: this machine refuses unprivileged perf sampling: {error}");
                return;
            }
        };
        sampler.enable().unwrap();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let (tid_sender, tid) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            // SAFETY: a read-only query of this thread's own id.
            tid_sender.send(unsafe { libc::gettid() }).unwrap();
            wait.recv().unwrap();
            let clock = || {
                let mut now = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                // SAFETY: `now` is a valid timespec for the kernel to write.
                unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
                now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
            };
            let until = clock() + BURN_NS;
            while clock() < until {
                std::hint::spin_loop();
            }
        });
        let worker_tid = tid.recv().unwrap();
        let found = sampler.discover();
        assert!(
            found.iter().any(|found| found.task.tid == worker_tid),
            "the worker was started after the open and not found: {found:?}"
        );
        go.send(()).unwrap();
        worker.join().unwrap();
        sampler.disable().unwrap();
        let mut samples = 0u64;
        sampler.drain(|record| {
            if matches!(record, Record::Sample(ref sample) if sample.tid == worker_tid as u32) {
                samples += 1;
            }
        });
        let expected = BURN_NS * HZ / 1_000_000_000;
        assert!(
            samples.abs_diff(expected) <= expected * 3 / 10 + 1,
            "a thread that burned {} ms of its own cpu at {HZ} Hz was sampled {samples} times, \
             where about {expected} were due",
            BURN_NS / 1_000_000
        );
    }

    /// Ground truth: a thread that exists before the events are opened burns a
    /// chosen amount of its own cpu, and a cpu-clock sampler over the process
    /// takes about one sample per period of it. Opened on the process alone,
    /// the event followed the main thread -- which libtest parks -- and took
    /// none.
    #[test]
    fn a_sampler_opened_on_a_running_process_samples_a_thread_it_already_had() {
        const HZ: u64 = 1000;
        const BURN_NS: u64 = 200_000_000;
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let (tid_sender, tid) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            // SAFETY: a read-only query of this thread's own id.
            tid_sender.send(unsafe { libc::gettid() }).unwrap();
            wait.recv().unwrap();
            let clock = || {
                let mut now = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                // SAFETY: `now` is a valid timespec for the kernel to write.
                unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
                now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
            };
            let until = clock() + BURN_NS;
            while clock() < until {
                std::hint::spin_loop();
            }
        });
        let worker_tid = tid.recv().unwrap() as u32;

        let spec = SampleSpec::software(
            sys::bindings::PERF_COUNT_SW_CPU_CLOCK as u64,
            1_000_000_000 / HZ,
            Callchain::Off,
        );
        let mut sampler = match ThreadSampler::open_process(&spec, std::process::id() as i32, 8) {
            Ok(sampler) => sampler,
            Err(error) => {
                eprintln!("skipped: this machine refuses unprivileged perf sampling: {error}");
                drop(go);
                let _ = worker.join();
                return;
            }
        };
        assert!(
            sampler.threads().contains(&(worker_tid as i32)),
            "{:?}",
            sampler.threads()
        );
        sampler.enable().unwrap();
        go.send(()).unwrap();
        worker.join().unwrap();
        sampler.disable().unwrap();

        let mut samples = 0u64;
        let mut lost = 0u64;
        sampler.drain(|record| match record {
            Record::Sample(sample) if sample.tid == worker_tid => samples += 1,
            Record::Lost { count } => lost += count,
            _ => {}
        });
        let expected = BURN_NS * HZ / 1_000_000_000;
        assert!(
            samples + lost >= expected * 7 / 10 && samples <= expected * 13 / 10 + 1,
            "a thread that burned {} ms of its own cpu at {HZ} Hz was sampled {samples} times, \
             {lost} lost, where about {expected} were due",
            BURN_NS / 1_000_000
        );
    }
}
