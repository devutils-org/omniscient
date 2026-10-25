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
    /// How many processes the program started the stop found and could not
    /// signal (a setuid child: `EPERM`), and which are therefore still
    /// running: the tool says what it left behind rather than claim it left
    /// nothing.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub left_running: u32,
    /// How many processes the end of the run ended that had left the
    /// program's tree before it -- orphaned, their parent gone first, and
    /// adopted by this process ([`adopt_orphans`]) -- and that a walk of the
    /// program's tree no longer finds. Counted, so a run says it reached them.
    ///
    /// At a stop, and when the program exited on its own too: a double-forked
    /// daemon outlives a program that ends by itself as surely as one that is
    /// stopped. One that had already exited is reaped and not counted.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub adopted_ended: u32,
    /// The program's exit code, when it exited rather than being killed by a
    /// signal: what a shell's `$?` would say, so a caller reading the results
    /// need not wait on the program itself to know whether it succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The signal that ended the program, when one did -- `9` for a program
    /// the run stopped (`terminated`), or whatever the program died of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

/// Whether this process adopts the orphans of the programs it launches
/// ([`adopt_orphans`]).
static ADOPTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Makes this process the subreaper of what it launches: a descendant of the
/// launched program whose parent exits before it is reparented here rather
/// than to init, where a stop can still find it. Without it, a process that
/// was orphaned and left the program's process group (`setsid`, a daemon's
/// double fork) was in no tree a stop walks, and outlived the run.
///
/// For the `omn` binary alone, which starts no process of its own that
/// outlives the call that started it: at a stop, every child of this process
/// other than the program is then one of the program's, and is ended with it.
/// A library caller -- a test harness with other tests' children beside the
/// program -- never calls this, and a stop then ends the program's tree only.
pub fn adopt_orphans() -> bool {
    // SAFETY: a prctl that takes no pointer.
    let adopted = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0;
    ADOPTING.store(adopted, std::sync::atomic::Ordering::Relaxed);
    adopted
}

/// Whether this process adopts the orphans of the programs it launches:
/// [`adopt_orphans`] was called, and the kernel took it.
pub(crate) fn adopting() -> bool {
    ADOPTING.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether `pid` has exited and waits to be reaped: a signal still reaches
/// it, and succeeds, without ending anything.
fn is_zombie(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
    })
}

/// The helpers this process is running now ([`helper_output`]), by pid.
static HELPERS: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());

/// What the helpers already reaped spent, in nanoseconds: the share of this
/// process's children account that is not the launched program's.
static HELPERS_SPENT_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The helpers this process is running now: `omn`'s own children, which a
/// walk of its descendants must not take for the program's or its orphans.
pub(crate) fn helpers_running() -> Vec<i32> {
    HELPERS.lock().map(|live| live.clone()).unwrap_or_default()
}

/// What the helpers reaped so far spent, to be taken out of
/// `getrusage(RUSAGE_CHILDREN)`. Read *after* that account: a helper's share is
/// added here before it is reaped into the account, so a read in between
/// weighs the program short by the helper, never long.
pub(crate) fn helpers_spent_ns() -> u64 {
    HELPERS_SPENT_NS.load(std::sync::atomic::Ordering::SeqCst)
}

/// Runs a helper this process needs -- `curl` fetching a debug file, a
/// container runtime naming a pid -- as `Command::output` runs it, but kept
/// apart from the launched program. While it runs it is in
/// [`helpers_running`], so neither the program's cpu account nor the end of
/// the run ([`end_adopted`]) takes it for an orphan; once it has exited, what
/// it spent (its children's included, as `getrusage` gives a zombie's) is
/// added to [`helpers_spent_ns`] *before* it is reaped into this process's
/// children account, so the program's denominator never holds it.
pub(crate) fn helper_output(
    command: &mut std::process::Command,
) -> io::Result<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id() as i32;
    if let Ok(mut live) = HELPERS.lock() {
        live.push(pid);
    }
    let mut stderr = child.stderr.take();
    let reading = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(stderr) = stderr.as_mut() {
            let _ = stderr.read_to_end(&mut bytes);
        }
        bytes
    });
    let mut stdout = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_end(&mut stdout);
    }
    let stderr = reading.join().unwrap_or_default();
    // Waited for without being reaped, which hands over the zombie's whole
    // usage: the amount the reap is about to add to the children account.
    // SAFETY: `waitid` fills a siginfo and an rusage this function owns.
    let spent = unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let mut usage: libc::rusage = std::mem::zeroed();
        loop {
            let result = libc::syscall(
                libc::SYS_waitid,
                libc::P_PID,
                pid,
                &mut info as *mut libc::siginfo_t,
                libc::WEXITED | libc::WNOWAIT,
                &mut usage as *mut libc::rusage,
            );
            if result == 0 {
                break Some(cpu_ns(&usage));
            }
            if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break None;
            }
        }
    };
    if let Some(spent) = spent {
        HELPERS_SPENT_NS.fetch_add(spent, std::sync::atomic::Ordering::SeqCst);
    }
    let status = child.wait();
    if let Ok(mut live) = HELPERS.lock() {
        live.retain(|helper| *helper != pid);
    }
    Ok(std::process::Output {
        status: status?,
        stdout,
        stderr,
    })
}

/// Ends every process this one adopted, and their descendants, once the
/// program has been reaped: stopped first, as [`freeze`] stops a tree, then
/// killed and reaped. Returns how many it ended and how many it could not
/// signal; one that had exited already is reaped and in neither count.
fn end_adopted() -> (u32, u32) {
    if !ADOPTING.load(std::sync::atomic::Ordering::Relaxed) {
        return (0, 0);
    }
    // SAFETY: getpid cannot fail.
    let me = unsafe { libc::getpid() };
    let mut stopped: Vec<i32> = Vec::new();
    for _ in 0..64 {
        // A helper of this process's own is not an orphan, nor what it runs.
        let helpers = super::task::with_descendants(&helpers_running());
        let new: Vec<i32> = super::task::descendants(me)
            .into_iter()
            .filter(|adopted| !stopped.contains(adopted) && !helpers.contains(adopted))
            .collect();
        if new.is_empty() {
            break;
        }
        for adopted in new {
            // SAFETY: stopping a process the launched program started.
            unsafe { libc::kill(adopted, libc::SIGSTOP) };
            stopped.push(adopted);
        }
    }
    let (mut ended, mut refused) = (0, 0);
    for adopted in &stopped {
        if is_zombie(*adopted) {
            continue;
        }
        // SAFETY: killing a process the launched program started.
        if unsafe { libc::kill(*adopted, libc::SIGKILL) } == 0 {
            ended += 1;
        } else if std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            refused += 1;
        }
    }
    // The adopted are this process's children now, and their killed
    // descendants become so as their parents die: reaped until none is left,
    // bounded, since a child this process could not signal never ends.
    for _ in 0..200 {
        let mut status = 0;
        // SAFETY: reaping any child; the program was reaped before this.
        match unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) } {
            0 => std::thread::sleep(std::time::Duration::from_millis(1)),
            pid if pid > 0 => {}
            _ => break,
        }
    }
    (ended, refused)
}

fn is_zero(count: &u32) -> bool {
    *count == 0
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
    /// What the stop could not signal ([`Lifetime::left_running`]).
    left_running: u32,
    /// What the end of the run ended outside the program's tree
    /// ([`Lifetime::adopted_ended`]).
    adopted_ended: u32,
    /// Whether [`Gated::end_orphans`] has run: once per program.
    orphans_ended: bool,
    /// Whether the exited program has been reaped. [`Gated::finished`] sees
    /// the exit without reaping it, so the zombie's `/proc/<pid>` -- its
    /// `stat`, its cpu times -- is still there for the run's closing round;
    /// the reap is [`Gated::end_orphans`]'s, after the collectors stopped.
    reaped: bool,
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

    /// Whether the child has exited, without blocking, and without reaping
    /// it.
    ///
    /// The exit is seen with `WNOWAIT`, which leaves the program a zombie:
    /// its `/proc/<pid>/stat` stays readable, with the cpu it spent to the
    /// end, until [`Gated::end_orphans`] reaps it once the run's collectors
    /// have taken their closing reads. A reap here made the closing round of
    /// a procfs collector find the program gone, and `cpu.oncpu_time` on
    /// `fs:proc` lost its last interval (sysstat 9).
    pub fn finished(&mut self) -> Option<String> {
        if let Some(status) = self.status {
            return Some(describe(status));
        }
        let asked_ns = monotonic_ns();
        // SAFETY: waiting on a child of this process, without blocking and
        // without reaping it, into a siginfo this function owns.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        // SAFETY: `si_pid` and `si_status` are the fields `waitid` fills
        // for a child that changed state; zero when none did.
        let (pid, code, value) = unsafe { (info.si_pid(), info.si_code, info.si_status()) };
        if result == 0 && pid == self.pid {
            let seen_ns = monotonic_ns();
            let status = wait_status(code, value);
            self.status = Some(status);
            self.exited = Some((seen_ns, seen_ns.saturating_sub(self.alive_ns)));
            Some(describe(status))
        } else {
            self.alive_ns = asked_ns;
            None
        }
    }

    /// Reaps a program [`Gated::finished`] saw exit, once, taking the cpu
    /// accounting the kernel hands over at the reap.
    ///
    /// `wait4` rather than `waitpid` for that one field: exact to the
    /// microsecond, where `/proc/<pid>/stat` is in ticks of a hundredth of a
    /// second -- the difference `cpu.oncpu_time` on a launched program was
    /// short by, always a multiple of the tick.
    fn reap(&mut self) {
        if self.reaped || self.status.is_none() {
            return;
        }
        let mut status = 0;
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: reaping a child of this process that has exited, into two
        // locals this function owns; it is a zombie, so this does not block.
        let result = unsafe { libc::wait4(self.pid, &mut status, 0, &mut usage) };
        self.reaped = true;
        if result == self.pid {
            self.status = Some(status);
            self.cpu_ns = Some(cpu_ns(&usage));
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
    ///
    /// A program that has exited already is not signalled again; what it left
    /// running outside its tree is still ended ([`Gated::end_orphans`]).
    pub fn terminate(&mut self) {
        if self.status.is_some() {
            self.end_orphans();
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
                if libc::kill(*descendant, libc::SIGKILL) != 0 {
                    let error = std::io::Error::last_os_error();
                    // Gone already is what the kill was for.
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        self.left_running += 1;
                    }
                }
            }
            libc::kill(self.pid, libc::SIGKILL);
            let mut status = 0;
            let mut usage: libc::rusage = std::mem::zeroed();
            libc::wait4(self.pid, &mut status, 0, &mut usage);
            self.status = Some(status);
            self.cpu_ns = Some(cpu_ns(&usage));
        }
        self.reaped = true;
        self.end_orphans();
        let reaped_ns = monotonic_ns();
        // Its end is somewhere after it was last seen running, which may be
        // before the signal: a program that exited a moment before it was
        // killed is a zombie the signal still reaches, and nothing tells the
        // two apart. So the interval starts there, not at the kill.
        self.exited = Some((reaped_ns, reaped_ns.saturating_sub(self.alive_ns)));
        self.terminated = true;
    }

    /// Ends what the program left running outside its tree, once it has been
    /// reaped -- by a stop ([`Gated::terminate`]) or because it exited on its
    /// own -- and reaps whatever of it had exited already, so nothing it
    /// started is left behind as a process or a zombie. Once per program; a
    /// no-op before the reap, and for a caller that never adopted
    /// ([`adopt_orphans`]).
    ///
    /// Not at the reap itself ([`Gated::finished`]): the run's collectors are
    /// still attached then, and their last reads belong to the run as it was.
    pub fn end_orphans(&mut self) {
        if self.status.is_none() || self.orphans_ended {
            return;
        }
        // The program first: what follows reaps any child of this process,
        // and the program's own reap is the one that hands over its cpu.
        self.reap();
        self.orphans_ended = true;
        let (ended, refused) = end_adopted();
        self.adopted_ended += ended;
        self.left_running += refused;
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
            left_running: self.left_running,
            adopted_ended: self.adopted_ended,
            exit_code: self
                .status
                .filter(|status| libc::WIFEXITED(*status))
                .map(|status| libc::WEXITSTATUS(status)),
            signal: self
                .status
                .filter(|status| libc::WIFSIGNALED(*status))
                .map(|status| libc::WTERMSIG(status)),
        })
    }
}

impl Drop for Gated {
    fn drop(&mut self) {
        // A child still parked at the gate is killed there, before its exec:
        // a handle dropped unreleased -- a collector that refused to attach
        // after the fork -- means the program was never meant to run, and
        // releasing it first would let it run for as long as the kill takes
        // to land (a write, a file created, a request sent).
        if self.gate >= 0 && self.status.is_none() {
            let mut status = 0;
            // SAFETY: killing and reaping a child of this process that is
            // blocked reading the gate, then closing the gate it owns.
            unsafe {
                libc::kill(self.pid, libc::SIGKILL);
                libc::waitpid(self.pid, &mut status, 0);
                libc::close(self.gate);
            }
            self.gate = -1;
            self.status = Some(status);
            self.reaped = true;
        }
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

/// The wait status `waitpid` would have handed over, from what `waitid`
/// says of a child: an exit code or the signal that ended it.
fn wait_status(code: i32, value: i32) -> i32 {
    match code {
        libc::CLD_EXITED => (value & 0xff) << 8,
        libc::CLD_DUMPED => (value & 0x7f) | 0x80,
        _ => value & 0x7f,
    }
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

/// The variable naming a file the launched program's stderr is written to,
/// rather than the stderr it would share with `omn`.
///
/// A caller reading `omn`'s stderr -- the MCP server's `run_session` -- gets
/// the program's lines mixed into `omn`'s, with nothing to tell them apart. A
/// file of its own separates them at the source, without a guess at whose a
/// line is. The variable is `omn`'s and is not passed on to the program.
pub const PROGRAM_STDERR_VARIABLE: &str = "OMN_PROGRAM_STDERR";

/// The variable naming a file the launched program's stdout is written to,
/// rather than the stdout it would share with `omn`.
///
/// `omn trace ... --format yaml -- ls -R | yq` hands `yq` the program's lines
/// and `omn`'s document in one stream. A file of the program's own leaves
/// `omn`'s stdout holding its document alone. Like the stderr one, it is
/// `omn`'s and is not passed on to the program.
pub const PROGRAM_STDOUT_VARIABLE: &str = "OMN_PROGRAM_STDOUT";

/// Opens the file `variable` names, created or truncated, close-on-exec in
/// this process; `None` when the variable is not set.
fn program_output(variable: &str, path: Option<&std::ffi::OsStr>) -> io::Result<Option<RawFd>> {
    let Some(path) = path.filter(|path| !path.is_empty()) else {
        return Ok(None);
    };
    let c_path = CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL"))?;
    // SAFETY: a NUL-terminated path this frame owns.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC,
            0o644,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!(
                "{variable}={}: {error}",
                std::path::Path::new(&path).display()
            ),
        ));
    }
    Ok(Some(fd))
}

/// Where `execvp` would find `program`: itself when it names a path, the
/// first entry of `path` holding an executable file of that name otherwise,
/// and the name unchanged when none does -- so the one `execve` fails as
/// `execvp`'s last one would, and the child exits 127.
pub(crate) fn find_program(program: &str, path: Option<&std::ffi::OsStr>) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStrExt;
    if program.contains('/') || program.is_empty() {
        return program.into();
    }
    // The default glibc searches when `PATH` is unset.
    let path = path.map_or(&b"/bin:/usr/bin"[..], OsStrExt::as_bytes);
    for directory in path.split(|byte| *byte == b':') {
        // An empty entry is the current directory, as the shell reads it.
        let directory = if directory.is_empty() {
            b"."
        } else {
            directory
        };
        let candidate = std::path::Path::new(std::ffi::OsStr::from_bytes(directory)).join(program);
        let Ok(c_candidate) = CString::new(candidate.as_os_str().as_bytes()) else {
            continue;
        };
        // SAFETY: a NUL-terminated path this frame owns.
        let executable = unsafe { libc::access(c_candidate.as_ptr(), libc::X_OK) } == 0;
        if executable && candidate.metadata().is_ok_and(|meta| meta.is_file()) {
            return candidate;
        }
    }
    program.into()
}

/// Forks a child that waits at a gate, then execs `argv` when released.
///
/// `environment` is added to the child's environment, which is how a preloaded
/// shim is told where to send what it sees.
pub fn gated(argv: &[String], environment: &[(String, String)]) -> io::Result<Gated> {
    gated_with(
        argv,
        environment,
        std::env::var_os(PROGRAM_STDERR_VARIABLE).as_deref(),
        std::env::var_os(PROGRAM_STDOUT_VARIABLE).as_deref(),
    )
}

/// [`gated`], with the program's stderr and stdout written to the files
/// given, when they are.
fn gated_with(
    argv: &[String],
    environment: &[(String, String)],
    stderr: Option<&std::ffi::OsStr>,
    stdout: Option<&std::ffi::OsStr>,
) -> io::Result<Gated> {
    let (program, _) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no program to launch"))?;

    // Everything the child needs is built before the fork: between fork and
    // exec only async-signal-safe calls are allowed, and allocating is not one.
    //
    // The program is found on `PATH` here, in the parent, and the child makes
    // one `execve`. `execvpe` searched in the child, after the gate, so each
    // directory it tried before the one holding the program was an `execve`
    // of the measured program's own -- three failed ones on this machine's
    // `PATH` for `ls`, which `proc.exec` and a `perf trace` beside it counted
    // as the program's (perfsched 2).
    let found = find_program(program, std::env::var_os("PATH").as_deref());
    let c_program = CString::new(found.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "program name contains a NUL"))?;
    // What `execvp` does with a file the kernel does not know how to run --
    // a script with no `#!` line -- is hand it to the shell; built here, as
    // everything the child needs is.
    let c_shell = CString::new("/bin/sh").expect("no NUL");
    let mut shell_pointers: Vec<*const libc::c_char> = vec![c_shell.as_ptr(), c_program.as_ptr()];
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
    shell_pointers.extend(argv_pointers.iter().skip(1));

    let mut variables: Vec<CString> = std::env::vars()
        .filter(|(name, _)| !environment.iter().any(|(added, _)| added == name))
        .chain(environment.iter().cloned())
        .filter(|(name, _)| name != PROGRAM_STDERR_VARIABLE && name != PROGRAM_STDOUT_VARIABLE)
        .map(|(name, value)| {
            CString::new(format!("{name}={value}")).unwrap_or_else(|_| CString::new("").unwrap())
        })
        .collect();
    variables.retain(|variable| !variable.as_bytes().is_empty());
    let mut environment_pointers: Vec<*const libc::c_char> =
        variables.iter().map(|variable| variable.as_ptr()).collect();
    environment_pointers.push(std::ptr::null());

    let stderr_fd = program_output(PROGRAM_STDERR_VARIABLE, stderr)?;
    // One file named for both is opened once: two descriptors of one file,
    // each truncating it and each with its own offset, would write over each
    // other's lines.
    let shared = stdout.is_some() && stdout == stderr;
    let stdout_fd = if shared {
        stderr_fd
    } else {
        match program_output(PROGRAM_STDOUT_VARIABLE, stdout) {
            Ok(fd) => fd,
            Err(error) => {
                if let Some(fd) = stderr_fd {
                    // SAFETY: a descriptor `program_output` opened, closed once.
                    unsafe { libc::close(fd) };
                }
                return Err(error);
            }
        }
    };
    let close_outputs = || {
        if let Some(fd) = stderr_fd {
            // SAFETY: a descriptor `program_output` opened, closed once.
            unsafe { libc::close(fd) };
        }
        if let Some(fd) = stdout_fd.filter(|_| !shared) {
            // SAFETY: as above, and not the stderr one.
            unsafe { libc::close(fd) };
        }
    };

    let mut pipe_fds = [0i32; 2];
    // SAFETY: `pipe` fills two file descriptors into an array we own.
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        close_outputs();
        return Err(io::Error::last_os_error());
    }
    let (read_fd, write_fd) = (pipe_fds[0], pipe_fds[1]);

    let memlock_before = crate::bpf::MEMLOCK_BEFORE.get().copied();
    // SAFETY: this process is single threaded at launch time, and the child
    // path below calls only async-signal-safe functions before exec.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
        close_outputs();
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
            // The memory-lock limit this tool raised for its own maps
            // (`bpf::lift_memlock`) is not the program's: it gets its own back.
            if let Some(before) = memlock_before {
                libc::setrlimit(libc::RLIMIT_MEMLOCK, &before);
            }
            libc::close(write_fd);
            let mut go = [0u8; 1];
            // A short read or an interrupted one still means "go": the parent
            // only ever writes when it is ready, and closing the pipe without
            // writing means it gave up.
            libc::read(read_fd, go.as_mut_ptr() as *mut libc::c_void, 1);
            libc::close(read_fd);
            // The program's own stderr, when one was asked for: `dup2`
            // clears close-on-exec on the copy, so the program keeps it.
            if let Some(fd) = stderr_fd {
                libc::dup2(fd, 2);
            }
            if let Some(fd) = stdout_fd {
                libc::dup2(fd, 1);
            }
            libc::execve(
                c_program.as_ptr(),
                argv_pointers.as_ptr(),
                environment_pointers.as_ptr(),
            );
            if *libc::__errno_location() == libc::ENOEXEC {
                libc::execve(
                    c_shell.as_ptr(),
                    shell_pointers.as_ptr(),
                    environment_pointers.as_ptr(),
                );
            }
            // Only reached when exec failed; 127 is what a shell reports for
            // "command not found", and the parent surfaces it as the exit.
            libc::_exit(127);
        }
    }

    // SAFETY: closing the end of the pipe this process does not use.
    unsafe { libc::close(read_fd) };
    close_outputs();
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
        left_running: 0,
        adopted_ended: 0,
        orphans_ended: false,
        reaped: false,
    })
}

/// Processes a run watches and did not start: a `--pid` target, whose exit
/// ends the run the way a launched program's does, and the way `perf stat -p`
/// ends when its process does.
///
/// Watched through a pidfd each, which becomes readable when its process
/// exits and does not need the process to be this one's child. A pid the
/// kernel will not give a pidfd for -- gone already, or a kernel below 5.3 --
/// is not watched, and the run then ends at its duration as it did.
pub struct Exits {
    watched: Vec<(i32, RawFd)>,
}

impl Exits {
    /// Watches each of `pids` that is running now.
    pub fn of(pids: &[i32]) -> Exits {
        let watched = pids
            .iter()
            .filter_map(|pid| {
                // SAFETY: `pidfd_open` on a pid, which takes no pointer.
                let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, *pid, 0) } as RawFd;
                (fd >= 0).then(|| {
                    // SAFETY: marking a descriptor this process just opened.
                    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
                    (*pid, fd)
                })
            })
            .collect();
        Exits { watched }
    }

    /// Whether every pid asked for is watched: a run whose end is these
    /// exits needs all of them, or one it cannot see would never end it.
    pub fn watches_all(&self, pids: &[i32]) -> bool {
        !self.watched.is_empty() && self.watched.len() == pids.len()
    }

    /// The pids watched that have exited, and the one descriptor a run can
    /// sleep on when it watches one process alone. With several, the first
    /// to exit would stay readable and wake the run on every round, so the
    /// run's own cadence is what notices.
    pub fn wake_fd(&self) -> Option<RawFd> {
        match self.watched.as_slice() {
            [(_, fd)] => Some(*fd),
            _ => None,
        }
    }

    /// Why the run ends, once every watched process has exited.
    pub fn all_exited(&self) -> Option<String> {
        if self.watched.is_empty() {
            return None;
        }
        let gone = self.watched.iter().all(|(_, fd)| {
            let mut watched = libc::pollfd {
                fd: *fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one pollfd this frame owns, without waiting.
            let ready = unsafe { libc::poll(&mut watched, 1, 0) };
            ready > 0 && watched.revents & libc::POLLIN != 0
        });
        gone.then(|| match self.watched.as_slice() {
            [(pid, _)] => format!("pid {pid} exited"),
            several => format!("the {} pids watched exited", several.len()),
        })
    }
}

impl Drop for Exits {
    fn drop(&mut self) {
        for (_, fd) in &self.watched {
            // SAFETY: closing a descriptor this value owns, once.
            unsafe { libc::close(*fd) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A helper's cpu is set aside before it reaches the children account, and
    /// its output is what `Command::output` would have handed back.
    #[test]
    fn a_helper_s_cpu_is_set_aside_from_the_children_account() {
        let children = || {
            // SAFETY: `getrusage` fills a struct this closure owns.
            let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) };
            cpu_ns(&usage)
        };
        let (account, set_aside) = (children(), helpers_spent_ns());
        let output = helper_output(std::process::Command::new("python3").args([
            "-c",
            "import sys, time\nt = time.process_time()\n\
             while time.process_time() - t < 0.2: pass\nprint('burnt')\n\
             print('said', file=sys.stderr)",
        ]));
        let Ok(output) = output else {
            return eprintln!("skipped: no python3 to run as a helper");
        };
        let (account, set_aside) = (children() - account, helpers_spent_ns() - set_aside);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "burnt\n");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "said\n");
        assert!(
            set_aside >= 200_000_000,
            "the helper's 200 ms are set aside: {set_aside} ns"
        );
        assert!(
            account >= set_aside,
            "what is set aside is in the children account once reaped, so taking it out \
             takes out nothing else: {set_aside} ns set aside, {account} ns reaped"
        );
    }

    #[test]
    fn a_child_waits_at_the_gate_until_it_is_released() {
        let mut child = gated(&["true".to_owned()], &[]).expect("fork");
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

    /// A memory-lock limit this tool raised for its own maps is not the
    /// program's: the child gets the limit from before back, before its exec.
    /// Shown with a lower soft limit than this process has, as the "before".
    #[test]
    fn a_launched_program_runs_under_the_memlock_limit_from_before_the_lift() {
        let mut now = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: a valid pointer to an rlimit this frame owns.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut now) },
            0
        );
        let before = libc::rlimit {
            rlim_cur: 32 * 1024,
            rlim_max: now.rlim_max,
        };
        if crate::bpf::MEMLOCK_BEFORE.set(before).is_err() {
            return eprintln!("skipped: the limit from before is already set in this process");
        }
        let out = std::env::temp_dir().join(format!("omn-memlock-{}", std::process::id()));
        let mut child = gated(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                format!("ulimit -l > {}", out.display()),
            ],
            &[],
        )
        .expect("fork");
        ended_by_itself(&mut child);
        let said = std::fs::read_to_string(&out).expect("the child wrote its limit");
        let _ = std::fs::remove_file(&out);
        assert_eq!(said.trim(), "32", "ulimit -l is in KiB");
    }

    fn ended_by_itself(child: &mut Gated) {
        child.release();
        for _ in 0..500 {
            if child.finished().is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the child never finished");
    }

    /// The exit code is in the lifetime, so a reader of the results knows
    /// whether the program succeeded without waiting on it.
    #[test]
    fn the_lifetime_carries_the_exit_code_or_the_signal() {
        let mut child = gated(
            &["sh".to_owned(), "-c".to_owned(), "exit 3".to_owned()],
            &[],
        )
        .expect("fork");
        ended_by_itself(&mut child);
        child.end_orphans();
        let life = child.lifetime().expect("a lifetime");
        assert_eq!((life.exit_code, life.signal), (Some(3), None));
        assert!(life.cpu_ns.is_some(), "the reap hands over the cpu");

        let mut child = gated(&["sleep".to_owned(), "30".to_owned()], &[]).expect("fork");
        child.release();
        child.terminate();
        let life = child.lifetime().expect("a lifetime");
        assert_eq!((life.exit_code, life.signal), (None, Some(9)));
        assert!(life.terminated);
    }

    /// An exit seen is not an exit reaped: the zombie's `stat` is still
    /// there for the closing round, and the reap comes with the end of the
    /// run.
    #[test]
    fn a_program_seen_exiting_is_readable_until_the_run_reaps_it() {
        let mut child = gated(&["true".to_owned()], &[]).expect("fork");
        let pid = child.pid();
        ended_by_itself(&mut child);
        assert!(
            is_zombie(pid),
            "seen exiting and not reaped: /proc/{pid}/stat is still readable"
        );
        child.end_orphans();
        assert!(!is_zombie(pid), "reaped at the end of the run");
        assert_eq!(child.lifetime().and_then(|life| life.exit_code), Some(0));
    }

    /// The search is the parent's: the child makes one `execve`, of the
    /// file the search found, and none the program could be charged with.
    #[test]
    fn the_program_is_found_on_path_before_the_fork() {
        let dir = std::env::temp_dir().join(format!("omn-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = dir.join("omn-found-here");
        std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = std::fs::metadata(&tool).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&tool, permissions).unwrap();
        let path = format!("/nonexistent-a:/nonexistent-b:{}", dir.display());
        assert_eq!(
            find_program("omn-found-here", Some(std::ffi::OsStr::new(&path))),
            tool
        );
        assert_eq!(
            find_program("omn-nowhere", Some(std::ffi::OsStr::new(&path))),
            std::path::PathBuf::from("omn-nowhere"),
            "not found is left to the exec, which fails as execvp's would"
        );
        assert_eq!(
            find_program("./x", Some(std::ffi::OsStr::new(&path))),
            std::path::PathBuf::from("./x")
        );
        // A script with no `#!` line runs through the shell, as under execvp.
        let bare = dir.join("omn-no-shebang");
        std::fs::write(&bare, "exit 4\n").unwrap();
        let mut permissions = std::fs::metadata(&bare).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&bare, permissions).unwrap();
        let mut child = gated(&[bare.display().to_string()], &[]).expect("fork");
        ended_by_itself(&mut child);
        assert_eq!(child.finished().as_deref(), Some("exit status 4"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A gated child is parked in a `read` that only the parent can end. If the
    /// parent goes away without releasing it -- a failed collector, a panic,
    /// an early return -- the child would wait forever, holding whatever the
    /// caller wanted measured. Dropping the handle has to end it.
    #[test]
    fn dropping_the_handle_does_not_leave_the_child_parked_forever() {
        let pid = {
            let child = gated(&["sleep".to_owned(), "30".to_owned()], &[]).expect("fork");
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

    /// A handle dropped before its release ends the child at the gate: the
    /// program never runs, not even for the moment a kill takes to land.
    #[test]
    fn a_child_dropped_unreleased_never_runs_the_program() {
        let marker = std::env::temp_dir().join(format!(
            "omn-launch-unreleased-{}-{}",
            std::process::id(),
            monotonic_ns()
        ));
        for _ in 0..20 {
            let child = gated(
                &[
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    format!("echo ran >> {}", marker.display()),
                ],
                &[],
            )
            .expect("fork");
            drop(child);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        let ran = marker.exists();
        let _ = std::fs::remove_file(&marker);
        assert!(!ran, "a program whose handle was dropped unreleased ran");
    }

    #[test]
    fn terminating_a_running_child_reaps_it_rather_than_leaving_a_zombie() {
        // `sleep` rather than a program that reads stdin: a child inherits the
        // test harness's stdin, which is closed, so anything reading it sees end
        // of file and exits before the test can terminate it.
        let mut child = gated(&["sleep".to_owned(), "30".to_owned()], &[]).expect("fork");
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
        let mut child = gated(&["sleep".to_owned(), "30".to_owned()], &[]).expect("fork");
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

    /// The program's stderr goes to the file the variable names, and the
    /// variable is not passed on to it.
    #[test]
    fn a_program_given_a_stderr_of_its_own_writes_there_and_is_not_told_about_it() {
        let file = std::env::temp_dir().join(format!("omn-launch-stderr-{}", std::process::id()));
        let launched = gated_with(
            &[
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                format!(
                    "echo the program said this >&2; echo \"[${PROGRAM_STDERR_VARIABLE}]\" >&2"
                ),
            ],
            &[(PROGRAM_STDERR_VARIABLE.to_owned(), "set".to_owned())],
            Some(file.as_os_str()),
            None,
        );
        let mut child = launched.expect("fork");
        child.release();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while child.finished().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let written = std::fs::read_to_string(&file).expect("the file was created");
        let _ = std::fs::remove_file(&file);
        assert_eq!(written, "the program said this\n[]\n");
    }

    /// Runs `script` under `sh -c` with its outputs sent to the files given,
    /// and waits for it.
    fn run_to(script: &str, stderr: Option<&std::path::Path>, stdout: Option<&std::path::Path>) {
        let mut child = gated_with(
            &["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
            &[],
            stderr.map(std::path::Path::as_os_str),
            stdout.map(std::path::Path::as_os_str),
        )
        .expect("fork");
        child.release();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while child.finished().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// The program's stdout goes to its own file, its stderr where it was,
    /// and the variable is not passed on to it.
    #[test]
    fn a_program_given_a_stdout_of_its_own_writes_there_and_is_not_told_about_it() {
        let out = std::env::temp_dir().join(format!("omn-launch-stdout-{}", std::process::id()));
        let err =
            std::env::temp_dir().join(format!("omn-launch-stdout-err-{}", std::process::id()));
        run_to(
            &format!("echo to stdout; echo \"[${PROGRAM_STDOUT_VARIABLE}]\"; echo to stderr >&2"),
            Some(&err),
            Some(&out),
        );
        let (written, said) = (std::fs::read_to_string(&out), std::fs::read_to_string(&err));
        let _ = (std::fs::remove_file(&out), std::fs::remove_file(&err));
        assert_eq!(written.expect("the stdout file"), "to stdout\n[]\n");
        assert_eq!(said.expect("the stderr file"), "to stderr\n");
    }

    /// One file named for both holds both, in the order they were written,
    /// rather than one stream written over the other.
    #[test]
    fn one_file_for_both_outputs_keeps_every_line_in_order() {
        let both = std::env::temp_dir().join(format!("omn-launch-both-{}", std::process::id()));
        run_to(
            "echo one; echo two >&2; echo three",
            Some(&both),
            Some(&both),
        );
        let written = std::fs::read_to_string(&both);
        let _ = std::fs::remove_file(&both);
        assert_eq!(written.expect("the file"), "one\ntwo\nthree\n");
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
