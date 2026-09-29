//! `net.drops`: every packet the kernel threw away, and why.
//!
//! `kfree_skb` is the kernel's one free-with-a-reason: `consume_skb` is what it
//! calls when a buffer has served its purpose, and `kfree_skb` is what it calls
//! when a packet is being dropped. Since 5.17 the tracepoint carries a reason
//! -- a value of `enum skb_drop_reason` -- and that is the whole point of this
//! source. A drop count alone says a link is unhealthy; a drop count with
//! `NO_SOCKET` beside it says nothing was listening, which is a different
//! problem with a different fix.
//!
//! **This is the source that makes the family's headline honest.** `net.iface`
//! counts the frames a device lost and cannot see a frame dropped inside the
//! stack; `net.qdisc` sees the queueing discipline; this sees everywhere else.
//!
//! It streams rather than counting in the kernel, and that is a choice the
//! emits force: the entry declares `raw` and `topk`, which want each drop
//! rather than a bucket of them, and a drop carries a reason and a socket that
//! an aggregate would have to throw away. A machine dropping packets faster
//! than the ring is drained says so through the drop counter, which is the
//! honest failure rather than the quiet one.
//!
//! **What the record cannot carry, and why.** A dropped packet's protocol,
//! interface and addresses live in `sk_buff` fields that sit inside anonymous
//! unions -- `skb->protocol`, `skb->skb_iif`, `skb->dev` -- and
//! `Btf::member_offset` finds members by name in a named structure, so those
//! offsets cannot be looked up at all. Reading them would mean writing an
//! offset into this file, which is exactly the staleness the whole layer exists
//! to prevent. So the protocol here is the *socket's* protocol when the drop
//! had a socket, and is reported as unknown when it did not -- which is most
//! receive-side drops, including the `NO_SOCKET` ones. The request for the walk
//! through anonymous members is under `## Needs from another stream` in
//! `docs/open-findings.md`.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters};
use super::net_program::{
    count_drop, expect_signature, load, read_field, read_narrow, SocketInode,
};
use super::ring::{ring_bytes, RingBuffer};
use super::syscall::{self, BPF_MAP_TYPE_RINGBUF};

/// The prototypes this program is written against.
///
/// The fourth argument, the socket a receive-side drop belonged to, arrived in
/// 6.9. A kernel without it is served without it rather than refused: the
/// program falls back to `skb->sk`, which is set on the transmit side.
const WITH_RX_SOCKET: [&str; 4] = ["sk_buff *", "void *", "skb_drop_reason", "sock *"];
const WITHOUT_RX_SOCKET: [&str; 3] = ["sk_buff *", "void *", "skb_drop_reason"];

/// Arguments of `kfree_skb`, as byte offsets into the raw tracepoint's context.
const ARG_SKB: i16 = 0;
const ARG_LOCATION: i16 = 8;
const ARG_REASON: i16 = 16;
const ARG_RX_SOCKET: i16 = 24;

/// One dropped packet, as the program writes it and userspace reads it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketDrop {
    /// `bpf_ktime_get_ns`, which is `CLOCK_MONOTONIC`.
    pub ts_ns: u64,
    /// The kernel's own `enum skb_drop_reason` value.
    ///
    /// A number rather than a name, because the names are in the kernel's BTF
    /// and this layer cannot yet read an enum out of it. A table of names
    /// written here would be a constant that no run could check against the
    /// kernel it is running on, which is the mistake the field offsets in
    /// `btf.rs` exist not to make.
    pub reason: u32,
    /// The socket's protocol, or [`PacketDrop::NO_PROTOCOL`] when the drop had
    /// no socket to read one from. [`PacketDrop::protocol`] is the reading of
    /// it that cannot be mistaken for `IPPROTO_IP`.
    pub protocol: u32,
    /// The packet's length, as the kernel counted it.
    pub len: u32,
    /// The socket's address family, or zero when there was no socket.
    pub family: u32,
    /// The socket cookie, or zero.
    ///
    /// Zero is not a cookie: the kernel generates one lazily, the first time
    /// something asks, so a socket nothing has ever dumped has none. An event
    /// whose cookie is zero carries no socket key rather than a key of zero,
    /// because a key of zero would join every such event to every other.
    pub cookie: u64,
    /// The socket's inode, which is what `/proc/net/tcp` and `/proc/<pid>/fd`
    /// call it and is set from the moment the socket exists.
    pub inode: u64,
    /// The kernel address that called `kfree_skb`. Carried because it is the
    /// one thing that says *where* in the stack the packet died; nothing here
    /// resolves it to a symbol.
    pub location: u64,
}

/// The size of the record the program writes.
const RECORD_BYTES: usize = 48;

impl PacketDrop {
    /// What a protocol field holds when the drop had no socket.
    pub const NO_PROTOCOL: u32 = u32::MAX;

    /// The socket's protocol, when the drop had a socket.
    ///
    /// Not the raw field: zero is `IPPROTO_IP` and is a protocol, so a drop
    /// with no socket cannot be written as zero without reading as one.
    pub fn protocol(&self) -> Option<u32> {
        (self.protocol != Self::NO_PROTOCOL).then_some(self.protocol)
    }

    /// Reads one record out of the ring, or refuses one that is too short.
    ///
    /// Public because the collector's tests build a record to render, and a
    /// record built by naming fields is a record that never went through the
    /// layout this decodes.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_BYTES {
            return None;
        }
        let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        let long = |at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("8"));
        Some(Self {
            ts_ns: long(0),
            reason: word(8),
            protocol: word(12),
            len: word(16),
            family: word(20),
            cookie: long(24),
            inode: long(32),
            location: long(40),
        })
    }
}

/// Stack offsets, negative from the frame pointer, matching `PacketDrop`.
const EVENT: i16 = -48;
const TS: i16 = EVENT;
const REASON: i16 = EVENT + 8;
const PROTOCOL: i16 = EVENT + 12;
const LEN: i16 = EVENT + 16;
const FAMILY: i16 = EVENT + 20;
const COOKIE: i16 = EVENT + 24;
const INODE: i16 = EVENT + 32;
const LOCATION: i16 = EVENT + 40;
/// The one-word key of the drop counter, and eight bytes a pointer read lands
/// in, both below the record.
const DROP_KEY: i16 = EVENT - 8;
const SCRATCH: i16 = EVENT - 16;

/// Builds the program, or says why this kernel cannot be read.
fn instructions(
    btf: &Btf,
    inode: SocketInode,
    ring_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let signature = expect_signature(btf, "kfree_skb", &[&WITH_RX_SOCKET, &WITHOUT_RX_SOCKET])?;
    let has_rx_socket = signature.len() == WITH_RX_SOCKET.len();

    // Every offset is composed out of named members, so nothing here is a
    // number written down: `sock` starts with `__sk_common`, and a `sock *` is
    // what the tracepoint hands over.
    let len_at = btf.member_offset("sk_buff", "len")?;
    let skb_sk_at = btf.member_offset("sk_buff", "sk")?;
    let common_at = btf.member_offset("sock", "__sk_common")?;
    let family_at = common_at + btf.member_offset("sock_common", "skc_family")?;
    let cookie_at = common_at + btf.member_offset("sock_common", "skc_cookie")?;
    let protocol_at = btf.member_offset("sock", "sk_protocol")?;

    let mut program = Program::new();
    // r6 keeps the context, r7 the buffer and r8 the socket; all three survive
    // helper calls, which clobber r1 through r5.
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, ARG_SKB));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, TS, 0));

    program.push(Insn::ldx(BPF_DW, 1, 6, ARG_REASON));
    program.push(Insn::stx(BPF_W, R10, REASON, 1));
    program.push(Insn::ldx(BPF_DW, 1, 6, ARG_LOCATION));
    program.push(Insn::stx(BPF_DW, R10, LOCATION, 1));

    read_narrow(&mut program, LEN, 4, 7, len_at);

    // Written before anything conditional writes them, because every byte of
    // the record has to be set before it is published: a padding word or a
    // field on a path not taken is whatever the last event left on the stack.
    program.push(Insn::st_imm(
        BPF_W,
        R10,
        PROTOCOL,
        PacketDrop::NO_PROTOCOL as i32,
    ));
    program.push(Insn::st_imm(BPF_W, R10, FAMILY, 0));
    program.push(Insn::st_imm(BPF_DW, R10, COOKIE, 0));
    program.push(Insn::st_imm(BPF_DW, R10, INODE, 0));

    if has_rx_socket {
        // The receive side has no `skb->sk`: the lookup that would have set one
        // is what did not happen. This argument is the socket the kernel found,
        // when it found one, and is the only way a receive-side drop is
        // attributable at all.
        program.push(Insn::ldx(BPF_DW, 8, 6, ARG_RX_SOCKET));
        program.jump_to("have_socket", Insn::jne_imm(8, 0, 0));
    }
    super::net_program::read_pointer(&mut program, SCRATCH, 8, 7, skb_sk_at);
    program.label("have_socket");
    program.jump_to("emit", Insn::jeq_imm(8, 0, 0));

    read_narrow(&mut program, PROTOCOL, 2, 8, protocol_at);
    read_narrow(&mut program, FAMILY, 2, 8, family_at);
    read_field(&mut program, COOKIE, 8, 8, cookie_at);
    inode.read(&mut program, INODE, 8, SCRATCH, "inode_read");

    program.label("emit");
    program.extend(Insn::load_map_fd(1, ring_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, EVENT as i32));
    program.push(Insn::mov64_imm(3, RECORD_BYTES as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::RINGBUF_OUTPUT));

    // A full ring returns an error and the drop is gone. Nothing else would
    // ever record that: a measurement of what the machine threw away, itself
    // quietly short, is the worst shape this source could take.
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, 0, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// A loaded, attached drop program and the two maps it writes.
pub struct DropProgram {
    ring: RingBuffer,
    drops: Counters,
    /// Whether this kernel's tracepoint carries the receive-side socket.
    has_rx_socket: bool,
    program: OwnedFd,
    _link: OwnedFd,
}

impl DropProgram {
    /// Loads and attaches the program, or says why the kernel refused.
    pub fn load(requested_ring_bytes: usize) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let bytes = ring_bytes(requested_ring_bytes);
        let has_rx_socket = btf.tracepoint_signature("kfree_skb")?.len() == WITH_RX_SOCKET.len();

        let ring_map = syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_drops")
            .map_err(|error| map_failure("drop ring buffer", error))?;
        let drops = Counters::create(1, "omn_drop_lost")
            .map_err(|error| map_failure("drop counter", error))?;

        let instructions = instructions(
            btf,
            SocketInode::locate(btf)?,
            ring_map.as_raw_fd(),
            drops.as_raw_fd(),
        )?;
        let program = load(&instructions, "omn_kfree_skb")?;
        let link = syscall::raw_tracepoint_open("kfree_skb", program.as_raw_fd())
            .map_err(|error| format!("cannot attach to the `kfree_skb` tracepoint: {error}"))?;
        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the drop ring buffer: {error}"))?;

        Ok(Self {
            ring,
            drops,
            has_rx_socket,
            program,
            _link: link,
        })
    }

    /// Whether this kernel's tracepoint says which socket a received packet
    /// would have gone to. Without it a receive-side drop carries no socket at
    /// all, which changes what the source can be joined on.
    pub fn has_receive_socket(&self) -> bool {
        self.has_rx_socket
    }

    /// Hands every complete drop to `handle`, counting the records that were
    /// not the size this reader expects rather than reinterpreting them.
    pub fn drain(&mut self, mut handle: impl FnMut(PacketDrop), undecoded: &mut u64) {
        self.ring
            .drain(|bytes| match PacketDrop::from_bytes(bytes) {
                Some(dropped) => handle(dropped),
                None => *undecoded += 1,
            });
    }

    /// Runs of these programs the kernel skipped because one was already
    /// running on that cpu; see `BlockProgram::skipped`, where it was measured.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped([&self.program])
    }

    /// Drops the program could not publish because the ring was full.
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

    /// The program writes fields at stack offsets and `from_bytes` reads them
    /// at struct offsets. Nothing but this test connects the two, and a
    /// mismatch would be a field quietly read from the wrong place.
    #[test]
    fn the_record_struct_matches_what_the_program_writes() {
        use std::mem::offset_of;
        assert_eq!(std::mem::size_of::<PacketDrop>(), RECORD_BYTES);
        for (stack, field, name) in [
            (TS, offset_of!(PacketDrop, ts_ns), "ts_ns"),
            (REASON, offset_of!(PacketDrop, reason), "reason"),
            (PROTOCOL, offset_of!(PacketDrop, protocol), "protocol"),
            (LEN, offset_of!(PacketDrop, len), "len"),
            (FAMILY, offset_of!(PacketDrop, family), "family"),
            (COOKIE, offset_of!(PacketDrop, cookie), "cookie"),
            (INODE, offset_of!(PacketDrop, inode), "inode"),
            (LOCATION, offset_of!(PacketDrop, location), "location"),
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
        assert!(PacketDrop::from_bytes(&[0u8; 24]).is_none());
    }

    /// A drop with no socket has no protocol, and the absence is an absence
    /// rather than protocol zero -- which is `IPPROTO_IP` and would read as a
    /// measurement.
    #[test]
    fn a_drop_without_a_socket_reports_no_protocol_rather_than_zero() {
        let mut bytes = [0u8; RECORD_BYTES];
        bytes[12..16].copy_from_slice(&PacketDrop::NO_PROTOCOL.to_ne_bytes());
        let dropped = PacketDrop::from_bytes(&bytes).expect("a full record");
        assert_eq!(dropped.protocol(), None);

        bytes[12..16].copy_from_slice(&0u32.to_ne_bytes());
        let dropped = PacketDrop::from_bytes(&bytes).expect("a full record");
        assert_eq!(dropped.protocol(), Some(0), "IPPROTO_IP is a protocol");
    }

    /// Every byte of the record is written before it is published, including
    /// the fields on the path where there is no socket. An uninitialised one is
    /// whatever the last drop left on the stack, which reads as a plausible
    /// cookie belonging to a different connection.
    #[test]
    fn every_field_is_written_before_the_record_is_published() {
        let Some(btf) = kernel_btf() else { return };
        let program =
            instructions(btf, SocketInode::locate(btf).unwrap(), 3, 4).expect("a program");
        let published = program
            .iter()
            .position(|insn| insn.code == 0x85 && insn.imm == helper::RINGBUF_OUTPUT)
            .expect("the program publishes its record");
        // The unconditional writes are the ones before any jump into `emit`,
        // which is the path a drop with no socket takes.
        for (offset, name) in [
            (TS, "ts_ns"),
            (REASON, "reason"),
            (PROTOCOL, "protocol"),
            (LEN, "len"),
            (FAMILY, "family"),
            (COOKIE, "cookie"),
            (INODE, "inode"),
            (LOCATION, "location"),
        ] {
            assert!(
                program[..published].iter().any(|insn| {
                    // A store, of either the immediate or the register form,
                    // whose base is the frame pointer and whose offset is this
                    // field's. Both of the reading paths below start with one.
                    let class = insn.code & 0x07;
                    (class == 0x02 || class == 0x03) && insn.regs & 0xf == 10 && insn.off == offset
                }),
                "`{name}` is never written before the record is published"
            );
        }
    }

    /// The size handed to `bpf_ringbuf_output` is the size of the record the
    /// rest of the program wrote. A smaller one publishes a truncated record
    /// and a larger one publishes whatever is on the stack behind it.
    #[test]
    fn the_published_record_is_exactly_the_size_of_the_struct() {
        let Some(btf) = kernel_btf() else { return };
        let program =
            instructions(btf, SocketInode::locate(btf).unwrap(), 3, 4).expect("a program");
        let output_at = program
            .iter()
            .position(|insn| insn.code == 0x85 && insn.imm == helper::RINGBUF_OUTPUT)
            .expect("the program publishes its record");
        let length = program[..output_at]
            .iter()
            .rev()
            .find(|insn| insn.regs & 0xf == 3 && insn.code == 0xb7)
            .expect("a length is moved into r3");
        assert_eq!(length.imm as usize, std::mem::size_of::<PacketDrop>());
    }

    /// Invariant 6: one clock domain. A program reading boot time would put
    /// these events in a different domain from every other collector and every
    /// join across them would be silently wrong.
    #[test]
    fn the_program_reads_the_monotonic_clock_and_no_other() {
        let Some(btf) = kernel_btf() else { return };
        const KTIME_GET_BOOT_NS: i32 = 125;
        let called: Vec<i32> = instructions(btf, SocketInode::locate(btf).unwrap(), 3, 4)
            .expect("a program")
            .iter()
            .filter(|insn| insn.code == 0x85)
            .map(|insn| insn.imm)
            .collect();
        assert!(called.contains(&helper::KTIME_GET_NS));
        assert!(!called.contains(&KTIME_GET_BOOT_NS));
    }

    /// This kernel's tracepoint is one of the two shapes the program knows,
    /// and which one decides whether a receive-side drop has a socket at all.
    #[test]
    fn this_kernels_kfree_skb_is_a_shape_the_program_knows() {
        let Some(btf) = kernel_btf() else { return };
        let signature = expect_signature(btf, "kfree_skb", &[&WITH_RX_SOCKET, &WITHOUT_RX_SOCKET])
            .expect("a kernel this program can read");
        assert!(
            signature.len() == 3 || signature.len() == 4,
            "{signature:?}"
        );
        assert_eq!(signature[0], "sk_buff *");
        assert_eq!(signature[2], "skb_drop_reason", "the reason is the source");
    }

    /// 6.6 and older have no `sock.sk_ino`, and the program reads the inode
    /// through `sk_socket` there. That shape is built and audited here, on a
    /// kernel that would never choose it, because the kernels that do are
    /// only reached by the portability matrix.
    #[test]
    fn the_program_builds_with_the_walk_to_the_inode_older_kernels_need() {
        let Some(btf) = kernel_btf() else { return };
        let walk = SocketInode::through_socket(btf).expect("every kernel has the walk");
        let SocketInode::ThroughSocket { ino_at, .. } = walk else {
            unreachable!()
        };
        let program = instructions(btf, walk, 3, 4).expect("a program");
        assert!(
            program
                .iter()
                .any(|insn| insn.code == 0x07 && insn.regs & 0xf == 3 && insn.imm == ino_at),
            "the inode number is read at its offset from the `struct socket`"
        );
        assert!(
            !program.iter().any(
                |insn| btf
                    .member_offset("sock", "sk_ino")
                    .is_ok_and(|at| insn.code == 0x07 && insn.imm == at && insn.regs & 0xf == 3)
            ),
            "and `sk_ino` is not read at all"
        );
    }
}
