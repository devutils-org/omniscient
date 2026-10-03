//! `mem.reclaim` and `mem.compaction` over pairs of raw tracepoints, bucketed
//! in the kernel.
//!
//! **Direct reclaim is the clearest memory problem there is, and until now this
//! repository's worked example of a source that is vocabulary and not a
//! measurement.** A thread asks for a page, there is none free, and instead of
//! being handed one it is made to go and find one: write back dirty pages, drop
//! cache, swap. That happens on the thread's own stack, inside the allocation,
//! and it is invisible to every other source here. A cpu profile shows the
//! thread running -- it is running, in `shrink_node` -- a footprint shows a flat
//! resident size, and the program is simply slow.
//!
//! Compaction is the same bargain for contiguity: the allocator wants a
//! two-megabyte block, the free pages are scattered, and somebody has to move
//! them. It is what a machine pays for transparent huge pages when memory is
//! fragmented, and it is the reason `mem.thp` and this source are read together.
//!
//! Neither is one event, so `src/bpf/mem_stall.rs` loads two programs per
//! source and this file drains what they paired up. As with the other
//! kernel-aggregating sources here, **one event is not one episode**: a row
//! carries the episodes it stands for as `emit::WEIGHT_ATTR`, and the exact
//! total beside the bucket, because a log2 bucket is a factor of two wide.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration as StdDuration;

use super::allocations::descendants;
use super::cgroup::Cgroup;
use crate::bpf::{Stall, StallDrops, StallProgram, StallRow, SCOPE_CGROUP};
use crate::clock::monotonic_ns;
use crate::collect::{cannot_mean_anything, not_built, Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the rows are drained.
///
/// One syscall per `(task, cgroup, bucket)` row that saw an episode, not per
/// episode. Reclaim is rare enough that most drains find nothing at all, which
/// is what makes a hundred milliseconds cheap here.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// Holds the request until the target exists.
pub struct Prepare {
    pub stall: Stall,
    pub mode: Mode,
    /// The request's predicate, applied over the drained rows; see
    /// [`StallCollector::predicate`].
    pub filter: Option<FilterExpr>,
}

impl Prepare {
    fn source(&self) -> &'static str {
        source_of(self.stall)
    }
}

fn source_of(stall: Stall) -> &'static str {
    match stall {
        Stall::Reclaim => "mem.reclaim",
        Stall::Compaction => "mem.compaction",
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(self.mode, Mode::Trace | Mode::Count) {
            return Err(cannot_mean_anything(format!(
                "`{}` mode cannot measure {}: the measurement is the gap between two events, \
                 and a sample that catches one of them without the other has no duration to \
                 report",
                self.mode,
                self.source(),
            )));
        }

        let mut tgid = None;
        let mut cgroups = None;
        match (self.stall, target) {
            (_, Target::System) => {}
            (Stall::Reclaim, Target::Pid(_) | Target::Tid(_)) => {
                tgid = Some(target.first_pid().expect("a pid target names a pid") as u32);
            }
            (Stall::Reclaim, Target::Cgroup(_)) => {
                cgroups = Some(descendants(&Cgroup::resolve(target)?)?);
            }
            (Stall::Reclaim, Target::Container(_)) => {
                return Err(not_built(
                    "a container has to be resolved to its cgroup first, which is not built \
                     yet; pass `--cgroup` with the path",
                ))
            }
            // Compaction moves pages for a whole zone. The thread that entered
            // it is the one that wanted the contiguous block, and the work is
            // done for everyone; attributing the cost to that thread's process
            // would report a machine-wide job as one program's.
            (Stall::Compaction, other) => {
                return Err(cannot_mean_anything(format!(
                    "compaction is work done for a whole memory zone and a {} target names one \
                     part of the machine; use --system",
                    other.kind()
                )))
            }
            (Stall::Reclaim, other) => {
                return Err(cannot_mean_anything(format!(
                    "direct reclaim is attributed to the thread that entered it, and a {} \
                     target names none; use --pid, --cgroup or --system",
                    other.kind()
                )))
            }
        }

        Ok(Box::new(StallCollector::new(
            self.stall,
            self.mode,
            tgid,
            cgroups,
            self.filter,
        )))
    }
}

pub struct StallCollector {
    id: CollectorId,
    stall: Stall,
    mode: Mode,
    /// The thread group whose episodes are kept, filtered in the program.
    tgid: Option<u32>,
    /// The cgroups whose episodes are kept, matched over the drained rows.
    cgroups: Option<BTreeSet<u64>>,
    program: Option<StallProgram>,
    losses: Losses,
    /// The kernel's skipped runs of the program, cumulative, as last read.
    skipped: u64,
    /// The programs' counters are cumulative, so what is new is the difference.
    dropped: StallDrops,
    window_opened_ns: u64,
    run_opened_ns: u64,
    counted: BTreeMap<(u32, u64, u32, u32), StallRow>,
    /// Episodes this run measured, which is what the event count is not.
    episodes: u64,
    rows: u64,
    /// Rows whose cgroup was not the one the request named.
    outside: u64,
    /// The request's predicate. The resolver credits a `bpf` backend with
    /// filtering in the kernel from its *name*, so a collector that ignores the
    /// predicate returns everything and nothing says the filter never ran.
    predicate: Option<FilterExpr>,
    filtered_out: u64,
    filtered_in: u64,
    /// Episodes still in progress when the run ended, which is a fact about the
    /// window rather than a loss.
    still_inside: usize,
    /// Halves of the source this kernel does not publish, read off the programs
    /// once they are loaded.
    absent: Vec<String>,
    /// Episodes by scope, so the run can say how much of what it saw was the
    /// machine running out and how much was a cgroup running out.
    global: u64,
    in_cgroup: u64,
}

impl StallCollector {
    pub fn new(
        stall: Stall,
        mode: Mode,
        tgid: Option<u32>,
        cgroups: Option<BTreeSet<u64>>,
        predicate: Option<FilterExpr>,
    ) -> Self {
        Self {
            id: CollectorId::new(match stall {
                Stall::Reclaim => "mem.reclaim/bpf:raw_tp",
                Stall::Compaction => "mem.compaction/bpf:raw_tp",
            }),
            stall,
            mode,
            tgid,
            cgroups,
            program: None,
            losses: Losses::default(),
            skipped: 0,
            dropped: StallDrops::default(),
            window_opened_ns: 0,
            run_opened_ns: 0,
            counted: BTreeMap::new(),
            episodes: 0,
            rows: 0,
            outside: 0,
            predicate,
            filtered_out: 0,
            filtered_in: 0,
            still_inside: 0,
            absent: Vec::new(),
            global: 0,
            in_cgroup: 0,
        }
    }

    fn counting(&self) -> bool {
        self.mode == Mode::Count
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
                self.losses.failed_reads += 1;
                self.losses.note(reason);
                return;
            }
        };
        self.window_opened_ns = closed_ns;

        if drain_losses.vanished > 0 {
            self.losses.undecoded += drain_losses.vanished;
            self.losses
                .note("a stall row was gone between being listed and being read");
        }
        if drain_losses.truncated {
            self.losses
                .note("the stall histogram held more rows than one drain can walk");
        }

        let stall = self.stall;
        let mut index = 0;
        for row in rows {
            if let Some(wanted) = &self.cgroups {
                if !wanted.contains(&row.cgroup_id) {
                    self.outside += row.count;
                    continue;
                }
            }
            // Stamped at the moment the window's first episode began, rather
            // than at the drain that read the row. A bucketed source emits one
            // event per row per window, and a join that *opens* on it used to
            // open at the close -- so everything that happened in the same
            // window as the first episode fell outside the state it was meant
            // to open. The seventh wave's machine crossing worked around that
            // by sleeping 300 ms between writing and reading.
            //
            // `ts_err_ns` stays zero, and that is deliberate: the instant is
            // exact and taken in this domain, which is what that field is
            // about. What is wide is the *window*, and `window_ns` beside the
            // value already says how wide.
            let ts_ns = stamp(row.first_ns, closed_ns);
            let mut attrs = attributes(stall, &row, window_ns);
            attrs.push(Attr::uint(
                "enclosing_ns",
                enclosing(&row, ts_ns, closed_ns),
            ));
            let event = Event {
                ts_ns,
                ts_err_ns: 0,
                kind: source_of(stall).to_owned(),
                keys: keys_of(stall, &row),
                attrs,
                collector_id: self.id.clone(),
                seq: seq + index,
            };
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += row.count;
                    continue;
                }
                self.filtered_in += row.count;
            }
            self.episodes += row.count;
            if row.scope == SCOPE_CGROUP {
                self.in_cgroup += row.count;
            } else {
                self.global += row.count;
            }
            if self.counting() {
                self.counted
                    .entry((row.tid, row.cgroup_id, row.bucket, row.scope))
                    .and_modify(|held| {
                        held.count += row.count;
                        held.total_ns += row.total_ns;
                        held.yielded += row.yielded;
                        held.comm.clone_from(&row.comm);
                    })
                    .or_insert(row);
                continue;
            }
            self.rows += 1;
            index += 1;
            events.push(event);
        }
    }

    /// Reads the programs' own counters of what they could not record.
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
            self.losses.note(
                "the kernel skipped a run of this program because one was already running on \
                 that cpu, so an occurrence was never seen; the count is the machine's, an \
                 upper bound on the target's share, and every total here is a lower bound",
            );
            self.skipped = skipped;
        }
        let now = program.dropped();
        let episode = self.stall.episode();
        for (new, old, reason) in [
            (
                now.rows,
                self.dropped.rows,
                format!(
                    "more distinct (task, cgroup, duration) rows appeared in one interval than \
                     the stall histogram holds, so those episodes of {episode} were never counted"
                ),
            ),
            (
                now.in_flight,
                self.dropped.in_flight,
                format!(
                    "more threads were inside {episode} at once than the in-flight map holds, so \
                     their episodes were never timed"
                ),
            ),
            (
                now.unpaired,
                self.dropped.unpaired,
                format!(
                    "{episode} ended that this run never saw begin -- it was already under way \
                     when the programs attached, or its beginning found the map full -- so it \
                     happened and has no duration"
                ),
            ),
        ] {
            if new > old {
                self.losses.dropped += new - old;
                self.losses.note(reason);
            }
        }
        self.dropped = now;
    }
}

fn keys_of(stall: Stall, row: &StallRow) -> Vec<KeyValue> {
    match stall {
        // The thread that paid for it is the series: this is its stall, not the
        // machine's.
        Stall::Reclaim => {
            let mut keys = vec![KeyValue::task(row.tid as i32)];
            if row.cgroup_id != 0 {
                keys.push(KeyValue::Cgroup { id: row.cgroup_id });
            }
            keys
        }
        // The entry declares no key, and it is right not to: the work is done
        // for a zone, and keying it by the thread that happened to trigger it
        // would invite a reader to add up one program's share of a machine-wide
        // job. The thread is still on the event, as an attribute.
        Stall::Compaction => Vec::new(),
    }
}

/// What an event carries beside its value.
fn attributes(stall: Stall, row: &StallRow, window_ns: u64) -> Vec<Attr> {
    let mut attrs = vec![
        // The low bound of the bucket rather than its mean, because it is the
        // one number that puts the row back in the same bucket when the emit
        // layer buckets it again.
        Attr::uint("stall_ns", row.duration_ns()),
        // How many episodes this event stands for.
        Attr::uint("count", row.count),
        Attr::uint(crate::emit::WEIGHT_ATTR, row.count),
        // The exact sum, which the bucket bounds cannot give back.
        Attr::uint("total_ns", row.total_ns),
        Attr::uint("bucket", u64::from(row.bucket)),
        Attr::uint("window_ns", window_ns),
        Attr::uint("tid", u64::from(row.tid)),
        Attr::uint("pid", u64::from(row.tgid)),
        Attr::text("comm", row.comm.clone()),
    ];
    if stall == Stall::Reclaim {
        // The other half of the question. A thread that stalled ten
        // milliseconds and reclaimed nothing is in a different situation from
        // one that stalled ten milliseconds and freed a hundred megabytes, and
        // a duration alone cannot tell them apart.
        attrs.push(Attr::uint("pages_reclaimed", row.yielded));
        // Why it was reclaiming, which is two different problems with the same
        // shape: the machine has no free memory, or this cgroup has none of its
        // allowance left. The second is a limit somebody chose and can change.
        attrs.push(Attr::text(
            "scope",
            if row.scope == SCOPE_CGROUP {
                "cgroup"
            } else {
                "global"
            },
        ));
    }
    if row.cgroup_id != 0 {
        attrs.push(Attr::uint("cgroup_id", row.cgroup_id));
    }
    attrs
}

impl Collector for StallCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "stall_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        let program = StallProgram::load(self.stall, self.tgid)?;
        self.absent = program.absent().to_vec();
        self.program = Some(program);
        self.run_opened_ns = monotonic_ns();
        self.window_opened_ns = self.run_opened_ns;
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
        if let Some(inside) = self.program.as_ref().map(StallProgram::still_inside) {
            self.still_inside = inside;
        }
        let stall = self.stall;
        if self.counting() {
            let closed_ns = monotonic_ns();
            let window_ns = closed_ns.saturating_sub(self.run_opened_ns);
            let seq = u64::MAX / 2;
            let counted = std::mem::take(&mut self.counted);
            self.rows += counted.len() as u64;
            for (index, row) in counted.into_values().enumerate() {
                trailing.push(Event {
                    ts_ns: closed_ns,
                    ts_err_ns: 0,
                    kind: source_of(stall).to_owned(),
                    keys: keys_of(stall, &row),
                    attrs: attributes(stall, &row, window_ns),
                    collector_id: self.id.clone(),
                    seq: seq + index as u64,
                });
            }
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
        if self.episodes > 0 {
            notes.push(format!(
                "{} episodes are reported as {} rows of a kernel-side log2 histogram, each \
                 carrying the episodes it stands for; \
                 `stall_ns` is its bucket's low bound, a factor of two below the next: the buckets \
                 and `min` are bucketed and `max` is a lower bound, while the sum, mean, rate, \
                 topk and stack totals come from `total_ns`, the exact sum",
                self.episodes, self.rows,
            ));
        }
        if self.global > 0 && self.in_cgroup > 0 {
            notes.push(format!(
                "{} of those episodes were the machine itself out of free memory and {} were a \
                 cgroup out of its own allowance; the two are one measurement and two problems, \
                 and each event says which under `scope`",
                self.global, self.in_cgroup,
            ));
        }
        // Said whether or not anything was measured: zero episodes on a kernel
        // that publishes only half the source is not the same answer as zero on
        // one that publishes all of it.
        for reason in &self.absent {
            notes.push(format!(
                "one half of this source was not attached, so nothing it would have seen is in \
                 these numbers: {reason}"
            ));
        }
        if self.still_inside > 0 {
            notes.push(format!(
                "{} thread{} still inside when the run ended; that episode has no end yet and \
                 is in no bucket",
                self.still_inside,
                if self.still_inside == 1 {
                    " was"
                } else {
                    "s were"
                },
            ));
        }
        if self.outside > 0 {
            notes.push(format!(
                "{} episodes happened outside the cgroup this run named and were dropped here, \
                 after the kernel had timed them: the cgroup is matched over the drained rows \
                 rather than in the program",
                self.outside,
            ));
        }
        if self.filtered_out > 0 || self.filtered_in > 0 {
            notes.push(format!(
                "the filter kept {} episodes and removed {}; it ran here, over the drained rows, \
                 not in the kernel, so the removed ones were counted before they were dropped",
                self.filtered_in, self.filtered_out,
            ));
        }
        if self.counting() {
            notes.push(
                "`count` reports one total per row over the whole window, so there is no series \
                 here and `rate` over it is the window's mean; `--mode trace` is the one that \
                 reports a count per drain"
                    .to_owned(),
            );
        }
        notes
    }
}

/// When an aggregated row happened, as far as this source can say.
///
/// The first episode of the window if the kernel recorded one, and the drain
/// that read the row otherwise. A zero `first_ns` means unknown -- no row this
/// build creates leaves it unset -- and is read as unknown rather than as the
/// beginning of time, which would put every such event before the run started.
fn stamp(first_ns: u64, closed_ns: u64) -> u64 {
    match first_ns {
        0 => closed_ns,
        first => first,
    }
}

/// The length, from a drained row's stamp, of a window that encloses every
/// episode it counts: to the last episode's end when the kernel recorded one,
/// and to the drain's close otherwise, which also encloses them, only less
/// tightly. An `overlap` rule's `enclosing:` reads it (`join::Rule::enclosing`).
///
/// Only on the rows a drain emits. A `count` run's rows are added up across
/// drains and stamped at the run's close, so they have no window forward of
/// their stamp; a rule that names this attribute on them is refused by the
/// join for the missing attribute rather than given a window of zero.
fn enclosing(row: &StallRow, ts_ns: u64, closed_ns: u64) -> u64 {
    if row.last_ns > ts_ns {
        row.last_ns - ts_ns
    } else {
        closed_ns.saturating_sub(ts_ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> StallRow {
        StallRow {
            tid: 31,
            tgid: 30,
            cgroup_id: 0,
            // Bucket 21 starts at 2^20 nanoseconds, just over a millisecond.
            bucket: 21,
            scope: crate::bpf::SCOPE_GLOBAL,
            count: 6,
            total_ns: 9_000_000,
            yielded: 512,
            first_ns: 0,
            last_ns: 0,
            comm: "hog".to_owned(),
        }
    }

    fn of(attributes: &[Attr], name: &str) -> f64 {
        attributes
            .iter()
            .find(|attr| attr.name == name)
            .and_then(|attr| attr.value.as_f64())
            .unwrap_or_else(|| panic!("no `{name}` attribute"))
    }

    /// Every filter field the catalog offers must be on the events, or a
    /// predicate over it is false everywhere and silently empties the series.
    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        for stall in [Stall::Reclaim, Stall::Compaction] {
            let attributes = attributes(stall, &row(), 100_000);
            let source = source_of(stall);
            for field in &catalog
                .get(&source.parse().expect("a source id"))
                .unwrap_or_else(|| panic!("{source} is in the catalog"))
                .filter_fields
            {
                assert!(
                    attributes.iter().any(|attr| attr.name == *field),
                    "{source} offers a `{field}` filter and no event carries one"
                );
            }
        }
    }

    /// The value is the bucket's low bound and the total is exact. Six episodes
    /// in `[1 ms, 2 ms)` are at least six milliseconds, and the low bound alone
    /// would say one.
    #[test]
    fn the_value_is_the_buckets_low_bound_and_the_total_is_exact() {
        let attributes = attributes(Stall::Reclaim, &row(), 1);
        assert_eq!(of(&attributes, "stall_ns"), (1u64 << 20) as f64);
        assert_eq!(of(&attributes, "count"), 6.0);
        assert_eq!(of(&attributes, "total_ns"), 9_000_000.0);
        assert_eq!(of(&attributes, crate::emit::WEIGHT_ATTR), 6.0);
        assert_eq!(of(&attributes, "pages_reclaimed"), 512.0);
    }

    /// A row's enclosing window runs from its stamp to its last episode's end,
    /// and to the drain's close when the kernel recorded no end -- which still
    /// encloses every episode, only less tightly.
    #[test]
    fn a_drained_rows_window_ends_at_its_last_episode_or_else_at_the_drain() {
        let mut row = row();
        row.first_ns = 1_000;
        row.last_ns = 4_000;
        let ts = stamp(row.first_ns, 10_000);
        assert_eq!(enclosing(&row, ts, 10_000), 3_000);
        row.last_ns = 0;
        assert_eq!(enclosing(&row, ts, 10_000), 9_000);
    }

    /// Compaction's entry declares no key, and the events must carry none: the
    /// work is done for a zone, and a key would invite a reader to add up one
    /// program's share of a machine-wide job. The thread is still an attribute.
    #[test]
    fn compaction_carries_no_key_and_reclaim_carries_the_thread() {
        assert!(keys_of(Stall::Compaction, &row()).is_empty());
        assert_eq!(keys_of(Stall::Reclaim, &row()), vec![KeyValue::task(31)]);

        let compaction = attributes(Stall::Compaction, &row(), 1);
        assert!(compaction.iter().any(|attr| attr.name == "comm"));
        assert!(
            !compaction.iter().any(|attr| attr.name == "pages_reclaimed"),
            "compaction's end tracepoint carries a status, not a quantity"
        );
    }

    /// A sampled stall is a stall whose beginning or end was not seen, and a
    /// duration built from one of the two is not a short duration: it is not a
    /// duration.
    #[test]
    fn a_sampled_mode_is_refused_with_the_reason() {
        let prepared = Box::new(Prepare {
            stall: Stall::Reclaim,
            mode: Mode::Sample {
                freq: "997hz".parse().expect("a frequency"),
            },
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::System) else {
            panic!("a sampled mode must be refused");
        };
        assert!(error.contains("mem.reclaim"), "{error}");
        assert!(error.contains("gap between two events"), "{error}");
    }

    /// Compaction is machine-wide, and a target that names less than the
    /// machine is refused by name rather than answered with the machine's
    /// number under that target's label.
    #[test]
    fn a_compaction_request_for_one_process_is_refused_by_name() {
        let prepared = Box::new(Prepare {
            stall: Stall::Compaction,
            mode: Mode::Trace,
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::Pid(vec![1])) else {
            panic!("compaction is not accounted per process");
        };
        assert!(error.contains("whole memory zone"), "{error}");
        assert!(error.contains("--system"), "{error}");
    }

    /// A process target filters in the program, which is what the resolver
    /// credits the backend with.
    #[test]
    fn a_process_target_is_carried_into_the_program_as_a_filter() {
        let prepared = Box::new(Prepare {
            stall: Stall::Reclaim,
            mode: Mode::Trace,
            filter: None,
        });
        let collector = prepared
            .attach(&Target::Pid(vec![4242]))
            .expect("a process is a target reclaim is accounted per");
        assert_eq!(collector.id().as_str(), "mem.reclaim/bpf:raw_tp");
    }

    /// An episode that ended without a beginning happened and has no duration,
    /// and a run that saw one has to count it rather than drop it silently.
    #[test]
    fn an_episode_with_no_duration_is_counted_as_a_loss_with_its_reason() {
        let mut collector = StallCollector::new(Stall::Reclaim, Mode::Trace, None, None, None);
        collector.program = None;
        // What `absorb_drops` would have read off the programs.
        collector.losses.dropped += 3;
        collector
            .losses
            .note("a direct reclaim ended that this run never saw begin");
        assert_eq!(collector.losses().dropped, 3);
        assert!(collector
            .losses()
            .reasons
            .keys()
            .any(|reason| reason.contains("never saw begin")));
    }

    /// A thread still inside when the run ended is a fact about the window, not
    /// a loss, and the two must not be reported as the same thing.
    #[test]
    fn a_thread_still_inside_at_the_end_is_a_note_and_not_a_loss() {
        let mut collector = StallCollector::new(Stall::Reclaim, Mode::Trace, None, None, None);
        collector.still_inside = 1;
        let notes = collector.notes().join(" ");
        assert!(notes.contains("1 thread was still inside"), "{notes}");
        assert_eq!(collector.losses().dropped, 0);
    }

    /// A `count` run's row is summed over the whole run and stamped at its
    /// close, so it has no window that starts at its stamp, and it carries no
    /// `enclosing_ns`. A rule that names the attribute on it is then refused by
    /// the join as a missing span (`join`'s test of `enclosing`), rather than
    /// given a window of zero width.
    #[test]
    fn a_counted_row_carries_no_enclosing_window() {
        let mut collector = StallCollector::new(Stall::Reclaim, Mode::Count, None, None, None);
        let mut held = row();
        held.first_ns = 1_000;
        held.last_ns = 9_000;
        collector
            .counted
            .insert((held.tid, held.cgroup_id, held.bucket, held.scope), held);
        let events = collector.stop();
        assert_eq!(events.len(), 1, "the one counted row: {events:?}");
        assert!(
            events[0]
                .attrs
                .iter()
                .all(|attr| attr.name != "enclosing_ns"),
            "a counted row has no enclosing window: {:?}",
            events[0].attrs
        );
        assert!(
            events[0].attrs.iter().any(|attr| attr.name == "count"),
            "and it is still a row: {:?}",
            events[0].attrs
        );
    }

    /// An aggregated row is stamped where it happened, not where it was read.
    ///
    /// A bucketed source emits one event per row per window, and a join that
    /// *opens* on it opened at the close before this: everything in the same
    /// window as the first episode fell outside the state it was meant to
    /// open. The seventh wave's machine crossing worked around it by sleeping
    /// 300 ms between writing and reading, and said so in the session.
    #[test]
    fn a_row_is_stamped_at_its_first_episode_rather_than_at_the_drain() {
        let closed = 1_000_000_000;
        let first = closed - 80_000_000;
        assert_eq!(stamp(first, closed), first);
        assert!(
            stamp(first, closed) < closed,
            "the whole point is that it is earlier than the drain"
        );
    }

    /// And a row whose first episode the kernel did not record falls back to
    /// the drain, rather than to zero -- which would place it before the run.
    #[test]
    fn a_row_with_no_recorded_beginning_falls_back_to_the_drain() {
        assert_eq!(stamp(0, 1_000_000_000), 1_000_000_000);
    }
}
