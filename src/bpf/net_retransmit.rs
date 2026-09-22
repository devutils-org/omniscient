//! `net.tcp.retransmit`: a segment the stack decided to send again.
//!
//! One tracepoint, `tcp_retransmit_skb`, one record per retransmission. There
//! is nothing to aggregate in the kernel here: a retransmission is rare by
//! construction -- a connection whose retransmissions are frequent is the
//! problem being looked for -- and each one carries a socket and a connection
//! state that an aggregate would have to throw away.
//!
//! **The state is on the record, and it is what makes the number readable.** A
//! retransmission from `SYN_SENT` is a handshake nobody answered; one from
//! `ESTABLISHED` is a path that lost data; one from `FIN_WAIT1` is a close that
//! did not land. They are three different faults with one count, and the
//! catalog's unit -- a count -- cannot tell them apart on its own.
//!
//! The same limit as `net_lifetime.rs` applies to the peer's address and port:
//! they live in anonymous unions of `sock_common`, which `Btf::member_offset`
//! cannot reach, so what travels here is the local endpoint.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters};
use super::net_program::{count_drop, expect_signature, load, read_field, read_narrow};
use super::ring::{ring_bytes, RingBuffer};
use super::syscall::{self, BPF_MAP_TYPE_RINGBUF};

/// The prototypes this program is written against.
///
/// The third argument, the error the send returned, arrived recently. A kernel
/// without it is served without it rather than refused.
const WITH_ERROR: [&str; 3] = ["sock *", "sk_buff *", "int"];
const WITHOUT_ERROR: [&str; 2] = ["sock *", "sk_buff *"];

/// Arguments of `tcp_retransmit_skb`, as byte offsets into the context.
const ARG_SOCK: i16 = 0;
const ARG_ERROR: i16 = 16;

/// `AF_INET6`, from the kernel's frozen address-family numbering.
const AF_INET6: i32 = 10;

/// One retransmission, as the program writes it and userspace reads it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retransmit {
    /// `bpf_ktime_get_ns`, which is `CLOCK_MONOTONIC`.
    pub ts_ns: u64,
    /// The socket cookie, or zero when the kernel never generated one.
    pub cookie: u64,
    /// The socket's inode, which `/proc/<pid>/fd` shows as `socket:[n]`.
    pub inode: u64,
    /// The local address: four bytes for `AF_INET`, sixteen for `AF_INET6`.
    saddr: [u8; 16],
    pub family: u32,
    /// The local port, in network order as the kernel holds it.
    sport: u32,
    /// The connection's state, which says what kind of retransmission this is.
    pub state: u32,
    /// What the send returned, zero when it succeeded. Zero on a kernel whose
    /// tracepoint does not carry it, which [`RetransmitProgram::has_error`]
    /// distinguishes.
    pub error: i32,
}

/// The size of the record the program writes.
const RECORD_BYTES: usize = 56;

impl Retransmit {
    /// The local port, in host order.
    pub fn local_port(&self) -> u16 {
        u16::from_be(self.sport as u16)
    }

    /// The local address, as a string, or `None` for a family this does not
    /// render.
    pub fn local_address(&self) -> Option<String> {
        match self.family {
            2 => {
                let octets: [u8; 4] = self.saddr[..4].try_into().expect("four bytes");
                Some(std::net::Ipv4Addr::from(octets).to_string())
            }
            10 => Some(std::net::Ipv6Addr::from(self.saddr).to_string()),
            _ => None,
        }
    }

    /// Reads one record out of the ring, or refuses one that is too short.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_BYTES {
            return None;
        }
        let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        let long = |at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("8"));
        let mut saddr = [0u8; 16];
        saddr.copy_from_slice(&bytes[24..40]);
        Some(Self {
            ts_ns: long(0),
            cookie: long(8),
            inode: long(16),
            saddr,
            family: word(40),
            sport: word(44),
            state: word(48),
            error: word(52) as i32,
        })
    }
}

/// Stack offsets, negative from the frame pointer, matching `Retransmit`.
const EVENT: i16 = -56;
const TS: i16 = EVENT;
const COOKIE: i16 = EVENT + 8;
const INODE: i16 = EVENT + 16;
const SADDR: i16 = EVENT + 24;
const FAMILY: i16 = EVENT + 40;
const SPORT: i16 = EVENT + 44;
const STATE: i16 = EVENT + 48;
const ERROR: i16 = EVENT + 52;
const DROP_KEY: i16 = EVENT - 8;

/// Builds the program, or says why this kernel cannot be read.
fn instructions(btf: &Btf, ring_fd: i32, drops_fd: i32) -> Result<Vec<Insn>, String> {
    let signature = expect_signature(btf, "tcp_retransmit_skb", &[&WITH_ERROR, &WITHOUT_ERROR])?;
    let has_error = signature.len() == WITH_ERROR.len();

    let sock_at = btf.member_offset("inet_sock", "sk")?;
    let common_at = btf.member_offset("sock", "__sk_common")?;
    let cookie_at = common_at + btf.member_offset("sock_common", "skc_cookie")?;
    let family_at = common_at + btf.member_offset("sock_common", "skc_family")?;
    let state_at = common_at + btf.member_offset("sock_common", "skc_state")?;
    let v6_saddr_at = common_at + btf.member_offset("sock_common", "skc_v6_rcv_saddr")?;
    let inode_at = btf.member_offset("sock", "sk_ino")?;
    let saddr_at = btf.member_offset("inet_sock", "inet_saddr")? - sock_at;
    let sport_at = btf.member_offset("inet_sock", "inet_sport")? - sock_at;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, ARG_SOCK));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, TS, 0));

    read_field(&mut program, COOKIE, 8, 7, cookie_at);
    read_field(&mut program, INODE, 8, 7, inode_at);
    read_narrow(&mut program, FAMILY, 2, 7, family_at);
    read_narrow(&mut program, SPORT, 2, 7, sport_at);
    // `skc_state` is one byte, and a narrow read is the only way to take it
    // without reading the byte beside it as part of the value.
    read_narrow(&mut program, STATE, 1, 7, state_at);

    if has_error {
        program.push(Insn::ldx(BPF_DW, 1, 6, ARG_ERROR));
        program.push(Insn::stx(BPF_W, R10, ERROR, 1));
    } else {
        program.push(Insn::st_imm(BPF_W, R10, ERROR, 0));
    }

    // Sixteen bytes cleared before either branch writes into them: a v4 address
    // fills four and the twelve behind it would otherwise be whatever the last
    // retransmission left on the stack.
    program.push(Insn::st_imm(BPF_DW, R10, SADDR, 0));
    program.push(Insn::st_imm(BPF_DW, R10, SADDR + 8, 0));
    program.push(Insn::ldx(BPF_W, 1, R10, FAMILY));
    program.jump_to("v6", Insn::jeq_imm(1, AF_INET6, 0));
    read_field(&mut program, SADDR, 4, 7, saddr_at);
    program.jump_to("addressed", Insn::ja());
    program.label("v6");
    read_field(&mut program, SADDR, 16, 7, v6_saddr_at);
    program.label("addressed");

    program.extend(Insn::load_map_fd(1, ring_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, EVENT as i32));
    program.push(Insn::mov64_imm(3, RECORD_BYTES as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::RINGBUF_OUTPUT));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, 0, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// A loaded, attached retransmission program and the maps it writes.
pub struct RetransmitProgram {
    ring: RingBuffer,
    drops: Counters,
    has_error: bool,
    _program: OwnedFd,
    _link: OwnedFd,
}

impl RetransmitProgram {
    /// Loads and attaches the program, or says why the kernel refused.
    pub fn load(requested_ring_bytes: usize) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let bytes = ring_bytes(requested_ring_bytes);
        let has_error = btf.tracepoint_signature("tcp_retransmit_skb")?.len() == WITH_ERROR.len();

        let ring_map = syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_retrans")
            .map_err(|error| map_failure("retransmission ring buffer", error))?;
        let drops = Counters::create(1, "omn_retr_lost")
            .map_err(|error| map_failure("retransmission counter", error))?;

        let instructions = instructions(btf, ring_map.as_raw_fd(), drops.as_raw_fd())?;
        let program = load(&instructions, "omn_retransmit")?;
        let link = syscall::raw_tracepoint_open("tcp_retransmit_skb", program.as_raw_fd())
            .map_err(|error| {
                format!("cannot attach to the `tcp_retransmit_skb` tracepoint: {error}")
            })?;
        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the retransmission ring buffer: {error}"))?;

        Ok(Self {
            ring,
            drops,
            has_error,
            _program: program,
            _link: link,
        })
    }

    /// Whether this kernel's tracepoint says what the send returned. Without
    /// it every record's error is zero, which must not be read as "the
    /// retransmission succeeded".
    pub fn has_error(&self) -> bool {
        self.has_error
    }

    /// Hands every complete record to `handle`, counting the ones that were not
    /// the size this reader expects rather than reinterpreting them.
    pub fn drain(&mut self, mut handle: impl FnMut(Retransmit), undecoded: &mut u64) {
        self.ring
            .drain(|bytes| match Retransmit::from_bytes(bytes) {
                Some(again) => handle(again),
                None => *undecoded += 1,
            });
    }

    /// Retransmissions the program could not publish because the ring was full.
    pub fn dropped(&self) -> u64 {
        self.drops.read(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The running kernel's BTF, read once for the whole process.
    ///
    /// `Btf::from_kernel` parses six megabytes, and there are a dozen tests in
    /// this family that want it. Reading it once is what `crate::bpf::
    /// kernel_btf` is for, and a test suite that parsed it per test loaded the
    /// machine enough to unsettle the cpu-accounting tests running beside it.
    fn kernel_btf() -> Option<&'static Btf> {
        match super::super::kernel_btf() {
            Ok(btf) => Some(btf),
            Err(reason) => {
                eprintln!("skipped: {reason}");
                None
            }
        }
    }

    #[test]
    fn the_record_struct_matches_what_the_program_writes() {
        use std::mem::offset_of;
        assert_eq!(std::mem::size_of::<Retransmit>(), RECORD_BYTES);
        for (stack, field, name) in [
            (TS, offset_of!(Retransmit, ts_ns), "ts_ns"),
            (COOKIE, offset_of!(Retransmit, cookie), "cookie"),
            (INODE, offset_of!(Retransmit, inode), "inode"),
            (SADDR, offset_of!(Retransmit, saddr), "saddr"),
            (FAMILY, offset_of!(Retransmit, family), "family"),
            (SPORT, offset_of!(Retransmit, sport), "sport"),
            (STATE, offset_of!(Retransmit, state), "state"),
            (ERROR, offset_of!(Retransmit, error), "error"),
        ] {
            assert_eq!(
                (stack - EVENT) as usize,
                field,
                "the program writes `{name}` where the struct does not read it"
            );
        }
    }

    #[test]
    fn a_record_shorter_than_the_struct_is_refused_rather_than_padded() {
        assert!(Retransmit::from_bytes(&[0u8; 32]).is_none());
    }

    /// The error is signed: a send that failed returns a negative errno, and
    /// reading it unsigned reports `-11` as four billion.
    #[test]
    fn a_failed_send_reaches_the_reader_as_a_negative_number() {
        let mut bytes = [0u8; RECORD_BYTES];
        bytes[52..56].copy_from_slice(&(-11i32).to_ne_bytes());
        assert_eq!(
            Retransmit::from_bytes(&bytes).expect("a full record").error,
            -11
        );
    }

    /// The published size is the struct's size; anything else truncates the
    /// record or publishes the stack behind it.
    #[test]
    fn the_published_record_is_exactly_the_size_of_the_struct() {
        let Some(btf) = kernel_btf() else { return };
        let program = instructions(btf, 3, 4).expect("a program");
        let output_at = program
            .iter()
            .position(|insn| insn.code == 0x85 && insn.imm == helper::RINGBUF_OUTPUT)
            .expect("the program publishes its record");
        let length = program[..output_at]
            .iter()
            .rev()
            .find(|insn| insn.regs & 0xf == 3 && insn.code == 0xb7)
            .expect("a length is moved into r3");
        assert_eq!(length.imm as usize, std::mem::size_of::<Retransmit>());
    }

    /// This kernel's tracepoint is one of the two shapes the program knows.
    #[test]
    fn this_kernels_tracepoint_is_a_shape_the_program_knows() {
        let Some(btf) = kernel_btf() else { return };
        let signature = expect_signature(btf, "tcp_retransmit_skb", &[&WITH_ERROR, &WITHOUT_ERROR])
            .expect("a kernel this program can read");
        assert_eq!(signature[0], "sock *");
        assert_eq!(signature[1], "sk_buff *");
    }

    /// Invariant 6: one clock domain.
    #[test]
    fn the_program_reads_the_monotonic_clock_and_no_other() {
        let Some(btf) = kernel_btf() else { return };
        const KTIME_GET_BOOT_NS: i32 = 125;
        let called: Vec<i32> = instructions(btf, 3, 4)
            .expect("a program")
            .iter()
            .filter(|insn| insn.code == 0x85)
            .map(|insn| insn.imm)
            .collect();
        assert!(called.contains(&helper::KTIME_GET_NS));
        assert!(!called.contains(&KTIME_GET_BOOT_NS));
    }
}
