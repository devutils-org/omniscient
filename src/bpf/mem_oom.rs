// SPDX-License-Identifier: MIT OR GPL-2.0
//! The OOM killer's victims, from `oom:mark_victim`.
//!
//! `/proc/vmstat` and `memory.events` count kills; they never say who died.
//! The kernel decides that in `__oom_kill_process` and announces it on
//! `mark_victim`, before the `SIGKILL` is delivered, while the victim still
//! holds its memory. So this program reads, out of the task it is handed, what
//! the tracepoint's own format prints: the task, its uid, its `oom_score_adj`,
//! and the pages its address space held, by kind -- which is what a reader of
//! a kill wants first, "what did it have".
//!
//! The page counts are `mm->rss_stat[]`'s, read as `get_mm_counter` reads
//! them: the shared counter, without folding in each cpu's unflushed part.
//! That is the tracepoint's own number; it can lag the exact one by a few
//! pages per cpu.
//!
//! One record per victim in a ring, the streaming shape of
//! `proc_events.rs`, whose loader and drop counter this reuses: a kill is
//! rare and each one matters on its own.

use super::btf::Btf;
use super::insn::{
    helper, push_task_filter, read_field, read_narrow, read_pointer, Insn, Program, Subject,
    TaskFields, Whose, BPF_DW, BPF_W, R10,
};
use super::proc_events::{publish, EventStream};

/// `mark_victim(task_struct *, uid_t)` since 6.8; the uid was added then, and
/// before it the tracepoint was handed the task alone.
pub(super) const SIGNATURE: [&str; 2] = ["task_struct *", "uid_t"];
pub(super) const SIGNATURE_WITHOUT_UID: [&str; 1] = ["task_struct *"];

const COMM_BYTES: usize = 16;

/// `uid` on a kernel whose tracepoint does not pass it.
pub const NO_UID: u32 = u32::MAX;

/// The order of `mm->rss_stat[]`, `MM_FILEPAGES` to `MM_SHMEMPAGES`, unchanged
/// since the array was introduced.
const MM_FILEPAGES: i32 = 0;
const MM_ANONPAGES: i32 = 1;
const MM_SWAPENTS: i32 = 2;
const MM_SHMEMPAGES: i32 = 3;

/// One victim, as the program writes it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Victim {
    pub ts_ns: u64,
    pub tid: u32,
    pub tgid: u32,
    /// [`NO_UID`] where the tracepoint does not pass it.
    pub uid: u32,
    /// `signal->oom_score_adj`, a `short`, in the low half.
    pub(crate) adj: u32,
    pub(crate) comm: [u8; COMM_BYTES],
    /// `mm->total_vm`, in pages.
    pub total_vm: u64,
    /// `mm->rss_stat[]`, in pages.
    pub file_rss: u64,
    pub anon_rss: u64,
    pub swap_ents: u64,
    pub shmem_rss: u64,
    pub cpu: u32,
    pub(crate) pad: u32,
}

const VICTIM_BYTES: usize = 88;

impl Victim {
    pub fn comm(&self) -> String {
        super::sched_common::text_of(&self.comm)
    }

    /// The adjustment the victim's score carried, -1000 to 1000.
    pub fn oom_score_adj(&self) -> i32 {
        i32::from(self.adj as u16 as i16)
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < VICTIM_BYTES {
            return None;
        }
        let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        let long = |at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("8"));
        let mut comm = [0u8; COMM_BYTES];
        comm.copy_from_slice(&bytes[24..40]);
        Some(Self {
            ts_ns: long(0),
            tid: word(8),
            tgid: word(12),
            uid: word(16),
            adj: word(20),
            comm,
            total_vm: long(40),
            file_rss: long(48),
            anon_rss: long(56),
            swap_ents: long(64),
            shmem_rss: long(72),
            cpu: word(80),
            pad: 0,
        })
    }
}

const EVENT: i16 = -88;
const TID: i16 = EVENT + 8;
const TGID: i16 = EVENT + 12;
const UID: i16 = EVENT + 16;
const ADJ: i16 = EVENT + 20;
const COMM: i16 = EVENT + 24;
const TOTAL_VM: i16 = EVENT + 40;
const RSS: i16 = EVENT + 48;
const CPU: i16 = EVENT + 80;
const PAD: i16 = EVENT + 84;
const DROP_KEY: i16 = EVENT - 8;
const FILTER: i16 = EVENT - 24;
const SCRATCH: i16 = EVENT - 32;

/// Where one of `mm->rss_stat[]`'s counters is, from the start of the
/// `mm_struct`.
///
/// Two layouts: since 6.2 an array of `percpu_counter`, whose `count` is the
/// shared part and whose size depends on the configuration; before it a
/// `struct mm_rss_stat` holding an array of `atomic_long_t`.
fn rss_counters(btf: &Btf) -> Result<[i32; 4], String> {
    let base = btf.member_offset("mm_struct", "rss_stat")?;
    let (first, stride) = if btf.has_type("mm_rss_stat") {
        (base + btf.member_offset("mm_rss_stat", "count")?, 8)
    } else {
        (
            base + btf.member_offset("percpu_counter", "count")?,
            btf.struct_size("percpu_counter")?,
        )
    };
    Ok([MM_FILEPAGES, MM_ANONPAGES, MM_SWAPENTS, MM_SHMEMPAGES].map(|slot| first + slot * stride))
}

fn instructions(btf: &Btf, whose: Whose, ring_fd: i32, drops_fd: i32) -> Result<Vec<Insn>, String> {
    let signature = btf.tracepoint_signature("mark_victim")?;
    let passes_uid = if signature == SIGNATURE {
        true
    } else if signature == SIGNATURE_WITHOUT_UID {
        false
    } else {
        return Err(format!(
            "this kernel declares `mark_victim({})`, and this program is written for \
             `mark_victim({})` or `mark_victim({})`: it reads its arguments out of fixed slots, \
             and against another prototype those slots hold something else",
            signature.join(", "),
            SIGNATURE.join(", "),
            SIGNATURE_WITHOUT_UID.join(", "),
        ));
    };
    let fields = TaskFields::locate(btf, whose)?;
    let pid_offset = btf.member_offset("task_struct", "pid")?;
    let tgid_offset = btf.member_offset("task_struct", "tgid")?;
    let comm_offset = btf.member_offset("task_struct", "comm")?;
    let mm_offset = btf.member_offset("task_struct", "mm")?;
    let signal_offset = btf.member_offset("task_struct", "signal")?;
    let adj_offset = btf.member_offset("signal_struct", "oom_score_adj")?;
    let total_vm_offset = btf.member_offset("mm_struct", "total_vm")?;
    let counters = rss_counters(btf)?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, 0));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, EVENT, 0));
    program.push(Insn::call(helper::GET_SMP_PROCESSOR_ID));
    program.push(Insn::stx(BPF_W, R10, CPU, 0));
    program.push(Insn::st_imm(BPF_W, R10, PAD, 0));

    read_field(&mut program, TID, 4, 7, pid_offset);
    read_field(&mut program, TGID, 4, 7, tgid_offset);
    push_task_filter(
        &mut program,
        &fields,
        whose,
        Subject::with_ids(7, TID, TGID),
        FILTER,
        "done",
    );
    if passes_uid {
        program.push(Insn::ldx(BPF_DW, 1, 6, 8));
        program.push(Insn::stx(BPF_W, R10, UID, 1));
    } else {
        program.push(Insn::st_imm(BPF_W, R10, UID, NO_UID as i32));
    }
    read_field(&mut program, COMM, COMM_BYTES as i32, 7, comm_offset);

    // `signal->oom_score_adj`; a null `signal` leaves it zero.
    program.push(Insn::st_imm(BPF_W, R10, ADJ, 0));
    read_pointer(&mut program, SCRATCH, 8, 7, signal_offset);
    program.jump_to("adj_read", Insn::jeq_imm(8, 0, 0));
    read_narrow(&mut program, ADJ, 2, 8, adj_offset);
    program.label("adj_read");

    // What the address space held. A task with no `mm` is a kernel thread,
    // which the OOM killer never picks; it is recorded holding nothing rather
    // than skipped, so a kill is never lost for want of its sizes.
    program.push(Insn::st_imm(BPF_DW, R10, TOTAL_VM, 0));
    for slot in 0..4 {
        program.push(Insn::st_imm(BPF_DW, R10, RSS + 8 * slot, 0));
    }
    read_pointer(&mut program, SCRATCH, 8, 7, mm_offset);
    program.jump_to("sizes_read", Insn::jeq_imm(8, 0, 0));
    read_field(&mut program, TOTAL_VM, 8, 8, total_vm_offset);
    for (slot, offset) in counters.iter().enumerate() {
        read_field(&mut program, RSS + 8 * slot as i16, 8, 8, *offset);
    }
    program.label("sizes_read");

    publish(
        &mut program,
        ring_fd,
        drops_fd,
        EVENT,
        VICTIM_BYTES,
        DROP_KEY,
    );

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The `mark_victim` stream.
pub struct VictimProgram(EventStream);

impl VictimProgram {
    /// See `EventStream::skipped`.
    pub fn skipped(&self) -> u64 {
        self.0.skipped()
    }

    pub fn load(whose: Whose, requested_ring_bytes: usize) -> Result<Self, String> {
        EventStream::load(
            "mark_victim",
            "omn_oom_victim",
            requested_ring_bytes,
            move |btf, ring, drops| instructions(btf, whose, ring, drops),
        )
        .map(Self)
    }

    /// Whether this kernel's tracepoint passes the victim's uid.
    pub fn uid_is_passed() -> bool {
        super::kernel_btf()
            .and_then(|btf| btf.tracepoint_signature("mark_victim"))
            .is_ok_and(|signature| signature == SIGNATURE)
    }

    pub fn drain(&mut self, handle: impl FnMut(Victim), undecoded: &mut u64) {
        self.0.drain(Victim::from_bytes, handle, undecoded);
    }

    pub fn dropped(&self) -> u64 {
        self.0.dropped()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel_btf() -> Option<Btf> {
        match Btf::from_kernel() {
            Ok(btf) => Some(btf),
            Err(reason) => {
                eprintln!("skipped: {reason}");
                None
            }
        }
    }

    #[test]
    fn the_record_is_the_size_the_program_writes() {
        assert_eq!(std::mem::size_of::<Victim>(), VICTIM_BYTES);
    }

    #[test]
    fn a_record_decodes_with_its_score_adjustment_signed() {
        let mut bytes = vec![0u8; VICTIM_BYTES];
        bytes[0..8].copy_from_slice(&7u64.to_ne_bytes());
        bytes[8..12].copy_from_slice(&41u32.to_ne_bytes());
        bytes[12..16].copy_from_slice(&40u32.to_ne_bytes());
        bytes[16..20].copy_from_slice(&1000u32.to_ne_bytes());
        bytes[20..24].copy_from_slice(&u32::from((-500i16) as u16).to_ne_bytes());
        bytes[24..28].copy_from_slice(b"hog\0");
        bytes[56..64].copy_from_slice(&262_144u64.to_ne_bytes());
        let victim = Victim::from_bytes(&bytes).expect("a record");
        assert_eq!(victim.tgid, 40);
        assert_eq!(victim.oom_score_adj(), -500);
        assert_eq!(victim.comm(), "hog");
        assert_eq!(victim.anon_rss, 262_144);
        assert!(Victim::from_bytes(&bytes[..VICTIM_BYTES - 1]).is_none());
    }

    /// The program builds against the running kernel, reads the four counters
    /// at distinct offsets, and compares a target before it reads anything
    /// the record carries.
    #[test]
    fn the_program_builds_against_this_kernel() {
        let Some(btf) = kernel_btf() else { return };
        let counters = rss_counters(&btf).expect("the counters");
        assert!(
            counters.windows(2).all(|pair| pair[0] < pair[1]),
            "{counters:?}"
        );
        let program = instructions(&btf, Whose::Process(4242), 3, 4).expect("a program");
        let compared = program
            .iter()
            .position(|insn| insn.imm == 4242)
            .expect("the comparison");
        let published = program
            .iter()
            .position(|insn| insn.code == 0x85 && insn.imm == helper::RINGBUF_OUTPUT)
            .expect("the publish");
        assert!(compared < published);
        let machine = instructions(&btf, Whose::Any, 3, 4).expect("a program");
        assert!(!machine.iter().any(|insn| insn.imm == 4242));
    }
}
