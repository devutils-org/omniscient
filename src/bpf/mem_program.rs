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

use super::insn::{self, helper, Insn, Log2Labels, Program, BPF_DW, R10};

// Written once in `insn.rs` since the seventh wave. `bucket_low` keeps this
// family's name for it: the arithmetic is the layout's and carries no unit,
// where a `mem` bucket may hold pages, bytes or nanoseconds.
pub use super::insn::bucket_low_ns as bucket_low;
pub use super::insn::count_drop;
pub use super::insn::helper::GET_CURRENT_COMM;
use super::syscall;

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

/// This family's own labels for the shared unrolled log2.
///
/// The sequence itself is `insn::push_log2_bucket`, which is where the one
/// definition of `emit::BucketLayout::Log2`'s numbering lives. What stays here
/// is the label set, because a label is defined once per instruction stream.
const LOG2: Log2Labels = [
    "mem_log2_32",
    "mem_log2_16",
    "mem_log2_8",
    "mem_log2_4",
    "mem_log2_2",
    "mem_log2_1",
    "mem_log2_done",
];

/// Computes the log2 bucket of the value in `value_reg` into `r3`.
///
/// A program may only call this once: the labels are fixed, and a second call
/// in the same program would place each of them twice.
pub fn push_log2_bucket(program: &mut Program, value_reg: u8) {
    insn::push_log2_bucket(program, value_reg, &LOG2);
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
