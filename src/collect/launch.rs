//! Launching the observed program.
//!
//! A program launched to be measured has to be held at the starting line until
//! the collectors are attached. Otherwise a short program is half over before
//! anything is watching, and the allocations that happen during startup -- the
//! ones most likely to explain the memory curve -- are missing.
//!
//! `std::process::Command` cannot do this. It waits for the child's exec to
//! close a status pipe, so a child that stops before exec blocks the parent
//! forever. So the fork and the exec are done here, with a pipe used the other
//! way round: the child blocks reading it, the parent writes one byte when the
//! collectors are ready, and the child execs immediately after.
//!
//! The pid does not change across the exec, so an event attached before the
//! program exists follows it into the program.
//!
//! Both ends of the program's life are stamped here, because this is the only
//! place that sees them: the write that opens the gate, and the wait that finds
//! the program gone. What lies between is the whole a decomposition of the
//! program's time is a decomposition *of*, and the window a run reports is not
//! it -- the window opens after the collectors are attached and closes after
//! the last drain, which is milliseconds either side.

use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;

use serde::{Deserialize, Serialize};

use crate::clock::monotonic_ns;

/// When a launched program ran, on `CLOCK_MONOTONIC`.
///
/// The start is exact and the end is not, and the struct says by how much. The
/// gate is a write this process makes, so the moment before it is a bound the
/// program cannot have started ahead of. The exit is something the kernel
/// tells this process about when it next asks, so all that is known is the last
/// time the program was seen alive and the first time it was seen gone: the
/// exit happened in `[exited_ns - exited_err_ns, exited_ns]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lifetime {
    /// `CLOCK_MONOTONIC` just before the write that released the gate. The
    /// program's first instruction is after it.
    pub released_ns: u64,
    /// `CLOCK_MONOTONIC` when the wait found the program gone. Its exit is at
    /// or before this.
    pub exited_ns: u64,
    /// How long before `exited_ns` the program was last seen alive: the exit
    /// happened somewhere in that interval, and nothing here can say where.
    pub exited_err_ns: u64,
    /// The program did not end by itself: the run's duration elapsed, or a
    /// collector failed, and this tool killed it. `exited_ns` is then the
    /// reap after the kill, which is the tool's deadline rather than anything
    /// the program did.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub terminated: bool,
    /// User plus system cpu the program spent, as the kernel handed it over at
    /// the reap: `wait4`'s `rusage`, exact to the microsecond.
    ///
    /// It is here because it is the one cpu figure for a launched program that
    /// nothing has to round. `cpu.oncpu_time` reads `/proc/<pid>/stat` in ticks
    /// of a hundredth of a second and stops being able to read anything the
    /// moment the process is gone, so its series is short by the cpu spent
    /// after the last poll -- always by a multiple of the tick, and by 10 to
    /// 26 ms in the seventh wave's measurements. The series stays what it is,
    /// a series; this is the total it sums towards.
    ///
    /// Absent when the program has not been reaped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_ns: Option<u64>,
}

/// User plus system cpu of one `rusage`, in nanoseconds.
fn cpu_ns(usage: &libc::rusage) -> u64 {
    let of = |time: &libc::timeval| {
        (time.tv_sec.max(0) as u64) * 1_000_000_000 + (time.tv_usec.max(0) as u64) * 1_000
    };
    of(&usage.ru_utime) + of(&usage.ru_stime)
}

/// A program forked and waiting for permission to exec.
pub struct Gated {
    pid: i32,
    /// Written to once, to release the child.
    gate: i32,
    /// Readable once the program has exited, so a run can sleep on it rather
    /// than finding out a tick late. `-1` where `pidfd_open` is refused, and
    /// then the tick is what the exit is known to.
    pidfd: RawFd,
    status: Option<i32>,
    released_ns: Option<u64>,
    /// The last moment the program is known to have been alive: the instant
    /// before a wait that found it still running.
    alive_ns: u64,
    exited: Option<(u64, u64)>,
    terminated: bool,
    /// The child's own cpu accounting, taken from the reap.
    cpu_ns: Option<u64>,
}

impl Gated {
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// A descriptor that becomes readable when the program exits, when the
    /// kernel would give one.
    pub fn exit_fd(&self) -> Option<RawFd> {
        (self.pidfd >= 0).then_some(self.pidfd)
    }

    /// Lets the child exec.
    pub fn release(&mut self) {
        if self.gate >= 0 {
            let go = [1u8];
            let released_ns = monotonic_ns();
            // SAFETY: writing one byte to a pipe this process owns.
            unsafe {
                libc::write(self.gate, go.as_ptr() as *const libc::c_void, 1);
                libc::close(self.gate);
            }
            self.gate = -1;
            self.released_ns = Some(released_ns);
            self.alive_ns = released_ns;
        }
    }

    /// Whether the child has exited, without blocking.
    pub fn finished(&mut self) -> Option<String> {
        if let Some(status) = self.status {
            return Some(describe(status));
        }
        let asked_ns = monotonic_ns();
        let mut status = 0;
        // `wait4` rather than `waitpid` for one field: the kernel hands over
        // the child's own cpu accounting at the reap, exact to the microsecond,
        // where `/proc/<pid>/stat` is in ticks of a hundredth of a second and
        // stops being readable the moment the process is gone. That is the
        // difference `cpu.oncpu_time` on a launched program was short by --
        // always a multiple of the tick, because the cpu spent after the last
        // poll is in no reading.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: waiting on a child of this process, without blocking, into
        // two locals this function owns.
        let result = unsafe { libc::wait4(self.pid, &mut status, libc::WNOHANG, &mut usage) };
        if result == self.pid {
            let seen_ns = monotonic_ns();
            self.status = Some(status);
            self.cpu_ns = Some(cpu_ns(&usage));
            self.exited = Some((seen_ns, seen_ns.saturating_sub(self.alive_ns)));
            Some(describe(status))
        } else {
            self.alive_ns = asked_ns;
            None
        }
    }

    /// Ends the child and everything it started, and reaps it, so the tool
    /// leaves nothing behind.
    ///
    /// The whole tree, not the launched process alone: a compiler driver
    /// killed on its own leaves `cc1` running, reparented, still writing the
    /// object a retry is about to write too (`docs/fixes/w17-build.md`).
    /// The tree is frozen first ([`freeze`]) so that nothing in it can fork
    /// past the kill, then killed, and a process group the program made
    /// for itself goes with it. The program is not put in a group of its own
    /// at launch: a process group that is not the terminal's foreground one
    /// is stopped the moment it reads the terminal, and a target that reads
    /// stdin would hang under the tool.
    pub fn terminate(&mut self) {
        if self.status.is_some() {
            return;
        }
        let tree = freeze(self.pid);
        // SAFETY: signalling the processes this program started, then
        // signalling and reaping the child of this process.
        unsafe {
            // A group the program leads is one it made for itself (`setsid`,
            // a shell's job control); its members may have left the tree.
            if libc::getpgid(self.pid) == self.pid {
                libc::kill(-self.pid, libc::SIGKILL);
            }
            for descendant in &tree {
                libc::kill(*descendant, libc::SIGKILL);
            }
            libc::kill(self.pid, libc::SIGKILL);
            let mut status = 0;
            let mut usage: libc::rusage = std::mem::zeroed();
            libc::wait4(self.pid, &mut status, 0, &mut usage);
            self.status = Some(status);
            self.cpu_ns = Some(cpu_ns(&usage));
        }
        let reaped_ns = monotonic_ns();
        // Its end is somewhere after it was last seen running, which may be
        // before the signal: a program that exited a moment before it was
        // killed is a zombie the signal still reaches, and nothing tells the
        // two apart. So the interval starts there, not at the kill.
        self.exited = Some((reaped_ns, reaped_ns.saturating_sub(self.alive_ns)));
        self.terminated = true;
    }

    /// When the program ran, once it has been released and has ended. `None`
    /// before either: a program that never passed the gate never ran, and one
    /// still running has no end to report.
    pub fn lifetime(&self) -> Option<Lifetime> {
        let released_ns = self.released_ns?;
        let (exited_ns, exited_err_ns) = self.exited?;
        Some(Lifetime {
            released_ns,
            exited_ns,
            exited_err_ns,
            terminated: self.terminated,
            cpu_ns: self.cpu_ns,
        })
    }
}

impl Drop for Gated {
    fn drop(&mut self) {
        // A child still parked at the gate would wait forever.
        self.release();
        self.terminate();
        if self.pidfd >= 0 {
            // SAFETY: closing a descriptor this handle owns, once.
            unsafe { libc::close(self.pidfd) };
            self.pidfd = -1;
        }
    }
}

/// Stops `pid` and every descendant it has, until a pass finds none that was
/// not already stopped, and returns the descendants.
///
/// A stopped process cannot fork, and a fork with a signal pending is
/// restarted rather than completed, so once a pass finds nothing new the
/// tree is fixed and a kill reaches all of it. Bounded, so that a tree
/// growing faster than it can be walked still ends; whatever is left then is
/// what the kill misses.
fn freeze(pid: i32) -> Vec<i32> {
    // SAFETY: stopping a child of this process.
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let mut stopped: Vec<i32> = Vec::new();
    for _ in 0..64 {
        let new: Vec<i32> = super::task::descendants(pid)
            .into_iter()
            .filter(|descendant| !stopped.contains(descendant))
            .collect();
        if new.is_empty() {
            break;
        }
        for descendant in new {
            // SAFETY: stopping a process the launched program started.
            unsafe { libc::kill(descendant, libc::SIGSTOP) };
            stopped.push(descendant);
        }
    }
    stopped
}

fn describe(status: i32) -> String {
    if libc::WIFEXITED(status) {
        format!("exit status {}", libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        format!("signal {}", libc::WTERMSIG(status))
    } else {
        format!("status {status}")
    }
}

/// Forks a child that waits at a gate, then execs `argv` when released.
///
/// `environment` is added to the child's environment, which is how a preloaded
/// shim is told where to send what it sees.
pub fn gated(argv: &[String], environment: &[(String, String)]) -> io::Result<Gated> {
    let (program, _) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no program to launch"))?;

    // Everything the child needs is built before the fork: between fork and
    // exec only async-signal-safe calls are allowed, and allocating is not one.
    let c_program = CString::new(program.as_str())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "program name contains a NUL"))?;
    let c_argv: Vec<CString> = argv
        .iter()
        .map(|argument| {
            CString::new(argument.as_str())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "argument contains a NUL"))
        })
        .collect::<io::Result<_>>()?;
    let mut argv_pointers: Vec<*const libc::c_char> =
        c_argv.iter().map(|argument| argument.as_ptr()).collect();
    argv_pointers.push(std::ptr::null());

    let mut variables: Vec<CString> = std::env::vars()
        .filter(|(name, _)| !environment.iter().any(|(added, _)| added == name))
        .chain(environment.iter().cloned())
        .map(|(name, value)| {
            CString::new(format!("{name}={value}")).unwrap_or_else(|_| CString::new("").unwrap())
        })
        .collect();
    variables.retain(|variable| !variable.as_bytes().is_empty());
    let mut environment_pointers: Vec<*const libc::c_char> =
        variables.iter().map(|variable| variable.as_ptr()).collect();
    environment_pointers.push(std::ptr::null());

    let mut pipe_fds = [0i32; 2];
    // SAFETY: `pipe` fills two file descriptors into an array we own.
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (read_fd, write_fd) = (pipe_fds[0], pipe_fds[1]);

    // SAFETY: this process is single threaded at launch time, and the child
    // path below calls only async-signal-safe functions before exec.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
        return Err(error);
    }

    if pid == 0 {
        // Child: wait at the gate, then become the program.
        unsafe {
            // A process forked from a binary that carries file capabilities
            // inherits `dumpable = 0`, which makes its own /proc entries
            // root-owned and unreadable to the user who started the run. The
            // measured program is not the privileged one and has no reason to
            // be protected from its own user, and without this every procfs
            // collector reads EACCES on a program this tool launched -- but
            // only once the tool has been given capabilities, which is the
            // worst time to discover it.
            libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0);
            libc::close(write_fd);
            let mut go = [0u8; 1];
            // A short read or an interrupted one still means "go": the parent
            // only ever writes when it is ready, and closing the pipe without
            // writing means it gave up.
            libc::read(read_fd, go.as_mut_ptr() as *mut libc::c_void, 1);
            libc::close(read_fd);
            libc::execvpe(
                c_program.as_ptr(),
                argv_pointers.as_ptr(),
                environment_pointers.as_ptr(),
            );
            // Only reached when exec failed; 127 is what a shell reports for
            // "command not found", and the parent surfaces it as the exit.
            libc::_exit(127);
        }
    }

    // SAFETY: closing the end of the pipe this process does not use.
    unsafe { libc::close(read_fd) };
    // SAFETY: `pidfd_open` on a child of this process that cannot have been
    // reaped yet, so the pid still names it. Refused only on a kernel older
    // than 5.3, below this project's floor, and then the run falls back to its
    // tick.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as RawFd;
    if pidfd >= 0 {
        // SAFETY: marking a descriptor this process just opened.
        unsafe { libc::fcntl(pidfd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok(Gated {
        cpu_ns: None,
        pid,
        gate: write_fd,
        pidfd: pidfd.max(-1),
        status: None,
        released_ns: None,
        alive_ns: 0,
        exited: None,
        terminated: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_waits_at_the_gate_until_it_is_released() {
        let mut child = gated(&["/bin/true".to_owned()], &[]).expect("fork");
        // Nothing released it yet, so it cannot have run.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            child.finished().is_none(),
            "the child ran before it was released"
        );

        child.release();
        for _ in 0..100 {
            if let Some(status) = child.finished() {
                assert_eq!(status, "exit status 0");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the child never finished after being released");
    }

    #[test]
    fn a_program_that_does_not_exist_exits_with_the_shell_convention() {
        let mut child = gated(&["/nonexistent/program".to_owned()], &[]).expect("fork");
        child.release();
        for _ in 0..100 {
            if let Some(status) = child.finished() {
                assert_eq!(status, "exit status 127");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the child never finished");
    }

    /// A gated child is parked in a `read` that only the parent can end. If the
    /// parent goes away without releasing it -- a failed collector, a panic,
    /// an early return -- the child would wait forever, holding whatever the
    /// caller wanted measured. Dropping the handle has to end it.
    #[test]
    fn dropping_the_handle_does_not_leave_the_child_parked_forever() {
        let pid = {
            let child = gated(&["/bin/sleep".to_owned(), "30".to_owned()], &[]).expect("fork");
            child.pid()
        };

        // The child is gone once it is reaped, so its pid stops naming a live
        // process. Give the kernel a moment to finish the teardown.
        let mut alive = true;
        for _ in 0..200 {
            // SAFETY: signal zero checks for existence without sending one.
            if unsafe { libc::kill(pid, 0) } != 0 {
                alive = false;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !alive,
            "process {pid} is still running after its handle was dropped"
        );
    }

    #[test]
    fn terminating_a_running_child_reaps_it_rather_than_leaving_a_zombie() {
        // `sleep` rather than a program that reads stdin: a child inherits the
        // test harness's stdin, which is closed, so anything reading it sees end
        // of file and exits before the test can terminate it.
        let mut child = gated(&["/bin/sleep".to_owned(), "30".to_owned()], &[]).expect("fork");
        child.release();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            child.finished().is_none(),
            "sleep 30 should still be running"
        );

        child.terminate();
        let status = child.finished().expect("a terminated child has a status");
        assert!(
            status.contains("signal"),
            "killed by a signal, not exited: {status}"
        );

        // Reaped: waiting on it again must fail with ECHILD rather than
        // returning a pid, which is what a lingering zombie would do.
        let mut ignored = 0;
        // SAFETY: waiting on a pid this process created and already reaped.
        let result = unsafe { libc::waitpid(child.pid(), &mut ignored, libc::WNOHANG) };
        assert_eq!(result, -1, "the child was not reaped and is now a zombie");
    }

    /// Whether `pid` is gone or only waiting to be reaped by whoever inherited
    /// it: either way it runs no more.
    fn ended(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rfind(')')
                .and_then(|end| stat[end + 1..].split_whitespace().next())
                .is_some_and(|state| state == "Z" || state == "X"),
        }
    }

    /// The stop ends what the program started, not its main process alone:
    /// a shell waiting on one child with another in the background, each with
    /// a child of its own, is ended whole, and nothing it started is left
    /// running, reparented, once `terminate` returns.
    #[test]
    fn terminating_a_program_ends_every_process_it_started() {
        let mut child = gated(
            &[
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "sh -c 'sleep 30; :' & sh -c 'sleep 30; :'; :".to_owned(),
            ],
            &[],
        )
        .expect("fork");
        child.release();
        let mut tree = Vec::new();
        for _ in 0..500 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            tree = crate::collect::task::descendants(child.pid());
            if tree.len() >= 4 {
                break;
            }
        }
        assert!(
            tree.len() >= 4,
            "two shells and their two sleeps had to start: {tree:?}"
        );

        child.terminate();
        let mut left: Vec<i32> = tree.clone();
        for _ in 0..200 {
            left.retain(|pid| !ended(*pid));
            if left.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            left.is_empty(),
            "still running after the stop: {left:?} of {tree:?}"
        );
    }

    /// A launched program must be readable in procfs by the user who launched
    /// it, whatever privileges the tool itself was given.
    #[test]
    fn a_launched_program_is_readable_in_procfs_by_its_launcher() {
        let mut child = gated(&["/bin/sleep".to_owned(), "30".to_owned()], &[]).expect("fork");
        child.release();
        // Give the exec a moment; before it, the child is still this process's
        // fork and the question is not yet meaningful.
        std::thread::sleep(std::time::Duration::from_millis(100));

        let fds = format!("/proc/{}/fd", child.pid());
        let listed = std::fs::read_dir(&fds);
        child.terminate();
        assert!(
            listed.is_ok(),
            "{fds} is not readable by the user who launched it: {:?}",
            listed.err()
        );
    }

    #[test]
    fn the_child_environment_carries_what_was_added() {
        let marker = "OMN_LAUNCH_TEST";
        let mut child = gated(
            &[
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                format!("test \"${marker}\" = ok"),
            ],
            &[(marker.to_owned(), "ok".to_owned())],
        )
        .expect("fork");
        child.release();
        for _ in 0..100 {
            if let Some(status) = child.finished() {
                assert_eq!(
                    status, "exit status 0",
                    "the variable did not reach the child"
                );
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the child never finished");
    }
}
