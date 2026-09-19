//! `AF_PACKET` with a TPACKETv3 ring, spoken directly.
//!
//! A packet socket is the only mechanism in this tool that can see a frame the
//! kernel has not yet decided anything about: before the routing, before the
//! socket lookup, before any counter is incremented. Everything else in `net`
//! reports what the stack concluded.
//!
//! Three properties of the ring matter enough to be built rather than assumed.
//!
//! **The copy is capped in the kernel.** A classic BPF filter of one
//! instruction -- `ret #SNAPLEN` -- tells the kernel how many bytes of each
//! frame to copy into the ring. Set to the length of the headers this module
//! parses, the payload never crosses into this process at all. That is a
//! stronger promise than "the code does not print it": there is nothing to
//! print. The frame's real length is still reported, because `tp_len` is the
//! length on the wire and `tp_snaplen` is what was copied.
//!
//! **A full ring is a loss and the kernel counts it.** `PACKET_STATISTICS`
//! returns packets and drops since the last read, and this module reads it on
//! every drain. A capture that quietly lost a third of its packets is exactly
//! the failure this project is built against.
//!
//! **The timestamps are not `CLOCK_MONOTONIC`.** The kernel stamps a frame
//! from the realtime clock, which is a different clock domain, so the caller is
//! handed the raw stamp and a calibration taken in the same drain rather than a
//! number that pretends to be in this tool's domain.

use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU32, Ordering};

/// `SOL_PACKET`, and the socket options this module sets.
const SOL_PACKET: libc::c_int = 263;
const PACKET_RX_RING: libc::c_int = 5;
const PACKET_STATISTICS: libc::c_int = 6;
const PACKET_VERSION: libc::c_int = 10;

/// `TPACKET_V3`: variable-length packing inside a block, and a block handed
/// over on a timeout rather than only when it fills. Both matter here: a
/// capture capped at header length wastes most of a fixed frame, and a quiet
/// link must still deliver what it saw within the drain interval.
const TPACKET_V3: libc::c_int = 2;

/// Every protocol, in network byte order as `bind` wants it.
const ETH_P_ALL: u16 = 0x0003;

/// The block is filled and belongs to userspace until it is given back.
const TP_STATUS_USER: u32 = 1;

/// Offsets into `struct tpacket_block_desc`, which begins with two `u32` and
/// then carries `struct tpacket_hdr_v1`.
mod block {
    pub const STATUS: usize = 8;
    pub const NUM_PKTS: usize = 12;
    pub const OFFSET_TO_FIRST_PKT: usize = 16;
}

/// Offsets into `struct tpacket3_hdr`, and the `struct sockaddr_ll` the kernel
/// places immediately after it.
mod frame {
    pub const NEXT_OFFSET: usize = 0;
    pub const SEC: usize = 4;
    pub const NSEC: usize = 8;
    pub const SNAPLEN: usize = 12;
    pub const LEN: usize = 16;
    pub const MAC: usize = 24;
    pub const NET: usize = 26;
    /// `TPACKET3_HDRLEN` places the `sockaddr_ll` at the aligned end of the
    /// header, and `sizeof(struct tpacket3_hdr)` is already 48-byte aligned.
    pub const SLL: usize = 48;
    pub const SLL_IFINDEX: usize = SLL + 4;
    pub const SLL_PKTTYPE: usize = SLL + 10;
}

/// `TPACKET3_HDRLEN`: the aligned `struct tpacket3_hdr` and the `sockaddr_ll`
/// after it. No record is shorter than this, so a block with less space left
/// than this holds no further record.
const TPACKET3_HDRLEN: usize = frame::SLL + 20;

/// How much of each frame the kernel is asked to copy.
///
/// Enough for an Ethernet header, the largest IPv6 header this module walks,
/// and a TCP header: 14 + 40 + 20 = 74, rounded up. Anything past it is
/// payload, which this tool has no business holding.
pub const SNAPLEN: u32 = 96;

/// The ring's geometry. One mebibyte, in blocks big enough that a burst does
/// not straddle many of them.
const BLOCK_SIZE: u32 = 128 * 1024;
const BLOCK_COUNT: u32 = 8;
/// Unused by TPACKETv3's packing, but the kernel still validates it and
/// derives `tp_frame_nr` from it.
const FRAME_SIZE: u32 = 2048;
/// How long the kernel may hold a partly filled block before handing it over.
/// Shorter than the drain interval, so a quiet link still reports within one
/// tick rather than at the end of the run.
const RETIRE_TOV_MS: u32 = 5;

/// What the kernel says it did with the ring since the last time it was asked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub packets: u32,
    /// Frames the kernel had nowhere to put. Not an estimate: the socket keeps
    /// this count itself.
    pub drops: u32,
    /// Times the ring ran out of blocks and the kernel had to wait for one.
    pub queue_freezes: u32,
}

/// One captured frame, borrowed out of the ring.
///
/// Nothing is copied to produce this: it points into the mapped block, which is
/// why it cannot outlive the drain that yielded it.
pub struct Frame<'ring> {
    /// The kernel's timestamp, in its own clock domain. Converting it is the
    /// caller's job, because the caller is the one that knows what error to
    /// attach to the result.
    pub realtime_ns: u64,
    /// The frame's length on the wire, whatever was copied.
    pub wire_len: u32,
    /// How much of it reached this process.
    pub captured_len: u32,
    pub ifindex: u32,
    /// `PACKET_HOST`, `PACKET_OUTGOING` and the rest, as the kernel classified
    /// the frame.
    pub packet_type: u8,
    /// The bytes from the link-layer header onwards, capped at `SNAPLEN`.
    pub bytes: &'ring [u8],
    /// Where the network header starts within `bytes`. The kernel computed it,
    /// so a link layer this module does not know still parses.
    pub network_offset: usize,
}

/// A packet socket with a mapped receive ring.
pub struct Capture {
    fd: RawFd,
    base: *mut u8,
    map_len: usize,
    /// The block the next drain starts at. Blocks are handed over in order, so
    /// walking them in order is what keeps the sequence right.
    next_block: usize,
}

impl Capture {
    /// Opens a capture on one interface, or on every interface when `ifindex`
    /// is zero.
    ///
    /// The order is deliberate: the filter that caps the copy is attached
    /// *before* the socket is bound, so there is no window in which a full
    /// frame could be copied into the ring.
    pub fn open(ifindex: u32) -> io::Result<Self> {
        // SAFETY: creating a socket; the arguments are constants.
        let fd = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                (ETH_P_ALL as i32).to_be(),
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut capture = Self {
            fd,
            base: std::ptr::null_mut(),
            map_len: 0,
            next_block: 0,
        };

        capture.cap_the_copy()?;
        capture.set_option(PACKET_VERSION, &TPACKET_V3)?;
        capture.request_ring()?;
        capture.map_ring()?;
        capture.bind(ifindex)?;
        Ok(capture)
    }

    /// Attaches the one-instruction classic BPF program that limits how much of
    /// each frame the kernel copies.
    ///
    /// `BPF_RET | BPF_K` with a constant is the whole program: the return value
    /// of a socket filter is the number of bytes to keep. Returning `SNAPLEN`
    /// keeps every packet and copies only its headers.
    fn cap_the_copy(&self) -> io::Result<()> {
        // BPF_RET (0x06) | BPF_K (0x00).
        let mut program = [libc::sock_filter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: SNAPLEN,
        }];
        let filter = libc::sock_fprog {
            len: program.len() as u16,
            filter: program.as_mut_ptr(),
        };
        // SAFETY: the program outlives the call, and the kernel copies it.
        let set = unsafe {
            libc::setsockopt(
                self.fd,
                libc::SOL_SOCKET,
                libc::SO_ATTACH_FILTER,
                (&raw const filter).cast::<libc::c_void>(),
                std::mem::size_of::<libc::sock_fprog>() as libc::socklen_t,
            )
        };
        if set < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn set_option<T>(&self, option: libc::c_int, value: &T) -> io::Result<()> {
        // SAFETY: `value` is borrowed for the duration of the call and its size
        // is its own.
        let set = unsafe {
            libc::setsockopt(
                self.fd,
                SOL_PACKET,
                option,
                std::ptr::from_ref(value).cast::<libc::c_void>(),
                std::mem::size_of::<T>() as libc::socklen_t,
            )
        };
        if set < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// `struct tpacket_req3`, which is seven `u32` and nothing else.
    fn request_ring(&mut self) -> io::Result<()> {
        let frames_per_block = BLOCK_SIZE / FRAME_SIZE;
        let request: [u32; 7] = [
            BLOCK_SIZE,
            BLOCK_COUNT,
            FRAME_SIZE,
            frames_per_block * BLOCK_COUNT,
            RETIRE_TOV_MS,
            0, // tp_sizeof_priv
            0, // tp_feature_req_word
        ];
        self.set_option(PACKET_RX_RING, &request)?;
        self.map_len = (BLOCK_SIZE * BLOCK_COUNT) as usize;
        Ok(())
    }

    fn map_ring(&mut self) -> io::Result<()> {
        // SAFETY: mapping the ring the kernel just allocated for this socket.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                self.map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_LOCKED,
                self.fd,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            // `MAP_LOCKED` needs a memlock allowance this process may not have,
            // and the ring works without it -- it just may be paged out. Losing
            // the capture over a resource limit would be the wrong trade.
            // SAFETY: same mapping, without the lock.
            let base = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    self.map_len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    self.fd,
                    0,
                )
            };
            if base == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            self.base = base.cast::<u8>();
            return Ok(());
        }
        self.base = base.cast::<u8>();
        Ok(())
    }

    fn bind(&self, ifindex: u32) -> io::Result<()> {
        // SAFETY: a plain value, zeroed then filled.
        let mut address: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        address.sll_family = libc::AF_PACKET as libc::sa_family_t;
        address.sll_protocol = ETH_P_ALL.to_be();
        address.sll_ifindex = ifindex as libc::c_int;
        // SAFETY: binding a socket we own to an address we own.
        let bound = unsafe {
            libc::bind(
                self.fd,
                (&raw const address).cast::<libc::sockaddr>(),
                std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if bound < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Reads and resets the kernel's own accounting for this socket.
    ///
    /// `PACKET_STATISTICS` is destructive: each read reports what happened
    /// since the previous one, which is why the caller accumulates rather than
    /// taking the last answer as a total.
    pub fn statistics(&self) -> io::Result<Stats> {
        let mut stats = [0u32; 3];
        let mut length = std::mem::size_of_val(&stats) as libc::socklen_t;
        // SAFETY: writing into a buffer we own, with its true length.
        let read = unsafe {
            libc::getsockopt(
                self.fd,
                SOL_PACKET,
                PACKET_STATISTICS,
                stats.as_mut_ptr().cast::<libc::c_void>(),
                &mut length,
            )
        };
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Stats {
            packets: stats[0],
            drops: stats[1],
            // `tpacket_stats_v3` carries a third counter; an older kernel
            // answering with the v1 struct leaves it untouched at zero.
            queue_freezes: if length as usize >= std::mem::size_of_val(&stats) {
                stats[2]
            } else {
                0
            },
        })
    }

    /// Hands every frame the kernel has finished with to `each`, and gives the
    /// blocks back.
    ///
    /// Returns how many frames were yielded. Blocks are walked in the order the
    /// kernel fills them and released as soon as they are read, because a block
    /// held here is a block the kernel cannot write into.
    pub fn drain<'ring>(&'ring mut self, mut each: impl FnMut(Frame<'ring>)) -> usize {
        let mut yielded = 0usize;
        for _ in 0..BLOCK_COUNT {
            let block = self.block(self.next_block);
            // Acquire: everything the kernel wrote into the block happened
            // before it published the status, and must be visible here.
            if self.status(block).load(Ordering::Acquire) & TP_STATUS_USER == 0 {
                break;
            }

            let count = read_u32(block, block::NUM_PKTS);
            let mut offset = read_u32(block, block::OFFSET_TO_FIRST_PKT) as usize;
            for _ in 0..count {
                // The block is shared with the kernel, so every offset in it is
                // checked against the block's own size rather than trusted.
                let Some(remaining) = (BLOCK_SIZE as usize).checked_sub(offset) else {
                    break;
                };
                if remaining < TPACKET3_HDRLEN {
                    break;
                }
                // SAFETY: the offset is inside the block, which is mapped.
                let header = unsafe { block.add(offset) };
                if let Some(captured) = frame_at(header, remaining) {
                    each(captured);
                    yielded += 1;
                }
                let next = read_u32(header, frame::NEXT_OFFSET) as usize;
                if next == 0 {
                    break;
                }
                offset += next;
            }

            // Release: the reads above happen before the kernel sees the block
            // as free, or it may refill it under us.
            self.status(block).store(0, Ordering::Release);
            self.next_block = (self.next_block + 1) % BLOCK_COUNT as usize;
        }
        yielded
    }

    fn block(&self, index: usize) -> *mut u8 {
        // SAFETY: `index` is taken modulo the block count, so the result is
        // inside the mapping.
        unsafe { self.base.add(index * BLOCK_SIZE as usize) }
    }

    fn status(&self, block: *mut u8) -> &AtomicU32 {
        // SAFETY: the field is a `u32` at a fixed offset inside a mapped block,
        // written by the kernel and by this process and by nothing else, which
        // is exactly what an atomic models.
        unsafe { &*block.add(block::STATUS).cast::<AtomicU32>() }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if !self.base.is_null() {
            // SAFETY: unmapping the mapping this struct made, once.
            unsafe { libc::munmap(self.base.cast::<libc::c_void>(), self.map_len) };
        }
        // SAFETY: closing a descriptor this struct owns, once. The ring the
        // kernel allocated goes with it.
        unsafe { libc::close(self.fd) };
    }
}

/// Reads one `u32` out of the ring at a byte offset.
///
/// The ring is mapped memory the kernel writes; nothing in it is guaranteed to
/// be aligned for a Rust reference, so every field is read unaligned.
fn read_u32(base: *mut u8, offset: usize) -> u32 {
    // SAFETY: the caller has checked the offset is inside a mapped block.
    unsafe { base.add(offset).cast::<u32>().read_unaligned() }
}

fn read_u16(base: *mut u8, offset: usize) -> u16 {
    // SAFETY: the caller has checked the offset is inside a mapped block.
    unsafe { base.add(offset).cast::<u16>().read_unaligned() }
}

/// Builds a `Frame` over one `struct tpacket3_hdr`.
///
/// `remaining` is how much of the block is left from this record onwards, and
/// nothing is read past it. Returns nothing for a record whose own lengths do
/// not agree with each other or with the space they claim to be in: a record
/// this reader does not understand is one it may not guess at.
fn frame_at<'ring>(header: *mut u8, remaining: usize) -> Option<Frame<'ring>> {
    let captured_len = read_u32(header, frame::SNAPLEN);
    let wire_len = read_u32(header, frame::LEN);
    let mac = read_u16(header, frame::MAC) as usize;
    let net = read_u16(header, frame::NET) as usize;
    if captured_len > SNAPLEN || net < mac || mac.checked_add(captured_len as usize)? > remaining {
        return None;
    }

    // SAFETY: `mac` is the kernel's own offset to the frame within this record,
    // and `captured_len` is the number of bytes it copied there.
    let bytes = unsafe { std::slice::from_raw_parts(header.add(mac), captured_len as usize) };

    Some(Frame {
        realtime_ns: read_u32(header, frame::SEC) as u64 * 1_000_000_000
            + read_u32(header, frame::NSEC) as u64,
        wire_len,
        captured_len,
        ifindex: read_u32(header, frame::SLL_IFINDEX),
        // SAFETY: a single byte at a fixed offset inside the record.
        packet_type: unsafe { header.add(frame::SLL_PKTTYPE).read() },
        bytes,
        network_offset: net - mac,
    })
}

/// How the kernel classified a frame, in the words `tcpdump` uses.
pub fn packet_type_name(packet_type: u8) -> &'static str {
    match packet_type {
        0 => "host",
        1 => "broadcast",
        2 => "multicast",
        3 => "otherhost",
        4 => "outgoing",
        5 => "loopback",
        6 => "user",
        7 => "kernel",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_geometry_is_one_the_kernel_accepts() {
        // The kernel checks these three before allocating anything: the block
        // size must be a whole number of pages, the frame size must be aligned
        // and no smaller than a header, and the frame count must be exactly
        // what the other three imply.
        let page = 4096;
        assert_eq!(BLOCK_SIZE % page, 0, "a block is a whole number of pages");
        assert_eq!(FRAME_SIZE % 16, 0, "TPACKET_ALIGNMENT is 16");
        assert!(
            FRAME_SIZE as usize > frame::SLL + 16,
            "a frame holds a header"
        );
        assert_eq!(
            (BLOCK_SIZE / FRAME_SIZE) * BLOCK_COUNT,
            BLOCK_SIZE * BLOCK_COUNT / FRAME_SIZE
        );
    }

    #[test]
    fn the_snaplen_stops_before_any_payload() {
        // Ethernet, the largest IPv6 header this module walks, and a TCP header
        // with no options. A snaplen past that would be copying payload.
        const HEADERS: u32 = 14 + 40 + 20;
        // Checked when this compiles rather than when it runs: the snaplen is
        // the privacy boundary, and a build that got it wrong should not exist
        // long enough to be tested.
        const { assert!(SNAPLEN >= HEADERS, "the headers must fit") };
        const {
            assert!(
                SNAPLEN < HEADERS + 64,
                "a snaplen far past the headers is payload this tool has no business holding"
            )
        };
    }

    #[test]
    fn a_capture_without_cap_net_raw_says_which_privilege_is_missing() {
        match Capture::open(0) {
            Ok(_) => {
                // This process may capture; the ground truth for that path is
                // in `tests/family_net.rs`, which can create a namespace where
                // it holds the capability.
            }
            Err(error) => assert_eq!(
                error.raw_os_error(),
                Some(libc::EPERM),
                "the only expected refusal is the missing capability: {error}"
            ),
        }
    }
}
