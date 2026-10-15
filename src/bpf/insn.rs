// SPDX-License-Identifier: MIT OR GPL-2.0
//! BPF instructions, built in Rust.
//!
//! A program here is an array of 64-bit instructions handed to the verifier,
//! not a C file compiled by clang at build time. That keeps `omn` a single
//! binary with no build-time toolchain, and it is what makes the field offsets
//! in `btf` usable: they are read from the running kernel and written into the
//! instruction stream at load time, which is the same relocation a
//! compile-time CO-RE build does, performed a few milliseconds later.
//!
//! The encoding is the one in `include/uapi/linux/bpf_common.h`: an 8-bit
//! opcode, two 4-bit registers packed into one byte, a 16-bit offset and a
//! 32-bit immediate.

/// One instruction. The layout is the kernel's `struct bpf_insn`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Insn {
    pub code: u8,
    /// Destination in the low nibble, source in the high one.
    pub regs: u8,
    pub off: i16,
    pub imm: i32,
}

/// Instruction classes.
const BPF_LD: u8 = 0x00;
const BPF_LDX: u8 = 0x01;
const BPF_ST: u8 = 0x02;
const BPF_STX: u8 = 0x03;
const BPF_JMP: u8 = 0x05;
/// The 32-bit comparison class. Nothing here emits one yet; the audit knows it
/// because a jump of either class has to land inside the program.
const BPF_JMP32: u8 = 0x06;
const BPF_ALU64: u8 = 0x07;

/// Operand widths.
pub const BPF_W: u8 = 0x00;
/// The half-word width: a typed load has to be exactly the member's width, and
/// `family`, `dport` and `i_mode` are two bytes.
pub const BPF_H: u8 = 0x08;
pub const BPF_DW: u8 = 0x18;

/// Addressing modes.
const BPF_IMM: u8 = 0x00;
const BPF_MEM: u8 = 0x60;
/// A read-modify-write the other cpus cannot interleave with.
const BPF_ATOMIC: u8 = 0xc0;

/// Where the second operand comes from.
const BPF_K: u8 = 0x00;
const BPF_X: u8 = 0x08;

/// ALU operations.
const BPF_ADD: u8 = 0x00;
const BPF_SUB: u8 = 0x10;
const BPF_OR: u8 = 0x40;
const BPF_AND: u8 = 0x50;
const BPF_LSH: u8 = 0x60;
const BPF_RSH: u8 = 0x70;
const BPF_ARSH: u8 = 0xc0;
const BPF_MOV: u8 = 0xb0;

/// Jump operations.
/// `BPF_JA` is zero, which is why an instruction whose jump operation was
/// never filled in reads as an unconditional jump rather than as nonsense.
const BPF_JA: u8 = 0x00;
const BPF_JEQ: u8 = 0x10;
const BPF_JGT: u8 = 0x20;
const BPF_JGE: u8 = 0x30;
const BPF_JSET: u8 = 0x40;
const BPF_JNE: u8 = 0x50;
const BPF_JSGE: u8 = 0x70;
const BPF_CALL: u8 = 0x80;
const BPF_EXIT: u8 = 0x90;

/// `src_reg` value that marks the 64-bit immediate as a map file descriptor,
/// which the kernel replaces with the map's address when the program loads.
const BPF_PSEUDO_MAP_FD: u8 = 1;
/// The source register of a 64-bit load the kernel resolves to the address of
/// the kernel variable whose BTF id is in the immediate.
pub const BPF_PSEUDO_BTF_ID: u8 = 3;

/// The helpers this layer calls, by their number in `bpf_func_id`.
pub mod helper {
    pub const MAP_LOOKUP_ELEM: i32 = 1;
    pub const MAP_UPDATE_ELEM: i32 = 2;
    pub const MAP_DELETE_ELEM: i32 = 3;
    pub const KTIME_GET_NS: i32 = 5;
    pub const GET_SMP_PROCESSOR_ID: i32 = 8;
    pub const GET_CURRENT_PID_TGID: i32 = 14;
    /// `bpf_get_current_comm(buf, size)`.
    ///
    /// For a program attached to something that is not a scheduler tracepoint:
    /// a block request, a `sys_exit` or a socket hands over no task at all, so
    /// the caller's name cannot be read out of a `task_struct` the way the
    /// scheduler programs read it. Resolving it from `/proc/<tid>/comm` at
    /// drain time answers for whoever holds that tid *then*, which for a
    /// short-lived task is somebody else.
    pub const GET_CURRENT_COMM: i32 = 16;
    pub const GET_STACKID: i32 = 27;
    /// `bpf_get_socket_cookie`. For a tracing program it takes a
    /// `struct sock *` and generates the cookie if nothing has yet, which is
    /// what makes it a key rather than a zero.
    pub const GET_SOCKET_COOKIE: i32 = 46;
    /// `bpf_probe_read_user`, for an argument a probe left in user memory.
    pub const PROBE_READ_USER: i32 = 112;
    pub const PROBE_READ_KERNEL: i32 = 113;
    /// `bpf_probe_read_kernel_str(dst, size, unsafe_ptr)`.
    ///
    /// Not interchangeable with `PROBE_READ_KERNEL`: it stops at the
    /// terminator, where a fixed span over a `const char *` reads past the end
    /// of whatever allocation holds the name. It does not zero the rest of the
    /// destination, so the slot has to be cleared first -- otherwise the map
    /// carries this program's own stack contents to userspace.
    pub const PROBE_READ_KERNEL_STR: i32 = 115;
    pub const RINGBUF_OUTPUT: i32 = 130;
    /// `bpf_seq_write(seq, data, len)`: an iterator program's output.
    pub const SEQ_WRITE: i32 = 127;
    /// `bpf_task_storage_get(map, task, value, flags)`.
    pub const TASK_STORAGE_GET: i32 = 156;
    /// `bpf_task_storage_delete(map, task)`.
    pub const TASK_STORAGE_DELETE: i32 = 157;
    /// `bpf_get_current_task_btf()`: the current task as a pointer whose type
    /// the verifier knows, which is what the storage helpers take.
    pub const GET_CURRENT_TASK_BTF: i32 = 158;
    /// `bpf_task_pt_regs(task)` (5.15): the registers `task` entered the
    /// kernel with, its user ones for a user task.
    pub const TASK_PT_REGS: i32 = 175;
    /// `bpf_this_cpu_ptr(percpu_ptr)`: this cpu's copy of a per-cpu kernel
    /// variable, whose address a [`Insn::load_btf_id`] put in `r1`.
    pub const THIS_CPU_PTR: i32 = 154;
    /// `u64 bpf_get_current_ancestor_cgroup_id(int ancestor_level)`: the id of
    /// the current task's cgroup's ancestor at that level of the unified
    /// hierarchy, or zero when the task's cgroup is not that deep. The one call
    /// that answers "is the current task inside this cgroup or below it"; see
    /// [`super::Whose`].
    pub const GET_CURRENT_ANCESTOR_CGROUP_ID: i32 = 123;
}

/// The frame pointer. Read-only, and the only way to address the stack.
pub const R10: u8 = 10;

impl Insn {
    fn new(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> Self {
        Self {
            code,
            regs: (dst & 0xf) | ((src & 0xf) << 4),
            off,
            imm,
        }
    }

    /// `dst = imm`
    pub fn mov64_imm(dst: u8, imm: i32) -> Self {
        Self::new(BPF_ALU64 | BPF_MOV | BPF_K, dst, 0, 0, imm)
    }

    /// `dst = src`
    pub fn mov64_reg(dst: u8, src: u8) -> Self {
        Self::new(BPF_ALU64 | BPF_MOV | BPF_X, dst, src, 0, 0)
    }

    /// `dst += imm`
    pub fn add64_imm(dst: u8, imm: i32) -> Self {
        Self::new(BPF_ALU64 | BPF_ADD | BPF_K, dst, 0, 0, imm)
    }

    /// `dst += src`
    pub fn add64_reg(dst: u8, src: u8) -> Self {
        Self::new(BPF_ALU64 | BPF_ADD | BPF_X, dst, src, 0, 0)
    }

    /// `dst <<= imm`
    pub fn lsh64_imm(dst: u8, imm: i32) -> Self {
        Self::new(BPF_ALU64 | BPF_LSH | BPF_K, dst, 0, 0, imm)
    }

    /// `dst -= src`
    pub fn sub64_reg(dst: u8, src: u8) -> Self {
        Self::new(BPF_ALU64 | BPF_SUB | BPF_X, dst, src, 0, 0)
    }

    /// `dst &= imm`
    ///
    /// Masking flag bits off a value the way the kernel does. The four-byte
    /// load trick -- write a register to the stack and read it back with
    /// `BPF_W` -- truncates to 32 bits and nothing narrower, so without this a
    /// program strips two flag bits by conditional subtraction: correct, and
    /// eight instructions where two do.
    pub fn and64_imm(dst: u8, imm: i32) -> Self {
        Self::new(BPF_ALU64 | BPF_AND | BPF_K, dst, 0, 0, imm)
    }

    /// `dst >>= imm`, unsigned.
    pub fn rsh64_imm(dst: u8, imm: i32) -> Self {
        Self::new(BPF_ALU64 | BPF_RSH | BPF_K, dst, 0, 0, imm)
    }

    /// `dst >>= imm`, keeping the sign.
    pub fn arsh64_imm(dst: u8, imm: i32) -> Self {
        Self::new(BPF_ALU64 | BPF_ARSH | BPF_K, dst, 0, 0, imm)
    }

    /// `dst |= imm`
    pub fn or64_imm(dst: u8, imm: i32) -> Self {
        Self::new(BPF_ALU64 | BPF_OR | BPF_K, dst, 0, 0, imm)
    }

    /// `dst = *(size *)(src + off)`
    pub fn ldx(size: u8, dst: u8, src: u8, off: i16) -> Self {
        Self::new(BPF_LDX | size | BPF_MEM, dst, src, off, 0)
    }

    /// `*(size *)(dst + off) = src`
    pub fn stx(size: u8, dst: u8, off: i16, src: u8) -> Self {
        Self::new(BPF_STX | size | BPF_MEM, dst, src, off, 0)
    }

    /// `*(size *)(dst + off) = imm`
    pub fn st_imm(size: u8, dst: u8, off: i16, imm: i32) -> Self {
        Self::new(BPF_ST | size | BPF_MEM, dst, 0, off, imm)
    }

    /// `lock *(size *)(dst + off) += src`
    ///
    /// The increment an aggregating program needs. A map value in a
    /// `BPF_MAP_TYPE_HASH` is one allocation shared by every cpu, so a plain
    /// load-add-store loses whichever increment lands between the two halves
    /// of another cpu's -- which shows up as an off-cpu total that is quietly
    /// short on exactly the busy workloads the aggregation exists for. The
    /// per-cpu maps do not need this; the shared ones cannot do without it.
    pub fn atomic_add(size: u8, dst: u8, off: i16, src: u8) -> Self {
        Self::new(BPF_STX | size | BPF_ATOMIC, dst, src, off, BPF_ADD as i32)
    }

    /// `if dst == imm goto off`
    pub fn jeq_imm(dst: u8, imm: i32, off: i16) -> Self {
        Self::new(BPF_JMP | BPF_JEQ | BPF_K, dst, 0, off, imm)
    }

    /// `goto off`
    pub fn ja() -> Self {
        Self::new(BPF_JMP | BPF_JA, 0, 0, 0, 0)
    }

    /// `if dst & imm goto off`
    ///
    /// A task state is a bit set, not a number: `TASK_UNINTERRUPTIBLE` and
    /// `TASK_UNINTERRUPTIBLE | TASK_NOLOAD` are both a task off the cpu and
    /// are not equal to each other, so the question a program asks about one
    /// is which bits are set and never which value it is.
    pub fn jset_imm(dst: u8, imm: i32, off: i16) -> Self {
        Self::new(BPF_JMP | BPF_JSET | BPF_K, dst, 0, off, imm)
    }

    /// `if dst != imm goto off`
    /// `if dst > src goto +off`, unsigned: every operand these programs
    /// compare is a timestamp, a duration or an address.
    pub fn jgt_reg(dst: u8, src: u8, off: i16) -> Self {
        Self::new(BPF_JMP | BPF_JGT | BPF_X, dst, src, off, 0)
    }

    /// `if dst >= src goto +off`, unsigned.
    pub fn jge_reg(dst: u8, src: u8, off: i16) -> Self {
        Self::new(BPF_JMP | BPF_JGE | BPF_X, dst, src, off, 0)
    }

    /// `if dst != src goto +off`
    pub fn jne_reg(dst: u8, src: u8, off: i16) -> Self {
        Self::new(BPF_JMP | BPF_JNE | BPF_X, dst, src, off, 0)
    }

    pub fn jne_imm(dst: u8, imm: i32, off: i16) -> Self {
        Self::new(BPF_JMP | BPF_JNE | BPF_K, dst, 0, off, imm)
    }

    /// `if dst >= imm goto off`, signed: helper return values are negative
    /// errnos, so the comparison that separates success from failure is a
    /// signed one.
    pub fn jsge_imm(dst: u8, imm: i32, off: i16) -> Self {
        Self::new(BPF_JMP | BPF_JSGE | BPF_K, dst, 0, off, imm)
    }

    pub fn call(helper: i32) -> Self {
        Self::new(BPF_JMP | BPF_CALL, 0, 0, 0, helper)
    }

    pub fn exit() -> Self {
        Self::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0)
    }

    /// `dst = <map>`: a wide instruction the kernel rewrites into the map's
    /// address while it loads the program.
    pub fn load_map_fd(dst: u8, map_fd: i32) -> [Self; 2] {
        [
            Self::new(BPF_LD | BPF_DW | BPF_IMM, dst, BPF_PSEUDO_MAP_FD, 0, map_fd),
            Self::default(),
        ]
    }

    /// `dst = &<kernel variable>`, the variable named by its id in vmlinux's
    /// BTF: the kernel resolves it at load, so the program carries no address.
    /// The second slot's immediate is the BTF object's fd, and 0 is vmlinux.
    pub fn load_btf_id(dst: u8, btf_id: u32) -> [Self; 2] {
        [
            Self::new(
                BPF_LD | BPF_DW | BPF_IMM,
                dst,
                BPF_PSEUDO_BTF_ID,
                0,
                btf_id as i32,
            ),
            Self::default(),
        ]
    }

    /// `dst = value`, a full 64-bit constant, in the two slots the encoding
    /// takes: the low half in the first, the high half in the second.
    pub fn ld_imm64(dst: u8, value: i64) -> [Self; 2] {
        [
            Self::new(BPF_LD | BPF_DW | BPF_IMM, dst, 0, 0, value as i32),
            Self::new(0, 0, 0, 0, (value >> 32) as i32),
        ]
    }
}

/// Checks a program for the structural mistakes the verifier would reject, but
/// would reject in its own language.
///
/// The kernel's answer to a jump past the end of a program is a register dump
/// and an instruction index; the answer to a half-written 64-bit load is
/// "invalid bpf_ld_imm64 insn". Neither tells the person who wrote the program
/// what to change, and unlike a compiled program there is no compiler in
/// between to have caught it. So the same checks run here first, in words, and
/// cost a few microseconds once per load.
///
/// This is not a verifier. It knows nothing about types, about what a helper
/// expects, or about whether a pointer is where the program thinks it is --
/// only the kernel can say that, and it still does.
pub fn audit(instructions: &[Insn]) -> Result<(), String> {
    if instructions.is_empty() {
        return Err("a program with no instructions".to_owned());
    }
    if instructions.last().map(|insn| insn.code) != Some(BPF_JMP | BPF_EXIT) {
        return Err("the last instruction is not `exit`".to_owned());
    }

    let mut index = 0;
    while index < instructions.len() {
        let insn = instructions[index];
        let (dst, src) = (insn.regs & 0xf, insn.regs >> 4);

        // A 64-bit immediate load occupies two slots, and the second is not an
        // instruction: it carries the high half and must be otherwise empty.
        if insn.code == (BPF_LD | BPF_DW | BPF_IMM) {
            match instructions.get(index + 1) {
                None => {
                    return Err(format!(
                        "instruction {index} loads a 64-bit immediate but is the last one; it \
                         needs a second slot"
                    ))
                }
                Some(second) if second.code != 0 => {
                    return Err(format!(
                        "instruction {index} loads a 64-bit immediate and the slot after it is \
                         an instruction ({:#04x}), not the value's high half",
                        second.code
                    ))
                }
                Some(_) => {}
            }
            index += 2;
            continue;
        }

        if dst > 10 || src > 10 {
            return Err(format!(
                "instruction {index} names register r{} or r{}, and there are only r0 to r10",
                dst.max(src),
                dst.min(src)
            ));
        }

        let class = insn.code & 0x07;
        let operation = insn.code & 0xf0;
        if (class == BPF_JMP || class == BPF_JMP32)
            && operation != BPF_CALL
            && operation != BPF_EXIT
        {
            let target = index as i64 + 1 + insn.off as i64;
            if target < 0 || target >= instructions.len() as i64 {
                return Err(format!(
                    "the jump at instruction {index} lands at {target}, outside a program of {} \
                     instructions",
                    instructions.len()
                ));
            }
        }

        if class == BPF_JMP && operation == BPF_CALL && insn.imm <= 0 {
            return Err(format!(
                "instruction {index} calls helper {}, and helper numbers start at 1",
                insn.imm
            ));
        }

        // The stack is 512 bytes below the frame pointer and nothing else.
        let base = match class {
            BPF_LDX => Some(src),
            BPF_STX | BPF_ST => Some(dst),
            _ => None,
        };
        if base == Some(10) && !(-512..0).contains(&insn.off) {
            return Err(format!(
                "instruction {index} touches the stack at {} bytes from the frame pointer, and \
                 the stack is the 512 bytes below it",
                insn.off
            ));
        }

        index += 1;
    }
    Ok(())
}

/// Instructions being assembled, with the jump bookkeeping that makes a
/// hand-written program survive an edit.
///
/// A jump offset counts instructions from the one after the jump, so inserting
/// a line silently breaks every jump that crosses it. Labels are resolved here
/// instead: the program says where it wants to go, and the offsets are computed
/// once at the end.
#[derive(Default)]
pub struct Program {
    instructions: Vec<Insn>,
    /// Jumps waiting for their label, as (instruction index, label).
    pending: Vec<(usize, &'static str)>,
    labels: Vec<(&'static str, usize)>,
}

impl Program {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, instruction: Insn) -> &mut Self {
        self.instructions.push(instruction);
        self
    }

    pub fn extend(&mut self, instructions: impl IntoIterator<Item = Insn>) -> &mut Self {
        self.instructions.extend(instructions);
        self
    }

    /// Emits a jump whose offset is filled in when `label` is placed.
    pub fn jump_to(&mut self, label: &'static str, mut jump: Insn) -> &mut Self {
        jump.off = 0;
        self.pending.push((self.instructions.len(), label));
        self.instructions.push(jump);
        self
    }

    pub fn label(&mut self, label: &'static str) -> &mut Self {
        self.labels.push((label, self.instructions.len()));
        self
    }

    /// Resolves the jumps and returns the program.
    ///
    /// # Panics
    ///
    /// If a jump names a label that was never placed. That is a bug in the
    /// program being built, not a condition of the machine, so it fails here
    /// rather than reaching the verifier as an incomprehensible rejection.
    pub fn build(mut self) -> Vec<Insn> {
        for (index, label) in std::mem::take(&mut self.pending) {
            let target = self
                .labels
                .iter()
                .find(|(name, _)| *name == label)
                .map(|(_, at)| *at)
                .unwrap_or_else(|| panic!("jump to `{label}`, which is never placed"));
            self.instructions[index].off = (target as i64 - index as i64 - 1) as i16;
        }
        self.instructions
    }
}

// The sequences below are instructions rather than single opcodes, and they are
// here because a program built as instructions has no compiler to notice that
// two copies of one sequence have drifted apart. Each of them was written
// between two and eight times across the family programs before it was moved
// here, and `push_log2_bucket` is the one that made it urgent: it decides which
// bucket every kernel-built histogram in this repository puts a measurement in,
// and two copies disagreeing by one report every duration in the neighbouring
// bucket -- a factor of two -- with nothing anywhere saying so.

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

/// Reads a field narrower than a word into a four-byte slot, zero-extended.
///
/// Much of what these programs want out of the kernel is two bytes wide -- an
/// address family, a protocol number, a port -- and the loads this layer
/// exposes take four or eight. So the slot is cleared first and the kernel is
/// asked for exactly the field's own width: what lands is the field, and the
/// bytes beside it are zero rather than whatever the stack held.
///
/// Reading four bytes and masking would be the other way round, and it reads
/// one to three bytes of the next field to do it -- a fault where the field is
/// the last of a page-aligned structure, and a wrong number the rest of the
/// time.
pub fn read_narrow(program: &mut Program, into: i16, size: i32, from: u8, field_offset: i32) {
    program.push(Insn::st_imm(BPF_W, R10, into, 0));
    read_field(program, into, size, from, field_offset);
}

/// Follows a pointer field into a register.
///
/// `scratch` is eight bytes of stack the read lands in before the load; `into`
/// is where the pointer is wanted, and should be one of `r6` through `r9` if
/// anything after the next helper call reads it.
pub fn read_pointer(program: &mut Program, scratch: i16, into: u8, from: u8, field_offset: i32) {
    program.push(Insn::st_imm(BPF_DW, R10, scratch, 0));
    read_field(program, scratch, 8, from, field_offset);
    program.push(Insn::ldx(BPF_DW, into, R10, scratch));
}

/// Adds one to a slot of a per-cpu drop counter.
///
/// `drop_key` is four bytes of stack the key is written into. `resume` is where
/// control goes both when the count has been made and when the counter could
/// not be found: failing to record a loss is not a reason to stop recording
/// measurements.
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

/// The labels the unrolled `log2` jumps to, six steps and the end.
///
/// There is no count-leading-zeros instruction in BPF and no loop worth writing
/// on a path this hot, so the exponent is found by binary search: six compares,
/// thirty instructions, no branch that depends on anything but the value.
///
/// Each call passes its own labels, because two of these in one instruction
/// stream would define the same label twice.
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
    // A measurement of exactly zero nanoseconds -- two events inside one clock
    // tick -- is bucket zero, not the log2 of zero.
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

// ------------------------------------------------------------ whose events

/// Whose events a program keeps, compared in the kernel.
///
/// Every `sched` and `proc` program narrows to a target before it does any
/// work, because the paths they sit on fire for every task on the machine and
/// a comparison is the cheapest thing a program can do. Until the thirteenth
/// wave each one wrote its own comparison, against the thread group alone, so
/// a `--tid` target compared a thread id with a process id and kept nothing
/// (`docs/scenarios/cross-io.md` defect 2), and a `--cgroup` target was refused
/// by every one of them. One type, and the two instruction sequences below, is
/// what every program compares with now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Whose {
    /// The whole machine: no comparison at all.
    #[default]
    Any,
    /// A thread group: `task_struct.tgid`, the high half of
    /// `bpf_get_current_pid_tgid`.
    Process(u32),
    /// One thread: `task_struct.pid`, which is the kernel's thread id, and the
    /// low half of `bpf_get_current_pid_tgid`.
    Thread(u32),
    /// Every task in a cgroup or in any cgroup below it.
    Cgroup(Cgroup),
}

/// A cgroup of the unified hierarchy, as a program compares it.
///
/// **Why the ancestor, and not the task's own cgroup.** Two helpers were on
/// offer (`docs/open-findings.md`, the fifth wave): `bpf_get_current_cgroup_id`
/// against one id, or `bpf_current_task_under_cgroup` against a
/// `BPF_MAP_TYPE_CGROUP_ARRAY`. The first answers only for the cgroup a task is
/// directly in, and a container's or a service's tasks are almost always in
/// cgroups *below* the path a reader names -- a `--cgroup user.slice` that
/// missed every session under it would be a filter that keeps nothing and
/// looks like a quiet machine. The second answers the ancestor question, but
/// only for the *current* task, and half of these programs judge a task that
/// is not current: `sched_switch`'s incoming task, `sched_waking`'s wakee, the
/// task a signal is for, the task being migrated.
///
/// So the comparison is the kernel's own `cgroup_is_descendant`, by level: a
/// cgroup at level `L` of the hierarchy is an ancestor of a task's cgroup
/// exactly when the task's cgroup is at least `L` deep and its ancestor at `L`
/// is that cgroup. For the current task that is one helper call
/// (`bpf_get_current_ancestor_cgroup_id(L)`); for any other task it is the same
/// question asked of `task->cgroups->dfl_cgrp` through the `ancestors` array
/// the kernel keeps for exactly this, read with BTF offsets. Neither needs a
/// map, and both cost the same whatever the depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cgroup {
    /// The cgroup's id: its directory's inode number, which is `kn->id` and is
    /// what every cgroup-id helper returns.
    pub id: u64,
    /// Its depth below the hierarchy's root, which is level 0.
    pub level: u32,
}

/// Where the unified hierarchy is mounted, as every other reader in this
/// repository assumes.
const CGROUP2_ROOT: &str = "/sys/fs/cgroup";

impl Cgroup {
    /// Resolves a `--cgroup` path, written either rooted at the hierarchy (the
    /// way `/proc/<pid>/cgroup` writes it) or as the mounted directory.
    ///
    /// The level is the path's depth below the mount. Inside a cgroup
    /// namespace the mount's root is the namespace's, so the depth read here is
    /// short by the namespace's own depth, which `pidns::cgroup_namespace_depth`
    /// asks the kernel for and adds.
    pub fn resolve(path: &std::path::Path) -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt;
        use std::path::{Component, Path};

        let root = Path::new(CGROUP2_ROOT);
        let relative = path
            .strip_prefix(root)
            .unwrap_or_else(|_| path.strip_prefix("/").unwrap_or(path));
        let mut level = 0u32;
        for component in relative.components() {
            match component {
                Component::Normal(_) => level += 1,
                Component::CurDir => {}
                // A `..` would make the depth something other than the count
                // of names, and a cgroup path never needs one.
                _ => {
                    return Err(format!(
                        "`{}` is not a cgroup path this can compare: write it as the \
                         directory under {CGROUP2_ROOT}, or rooted at the hierarchy the way \
                         /proc/<pid>/cgroup writes it, without `..`",
                        path.display()
                    ))
                }
            }
        }
        // Inside a cgroup namespace the mount's root is the namespace's, and
        // the kernel compares levels in the whole hierarchy.
        level += super::pidns::cgroup_namespace_depth()?;
        let directory = root.join(relative);
        let metadata = std::fs::metadata(&directory)
            .map_err(|error| format!("cannot stat the cgroup {}: {error}", directory.display()))?;
        if !metadata.is_dir() || !directory.join("cgroup.procs").exists() {
            return Err(format!(
                "{} is not a cgroup of the unified hierarchy: it has no `cgroup.procs`",
                directory.display()
            ));
        }
        Ok(Self {
            id: metadata.ino(),
            level,
        })
    }
}

impl Whose {
    /// The comparison a target means.
    ///
    /// `Err` names what the target is and why a task comparison cannot mean
    /// it; each collector puts its own sentence in front, because only it
    /// knows what it attributes per task. A launched program arrives here as
    /// the pid it was bound to. The ids are the kernel's, which outside the
    /// initial pid namespace are not the ones the target was named by
    /// (`pidns`).
    pub fn of(target: &crate::request::Target) -> Result<Self, String> {
        use crate::request::Target;
        match target {
            Target::System => Ok(Self::Any),
            Target::Tid(tids) => super::pidns::kernel_tid(tids[0] as u32).map(Self::Thread),
            Target::Cgroup(path) => Cgroup::resolve(path).map(Self::Cgroup),
            Target::Container(name) => Err(format!(
                "the container `{name}` would have to be resolved to its cgroup first, which \
                 this build does not do; pass --cgroup with the container's cgroup path"
            )),
            other => other
                .first_pid()
                .ok_or_else(|| format!("a {} target names no task", other.kind()))
                .and_then(|pid| super::pidns::kernel_tgid(pid as u32))
                .map(Self::Process),
        }
    }

    /// The thread group, when this compares one.
    pub fn tgid(self) -> Option<u32> {
        match self {
            Self::Process(tgid) => Some(tgid),
            _ => None,
        }
    }
}

/// The offsets [`push_task_filter`] reads, from the running kernel's BTF.
///
/// Located once per program, and only for the comparison asked for: a pid
/// target never looks at the cgroup structures, so a kernel whose
/// `struct cgroup` this cannot read still serves every other target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskFields {
    pid: i32,
    tgid: i32,
    cgroup: Option<CgroupWalk>,
}

/// How a task's cgroup ancestry is laid out on this kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CgroupWalk {
    /// Since 6.1: `struct cgroup *ancestors[]`, and the id is on each
    /// ancestor's `kernfs_node`.
    Ancestors {
        cgroups: i32,
        dfl_cgrp: i32,
        level: i32,
        ancestors: i32,
        kn: i32,
        kn_id: i32,
    },
    /// Up to 6.0: `u64 ancestor_ids[]`, the ids themselves.
    AncestorIds {
        cgroups: i32,
        dfl_cgrp: i32,
        level: i32,
        ancestor_ids: i32,
    },
}

impl TaskFields {
    pub fn locate(btf: &super::btf::Btf, whose: Whose) -> Result<Self, String> {
        let pid = btf.member_offset("task_struct", "pid")?;
        let tgid = btf.member_offset("task_struct", "tgid")?;
        let cgroup = match whose {
            Whose::Cgroup(_) => Some(CgroupWalk::locate(btf)?),
            _ => None,
        };
        Ok(Self { pid, tgid, cgroup })
    }
}

impl CgroupWalk {
    fn locate(btf: &super::btf::Btf) -> Result<Self, String> {
        let cgroups = btf.member_offset("task_struct", "cgroups")?;
        let dfl_cgrp = btf.member_offset("css_set", "dfl_cgrp")?;
        let level = btf.member_offset("cgroup", "level")?;
        if let Ok(ancestors) = btf.member_offset("cgroup", "ancestors") {
            return Ok(Self::Ancestors {
                cgroups,
                dfl_cgrp,
                level,
                ancestors,
                kn: btf.member_offset("cgroup", "kn")?,
                kn_id: btf.member_offset("kernfs_node", "id")?,
            });
        }
        let ancestor_ids = btf.member_offset("cgroup", "ancestor_ids").map_err(|_| {
            "this kernel's `struct cgroup` has neither `ancestors` (6.1 and later) nor \
             `ancestor_ids` (earlier), so a task's cgroup ancestry cannot be read"
                .to_owned()
        })?;
        Ok(Self::AncestorIds {
            cgroups,
            dfl_cgrp,
            level,
            ancestor_ids,
        })
    }
}

/// The task a filter judges: a `task_struct *` in a register, and, where the
/// program has already read them, the task's thread id and thread group in
/// four-byte stack slots -- which a process or thread comparison then reads
/// instead of the task, saving a helper call on the hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subject {
    pub task_reg: u8,
    pub ids: Option<(i16, i16)>,
}

impl Subject {
    pub fn task(task_reg: u8) -> Self {
        Self {
            task_reg,
            ids: None,
        }
    }

    pub fn with_ids(task_reg: u8, tid_slot: i16, tgid_slot: i16) -> Self {
        Self {
            task_reg,
            ids: Some((tid_slot, tgid_slot)),
        }
    }
}

/// Falls through when the task `subject` names is one `whose` keeps, and
/// jumps to `skip` when it is not.
///
/// Clobbers `r1` to `r5` and the sixteen bytes of stack at `scratch`; the task
/// register should be one of `r6` to `r9`, since the reads are helper calls.
/// A pointer that reads as null anywhere along the cgroup walk is a task this
/// run cannot place, and is skipped rather than kept: a filter that keeps what
/// it cannot judge is not a filter.
pub fn push_task_filter(
    program: &mut Program,
    fields: &TaskFields,
    whose: Whose,
    subject: Subject,
    scratch: i16,
    skip: &'static str,
) {
    let task_reg = subject.task_reg;
    match whose {
        Whose::Any => {}
        Whose::Process(tgid) => {
            let slot = match subject.ids {
                Some((_, tgid_slot)) => tgid_slot,
                None => {
                    read_narrow(program, scratch, 4, task_reg, fields.tgid);
                    scratch
                }
            };
            program.push(Insn::ldx(BPF_W, 1, R10, slot));
            program.jump_to(skip, Insn::jne_imm(1, tgid as i32, 0));
        }
        Whose::Thread(tid) => {
            let slot = match subject.ids {
                Some((tid_slot, _)) => tid_slot,
                None => {
                    read_narrow(program, scratch, 4, task_reg, fields.pid);
                    scratch
                }
            };
            program.push(Insn::ldx(BPF_W, 1, R10, slot));
            program.jump_to(skip, Insn::jne_imm(1, tid as i32, 0));
        }
        Whose::Cgroup(cgroup) => {
            let walk = fields
                .cgroup
                .expect("TaskFields::locate reads the cgroup offsets for a cgroup target");
            push_cgroup_walk(program, walk, cgroup, task_reg, scratch, skip);
        }
    }
}

/// `cgroup_is_descendant(task->cgroups->dfl_cgrp, target)`, by level.
///
/// The pointer being followed is carried in `r4`, which `read_field` does not
/// touch before its call; after each call it is loaded back from the stack.
fn push_cgroup_walk(
    program: &mut Program,
    walk: CgroupWalk,
    cgroup: Cgroup,
    task_reg: u8,
    scratch: i16,
    skip: &'static str,
) {
    let (cgroups, dfl_cgrp, level) = match walk {
        CgroupWalk::Ancestors {
            cgroups,
            dfl_cgrp,
            level,
            ..
        }
        | CgroupWalk::AncestorIds {
            cgroups,
            dfl_cgrp,
            level,
            ..
        } => (cgroups, dfl_cgrp, level),
    };
    let pointer = scratch;
    let word = scratch + 8;

    // task->cgroups, then its default-hierarchy cgroup.
    read_pointer(program, pointer, 4, task_reg, cgroups);
    program.jump_to(skip, Insn::jeq_imm(4, 0, 0));
    read_pointer(program, pointer, 4, 4, dfl_cgrp);
    program.jump_to(skip, Insn::jeq_imm(4, 0, 0));

    // Shallower than the target: it cannot be inside it.
    read_narrow(program, word, 4, 4, level);
    program.push(Insn::ldx(BPF_W, 1, R10, word));
    program.push(Insn::mov64_imm(2, cgroup.level as i32));
    program.jump_to(skip, Insn::jgt_reg(2, 1, 0));

    // The ancestor at the target's level, by id. The cgroup pointer is still in
    // the pointer slot from the read above.
    program.push(Insn::ldx(BPF_DW, 4, R10, pointer));
    match walk {
        CgroupWalk::Ancestors {
            ancestors,
            kn,
            kn_id,
            ..
        } => {
            read_pointer(program, pointer, 4, 4, ancestors + 8 * cgroup.level as i32);
            program.jump_to(skip, Insn::jeq_imm(4, 0, 0));
            read_pointer(program, pointer, 4, 4, kn);
            program.jump_to(skip, Insn::jeq_imm(4, 0, 0));
            program.push(Insn::st_imm(BPF_DW, R10, pointer, 0));
            read_field(program, pointer, 8, 4, kn_id);
        }
        CgroupWalk::AncestorIds { ancestor_ids, .. } => {
            program.push(Insn::st_imm(BPF_DW, R10, pointer, 0));
            read_field(
                program,
                pointer,
                8,
                4,
                ancestor_ids + 8 * cgroup.level as i32,
            );
        }
    }
    program.push(Insn::ldx(BPF_DW, 1, R10, pointer));
    program.extend(Insn::ld_imm64(2, cgroup.id as i64));
    program.jump_to(skip, Insn::jne_reg(1, 2, 0));
}

/// The same comparison for the task the program runs in, through helpers.
///
/// For the programs whose subject is the current task -- a syscall, a lock
/// wait, a signal's sender -- one helper call replaces the reads. Clobbers
/// `r0` to `r5` and the eight bytes at `scratch`.
pub fn push_current_filter(program: &mut Program, whose: Whose, scratch: i16, skip: &'static str) {
    match whose {
        Whose::Any => {}
        Whose::Process(tgid) => {
            program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
            program.push(Insn::rsh64_imm(0, 32));
            program.jump_to(skip, Insn::jne_imm(0, tgid as i32, 0));
        }
        Whose::Thread(tid) => {
            // The low half, compared 32 bits wide: the high half holds the
            // thread group, and a 64-bit comparison would never match.
            program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
            program.push(Insn::stx(BPF_W, R10, scratch, 0));
            program.push(Insn::ldx(BPF_W, 1, R10, scratch));
            program.jump_to(skip, Insn::jne_imm(1, tid as i32, 0));
        }
        Whose::Cgroup(cgroup) => {
            program.push(Insn::mov64_imm(1, cgroup.level as i32));
            program.push(Insn::call(helper::GET_CURRENT_ANCESTOR_CGROUP_ID));
            program.extend(Insn::ld_imm64(1, cgroup.id as i64));
            program.jump_to(skip, Insn::jne_reg(0, 1, 0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The opcodes are the kernel's (`include/uapi/linux/bpf_common.h` and
    /// `bpf.h`), written out as the numbers the verifier reads rather than
    /// rebuilt from this file's own constants.
    #[test]
    fn the_shared_alu_encodings_are_the_kernels_numbers() {
        assert_eq!(
            Insn::or64_imm(3, 0x10).code,
            0x47,
            "BPF_ALU64 | BPF_OR | BPF_K"
        );
        assert_eq!(
            Insn::arsh64_imm(3, 8).code,
            0xc7,
            "BPF_ALU64 | BPF_ARSH | BPF_K"
        );
        let [low, high] = Insn::ld_imm64(2, -2);
        assert_eq!(low.code, 0x18, "BPF_LD | BPF_DW | BPF_IMM");
        assert_eq!(low.regs, 2, "the destination, and no pseudo source");
        assert_eq!(
            (low.imm as u32, high.imm as u32),
            (0xffff_fffe, 0xffff_ffff)
        );
        assert_eq!(high.code, 0, "the second slot carries only the high half");

        let [load, vmlinux] = Insn::load_btf_id(1, 4242);
        assert_eq!(load.code, 0x18, "a 64-bit immediate load");
        assert_eq!(load.regs, 1 | (3 << 4), "into r1, from BPF_PSEUDO_BTF_ID");
        assert_eq!(
            (load.imm, vmlinux.imm),
            (4242, 0),
            "the id, then vmlinux's fd"
        );
    }

    #[test]
    fn registers_pack_into_one_byte() {
        let insn = Insn::ldx(BPF_DW, 7, 6, 0);
        assert_eq!(insn.regs & 0xf, 7, "destination is the low nibble");
        assert_eq!(insn.regs >> 4, 6, "source is the high one");
    }

    #[test]
    fn a_forward_jump_counts_from_the_instruction_after_it() {
        let mut program = Program::new();
        program.jump_to("end", Insn::jeq_imm(1, 0, 0));
        program.push(Insn::mov64_imm(0, 1));
        program.label("end");
        program.push(Insn::exit());
        let built = program.build();
        assert_eq!(
            built[0].off, 1,
            "the jump skips exactly the one instruction"
        );
    }

    #[test]
    fn a_backward_jump_is_negative() {
        let mut program = Program::new();
        program.label("top");
        program.push(Insn::mov64_imm(0, 0));
        program.jump_to("top", Insn::jeq_imm(0, 0, 0));
        let built = program.build();
        assert_eq!(built[1].off, -2);
    }

    #[test]
    #[should_panic(expected = "jump to `missing`")]
    fn a_jump_to_a_label_that_was_never_placed_fails_here_not_in_the_verifier() {
        let mut program = Program::new();
        program.jump_to("missing", Insn::jeq_imm(0, 0, 0));
        program.build();
    }

    /// The kernel reads instructions as an array of 8-byte structs; a layout
    /// that is off by a byte would be rejected as gibberish, or worse, loaded.
    #[test]
    fn an_instruction_is_eight_bytes() {
        assert_eq!(std::mem::size_of::<Insn>(), 8);
    }

    /// The audit exists because these mistakes are silent until the kernel
    /// rejects them in its own language, and there is no compiler in between.
    /// Each case below is a program that would come back as a register dump.
    #[test]
    fn a_jump_past_the_end_is_caught_before_the_kernel_sees_it() {
        let program = vec![Insn::jeq_imm(1, 0, 40), Insn::exit()];
        let error = audit(&program).unwrap_err();
        assert!(error.contains("outside a program of 2"), "{error}");
    }

    #[test]
    fn a_backward_jump_before_the_first_instruction_is_caught_too() {
        let program = vec![Insn::mov64_imm(0, 0), Insn::jeq_imm(0, 0, -5), Insn::exit()];
        let error = audit(&program).unwrap_err();
        assert!(error.contains("lands at -3"), "{error}");
    }

    #[test]
    fn a_half_written_sixty_four_bit_load_is_caught() {
        let mut program = vec![];
        program.extend(Insn::load_map_fd(1, 3));
        program.push(Insn::exit());
        assert!(audit(&program).is_ok(), "the well-formed pair is accepted");

        // The same load with an instruction where its high half should be.
        let broken = vec![
            Insn::load_map_fd(1, 3)[0],
            Insn::mov64_imm(2, 0),
            Insn::exit(),
        ];
        let error = audit(&broken).unwrap_err();
        assert!(error.contains("high half"), "{error}");

        let truncated = vec![Insn::load_map_fd(1, 3)[0]];
        assert!(audit(&truncated).is_err());
    }

    #[test]
    fn a_register_that_does_not_exist_is_caught() {
        let program = vec![Insn::mov64_reg(11, 1), Insn::exit()];
        let error = audit(&program).unwrap_err();
        assert!(error.contains("r11"), "{error}");
    }

    /// The stack is the 512 bytes below the frame pointer. A positive offset
    /// reads the caller's frame, which the verifier refuses and which would be
    /// a real bug if it did not.
    #[test]
    fn a_stack_access_outside_the_frame_is_caught() {
        for offset in [8, -600] {
            let program = vec![Insn::stx(BPF_DW, R10, offset, 1), Insn::exit()];
            let error = audit(&program).unwrap_err();
            assert!(error.contains("frame pointer"), "{offset}: {error}");
        }
        assert!(audit(&[Insn::stx(BPF_DW, R10, -8, 1), Insn::exit()]).is_ok());
    }

    #[test]
    fn a_program_that_does_not_end_in_exit_is_caught() {
        let error = audit(&[Insn::mov64_imm(0, 0)]).unwrap_err();
        assert!(error.contains("not `exit`"), "{error}");
        assert!(audit(&[]).is_err(), "and neither is an empty one a program");
    }

    /// The kernel decodes an atomic add by its opcode byte, and there is no
    /// other way to tell it apart from an ordinary store: `0xdb` is
    /// `lock *(u64 *)(dst + off) += src` and `0xc3` is the 32-bit form, both
    /// with `BPF_ADD` in the immediate. A wrong byte here is a store that
    /// overwrites a total instead of adding to it, which reads as a plausible
    /// number rather than as an error.
    #[test]
    fn an_atomic_add_is_encoded_the_way_the_kernel_decodes_one() {
        let wide = Insn::atomic_add(BPF_DW, 0, 16, 9);
        assert_eq!(wide.code, 0xdb);
        assert_eq!(wide.imm, 0, "the operation is BPF_ADD");
        assert_eq!(wide.regs & 0xf, 0, "the destination holds the address");
        assert_eq!(wide.regs >> 4, 9, "the source holds the addend");
        assert_eq!(wide.off, 16);
        assert_eq!(Insn::atomic_add(BPF_W, 1, 0, 2).code, 0xc3);
        assert!(audit(&[wide, Insn::exit()]).is_ok());
    }

    /// An unconditional jump is the one whose operation bits are zero, so a
    /// label resolved through `jump_to` is the only thing that distinguishes
    /// it from an instruction nobody finished writing. The audit is what
    /// catches the second case, and it has to catch it on the first too.
    #[test]
    fn an_unconditional_jump_is_bounded_like_any_other() {
        let mut program = Program::new();
        program.jump_to("end", Insn::ja());
        program.push(Insn::mov64_imm(0, 1));
        program.label("end");
        program.push(Insn::exit());
        let built = program.build();
        assert_eq!(built[0].code, 0x05, "BPF_JMP | BPF_JA");
        assert_eq!(built[0].off, 1);
        assert!(audit(&built).is_ok());

        let error = audit(&[Insn::new(0x05, 0, 0, 90, 0), Insn::exit()]).unwrap_err();
        assert!(error.contains("outside a program of 2"), "{error}");
    }

    #[test]
    fn a_bit_test_is_a_jump_on_the_mask_not_on_the_value() {
        let insn = Insn::jset_imm(1, 0x80, 0);
        assert_eq!(insn.code, 0x45, "BPF_JMP | BPF_JSET | BPF_K");
        assert_eq!(insn.imm, 0x80);
    }

    #[test]
    fn a_subtraction_between_registers_is_the_alu64_form() {
        let insn = Insn::sub64_reg(9, 8);
        assert_eq!(insn.code, 0x1f, "BPF_ALU64 | BPF_SUB | BPF_X");
        assert_eq!((insn.regs & 0xf, insn.regs >> 4), (9, 8));
    }

    /// A call whose helper number was never filled in reads as `call 0`, which
    /// the kernel reports as an invalid function.
    #[test]
    fn a_call_to_no_helper_is_caught() {
        let error = audit(&[Insn::call(0), Insn::exit()]).unwrap_err();
        assert!(error.contains("helper numbers start at 1"), "{error}");
    }

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
    /// measurement is reported in the neighbouring bucket -- a factor of two --
    /// and nothing anywhere says so.
    ///
    /// Four copies of this test existed, one beside each copy of the sequence.
    /// One sequence needs one.
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

    /// The unrolled binary search has to compute `floor(log2(v)) + 1`, checked
    /// against the same shift arithmetic the instruction stream performs --
    /// the instructions themselves cannot be run outside the kernel.
    #[test]
    fn the_unrolled_log2_is_the_same_arithmetic_as_the_layouts() {
        let unrolled = |value: u64| -> u64 {
            if value == 0 {
                return 0;
            }
            let (mut value, mut exponent) = (value, 0u64);
            for shift in [32, 16, 8, 4, 2, 1] {
                let shifted = value >> shift;
                if shifted != 0 {
                    exponent += shift as u64;
                    value = shifted;
                }
            }
            exponent + 1
        };
        for value in [0u64, 1, 2, 3, 4, 7, 8, 1_000, 999_999, 1 << 40, u64::MAX] {
            assert_eq!(
                unrolled(value),
                reference_index(value),
                "{value} is bucketed differently by the two"
            );
        }
    }

    /// One log2 in one program places each of its labels once, which is what
    /// the caller-supplied label set exists for: two copies in one instruction
    /// stream would define the same label twice and the builder would refuse.
    #[test]
    fn two_log2_searches_in_one_program_keep_their_labels_apart() {
        const FIRST: Log2Labels = ["a32", "a16", "a8", "a4", "a2", "a1", "a_done"];
        const SECOND: Log2Labels = ["b32", "b16", "b8", "b4", "b2", "b1", "b_done"];
        let mut program = Program::new();
        push_log2_bucket(&mut program, 7, &FIRST);
        push_log2_bucket(&mut program, 8, &SECOND);
        program.push(Insn::exit());
        let built = program.build();
        audit(&built).expect("two searches in one stream are a well-formed sequence");
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
        audit(&built).expect("a well-formed sequence");
    }

    /// `and64_imm` is the ALU64 `and` with an immediate, and the encoding is
    /// checked rather than assumed: a wrong opcode here masks with something
    /// else and the verifier accepts it.
    #[test]
    fn a_bitwise_and_is_encoded_as_the_alu64_and() {
        let masked = Insn::and64_imm(6, !3);
        assert_eq!(masked.code, BPF_ALU64 | BPF_AND | BPF_K);
        assert_eq!(masked.regs & 0xf, 6);
        assert_eq!(masked.imm, !3);
    }

    /// The register forms three files had been writing by hand, checked
    /// against `include/uapi/linux/bpf_common.h` rather than against each
    /// other. A wrong opcode here is not refused by the verifier: `jgt` read as
    /// `jge` is a comparison that is off by one exactly at the boundary, which
    /// is where a duration and a deadline meet.
    #[test]
    // The zeros are the point: `BPF_ADD` and `BPF_K` are zero, and an encoding
    // written as class | op | source reads against the header only when all
    // three parts are spelled out.
    #[allow(clippy::identity_op)]
    fn the_register_forms_are_the_kernels_encodings() {
        // class | op | source, as `bpf_common.h` spells them.
        assert_eq!(Insn::add64_reg(1, 2).code, 0x07 | 0x00 | 0x08);
        assert_eq!(Insn::lsh64_imm(1, 3).code, 0x07 | 0x60 | 0x00);
        assert_eq!(Insn::jgt_reg(1, 2, 0).code, 0x05 | 0x20 | 0x08);
        assert_eq!(Insn::jge_reg(1, 2, 0).code, 0x05 | 0x30 | 0x08);
        assert_eq!(Insn::jne_reg(1, 2, 0).code, 0x05 | 0x50 | 0x08);
        // dst in the low nibble, src in the high one.
        let jump = Insn::jgt_reg(3, 9, -4);
        assert_eq!(jump.regs & 0xf, 3);
        assert_eq!(jump.regs >> 4, 9);
        assert_eq!(jump.off, -4);
        assert_eq!(BPF_H, 0x08, "the half-word width");
    }

    /// A `--cgroup` path names one cgroup whichever way it is written, and its
    /// level is its depth below the hierarchy's root: the number the
    /// comparison asks the kernel's `ancestors` array at.
    #[test]
    fn a_cgroup_path_resolves_to_its_inode_and_its_depth_either_way_it_is_written() {
        use std::os::unix::fs::MetadataExt;
        let root = std::path::Path::new(CGROUP2_ROOT);
        if !root.join("cgroup.procs").exists() {
            eprintln!("skipped: no cgroup2 hierarchy is mounted at {CGROUP2_ROOT}");
            return;
        }
        // Inside a cgroup namespace the mount's root is the namespace's, which
        // the kernel places this many levels down (`pidns`); zero outside one.
        let above = super::super::pidns::cgroup_namespace_depth().expect("the namespace's depth");
        let at_root = Cgroup::resolve(root).expect("the root is a cgroup");
        assert_eq!(at_root.level, above);
        assert_eq!(at_root.id, std::fs::metadata(root).expect("the root").ino());
        assert_eq!(Cgroup::resolve(std::path::Path::new("/")), Ok(at_root));

        let own = std::fs::read_to_string("/proc/self/cgroup").expect("our cgroup");
        let own = own
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .expect("a unified hierarchy line")
            .trim();
        let depth = own.split('/').filter(|part| !part.is_empty()).count() as u32;
        let rooted = Cgroup::resolve(std::path::Path::new(own)).expect("our own cgroup");
        let mounted = Cgroup::resolve(&root.join(own.trim_start_matches('/'))).expect("the same");
        assert_eq!(rooted, mounted);
        assert_eq!(rooted.level, above + depth);

        let Err(error) = Cgroup::resolve(std::path::Path::new("/user.slice/../system.slice"))
        else {
            panic!("a `..` makes the depth something other than the count of names");
        };
        assert!(error.contains(".."), "{error}");
        let Err(error) = Cgroup::resolve(std::path::Path::new("/no-such-cgroup-anywhere")) else {
            panic!("a cgroup that does not exist is refused");
        };
        assert!(error.contains("no-such-cgroup-anywhere"), "{error}");
    }

    /// A process and a thread are compared against different fields, and read
    /// out of the task only when the program has not read them already.
    #[test]
    fn a_thread_is_compared_against_the_thread_id_and_a_process_against_the_group() {
        let fields = TaskFields {
            pid: 100,
            tgid: 104,
            cgroup: None,
        };
        let build = |whose: Whose, subject: Subject| {
            let mut program = Program::new();
            push_task_filter(&mut program, &fields, whose, subject, -16, "skip");
            program.label("skip");
            program.push(Insn::exit());
            program.build()
        };
        let reads = |program: &[Insn]| -> Vec<i32> {
            program
                .windows(2)
                .filter(|pair| pair[1].code == 0x85 && pair[1].imm == helper::PROBE_READ_KERNEL)
                .map(|pair| pair[0].imm)
                .collect()
        };
        assert_eq!(reads(&build(Whose::Thread(7), Subject::task(7))), vec![100]);
        assert_eq!(
            reads(&build(Whose::Process(7), Subject::task(7))),
            vec![104]
        );
        assert!(reads(&build(Whose::Thread(7), Subject::with_ids(7, -40, -36))).is_empty());
        assert!(
            build(Whose::Any, Subject::task(7)).len() == 1,
            "no comparison at all"
        );
    }

    /// The cgroup comparison needs no map and names the id it compares, and
    /// the current task's form is one helper call.
    #[test]
    fn a_cgroup_comparison_is_the_id_at_the_targets_level_and_no_map() {
        let cgroup = Cgroup {
            id: 0x1234_5678_9abc,
            level: 3,
        };
        let mut current = Program::new();
        push_current_filter(&mut current, Whose::Cgroup(cgroup), -8, "skip");
        current.label("skip");
        current.push(Insn::exit());
        let current = current.build();
        let calls: Vec<i32> = current
            .iter()
            .filter(|insn| insn.code == 0x85)
            .map(|insn| insn.imm)
            .collect();
        assert_eq!(calls, vec![helper::GET_CURRENT_ANCESTOR_CGROUP_ID]);
        assert_eq!(
            current[0],
            Insn::mov64_imm(1, 3),
            "asked at the target's level"
        );
        assert!(current
            .windows(2)
            .any(|pair| pair[0].imm == 0x5678_9abc_u32 as i32 && pair[1].imm == 0x1234));

        let walk = CgroupWalk::Ancestors {
            cgroups: 1000,
            dfl_cgrp: 8,
            level: 16,
            ancestors: 64,
            kn: 24,
            kn_id: 32,
        };
        let fields = TaskFields {
            pid: 0,
            tgid: 0,
            cgroup: Some(walk),
        };
        let mut task = Program::new();
        push_task_filter(
            &mut task,
            &fields,
            Whose::Cgroup(cgroup),
            Subject::task(7),
            -16,
            "skip",
        );
        task.label("skip");
        task.push(Insn::exit());
        let task = task.build();
        audit(&task).expect("a well-formed walk");
        assert!(
            task.iter().any(|insn| insn.imm == 64 + 8 * 3),
            "the ancestor is read at `ancestors[level]`"
        );
        assert!(
            !task
                .iter()
                .any(|insn| insn.regs >> 4 == BPF_PSEUDO_MAP_FD && insn.code == 0x18),
            "the walk uses no map"
        );
    }
}
