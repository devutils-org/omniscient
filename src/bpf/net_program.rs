// SPDX-License-Identifier: MIT OR GPL-2.0
//! What the `net` family's eBPF programs have in common.
//!
//! Five sources, six programs, and the same half-dozen moves in every one of
//! them: read a field out of a kernel structure the verifier knows nothing
//! about, follow a pointer, count what a full map could not record, and turn a
//! duration into the log2 bucket the emit layer buckets by. They are gathered
//! here rather than repeated per source, because a program written as
//! instructions has no compiler to notice that two copies of the same sequence
//! have drifted apart.
//!
//! Three of these are near-duplicates of private helpers in
//! `src/bpf/runqlat.rs`, and that is a gap in the shared layer rather than a
//! choice: `read_field`, `count_drop` and the unrolled `log2` are general and
//! belong beside `insn.rs`. The request is under `## Needs from another stream`
//! in `docs/open-findings.md`; until it lands this family keeps its own copy,
//! and `the_kernels_buckets_and_the_emit_layers_are_the_same_buckets` below
//! crosses this copy of the bucketing against the definition independently, so
//! the two cannot drift silently.

use std::os::fd::OwnedFd;

use super::btf::Btf;
use super::insn::{self, Insn, Log2Labels, Program, BPF_B, R10};

// Written once in `insn.rs` since the seventh wave, and re-exported here so a
// `net` program still reads as one import.
pub(super) use super::insn::helper::GET_CURRENT_COMM;
pub(super) use super::insn::{bucket_low_ns, count_drop, read_field, read_narrow, read_pointer};
use super::syscall;

/// The license these programs declare.
///
/// `bpf_probe_read_kernel` is GPL-only and every field any of them reads comes
/// out of a `sock`, an `sk_buff`, a `Qdisc` or a `net_device` through it. The
/// declaration covers the instruction streams in the `net_*.rs` files, which is
/// what the kernel is asking about; `omn` itself stays under its own license.
pub(super) use crate::bpf::program::LICENSE;

/// This family's own labels for the shared unrolled log2.
///
/// The sequence itself is `insn::push_log2_bucket`, which is where the one
/// definition of `emit::BucketLayout::Log2`'s numbering lives. What stays here
/// is the label set, because a label is defined once per instruction stream.
const LOG2: Log2Labels = [
    "net_log2_32",
    "net_log2_16",
    "net_log2_8",
    "net_log2_4",
    "net_log2_2",
    "net_log2_1",
    "net_log2_done",
];

/// Computes the log2 bucket of the value in `value_reg` into `r3`.
///
/// A program may only call this once: the labels are fixed, and a second call
/// in the same program would place each of them twice.
pub(super) fn push_log2_bucket(program: &mut Program, value_reg: u8) {
    insn::push_log2_bucket(program, value_reg, &LOG2);
}

/// Refuses a kernel whose tracepoint is not the shape a program was written
/// for.
///
/// A raw tracepoint hands its program an array of `u64` and nothing in it says
/// what any of them is: a program reads slot one as a `sock *` because the
/// prototype says so. Prototypes change -- `kfree_skb` gained a fourth
/// argument in 6.9 and `tcp_retransmit_skb` a third -- and a program written
/// for the other shape reads a pointer out of a slot holding an integer and
/// gets a plausible number rather than an error. So the shape is checked
/// against the running kernel's own BTF before an instruction is built.
pub(super) fn expect_signature(
    btf: &Btf,
    tracepoint: &str,
    accepted: &[&[&str]],
) -> Result<Vec<String>, String> {
    let signature = btf.tracepoint_signature(tracepoint)?;
    if accepted.iter().any(|shape| signature == *shape) {
        return Ok(signature);
    }
    Err(format!(
        "this kernel declares `{tracepoint}({})`, and this program is written for {}: it reads \
         its arguments out of fixed slots, and against another prototype those slots hold \
         something else",
        signature.join(", "),
        accepted
            .iter()
            .map(|shape| format!("`{tracepoint}({})`", shape.join(", ")))
            .collect::<Vec<_>>()
            .join(" or "),
    ))
}

/// Where a socket's inode number is read from, on this kernel.
///
/// The inode is the name `/proc/net/tcp` and `/proc/<pid>/fd` give a socket
/// (`socket:[n]`), and it is what lets an event be joined to the process that
/// holds the descriptor. Newer kernels keep it on the socket itself, as
/// `sock.sk_ino`; 6.6 and older do not have that member, and `sock_i_ino` reads
/// it where the kernel keeps it there: in the inode allocated beside the
/// `struct socket` the `sock` points back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SocketInode {
    /// `sock.sk_ino`, at this offset.
    Member(i32),
    /// `sock.sk_socket`, then the `vfs_inode` of the `socket_alloc` that
    /// embeds that `socket`, then its `i_ino` -- what `SOCK_INODE` computes.
    /// Zero once the socket is orphaned, which `sk_ino` is too: `sock_orphan`
    /// clears both on the way into `tcp_close`.
    ThroughSocket {
        /// `sock.sk_socket`.
        socket_at: i32,
        /// From the `struct socket` to the inode number beside it.
        ino_at: i32,
    },
}

impl SocketInode {
    /// The member where this kernel has it, the walk where it does not.
    pub(super) fn locate(btf: &Btf) -> Result<Self, String> {
        match btf.member_offset("sock", "sk_ino") {
            Ok(at) => Ok(Self::Member(at)),
            Err(_) => Self::through_socket(btf),
        }
    }

    /// The walk through `sk_socket`, which every kernel this tool serves has.
    ///
    /// Every offset is a named member, as everywhere in this layer: the
    /// `socket` inside `socket_alloc` is at zero on every kernel anyone has
    /// seen, and it is subtracted anyway.
    pub(super) fn through_socket(btf: &Btf) -> Result<Self, String> {
        let socket_at = btf.member_offset("sock", "sk_socket")?;
        let ino_at = btf.member_offset("socket_alloc", "vfs_inode")?
            - btf.member_offset("socket_alloc", "socket")?
            + btf.member_offset("inode", "i_ino")?;
        Ok(Self::ThroughSocket { socket_at, ino_at })
    }

    /// Reads the inode of the `sock *` in `sock` into eight bytes of stack at
    /// `into`, or zero when the socket has no `struct socket` any more.
    ///
    /// Clobbers r1 to r3 and the eight bytes at `scratch`. `skip` is a label
    /// the walk places, so it must be unique within the program.
    pub(super) fn read(
        self,
        program: &mut Program,
        into: i16,
        sock: u8,
        scratch: i16,
        skip: &'static str,
    ) {
        match self {
            Self::Member(at) => read_field(program, into, 8, sock, at),
            Self::ThroughSocket { socket_at, ino_at } => {
                program.push(Insn::st_imm(insn::BPF_DW, insn::R10, into, 0));
                read_pointer(program, scratch, 3, sock, socket_at);
                program.jump_to(skip, Insn::jeq_imm(3, 0, 0));
                read_field(program, into, 8, 3, ino_at);
                program.label(skip);
            }
        }
    }
}

/// Loads one program, turning a refusal on privilege grounds into the advice
/// that names the two capabilities and how to grant them.
pub(super) fn load(instructions: &[Insn], name: &str) -> Result<OwnedFd, String> {
    syscall::prog_load(
        syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
        instructions,
        name,
        LICENSE,
    )
    .map_err(|error| {
        if error.is_permission() {
            super::permission_advice("loading a raw tracepoint program")
        } else {
            format!("the kernel refused the `{name}` program: {error}")
        }
    })
}

/// Stores the host-order 16-bit port in `r1` at `at` in network order, the
/// order the raw program stores `inet_sport` in, and zeroes the word's upper
/// half. Clobbers `r2`.
pub(super) fn store_port_be(program: &mut Program, at: i16) {
    program.push(Insn::mov64_reg(2, 1));
    program.push(Insn::rsh64_imm(2, 8));
    program.push(Insn::stx(BPF_B, R10, at, 2));
    program.push(Insn::stx(BPF_B, R10, at + 1, 1));
    program.push(Insn::st_imm(BPF_B, R10, at + 2, 0));
    program.push(Insn::st_imm(BPF_B, R10, at + 3, 0));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both ways to a socket's inode can be composed on the running kernel,
    /// and the walk lands on the same `i_ino` `sock_i_ino` reads: the inode
    /// that sits right after the `struct socket` in its allocation.
    #[test]
    fn the_walk_to_a_sockets_inode_is_composed_from_named_members() {
        let Ok(btf) = super::super::kernel_btf() else {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        };
        let SocketInode::ThroughSocket { socket_at, ino_at } =
            SocketInode::through_socket(btf).expect("every kernel has the walk")
        else {
            panic!("the walk is a walk");
        };
        assert!(
            socket_at > 0,
            "`sk_socket` is not the first member of `sock`"
        );
        let socket_bytes = btf
            .member_offset("socket_alloc", "vfs_inode")
            .expect("the inode is beside the socket");
        assert!(
            ino_at > socket_bytes,
            "the inode number is inside the inode, past the socket: {ino_at} against {socket_bytes}"
        );
        match SocketInode::locate(btf).expect("one or the other") {
            SocketInode::Member(at) => assert_eq!(Ok(at), btf.member_offset("sock", "sk_ino")),
            walk => assert_eq!(walk, SocketInode::through_socket(btf).unwrap()),
        }
    }

    /// Every accepted prototype is accepted, and a kernel declaring something
    /// else is refused with both the shape it has and the shapes the program
    /// knows -- because a reader whose kernel is refused needs to know which
    /// of the two it is.
    #[test]
    fn a_prototype_that_is_not_one_of_the_accepted_shapes_is_refused_by_name() {
        let Ok(btf) = super::super::kernel_btf() else {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        };
        let accepted: &[&[&str]] = &[&["unsigned int"]];
        assert!(
            expect_signature(btf, "softirq_entry", accepted).is_ok(),
            "softirq_entry takes a vector number and nothing else"
        );
        let error = expect_signature(btf, "softirq_entry", &[&["task_struct *"]]).unwrap_err();
        assert!(error.contains("unsigned int"), "{error}");
        assert!(error.contains("task_struct *"), "{error}");
    }
}
