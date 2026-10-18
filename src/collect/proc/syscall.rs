//! `proc.syscall` over two raw tracepoints, bucketed in the kernel.
//!
//! The measurement is how long each call into the kernel took: a wait opens at
//! `sys_enter` and closes at the `sys_exit` that answers it, and no single
//! event is the number. `src/bpf/proc_syscall.rs` holds both programs and the
//! map between them; this file drains what they counted and turns it into the
//! family's events.
//!
//! **What comes out is already a histogram.** Each drained row is one log2
//! bucket of one `(task, syscall)`, with how many calls fell into it and their
//! exact total. That is not a refinement, it is what makes the source
//! affordable at all: `raw_syscalls:sys_enter` fires on every syscall of every
//! process on the machine, and the catalog entry's own caveat says so. One ring
//! record per syscall would be a measurement whose cost grows with the workload
//! and whose numbers describe a machine running `omn`.
//!
//! **A predicate naming one syscall runs in the kernel.** The catalog says to
//! filter by syscall number inside the program, and the resolver reports any
//! `bpf` backend's filter as pushed down. This collector makes that true rather
//! than merely reported: the number is written into the instruction stream
//! ahead of the map write, so a `syscall == 'read'` run does not pay for the
//! machine's writes. Everything else the predicate says -- `comm`, `pid` -- is
//! applied here over the drained rows, and the notes say which ran where,
//! because a filter reported as pushed down and applied in userspace is the
//! shape `DECISIONS.md` calls the worst a bug takes in this project.

use std::time::Duration as StdDuration;

use super::syscall_names;
use crate::bpf::{Narrowing, SyscallProgram, SyscallRow, Whose};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::{CompareOp, FilterExpr, Value};
use crate::request::{Mode, Target};

/// How often the rows are drained.
///
/// The cost of a drain is one syscall per `(task, syscall, bucket)` row that
/// saw a call in the interval, not per call, which is the whole point of
/// bucketing in the kernel. Draining often also keeps the map far from the
/// capacity at which it would start refusing rows -- and the drain's own
/// syscalls are counted too on a machine-wide run, which is a reason to make
/// few of them.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    /// The request's predicate. Part of it may be pushed into the program and
    /// the rest is applied here; `Prepare` is where the split is decided,
    /// because it is the last place a request can still be refused.
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(self.mode, Mode::Trace | Mode::Count) {
            return Err(format!(
                "`{}` mode cannot measure syscall latency: a call is the gap between two events, \
                 and a sample that catches one of them without the other has no duration to \
                 report",
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
        Ok(Box::new(SyscallCollector::new(
            Narrowing { whose, syscall_nr },
            self.mode,
            self.filter.map(numbers_read_as_nr),
        )))
    }
}

/// The predicate as the userspace pass has to read it: a number compared with
/// `syscall` is compared with `nr`.
///
/// The event's `syscall` is the name, from this build's table, and `nr` is the
/// number the kernel reported. `syscall == 110` is a question about the number
/// -- the push-down reads it that way -- and evaluated against the name it is
/// false for every row, so the kernel would keep getppid and this pass would
/// then drop every call it kept: an empty series from a workload that made
/// thousands, with the program's narrowing reported as though it had worked.
/// Rewritten here once, the kernel's half and this half answer the same
/// question in every position a comparison can sit in, `||` and `!` included.
pub(super) fn numbers_read_as_nr(filter: FilterExpr) -> FilterExpr {
    match filter {
        FilterExpr::And(left, right) => FilterExpr::And(
            Box::new(numbers_read_as_nr(*left)),
            Box::new(numbers_read_as_nr(*right)),
        ),
        FilterExpr::Or(left, right) => FilterExpr::Or(
            Box::new(numbers_read_as_nr(*left)),
            Box::new(numbers_read_as_nr(*right)),
        ),
        FilterExpr::Not(inner) => FilterExpr::Not(Box::new(numbers_read_as_nr(*inner))),
        FilterExpr::Compare {
            field,
            operator,
            value,
        } if field == "syscall" => match value {
            Value::Int { .. } => FilterExpr::Compare {
                field: "nr".to_owned(),
                operator,
                value,
            },
            // A set of numbers is a set of `nr`s; a set mixing numbers and
            // names is either one.
            Value::List(values) => {
                let (numbers, names): (Vec<Value>, Vec<Value>) = values
                    .into_iter()
                    .partition(|value| matches!(value, Value::Int { .. }));
                let compare = |field: &str, values: Vec<Value>| FilterExpr::Compare {
                    field: field.to_owned(),
                    operator,
                    value: Value::List(values),
                };
                match (numbers.is_empty(), names.is_empty()) {
                    (true, _) => compare("syscall", names),
                    (false, true) => compare("nr", numbers),
                    (false, false) => FilterExpr::Or(
                        Box::new(compare("nr", numbers)),
                        Box::new(compare("syscall", names)),
                    ),
                }
            }
            value => FilterExpr::Compare {
                field,
                operator,
                value,
            },
        },
        other => other,
    }
}

/// The one syscall a predicate narrows the program to, if it names one.
///
/// Only a conjunction is looked through. `syscall == 'read' || comm == 'sh'`
/// names a syscall and does not narrow to it: pushing the comparison down would
/// discard every call the other half of the `or` was about, and the events that
/// came back would be a subset nobody asked for. A predicate this cannot use is
/// not an error -- it runs in userspace like any other and the notes say so.
///
/// A name this build's table does not hold **is** an error, and refusing it is
/// the point: `syscall == 'wriet'` would otherwise push nothing down, match
/// nothing in userspace, and report an empty series, which reads exactly like a
/// process that made no syscalls.
pub(super) fn pushed_down_syscall(filter: &FilterExpr) -> Result<Option<u32>, String> {
    match filter {
        FilterExpr::And(left, right) => match pushed_down_syscall(left)? {
            Some(number) => Ok(Some(number)),
            None => pushed_down_syscall(right),
        },
        FilterExpr::Compare {
            field,
            operator: CompareOp::Eq,
            value,
        } if field == "syscall" => match value {
            Value::Int { value, .. } => u32::try_from(*value)
                .map(Some)
                .map_err(|_| format!("`{value}` is not a syscall number; they start at zero")),
            Value::Str(name) => match syscall_names::number(name) {
                Some(number) => Ok(Some(number)),
                None if syscall_names::table_is_for_this_machine() => Err(format!(
                    "this build knows no syscall named `{name}` on {}. A predicate naming one \
                     that does not exist matches nothing, which reads as a process that made no \
                     calls; filter on the number instead if this kernel has one the table does \
                     not",
                    syscall_names::ARCHITECTURE
                )),
                None => Err(format!(
                    "this build carries the syscall table for {} and is not running on it, so \
                     `{name}` cannot be resolved to a number; filter on the number instead",
                    syscall_names::ARCHITECTURE
                )),
            },
            Value::List(_) => Ok(None),
        },
        _ => Ok(None),
    }
}

/// One `(task, comm, syscall, bucket)` row's totals over a counted run.
///
/// Named rather than a tuple in the map's value because three numbers of the
/// same type in a row is where a swap goes unnoticed.
#[derive(Default)]
struct CountedRow {
    calls: u64,
    total_ns: u64,
    errors: u64,
}

/// A counted run's rows, keyed the way the kernel keys them: `(tid, tgid,
/// nr, bucket, comm)`. The name is in the key for the reason the kernel's row
/// carries it there -- a task that renames itself is two rows, each call under
/// the name it was made with.
type CountedRows = std::collections::BTreeMap<(u32, u32, u32, u32, String), CountedRow>;

pub struct SyscallCollector {
    id: CollectorId,
    narrowing: Narrowing,
    mode: Mode,
    program: Option<SyscallProgram>,
    losses: Losses,
    /// The kernel's skipped runs of the program, cumulative, as last read.
    skipped: u64,
    /// Processes the target started that were not followed, so far.
    unfollowed: u64,
    /// The target's calls to other syscalls, which the pushed-down comparison
    /// declined to time, as the program counted them at `stop`.
    declined: u64,
    notes: Vec<String>,
    /// The programs' counters are cumulative, so what is new is the difference.
    dropped_pending: u64,
    dropped_rows: u64,
    /// The end of the last interval, which is the start of this one.
    window_opened_ns: u64,
    /// How many calls this run has bucketed, which is what the event count is
    /// not: each event stands for as many calls as its `count` says.
    calls: u64,
    /// How many rows those calls were reported as, which is the event count.
    rows: u64,
    /// The request's predicate, applied to each row as it is drained.
    predicate: Option<FilterExpr>,
    /// Calls the predicate removed, and calls it kept. Counted in calls rather
    /// than in rows, because a row is not one measurement here.
    filtered_out: u64,
    filtered_in: u64,
    /// A counted run's running totals, by `(tid, tgid, nr, bucket)`.
    totals: CountedRows,
}

impl SyscallCollector {
    pub fn new(narrowing: Narrowing, mode: Mode, predicate: Option<FilterExpr>) -> Self {
        Self {
            id: CollectorId::new("proc.syscall/bpf:raw_tp"),
            narrowing,
            mode,
            program: None,
            losses: Losses::default(),
            skipped: 0,
            unfollowed: 0,
            declined: 0,
            notes: Vec::new(),
            dropped_pending: 0,
            dropped_rows: 0,
            window_opened_ns: 0,
            calls: 0,
            rows: 0,
            predicate,
            filtered_out: 0,
            filtered_in: 0,
            totals: CountedRows::new(),
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let closed_ns = monotonic_ns();
        let window_ns = closed_ns.saturating_sub(self.window_opened_ns);
        let (rows, drain_losses) = match program.drain() {
            Ok(drained) => drained,
            Err(reason) => {
                // A drain that failed is a window whose rows are still in the
                // map: they are not lost, they arrive with the next one. What
                // is lost is knowing which window they belonged to.
                self.losses.failed_reads += 1;
                self.losses.note_times(reason, 1);
                return;
            }
        };
        self.window_opened_ns = closed_ns;

        if drain_losses.vanished > 0 {
            self.losses.undecoded += drain_losses.vanished;
            self.losses
                .note("a syscall row was gone between being listed and being read");
        }
        if drain_losses.truncated {
            self.losses
                .note("the syscall histogram held more rows than one drain can walk");
        }

        let counting = self.mode == Mode::Count;
        for (index, row) in rows.into_iter().enumerate() {
            let event = Event {
                // Stamped where the row's window began rather than at the
                // drain that read it, which is finding 44: a join that opens on
                // a bucketed source would otherwise open at the window's close.
                // A zero is the kernel having recorded no beginning, and falls
                // back to the drain rather than to the beginning of time.
                ts_ns: if row.first_ns == 0 {
                    closed_ns
                } else {
                    row.first_ns
                },
                ts_err_ns: 0,
                kind: "proc.syscall".to_owned(),
                // The calling task is the series: this is its latency, not the
                // machine's. The syscall and the name ride beside it, so `-k
                // syscall` is one run's `strace -c` table and `-k comm` the
                // bpftrace tools' per-name one.
                keys: keys(&row),
                attrs: attributes(&row, window_ns),
                collector_id: self.id.clone(),
                seq: seq + index as u64,
            };
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += row.count;
                    continue;
                }
                self.filtered_in += row.count;
            }
            self.calls += row.count;
            if counting {
                let total = self
                    .totals
                    .entry((row.tid, row.tgid, row.nr, row.bucket, row.comm))
                    .or_default();
                total.calls += row.count;
                total.total_ns += row.total_ns;
                total.errors += row.errors;
                continue;
            }
            self.rows += 1;
            events.push(event);
        }
    }

    /// A counted run's one report: the same rows, summed over the whole run
    /// instead of over one interval.
    ///
    /// Deliberately still a histogram. The other shape `count` could take is
    /// one mean per `(task, syscall)`, and a mean over a distribution this
    /// skewed is the thing `STATE.md` records as hiding exactly what it was
    /// read to find -- a task whose `futex` calls are mostly instant and
    /// occasionally forty milliseconds has no meaningful average.
    fn counted_events(&mut self, seq: u64) -> Vec<Event> {
        let window_ns = monotonic_ns().saturating_sub(self.window_opened_ns);
        let closed_ns = monotonic_ns();
        let mut events = Vec::with_capacity(self.totals.len());
        for (index, ((tid, tgid, nr, bucket, comm), total)) in self.totals.iter().enumerate() {
            let row = SyscallRow {
                tid: *tid,
                tgid: *tgid,
                nr: *nr,
                bucket: *bucket,
                count: total.calls,
                total_ns: total.total_ns,
                errors: total.errors,
                first_ns: 0,
                comm: comm.clone(),
            };
            self.rows += 1;
            events.push(Event {
                // Stamped where the row's window began rather than at the
                // drain that read it, which is finding 44: a join that opens on
                // a bucketed source would otherwise open at the window's close.
                // A zero is the kernel having recorded no beginning, and falls
                // back to the drain rather than to the beginning of time.
                ts_ns: if row.first_ns == 0 {
                    closed_ns
                } else {
                    row.first_ns
                },
                ts_err_ns: 0,
                kind: "proc.syscall".to_owned(),
                keys: keys(&row),
                attrs: attributes(&row, window_ns),
                collector_id: self.id.clone(),
                seq: seq + index as u64,
            });
        }
        events
    }

    /// Reads the programs' own counters of what they could not record.
    ///
    /// A full map is the only way this source loses a call, and nothing else
    /// anywhere would record that it happened: the helper returns an error in
    /// the kernel and the call is simply never counted.
    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        // The kernel's own count of runs it skipped, which no counter of the
        // program's can hold. It is the machine's -- the program runs on every
        // occurrence and narrows to the target after -- so it is an upper
        // bound on what the target lost, and the reason says so.
        let skipped = program.skipped();
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note_times(
                "the kernel skipped a run of this program because one was already running on \
                 that cpu, so an occurrence was never seen; the count is the machine's, an \
                 upper bound on the target's share, and every total here is a lower bound",
                skipped - self.skipped,
            );
            self.skipped = skipped;
        }
        let unfollowed = program.unfollowed();
        if unfollowed > self.unfollowed {
            self.losses.dropped += unfollowed - self.unfollowed;
            self.losses.note_times(
                "a process the target started was not followed: the map of the processes it \
                 started was full, so its calls are in no total",
                unfollowed - self.unfollowed,
            );
            self.unfollowed = unfollowed;
        }
        let (pending, rows) = program.dropped();
        if pending > self.dropped_pending {
            self.losses.dropped += pending - self.dropped_pending;
            self.losses.note_times(
                "more threads were inside a syscall at once than the pending map holds, so \
                 their calls were never timed",
                pending - self.dropped_pending,
            );
            self.dropped_pending = pending;
        }
        if rows > self.dropped_rows {
            self.losses.dropped += rows - self.dropped_rows;
            self.losses.note_times(
                "more distinct (task, syscall, bucket) rows appeared in one interval than the \
                 histogram holds, so their calls were never counted",
                rows - self.dropped_rows,
            );
            self.dropped_rows = rows;
        }
    }
}

/// The keys a row's event carries: its task first, which is the series when
/// no key is asked for, then the syscall and the name.
fn keys(row: &SyscallRow) -> Vec<KeyValue> {
    vec![
        KeyValue::task(row.tid as i32),
        KeyValue::syscall(syscall_names::label(row.nr)),
        KeyValue::comm(row.comm.clone()),
    ]
}

/// What an event carries beside its value.
///
/// `comm`, `pid` and `syscall` are here because the catalog entry declares them
/// as filter fields: a predicate over a field no event carries is false
/// everywhere, which empties the series and looks exactly like a process that
/// made no calls.
fn attributes(row: &SyscallRow, window_ns: u64) -> Vec<Attr> {
    vec![
        // The value. The low bound of the bucket rather than its mean, because
        // it is the one number that puts the row back in the same bucket when
        // the emit layer buckets it again: a histogram built in the kernel and
        // one built in userspace from these events agree exactly.
        Attr::uint("lat_ns", row.bucket_low_ns()),
        // How many calls this event stands for. Every number derived from
        // `lat_ns` is this many times over, which is what the emit layer reads
        // out of `weight`; `count` carries the same number under the name a
        // reader and the filter grammar expect.
        Attr::uint("count", row.count),
        Attr::uint(crate::emit::WEIGHT_ATTR, row.count),
        // The exact sum of those durations, which the bucket cannot give back:
        // a log2 bucket is a factor of two wide, so a total built from bounds
        // inherits that error and this does not.
        Attr::uint("total_ns", row.total_ns),
        // How many of `count` returned an errno (`[-4095, -1]`), which is
        // `strace -c`'s `errors` column. Summed, not weighted: it is already
        // a count of calls.
        Attr::uint("errors", row.errors),
        Attr::uint("bucket", u64::from(row.bucket)),
        // The interval the row was collected over. Not a denominator: a call is
        // counted where it ended, so a call longer than the tick reports
        // nanoseconds the window does not contain.
        Attr::uint("window_ns", window_ns),
        Attr::uint("tid", u64::from(row.tid)),
        Attr::uint("pid", u64::from(row.tgid)),
        Attr::text("comm", row.comm.clone()),
        // The name where this build has one, and `syscall_<n>` where it does
        // not. The number travels beside it, because the name is this build's
        // table and the number is what the kernel said.
        Attr::text("syscall", syscall_names::label(row.nr)),
        Attr::uint("nr", u64::from(row.nr)),
    ]
}

impl Collector for SyscallCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "lat_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(SyscallProgram::load(self.narrowing)?);
        self.window_opened_ns = monotonic_ns();
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
        // A thread inside a syscall when the run ended has a real call that is
        // in no bucket, because it has no end yet. A machine with threads
        // parked in `epoll_wait` always has some, so this is a fact about the
        // window rather than something lost.
        if let Some(program) = self.program.as_ref() {
            self.declined = program.skipped_by_predicate();
            let open = program.still_pending();
            if open > 0 {
                self.notes.push(format!(
                    "{open} thread{} inside a syscall when the run ended; the call it was in has \
                     no end yet and is in no bucket",
                    if open == 1 { " was" } else { "s were" }
                ));
            }
        }
        self.program = None;
        if self.mode == Mode::Count {
            return self.counted_events(u64::MAX / 2);
        }
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = self.notes.clone();
        // Where the predicate ran, said in the run's own output. The resolver
        // reports any `bpf` backend's filter as pushed down, and for this
        // source that is true of one comparison and not of the others; a
        // reader deciding whether the cost was paid needs to know which.
        match self.narrowing.syscall_nr {
            Some(nr) => notes.push(format!(
                "only `{}` ({nr}) was timed: the predicate on `syscall` was pushed into the \
                 program, so no other syscall is in these numbers at all; the program skipped \
                 {} call{} to other syscalls without timing them",
                syscall_names::label(nr),
                self.declined,
                if self.declined == 1 { "" } else { "s" },
            )),
            None if self.predicate.is_some() => notes.push(
                "the predicate names no single syscall, so every syscall in range was timed and \
                 the predicate was applied here, over the drained rows: the events it removed \
                 were counted before they were dropped"
                    .to_owned(),
            ),
            None => {}
        }
        if self.filtered_out > 0 {
            notes.push(format!(
                "the predicate removed {} calls here and kept {}",
                self.filtered_out, self.filtered_in,
            ));
        }
        if self.calls > 0 {
            // Said in the run's own output rather than left in documentation,
            // because every other source in this tool makes one event one
            // measurement and this one does not.
            notes.push(format!(
                "{} calls are reported as {} rows of a kernel-side log2 histogram, each carrying \
                 the calls it stands for; \
                 `lat_ns` is its bucket's low bound, a factor of two below the next: the buckets \
                 and `min` are bucketed and `max` is a lower bound, while the sum, mean, rate, \
                 topk and stack totals come from `total_ns`, the exact sum",
                self.calls, self.rows,
            ));
        }
        if self.narrowing.whose == Whose::Any {
            notes.push(
                "machine-wide: every syscall on the machine, including the ones this tool makes \
                 to drain the histogram"
                    .to_owned(),
            );
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> SyscallRow {
        SyscallRow {
            tid: 7,
            tgid: 7,
            nr: 1,
            bucket: 11,
            count: 42,
            total_ns: 55_000,
            errors: 3,
            first_ns: 0,
            comm: "sh".to_owned(),
        }
    }

    /// Every filter field the catalog offers must be on the events, or a
    /// predicate over it is false everywhere and silently empties the series.
    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let attributes = attributes(&row(), 100_000);
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&"proc.syscall".parse().expect("a source id"))
            .expect("proc.syscall is in the catalog")
            .filter_fields;
        assert!(!declared.is_empty(), "the entry declares filter fields");
        for field in declared {
            assert!(
                attributes.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    /// The name and the number are both on the event, because they answer
    /// different questions: the number is what the kernel said and the name is
    /// this build's table talking about it.
    #[test]
    fn an_event_carries_the_syscall_by_name_and_by_number() {
        let attributes = attributes(&row(), 100_000);
        let value = |name: &str| {
            attributes
                .iter()
                .find(|attr| attr.name == name)
                .map(|attr| attr.value.to_string())
        };
        assert_eq!(value("nr").as_deref(), Some("1"));
        if syscall_names::table_is_for_this_machine() {
            // 1 is `write` on x86-64 and `io_destroy` on aarch64.
            let expected = if cfg!(target_arch = "x86_64") {
                "write"
            } else {
                "io_destroy"
            };
            assert_eq!(value("syscall").as_deref(), Some(expected));
        }
    }

    #[test]
    fn a_sampled_mode_is_refused_with_the_reason() {
        let prepared = Box::new(Prepare {
            mode: Mode::Sample {
                freq: "997hz".parse().expect("a frequency"),
            },
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::Pid(vec![1])) else {
            panic!("a sampled mode must be refused");
        };
        assert!(error.contains("syscall latency"), "{error}");
    }

    #[test]
    fn a_target_without_a_process_is_refused_by_name() {
        let prepared = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::Iface("eth0".to_owned())) else {
            panic!("an interface makes no syscalls");
        };
        assert!(error.contains("iface"), "{error}");
        assert!(error.contains("--system"), "{error}");
    }

    /// The whole promise of this source's caveat, from the request's side.
    #[test]
    fn a_predicate_naming_one_syscall_becomes_a_number_the_program_can_hold() {
        let Some(getppid) = syscall_names::number("getppid") else {
            return;
        };
        let by_name: FilterExpr = "syscall == 'getppid'".parse().expect("a predicate");
        assert_eq!(pushed_down_syscall(&by_name), Ok(Some(getppid)));
        let by_number: FilterExpr = format!("syscall == {getppid}")
            .parse()
            .expect("a predicate");
        assert_eq!(pushed_down_syscall(&by_number), Ok(Some(getppid)));
        // A conjunction still names it; the other half runs in userspace.
        let conjunction: FilterExpr = "comm == 'python3' && syscall == 'getppid'"
            .parse()
            .expect("a predicate");
        assert_eq!(pushed_down_syscall(&conjunction), Ok(Some(getppid)));
    }

    /// `syscall == <getppid's number>` is pushed down as the number, so the userspace pass has
    /// to read it as the number too: against the name, every row the kernel
    /// kept would be dropped.
    #[test]
    fn a_syscall_number_is_compared_with_nr_in_userspace_wherever_it_sits() {
        let (Some(p), Some(w)) = (
            syscall_names::number("getppid"),
            syscall_names::number("write"),
        ) else {
            return;
        };
        let event = |nr: u32| Event {
            ts_ns: 1,
            ts_err_ns: 0,
            kind: "proc.syscall".to_owned(),
            keys: Vec::new(),
            attrs: attributes(&SyscallRow { nr, ..row() }, 100_000),
            collector_id: CollectorId::new("test"),
            seq: 0,
        };
        let getppid = event(p);
        let write = event(w);
        for (text, keeps_getppid, keeps_write) in [
            (format!("syscall == {p}"), true, false),
            (format!("syscall != {p}"), false, true),
            (format!("syscall in ({p}, 9999)"), true, false),
            (format!("syscall in ({p}, 'write')"), true, true),
            (format!("comm == 'sh' && syscall == {p}"), true, false),
            (format!("syscall == {p} || syscall == 'write'"), true, true),
            (format!("!(syscall == {w})"), true, false),
            ("syscall == 'getppid'".to_owned(), true, false),
        ] {
            let predicate = numbers_read_as_nr(text.parse().expect("a predicate"));
            assert_eq!(
                crate::filter::matches(&predicate, &getppid),
                keeps_getppid,
                "`{text}` read as `{predicate}` on getppid"
            );
            assert_eq!(
                crate::filter::matches(&predicate, &write),
                keeps_write,
                "`{text}` read as `{predicate}` on write"
            );
        }
    }

    /// A disjunction names a syscall and does not narrow to one: pushing the
    /// comparison down would throw away everything the other half was about,
    /// and the series that came back would be a subset nobody asked for.
    #[test]
    fn a_disjunction_is_not_pushed_down_even_though_it_names_a_syscall() {
        let expression: FilterExpr = "syscall == 'getppid' || comm == 'sh'"
            .parse()
            .expect("a predicate");
        assert_eq!(pushed_down_syscall(&expression), Ok(None));
    }

    /// A misspelled syscall would push nothing down, match nothing in
    /// userspace, and report an empty series -- which reads exactly like a
    /// process that made no calls. It is refused instead, by name.
    #[test]
    fn a_syscall_name_this_build_does_not_know_is_refused_rather_than_matched() {
        let expression: FilterExpr = "syscall == 'wriet'".parse().expect("a predicate");
        let Err(reason) = pushed_down_syscall(&expression) else {
            panic!("`wriet` is not a syscall");
        };
        assert!(reason.contains("wriet"), "{reason}");
        assert!(reason.contains(syscall_names::ARCHITECTURE), "{reason}");

        let prepared = Box::new(Prepare {
            mode: Mode::Trace,
            filter: Some(expression),
        });
        assert!(prepared.attach(&Target::Pid(vec![1])).is_err());
    }

    /// The notes have to say where the filter ran. The resolver credits every
    /// `bpf` backend with pushing predicates down, and for this source that is
    /// true of one comparison and not of the others.
    #[test]
    fn the_notes_say_which_half_of_the_predicate_ran_in_the_kernel() {
        let Some(getppid) = syscall_names::number("getppid") else {
            return;
        };
        let collector = SyscallCollector::new(
            Narrowing {
                whose: Whose::Process(1),
                syscall_nr: Some(getppid),
            },
            Mode::Trace,
            Some("syscall == 'getppid'".parse().expect("a predicate")),
        );
        let notes = collector.notes().join("\n");
        assert!(notes.contains("pushed into the program"), "{notes}");
        assert!(notes.contains("getppid"), "{notes}");

        let userspace = SyscallCollector::new(
            Narrowing {
                whose: Whose::Process(1),
                syscall_nr: None,
            },
            Mode::Trace,
            Some("comm == 'sh'".parse().expect("a predicate")),
        );
        let notes = userspace.notes().join("\n");
        assert!(notes.contains("applied here"), "{notes}");
    }

    /// Whether this machine lets the tests load a program at all.
    fn skip_without_bpf(collector: &mut SyscallCollector) -> bool {
        match collector.start() {
            Ok(()) => false,
            Err(reason) => {
                eprintln!("skipped: {reason}");
                true
            }
        }
    }

    /// Ground truth: this thread calls `getppid` a number of times this test
    /// chose, and `getppid` is a syscall glibc does not cache and nothing else
    /// in a Rust test binary makes.
    ///
    /// The comparison is against that number, not against what the collector
    /// computed. Narrowed to this process and to that one syscall, so the rest
    /// of the parallel test suite cannot contribute.
    #[test]
    fn a_known_number_of_calls_is_counted_as_that_number() {
        const CALLS: u64 = 500;
        let getppid = syscall_names::number("getppid").expect("the table has getppid");
        let mut collector = SyscallCollector::new(
            Narrowing {
                // The program compares the kernel's id, which in a container
                // is not the one this process reads as its own.
                whose: Whose::Process(
                    crate::bpf::pidns::kernel_tgid(std::process::id()).expect("this process"),
                ),
                syscall_nr: Some(getppid),
            },
            Mode::Trace,
            None,
        );
        if skip_without_bpf(&mut collector) {
            return;
        }

        for _ in 0..CALLS {
            // Safety: `getppid` takes no arguments, returns a pid and cannot
            // fail. It is here rather than behind a crate because the point is
            // to make exactly this many of exactly this syscall.
            unsafe { libc::getppid() };
        }

        let mut events = collector.sample(0).expect("a drain");
        events.extend(collector.stop());
        let counted: f64 = events
            .iter()
            .filter(|event| {
                event.attr("nr").and_then(crate::event::AttrValue::as_f64)
                    == Some(f64::from(getppid))
            })
            .filter_map(|event| {
                event
                    .attr("count")
                    .and_then(crate::event::AttrValue::as_f64)
            })
            .sum();
        assert_eq!(
            counted as u64,
            CALLS,
            "{CALLS} calls to getppid were made and {counted} were counted; losses {:?}",
            collector.losses()
        );
        assert!(collector.losses().is_clean(), "{:?}", collector.losses());
    }

    /// Ground truth of the *duration*, which the count says nothing about: a
    /// sleep of a length this test chose must land in the bucket that holds it
    /// and have a mean within a tenth of it.
    ///
    /// The mean comes from `total_ns`, which is exact, rather than from the
    /// bucket, which is a factor of two wide.
    #[test]
    fn an_injected_delay_is_reported_as_that_delay() {
        const SLEEPS: u64 = 10;
        const NANOS: u64 = 50_000_000;
        let clock_nanosleep =
            syscall_names::number("clock_nanosleep").expect("the table has clock_nanosleep");
        let mut collector = SyscallCollector::new(
            Narrowing {
                // The program compares the kernel's id, which in a container
                // is not the one this process reads as its own.
                whose: Whose::Process(
                    crate::bpf::pidns::kernel_tgid(std::process::id()).expect("this process"),
                ),
                syscall_nr: Some(clock_nanosleep),
            },
            Mode::Trace,
            None,
        );
        if skip_without_bpf(&mut collector) {
            return;
        }

        for _ in 0..SLEEPS {
            std::thread::sleep(StdDuration::from_nanos(NANOS));
        }

        let mut events = collector.sample(0).expect("a drain");
        events.extend(collector.stop());
        let number = |event: &Event, name: &str| {
            event
                .attr(name)
                .and_then(crate::event::AttrValue::as_f64)
                .unwrap_or_default()
        };
        let mine: Vec<_> = events
            .iter()
            .filter(|event| number(event, "nr") == f64::from(clock_nanosleep))
            .collect();
        let count: f64 = mine.iter().map(|event| number(event, "count")).sum();
        let total: f64 = mine.iter().map(|event| number(event, "total_ns")).sum();
        assert!(
            count >= SLEEPS as f64,
            "{SLEEPS} sleeps were performed and {count} calls counted; losses {:?}",
            collector.losses()
        );
        let mean = total / count;
        // A sleep is a lower bound on its own duration -- the kernel returns
        // when the timer has expired and the thread is scheduled -- so this
        // asserts the separation rather than a calibrated threshold: the mean
        // is at least the sleep and not twice it.
        assert!(
            (NANOS as f64..2.0 * NANOS as f64).contains(&mean),
            "a {NANOS}ns sleep reported a mean of {mean}ns over {count} calls"
        );
    }
}
