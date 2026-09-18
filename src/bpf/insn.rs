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
pub const BPF_DW: u8 = 0x18;

/// Addressing modes.
const BPF_IMM: u8 = 0x00;
const BPF_MEM: u8 = 0x60;

/// Where the second operand comes from.
const BPF_K: u8 = 0x00;
const BPF_X: u8 = 0x08;

/// ALU operations.
const BPF_ADD: u8 = 0x00;
const BPF_RSH: u8 = 0x70;
const BPF_MOV: u8 = 0xb0;

/// Jump operations.
const BPF_JEQ: u8 = 0x10;
const BPF_JNE: u8 = 0x50;
const BPF_JSGE: u8 = 0x70;
const BPF_CALL: u8 = 0x80;
const BPF_EXIT: u8 = 0x90;

/// `src_reg` value that marks the 64-bit immediate as a map file descriptor,
/// which the kernel replaces with the map's address when the program loads.
const BPF_PSEUDO_MAP_FD: u8 = 1;

/// The helpers this layer calls, by their number in `bpf_func_id`.
pub mod helper {
    pub const MAP_LOOKUP_ELEM: i32 = 1;
    pub const KTIME_GET_NS: i32 = 5;
    pub const GET_SMP_PROCESSOR_ID: i32 = 8;
    pub const GET_CURRENT_PID_TGID: i32 = 14;
    pub const PROBE_READ_KERNEL: i32 = 113;
    pub const RINGBUF_OUTPUT: i32 = 130;
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

    /// `dst >>= imm`, unsigned.
    pub fn rsh64_imm(dst: u8, imm: i32) -> Self {
        Self::new(BPF_ALU64 | BPF_RSH | BPF_K, dst, 0, 0, imm)
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

    /// `if dst == imm goto off`
    pub fn jeq_imm(dst: u8, imm: i32, off: i16) -> Self {
        Self::new(BPF_JMP | BPF_JEQ | BPF_K, dst, 0, off, imm)
    }

    /// `if dst != imm goto off`
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
                        "instruction {index} loads a 64-bit immediate but is the last one; it                          needs a second slot"
                    ))
                }
                Some(second) if second.code != 0 => {
                    return Err(format!(
                        "instruction {index} loads a 64-bit immediate and the slot after it is                          an instruction ({:#04x}), not the value's high half",
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
                    "the jump at instruction {index} lands at {target}, outside a program of {}                      instructions",
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
                "instruction {index} touches the stack at {} bytes from the frame pointer, and                  the stack is the 512 bytes below it",
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A call whose helper number was never filled in reads as `call 0`, which
    /// the kernel reports as an invalid function.
    #[test]
    fn a_call_to_no_helper_is_caught() {
        let error = audit(&[Insn::call(0), Insn::exit()]).unwrap_err();
        assert!(error.contains("helper numbers start at 1"), "{error}");
    }
}
