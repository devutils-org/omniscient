//! What the family's tests need in order to *cause* the thing they measure.
//!
//! Ground truth for this family means making a known amount of memory exist,
//! putting it somewhere the kernel accounts separately, and then checking that
//! the collector saw the amount that was made. Two pieces are needed for that,
//! and neither belongs in a collector: a cgroup this user may create and limit,
//! and a process that touches a known number of bytes inside it and then holds
//! still while it is measured.
//!
//! The cgroup is made under the session's own delegation root rather than
//! anywhere fixed. systemd delegates `user@<uid>.service` to the user who owns
//! the session, so a cgroup created there needs no privileges at all; a machine
//! that delegates nothing answers `None`, which is a machine that cannot serve
//! the request rather than a test that failed.
//!
//! The process is a fork of the test binary. It writes its own pid into
//! `cgroup.procs`, faults in the bytes it was asked for, raises a flag in a
//! mapping shared with its parent, and waits to be killed. A launched
//! interpreter would do as well and is what the integration tests use; in a
//! unit test a fork costs nothing and depends on nothing being installed.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::cgroup::ROOT;
use crate::clock::monotonic_ns;

/// A cgroup that exists for the duration of one test.
pub(super) struct Scratch {
    path: PathBuf,
    id: u64,
}

impl Scratch {
    /// Creates a cgroup under this session's delegation root, or answers
    /// `None` where this user may not create one.
    pub fn cgroup(name: &str) -> Option<Self> {
        let parent = delegation_root()?;
        // The memory controller has to be enabled on the parent for the child
        // to have any of these files at all. Where it is not, this user cannot
        // enable it either, and there is nothing to measure.
        let controllers = std::fs::read_to_string(parent.join("cgroup.subtree_control")).ok()?;
        if !controllers.split_whitespace().any(|name| name == "memory") {
            return None;
        }
        let path = parent.join(format!(
            "omn-{name}-{}-{}",
            std::process::id(),
            monotonic_ns()
        ));
        std::fs::create_dir(&path).ok()?;
        let id = std::fs::metadata(&path).ok()?.ino();
        Some(Self { path, id })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// Writes one of the cgroup's own control files, answering whether it took.
    pub fn write(&self, file: &str, value: &str) -> bool {
        std::fs::write(self.path.join(file), value).is_ok()
    }

    /// Why a child of this cgroup could not push `needed` bytes out to swap,
    /// or `None` when it could: the free room the swap devices have, and the
    /// `memory.swap.max` of this cgroup and every ancestor that has one.
    ///
    /// A limited cgroup that touches more than its limit either swaps or is
    /// killed, and a child that is killed never reports that it finished --
    /// which on the CI's second runner read as *"the child never finished
    /// touching its memory"* after thirty seconds, three times, and as a
    /// zero-filled cgroup holding 23 MB of swap where it had 160 MB to hold.
    /// Both are the machine's swap, not the collector; this says so with the
    /// numbers, before anything is forked.
    pub fn no_swap_room(&self, needed: u64) -> Option<String> {
        let free = free_swap();
        if free < needed {
            return Some(format!(
                "this machine cannot serve a cgroup that has to swap: its swap devices have \
                 {free} bytes free and a child of a limited cgroup has to push about {needed} \
                 out (/proc/swaps)"
            ));
        }
        let mut at = Some(self.path.as_path());
        while let Some(dir) = at {
            let Ok(text) = std::fs::read_to_string(dir.join("memory.swap.max")) else {
                break;
            };
            if let Ok(limit) = text.trim().parse::<u64>() {
                if limit < needed {
                    return Some(format!(
                        "this machine cannot serve a cgroup that has to swap: {}/memory.swap.max \
                         is {limit} bytes and a child of a limited cgroup has to push about \
                         {needed} out",
                        dir.display()
                    ));
                }
            }
            at = dir.parent();
        }
        None
    }

    /// Forks a process into this cgroup, has it touch `bytes` of anonymous
    /// memory, and returns once every page of it has been written.
    ///
    /// Written rather than merely mapped: `malloc` and `mmap` allocate nothing,
    /// and a test that measured an untouched mapping would be measuring
    /// address space rather than memory. That distinction is this family's
    /// cardinal rule and is the easiest way to write a test that proves
    /// nothing.
    pub fn spawn_toucher(&self, bytes: u64) -> Toucher {
        Toucher::start(Some(&self.path.join("cgroup.procs")), bytes)
    }

    /// The same, but every page is left full of zeros.
    ///
    /// The kernel does not write a zero-filled page to a swap device: it
    /// records that the page was zero and counts it in `swpout_zero`, so the
    /// cgroup's swap *stock* grows while its swap *flow* does not. A test that
    /// allocated a `bytearray` and expected `pswpout` to move is measuring
    /// that, and the difference between the two children is the only way to
    /// tell it apart from a collector that reads the wrong field.
    pub fn spawn_zero_toucher(&self, bytes: u64) -> Toucher {
        Toucher::spawn_filled(Some(&self.path.join("cgroup.procs")), bytes, None, 0)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A cgroup with a process still in it cannot be removed, which is why
        // every test kills its child before the scratch goes out of scope. A
        // failure here is not worth panicking over in a destructor: it leaves
        // an empty directory behind and nothing else.
        let _ = std::fs::remove_dir(&self.path);
    }
}

/// The topmost cgroup this user may create children in.
///
/// Delegation is expressed as ownership: systemd chowns the session's own
/// subtree to the user. So the root of what this user controls is the highest
/// ancestor of its own cgroup that it can still write to.
fn delegation_root() -> Option<PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let relative = text.lines().find_map(|line| line.strip_prefix("0::"))?;
    let mut current = PathBuf::from(ROOT).join(relative.trim().trim_start_matches('/'));
    let mut best = None;
    while current.starts_with(ROOT) {
        if writable(&current) {
            best = Some(current.clone());
        }
        if current == Path::new(ROOT) {
            break;
        }
        current = current.parent()?.to_path_buf();
    }
    best
}

fn writable(directory: &Path) -> bool {
    // SAFETY: `access` reads a path and a mode and touches nothing else.
    let path = std::ffi::CString::new(directory.as_os_str().as_encoded_bytes()).ok();
    match path {
        Some(path) => (unsafe { libc::access(path.as_ptr(), libc::W_OK) }) == 0,
        None => false,
    }
}

/// A forked process holding a known amount of resident anonymous memory.
pub(super) struct Toucher {
    pid: i32,
    handshake: *mut libc::c_void,
    stopped: bool,
}

impl Toucher {
    /// Starts a process that touches `bytes` and then waits.
    ///
    /// `join` names a `cgroup.procs` the child writes itself into before it
    /// allocates anything, so that every page it faults is charged there.
    pub fn start(join: Option<&Path>, bytes: u64) -> Self {
        Self::spawn(join, bytes, None)
    }

    /// Starts a process that makes `resident` bytes resident and then spends
    /// the rest of its life touching only the first `working` of them.
    ///
    /// That is the shape a working-set measurement exists to find, and it
    /// cannot be produced by allocating alone: a process that touches
    /// everything it holds has a working set equal to its resident set, and a
    /// collector that reported the resident set under either name would pass.
    pub fn walker(resident: u64, working: u64) -> Self {
        Self::spawn(None, resident, Some(working))
    }

    /// The pages are filled with `0xA5` rather than with zeros, and that is not
    /// arbitrary: the kernel never writes a zero-filled page to a swap device,
    /// so a child that allocated and left its memory zero would be reclaimed
    /// without a single page of swap traffic.
    fn spawn(join: Option<&Path>, bytes: u64, working: Option<u64>) -> Self {
        Self::spawn_filled(join, bytes, working, 0xA5)
    }

    fn spawn_filled(join: Option<&Path>, bytes: u64, working: Option<u64>, fill: u8) -> Self {
        let page = page_size();
        // One page, shared with the child, holding the flag that says the
        // memory exists. A sleep would be a guess; this is the fact.
        // SAFETY: an anonymous shared mapping of our own, unmapped in `stop`.
        let handshake = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(handshake, libc::MAP_FAILED, "the handshake page must map");

        let joined = join.map(|path| {
            std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("a cgroup path")
        });

        // SAFETY: the child calls only async-signal-safe things -- write,
        // mmap, memset on its own mapping, pause, _exit -- so a lock held by
        // another thread of this test binary at fork cannot deadlock it.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork must succeed");
        if pid == 0 {
            // SAFETY: the child's own file descriptor and its own mappings.
            unsafe {
                if let Some(path) = joined {
                    let fd = libc::open(path.as_ptr(), libc::O_WRONLY);
                    if fd >= 0 {
                        let own = format!("{}\n", libc::getpid());
                        libc::write(fd, own.as_ptr().cast(), own.len());
                        libc::close(fd);
                    }
                }
                let block = libc::mmap(
                    std::ptr::null_mut(),
                    bytes as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                );
                // A block of zeros is small pages too: a huge page that is
                // all zeros, split under reclaim, can have its zero subpages
                // remapped to the shared zero page and freed
                // (`thp_underused_split_page`, 6.12 on), which takes them out
                // of the cgroup without a swap entry -- and the zero-page test
                // is about what the swap path does with them.
                if block != libc::MAP_FAILED && (working.is_some() || fill == 0) {
                    // A walker's pages are small pages, on purpose: the
                    // referenced bit is set when the cpu walks the page tables,
                    // and a working set of eight huge pages stays in the TLB,
                    // so after a clear nothing walks them and nothing sets it
                    // -- measured, 4096 bytes referenced of 16 MiB touched
                    // twice. Four thousand small pages do not fit, so every
                    // walk walks the tables.
                    libc::madvise(block, bytes as usize, libc::MADV_NOHUGEPAGE);
                }
                if block != libc::MAP_FAILED {
                    // Every page written, not just mapped.
                    std::ptr::write_bytes(block.cast::<u8>(), fill, bytes as usize);
                }
                handshake.cast::<u8>().write_volatile(1);
                match working {
                    // Touch one byte per page of the working set, over and
                    // over, and never look at the rest again. The write is
                    // volatile so that nothing here can be optimised into a
                    // loop that touches no memory at all.
                    // Each completed walk is counted in the handshake page's
                    // second word, so a test waits for walks rather than for
                    // time: under load a child descheduled for most of a
                    // fixed sleep walked fewer times than the sleep assumed.
                    Some(working) if block != libc::MAP_FAILED => loop {
                        for offset in (0..working as usize).step_by(page) {
                            block.cast::<u8>().add(offset).write_volatile(fill ^ 0xFF);
                        }
                        let walks = handshake.cast::<u64>().add(1);
                        walks.write_volatile(walks.read_volatile() + 1);
                    },
                    _ => {
                        libc::pause();
                        libc::_exit(0);
                    }
                }
            }
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        // SAFETY: reading the first byte of our own shared mapping.
        while unsafe { handshake.cast::<u8>().read_volatile() } == 0 {
            // A child that died is said to have died, with how, rather than
            // waited on for thirty seconds and reported as slow.
            let mut status = 0;
            // SAFETY: a non-blocking wait on our own child.
            if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid {
                let events = join
                    .and_then(|procs| procs.parent())
                    .and_then(|dir| std::fs::read_to_string(dir.join("memory.events")).ok())
                    .unwrap_or_default();
                panic!(
                    "the child died before it finished touching its memory (wait status \
                     {status:#x}, signal {}); its cgroup's memory.events: {}",
                    libc::WTERMSIG(status),
                    events.split_whitespace().collect::<Vec<_>>().join(" ")
                );
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the child never finished touching its memory"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        Self {
            pid,
            handshake,
            stopped: false,
        }
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// How many times a walker has walked its working set.
    pub fn walks(&self) -> u64 {
        // SAFETY: the second word of our own shared mapping, which only the
        // child writes.
        unsafe { self.handshake.cast::<u64>().add(1).read_volatile() }
    }

    /// Waits until a walker has walked its working set `more` times from now,
    /// which is an event the child announces rather than a guessed duration.
    pub fn wait_for_walks(&self, more: u64) {
        let target = self.walks() + more;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while self.walks() < target {
            assert!(
                std::time::Instant::now() < deadline,
                "the walker did not walk {more} more times in 30 s"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Kills the child and waits for it, so that a cgroup holding it can be
    /// removed.
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        // SAFETY: the child is ours, and so is the mapping.
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
            libc::waitpid(self.pid, std::ptr::null_mut(), 0);
            libc::munmap(self.handshake, page_size());
        }
    }
}

impl Drop for Toucher {
    fn drop(&mut self) {
        self.stop();
    }
}

fn page_size() -> usize {
    // SAFETY: `sysconf` takes an integer and returns one.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as usize
    } else {
        4096
    }
}

/// The free room on every swap device, in bytes: `/proc/swaps`' size less
/// its used, summed. Zero where there is none or it cannot be read.
pub(super) fn free_swap() -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/swaps") else {
        return 0;
    };
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let size: u64 = fields.get(2)?.parse().ok()?;
            let used: u64 = fields.get(3)?.parse().ok()?;
            Some(size.saturating_sub(used) * 1024)
        })
        .sum()
}
