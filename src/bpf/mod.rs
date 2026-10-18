// SPDX-License-Identifier: MIT OR GPL-2.0
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
mod io_pagecache;
mod io_writeback;
mod map;
mod mem_alloc;
mod mem_oom;
mod mem_program;
mod mem_stall;
mod migrate;
mod net_drops;
mod net_lifetime;
mod net_program;
mod net_qdisc;
mod net_retransmit;
mod net_softirq;
mod offcpu;
// The `cpu` stream's: `cpu.oncpu_time` through `sched_switch`, per thread.
mod oncpu;
// The kernel's ids for what `omn` names, when it runs in a container.
pub mod pidns;
mod proc_calls;
mod proc_events;
mod proc_renames;
mod proc_syscall;
mod request;
mod ring;
mod runqlat;
// The sixth wave's `sched` programs. `sched_common` holds what they share; the
// rest is one file per source, named for the tracepoints it attaches to.
mod sched_common;
mod sched_drm;
mod sched_futex;
mod sched_irq;
mod sched_lock;
mod sched_numa;
mod switch_count;
mod syscall;
// Stream U's: reading `.note.stapsdt` and putting a uprobe on what it names.
pub mod usdt;
mod wakeup;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

pub use insn::{Cgroup, Whose};
pub use io::{BlockEvent, BlockProgram, HistRow, Stage, Submitter, NO_OPEN, SCOPE_IS_OURS};
pub use io_pagecache::{Narrowing as PageCacheNarrowing, PageCacheProgram, PageCacheRow};
pub use io_writeback::{Writeback, WritebackProgram};
pub use mem_alloc::{AllocProgram, AllocRow, Allocator};
pub use mem_oom::{Victim, VictimProgram, NO_UID};
pub use mem_stall::{Stall, StallDrops, StallProgram, StallRow, SCOPE_CGROUP, SCOPE_GLOBAL};
pub use migrate::{Migration, MigrationProgram};
pub use net_drops::{DropProgram, PacketDrop};
pub use net_lifetime::{TcpLifetime, TcpLifetimeProgram};
pub use net_qdisc::{QdiscDrain, QdiscProgram, QdiscWait};
pub use net_retransmit::{Retransmit, RetransmitProgram};
pub use net_softirq::{SoftIrq, SoftIrqDrain, SoftIrqProgram};
pub use offcpu::{DrainLosses, OffCpu, OffCpuProgram};
pub use oncpu::{OnCpu, OnCpuProgram};
pub use proc_calls::{Call, CallsNarrowing, CallsProgram};
pub use proc_events::{Exec, ExecProgram, Exit, ExitProgram, Signal, SignalProgram};
pub use proc_renames::Renames;
pub use proc_syscall::{Narrowing, SyscallDrainLosses, SyscallProgram, SyscallRow};
pub use request::{request_key, RequestCounts, RequestProgram, RequestRecord, ID_MISMATCH};
pub use runqlat::{RunqLat, RunqLatProgram};
pub use sched_drm::{GpuJob, GpuJobDrops, GpuJobProgram};
pub use sched_futex::{FutexDrops, FutexProgram, FutexWait};
pub use sched_irq::{IrqDrops, IrqProgram, IrqRun, KIND_HARD, KIND_SOFT};
pub use sched_lock::{LockDrops, LockProgram, LockWait};
pub use sched_numa::{NumaDrops, NumaProgram, NumaStick};
pub use switch_count::{SwitchCount, SwitchCountProgram};
pub use wakeup::{Wakeup, WakeupProgram};

use btf::Btf;

/// Asks the kernel to time every eBPF program's runs for as long as the
/// returned descriptor is held: `BPF_ENABLE_STATS` with `BPF_STATS_RUN_TIME`.
///
/// Refused with `EPERM` without `CAP_SYS_ADMIN`, which the `CAP_BPF` this
/// tool is granted does not include; the `kernel.bpf_stats_enabled` sysctl is
/// the other switch. Here, in the layer's public face, for the runner in
/// `src/collect/mod.rs`, which kept its own copy of the call because
/// `syscall` is private to this layer (stream R's request of the thirteenth
/// wave). `syscall.rs` is where a `bpf()` command belongs; it is not the
/// kernel stream's file this wave, so the call is written here and can move
/// there behind this same name.
pub fn enable_run_time_stats() -> std::io::Result<std::os::fd::OwnedFd> {
    /// `include/uapi/linux/bpf.h`.
    const BPF_ENABLE_STATS: libc::c_long = 32;
    const BPF_STATS_RUN_TIME: u32 = 0;
    let mut kind = BPF_STATS_RUN_TIME;
    // SAFETY: the attribute is the one `u32` the command reads.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_ENABLE_STATS,
            &mut kind as *mut u32 as *mut libc::c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    use std::os::fd::FromRawFd;
    // SAFETY: a descriptor the kernel just returned to this process.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) })
}

/// Whether a requirement of this layer is met, could not be met, or could not
/// be checked.
///
/// The third case is not a soft no. A requirement nobody could check must never
/// be reported as satisfied, and must be told apart from one that was checked
/// and found absent -- one calls for privileges, the other for a different
/// kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
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
            "Dual MIT/GPL",
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

/// Whether a raw tracepoint exists on the running kernel, and -- when the
/// requirement names one -- whether it has the prototype its program reads.
///
/// Every tracepoint the kernel defines carries a `btf_trace_<name>` type in its
/// BTF, so the question is answered from a file any user can read, without
/// tracefs and without loading anything.
///
/// The requirement is `name` or `name(arg, ...)`, and a program written for
/// more than one shape lists each: `name(a, b)|(a, b, c)`. The arguments are
/// spelled the way `Btf::tracepoint_signature` spells them -- no `struct`, no
/// qualifier -- because that is what the programs compare against at attach.
/// Existence alone was the question until the portability matrix booted 6.6
/// and 5.15: `sched_process_exit` exists on both and takes one argument
/// there, so the probe said yes, the resolver chose the backend, the target
/// launched, and the attach then refused it (P1 in `docs/portability.md`).
pub fn raw_tracepoint_support(requirement: &str) -> Support {
    let (name, prototypes) = match parse_raw_tracepoint(requirement) {
        Ok(parsed) => parsed,
        Err(reason) => return Support::Unchecked(reason),
    };
    match kernel_btf() {
        Ok(btf) if btf.has_type(&format!("btf_trace_{name}")) => {
            if let Some(refusal) = prototype_mismatch(btf, name, &prototypes) {
                return Support::No(refusal);
            }
            if VERIFY_ATTACH.load(Ordering::Relaxed) {
                raw_tracepoint_attaches(name)
            } else {
                Support::Yes
            }
        }
        Ok(_) => Support::No(format!(
            "this kernel defines no `{name}` tracepoint; it is absent from its own BTF"
        )),
        Err(reason) => Support::Unchecked(format!(
            "whether `{name}` exists cannot be checked: {reason}"
        )),
    }
}

/// Splits `name(a, b)|(c)` into the name and the prototypes, each a list of
/// argument types. A bare name has none, and asks for existence only.
fn parse_raw_tracepoint(requirement: &str) -> Result<(&str, Vec<Vec<String>>), String> {
    let Some(open) = requirement.find('(') else {
        return Ok((requirement.trim(), Vec::new()));
    };
    let name = requirement[..open].trim();
    let mut prototypes = Vec::new();
    for shape in requirement[open..].split('|') {
        let Some(inner) = shape
            .trim()
            .strip_prefix('(')
            .and_then(|rest| rest.strip_suffix(')'))
        else {
            return Err(format!(
                "`raw_tracepoint:{requirement}` is neither `name(arg, ...)` nor \
                 `name(arg, ...)|(arg, ...)`, so its prototype cannot be compared"
            ));
        };
        prototypes.push(
            inner
                .split(',')
                .map(str::trim)
                .filter(|argument| !argument.is_empty())
                .map(str::to_owned)
                .collect(),
        );
    }
    Ok((name, prototypes))
}

/// The refusal a program gives at attach when the kernel's prototype is none
/// of the ones the requirement names; `None` when it is one of them, or when
/// the requirement names none.
///
/// Worded as the programs word it (`check_signature` in the families' files),
/// so that what `omn list` says is what the attach used to say, one step
/// earlier and before anything is launched.
fn prototype_mismatch(btf: &Btf, name: &str, prototypes: &[Vec<String>]) -> Option<String> {
    if prototypes.is_empty() {
        return None;
    }
    let declared = match btf.tracepoint_signature(name) {
        Ok(declared) => declared,
        Err(reason) => return Some(reason),
    };
    if prototypes.contains(&declared) {
        return None;
    }
    Some(format!(
        "this kernel declares `{name}({})`, and this program is written for {}: it reads its \
         arguments out of fixed slots, and against another prototype those slots hold \
         something else",
        declared.join(", "),
        prototypes
            .iter()
            .map(|shape| format!("`{name}({})`", shape.join(", ")))
            .collect::<Vec<_>>()
            .join(" or "),
    ))
}

/// Whether a struct in the running kernel's BTF has a member, directly or in
/// an anonymous struct or union inside it. The requirement `btf_member:` asks
/// it as `struct.member`.
///
/// A program's field offsets are looked up when it is built, and a member one
/// kernel has and another does not is a refusal at attach: `sock.sk_ino` is
/// absent from 6.6 and older, and three `net` programs read it (P2 in
/// `docs/portability.md`). The lookup is the one the program makes --
/// `Btf::member_offset` -- so the probe cannot answer a narrower question than
/// the attach asks, and its refusal is the attach's own words.
pub fn btf_member_support(struct_dot_member: &str) -> Support {
    let Some((structure, member)) = struct_dot_member.split_once('.') else {
        return Support::Unchecked(format!(
            "`btf_member:{struct_dot_member}` is not `struct.member`"
        ));
    };
    match kernel_btf() {
        Ok(btf) => match btf.member_offset(structure, member) {
            Ok(_) => Support::Yes,
            Err(reason) => Support::No(reason),
        },
        Err(reason) => Support::Unchecked(format!(
            "whether `{structure}` has a member `{member}` cannot be checked: {reason}"
        )),
    }
}

/// Set by `--verify-attach`, and read by the probe above.
///
/// A process-global rather than a parameter threaded through `Capabilities`
/// and the resolver, deliberately: it is set once at startup, before anything
/// probes, and it changes no answer -- only how thoroughly the same question is
/// asked. Threading it would have meant a signature change in two layers while
/// six streams were mid-flight against them, which is a cost with no matching
/// benefit.
static VERIFY_ATTACH: AtomicBool = AtomicBool::new(false);

/// Asks every later raw-tracepoint probe to attach rather than read BTF.
pub fn verify_attach() {
    VERIFY_ATTACH.store(true, Ordering::Relaxed);
}

/// Whether `BPF_RAW_TRACEPOINT_OPEN` will actually bind to this name.
///
/// The question `raw_tracepoint_support` above does *not* ask, and the reason
/// it does not is a measurement rather than an assumption. Finding 38 has it:
/// asking this of every entry turns `omn list` from 150 ms into 1.3 s, because
/// attaching and detaching a tracepoint costs about thirty milliseconds of RCU
/// synchronisation apiece -- and on this kernel all thirty-eight names the
/// catalog gives attach, so the expensive question has never had a different
/// answer from the cheap one.
///
/// It is kept, and reachable as `omn list --verify-attach`, which is what
/// `tests/raw_tracepoints.rs` runs: the two probes are required to reach the
/// same answer, so "they have never disagreed" is checked on every machine the
/// suite runs on rather than believed. The test is an integration test and not
/// a unit one for the reason `CLAUDE.md` gives -- an in-process test binary is
/// rebuilt under a fresh hash every run and cannot carry file capabilities, so
/// a unit version of this would skip here and always have.
///
/// If it ever fails, this is the function the probe should start calling.
pub fn raw_tracepoint_attaches(name: &str) -> Support {
    let probe = [insn::Insn::mov64_imm(0, 0), insn::Insn::exit()];
    let program = match syscall::prog_load(
        syscall::BPF_PROG_TYPE_RAW_TRACEPOINT,
        &probe,
        "omn_rtp_probe",
        "Dual MIT/GPL",
    ) {
        Ok(program) => program,
        Err(error) if error.is_permission() => {
            return Support::Unchecked(permission_advice("attaching to a raw tracepoint"))
        }
        Err(error) => {
            return Support::Unchecked(format!(
                "this kernel refused the smallest possible tracing program ({error})"
            ))
        }
    };

    use std::os::fd::AsRawFd;
    match syscall::raw_tracepoint_open(name, program.as_raw_fd()) {
        Ok(_link) => Support::Yes,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            Support::Unchecked(permission_advice("attaching to a raw tracepoint"))
        }
        Err(error) => Support::No(format!(
            "`{name}` is in this kernel's BTF and `BPF_RAW_TRACEPOINT_OPEN` refuses it ({error})"
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
    advice_for(
        action,
        &disabled,
        crate::capabilities::withheld_file_capabilities().as_deref(),
    )
}

/// [`permission_advice`] over what it read: the sysctl's value, and why the
/// binary's own grant did not reach this process, if it carries one.
///
/// A grant already made is not advised again. A binary that carries
/// `CAP_BPF`/`CAP_PERFMON` on a `nosuid` mount, or run under `no_new_privs`,
/// was told to `setcap` the file it had just `setcap`ed; the probe said why
/// since the fifteenth wave, and the attach said the old line.
fn advice_for(action: &str, disabled: &str, withheld: Option<&str>) -> String {
    let sysctl = match disabled {
        "1" => {
            " (`kernel.unprivileged_bpf_disabled` is 1, so bpf() is refused outright without \
                CAP_BPF)"
        }
        "2" => " (`kernel.unprivileged_bpf_disabled` is 2, so bpf() needs CAP_BPF)",
        _ => "",
    };
    if let Some(withheld) = withheld {
        return format!("{action} needs CAP_BPF and CAP_PERFMON{sysctl}. {withheld}.");
    }
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

    /// A grant already made is not advised again: where the binary carries the
    /// capabilities and this process was not given them, the advice is why,
    /// and never the `setcap` the reader already ran. Where nothing was
    /// granted, the `setcap` is the answer.
    #[test]
    fn the_advice_says_why_a_grant_did_not_reach_the_process_rather_than_to_grant_it() {
        let withheld = "The binary /mnt/omn carries CAP_BPF and CAP_PERFMON, but this process \
                        was not given them: the filesystem at /mnt is mounted nosuid";
        let advice = advice_for("loading the `x` program", "2", Some(withheld));
        assert!(advice.contains("mounted nosuid"), "{advice}");
        assert!(
            advice.contains("unprivileged_bpf_disabled` is 2"),
            "{advice}"
        );
        assert!(
            !advice.contains("setcap"),
            "a grant already made, advised again: {advice}"
        );

        let advice = advice_for("loading the `x` program", "0", None);
        assert!(
            advice.contains("sudo setcap cap_bpf,cap_perfmon+ep"),
            "{advice}"
        );
        // And the process this test runs in: its binary is rebuilt under a
        // fresh hash every run and carries nothing, so the real advice is the
        // grant.
        if crate::capabilities::withheld_file_capabilities().is_none() {
            assert!(permission_advice("x").contains("setcap"));
        }
    }

    /// The statistics switch is granted or refused for want of
    /// `CAP_SYS_ADMIN`, and nothing else: an unknown command would be
    /// `EINVAL`, which is what a wrong constant here would read as.
    #[test]
    fn the_statistics_switch_is_granted_or_refused_for_privilege() {
        match enable_run_time_stats() {
            Ok(_) => {}
            Err(error) => assert!(
                matches!(error.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES)),
                "BPF_ENABLE_STATS was refused for another reason than privilege: {error}"
            ),
        }
    }

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

    /// The four-argument `sched_switch` is every kernel's since 5.18, and the
    /// three-argument one is 5.15's: the probe must tell them apart, and say
    /// both shapes when it refuses, the way the programs' own check does.
    #[test]
    fn a_prototype_the_kernel_declares_is_available_and_another_is_refused_naming_both() {
        let Ok(btf) = kernel_btf() else {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        };
        let declared = btf
            .tracepoint_signature("sched_switch")
            .expect("sched_switch is a raw tracepoint everywhere")
            .join(", ");
        assert_eq!(
            raw_tracepoint_support(&format!("sched_switch({declared})")),
            Support::Yes
        );
        let Support::No(refusal) = raw_tracepoint_support("sched_switch(bool, int)") else {
            panic!("a two-argument sched_switch is no kernel's");
        };
        assert!(
            refusal.contains(&format!("declares `sched_switch({declared})`")),
            "{refusal}"
        );
        assert!(
            refusal.contains("written for `sched_switch(bool, int)`"),
            "{refusal}"
        );
        // One of several shapes is enough, in either position.
        assert_eq!(
            raw_tracepoint_support(&format!("sched_switch(bool, int)|({declared})")),
            Support::Yes
        );
        // A bare name still asks for existence alone.
        assert_eq!(raw_tracepoint_support("sched_switch"), Support::Yes);
        // A tracepoint the kernel does not have is missing whatever its shape.
        assert!(matches!(
            raw_tracepoint_support("definitely_not_a_tracepoint(int)"),
            Support::No(_)
        ));
    }

    /// A catalog typo in a prototype must not read as a kernel that lacks it:
    /// it is a requirement nobody can check.
    #[test]
    fn a_malformed_prototype_is_unchecked_rather_than_missing() {
        assert!(matches!(
            raw_tracepoint_support("sched_switch(bool"),
            Support::Unchecked(_)
        ));
        assert!(matches!(
            raw_tracepoint_support("sched_switch(bool)|bool"),
            Support::Unchecked(_)
        ));
    }

    /// Every prototype the catalog writes parses, on any machine: the kernel
    /// decides whether it matches, and a requirement that cannot be read
    /// would be unknown on every kernel.
    #[test]
    fn every_prototype_in_the_catalog_is_well_formed() {
        let catalog = crate::catalog::Catalog::embedded().expect("the embedded catalog loads");
        let mut prototypes = 0;
        for entry in catalog.entries() {
            for backend in &entry.backends {
                for requirement in &backend.requires {
                    let Some(event) = requirement.strip_prefix("raw_tracepoint:") else {
                        continue;
                    };
                    let (name, shapes) = parse_raw_tracepoint(event)
                        .unwrap_or_else(|reason| panic!("{} {}: {reason}", entry.id, backend.id));
                    assert!(
                        !name.is_empty() && !name.contains(' '),
                        "{} {}: `{event}` names no tracepoint",
                        entry.id,
                        backend.id
                    );
                    prototypes += shapes.len();
                }
            }
        }
        assert!(
            prototypes > 30,
            "only {prototypes} prototypes in the catalog"
        );
    }

    /// The member lookup is the programs' own: a member `sock` has on every
    /// kernel is there, one it has on none is refused in the words the attach
    /// used, and a malformed requirement is unchecked.
    #[test]
    fn a_btf_member_is_found_refused_or_unchecked() {
        if kernel_btf().is_err() {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        }
        assert_eq!(btf_member_support("sock.__sk_common"), Support::Yes);
        let Support::No(refusal) = btf_member_support("sock.definitely_not_a_member") else {
            panic!("no kernel's `sock` has that member");
        };
        assert!(refusal.contains("no member named"), "{refusal}");
        assert!(matches!(btf_member_support("sock"), Support::Unchecked(_)));
    }

    /// The refusal has to send the reader somewhere. A bare "permission denied"
    /// is what this project exists not to print.
    #[test]
    fn a_refusal_names_the_capabilities_and_a_way_to_get_them() {
        let advice = permission_advice("loading a tracing program");
        assert!(advice.contains("CAP_BPF"), "{advice}");
        assert!(advice.contains("setcap"), "{advice}");
    }

    /// Every file that attaches a raw tracepoint says how many runs the kernel
    /// skipped. A raw tracepoint's program runs behind a per-cpu guard, and a
    /// run that finds it taken -- an interrupt re-entering it -- is counted in
    /// `bpf_prog_info.recursion_misses` and nowhere else. Fifteen programs went
    /// three waves without reading it, because the file that first learned it
    /// had no way to reach the others; this is the test in every file of the
    /// kind that finding 46 asks for. `mod.rs` and `syscall.rs` define the
    /// attach and attach nothing themselves.
    #[test]
    fn every_program_that_attaches_a_raw_tracepoint_reports_the_runs_it_skipped() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bpf");
        let mut silent = Vec::new();
        for entry in std::fs::read_dir(&directory).expect("src/bpf is readable") {
            let path = entry.expect("a directory entry").path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if !name.ends_with(".rs") || name == "mod.rs" || name == "syscall.rs" {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("a source file");
            if source.contains("raw_tracepoint_open(") && !source.contains("pub fn skipped(") {
                silent.push(name.to_owned());
            }
        }
        silent.sort();
        assert!(
            silent.is_empty(),
            "these attach a raw tracepoint and never read the runs the kernel skipped: {silent:?}"
        );
    }
}
