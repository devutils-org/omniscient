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

use super::btf::Btf;
use super::insn::{read_field, Insn, Program};
use super::insn::{BPF_DW, R10};

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
