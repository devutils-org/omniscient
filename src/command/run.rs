//! A resolved session run, recorded and written: the steps `omn trace` and
//! `omn top` share.

use super::*;

/// Says so when the document about to be written shares its stdout with the
/// program being measured.
///
/// A launched program inherits this process's stdout, which is right: its
/// output is the user's and swallowing it would be worse. But a structured
/// format is one document, and anything the target prints lands in the middle
/// of it -- so `omn ... --format json -- ./prog | jq` works exactly until
/// `./prog` says something, and the failure arrives as a parse error in
/// whatever consumed the pipe rather than as anything about this tool.
///
/// The way out already exists and the run already names it; what was missing
/// was being told before rather than after. Text is left alone: it is a report
/// for a person, who can see what came from where.
pub(crate) fn shared_stdout_warning(session: &Session) -> Option<String> {
    // Text is left out although it "renders documents": it is a report for a
    // person, who can see what came from where, and it is the format every run
    // launching a program in a terminal uses.
    let structured = session.format != OutputFormat::Text;
    let launches = !session.launch.is_empty()
        || session
            .requests
            .iter()
            .any(|request| matches!(request.target, crate::request::Target::Launch(_)));
    (structured && launches && session.out.is_none()).then(|| {
        format!(
            "`{}` is one document and the launched program writes to the same stdout, so \
             anything it prints lands inside it; pass `-o DIR` to write results.{} beside the \
             resolved request instead, or redirect the program's output",
            session.format,
            session.format.extension(),
        )
    })
}

/// Prints a refusal from the collect layer and answers with its own code.
///
/// The four answers the tool has are one table now, in [`exit_for`], and the
/// collect layer names which of them it is giving. What is left here is the
/// part a person reads: why the better backends were passed over, and what this
/// build does have.
pub(crate) fn refused_at_collect(
    catalog: &Catalog,
    error: &collect::CollectError,
    resolved: &[ResolvedRequest],
) -> ExitCode {
    use crate::collect::Whose;

    let refusals = error.each();
    // A chain that ends on a backend nobody wrote is refused for that
    // backend, and the backend the reader can do something about is one of
    // those passed over on the way: led with first, since the first line is
    // the one read (`docs/open-findings.md`, the fourteenth wave, stream D).
    let leads: Vec<Option<PassedOver>> = refusals
        .iter()
        .map(|refusal| match refusal {
            collect::CollectError::NoCollector { source_id, backend } => {
                passed_over_first(catalog, source_id, backend, resolved)
            }
            _ => None,
        })
        .collect();
    let lines: Vec<String> = refusals
        .iter()
        .zip(&leads)
        .map(|(refusal, lead)| match lead {
            Some(lead) => lead.sentence(),
            None => refusal.to_string(),
        })
        .collect();
    eprintln!("error: {}", lines.join("\n"));

    // The selected backend is often the last one standing, and why the better
    // ones were passed over is the answer the reader actually needs: a counter
    // that needs root reads as "no collector for `pmu`" unless the chain's own
    // verdicts come with it.
    if refusals
        .iter()
        .any(|refusal| matches!(refusal, collect::CollectError::NoCollector { .. }))
    {
        for resolved in resolved {
            for considered in &resolved.chain {
                let led = leads.iter().flatten().any(|lead| {
                    lead.source == resolved.request.source.as_str() && lead.backend == considered.id
                });
                if led {
                    continue;
                }
                if let crate::resolve::Verdict::Rejected(reasons) = &considered.verdict {
                    eprintln!(
                        "  `{}` was passed over: {}",
                        considered.id,
                        reasons.join("; ")
                    );
                }
            }
        }
        eprintln!(
            "\nCollectors exist for: {}.",
            collect::implemented().join(", ")
        );
    }

    // A refusal whose every line leads with a backend this build does collect
    // is not the build's gap: that backend attaches on this build wherever its
    // requirement is met, so what is missing is this machine's.
    let machine = leads.iter().all(Option::is_some);
    if machine {
        eprintln!(
            "\nhint: the request is valid, and this build collects it: it is this machine that \
             lacks what the first line names."
        );
        return ExitCode::from(exit_for(Whose::Machine));
    }

    // Only the build's own gap gets a line here. A refusal against a real
    // target has already said what went wrong in its own words, and a second
    // sentence promising the request is fine would be a guess about a message
    // this layer did not write.
    if error.whose() == Whose::Build {
        eprintln!(
            "\nThe request itself is valid -- `--dump-request` shows how it would be \
             obtained on a build that has it."
        );
    }
    ExitCode::from(exit_for(error.whose()))
}

/// A backend passed over on the way to one no collector is written for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PassedOver {
    source: String,
    /// The backend passed over.
    backend: String,
    reasons: Vec<String>,
    /// The backend the chain ended on, which has none.
    unwritten: String,
}

impl PassedOver {
    fn sentence(&self) -> String {
        format!(
            "`{}` has no viable backend on this machine: `{}`, which this build collects, was \
             passed over -- {}; `{}`, the backend left, has no collector written yet",
            self.source,
            self.backend,
            self.reasons.join("; "),
            self.unwritten
        )
    }
}

/// The backend to lead a no-collector refusal with: of those the chain passed
/// over, the first this build collects and that serves the request's target
/// and mode -- the one a reader can make work. `None` when there is none, and
/// the refusal is the build's alone.
pub(crate) fn passed_over_first(
    catalog: &Catalog,
    source: &str,
    unwritten: &str,
    resolved: &[ResolvedRequest],
) -> Option<PassedOver> {
    let resolved = resolved.iter().find(|resolved| {
        resolved.request.source.as_str() == source && resolved.backend.id == unwritten
    })?;
    let rejected: Vec<(&str, &Vec<String>)> = resolved
        .chain
        .iter()
        .filter_map(|considered| match &considered.verdict {
            crate::resolve::Verdict::Rejected(reasons) => Some((considered.id.as_str(), reasons)),
            _ => None,
        })
        .collect();
    // Only a backend this build collects, passed over for what this machine
    // lacks rather than for a target or a mode it cannot serve anywhere: the
    // one line the reader can act on. Anything else keeps the build's own
    // sentence first, which is then the true one.
    let entry = catalog.get(&resolved.request.source)?;
    let (backend, reasons) = rejected.iter().find(|(id, _)| {
        collect::implements(source, id)
            && entry
                .backend(id)
                .is_some_and(|backend| resolve::serves(entry, backend, &resolved.request))
    })?;
    Some(PassedOver {
        source: source.to_owned(),
        backend: (*backend).to_owned(),
        reasons: (*reasons).clone(),
        unwritten: unwritten.to_owned(),
    })
}

/// Runs the session's requests over one window and prints what they produced.
pub(crate) fn measure(
    catalog: &Catalog,
    capabilities: &Capabilities,
    session: &Session,
    report: &Report,
    defaulted: &Defaulted,
    resolved: &[ResolvedRequest],
    unserved: &[collect::Unserved],
) -> anyhow::Result<ExitCode> {
    announce(session, report, defaulted);

    let mut ladder = Ladder::new(catalog, capabilities, resolved);
    let bound = (!session.launch.is_empty()).then_some(session.launch.as_slice());
    let collections = match collect::run_all_bounded(resolved, &mut ladder, unserved, bound) {
        Ok(collections) => collections,
        Err(error) => return Ok(refused_at_collect(catalog, &error, resolved)),
    };
    RUN_STARTED.store(true, std::sync::atomic::Ordering::SeqCst);
    let failures = failures(&collections);
    attached_late(resolved, &collections);
    let (run, costs, served_by) = assemble(catalog, session, &ladder, resolved, collections)?;

    // Bytes rather than a `String`, because one of the nine formats is not
    // text: a `pprof` profile is a gzipped protobuf. Everything else is UTF-8
    // and reaches stdout unchanged.
    let output = encode::run_output_bytes(session.format, &run)?;
    let unshowable = session.format.is_binary() && std::io::stdout().is_terminal();
    if unshowable && session.out.is_none() {
        anyhow::bail!(
            "`{}` is {}, which no terminal can show: redirect it to a file or pass --out <dir>",
            session.format,
            session.format.describe()
        );
    }
    if !unshowable {
        std::io::stdout().write_all(&output)?;
    }

    report_costs(&costs, &run);
    for caveat in report.caveats() {
        eprintln!("caveat: {caveat}");
    }

    if let Some(directory) = &session.out {
        write_artifacts(directory, session.format, &served_by, &costs, &output)?;
        eprintln!(
            "\nwrote results, the resolved requests and what they cost to {}",
            directory.display()
        );
    }

    Ok(outcome(&failures))
}

/// `-r N`: the session run `repeat` times, one after another, each run a
/// launch of its own, and each summary value's mean and spread across them
/// ([`repeat::spread`]). Every run's losses, degradations and costs are
/// said on stderr under its `run i of N:` line, and the text spread ends
/// naming the runs that lost anything; every run's results are kept in the document, and under
/// `-o` in a directory of their own, `run-<i>`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn measure_repeated(
    catalog: &Catalog,
    capabilities: &Capabilities,
    session: &Session,
    report: &Report,
    defaulted: &Defaulted,
    resolved: &[ResolvedRequest],
    unserved: &[collect::Unserved],
    repeat: u32,
) -> anyhow::Result<ExitCode> {
    announce(session, report, defaulted);
    let bound = (!session.launch.is_empty()).then_some(session.launch.as_slice());
    let mut runs = Vec::with_capacity(repeat as usize);
    let mut broke = Vec::new();
    for index in 1..=repeat {
        let mut ladder = Ladder::new(catalog, capabilities, resolved);
        let collections = match collect::run_all_bounded(resolved, &mut ladder, unserved, bound) {
            Ok(collections) => collections,
            Err(error) => {
                eprintln!("error: run {index} of {repeat} could not run");
                if index > 1 {
                    let written = match &session.out {
                        Some(directory) => {
                            format!(", and wrote theirs to {}/run-<i>", directory.display())
                        }
                        None => String::new(),
                    };
                    eprintln!(
                        "the {} run(s) before it measured{written}; no spread was taken",
                        index - 1
                    );
                }
                let code = refused_at_collect(catalog, &error, resolved);
                if index == 1 {
                    return Ok(code);
                }
                // Whoever's gap the refusal names, the session already ran:
                // what failed is the run, not the request or the build.
                said_which(Unavailable::FailedUnwritten);
                return Ok(ExitCode::from(EXIT_UNAVAILABLE));
            }
        };
        RUN_STARTED.store(true, std::sync::atomic::Ordering::SeqCst);
        for failure in failures(&collections) {
            if !broke.contains(&failure) {
                broke.push(failure);
            }
        }
        attached_late(resolved, &collections);
        let (run, costs, served_by) = assemble(catalog, session, &ladder, resolved, collections)?;
        eprintln!("run {index} of {repeat}:");
        eprint!("{}", repeat::losses(&run));
        report_costs(&costs, &run);
        if let Some(directory) = &session.out {
            let output = encode::run_output_bytes(session.format, &run)?;
            write_artifacts(
                &directory.join(format!("run-{index}")),
                session.format,
                &served_by,
                &costs,
                &output,
            )?;
        }
        runs.push(run);
    }
    // The requests whose program each run launched anew, by the label their
    // results carry.
    let launched: Vec<String> = resolved
        .iter()
        .zip(&runs[0].results)
        .filter(|(resolved, _)| {
            matches!(resolved.request.target, crate::request::Target::Launch(_))
        })
        .map(|(_, results)| results.label.clone())
        .collect();
    let spread = repeat::spread(&runs, &launched);
    let document = repeat::Repeated {
        repeat,
        spread: &spread,
        runs: &runs,
    };
    let output = match session.format {
        OutputFormat::Text => repeat::text(repeat, &spread, &repeat::lossy(&runs)),
        format => format.serialize_document(&document)?,
    };
    print!("{output}");
    for caveat in report.caveats() {
        eprintln!("caveat: {caveat}");
    }
    if let Some(directory) = &session.out {
        std::fs::create_dir_all(directory)?;
        std::fs::write(
            directory.join("repeat.json"),
            serde_json::to_string_pretty(&document)?,
        )?;
        eprintln!(
            "\nwrote each run's results to {0}/run-<i>, and the spread across them to \
             {0}/repeat.json",
            directory.display()
        );
    }
    Ok(outcome(&broke))
}

/// What the collectors said they broke ([`collect::Collection::failure`]),
/// each sentence once however many collections of one collector carried it.
pub(crate) fn failures(collections: &[collect::Collection]) -> Vec<String> {
    let mut failures: Vec<String> = Vec::new();
    for failure in collections.iter().filter_map(|c| c.failure.as_ref()) {
        if !failures.contains(failure) {
            failures.push(failure.clone());
        }
    }
    failures
}

/// The run's exit code once everything it produced is written: 1 with each
/// sentence on stderr when a collector broke the program it measured, so a
/// script does not read the tool's failure as a clean run. The results stand
/// beside it -- what was read before the break was read.
pub(crate) fn outcome(failures: &[String]) -> ExitCode {
    for failure in failures {
        eprintln!("error: {failure}");
    }
    if failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        said_which(Unavailable::FailedWritten);
        ExitCode::FAILURE
    }
}

/// What is said on stderr before any run: the defaults taken, the
/// validator's warnings, and a stdout the program would share.
pub(crate) fn announce(session: &Session, report: &Report, defaulted: &Defaulted) {
    for note in defaulted.iter() {
        eprintln!("note: {note}");
    }
    for warning in report.warnings() {
        eprintln!("warning: {warning}");
    }
    if let Some(warning) = shared_stdout_warning(session) {
        eprintln!("warning: {warning}");
    }
}

/// A recording: a session whose duration is until its trigger fires, as many
/// times as its bound on documents allows, writing one document per firing as
/// it fires.
///
/// Each document is the run output of one window -- the retention's span when
/// the trigger fired -- with the trigger beside it, written the moment it is
/// cut: a recorder whose documents waited for the end of the run would lose
/// every one of them to a crash, which is when they are wanted. With `-o DIR`
/// they are `trigger-<n>.<ext>` beside `resolved.json`, `cost.json` and
/// `recording.json`, the record of every trigger and of what was read and is
/// in no document; without it they go to stdout one after another, which only
/// a stream of text or `ndjson` can carry.
pub(crate) fn record(
    catalog: &Catalog,
    capabilities: &Capabilities,
    session: &Session,
    report: &Report,
    defaulted: &Defaulted,
    resolved: &[ResolvedRequest],
) -> anyhow::Result<ExitCode> {
    use std::cell::RefCell;

    let Some(asked) = session.record.as_ref() else {
        anyhow::bail!("a recording needs a `record:` block");
    };
    let one_stream = matches!(session.format, OutputFormat::Text | OutputFormat::Ndjson);
    if session.out.is_none() && !one_stream {
        eprintln!(
            "error: a recording writes one document per trigger, and `{}` is one document to a \
             stream: pass `-o DIR` to write each as its own file, or use `text` or `ndjson` on \
             stdout",
            session.format
        );
        return Ok(ExitCode::from(EXIT_REJECTED));
    }
    let on = match session.recorded_index() {
        Ok(on) => on,
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(ExitCode::from(EXIT_REJECTED));
        }
    };
    let Some(trigger) = asked.trigger.clone() else {
        anyhow::bail!("a recording needs a trigger");
    };
    announce(session, report, defaulted);
    if let Some(directory) = &session.out {
        std::fs::create_dir_all(directory)?;
    }

    // The ladder is the budget's, and the documents need it too: which
    // backend was serving when a document was cut is its record. One of the
    // two at a time, never both, so a cell is enough.
    let ladder = RefCell::new(Ladder::new(catalog, capabilities, resolved));
    let mut sink = |document: collect::Document| -> Result<(), String> {
        let number = document.trigger.number;
        let (run, _, _) = assemble(
            catalog,
            session,
            &ladder.borrow(),
            resolved,
            document.collections,
        )
        .map_err(|error| error.to_string())?;
        let bytes = encode::triggered_bytes(session.format, &document.trigger, &run)
            .map_err(|error| error.to_string())?;
        match &session.out {
            Some(directory) => {
                let path =
                    directory.join(format!("trigger-{number}.{}", session.format.extension()));
                std::fs::write(&path, &bytes)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                eprintln!(
                    "recording: document {number} written to {}, `{}` fired on `{}`",
                    path.display(),
                    document.trigger.predicate,
                    document.trigger.on
                );
            }
            None => {
                let mut stdout = std::io::stdout();
                stdout
                    .write_all(&bytes)
                    .and_then(|()| stdout.flush())
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    };
    let mut recorder = collect::Recorder {
        on,
        trigger,
        retain_ns: asked.retain_ns(),
        after_ns: asked.after_ns(),
        documents: asked.documents.unwrap_or(1),
        sink: &mut sink,
    };
    let mut cheaper = Shared(&ladder);
    let bound = (!session.launch.is_empty()).then_some(session.launch.as_slice());
    let (collections, recorded) =
        match collect::record_bounded(resolved, &mut cheaper, &mut recorder, bound) {
            Ok(done) => done,
            Err(error) => return Ok(refused_at_collect(catalog, &error, resolved)),
        };
    RUN_STARTED.store(true, std::sync::atomic::Ordering::SeqCst);
    drop(recorder);
    let failures = failures(&collections);

    let costs: Vec<MeasuredCost> = collections
        .iter()
        .enumerate()
        .map(|(index, collection)| {
            let serving = ladder
                .borrow()
                .record(index, &collection.served_by, &resolved[index]);
            let mut cost = measured_cost(catalog, &serving, collection);
            cost.mode = ladder
                .borrow()
                .modes(index, &collection.served_by, &resolved[index]);
            // Weighed over a rolling span, so the whole window's figure is
            // not what held or broke the budget: the run's own stop is.
            cost.weighed_over_ns = Some(recorded.budget_interval_ns);
            if let Some(ceiling) = cost.budget_percent {
                let over = over_budget(&recorded.stopped_because, &cost.source, ceiling);
                cost.within_budget = Some(!over);
                cost.stopped_by_budget = over;
                if over {
                    cost.stopped_because = recorded.stopped_because.clone();
                }
            }
            cost
        })
        .collect();
    report_budgets(&costs);
    eprint!("{}", encode::recorded_text(&recorded));
    for caveat in report.caveats() {
        eprintln!("caveat: {caveat}");
    }

    if let Some(directory) = &session.out {
        let served_by: Vec<ResolvedRequest> = collections
            .iter()
            .enumerate()
            .map(|(index, collection)| {
                ladder
                    .borrow()
                    .record(index, &collection.served_by, &resolved[index])
            })
            .collect();
        std::fs::write(
            directory.join("resolved.json"),
            serde_json::to_string_pretty(&served_by)?,
        )?;
        std::fs::write(
            directory.join("cost.json"),
            serde_json::to_string_pretty(&costs)?,
        )?;
        std::fs::write(
            directory.join("recording.json"),
            serde_json::to_string_pretty(&recorded)?,
        )?;
        eprintln!(
            "\nwrote {} document(s), the resolved requests, what they cost and the recording's \
             record to {}",
            recorded.documents.len(),
            directory.display()
        );
    }
    Ok(outcome(&failures))
}

/// Turns a run's collections into its output: every request's results, the
/// joins across them and the decompositions, with what each cost and the
/// request that was serving it when it ended.
pub(crate) fn assemble(
    catalog: &Catalog,
    session: &Session,
    ladder: &Ladder<'_>,
    resolved: &[ResolvedRequest],
    collections: Vec<collect::Collection>,
) -> anyhow::Result<(encode::RunOutput, Vec<MeasuredCost>, Vec<ResolvedRequest>)> {
    // Every source's events go into one stream for the joins: they were
    // collected over one window against one target, which is what makes them
    // comparable in the first place.
    let mut merged = Vec::new();
    // Where each collection ends in `merged`: the joins walk them apart, in the
    // engine's order, with no index of the whole run (`join::apply_merging`).
    let mut bounds = vec![0];
    // Only the sources a join or a decomposition reads: every other
    // collection's events are consumed by `aggregate` and never cloned.
    let read = join::sources_read(&session.joins, &session.decompose);
    let mut run = encode::RunOutput {
        results: Vec::new(),
        joins: Vec::new(),
        decompositions: Vec::new(),
        machine: encode::machine_report(),
    };
    let mut costs: Vec<MeasuredCost> = Vec::new();
    let mut served_by: Vec<ResolvedRequest> = Vec::new();

    // The sources a detach ended, which is not the same as a window that
    // closed a little before another's.
    let detached: std::collections::BTreeSet<String> = collections
        .iter()
        .zip(resolved)
        .filter(|(collection, _)| collection.detached)
        .map(|(_, resolved)| resolved.request.source.to_string())
        .collect();
    for (index, collection) in collections.into_iter().enumerate() {
        // The record of how these events were obtained is the request that was
        // serving when they arrived, which a budget may have moved. Handing
        // `aggregate` the file written before the run would name a backend that
        // stopped producing half way through it.
        let serving = ladder.record(index, &collection.served_by, &resolved[index]);
        let modes = ladder.modes(index, &collection.served_by, &resolved[index]);
        let mut serving = serving;
        if let Some(note) = unweighed_after_a_mode_move(&resolved[index], &serving, &collection) {
            serving.degradations.push(note);
        }
        // A run the budget stopped with a mode rung it would not take says
        // why that rung was not one, beside the reason it stopped.
        if collection.stopped_because.contains("nothing cheaper than") {
            if let Some(why) = unweighed_mode_rungs(catalog, &serving) {
                serving.degradations.push(format!(
                    "the budget stopped the run, and the ladder's cheaper mode, {why}"
                ));
            }
        }
        if read.contains(resolved[index].request.source.as_str()) {
            merged.extend(collection.events.iter().cloned());
        }
        bounds.push(merged.len());
        let unit = catalog
            .get(&serving.request.source)
            .map(|entry| entry.backend_unit(&serving.backend.id))
            .unwrap_or(crate::catalog::Unit::Count);
        let value_attr = collection.value_attr.clone();
        let mut cost = measured_cost(catalog, &serving, &collection);
        cost.mode = modes;
        costs.push(cost);
        run.results
            .push(emit::aggregate(&serving, collection, unit, &value_attr)?);
        served_by.push(serving);
    }

    // When each source was watching, for the joins across a window. Two
    // requests of one source are merged by their union: the validator has
    // refused a join from a source whose requests asked for different
    // windows, so what is left differs only by when each was attached.
    let mut windows: BTreeMap<String, collect::Watched> = BTreeMap::new();
    for results in &run.results {
        windows
            .entry(results.source.clone())
            .and_modify(
                |watched| match (watched.attached, results.window.attached) {
                    (_, false) => {}
                    (false, true) => *watched = results.window,
                    (true, true) => {
                        watched.opened_ns = watched.opened_ns.min(results.window.opened_ns);
                        watched.closed_ns = watched.closed_ns.max(results.window.closed_ns);
                    }
                },
            )
            .or_insert(results.window);
    }
    // A window that closed with the run rather than at a detach closed before
    // the collectors were stopped, and what their stops drained is later than
    // it: to a join such a source watched to the end of the run, and its
    // states still open then are open, not ended by a detach.
    for (source, watched) in windows.iter_mut() {
        if watched.attached && !detached.contains(source) {
            watched.closed_ns = u64::MAX;
        }
    }
    let streams: Vec<&[crate::event::Event]> =
        bounds.windows(2).map(|at| &merged[at[0]..at[1]]).collect();
    // A query-tier rule is applied to the stored run (`omn join`), not here.
    for rule in session
        .joins
        .iter()
        .filter(|rule| rule.tier == join::Tier::Agent)
    {
        run.joins
            .push(join::apply_merging(rule, &streams, &windows)?);
    }
    // The served requests rather than the session's: a budget can lower a
    // sampling rate, and a sample stands for the period it was taken at.
    let served: Vec<crate::request::Request> = served_by
        .iter()
        .map(|serving| serving.request.clone())
        .collect();
    for decomposition in &session.decompose {
        // With the session's joins: a part a join defines (`value: paired`)
        // names the join by id.
        run.decompositions.push(join::decompose_within(
            decomposition,
            &served,
            &run.results,
            &merged,
            &session.joins,
            &windows,
        ));
    }
    Ok((run, costs, served_by))
}

/// Says so when a series moved from one mode to another and none of its events
/// says what it stands for.
///
/// The emits that survive a move between modes weigh each event by its
/// `weight` ([`crate::request::EmitKind::weighs_each_event`]). A collector whose
/// sampled events carry none has each of them count once where it stood for a
/// period, and the totals from the move on are short by that factor -- the one
/// thing about the move the events themselves can show, so it is read off
/// them rather than assumed.
pub(crate) fn unweighed_after_a_mode_move(
    original: &ResolvedRequest,
    serving: &ResolvedRequest,
    collection: &collect::Collection,
) -> Option<String> {
    if original.request.mode.kind() == serving.request.mode.kind() {
        return None;
    }
    let weighed = collection.events.iter().any(|event| {
        event
            .attr(crate::emit::WEIGHT_ATTR)
            .and_then(|weight| weight.as_f64())
            .is_some_and(|weight| weight > 1.0)
    });
    (!weighed).then(|| {
        format!(
            "the budget moved this collection from `{}` to `{}`, and none of its {} events \
             carries a `{}`: from the move on each counts once where it stood for its period, \
             so every total here is short by that factor after the move",
            original.request.mode,
            serving.request.mode,
            collection.events.len(),
            crate::emit::WEIGHT_ATTR
        )
    })
}

/// Writes the numbers and, beside them, the record of how they were obtained.
///
/// The resolved requests are always written, in JSON, whatever the results
/// format is: results without them are numbers whose provenance is gone.
pub(crate) fn write_artifacts(
    directory: &Path,
    format: OutputFormat,
    resolved: &[ResolvedRequest],
    costs: &[MeasuredCost],
    encoded: &[u8],
) -> anyhow::Result<()> {
    std::fs::create_dir_all(directory)?;
    std::fs::write(
        directory.join("resolved.json"),
        serde_json::to_string_pretty(resolved)?,
    )?;
    // Always JSON, like `resolved.json` and for the same reason: what a
    // measurement cost is part of its provenance, and a reader comparing two
    // runs needs it in a shape a script can read.
    std::fs::write(
        directory.join("cost.json"),
        serde_json::to_string_pretty(costs)?,
    )?;
    // The extension comes from the format rather than from a match here,
    // because one of the nine is a gzipped protobuf whose bytes are not a
    // `String` and whose extension is not its own name.
    std::fs::write(
        directory.join(format!("results.{}", format.extension())),
        encoded,
    )?;
    Ok(())
}

pub(crate) fn print_resolved(
    format: OutputFormat,
    resolutions: &[ResolvedRequest],
) -> anyhow::Result<()> {
    match format {
        OutputFormat::Text => {
            for resolved in resolutions {
                print!("{}", serde_json::to_string_pretty(resolved)?);
                println!();
            }
        }
        format if format.renders_documents() => {
            print!("{}", format.serialize_document(&resolutions.to_vec())?)
        }
        format => anyhow::bail!("`{format}` describes results, not a resolved request"),
    }
    Ok(())
}

pub(crate) fn print_session(
    catalog: &Catalog,
    session: &Session,
    report: &Report,
    defaulted: &Defaulted,
    resolutions: &[ResolvedRequest],
) -> anyhow::Result<()> {
    match session.format {
        OutputFormat::Text => {
            for note in defaulted.iter() {
                eprintln!("note: {note}");
            }
            print!("{}", render::session(catalog, session, report, resolutions));
        }
        format if format.renders_documents() => {
            print!("{}", format.serialize_document(session)?);
            for note in defaulted.iter() {
                eprintln!("note: {note}");
            }
            for warning in report.warnings() {
                eprintln!("warning: {warning}");
            }
            for caveat in report.caveats() {
                eprintln!("caveat: {caveat}");
            }
        }
        format => {
            let phase = match format.status() {
                FormatStatus::Planned { phase } => phase,
                FormatStatus::Ready => "a later phase",
            };
            anyhow::bail!(
                "`{format}` describes results, and results arrive in {phase}; \
                 a request dumps as text, json, yaml or toml"
            );
        }
    }
    Ok(())
}

/// The resolved requests that cannot share their launch with the others,
/// as the request's error: a family's rule about the library that will
/// serve each one (`collect::gpu::launch_refusals`), so it is asked of the
/// backends the resolver chose rather than of the sources' names.
pub(crate) fn launch_rejections(resolutions: &[ResolvedRequest]) -> Vec<Rejection> {
    let pairs: Vec<(&crate::request::Request, &str)> = resolutions
        .iter()
        .map(|resolved| (&resolved.request, resolved.backend.id.as_str()))
        .collect();
    collect::gpu::launch_refusals(&pairs)
        .into_iter()
        .map(|(index, reason)| {
            let request = &resolutions[index].request;
            Rejection {
                request: Some(
                    request
                        .label
                        .clone()
                        .unwrap_or_else(|| format!("{} #{}", request.source, index + 1)),
                ),
                message: reason,
                hint: None,
            }
        })
        .collect()
}

pub(crate) fn reject(rejections: &[Rejection]) -> ExitCode {
    eprint!("{}", render::rejections(rejections));
    ExitCode::from(EXIT_REJECTED)
}
