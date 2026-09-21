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

use crate::bpf::{Exec, ExecProgram, Exit, ExitProgram, Signal, SignalProgram};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
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
}

impl Streaming {
    fn new(id: &str) -> Self {
        Self {
            id: CollectorId::new(id),
            losses: Losses::default(),
            dropped: 0,
        }
    }

    fn note_undecoded(&mut self, undecoded: u64, what: &str) {
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses.note(format!(
                "a record in the {what} ring was shorter than one event"
            ));
        }
    }

    /// Reads a program's own drop counter, which is the only place an event the
    /// ring could not hold is recorded: the BPF ring buffer does not report its
    /// own overflows, so an unread drop is an event that simply never existed.
    fn absorb_drops(&mut self, dropped: u64, what: &str) {
        if dropped > self.dropped {
            self.losses.dropped += dropped - self.dropped;
            self.losses.note(format!(
                "the {what} ring was full: events happened faster than they were drained"
            ));
            self.dropped = dropped;
        }
    }
}

/// The one target these two sources have, checked by name.
///
/// `proc.exec` and `proc.exit` are machine-wide by construction: an exec is the
/// moment a pid stops being one program and starts being another, so "watch
/// this process's execs" is a question about a pid that is about to mean
/// something else. The catalog declares `system`, `cgroup` and `container` and
/// no `pid`, and this is the collector half of that.
fn machine_wide_only(target: &Target, source: &str) -> Result<(), String> {
    match target {
        Target::System => Ok(()),
        Target::Cgroup(_) | Target::Container(_) => Err(format!(
            "`{source}` cannot be narrowed to a {} in this build: the program keeps every event \
             on the machine, and filtering one out by cgroup needs a helper the eBPF layer does \
             not have yet. Use --system and filter on `comm`, which costs the events rather than \
             reporting the wrong ones",
            target.kind()
        )),
        other => Err(format!(
            "`{source}` observes the machine, and a {} target names something narrower than it \
             can watch; use --system",
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

pub struct PrepareExec(pub Mode);

impl Prepared for PrepareExec {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        trace_only(self.0, "proc.exec", "an exec")?;
        machine_wide_only(target, "proc.exec")?;
        Ok(Box::new(ExecCollector::new()))
    }
}

pub struct ExecCollector {
    inner: Streaming,
    program: Option<ExecProgram>,
    /// Paths that filled the record. Counted rather than silently shortened,
    /// because a truncated path and a short one look the same afterwards.
    truncated: u64,
    /// Execs whose path could not be read out of the kernel at all.
    unnamed: u64,
}

impl ExecCollector {
    pub fn new() -> Self {
        Self {
            inner: Streaming::new("proc.exec/bpf:raw_tp"),
            program: None,
            truncated: 0,
            unnamed: 0,
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
        program.drain(
            |exec: Exec| {
                if exec.filename_truncated() {
                    truncated += 1;
                }
                let filename = exec.filename();
                if filename.is_empty() {
                    unnamed += 1;
                }
                events.push(Event {
                    ts_ns: exec.ts_ns,
                    ts_err_ns: 0,
                    kind: "proc.exec".to_owned(),
                    keys: vec![KeyValue::task(exec.tid as i32)],
                    attrs: exec_attributes(&exec, &filename),
                    collector_id: id.clone(),
                    seq: index,
                });
                index += 1;
            },
            &mut undecoded,
        );
        self.truncated += truncated;
        self.unnamed += unnamed;
        self.inner.note_undecoded(undecoded, "exec");
    }
}

impl Default for ExecCollector {
    fn default() -> Self {
        Self::new()
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
        self.program = Some(ExecProgram::load(RING_BYTES)?);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        if let Some(program) = self.program.as_ref() {
            let dropped = program.dropped();
            self.inner.absorb_drops(dropped, "exec");
        }
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        if let Some(program) = self.program.as_ref() {
            let dropped = program.dropped();
            self.inner.absorb_drops(dropped, "exec");
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
        notes
    }
}

// ---------------------------------------------------------------- proc.exit

pub struct PrepareExit(pub Mode);

impl Prepared for PrepareExit {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        trace_only(self.0, "proc.exit", "an exit")?;
        machine_wide_only(target, "proc.exit")?;
        Ok(Box::new(ExitCollector::new()))
    }
}

pub struct ExitCollector {
    inner: Streaming,
    program: Option<ExitProgram>,
    /// Threads that left, and processes that left. They are different numbers
    /// and only one of them is what a reader counting programs means.
    threads: u64,
    processes: u64,
}

impl ExitCollector {
    pub fn new() -> Self {
        Self {
            inner: Streaming::new("proc.exit/bpf:raw_tp"),
            program: None,
            threads: 0,
            processes: 0,
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
        program.drain(
            |exit: Exit| {
                threads += 1;
                if exit.group_dead != 0 {
                    processes += 1;
                }
                events.push(Event {
                    ts_ns: exit.ts_ns,
                    ts_err_ns: 0,
                    kind: "proc.exit".to_owned(),
                    keys: vec![KeyValue::task(exit.tid as i32)],
                    attrs: exit_attributes(&exit),
                    collector_id: id.clone(),
                    seq: index,
                });
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
        Self::new()
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
        self.program = Some(ExitProgram::load(RING_BYTES)?);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        if let Some(program) = self.program.as_ref() {
            let dropped = program.dropped();
            self.inner.absorb_drops(dropped, "exit");
        }
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        if let Some(program) = self.program.as_ref() {
            let dropped = program.dropped();
            self.inner.absorb_drops(dropped, "exit");
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
        if self.threads == 0 {
            return Vec::new();
        }
        vec![format!(
            "{} thread{} left, of which {} {} the last of {} process; a count of these events \
             is a count of threads, and `group_dead` is what separates the two",
            self.threads,
            if self.threads == 1 { "" } else { "s" },
            self.processes,
            if self.processes == 1 { "was" } else { "were" },
            if self.processes == 1 { "its" } else { "their" },
        )]
    }
}

// ------------------------------------------------------------- proc.signals

pub struct PrepareSignals(pub Mode);

impl Prepared for PrepareSignals {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let mode = self.0;
        if !matches!(mode, Mode::Trace | Mode::Count) {
            return Err(format!(
                "`{mode}` mode cannot observe signals: a signal is an event, and sampling one in \
                 a hundred of them reports whichever signals happened to be under the clock"
            ));
        }
        let filter = match target {
            Target::System => None,
            Target::Pid(_) | Target::Tid(_) => {
                Some(target.first_pid().expect("a pid target names a process") as u32)
            }
            Target::Cgroup(_) | Target::Container(_) => {
                return Err(format!(
                    "`proc.signals` cannot be narrowed to a {} in this build: the program filters \
                     by thread group, and filtering by cgroup needs a helper the eBPF layer does \
                     not have yet. Use --system, or --pid for one process",
                    target.kind()
                ))
            }
            other => {
                return Err(format!(
                    "a {} target names no process, and a signal is sent to one; use --pid, or \
                     --system to watch the whole machine",
                    other.kind()
                ))
            }
        };
        Ok(Box::new(SignalCollector::new(filter, mode)))
    }
}

pub struct SignalCollector {
    inner: Streaming,
    /// The thread group whose signals are kept, or none for the machine.
    filter: Option<u32>,
    mode: Mode,
    program: Option<SignalProgram>,
    /// A counted run's running totals, by `(target tid, signal)`.
    totals: BTreeMap<(u32, i32), Counted>,
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
    pub fn new(filter: Option<u32>, mode: Mode) -> Self {
        Self {
            inner: Streaming::new("proc.signals/bpf:raw_tp"),
            filter,
            mode,
            program: None,
            totals: BTreeMap::new(),
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_mut() else {
            return;
        };
        let id = self.inner.id.clone();
        let counting = self.mode == Mode::Count;
        let totals = &mut self.totals;
        let mut index = seq;
        let mut undecoded = 0;
        program.drain(
            |signal: Signal| {
                if counting {
                    let row = totals.entry((signal.target_tid, signal.sig)).or_default();
                    row.tgid = signal.target_tgid;
                    row.comm = signal.comm();
                    row.generated += 1;
                    row.delivered += u64::from(signal.result == 0);
                    row.last_ts_ns = signal.ts_ns;
                    return;
                }
                events.push(Event {
                    ts_ns: signal.ts_ns,
                    ts_err_ns: 0,
                    kind: "proc.signals".to_owned(),
                    keys: vec![KeyValue::task(signal.target_tid as i32)],
                    attrs: signal_attributes(&signal),
                    collector_id: id.clone(),
                    seq: index,
                });
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
                keys: vec![KeyValue::task(*tid as i32)],
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
            self.inner.absorb_drops(dropped, "signal");
        }
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        if let Some(program) = self.program.as_ref() {
            let dropped = program.dropped();
            self.inner.absorb_drops(dropped, "signal");
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
        if self.filter.is_none() {
            notes.push(
                "machine-wide: every signal on the machine, including the ones this tool \
                 receives itself"
                    .to_owned(),
            );
        }
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
        }
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

    /// `proc.exec` and `proc.exit` watch the machine, and a target that names
    /// something narrower is refused by name rather than quietly widened.
    #[test]
    fn a_narrower_target_than_the_machine_is_refused_by_name() {
        let Err(error) = Box::new(PrepareExec(Mode::Trace)).attach(&Target::Pid(vec![1])) else {
            panic!("`proc.exec` does not watch one process");
        };
        assert!(error.contains("pid"), "{error}");
        assert!(error.contains("--system"), "{error}");

        let Err(error) =
            Box::new(PrepareExit(Mode::Trace)).attach(&Target::Cgroup("/sys/fs/cgroup".into()))
        else {
            panic!("a cgroup cannot be filtered on by this build");
        };
        assert!(error.contains("cgroup"), "{error}");
        assert!(error.contains("helper"), "{error}");
    }

    /// A mode that cannot mean anything here is refused with the reason: a
    /// polled exec count is a count of the execs that happened to be under the
    /// clock, which is a number that reads like a measurement.
    #[test]
    fn a_polled_mode_is_refused_with_the_reason() {
        let prepared = Box::new(PrepareExec(Mode::Poll {
            interval: "100ms".parse().expect("an interval"),
        }));
        let Err(error) = prepared.attach(&Target::System) else {
            panic!("a polled mode must be refused");
        };
        assert!(error.contains("an exec is an event"), "{error}");
    }

    #[test]
    fn the_machine_target_filters_on_nothing_and_a_pid_target_filters_on_it() {
        let collector = Box::new(PrepareSignals(Mode::Trace))
            .attach(&Target::System)
            .expect("a machine-wide collector");
        assert_eq!(collector.id().as_str(), "proc.signals/bpf:raw_tp");
        let collector = Box::new(PrepareSignals(Mode::Trace))
            .attach(&Target::Pid(vec![4242]))
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
        let mut collector = ExecCollector::new();
        if skip_without_bpf(&mut collector) {
            return;
        }
        let status = std::process::Command::new("/bin/true")
            .status()
            .expect("/bin/true runs");
        assert!(status.success());

        let mut events = collector.sample(0).expect("a drain");
        events.extend(collector.stop());
        let found = events.iter().any(|event| {
            event.attr("filename").map(ToString::to_string) == Some("/bin/true".to_owned())
        });
        assert!(
            found,
            "an exec of /bin/true was performed and not reported; {} events seen, losses {:?}",
            events.len(),
            collector.losses()
        );
    }

    /// Ground truth: a process this test started with a status it chose must be
    /// reported leaving with that status.
    #[test]
    fn an_exit_status_this_test_chose_is_the_one_reported() {
        let mut collector = ExitCollector::new();
        if skip_without_bpf(&mut collector) {
            return;
        }
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 7"])
            .spawn()
            .expect("a shell runs");
        let pid = child.id();
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
