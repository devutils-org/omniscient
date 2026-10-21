//! The rounds: when each collector is read, and what one read hands over.

use super::*;

/// How one collector is driven: how often it is read, and whether one read is
/// the whole of it.
///
/// One of these per request, not one per run. A session used to take a single
/// interval -- the fastest collector's tick, or, where none declared one, the
/// mode of whichever request happened to be written first -- and apply it to
/// everything. So `poll@1s` beside `poll@10ms` was read every ten
/// milliseconds, `poll@10ms` beside a `count` was read once a second, and a
/// `snapshot` beside either became a series: request order decided another
/// request's sampling rate, which is the one thing a declarative session must
/// not do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Pace {
    pub(super) interval: StdDuration,
    /// A single read, after which this collector is done: there is no series
    /// and no second round. When every collector says so, the run itself is
    /// over after one round.
    pub(super) snapshot: bool,
}

impl Pace {
    /// The pace of a request whose collector is not attached yet, replaced
    /// by its own the moment it is.
    pub(super) const UNSET: Pace = Pace {
        interval: StdDuration::ZERO,
        snapshot: false,
    };
}

/// How one request is driven, from the mode it asked for and what its collector
/// needs.
///
/// A streaming collector's tick is not a preference: a ring drained less often
/// than it fills loses events, and nothing a request says makes that
/// acceptable, so it wins. A collector that declares none is read at the
/// cadence the request named -- and a mode that names no cadence at all, which
/// is `count`, `trace` and `sample` on a collector that does not stream, falls
/// back to the default poll interval as it always did.
pub(super) fn pace_of(collector: &dyn Collector, mode: Mode) -> Pace {
    if mode == Mode::Snapshot {
        return Pace {
            interval: StdDuration::ZERO,
            snapshot: true,
        };
    }
    let interval = collector.tick().unwrap_or(match mode {
        Mode::Poll { interval } => StdDuration::from_nanos(interval.as_nanos()),
        _ => StdDuration::from_nanos(Mode::DEFAULT_POLL_INTERVAL.as_nanos()),
    });
    Pace {
        interval,
        snapshot: false,
    }
}

/// What a run's collectors produced, per request, and why the run ended.
pub(super) struct Driven {
    pub(super) events: Vec<Vec<Event>>,
    pub(super) losses: Vec<Losses>,
    /// The span each request was attached for.
    pub(super) watched: Vec<Watched>,
    /// Why a request stopped before the run did, when it did: its window
    /// closed, or it was never attached.
    pub(super) closed_because: Vec<Option<String>>,
    pub(super) stopped_because: String,
    /// What the escalation said about each request once the run was over.
    pub(super) escalated: Vec<Vec<String>>,
    pub(super) drains: Vec<Drains>,
}

/// Why one request stopped being read before the run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Finished {
    /// It was a single read, and that read is taken.
    Snapshot,
    /// Its target went away before the workload did.
    Gone,
    /// Its window closed, or never opened on anything.
    Window,
}

/// Drives a set of collectors over one run, and closes it.
///
/// The order inside a round is the whole of the edge convention: **read
/// first, then decide**. A round reads every collector that is due, and only
/// then asks whether the run is over, so the last round of reads happens at the
/// moment the run ends rather than one interval before it. That is what makes a
/// polled counter's series complete without any collector reading again in
/// `stop`, and `the_runner_closes_the_window` measures how little time is
/// left over for such a read to cover.
///
/// *Due* is per collector, from its own [`Pace`]. A collector whose interval is
/// longer than another's is simply not read on the rounds in between, which is
/// what makes a session of several cadences mean what it says; the loop wakes
/// at the earliest of them, so the run is no less responsive to its stop
/// condition than when one interval governed everything.
///
/// A request with a window is attached and detached here, by its [`Plan`],
/// while the rest keep running. The same convention holds at both of its
/// edges. A window opens at the top of a round, so the collector's first read
/// is the round it was attached in -- a counter's baseline is taken at its
/// attach, not one interval later. And it closes at the bottom: a collector
/// whose window is closing is due at that moment whatever its pace, it is read,
/// and only then stopped and dropped, so its last interval is measured to its
/// detach just as the run's last interval is measured to the run's end.
///
/// A request held for the escalation has no time in its plan. The escalation
/// is asked once per round, after the reads and after the stop condition --
/// a run that is ending attaches nothing -- and what it answers is applied
/// detaches first: the layer it leaves is released before the one it names is
/// loaded, so the run never pays for two detailed layers at once. A collector
/// it attaches is due at once and read on the next round, which follows
/// without a sleep.
///
/// `ended` is consulted once per round and returns why the run should stop.
/// It is a closure rather than a launched child and a deadline so that the
/// convention can be tested against a stop condition a test controls.
///
/// `about_workload` says, per collector, whether its target going away ends the
/// run. A core or an interface beside a program is read for as long as the
/// program runs: one that disappears first stops being read, and its collection
/// says from when, but the program is still the thing being measured. `wake`
/// is a descriptor that becomes readable when the run should look again at
/// once -- the launched program's pidfd -- so that a program exiting between
/// two ticks ends the run at its exit rather than at the next tick.
#[allow(clippy::too_many_arguments)]
pub(super) fn drive(
    slots: &mut [Slot],
    paces: &mut [Pace],
    plans: &[Plan],
    about_workload: &[bool],
    wake: Option<RawFd>,
    notice_exit: bool,
    spending: &mut Spending<'_>,
    mut escalation: Option<&mut dyn Escalate>,
    mut recording: Option<&mut Recording<'_, '_>>,
    mut ended: impl FnMut() -> Option<String>,
) -> Driven {
    let count = slots.len();
    let opened_ns = spending.opened_ns;
    let mut ledger = Ledger {
        events: vec![Vec::new(); count],
        losses: vec![Losses::default(); count],
        sequences: vec![0u64; count],
        // Everything is due at once on the first round: a series starts with
        // a reading, and a counter's first read is the baseline the rest are
        // against.
        due_ns: vec![monotonic_ns(); count],
        finished: vec![None; count],
        watched: slots
            .iter()
            .map(|slot| Watched {
                run_opened_ns: opened_ns,
                opened_ns,
                closed_ns: opened_ns,
                attached: matches!(slot, Slot::Attached(_)),
                attach_ns: None,
            })
            .collect(),
        closed_because: vec![None; count],
        // Only a run serving several collectors has to share its cpu time
        // out between them, and only then are the two clock reads a round
        // worth taking: with one collector the share is the whole.
        attributing: count > 1,
        drains: vec![Drains::default(); count],
    };
    // A held request the runner could not hold -- a collector given at exec
    // -- is never attached, and has said why already.
    for (index, slot) in slots.iter().enumerate() {
        if matches!(slot, Slot::Missed) {
            ledger.finished[index] = Some(Finished::Window);
            if let Some(reason) = &plans[index].unheld {
                ledger.closed_because[index] = Some(format!("never attached: {reason}"));
            }
        }
    }
    // When a collector's window closes, which makes it due then whatever its
    // pace says.
    let detach_at = |index: usize| plans[index].detach_ns.map(|ns| opened_ns + ns);

    let stopped_because = loop {
        let round_started_ns = monotonic_ns();

        // Windows that open now. The account is opened before the attach, so
        // what attaching costs while the target runs is this request's.
        for index in 0..count {
            if plans[index].held
                || !matches!(slots[index], Slot::Waiting(_))
                || round_started_ns < opened_ns + plans[index].attach_ns
            {
                continue;
            }
            ledger.attach(index, slots, paces, spending, None);
        }

        let mut gone = None;
        // Where each buffer stood before this round, and which requests read
        // in it: what a recording's trigger reads is the round's own events.
        let before: Vec<usize> = if recording.is_some() {
            ledger.events.iter().map(Vec::len).collect()
        } else {
            Vec::new()
        };
        let mut read_now = vec![false; count];
        // When each read of this round ended: a round can outlast its run.
        let mut read_at_ns = vec![0u64; count];
        // The round's drained row maps retired together and waited for once,
        // rather than each drain waiting for a grace period of its own. A
        // failure here leaves each drain to wait as before.
        let due_now = |index: usize| {
            matches!(slots[index], Slot::Attached(_))
                && ledger.finished[index].is_none()
                && round_started_ns
                    >= detach_at(index)
                        .map_or(ledger.due_ns[index], |at| ledger.due_ns[index].min(at))
        };
        let _ = crate::bpf::retire_owned_by(due_now);
        for (index, slot) in slots.iter_mut().enumerate() {
            let Slot::Attached(collector) = slot else {
                continue;
            };
            let due =
                detach_at(index).map_or(ledger.due_ns[index], |at| ledger.due_ns[index].min(at));
            if ledger.finished[index].is_some() || round_started_ns < due {
                continue;
            }
            read_now[index] = true;
            if let Some(reason) = ledger.read(
                index,
                collector.as_mut(),
                paces[index],
                about_workload[index],
                round_started_ns,
                spending,
            ) {
                gone = Some(reason);
            }
            read_at_ns[index] = monotonic_ns();
        }
        if let Some(reason) = gone {
            break reason;
        }

        // Windows that close now, after the round's reads: the read that was
        // due at the detach has been taken, and what the collector still
        // holds is drained into its own collection before it is dropped.
        for index in 0..count {
            let closing = detach_at(index).is_some_and(|at| round_started_ns >= at);
            if !closing || !matches!(slots[index], Slot::Attached(_)) {
                continue;
            }
            ledger.detach(index, slots, spending, |at| {
                format!("its window closed: {}, as asked", detached_at(at))
            });
        }

        // A held request still waiting keeps nothing going: the escalation
        // decides from the requests that run, and once they are all done
        // there is nothing left for it to decide from.
        let waiting = slots
            .iter()
            .zip(plans)
            .any(|(slot, plan)| !plan.held && matches!(slot, Slot::Waiting(_)));
        let settled = |index: usize| {
            ledger.finished[index].is_some()
                || (plans[index].held && matches!(slots[index], Slot::Waiting(_)))
        };
        if !waiting && (0..count).all(settled) && ledger.finished.iter().any(Option::is_some) {
            let done: Vec<Finished> = ledger.finished.iter().flatten().copied().collect();
            if done.iter().all(|kind| *kind == Finished::Gone) {
                break "every target of the run went away".to_owned();
            }
            if done.contains(&Finished::Snapshot) {
                break "snapshot: a single read".to_owned();
            }
            break "every request's window had closed".to_owned();
        }

        // A recording reads the round before the stop condition is asked, so
        // a trigger that fires on the run's last round still writes its
        // document, and a document due now is cut from this round's reads.
        if let Some(recording) = recording.as_deref_mut() {
            if let Some(reason) = recording.round(&read_now, &before, &mut ledger, slots, spending)
            {
                break reason;
            }
        }

        if let Some(reason) = ended() {
            // The runner closes the window, so the round that meets the stop
            // condition is the last round of reads, and every polled
            // collector takes its closing read in it -- also one whose next
            // read was not due yet. Since the run wakes at its duration, the
            // stop is met before the read that used to follow it: a counted
            // window read at the start and due again a second later ended
            // with its baseline alone, and reported nothing -- as a polled
            // counter beside a launched program always had at its exit. A
            // collector read
            // within the last millisecond has its closing read already, and a
            // second one would be a point of nothing beside it; one read
            // earlier than that, a baseline included, has not.
            let closing_ns = monotonic_ns();
            for (index, slot) in slots.iter_mut().enumerate() {
                let Slot::Attached(collector) = slot else {
                    continue;
                };
                let pace = paces[index];
                let interval_ns = pace.interval.as_nanos() as u64;
                let last_read_ns = ledger.due_ns[index].saturating_sub(interval_ns);
                // Only a collector the runner paces: one that keeps its own
                // tick -- a per-task counter, a ring -- takes its closing read
                // in `stop`, where it can wait for what an exited target's
                // children fold into it, and a read here would take the
                // interval from it and report nothing in its place.
                // Read in this round, and not long ago: a collector read
                // first in a round another collector's read made outlast the
                // run -- a machine-wide `sched.offcpu` loading modules for
                // seconds -- has its baseline alone otherwise, no event and
                // nothing late. Read again, it is late, and counted.
                let read_lately = read_now[index]
                    && closing_ns.saturating_sub(read_at_ns[index]) < CLOSING_READ_SLACK_NS;
                if read_lately
                    || (read_now[index] && interval_ns == 0)
                    || collector.tick().is_some()
                    || pace.snapshot
                    || interval_ns == 0
                    || ledger.finished[index].is_some()
                    || closing_ns.saturating_sub(last_read_ns) < CLOSING_READ_SLACK_NS
                {
                    continue;
                }
                // Its target going away now changes nothing: the run is over.
                let _ = ledger.read(
                    index,
                    collector.as_mut(),
                    pace,
                    about_workload[index],
                    closing_ns,
                    spending,
                );
            }
            break reason;
        }

        // Asked after the stop condition, so a run that is ending attaches
        // nothing; and before the budget, so a layer attached now is weighed
        // from its first round.
        if let Some(escalation) = escalation.as_deref_mut() {
            let said: Vec<Described<'_>> = slots
                .iter()
                .zip(&ledger.drains)
                .map(|(slot, drains)| {
                    let (value_attr, row_stamp) = match slot {
                        Slot::Attached(collector) => {
                            (collector.value_attr(), collector.row_stamp())
                        }
                        Slot::Detached(record) => (record.value_attr, record.row_stamp),
                        Slot::Waiting(_) | Slot::Missed => ("", None),
                    };
                    Described {
                        value_attr,
                        row_stamp,
                        drains: &drains.placed,
                    }
                })
                .collect();
            escalation.described(&said);
            drop(said);
            let standing = standing_of(slots, &ledger.closed_because);
            let switches = escalation.decide(&Progress {
                opened_ns,
                now_ns: monotonic_ns(),
                events: &ledger.events,
                standing: &standing,
                bound: &spending.bound,
            });
            ledger.switch(switches, slots, paces, spending);
        }

        // Asked after the stop condition, so a run that was going to end anyway
        // ends for the reason it was going to end for rather than being
        // condemned on its last tick.
        if let Some(reason) = spending.check(slots, &mut ledger.events, &mut ledger.losses) {
            break reason;
        }
        // A request the escalation attached and nothing cheaper serves is
        // offered back to it before the run is ended for it: detaching it
        // keeps the budget and the cheap answer.
        let mut over = None;
        for (index, reason) in std::mem::take(&mut spending.over) {
            let shed = escalation
                .as_deref_mut()
                .and_then(|escalation| escalation.shed(index, &reason));
            match shed {
                Some(why) => ledger.detach(index, slots, spending, |at| {
                    format!("{} to hold the budget: {why}", detached_at(at))
                }),
                None => {
                    over = Some(reason);
                    break;
                }
            }
        }
        if let Some(reason) = over {
            break reason;
        }

        // Sleep until the next collector falls due, or the next window opens
        // or closes. A collector already behind its own schedule puts that
        // moment in the past, and the run goes straight round again.
        let next_due_ns = (0..count)
            .filter_map(|index| match &slots[index] {
                Slot::Waiting(_) if plans[index].held => None,
                Slot::Waiting(_) => Some(opened_ns + plans[index].attach_ns),
                Slot::Attached(_) if ledger.finished[index].is_none() => Some(
                    detach_at(index)
                        .map_or(ledger.due_ns[index], |at| ledger.due_ns[index].min(at)),
                ),
                Slot::Attached(_) => detach_at(index),
                Slot::Detached(_) | Slot::Missed => None,
            })
            .min();
        // A document due to be cut is a moment to wake for too, and so is
        // the end of the run's duration: the run ends there, not at whichever
        // read would have come after it.
        let next_due_ns = [
            recording.as_deref().and_then(Recording::due_ns),
            spending.deadline_ns,
        ]
        .into_iter()
        .flatten()
        .fold(next_due_ns, |next, at| {
            Some(next.map_or(at, |next| next.min(at)))
        });
        if let Some(next_due_ns) = next_due_ns {
            let remaining_ns = next_due_ns.saturating_sub(monotonic_ns());
            if remaining_ns > 0 {
                if notice_exit && wake.is_none() {
                    sleep_noticing_exit(remaining_ns, &mut ended);
                } else {
                    sleep_or_wake(remaining_ns, wake);
                }
            }
        }
    };

    if let Some(recording) = recording {
        recording.end(&stopped_because, &mut ledger, slots, spending);
    }

    // What the escalation has to say about each request, now that it has
    // seen the whole run.
    let mut escalated = vec![Vec::new(); count];
    if let Some(escalation) = escalation {
        let standing = standing_of(slots, &ledger.closed_because);
        for (index, note) in escalation.close(&Progress {
            opened_ns,
            now_ns: monotonic_ns(),
            events: &ledger.events,
            standing: &standing,
            bound: &spending.bound,
        }) {
            if let Some(notes) = escalated.get_mut(index) {
                notes.push(note);
            }
        }
    }

    Driven {
        events: ledger.events,
        losses: ledger.losses,
        watched: ledger.watched,
        closed_because: ledger.closed_because,
        stopped_because,
        escalated,
        drains: ledger.drains,
    }
}

/// Where each request stands, as the escalation is shown it.
pub(super) fn standing_of(slots: &[Slot], closed_because: &[Option<String>]) -> Vec<Standing> {
    slots
        .iter()
        .zip(closed_because)
        .map(|(slot, because)| match slot {
            Slot::Waiting(_) => Standing::Waiting,
            Slot::Attached(_) => Standing::Attached,
            Slot::Detached(_) => Standing::Detached,
            Slot::Missed => Standing::Missed(
                because
                    .clone()
                    .unwrap_or_else(|| "it was never attached".to_owned()),
            ),
        })
        .collect()
}

/// Everything a run keeps per request while it drives them: what each has
/// produced and lost, when it is next due, and the span it was watched for.
pub(super) struct Ledger {
    pub(super) events: Vec<Vec<Event>>,
    pub(super) losses: Vec<Losses>,
    pub(super) sequences: Vec<u64>,
    pub(super) due_ns: Vec<u64>,
    pub(super) finished: Vec<Option<Finished>>,
    pub(super) watched: Vec<Watched>,
    pub(super) closed_because: Vec<Option<String>>,
    pub(super) attributing: bool,
    /// Each request's drops placed in time, and the total they sum to.
    pub(super) drains: Vec<Drains>,
}

impl Ledger {
    fn seconds_in(&self, ns: u64) -> f64 {
        ns.saturating_sub(self.watched.first().map_or(ns, |one| one.run_opened_ns)) as f64 / 1e9
    }

    /// Reads one collector that is due. Returns why the run must end, when
    /// the workload's own target went away.
    pub(super) fn read(
        &mut self,
        index: usize,
        collector: &mut dyn Collector,
        pace: Pace,
        about_workload: bool,
        round_started_ns: u64,
        spending: &mut Spending<'_>,
    ) -> Option<String> {
        let entered_ns = if self.attributing { monotonic_ns() } else { 0 };
        match collector.sample(self.sequences[index]) {
            Ok(mut sampled) => {
                into_our_ids(
                    spending.serving.get(index),
                    &mut sampled,
                    &mut self.losses[index],
                );
                self.sequences[index] += sampled.len() as u64;
                self.events[index].append(&mut sampled);
                // A drained collector's count is read at each drain, which is
                // what places its drops in time; a polled one's is read once,
                // at its stop, for what a read of its losses would cost.
                if collector.tick().is_some() {
                    let dropped = self.losses[index].dropped + collector.losses().dropped;
                    self.drains[index].grew_to(dropped, monotonic_ns());
                }
            }
            Err(SampleError::TargetGone(reason)) if about_workload => return Some(reason),
            Err(SampleError::TargetGone(reason)) => {
                let at = self.seconds_in(round_started_ns);
                self.losses[index].note(format!(
                    "stopped reading {at:.3} s into the run, before the workload ended: {reason}"
                ));
                self.finished[index] = Some(Finished::Gone);
                return None;
            }
            Err(SampleError::Transient(reason)) => {
                self.losses[index].failed_reads += 1;
                self.losses[index].note(reason);
            }
        }
        if self.attributing {
            spending.inside_ns[index] += monotonic_ns().saturating_sub(entered_ns);
        }

        if pace.snapshot {
            self.finished[index] = Some(Finished::Snapshot);
            return None;
        }
        // When the next reading was due, and whether this one got in the
        // way of it. A round that overran is counted as a late tick -- at
        // that point the series no longer has the spacing it claims -- and
        // the schedule is re-based on now, so that being late once does not
        // leave every reading after it owing time it can never make up.
        let interval_ns = pace.interval.as_nanos() as u64;
        let finished_ns = monotonic_ns();
        let took_ns = finished_ns.saturating_sub(self.due_ns[index]);
        // An interval of zero is "as fast as this can be read", which is
        // not a cadence and cannot be fallen behind. Counting every round
        // of such a run as late is how a snapshot served as a poll came to
        // report 158 losses that had not happened.
        if interval_ns > 0 && took_ns > interval_ns {
            self.losses[index].late_tick(took_ns, interval_ns);
            self.due_ns[index] = finished_ns;
        } else {
            self.due_ns[index] += interval_ns;
        }
        None
    }

    /// Attaches a request that was waiting, while the target runs: by its
    /// window when `escalated` is `None`, by the escalation otherwise, whose
    /// reason is then the first thing its collection says.
    fn attach(
        &mut self,
        index: usize,
        slots: &mut [Slot],
        paces: &mut [Pace],
        spending: &mut Spending<'_>,
        escalated: Option<&str>,
    ) {
        let Slot::Waiting(prepared) = std::mem::replace(&mut slots[index], Slot::Missed) else {
            unreachable!("only a waiting request is attached");
        };
        spending.attaching(index);
        let entered_ns = monotonic_ns();
        let backend = spending.serving[index].backend.id.clone();
        if loads_programs(&backend) {
            crate::bpf::pidns::remember();
        }
        let before = loads_programs(&backend).then(programs_held);
        let account = prepared.inside();
        Target::narrowed_to_one();
        let mut attached = match blind_to_running_threads(prepared.as_ref(), &spending.bound[index])
        {
            Some(reason) => Err(reason),
            None => crate::bpf::owned_by(index, || {
                prepared
                    .attach(&spending.bound[index])
                    .and_then(|mut collector| collector.start().map(|()| collector))
            }),
        };
        if let Some(named) = Target::narrowed_to_one() {
            attached = Err(one_process_of_several(&spending.serving[index], named).to_string());
        }
        let attached_ns = monotonic_ns();
        if attached.is_ok() {
            // The programs it loaded are its own from their first run: what
            // they have spent is all this collection's, from zero.
            let programs = programs_since(before.as_ref());
            spending.inside[index] =
                InsideMeter::of(&backend, account, programs, &spending.program_stats);
            let opened = spending.inside[index].read().unwrap_or_default();
            spending.since[index].in_target = opened;
            if let Some(since) = spending.window_since[index].as_mut() {
                since.in_target = opened;
            }
        }
        let took_ns = attached_ns.saturating_sub(entered_ns);
        if self.attributing {
            spending.inside_ns[index] += took_ns;
        }
        let source = spending.serving[index].request.source.to_string();
        let at = self.seconds_in(entered_ns);
        match attached {
            Ok(collector) => {
                let mode = spending.serving[index].request.mode;
                paces[index] = pace_of(collector.as_ref(), mode);
                if let Some(note) =
                    unpaced(&spending.serving[index], collector.as_ref(), paces[index])
                {
                    spending.notes[index].push(note);
                }
                spending.notes[index].push(match escalated {
                    None => format!(
                        "watched only from {at:.3} s into the run, as its window asked: \
                         attaching it took {} while the target ran, and its numbers cover its \
                         window alone",
                        span_ns(took_ns),
                    ),
                    Some(why) => format!(
                        "attached {at:.3} s into the run by the escalation, and watched from \
                         then only: {why}; attaching it took {} while the target ran \
                         (CLOCK_MONOTONIC {entered_ns} to {attached_ns} ns)",
                        span_ns(took_ns),
                    ),
                });
                self.due_ns[index] = attached_ns;
                self.watched[index].opened_ns = attached_ns;
                self.watched[index].attached = true;
                self.watched[index].attach_ns = Some(took_ns);
                slots[index] = Slot::Attached(collector);
            }
            Err(reason) => {
                // A window that asked for something and got nothing: a loss,
                // not an empty series that reads as a quiet one.
                self.losses[index].failed_reads += 1;
                self.losses[index].note(format!(
                    "`{source}` could not be attached {at:.3} s into the run, so nothing was \
                     collected over its window: {reason}"
                ));
                self.closed_because[index] = Some(format!("never attached: {reason}"));
                slots[index] = Slot::Missed;
                self.finished[index] = Some(Finished::Window);
            }
        }
    }

    /// Stops and drops an attached collector, draining what it still holds
    /// into its own collection first. `because` is given how far into the run
    /// the detach happened and says why, as the collection's
    /// `stopped_because`.
    fn detach(
        &mut self,
        index: usize,
        slots: &mut [Slot],
        spending: &mut Spending<'_>,
        because: impl FnOnce(f64) -> String,
    ) {
        let Slot::Attached(mut collector) = std::mem::replace(&mut slots[index], Slot::Missed)
        else {
            unreachable!("only an attached request is detached");
        };
        // Its window closes before its last read, as at the end of the run:
        // what the stop drains was all written before it returns.
        let closed_ns = monotonic_ns();
        self.watched[index].closed_ns = closed_ns;
        // Read before the stop as well as after it: a collector whose `stop`
        // drops its programs leaves them nothing to report afterwards.
        let _ = spending.inside[index].read();
        let mut trailing = collector.stop();
        into_our_ids(
            spending.serving.get(index),
            &mut trailing,
            &mut self.losses[index],
        );
        self.events[index].append(&mut trailing);
        self.losses[index].absorb(collector.losses());
        self.drains[index].grew_to(self.losses[index].dropped, closed_ns);
        let record = Detached {
            value_attr: collector.value_attr(),
            stacks: collector.stacks(),
            notes: collector.notes(),
            by_ring: collector.losses_by_ring(),
            facts: collector.facts(),
            row_stamp: collector.row_stamp(),
            failure: collector.failure(),
            fields: collector.fields(),
            provenance: collector.provenance(),
        };
        // The last look at what it spent inside the target, while its
        // programs are still open to be read.
        let _ = spending.inside[index].read();
        drop(collector);
        // What it cost is weighed up to the end of its stop, which it paid.
        let stopped_ns = monotonic_ns();
        spending.detached(index, stopped_ns);
        self.closed_because[index] = Some(because(self.seconds_in(stopped_ns)));
        slots[index] = Slot::Detached(Box::new(record));
        // A collection whose target had already gone keeps saying so.
        self.finished[index].get_or_insert(Finished::Window);
    }

    /// Applies what the escalation answered: detaches, then attaches.
    fn switch(
        &mut self,
        switches: Vec<Switch>,
        slots: &mut [Slot],
        paces: &mut [Pace],
        spending: &mut Spending<'_>,
    ) {
        let (detaches, attaches): (Vec<Switch>, Vec<Switch>) =
            switches.into_iter().partition(|one| !one.attach);
        for Switch { index, why, .. } in detaches {
            if matches!(slots.get(index), Some(Slot::Attached(_))) {
                self.detach(index, slots, spending, |at| {
                    format!("{} by the escalation: {why}", detached_at(at))
                });
            }
        }
        for Switch { index, why, .. } in attaches {
            if matches!(slots.get(index), Some(Slot::Waiting(_))) {
                self.attach(index, slots, paces, spending, Some(&why));
            }
        }
    }
}

/// How long a run without a descriptor to wake on sleeps before it asks
/// again whether the launched program exited.
pub(super) const EXIT_SLICE_NS: u64 = 10_000_000;

/// Sleeps for `ns`, or until `ended` says the run is over, asking it every
/// [`EXIT_SLICE_NS`].
///
/// For a launched program with no pidfd (Linux before 5.3), whose exit only
/// the next round noticed: the closing round came up to a whole pace after
/// the exit, and every polled rate's last interval was divided by time the
/// program was no longer there for -- `power.wakeups` at `poll@1s` read 75 a
/// second for the last interval of a program that woke 930 times a second to
/// its end (Ubuntu 18.04's 4.15, distroproof). Asking also moves the last time
/// the program was seen alive, which is what bounds its exit's uncertainty.
pub(super) fn sleep_noticing_exit(ns: u64, ended: &mut impl FnMut() -> Option<String>) {
    let until_ns = monotonic_ns().saturating_add(ns);
    loop {
        let now_ns = monotonic_ns();
        if now_ns >= until_ns {
            return;
        }
        std::thread::sleep(StdDuration::from_nanos(
            (until_ns - now_ns).min(EXIT_SLICE_NS),
        ));
        if ended().is_some() {
            return;
        }
    }
}

/// Sleeps for `ns`, or until `wake` becomes readable if that is sooner.
///
/// A readable `wake` is not consumed: a pidfd stays readable once its process
/// has exited, and the round after this one is what asks why.
pub(super) fn sleep_or_wake(ns: u64, wake: Option<RawFd>) {
    let Some(fd) = wake else {
        std::thread::sleep(StdDuration::from_nanos(ns));
        return;
    };
    let timeout = libc::timespec {
        tv_sec: (ns / 1_000_000_000) as _,
        tv_nsec: (ns % 1_000_000_000) as libc::c_long,
    };
    let mut watched = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd this frame owns, and a timeout it owns; no signal
    // mask is changed.
    let result = unsafe { libc::ppoll(&mut watched, 1, &timeout, std::ptr::null()) };
    if result < 0 {
        // Interrupted, or refused: the tick is still an honest fallback, and
        // the round after it asks the program directly.
        std::thread::sleep(StdDuration::from_nanos(ns));
    }
}
