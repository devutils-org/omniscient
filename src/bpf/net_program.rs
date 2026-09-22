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
use super::insn::{self, Insn, Log2Labels, Program};

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
pub(super) const LICENSE: &str = "GPL";

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

#[cfg(test)]
mod tests {
    use super::*;

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
