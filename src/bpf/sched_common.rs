//! The pieces the `sched` programs added in the sixth wave share.
//!
//! Four of them -- lock contention, futex waits, interrupt handlers and NUMA
//! balancing -- are the same shape seen four times: a pair of tracepoints that
//! open and close an interval, a per-key log2 histogram built in the kernel,
//! and a per-cpu counter of what could not be recorded. The parts that are
//! genuinely identical live here rather than four times over, because a log2
//! that disagrees with `emit::BucketLayout::Log2` in one of four copies is a
//! defect nobody would find by reading any single file.
//!
//! What is deliberately *not* here is the key layout, the filter and the
//! pairing. Those differ in each program for reasons the program explains --
//! a lock is keyed by the contending task, an interrupt by the cpu it landed
//! on -- and a generic version of them would hide exactly what a reader of
//! those files needs to see.

use super::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};

/// Helpers this layer's own `insn::helper` does not list yet.
///
/// They are numbers in the kernel's `bpf_func_id` enum, and naming them here
/// rather than in `insn.rs` keeps this wave out of a file five streams share.
/// The request to move them is in `docs/open-findings.md`.
pub mod extra_helper {
    /// `bpf_get_current_comm(buf, size)`.
    pub const GET_CURRENT_COMM: i32 = 16;
    /// `bpf_probe_read_kernel_str(dst, size, unsafe_ptr)`, which stops at the
    /// terminator rather than reading a fixed span past the end of a string.
    pub const PROBE_READ_KERNEL_STR: i32 = 115;
}

/// `BPF_F_USER_STACK`: walk the task's own stack rather than the kernel's.
///
/// The kernel side of a lock wait or a futex wait is the lock primitive, which
/// is the same handful of functions every time and says nothing about the
/// program. The user side is the call that took the lock, which is the answer
/// someone arrived with the question for. It is also the only side this build
/// can name: kernel frames would need `/proc/kallsyms`, which a normal machine
/// restricts to root, and `CAP_SYS_ADMIN` is more than the two capabilities
/// this project settled on.
pub const BPF_F_USER_STACK: i32 = 1 << 8;

/// Reads `size` bytes of a kernel structure into the program's stack.
///
/// `bpf_probe_read_kernel` rather than a plain load: the pointer came out of a
/// tracepoint argument and the verifier knows nothing about what it points at.
pub fn read_field(program: &mut Program, into: i16, size: i32, from: u8, field_offset: i32) {
    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, into as i32));
    program.push(Insn::mov64_imm(2, size));
    program.push(Insn::mov64_reg(3, from));
    program.push(Insn::add64_imm(3, field_offset));
    program.push(Insn::call(helper::PROBE_READ_KERNEL));
}

/// Adds one to a slot of a per-cpu drop counter.
///
/// `resume` is where control goes both when the count has been made and when
/// the counter could not be found: failing to record a loss is not a reason to
/// stop recording measurements.
pub fn count_drop(
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

/// Errno values `bpf_map_update_elem` returns, negated the way a program sees
/// them.
///
/// `-E2BIG` is a hash map with no room for a new key. `-EEXIST` is
/// `BPF_NOEXIST` refusing to overwrite an entry that is already there, which
/// for an in-flight map means the same key opened an interval that has not
/// closed. Anything else is the kernel declining the update at that instant --
/// for these programs almost always `-EBUSY`, the hash map's own bucket lock
/// already held on this cpu by an outer program.
pub const E2BIG: i32 = 7;
pub const EEXIST: i32 = 17;

/// `BPF_NOEXIST`: fail rather than overwrite.
pub const BPF_NOEXIST: i32 = 1;

/// Counts a failed `bpf_map_update_elem` under the reason the kernel gave.
///
/// The reasons are different facts about the run and are answered differently:
/// a full map is a size this build chose and can raise, a refused overwrite is
/// an interval inside another interval, and a busy bucket is the kernel
/// declining a re-entrant update on a path that by its nature re-enters. A
/// loss reported under the wrong one sends a reader to change something that
/// was never the problem, which costs more than a bare number would.
///
/// Expects the failed return value still in `r0`. Each reason carries the
/// label that skips it, because a program may contain more than one of these
/// and a label is defined once.
pub fn count_update_failure(
    program: &mut Program,
    drops_fd: i32,
    drop_key: i16,
    reasons: &[(i32, u32, &'static str)],
    fallback_slot: u32,
    resume: &'static str,
) {
    for (errno, slot, next) in reasons {
        program.jump_to(next, Insn::jne_imm(0, -errno, 0));
        count_drop(program, drops_fd, drop_key, *slot, resume);
        program.jump_to(resume, Insn::ja());
        program.label(next);
    }
    count_drop(program, drops_fd, drop_key, fallback_slot, resume);
}

/// The steps of the unrolled `log2`, as (shift, the label that skips it).
///
/// There is no count-leading-zeros instruction in BPF and no loop worth writing
/// on a path this hot, so the exponent is found by binary search: six compares,
/// thirty instructions, no branches that depend on anything but the value.
///
/// Each program passes its own labels, because two copies of this in one
/// instruction stream would define the same label twice.
pub type Log2Labels = [&'static str; 7];

/// Computes the log2 bucket of the value in `value_reg` into `r3`.
///
/// The numbering is `emit::BucketLayout::Log2`'s, deliberately: zero is bucket
/// zero and everything else is `floor(log2(v)) + 1`, so that bucket *k* holds
/// `[2^(k-1), 2^k)` on both sides of the kernel boundary. That is what lets a
/// histogram built in the kernel pass through the emit layer's own bucketing
/// without being re-bucketed into something else.
///
/// `r1` and `r2` are scratch, which is safe because nothing between here and
/// the next helper call reads them.
pub fn push_log2_bucket(program: &mut Program, value_reg: u8, labels: &Log2Labels) {
    const SHIFTS: [i32; 6] = [32, 16, 8, 4, 2, 1];
    program.push(Insn::mov64_reg(1, value_reg));
    program.push(Insn::mov64_imm(3, 0));
    // A wait of exactly zero nanoseconds -- two events inside one clock tick --
    // is bucket zero, not the log2 of zero.
    program.jump_to(labels[6], Insn::jeq_imm(1, 0, 0));
    for (step, shift) in SHIFTS.iter().enumerate() {
        program.push(Insn::mov64_reg(2, 1));
        program.push(Insn::rsh64_imm(2, *shift));
        program.jump_to(labels[step], Insn::jeq_imm(2, 0, 0));
        program.push(Insn::add64_imm(3, *shift));
        program.push(Insn::mov64_reg(1, 2));
        program.label(labels[step]);
    }
    // `floor(log2(v))` is the exponent; the layout's index is one more, so that
    // bucket 0 can mean "below one" rather than "exactly one".
    program.push(Insn::add64_imm(3, 1));
    program.label(labels[6]);
}

/// The low bound of a log2 bucket, in the same numbering the program uses.
///
/// This is the value that travels on an event, because it is the one the emit
/// layer's own log2 bucketing maps back to exactly this bucket: a histogram
/// built in the kernel and one built in userspace from these events agree.
pub fn bucket_low_ns(bucket: u32) -> u64 {
    match bucket {
        0 => 0,
        // A duration is a nanosecond count and `bucket` is derived from one, so
        // this cannot reach 64 -- but a shift that would overflow is a panic
        // rather than a wrong answer, so the case is written down.
        k if k >= 64 => 1 << 63,
        k => 1 << (k - 1),
    }
}

/// Where a name ends inside a fixed-width field.
pub fn text_of(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout's own definition, written out rather than called: bucket *k*
    /// holds `[2^(k-1), 2^k)`, so the index of a value is `floor(log2(v)) + 1`.
    /// An independent expression of it is the only thing that can catch the
    /// unrolled search, or this table, being off by one.
    fn reference_index(value: u64) -> u32 {
        u64::BITS - value.leading_zeros()
    }

    /// The whole point of building a histogram in the kernel is that the emit
    /// layer can bucket the events again and land in the same places. If these
    /// two definitions ever part, every kernel-built distribution in this
    /// family is quietly reported one bucket -- a factor of two -- away from
    /// where it belongs, and nothing says so.
    #[test]
    fn a_buckets_low_bound_belongs_to_that_bucket_and_not_the_one_below() {
        for bucket in 0..48u32 {
            let low = bucket_low_ns(bucket);
            assert_eq!(
                reference_index(low),
                bucket,
                "bucket {bucket} starts at {low}, which the layout puts elsewhere"
            );
            if bucket > 0 {
                assert_eq!(
                    reference_index(low - 1),
                    bucket - 1,
                    "bucket {bucket} and the one below it overlap"
                );
            }
        }
    }

    /// Every value has to land in the bucket whose half-open range contains
    /// it, including the two ends the unrolled search treats specially: zero,
    /// which is bucket zero rather than the log2 of nothing, and one.
    #[test]
    fn every_value_falls_in_the_bucket_that_contains_it() {
        for value in [
            0u64,
            1,
            2,
            3,
            4,
            7,
            8,
            1023,
            1024,
            300_000_000,
            u64::MAX / 2,
        ] {
            let bucket = reference_index(value);
            assert!(
                bucket_low_ns(bucket) <= value,
                "{value} is below its own bucket's low bound"
            );
            assert!(
                value < bucket_low_ns(bucket + 1),
                "{value} reaches into the next bucket"
            );
        }
        assert_eq!(bucket_low_ns(0), 0);
    }

    #[test]
    fn a_name_stops_at_its_terminator_and_survives_a_full_field() {
        assert_eq!(text_of(b"eth0\0\0\0\0"), "eth0");
        assert_eq!(text_of(b"abcd"), "abcd");
        assert_eq!(text_of(b"\0rest"), "");
    }
}
