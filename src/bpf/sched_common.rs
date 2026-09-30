// SPDX-License-Identifier: MIT OR GPL-2.0
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

use super::insn::{Insn, Program};

// The fragments these four programs share with every other family now live in
// `insn.rs`, where one copy decides what every kernel-built histogram in this
// repository does. They are re-exported here so that a `sched` program reads as
// one import, and because the seventh wave moved them rather than the sixth.
pub use super::insn::{bucket_low_ns, count_drop, push_log2_bucket, read_field, Log2Labels};

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
