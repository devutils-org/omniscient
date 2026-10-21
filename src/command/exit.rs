//! The exit codes, and what a run says about which one it is.

use super::*;

/// Something went wrong while running -- a counter that would not open, a
/// target that went away, a file that could not be written -- or this machine
/// cannot serve the request at all.
///
/// One code because they are one answer: *this run cannot happen here*. Which
/// of them it was is in the message; what a caller does next -- try another
/// machine, or stop -- is the same either way, and is not what it does about
/// the other two codes.
pub const EXIT_UNAVAILABLE: u8 = 1;
/// The request was refused: it cannot mean what it asks for.
pub const EXIT_REJECTED: u8 = 2;
/// The request is valid, but the part of the tool that would answer it does not
/// exist yet.
pub const EXIT_NOT_IMPLEMENTED: u8 = 3;

/// The exit code for a refusal, from whose gap it is.
///
/// One function, so that the three answers cannot drift apart again: they did,
/// and nine families found it independently. `man omn.1` and `GUIDE.md` carry
/// the same table in words.
pub fn exit_for(whose: crate::collect::Whose) -> u8 {
    use crate::collect::Whose;
    match whose {
        Whose::Request => EXIT_REJECTED,
        // A refusal after a run started -- the second run of `-r N`, say --
        // is a session that measured and then failed, not a machine that
        // refused it: the earlier runs read what they read.
        Whose::Machine => {
            said_which(if RUN_STARTED.load(std::sync::atomic::Ordering::SeqCst) {
                Unavailable::FailedUnwritten
            } else {
                Unavailable::Machine
            });
            EXIT_UNAVAILABLE
        }
        Whose::Build => EXIT_NOT_IMPLEMENTED,
    }
}

/// Which of the two answers exit 1 gives: the machine refused the run, or
/// the run failed. A caller deciding whether a pair is a skip (the machine
/// cannot) or a failure (it could, and something broke) reads it from the
/// last line of stderr, which is one of [`Unavailable::line`]'s, rather than
/// guess it from whether results were written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// Refused before anything ran: no backend here, a counter that would
    /// not open, a target that is not there. Nothing was measured.
    Machine,
    /// The run started and failed before it wrote its results.
    FailedUnwritten,
    /// The run reached its end and wrote its results, and a collector broke
    /// the program it measured on the way.
    FailedWritten,
}

impl Unavailable {
    pub(crate) fn line(self) -> &'static str {
        match self {
            Unavailable::Machine => "exit 1: the machine refused the run; nothing was measured",
            Unavailable::FailedUnwritten => {
                "exit 1: the run failed after it started, before writing its results"
            }
            Unavailable::FailedWritten => {
                "exit 1: the run failed after it started; what it read is written above"
            }
        }
    }
}

/// Whether a run has started: the collectors attached and the program
/// released. An error after it is a run that failed, not a refusal.
pub static RUN_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Says which exit 1 this is, once per process.
pub fn said_which(which: Unavailable) {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !SAID.swap(true, std::sync::atomic::Ordering::SeqCst) {
        eprintln!("{}", which.line());
    }
}

/// Undoes the `dumpable = 0` that comes with carrying file capabilities.
///
/// A binary that gains privileges through `setcap` is marked non-dumpable at
/// exec, and the kernel then reassigns its own `/proc/<pid>` entries to root.
/// The collectors that read this process's procfs -- the heap shim's, which
/// walk `pagemap` and `smaps_rollup` -- get EACCES on files belonging to the
/// user who started the run.
///
/// The symptom is the one worth naming, because it is backwards: granting
/// `CAP_BPF` and `CAP_PERFMON` to unlock the eBPF backends *removes*
/// `mem.heap.alloc`, `.live` and `.retained`, which needed no privileges at
/// all. Two identical binaries, one with capabilities, disagree about what the
/// machine can serve.
///
/// This does not give anything away. `ptrace` of a task holding capabilities
/// the tracer lacks is refused by the capability subset check, which is a
/// separate barrier and still stands; dumpability governs core dumps and the
/// ownership of procfs entries, and this process has no business hiding either
/// from the user running it. The same call is made in every launched child,
/// for the same reason and with the same comment.
pub fn restore_own_dumpability() {
    // Safety: `prctl` with this option takes no pointers and cannot fail in a
    // way that matters -- a kernel that refuses it leaves the old behaviour,
    // which is what this build had before.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0);
    }
}

/// When this process started, on `CLOCK_MONOTONIC`: what a run attached to a
/// process already running says its attach came after.
pub static STARTED_NS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// What a run attached to processes already running says of when it
/// attached: what they did between the command and the attach is in no
/// reading, and `perf stat -p` and `perf record -p` attach within a few
/// milliseconds, so a reader comparing against them needs to know.
pub fn attached_late(resolved: &[ResolvedRequest], collections: &[collect::Collection]) {
    let Some(started_ns) = STARTED_NS.get() else {
        return;
    };
    for (resolved, collection) in resolved.iter().zip(collections) {
        let target = &resolved.request.target;
        if !matches!(
            target,
            crate::request::Target::Pid(_)
                | crate::request::Target::Tid(_)
                | crate::request::Target::Cgroup(_)
                | crate::request::Target::Container(_)
        ) || !collection.window.attached
        {
            continue;
        }
        let after_ns = collection.window.opened_ns.saturating_sub(*started_ns);
        eprintln!(
            "note: `{}` attached to {target} {} after omn started; what it did before that is \
             in no reading",
            resolved
                .request
                .label
                .as_deref()
                .unwrap_or(resolved.request.source.as_str()),
            span(after_ns)
        );
    }
}
