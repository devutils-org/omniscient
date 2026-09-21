//! The eBPF layer.
//!
//! Sixty-two of the catalog's hundred and fifty-five backends are eBPF, so this
//! is a layer rather than a collector's private detail. It does four things: it
//! builds programs as instruction arrays, it reads the running kernel's BTF for
//! the field offsets those programs need, it loads and attaches them through
//! `bpf()`, and it consumes their ring buffers while counting what was lost.
//!
//! There is no clang here, and no object file. A compile-time CO-RE build
//! records relocations for libbpf to patch into a compiled program; this does
//! the same patching from the same information -- `/sys/kernel/btf/vmlinux` --
//! at the moment the program is loaded, against the kernel that is actually
//! running. What it buys is that `omn` stays one binary with no build-time
//! toolchain, and that the offsets can never be stale, because there is no
//! earlier moment at which they could have been baked in.
//!
//! What it costs is that a program is written as instructions, and that a
//! verifier rejection arrives as a log rather than as a compiler error. The
//! log travels with the error for that reason.
//!
//! Attachment is through `BPF_RAW_TRACEPOINT_OPEN`, which takes a tracepoint's
//! name. The classic path reads a numeric id out of `/sys/kernel/tracing`,
//! which is mounted and unreadable on a normal machine -- including this
//! project's development machine -- so an attachment that needs nothing but
//! BTF and a name is strictly less demanding, and sees exactly the same events.
//!
//! See `docs/bpf-layer.md` for what the other families have to call.

pub mod attach;
pub mod btf;
mod insn;
mod io;
mod io_writeback;
mod map;
mod migrate;
mod offcpu;
mod ring;
mod runqlat;
mod syscall;
mod wakeup;

use std::sync::OnceLock;

pub use io::{BlockEvent, BlockProgram, Stage, Submitter, NO_OPEN, SCOPE_IS_OURS};
pub use io_writeback::{Writeback, WritebackProgram};
pub use migrate::{Migration, MigrationProgram};
pub use offcpu::{DrainLosses, OffCpu, OffCpuProgram};
pub use runqlat::{RunqLat, RunqLatProgram};
pub use wakeup::{Wakeup, WakeupProgram};

use btf::Btf;

/// Whether a requirement of this layer is met, could not be met, or could not
/// be checked.
///
/// The third case is not a soft no. A requirement nobody could check must never
/// be reported as satisfied, and must be told apart from one that was checked
/// and found absent -- one calls for privileges, the other for a different
/// kernel.
pub enum Support {
    Yes,
    No(String),
    Unchecked(String),
}

/// The running kernel's BTF, read once.
///
/// It is a few megabytes and every program needs it, so it is read on first use
/// and kept. A kernel without it cannot serve any of this layer's backends, and
/// the reason is carried rather than reduced to a boolean.
pub fn kernel_btf() -> Result<&'static Btf, String> {
    static KERNEL_BTF: OnceLock<Result<Btf, String>> = OnceLock::new();
    KERNEL_BTF
        .get_or_init(Btf::from_kernel)
        .as_ref()
        .map_err(Clone::clone)
}

/// Whether this process can load a tracing program at all.
///
/// The probe is a load, not a guess: two instructions, the smallest program the
/// verifier accepts, of the same type and privilege class as the real ones. A
/// check of `unprivileged_bpf_disabled` or of the effective capabilities would
/// be reasoning about the kernel's answer instead of asking for it.
pub fn load_support() -> &'static Support {
    static SUPPORT: OnceLock<Support> = OnceLock::new();
    SUPPORT.get_or_init(|| {
        let probe = [insn::Insn::mov64_imm(0, 0), insn::Insn::exit()];
        match syscall::prog_load(
            syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
            &probe,
            "omn_probe",
            "GPL",
        ) {
            Ok(_) => Support::Yes,
            Err(error) if error.is_permission() => {
                Support::No(permission_advice("loading a tracing program"))
            }
            Err(error) => Support::No(format!(
                "this kernel refused the smallest possible tracing program ({error}), so no \
                 eBPF backend can run here"
            )),
        }
    })
}

/// Whether a raw tracepoint of this name exists on the running kernel.
///
/// Every tracepoint the kernel defines carries a `btf_trace_<name>` type in its
/// BTF, so the question is answered from a file any user can read, without
/// tracefs and without loading anything.
pub fn raw_tracepoint_support(name: &str) -> Support {
    match kernel_btf() {
        Ok(btf) if btf.has_type(&format!("btf_trace_{name}")) => Support::Yes,
        Ok(_) => Support::No(format!(
            "this kernel defines no `{name}` tracepoint; it is absent from its own BTF"
        )),
        Err(reason) => Support::Unchecked(format!(
            "whether `{name}` exists cannot be checked: {reason}"
        )),
    }
}

/// What to tell someone the kernel just refused on privilege grounds.
///
/// The capabilities are the answer, and the sysctl is the thing that most often
/// makes an otherwise capable machine refuse, so both are named. Nothing here
/// suggests running the whole tool as root when two capabilities are enough.
pub fn permission_advice(action: &str) -> String {
    let disabled = std::fs::read_to_string("/proc/sys/kernel/unprivileged_bpf_disabled")
        .map(|text| text.trim().to_owned())
        .unwrap_or_default();
    let sysctl = match disabled.as_str() {
        "1" => {
            " (`kernel.unprivileged_bpf_disabled` is 1, so bpf() is refused outright without \
                CAP_BPF)"
        }
        "2" => " (`kernel.unprivileged_bpf_disabled` is 2, so bpf() needs CAP_BPF)",
        _ => "",
    };
    format!(
        "{action} needs CAP_BPF and CAP_PERFMON{sysctl}. Grant them to the binary with \
         `sudo setcap cap_bpf,cap_perfmon+ep $(which omn)`, or run it under sudo. This is the \
         privilege model the project settled on: an unprivileged run is refused rather than \
         served by a backend that answers a different question."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tracepoint_the_kernel_does_not_have_is_reported_missing_not_unknown() {
        if kernel_btf().is_err() {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        }
        assert!(matches!(
            raw_tracepoint_support("definitely_not_a_tracepoint"),
            Support::No(_)
        ));
        assert!(matches!(
            raw_tracepoint_support("sched_waking"),
            Support::Yes
        ));
    }

    /// The refusal has to send the reader somewhere. A bare "permission denied"
    /// is what this project exists not to print.
    #[test]
    fn a_refusal_names_the_capabilities_and_a_way_to_get_them() {
        let advice = permission_advice("loading a tracing program");
        assert!(advice.contains("CAP_BPF"), "{advice}");
        assert!(advice.contains("setcap"), "{advice}");
    }
}
