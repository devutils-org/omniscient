//! What every program of this layer does the same way: the license it is
//! loaded under, the load itself and what a refusal says, the prototype check
//! before it reads a tracepoint's arguments, and the raw tracepoint attach.
//!
//! Each was written once per program -- twenty-three licenses, a dozen loads,
//! seven prototype checks with seven wordings -- and a fix to one (the
//! permission advice, the index of the shape a kernel declares) reached the
//! programs whose author remembered it.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::Insn;
use super::syscall;

/// The license every program is loaded under: GPL-compatible, which the
/// kernel requires of a program that calls a GPL-only helper.
pub const LICENSE: &str = "Dual MIT/GPL";

/// Loads a program of `prog_type`, saying what to change when the kernel
/// refuses it for want of a privilege.
pub fn load(prog_type: u32, instructions: &[Insn], name: &str) -> Result<OwnedFd, String> {
    syscall::prog_load(prog_type, instructions, name, LICENSE).map_err(|error| {
        if error.is_permission() {
            super::permission_advice(&format!("loading the `{name}` program"))
        } else {
            format!("the kernel refused the `{name}` program: {error}")
        }
    })
}

/// Loads a raw tracepoint program.
pub fn load_raw_tracepoint(instructions: &[Insn], name: &str) -> Result<OwnedFd, String> {
    load(syscall::BPF_PROG_TYPE_RAW_TRACEPOINT, instructions, name)
}

/// Attaches a loaded raw tracepoint program to `tracepoint`; the link it
/// returns holds it there until it is dropped.
pub fn attach_raw(tracepoint: &str, program: &OwnedFd) -> Result<OwnedFd, String> {
    syscall::raw_tracepoint_open(tracepoint, program.as_raw_fd())
        .map_err(|error| format!("cannot attach to the `{tracepoint}` tracepoint: {error}"))
}

/// Which of the prototypes a program is written for this kernel declares for
/// `tracepoint`, by index into `accepted`.
///
/// A raw tracepoint hands a program its arguments in fixed slots, and against
/// another prototype a slot holds something else -- which does not fail, it
/// produces a number. So the prototype is compared before the program is
/// built, and a kernel that declares none of the program's is refused with
/// both written out. `reads` names what the program reads out of the slots.
pub fn prototype(
    btf: &Btf,
    tracepoint: &str,
    accepted: &[&[&str]],
    reads: &str,
) -> Result<usize, String> {
    let signature = btf.tracepoint_signature(tracepoint)?;
    if let Some(shape) = accepted.iter().position(|shape| signature == *shape) {
        return Ok(shape);
    }
    Err(format!(
        "this kernel declares `{tracepoint}({})`, and this program is written for {}: it reads \
         {reads} out of fixed slots, and against another prototype those slots hold something \
         else -- which does not fail, it produces a number",
        signature.join(", "),
        accepted
            .iter()
            .map(|shape| format!("`{tracepoint}({})`", shape.join(", ")))
            .collect::<Vec<_>>()
            .join(" or "),
    ))
}

/// Every run the kernel skipped across a set of programs, of any attach kind:
/// a program runs behind a per-cpu guard, and a run that finds it taken -- an
/// interrupt re-entering it -- is counted in `bpf_prog_info.recursion_misses`
/// and nowhere else.
pub fn skipped<'a>(programs: impl IntoIterator<Item = &'a OwnedFd>) -> u64 {
    programs
        .into_iter()
        .map(|program| super::fentry::recursion_misses(program).unwrap_or(0))
        .sum()
}
