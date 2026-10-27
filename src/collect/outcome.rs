//! What a run hands back per request: its collection, and the account of
//! what it lost, watched and filtered.

use super::*;

/// What a run produced, and what it lost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    pub events: Vec<Event>,
    /// The attribute holding the measured value, named by the collector rather
    /// than guessed from the unit: a source may carry several numbers.
    pub value_attr: String,
    pub losses: Losses,
    /// Wall time actually observed, which is not the requested duration when a
    /// launched program exits early or a target disappears.
    pub observed_ns: u64,
    /// Why the run stopped.
    pub stopped_because: String,
    /// The frames behind each stack identity this run saw, root first.
    ///
    /// Carried here because a stack is interned by the collector that captured
    /// it and rendered by the layer that aggregates it, and neither should have
    /// to know about the other's types. The events name ids; this says what
    /// they were.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stacks: BTreeMap<u64, Vec<String>>,
    /// What this collector had to give up, discovered while running rather than
    /// while resolving: a counter the machine would only open for user mode, a
    /// group the hardware had to time-share. The resolver cannot know these
    /// until the event is opened, and they change what the number means, so
    /// they travel with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// What the request's filter removed, when it had to run here. `None` when
    /// the request carried no filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filtered: Option<Filtered>,
    /// The backends that produced these events, in the order they served.
    ///
    /// One entry, the resolver's choice, unless a budget moved the collection
    /// onto a cheaper backend part way through; then the series is two
    /// mechanisms end to end and `notes` says when it changed. Recorded here
    /// because the resolved request is written down before the run and cannot
    /// know.
    #[serde(default)]
    pub served_by: Vec<String>,
    /// What this collection cost, measured rather than declared.
    #[serde(default)]
    pub overhead: Overhead,
    /// When the run's launched program ran, from the gate to the reap, on
    /// every collection of a run that launched one -- including those of
    /// machine targets beside it, since they share its window and it is the
    /// whole they would be read against. `None` when nothing was launched.
    ///
    /// Not `observed_ns`, and deliberately beside it: the window opens once
    /// every collector is attached and closes after the last drain, so it runs
    /// past the program by the length of a drain, and a residue smaller than
    /// that difference cannot be told from zero against it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime: Option<Lifetime>,
    /// When this collection was watching, which is its own and not the run's
    /// when its request carried a `window`.
    ///
    /// `observed_ns` is the length of this span, so a rate over a source
    /// attached for two seconds of a ten-second run is a rate over two.
    #[serde(default)]
    pub window: Watched,
    /// Whether that window closed at a detach rather than with the run.
    /// Every window closes where its own collector stopped, and the run stops
    /// them in turn, so a close earlier than another's is not by itself a
    /// detach: a join across a source that ran to the end treats it as
    /// watching to the end, and one across a detached source ends its states
    /// at the detach.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
    /// The part of `losses` each ring lost, for a collector that reads one
    /// ring per key; empty when the losses belong to the whole alike.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_ring: Vec<RingLoss>,
    /// What the collector states about the run that is not a concession:
    /// [`Collector::facts`]. `notes` is what it had to give up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<String>,
    /// How the collector stamps a row it read from a table the kernel filled
    /// between drains: [`Collector::row_stamp`]. `None` when it does not say,
    /// which is not a claim that each row is stamped at its own event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_stamp: Option<RowStamp>,
    /// What the kernel dropped, placed in time: one entry per read of a
    /// drained collector that found its drop count had grown, with that
    /// read's time. Their sum is `losses.dropped`, so a run that lost a burst
    /// in its first second reads apart from one that lost steadily. Empty for
    /// a collector that is not drained, or that dropped nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drains: Vec<DrainDrop>,
    /// A collector failed, or no request attached before the run ended.
    /// Includes [`Collector::failure`] when collection broke the program.
    /// The run's outcome reads it and exits non-zero with the sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// What the collector counted about the run as numbers a program can
    /// read: [`Collector::fields`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: crate::emit::Fields,
    /// What the stacks' labels cannot say: [`Collector::provenance`].
    #[serde(default, skip_serializing_if = "crate::stack::Provenance::is_empty")]
    pub provenance: crate::stack::Provenance,
}

/// How a drained collector stamps a row it read from a table the kernel
/// filled between two drains -- a row that stands for many events, not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowStamp {
    /// At the close of the drain that read it: the row covers the
    /// `window_ns` before its timestamp.
    DrainClose,
    /// Where the row's first event began, and at the close of the drain that
    /// read it when the kernel recorded no beginning: the row covers from its
    /// timestamp to that close.
    FirstEvent,
}

/// What one read of a drained collector found the kernel had dropped since
/// the read before it, and when that read was taken, on `CLOCK_MONOTONIC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainDrop {
    pub ts_ns: u64,
    pub dropped: u64,
}

/// What one ring of a collection lost: the key the ring belongs to -- the
/// thread a per-thread profile opened it for -- and its share of the
/// collection's losses.
///
/// A series keyed by that key is short by this and by nothing else, which is
/// the point: a profile keyed by `task` whose one thread overflowed its ring
/// used to mark every thread's series with the run's whole drop count, and a
/// reader could not tell which thread was missing samples (`docs/dev/STATE.md`, the
/// note on `cross_compute`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingLoss {
    pub key: crate::event::KeyValue,
    pub losses: Losses,
}

/// The span one collection watched, on `CLOCK_MONOTONIC`.
///
/// Every collection carries one. For a request without a window it is the
/// run's own, from the moment the accounting opened to the moment the last
/// collector was stopped; for a request attached or detached mid-run it is the
/// part of the run it was attached for, and an event of another source outside
/// it was outside *this source's* view rather than outside everything the run
/// saw -- which is what a join across it needs to know.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watched {
    /// When the run itself opened: what the request's window is relative to.
    pub run_opened_ns: u64,
    /// When this collector was attached and first read. Equal to the run's
    /// open for a request attached with the rest.
    pub opened_ns: u64,
    /// When it was stopped: its detach, or the end of the run.
    pub closed_ns: u64,
    /// Whether it was ever attached. A request whose window opened after the
    /// run had ended never was, and its empty series is not a measurement of
    /// nothing.
    pub attached: bool,
    /// Wall time spent attaching and starting it mid-run, while the target
    /// ran. Absent for a collector attached before the launch gate opened,
    /// whose attach is this tool's startup rather than part of the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_ns: Option<u64>,
}

/// How a collection's `stopped_because` says it was detached `at_s` seconds
/// into the run, whatever detached it: its window, the budget or the
/// escalation. One spelling, so [`stopped_at_a_detach`] can read it back.
pub(super) fn detached_at(at_s: f64) -> String {
    format!("detached {at_s:.3} s into the run")
}

/// Whether a stored `stopped_because` says its collection was detached before
/// the run ended, rather than stopped with it: what a join over a stored run
/// needs to end the states still open at its close where the agent tier did.
pub fn stopped_at_a_detach(stopped_because: &str) -> bool {
    let Some((_, after)) = stopped_because.split_once("detached ") else {
        return false;
    };
    after
        .split_once(" s into the run")
        .is_some_and(|(seconds, _)| seconds.parse::<f64>().is_ok())
}

impl Watched {
    /// Whether an event stamped at `ts_ns` fell inside this collection's view.
    pub fn contains(&self, ts_ns: u64) -> bool {
        self.attached && self.opened_ns <= ts_ns && ts_ns <= self.closed_ns
    }

    /// How long this collection watched.
    pub fn span_ns(&self) -> u64 {
        self.closed_ns.saturating_sub(self.opened_ns)
    }
}

/// What a filter the resolver could not push down did to a collection.
///
/// Events a filter removed are not losses: they were asked to go. But they were
/// collected, paid for, and are absent from every number that follows, so the
/// count belongs beside the results for the same reason the loss count does --
/// a series that quietly holds three of forty events looks exactly like a
/// series of three.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filtered {
    /// The predicate, as the request wrote it.
    pub predicate: String,
    pub kept: u64,
    pub dropped: u64,
    /// Fields the predicate names that no event carried. A predicate over a
    /// field nothing produces is false everywhere, which empties the series and
    /// looks exactly like a workload that did nothing -- so the reason is
    /// recorded rather than left to be guessed at.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unseen_fields: Vec<String>,
}

/// Everything that did not make it into `events`.
///
/// Zero everywhere is the only reading that means "these numbers are complete".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Losses {
    /// Events the kernel had nowhere to put. Reported by the ring itself.
    pub dropped: u64,
    /// Reads that returned nothing.
    pub failed_reads: u64,
    /// Ticks that started late enough to shift the series.
    pub late_ticks: u64,
    /// Records the reader did not know how to decode. Counted rather than
    /// skipped, because a record shape nobody handles is a gap in the data.
    pub undecoded: u64,
    /// Periods in which the kernel throttled the event because it was costing
    /// too much.
    ///
    /// Deliberately not folded into `dropped`. A drop count is a number of
    /// events the ring reported losing; a throttle is the kernel declining to
    /// deliver anything for a while, and how many events happened inside that
    /// window is not knowable from userspace. Counting one throttle as one
    /// drop claims a precision that does not exist, and is the reason a
    /// throttled run of `sched.switches` under-reported its own loss rather
    /// than saying it could not tell.
    ///
    /// `kernel.perf_event_max_sample_rate` is what decides this, and a run
    /// with a non-zero count here is not comparable to one without.
    pub throttled: u64,
    /// Events a recording read and evicted for its retention bound, before
    /// any document held them: observed, paid for, and in no document.
    ///
    /// Only a recording evicts, and only its documents carry this: each one
    /// counts what was evicted since the document before it. It does not make
    /// the document's distribution a lower bound -- every event of the
    /// document's window is in it -- but it is what the run saw and let go,
    /// and a count nobody is shown is a silent loss.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub evicted: u64,
    /// The reasons behind the counts above, with how often each occurred.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reasons: BTreeMap<String, u64>,
    /// By how much the late ticks were late, when any were.
    ///
    /// Separate from the reason because they are two different things. The
    /// reason is a *kind* -- "a round of reads outran its tick" -- and the map
    /// above counts kinds; the duration is a magnitude, and putting it in the
    /// key made every occurrence its own kind with a count of one. A run that
    /// was late two hundred times then printed two hundred lines, aggregated
    /// none of them, and carried two hundred keys into every structured output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrun: Option<Overrun>,
}

/// How far a poll fell behind the cadence it was asked for.
///
/// Three numbers rather than one: the worst is what tells a reader whether the
/// series is usable at all, the total is what says whether being late was the
/// exception or the shape of the whole run, and the tick is what both are
/// against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overrun {
    /// The interval the reads were supposed to keep.
    pub tick_ns: u64,
    /// The longest single round of reads.
    pub worst_ns: u64,
    /// Every late round added up, so a mean can be taken against the count.
    pub total_ns: u64,
    /// The readings that fell due while a late round held the run and were
    /// never taken: the schedule is re-based on the late read, so a round
    /// that held a 250 ms poll for three seconds is one late tick and eleven
    /// readings that never ran. A machine-wide `omn top` starving its
    /// pollers showed as one late tick a poller, and these were counted
    /// nowhere.
    #[serde(default)]
    pub skipped: u64,
}

impl Losses {
    pub fn is_clean(&self) -> bool {
        self.dropped == 0
            && self.failed_reads == 0
            && self.late_ticks == 0
            && self.undecoded == 0
            && self.throttled == 0
            && self.evicted == 0
    }

    /// What was lost since `earlier`, a reading of the same account taken
    /// before: the counts and the reasons by difference. The overrun's
    /// magnitudes cannot be taken apart, so the later ones are kept whole,
    /// which never understates the worst round.
    pub fn since(&self, earlier: &Losses) -> Losses {
        let mut reasons = BTreeMap::new();
        for (reason, count) in &self.reasons {
            let before = earlier.reasons.get(reason).copied().unwrap_or(0);
            if *count > before {
                reasons.insert(reason.clone(), count - before);
            }
        }
        let late_ticks = self.late_ticks.saturating_sub(earlier.late_ticks);
        Losses {
            dropped: self.dropped.saturating_sub(earlier.dropped),
            failed_reads: self.failed_reads.saturating_sub(earlier.failed_reads),
            late_ticks,
            undecoded: self.undecoded.saturating_sub(earlier.undecoded),
            throttled: self.throttled.saturating_sub(earlier.throttled),
            evicted: self.evicted.saturating_sub(earlier.evicted),
            reasons,
            overrun: self.overrun.filter(|_| late_ticks > 0),
        }
    }

    pub fn note(&mut self, reason: impl Into<String>) {
        self.note_times(reason, 1);
    }

    /// Records `reason` as the cause of `times` losses: the call beside a
    /// counter's `+= n`, so that the reasons count what the counters count
    /// and the summary's `{n}x {reason}` reads as that many lost, not as the
    /// number of drains that lost some.
    pub fn note_times(&mut self, reason: impl Into<String>, times: u64) {
        *self.reasons.entry(reason.into()).or_insert(0) += times;
    }

    /// Records one round of reads that outran the interval it was given.
    ///
    /// The count and the magnitudes are kept apart here and joined again in
    /// [`Losses::summarize`], once, when the run is over. Formatting the
    /// duration into the reason at this point is what made a late run print one
    /// line per late tick -- see [`Losses::overrun`].
    pub fn late_tick(&mut self, spent_ns: u64, tick_ns: u64) {
        self.late_ticks += 1;
        let overrun = self.overrun.get_or_insert(Overrun {
            tick_ns,
            ..Overrun::default()
        });
        overrun.tick_ns = tick_ns;
        overrun.worst_ns = overrun.worst_ns.max(spent_ns);
        overrun.total_ns += spent_ns;
        // The readings due at one tick, two ticks, ... after the late one's
        // own due time and before it finished.
        overrun.skipped += spent_ns.saturating_sub(1) / tick_ns.max(1);
    }

    /// Turns the accumulated magnitudes into the one reason a reader sees.
    ///
    /// Called once, after everything has been absorbed, so that the reason is a
    /// single key carrying the count it explains -- `77x a round of reads
    /// outran the 1ms tick` -- with the worst and the average beside it rather
    /// than in place of it.
    pub fn summarize(&mut self) {
        let Some(overrun) = self.overrun else {
            return;
        };
        if self.late_ticks == 0 {
            return;
        }
        let mean_ns = overrun.total_ns / self.late_ticks;
        let mut reason = format!(
            "a round of reads outran the {} tick: worst {}, {} on average",
            span_ns(overrun.tick_ns),
            span_ns(overrun.worst_ns),
            span_ns(mean_ns),
        );
        if overrun.skipped > 0 {
            reason.push_str(&format!(
                "; {} more reading{} fell due meanwhile and never ran",
                overrun.skipped,
                if overrun.skipped == 1 { "" } else { "s" },
            ));
        }
        self.note_times(reason, self.late_ticks);
    }

    /// Records one throttled period, with what the reader needs to know about
    /// it. The count is of periods, not of events: the events are the part
    /// nobody can count.
    pub fn throttle(&mut self, reason: impl Into<String>) {
        self.throttled += 1;
        self.note(reason);
    }

    pub(super) fn absorb(&mut self, other: Losses) {
        self.dropped += other.dropped;
        self.failed_reads += other.failed_reads;
        self.late_ticks += other.late_ticks;
        self.undecoded += other.undecoded;
        self.throttled += other.throttled;
        self.evicted += other.evicted;
        self.overrun = match (self.overrun, other.overrun) {
            (Some(mine), Some(theirs)) => Some(Overrun {
                // The two halves of one collection are driven at one cadence,
                // so the ticks agree; the larger is kept rather than averaged
                // so that the reason never understates what it is against.
                tick_ns: mine.tick_ns.max(theirs.tick_ns),
                worst_ns: mine.worst_ns.max(theirs.worst_ns),
                total_ns: mine.total_ns + theirs.total_ns,
                skipped: mine.skipped + theirs.skipped,
            }),
            (mine, theirs) => mine.or(theirs),
        };
        for (reason, count) in other.reasons {
            *self.reasons.entry(reason).or_insert(0) += count;
        }
    }
}

/// Where a budget moved a collection, and the span in which the backend it
/// left and the one it moved onto were both observing the target.
pub(super) struct Moved {
    pub(super) onto: String,
    /// The mode it was moved into, when the move kept the backend and changed
    /// the mode (`trace` -> `sample`): then the backend alone does not say
    /// what changed.
    pub(super) into_mode: Option<String>,
    pub(super) overlap_from_ns: u64,
    pub(super) overlap_to_ns: u64,
}

/// Nanoseconds a reader can take in at a glance, for the loss reasons.
///
/// Deliberately not the raw count. "a round of reads took 104754353ns" is a
/// number nobody reads as a tenth of a second, and the reason line exists to be
/// read rather than parsed -- the structured output carries the nanoseconds.
pub(super) fn span_ns(ns: u64) -> String {
    match ns {
        ns if ns < 1_000 => format!("{ns}ns"),
        ns if ns < 1_000_000 => format!("{:.1}us", ns as f64 / 1e3),
        ns if ns < 1_000_000_000 => format!("{:.1}ms", ns as f64 / 1e6),
        ns => format!("{:.2}s", ns as f64 / 1e9),
    }
}
