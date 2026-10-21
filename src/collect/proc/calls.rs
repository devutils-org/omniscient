//! `proc.calls`: one event per syscall, from `src/bpf/proc_calls.rs`.
//!
//! What `strace -T`, `perf trace` and `opensnoop` print a line for: which call,
//! when it began, how long it took, what it returned -- an errno as `errno`
//! beside the raw `ret` -- and for `openat` the path and the descriptor it
//! returned. `proc.syscall` is the cheap aggregate of the same calls; this is
//! the record of each.

use std::time::Duration as StdDuration;

use super::syscall::{numbers_read_as_nr, pushed_down_syscall};
use super::syscall_names;
use crate::bpf::{Call, CallsNarrowing, CallsProgram, PathCall, Whose, PATH_CALLS};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// 4 MiB: about twenty thousand records between two drains.
const RING_BYTES: usize = 4 * 1024 * 1024;

pub struct Prepare {
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
    /// The classic tracepoints (`bpf:tp`), for a kernel without BTF.
    pub classic: bool,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot record a call: each record is one call's beginning and end, \
                 which only `trace` sees",
                self.mode
            ));
        }
        let whose = Whose::of(target).map_err(|reason| {
            format!(
                "{reason}, and a syscall is made by a task; name a process with --pid, a thread \
                 with --tid, a cgroup with --cgroup, or use --system to watch the whole machine"
            )
        })?;
        let syscall_nr = match self.filter.as_ref() {
            Some(filter) => pushed_down_syscall(filter)?,
            None => None,
        };
        let paths = if syscall_names::table_is_for_this_machine() {
            path_calls()
        } else {
            [None; PATH_CALLS]
        };
        Ok(Box::new(CallsCollector {
            classic: self.classic,
            narrowing: CallsNarrowing {
                whose,
                syscall_nr,
                paths,
            },
            program: None,
            predicate: self.filter.map(numbers_read_as_nr),
            losses: Losses::default(),
            dropped_pending: 0,
            unfollowed: 0,
            dropped_ring: 0,
            skipped: 0,
            unread_paths: 0,
            truncated_paths: 0,
            filtered_out: 0,
            open_at_end: 0,
        }))
    }
}

pub struct CallsCollector {
    /// Over `raw_syscalls:*` rather than the raw tracepoints.
    classic: bool,
    narrowing: CallsNarrowing,
    program: Option<CallsProgram>,
    predicate: Option<FilterExpr>,
    losses: Losses,
    dropped_pending: u64,
    /// Processes the target started that were not followed, so far.
    unfollowed: u64,
    dropped_ring: u64,
    skipped: u64,
    /// `openat` calls whose path could not be read out of the caller.
    unread_paths: u64,
    truncated_paths: u64,
    filtered_out: u64,
    open_at_end: usize,
}

/// The errno a return value carries, or zero: the kernel returns `-errno` in
/// `[-4095, -1]`, and every other value is a result.
pub fn errno_of(ret: i64) -> u64 {
    if (-4095..0).contains(&ret) {
        (-ret) as u64
    } else {
        0
    }
}

/// The calls whose path a record carries, by this machine's numbers: the
/// opens (the path their argument names, read when they return) and the
/// execs (read when they begin). A call this architecture does not have --
/// `open` and `creat` on aarch64 -- is left out.
fn path_calls() -> [Option<PathCall>; PATH_CALLS] {
    [
        ("open", 0, false),
        ("openat", 1, false),
        ("openat2", 1, false),
        ("creat", 0, false),
        ("execve", 0, true),
        ("execveat", 1, true),
    ]
    .map(|(name, argument, at_entry)| {
        syscall_names::number(name).map(|nr| PathCall {
            nr,
            argument,
            at_entry,
        })
    })
}

/// The path call `nr` is, when it is one.
fn path_call(paths: &[Option<PathCall>; PATH_CALLS], nr: u32) -> Option<PathCall> {
    paths.iter().flatten().find(|call| call.nr == nr).copied()
}

fn event_of(call: &Call, seq: u64, paths: &[Option<PathCall>; PATH_CALLS]) -> Event {
    let syscall = syscall_names::label(call.nr);
    let errno = errno_of(call.ret);
    let mut attrs = vec![
        Attr::uint("lat_ns", call.dur_ns),
        Attr::int("ret", call.ret),
        Attr::uint("errno", errno),
        Attr::uint("error", u64::from(errno != 0)),
        Attr::uint("tid", u64::from(call.tid)),
        Attr::uint("pid", u64::from(call.tgid)),
        Attr::text("comm", call.comm()),
        Attr::text("syscall", syscall.clone()),
        Attr::uint("nr", u64::from(call.nr)),
    ];
    if let Some(path_call) = path_call(paths, call.nr) {
        attrs.push(Attr::text("path", call.path().unwrap_or_default()));
        // An open returns the descriptor; an exec returns nothing to a
        // caller that succeeded.
        if call.ret >= 0 && !path_call.at_entry {
            attrs.push(Attr::int("fd", call.ret));
        }
    }
    Event {
        ts_ns: call.ts_ns,
        ts_err_ns: 0,
        kind: "proc.calls".to_owned(),
        keys: vec![
            KeyValue::task(call.tid as i32),
            KeyValue::syscall(syscall),
            KeyValue::comm(call.comm()),
        ],
        attrs,
        collector_id: CollectorId::unstamped(),
        seq,
    }
}

impl CallsCollector {
    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_mut() else {
            return;
        };
        let mut undecoded = 0;
        let mut drained = Vec::new();
        program.drain(|call| drained.push(call), &mut undecoded);
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the call ring was shorter than one call");
        }
        for call in drained {
            if path_call(&self.narrowing.paths, call.nr).is_some() {
                if call.path_read < 0 {
                    self.unread_paths += 1;
                } else if call.path_truncated() {
                    self.truncated_paths += 1;
                }
            }
            let event = event_of(&call, seq + events.len() as u64, &self.narrowing.paths);
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += 1;
                    continue;
                }
            }
            events.push(event);
        }
    }

    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let skipped = program.skipped();
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note_times(
                "the kernel skipped a run of a call program because one was already running on \
                 that cpu, so a call was never seen; the count is the machine's",
                skipped - self.skipped,
            );
            self.skipped = skipped;
        }
        let unfollowed = program.unfollowed();
        if unfollowed > self.unfollowed {
            self.losses.dropped += unfollowed - self.unfollowed;
            self.losses.note_times(
                "a process the target started was not followed: the map of the processes it \
                 started was full, so its calls are in no record",
                unfollowed - self.unfollowed,
            );
            self.unfollowed = unfollowed;
        }
        let (pending, ring) = program.dropped();
        if pending > self.dropped_pending {
            self.losses.dropped += pending - self.dropped_pending;
            self.losses.note_times(
                "more threads were inside a syscall at once than the pending map holds, so \
                 their calls were never recorded",
                pending - self.dropped_pending,
            );
            self.dropped_pending = pending;
        }
        if ring > self.dropped_ring {
            self.losses.dropped += ring - self.dropped_ring;
            self.losses.note_times(
                "the call ring was full: calls returned faster than they were drained; a \
                 `syscall` filter narrows the program to one call",
                ring - self.dropped_ring,
            );
            self.dropped_ring = ring;
        }
    }
}

impl Collector for CallsCollector {
    fn value_attr(&self) -> &'static str {
        "lat_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(if self.classic {
            CallsProgram::load_classic(self.narrowing, RING_BYTES)?
        } else {
            CallsProgram::load(self.narrowing, RING_BYTES)?
        });
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        self.absorb_drops();
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        self.absorb_drops();
        if let Some(program) = self.program.as_ref() {
            self.open_at_end = program.still_pending();
        }
        self.program = None;
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some(nr) = self.narrowing.syscall_nr {
            notes.push(format!(
                "only `{}` ({nr}) was recorded: the predicate on `syscall` was pushed into the \
                 program",
                syscall_names::label(nr)
            ));
        }
        if self.filtered_out > 0 {
            notes.push(format!(
                "the predicate removed {} calls here, after they were recorded",
                self.filtered_out
            ));
        }
        if self.unread_paths > 0 {
            notes.push(format!(
                "{} `openat` path{} could not be read out of the caller's memory and {} empty",
                self.unread_paths,
                if self.unread_paths == 1 { "" } else { "s" },
                if self.unread_paths == 1 { "is" } else { "are" },
            ));
        }
        if self.truncated_paths > 0 {
            notes.push(format!(
                "{} `openat` path{} filled the record and {} cut at {} bytes",
                self.truncated_paths,
                if self.truncated_paths == 1 { "" } else { "s" },
                if self.truncated_paths == 1 {
                    "is"
                } else {
                    "are"
                },
                Call::PATH_BYTES,
            ));
        }
        if self.open_at_end > 0 {
            notes.push(format!(
                "{} thread{} inside a syscall when the run ended, and no record of that call",
                self.open_at_end,
                if self.open_at_end == 1 {
                    " was"
                } else {
                    "s were"
                }
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_errno_is_a_return_in_the_kernels_range_and_nothing_else() {
        assert_eq!(errno_of(-2), 2);
        assert_eq!(errno_of(-4095), 4095);
        assert_eq!(errno_of(-4096), 0);
        assert_eq!(errno_of(0), 0);
        assert_eq!(errno_of(3), 0);
    }

    #[test]
    fn every_filter_field_the_catalog_offers_is_on_an_openat_event() {
        let Some(openat) = syscall_names::number("openat") else {
            return;
        };
        let catalog = crate::catalog::Catalog::embedded().unwrap();
        let entry = catalog.get(&"proc.calls".parse().unwrap()).unwrap();
        let mut bytes = [0u8; 184];
        bytes[16..24].copy_from_slice(&3i64.to_ne_bytes());
        bytes[32..36].copy_from_slice(&openat.to_ne_bytes());
        bytes[36..40].copy_from_slice(&5i32.to_ne_bytes());
        bytes[56..60].copy_from_slice(b"/etc");
        let call = Call::from_bytes(&bytes).unwrap();
        let event = event_of(&call, 0, &path_calls());
        for field in &entry.filter_fields {
            assert!(event.attr(field).is_some(), "`{field}` is not on the event");
        }
        assert_eq!(event.attr("fd").unwrap().as_f64(), Some(3.0));
    }

    /// An exec carries its path and no descriptor; every open this machine
    /// has carries a path.
    #[test]
    fn an_exec_carries_its_path_and_no_descriptor() {
        let Some(execve) = syscall_names::number("execve") else {
            return;
        };
        let mut bytes = [0u8; 184];
        bytes[32..36].copy_from_slice(&execve.to_ne_bytes());
        bytes[36..40].copy_from_slice(&10i32.to_ne_bytes());
        bytes[56..65].copy_from_slice(b"/bin/true");
        let call = Call::from_bytes(&bytes).unwrap();
        let event = event_of(&call, 0, &path_calls());
        assert_eq!(
            event.attr("path").map(|value| value.to_string()),
            Some("/bin/true".to_owned())
        );
        assert!(event.attr("fd").is_none());
        let paths = path_calls();
        for name in ["openat", "openat2", "execve", "execveat"] {
            let nr = syscall_names::number(name).expect(name);
            assert!(path_call(&paths, nr).is_some(), "{name}");
        }
    }
}
