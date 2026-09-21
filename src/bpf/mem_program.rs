//! The pieces the `mem` family's four eBPF programs share.
//!
//! `mem_alloc.rs` counts what the page and slab allocators hand out, and
//! `mem_stall.rs` times what the kernel does inside an allocating thread when
//! it cannot. They are different measurements, and below the shape they are the
//! same program four times over: identify the task, bucket a number, and add
//! one to a row of a hash map that userspace drains on a tick.
//!
//! So the fragments live here rather than in each of them. A fragment is a few
//! instructions with an argument that is wrong in a way no compiler can see --
//! a `bpf_get_current_comm` given the wrong buffer length writes over the field
//! beside it, and a bucket computed one step short puts every measurement in
//! the neighbouring bucket, which is a factor of two and looks like a number.
//! Written once, each of them is checked once.
//!
//! Nothing here is public beyond `src/bpf/`. The shared layer proper is
//! `attach.rs`, `insn.rs`, `map.rs`, `btf.rs` and `syscall.rs`, and this is not
//! a fifth: it is one family's own scaffolding, and the moment a second family
//! wants the log2 bucket it belongs in `insn.rs` instead. That request is in
//! `docs/families/mem.md`, because this is already its third copy.

use std::os::fd::OwnedFd;

use super::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::syscall;

/// `bpf_get_current_comm(void *buf, u32 size)`.
///
/// Not in `insn::helper`, which names the eight the layer used before this
/// family. Declared here rather than added there because `insn.rs` is the
/// shared layer and a stream does not edit it; the request to hoist both of
/// these is written down in `docs/families/mem.md`.
pub const GET_CURRENT_COMM: i32 = 16;

/// `bpf_get_current_cgroup_id(void)`.
///
/// Returns the id of the current task's cgroup in the v2 hierarchy, which is
/// the inode number of that cgroup's directory under `/sys/fs/cgroup`. That
/// equality is what lets userspace turn a `--cgroup` path into a number this
/// side can compare against, without a second map and without reading
/// `task_struct->cgroups` through four pointers whose offsets all move.
pub const GET_CURRENT_CGROUP_ID: i32 = 80;

/// How many bytes of `comm` a row carries. The kernel's own `TASK_COMM_LEN`,
/// which is what `bpf_get_current_comm` writes and what a shorter buffer would
/// silently truncate.
pub const COMM_BYTES: usize = 16;

/// `bpf_probe_read_kernel` is GPL-only, and so is `bpf_get_current_cgroup_id`
/// on some kernels. Every program in this family declares GPL for that reason.
pub const LICENSE: &str = "GPL";

/// The steps of the unrolled `log2`, as (shift, the label that skips it).
///
/// There is no count-leading-zeros instruction in BPF and no loop worth writing
/// on a path this hot, so the exponent is found by binary search: six compares
/// and no branch that depends on anything but the value.
const LOG2_STEPS: [(i32, &str); 6] = [
    (32, "mem_log2_32"),
    (16, "mem_log2_16"),
    (8, "mem_log2_8"),
    (4, "mem_log2_4"),
    (2, "mem_log2_2"),
    (1, "mem_log2_1"),
];

/// Computes the log2 bucket of the value in `value_reg` into `r3`.
///
/// The numbering is `emit::BucketLayout::Log2`'s, deliberately: zero is bucket
/// zero and everything else is `floor(log2(v)) + 1`, so bucket *k* holds
/// `[2^(k-1), 2^k)` on both sides of the kernel boundary. A histogram built
/// here passes through the emit layer's own bucketing unchanged instead of
/// being re-bucketed into something that is neither.
///
/// `r1` and `r2` are scratch. A program must not call a helper between this and
/// the store of `r3`.
pub fn push_log2_bucket(program: &mut Program, value_reg: u8) {
    program.push(Insn::mov64_reg(1, value_reg));
    program.push(Insn::mov64_imm(3, 0));
    // A value of exactly zero is bucket zero, not the log2 of zero.
    program.jump_to("mem_log2_done", Insn::jeq_imm(1, 0, 0));
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
    program.label("mem_log2_done");
}

/// The same function, in Rust, as the reference the tests check the search
/// against.
///
/// Deliberately a different expression of the definition from the six-step
/// search above, and deliberately not a call into `emit`: an independent one is
/// the only thing that can catch the search being off by one, and off by one
/// here is a factor of two in every number these sources report. Nothing on the
/// collecting path calls it -- a row arrives with its bucket already computed --
/// which is why it is not compiled into the binary.
#[cfg(test)]
pub fn log2_bucket(value: u64) -> u32 {
    u64::BITS - value.leading_zeros()
}

/// The low bound of a log2 bucket, which is the value that travels on an event.
///
/// It is the one number the emit layer's own bucketing maps back to exactly the
/// bucket it came from, so a histogram built in the kernel and one built in
/// userspace from these events put the measurements in the same places.
pub fn bucket_low(bucket: u32) -> u64 {
    match bucket {
        0 => 0,
        // Unreachable from a bucket this family computes -- the values are page
        // counts, byte counts and nanosecond counts -- but a shift that would
        // overflow is a panic rather than a wrong answer, so it is written down.
        k if k >= 64 => 1 << 63,
        k => 1 << (k - 1),
    }
}

/// Reads `bpf_get_current_pid_tgid()` onto the stack at `at`.
///
/// The helper returns one register holding `(tgid << 32) | pid`, and there is
/// no 32-bit shift in this encoding worth using when the two halves can be read
/// back off the stack as words: `at` is the thread id and `at + 4` is the
/// thread group. Eight bytes are written, so both halves are initialised.
pub fn read_pid_tgid(program: &mut Program, at: i16) {
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_DW, R10, at, 0));
}

/// Writes the current task's name into `into`, which must have [`COMM_BYTES`]
/// of room.
///
/// The helper zero-fills the buffer when it cannot read the name, so every byte
/// of the field is written either way -- which matters, because this field is
/// in a map *value* that the program hands to `bpf_map_update_elem` whole.
pub fn read_comm(program: &mut Program, into: i16) {
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, into as i32));
    program.push(Insn::mov64_imm(2, COMM_BYTES as i32));
    program.push(Insn::call(GET_CURRENT_COMM));
}

/// Writes the current task's cgroup id into `into`, or a zero where the source
/// declares no cgroup key.
///
/// The zero is not a fallback for a helper that failed: it is the program
/// saying this source has no cgroup dimension, and it is written rather than
/// left because the kernel hashes every byte of a key. `mem.kmalloc` is the
/// hottest program in this family and its entry declares no cgroup, so it does
/// not pay for the call.
pub fn read_cgroup_id(program: &mut Program, into: i16, wanted: bool) {
    if !wanted {
        program.push(Insn::st_imm(BPF_DW, R10, into, 0));
        return;
    }
    program.push(Insn::call(GET_CURRENT_CGROUP_ID));
    program.push(Insn::stx(BPF_DW, R10, into, 0));
}

/// Adds one to a slot of the per-cpu drop counter.
///
/// `resume` is where control goes both when the count has been made and when
/// the counter could not be found: failing to record a loss is not a reason to
/// stop recording measurements.
pub fn count_drop(
    program: &mut Program,
    drops_fd: i32,
    key_at: i16,
    slot: u32,
    resume: &'static str,
) {
    program.push(Insn::st_imm(BPF_W, R10, key_at, slot as i32));
    program.extend(Insn::load_map_fd(1, drops_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, key_at as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to(resume, Insn::jeq_imm(0, 0, 0));
    // Per-cpu, so this slot belongs to this cpu alone and needs no atomic.
    program.push(Insn::ldx(BPF_DW, 1, 0, 0));
    program.push(Insn::add64_imm(1, 1));
    program.push(Insn::stx(BPF_DW, 0, 0, 1));
}

/// Loads a raw tracepoint program, turning a refusal into something to read.
pub fn load(instructions: &[Insn], name: &str) -> Result<OwnedFd, String> {
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

/// Refuses a kernel whose tracepoint is not the shape a program was written
/// for.
///
/// A raw tracepoint hands its program an array of `u64` and nothing in that
/// array says what any of them is: a program reads slot two as a byte count
/// because the prototype says so, and against another prototype that slot holds
/// a pointer and the byte count is four hundred billion. So the prototype is
/// compared before an instruction is built, and a mismatch is an error naming
/// both shapes rather than a measurement nobody can tell is wrong.
pub fn check_signature(
    btf: &super::btf::Btf,
    tracepoint: &str,
    expected: &[&str],
    reads: &str,
) -> Result<(), String> {
    let signature = btf.tracepoint_signature(tracepoint)?;
    if signature != expected {
        return Err(format!(
            "this kernel declares `{tracepoint}({})`, and this program is written for \
             `{tracepoint}({})`: it reads {reads} out of fixed slots, and against another \
             prototype those slots hold something else",
            signature.join(", "),
            expected.join(", "),
        ));
    }
    Ok(())
}

/// Reads a `u32` field of a row key out of a drained record.
pub fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

/// Reads a `u64` field out of a drained record.
pub fn long(bytes: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

/// Reads a `comm` field out of a drained record, up to its first zero byte.
pub fn comm(bytes: &[u8], at: usize) -> String {
    let field = &bytes[at..at + COMM_BYTES];
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel's bucketing and the emit layer's must be the same function.
    ///
    /// This is the whole reason a histogram can be built in the kernel at all:
    /// the rows come back as `(bucket, count)` and are rendered by a layer that
    /// computes bucket indices itself. If the two disagree by one, every
    /// measurement is reported in the neighbouring bucket -- a factor of two --
    /// and nothing anywhere says so.
    #[test]
    fn the_bucket_index_and_its_low_bound_are_inverses() {
        for bucket in 0..48u32 {
            let low = bucket_low(bucket);
            assert_eq!(
                log2_bucket(low),
                bucket,
                "bucket {bucket} starts at {low}, which the layout puts elsewhere"
            );
            if bucket > 0 {
                assert_eq!(
                    log2_bucket(low - 1),
                    bucket - 1,
                    "bucket {bucket} and the one below it overlap"
                );
            }
        }
    }

    /// The unrolled binary search has to compute `floor(log2(v)) + 1`, which is
    /// what the emit layer's index is. Checked against the same shift
    /// arithmetic the instruction stream performs, because the instructions
    /// themselves cannot be run outside the kernel.
    #[test]
    fn the_unrolled_log2_is_the_same_arithmetic_as_the_layouts() {
        let unrolled = |value: u64| -> u32 {
            if value == 0 {
                return 0;
            }
            let (mut value, mut exponent) = (value, 0u32);
            for (shift, _) in LOG2_STEPS {
                let shifted = value >> shift;
                if shifted != 0 {
                    exponent += shift as u32;
                    value = shifted;
                }
            }
            exponent + 1
        };
        for value in [0u64, 1, 2, 3, 4, 7, 8, 4096, 999_999, 1 << 40, u64::MAX] {
            assert_eq!(
                unrolled(value),
                log2_bucket(value),
                "{value} is bucketed differently by the two"
            );
        }
    }

    /// A page allocation of order *k* is `2^k` pages, and this family reports
    /// its bucket as `k + 1` without running the search at all. That shortcut is
    /// only correct while the two agree.
    #[test]
    fn an_orders_bucket_is_the_bucket_of_its_page_count() {
        for order in 0..11u32 {
            assert_eq!(
                log2_bucket(1 << order),
                order + 1,
                "order {order} is {} pages, which belongs in another bucket",
                1u64 << order
            );
        }
    }

    #[test]
    fn a_name_shorter_than_its_field_stops_at_its_own_end() {
        let mut field = [0u8; COMM_BYTES];
        field[..4].copy_from_slice(b"omn\0");
        assert_eq!(comm(&field, 0), "omn");
        let full = [b'x'; COMM_BYTES];
        assert_eq!(comm(&full, 0).len(), COMM_BYTES);
    }
}
