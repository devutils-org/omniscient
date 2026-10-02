//! `mem.faults` over `perf_event_open`.
//!
//! Every page fault is a sample, and every sample carries the address that
//! faulted. That address is the whole point: it is what lets a fault be
//! attributed to the allocation whose range contains it, which is the only way
//! to answer "which allocation actually cost memory" rather than "how much
//! memory exists".
//!
//! The kernel pushes these into a ring buffer, so this collector streams: it
//! asks to be drained often, and reports what the ring dropped when it was not
//! drained often enough.
//!
//! **It follows every thread**, the ones the target already has and the ones
//! it starts: an inherited event per thread per cpu, the events of one cpu
//! sharing its ring ([`ThreadSampler`]). Until the eleventh wave it sampled the
//! task it was opened on and said so, and a program that faulted its memory in
//! from a worker thread read as a program that faulted nothing.
//!
//! **`count` mode is the kernel's counter**, not this ring: one inherited
//! software counter per thread, read at the end, through the `cpu` family's
//! counting group ([`crate::collect::cpu::counters::PAGE_FAULTS`]). It has no
//! addresses, and it is exact.

use std::time::Duration as StdDuration;

use perf_event_open_sys::bindings as perf;

use crate::collect::task::Comms;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::perf::{Callchain, Record, SampleSpec, ThreadSampler};
use crate::request::Mode;

/// How often the ring is drained.
///
/// Page faults arrive in bursts -- a program that touches a fresh arena
/// produces thousands in a millisecond -- so the ring is sized generously and
/// drained often. When that is still not enough, the loss counter says so
/// instead of the numbers quietly shrinking.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// Data pages to ask for, per cpu. `perf_event_mlock_kb` usually caps an
/// unprivileged process below this, and each ring shrinks to what the machine
/// allows.
///
/// Per cpu rather than one ring for the whole target, because an inherited
/// event can only be mapped pinned to a cpu. A burst lands on the cpus the
/// faulting threads run on, so a ring a quarter the size of the one the old
/// single-task event had is what one busy cpu gets to itself.
const RING_PAGES: usize = 128;

/// Holds the mode until the target exists.
pub struct Prepare(pub Mode);

impl Prepared for Prepare {
    fn attach(
        self: Box<Self>,
        target: &crate::request::Target,
    ) -> Result<Box<dyn Collector>, String> {
        if self.0 == Mode::Count {
            // Counted, not sampled: the page-fault counter of the `cpu`
            // family's group, which serves every target the catalog declares
            // -- a process through one counter per thread, a cpu or the
            // machine through one per cpu.
            let counted = crate::collect::cpu::counters::Prepare::new(
                &crate::collect::cpu::counters::PAGE_FAULTS,
                Mode::Count,
            )?;
            return Box::new(counted).attach(target);
        }
        // A machine or a set of cores is sampled by one event per cpu, over
        // every task that runs there: the kernel's own shape for the
        // question, and the one the catalog's `system: native` promises.
        let cpus = match target {
            crate::request::Target::System => Some(
                crate::perf::online_cpus()
                    .map_err(|error| format!("cannot tell which cpus are online: {error}"))?,
            ),
            crate::request::Target::Cpu(set) => Some(set.cpus().to_vec()),
            _ => None,
        };
        if let Some(cpus) = cpus {
            let mut collector = PageFaultCollector::new(-1, self.0);
            collector.cpus = Some(cpus);
            return Ok(Box::new(collector));
        }
        // A cgroup is one event per cpu too, each filtered by the kernel to
        // the cgroup's tasks (`crate::collect::cpu::cgroup`).
        if let crate::request::Target::Cgroup(path) = target {
            let cgroup = crate::collect::cpu::cgroup::Cgroup::resolve(path)?;
            let mut collector = PageFaultCollector::new(-1, self.0);
            collector.cpus = Some(
                crate::perf::online_cpus()
                    .map_err(|error| format!("cannot tell which cpus are online: {error}"))?,
            );
            collector.cgroup = Some(cgroup);
            return Ok(Box::new(collector));
        }
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "sampling the page faults of a {} target is not built: faults are sampled per \
                 process, per core, per cgroup or for the whole machine",
                target.kind()
            )
        })?;
        let mut collector = PageFaultCollector::new(pid, self.0);
        // A `--tid` target is that task and what it starts; every other target
        // that names a process is every thread it has.
        collector.whole_process = !matches!(target, crate::request::Target::Tid(_));
        Ok(Box::new(collector))
    }
}

pub struct PageFaultCollector {
    id: CollectorId,
    pid: i32,
    mode: Mode,
    event: Option<ThreadSampler>,
    /// Names by tid, not by the collector's pid: an event opened over a process
    /// receives samples from whichever of its threads faulted, and `comm` is a
    /// declared filter field on this source.
    comms: Comms,
    /// Counted here rather than in the ring so it survives `stop`.
    losses: Losses,
    /// Whether every thread of `pid` is followed rather than `pid` alone.
    whole_process: bool,
    /// The cpus sampled, for a machine or a core target: every task that runs
    /// on them, one event per cpu. `None` samples `pid`.
    cpus: Option<Vec<u32>>,
    /// The cgroup whose tasks are sampled on `cpus`, for a cgroup target.
    cgroup: Option<crate::collect::cpu::cgroup::Cgroup>,
    /// Throttled periods already reported, so that each drain reports what the
    /// sampler's own count moved by.
    throttled: u64,
    /// The tasks faults were sampled from, for the note that says how many.
    sampled_tasks: std::collections::BTreeSet<i32>,
    /// What was opened, read at `start` because `stop` closes it before the
    /// notes are gathered: the threads followed from the start, and the ones
    /// that appeared while the events were being opened.
    followed: (usize, usize),
    /// Whether the machine allowed user-mode faults only, so that the faults
    /// the kernel took on the program's behalf are not in the samples. Set
    /// before `start` by a test, it is the paranoid-2 path taken on a machine
    /// that allows more.
    user_mode_only: bool,
}

/// A refusal in the shape `crate::perf` returns one: the errno's kind, and a
/// message in place of the number. What the paranoid-2 path is taken with when
/// a test forces it, so that the forced path and the machine's are one path.
fn refused_as_perf_says_it() -> std::io::Error {
    let os = std::io::Error::from_raw_os_error(libc::EACCES);
    std::io::Error::new(os.kind(), format!("perf_event_open: {os}"))
}

/// Whether `crate::perf` refused an open for want of privilege, which is what
/// the user-mode retry answers. By kind: EACCES and EPERM are both
/// `PermissionDenied`, and the number itself is not kept.
fn is_refusal(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::PermissionDenied
}

impl PageFaultCollector {
    pub fn new(pid: i32, mode: Mode) -> Self {
        Self {
            id: CollectorId::new("mem.faults/pmu:sw"),
            pid,
            mode,
            event: None,
            comms: Comms::new(),
            losses: Losses::default(),
            whole_process: true,
            cpus: None,
            cgroup: None,
            throttled: 0,
            sampled_tasks: std::collections::BTreeSet::new(),
            followed: (0, 0),
            user_mode_only: false,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        // A sampled fault stands for the period it was taken at: one event of
        // `sample@100` is a hundred faults to every emit that counts them.
        let period = match self.mode {
            Mode::Sample { freq } => freq.as_hz(),
            _ => 1,
        };
        let Some(event) = self.event.as_mut() else {
            return;
        };
        let id = self.id.clone();
        let comms = &mut self.comms;
        let seen = &mut self.sampled_tasks;
        let cgroup_key = self.cgroup.as_ref().map(|cgroup| cgroup.key());
        let mut index = seq;
        let mut dropped = 0u64;
        let mut undecoded = 0u64;
        event.drain(|record| match record {
            Record::Sample(sample) => {
                seen.insert(sample.tid as i32);
                events.push(Event {
                    ts_ns: sample.time_ns,
                    ts_err_ns: 0,
                    kind: "mem.faults".to_owned(),
                    keys: {
                        let mut keys = vec![
                            // First key is the series key, so faults group by
                            // the thread that took them rather than by
                            // address, which would make one series per page.
                            KeyValue::task(sample.tid as i32),
                            // The faulting address, as a one-byte range: an
                            // interval join asks which allocation contains it.
                            KeyValue::Mem {
                                base: sample.addr,
                                len: 1,
                            },
                        ];
                        // The cgroup the kernel filtered the sample to, when
                        // one was asked for.
                        keys.extend(cgroup_key.clone());
                        keys
                    },
                    attrs: {
                        let mut attrs = vec![
                            Attr::uint("count", 1),
                            Attr::uint("addr", sample.addr),
                            Attr::uint("ip", sample.ip),
                        ];
                        if let Some(comm) = comms.get(sample.tid as i32) {
                            attrs.push(Attr::text("comm", comm));
                        }
                        if period > 1 {
                            attrs.push(Attr::uint(crate::emit::WEIGHT_ATTR, period));
                        }
                        attrs
                    },
                    collector_id: id.clone(),
                    seq: index,
                });
                index += 1;
            }
            Record::Lost { count } => dropped += count,
            // Counted by the sampler itself (`ThreadSampler::throttled`).
            Record::Throttle { .. } => {}
            Record::Other { .. } => undecoded += 1,
        });
        let throttles = event.throttled() - self.throttled;
        self.throttled += throttles;
        // A throttle is the kernel deciding this event costs too much and
        // delivering nothing for a while. How many faults happened inside that
        // window is not knowable from userspace, so it is counted as a throttle
        // rather than folded into `dropped`, which would claim a precision that
        // does not exist. This source ignored it entirely until now, and a
        // throttled run reported itself as clean.
        if throttles > 0 {
            self.losses.throttled += throttles;
            self.losses.note(
                "the kernel throttled page-fault sampling: an unknown number of faults in each \
                 throttled period were not sampled (see kernel.perf_event_max_sample_rate)",
            );
        }
        if dropped > 0 {
            self.losses.dropped += dropped;
            self.losses
                .note("the page-fault ring overflowed: faults happened faster than they were read");
        }
        self.losses.undecoded += undecoded;
    }
}

impl Collector for PageFaultCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "count"
    }

    fn start(&mut self) -> Result<(), String> {
        // `trace` means every fault, which is what an interval join needs: a
        // sampled subset would attribute a fraction of the pages and look like
        // an allocation that cost less than it did.
        let period = match self.mode {
            Mode::Trace => 1,
            Mode::Sample { freq } => freq.as_hz(),
            // `count` is served by the counting group, which `Prepare` hands
            // it to; reaching here is a caller that built this collector
            // directly.
            other => return Err(format!("`{other}` mode does not sample page faults")),
        };

        let spec = SampleSpec::software(
            perf::PERF_COUNT_SW_PAGE_FAULTS as u64,
            period,
            Callchain::Off,
        );
        if let (Some(cpus), Some(cgroup)) = (&self.cpus, &self.cgroup) {
            let event = ThreadSampler::open_cgroup(&spec, &cgroup.directory, cpus, RING_PAGES)?;
            event.enable().map_err(|error| error.to_string())?;
            self.event = Some(event);
            return Ok(());
        }
        if let Some(cpus) = &self.cpus {
            let event = ThreadSampler::open_cpus(&spec, cpus, RING_PAGES)?;
            event.enable().map_err(|error| error.to_string())?;
            self.event = Some(event);
            return Ok(());
        }
        let open = |spec: &SampleSpec| {
            if self.whole_process {
                ThreadSampler::open_process(spec, self.pid, RING_PAGES)
            } else {
                crate::perf::online_cpus()
                    .and_then(|cpus| ThreadSampler::open(spec, &[self.pid], &cpus, RING_PAGES))
            }
        };
        // At `perf_event_paranoid` 2 -- the kernel's default -- a process may
        // sample another one it owns in user mode only, and an event that asks
        // for kernel mode as well is refused outright: the CI's paranoid-2 cell
        // refused `mem.faults` for every launched program. A fault is raised
        // with the registers of the code that took it, so a user-mode event
        // still sees every fault the program's own code takes; what it loses is
        // the faults the kernel takes on the program's behalf, copying into a
        // buffer it has not touched yet, and the notes say so.
        //
        // The refusal is matched by its kind, not its number: `crate::perf`
        // wraps the errno in a message (`perf_event_open: Permission denied`),
        // which keeps the kind and drops `raw_os_error`, and a match on the
        // number never fired -- the CI's paranoid-2 cell was refused for every
        // launched program after this retry was written. EACCES and EPERM are
        // both `PermissionDenied`.
        let asked = if self.user_mode_only {
            Err(refused_as_perf_says_it())
        } else {
            open(&spec)
        };
        let opened = match asked {
            Err(error) if is_refusal(&error) => {
                let user_only = SampleSpec {
                    exclude_kernel: true,
                    ..spec
                };
                let reopened = open(&user_only);
                self.user_mode_only = reopened.is_ok();
                reopened.map_err(|_| error)
            }
            other => other,
        };
        let event = opened.map_err(|error| {
            format!(
                "perf_event_open for page faults on pid {} was refused ({error}); \
                 check /proc/sys/kernel/perf_event_paranoid",
                self.pid
            )
        })?;
        event.enable().map_err(|error| error.to_string())?;
        self.followed = (event.threads().len(), event.unlisted());
        self.event = Some(event);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        // Whatever the kernel wrote between the last drain and now is still
        // part of the run; dropping it would silently truncate the tail.
        let mut trailing = Vec::new();
        if let Some(event) = self.event.as_ref() {
            let _ = event.disable();
        }
        self.drain_into(&mut trailing, u64::MAX / 2);
        if let Some(event) = self.event.as_ref() {
            self.losses.undecoded += event.undecoded();
        }
        self.event = None;
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    /// What the events followed, when it is more than a launched program's
    /// one thread, and what they may have missed.
    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.user_mode_only {
            notes.push(
                "this machine would sample page faults in user mode only \
                 (perf_event_paranoid 2 without CAP_PERFMON), so faults the kernel took on the \
                 program's behalf -- a read() into a buffer never touched -- are not in these \
                 samples; every fault the program's own code took is"
                    .to_owned(),
            );
        }
        if let (Some(cpus), Some(cgroup)) = (&self.cpus, &self.cgroup) {
            notes.push(format!(
                "every task of the cgroup {} was sampled while it ran, one event per cpu on {} \
                 cpus; faults came from {} task{}",
                cgroup.directory.display(),
                cpus.len(),
                self.sampled_tasks.len(),
                if self.sampled_tasks.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ));
            return notes;
        }
        if let Some(cpus) = &self.cpus {
            notes.push(format!(
                "every task on {} cpu{} was sampled, one event per cpu; faults came from {} \
                 task{}",
                cpus.len(),
                if cpus.len() == 1 { "" } else { "s" },
                self.sampled_tasks.len(),
                if self.sampled_tasks.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ));
            return notes;
        }
        let (threads, unlisted) = self.followed;
        if threads > 1 {
            notes.push(format!(
                "process {} was already running {threads} threads when sampling began, so each \
                 has an event of its own and each follows what it starts; faults came from {} \
                 task{}",
                self.pid,
                self.sampled_tasks.len(),
                if self.sampled_tasks.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
        if unlisted > 0 {
            notes.push(format!(
                "{unlisted} thread{} of process {} appeared while the events were being opened; \
                 each is sampled if the thread that started it already had its event, and not \
                 otherwise, and which it was cannot be told from here",
                if unlisted == 1 { "" } else { "s" },
                self.pid
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN_TEST_RING: usize = 8;

    /// An anonymous mapping of `pages` pages, none of them resident yet.
    fn fresh_block(pages: usize) -> *mut libc::c_void {
        // SAFETY: an anonymous mapping of our own; the caller unmaps it.
        let block = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                pages * 4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(block, libc::MAP_FAILED, "the mapping must succeed");
        block
    }

    /// How many of the block's pages took at least one sampled fault.
    ///
    /// Pages, not samples: a fault the kernel retries (`VM_FAULT_RETRY`, when
    /// the mmap lock is contended -- five other test binaries on the machine
    /// are enough) raises the software event again for the same page, and
    /// one run here counted 65 samples in 64 pages. What the block's owner
    /// knows is which pages its code touched, not how often the kernel had to
    /// try.
    fn landed_in(events: &[Event], block: *mut libc::c_void, pages: usize) -> usize {
        let base = block as u64;
        events
            .iter()
            .filter_map(|event| match event.key(crate::request::KeyKind::Mem) {
                Some(KeyValue::Mem { base: addr, .. })
                    if (base..base + (pages * 4096) as u64).contains(addr) =>
                {
                    Some((addr - base) / 4096)
                }
                _ => None,
            })
            .collect::<std::collections::BTreeSet<u64>>()
            .len()
    }

    /// The paranoid-2 path, taken here on purpose: sampled in user mode only,
    /// the faults this thread's own code takes all arrive, and the ones the
    /// kernel takes for it -- `read()` from `/dev/zero` into pages nobody
    /// touched -- do not, which is exactly what the note says is lost.
    ///
    /// Ground truth is which code touched each block: the first by stores in
    /// this function, the second only by the kernel's copy. Both blocks are
    /// fresh mappings of this thread, so no other test can fault inside them.
    #[test]
    fn sampled_in_user_mode_the_programs_own_faults_arrive_and_the_kernels_do_not() {
        const PAGES: usize = 64;
        let mut collector = PageFaultCollector::new(0, Mode::Trace);
        collector.user_mode_only = true;
        if let Err(reason) = collector.start() {
            eprintln!("skipped: this machine will not sample page faults at all: {reason}");
            return;
        }
        let touched = fresh_block(PAGES);
        for index in 0..PAGES {
            // SAFETY: inside the mapping, one byte per page.
            unsafe { touched.cast::<u8>().add(index * 4096).write_volatile(1) };
        }
        let copied = fresh_block(PAGES);
        let zero = std::fs::File::open("/dev/zero").expect("/dev/zero");
        let read =
            unsafe { libc::read(std::os::fd::AsRawFd::as_raw_fd(&zero), copied, PAGES * 4096) };
        assert_eq!(read, (PAGES * 4096) as isize, "the kernel filled the block");

        let mut events = collector.sample(0).unwrap();
        events.extend(collector.stop());
        let (own, kernels) = (
            landed_in(&events, touched, PAGES),
            landed_in(&events, copied, PAGES),
        );
        // SAFETY: unmapping what was mapped above, after the last read.
        unsafe {
            libc::munmap(touched, PAGES * 4096);
            libc::munmap(copied, PAGES * 4096);
        }
        // The same bounds as the kernel-mode test below, for the same reasons
        // (a multi-size THP fault installs a whole folio).
        assert!(
            (PAGES / 16..=PAGES).contains(&own),
            "user-mode samples landed in {own} of the {PAGES} pages this code touched"
        );
        assert_eq!(
            kernels, 0,
            "{kernels} samples landed in pages only the kernel's copy touched"
        );
        let notes = collector.notes().join("\n");
        assert!(notes.contains("user mode only"), "{notes}");
        assert!(collector.losses().is_clean(), "{:?}", collector.losses());

        // And the zero is the mode's, not the workload's: where the machine
        // allows kernel mode, the same copy is seen.
        let mut both = PageFaultCollector::new(0, Mode::Trace);
        if both.start().is_err() || both.user_mode_only {
            eprintln!(
                "skipped the kernel-mode half: this machine samples page faults in user mode only"
            );
            return;
        }
        let copied = fresh_block(PAGES);
        let read =
            unsafe { libc::read(std::os::fd::AsRawFd::as_raw_fd(&zero), copied, PAGES * 4096) };
        assert_eq!(read, (PAGES * 4096) as isize, "the kernel filled the block");
        let mut events = both.sample(0).unwrap();
        events.extend(both.stop());
        let kernels = landed_in(&events, copied, PAGES);
        unsafe { libc::munmap(copied, PAGES * 4096) };
        assert!(
            (PAGES / 16..=PAGES).contains(&kernels),
            "with kernel mode, {kernels} samples landed in the {PAGES} pages the kernel's copy \
             touched"
        );
    }

    /// The retry for paranoid 2 has to recognise a refusal the way `crate::perf`
    /// actually returns one. Until the sixteenth wave it matched the errno, which
    /// `crate::perf` does not keep, so the retry never ran and the CI's
    /// paranoid-2 cell refused `mem.faults` for every launched program.
    ///
    /// Ground truth from outside this code: pid 1 belongs to root, and the
    /// kernel lets no unprivileged user open an event on another user's
    /// process at any `perf_event_paranoid` (`ptrace_may_access`). A test run
    /// as root, or with `CAP_PERFMON`, is let in, and skips saying so.
    #[test]
    fn a_refusal_is_recognised_in_the_shape_perf_returns_it() {
        let owner =
            std::fs::metadata("/proc/1").map(|meta| std::os::unix::fs::MetadataExt::uid(&meta));
        // SAFETY: getuid has no failure mode.
        let me = unsafe { libc::getuid() };
        if owner.ok() == Some(me) {
            eprintln!("skipped: pid 1 belongs to this user, so nothing refuses the open");
            return;
        }
        let spec = SampleSpec::software(perf::PERF_COUNT_SW_PAGE_FAULTS as u64, 1, Callchain::Off);
        match ThreadSampler::open_process(&spec, 1, MIN_TEST_RING) {
            Ok(_) => eprintln!(
                "skipped: this process may sample pid 1 (root, or CAP_PERFMON), so there is no \
                 refusal to recognise"
            ),
            Err(error) => {
                assert!(
                    is_refusal(&error),
                    "a refusal not recognised as one: {error:?}"
                );
                assert!(is_refusal(&refused_as_perf_says_it()));
            }
        }
    }

    #[test]
    fn faults_are_reported_at_the_address_that_faulted() {
        let mut collector = PageFaultCollector::new(0, Mode::Trace);
        if collector.start().is_err() {
            eprintln!("skipped: this machine refuses unprivileged perf sampling");
            return;
        }

        const PAGES: usize = 64;
        let page = 4096;

        // Mapped rather than allocated. A block from the allocator's arena can
        // come back on pages another test in this binary already made resident,
        // and a page that is already there does not fault: the test would then
        // be waiting for events that had no reason to happen.
        // SAFETY: an anonymous mapping of our own, unmapped below.
        let block = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PAGES * page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(block, libc::MAP_FAILED, "the mapping must succeed");
        let base = block as u64;
        // From here until the unmap the range is this test's alone. Before
        // it, the same addresses may have been another thread's mapping, whose
        // faults the process-wide collector saw too -- at addresses inside
        // this block that this test never wrote.
        let mapped_at = crate::clock::monotonic_ns();
        for index in 0..PAGES {
            // SAFETY: writing inside the mapping, one byte per page, which is
            // what makes each of them fault in.
            unsafe { block.cast::<u8>().add(index * page).write_volatile(1) };
        }

        let mut events = collector.sample(0).unwrap();
        events.extend(collector.stop());
        // SAFETY: unmapping what was mapped above, after the last read of it.
        unsafe { libc::munmap(block, PAGES * page) };

        let addresses: Vec<u64> = events
            .iter()
            .filter(|event| event.ts_ns >= mapped_at)
            .filter_map(|event| match event.key(crate::request::KeyKind::Mem) {
                Some(KeyValue::Mem { base: addr, .. })
                    if (base..base + (PAGES * page) as u64).contains(addr) =>
                {
                    Some(*addr)
                }
                _ => None,
            })
            .collect();
        let inside = addresses.len();
        let pages: std::collections::BTreeSet<u64> = addresses.iter().copied().collect();

        // Neither bound is the page count, and both of the reasons matter.
        //
        // Touching N pages does not produce N faults: with multi-size
        // transparent huge pages one fault installs a whole folio, so the count
        // is a fraction of the pages touched and varies between runs. And this
        // collector is scoped to the process, not to this thread, so the other
        // threads of a test binary running its suite in parallel contribute
        // faults of their own -- correctly, since `mem.faults --pid` means the
        // process.
        //
        // What can be asserted is that the addresses are real. The block sits
        // where the kernel put it, so samples landing inside it while it was
        // ours are not coincidence; and this test wrote exactly one byte, the
        // first, of each page, so a fault inside it at any other offset is an
        // address that was never touched. The number of events is not bounded
        // by the pages: one page can fault twice (a fault retried, a page
        // migrated under compaction), and a run recorded in
        // docs/scenarios/cross-machine.md saw 65 events in these 64 pages.
        // The number of distinct pages is.
        assert!(!events.is_empty(), "no page-fault event arrived at all");
        assert!(
            inside >= PAGES / 16,
            "only {inside} of {} events landed in the {PAGES} pages that were touched, which is \
             below even one fault per sixteen-page folio",
            events.len()
        );
        let untouched: Vec<u64> = addresses
            .iter()
            .filter(|addr| (**addr - base) % page as u64 != 0)
            .map(|addr| addr - base)
            .collect();
        assert!(
            untouched.is_empty(),
            "faults reported at offsets {untouched:x?} of the block, where nothing was written: \
             only the first byte of each page was"
        );
        assert!(
            pages.len() <= PAGES,
            "{} distinct addresses in a block of {PAGES} pages",
            pages.len()
        );
        assert!(collector.losses().is_clean(), "{:?}", collector.losses());
    }
}
