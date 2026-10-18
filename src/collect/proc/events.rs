//! `proc.exec`, `proc.exit` and `proc.signals` over raw tracepoints.
//!
//! Three sources, one shape: a program in `src/bpf/proc_events.rs` writes one
//! record per event into a ring, and this file turns those records into events
//! the rest of the tool understands. What belongs here is what belongs to the
//! family -- what each event carries, what is refused and why, and what is
//! counted as lost.
//!
//! **What these two unlock is bigger than what they measure.** A count of
//! `execve` calls is not an interesting number on its own. What is interesting
//! is that until now this tool inferred a launched program's lifecycle by
//! polling `/proc`, and defects 1 and 2 of `docs/scenarios/proc.md` are both
//! races that creates: before the child reaches `execve` its `/proc` entry
//! still describes `omn`'s own forked image, and after it exits the entry
//! describes a zombie. `sched_process_exec` and `sched_process_exit` are those
//! two instants published by the kernel rather than guessed at from outside.
//!
//! **A cgroup or container target is refused here rather than served.** The
//! catalog marks both `bpf_only`, which is a statement about which backend
//! could serve them, not a claim that this build does: narrowing a program to a
//! cgroup needs a helper this layer does not have yet, and the request for it
//! is in `docs/open-findings.md`. A target that is refused by name costs a
//! reader one message; a target that is accepted and quietly watches the whole
//! machine costs them a wrong answer.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use crate::bpf::{Exec, ExecProgram, Exit, ExitProgram, Signal, SignalProgram, Whose};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often each ring is drained.
///
/// These events are bursty in the way that fills a ring -- a build system
/// starts a hundred processes in a few milliseconds -- and draining often is
/// cheap. What a drain cannot absorb is counted rather than hidden.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// The ring each program writes into.
///
/// An exec record is 104 bytes and an exit or signal record is 48, so this
/// holds a few thousand of either between drains.
const RING_BYTES: usize = 256 * 1024;

/// The bookkeeping every streaming collector in this file does the same way.
struct Streaming {
    id: CollectorId,
    losses: Losses,
    /// The program's counter is cumulative, so what is new is the difference.
    dropped: u64,
    /// The kernel's skipped runs, cumulative in the same way.
    skipped: u64,
}

impl Streaming {
    fn new(id: &str) -> Self {
        Self {
            id: CollectorId::new(id),
            losses: Losses::default(),
            dropped: 0,
            skipped: 0,
        }
    }

    fn note_undecoded(&mut self, undecoded: u64, what: &str) {
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses.note_times(
                format!("a record in the {what} ring was shorter than one event"),
                undecoded,
            );
        }
    }

    /// Reads a program's own drop counter, which is the only place an event the
    /// ring could not hold is recorded: the BPF ring buffer does not report its
    /// own overflows, so an unread drop is an event that simply never existed.
    ///
    /// `skipped` is the kernel's count of runs it skipped because the program
    /// was already running on that cpu, which no counter of the program's can
    /// hold. It is the machine's, and the reason says so.
    fn absorb_drops(&mut self, dropped: u64, skipped: u64, what: &str) {
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note_times(
                format!(
                "the kernel skipped a run of the {what} program because one was already running \
                 on that cpu, so an event was never seen; the count is the machine's"
            ),
                skipped - self.skipped,
            );
            self.skipped = skipped;
        }
        if dropped > self.dropped {
            self.losses.dropped += dropped - self.dropped;
            self.losses.note_times(
                format!("the {what} ring was full: events happened faster than they were drained"),
                dropped - self.dropped,
            );
            self.dropped = dropped;
        }
    }
}

/// The request's predicate, applied to each record as it is drained.
///
/// The resolver credits every `bpf` backend with evaluating predicates in the
/// program and the runner then leaves its events alone, so a collector that
/// does not apply the filter itself applies it nowhere -- which is what these
/// three did: `proc.exec --filter 'filename == "/nonexistent"'` reported every
/// exec on the machine and said nothing. None of the three programs can narrow
/// on a string or on the fields a predicate names, so the whole predicate runs
/// here, and the run says so and counts what it removed.
struct Predicate {
    expression: Option<FilterExpr>,
    kept: u64,
    removed: u64,
}

impl Predicate {
    fn new(expression: Option<FilterExpr>) -> Self {
        Self {
            expression,
            kept: 0,
            removed: 0,
        }
    }

    /// Whether there is a predicate at all.
    fn is_set(&self) -> bool {
        self.expression.is_some()
    }

    /// Whether the event is one the request asked for, counting the answer.
    fn keeps(&mut self, event: &Event) -> bool {
        let Some(expression) = &self.expression else {
            return true;
        };
        if crate::filter::matches(expression, event) {
            self.kept += 1;
            true
        } else {
            self.removed += 1;
            false
        }
    }

    /// Where the predicate ran and what it did, in the run's own output.
    fn note(&self, what: &str) -> Option<String> {
        let expression = self.expression.as_ref()?;
        Some(format!(
            "the predicate `{expression}` was applied here, over the drained records, not in the \
             program: every {what} on the machine was published and paid for, and it removed {} \
             and kept {}",
            self.removed, self.kept,
        ))
    }
}

/// The targets these two sources have, checked by name.
///
/// `proc.exec` and `proc.exit` are machine-wide by construction: an exec is the
/// moment a pid stops being one program and starts being another, so "watch
/// this process's execs" is a question about a pid that is about to mean
/// something else. A cgroup is not: every process started inside it stays
/// inside it, and the program compares the task's cgroup in the kernel. The
/// catalog declares `system` and `cgroup` and no `pid`, and this is the
/// collector half of that.
fn machine_or_cgroup(target: &Target, source: &str) -> Result<Whose, String> {
    match target {
        Target::System | Target::Cgroup(_) => Whose::of(target),
        Target::Container(_) => Whose::of(target).map_err(|reason| format!("`{source}`: {reason}")),
        other => Err(format!(
            "`{source}` observes the machine or a cgroup, and a {} target names something it \
             cannot watch: a pid is what an exec changes the meaning of. Use --system, or \
             --cgroup for the cgroup the process runs in",
            other.kind()
        )),
    }
}

fn trace_only(mode: Mode, source: &str, what: &str) -> Result<(), String> {
    if mode == Mode::Trace {
        return Ok(());
    }
    Err(format!(
        "`{mode}` mode cannot observe `{source}`: {what} is an event, and a mode that looks on a \
         clock sees the ones that happened to be under the clock"
    ))
}

// ---------------------------------------------------------------- proc.exec

pub struct PrepareExec {
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
}

impl Prepared for PrepareExec {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        trace_only(self.mode, "proc.exec", "an exec")?;
        // A process, or the program a run launched: the execs of it and of
        // its descendants, which the program on the machine reports and this
        // collector keeps by parentage (`Tree`).
        if let Target::Pid(_) = target {
            let root = target.first_pid().expect("a pid target names a pid") as u32;
            // The events carry the kernel's ids, so the tree is rooted at it.
            let root = crate::bpf::pidns::kernel_tgid(root)?;
            let mut collector = ExecCollector::new(self.filter);
            collector.tree = Some(Tree::rooted_at(root));
            collector.root = Some(root);
            return Ok(Box::new(collector));
        }
        let whose = machine_or_cgroup(target, "proc.exec")?;
        Ok(Box::new(ExecCollector::new(self.filter).within(whose)))
    }
}

pub struct ExecCollector {
    inner: Streaming,
    /// The machine, or a cgroup and everything below it.
    whose: Whose,
    program: Option<ExecProgram>,
    /// Paths that filled the record. Counted rather than silently shortened,
    /// because a truncated path and a short one look the same afterwards.
    truncated: u64,
    /// Execs whose path could not be read out of the kernel at all.
    unnamed: u64,
    predicate: Predicate,
    /// A process and its descendants, for a `pid` or launched target.
    tree: Option<Tree>,
    /// The tree's root, and the processes it started as the kernel saw each
    /// fork (`sched_process_fork`): what makes the tree exact, a subshell
    /// that forked and exited before any drain included.
    root: Option<u32>,
    descendants: Option<crate::bpf::Descendants>,
    /// Why the forks are not followed, when they are not: the tree is then
    /// found by walking `/proc` at the drain.
    unfollowed: Option<String>,
    /// Forks the descendants map had no room for, as last read.
    missed_forks: u64,
    /// The last drain's execs the tree did not take, looked at once more.
    undecided: Vec<Exec>,
    /// Set for the drain at `stop`, after which nothing is looked at again.
    last_drain: bool,
}

/// A process and every process descended from it that this run has seen.
///
/// An exec carries its parent's thread group, read in the kernel at the exec.
/// A process is in the tree when it is the root, when its parent is, or when
/// a walk up `/proc/<pid>/stat`'s parents from its parent reaches the tree --
/// the case of a fork that did not exec (a subshell) before its child did.
/// That walk is read at the drain, so a forked parent that never exec'd and
/// had already exited by then is not found, and its child's exec is not
/// kept; the catalog says so.
#[derive(Debug, Clone)]
pub struct Tree {
    members: std::collections::HashSet<u32>,
}

impl Tree {
    pub fn rooted_at(root: u32) -> Self {
        Self {
            members: std::iter::once(root).collect(),
        }
    }

    /// Takes in every `(process, parent)` the kernel saw the tree fork,
    /// in any order: a process is the tree's when its parent is.
    pub fn adopt(&mut self, forks: Vec<(u32, u32)>) {
        let mut pending = forks;
        loop {
            let before = pending.len();
            pending.retain(|(pid, ppid)| {
                if self.members.contains(ppid) {
                    self.members.insert(*pid);
                    false
                } else {
                    true
                }
            });
            if pending.len() == before {
                return;
            }
        }
    }

    /// Which of a batch of execs, `(pid, ppid)` in the order they happened,
    /// are the tree's. In order first, walking `/proc` for a parent it has
    /// not seen; then again until nothing changes, for a child whose parent's
    /// own exec came after it (a subshell that forked a command and then
    /// exec'd its last one).
    pub fn select(&mut self, execs: &[(u32, u32)]) -> Vec<Verdict> {
        let mut verdicts: Vec<Verdict> = execs
            .iter()
            .map(|(pid, ppid)| self.admits(*pid, *ppid))
            .collect();
        loop {
            let mut changed = false;
            for (at, (pid, ppid)) in execs.iter().enumerate() {
                if verdicts[at] != Verdict::In && self.members.contains(ppid) {
                    self.members.insert(*pid);
                    verdicts[at] = Verdict::In;
                    changed = true;
                }
            }
            if !changed {
                return verdicts;
            }
        }
    }

    /// Whether an exec of `pid`, whose parent is `ppid`, is the tree's, and
    /// takes it in when it is.
    pub fn admits(&mut self, pid: u32, ppid: u32) -> Verdict {
        if self.members.contains(&pid) || self.members.contains(&ppid) {
            self.members.insert(pid);
            return Verdict::In;
        }
        let mut walked = Vec::new();
        let mut at = ppid;
        // Bounded: a chain of parents is short, and pid 1 is everyone's end.
        for _ in 0..64 {
            if at <= 1 {
                return Verdict::Out;
            }
            if self.members.contains(&at) {
                self.members.extend(walked);
                self.members.insert(ppid);
                self.members.insert(pid);
                return Verdict::In;
            }
            walked.push(at);
            // A parent that has left `/proc` ends the walk without an
            // answer: its own parent, which might have been the tree's, is
            // no longer readable.
            let Some(parent) = parent_of(at) else {
                return Verdict::Unknown;
            };
            at = parent;
        }
        Verdict::Unknown
    }
}

/// What a [`Tree`] can say of one exec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The tree's.
    In,
    /// Shown not to be: its parents lead to pid 1 without meeting the tree.
    Out,
    /// Not known: a parent on the way had exited, so the chain stops short.
    Unknown,
}

/// Why an exec of unknown parentage is counted among the losses.
const UNKNOWN_PARENTAGE: &str =
    "an exec whose parent had exited before its parentage could be read \
     from `/proc`; whether it descended from the target is unknown, and it is not in the series";

/// The parent of a live process, from `/proc/<pid>/stat`.
fn parent_of(pid: u32) -> Option<u32> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = text.rsplit_once(')')?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

impl ExecCollector {
    /// Narrows the program to a cgroup; the machine otherwise.
    pub fn within(mut self, whose: Whose) -> Self {
        self.whose = whose;
        self
    }

    pub fn new(filter: Option<FilterExpr>) -> Self {
        Self {
            inner: Streaming::new("proc.exec/bpf:raw_tp"),
            whose: Whose::Any,
            program: None,
            truncated: 0,
            unnamed: 0,
            predicate: Predicate::new(filter),
            tree: None,
            root: None,
            descendants: None,
            unfollowed: None,
            missed_forks: 0,
            undecided: Vec::new(),
            last_drain: false,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_mut() else {
            return;
        };
        let id = self.inner.id.clone();
        let mut index = seq;
        let mut undecoded = 0;
        let mut truncated = 0;
        let mut unnamed = 0;
        let mut drained = Vec::new();
        program.drain(|exec: Exec| drained.push(exec), &mut undecoded);
        // Every process the root started, as the kernel saw the fork: read
        // after the execs were drained, so a fork that preceded one of them
        // is in it. Exact where the `/proc` walk below guesses.
        if let (Some(tree), Some(descendants)) = (self.tree.as_mut(), self.descendants.as_ref()) {
            tree.adopt(descendants.parents());
            let missed = descendants.missed();
            if missed > self.missed_forks {
                self.inner.losses.dropped += missed - self.missed_forks;
                self.inner.losses.note_times(
                    "a process the target started found the descendants map full, so its \
                     execs are placed by walking `/proc`",
                    missed - self.missed_forks,
                );
                self.missed_forks = missed;
            }
        }
        if let Some(tree) = self.tree.as_mut() {
            // The previous drain's execs that were not the tree's are looked
            // at once more, and only once: a parent's own exec may arrive in
            // the drain after its child's.
            let mut batch = std::mem::take(&mut self.undecided);
            let carried = batch.len();
            batch.append(&mut drained);
            let pairs: Vec<(u32, u32)> = batch.iter().map(|exec| (exec.tgid, exec.ppid)).collect();
            let verdicts = tree.select(&pairs);
            let mut unknown = 0;
            for (at, (exec, verdict)) in batch.into_iter().zip(verdicts).enumerate() {
                if verdict == Verdict::In {
                    drained.push(exec);
                } else if at >= carried && !self.last_drain {
                    self.undecided.push(exec);
                } else if verdict == Verdict::Unknown {
                    // Dropped for good with its parentage unknown: it may
                    // have been the tree's, so it is a loss, not a filter.
                    unknown += 1;
                }
            }
            if unknown > 0 {
                self.inner.losses.failed_reads += unknown;
                for _ in 0..unknown {
                    self.inner.losses.note(UNKNOWN_PARENTAGE);
                }
            }
            drained.sort_by_key(|exec| exec.ts_ns);
        }
        for exec in drained {
            if exec.filename_truncated() {
                truncated += 1;
            }
            let filename = exec.filename();
            if filename.is_empty() {
                unnamed += 1;
            }
            let event = Event {
                ts_ns: exec.ts_ns,
                ts_err_ns: 0,
                kind: "proc.exec".to_owned(),
                keys: vec![KeyValue::task(exec.tid as i32), KeyValue::comm(exec.comm())],
                attrs: exec_attributes(&exec, &filename),
                collector_id: id.clone(),
                seq: index,
            };
            if self.predicate.keeps(&event) {
                events.push(event);
                index += 1;
            }
        }
        self.truncated += truncated;
        self.unnamed += unnamed;
        self.inner.note_undecoded(undecoded, "exec");
    }
}

impl Default for ExecCollector {
    fn default() -> Self {
        Self::new(None)
    }
}

/// What an exec event carries beside its value.
///
/// `comm` and `filename` are here because the catalog declares them as filter
/// fields, and a predicate over a field no event carries is false everywhere --
/// which empties the series and looks exactly like a machine that ran nothing.
fn exec_attributes(exec: &Exec, filename: &str) -> Vec<Attr> {
    vec![
        Attr::uint("count", 1),
        Attr::uint("tid", u64::from(exec.tid)),
        Attr::uint("pid", u64::from(exec.tgid)),
        Attr::uint("ppid", u64::from(exec.ppid)),
        // The thread id the task had before this exec. For the ordinary
        // fork-then-exec it is the same as `tid`; it differs when a thread
        // other than the group leader execs and replaces the whole group, which
        // is the one case where "the process that ran this program" and "the
        // process that asked for it" are different numbers.
        Attr::int("old_pid", i64::from(exec.old_pid)),
        Attr::uint("cpu", u64::from(exec.cpu)),
        Attr::text("comm", exec.comm()),
        Attr::text("filename", filename),
        // A path that filled the record is reported as such, because a
        // truncated path and a short one are indistinguishable afterwards and
        // only one of them can be matched against.
        Attr::uint("truncated", u64::from(exec.filename_truncated())),
    ]
}

impl Collector for ExecCollector {
    fn id(&self) -> &CollectorId {
        &self.inner.id
    }

    fn value_attr(&self) -> &'static str {
        "count"
    }

    fn start(&mut self) -> Result<(), String> {
        // The forks first, so none between the two attaches is missed.
        if let Some(root) = self.root {
            match crate::bpf::Descendants::follow(root) {
                Ok(descendants) => self.descendants = Some(descendants),
                Err(reason) => self.unfollowed = Some(reason),
            }
        }
        self.program = Some(ExecProgram::load(self.whose, RING_BYTES)?);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        if let Some(program) = self.program.as_ref() {
            let dropped = program.dropped();
            self.inner.absorb_drops(dropped, program.skipped(), "exec");
        }
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.last_drain = true;
        self.drain_into(&mut trailing, u64::MAX / 2);
        if let Some(program) = self.program.as_ref() {
            let dropped = program.dropped();
            self.inner.absorb_drops(dropped, program.skipped(), "exec");
        }
        self.program = None;
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.inner.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.truncated > 0 {
            notes.push(format!(
                "{} path{} longer than the 64 bytes a record carries and {} reported cut short",
                self.truncated,
                if self.truncated == 1 {
                    " was"
                } else {
                    "s were"
                },
                if self.truncated == 1 { "is" } else { "are" },
            ));
        }
        if self.unnamed > 0 {
            notes.push(format!(
                "{} exec{} no path: the kernel could not be read at the address the binprm held",
                self.unnamed,
                if self.unnamed == 1 {
                    " carried"
                } else {
                    "s carried"
                },
            ));
        }
        notes.extend(self.predicate.note("exec"));
        if let Some(reason) = &self.unfollowed {
            notes.push(format!(
                "the target's forks are not followed in the kernel ({reason}), so its tree is \
                 found by walking `/proc` at each drain: a process that forked and exited \
                 between two drains is not found, and its children's execs are counted lost"
            ));
        }
        notes
    }
}

// ---------------------------------------------------------------- proc.exit

pub struct PrepareExit {
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
}

impl Prepared for PrepareExit {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        trace_only(self.mode, "proc.exit", "an exit")?;
        let whose = machine_or_cgroup(target, "proc.exit")?;
        Ok(Box::new(ExitCollector::new(self.filter).within(whose)))
    }
}

pub struct ExitCollector {
    inner: Streaming,
    /// The machine, or a cgroup and everything below it.
    whose: Whose,
    program: Option<ExitProgram>,
    /// Threads that left, and processes that left. They are different numbers
    /// and only one of them is what a reader counting programs means.
    threads: u64,
    processes: u64,
    predicate: Predicate,
}

impl ExitCollector {
    /// Narrows the program to a cgroup; the machine otherwise.
    pub fn within(mut self, whose: Whose) -> Self {
        self.whose = whose;
        self
    }

    pub fn new(filter: Option<FilterExpr>) -> Self {
        Self {
            inner: Streaming::new("proc.exit/bpf:raw_tp"),
            whose: Whose::Any,
            program: None,
            threads: 0,
            processes: 0,
            predicate: Predicate::new(filter),
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_mut() else {
            return;
        };
        let id = self.inner.id.clone();
        let mut index = seq;
        let mut undecoded = 0;
        let mut threads = 0;
        let mut processes = 0;
        let predicate = &mut self.predicate;
        program.drain(
            |exit: Exit| {
                let event = Event {
                    ts_ns: exit.ts_ns,
                    ts_err_ns: 0,
                    kind: "proc.exit".to_owned(),
                    keys: vec![KeyValue::task(exit.tid as i32), KeyValue::comm(exit.comm())],
                    attrs: exit_attributes(&exit),
                    collector_id: id.clone(),
                    seq: index,
                };
                if !predicate.keeps(&event) {
                    return;
                }
                threads += 1;
                if exit.group_dead != 0 {
                    processes += 1;
                }
                events.push(event);
                index += 1;
            },
            &mut undecoded,
        );
        self.threads += threads;
        self.processes += processes;
        self.inner.note_undecoded(undecoded, "exit");
    }
}

impl Default for ExitCollector {
    fn default() -> Self {
        Self::new(None)
    }
}

/// What an exit event carries.
///
/// `code` is the shell's convention and says so: a status for a task that
/// returned, and `128 + signal` for one a signal killed. That is a choice, and
/// the two unambiguous halves travel beside it -- `status` is absent for a
/// signalled death and `signal` is zero for a clean one -- so a reader who does
/// not want the convention never has to accept it. Reporting the kernel's raw
/// `exit_code` under the name `code` would be worse than either: a task killed
/// by `SIGKILL` carries 9 in that word and would read as having exited 9.
fn exit_attributes(exit: &Exit) -> Vec<Attr> {
    let mut attrs = vec![
        Attr::uint("count", 1),
        Attr::uint("tid", u64::from(exit.tid)),
        Attr::uint("pid", u64::from(exit.tgid)),
        Attr::int(
            "code",
            exit.status()
                .map_or_else(|| 128 + i64::from(exit.signal().unwrap_or(0)), i64::from),
        ),
        Attr::int("signal", i64::from(exit.signal().unwrap_or(0))),
        Attr::int("exit_code", i64::from(exit.exit_code)),
        Attr::uint("core_dumped", u64::from(exit.core_dumped())),
        // Every thread's exit fires this tracepoint. Only the last one of a
        // process has this set, and a reader counting programs wants only
        // those -- a count of exits is a count of threads until it is filtered.
        Attr::uint("group_dead", u64::from(exit.group_dead != 0)),
        Attr::uint("cpu", u64::from(exit.cpu)),
        Attr::text("comm", exit.comm()),
    ];
    if let Some(status) = exit.status() {
        attrs.push(Attr::int("status", i64::from(status)));
    }
    attrs
}

impl Collector for ExitCollector {
    fn id(&self) -> &CollectorId {
        &self.inner.id
    }

    fn value_attr(&self) -> &'static str {
        "count"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(ExitProgram::load(self.whose, RING_BYTES)?);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        if let Some(program) = self.program.as_ref() {
            let dropped = program.dropped();
            self.inner.absorb_drops(dropped, program.skipped(), "exit");
        }
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        if let Some(program) = self.program.as_ref() {
            let dropped = program.dropped();
            self.inner.absorb_drops(dropped, program.skipped(), "exit");
        }
        self.program = None;
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.inner.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes: Vec<String> = self.predicate.note("exit").into_iter().collect();
        if self.threads == 0 {
            return notes;
        }
        notes.push(format!(
            "{} thread{} left, of which {} {} the last of {} process; a count of these events \
             is a count of threads, and `group_dead` is what separates the two",
            self.threads,
            if self.threads == 1 { "" } else { "s" },
            self.processes,
            if self.processes == 1 { "was" } else { "were" },
            if self.processes == 1 { "its" } else { "their" },
        ));
        if !ExitProgram::group_dead_is_passed() {
            notes.push(
                "this kernel's `sched_process_exit` does not say whether a thread was its \
                 process's last, so `group_dead` is read from the process's count of live \
                 threads when the tracepoint fires: two threads of one process leaving at the \
                 same instant can both carry it"
                    .to_owned(),
            );
        }
        notes
    }
}

// ------------------------------------------------------------- proc.signals

pub struct PrepareSignals {
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
}

impl Prepared for PrepareSignals {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let mode = self.mode;
        if !matches!(mode, Mode::Trace | Mode::Count) {
            return Err(format!(
                "`{mode}` mode cannot observe signals: a signal is an event, and sampling one in \
                 a hundred of them reports whichever signals happened to be under the clock"
            ));
        }
        let filter = Whose::of(target).map_err(|reason| {
            format!(
                "{reason}, and a signal is sent to a task; use --pid, --cgroup, or --system to \
                 watch the whole machine"
            )
        })?;
        Ok(Box::new(
            SignalCollector::new(filter, mode).with_predicate(self.filter),
        ))
    }
}

pub struct SignalCollector {
    inner: Streaming,
    /// Whose signals are kept, at either end.
    filter: Whose,
    mode: Mode,
    program: Option<SignalProgram>,
    /// A counted run's running totals, by `(target tid, signal)`.
    totals: BTreeMap<(u32, i32), Counted>,
    predicate: Predicate,
}

/// One `(task, signal)` pair's totals over a counted run.
#[derive(Default)]
struct Counted {
    tgid: u32,
    comm: String,
    generated: u64,
    delivered: u64,
    last_ts_ns: u64,
}

impl SignalCollector {
    pub fn new(filter: Whose, mode: Mode) -> Self {
        Self {
            inner: Streaming::new("proc.signals/bpf:raw_tp"),
            filter,
            mode,
            program: None,
            totals: BTreeMap::new(),
            predicate: Predicate::new(None),
        }
    }

    /// The request's predicate, applied to every signal before it is counted
    /// or reported.
    pub fn with_predicate(mut self, filter: Option<FilterExpr>) -> Self {
        self.predicate = Predicate::new(filter);
        self
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_mut() else {
            return;
        };
        let id = self.inner.id.clone();
        let counting = self.mode == Mode::Count;
        let totals = &mut self.totals;
        let predicate = &mut self.predicate;
        let mut index = seq;
        let mut undecoded = 0;
        program.drain(
            |signal: Signal| {
                let build = || Event {
                    ts_ns: signal.ts_ns,
                    ts_err_ns: 0,
                    kind: "proc.signals".to_owned(),
                    keys: vec![
                        KeyValue::task(signal.target_tid as i32),
                        KeyValue::comm(signal.comm()),
                    ],
                    attrs: signal_attributes(&signal),
                    collector_id: id.clone(),
                    seq: index,
                };
                // Built only when something reads it: a counted storm of three
                // hundred thousand signals drains into a map, and building an
                // event per signal it never keeps made the drain outrun its
                // tick.
                let event = (predicate.is_set() || !counting).then(build);
                if let Some(event) = &event {
                    if !predicate.keeps(event) {
                        return;
                    }
                }
                if counting {
                    let row = totals.entry((signal.target_tid, signal.sig)).or_default();
                    row.tgid = signal.target_tgid;
                    row.comm = signal.comm();
                    row.generated += 1;
                    row.delivered += u64::from(signal.result == 0);
                    row.last_ts_ns = signal.ts_ns;
                    return;
                }
                events.extend(event);
                index += 1;
            },
            &mut undecoded,
        );
        self.inner.note_undecoded(undecoded, "signal");
    }

    /// A counted run's one report, which is the shape `count` mode means: the
    /// totals over the whole run rather than a series over it.
    fn counted_events(&self, seq: u64) -> Vec<Event> {
        self.totals
            .iter()
            .enumerate()
            .map(|(index, ((tid, sig), row))| Event {
                ts_ns: row.last_ts_ns,
                ts_err_ns: 0,
                kind: "proc.signals".to_owned(),
                keys: vec![
                    KeyValue::task(*tid as i32),
                    KeyValue::comm(row.comm.clone()),
                ],
                attrs: vec![
                    Attr::uint("count", row.generated),
                    Attr::uint("delivered", row.delivered),
                    Attr::uint("tid", u64::from(*tid)),
                    Attr::uint("pid", u64::from(row.tgid)),
                    Attr::int("sig", i64::from(*sig)),
                    Attr::text("signame", signal_name(*sig)),
                    Attr::text("comm", row.comm.clone()),
                ],
                collector_id: self.inner.id.clone(),
                seq: seq + index as u64,
            })
            .collect()
    }
}

/// What a signal event carries.
///
/// `sig` is the number, because that is what the kernel passes and what is
/// unambiguous; `signame` is beside it so a reader does not have to keep the
/// table in their head. The catalog declares `sig` as the filter field, so a
/// predicate is written against the number.
fn signal_attributes(signal: &Signal) -> Vec<Attr> {
    vec![
        Attr::uint("count", 1),
        Attr::int("sig", i64::from(signal.sig)),
        Attr::text("signame", signal_name(signal.sig)),
        Attr::uint("tid", u64::from(signal.target_tid)),
        Attr::uint("pid", u64::from(signal.target_tgid)),
        Attr::uint("sender_tid", u64::from(signal.sender_tid)),
        Attr::uint("sender_pid", u64::from(signal.sender_tgid)),
        Attr::text("sender_comm", signal.sender_comm()),
        // The kernel's own verdict. A generated signal that was already pending
        // is not delivered twice, so a count of these events is a count of
        // signals raised rather than of signals a handler will see.
        Attr::int("result", i64::from(signal.result)),
        Attr::uint("delivered", u64::from(signal.result == 0)),
        Attr::text("comm", signal.comm()),
    ]
}

/// The name of a signal number, where it has one that is the same everywhere.
///
/// The real-time range is numbered rather than named, because `SIGRTMIN` is a
/// libc constant rather than a kernel one and naming those from a table would
/// be this tool's opinion about somebody's libc.
fn signal_name(sig: i32) -> String {
    const NAMES: [(i32, &str); 31] = [
        (1, "SIGHUP"),
        (2, "SIGINT"),
        (3, "SIGQUIT"),
        (4, "SIGILL"),
        (5, "SIGTRAP"),
        (6, "SIGABRT"),
        (7, "SIGBUS"),
        (8, "SIGFPE"),
        (9, "SIGKILL"),
        (10, "SIGUSR1"),
        (11, "SIGSEGV"),
        (12, "SIGUSR2"),
        (13, "SIGPIPE"),
        (14, "SIGALRM"),
        (15, "SIGTERM"),
        (16, "SIGSTKFLT"),
        (17, "SIGCHLD"),
        (18, "SIGCONT"),
        (19, "SIGSTOP"),
        (20, "SIGTSTP"),
        (21, "SIGTTIN"),
        (22, "SIGTTOU"),
        (23, "SIGURG"),
        (24, "SIGXCPU"),
        (25, "SIGXFSZ"),
        (26, "SIGVTALRM"),
        (27, "SIGPROF"),
        (28, "SIGWINCH"),
        (29, "SIGIO"),
        (30, "SIGPWR"),
        (31, "SIGSYS"),
    ];
    NAMES
        .iter()
        .find(|(number, _)| *number == sig)
        .map_or_else(|| format!("signal_{sig}"), |(_, name)| (*name).to_owned())
}

impl Collector for SignalCollector {
    fn id(&self) -> &CollectorId {
        &self.inner.id
    }

    fn value_attr(&self) -> &'static str {
        "count"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(SignalProgram::load(self.filter, RING_BYTES)?);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        if let Some(program) = self.program.as_ref() {
            let dropped = program.dropped();
            self.inner
                .absorb_drops(dropped, program.skipped(), "signal");
        }
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        if let Some(program) = self.program.as_ref() {
            let dropped = program.dropped();
            self.inner
                .absorb_drops(dropped, program.skipped(), "signal");
        }
        self.program = None;
        if self.mode == Mode::Count {
            trailing = self.counted_events(u64::MAX / 2);
        }
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.inner.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.mode == Mode::Count {
            // Said rather than left to be assumed: this mode reports one total
            // per (task, signal) and the ring still carried every signal to get
            // there, so `count` is cheaper to read and not cheaper to take.
            notes.push(
                "counted: one row per (task, signal) over the whole run. The program still \
                 published one record per signal, so this mode costs what `trace` costs and \
                 reports less"
                    .to_owned(),
            );
        }
        if self.filter == Whose::Any {
            notes.push(
                "machine-wide: every signal on the machine, including the ones this tool \
                 receives itself"
                    .to_owned(),
            );
        }
        notes.extend(self.predicate.note("signal"));
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every filter field the catalog offers has to be on the events, or a
    /// predicate over it is false everywhere and silently empties the series --
    /// which looks exactly like a machine that did nothing.
    fn assert_every_declared_filter_field_is_present(source: &str, attrs: &[Attr]) {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&source.parse().expect("a source id"))
            .unwrap_or_else(|| panic!("{source} is in the catalog"))
            .filter_fields;
        assert!(!declared.is_empty(), "{source} declares filter fields");
        for field in declared {
            assert!(
                attrs.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter on {source} and no event carries one"
            );
        }
    }

    /// A NUL-padded fixed-width name, the way the kernel holds one.
    fn padded<const N: usize>(text: &str) -> [u8; N] {
        let mut bytes = [0u8; N];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        bytes
    }

    fn an_exec() -> Exec {
        Exec {
            ts_ns: 1_000,
            tid: 7,
            tgid: 7,
            old_pid: 7,
            cpu: 3,
            comm: padded("true"),
            filename: padded("/bin/true"),
            ppid: 1,
            pad: 0,
        }
    }

    /// A child whose parent's own exec comes after it in the batch is the
    /// tree's (a subshell that forked a command, then exec'd its last one);
    /// an exec of an unrelated parent is not.
    /// The forks the kernel saw place a process whose parent left `/proc`
    /// long ago: the case the walk cannot answer.
    #[test]
    fn the_forks_place_what_the_walk_cannot() {
        let mut tree = Tree::rooted_at(100);
        // 102 forked from 101, which forked from the root and is gone;
        // 300 is a stranger's child. Given out of order.
        tree.adopt(vec![(102, 101), (300, 299), (101, 100)]);
        assert_eq!(tree.admits(103, 102), Verdict::In);
        assert!(!tree.members.contains(&300));
    }

    #[test]
    fn a_tree_takes_descendants_in_any_order_and_nothing_else() {
        let mut tree = Tree::rooted_at(100);
        // 100 execs; 102 is the child of 101, which execs after it; 900 is a
        // process whose parent is pid 1's and not ours.
        let kept = tree.select(&[(100, 50), (102, 101), (101, 100), (900, 1), (103, 102)]);
        use Verdict::{In, Out};
        assert_eq!(kept, vec![In, In, In, Out, In]);
    }

    /// A parent that has left `/proc` is not evidence the exec is outside
    /// the tree: the verdict is unknown, and a drop of it is a loss.
    #[test]
    fn a_parent_gone_from_proc_leaves_the_exec_unknown_not_outside() {
        let mut tree = Tree::rooted_at(100);
        // No pid above `pid_max` (at most 2^22) is ever in `/proc`.
        let gone = (1 << 22) + 7;
        assert_eq!(tree.admits(901, gone), Verdict::Unknown);
        // Our own parentage reaches pid 1 through live processes.
        let me = std::process::id();
        let parent = parent_of(me).unwrap();
        assert_eq!(tree.admits(me, parent), Verdict::Out);
    }

    fn a_signal() -> Signal {
        Signal {
            ts_ns: 1_000,
            target_tid: 7,
            target_tgid: 7,
            sender_tid: 8,
            sender_tgid: 8,
            comm: padded("sleep"),
            sig: 10,
            result: 0,
            sender_comm: padded("kill"),
        }
    }

    fn an_exit(exit_code: i32) -> Exit {
        Exit {
            ts_ns: 1_000,
            tid: 7,
            tgid: 7,
            exit_code,
            group_dead: 1,
            comm: padded("sh"),
            cpu: 3,
            pad: 0,
        }
    }

    #[test]
    fn every_filter_field_proc_exec_declares_is_on_an_event() {
        let exec = an_exec();
        let filename = exec.filename();
        assert_every_declared_filter_field_is_present(
            "proc.exec",
            &exec_attributes(&exec, &filename),
        );
    }

    #[test]
    fn every_filter_field_proc_exit_declares_is_on_an_event() {
        assert_every_declared_filter_field_is_present(
            "proc.exit",
            &exit_attributes(&an_exit(7 << 8)),
        );
    }

    #[test]
    fn every_filter_field_proc_signals_declares_is_on_an_event() {
        assert_every_declared_filter_field_is_present(
            "proc.signals",
            &signal_attributes(&a_signal()),
        );
    }

    /// A task a signal killed and a task that returned a status carry the same
    /// word in the kernel, and reporting the raw word as `code` would make a
    /// program killed by `SIGKILL` read as having exited 9.
    #[test]
    fn a_signalled_death_and_a_clean_exit_do_not_report_the_same_code() {
        let clean = exit_attributes(&an_exit(7 << 8));
        let value = |attrs: &[Attr], name: &str| {
            attrs
                .iter()
                .find(|attr| attr.name == name)
                .map(|attr| attr.value.to_string())
        };
        assert_eq!(value(&clean, "code").as_deref(), Some("7"));
        assert_eq!(value(&clean, "signal").as_deref(), Some("0"));
        assert_eq!(value(&clean, "status").as_deref(), Some("7"));

        let killed = exit_attributes(&an_exit(9));
        assert_eq!(value(&killed, "code").as_deref(), Some("137"));
        assert_eq!(value(&killed, "signal").as_deref(), Some("9"));
        assert_eq!(
            value(&killed, "status"),
            None,
            "a task a signal killed returned no status, and inventing one is the defect"
        );
    }

    #[test]
    fn an_exec_event_carries_the_path_and_the_name_it_took() {
        let exec = an_exec();
        assert_eq!(exec.filename(), "/bin/true");
        assert_eq!(exec.comm(), "true");
    }

    /// `proc.exit` watches the machine or a cgroup, and a process target is
    /// refused by name rather than quietly widened; `proc.exec` takes a
    /// process, as its tree (the twenty-fourth wave).
    #[test]
    fn a_process_target_is_refused_and_a_cgroup_is_taken() {
        let Err(error) = Box::new(PrepareExit {
            mode: Mode::Trace,
            filter: None,
        })
        .attach(&Target::Pid(vec![1])) else {
            panic!("`proc.exit` does not watch one process");
        };
        assert!(error.contains("pid"), "{error}");
        assert!(error.contains("--system"), "{error}");
        assert!(Box::new(PrepareExec {
            mode: Mode::Trace,
            filter: None,
        })
        .attach(&Target::Pid(vec![1]))
        .is_ok());

        // The hierarchy's root is a cgroup every machine with cgroup2 has.
        if std::path::Path::new("/sys/fs/cgroup/cgroup.procs").exists() {
            assert!(Box::new(PrepareExit {
                mode: Mode::Trace,
                filter: None,
            })
            .attach(&Target::Cgroup("/sys/fs/cgroup".into()))
            .is_ok());
        }
        let Err(error) = Box::new(PrepareExit {
            mode: Mode::Trace,
            filter: None,
        })
        .attach(&Target::Cgroup("/sys/fs/cgroup/no-such-cgroup-here".into())) else {
            panic!("a cgroup that does not exist names nothing to watch");
        };
        assert!(error.contains("no-such-cgroup-here"), "{error}");
    }

    /// A mode that cannot mean anything here is refused with the reason: a
    /// polled exec count is a count of the execs that happened to be under the
    /// clock, which is a number that reads like a measurement.
    #[test]
    fn a_polled_mode_is_refused_with_the_reason() {
        let prepared = Box::new(PrepareExec {
            mode: Mode::Poll {
                interval: "100ms".parse().expect("an interval"),
            },
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::System) else {
            panic!("a polled mode must be refused");
        };
        assert!(error.contains("an exec is an event"), "{error}");
    }

    #[test]
    fn the_machine_target_filters_on_nothing_and_a_pid_target_filters_on_it() {
        let collector = Box::new(PrepareSignals {
            mode: Mode::Trace,
            filter: None,
        })
        .attach(&Target::System)
        .expect("a machine-wide collector");
        assert_eq!(collector.id().as_str(), "proc.signals/bpf:raw_tp");
        let collector = Box::new(PrepareSignals {
            mode: Mode::Trace,
            filter: None,
        })
        // A process that exists: in a container its id is translated to the
        // kernel's, and a pid nobody has is refused there.
        .attach(&Target::Pid(vec![std::process::id() as i32]))
        .expect("a per-process collector");
        assert_eq!(collector.id().as_str(), "proc.signals/bpf:raw_tp");
    }

    #[test]
    fn a_signal_number_that_has_a_portable_name_gets_it_and_the_rest_are_numbered() {
        assert_eq!(signal_name(9), "SIGKILL");
        assert_eq!(signal_name(10), "SIGUSR1");
        assert_eq!(signal_name(15), "SIGTERM");
        // The real-time range is a libc constant rather than a kernel one.
        assert_eq!(signal_name(34), "signal_34");
    }

    /// Whether this machine lets the tests load a program at all. Without the
    /// capabilities there is nothing to test here, and saying so beats a test
    /// that passes because it did nothing.
    fn skip_without_bpf(collector: &mut dyn Collector) -> bool {
        match collector.start() {
            Ok(()) => false,
            Err(reason) => {
                eprintln!("skipped: {reason}");
                true
            }
        }
    }

    /// Ground truth: a process that execs a program whose path this test chose
    /// must appear, with that path.
    ///
    /// The path is the check rather than a count, because the machine is
    /// executing other things at the same time and only this one is known.
    #[test]
    fn an_exec_this_test_performed_is_reported_with_its_path() {
        let mut collector = ExecCollector::new(None);
        if skip_without_bpf(&mut collector) {
            return;
        }
        let true_path =
            crate::collect::launch::find_program("true", std::env::var_os("PATH").as_deref());
        let status = std::process::Command::new(&true_path)
            .status()
            .unwrap_or_else(|error| panic!("{} runs: {error}", true_path.display()));
        assert!(status.success());

        let mut events = collector.sample(0).expect("a drain");
        events.extend(collector.stop());
        let expected = true_path.to_string_lossy().into_owned();
        let found = events
            .iter()
            .any(|event| event.attr("filename").map(ToString::to_string) == Some(expected.clone()));
        assert!(
            found,
            "an exec of {expected} was performed and not reported; {} events seen, losses {:?}",
            events.len(),
            collector.losses()
        );
    }

    /// Ground truth: a process this test started with a status it chose must be
    /// reported leaving with that status.
    #[test]
    fn an_exit_status_this_test_chose_is_the_one_reported() {
        let mut collector = ExitCollector::new(None);
        if skip_without_bpf(&mut collector) {
            return;
        }
        // Alive long enough to be named by the kernel's id, which is the one
        // the program reports and, in a container, not `child.id()`.
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 0.2; exit 7"])
            .spawn()
            .expect("a shell runs");
        let pid = crate::bpf::pidns::kernel_tgid(child.id()).expect("the shell is running");
        let status = child.wait_with_output().expect("the shell finishes").status;
        assert_eq!(status.code(), Some(7));

        let mut events = collector.sample(0).expect("a drain");
        events.extend(collector.stop());
        let mine: Vec<_> = events
            .iter()
            .filter(|event| {
                event.attr("pid").and_then(crate::event::AttrValue::as_f64) == Some(f64::from(pid))
            })
            .collect();
        assert!(
            !mine.is_empty(),
            "a process that exited was not reported; {} events seen, losses {:?}",
            events.len(),
            collector.losses()
        );
        for event in mine {
            assert_eq!(
                event.attr("code").and_then(crate::event::AttrValue::as_f64),
                Some(7.0),
                "the shell was told to exit 7 and the event says otherwise: {event:?}"
            );
        }
    }
}

#[cfg(test)]
mod predicate_tests {
    use super::*;

    fn exec(filename: &str) -> Event {
        Event {
            ts_ns: 1,
            ts_err_ns: 0,
            kind: "proc.exec".to_owned(),
            keys: Vec::new(),
            attrs: vec![Attr::text("filename", filename), Attr::text("comm", "true")],
            collector_id: CollectorId::new("test"),
            seq: 0,
        }
    }

    /// A predicate these programs cannot carry is applied here, counted, and
    /// said -- never accepted and dropped.
    #[test]
    fn a_predicate_is_applied_counted_and_said() {
        let mut predicate = Predicate::new(Some(
            "filename == '/nonexistent/path'"
                .parse()
                .expect("a predicate"),
        ));
        assert!(!predicate.keeps(&exec("/bin/true")));
        assert!(!predicate.keeps(&exec("/usr/bin/env")));
        assert!(predicate.keeps(&exec("/nonexistent/path")));
        let note = predicate.note("exec").expect("a filtered run says so");
        assert!(note.contains("applied here"), "{note}");
        assert!(note.contains("removed 2 and kept 1"), "{note}");

        let mut none = Predicate::new(None);
        assert!(none.keeps(&exec("/bin/true")));
        assert_eq!(none.note("exec"), None);
    }
}
