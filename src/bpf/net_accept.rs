// SPDX-License-Identifier: MIT OR GPL-2.0
//! `net.tcp.accept`: every connection a server took off its accept queue.
//!
//! One `fexit` program on `inet_csk_accept`, which every TCP `accept` passes
//! through in the accepting task's context, with the listening socket as its
//! first argument and the accepted one as its return value (NULL when the
//! accept failed or would block). What `tcpaccept` prints: who accepted, both
//! ends, and the listener's queue at that moment.
//!
//! **The queue is the listener's, read after the accept took one off it.**
//! `sk_ack_backlog` is how many established connections are still waiting,
//! `sk_max_ack_backlog` the bound `listen` gave it; a server whose accepts
//! keep finding the queue near its bound is the one `net.backlog`'s
//! overflows are about.
//!
//! Declared beside `net_connect.rs` and loaded from it (`net_connect::accept`),
//! since a program file of this family goes there.

use std::os::fd::AsRawFd;

use crate::bpf::attach::fentry::{self, Function, Link, Narrowing, Side, BPF_H, GET_SOCKET_COOKIE};
use crate::bpf::btf::Btf;
use crate::bpf::insn::{self, count_drop, helper, read_field, Insn, Program, BPF_DW, BPF_W, R10};
use crate::bpf::map::{map_failure, Counters};
use crate::bpf::ring::{ring_bytes, RingBuffer};
use crate::bpf::syscall::{self, BPF_MAP_TYPE_RINGBUF};

/// `inet_csk_accept`'s prototypes: a `proto_accept_arg *` since 6.10, the
/// flags, an error pointer and `kern` before. The program reads only the
/// first argument and the return value.
pub(crate) const ACCEPT_SIGNATURES: [&[&str]; 2] = [
    &["sock *", "proto_accept_arg *"],
    &["sock *", "int", "int *", "bool"],
];

const DROP_RING: u32 = 0;

/// 256 KiB of 96-byte records drained every ten milliseconds.
pub const RING_BYTES: usize = 256 * 1024;

/// One accepted connection, as the program writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accept {
    pub ts_ns: u64,
    /// The accepted socket's cookie, the one `sock_diag` reports.
    pub cookie: u64,
    saddr: [u8; 16],
    daddr: [u8; 16],
    pub family: u32,
    /// The local port, in host order: `skc_num` is kept that way.
    pub sport: u16,
    /// The peer's port, in host order.
    pub dport: u16,
    pub tid: u32,
    pub tgid: u32,
    /// The listener's queue after this accept, and its bound.
    pub backlog: u32,
    pub max_backlog: u32,
    pub comm: String,
}

const RECORD_BYTES: usize = 96;
const EVENT: i16 = -96;
const R_TS: i16 = 0;
const R_COOKIE: i16 = 8;
const R_SADDR: i16 = 16;
const R_DADDR: i16 = 32;
const R_FAMILY: i16 = 48;
const R_SPORT: i16 = 52;
const R_DPORT: i16 = 56;
const R_TID: i16 = 60;
const R_TGID: i16 = 64;
const R_BACKLOG: i16 = 68;
const R_MAX_BACKLOG: i16 = 72;
/// Four bytes of padding at 76, cleared with the rest and never read.
const R_COMM: i16 = 80;
const DROP_KEY: i16 = -100;

/// `AF_INET6`, from the frozen `socket(2)` numbering.
const AF_INET6: i32 = 10;

impl Accept {
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
            cookie: long(R_COOKIE),
            saddr,
            daddr,
            family: word(R_FAMILY),
            sport: word(R_SPORT) as u16,
            dport: u16::from_be(word(R_DPORT) as u16),
            tid: word(R_TID),
            tgid: word(R_TGID),
            backlog: word(R_BACKLOG),
            max_backlog: word(R_MAX_BACKLOG),
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
    dport: i32,
    daddr: i32,
    rcv_saddr: i32,
    v6_daddr: i32,
    v6_rcv_saddr: i32,
    ack_backlog: i32,
    max_ack_backlog: i32,
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
            dport: at("skc_dport")?,
            daddr: at("skc_daddr")?,
            rcv_saddr: at("skc_rcv_saddr")?,
            v6_daddr: at("skc_v6_daddr")?,
            v6_rcv_saddr: at("skc_v6_rcv_saddr")?,
            ack_backlog: btf.member_offset("sock", "sk_ack_backlog")?,
            max_ack_backlog: btf.member_offset("sock", "sk_max_ack_backlog")?,
        })
    }
}

/// The exit half of `inet_csk_accept`: one record per socket it returned.
fn instructions(
    function: &Function,
    narrowing: Narrowing,
    at: &Offsets,
    ring_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    // NULL: the accept failed, or would have blocked on a non-blocking
    // listener. Nothing was accepted.
    program.push(Insn::ldx(BPF_DW, 7, 6, function.ret()));
    program.jump_to("done", Insn::jeq_imm(7, 0, 0));
    program.push(Insn::ldx(BPF_DW, 8, 6, function.arg(0)));

    for slot in 0..(RECORD_BYTES as i16 / 8) {
        program.push(Insn::st_imm(BPF_DW, R10, EVENT + slot * 8, 0));
    }
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    if let Some(tgid) = narrowing.tgid {
        program.push(Insn::mov64_reg(1, 0));
        program.push(Insn::rsh64_imm(1, 32));
        program.jump_to("done", Insn::jne_imm(1, tgid as i32, 0));
    }
    program.push(Insn::stx(BPF_W, R10, EVENT + R_TID, 0));
    program.push(Insn::rsh64_imm(0, 32));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_TGID, 0));
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, (EVENT + R_COMM) as i32));
    program.push(Insn::mov64_imm(2, 16));
    program.push(Insn::call(helper::GET_CURRENT_COMM));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, EVENT + R_TS, 0));

    // The listener's queue, after this accept took one off it.
    program.push(Insn::ldx(BPF_W, 1, 8, at.ack_backlog as i16));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_BACKLOG, 1));
    program.push(Insn::ldx(BPF_W, 1, 8, at.max_ack_backlog as i16));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_MAX_BACKLOG, 1));

    // The accepted socket's names: the cookie asked for, which generates it.
    program.push(Insn::mov64_reg(1, 7));
    program.push(Insn::call(GET_SOCKET_COOKIE));
    program.push(Insn::stx(BPF_DW, R10, EVENT + R_COOKIE, 0));
    program.push(Insn::ldx(BPF_H, 1, 7, at.family as i16));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_FAMILY, 1));
    program.push(Insn::ldx(BPF_H, 2, 7, at.num as i16));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_SPORT, 2));
    program.push(Insn::ldx(BPF_H, 2, 7, at.dport as i16));
    program.push(Insn::stx(BPF_W, R10, EVENT + R_DPORT, 2));
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

/// The program, attached, and its ring.
pub struct AcceptProgram {
    ring: RingBuffer,
    drops: Counters,
    links: Vec<Link>,
}

impl AcceptProgram {
    pub fn load(narrowing: Narrowing) -> Result<Self, String> {
        let btf = crate::bpf::kernel_btf()?;
        let offsets = Offsets::read(btf)?;
        let accept = fentry::expect("inet_csk_accept", &ACCEPT_SIGNATURES)?;
        let bytes = ring_bytes(RING_BYTES);
        let ring_map =
            syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_acc_ring")
                .map_err(|error| map_failure("accept ring buffer", error))?;
        let drops = Counters::create(1, "omn_acc_drop")
            .map_err(|error| map_failure("accept drop counter", error))?;
        let program = instructions(
            &accept,
            narrowing,
            &offsets,
            ring_map.as_raw_fd(),
            drops.as_raw_fd(),
        )?;
        let links = vec![fentry::attach(&accept, Side::Exit, &program, "omn_accept")?];
        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the accept ring buffer: {error}"))?;
        Ok(Self { ring, drops, links })
    }

    pub fn drain(&mut self, mut handle: impl FnMut(Accept), undecoded: &mut u64) {
        self.ring.drain(|bytes| match Accept::from_bytes(bytes) {
            Some(record) => handle(record),
            None => *undecoded += 1,
        });
    }

    /// Accepts recorded and never seen: the ring was full.
    pub fn dropped(&self) -> u64 {
        self.drops.read(DROP_RING)
    }

    /// Runs the kernel skipped; see `fentry::Link::misses`.
    pub fn missed(&self) -> u64 {
        fentry::misses(&self.links)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_layout_covers_every_byte_and_the_stack_does_not_overlap() {
        let fields = [
            (R_TS, 8),
            (R_COOKIE, 8),
            (R_SADDR, 16),
            (R_DADDR, 16),
            (R_FAMILY, 4),
            (R_SPORT, 4),
            (R_DPORT, 4),
            (R_TID, 4),
            (R_TGID, 4),
            (R_BACKLOG, 4),
            (R_MAX_BACKLOG, 4),
            (76, 4),
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
        assert!(DROP_KEY + 4 <= EVENT, "the drop key sits below the record");
    }

    #[test]
    fn a_record_decodes_its_ports_in_the_order_a_person_reads_them() {
        let mut bytes = [0u8; RECORD_BYTES];
        bytes[R_FAMILY as usize..][..4].copy_from_slice(&2u32.to_ne_bytes());
        bytes[R_SPORT as usize..][..4].copy_from_slice(&8080u32.to_ne_bytes());
        bytes[R_DPORT as usize..][..4].copy_from_slice(&u32::from(50000u16.to_be()).to_ne_bytes());
        bytes[R_SADDR as usize..][..4].copy_from_slice(&[127, 0, 0, 1]);
        bytes[R_BACKLOG as usize..][..4].copy_from_slice(&3u32.to_ne_bytes());
        bytes[R_COMM as usize..][..4].copy_from_slice(b"srv\0");
        let record = Accept::from_bytes(&bytes).expect("a full record");
        assert_eq!((record.sport, record.dport), (8080, 50000));
        assert_eq!(record.local_address().as_deref(), Some("127.0.0.1"));
        assert_eq!(record.backlog, 3);
        assert_eq!(record.comm, "srv");
        assert!(Accept::from_bytes(&bytes[..64]).is_none());
    }

    #[test]
    fn the_program_builds_against_the_running_kernel() {
        let Ok(btf) = crate::bpf::kernel_btf() else {
            eprintln!("skipped: no kernel BTF");
            return;
        };
        let Ok(accept) = fentry::expect("inet_csk_accept", &ACCEPT_SIGNATURES) else {
            eprintln!("skipped: inet_csk_accept has neither known prototype here");
            return;
        };
        let offsets = Offsets::read(btf).expect("the members are named");
        instructions(&accept, Narrowing::default(), &offsets, 3, 4).expect("a program");
    }
}
