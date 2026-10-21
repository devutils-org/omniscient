// SPDX-License-Identifier: MIT OR GPL-2.0
//! `net.softirq`: how long the kernel spent in a network softirq, per cpu.
//!
//! A packet that arrives while no process is asking for it is processed in
//! softirq context: the driver raises `NET_RX`, the kernel runs the stack out
//! of `__do_softirq`, and none of that time belongs to any process. It is
//! invisible to every per-process measurement this tool has, and on a machine
//! moving real traffic it is where the cpu went.
//!
//! Two tracepoints, and the measurement is the gap between them. `softirq_entry`
//! says a vector's handler started on this cpu, `softirq_exit` says it
//! finished, and neither says anything on its own. So one program writes the
//! moment the handler started and the other reads it back.
//!
//! **The state lives in a per-cpu array, and that is not an optimisation.** A
//! softirq handler is not preempted by another softirq on the same cpu, so at
//! any instant there is at most one open interval per `(cpu, vector)` -- which
//! is exactly what a per-cpu array slot per vector is. A hash keyed by cpu
//! would be the same fact written more expensively, and it would need an atomic
//! on a path that runs tens of thousands of times a second.
//!
//! **The histogram is built in the kernel**, for the reason `runqlat.rs` gives:
//! `NET_RX` fires once per interrupt-coalesced batch of packets, which on a
//! loaded link is tens of thousands of times a second, and one ring record per
//! handler would make the cost of the measurement grow with the traffic being
//! measured. What the kernel keeps instead is a count and an exact total per
//! `(cpu, vector, log2 bucket)`.
//!
//! **Which vectors are network vectors is not written down here.** The numbers
//! the tracepoint reports are indices into the kernel's own `softirq_to_name`
//! table, and the kernel prints that table, in that order, as the row labels of
//! `/proc/softirqs`. So the collector reads the numbers out of the running
//! kernel and hands them in; nothing in this file assumes that `NET_RX` is 3.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, row_map, select_rows, Counters, Rows};
use super::net_program::{bucket_low_ns, count_drop, expect_signature, load, push_log2_bucket};
use super::syscall::{self, BPF_MAP_TYPE_PERCPU_ARRAY};

/// The prototype both programs are written against.
///
/// `softirq_entry` and `softirq_exit` share one event class and have taken a
/// bare vector number since tracing existed, but the check costs nothing and
/// the alternative to it is reading a vector number out of a slot that holds
/// something else.
const SIGNATURE: [&str; 1] = ["unsigned int"];

/// How many softirq vectors the kernel has. The per-cpu array is indexed by
/// vector number, so it needs one slot for each; `NR_SOFTIRQS` has been ten
/// since 2009 and a vector beyond the array is refused rather than folded.
const VECTORS: u32 = 10;

/// How many distinct `(cpu, vector, bucket)` rows one interval may hold. Two
/// network vectors, one row per bucket per cpu: a few hundred at the very most,
/// whatever the traffic does, which is the point of counting in the kernel.
const MAX_ROWS: u32 = 16384;

/// Slots of the counter map, one per thing worth counting apart.
const DROP_ROWS: u32 = 0;
const UNPAIRED_EXITS: u32 = 1;
const COUNTER_SLOTS: u32 = 2;

/// Where each map's key and value live on the program's stack.
const ROW_VALUE: i16 = -56;
const ROW_KEY: i16 = -32;
const AT_KEY: i16 = -20;
const DROP_KEY: i16 = -16;
/// The row map of this interval, as the generation slot named it, kept in
/// this slot once per run; see `map::select_rows`.
const ROWS_SLOT: i16 = -64;

/// The layout of a row key.
const KEY_CPU: i16 = 0;
const KEY_VEC: i16 = 4;
/// The log2 bucket the handler's duration fell into. In the key is what makes
/// this a histogram rather than a mean: a cpu that mostly spends two
/// microseconds in `NET_RX` and occasionally spends four milliseconds is two
/// rows here and one indistinguishable average anywhere else.
const KEY_BUCKET: i16 = 8;
const ROW_KEY_BYTES: usize = 12;

/// The layout of a row value.
const VALUE_COUNT: i16 = 0;
/// The exact sum of the durations in this bucket, kept beside the count because
/// the bucket is a factor of two wide and a total built from its bounds would
/// inherit that. One more atomic add, and the mean becomes exact.
const VALUE_TOTAL_NS: i16 = 8;
/// When the first run of this row's window *began*.
///
/// A bucketed source emits one event per row per window, and stamping that
/// event at the drain puts it at the window's end -- so a join that *opens* on
/// this source opens after everything that happened in the same window as the
/// first run. Finding 44 has the argument; `mem.reclaim` and
/// `sched.runqlat` carried it first.
///
/// Written once, where the row is created, which is that key's first run of
/// the window: the rows map is drained and emptied every window.
const VALUE_FIRST_NS: i16 = 16;
const ROW_VALUE_BYTES: usize = 24;

/// One `(cpu, vector, bucket)` row's handlers over one drain interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftIrq {
    pub cpu: u32,
    /// The kernel's own vector number, as `/proc/softirqs` orders its rows.
    pub vec: u32,
    /// The log2 bucket, in `emit::BucketLayout::Log2`'s numbering.
    pub bucket: u32,
    /// How many handler runs landed in this bucket.
    pub count: u64,
    /// Their exact total, which the bucket bounds cannot give back.
    pub total_ns: u64,
    /// When the first of this row's window began, which is what the event is
    /// timestamped with. Zero means unknown, and falls back to the drain.
    pub first_ns: u64,
}

impl SoftIrq {
    /// The low bound of this row's bucket, which is the value that travels on
    /// the event.
    pub fn bucket_low_ns(&self) -> u64 {
        bucket_low_ns(self.bucket)
    }

    fn decode(key: &[u8], value: &[u8]) -> Option<Self> {
        if key.len() < ROW_KEY_BYTES || value.len() < ROW_VALUE_BYTES {
            return None;
        }
        let word =
            |bytes: &[u8], at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        let long =
            |bytes: &[u8], at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("8"));
        Some(Self {
            cpu: word(key, KEY_CPU as usize),
            vec: word(key, KEY_VEC as usize),
            bucket: word(key, KEY_BUCKET as usize),
            count: long(value, VALUE_COUNT as usize),
            total_ns: long(value, VALUE_TOTAL_NS as usize),
            first_ns: long(value, VALUE_FIRST_NS as usize),
        })
    }
}

/// Emits the comparison that keeps only the vectors this run was asked for.
///
/// Control reaches `keep` for a wanted vector and `skip` for every other, and
/// a run given no vectors keeps nothing -- which is a refusal the caller makes
/// before loading, not a program that silently measures the whole machine's
/// softirq time under a network source's name.
fn only_these_vectors(
    program: &mut Program,
    vectors: &[u32],
    vec_reg: u8,
    keep: &'static str,
    skip: &'static str,
) {
    for vector in vectors {
        program.jump_to(keep, Insn::jeq_imm(vec_reg, *vector as i32, 0));
    }
    program.jump_to(skip, Insn::ja());
    program.label(keep);
}

/// The program on `softirq_entry`: a handler is about to run on this cpu.
fn entry_instructions(btf: &Btf, vectors: &[u32], at_fd: i32) -> Result<Vec<Insn>, String> {
    expect_signature(btf, "softirq_entry", &[&SIGNATURE])?;

    let mut program = Program::new();
    // r6 keeps the context and r7 the vector number; both survive the helper
    // calls, which clobber r1 through r5.
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));
    only_these_vectors(&mut program, vectors, 7, "measure", "done");

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    // The slot is the vector, so the two network vectors cannot close each
    // other's intervals even though they run on the same cpu in the same pass
    // of `__do_softirq`.
    program.push(Insn::stx(BPF_W, R10, AT_KEY, 7));
    program.extend(Insn::load_map_fd(1, at_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, AT_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    // A per-cpu array of this size cannot miss, and a lookup that fails is not
    // a reason to stop: the interval is simply not timed.
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::stx(BPF_DW, 0, 0, 8));

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program on `softirq_exit`: the handler has finished, so the interval
/// that opened at `softirq_entry` can be closed and bucketed.
fn exit_instructions(
    btf: &Btf,
    vectors: &[u32],
    at_fd: i32,
    rows_fd: i32,
    counters_fd: i32,
) -> Result<Vec<Insn>, String> {
    expect_signature(btf, "softirq_exit", &[&SIGNATURE])?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));
    only_these_vectors(&mut program, vectors, 7, "measure", "done");

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    program.push(Insn::stx(BPF_W, R10, AT_KEY, 7));
    program.extend(Insn::load_map_fd(1, at_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, AT_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("done", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 9, 0, 0));
    // Cleared whatever happens next, and before the test below: an exit whose
    // entry this run never saw must not be closed by the *next* exit against a
    // timestamp that is already spent.
    program.push(Insn::st_imm(BPF_DW, 0, 0, 0));
    // Zero is not a time this clock returns, so it is how "no entry was
    // recorded" is written. It happens once per cpu per vector at attach --
    // the run started inside a handler -- which is a fact about the window
    // rather than a loss, and is counted apart from one.
    program.jump_to("unpaired", Insn::jeq_imm(9, 0, 0));

    // r9 = now - when the handler started, through r1, which nothing between
    // here and the next call reads.
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 9));
    program.push(Insn::mov64_reg(9, 1));

    // The cpu goes into the key before the bucket does: this call clobbers r1
    // through r5, and the bucket is computed in them.
    program.push(Insn::call(helper::GET_SMP_PROCESSOR_ID));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_CPU, 0));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_VEC, 7));
    push_log2_bucket(&mut program, 9);
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_BUCKET, 3));

    // The row map of this interval, taken once: a drain may swap it at any
    // instruction, and a lookup and an insert that named two maps would put
    // one handler in each. See `map::Rows`. r8 and r9 survive the lookup.
    select_rows(&mut program, rows_fd, ROWS_SLOT, "no_rows");
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("first", Insn::jeq_imm(0, 0, 0));
    // One allocation shared by every cpu, so both increments have to be ones
    // the other cpus cannot interleave with.
    program.push(Insn::mov64_imm(1, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_COUNT, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_TOTAL_NS, 9));
    program.jump_to("done", Insn::ja());

    program.label("first");
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_COUNT, 1));
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_TOTAL_NS, 9));
    // When this one began: now, less how long it lasted. r8 is the reading
    // taken at the top of the program and r9 is the duration, so the beginning
    // is recovered rather than carried through a second map field.
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 9));
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_FIRST_NS, 1));
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, ROW_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, counters_fd, DROP_KEY, DROP_ROWS, "done");
    // A lost row is counted once. Without this jump control would fall into
    // the block below and count the same handler as an unpaired exit as well,
    // which would report one loss as two different things.
    program.jump_to("done", Insn::ja());

    // The slot held no map, which it always does; counted rather than assumed.
    program.label("no_rows");
    count_drop(&mut program, counters_fd, DROP_KEY, DROP_ROWS, "done");
    program.jump_to("done", Insn::ja());

    program.label("unpaired");
    count_drop(&mut program, counters_fd, DROP_KEY, UNPAIRED_EXITS, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// Two loaded, attached programs and the maps they share.
///
/// Dropping it detaches both: the links, the programs and the maps are file
/// descriptors, and the kernel unloads a program when the last one closes.
pub struct SoftIrqProgram {
    rows: Rows,
    counters: Counters,
    _at: OwnedFd,
    entry_program: OwnedFd,
    _entry_link: OwnedFd,
    exit_program: OwnedFd,
    _exit_link: OwnedFd,
}

/// What one drain found, beside the measurements.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SoftIrqDrain {
    pub rows: Vec<SoftIrq>,
    /// Rows the walk returned and the read no longer found.
    pub vanished: u64,
    /// The walk stopped at the map's capacity rather than at its end.
    pub truncated: bool,
}

impl SoftIrqProgram {
    /// Loads and attaches both programs, or says why the kernel refused.
    ///
    /// `vectors` are the softirq vector numbers to measure, which the caller
    /// read out of the running kernel. An empty list is a bug in the caller
    /// rather than a request for the whole machine, and is refused here.
    pub fn load(vectors: &[u32]) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        if vectors.is_empty() {
            return Err(
                "no softirq vector was named, and a network source measuring every vector would \
                 be reporting timer and block time under its own name"
                    .to_owned(),
            );
        }
        if let Some(beyond) = vectors.iter().find(|vector| **vector >= VECTORS) {
            return Err(format!(
                "softirq vector {beyond} is beyond the {VECTORS} this program holds state for; \
                 this kernel has more vectors than it was written against"
            ));
        }

        // Indexed by vector number, one element per cpu, holding the moment a
        // handler started. A per-cpu array rather than a hash because there is
        // at most one open handler per (cpu, vector) and no atomic is needed.
        let at = syscall::map_create(BPF_MAP_TYPE_PERCPU_ARRAY, 4, 8, VECTORS, "omn_sirq_at")
            .map_err(|error| map_failure("softirq entry map", error))?;
        let rows = Rows::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, "omn_sirq_hist")
            .map_err(|error| map_failure("softirq histogram map", error))?;
        let counters = Counters::create(COUNTER_SLOTS, "omn_sirq_lost")
            .map_err(|error| map_failure("softirq counters", error))?;

        let entry = entry_instructions(btf, vectors, at.as_raw_fd())?;
        let exit = exit_instructions(
            btf,
            vectors,
            at.as_raw_fd(),
            rows.as_raw_fd(),
            counters.as_raw_fd(),
        )?;
        let entry_program = load(&entry, "omn_sirq_in")?;
        let exit_program = load(&exit, "omn_sirq_out")?;

        // The entry program is attached first, on purpose. An interval opened
        // by a program whose partner is not attached yet is simply never
        // closed and is overwritten by the next entry; an exit seen before any
        // entry was recorded would close an interval nobody opened, and the
        // zero test is the only thing standing between that and a duration
        // measured from an uninitialised slot.
        let entry_link = crate::bpf::program::attach_raw("softirq_entry", &entry_program)?;
        let exit_link = crate::bpf::program::attach_raw("softirq_exit", &exit_program)?;

        Ok(Self {
            rows,
            counters,
            _at: at,
            entry_program,
            _entry_link: entry_link,
            exit_program,
            _exit_link: exit_link,
        })
    }

    /// Takes every row the kernel has accumulated since the last drain.
    ///
    /// Each row is a delta over the interval between two drains: rows are
    /// removed as they are read, so what comes back next time is what happened
    /// in between.
    pub fn drain(&self) -> Result<SoftIrqDrain, String> {
        let drained = self
            .rows
            .drain()
            .map_err(|error| format!("cannot read the softirq histogram: {error}"))?;
        let mut result = SoftIrqDrain {
            rows: Vec::with_capacity(drained.entries.len()),
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        for (key, value) in &drained.entries {
            match SoftIrq::decode(key, value) {
                Some(row) => result.rows.push(row),
                // A record the wrong size means this reader and the program
                // disagree about the layout, which is a bug rather than a
                // machine condition -- but counting it beats reinterpreting it.
                None => result.vanished += 1,
            }
        }
        Ok(result)
    }

    /// Runs of these programs the kernel skipped because one was already
    /// running on that cpu; see `BlockProgram::skipped`, where it was measured.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped([&self.entry_program, &self.exit_program])
    }

    /// Handler runs the histogram could not hold, which are lost.
    pub fn dropped(&self) -> u64 {
        self.counters.read(DROP_ROWS)
    }

    /// Handler exits whose entry this run never saw, which are not lost: they
    /// are the handlers that were already running when the programs attached.
    pub fn unpaired(&self) -> u64 {
        self.counters.read(UNPAIRED_EXITS)
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

    /// The program writes fields at stack offsets and `decode` reads them at
    /// byte offsets in the drained bytes. Nothing but this connects the two.
    #[test]
    fn the_row_the_program_writes_is_the_row_the_reader_decodes() {
        let mut key = vec![0u8; ROW_KEY_BYTES];
        key[KEY_CPU as usize..][..4].copy_from_slice(&7u32.to_ne_bytes());
        key[KEY_VEC as usize..][..4].copy_from_slice(&3u32.to_ne_bytes());
        key[KEY_BUCKET as usize..][..4].copy_from_slice(&11u32.to_ne_bytes());
        let mut value = vec![0u8; ROW_VALUE_BYTES];
        value[VALUE_COUNT as usize..][..8].copy_from_slice(&42u64.to_ne_bytes());
        value[VALUE_TOTAL_NS as usize..][..8].copy_from_slice(&55_000u64.to_ne_bytes());

        let row = SoftIrq::decode(&key, &value).expect("a full row");
        assert_eq!(
            row,
            SoftIrq {
                cpu: 7,
                vec: 3,
                bucket: 11,
                count: 42,
                total_ns: 55_000,
                first_ns: 0,
            }
        );
        assert_eq!(row.bucket_low_ns(), 1024, "bucket 11 starts at 2^10");
    }

    #[test]
    fn a_row_shorter_than_the_layout_is_refused_rather_than_padded() {
        assert!(SoftIrq::decode(&[0u8; 4], &[0u8; 16]).is_none());
        assert!(SoftIrq::decode(&[0u8; 12], &[0u8; 8]).is_none());
    }

    /// Both programs are built without any privilege, so this checks their
    /// shape even on a machine that would refuse to load them.
    #[test]
    fn both_programs_are_built_against_the_running_kernel() {
        let Some(btf) = kernel_btf() else { return };
        let entry = entry_instructions(btf, &[2, 3], 3).expect("an entry program");
        let exit = exit_instructions(btf, &[2, 3], 3, 4, 5).expect("an exit program");
        assert_eq!(entry.last().copied(), Some(Insn::exit()));
        assert_eq!(exit.last().copied(), Some(Insn::exit()));
        assert!(
            exit.len() > entry.len(),
            "closing an interval and bucketing it is the longer half"
        );
    }

    /// Invariant 6, checked in the instruction stream rather than trusted.
    ///
    /// `bpf_ktime_get_boot_ns` includes suspend time. A program that called it
    /// would put its durations in a different domain from every other
    /// collector, every join across them would be wrong, and nothing anywhere
    /// would report an error.
    #[test]
    fn the_programs_read_the_monotonic_clock_and_no_other() {
        let Some(btf) = kernel_btf() else { return };
        const KTIME_GET_BOOT_NS: i32 = 125;
        for program in [
            entry_instructions(btf, &[3], 3).expect("an entry program"),
            exit_instructions(btf, &[3], 3, 4, 5).expect("an exit program"),
        ] {
            let called: Vec<i32> = program
                .iter()
                .filter(|insn| insn.code == 0x85)
                .map(|insn| insn.imm)
                .collect();
            assert!(called.contains(&helper::KTIME_GET_NS));
            assert!(
                !called.contains(&KTIME_GET_BOOT_NS),
                "boot time includes suspend, and mixing clock domains breaks every join silently"
            );
        }
    }

    /// The vector filter is in the program, which is what makes the source a
    /// network source rather than a softirq source: one comparison per vector
    /// the run asked for, and every other vector never reaches the map.
    #[test]
    fn the_vectors_the_run_asked_for_are_compared_in_the_program() {
        let Some(btf) = kernel_btf() else { return };
        // A file descriptor number that cannot be mistaken for a vector: the
        // map descriptor is written into the instruction stream as an
        // immediate too, and 3 is both a plausible fd and NET_RX.
        let one = entry_instructions(btf, &[3], 77).expect("a program");
        let two = entry_instructions(btf, &[2, 3], 77).expect("a program");
        assert_eq!(
            one.iter().filter(|insn| insn.imm == 3).count(),
            1,
            "the one vector is compared once"
        );
        assert_eq!(
            two.len(),
            one.len() + 1,
            "a second vector is one more comparison and nothing else"
        );
        assert!(
            !one.iter().any(|insn| insn.imm == 2),
            "a run that asked for NET_RX must not also measure NET_TX"
        );
    }

    /// A vector number this program keeps no state for is refused by name. The
    /// alternative is a write past the end of the per-cpu array, which the
    /// kernel refuses at run time and which would silently lose every handler
    /// on that vector.
    #[test]
    fn a_vector_beyond_the_state_this_program_holds_is_refused() {
        let Err(error) = SoftIrqProgram::load(&[VECTORS]) else {
            panic!("a vector beyond the array must be refused");
        };
        assert!(error.contains("beyond"), "{error}");
    }

    /// Measuring every vector under a network source's name would be a wrong
    /// number with nothing saying so, so an empty list is a refusal.
    #[test]
    fn no_vector_at_all_is_refused_rather_than_meaning_all_of_them() {
        let Err(error) = SoftIrqProgram::load(&[]) else {
            panic!("an empty vector list must be refused");
        };
        assert!(error.contains("timer and block time"), "{error}");
    }

    /// The stack regions this program writes must not overlap, and must stay
    /// inside the frame.
    ///
    /// They are addressed by offset arithmetic, and two that overlap produce a
    /// key built partly out of a value -- which hashes to a bucket nobody looks
    /// in, so the numbers are simply short, or comes back as a field with a
    /// plausible-looking number in it that was never written there.
    ///
    /// This file had no such test until the eighth wave, and that is exactly
    /// where the defect landed: a field added to the row value grew it into the
    /// row key, and what caught it was another stream's scenario reporting
    /// `vector 1760` for a softirq vector that only goes up to nine. Every
    /// program that keeps a row now has this test.
    #[test]
    fn the_stack_regions_do_not_overlap_and_stay_inside_the_frame() {
        let regions = [
            (ROW_VALUE, ROW_VALUE_BYTES as i16, "row value"),
            (ROW_KEY, ROW_KEY_BYTES as i16, "row key"),
            (AT_KEY, 4, "at key"),
            (DROP_KEY, 4, "drop key"),
            (ROWS_SLOT, 8, "row map slot"),
        ];
        for (at, size, name) in regions {
            assert!(at >= -512 && at + size <= 0, "`{name}` is off the frame");
            for (other_at, other_size, other) in regions {
                if name == other {
                    continue;
                }
                assert!(
                    at + size <= other_at || other_at + other_size <= at,
                    "`{name}` overlaps `{other}`"
                );
            }
        }
    }

    /// The row map is found through the generation slot once per run, and the
    /// lookup and the insert both use what it found -- the property that makes
    /// the drain's swap safe; see `map::Rows`.
    #[test]
    fn the_row_map_is_taken_from_the_slot_once_per_run() {
        let Some(btf) = kernel_btf() else { return };
        let program = exit_instructions(btf, &[2, 3], 3, 4, 5).expect("an exit program");
        crate::bpf::map::assert_rows_taken_once(&program, 4);
    }

    /// Every byte of the row value is written by the program before the value
    /// is handed to a helper. A byte nobody wrote is whatever the stack held.
    #[test]
    fn every_byte_of_the_row_value_is_accounted_for() {
        let fields = [(VALUE_COUNT, 8), (VALUE_TOTAL_NS, 8), (VALUE_FIRST_NS, 8)];
        let mut covered = [false; ROW_VALUE_BYTES];
        for (at, size) in fields {
            for byte in at..at + size {
                let byte = byte as usize;
                assert!(
                    byte < ROW_VALUE_BYTES,
                    "the row value is written past its end"
                );
                assert!(!covered[byte], "the row value writes byte {byte} twice");
                covered[byte] = true;
            }
        }
        assert!(
            covered.iter().all(|byte| *byte),
            "the row value has a byte the program never writes"
        );
    }
}
