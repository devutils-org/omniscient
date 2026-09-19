//! Where a process is in its life, as far as procfs will say.
//!
//! Both ends of a launched run are traps, and procfs reports neither of them as
//! an error: at the start the pid exists and is still this tool's forked image,
//! and at the end the pid exists and is a zombie with nothing left to count.
//! Reading either produces a number that is well-formed, plausible and about
//! the wrong thing, which is the failure this project is built against.
//!
//! Both are answered here rather than in the collector, because they are
//! properties of the target and not of what is being counted. `proc.fd` reads a
//! zombie as zero open descriptors and `proc.threads` reads it as one -- two
//! different wrong answers with one cause.

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long a launched target is given to replace this tool's image with its
/// own before it is read anyway.
///
/// An `execve` that the kernel has already been asked for takes microseconds,
/// so this is never reached in practice by a program that is going to run. What
/// it bounds is the one case the test below cannot tell apart from a pre-exec
/// child -- a run whose target *is* this binary -- where waiting the whole
/// limit and then reading gives the right answer anyway, a tenth of a second
/// late.
const EXEC_DEADLINE: Duration = Duration::from_millis(100);

/// How often the exec is checked for. Two `readlink` calls and a small read,
/// which at this spacing is a few hundred of them across the whole deadline.
const EXEC_POLL: Duration = Duration::from_micros(200);

/// The single letter `/proc/<pid>/stat` uses for the process state.
///
/// The command name is in parentheses and may itself contain spaces and
/// parentheses, so the state is found from the *last* `)` rather than by
/// counting fields from the left.
fn state(pid: i32) -> Option<char> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = text.rsplit_once(')')?;
    rest.split_whitespace().next()?.chars().next()
}

/// Why this process has nothing left to be measured, if it has not.
///
/// A process that has exited and not been reaped keeps a `/proc/<pid>`: `fd` is
/// an empty directory that reads successfully, `status` still carries a
/// `Threads:` line saying one, and `statm` reads all zeros. None of the three
/// is an error, so a collector that only classified errors reported a departed
/// process as one holding nothing -- with its whole descriptor limit as
/// headroom, no loss, and a clean bill of health.
pub fn departed(pid: i32) -> Option<String> {
    match state(pid) {
        Some('Z') => Some(format!(
            "process {pid} exited and has not been reaped, so there is nothing left to count"
        )),
        Some('X') | Some('x') => Some(format!("process {pid} exited")),
        _ => None,
    }
}

/// Whether this pid is still the forked image of this tool, before its `execve`.
///
/// A launched target is forked and held at a gate, so between the release and
/// the exec the pid is right and everything the pid *says* belongs to `omn`:
/// its descriptors, its command name, its maps. Two facts hold together only
/// then -- the process is a child of this one, and it is running this same
/// binary.
///
/// The one case that satisfies both after the exec as well is a run whose
/// target is this binary. That is why the wait below is bounded rather than
/// unconditional: there, reading late is right and reading never is not.
fn before_exec(pid: i32) -> bool {
    let Some(parent) = parent_of(pid) else {
        return false;
    };
    if parent != std::process::id() as i32 {
        return false;
    }
    let Ok(theirs) = std::fs::read_link(PathBuf::from(format!("/proc/{pid}/exe"))) else {
        // No image yet, or no permission to see it: either way this is not
        // evidence of a pre-exec child, and inventing a wait on it would stall
        // every run against a process the tool cannot read.
        return false;
    };
    std::fs::read_link("/proc/self/exe").is_ok_and(|ours| ours == theirs)
}

/// The fourth field of `/proc/<pid>/stat`, counted from the closing bracket of
/// the command name.
fn parent_of(pid: i32) -> Option<i32> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = text.rsplit_once(')')?;
    // `) S ppid ...`
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Waits for a launched target to become the program it was asked to be.
///
/// Returns how long it waited, or `None` when it gave up: the caller reads
/// either way, because a reading that is late is a measurement and a reading
/// that never happens is not.
///
/// This is the same trap the symbolizer records in `STATE.md` -- for a launched
/// program, `start` is too early -- and the same answer: the first read is the
/// right moment, once the program exists.
pub fn wait_for_exec(pid: i32) -> Option<Duration> {
    if !before_exec(pid) {
        return Some(Duration::ZERO);
    }
    let started = Instant::now();
    while started.elapsed() < EXEC_DEADLINE {
        std::thread::sleep(EXEC_POLL);
        if !before_exec(pid) {
            return Some(started.elapsed());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zombie is the case this exists for, and it is made rather than
    /// described: a child that exits and is not waited for.
    #[test]
    fn a_process_that_exited_and_was_not_reaped_is_reported_as_departed() {
        // SAFETY: the child does nothing but `_exit`, which is
        // async-signal-safe, so forking from a threaded test process is sound.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // SAFETY: the only call the child makes.
            unsafe { libc::_exit(0) };
        }

        let mut reason = None;
        for _ in 0..500 {
            reason = departed(pid);
            if reason.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let reason = reason.expect("a child that exited and was not reaped is a zombie");
        assert!(reason.contains("not been reaped"), "{reason}");

        // Reaped, so the suite leaves nothing behind. Now the pid is gone
        // entirely and there is no state to read at all.
        // SAFETY: waiting on a child of this process.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        assert!(
            departed(pid).is_none(),
            "a reaped pid has no `stat` to read"
        );
    }

    #[test]
    fn a_living_process_is_not_departed() {
        assert!(departed(std::process::id() as i32).is_none());
    }

    /// The state letter is read from the last `)` and not from the third
    /// field, because a program is free to call itself `a b) c`.
    #[test]
    fn this_process_reports_a_state_letter_at_all() {
        let state = state(std::process::id() as i32).expect("we have a /proc/self/stat");
        assert!(state.is_ascii_alphabetic(), "state was {state:?}");
    }

    #[test]
    fn a_process_that_is_not_our_child_is_never_waited_for() {
        // Process one is not this process's child, so nothing about it can be
        // a gated launch and the wait must return immediately.
        let waited = wait_for_exec(1).expect("no wait at all");
        assert!(waited < Duration::from_millis(10), "waited {waited:?}");
    }
}
