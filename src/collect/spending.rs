//! The budget, held while the run goes: what each request is spending, and
//! when it moves down a rung or stops.

use super::*;

/// How far over its ceiling, in the tool's cpu, a collection may be before
/// the budget acts: once per run at most, 10 ms of cpu over the budget.
pub(super) const BUDGET_SLACK_NS: u64 = 10_000_000;

/// How often a run carrying a budget looks at what it is spending.
///
/// Every check is two cheap reads -- `getrusage` and one `/proc` file -- and
/// `Overhead::measurement_ns` reports what they came to over the run. Ten times
/// a second is often enough to stop a collection that is running away, and rare
/// enough that the answer is not mostly itself: at roughly ten microseconds a
/// check this is about a thousandth of a percent of one cpu.
pub(super) const BUDGET_CHECK_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// Where one collection's accounting starts.
///
/// The whole run, normally. A collection a budget moved onto a cheaper backend
/// starts again from the moment it moved: what the backend before it spent is
/// sunk, and charging the replacement for its predecessor would condemn it
/// before it had read anything.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Since {
    pub(super) self_cpu_ns: u64,
    pub(super) observed_cpu_ns: u64,
    pub(super) inside_ns: u64,
    /// Everything spent inside every collector by then, so a share is a share
    /// of the same span as the cpu time it divides.
    pub(super) total_inside_ns: u64,
    pub(super) wall_opened_ns: u64,
    /// What the collector had spent inside the target by then.
    pub(super) in_target: Inside,
    /// This process's one-time loads by then ([`crate::stack::one_time_cpu_ns`]).
    pub(super) one_time_ns: u64,
}

/// The cost half of a run: what it is spending, what it was allowed to spend,
/// and where it turns when the two disagree.
pub(super) struct Spending<'a> {
    /// What each collection's overhead is a percentage of: its own target's
    /// cpu account, since a core beside a program is weighed against the core.
    pub(super) meters: Vec<Meter>,
    /// Where each collection's cost inside its target is read, which the
    /// tool's own cpu does not include (finding 47).
    pub(super) inside: Vec<InsideMeter>,
    /// What each had spent inside its target when the accounting opened.
    pub(super) inside_opened: Vec<Inside>,
    /// Set at the close when the kernel stopped timing programs part way
    /// through a run that did not hold them on itself.
    pub(super) stats_lost: Option<String>,
    /// Whether a program loaded during the run is timed by the kernel.
    pub(super) program_stats: Result<(), String>,
    /// The target each collector is bound to, which a replacement has to be
    /// attached to as well.
    pub(super) bound: Vec<Target>,
    /// The request each collector is serving *now*. A degradation changes it,
    /// which is why the resolved request written before the run cannot be the
    /// record of what produced the numbers.
    pub(super) serving: Vec<ResolvedRequest>,
    pub(super) ceilings: Vec<Option<f64>>,
    pub(super) cheaper: &'a mut dyn Cheaper,
    /// Wall time spent inside each collector's own calls, which is how one
    /// process's cpu time is shared out between several collectors. Left at
    /// zero when a run has a single collector, because then the share is the
    /// whole and two clock reads per tick would buy nothing.
    pub(super) inside_ns: Vec<u64>,
    pub(super) since: Vec<Since>,
    pub(super) opened_ns: u64,
    pub(super) self_cpu_opened_ns: u64,
    /// [`crate::stack::one_time_cpu_ns`] when the run opened.
    pub(super) one_time_opened_ns: u64,
    pub(super) observed_opened: Vec<u64>,
    pub(super) last_check_ns: u64,
    pub(super) measurement_ns: u64,
    pub(super) measurements: u64,
    /// What a budget did to each collection, in the words the user will read.
    pub(super) notes: Vec<Vec<String>>,
    /// The backends each collection has run through, in order.
    pub(super) served_by: Vec<Vec<String>>,
    /// Where a collection attached mid-run started paying. Its overhead is
    /// over its own window, because that is when it was running.
    pub(super) window_since: Vec<Option<Since>>,
    /// The overhead of a collection that was detached, taken at the detach:
    /// what the run spent after it is not its cost.
    pub(super) frozen: Vec<Option<Overhead>>,
    /// Which collections the escalation attached, and may detach rather than
    /// see the run end when they are over budget.
    pub(super) sheddable: Vec<bool>,
    /// Collections found over budget with nowhere cheaper to go that the
    /// escalation may shed, with the reason the run would otherwise stop for.
    pub(super) over: Vec<(usize, String)>,
    /// Set for a run with no fixed end: each collection is weighed over the
    /// last this many nanoseconds rather than since the run opened. A share
    /// of a run that idled for an hour is a share of that hour, and a burst
    /// would be under any ceiling against it.
    pub(super) rolling_ns: Option<u64>,
    /// Where each account stood at every check within the rolling span, the
    /// oldest first: the baseline a rolling weighing is against.
    pub(super) snapshots: Vec<std::collections::VecDeque<Since>>,
    /// When the run's duration elapses, which the drive loop wakes for: a
    /// counted request is read at a slow cadence, and a run that slept to its
    /// next read ended there instead of at its duration -- `-m count -d 600ms`
    /// observed a second (`open-requests 2947`, `6181`).
    pub(super) deadline_ns: Option<u64>,
}

impl<'a> Spending<'a> {
    /// Opens the accounting, with the collectors attached and the program
    /// running: what was spent resolving, probing and attaching is this tool's
    /// startup, not the price of the measurement.
    pub(super) fn open(
        meters: Vec<Meter>,
        mut inside: Vec<InsideMeter>,
        bound: Vec<Target>,
        serving: Vec<ResolvedRequest>,
        cheaper: &'a mut dyn Cheaper,
    ) -> Self {
        let count = serving.len();
        inside.resize_with(count, || {
            InsideMeter::Unknown("this run was not weighed".to_owned())
        });
        let inside_opened: Vec<Inside> = inside
            .iter_mut()
            .map(|meter| meter.read().unwrap_or_default())
            .collect();
        let inside_since = inside_opened.clone();
        let serving_ids = serving
            .iter()
            .map(|resolved| vec![resolved.backend.id.clone()])
            .collect();
        let ceilings = serving
            .iter()
            .map(|resolved| resolved.request.budget.map(|budget| budget.percent()))
            .collect();
        let opened_ns = monotonic_ns();
        let self_cpu_opened_ns = rusage_ns(libc::RUSAGE_SELF);
        let one_time_opened_ns = crate::stack::one_time_cpu_ns();
        let observed_opened: Vec<u64> = Meter::read_each(&meters)
            .into_iter()
            .map(|read| read.unwrap_or(0))
            .collect();
        Self {
            meters,
            inside,
            inside_opened,
            stats_lost: None,
            program_stats: Err("this run was not weighed".to_owned()),
            bound,
            serving,
            ceilings,
            cheaper,
            inside_ns: vec![0; count],
            since: (0..count)
                .map(|index| Since {
                    self_cpu_ns: self_cpu_opened_ns,
                    observed_cpu_ns: observed_opened.get(index).copied().unwrap_or(0),
                    inside_ns: 0,
                    total_inside_ns: 0,
                    wall_opened_ns: opened_ns,
                    in_target: inside_since.get(index).copied().unwrap_or_default(),
                    one_time_ns: one_time_opened_ns,
                })
                .collect(),
            opened_ns,
            self_cpu_opened_ns,
            one_time_opened_ns,
            observed_opened,
            last_check_ns: opened_ns,
            measurement_ns: 0,
            measurements: 0,
            notes: vec![Vec::new(); count],
            served_by: serving_ids,
            window_since: vec![None; count],
            frozen: vec![None; count],
            sheddable: vec![false; count],
            over: Vec::new(),
            rolling_ns: None,
            snapshots: vec![std::collections::VecDeque::new(); count],
            deadline_ns: None,
        }
    }

    /// What collection `index` is weighed from now: the whole run, or since
    /// it last moved -- or, in a rolling run, the oldest reading of its
    /// account inside the rolling span, whichever is later. Records this
    /// reading for the checks to come.
    pub(super) fn baseline(&mut self, index: usize, now: Since) -> Since {
        let since = self.since[index];
        let Some(rolling_ns) = self.rolling_ns else {
            return since;
        };
        // The latest reading at or before the horizon is the baseline, so
        // the span weighed is the rolling one or a check interval more; none
        // that old yet is the run's first span, weighed from its start.
        let horizon_ns = now.wall_opened_ns.saturating_sub(rolling_ns);
        let snapshots = &mut self.snapshots[index];
        while snapshots.len() >= 2 && snapshots[1].wall_opened_ns <= horizon_ns {
            snapshots.pop_front();
        }
        let baseline = match snapshots.front() {
            Some(oldest)
                if oldest.wall_opened_ns <= horizon_ns
                    && oldest.wall_opened_ns >= since.wall_opened_ns =>
            {
                *oldest
            }
            _ => since,
        };
        snapshots.push_back(now);
        baseline
    }

    /// Whether anything here has a ceiling at all. A run with no budget is
    /// never interrupted to be weighed.
    fn enforcing(&self) -> bool {
        self.ceilings.iter().any(Option::is_some)
    }

    /// What one collection has cost since a baseline.
    pub(super) fn spent(
        &self,
        index: usize,
        since: Since,
        self_cpu_ns: u64,
        observed_cpu_ns: Option<u64>,
        in_target: &Result<Inside, String>,
        now_ns: u64,
    ) -> Overhead {
        let collector_cpu_ns = self_cpu_ns.saturating_sub(since.self_cpu_ns);
        // One process serves every request, so `getrusage` answers for all of
        // them at once. With one collector the share is the whole and there is
        // nothing to approximate; with several it is the fraction of the time
        // spent inside collectors that went to this one.
        let total_inside_ns: u64 = self
            .inside_ns
            .iter()
            .sum::<u64>()
            .saturating_sub(since.total_inside_ns);
        let attributed_cpu_ns = match (self.inside_ns.len(), total_inside_ns) {
            (1, _) => collector_cpu_ns,
            (count, 0) => collector_cpu_ns / count as u64,
            (_, total) => {
                let mine = self.inside_ns[index].saturating_sub(since.inside_ns);
                (collector_cpu_ns as u128 * mine as u128 / total as u128) as u64
            }
        };
        // A one-time load is in the process's account; this collection's
        // share of it is its share of that account, all of it when it is
        // the only one.
        let one_time_ns = crate::stack::one_time_cpu_ns().saturating_sub(since.one_time_ns);
        let one_time_cpu_ns = if collector_cpu_ns == 0 {
            0
        } else {
            ((one_time_ns as u128 * attributed_cpu_ns as u128 / collector_cpu_ns as u128) as u64)
                .min(attributed_cpu_ns)
        };
        let in_target = match (&self.stats_lost, in_target) {
            (Some(lost), Ok(_)) if matches!(self.inside[index], InsideMeter::Programs { .. }) => {
                Err(lost.clone())
            }
            (_, reading) => reading.clone(),
        };
        let (in_target_cpu_ns, in_target_runs, in_target_left_out, in_target_from) = match in_target
        {
            Ok(now) => {
                let counted = !matches!(self.inside[index], InsideMeter::Outside(_));
                (
                    Some(now.cpu_ns.saturating_sub(since.in_target.cpu_ns)),
                    counted.then(|| now.runs.saturating_sub(since.in_target.runs)),
                    now.left_out.saturating_sub(since.in_target.left_out),
                    self.inside[index].describes(),
                )
            }
            Err(why) => (None, None, 0, why),
        };
        Overhead {
            before_window_cpu_ns: since.self_cpu_ns,
            after_window_cpu_ns: 0,
            collector_cpu_ns,
            attributed_cpu_ns,
            observed_cpu_ns: observed_cpu_ns
                .map(|observed| observed.saturating_sub(since.observed_cpu_ns)),
            observed_from: self.meters[index].describes(),
            wall_ns: now_ns.saturating_sub(since.wall_opened_ns),
            measurement_ns: self.measurement_ns,
            measurements: self.measurements,
            in_target_cpu_ns,
            in_target_runs,
            in_target_left_out,
            in_target_from,
            one_time_cpu_ns,
        }
    }

    /// Looks at what the run has cost, and acts when a collection is over its
    /// ceiling. Returns the reason the run must stop, when there is one.
    pub(super) fn check(
        &mut self,
        slots: &mut [Slot],
        events: &mut [Vec<Event>],
        losses: &mut [Losses],
    ) -> Option<String> {
        if !self.enforcing() {
            return None;
        }
        let entered_ns = monotonic_ns();
        if entered_ns.saturating_sub(self.last_check_ns) < BUDGET_CHECK_INTERVAL.as_nanos() as u64 {
            return None;
        }
        let entered_cpu_ns = thread_cpu_ns();
        self.last_check_ns = entered_ns;
        let self_cpu_ns = rusage_ns(libc::RUSAGE_SELF);
        self.measurements += 1;

        let mut stop = None;
        for index in 0..slots.len() {
            let Some(ceiling) = self.ceilings[index] else {
                continue;
            };
            // Only what is running is spending: a request waiting for its
            // window, or past it, has nothing here to weigh.
            if !matches!(slots[index], Slot::Attached(_)) {
                continue;
            }
            // A collection attached mid-run is weighed once it has run for
            // one check interval. Before that its attach -- loading a program,
            // reading the target's maps -- is most of its account, and a
            // percentage taken a few milliseconds in says how recently it was
            // attached rather than what it costs to run.
            if self.window_since[index].is_some_and(|since| {
                entered_ns.saturating_sub(since.wall_opened_ns)
                    < BUDGET_CHECK_INTERVAL.as_nanos() as u64
            }) {
                continue;
            }
            let observed_cpu_ns = self.meters[index].read();
            let in_target = self.inside[index].read();
            let baseline = self.baseline(
                index,
                Since {
                    self_cpu_ns,
                    observed_cpu_ns: observed_cpu_ns.unwrap_or(0),
                    inside_ns: self.inside_ns[index],
                    total_inside_ns: self.inside_ns.iter().sum(),
                    wall_opened_ns: entered_ns,
                    in_target: in_target.clone().unwrap_or_default(),
                    one_time_ns: crate::stack::one_time_cpu_ns(),
                },
            );
            let spent = self.spent(
                index,
                baseline,
                self_cpu_ns,
                observed_cpu_ns,
                &in_target,
                entered_ns,
            );
            // No denominator yet is not "within budget": it is "not knowable
            // yet", and a target that has burned no cpu at all has nothing for
            // an overhead to be a percentage of.
            let Some(percent) = spent.percent() else {
                continue;
            };

            if percent <= ceiling {
                continue;
            }
            // Over by less than the slack is not over yet: a program that
            // waits has burnt a few milliseconds by the first check, and the
            // collection's first drains are most of its account then (5 runs
            // of 6 of a waiting program stopped 0.1 s in, at 45 to 310 %,
            // where their whole windows read 16 to 26 %). A collection over
            // its ceiling at its running rate passes the slack within a
            // second or two of a busy target (`docs/DECISIONS.md`).
            if spent
                .excess_ns(ceiling)
                .is_some_and(|excess| excess < BUDGET_SLACK_NS as f64)
            {
                continue;
            }
            let source = self.serving[index].request.source.to_string();
            let was = self.serving[index].backend.id.clone();
            // What the collector spent inside the target is in the percentage,
            // and the reason says how much of it, since the tool's own cpu
            // alone would read as far under the ceiling.
            let inside = match spent.in_target_cpu_ns {
                Some(ns) if ns > 0 => format!(" ({} of it inside the target)", span_ns(ns)),
                _ => String::new(),
            };
            let inside = if spent.one_time_cpu_ns > 0 {
                format!(
                    "{inside} (and {} once, loading symbol tables, weighed apart)",
                    span_ns(spent.one_time_cpu_ns)
                )
            } else {
                inside
            };
            // A collection a budget has already moved once is weighed on the
            // window since it moved, so the reason has to say which window it
            // is talking about or it reads as disagreeing with the run's own
            // total.
            let window = match self.rolling_ns {
                Some(rolling_ns) => format!(
                    " over the last {}, {ROLLING_BUDGET}",
                    span_ns(
                        entered_ns
                            .saturating_sub(baseline.wall_opened_ns)
                            .min(rolling_ns)
                    )
                ),
                None if self.served_by[index].len() > 1 => {
                    format!(" since it was moved onto `{was}`")
                }
                None => String::new(),
            };
            match self.cheaper.next(index, &spent, ceiling) {
                Some(next) => match self.degrade(index, next, slots, events, losses) {
                    Ok(moved) => self.notes[index].push(format!(
                        "over budget at {:.2} ms into the run: `{source}` through `{was}` spent \
                         {percent:.2} % of the target's cpu time{inside} against a ceiling of \
                         {ceiling} %, so it was moved onto `{}`{} for the rest of it; both \
                         counted from {:.3} ms to {:.3} ms into the run, and what the target did \
                         in that span can be in both",
                        entered_ns.saturating_sub(self.opened_ns) as f64 / 1e6,
                        moved.onto,
                        moved
                            .into_mode
                            .as_ref()
                            .map_or(String::new(), |mode| format!(" in `{mode}`")),
                        moved.overlap_from_ns.saturating_sub(self.opened_ns) as f64 / 1e6,
                        moved.overlap_to_ns.saturating_sub(self.opened_ns) as f64 / 1e6,
                    )),
                    Err(why) => {
                        stop = Some(format!(
                            "`{source}` spent {percent:.2} % of the target's cpu time{inside}\
                             {window} {OVER_ITS_CEILING} {ceiling} %, and the cheaper backend \
                             could not be started: {why}"
                        ));
                        break;
                    }
                },
                None => {
                    let reason = format!(
                        "`{source}` spent {percent:.2} % of the target's cpu time{inside}{window} \
                         {OVER_ITS_CEILING} {ceiling} %, and nothing cheaper than `{was}` is \
                         available"
                    );
                    if self.sheddable[index] {
                        self.over.push((index, reason));
                        continue;
                    }
                    stop = Some(reason);
                    break;
                }
            }
        }

        self.measurement_ns += thread_cpu_ns().saturating_sub(entered_cpu_ns);
        stop
    }

    /// Moves one collection onto a cheaper backend without ending the run.
    ///
    /// Everything the outgoing collector still held is drained into the run
    /// first: those events were paid for and are as real as the ones that come
    /// after. What changes from here is how the next ones are obtained, which
    /// is why the note says *when* it changed.
    fn degrade(
        &mut self,
        index: usize,
        next: ResolvedRequest,
        slots: &mut [Slot],
        events: &mut [Vec<Event>],
        losses: &mut [Losses],
    ) -> Result<Moved, String> {
        let onto = next.backend.id.clone();
        let Slot::Attached(current) = &mut slots[index] else {
            return Err("it is not attached".to_owned());
        };
        let before = loads_programs(&onto).then(programs_held);
        let prepared = prepare(&next).map_err(|error| error.to_string())?;
        let account = prepared.inside();
        Target::narrowed_to_one();
        // The replacement may be counting from its attach, and the outgoing
        // collector counts until its last read inside `stop`: between the two
        // both see the target. Stopping first would trade that overlap for a
        // gap, whose events nobody counts and nothing could show; an overlap
        // is bounded, and said. Nothing else runs between the replacement's
        // start and the outgoing's stop, which keeps it to the two calls.
        let overlap_from_ns = monotonic_ns();
        let mut replacement = crate::bpf::owned_by(index, || prepared.attach(&self.bound[index]))?;
        if let Some(named) = Target::narrowed_to_one() {
            return Err(one_process_of_several(&next, named).to_string());
        }
        crate::bpf::owned_by(index, || replacement.start())?;
        // What the outgoing collector spent inside the target, read while its
        // programs are open: a `stop` that drops them leaves nothing to read.
        let _ = self.inside[index].read();
        let mut trailing = current.stop();
        let overlap_to_ns = monotonic_ns();
        into_our_ids(self.serving.get(index), &mut trailing, &mut losses[index]);
        events[index].append(&mut trailing);
        losses[index].absorb(current.losses());

        let programs = programs_since(before.as_ref());
        let meter = InsideMeter::of(&onto, account, programs, &self.program_stats);
        self.inside[index].moved_onto(meter);
        let was = &self.serving[index];
        let into_mode = (was.backend.id == onto).then(|| next.request.mode.to_string());
        losses[index].note(if was.backend.id == onto {
            format!(
                "the budget moved this collection from `{}` to `{}` on `{onto}` mid-run; the \
                 series is two modes end to end",
                was.request.mode, next.request.mode
            )
        } else {
            format!(
                "the budget moved this collection from `{}` to `{onto}` mid-run; the series is \
                 two backends end to end",
                was.backend.id
            )
        });
        *current = replacement;
        self.serving[index] = next;
        self.served_by[index].push(onto.clone());
        // The replacement is weighed on its own account.
        self.since[index] = Since {
            wall_opened_ns: self.opened_ns,
            ..self.now(index)
        };
        Ok(Moved {
            onto,
            into_mode,
            overlap_from_ns,
            overlap_to_ns,
        })
    }

    /// Where one collection's account stands at this instant.
    fn now(&mut self, index: usize) -> Since {
        Since {
            self_cpu_ns: rusage_ns(libc::RUSAGE_SELF),
            observed_cpu_ns: self.meters[index].read().unwrap_or(0),
            inside_ns: self.inside_ns[index],
            total_inside_ns: self.inside_ns.iter().sum(),
            wall_opened_ns: monotonic_ns(),
            in_target: self.inside[index].read().unwrap_or_default(),
            one_time_ns: crate::stack::one_time_cpu_ns(),
        }
    }

    /// Opens the account of a collection attached mid-run: its budget, and
    /// the overhead its results report, are over its own window.
    ///
    /// Taken *before* the attach, so that what attaching cost -- loading a
    /// program into the kernel while the target runs -- is in its account
    /// rather than in nobody's.
    pub(super) fn attaching(&mut self, index: usize) {
        let entered_cpu_ns = thread_cpu_ns();
        let since = self.now(index);
        self.since[index] = since;
        self.window_since[index] = Some(since);
        self.measurements += 1;
        self.measurement_ns += thread_cpu_ns().saturating_sub(entered_cpu_ns);
    }

    /// Closes the account of a collection detached mid-run.
    pub(super) fn detached(&mut self, index: usize, closed_ns: u64) {
        let entered_cpu_ns = thread_cpu_ns();
        let since = self.window_since[index].unwrap_or(self.whole_run(index));
        let self_cpu_ns = rusage_ns(libc::RUSAGE_SELF);
        let observed_cpu_ns = self.meters[index].read();
        let in_target = self.inside[index].read();
        self.measurements += 1;
        self.measurement_ns += thread_cpu_ns().saturating_sub(entered_cpu_ns);
        self.frozen[index] = Some(self.spent(
            index,
            since,
            self_cpu_ns,
            observed_cpu_ns,
            &in_target,
            closed_ns,
        ));
    }

    /// Reads every meter once more, just before the launched program and the
    /// processes it started are killed, so that a meter keeping its highest
    /// reading holds what the tree spent up to the stop (`HighWater`): read
    /// after the kill, a launched program's account is only what it reaped.
    pub(super) fn read_before_the_stop(&mut self) {
        let entered_cpu_ns = thread_cpu_ns();
        let _ = Meter::read_each(&self.meters);
        self.measurements += 1;
        self.measurement_ns += thread_cpu_ns().saturating_sub(entered_cpu_ns);
    }

    /// Reads every collection's in-target meter just before the collectors
    /// are stopped. A meter keeps its last reading, and a collector whose
    /// `stop` drops its programs closes the descriptors the kernel's account
    /// is read through: read only at the close, such a run without a budget
    /// -- never checked, so never read -- said zero spent inside the target
    /// (the block programs read "0 runs for 0 ns" beside 26.8 M requests).
    pub(super) fn read_in_target_before_the_stops(&mut self) {
        let entered_cpu_ns = thread_cpu_ns();
        for meter in &mut self.inside {
            let _ = meter.read();
        }
        self.measurements += 1;
        self.measurement_ns += thread_cpu_ns().saturating_sub(entered_cpu_ns);
    }

    /// The account of a collection that ran for the whole of the run.
    pub(super) fn whole_run(&self, index: usize) -> Since {
        Since {
            self_cpu_ns: self.self_cpu_opened_ns,
            observed_cpu_ns: self.observed_opened.get(index).copied().unwrap_or(0),
            inside_ns: 0,
            total_inside_ns: 0,
            wall_opened_ns: self.opened_ns,
            in_target: self.inside_opened.get(index).copied().unwrap_or_default(),
            one_time_ns: self.one_time_opened_ns,
        }
    }

    /// Closes the accounting and shares the run's cost out between the
    /// collections. This is the whole window, not the part since a degradation:
    /// what the run cost is what it cost.
    pub(super) fn close(&mut self) -> Vec<Overhead> {
        let entered_cpu_ns = thread_cpu_ns();
        let self_cpu_ns = rusage_ns(libc::RUSAGE_SELF);
        let observed_cpu_ns = Meter::read_each(&self.meters);
        let in_target: Vec<Result<Inside, String>> =
            self.inside.iter_mut().map(InsideMeter::read).collect();
        self.measurements += 1;
        let closed_ns = monotonic_ns();
        self.measurement_ns += thread_cpu_ns().saturating_sub(entered_cpu_ns);

        (0..self.ceilings.len())
            .map(|index| {
                if let Some(frozen) = &self.frozen[index] {
                    return frozen.clone();
                }
                let since = self.window_since[index].unwrap_or(self.whole_run(index));
                self.spent(
                    index,
                    since,
                    self_cpu_ns,
                    observed_cpu_ns[index],
                    &in_target[index],
                    closed_ns,
                )
            })
            .collect()
    }
}

#[cfg(test)]
impl Spending<'static> {
    /// A run with no ceiling and nothing to weigh it against, for the tests
    /// that are about the pacing rather than the price.
    pub(super) fn unweighed(count: usize) -> Spending<'static> {
        let mut spending = Spending::open(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Box::leak(Box::new(StopWhenOverBudget)),
        );
        spending.meters = (0..count)
            .map(|_| Meter::Nothing("this run was not weighed"))
            .collect();
        spending.inside = (0..count).map(|_| InsideMeter::Outside(OUTSIDE)).collect();
        spending.inside_opened = vec![Inside::default(); count];
        spending.bound = vec![Target::System; count];
        spending.ceilings = vec![None; count];
        spending.inside_ns = vec![0; count];
        spending.since = vec![Since::default(); count];
        spending.notes = vec![Vec::new(); count];
        spending.served_by = vec![Vec::new(); count];
        spending.window_since = vec![None; count];
        spending.frozen = vec![None; count];
        spending.sheddable = vec![false; count];
        spending.snapshots = vec![std::collections::VecDeque::new(); count];
        spending
    }

    /// A run weighed against this process's own cpu time.
    ///
    /// Numerator and denominator are then the same account -- `getrusage` and
    /// `/proc/self/stat` report the same process -- so the ratio is around one
    /// hundred percent however much or little the machine is doing, and a test
    /// can put a ceiling either side of it without depending on the load. Which
    /// matters here: `cargo test` runs its tests in parallel threads of one
    /// process, and a measurement of "this process" is a measurement of all of
    /// them.
    /// The same, with this thread as the denominator rather than the process.
    ///
    /// For a request attached mid-run the numerator is that one collector's
    /// cpu, read on the thread that drives it, while "this process" is every
    /// test `cargo test` runs in parallel beside it -- twenty-odd threads in a
    /// full run, which put a collector spinning a whole thread under a 50 %
    /// ceiling and made the escalation's shedding test fail in every full run
    /// and pass alone. `/proc/<tid>/task/<tid>/stat` is the thread's own
    /// account; `/proc/<tid>/stat` is its whole process and every child it
    /// reaped, which let the forked children of other tests keep this run
    /// under its ceiling.
    pub(super) fn weighed_against_this_thread(serving: Vec<ResolvedRequest>) -> Spending<'static> {
        // SAFETY: `gettid` takes no argument and cannot fail.
        let thread = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let count = serving.len();
        Spending::open(
            (0..count).map(|_| Meter::Threads(vec![thread])).collect(),
            (0..count).map(|_| InsideMeter::Outside(OUTSIDE)).collect(),
            vec![Target::Pid(vec![thread]); count],
            serving,
            Box::leak(Box::new(StopWhenOverBudget)),
        )
    }

    pub(super) fn weighed_against_this_process(serving: Vec<ResolvedRequest>) -> Spending<'static> {
        let own = std::process::id() as i32;
        let count = serving.len();
        Spending::open(
            (0..count).map(|_| Meter::Tasks(vec![own])).collect(),
            (0..count).map(|_| InsideMeter::Outside(OUTSIDE)).collect(),
            vec![Target::Pid(vec![own]); count],
            serving,
            Box::leak(Box::new(StopWhenOverBudget)),
        )
    }
}
