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
use super::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::syscall;

/// The license these programs declare.
///
/// `bpf_probe_read_kernel` is GPL-only and every field any of them reads comes
/// out of a `sock`, an `sk_buff`, a `Qdisc` or a `net_device` through it. The
/// declaration covers the instruction streams in the `net_*.rs` files, which is
/// what the kernel is asking about; `omn` itself stays under its own license.
pub(super) const LICENSE: &str = "GPL";

/// `bpf_get_current_comm`, which `insn::helper` does not name yet.
///
/// Declared here rather than reaching into the shared module, for the same
/// reason as the helpers above: the request to move it is in
/// `docs/open-findings.md`.
pub(super) const GET_CURRENT_COMM: i32 = 16;

/// Reads `size` bytes of a kernel structure into the program's stack.
///
/// `bpf_probe_read_kernel` rather than a plain load: the pointer came out of a
/// tracepoint argument and the verifier knows nothing about what it points at.
pub(super) fn read_field(program: &mut Program, into: i16, size: i32, from: u8, field_offset: i32) {
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, into as i32));
    program.push(Insn::mov64_imm(2, size));
    program.push(Insn::mov64_reg(3, from));
    program.push(Insn::add64_imm(3, field_offset));
    program.push(Insn::call(helper::PROBE_READ_KERNEL));
}

/// Reads a field narrower than a word into a four-byte slot, zero-extended.
///
/// Most of what this family wants out of the kernel is two bytes wide -- an
/// address family, a protocol number, a port -- and the instruction encoding
/// this layer exposes loads four or eight. So the slot is cleared first and the
/// kernel is asked for exactly the field's own width: what lands is the field,
/// and the bytes beside it are zero rather than whatever the stack held.
///
/// Reading four bytes and masking would be the other way round, and it reads
/// one to three bytes of the next field to do it -- which is a fault where the
/// field is the last of a page-aligned structure, and a wrong number the rest
/// of the time.
pub(super) fn read_narrow(
    program: &mut Program,
    into: i16,
    size: i32,
    from: u8,
    field_offset: i32,
) {
    program.push(Insn::st_imm(BPF_W, R10, into, 0));
    read_field(program, into, size, from, field_offset);
}

/// Follows a pointer field into a register.
///
/// `scratch` is four bytes of stack the read lands in before the load; the
/// register is where the pointer is wanted. `into` should be one of `r6`
/// through `r9` if anything after the next helper call reads it.
pub(super) fn read_pointer(
    program: &mut Program,
    scratch: i16,
    into: u8,
    from: u8,
    field_offset: i32,
) {
    program.push(Insn::st_imm(BPF_DW, R10, scratch, 0));
    read_field(program, scratch, 8, from, field_offset);
    program.push(Insn::ldx(BPF_DW, into, R10, scratch));
}

/// Adds one to a slot of a per-cpu drop counter.
///
/// `resume` is where control goes both when the count has been made and when
/// the counter could not be found: failing to record a loss is not a reason to
/// stop recording measurements.
pub(super) fn count_drop(
    program: &mut Program,
    drops_fd: i32,
    drop_key: i16,
    slot: u32,
    resume: &'static str,
) {
    program.push(Insn::st_imm(BPF_W, R10, drop_key, slot as i32));
    program.extend(Insn::load_map_fd(1, drops_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, drop_key as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to(resume, Insn::jeq_imm(0, 0, 0));
    // Per-cpu, so this slot belongs to this cpu alone and needs no atomic.
    program.push(Insn::ldx(BPF_DW, 1, 0, 0));
    program.push(Insn::add64_imm(1, 1));
    program.push(Insn::stx(BPF_DW, 0, 0, 1));
}

/// The steps of the unrolled `log2`, as (shift, the label that skips it).
///
/// There is no count-leading-zeros instruction in BPF and no loop worth writing
/// on a path this hot, so the exponent is found by binary search: six compares,
/// thirty instructions, no branches that depend on anything but the value.
const LOG2_STEPS: [(i32, &str); 6] = [
    (32, "net_log2_32"),
    (16, "net_log2_16"),
    (8, "net_log2_8"),
    (4, "net_log2_4"),
    (2, "net_log2_2"),
    (1, "net_log2_1"),
];

/// Computes the log2 bucket of the value in `value_reg` into `r3`.
///
/// The numbering is `emit::BucketLayout::Log2`'s, deliberately: zero is bucket
/// zero and everything else is `floor(log2(v)) + 1`, so that bucket *k* holds
/// `[2^(k-1), 2^k)` on both sides of the kernel boundary. `r1` and `r2` are
/// scratch, which is safe because nothing between here and the next helper call
/// reads them.
///
/// A program may only call this once: the labels are fixed, and a second call
/// in the same program would place each of them twice.
pub(super) fn push_log2_bucket(program: &mut Program, value_reg: u8) {
    program.push(Insn::mov64_reg(1, value_reg));
    program.push(Insn::mov64_imm(3, 0));
    // A duration of exactly zero nanoseconds -- two events inside one clock
    // tick -- is bucket zero, not the log2 of zero.
    program.jump_to("net_log2_done", Insn::jeq_imm(1, 0, 0));
    for (shift, label) in LOG2_STEPS {
        program.push(Insn::mov64_reg(2, 1));
        program.push(Insn::rsh64_imm(2, shift));
        program.jump_to(label, Insn::jeq_imm(2, 0, 0));
        program.push(Insn::add64_imm(3, shift));
        program.push(Insn::mov64_reg(1, 2));
        program.label(label);
    }
    // `floor(log2(v))` is the exponent; the layout's index is one more, so that
    // bucket 0 can mean "below one" rather than "exactly one".
    program.push(Insn::add64_imm(3, 1));
    program.label("net_log2_done");
}

/// The low bound of a log2 bucket, which is the value that travels on an event.
///
/// It is the one number the emit layer's own bucketing maps back to exactly
/// this bucket, so a histogram built in the kernel and one built in userspace
/// from these events put the same measurement in the same place.
pub(super) fn bucket_low_ns(bucket: u32) -> u64 {
    match bucket {
        0 => 0,
        // A duration is a nanosecond count and `bucket` is derived from one, so
        // this cannot reach 64 -- but a shift that would overflow is a panic
        // rather than a wrong answer, so the case is written down.
        k if k >= 64 => 1 << 63,
        k => 1 << (k - 1),
    }
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

    /// The bucket index the emit layer computes, written as the machine's own
    /// count-leading-zeros rather than as the six-step search the program
    /// performs. `emit::BucketLayout::Log2` documents exactly this: bucket 0
    /// holds what is below one, and bucket *k* holds `[2^(k-1), 2^k)`.
    ///
    /// Deliberately not a call into `emit`, and deliberately not the same
    /// arithmetic as the instruction stream: an independent expression of the
    /// definition is the only thing that can catch the search being off by one.
    fn reference_index(value: u64) -> u64 {
        u64::from(u64::BITS - value.leading_zeros())
    }

    /// The kernel's bucketing and the emit layer's must be the same function.
    ///
    /// This is the whole reason a histogram can be built in the kernel at all:
    /// the rows come back as `(bucket, count)` and are rendered by a layer that
    /// computes bucket indices itself. If the two disagree by one, every
    /// duration is reported in the neighbouring bucket -- a factor of two --
    /// and nothing anywhere says so.
    #[test]
    fn the_kernels_buckets_and_the_emit_layers_are_the_same_buckets() {
        assert_eq!(bucket_low_ns(0), 0, "bucket zero holds only zero");
        for bucket in 1..48u32 {
            let low = bucket_low_ns(bucket);
            assert_eq!(
                reference_index(low),
                u64::from(bucket),
                "bucket {bucket} starts at {low}, which the emit layer buckets elsewhere"
            );
            assert_eq!(
                reference_index(low * 2 - 1),
                u64::from(bucket),
                "the top of bucket {bucket} is in it too"
            );
        }
    }

    /// The narrow read clears its slot before the kernel writes into it. A slot
    /// left alone holds whatever the last event put there, and a two-byte
    /// address family read into it comes back with two bytes of somebody
    /// else's port above it.
    #[test]
    fn a_narrow_read_clears_its_slot_first() {
        let mut program = Program::new();
        read_narrow(&mut program, -16, 2, 7, 24);
        program.push(Insn::exit());
        let built = program.build();
        assert_eq!(built[0], Insn::st_imm(BPF_W, R10, -16, 0));
        assert!(
            built
                .iter()
                .any(|insn| insn.code == 0x85 && insn.imm == helper::PROBE_READ_KERNEL),
            "the field itself is read through bpf_probe_read_kernel"
        );
        let size = built
            .iter()
            .find(|insn| insn.code == 0xb7 && insn.regs & 0xf == 2)
            .expect("a size in r2");
        assert_eq!(size.imm, 2, "the kernel is asked for the field's own width");
        super::super::insn::audit(&built).expect("a well-formed sequence");
    }

    /// Every accepted prototype is accepted, and a kernel declaring something
    /// else is refused with both the shape it has and the shapes the program
    /// knows -- because a reader whose kernel is refused needs to know which
    /// of the two it is.
    #[test]
    fn a_prototype_that_is_not_one_of_the_accepted_shapes_is_refused_by_name() {
        let Ok(btf) = Btf::from_kernel() else {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        };
        let accepted: &[&[&str]] = &[&["unsigned int"]];
        assert!(
            expect_signature(&btf, "softirq_entry", accepted).is_ok(),
            "softirq_entry takes a vector number and nothing else"
        );
        let error = expect_signature(&btf, "softirq_entry", &[&["task_struct *"]]).unwrap_err();
        assert!(error.contains("unsigned int"), "{error}");
        assert!(error.contains("task_struct *"), "{error}");
    }
}
