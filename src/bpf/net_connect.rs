// SPDX-License-Identifier: MIT OR GPL-2.0
//! `net.tcp.connect`: how long a TCP handshake took, from the caller's side.
//!
//! The interval opens where the connecting task asks for it and closes where
//! the kernel learns the answer, and those are two different contexts:
//!
//! | Moment | Function | Context |
//! |---|---|---|
//! | the SYN is sent | `tcp_v4_connect`, `tcp_v6_connect` | the connecting task |
//! | the SYN-ACK arrives | `tcp_finish_connect` | softirq, on whatever cpu took the packet |
//! | a RST or a timeout | `tcp_done_with_error` | softirq or a timer |
//! | the connect itself failed | the exit half of `tcp_v4_connect` | the connecting task |
//!
//! So the stamp is keyed by the **socket** rather than the thread, and carries
//! the connecting task with it: at the close the current task is whoever the
//! softirq interrupted, and naming that task would attribute a handshake to a
//! bystander.
//!
//! Every connection ends in exactly one of three outcomes, and each is an
//! event: `established`, `refused` (the peer or a timer ended it, with the
//! errno the kernel gave the application), or `failed` (the connect call
//! returned an error before any packet left). A handshake a program abandoned
//! -- closed while still in `SYN_SENT` -- closes through none of them, and is
//! counted at the end of the run as still connecting rather than guessed at.
//!
//! **The cookie is generated here**, as `net.socket` does and for the same
//! reason: a cookie read out of `skc_cookie` is zero on a socket nothing has
//! dumped, and this one is the number `sock_diag` and `SO_COOKIE` report, so a
//! connection is one key from its handshake to its last byte.

use std::os::fd::{AsRawFd, OwnedFd};

/// `net.tcp.accept`'s program, the other end of a handshake.
#[path = "net_accept.rs"]
pub mod accept;

use crate::bpf::attach::fentry::{self, Function, Link, Side, BPF_H, GET_SOCKET_COOKIE, SKIP};
use crate::bpf::btf::Btf;
use crate::bpf::insn::{
    self, count_drop, helper, push_current_filter, read_field, Insn, Program, Whose, BPF_DW, BPF_W,
    R10,
};
use crate::bpf::map::{map_failure, Counters, Hash};
use crate::bpf::ring::{ring_bytes, RingBuffer};
use crate::bpf::syscall::{self, BPF_MAP_TYPE_RINGBUF};

/// The prototypes these programs read. `tcp_v4_connect` took a `sockaddr *`
/// until the 7.x series and a `sockaddr_unsized *` since; the program reads
/// neither, so both are accepted.
pub(crate) const CONNECT_SIGNATURES: [&[&str]; 2] = [
    &["sock *", "sockaddr_unsized *", "int"],
    &["sock *", "sockaddr *", "int"],
];
const FINISH_SIGNATURE: [&str; 2] = ["sock *", "sk_buff *"];
const DONE_SIGNATURE: [&str; 2] = ["sock *", "int"];
/// `tcp_done(sk)`, which `tcp_done_with_error` calls once it has written the
/// errno into `sk->sk_err`: where a refusal is read on a kernel that inlined
/// `tcp_done_with_error` into its callers and keeps an out-of-line copy
/// nobody calls -- a clang build with link-time optimisation does (OrbStack's
/// 7.2 on aarch64: five refused connects, `tcp_done_with_error` run zero
/// times, `tcp_done` five). The first of the two to run takes the socket's
/// stamp, so on a kernel where both run the second finds none.
const DONE_BEHIND_SIGNATURE: [&str; 1] = ["sock *"];

/// How a handshake ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Established = 0,
    /// A RST, or a timer giving up: the errno says which.
    Refused = 1,
    /// The connect call failed before a SYN left.
    Failed = 2,
}

impl Outcome {
    fn from_record(value: u32) -> Option<Self> {
        match value {
            0 => Some(Outcome::Established),
            1 => Some(Outcome::Refused),
            2 => Some(Outcome::Failed),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Outcome::Established => "established",
            Outcome::Refused => "refused",
            Outcome::Failed => "failed",
        }
    }
}

/// Handshakes that may be in flight at once before new ones stop being timed.
///
/// A handshake lasts a round trip, so four thousand in flight at one
/// millisecond each is four million connections a second. A machine past that
/// is counted as losing them, and the scenarios drive exactly that: five
/// thousand connects parked in `SYN_SENT` against a listener that drops them.
pub const MAX_CONNECTING: u32 = 4096;

const DROP_PENDING: u32 = 0;
const DROP_RING: u32 = 1;

/// 256 KiB of records at 112 bytes each, drained every ten milliseconds: more
/// than two hundred thousand handshakes a second.
pub const RING_BYTES: usize = 256 * 1024;

/// One handshake, as the program writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connect {
    pub ts_ns: u64,
    pub duration_ns: u64,
    /// Zero only where the kernel had no socket to give one to.
    pub cookie: u64,
    saddr: [u8; 16],
    daddr: [u8; 16],
    pub family: u32,
    /// The local port, in host order: `skc_num` is kept that way.
    pub sport: u16,
    /// The peer port, in host order.
    pub dport: u16,
    pub outcome: Option<Outcome>,
    /// The errno the application was, or will be, given; zero when
    /// established.
    pub errno: i32,
    /// The task that called `connect`, carried from the stamp.
    pub tid: u32,
    pub tgid: u32,
    pub comm: String,
}

const RECORD_BYTES: usize = 104;
const EVENT: i16 = -104;
const R_TS: i16 = 0;
const R_DURATION: i16 = 8;
const R_COOKIE: i16 = 16;
const R_SADDR: i16 = 24;
const R_DADDR: i16 = 40;
const R_FAMILY: i16 = 56;
const R_SPORT: i16 = 60;
const R_DPORT: i16 = 64;
const R_OUTCOME: i16 = 68;
const R_ERRNO: i16 = 72;
const R_TID: i16 = 76;
const R_TGID: i16 = 80;
/// Four bytes of padding at 84, cleared with the rest and never read.
const R_COMM: i16 = 88;

/// The pending map: socket pointer to the stamp and who made it.
const PEND_KEY: i16 = -112;
const PEND_VALUE: i16 = -144;
const PEND_TS: i16 = 0;
const PEND_WHO: i16 = 8;
const PEND_COMM: i16 = 16;
const PEND_VALUE_BYTES: usize = 32;
const DROP_KEY: i16 = -148;

/// `AF_INET6`, from the frozen `socket(2)` numbering.
const AF_INET6: i32 = 10;

impl Connect {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_BYTES {
            return None;
        }
        let word = |at: i16| {
            u32::from_ne_bytes(bytes[at as usize..at as usize + 4].try_into().expect("4"))
        };
        let long = |at: i16| {
            u64::from_ne_bytes(bytes[at as usize..at as usize + 8].try_into().expect("8"))
        };
        let mut saddr = [0u8; 16];
        saddr.copy_from_slice(&bytes[R_SADDR as usize..R_SADDR as usize + 16]);
        let mut daddr = [0u8; 16];
        daddr.copy_from_slice(&bytes[R_DADDR as usize..R_DADDR as usize + 16]);
        let comm = &bytes[R_COMM as usize..R_COMM as usize + 16];
        let end = comm.iter().position(|b| *b == 0).unwrap_or(comm.len());
        Some(Self {
            ts_ns: long(R_TS),
            duration_ns: long(R_DURATION),
            cookie: long(R_COOKIE),
            saddr,
            daddr,
            family: word(R_FAMILY),
            sport: word(R_SPORT) as u16,
            dport: u16::from_be(word(R_DPORT) as u16),
            outcome: Outcome::from_record(word(R_OUTCOME)),
            errno: word(R_ERRNO) as i32,
            tid: word(R_TID),
            tgid: word(R_TGID),
            comm: String::from_utf8_lossy(&comm[..end]).into_owned(),
        })
    }

    fn address(&self, bytes: &[u8; 16]) -> Option<String> {
        match self.family {
            2 => Some(
                std::net::Ipv4Addr::from(<[u8; 4]>::try_from(&bytes[..4]).expect("4")).to_string(),
            ),
            10 => Some(std::net::Ipv6Addr::from(*bytes).to_string()),
            _ => None,
        }
    }

    pub fn local_address(&self) -> Option<String> {
        self.address(&self.saddr)
    }

    pub fn peer_address(&self) -> Option<String> {
        self.address(&self.daddr)
    }
}

struct Offsets {
    family: i32,
    num: i32,
    /// `inet_sock`'s `inet_sport`, network order, from the `sock`: the port a
    /// closing socket still names after `inet_put_port` zeroed `skc_num`.
    sport: i32,
    dport: i32,
    daddr: i32,
    rcv_saddr: i32,
    v6_daddr: i32,
    v6_rcv_saddr: i32,
    err: i32,
}

impl Offsets {
    fn read(btf: &Btf) -> Result<Self, String> {
        let common = btf.member_offset("sock", "__sk_common")?;
        let at = |member: &str| {
            btf.member_offset("sock_common", member)
                .map(|offset| common + offset)
        };
        Ok(Self {
            family: at("skc_family")?,
            num: at("skc_num")?,
            sport: btf.member_offset("inet_sock", "inet_sport")?
                - btf.member_offset("inet_sock", "sk")?,
            dport: at("skc_dport")?,
            daddr: at("skc_daddr")?,
            rcv_saddr: at("skc_rcv_saddr")?,
            v6_daddr: at("skc_v6_daddr")?,
            v6_rcv_saddr: at("skc_v6_rcv_saddr")?,
            err: btf.member_offset("sock", "sk_err")?,
        })
    }
}

/// The entry half of `tcp_v4_connect` or `tcp_v6_connect`: stamp the socket
/// with the moment and the task.
///
/// The connect call runs in the task that made it, so whose it is -- a
/// process, a thread, a cgroup or anything below it -- is asked of the current
/// task, here and only here: the closing halves run in softirq context, where
/// the current task is whoever was interrupted, and they close only what this
/// half stamped.
fn open_instructions(
    function: &Function,
    whose: Whose,
    pending_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    // The pending key's slot is the filter's scratch: it is written after.
    push_current_filter(&mut program, whose, PEND_KEY, SKIP);
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_DW, R10, PEND_VALUE + PEND_WHO, 0));
    program.push(Insn::ldx(BPF_DW, 1, 6, function.arg(0)));
    program.push(Insn::stx(BPF_DW, R10, PEND_KEY, 1));
    program.push(Insn::st_imm(BPF_DW, R10, PEND_VALUE + PEND_COMM, 0));
    program.push(Insn::st_imm(BPF_DW, R10, PEND_VALUE + PEND_COMM + 8, 0));
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, (PEND_VALUE + PEND_COMM) as i32));
    program.push(Insn::mov64_imm(2, 16));
    program.push(Insn::call(helper::GET_CURRENT_COMM));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, PEND_VALUE + PEND_TS, 0));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, PEND_VALUE as i32));
    // Any: a socket reconnected after a failure is a new handshake, and the
    // stamp of the old one has nothing left to close it.
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to(SKIP, Insn::jsge_imm(0, 0, 0));
    // The map is full: this handshake will never be timed.
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_PENDING, SKIP);
    program.label(SKIP);
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    insn::audit(&instructions)?;
    Ok(instructions)
}

/// How a closing program learns the outcome and the errno.
#[derive(Clone, Copy)]
enum Close {
    /// `tcp_finish_connect`: established, errno zero.
    Finished,
    /// `tcp_done_with_error(sk, err)`: the errno is the second argument.
    Done,
    /// `tcp_done(sk)`: the errno is `sk->sk_err`, and a socket done with
    /// none -- closed while its handshake was under way -- is not a refusal
    /// and is left to the connect call's return.
    DoneBehind,
    /// The exit half of the connect call: only a non-zero return closes, and
    /// the errno is its negation.
    Returned,
}

/// A closing program: find the socket's stamp, fill one record, publish it.
fn close_instructions(
    function: &Function,
    close: Close,
    at: &Offsets,
    pending_fd: i32,
    ring_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    if let Close::Returned = close {
        // Zero is a handshake under way, which one of the other two closes.
        program.push(Insn::ldx(BPF_DW, 1, 6, function.ret()));
        program.jump_to("done", Insn::jeq_imm(1, 0, 0));
    }
    program.push(Insn::ldx(BPF_DW, 7, 6, function.arg(0)));
    if let Close::DoneBehind = close {
        program.push(Insn::ldx(BPF_W, 1, 7, at.err as i16));
        program.jump_to("done", Insn::jeq_imm(1, 0, 0));
    }
    program.push(Insn::stx(BPF_DW, R10, PEND_KEY, 7));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    // A socket this run never saw connect: a server's accepted child, or one
    // that began its handshake before the programs attached.
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));

    // Every byte of the record, then what the stamp holds, copied out before
    // the delete frees it.
    for slot in 0..(RECORD_BYTES as i16 / 8) {
        program.push(Insn::st_imm(BPF_DW, R10, EVENT + slot * 8, 0));
    }
    program.push(Insn::ldx(BPF_DW, 9, 0, PEND_TS));
    program.push(Insn::ldx(BPF_DW, 1, 0, PEND_WHO));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_TID, 1));
    program.push(Insn::rsh64_imm(1, 32));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_TGID, 1));
    program.push(Insn::ldx(BPF_DW, 1, 0, PEND_COMM));
    program.push(Insn::stx(BPF_DW, R10, EVENT + R_COMM, 1));
    program.push(Insn::ldx(BPF_DW, 1, 0, PEND_COMM + 8));
    program.push(Insn::stx(BPF_DW, R10, EVENT + R_COMM + 8, 1));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, EVENT + R_TS, 0));
    program.push(Insn::sub64_reg(0, 9));
    program.push(Insn::stx(BPF_DW, R10, EVENT + R_DURATION, 0));

    match close {
        Close::Finished => {
            program.push(Insn::st_imm(
                BPF_W,
                R10,
                EVENT + R_OUTCOME,
                Outcome::Established as i32,
            ));
        }
        Close::Done => {
            program.push(Insn::st_imm(
                BPF_W,
                R10,
                EVENT + R_OUTCOME,
                Outcome::Refused as i32,
            ));
            program.push(Insn::ldx(BPF_DW, 1, 6, function.arg(1)));
            program.push(Insn::stx(BPF_W, R10, EVENT + R_ERRNO, 1));
        }
        Close::DoneBehind => {
            program.push(Insn::st_imm(
                BPF_W,
                R10,
                EVENT + R_OUTCOME,
                Outcome::Refused as i32,
            ));
            program.push(Insn::ldx(BPF_W, 1, 7, at.err as i16));
            program.push(Insn::stx(BPF_W, R10, EVENT + R_ERRNO, 1));
        }
        Close::Returned => {
            program.push(Insn::st_imm(
                BPF_W,
                R10,
                EVENT + R_OUTCOME,
                Outcome::Failed as i32,
            ));
            program.push(Insn::ldx(BPF_DW, 2, 6, function.ret()));
            program.push(Insn::mov64_imm(1, 0));
            program.push(Insn::sub64_reg(1, 2));
            program.push(Insn::stx(BPF_W, R10, EVENT + R_ERRNO, 1));
        }
    }

    // The socket's names. The cookie is asked for, which generates it; the
    // family and the ports are typed loads the verifier checks.
    program.push(Insn::mov64_reg(1, 7));
    program.push(Insn::call(GET_SOCKET_COOKIE));
    program.push(Insn::stx(BPF_DW, R10, EVENT + R_COOKIE, 0));
    program.push(Insn::ldx(BPF_H, 1, 7, at.family as i16));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_FAMILY, 1));
    program.push(Insn::ldx(BPF_H, 2, 7, at.num as i16));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_SPORT, 2));
    program.push(Insn::ldx(BPF_H, 2, 7, at.dport as i16));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_DPORT, 2));
    // The addresses are byte arrays, which a typed load does not span; they
    // are copied with a probe read, sixteen bytes for v6 and four for v4 into
    // the front of a slot that is already zero.
    program.jump_to("v6", Insn::jeq_imm(1, AF_INET6, 0));
    read_field(&mut program, EVENT + R_SADDR, 4, 7, at.rcv_saddr);
    read_field(&mut program, EVENT + R_DADDR, 4, 7, at.daddr);
    program.jump_to("named", Insn::ja());
    program.label("v6");
    read_field(&mut program, EVENT + R_SADDR, 16, 7, at.v6_rcv_saddr);
    read_field(&mut program, EVENT + R_DADDR, 16, 7, at.v6_daddr);
    program.label("named");

    program.extend(Insn::load_map_fd(1, ring_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, EVENT as i32));
    program.push(Insn::mov64_imm(3, RECORD_BYTES as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::RINGBUF_OUTPUT));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_RING, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    insn::audit(&instructions)?;
    Ok(instructions)
}

/// `inet_sock_set_state(sock *, int, int)`: the road that needs no trampoline.
const STATE_SIGNATURE: [&str; 3] = ["sock *", "int", "int"];

/// The TCP states the road reads, the frozen numbering `/proc/net/tcp` prints.
const TCP_ESTABLISHED: i32 = 1;
const TCP_SYN_SENT: i32 = 2;

/// The road over `inet_sock_set_state`: one raw tracepoint where the fentry
/// road attaches six trampolines, whose detach is most of what a short run
/// costs (`docs/bench.md`).
///
/// A socket entering `SYN_SENT` is stamped, in the connecting task's context,
/// which is where `tcp_v4_connect` and `tcp_v6_connect` set it; a socket
/// leaving `SYN_SENT` closes the stamp: `ESTABLISHED` is established, `CLOSE`
/// is refused with `sk_err`'s errno (a RST or a timer). A connect that failed
/// before the SYN left never entered `SYN_SENT`, so `failed` is not seen; and
/// the cookie is `skc_cookie` as it stands -- zero on a socket nothing asked
/// for one -- since a raw tracepoint cannot call the helper that generates it.
fn state_instructions(
    whose: Whose,
    at: &Offsets,
    cookie_at: i32,
    pending_fd: i32,
    ring_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 1, 6, 16));
    program.jump_to("open", Insn::jeq_imm(1, TCP_SYN_SENT, 0));
    program.push(Insn::ldx(BPF_DW, 1, 6, 8));
    program.jump_to("done", Insn::jne_imm(1, TCP_SYN_SENT, 0));

    // Leaving SYN_SENT: close the stamp.
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));
    program.push(Insn::stx(BPF_DW, R10, PEND_KEY, 7));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    for slot in 0..(RECORD_BYTES as i16 / 8) {
        program.push(Insn::st_imm(BPF_DW, R10, EVENT + slot * 8, 0));
    }
    program.push(Insn::ldx(BPF_DW, 9, 0, PEND_TS));
    program.push(Insn::ldx(BPF_DW, 1, 0, PEND_WHO));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_TID, 1));
    program.push(Insn::rsh64_imm(1, 32));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_TGID, 1));
    program.push(Insn::ldx(BPF_DW, 1, 0, PEND_COMM));
    program.push(Insn::stx(BPF_DW, R10, EVENT + R_COMM, 1));
    program.push(Insn::ldx(BPF_DW, 1, 0, PEND_COMM + 8));
    program.push(Insn::stx(BPF_DW, R10, EVENT + R_COMM + 8, 1));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, EVENT + R_TS, 0));
    program.push(Insn::sub64_reg(0, 9));
    program.push(Insn::stx(BPF_DW, R10, EVENT + R_DURATION, 0));

    program.push(Insn::ldx(BPF_DW, 1, 6, 16));
    program.jump_to("refused", Insn::jne_imm(1, TCP_ESTABLISHED, 0));
    program.push(Insn::st_imm(
        BPF_W,
        R10,
        EVENT + R_OUTCOME,
        Outcome::Established as i32,
    ));
    program.jump_to("outcome", Insn::ja());
    program.label("refused");
    // `TCP_CLOSE`, or a state no kernel enters from SYN_SENT today: either
    // way the handshake did not establish, and `sk_err` says why.
    program.push(Insn::st_imm(
        BPF_W,
        R10,
        EVENT + R_OUTCOME,
        Outcome::Refused as i32,
    ));
    read_field(&mut program, EVENT + R_ERRNO, 4, 7, at.err);
    program.label("outcome");

    read_field(&mut program, EVENT + R_COOKIE, 8, 7, cookie_at);
    read_field(&mut program, EVENT + R_FAMILY, 2, 7, at.family);
    // `inet_sport`, not `skc_num`: a refusal's `TCP_CLOSE` is announced after
    // `inet_put_port` zeroed the number, never the port. Its two bytes are in
    // network order, so each goes to the other's place in the zeroed slot.
    read_field(&mut program, EVENT + R_SPORT, 1, 7, at.sport + 1);
    read_field(&mut program, EVENT + R_SPORT + 1, 1, 7, at.sport);
    read_field(&mut program, EVENT + R_DPORT, 2, 7, at.dport);
    program.push(Insn::ldx(BPF_W, 1, R10, EVENT + R_FAMILY));
    program.jump_to("v6", Insn::jeq_imm(1, AF_INET6, 0));
    read_field(&mut program, EVENT + R_SADDR, 4, 7, at.rcv_saddr);
    read_field(&mut program, EVENT + R_DADDR, 4, 7, at.daddr);
    program.jump_to("named", Insn::ja());
    program.label("v6");
    read_field(&mut program, EVENT + R_SADDR, 16, 7, at.v6_rcv_saddr);
    read_field(&mut program, EVENT + R_DADDR, 16, 7, at.v6_daddr);
    program.label("named");
    program.extend(Insn::load_map_fd(1, ring_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, EVENT as i32));
    program.push(Insn::mov64_imm(3, RECORD_BYTES as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::RINGBUF_OUTPUT));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_RING, "done");
    program.jump_to("done", Insn::ja());

    // Entering SYN_SENT: the connecting task's context, compared here.
    program.label("open");
    push_current_filter(&mut program, whose, PEND_KEY, "done");
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_DW, R10, PEND_VALUE + PEND_WHO, 0));
    program.push(Insn::ldx(BPF_DW, 1, 6, 0));
    program.push(Insn::stx(BPF_DW, R10, PEND_KEY, 1));
    program.push(Insn::st_imm(BPF_DW, R10, PEND_VALUE + PEND_COMM, 0));
    program.push(Insn::st_imm(BPF_DW, R10, PEND_VALUE + PEND_COMM + 8, 0));
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, (PEND_VALUE + PEND_COMM) as i32));
    program.push(Insn::mov64_imm(2, 16));
    program.push(Insn::call(helper::GET_CURRENT_COMM));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, PEND_VALUE + PEND_TS, 0));
    program.extend(Insn::load_map_fd(1, pending_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, PEND_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, PEND_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_PENDING, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    insn::audit(&instructions)?;
    Ok(instructions)
}

/// The programs, attached, and the ring between them and userspace.
pub struct ConnectProgram {
    ring: RingBuffer,
    drops: Counters,
    pending: Hash,
    links: Vec<Link>,
    /// The `inet_sock_set_state` road's program and link, in place of
    /// `links`.
    state: Option<(OwnedFd, OwnedFd)>,
    /// Whether `tcp_v6_connect` was found and timed.
    pub v6: bool,
}

impl ConnectProgram {
    pub fn load(whose: Whose) -> Result<Self, String> {
        let btf = crate::bpf::kernel_btf()?;
        let offsets = Offsets::read(btf)?;
        let v4 = fentry::expect("tcp_v4_connect", &CONNECT_SIGNATURES)?;
        // IPv6 can be a module, whose functions are in its own BTF.
        let v6 = fentry::expect("tcp_v6_connect", &CONNECT_SIGNATURES).ok();
        let finish = fentry::expect("tcp_finish_connect", &[&FINISH_SIGNATURE])?;
        let done = fentry::expect("tcp_done_with_error", &[&DONE_SIGNATURE])?;
        let done_behind = fentry::expect("tcp_done", &[&DONE_BEHIND_SIGNATURE])?;

        let bytes = ring_bytes(RING_BYTES);
        let ring_map =
            syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_conn_ring")
                .map_err(|error| map_failure("connect ring buffer", error))?;
        let drops = Counters::create(2, "omn_conn_drop")
            .map_err(|error| map_failure("connect drop counter", error))?;
        let pending = Hash::create(8, PEND_VALUE_BYTES, MAX_CONNECTING, "omn_conn_pend")
            .map_err(|error| map_failure("connect pending map", error))?;
        let (p, r, d) = (pending.as_raw_fd(), ring_map.as_raw_fd(), drops.as_raw_fd());

        let mut closes = vec![
            (
                &finish,
                Side::Entry,
                close_instructions(&finish, Close::Finished, &offsets, p, r, d)?,
            ),
            (
                &done,
                Side::Entry,
                close_instructions(&done, Close::Done, &offsets, p, r, d)?,
            ),
            (
                &done_behind,
                Side::Entry,
                close_instructions(&done_behind, Close::DoneBehind, &offsets, p, r, d)?,
            ),
            (
                &v4,
                Side::Exit,
                close_instructions(&v4, Close::Returned, &offsets, p, r, d)?,
            ),
        ];
        let mut opens = vec![(&v4, open_instructions(&v4, whose, p, d)?)];
        if let Some(v6) = v6.as_ref() {
            closes.push((
                v6,
                Side::Exit,
                close_instructions(v6, Close::Returned, &offsets, p, r, d)?,
            ));
            opens.push((v6, open_instructions(v6, whose, p, d)?));
        }
        // Every close before any open, for the reason `HistogramProgram::load`
        // gives.
        let mut links = Vec::new();
        for (function, side, instructions) in &closes {
            links.push(fentry::attach(
                function,
                *side,
                instructions,
                "omn_conn_close",
            )?);
        }
        for (function, instructions) in &opens {
            links.push(fentry::attach(
                function,
                Side::Entry,
                instructions,
                "omn_conn_open",
            )?);
        }
        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the connect ring buffer: {error}"))?;
        Ok(Self {
            ring,
            drops,
            pending,
            links,
            state: None,
            v6: v6.is_some(),
        })
    }

    /// The road over `inet_sock_set_state` (`bpf:raw_tp`): see
    /// [`state_instructions`] for what it does not see.
    pub fn load_state(whose: Whose) -> Result<Self, String> {
        let btf = crate::bpf::kernel_btf()?;
        let offsets = Offsets::read(btf)?;
        let cookie_at = btf.member_offset("sock", "__sk_common")?
            + btf.member_offset("sock_common", "skc_cookie")?;
        let signature = btf.tracepoint_signature("inet_sock_set_state")?;
        if signature != STATE_SIGNATURE {
            return Err(format!(
                "this kernel declares `inet_sock_set_state({})`, and this program is written for \
                 `inet_sock_set_state({})`",
                signature.join(", "),
                STATE_SIGNATURE.join(", ")
            ));
        }
        let bytes = ring_bytes(RING_BYTES);
        let ring_map =
            syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_conn_ring")
                .map_err(|error| map_failure("connect ring buffer", error))?;
        let drops = Counters::create(2, "omn_conn_drop")
            .map_err(|error| map_failure("connect drop counter", error))?;
        let pending = Hash::create(8, PEND_VALUE_BYTES, MAX_CONNECTING, "omn_conn_pend")
            .map_err(|error| map_failure("connect pending map", error))?;
        let instructions = state_instructions(
            whose,
            &offsets,
            cookie_at,
            pending.as_raw_fd(),
            ring_map.as_raw_fd(),
            drops.as_raw_fd(),
        )?;
        let program = syscall::prog_load(
            syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
            &instructions,
            "omn_conn_state",
            "Dual MIT/GPL",
        )
        .map_err(|error| {
            if error.is_permission() {
                crate::bpf::permission_advice("loading a raw tracepoint program")
            } else {
                format!("the kernel refused the connect state program: {error}")
            }
        })?;
        let link = syscall::raw_tracepoint_open("inet_sock_set_state", program.as_raw_fd())
            .map_err(|error| {
                format!("cannot attach to the `inet_sock_set_state` tracepoint: {error}")
            })?;
        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the connect ring buffer: {error}"))?;
        Ok(Self {
            ring,
            drops,
            pending,
            links: Vec::new(),
            state: Some((program, link)),
            v6: true,
        })
    }

    pub fn drain(&mut self, mut handle: impl FnMut(Connect), undecoded: &mut u64) {
        self.ring.drain(|bytes| match Connect::from_bytes(bytes) {
            Some(record) => handle(record),
            None => *undecoded += 1,
        });
    }

    /// `(pending, ring)`: handshakes never timed, and handshakes timed and
    /// never seen.
    pub fn dropped(&self) -> (u64, u64) {
        (self.drops.read(DROP_PENDING), self.drops.read(DROP_RING))
    }

    /// Runs the kernel skipped; see `fentry::Link::misses`.
    pub fn missed(&self) -> u64 {
        fentry::misses(&self.links) + self.skipped()
    }

    /// Runs of the `inet_sock_set_state` road's program the kernel skipped
    /// because one was already running on that cpu; zero on the fentry road,
    /// whose misses are the links'.
    pub fn skipped(&self) -> u64 {
        self.state
            .as_ref()
            .map_or(0, |(program, _)| fentry::skipped([program]))
    }

    /// Handshakes stamped and not closed: still in `SYN_SENT`, or abandoned by
    /// a program that closed the socket before an answer came.
    pub fn still_connecting(&self) -> usize {
        self.pending.len().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_layout_covers_every_byte_and_the_stack_does_not_overlap() {
        let fields = [
            (R_TS, 8),
            (R_DURATION, 8),
            (R_COOKIE, 8),
            (R_SADDR, 16),
            (R_DADDR, 16),
            (R_FAMILY, 4),
            (R_SPORT, 4),
            (R_DPORT, 4),
            (R_OUTCOME, 4),
            (R_ERRNO, 4),
            (R_TID, 4),
            (R_TGID, 4),
            (84, 4),
            (R_COMM, 16),
        ];
        let mut covered = [false; RECORD_BYTES];
        for (at, size) in fields {
            for byte in at..at + size {
                assert!(!covered[byte as usize], "byte {byte} twice");
                covered[byte as usize] = true;
            }
        }
        assert!(covered.iter().all(|byte| *byte));
        let regions = [
            (EVENT, RECORD_BYTES as i16),
            (PEND_KEY, 8),
            (PEND_VALUE, PEND_VALUE_BYTES as i16),
            (DROP_KEY, 4),
        ];
        for (index, (at, size)) in regions.iter().enumerate() {
            assert!(*at >= -512 && at + size <= 0);
            for (other_at, other_size) in &regions[index + 1..] {
                assert!(at + size <= *other_at || other_at + other_size <= *at);
            }
        }
    }

    #[test]
    fn a_record_reads_back_its_addresses_ports_and_outcome() {
        let mut bytes = [0u8; RECORD_BYTES];
        bytes[R_SADDR as usize..R_SADDR as usize + 4].copy_from_slice(&[127, 0, 0, 1]);
        bytes[R_DADDR as usize..R_DADDR as usize + 4].copy_from_slice(&[10, 1, 2, 3]);
        bytes[R_FAMILY as usize..R_FAMILY as usize + 4].copy_from_slice(&2u32.to_ne_bytes());
        bytes[R_SPORT as usize..R_SPORT as usize + 4].copy_from_slice(&40000u32.to_ne_bytes());
        bytes[R_DPORT as usize..R_DPORT as usize + 4]
            .copy_from_slice(&u32::from(8080u16.to_be()).to_ne_bytes());
        bytes[R_OUTCOME as usize..R_OUTCOME as usize + 4].copy_from_slice(&1u32.to_ne_bytes());
        bytes[R_ERRNO as usize..R_ERRNO as usize + 4].copy_from_slice(&111i32.to_ne_bytes());
        let record = Connect::from_bytes(&bytes).expect("a whole record");
        assert_eq!(record.local_address().as_deref(), Some("127.0.0.1"));
        assert_eq!(record.peer_address().as_deref(), Some("10.1.2.3"));
        assert_eq!((record.sport, record.dport), (40000, 8080));
        assert_eq!(record.outcome, Some(Outcome::Refused));
        assert_eq!(record.errno, 111);
        assert!(Connect::from_bytes(&bytes[..50]).is_none());
    }

    /// The probe answers from the catalog's line and the attach from this
    /// program: the two list the same shapes of `tcp_v4_connect`, in the same
    /// order, or a kernel reads *usable* where the attach refuses it, or
    /// *missing* where the program would have served it.
    #[test]
    fn the_catalog_lists_every_shape_of_the_connect_call_the_program_reads() {
        let catalog = crate::catalog::Catalog::embedded().expect("the embedded catalog loads");
        let entry = catalog
            .entries()
            .iter()
            .find(|entry| entry.id.as_str() == "net.tcp.connect")
            .expect("net.tcp.connect");
        let listed: Vec<Vec<String>> = entry
            .backends
            .iter()
            .flat_map(|backend| &backend.requires)
            .filter_map(|requirement| requirement.strip_prefix("fentry:"))
            .map(|line| crate::bpf::parse_raw_tracepoint(line).expect("a well-formed line"))
            .find(|(name, _)| *name == "tcp_v4_connect")
            .map(|(_, shapes)| shapes)
            .expect("`net.tcp.connect` requires `fentry:tcp_v4_connect`");
        let program: Vec<Vec<String>> = CONNECT_SIGNATURES
            .iter()
            .map(|shape| {
                shape
                    .iter()
                    .map(|argument| (*argument).to_owned())
                    .collect()
            })
            .collect();
        assert_eq!(listed, program);
    }

    /// Each of the three closes is built against this kernel, and the one on
    /// the connect call's return closes only a non-zero return.
    #[test]
    fn the_programs_are_built_against_the_running_kernel() {
        let Ok(btf) = crate::bpf::kernel_btf() else {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        };
        let offsets = Offsets::read(btf).expect("the offsets");
        let v4 = fentry::expect("tcp_v4_connect", &CONNECT_SIGNATURES).expect("tcp_v4_connect");
        let returned =
            close_instructions(&v4, Close::Returned, &offsets, 3, 4, 5).expect("a program");
        assert_eq!(
            returned[1],
            Insn::ldx(BPF_DW, 1, 6, v4.ret()),
            "the return value is read first"
        );
        assert_eq!(returned[2].code, 0x15, "and a zero return is left alone");
        for (name, signature, close) in [
            ("tcp_finish_connect", &FINISH_SIGNATURE[..], Close::Finished),
            ("tcp_done_with_error", &DONE_SIGNATURE[..], Close::Done),
            ("tcp_done", &DONE_BEHIND_SIGNATURE[..], Close::DoneBehind),
        ] {
            let function = fentry::expect(name, &[signature]).expect(name);
            let program =
                close_instructions(&function, close, &offsets, 3, 4, 5).expect("a program");
            assert!(program
                .iter()
                .any(|insn| insn.code == 0x85 && insn.imm == GET_SOCKET_COOKIE));
        }
        assert!(open_instructions(&v4, Whose::Any, 3, 5).is_ok());
    }

    /// The `inet_sock_set_state` road builds against this kernel, compares a
    /// target only on the way into SYN_SENT, and generates no cookie: a raw
    /// tracepoint cannot call that helper.
    #[test]
    fn the_state_road_builds_and_compares_the_target_at_the_syn() {
        let Ok(btf) = crate::bpf::kernel_btf() else {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        };
        assert_eq!(
            btf.tracepoint_signature("inet_sock_set_state")
                .expect("the tracepoint"),
            STATE_SIGNATURE
        );
        let offsets = Offsets::read(btf).expect("the offsets");
        let program =
            state_instructions(Whose::Process(4242), &offsets, 64, 3, 4, 5).expect("a program");
        assert!(!program
            .iter()
            .any(|insn| insn.code == 0x85 && insn.imm == GET_SOCKET_COOKIE));
        let output = program
            .iter()
            .position(|insn| insn.code == 0x85 && insn.imm == helper::RINGBUF_OUTPUT)
            .expect("the record");
        let compared = program
            .iter()
            .position(|insn| insn.imm == 4242)
            .expect("the comparison");
        assert!(
            compared > output,
            "the target is compared in the opening branch"
        );
    }

    /// A cgroup target is compared in the opening half, by the current task's
    /// ancestor at the cgroup's level, and a whole-machine one asks nothing.
    #[test]
    fn a_cgroup_target_is_compared_where_the_connect_call_runs() {
        if crate::bpf::kernel_btf().is_err() {
            eprintln!("skipped: no kernel BTF");
            return;
        }
        let v4 = fentry::expect("tcp_v4_connect", &CONNECT_SIGNATURES).expect("tcp_v4_connect");
        let calls = |whose| {
            open_instructions(&v4, whose, 3, 5)
                .expect("a program")
                .iter()
                .filter(|insn| {
                    insn.code == 0x85 && insn.imm == helper::GET_CURRENT_ANCESTOR_CGROUP_ID
                })
                .count()
        };
        let cgroup = Whose::Cgroup(insn::Cgroup { id: 4242, level: 2 });
        assert_eq!(calls(cgroup), 1);
        assert_eq!(calls(Whose::Any), 0);
        assert_eq!(calls(Whose::Process(7)), 0);
    }
}
