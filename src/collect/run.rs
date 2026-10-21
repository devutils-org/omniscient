//! The run: one window over one target, from the resolved requests to their
//! collections.

use super::*;

/// How a session is run: the one entry point to the runner.
///
/// Every run is one window over one target, whatever it is asked to do on
/// top: several requests share it deliberately -- running them one after
/// another would measure several moments and present them as one, and a join
/// across them would correlate a program with its own past. What varies is
/// what the run may turn to, and each is an option here with the default an
/// ordinary run takes:
///
/// - **`cheaper`**: where to turn when a `--budget` is exceeded. The split
///   keeps the collector layer from knowing about the catalog: *which backend
///   is cheaper and reachable here* is answered by whoever holds both. With
///   none, the run stops when it is over its budget.
/// - **`unserved`**: the session's held requests this machine could not
///   serve. They are never attached; the escalation is told of them, and a
///   held request that does not resolve is a layer the machine cannot serve,
///   not a failed run.
/// - **`bound`**: a program the run launches and no request targets, whose
///   life is the window -- as `perf stat -a -- prog` measures the machine for
///   a program's life. A request whose target is a launch beside it is
///   refused: the program is launched once.
/// - **`escalation`**: what is asked between rounds which held requests to
///   attach and detach. By default the session's own, when it holds a window
///   written `on <layer>`.
/// - **`recorder`**: a recording, with no fixed end but its longest
///   duration, keeping what `recorder.retain_ns` holds and handing a
///   [`Document`] to its sink each time its trigger fires. A recording asks
///   no escalation.
pub struct Run<'r, 'a> {
    resolved: &'r [ResolvedRequest],
    cheaper: Option<&'r mut dyn Cheaper>,
    unserved: &'r [Unserved],
    bound: Option<&'r [String]>,
    escalation: Asked<'r>,
    recorder: Option<&'r mut Recorder<'a>>,
}

/// Which escalation a run asks.
pub(super) enum Asked<'r> {
    /// The session's own, if it holds one.
    Session,
    /// This one, or none.
    Given(Option<&'r mut dyn Escalate>),
}

/// What a run returns: each request's collection, and what a recording did.
#[derive(Debug)]
pub struct Ran {
    pub collections: Vec<Collection>,
    pub recorded: Option<Recorded>,
}

impl<'r, 'a> Run<'r, 'a> {
    /// An ordinary run of `resolved`.
    pub fn new(resolved: &'r [ResolvedRequest]) -> Self {
        Self {
            resolved,
            cheaper: None,
            unserved: &[],
            bound: None,
            escalation: Asked::Session,
            recorder: None,
        }
    }

    pub fn cheaper(mut self, cheaper: &'r mut dyn Cheaper) -> Self {
        self.cheaper = Some(cheaper);
        self
    }

    pub fn unserved(mut self, unserved: &'r [Unserved]) -> Self {
        self.unserved = unserved;
        self
    }

    pub fn bound(mut self, bound: Option<&'r [String]>) -> Self {
        self.bound = bound;
        self
    }

    /// Asks `escalation` instead of the session's, or nothing.
    pub fn escalation(mut self, escalation: Option<&'r mut dyn Escalate>) -> Self {
        self.escalation = Asked::Given(escalation);
        self
    }

    pub fn recorder(mut self, recorder: &'r mut Recorder<'a>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    /// Runs it.
    pub fn go(self) -> Result<Ran, CollectError> {
        let mut stop = StopWhenOverBudget;
        let cheaper: &mut dyn Cheaper = match self.cheaper {
            Some(cheaper) => cheaper,
            None => &mut stop,
        };
        let recording = self.recorder.is_some();
        let (collections, recorded) = match (self.recorder, self.escalation) {
            (Some(recorder), _) => {
                recording_holds(self.resolved, recorder)?;
                run_core(self.resolved, cheaper, None, Some(recorder), self.bound)?
            }
            (None, Asked::Given(given)) => {
                run_core(self.resolved, cheaper, given, None, self.bound)?
            }
            (None, Asked::Session) => {
                let mut owned = escalation_for(self.resolved, self.unserved)?;
                let escalation = owned
                    .as_deref_mut()
                    .map(|escalation| escalation as &mut dyn Escalate);
                run_core(self.resolved, cheaper, escalation, None, self.bound)?
            }
        };
        Ok(Ran {
            collections,
            recorded: recording.then(|| recorded.unwrap_or_default()),
        })
    }
}

/// Refuses a recording the runner cannot hold, for a caller that built its
/// requests without a session. `Session::check_recording` says the same
/// things first, before anything is resolved.
pub(super) fn recording_holds(
    resolved: &[ResolvedRequest],
    recorder: &Recorder<'_>,
) -> Result<(), CollectError> {
    let refuse = |reason: String| Err(CollectError::Recording { reason });
    if recorder.on >= resolved.len() {
        return refuse(format!(
            "its trigger reads request {} of a run of {}",
            recorder.on,
            resolved.len()
        ));
    }
    if recorder.retain_ns == 0 || recorder.documents == 0 {
        return refuse(
            "it needs a retention bound and a bound on documents, both above zero".to_owned(),
        );
    }
    if !resolved.iter().any(|one| one.request.duration.is_some()) {
        return refuse("it needs a longest duration, and no request has one".to_owned());
    }
    for one in resolved {
        if one.request.window.is_some() {
            return refuse(format!(
                "`{}` has a window, and a request in a recording is watched for the whole run",
                one.request.source
            ));
        }
        if matches!(one.request.mode, Mode::Count | Mode::Snapshot) {
            return refuse(format!(
                "`{}` is a single reading, and a recording retains a series",
                one.request.source
            ));
        }
    }
    Ok(())
}

/// Where a session's escalation is answered from: the one line of the
/// collector layer that names triage.
///
/// The runner knows [`Escalate`] and nothing behind it, the way it knows
/// [`Cheaper`]; this is the registration, as `registry()` is for the
/// families. A session holds a request for the escalation with a window
/// written `on <layer>`, and one that holds none is run without asking
/// anything.
pub(super) fn escalation_for(
    resolved: &[ResolvedRequest],
    unserved: &[Unserved],
) -> Result<Option<Box<dyn Escalate>>, CollectError> {
    if !resolved
        .iter()
        .any(|one| one.request.window.is_some_and(|window| window.on.is_some()))
        && unserved.is_empty()
    {
        return Ok(None);
    }
    crate::triage::escalate::Policy::for_run_with(resolved, unserved)
        .map(|policy| Some(Box::new(policy) as Box<dyn Escalate>))
        .map_err(|refusal| CollectError::Escalation {
            reason: refusal.to_string(),
        })
}

/// The one runner behind every entry point: an ordinary run, an escalating
/// one, and a recording.
pub(super) fn run_core(
    resolved: &[ResolvedRequest],
    cheaper: &mut dyn Cheaper,
    escalation: Option<&mut dyn Escalate>,
    recorder: Option<&mut Recorder<'_>>,
    bound: Option<&[String]>,
) -> Result<(Vec<Collection>, Option<Recorded>), CollectError> {
    assert!(!resolved.is_empty(), "a run needs at least one request");
    let workload = workload_of(resolved)?;
    let bound = bound.filter(|argv| !argv.is_empty());
    // Whether each request is about the workload, which decides what it is
    // bound to, what its overhead is a percentage of, and whether its target
    // going away ends the run. With no workload in the session every target
    // is the run's own, as it was when a run had exactly one.
    let about_workload: Vec<bool> = resolved
        .iter()
        .map(|resolved| workload.is_none_or(|workload| &resolved.request.target == workload))
        .collect();
    let launched_argv: Option<&[String]> = match (workload, bound) {
        (Some(Target::Launch(argv)), None) => Some(argv),
        (Some(Target::Launch(argv)), Some(bound)) => {
            return Err(CollectError::MixedTargets {
                first: format!("a run bounded by `{}`", bound.join(" ")),
                second: Target::Launch(argv.clone()).to_string(),
            })
        }
        (_, bound) => bound,
    };
    // A process the run did not start, whose exit ends it as a launched
    // program's does: `--pid` alone, as `perf stat -p` ends with its
    // process. Not beside a bound, which is then the run's end.
    let exits = match (workload, launched_argv) {
        (Some(Target::Pid(pids)), None) => {
            let exits = launch::Exits::of(pids);
            exits.watches_all(pids).then_some(exits)
        }
        _ => None,
    };

    // Asked first, because "no collector exists for this backend" is a
    // different answer from "this target is not wired up yet", and reporting
    // the second when the first is true sends the reader to the wrong place.
    //
    // Every request is asked, not just requests up to the first refusal. A
    // session with five unbuilt sources used to report one of them and stop,
    // so fixing it took five runs; the validator has always answered for every
    // rejection at once, and this is the same courtesy one layer down.
    //
    // The kernel is asked to time eBPF programs first, so that it counts every
    // run of the ones about to be loaded; and each request's programs are the
    // ones that appear among this process's descriptors while it is prepared
    // and attached, which is what its cost inside the target is read from.
    let program_stats = ProgramStats::for_run(resolved);
    // In a container the ids every program writes are translated: the table
    // starts now, and with it the program that remembers each task's birth,
    // so that a task born and gone between two reads is still named.
    if resolved.iter().any(|one| loads_programs(&one.backend.id)) {
        crate::bpf::pidns::remember();
    }
    let mut programs: Vec<Vec<(RawFd, u32)>> = vec![Vec::new(); resolved.len()];
    let mut prepared: Vec<Box<dyn Prepared>> =
        all_or_nothing(resolved.iter().enumerate().map(|(index, resolved)| {
            let before = loads_programs(&resolved.backend.id).then(programs_held);
            Target::narrowed_to_one();
            let prepared = prepare(resolved);
            programs[index].extend(programs_since(before.as_ref()));
            match Target::narrowed_to_one() {
                Some(named) => Err(one_process_of_several(resolved, named)),
                None => prepared,
            }
        }))?;

    // A window this collector or this mode cannot honour is refused while
    // nothing is running, and before the environment is merged: a collector
    // given to the program at exec is exactly the one a window cannot move.
    let unheld: Vec<Option<String>> = all_or_nothing(
        resolved
            .iter()
            .zip(&prepared)
            .map(|(resolved, prepared)| window_honoured(resolved, prepared.as_ref())),
    )?;
    let mut plans: Vec<Plan> = resolved
        .iter()
        .map(|resolved| Plan::of(resolved.request.window))
        .collect();
    for (plan, unheld) in plans.iter_mut().zip(&unheld) {
        plan.unheld.clone_from(unheld);
    }

    // Only the collectors of the launch act on it. A machine target beside it
    // is not given to the program, so what its collector would want of the
    // program's environment -- nothing, for every collector written -- is not
    // the program's business. Nor is a held request's that this run cannot
    // hold: it is never attached.
    let launching: Vec<bool> = resolved
        .iter()
        .zip(&unheld)
        .map(|(resolved, unheld)| {
            matches!(resolved.request.target, Target::Launch(_)) && unheld.is_none()
        })
        .collect();
    share_launches(&mut prepared, &launching);
    // Asked once the launch is shared: the account a shared channel keeps is
    // the one the program was given.
    let mut accounts: Vec<Option<Box<dyn InsideAccount>>> =
        prepared.iter().map(|prepared| prepared.inside()).collect();
    let environment = merge_environments(
        prepared
            .iter()
            .zip(&launching)
            .filter_map(|(prepared, launching)| launching.then_some(prepared.as_ref())),
    )?;

    // A collector that can only be given to a program at exec is asked about
    // next, and before the stop condition, because the two refusals send the
    // reader to different places: "pass --duration" is advice for a request
    // that can work, and this one cannot. Answering the shallower question
    // first sent a caller off to add a flag that would change nothing.
    for (index, prepared) in prepared.iter().enumerate() {
        let target = &resolved[index].request.target;
        if matches!(target, Target::Launch(_)) || unheld[index].is_some() {
            continue;
        }
        if let Some(reason) = prepared.requires_launch() {
            return Err(CollectError::UnsupportedTarget {
                source_id: resolved[index].request.source.to_string(),
                target: format!("{} ({reason})", target.kind()),
            });
        }
    }

    // A run that nothing will ever stop is refused before the target is
    // launched or anything is attached to it -- but after the collectors have
    // been asked for, because "no collector is written for this" is the deeper
    // answer and the one worth giving first. A duration, a program whose exit
    // ends the run, a single read, or a window closing on every request: with
    // none of them there is no window, and observing a target that nothing
    // will stop watching is worse than refusing.
    let duration_ns = resolved
        .iter()
        .filter_map(|resolved| resolved.request.duration)
        .map(|duration| duration.as_nanos())
        .max();
    let snapshot = resolved
        .iter()
        .all(|resolved| resolved.request.mode == Mode::Snapshot);
    let every_window_closes = resolved.iter().all(|resolved| {
        resolved
            .request
            .window
            .is_some_and(|window| window.stop.is_some())
    });
    if duration_ns.is_none()
        && launched_argv.is_none()
        && exits.is_none()
        && !snapshot
        && !every_window_closes
    {
        return Err(CollectError::NoStopCondition);
    }
    if let Some(duration_ns) = duration_ns {
        all_or_nothing(
            resolved
                .iter()
                .map(|resolved| window_inside_the_run(resolved, duration_ns)),
        )?;
    }

    // A collector attached mid-run pays for its attach inside the run, while
    // the target runs and while every other collector waits for the round to
    // finish. The one fixed cost there known to be large is the kernel's
    // symbol table, which a profile that walks into the kernel reads on its
    // first start: measured at 962 ms for a `cpu.profile` the escalation
    // attached, during which nothing else was read and which the profile's
    // own cost then carried as 71 % of the target's cpu. It describes the
    // machine rather than the target, so it is paid here, before the launch,
    // whenever a request attached late asks for stacks.
    let late_stacks = resolved.iter().any(|resolved| {
        let request = &resolved.request;
        request.window.is_some_and(|window| window.attaches_late())
            && (request.stack.is_some() || request.emit.contains(&crate::request::Emit::Stack))
            // A user-only stack never walks into the kernel, so it never
            // reads the table this pays for (`profile.rs` primes it only for
            // a callchain with a kernel half).
            && resolved.stack.as_ref().is_none_or(|choice| !choice.user_only)
    });
    if late_stacks {
        crate::stack::prime_kernel_symbols();
    }

    // A launched program is both the target and the clock: the run ends when it
    // does, which is what makes `omn <source> -- ./prog` a complete measurement
    // rather than a sample of one. It is held at a gate so that collectors are
    // attached before its first instruction; otherwise the interesting part of
    // a short program is over before anything is watching.
    let mut child = match launched_argv {
        Some(argv) => {
            Some(
                launch::gated(argv, &environment).map_err(|error| CollectError::Launch {
                    program: argv.first().cloned().unwrap_or_default(),
                    error: error.to_string(),
                })?,
            )
        }
        None => None,
    };

    // A launched program replaces the target's own identity: the collectors
    // observe the process that was just created, not a pid the request named.
    // A target that names no process -- an interface, the machine -- is passed
    // through, and a collector that needs a pid refuses it by name. Each
    // request is bound to its own target: a core beside a program is the core.
    let bound: Vec<Target> = resolved
        .iter()
        .map(|resolved| match (&child, &resolved.request.target) {
            (Some(child), Target::Launch(_)) => Target::Pid(vec![child.pid()]),
            (_, target) => target.clone(),
        })
        .collect();

    // Everything is attached now except what a window asks to attach later,
    // which waits prepared: its factory has answered, so the only refusal left
    // for it is the one a running target can give.
    let mut slots: Vec<Slot> = Vec::with_capacity(prepared.len());
    for (index, prepared) in prepared.into_iter().enumerate() {
        if plans[index].unheld.is_some() {
            slots.push(Slot::Missed);
            continue;
        }
        if resolved[index]
            .request
            .window
            .is_some_and(|window| window.attaches_late())
        {
            slots.push(Slot::Waiting(prepared));
            continue;
        }
        let source_id = resolved[index].request.source.to_string();
        let before = loads_programs(&resolved[index].backend.id).then(programs_held);
        Target::narrowed_to_one();
        // These two happen against a real target -- a counter that would not
        // open, a file that is not readable, a process that has gone -- so an
        // unmarked refusal here is a failure of this run. A collector refusing
        // a shape it has not implemented says so with `not_built`.
        let attached = crate::bpf::owned_by(index, || prepared.attach(&bound[index]));
        if let Some(named) = Target::narrowed_to_one() {
            if let Some(child) = child.as_mut() {
                child.terminate();
            }
            return Err(one_process_of_several(&resolved[index], named));
        }
        let mut collector =
            attached.map_err(|reason| CollectError::start(&source_id, reason, Whose::Machine))?;
        if let Err(reason) = crate::bpf::owned_by(index, || collector.start()) {
            if let Some(child) = child.as_mut() {
                child.terminate();
            }
            return Err(CollectError::start(source_id, reason, Whose::Machine));
        }
        programs[index].extend(programs_since(before.as_ref()));
        slots.push(Slot::Attached(collector));
    }

    // Everything is attached; let the program run.
    if let Some(child) = child.as_mut() {
        child.release();
    }

    // Each request at its own pace, from its own mode and its own collector.
    // A request still waiting for its window is paced when it is attached.
    let mut paces: Vec<Pace> = slots
        .iter()
        .zip(resolved)
        .map(|(slot, resolved)| match slot {
            Slot::Attached(collector) => pace_of(collector.as_ref(), resolved.request.mode),
            _ => Pace::UNSET,
        })
        .collect();

    let mut degradations: Vec<Vec<String>> = slots
        .iter()
        .zip(resolved)
        .zip(&paces)
        .map(|((slot, resolved), pace)| match slot {
            Slot::Attached(collector) => unpaced(resolved, collector.as_ref(), *pace)
                .into_iter()
                .collect(),
            _ => Vec::new(),
        })
        .collect();

    let deadline_ns = duration_ns.map(|longest| monotonic_ns() + longest);

    // The cost accounting opens here, with everything attached and the program
    // released: what was spent loading the catalog, probing the machine and
    // opening counters is this tool's startup, not the price of the
    // measurement, and charging the measurement for it would make every short
    // run look ruinous. A collector attached later is attached inside this
    // window, and pays for its own attach.
    let launched_pid = child.as_ref().map(|child| child.pid());
    let meters = resolved
        .iter()
        .map(|resolved| Meter::of(&resolved.request.target, launched_pid))
        .collect();
    let counting = program_stats.counting();
    let inside = resolved
        .iter()
        .zip(accounts.iter_mut())
        .zip(programs)
        .map(|((resolved, account), programs)| {
            InsideMeter::of(&resolved.backend.id, account.take(), programs, &counting)
        })
        .collect();
    let mut spending = Spending::open(meters, inside, bound, resolved.to_vec(), cheaper);
    spending.program_stats = counting;
    spending.deadline_ns = deadline_ns;
    spending.sheddable = plans.iter().map(|plan| plan.held).collect();
    let started_ns = spending.opened_ns;
    let wake = child
        .as_ref()
        .and_then(|child| child.exit_fd())
        .or_else(|| exits.as_ref().and_then(launch::Exits::wake_fd));
    // A launched program the kernel gives no pidfd for (before Linux 5.3) is
    // asked between slices of the run's sleep instead.
    let notice_exit = child
        .as_ref()
        .is_some_and(|child| child.exit_fd().is_none());
    // A recording weighs its budget over a rolling span rather than the
    // whole run, which has no fixed end to be a share of.
    let budget_interval_ns = recorder.as_ref().map(|recorder| {
        recorder
            .retain_ns
            .max(RECORDING_BUDGET_FLOOR.as_nanos() as u64)
    });
    spending.rolling_ns = budget_interval_ns;
    let mut recording =
        recorder.map(|recorder| Recording::new(recorder, resolved, &slots, started_ns));
    let Driven {
        mut events,
        mut losses,
        mut watched,
        mut closed_because,
        stopped_because,
        mut escalated,
        mut drains,
    } = drive(
        &mut slots,
        &mut paces,
        &plans,
        &about_workload,
        wake,
        notice_exit,
        &mut spending,
        escalation,
        recording.as_mut(),
        || {
            if let Some(child) = child.as_mut() {
                if let Some(status) = child.finished() {
                    return Some(format!("the launched program exited with {status}"));
                }
            }
            if let Some(reason) = exits.as_ref().and_then(launch::Exits::all_exited) {
                return Some(reason);
            }
            match deadline_ns {
                Some(deadline_ns) if monotonic_ns() >= deadline_ns => {
                    Some("the requested duration elapsed".to_owned())
                }
                _ => None,
            }
        },
    );

    // Whatever the kernel or the shim published between the last tick and the
    // stop still belongs to this run.
    //
    // The window closes before any of them is stopped. A drained collector's
    // last read is inside its own `stop()`, and the run stops them one after
    // another, so a window closed after the last stop claimed the span after
    // each earlier collector's last read, where nothing of it could be in a
    // row: stream S measured 6 of 930 blocks with `sched.offcpu` alone and 71
    // with five collectors stopping after it. One instant before the first
    // stop is before every last read, and it is one window for every
    // collector the run held to the end, as a session's sources share one.
    let closing_ns = monotonic_ns();
    let ended_s = closing_ns.saturating_sub(started_ns) as f64 / 1e9;
    spending.read_in_target_before_the_stops();
    for (index, slot) in slots.iter_mut().enumerate() {
        match slot {
            Slot::Attached(collector) => {
                watched[index].closed_ns = closing_ns;
                let mut trailing = collector.stop();
                into_our_ids(
                    spending.serving.get(index),
                    &mut trailing,
                    &mut losses[index],
                );
                events[index].append(&mut trailing);
                let collector_losses = collector.losses();
                losses[index].absorb(collector_losses);
                drains[index].grew_to(losses[index].dropped, closing_ns);
                if let Some(stop) = resolved[index]
                    .request
                    .window
                    .and_then(|window| window.stop)
                {
                    degradations[index].push(format!(
                        "the run ended {ended_s:.3} s in, before this request's window closed at \
                         {stop}: it was watched for less than it asked"
                    ));
                }
            }
            Slot::Waiting(_) if plans[index].held => {
                let layer = resolved[index]
                    .request
                    .window
                    .and_then(|window| window.layer())
                    .map_or_else(String::new, |layer| format!(" `{layer}`"));
                let why = format!(
                    "never attached: the escalation did not name its layer{layer} in the {ended_s:.3} \
                     s the run lasted"
                );
                degradations[index].push(format!("{why}, so it measured nothing and cost nothing"));
                closed_because[index] = Some(why);
                *slot = Slot::Missed;
            }
            Slot::Waiting(_) => {
                let window = resolved[index].request.window.unwrap_or(Window {
                    start: None,
                    stop: None,
                    on: None,
                });
                let why = format!(
                    "never attached: the run ended {ended_s:.3} s in, before this request's \
                     window `{window}` opened"
                );
                degradations[index].push(format!("{why}, so its empty series measured nothing"));
                closed_because[index] = Some(why);
                *slot = Slot::Missed;
            }
            Slot::Detached(_) | Slot::Missed => {}
        }
    }

    // A launched program that outlived the duration is stopped: the tool does
    // not leave behind processes it started. Its tree is read first, the last
    // moment the processes it started can be: what they spent is in nobody's
    // account once they are killed, since whoever inherits them reaps them.
    if let Some(child) = child.as_mut() {
        if child.finished().is_none() {
            spending.read_before_the_stop();
            child.terminate();
        }
        // A program that exited on its own may have left processes running
        // outside its tree -- a daemon's double fork -- which this process
        // adopted: they end with the run as they would at a stop.
        child.end_orphans();
    }

    let closed_ns = monotonic_ns();
    // An attached collector's window closed before the first stop, above; a
    // detached one's at its detach.
    for window in watched.iter_mut().filter(|window| !window.attached) {
        // Never attached: a span of nothing, at the end.
        window.opened_ns = closed_ns;
        window.closed_ns = closed_ns;
    }
    let lifetime = child.as_ref().and_then(|child| child.lifetime());
    // Read once the child has been reaped: `getrusage(RUSAGE_CHILDREN)` is what
    // `time` prints, and it only prints it after the wait.
    spending.stats_lost = program_stats.lost();
    let overheads = spending.close();
    drop(program_stats);
    let recorded = recording.map(|recording| {
        recording.recorded(&stopped_because, closed_ns, budget_interval_ns.unwrap_or(0))
    });
    let mut collections = slots
        .iter()
        .enumerate()
        .map(|(index, slot)| {
            let (value_attr, stacks, collector_notes, by_ring, facts, row_stamp, failure) =
                match slot {
                    Slot::Attached(collector) => (
                        collector.value_attr(),
                        collector.stacks(),
                        collector.notes(),
                        collector.losses_by_ring(),
                        collector.facts(),
                        collector.row_stamp(),
                        collector.failure(),
                    ),
                    Slot::Detached(record) => (
                        record.value_attr,
                        record.stacks.clone(),
                        record.notes.clone(),
                        record.by_ring.clone(),
                        record.facts.clone(),
                        record.row_stamp,
                        record.failure.clone(),
                    ),
                    Slot::Waiting(_) | Slot::Missed => (
                        "",
                        BTreeMap::new(),
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                        None,
                        None,
                    ),
                };
            let fields = slot_fields(slot);
            let provenance = slot_provenance(slot);
            let mut collected = std::mem::take(&mut events[index]);
            collected.sort_by_key(|event| event.ts_ns);
            // The filter is the request's, and a degradation does not change
            // the request -- only how it was served.
            let filtered = apply_filter(&resolved[index], &mut collected);
            let mut notes = std::mem::take(&mut degradations[index]);
            notes.extend(std::mem::take(&mut spending.notes[index]));
            notes.extend(std::mem::take(&mut escalated[index]));
            notes.extend(collector_notes);
            // Everything is in: the runner's counts, what the collector knew it
            // lost, and anything a budget added. The magnitudes become the one
            // reason that carries them now, and not one reason each.
            let mut losses = std::mem::take(&mut losses[index]);
            losses.summarize();
            Collection {
                observed_ns: watched[index].span_ns(),
                value_attr: value_attr.to_owned(),
                events: collected,
                losses,
                stopped_because: closed_because[index]
                    .take()
                    .unwrap_or_else(|| stopped_because.clone()),
                notes,
                stacks,
                filtered,
                served_by: std::mem::take(&mut spending.served_by[index]),
                overhead: overheads[index].clone(),
                lifetime,
                window: watched[index],
                detached: matches!(slot, Slot::Detached(_)),
                by_ring,
                facts,
                row_stamp,
                drains: std::mem::take(&mut drains[index].placed),
                failure,
                fields,
                provenance,
            }
        })
        .collect::<Vec<Collection>>();
    // What the runner spent once each span was weighed, read last so that
    // before, during and after add up to this process's account.
    let handed_back_cpu_ns = rusage_ns(libc::RUSAGE_SELF);
    for collection in &mut collections {
        let overhead = &mut collection.overhead;
        overhead.after_window_cpu_ns = handed_back_cpu_ns
            .saturating_sub(overhead.before_window_cpu_ns + overhead.collector_cpu_ns);
    }
    Ok((collections, recorded))
}

/// The refusal for a collector that observes one process of the several its
/// request named: it would measure the first and drop the rest without a
/// word, which is a number that looks like the whole.
pub(super) fn one_process_of_several(resolved: &ResolvedRequest, named: usize) -> CollectError {
    let target = &resolved.request.target;
    let first = target
        .first_pid()
        .map_or_else(String::new, |pid| format!(" -- only {pid} --"));
    // The call above narrowed the target again; it is this refusal's own.
    Target::narrowed_to_one();
    CollectError::UnsupportedTarget {
        source_id: resolved.request.source.to_string(),
        target: format!(
            "{} (`{}` through `{}` observes one process, and would have measured one \
             of the {named} this request named{first} and dropped the others without saying so; \
             ask for each in a request of its own)",
            target.kind(),
            resolved.request.source,
            resolved.backend.id,
        ),
    }
}

/// Whether a request's window is one its collector and its mode can honour.
///
/// Two things cannot be moved by a window. A collector given to the program at
/// exec -- the preload shim -- exists only from the program's first
/// instruction to its last: it cannot be attached to a program that is already
/// running, and detaching it early would leave the program writing into a
/// channel nobody reads, which blocks it or kills it and in either case
/// changes the thing being measured. And a snapshot is a single read, which
/// can be taken late but has nothing to stop.
pub(super) fn window_honoured(
    resolved: &ResolvedRequest,
    prepared: &dyn Prepared,
) -> Result<Option<String>, CollectError> {
    let Some(window) = resolved.request.window else {
        return Ok(None);
    };
    let refuse = |reason: String, whose: Whose| CollectError::Window {
        source_id: resolved.request.source.to_string(),
        window: window.to_string(),
        reason,
        whose,
    };
    let at_exec = prepared.requires_launch().is_some() || !prepared.launch_environment().is_empty();
    if at_exec {
        // This build's gap rather than the request's: the catalog has
        // backends for these sources -- a uprobe, a USDT probe -- that attach
        // to a running process, and a build that had them could honour this.
        let what = if window.attaches_late() {
            "attached once the program is running"
        } else {
            "detached while the program is still running"
        };
        // Held for the escalation, it is not refused: the session asked for
        // it only in case its layer were named, and the cheap answer does not
        // need it. The escalation says so when it names that layer.
        if window.on.is_some() {
            return Ok(Some(format!(
                "it is served through `{}`, which is given to the program at exec and read \
                 until it exits, so it cannot be {what}{}",
                resolved.backend.id,
                passed_over(resolved),
            )));
        }
        return Err(refuse(
            format!(
                "it is served through `{}`, which is given to the program at exec and read until \
                 it exits, so it cannot be {what}; leave the window out, or force a backend that \
                 attaches to a running process",
                resolved.backend.id
            ),
            Whose::Build,
        ));
    }
    if resolved.request.mode == Mode::Snapshot && window.stop.is_some() {
        return Err(refuse(
            "a snapshot is a single read and has nothing to detach: give it a start and no \
             stop, such as `2s..`"
                .to_owned(),
            Whose::Request,
        ));
    }
    Ok(None)
}

/// What the resolver said of the backends it did not choose, for a request
/// held for a layer it cannot serve: the probes' own reasons, so the reader
/// learns what this machine lacks rather than only that it lacks something.
pub(super) fn passed_over(resolved: &ResolvedRequest) -> String {
    use crate::resolve::Verdict;
    let reasons: Vec<String> = resolved
        .chain
        .iter()
        .filter_map(|considered| match &considered.verdict {
            Verdict::Rejected(why) => Some(format!("`{}`: {}", considered.id, why.join("; "))),
            Verdict::Unimplemented => Some(format!(
                "`{}`: this build has no collector for it",
                considered.id
            )),
            Verdict::Selected | Verdict::NotReached => None,
        })
        .collect();
    if reasons.is_empty() {
        String::new()
    } else {
        format!(
            "; the rest of its chain was passed over -- {}",
            reasons.join(", ")
        )
    }
}

/// Whether a request's window lies inside the run a duration bounds.
///
/// A window that opens or closes after the run has ended would quietly become
/// a shorter one, or none, under the label that asked for more.
pub(super) fn window_inside_the_run(
    resolved: &ResolvedRequest,
    duration_ns: u64,
) -> Result<(), CollectError> {
    let Some(window) = resolved.request.window else {
        return Ok(());
    };
    let past = [window.start, window.stop]
        .into_iter()
        .flatten()
        .find(|end| end.as_nanos() > duration_ns);
    match past {
        Some(end) => Err(CollectError::Window {
            source_id: resolved.request.source.to_string(),
            window: window.to_string(),
            reason: format!(
                "the run lasts at most {}, and this window reaches {end} past its start",
                crate::scalar::Duration::from_nanos(duration_ns)
            ),
            whose: Whose::Request,
        }),
        None => Ok(()),
    }
}

/// The note for a request asked to be sampled and served by a collector with
/// no cadence of its own.
///
/// It is being read at a poll interval nobody asked for. The number that comes
/// back is a real measurement of something; it is simply not a thousand
/// samples a second, and the two are told apart here rather than left to be
/// guessed at from a plausible figure.
pub(super) fn unpaced(
    resolved: &ResolvedRequest,
    collector: &dyn Collector,
    pace: Pace,
) -> Option<String> {
    let mode = resolved.request.mode;
    (matches!(mode, Mode::Sample { .. } | Mode::Trace) && collector.tick().is_none()).then(|| {
        format!(
            "`{}` was asked for in `{mode}` mode and `{}` has no sampling of its own: it was \
             read every {}, which is a poll and not the rate that was requested",
            resolved.request.source,
            resolved.backend.id,
            span_ns(pace.interval.as_nanos() as u64),
        )
    })
}

/// Why a collector must not be attached late to this target, when it must
/// not: it would follow only threads started after it, and the target already
/// has others.
///
/// Asked at the attach rather than before the run, because how many threads a
/// program has by then is the program's business: a single-threaded one is
/// followed whole by inheritance whenever the collector is opened on it.
pub(super) fn blind_to_running_threads(prepared: &dyn Prepared, target: &Target) -> Option<String> {
    let reason = prepared.follows_only_later_threads()?;
    let pid = target.first_pid()?;
    let threads = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?.count();
    (threads > 1).then(|| {
        format!(
            "{reason}, and process {pid} was already running {threads} threads, so a count \
             from here would be of one of them"
        )
    })
}

/// Where one request is in its window.
///
/// A request without a window is attached before the gate and stays
/// `Attached` to the end, which is every request there was before hot attach.
pub(super) enum Slot {
    /// Prepared, and waiting for its window to open.
    Waiting(Box<dyn Prepared>),
    Attached(Box<dyn Collector>),
    /// Its window closed, and the collector with it: what is left is what the
    /// results need of it.
    Detached(Box<Detached>),
    /// Never attached. Why is in the collection's `stopped_because`.
    Missed,
}

/// What a detached collector leaves behind. Taken at the detach, because the
/// collector itself is dropped then: releasing what it held in the kernel is
/// the point of detaching.
pub(super) struct Detached {
    pub(super) value_attr: &'static str,
    pub(super) stacks: BTreeMap<u64, Vec<String>>,
    pub(super) notes: Vec<String>,
    pub(super) by_ring: Vec<RingLoss>,
    pub(super) facts: Vec<String>,
    pub(super) row_stamp: Option<RowStamp>,
    pub(super) failure: Option<String>,
    pub(super) fields: crate::emit::Fields,
    pub(super) provenance: crate::stack::Provenance,
}

/// What a collector's slot says of [`Collector::fields`]: the live
/// collector's count, what a detached one said at its detach, nothing for
/// one that never attached.
pub(super) fn slot_fields(slot: &Slot) -> crate::emit::Fields {
    match slot {
        Slot::Attached(collector) => collector.fields(),
        Slot::Detached(record) => record.fields.clone(),
        Slot::Waiting(_) | Slot::Missed => crate::emit::Fields::new(),
    }
}

/// What a collector's slot says of [`Collector::provenance`], as
/// [`slot_fields`] reads its fields.
pub(super) fn slot_provenance(slot: &Slot) -> crate::stack::Provenance {
    match slot {
        Slot::Attached(collector) => collector.provenance(),
        Slot::Detached(record) => record.provenance.clone(),
        Slot::Waiting(_) | Slot::Missed => crate::stack::Provenance::default(),
    }
}

/// One request's drops placed in time, as the runner reads them: each read
/// that found the collection's total grown adds the growth at its time.
#[derive(Debug, Clone, Default)]
pub(super) struct Drains {
    pub(super) placed: Vec<DrainDrop>,
    pub(super) seen: u64,
}

impl Drains {
    /// The collection has dropped `total` in all by `ts_ns`.
    pub(super) fn grew_to(&mut self, total: u64, ts_ns: u64) {
        if total > self.seen {
            self.placed.push(DrainDrop {
                ts_ns,
                dropped: total - self.seen,
            });
            self.seen = total;
        }
    }
}

/// When one request is attached and detached, in nanoseconds after the run
/// opened.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Plan {
    pub(super) attach_ns: u64,
    pub(super) detach_ns: Option<u64>,
    /// Held for the escalation, which decides both ends.
    pub(super) held: bool,
    /// Held, and this run cannot hold it: why. It is never attached, and the
    /// escalation is told so rather than the run refused, because the cheap
    /// answer does not need it.
    pub(super) unheld: Option<String>,
}

impl Plan {
    pub(super) fn of(window: Option<Window>) -> Plan {
        window.map_or(Plan::default(), |window| Plan {
            attach_ns: window.start_ns(),
            detach_ns: window.stop.map(crate::scalar::Duration::as_nanos),
            held: window.on.is_some(),
            unheld: None,
        })
    }
}

/// The one workload target of a session, if it has one.
///
/// Any number of machine targets may sit beside it, because that is the shape
/// of every confounder check: the machine's state is read at `system` or
/// `cpu`, and the thing it might explain is measured on the program. Two
/// workloads are still refused. They would have to share a launch, a stop
/// condition and an overhead denominator, and each of those is a question with
/// no single answer -- whose exit ends the run is the first of them.
pub(super) fn workload_of(resolved: &[ResolvedRequest]) -> Result<Option<&Target>, CollectError> {
    let mut workload: Option<&Target> = None;
    for resolved in resolved {
        let target = &resolved.request.target;
        if target.is_machine() {
            continue;
        }
        match workload {
            None => workload = Some(target),
            Some(first) if first == target => {}
            Some(first) => {
                return Err(CollectError::MixedTargets {
                    first: first.to_string(),
                    second: target.to_string(),
                })
            }
        }
    }
    Ok(workload)
}

/// Lets collectors of one launch read one channel between them, where they
/// can.
///
/// Each is offered every earlier one in order, and the first that takes it
/// wins; what "can" means is the collector's own answer, in
/// [`Prepared::share_launch`].
pub(super) fn share_launches(prepared: &mut [Box<dyn Prepared>], launching: &[bool]) {
    for later in 1..prepared.len() {
        if !launching[later] {
            continue;
        }
        let (earlier, rest) = prepared.split_at_mut(later);
        for (index, earlier) in earlier.iter_mut().enumerate() {
            if launching[index] && rest[0].share_launch(earlier.as_mut()) {
                break;
            }
        }
    }
}
/// Evaluates the request's filter over what was collected.
///
/// This is the other half of filter placement. The resolver decides where a
/// predicate runs and says so in the resolved file; for a backend that returns
/// whole records, "userspace" means here, after the event has been paid for. A
/// backend that was credited with pushing the predicate down is taken at its
/// word and its events are left alone -- running the filter twice would hide
/// the bug if it had not.
pub(super) fn apply_filter(
    resolved: &ResolvedRequest,
    events: &mut Vec<Event>,
) -> Option<Filtered> {
    let predicate = resolved.request.filter.as_ref()?;
    if !matches!(resolved.filter, Some(FilterPlacement::Userspace(_))) {
        return None;
    }
    let before = events.len();
    let unseen_fields = predicate
        .fields()
        .into_iter()
        .filter(|field| *field != "kind" && !events.iter().any(|event| event.attr(field).is_some()))
        .map(str::to_owned)
        .collect();
    events.retain(|event| crate::filter::matches(predicate, event));
    Some(Filtered {
        predicate: predicate.to_string(),
        kept: events.len() as u64,
        dropped: (before - events.len()) as u64,
        unseen_fields,
    })
}

/// Collects what every collector needs in the launched program's environment,
/// refusing a disagreement rather than letting one win silently.
pub(super) fn merge_environments<'a>(
    prepared: impl IntoIterator<Item = &'a dyn Prepared>,
) -> Result<Vec<(String, String)>, CollectError> {
    let mut merged: Vec<(String, String)> = Vec::new();
    for prepared in prepared {
        for (name, value) in prepared.launch_environment() {
            match merged.iter().find(|(existing, _)| *existing == name) {
                Some((_, existing)) if *existing != value => {
                    return Err(CollectError::ConflictingEnvironment {
                        variable: name,
                        first: existing.clone(),
                        second: value,
                    })
                }
                Some(_) => {}
                None => merged.push((name, value)),
            }
        }
    }
    Ok(merged)
}
