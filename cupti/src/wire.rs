//! What the injection library sends `omn`, byte for byte.
//!
//! This one file is compiled into both sides: the library includes it as a
//! module, and `omn` includes the same source by path
//! (`src/collect/gpu/kernel.rs`), so the two cannot disagree on a number. It
//! depends on nothing but `core`.
//!
//! **The channel** is one `AF_UNIX` `SOCK_SEQPACKET` socket `omn` creates
//! before the launch and names in `OMN_CUPTI_FD`. A sequenced-packet socket
//! keeps message boundaries and never interleaves two senders' messages, so a
//! program that forks after initialising CUDA -- two processes writing to one
//! descriptor -- still sends whole messages, each one saying whose it is.
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
/// Records the library had and could not send, cumulative.
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

/// CUPTI's own numbers, from `cupti_activity.h`.
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

/// Reads native-endian fields from a payload whose length was checked.
pub struct Reader<'a> {
    pub payload: &'a [u8],
    pub at: usize,
}

impl Reader<'_> {
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
