//! Where a program finds what a tracepoint hands it, whichever way it was
//! attached.
//!
//! A raw tracepoint (`bpf:raw_tp`) passes the tracepoint's arguments as they
//! are, and BTF says what each one is; a classic one (`bpf:tp`) passes a
//! record the kernel filled, whose offsets tracefs publishes and nothing else
//! does. For a tracepoint whose arguments are numbers the two carry the same
//! facts, and a program that asks its context for "argument n" rather than for
//! an offset serves both attach kinds with one body: the context is the
//! adapter, and a `bpf:tp` row is one more context rather than one more
//! program.
//!
//! A tracepoint whose arguments are pointers is not one of these. A classic
//! record holds the fields the tracepoint copied out (`sched_switch`'s
//! `prev_pid`, `block_rq_issue`'s `sector`), not the `task_struct` or the
//! `request` a raw program reads: its classic program reads other things, and
//! is another program.

use super::attach::{Field, Layout};
use super::btf::Btf;
use super::insn::{helper, read_field, Insn, Program};
use super::insn::{BPF_B, BPF_DW, BPF_H, BPF_W, R10};

/// `sys_enter(regs, id)` as BTF spells it.
pub(crate) const SYS_ENTER: [&str; 2] = ["pt_regs *", "long int"];
/// `sys_exit(regs, ret)` as BTF spells it.
pub(crate) const SYS_EXIT: [&str; 2] = ["pt_regs *", "long int"];

/// Where the syscall programs find the call's number, arguments and return:
/// `raw_syscalls:sys_enter` and `sys_exit`, either way.
#[derive(Clone, Copy)]
pub(crate) enum Syscall<'a> {
    /// `sys_enter(regs, id)`, `sys_exit(regs, ret)`: the arguments are read
    /// out of the saved registers, where BTF says each one is.
    Raw(&'a Btf),
    /// `raw_syscalls:sys_enter`'s `id` and `args[6]`, `sys_exit`'s `ret`, at
    /// these offsets of the record.
    Classic {
        id_at: i16,
        args_at: i16,
        ret_at: i16,
    },
}

impl Syscall<'_> {
    /// The classic tracepoints' offsets, from tracefs.
    pub(crate) fn classic() -> Result<Syscall<'static>, String> {
        let enter = super::attach::tracepoint_layout("raw_syscalls", "sys_enter")?;
        let exit = super::attach::tracepoint_layout("raw_syscalls", "sys_exit")?;
        let at = |layout: &super::attach::Layout, field: &str, tracepoint: &str| {
            layout
                .get(field)
                .map(|field| field.offset)
                .ok_or_else(|| format!("`raw_syscalls:{tracepoint}` has no `{field}` field"))
        };
        Ok(Syscall::Classic {
            id_at: at(&enter, "id", "sys_enter")?,
            args_at: at(&enter, "args", "sys_enter")?,
            ret_at: at(&exit, "ret", "sys_exit")?,
        })
    }

    /// Refuses a kernel whose raw `tracepoint` is not the shape the slots
    /// below assume; a classic record is laid out by tracefs, which says.
    pub(crate) fn check(self, tracepoint: &str, reads: &str) -> Result<(), String> {
        if let Syscall::Raw(btf) = self {
            let shape = if tracepoint == "sys_exit" {
                SYS_EXIT
            } else {
                SYS_ENTER
            };
            super::program::prototype(btf, tracepoint, &[&shape], reads)?;
        }
        Ok(())
    }

    /// The offset of the syscall number in the context `r1` points at.
    pub(crate) fn nr_at(self) -> i16 {
        match self {
            Syscall::Raw(_) => 8,
            Syscall::Classic { id_at, .. } => id_at,
        }
    }

    /// The offset of the return value in the exit's context.
    pub(crate) fn ret_at(self) -> i16 {
        match self {
            Syscall::Raw(_) => 8,
            Syscall::Classic { ret_at, .. } => ret_at,
        }
    }

    /// Writes argument `index` of the call into eight bytes of stack, the
    /// context's pointer in `r6`. False when this kernel's BTF does not say
    /// where the argument is.
    pub(crate) fn read_argument(self, program: &mut Program, index: usize, into: i16) -> bool {
        match self {
            Syscall::Raw(btf) => {
                let Ok(offset) = btf.syscall_argument(index) else {
                    return false;
                };
                program.push(Insn::ldx(BPF_DW, 7, 6, 0));
                read_field(program, into, 8, 7, offset);
            }
            Syscall::Classic { args_at, .. } => {
                program.push(Insn::ldx(BPF_DW, 1, 6, args_at + 8 * index as i16));
                program.push(Insn::stx(BPF_DW, R10, into, 1));
            }
        }
        true
    }
}

/// A classic tracepoint's record, as its `format` file lays it out: what a
/// `bpf:tp` program reads its fields at, by name, and at their width.
///
/// The layouts move between kernels -- `gfp_flags` is four bytes on 4.19 and
/// eight on 6.1, `inet_sock_set_state`'s `protocol` one byte and then two, a
/// field appears (`writeback`'s `cgroup_ino` widens, `kfree_skb` gains
/// `reason`) -- so nothing here is a constant: every offset and width is the
/// one this kernel's tracefs publishes, and a field it does not publish is a
/// refusal naming it.
#[derive(Debug, Clone)]
pub(crate) struct Record {
    name: String,
    layout: Layout,
}

impl Record {
    /// Reads `<tracefs>/events/<category>/<name>/format`.
    pub(crate) fn of(category: &str, name: &str) -> Result<Self, String> {
        Ok(Self {
            name: format!("{category}:{name}"),
            layout: super::attach::tracepoint_layout(category, name)?,
        })
    }

    /// A record laid out by hand, for the instruction tests.
    #[cfg(test)]
    pub(crate) fn from_layout(name: &str, layout: Layout) -> Self {
        Self {
            name: name.to_owned(),
            layout,
        }
    }

    /// The field, or why the program cannot read it here.
    pub(crate) fn field(&self, name: &str) -> Result<&Field, String> {
        self.layout.get(name).ok_or_else(|| {
            format!(
                "`{}` publishes no `{name}` field in its format on this kernel, and the program \
                 reads it",
                self.name
            )
        })
    }

    /// Whether this kernel's record has the field.
    pub(crate) fn has(&self, name: &str) -> bool {
        self.layout.contains_key(name)
    }

    /// `dst = record.<name>`, zero-extended, the context in `ctx_reg`.
    ///
    /// A record is read in place where the field is one, two, four or eight
    /// bytes at an offset of its own alignment -- what the verifier allows of
    /// a tracepoint's context -- and through a read of the record's memory
    /// otherwise (`daddr[4]` at offset 35 on 4.19), into `scratch`, eight
    /// bytes of stack the caller owns.
    pub(crate) fn load(
        &self,
        program: &mut Program,
        dst: u8,
        ctx_reg: u8,
        name: &str,
        scratch: i16,
    ) -> Result<(), String> {
        let field = self.field(name)?;
        let width = match field.size {
            1 => BPF_B,
            2 => BPF_H,
            4 => BPF_W,
            8 => BPF_DW,
            other => {
                return Err(format!(
                    "`{}`'s `{name}` is {other} bytes wide, not a number this program can read",
                    self.name
                ))
            }
        };
        if field.offset as usize % field.size == 0 {
            program.push(Insn::ldx(width, dst, ctx_reg, field.offset));
            return Ok(());
        }
        self.copy(program, ctx_reg, name, scratch, 8)?;
        program.push(Insn::ldx(width, dst, R10, scratch));
        Ok(())
    }

    /// Copies `bytes` of the record from field `name` on into the stack at
    /// `into`, zeroed first: a fixed array (`comm[16]`, `saddr_v6[16]`).
    /// Clobbers `r1` to `r5`.
    pub(crate) fn copy(
        &self,
        program: &mut Program,
        ctx_reg: u8,
        name: &str,
        into: i16,
        bytes: usize,
    ) -> Result<(), String> {
        let field = self.field(name)?;
        let bytes = bytes.min(field.size.max(1)).max(1);
        for at in (0..bytes.div_ceil(8) * 8).step_by(8) {
            program.push(Insn::st_imm(BPF_DW, R10, into + at as i16, 0));
        }
        program.push(Insn::mov64_reg(1, R10));
        program.push(Insn::add64_imm(1, into as i32));
        program.push(Insn::mov64_imm(2, bytes as i32));
        program.push(Insn::mov64_reg(3, ctx_reg));
        program.push(Insn::add64_imm(3, field.offset as i32));
        program.push(Insn::call(helper::PROBE_READ_KERNEL));
        Ok(())
    }

    /// Copies the string a `__data_loc` field names into the stack at
    /// `into`, `bytes` long with its terminator, zeroed first; `r0` is then
    /// what the read returned. The field is a word whose low half is the
    /// string's offset in the record and whose high half its length.
    /// Clobbers `r1` to `r5`.
    pub(crate) fn copy_string(
        &self,
        program: &mut Program,
        ctx_reg: u8,
        name: &str,
        into: i16,
        bytes: usize,
    ) -> Result<(), String> {
        let field = self.field(name)?;
        if !field.dynamic {
            return Err(format!(
                "`{}`'s `{name}` is not a `__data_loc` string on this kernel",
                self.name
            ));
        }
        for at in (0..bytes.div_ceil(8) * 8).step_by(8) {
            program.push(Insn::st_imm(BPF_DW, R10, into + at as i16, 0));
        }
        program.push(Insn::ldx(BPF_W, 3, ctx_reg, field.offset));
        program.push(Insn::and64_imm(3, 0xffff));
        program.push(Insn::add64_reg(3, ctx_reg));
        program.push(Insn::mov64_reg(1, R10));
        program.push(Insn::add64_imm(1, into as i32));
        program.push(Insn::mov64_imm(2, bytes as i32));
        program.push(Insn::call(helper::PROBE_READ_KERNEL_STR));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Record {
        let text = "\
\tfield:unsigned short common_type;\toffset:0;\tsize:2;\tsigned:0;
\tfield:__data_loc char[] filename;\toffset:8;\tsize:4;\tsigned:1;
\tfield:pid_t pid;\toffset:12;\tsize:4;\tsigned:1;
\tfield:__u8 protocol;\toffset:30;\tsize:1;\tsigned:0;
\tfield:__u8 daddr[4];\toffset:35;\tsize:4;\tsigned:0;
\tfield:char comm[16];\toffset:40;\tsize:16;\tsigned:1;
";
        Record::from_layout("test:record", super::super::attach::parse_layout(text))
    }

    #[test]
    fn an_aligned_field_is_loaded_in_place_at_its_width() {
        let mut program = Program::new();
        record().load(&mut program, 7, 6, "pid", -8).unwrap();
        record().load(&mut program, 8, 6, "protocol", -8).unwrap();
        assert_eq!(
            program.build(),
            vec![Insn::ldx(BPF_W, 7, 6, 12), Insn::ldx(BPF_B, 8, 6, 30)]
        );
    }

    /// The verifier refuses a context load at an offset that is not a
    /// multiple of its width, so `daddr` at 35 is copied out first.
    #[test]
    fn an_unaligned_field_is_copied_out_and_loaded_from_the_stack() {
        let mut program = Program::new();
        record().load(&mut program, 7, 6, "daddr", -16).unwrap();
        let instructions = program.build();
        assert!(instructions.contains(&Insn::call(helper::PROBE_READ_KERNEL)));
        assert_eq!(instructions.last(), Some(&Insn::ldx(BPF_W, 7, R10, -16)));
    }

    #[test]
    fn a_field_this_kernel_does_not_publish_is_refused_by_name() {
        let error = record()
            .load(&mut Program::new(), 7, 6, "group_dead", -8)
            .unwrap_err();
        assert!(
            error.contains("`group_dead`") && error.contains("test:record"),
            "{error}"
        );
    }

    #[test]
    fn a_data_loc_string_is_read_at_the_offset_its_word_holds() {
        let mut program = Program::new();
        record()
            .copy_string(&mut program, 6, "filename", -64, 64)
            .unwrap();
        let instructions = program.build();
        assert!(instructions.contains(&Insn::ldx(BPF_W, 3, 6, 8)));
        assert!(instructions.contains(&Insn::and64_imm(3, 0xffff)));
        assert!(record()
            .copy_string(&mut Program::new(), 6, "pid", -64, 64)
            .is_err());
    }
}
