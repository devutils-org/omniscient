//! `net.tcp.lifetime`: how long a connection lived, and what crossed it.
//!
//! One tracepoint, `inet_sock_set_state`, and two of its transitions. A socket
//! entering `TCP_ESTABLISHED` is a connection beginning; the same socket
//! entering `TCP_CLOSE` is that connection ending, and the gap between them is
//! the measurement. Everything else the tracepoint reports -- `SYN_SENT`,
//! `FIN_WAIT1`, `LISTEN` -- is a state change inside one of those two events.
//!
//! Both ends of a loopback connection are separate sockets and both are
//! measured, which is what makes a scenario able to check the answer: the test
//! knows which local port it bound and which it connected from, and each event
//! carries the port of the socket it belongs to.
//!
//! **The identity is the socket, and the socket's cookie is how it is named.**
//! `sock_diag` reports the same number -- it is what `net.tcp.rtt` already keys
//! its series on -- so the two sources join without a translation. The kernel
//! generates a cookie lazily, the first time something asks for one, so a
//! socket nothing has ever dumped has a cookie of zero. That is reported as no
//! cookie rather than as cookie zero: the inode travels beside it and is set
//! from the moment the socket exists.
//!
//! **What this backend cannot say, and why.** A connection's peer address and
//! peer port live in `sock_common`'s anonymous unions -- `skc_daddr` and
//! `skc_dport` are members of an anonymous struct inside an anonymous union --
//! and `Btf::member_offset` resolves a member by name inside a *named*
//! structure. Writing the offsets into this file instead is exactly the
//! staleness the whole layer exists to prevent, so what this reports is the
//! **local endpoint**: the address and port of the socket the event is about.
//! The request for a walk through anonymous members is under `## Needs from
//! another stream` in `docs/open-findings.md`.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{Counters, Hash};
use super::net_program::{
    count_drop, expect_signature, load, read_field, read_narrow, GET_CURRENT_COMM,
};
use super::ring::{ring_bytes, RingBuffer};
use super::syscall::{self, BPF_MAP_TYPE_RINGBUF};

/// The prototype this program is written against.
const SIGNATURE: [&str; 3] = ["sock *", "int", "int"];

/// Arguments of `inet_sock_set_state`, as byte offsets into the context.
const ARG_SOCK: i16 = 0;
const ARG_NEW_STATE: i16 = 16;

/// The two TCP states this source is about.
///
/// These are the numbers `/proc/net/tcp` prints in its state column and `ss`
/// decodes; they are a frozen part of the kernel's interface to userspace
/// rather than an internal enum, which is why they can be named here when a
/// struct offset cannot.
const TCP_ESTABLISHED: i32 = 1;
const TCP_CLOSE: i32 = 7;

/// `AF_INET6`, from the same frozen numbering as the states above.
const AF_INET6: i32 = 10;

/// How many connections may be open at once before the program stops being
/// able to record new ones. A machine serving ten thousand connections at an
/// instant is a large one; beyond that the losses are counted and reported.
const MAX_OPEN: u32 = 65536;

/// Slots of the drop counter.
const DROP_OPEN: u32 = 0;
const DROP_RING: u32 = 1;
const COUNTER_SLOTS: u32 = 2;

/// One closed connection, as the program writes it and userspace reads it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpLifetime {
    /// When the connection closed. `bpf_ktime_get_ns`, which is
    /// `CLOCK_MONOTONIC`.
    pub ts_ns: u64,
    /// How long it was established, in nanoseconds.
    pub duration_ns: u64,
    /// The socket cookie, or zero when the kernel never generated one.
    pub cookie: u64,
    /// The socket's inode, which `/proc/<pid>/fd` shows as `socket:[n]`.
    pub inode: u64,
    /// Payload bytes the socket sent and received, as the kernel counted them.
    /// Not segments and not wire bytes: headers and retransmissions are not in
    /// these.
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub segs_out: u32,
    pub segs_in: u32,
    /// The local address, four bytes of it for `AF_INET` and sixteen for
    /// `AF_INET6`. The peer's is not reachable; the module comment says why.
    saddr: [u8; 16],
    pub family: u32,
    /// The local port, in network order as the kernel holds it.
    sport: u32,
    /// The process that was current when the socket reached `TCP_CLOSE`.
    ///
    /// That is the closing process when an application closed the connection,
    /// and is whatever happened to be running on that cpu when the kernel
    /// closed it from a timer or on a reset. It is not "the process that owned
    /// the connection", which this tracepoint cannot say: a connection becomes
    /// established in softirq context at both ends, outside any process.
    pub pid: u32,
    pub tid: u32,
    comm: [u8; 16],
}

/// The size of the record the program writes.
const RECORD_BYTES: usize = 104;

impl TcpLifetime {
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

    /// The name of the task that was current at the close.
    pub fn comm(&self) -> String {
        let end = self
            .comm
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(self.comm.len());
        String::from_utf8_lossy(&self.comm[..end]).into_owned()
    }

    /// Reads one record out of the ring, or refuses one that is too short.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_BYTES {
            return None;
        }
        let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        let long = |at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("8"));
        let mut saddr = [0u8; 16];
        saddr.copy_from_slice(&bytes[56..72]);
        let mut comm = [0u8; 16];
        comm.copy_from_slice(&bytes[88..104]);
        Some(Self {
            ts_ns: long(0),
            duration_ns: long(8),
            cookie: long(16),
            inode: long(24),
            bytes_sent: long(32),
            bytes_received: long(40),
            segs_out: word(48),
            segs_in: word(52),
            saddr,
            family: word(72),
            sport: word(76),
            pid: word(80),
            tid: word(84),
            comm,
        })
    }
}

/// Stack offsets, negative from the frame pointer, matching `TcpLifetime`.
const EVENT: i16 = -104;
const TS: i16 = EVENT;
const DURATION: i16 = EVENT + 8;
const COOKIE: i16 = EVENT + 16;
const INODE: i16 = EVENT + 24;
const BYTES_SENT: i16 = EVENT + 32;
const BYTES_RECEIVED: i16 = EVENT + 40;
const SEGS_OUT: i16 = EVENT + 48;
const SEGS_IN: i16 = EVENT + 52;
const SADDR: i16 = EVENT + 56;
const FAMILY: i16 = EVENT + 72;
const SPORT: i16 = EVENT + 76;
const PID: i16 = EVENT + 80;
const TID: i16 = EVENT + 84;
const COMM: i16 = EVENT + 88;
/// The open map's key and value, and the drop counter's key, below the record.
const OPEN_KEY: i16 = EVENT - 8;
/// Three words: when the connection opened, and the two names the socket had
/// at that moment. See `HELD_COOKIE` for why the names are kept.
const OPEN_VALUE: i16 = EVENT - 40;
const OPEN_TS: i16 = 0;
const OPEN_COOKIE: i16 = 8;
const OPEN_INODE: i16 = 16;
/// What the open map held, copied out before the entry is removed.
///
/// A socket loses its inode before it reaches `TCP_CLOSE`: `tcp_close` calls
/// `sock_orphan` first, which unhooks the socket from the file that gave it
/// one. So the identity read at the close is often zero and the identity read
/// at the establishment is not, and the event carries whichever exists.
const HELD_COOKIE: i16 = EVENT - 48;
const HELD_INODE: i16 = EVENT - 56;
const DROP_KEY: i16 = EVENT - 60;

const OPEN_KEY_BYTES: usize = 8;
const OPEN_VALUE_BYTES: usize = 24;

/// Builds the program, or says why this kernel cannot be read.
fn instructions(btf: &Btf, ring_fd: i32, open_fd: i32, drops_fd: i32) -> Result<Vec<Insn>, String> {
    expect_signature(btf, "inet_sock_set_state", &[&SIGNATURE])?;

    // Every offset below is composed out of named members, which is what makes
    // it the running kernel's answer rather than a number written down. The
    // three leading members are all at zero on every kernel anyone has seen,
    // and they are subtracted anyway: a `sock *` is what the tracepoint hands
    // over, and these fields are named relative to the structures that embed
    // it.
    let sock_at = btf.member_offset("inet_sock", "sk")?;
    let inet_at = sock_at + btf.member_offset("inet_connection_sock", "icsk_inet")?;
    let tcp_at = inet_at + btf.member_offset("tcp_sock", "inet_conn")?;
    let from_inet = |member: &str| {
        btf.member_offset("inet_sock", member)
            .map(|at| at - sock_at)
    };
    let from_tcp = |member: &str| btf.member_offset("tcp_sock", member).map(|at| at - tcp_at);

    let common_at = btf.member_offset("sock", "__sk_common")?;
    let cookie_at = common_at + btf.member_offset("sock_common", "skc_cookie")?;
    let family_at = common_at + btf.member_offset("sock_common", "skc_family")?;
    let v6_saddr_at = common_at + btf.member_offset("sock_common", "skc_v6_rcv_saddr")?;
    let inode_at = btf.member_offset("sock", "sk_ino")?;
    let saddr_at = from_inet("inet_saddr")?;
    let sport_at = from_inet("inet_sport")?;
    let bytes_sent_at = from_tcp("bytes_sent")?;
    let bytes_received_at = from_tcp("bytes_received")?;
    let segs_out_at = from_tcp("segs_out")?;
    let segs_in_at = from_tcp("segs_in")?;

    let mut program = Program::new();
    // r6 keeps the context, r7 the socket and r8 the moment it was established;
    // all three survive helper calls, which clobber r1 through r5.
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, ARG_SOCK));
    program.push(Insn::ldx(BPF_DW, 1, 6, ARG_NEW_STATE));
    program.jump_to("opened", Insn::jeq_imm(1, TCP_ESTABLISHED, 0));
    program.jump_to("closed", Insn::jeq_imm(1, TCP_CLOSE, 0));
    program.jump_to("done", Insn::ja());

    // ---- the connection begins ----
    program.label("opened");
    program.push(Insn::stx(BPF_DW, R10, OPEN_KEY, 7));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, OPEN_VALUE + OPEN_TS, 0));
    // The socket's two names, taken now because the close is too late for one
    // of them.
    read_field(&mut program, OPEN_VALUE + OPEN_COOKIE, 8, 7, cookie_at);
    read_field(&mut program, OPEN_VALUE + OPEN_INODE, 8, 7, inode_at);
    program.extend(Insn::load_map_fd(1, open_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, OPEN_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, OPEN_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    // The map is full: this connection will never be timed, and nothing else
    // anywhere would record that it existed.
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_OPEN, "done");
    program.jump_to("done", Insn::ja());

    // ---- the connection ends ----
    program.label("closed");
    program.push(Insn::stx(BPF_DW, R10, OPEN_KEY, 7));
    program.extend(Insn::load_map_fd(1, open_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, OPEN_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    // A socket this run never saw reach `TCP_ESTABLISHED`: it was already open
    // when the program attached, or it never connected at all -- a refused
    // connect goes `SYN_SENT` straight to `TCP_CLOSE` and had no lifetime.
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 8, 0, OPEN_TS));
    // Copied off the map value before the entry is removed, because the delete
    // below frees it.
    program.push(Insn::ldx(BPF_DW, 1, 0, OPEN_COOKIE));
    program.push(Insn::stx(BPF_DW, R10, HELD_COOKIE, 1));
    program.push(Insn::ldx(BPF_DW, 1, 0, OPEN_INODE));
    program.push(Insn::stx(BPF_DW, R10, HELD_INODE, 1));
    program.extend(Insn::load_map_fd(1, open_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, OPEN_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, TS, 0));
    program.push(Insn::mov64_reg(1, 0));
    program.push(Insn::sub64_reg(1, 8));
    program.push(Insn::stx(BPF_DW, R10, DURATION, 1));

    // The names the socket has now, falling back to the ones it had when the
    // connection opened. A cookie is generated once and never cleared, so the
    // fallback only ever fills in an inode -- which `sock_orphan` takes away
    // on the way into `tcp_close`.
    read_field(&mut program, COOKIE, 8, 7, cookie_at);
    program.push(Insn::ldx(BPF_DW, 1, R10, COOKIE));
    program.jump_to("named", Insn::jne_imm(1, 0, 0));
    program.push(Insn::ldx(BPF_DW, 1, R10, HELD_COOKIE));
    program.push(Insn::stx(BPF_DW, R10, COOKIE, 1));
    program.label("named");
    read_field(&mut program, INODE, 8, 7, inode_at);
    program.push(Insn::ldx(BPF_DW, 1, R10, INODE));
    program.jump_to("numbered", Insn::jne_imm(1, 0, 0));
    program.push(Insn::ldx(BPF_DW, 1, R10, HELD_INODE));
    program.push(Insn::stx(BPF_DW, R10, INODE, 1));
    program.label("numbered");
    read_field(&mut program, BYTES_SENT, 8, 7, bytes_sent_at);
    read_field(&mut program, BYTES_RECEIVED, 8, 7, bytes_received_at);
    read_narrow(&mut program, SEGS_OUT, 4, 7, segs_out_at);
    read_narrow(&mut program, SEGS_IN, 4, 7, segs_in_at);
    read_narrow(&mut program, FAMILY, 2, 7, family_at);
    read_narrow(&mut program, SPORT, 2, 7, sport_at);

    // Sixteen bytes, cleared before either branch writes into them: a v4
    // address fills four of them and the twelve behind it would otherwise be
    // whatever the last connection left on the stack.
    program.push(Insn::st_imm(BPF_DW, R10, SADDR, 0));
    program.push(Insn::st_imm(BPF_DW, R10, SADDR + 8, 0));
    program.push(Insn::ldx(BPF_W, 1, R10, FAMILY));
    program.jump_to("v6", Insn::jeq_imm(1, AF_INET6, 0));
    read_field(&mut program, SADDR, 4, 7, saddr_at);
    program.jump_to("addressed", Insn::ja());
    program.label("v6");
    read_field(&mut program, SADDR, 16, 7, v6_saddr_at);
    program.label("addressed");

    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, TID, 0));
    program.push(Insn::rsh64_imm(0, 32));
    program.push(Insn::stx(BPF_W, R10, PID, 0));
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, COMM as i32));
    program.push(Insn::mov64_imm(2, 16));
    program.push(Insn::call(GET_CURRENT_COMM));

    program.extend(Insn::load_map_fd(1, ring_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, EVENT as i32));
    program.push(Insn::mov64_imm(3, RECORD_BYTES as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::RINGBUF_OUTPUT));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_RING, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// A loaded, attached lifetime program and the maps it writes.
pub struct TcpLifetimeProgram {
    ring: RingBuffer,
    open: Hash,
    drops: Counters,
    _program: OwnedFd,
    _link: OwnedFd,
}

impl TcpLifetimeProgram {
    /// Loads and attaches the program, or says why the kernel refused.
    pub fn load(requested_ring_bytes: usize) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let bytes = ring_bytes(requested_ring_bytes);

        let ring_map =
            syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_tcp_life")
                .map_err(|error| format!("cannot create the connection ring buffer: {error}"))?;
        let open = Hash::create(OPEN_KEY_BYTES, OPEN_VALUE_BYTES, MAX_OPEN, "omn_tcp_open")
            .map_err(|error| format!("cannot create the open connection map: {error}"))?;
        let drops = Counters::create(COUNTER_SLOTS, "omn_tcp_lost")
            .map_err(|error| format!("cannot create the connection counters: {error}"))?;

        let instructions = instructions(
            btf,
            ring_map.as_raw_fd(),
            open.as_raw_fd(),
            drops.as_raw_fd(),
        )?;
        let program = load(&instructions, "omn_sock_state")?;
        let link = syscall::raw_tracepoint_open("inet_sock_set_state", program.as_raw_fd())
            .map_err(|error| {
                format!("cannot attach to the `inet_sock_set_state` tracepoint: {error}")
            })?;
        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the connection ring buffer: {error}"))?;

        Ok(Self {
            ring,
            open,
            drops,
            _program: program,
            _link: link,
        })
    }

    /// Hands every complete record to `handle`, counting the ones that were not
    /// the size this reader expects rather than reinterpreting them.
    pub fn drain(&mut self, mut handle: impl FnMut(TcpLifetime), undecoded: &mut u64) {
        self.ring
            .drain(|bytes| match TcpLifetime::from_bytes(bytes) {
                Some(closed) => handle(closed),
                None => *undecoded += 1,
            });
    }

    /// Connections established and not yet closed when the run ended.
    ///
    /// Their lifetime is real and has no end yet, so it is in no event. On a
    /// machine holding long-lived connections that is the normal state, and it
    /// is a fact about the window rather than a loss.
    pub fn still_open(&self) -> usize {
        self.open.len().unwrap_or(0)
    }

    /// What the program could not record, as `(connections the map had no room
    /// for, records the ring had no room for)`.
    pub fn dropped(&self) -> (u64, u64) {
        (self.drops.read(DROP_OPEN), self.drops.read(DROP_RING))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel_btf() -> Option<Btf> {
        match Btf::from_kernel() {
            Ok(btf) => Some(btf),
            Err(reason) => {
                eprintln!("skipped: {reason}");
                None
            }
        }
    }

    /// The program writes fields at stack offsets and `from_bytes` reads them
    /// at struct offsets. Nothing but this test connects the two.
    #[test]
    fn the_record_struct_matches_what_the_program_writes() {
        use std::mem::offset_of;
        assert_eq!(std::mem::size_of::<TcpLifetime>(), RECORD_BYTES);
        for (stack, field, name) in [
            (TS, offset_of!(TcpLifetime, ts_ns), "ts_ns"),
            (
                DURATION,
                offset_of!(TcpLifetime, duration_ns),
                "duration_ns",
            ),
            (COOKIE, offset_of!(TcpLifetime, cookie), "cookie"),
            (INODE, offset_of!(TcpLifetime, inode), "inode"),
            (
                BYTES_SENT,
                offset_of!(TcpLifetime, bytes_sent),
                "bytes_sent",
            ),
            (
                BYTES_RECEIVED,
                offset_of!(TcpLifetime, bytes_received),
                "bytes_received",
            ),
            (SEGS_OUT, offset_of!(TcpLifetime, segs_out), "segs_out"),
            (SEGS_IN, offset_of!(TcpLifetime, segs_in), "segs_in"),
            (SADDR, offset_of!(TcpLifetime, saddr), "saddr"),
            (FAMILY, offset_of!(TcpLifetime, family), "family"),
            (SPORT, offset_of!(TcpLifetime, sport), "sport"),
            (PID, offset_of!(TcpLifetime, pid), "pid"),
            (TID, offset_of!(TcpLifetime, tid), "tid"),
            (COMM, offset_of!(TcpLifetime, comm), "comm"),
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
        assert!(TcpLifetime::from_bytes(&[0u8; 64]).is_none());
    }

    /// A port is `__be16` in the kernel and a person reads it in host order.
    /// Reading it the other way round turns 8080 into 36895, which is a port.
    #[test]
    fn a_port_comes_back_in_the_order_a_person_reads_it() {
        let mut bytes = [0u8; RECORD_BYTES];
        bytes[76..80].copy_from_slice(&u32::from(8080u16.to_be()).to_ne_bytes());
        bytes[72..76].copy_from_slice(&2u32.to_ne_bytes());
        bytes[56..60].copy_from_slice(&[127, 0, 0, 1]);
        let closed = TcpLifetime::from_bytes(&bytes).expect("a full record");
        assert_eq!(closed.local_port(), 8080);
        assert_eq!(closed.local_address().as_deref(), Some("127.0.0.1"));
    }

    /// A v6 socket's address is sixteen bytes, and the v4 branch must not leave
    /// twelve of them holding the previous connection's.
    #[test]
    fn a_v6_address_is_rendered_as_one() {
        let mut bytes = [0u8; RECORD_BYTES];
        bytes[72..76].copy_from_slice(&10u32.to_ne_bytes());
        bytes[56 + 15] = 1;
        let closed = TcpLifetime::from_bytes(&bytes).expect("a full record");
        assert_eq!(closed.local_address().as_deref(), Some("::1"));
    }

    /// The size handed to `bpf_ringbuf_output` is the size of the record the
    /// rest of the program wrote.
    #[test]
    fn the_published_record_is_exactly_the_size_of_the_struct() {
        let Some(btf) = kernel_btf() else { return };
        let program = instructions(&btf, 3, 4, 5).expect("a program");
        let output_at = program
            .iter()
            .position(|insn| insn.code == 0x85 && insn.imm == helper::RINGBUF_OUTPUT)
            .expect("the program publishes its record");
        let length = program[..output_at]
            .iter()
            .rev()
            .find(|insn| insn.regs & 0xf == 3 && insn.code == 0xb7)
            .expect("a length is moved into r3");
        assert_eq!(length.imm as usize, std::mem::size_of::<TcpLifetime>());
    }

    /// Invariant 6: one clock domain, for both ends of the duration.
    #[test]
    fn the_program_reads_the_monotonic_clock_and_no_other() {
        let Some(btf) = kernel_btf() else { return };
        const KTIME_GET_BOOT_NS: i32 = 125;
        let called: Vec<i32> = instructions(&btf, 3, 4, 5)
            .expect("a program")
            .iter()
            .filter(|insn| insn.code == 0x85)
            .map(|insn| insn.imm)
            .collect();
        assert_eq!(
            called
                .iter()
                .filter(|id| **id == helper::KTIME_GET_NS)
                .count(),
            2,
            "one read when the connection opens and one when it closes"
        );
        assert!(!called.contains(&KTIME_GET_BOOT_NS));
    }

    /// Two states and no others. A program that acted on `FIN_WAIT1` would
    /// close a connection that is still moving data, and one that acted on
    /// `SYN_SENT` would call a refused connect a connection.
    #[test]
    fn only_the_two_states_that_bound_a_connection_are_acted_on() {
        let Some(btf) = kernel_btf() else { return };
        let program = instructions(&btf, 3, 4, 5).expect("a program");
        let compared: Vec<i32> = program
            .iter()
            .filter(|insn| insn.code == 0x15 && insn.regs & 0xf == 1)
            .map(|insn| insn.imm)
            .collect();
        assert!(compared.contains(&TCP_ESTABLISHED), "{compared:?}");
        assert!(compared.contains(&TCP_CLOSE), "{compared:?}");
        assert!(compared.contains(&AF_INET6), "{compared:?}");
        assert_eq!(
            compared.len(),
            3,
            "three comparisons against a constant and no more: {compared:?}"
        );
    }
}
