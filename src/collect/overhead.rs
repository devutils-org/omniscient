//! What a collection costs: the tool's own cpu, and what its collectors
//! spend inside the target, read by mechanism.

use super::*;

/// What a run cost, measured rather than declared.
///
/// The catalog declares a cost *band* per backend -- `free`, `low`, `med` --
/// and until now that was a claim nobody had checked. This is the measurement
/// behind it: the cpu time this process burned collecting, against the cpu time
/// the thing being observed burned over the same window.
///
/// The denominator is the target's own cpu time on purpose. "Two percent
/// overhead" means "the measurement cost two percent of what the workload
/// cost"; a fraction of wall time would answer a different question and would
/// let the same collector read as cheap because the machine happened to be
/// idle. A target that burned no cpu, or that has no cpu account at all like a
/// network interface, has no denominator -- and that is said rather than
/// divided by.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Overhead {
    /// User plus system cpu time this whole process spent between the moment
    /// the collectors were attached and the moment the window closed.
    /// `getrusage(RUSAGE_SELF)`, differenced.
    pub collector_cpu_ns: u64,
    /// The share of `collector_cpu_ns` this collection is answerable for.
    ///
    /// One process serves every request in a session and `getrusage` cannot say
    /// which of them spent what, so the share is the fraction of the time spent
    /// inside `sample` that went to this collector. That is wall time, and
    /// therefore an approximation -- exactly equal to the whole when a session
    /// carries one request, which is the common case, and reported as a share
    /// when it does not.
    pub attributed_cpu_ns: u64,
    /// Cpu time the observed target spent over the same window.
    pub observed_cpu_ns: Option<u64>,
    /// Where the number above came from, or why there is none. It travels with
    /// the number because `getrusage(RUSAGE_CHILDREN)` and `/proc/<pid>/stat`
    /// and `/proc/stat` are three different accounts, and a reader comparing
    /// two runs needs to know which one they are reading.
    pub observed_from: String,
    /// Wall time of the window.
    pub wall_ns: u64,
    /// What measuring all of this cost: the `getrusage` calls, the target's
    /// procfs reads, the clock reads that share time out per collector, and --
    /// when a ceiling was exceeded -- moving the collection onto a cheaper
    /// backend. Enforcing a budget is part of the price of having one. Cpu
    /// time of the thread that did it, not wall time: a weighing preempted on a
    /// loaded machine did not cost the wait.
    ///
    /// Reported rather than subtracted. A measurement that hides its own price
    /// is exactly the thing this field exists to stop, and a budget check that
    /// ran often enough to dominate the answer would show up here first.
    pub measurement_ns: u64,
    /// How many times the cost was read during the run.
    pub measurements: u64,
    /// Cpu time the collector's own code spent inside the target's context,
    /// where the tool's own account cannot see it: an eBPF program run on
    /// every syscall the target makes, the preload shim's record on every
    /// allocation (finding 47). `Some(0)` for a collector that reads from
    /// outside the target; `None` when it runs inside and what it spent there
    /// is not knowable on this run, which `in_target_from` says why --
    /// **unknown, never zero**.
    #[serde(default)]
    pub in_target_cpu_ns: Option<u64>,
    /// How many times that code ran inside the target, when it is counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_target_runs: Option<u64>,
    /// How many of those runs are in `in_target_cpu_ns` at the mean of the
    /// others rather than as timed: the preload shim's calls a preemption fell
    /// in, whose spans are the scheduler's slice and not the hook's. Counted
    /// and said, because an account that silently left them out would read
    /// low with nothing to show it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub in_target_left_out: u64,
    /// Where `in_target_cpu_ns` came from, or why there is none.
    #[serde(default)]
    pub in_target_from: String,
    /// This process's cpu before this collection's span opened -- the span
    /// `collector_cpu_ns` weighs -- and not before the target ran.
    /// `getrusage(RUSAGE_SELF)` at the span's opening. For a collection open
    /// from the target's start that is the tool's own preparation: its start,
    /// the catalog, the probes, the attach, and whatever table the run was
    /// going to need and read then so that no drain would pay for it -- the
    /// kernel's symbols, for a profile with stacks, which a debug build parses
    /// in the better part of a second. For one attached mid-run, or detached
    /// and reopened, it also holds what every other collection's window cost
    /// before this one's opened.
    ///
    /// Outside the percentage on purpose: none of it is this collection's
    /// dilation. Reported, because a reader told a profile cost 3 % of its
    /// target, where the run cost the machine more than the target did, has
    /// been told something false (finding U-cost, fourteenth wave).
    #[serde(default)]
    pub before_window_cpu_ns: u64,
    /// This process's cpu after the span `collector_cpu_ns` weighs, up to the
    /// collection being handed back: what the runner did once the window was
    /// weighed -- the stacks, the filter, the other requests' tails. What
    /// formatting the results costs after that is not in it.
    #[serde(default)]
    pub after_window_cpu_ns: u64,
    /// The part of `attributed_cpu_ns` spent once rather than at a rate:
    /// loading a symbol table the first time a frame landed in its module.
    /// Weighed apart -- outside [`Overhead::percent`] and the budget's checks
    /// -- and said apart, because a load of tens of milliseconds read against
    /// the few milliseconds a waiting program burns by the first check is a
    /// percentage of how early the check came, not of what running costs.
    /// Still in `collector_cpu_ns` and [`Overhead::tool_cpu_ns`]: what the
    /// run cost is what it cost.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub one_time_cpu_ns: u64,
}

impl Overhead {
    /// What this collection cost, as a percentage of the work the target did
    /// of its own: the tool's cpu and what the collector spent inside the
    /// target, over the target's cpu less that second part -- which the
    /// target's account already holds, since it ran in the target's context.
    /// That is the dilation a program that times itself would see.
    ///
    /// Where the in-target part is unknown this is the tool's share alone, and
    /// [`Overhead::in_target_known`] is what says it is incomplete. Where the
    /// in-target part is larger than everything the target spent -- a program
    /// that ran for every task on the machine and filtered most of them out --
    /// the target's whole account is the denominator, which never understates.
    pub fn percent(&self) -> Option<f64> {
        let observed = self.observed_cpu_ns.filter(|observed| *observed > 0)?;
        let inside = self.in_target_cpu_ns.unwrap_or(0);
        let work = match observed.checked_sub(inside) {
            Some(work) if work > 0 => work,
            _ => observed,
        };
        Some((self.running_cpu_ns() + inside) as f64 * 100.0 / work as f64)
    }

    /// How far over `ceiling` (a percentage) this collection is, in cpu
    /// nanoseconds: what it spent, less what the ceiling allows of the work
    /// the target did. `None` where [`Overhead::percent`] is.
    pub fn excess_ns(&self, ceiling: f64) -> Option<f64> {
        let observed = self.observed_cpu_ns.filter(|observed| *observed > 0)?;
        let inside = self.in_target_cpu_ns.unwrap_or(0);
        let work = match observed.checked_sub(inside) {
            Some(work) if work > 0 => work,
            _ => observed,
        };
        Some((self.running_cpu_ns() + inside) as f64 - ceiling / 100.0 * work as f64)
    }

    /// The tool's share less what it spent once: the rate a budget weighs.
    pub fn running_cpu_ns(&self) -> u64 {
        self.attributed_cpu_ns.saturating_sub(self.one_time_cpu_ns)
    }

    /// Everything this process spent, by its own kernel account, from its
    /// start to the collection being handed back: before the window, in it,
    /// and after. What a `wait4` over the tool reports, less its target and
    /// the formatting of the results.
    ///
    /// A process total, not this collection's share: every collection of one
    /// run carries the same figure, up to when each was handed back, so the
    /// figures of a run's collections are compared, never summed.
    pub fn tool_cpu_ns(&self) -> u64 {
        self.before_window_cpu_ns + self.collector_cpu_ns + self.after_window_cpu_ns
    }

    /// The tool's own share of the target's cpu, as every run before finding
    /// 47 reported it.
    pub fn own_percent(&self) -> Option<f64> {
        let observed = self.observed_cpu_ns.filter(|observed| *observed > 0)?;
        Some(self.running_cpu_ns() as f64 * 100.0 / observed as f64)
    }

    /// Whether what the collector spent inside the target is part of
    /// [`Overhead::percent`].
    pub fn in_target_known(&self) -> bool {
        self.in_target_cpu_ns.is_some()
    }
}

/// What a collector's own code has spent inside its target so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Inside {
    pub cpu_ns: u64,
    pub runs: u64,
    /// How many of those runs are in `cpu_ns` at an estimate rather than as
    /// timed: the preload shim's calls a preemption fell in, whose spans were
    /// the scheduler's, priced at the mean of the others.
    pub left_out: u64,
}

impl std::ops::Add for Inside {
    type Output = Inside;
    fn add(self, other: Inside) -> Inside {
        Inside {
            cpu_ns: self.cpu_ns + other.cpu_ns,
            runs: self.runs + other.runs,
            left_out: self.left_out + other.left_out,
        }
    }
}

/// A collector's account of what it spends inside its target, for a
/// collector whose code runs there and that keeps one: the preload shim
/// publishes its own time in the ring it shares with `omn`.
///
/// Cumulative since the target started: the runner differences it.
pub trait InsideAccount {
    /// What has been spent so far, or why it cannot be said.
    fn read(&self) -> Result<Inside, String>;
    /// Where the number comes from, for the record beside it.
    fn describes(&self) -> String;
}

/// Whether the kernel is counting eBPF programs' run time for this run.
///
/// It counts only while `bpf_stats_enabled_key` is on -- the
/// `kernel.bpf_stats_enabled` sysctl, or a descriptor from `BPF_ENABLE_STATS`
/// held open -- because timing each run costs two clock reads. Off by default,
/// and turning it on needs `CAP_SYS_ADMIN`, which the `CAP_BPF` this tool is
/// granted does not include.
pub(super) enum ProgramStats {
    /// Nothing in the run loads a program.
    NotAsked,
    /// Counting, with the descriptor that keeps it on when this run turned it
    /// on, and without one when the sysctl already was.
    On(Option<std::os::fd::OwnedFd>),
    /// Not counting, with why and what would change it.
    Off(String),
}

pub(super) fn bpf_stats_sysctl() -> Option<bool> {
    std::fs::read_to_string("/proc/sys/kernel/bpf_stats_enabled")
        .ok()
        .map(|text| text.trim() != "0")
}

impl ProgramStats {
    /// Whether a program loaded now is timed, and why not when it is not.
    pub(super) fn counting(&self) -> Result<(), String> {
        match self {
            ProgramStats::On(_) => Ok(()),
            ProgramStats::Off(why) => Err(why.clone()),
            ProgramStats::NotAsked => Err(
                "this run did not ask the kernel to time eBPF programs, because none of its \
                 requests started on a backend that loads one"
                    .to_owned(),
            ),
        }
    }

    /// Whether the sysctl this run relied on was turned off under it.
    pub(super) fn lost(&self) -> Option<String> {
        match self {
            ProgramStats::On(None) if bpf_stats_sysctl() == Some(false) => Some(
                "`kernel.bpf_stats_enabled` was turned off during the run, so the kernel stopped \
                 timing the programs part way through and their run time is incomplete"
                    .to_owned(),
            ),
            _ => None,
        }
    }

    pub(super) fn for_run(resolved: &[ResolvedRequest]) -> ProgramStats {
        if !resolved.iter().any(|one| loads_programs(&one.backend.id)) {
            return ProgramStats::NotAsked;
        }
        if bpf_stats_sysctl() == Some(true) {
            return ProgramStats::On(None);
        }
        // The layer's own call (`src/bpf/mod.rs`), not a copy of it here.
        let error = match crate::bpf::enable_run_time_stats() {
            Ok(fd) => return ProgramStats::On(Some(fd)),
            Err(error) => error,
        };
        let sysctl = match bpf_stats_sysctl() {
            Some(_) => "`kernel.bpf_stats_enabled` is 0",
            None => "this kernel has no `kernel.bpf_stats_enabled`",
        };
        let errno = match error.raw_os_error() {
            Some(libc::EPERM) => "EPERM: it needs CAP_SYS_ADMIN".to_owned(),
            _ => error.to_string(),
        };
        ProgramStats::Off(format!(
            "the kernel is not timing eBPF programs on this run: {sysctl}, and BPF_ENABLE_STATS \
             was refused ({errno}); `sysctl kernel.bpf_stats_enabled=1` before the run makes \
             what the programs spend inside the target measurable"
        ))
    }
}

/// Whether a backend works by loading eBPF programs, whose run time is then
/// the kernel's to report.
pub(super) fn loads_programs(backend: &str) -> bool {
    BackendKind::of(backend).loads_programs()
}

/// Events as a collection hands them over, made the runner's: stamped with
/// the collector they came from, then their task ids in our numbering.
pub(super) fn into_our_ids(
    serving: Option<&ResolvedRequest>,
    events: &mut [Event],
    losses: &mut Losses,
) {
    stamp(serving, events);
    into_our_numbering(serving, events, losses);
}

/// Each event named after the collector built for its request, the registry's
/// `<source>/<backend>` key: named here, once, rather than by each collector,
/// whose own spelling of its pair could drift from the one it is registered
/// under. An event a collector already named is left alone only when no
/// request is known to say better, as in the runner's own tests.
pub(super) fn stamp(serving: Option<&ResolvedRequest>, events: &mut [Event]) {
    let Some(serving) = serving else {
        return;
    };
    let id = CollectorId::of(&serving.request.source.to_string(), &serving.backend.id);
    for event in events {
        event.collector_id = id.clone();
    }
}

/// An eBPF collector's task ids, written by the kernel in its own numbering,
/// rewritten into the one every other collector uses (`bpf::pidns`), and what
/// could not be said so.
pub(super) fn into_our_numbering(
    serving: Option<&ResolvedRequest>,
    events: &mut [Event],
    losses: &mut Losses,
) {
    if !serving.is_some_and(|serving| loads_programs(&serving.backend.id)) {
        return;
    }
    // Nothing handed over may be rows still in the kernel, whose tasks must
    // be seen alive to be translated when they come (`pidns::remember`).
    if events.is_empty() {
        crate::bpf::pidns::remember();
        return;
    }
    let missed = crate::bpf::pidns::translate(events);
    if missed > 0 {
        losses.note(crate::bpf::pidns::untranslated_note(missed));
    }
}

/// The eBPF programs this process holds a descriptor to, by descriptor, with
/// their id and what the kernel has counted of each.
///
/// Read from `/proc/self/fdinfo`, which prints `prog_id`, `run_time_ns` and
/// `run_cnt` for a program and no run time for a link or a map -- so whoever
/// loaded the program does not matter: the fentry trampolines load their own,
/// outside `src/bpf/syscall.rs`, and are found the same way.
pub(super) fn programs_held() -> BTreeMap<RawFd, (u32, Inside)> {
    let Ok(entries) = std::fs::read_dir("/proc/self/fdinfo") else {
        return BTreeMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<RawFd>().ok())
        .filter_map(|fd| program_run(fd).map(|run| (fd, run)))
        .collect()
}

/// What the kernel has counted of the program behind `fd`, with its id; `None`
/// when the descriptor is closed or is not a program.
pub(super) fn program_run(fd: RawFd) -> Option<(u32, Inside)> {
    let text = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).ok()?;
    parse_program_fdinfo(&text)
}

pub(super) fn parse_program_fdinfo(text: &str) -> Option<(u32, Inside)> {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == name).then(|| value.trim().parse::<u64>().ok())?
        })
    };
    // A link prints the id of the program it holds; only a program prints its
    // own run time.
    let cpu_ns = field("run_time_ns")?;
    Some((
        field("prog_id")? as u32,
        Inside {
            cpu_ns,
            runs: field("run_cnt").unwrap_or(0),
            left_out: 0,
        },
    ))
}

/// The programs that appeared among this process's descriptors since
/// `before`: what one collector's prepare or attach loaded.
pub(super) fn programs_since(before: Option<&BTreeMap<RawFd, (u32, Inside)>>) -> Vec<(RawFd, u32)> {
    let Some(before) = before else {
        return Vec::new();
    };
    let known: std::collections::BTreeSet<u32> = before.values().map(|(id, _)| *id).collect();
    let mut seen = std::collections::BTreeSet::new();
    programs_held()
        .into_iter()
        .filter(|(_, (id, _))| !known.contains(id) && seen.insert(*id))
        .map(|(fd, (id, _))| (fd, id))
        .collect()
}

/// Where a collection's in-target cost is read from.
pub(super) enum InsideMeter {
    /// Nothing of the collector runs in the target's context.
    Outside(&'static str),
    /// It does, and what it spends there cannot be known on this run.
    Unknown(String),
    /// The eBPF programs it loaded, read through the kernel's own account.
    Programs {
        held: Vec<(RawFd, u32)>,
        /// The last reading of each, kept for when its descriptor has closed.
        last: Vec<Inside>,
        /// What the backends before a degradation spent.
        sunk: Inside,
    },
    /// An account the collector keeps itself.
    Reported {
        account: Box<dyn InsideAccount>,
        last: Inside,
        sunk: Inside,
    },
}

pub(super) const OUTSIDE: &str =
    "nothing of this collector runs in the target's context: it reads the \
                       kernel's account, or a file, from its own";

pub(super) const PMU_INSIDE: &str =
    "a perf event's counter is saved and restored on every context switch \
                          of the target, and a sampling event interrupts it, in the target's own \
                          context; the kernel does not account that time apart from the target's";

impl InsideMeter {
    /// How a collection's in-target cost is read, decided by its mechanism:
    /// the account the collector keeps, the programs it loaded, or what its
    /// backend is known to do.
    pub(super) fn of(
        backend: &str,
        account: Option<Box<dyn InsideAccount>>,
        programs: Vec<(RawFd, u32)>,
        stats: &Result<(), String>,
    ) -> InsideMeter {
        if let Some(account) = account {
            return InsideMeter::Reported {
                account,
                last: Inside::default(),
                sunk: Inside::default(),
            };
        }
        if loads_programs(backend) {
            return match stats {
                Err(why) => InsideMeter::Unknown(why.clone()),
                Ok(()) if programs.is_empty() => InsideMeter::Unknown(
                    "this collector's eBPF programs were not among the descriptors this process \
                     holds, so the kernel's account of them cannot be read"
                        .to_owned(),
                ),
                _ => InsideMeter::Programs {
                    last: vec![Inside::default(); programs.len()],
                    held: programs,
                    sunk: Inside::default(),
                },
            };
        }
        let unknown = |why: &str| InsideMeter::Unknown(why.to_owned());
        match BackendKind::of(backend) {
            BackendKind::Pmu => unknown(PMU_INSIDE),
            BackendKind::Preload => unknown(
                "a preloaded library runs inside the target, and this one keeps no account of \
                 its own time",
            ),
            BackendKind::AfPacket => unknown(
                "the kernel copies each packet into the capture ring in whichever context \
                 received or sent it, which for a local program is often its own",
            ),
            BackendKind::SoTimestamping => unknown(
                "a timestamping socket option makes the kernel stamp the target's own packets \
                 in its context",
            ),
            BackendKind::Vendor if backend != "vendor:nvml" => unknown(
                "a vendor profiler injects into or replays the target, and says nothing of what \
                 that cost it",
            ),
            _ if backend == "fs:proc:clear_refs" || backend == "fs:sys:page_idle" => unknown(
                "clearing the pages' referenced bits makes the target take the faults or the \
                 TLB refills that set them again, in its own context",
            ),
            _ => InsideMeter::Outside(OUTSIDE),
        }
    }

    /// What has been spent inside the target so far, or why it cannot be
    /// said. Keeps each reading, so a program whose descriptor has since
    /// closed still counts what it last reported.
    pub(super) fn read(&mut self) -> Result<Inside, String> {
        match self {
            InsideMeter::Outside(_) => Ok(Inside::default()),
            InsideMeter::Unknown(why) => Err(why.clone()),
            InsideMeter::Programs { held, last, sunk } => {
                for ((fd, id), last) in held.iter().zip(last.iter_mut()) {
                    if let Some((now_id, run)) = program_run(*fd) {
                        if now_id == *id {
                            *last = run;
                        }
                    }
                }
                Ok(last.iter().fold(*sunk, |total, one| total + *one))
            }
            InsideMeter::Reported {
                account,
                last,
                sunk,
            } => {
                *last = account.read()?;
                Ok(*sunk + *last)
            }
        }
    }

    /// The same reading, without keeping it.
    pub(super) fn peek(&self) -> Result<Inside, String> {
        match self {
            InsideMeter::Programs { held, last, sunk } => {
                Ok(held
                    .iter()
                    .zip(last)
                    .fold(*sunk, |total, ((fd, id), last)| {
                        let now = program_run(*fd)
                            .filter(|(now_id, _)| now_id == id)
                            .map_or(*last, |(_, run)| run);
                        total + now
                    }))
            }
            InsideMeter::Reported { account, sunk, .. } => account.read().map(|now| *sunk + now),
            InsideMeter::Outside(_) => Ok(Inside::default()),
            InsideMeter::Unknown(why) => Err(why.clone()),
        }
    }

    /// Where the number comes from.
    pub(super) fn describes(&self) -> String {
        match self {
            InsideMeter::Outside(why) => (*why).to_owned(),
            InsideMeter::Unknown(why) => why.clone(),
            InsideMeter::Programs { held, .. } => format!(
                "the kernel's own run time of the {} eBPF program(s) this collector loaded \
                 (bpf_prog_info run_time_ns and run_cnt, through /proc/self/fdinfo). A lower \
                 bound of what the target paid: the kernel times the program and not the \
                 tracepoint, trampoline or probe that calls it, nor the slower path an attached \
                 syscall tracepoint puts every task on. And it counts every run, including the \
                 ones for other tasks that the program filtered out",
                held.len()
            ),
            InsideMeter::Reported { account, .. } => account.describes(),
        }
    }

    /// The meter of the backend a budget moved this collection onto, carrying
    /// what the one before it spent.
    pub(super) fn moved_onto(&mut self, mut next: InsideMeter) {
        let spent = self.read();
        let before = match self {
            InsideMeter::Outside(_) => Ok(Inside::default()),
            _ => spent,
        };
        match (&mut next, before) {
            (InsideMeter::Programs { sunk, .. } | InsideMeter::Reported { sunk, .. }, Ok(v)) => {
                *sunk = v;
            }
            (InsideMeter::Outside(_), Ok(v)) if v.cpu_ns > 0 || v.runs > 0 => {
                next = InsideMeter::Unknown(
                    "the backend this collection started on spent time inside the target that \
                     the one it moved onto has no account to add to"
                        .to_owned(),
                );
            }
            (_, Err(why)) => {
                next = InsideMeter::Unknown(format!(
                    "before a budget moved this collection, its first backend's cost inside the \
                     target was unknown: {why}"
                ));
            }
            _ => {}
        }
        *self = next;
    }
}

/// The cpu time the calling thread has been charged, in nanoseconds.
///
/// What weighing a run costs is timed on this clock rather than the monotonic
/// one: the price of a `getrusage` and a procfs read is the cpu they take, and
/// a weighing that is preempted half way through did not cost the time it
/// spent waiting for a core -- which on a loaded machine is what the monotonic
/// clock mostly measured (the flaky tally, #12: 12.3 ms of "weighing" in a
/// 605 ms run).
pub(crate) fn thread_cpu_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes a `timespec` this function owns.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
    (time.tv_sec as u64) * 1_000_000_000 + (time.tv_nsec as u64)
}

/// This process's cpu, user and system, as `getrusage` gives it: the account
/// every collection's cost is read from.
pub(crate) fn process_cpu_ns() -> u64 {
    rusage_ns(libc::RUSAGE_SELF)
}

/// User plus system cpu time of one `getrusage` account, in nanoseconds.
pub(super) fn rusage_ns(who: libc::c_int) -> u64 {
    // SAFETY: `getrusage` fills a struct this function owns, and writes nothing
    // else.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(who, &mut usage) } != 0 {
        return 0;
    }
    let ns =
        |time: libc::timeval| (time.tv_sec as u64) * 1_000_000_000 + (time.tv_usec as u64) * 1_000;
    ns(usage.ru_utime) + ns(usage.ru_stime)
}

/// The kernel's tick, which is what `/proc` reports cpu time in.
pub(crate) fn ticks_per_second() -> u64 {
    // SAFETY: `sysconf` takes an integer and returns one.
    match unsafe { libc::sysconf(libc::_SC_CLK_TCK) } {
        ticks if ticks > 0 => ticks as u64,
        _ => 100,
    }
}

/// Cpu time one task has consumed, its reaped children included.
///
/// `task::process_cpu`: the process cpu clock in nanoseconds plus `cutime` and
/// `cstime` -- a shell that forks is still the workload that was asked for,
/// and leaving its children out would make every collector look expensive
/// against it. `/proc/<pid>/stat`'s ticks only where the clock cannot be read.
pub(super) fn task_cpu_ns(pid: i32) -> Option<u64> {
    task::process_cpu(pid).map(|read| read.ns)
}

/// The cpu time one thread has been charged, its own and no other's, from
/// its `schedstat` in nanoseconds (`task::thread_cpu`).
pub(super) fn thread_cpu_of(tid: i32) -> Option<u64> {
    task::thread_cpu(tid).map(|read| read.ns)
}

/// Cpu time the machine, or a set of cpus, spent on anything but idling.
///
/// `iowait` is idle: a cpu waiting for a disk is available to anything else
/// that wants it, and counting it as work would inflate the denominator and
/// make every collector look cheap on a machine that is mostly blocked.
pub(super) fn cpus_busy_ns(wanted: Option<&[u32]>) -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let mut total = 0u64;
    let mut matched = false;
    for line in stat.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            break;
        };
        let (label, numbers) = match rest.split_once(char::is_whitespace) {
            Some(split) => split,
            None => continue,
        };
        match (wanted, label.parse::<u32>()) {
            // The summary line, which is what a machine-wide target wants.
            (None, Err(_)) => {}
            (Some(wanted), Ok(cpu)) if wanted.contains(&cpu) => {}
            _ => continue,
        }
        let fields: Vec<u64> = numbers
            .split_whitespace()
            .map(|field| field.parse().unwrap_or(0))
            .collect();
        // user nice system idle iowait irq softirq steal guest guest_nice.
        // The two guest columns are already counted inside user and nice, so
        // summing the line and taking the idle columns off would count a
        // virtualised machine's work twice; the busy columns are named instead.
        const BUSY: [usize; 6] = [0, 1, 2, 5, 6, 7];
        total += BUSY
            .iter()
            .filter_map(|column| fields.get(*column))
            .sum::<u64>();
        matched = true;
    }
    matched.then(|| total * 1_000_000_000 / ticks_per_second())
}

/// Cpu time a cgroup has been charged, from `cpu.stat`.
pub(super) fn cgroup_cpu_ns(directory: &std::path::Path) -> Option<u64> {
    let stat = std::fs::read_to_string(directory.join("cpu.stat")).ok()?;
    stat.lines()
        .find_map(|line| line.strip_prefix("usage_usec "))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|usec| usec * 1_000)
}

/// Resolves a cgroup target to the directory that holds its `cpu.stat`.
///
/// A target may be written the way `/proc/self/cgroup` writes it, rooted at the
/// hierarchy, or as the mounted path; both name the same cgroup. The same
/// two-way reading is in `cpu/throttle.rs`, which is where it was worked out.
pub(super) fn cgroup_directory(path: &std::path::Path) -> std::path::PathBuf {
    const ROOT: &str = "/sys/fs/cgroup";
    if path.starts_with(ROOT) {
        path.to_path_buf()
    } else {
        std::path::Path::new(ROOT).join(path.strip_prefix("/").unwrap_or(path))
    }
}

/// The highest cumulative reading a meter has given, so that it never goes
/// back: a cumulative account read as a sum over several processes can come
/// out short for one read (`task::tree_cpu`), and a denominator that shrank
/// would make the tool's cost jump for no reason of its own.
///
/// Equal to every other: two meters of the same target are the same meter
/// whatever each has read so far (`Meter::read_each`).
#[derive(Default)]
pub(super) struct HighWater(pub(super) std::cell::Cell<u64>);

impl HighWater {
    fn keep(&self, read: u64) -> u64 {
        let kept = self.0.get().max(read);
        self.0.set(kept);
        kept
    }
}

impl PartialEq for HighWater {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

/// Reads the cpu time of whatever is being observed, so that the tool's own
/// cost can be a fraction of something real.
#[derive(PartialEq)]
pub(super) enum Meter {
    /// A program this tool launched, and the processes it started. While it
    /// runs, its own `/proc` entry and its descendants' say what the tree has
    /// spent (`task::tree_cpu`); once it has been reaped its entry is gone and
    /// the kernel has rolled its usage, and that of every child it reaped,
    /// into this process's children account, which is the same number `time`
    /// prints and is read from there instead.
    ///
    /// The tree, not the launched process alone: a compiler driver that spent
    /// 4 ms in `wait4` while `cc1` spent seconds made a budget read 8581 % of
    /// "the target's cpu" (`docs/fixes/w17-build.md`), and the collectors that
    /// follow a target's children were watching `cc1` the whole time. A
    /// child's cpu was already in the denominator once it was reaped; this
    /// counts it while it runs.
    Launched(i32, HighWater),
    /// Processes the request named.
    Tasks(Vec<i32>),
    /// Threads the request named, each weighed on its own account.
    Threads(Vec<i32>),
    /// The machine, or the cpus the request named.
    Cpus(Option<Vec<u32>>),
    Cgroup(std::path::PathBuf),
    /// Nothing here keeps a cpu account, with the reason.
    Nothing(&'static str),
}

impl Meter {
    pub(super) fn of(target: &Target, launched: Option<i32>) -> Meter {
        match target {
            Target::Launch(_) => match launched {
                Some(pid) => Meter::Launched(pid, HighWater::default()),
                None => Meter::Nothing("the program was never launched"),
            },
            Target::Pid(pids) => Meter::Tasks(pids.clone()),
            Target::Tid(tids) => Meter::Threads(tids.clone()),
            Target::Cgroup(path) => Meter::Cgroup(cgroup_directory(path)),
            Target::Cpu(set) => Meter::Cpus(Some(set.cpus().to_vec())),
            Target::System => Meter::Cpus(None),
            Target::Iface(_) => Meter::Nothing(
                "a network interface consumes no cpu time of its own, so there is nothing \
                 for an overhead to be a percentage of",
            ),
            Target::Container(_) => Meter::Nothing(
                "a container has to be resolved to its cgroup before its cpu time can be \
                 read, which is not built yet",
            ),
        }
    }

    /// Every meter's [`Meter::read`], a meter equal to an earlier one taking
    /// that one's reading rather than its own. Two collections of one target
    /// are weighed against one denominator: read twice, a running target's
    /// nanosecond clock (`task::process_cpu`) moves between the two reads.
    ///
    /// The meter that took an earlier one's reading keeps it as its own high
    /// water too, so that a later [`Meter::read`] of it alone (a collection
    /// detached, or read after the program was killed) is not short of what
    /// the first one saw.
    pub(super) fn read_each(meters: &[Meter]) -> Vec<Option<u64>> {
        let mut read: Vec<Option<u64>> = Vec::with_capacity(meters.len());
        for (index, meter) in meters.iter().enumerate() {
            let earlier = meters[..index].iter().position(|other| other == meter);
            read.push(match earlier {
                Some(earlier) => meter.agree(read[earlier]),
                None => meter.read(),
            });
        }
        read
    }

    /// Takes `read`, an equal meter's reading, as this one's: a launched
    /// program's meter keeps it as its high water and gives the kept value.
    fn agree(&self, read: Option<u64>) -> Option<u64> {
        match (self, read) {
            (Meter::Launched(_, high), Some(read)) => Some(high.keep(read)),
            _ => read,
        }
    }

    /// The cumulative cpu time of the target, to be differenced against a
    /// baseline taken when the window opened.
    pub(super) fn read(&self) -> Option<u64> {
        match self {
            Meter::Launched(pid, high) => {
                // Adopting, every descendant of this process is the program's
                // -- its tree, and the orphans that left it while they live --
                // and every child reaped is in this process's children
                // account: one walk, and no orphan's work missing from the
                // denominator while it runs.
                // The helpers `omn` runs itself (`launch::helper_output`) are
                // its children too, and are no part of the program: the live
                // ones are left out of the walk, and what the reaped ones spent
                // out of the children account -- read after the account, so
                // that a helper reaped between the two reads is not counted.
                let children = || {
                    let account = rusage_ns(libc::RUSAGE_CHILDREN);
                    account.saturating_sub(launch::helpers_spent_ns())
                };
                let spent = if launch::adopting() {
                    // SAFETY: getpid cannot fail.
                    children()
                        + task::descendants_cpu(
                            unsafe { libc::getpid() },
                            &launch::helpers_running(),
                        )
                } else {
                    task::tree_cpu(*pid)
                        .map(|read| read.ns)
                        .unwrap_or_else(children)
                };
                Some(high.keep(spent))
            }
            Meter::Tasks(pids) => {
                let mut total = 0;
                let mut any = false;
                for pid in pids {
                    if let Some(cpu_ns) = task_cpu_ns(*pid) {
                        total += cpu_ns;
                        any = true;
                    }
                }
                any.then_some(total)
            }
            Meter::Threads(tids) => {
                let times: Vec<u64> = tids.iter().filter_map(|tid| thread_cpu_of(*tid)).collect();
                (!times.is_empty()).then(|| times.iter().sum())
            }
            Meter::Cpus(set) => cpus_busy_ns(set.as_deref()),
            Meter::Cgroup(directory) => cgroup_cpu_ns(directory),
            Meter::Nothing(_) => None,
        }
    }

    /// What to record beside the number, so a reader knows which account it is.
    pub(super) fn describes(&self) -> String {
        match self {
            Meter::Launched(pid, _) => format!(
                "the launched program's cpu time and its descendants' ({}, of {pid} and of \
                 every process it started while they ran, its orphans adopted by omn \
                 included, and getrusage(RUSAGE_CHILDREN) in microseconds once it was \
                 reaped, the helpers omn ran itself left out)",
                task::tree_cpu(*pid).map_or(
                    "the scheduler's own nanoseconds, or clock ticks where it could not be read",
                    |read| read.grain.says()
                ),
            ),
            Meter::Tasks(pids) => format!(
                "the cpu time of {} ({}, children reaped included)",
                pids.iter()
                    .map(|pid| pid.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                pids.first()
                    .and_then(|pid| task::process_cpu(*pid))
                    .map_or(task::CpuGrain::Ticks, |read| read.grain)
                    .says(),
            ),
            Meter::Threads(tids) => format!(
                "the cpu time of thread {} (/proc/<tid>/task/<tid>/schedstat, its own)",
                tids.iter()
                    .map(|tid| tid.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Meter::Cpus(None) => "everything but idle on every cpu (/proc/stat)".to_owned(),
            Meter::Cpus(Some(cpus)) => format!(
                "everything but idle on cpu {} (/proc/stat)",
                cpus.iter()
                    .map(|cpu| cpu.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Meter::Cgroup(directory) => {
                format!("{}/cpu.stat usage_usec", directory.display())
            }
            Meter::Nothing(reason) => (*reason).to_owned(),
        }
    }
}
