//! What the injection library sends `omn`, byte for byte.
//!
//! This one file is compiled into both sides: the library includes it as a
//! module, and `omn` includes the same source by path
//! (`src/collect/gpu/kernel.rs`), so the two cannot disagree on a number. It
//! depends on nothing but `core`.
//!
//! **The channel** is one `AF_UNIX` `SOCK_SEQPACKET` socket `omn` creates
//! before the launch and names in `OMN_CUPTI_FD`. A sequenced-packet socket
//! keeps message boundaries and never interleaves two senders' messages, so
//! every process of the program that loads the library -- a child that execs
//! a CUDA program loads it afresh -- sends whole messages on the one
//! descriptor, each one saying whose it is. A child *forked* after `cuInit`
//! without an exec sends nothing: it cannot use CUDA, and what it holds is a
//! copy of its parent's batch (`lib.rs`).
//!
//! **A message** is a [`HEADER`]-byte header -- magic, version, sender pid,
//! frame count -- followed by frames. A message is at most [`MESSAGE_MAX`]
//! bytes.
//!
//! **A frame** is a length (`u32`, the whole frame's, header included), an op
//! (`u8`) and three bytes of padding, then the op's fields, native endian:
//! both ends are the same build on the same machine. A frame is padded to a
//! multiple of eight bytes. A reader that meets an op it does not know skips
//! the frame by its length and counts it, so a newer library and an older
//! reader lose that op and nothing after it.

/// `omn`'s descriptor for the channel, inherited by the launched program.
pub const FD_VARIABLE: &str = "OMN_CUPTI_FD";

/// What the program is asked to report, as a decimal bit set of the
/// `REPORT_*` values. Zero is a library that loads and stays disarmed: the
/// negative control a GPU tracer's cost is measured against.
pub const REPORT_VARIABLE: &str = "OMN_CUPTI_REPORT";

/// Kernel, memcpy and memset activity: `gpu.kernel`.
pub const REPORT_ACTIVITY: u32 = 1;
/// Counters per kernel launch, by CUPTI's range profiler in kernel replay:
/// `gpu.counters`.
pub const REPORT_COUNTERS: u32 = 2;
/// Program counters sampled inside kernels, with their stall reasons, by
/// CUPTI's PC sampling: `gpu.profile`.
pub const REPORT_PC_SAMPLING: u32 = 4;
/// Every call into the CUDA driver API, through CUPTI's callback API
/// (`callback.rs`, `gpu.api` on `vendor:cupti`).
pub const REPORT_CALLBACK: u32 = 8;

/// The counters `gpu.counters` asks for, comma-separated, in place of
/// [`DEFAULT_METRICS`]: CUPTI's metric names (`smsp__inst_executed.sum`...).
pub const METRICS_VARIABLE: &str = "OMN_CUPTI_METRICS";
/// Instruction, memory and cache counters: what a kernel executed, what it
/// moved to and from video memory, and what went through its L2 and L1.
pub const DEFAULT_METRICS: [&str; 5] = [
    "smsp__inst_executed.sum",
    "dram__bytes_read.sum",
    "dram__bytes_write.sum",
    "lts__t_bytes.sum",
    "l1tex__t_bytes.sum",
];

/// The PC sampling period, as CUPTI takes it: a sample every `2^n` cycles of
/// each SM, `n` from 5 to 31.
pub const PC_PERIOD_VARIABLE: &str = "OMN_CUPTI_PC_PERIOD";
pub const DEFAULT_PC_PERIOD: u32 = 12;

/// Overrides of the activity buffers, for the tests that make CUPTI drop:
/// the size of one buffer in bytes, and how many may be held at once.
pub const BUFFER_BYTES_VARIABLE: &str = "OMN_CUPTI_BUFFER_BYTES";
pub const BUFFERS_VARIABLE: &str = "OMN_CUPTI_BUFFERS";
/// How often the flush thread drains CUPTI and takes a calibration pair, in
/// milliseconds.
pub const FLUSH_MS_VARIABLE: &str = "OMN_CUPTI_FLUSH_MS";

/// A buffer comes back to the library when it is full, and only then until
/// the program exits (`lib.rs` says why no flush forces it sooner), so its
/// size is also how many records a live reader waits for: about a thousand
/// kernels here. The bound on what is held at once is the product, 64 MiB.
pub const DEFAULT_BUFFER_BYTES: usize = 256 << 10;
pub const DEFAULT_BUFFERS: usize = 256;
pub const DEFAULT_FLUSH_MS: u64 = 50;

/// `"OMNC"`, little end first.
pub const MAGIC: u32 = 0x434e_4d4f;
pub const VERSION: u16 = 1;
/// magic `u32`, version `u16`, padding `u16`, pid `u32`, frames `u32`.
pub const HEADER: usize = 16;
pub const MESSAGE_MAX: usize = 60 * 1024;
pub const FRAME_HEADER: usize = 8;

/// The library is armed: CUPTI's version, the buffer size and count it uses,
/// and what it was asked to report.
pub const OP_HELLO: u8 = 1;
/// One calibration pair: `CLOCK_MONOTONIC` before, CUPTI's clock, and
/// `CLOCK_MONOTONIC` after.
pub const OP_CALIB: u8 = 2;
pub const OP_KERNEL: u8 = 3;
pub const OP_MEMCPY: u8 = 4;
pub const OP_MEMSET: u8 = 5;
/// A kernel name, interned: its id, then its bytes.
pub const OP_NAME: u8 = 6;
/// Records CUPTI itself dropped, from `cuptiActivityGetNumDroppedRecords`.
pub const OP_DROPPED: u8 = 7;
/// Records the library had and could not send, cumulative: activity
/// records, and the counts of dropped and unfinished records an unsent batch
/// carried.
pub const OP_LOST: u8 = 8;
/// Something the library could not do, in words.
pub const OP_ERROR: u8 = 9;
/// The program is exiting and everything has been flushed: the records seen
/// and lost over the whole run.
pub const OP_END: u8 = 10;
/// An activity record of a kind this library does not decode.
pub const OP_UNKNOWN: u8 = 11;
/// Records CUPTI handed back before their activity finished, without the
/// times to report: a count.
pub const OP_INCOMPLETE: u8 = 12;

/// Ops from here to [`OP_INSIDE_LAST`] are what the library learns *inside*
/// kernels -- counters, sampled program counters -- for the readers of
/// `gpu.counters` and `gpu.profile`. A reader of activity skips them
/// without counting them as unknown: they are another reader's frames on the
/// one socket of a launch, not a newer library's.
pub const OP_INSIDE_FIRST: u8 = 13;
/// The range profiler is set up on a context: [`CounterSetup`], then the
/// metric ids, in the order [`OP_COUNTER`] refers to them.
pub const OP_COUNTER_SETUP: u8 = 13;
/// One counter of one kernel launch: [`Counter`].
pub const OP_COUNTER: u8 = 14;
/// Something inside a kernel the library could not report, by kind
/// (`LOST_*`): a count, then the kind.
pub const OP_INSIDE_LOST: u8 = 15;
/// One stall reason of a context's table: its index, then its name.
pub const OP_STALL: u8 = 16;
/// PC sampling is configured on a context, and what CUPTI says it applied:
/// [`PcConfig`].
pub const OP_PC_CONFIG: u8 = 17;
/// The samples one stall reason took at one program counter: [`PcSample`].
pub const OP_PC: u8 = 18;
/// One drain of the PC sampling buffers: [`PcDrain`].
pub const OP_PC_DRAIN: u8 = 19;
/// Records inside kernels the library had and could not send, cumulative:
/// the [`OP_LOST`] of the inside readers.
pub const OP_INSIDE_UNSENT: u8 = 20;
pub const OP_INSIDE_LAST: u8 = 31;
/// The library's exit handler has started, before it tears anything down:
/// no payload. A process that sent this and never [`OP_END`] died inside
/// the library's own exit path -- the tool, not the program, ended it.
pub const OP_EXITING: u8 = 32;
/// One driver API call that returned: entered at (`CLOCK_MONOTONIC`, read in
/// the program), how long it took, the id of its name ([`OP_NAME`]), what it
/// returned (`CUresult`) and the thread.
pub const OP_API: u8 = 33;
/// The callback API's own: a call whose entry the library did not see (it
/// was under way at the subscribe), counted.
pub const OP_API_UNPAIRED: u8 = 34;

/// What an [`OP_INSIDE_LOST`] counts: ranges CUPTI's decode said it dropped.
pub const LOST_RANGES: u32 = 1;
/// Ranges and launches that did not pair: the counters of a launch with no
/// range, or of a range with no launch, are not reported.
pub const LOST_UNPAIRED: u32 = 2;
/// Launches whose counters were never decoded: a decode or an evaluation
/// failed, or the program exited with them still queued.
pub const LOST_UNDECODED: u32 = 3;
/// Samples PC sampling dropped, by its own count.
pub const LOST_SAMPLES: u32 = 4;
/// Times the PC sampling hardware buffer filled: samples lost, uncounted.
pub const LOST_HW_BUFFER_FULL: u32 = 5;
/// Program counters CUPTI still held when PC sampling was torn down, and
/// that did not fit in the buffer it was handed.
pub const LOST_PCS: u32 = 6;
/// CUPTI's own numbers, from `cupti_activity.h`.
/// [`OP_API`]'s payload, as both sides read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Api {
    pub start_ns: u64,
    pub duration_ns: u64,
    pub name: u32,
    pub result: i32,
    pub tid: u32,
}

impl Api {
    pub const LEN: usize = 32;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut bytes = [0u8; Self::LEN];
        bytes[0..8].copy_from_slice(&self.start_ns.to_ne_bytes());
        bytes[8..16].copy_from_slice(&self.duration_ns.to_ne_bytes());
        bytes[16..20].copy_from_slice(&self.name.to_ne_bytes());
        bytes[20..24].copy_from_slice(&self.result.to_ne_bytes());
        bytes[24..28].copy_from_slice(&self.tid.to_ne_bytes());
        bytes
    }

    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < Self::LEN {
            return None;
        }
        let word = |at: usize| u32::from_ne_bytes(payload[at..at + 4].try_into().expect("4"));
        let long = |at: usize| u64::from_ne_bytes(payload[at..at + 8].try_into().expect("8"));
        Some(Self {
            start_ns: long(0),
            duration_ns: long(8),
            name: word(16),
            result: word(20) as i32,
            tid: word(24),
        })
    }
}

pub mod cupti {
    pub const KIND_MEMCPY: u32 = 1;
    pub const KIND_MEMSET: u32 = 2;
    pub const KIND_KERNEL: u32 = 3;
    pub const KIND_CONCURRENT_KERNEL: u32 = 10;
    pub const SUCCESS: u32 = 0;
    pub const ERROR_MAX_LIMIT_REACHED: u32 = 12;
    pub const FLUSH_FORCED: u32 = 1;

    /// Field offsets every kernel record version since `CUpti_ActivityKernel4`
    /// shares (measured with `offsetof` against CUPTI 13.4's headers, and
    /// pinned in a test): what is read here is the common prefix, so the
    /// record's version does not matter.
    pub mod kernel {
        pub const START: usize = 16;
        pub const END: usize = 24;
        pub const DEVICE: usize = 40;
        pub const CONTEXT: usize = 44;
        pub const STREAM: usize = 48;
        pub const GRID: usize = 52;
        pub const BLOCK: usize = 64;
        pub const CORRELATION: usize = 92;
        pub const NAME: usize = 104;
        pub const REGISTERS: usize = 6;
        pub const STATIC_SHARED: usize = 76;
        pub const DYNAMIC_SHARED: usize = 80;
        pub const LEN: usize = 112;
    }

    /// The same, for `CUpti_ActivityMemcpy` through `Memcpy7` and
    /// `CUpti_ActivityMemset` through `Memset5`.
    pub mod copy {
        pub const COPY_KIND: usize = 4;
        pub const MEMSET_VALUE: usize = 4;
        pub const BYTES: usize = 8;
        pub const START: usize = 16;
        pub const END: usize = 24;
        pub const DEVICE: usize = 32;
        pub const CONTEXT: usize = 36;
        pub const STREAM: usize = 40;
        pub const CORRELATION: usize = 44;
        pub const LEN: usize = 48;
    }
}

/// The fields of a kernel frame, in order after the frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Kernel {
    pub start: u64,
    pub end: u64,
    pub device: u32,
    pub context: u32,
    pub stream: u32,
    pub correlation: u32,
    pub grid: [i32; 3],
    pub block: [i32; 3],
    pub name: u32,
    pub registers: u32,
    pub shared: u32,
}

/// The fields of a memcpy or memset frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Copy {
    pub start: u64,
    pub end: u64,
    pub bytes: u64,
    pub device: u32,
    pub context: u32,
    pub stream: u32,
    pub correlation: u32,
    /// The copy's kind (`CUpti_ActivityMemcpyKind`) or the memset's value.
    pub detail: u32,
}

pub const KERNEL_FIELDS: usize = 8 * 2 + 4 * 4 + 4 * 6 + 4 * 3;
pub const COPY_FIELDS: usize = 8 * 3 + 4 * 5;

/// Writes frames into a message buffer.
pub struct Writer<'a> {
    pub buffer: &'a mut [u8],
    pub at: usize,
    pub frames: u32,
}

impl<'a> Writer<'a> {
    pub fn new(buffer: &'a mut [u8], pid: u32) -> Self {
        assert!(buffer.len() >= MESSAGE_MAX);
        buffer[0..4].copy_from_slice(&MAGIC.to_ne_bytes());
        buffer[4..6].copy_from_slice(&VERSION.to_ne_bytes());
        buffer[6..8].copy_from_slice(&0u16.to_ne_bytes());
        buffer[8..12].copy_from_slice(&pid.to_ne_bytes());
        Self {
            buffer,
            at: HEADER,
            frames: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.frames == 0
    }

    /// Whether a frame of `payload` bytes still fits.
    pub fn fits(&self, payload: usize) -> bool {
        self.at + padded(FRAME_HEADER + payload) <= MESSAGE_MAX
    }

    /// The message so far, with its frame count written in.
    pub fn finish(&mut self) -> &[u8] {
        self.buffer[12..16].copy_from_slice(&self.frames.to_ne_bytes());
        &self.buffer[..self.at]
    }

    /// Starts over, keeping the header.
    pub fn reset(&mut self) {
        self.at = HEADER;
        self.frames = 0;
    }

    /// Appends one frame. The caller has checked [`Writer::fits`].
    pub fn frame(&mut self, op: u8, payload: &[u8]) {
        let length = padded(FRAME_HEADER + payload.len());
        let start = self.at;
        self.buffer[start..start + 4].copy_from_slice(&(length as u32).to_ne_bytes());
        self.buffer[start + 4] = op;
        self.buffer[start + 5..start + 8].fill(0);
        self.buffer[start + 8..start + 8 + payload.len()].copy_from_slice(payload);
        self.buffer[start + 8 + payload.len()..start + length].fill(0);
        self.at += length;
        self.frames += 1;
    }
}

pub fn padded(length: usize) -> usize {
    (length + 7) & !7
}

/// Appends native-endian fields to a small payload buffer.
pub struct Fields<const N: usize> {
    pub bytes: [u8; N],
    pub len: usize,
}

impl<const N: usize> Default for Fields<N> {
    fn default() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
        }
    }
}

impl<const N: usize> Fields<N> {
    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.bytes[self.len..self.len + 8].copy_from_slice(&value.to_ne_bytes());
        self.len += 8;
        self
    }

    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.bytes[self.len..self.len + 4].copy_from_slice(&value.to_ne_bytes());
        self.len += 4;
        self
    }

    pub fn i32(&mut self, value: i32) -> &mut Self {
        self.u32(value as u32)
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl Kernel {
    pub fn encode(&self) -> Fields<KERNEL_FIELDS> {
        let mut fields = Fields::default();
        fields
            .u64(self.start)
            .u64(self.end)
            .u32(self.device)
            .u32(self.context)
            .u32(self.stream)
            .u32(self.correlation);
        for value in self.grid.iter().chain(self.block.iter()) {
            fields.i32(*value);
        }
        fields.u32(self.name).u32(self.registers).u32(self.shared);
        fields
    }

    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < KERNEL_FIELDS {
            return None;
        }
        let mut read = Reader { payload, at: 0 };
        Some(Self {
            start: read.u64(),
            end: read.u64(),
            device: read.u32(),
            context: read.u32(),
            stream: read.u32(),
            correlation: read.u32(),
            grid: [read.i32(), read.i32(), read.i32()],
            block: [read.i32(), read.i32(), read.i32()],
            name: read.u32(),
            registers: read.u32(),
            shared: read.u32(),
        })
    }
}

impl Copy {
    pub fn encode(&self) -> Fields<COPY_FIELDS> {
        let mut fields = Fields::default();
        fields
            .u64(self.start)
            .u64(self.end)
            .u64(self.bytes)
            .u32(self.device)
            .u32(self.context)
            .u32(self.stream)
            .u32(self.correlation)
            .u32(self.detail);
        fields
    }

    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < COPY_FIELDS {
            return None;
        }
        let mut read = Reader { payload, at: 0 };
        Some(Self {
            start: read.u64(),
            end: read.u64(),
            bytes: read.u64(),
            device: read.u32(),
            context: read.u32(),
            stream: read.u32(),
            correlation: read.u32(),
            detail: read.u32(),
        })
    }
}

/// The range profiler, as set up on one context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CounterSetup {
    pub device: u32,
    /// The replay passes CUPTI needs for the counters asked: each launch is
    /// run this many times.
    pub passes: u32,
    /// The chip's name, interned.
    pub chip: u32,
    /// How many of the counters asked CUPTI accepted.
    pub metrics: u32,
}

/// One counter of one kernel launch.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Counter {
    /// The kernel's name and the counter's, interned.
    pub kernel: u32,
    pub metric: u32,
    /// CUPTI's correlation id of the launch, which `gpu.kernel` carries too.
    pub correlation: u32,
    pub device: u32,
    pub value: f64,
    /// Whether the launch's own blocks were all the device ran in its
    /// window, by [`WITNESS_METRIC`]: one of the `WITNESS_*` values.
    pub witness: u32,
    pub pad: u32,
    /// When the launch call was entered, in the program's
    /// `CLOCK_MONOTONIC`: the launch's work ran after it. Zero when the
    /// entry was not seen.
    pub launched_ns: u64,
}

/// The counter that says whether a launch's window held only the launch:
/// the blocks the SMs launched, which the launch's grid fixes exactly.
/// Another process's kernels running in the same window launch blocks of
/// their own, and the count is then larger (measured: 892 to 1 032 for a
/// grid of 272). Always collected, whatever else is asked.
pub const WITNESS_METRIC: &str = "sm__ctas_launched.sum";
/// The launch's blocks could not be compared: no grid known (a launch entry
/// point whose parameters the library does not read), or no witness counter
/// on this chip.
pub const WITNESS_UNKNOWN: u32 = 0;
/// The blocks launched in the window are the launch's grid, exactly.
pub const WITNESS_CLEAN: u32 = 1;
/// More blocks were launched than the grid has: other work was counted
/// with the launch's. (Fewer is not a disturbance: the range is then not
/// the launch's, and the library reports none of its batch.)
pub const WITNESS_DISTURBED: u32 = 2;

/// What PC sampling was asked on a context, and what CUPTI reports applying
/// when asked back: a configuration it accepted and did not apply shows as a
/// difference here, never as a status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PcConfig {
    pub device: u32,
    pub period_asked: u32,
    pub period_applied: u32,
    /// `CUpti_PCSamplingCollectionMode`, as read back.
    pub mode_applied: u32,
    pub stall_reasons: u32,
    pub pad: u32,
}

/// The samples one stall reason took at one program counter, since the last
/// drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PcSample {
    /// The module's CRC, as CUPTI computes it, and the offset of the program
    /// counter in its function.
    pub cubin_crc: u64,
    pub pc_offset: u64,
    /// The function's name, interned.
    pub function: u32,
    /// The stall reason's index in its context's table ([`OP_STALL`]).
    pub stall: u32,
    pub samples: u32,
    /// The source line, and its file interned, when the module carries line
    /// information; zero when it does not.
    pub line: u32,
    pub file: u32,
    pub device: u32,
}

/// One drain of PC sampling: when, over what window, and what CUPTI counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PcDrain {
    /// `CLOCK_MONOTONIC` at this drain and at the one before it (or at
    /// enable): the samples were taken between the two.
    pub at_ns: u64,
    pub since_ns: u64,
    /// CUPTI's counts over the drain: every sample taken, those it dropped,
    /// and those in kernels that are not the program's.
    pub total: u64,
    pub dropped: u64,
    pub non_user: u64,
    pub pcs: u32,
    pub device: u32,
    /// Kernel launches the library saw on the context so far: kernels ran,
    /// so a run of them with no sample at all is a configuration that was
    /// accepted and not applied.
    pub launches: u64,
}

pub const COUNTER_SETUP_FIELDS: usize = 4 * 4;
pub const COUNTER_FIELDS: usize = 4 * 4 + 8 + 4 * 2 + 8;
pub const PC_CONFIG_FIELDS: usize = 4 * 6;
pub const PC_SAMPLE_FIELDS: usize = 8 * 2 + 4 * 6;
pub const PC_DRAIN_FIELDS: usize = 8 * 5 + 4 * 2 + 8;

impl CounterSetup {
    pub fn encode(&self) -> Fields<COUNTER_SETUP_FIELDS> {
        let mut fields = Fields::default();
        fields
            .u32(self.device)
            .u32(self.passes)
            .u32(self.chip)
            .u32(self.metrics);
        fields
    }

    pub fn decode(payload: &[u8]) -> Option<Self> {
        let mut read = Reader::checked(payload, COUNTER_SETUP_FIELDS)?;
        Some(Self {
            device: read.u32(),
            passes: read.u32(),
            chip: read.u32(),
            metrics: read.u32(),
        })
    }
}

impl Counter {
    pub fn encode(&self) -> Fields<COUNTER_FIELDS> {
        let mut fields = Fields::default();
        fields
            .u32(self.kernel)
            .u32(self.metric)
            .u32(self.correlation)
            .u32(self.device)
            .u64(self.value.to_bits())
            .u32(self.witness)
            .u32(self.pad)
            .u64(self.launched_ns);
        fields
    }

    pub fn decode(payload: &[u8]) -> Option<Self> {
        let mut read = Reader::checked(payload, COUNTER_FIELDS)?;
        Some(Self {
            kernel: read.u32(),
            metric: read.u32(),
            correlation: read.u32(),
            device: read.u32(),
            value: f64::from_bits(read.u64()),
            witness: read.u32(),
            pad: read.u32(),
            launched_ns: read.u64(),
        })
    }
}

impl PcConfig {
    pub fn encode(&self) -> Fields<PC_CONFIG_FIELDS> {
        let mut fields = Fields::default();
        fields
            .u32(self.device)
            .u32(self.period_asked)
            .u32(self.period_applied)
            .u32(self.mode_applied)
            .u32(self.stall_reasons)
            .u32(self.pad);
        fields
    }

    pub fn decode(payload: &[u8]) -> Option<Self> {
        let mut read = Reader::checked(payload, PC_CONFIG_FIELDS)?;
        Some(Self {
            device: read.u32(),
            period_asked: read.u32(),
            period_applied: read.u32(),
            mode_applied: read.u32(),
            stall_reasons: read.u32(),
            pad: read.u32(),
        })
    }
}

impl PcSample {
    pub fn encode(&self) -> Fields<PC_SAMPLE_FIELDS> {
        let mut fields = Fields::default();
        fields
            .u64(self.cubin_crc)
            .u64(self.pc_offset)
            .u32(self.function)
            .u32(self.stall)
            .u32(self.samples)
            .u32(self.line)
            .u32(self.file)
            .u32(self.device);
        fields
    }

    pub fn decode(payload: &[u8]) -> Option<Self> {
        let mut read = Reader::checked(payload, PC_SAMPLE_FIELDS)?;
        Some(Self {
            cubin_crc: read.u64(),
            pc_offset: read.u64(),
            function: read.u32(),
            stall: read.u32(),
            samples: read.u32(),
            line: read.u32(),
            file: read.u32(),
            device: read.u32(),
        })
    }
}

impl PcDrain {
    pub fn encode(&self) -> Fields<PC_DRAIN_FIELDS> {
        let mut fields = Fields::default();
        fields
            .u64(self.at_ns)
            .u64(self.since_ns)
            .u64(self.total)
            .u64(self.dropped)
            .u64(self.non_user)
            .u32(self.pcs)
            .u32(self.device)
            .u64(self.launches);
        fields
    }

    pub fn decode(payload: &[u8]) -> Option<Self> {
        let mut read = Reader::checked(payload, PC_DRAIN_FIELDS)?;
        Some(Self {
            at_ns: read.u64(),
            since_ns: read.u64(),
            total: read.u64(),
            dropped: read.u64(),
            non_user: read.u64(),
            pcs: read.u32(),
            device: read.u32(),
            launches: read.u64(),
        })
    }
}

/// Reads native-endian fields from a payload whose length was checked.
pub struct Reader<'a> {
    pub payload: &'a [u8],
    pub at: usize,
}

impl<'a> Reader<'a> {
    /// A reader of a payload at least `length` bytes long.
    pub fn checked(payload: &'a [u8], length: usize) -> Option<Self> {
        (payload.len() >= length).then_some(Self { payload, at: 0 })
    }

    pub fn u64(&mut self) -> u64 {
        let value = u64::from_ne_bytes(
            self.payload[self.at..self.at + 8]
                .try_into()
                .expect("8 bytes"),
        );
        self.at += 8;
        value
    }

    pub fn u32(&mut self) -> u32 {
        let value = u32::from_ne_bytes(
            self.payload[self.at..self.at + 4]
                .try_into()
                .expect("4 bytes"),
        );
        self.at += 4;
        value
    }

    pub fn i32(&mut self) -> i32 {
        self.u32() as i32
    }
}

/// One frame of a received message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    pub op: u8,
    pub payload: &'a [u8],
}

/// A received message: who sent it, and its frames. `None` when the bytes
/// are not a message of this version.
pub fn parse(message: &[u8]) -> Option<(u32, Vec<Frame<'_>>)> {
    if message.len() < HEADER {
        return None;
    }
    let mut read = Reader {
        payload: message,
        at: 0,
    };
    if read.u32() != MAGIC {
        return None;
    }
    let version = u16::from_ne_bytes([message[4], message[5]]);
    if version != VERSION {
        return None;
    }
    read.at = 8;
    let pid = read.u32();
    let count = read.u32();
    let mut frames = Vec::with_capacity(count as usize);
    let mut at = HEADER;
    while at + FRAME_HEADER <= message.len() {
        let length = u32::from_ne_bytes(message[at..at + 4].try_into().expect("4 bytes")) as usize;
        if length < FRAME_HEADER || at + length > message.len() {
            break;
        }
        frames.push(Frame {
            op: message[at + 4],
            payload: &message[at + FRAME_HEADER..at + length],
        });
        at += length;
    }
    Some((pid, frames))
}
