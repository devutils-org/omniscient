//! `omn top`, live or from a recording.

use super::*;

// ---------------------------------------------------------------------------
// `omn top`
//
// The view is a projection, never a collector. Live, it builds a session --
// the sources its views need, or a session file's -- and runs it through the
// same resolver, ladder and runner as `omn trace`; between rounds a `Tap`
// hands what the runner already holds to the drawing thread. Nothing in the
// collectors changes for it, and a run without `omn top` never reaches here.

/// The longest a live `omn top` runs when no `--duration` is given.
pub(crate) const TOP_DEFAULT_DURATION: &str = "10m";

pub(crate) fn top(args: TopArgs) -> anyhow::Result<ExitCode> {
    let catalog = catalog(args.catalog.as_deref())?;
    if let Some(name) = &args.view {
        let names: Vec<&str> = omn_view::views::registry()
            .iter()
            .map(|view| view.name())
            .collect();
        if !names.contains(&name.as_str()) {
            eprintln!(
                "error: there is no view `{name}`; the views are {}",
                names.join(", ")
            );
            return Ok(ExitCode::from(EXIT_REJECTED));
        }
    }
    if args.against.is_some() && args.replay.is_none() && args.gallery.is_none() {
        eprintln!("error: --against compares two recordings, and needs --replay or --gallery");
        return Ok(ExitCode::from(EXIT_REJECTED));
    }
    match args.replay.as_ref().or(args.gallery.as_ref()) {
        Some(path) => top_replay(&catalog, &args, path),
        None => top_live(&catalog, &args),
    }
}

/// What the catalog says of each source a run names, which a result file
/// does not carry.
pub(crate) fn view_facts(catalog: &Catalog, data: &mut omn_view::model::Data) {
    let sources: Vec<String> = data
        .run
        .results
        .iter()
        .map(|results| results.source.clone())
        .chain(data.refused.iter().map(|refusal| refusal.source.clone()))
        .collect();
    for source in sources {
        if data.facts.contains_key(&source) {
            continue;
        }
        let Ok(id) = source.parse::<crate::request::SourceId>() else {
            continue;
        };
        if let Some(entry) = catalog.get(&id) {
            data.facts.insert(
                source,
                omn_view::model::SourceFacts {
                    determinism: entry.determinism.to_string(),
                    summary: entry.summary.clone(),
                },
            );
        }
    }
}

pub(crate) fn top_load(catalog: &Catalog, path: &Path) -> anyhow::Result<omn_view::model::Data> {
    let mut data = omn_view::model::Data::load(path).map_err(|error| anyhow::anyhow!("{error}"))?;
    view_facts(catalog, &mut data);
    Ok(data)
}

pub(crate) fn top_app(
    args: &TopArgs,
    data: omn_view::model::Data,
) -> anyhow::Result<omn_view::app::App> {
    let mut app = omn_view::app::App::new(data);
    app.gallery = args.gallery.is_some();
    if let Some(name) = &args.view {
        app.open(name).map_err(|error| anyhow::anyhow!(error))?;
    }
    Ok(app)
}

pub(crate) fn top_size(args: &TopArgs) -> anyhow::Result<(u16, u16)> {
    args.size
        .split_once('x')
        .and_then(|(columns, rows)| Some((columns.parse().ok()?, rows.parse().ok()?)))
        .filter(|(columns, rows): &(u16, u16)| *columns >= 40 && *rows >= 10)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "--size is COLUMNSxROWS, at least 40x10, such as 120x40; `{}` is not",
                args.size
            )
        })
}

pub(crate) fn top_replay(
    catalog: &Catalog,
    args: &TopArgs,
    path: &Path,
) -> anyhow::Result<ExitCode> {
    let data = top_load(catalog, path)?;
    let against = args
        .against
        .as_deref()
        .map(|path| top_load(catalog, path))
        .transpose()?;
    if args.batch {
        let table = omn_view::views::system::batch(&data, &args.sort)
            .map_err(|error| anyhow::anyhow!(error))?;
        std::io::stdout().lock().write_all(table.as_bytes())?;
        return Ok(ExitCode::SUCCESS);
    }
    if args.headless {
        let (columns, rows) = top_size(args)?;
        let names: Vec<String> = match (&args.gallery, &args.view) {
            (Some(_), None) => omn_view::views::registry()
                .iter()
                .map(|view| view.name().to_owned())
                .collect(),
            (_, Some(name)) => vec![name.clone()],
            (None, None) => vec![omn_view::views::registry()[0].name().to_owned()],
        };
        let mut out = std::io::stdout().lock();
        for name in names {
            let mut app = top_app(args, data.clone())?;
            app.against.clone_from(&against);
            app.cost = omn_view::app::Cost::Snapshot;
            app.open(&name).map_err(|error| anyhow::anyhow!(error))?;
            let buffer = omn_view::snapshot::draw_app(&mut app, columns, rows);
            write!(out, "{}", omn_view::snapshot::text(&buffer))?;
            writeln!(out)?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    let mut app = top_app(args, data)?;
    app.against = against;
    let screen = match omn_view::term::Screen::open() {
        Ok(screen) => screen,
        Err(error) => {
            eprintln!("error: {}", omn_view::term::no_screen(&error));
            return Ok(ExitCode::from(EXIT_REJECTED));
        }
    };
    let quit = std::sync::atomic::AtomicBool::new(false);
    screen.run(app, None, &quit, None)?;
    Ok(ExitCode::SUCCESS)
}

/// The session `omn top` runs live: a file's, or every view's needs on the
/// target given.
pub(crate) fn top_session(
    catalog: &Catalog,
    args: &TopArgs,
) -> anyhow::Result<(Session, Vec<omn_view::model::Refusal>, Vec<String>)> {
    let mut notes = Vec::new();
    let duration = match args.duration {
        Some(duration) => duration,
        None => {
            notes.push(format!(
                "the live run lasts at most {TOP_DEFAULT_DURATION} (--duration), or until the \
                 view is quit or the launched program exits"
            ));
            TOP_DEFAULT_DURATION.parse()?
        }
    };
    let mut refused = Vec::new();
    if let Some(path) = &args.file {
        let (mut session, _origin) = session_file::load(path, None)?;
        // The view watches the runner through the one hook an escalation
        // also needs, and a recording's documents are cut from the run the
        // view would be drawing: either is run with `omn trace -f`, and its
        // output replayed.
        if session.escalate.is_some() || session.record.is_some() {
            anyhow::bail!(
                "{} escalates or records, which `omn top` cannot watch live: run it with `omn \
                 trace -f {} --format json -o DIR`, then `omn top --replay DIR`",
                path.display(),
                path.display()
            );
        }
        // `--duration` moves every request, as on `omn trace -f`; without
        // it the file's own durations hold, and only a session that names
        // none takes the view's default.
        let given = args.duration.is_some();
        if given || session.duration.is_none() {
            session.duration = Some(duration);
        } else {
            // The file says how long: the default's note does not apply.
            notes.clear();
        }
        for request in &mut session.requests {
            if given || request.duration.is_none() {
                request.duration = session.duration;
            }
        }
        return Ok((session, refused, notes));
    }
    let target = if !args.program.is_empty() {
        crate::request::Target::Launch(args.program.clone())
    } else if !args.pid.is_empty() {
        crate::request::Target::Pid(args.pid.clone())
    } else if let Some(cgroup) = &args.cgroup {
        crate::request::Target::Cgroup(cgroup.clone())
    } else {
        crate::request::Target::System
    };
    let mut requests = Vec::new();
    for need in omn_view::views::all_needs().map_err(|why| anyhow::anyhow!(why))? {
        let refuse = |reason: String| omn_view::model::Refusal {
            label: need.label.clone(),
            source: need.source.to_owned(),
            reason,
        };
        let built = (|| -> Result<crate::request::Request, String> {
            Ok(crate::request::Request {
                source: need.source.parse().map_err(|error| format!("{error}"))?,
                target: match need.scope {
                    omn_view::views::Scope::Target => target.clone(),
                    omn_view::views::Scope::System => crate::request::Target::System,
                },
                mode: need.mode.parse().map_err(|error| format!("{error}"))?,
                emit: need
                    .emit
                    .iter()
                    .map(|emit| emit.parse().map_err(|error| format!("{error}")))
                    .collect::<Result<_, _>>()?,
                keys: need
                    .keys
                    .iter()
                    .map(|key| key.parse().map_err(|error| format!("{error}")))
                    .collect::<Result<_, _>>()?,
                filter: None,
                duration: Some(duration),
                budget: None,
                stack: need
                    .stack
                    .map(|stack| stack.parse().map_err(|error| format!("{error}")))
                    .transpose()?,
                symbols: None,
                backend: None,
                label: Some(need.label.clone()),
                window: None,
                value: None,
                sum: Vec::new(),
            })
        })();
        let request = match built {
            Ok(request) => request,
            Err(reason) => {
                refused.push(refuse(reason));
                continue;
            }
        };
        // Each need is asked alone: a view that asks for something the
        // catalog refuses loses that panel, not the run.
        if let Err(rejections) = validate::validate_request(catalog, &request, OutputFormat::Json) {
            let reason = rejections
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ");
            refused.push(refuse(reason));
            continue;
        }
        requests.push(request);
    }
    let session = Session {
        name: Some("omn top".to_owned()),
        duration: Some(duration),
        budget: None,
        out: args.out.clone(),
        format: OutputFormat::Json,
        requests,
        joins: Vec::new(),
        decompose: Vec::new(),
        escalate: None,
        record: None,
        launch: Vec::new(),
    };
    Ok((session, refused, notes))
}

pub(crate) fn top_live(catalog: &Catalog, args: &TopArgs) -> anyhow::Result<ExitCode> {
    if args.headless && !args.batch && args.out.is_none() && !args.dump_session {
        eprintln!(
            "error: a headless live run draws nothing, so it needs --out <dir> to write what it \
             collects"
        );
        return Ok(ExitCode::from(EXIT_REJECTED));
    }
    let (session, mut refused, notes) = top_session(catalog, args)?;
    if args.dump_session {
        print_triage_session(OutputFormat::Yaml, &session)?;
        for refusal in &refused {
            eprintln!("note: `{}` is not asked: {}", refusal.label, refusal.reason);
        }
        return Ok(ExitCode::SUCCESS);
    }
    for note in &notes {
        eprintln!("note: {note}");
    }
    let file_has_joins = !session.joins.is_empty() || !session.decompose.is_empty();
    if args.file.is_some() {
        if let Err(rejections) = validate::validate_session(catalog, &session) {
            return Ok(reject(&rejections));
        }
    }
    let capabilities = Capabilities::detect();
    let mut resolutions = Vec::new();
    for request in &session.requests {
        match resolve::resolve(catalog, &capabilities, request) {
            // Resolved onto a backend this build has no collector for: the
            // runner would refuse it in these words before anything starts.
            Ok(resolved)
                if !file_has_joins
                    && !collect::implements(
                        &resolved.request.source.to_string(),
                        &resolved.backend.id,
                    ) =>
            {
                refused.push(omn_view::model::Refusal {
                    label: request
                        .label
                        .clone()
                        .unwrap_or_else(|| request.source.to_string()),
                    source: request.source.to_string(),
                    reason: collect::CollectError::NoCollector {
                        source_id: resolved.request.source.to_string(),
                        backend: resolved.backend.id.clone(),
                    }
                    .to_string(),
                })
            }
            Ok(resolved) => resolutions.push(resolved),
            // A session file's joins and decompositions name its requests,
            // and one set aside would leave them naming nothing: such a
            // session is refused whole, as `omn trace` refuses it.
            Err(error) if file_has_joins => {
                eprintln!("error: {error}");
                return Ok(ExitCode::from(exit_for(resolution_is(&error))));
            }
            Err(error) => refused.push(omn_view::model::Refusal {
                label: request
                    .label
                    .clone()
                    .unwrap_or_else(|| request.source.to_string()),
                source: request.source.to_string(),
                reason: error.to_string(),
            }),
        }
    }
    if resolutions.is_empty() {
        eprintln!("error: no source the view needs can be served on this machine:");
        for refusal in &refused {
            eprintln!("  {}: {}", refusal.label, refusal.reason);
        }
        return Ok(ExitCode::from(exit_for(crate::collect::Whose::Machine)));
    }
    let launch_refused = launch_rejections(&resolutions);
    if !launch_refused.is_empty() {
        return Ok(reject(&launch_refused));
    }
    // The session as it runs: the requests that resolved.
    let mut running = session.clone();
    running.requests = resolutions
        .iter()
        .map(|resolved| resolved.request.clone())
        .collect();

    let origin = match (&args.file, args.program.is_empty(), args.pid.is_empty()) {
        (Some(path), _, _) => format!("live: {}", path.display()),
        (None, false, _) => format!("live: {}", args.program.join(" ")),
        (None, true, false) => format!(
            "live: pid {}",
            args.pid
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
        (None, true, true) => match &args.cgroup {
            Some(cgroup) => format!("live: cgroup {}", cgroup.display()),
            None => "live: the machine".to_owned(),
        },
    };
    let mut opening = omn_view::model::Data {
        origin: origin.clone(),
        refused: refused.clone(),
        live: Some(omn_view::model::Live {
            opened_ns: crate::clock::monotonic_ns(),
            now_ns: crate::clock::monotonic_ns(),
        }),
        ..Default::default()
    };
    opening.run.results = resolutions
        .iter()
        .map(|resolved| omn_view::model::Results {
            label: resolved
                .request
                .label
                .clone()
                .unwrap_or_else(|| resolved.request.source.to_string()),
            source: resolved.request.source.to_string(),
            backend: resolved.backend.id.clone(),
            degradations: resolved.degradations.clone(),
            ..Default::default()
        })
        .collect();
    view_facts(catalog, &mut opening);

    // The terminal is taken before anything is collected: a run with no
    // terminal to draw on is refused now, not after its whole duration.
    let screen = if args.headless || args.batch {
        None
    } else {
        match omn_view::term::Screen::open() {
            Ok(screen) => Some(screen),
            Err(error) => {
                eprintln!("error: {}", omn_view::term::no_screen(&error));
                return Ok(ExitCode::from(EXIT_REJECTED));
            }
        }
    };
    let quit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handover = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (sender, receiver) = std::sync::mpsc::channel();
    let drawing = match screen {
        None => {
            drop(receiver);
            None
        }
        Some(screen) => {
            // A target and no `--view`: the system view for that one process
            // is the first screen; the machine's is, without one.
            let start_on = args.view.clone().or_else(|| {
                (args.file.is_none() && (!args.program.is_empty() || !args.pid.is_empty()))
                    .then(|| "process".to_owned())
            });
            let opening = opening.clone();
            let quit = std::sync::Arc::clone(&quit);
            let handover = std::sync::Arc::clone(&handover);
            Some(std::thread::spawn(move || -> std::io::Result<()> {
                // The app is built on the thread that draws it: its views are
                // not shared with the runner.
                let mut app = omn_view::app::App::new(opening);
                if let Some(name) = start_on {
                    let _ = app.open(&name);
                }
                screen.run(app, Some(receiver), &quit, Some(handover))
            }))
        }
    };
    // What the view spends -- the drawing thread, and the hand-over on the
    // runner's thread -- is the view's, and is taken out of the collectors'.
    let mut view_cpu = drawing
        .as_ref()
        .map(|drawing| ViewCpu::new(drawing, std::sync::Arc::clone(&handover)));

    // A source the runner refuses before the run starts -- no collector in
    // this build, a target it cannot watch, a start or an attach that failed
    // -- is set aside with the runner's reason and the rest are asked again,
    // unless a session file's joins name it. A launched program may already
    // be forked when an attach refuses (`net.request` on a program that
    // declares no request), but it is still parked at its gate: the runner
    // drops it unreleased, which kills it there, before its exec
    // (`launch::Gated`'s drop), so asking again runs the program once.
    let (collections, ladder) = loop {
        let mut ladder = Ladder::new(catalog, &capabilities, &resolutions);
        let mut tap = Tap::new(
            opening.clone(),
            sender.clone(),
            std::sync::Arc::clone(&quit),
            view_cpu.as_mut(),
        );
        match collect::Run::new(&resolutions)
            .cheaper(&mut ladder)
            .escalation(Some(&mut tap))
            .go()
        {
            Ok(ran) => break (ran.collections, ladder),
            Err(error) => {
                let named: Option<Vec<(String, String)>> = error
                    .each()
                    .iter()
                    .map(|one| match one {
                        collect::CollectError::NoCollector { source_id, .. }
                        | collect::CollectError::UnsupportedTarget { source_id, .. }
                        | collect::CollectError::Start { source_id, .. }
                        | collect::CollectError::Window { source_id, .. } => {
                            Some((source_id.clone(), one.to_string()))
                        }
                        _ => None,
                    })
                    .collect();
                let set_aside = named.filter(|named| {
                    !file_has_joins
                        && !named.is_empty()
                        && resolutions.iter().any(|resolved| {
                            !named
                                .iter()
                                .any(|(source, _)| *source == resolved.request.source.to_string())
                        })
                });
                let Some(named) = set_aside else {
                    quit.store(true, std::sync::atomic::Ordering::SeqCst);
                    let _ = sender.send(omn_view::feed::Update::Abort);
                    drop(sender);
                    if let Some(drawing) = drawing {
                        let _ = drawing.join();
                    }
                    return Ok(refused_at_collect(catalog, &error, &resolutions));
                };
                let mut kept = Vec::new();
                let mut kept_results = Vec::new();
                for (resolved, shell) in resolutions.into_iter().zip(opening.run.results.drain(..))
                {
                    match named
                        .iter()
                        .find(|(source, _)| *source == resolved.request.source.to_string())
                    {
                        Some((_, reason)) => refused.push(omn_view::model::Refusal {
                            label: shell.label.clone(),
                            source: shell.source.clone(),
                            reason: reason.clone(),
                        }),
                        None => {
                            kept.push(resolved);
                            kept_results.push(shell);
                        }
                    }
                }
                resolutions = kept;
                opening.run.results = kept_results;
                opening.refused.clone_from(&refused);
                running.requests = resolutions
                    .iter()
                    .map(|resolved| resolved.request.clone())
                    .collect();
                // The view was drawing the requests as they stood; it is told
                // what was set aside by starting it over.
                let _ = sender.send(omn_view::feed::Update::Restart(Box::new(opening.clone())));
            }
        }
    };
    let mut collections = collections;
    // What the view spent inside each collection's window is not the
    // collection's: `getrusage` charges the whole process to every one.
    let view_spent = view_cpu.as_mut().map(|view_cpu| {
        view_cpu.sample();
        for collection in &mut collections {
            view_cpu.take_out(&mut collection.overhead);
        }
        view_cpu.spent()
    });
    // Each result's worst time error, from its events before they were
    // aggregated: a summary, a timeline or a histogram does not carry it.
    let ts_err_ns: BTreeMap<String, u64> = resolutions
        .iter()
        .zip(&collections)
        .map(|(resolved, collection)| {
            (
                resolved
                    .request
                    .label
                    .clone()
                    .unwrap_or_else(|| resolved.request.source.to_string()),
                collection
                    .events
                    .iter()
                    .map(|event| u64::from(event.ts_err_ns))
                    .max()
                    .unwrap_or(0),
            )
        })
        .collect();
    // A collector that broke the program it measured fails the live run as it
    // fails `measure`'s, once the screen is given back and everything else is
    // said and written.
    let failures = failures(&collections);
    let assembled = assemble(catalog, &running, &ladder, &resolutions, collections).and_then(
        |(run, costs, served_by)| {
            let value = serde_json::to_value(&run)?;
            let ended = omn_view::model::Data::from_json(value)?;
            Ok((run, costs, served_by, ended))
        },
    );
    let (run, costs, served_by, mut ended) = match assembled {
        Ok(assembled) => assembled,
        Err(error) => {
            // The screen is given back before the error is said on it.
            let _ = sender.send(omn_view::feed::Update::Abort);
            drop(sender);
            if let Some(drawing) = drawing {
                let _ = drawing.join();
            }
            return Err(error);
        }
    };
    ended.origin = origin.clone();
    ended.refused = refused.clone();
    view_facts(catalog, &mut ended);
    let beside = omn_view::model::Beside {
        origin,
        facts: ended.facts.clone(),
        refused: refused.clone(),
        ts_err_ns,
        view_cpu_ns: view_spent.map(|(drawing, handover)| drawing + handover),
        capacity: Some(omn_view::model::MachineCapacity::here()),
    };
    beside.apply_ts_err(&mut ended);
    let table = args
        .batch
        .then(|| omn_view::views::system::batch(&ended, &args.sort));
    let _ = sender.send(omn_view::feed::Update::Ended(Box::new(ended)));
    drop(sender);
    if let Some(drawing) = drawing {
        match drawing.join() {
            Ok(result) => result?,
            Err(_) => anyhow::bail!("the view's thread panicked"),
        }
    }

    report_costs(&costs, &run);
    if let Some((drawing, handover)) = view_spent {
        eprintln!(
            "the view: {} of cpu drawing and {} handing the events over, taken out of every \
             collector's figure above",
            span(drawing),
            span(handover)
        );
    }
    for refusal in &refused {
        eprintln!(
            "note: `{}` was not served: {}",
            refusal.label, refusal.reason
        );
    }
    if let Some(directory) = &args.out {
        let output = encode::run_output_bytes(OutputFormat::Json, &run)?;
        write_artifacts(directory, OutputFormat::Json, &served_by, &costs, &output)?;
        std::fs::write(
            directory.join(omn_view::model::BESIDE),
            serde_json::to_string_pretty(&beside)?,
        )?;
        eprintln!(
            "\nwrote results, the resolved requests, what they cost and the view's {} to {}: \
             `omn top --replay {}` draws it again",
            omn_view::model::BESIDE,
            directory.display(),
            directory.display()
        );
    }
    if let Some(table) = table {
        let table = table.map_err(|error| anyhow::anyhow!(error))?;
        std::io::stdout().lock().write_all(table.as_bytes())?;
    }
    Ok(outcome(&failures))
}

/// Hands what the runner holds to the drawing thread, between rounds.
///
/// An [`collect::Escalate`] that never attaches anything: it is the one place
/// the runner shows a run in progress, and the view only reads. When the user
/// quits, it lets every request go, which ends the run the way a closed
/// window does -- and each collection says it was detached because the view
/// was quit.
pub(crate) struct Tap<'a> {
    /// What each request has handed over already.
    pub(crate) handover: collect::live::Handover,
    sender: std::sync::mpsc::Sender<omn_view::feed::Update>,
    quit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    last: Option<std::time::Instant>,
    /// Set once the drawing thread is gone, or from the start when there is
    /// none (`--headless`): nothing more is converted.
    unheard: bool,
    /// What the view spends, kept for taking it out of the collectors'.
    view_cpu: Option<&'a mut ViewCpu>,
    /// The view's opening data, which it is given again with what the
    /// collectors say of their events ([`collect::Described`]) for as long as
    /// no event was handed over: a restart then loses nothing.
    shell: omn_view::model::Data,
}

impl<'a> Tap<'a> {
    /// The least time between two hand-overs: the screen redraws at 4 Hz, and
    /// converting events more often would be the view's cost for nothing.
    const EVERY: std::time::Duration = std::time::Duration::from_millis(200);

    pub(crate) fn new(
        shell: omn_view::model::Data,
        sender: std::sync::mpsc::Sender<omn_view::feed::Update>,
        quit: std::sync::Arc<std::sync::atomic::AtomicBool>,
        view_cpu: Option<&'a mut ViewCpu>,
    ) -> Self {
        Self {
            handover: collect::live::Handover::new(shell.run.results.len()),
            sender,
            quit,
            last: None,
            unheard: view_cpu.is_none(),
            view_cpu,
            shell,
        }
    }

    /// What the collectors say, set on the opening data: the indices whose
    /// value attribute, row stamp or drains changed.
    fn describe(&mut self, said: &[collect::Described<'_>]) -> Vec<usize> {
        let mut changed = Vec::new();
        for (index, (results, said)) in self.shell.run.results.iter_mut().zip(said).enumerate() {
            let value_attr = (!said.value_attr.is_empty()).then(|| said.value_attr.to_owned());
            let row_stamp = said.row_stamp.and_then(|stamp| {
                serde_json::to_value(stamp)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
            });
            let drains: Vec<omn_view::model::Drain> = said
                .drains
                .iter()
                .map(|drain| omn_view::model::Drain {
                    ts_ns: drain.ts_ns,
                    dropped: drain.dropped,
                })
                .collect();
            if results.value_attr != value_attr
                || results.row_stamp != row_stamp
                || results.drains != drains
            {
                results.value_attr = value_attr;
                results.row_stamp = row_stamp;
                results.drains = drains;
                changed.push(index);
            }
        }
        changed
    }

    /// What the collectors say, as the updates that carry it: the opening
    /// data again while nothing was handed over, and one
    /// `Update::Described` per changed result after, which keeps the events
    /// already drawing. A source attached after the first events, or drops
    /// placed after them, used to reach the view only at the end.
    pub(crate) fn described_updates(
        &mut self,
        said: &[collect::Described<'_>],
    ) -> Vec<omn_view::feed::Update> {
        let changed = self.describe(said);
        if changed.is_empty() {
            return Vec::new();
        }
        if !self.handover.any() {
            return vec![omn_view::feed::Update::Restart(Box::new(
                self.shell.clone(),
            ))];
        }
        changed
            .into_iter()
            .map(|index| {
                let results = &self.shell.run.results[index];
                omn_view::feed::Update::Described {
                    index,
                    value_attr: results.value_attr.clone(),
                    row_stamp: results.row_stamp.clone(),
                    drains: results.drains.clone(),
                }
            })
            .collect()
    }

    /// Hands the events read since the last hand-over to the drawing thread.
    fn hand_over(&mut self, run: &collect::Progress<'_>) {
        for (index, fresh) in self.handover.fresh(run) {
            let (converted, unconverted) = to_view_events(fresh);
            let update = omn_view::feed::Update::Events {
                index,
                events: converted,
                now_ns: run.now_ns,
                unconverted,
            };
            if self.sender.send(update).is_err() {
                self.unheard = true;
                return;
            }
        }
        if self
            .sender
            .send(omn_view::feed::Update::Tick { now_ns: run.now_ns })
            .is_err()
        {
            self.unheard = true;
        }
    }
}

/// `omn`'s events as the view's copy of their type, through their JSON. An
/// event that does not convert -- a float attribute that is not a number,
/// which JSON writes as `null` -- is counted, with the first reason, rather
/// than taking the rest of its batch with it.
pub(crate) fn to_view_events(
    events: &[crate::event::Event],
) -> (Vec<omn_view::model::Event>, Option<(u64, String)>) {
    let convert = |events: &[crate::event::Event]| {
        serde_json::to_value(events).and_then(serde_json::from_value::<Vec<omn_view::model::Event>>)
    };
    if let Ok(converted) = convert(events) {
        return (converted, None);
    }
    let mut converted = Vec::with_capacity(events.len());
    let mut unconverted: Option<(u64, String)> = None;
    for event in events {
        match convert(std::slice::from_ref(event)) {
            Ok(mut one) => converted.append(&mut one),
            Err(error) => {
                let held = unconverted.get_or_insert_with(|| (0, error.to_string()));
                held.0 += 1;
            }
        }
    }
    (converted, unconverted)
}

impl collect::Escalate for Tap<'_> {
    fn described(&mut self, said: &[collect::Described<'_>]) {
        if self.unheard {
            return;
        }
        for update in self.described_updates(said) {
            if self.sender.send(update).is_err() {
                self.unheard = true;
                return;
            }
        }
    }

    fn decide(&mut self, run: &collect::Progress<'_>) -> Vec<collect::Switch> {
        if self.quit.load(std::sync::atomic::Ordering::SeqCst) {
            return collect::live::detach_all(run, "the view was quit");
        }
        if self.unheard || self.last.is_some_and(|last| last.elapsed() < Self::EVERY) {
            return Vec::new();
        }
        self.last = Some(std::time::Instant::now());
        let opened = collect::thread_cpu_ns();
        self.hand_over(run);
        if let Some(view_cpu) = self.view_cpu.as_mut() {
            view_cpu.sample();
            view_cpu.handed_over(collect::thread_cpu_ns().saturating_sub(opened));
        }
        Vec::new()
    }

    fn shed(&mut self, _index: usize, _over: &str) -> Option<String> {
        None
    }

    fn close(&mut self, _run: &collect::Progress<'_>) -> Vec<(usize, String)> {
        Vec::new()
    }
}

/// What the view of a live run spends, and when, so that it can be taken out
/// of the collectors' figures.
///
/// Every collection's `collector_cpu_ns` is `getrusage(RUSAGE_SELF)` over its
/// window: the whole process, the drawing thread and the hand-over of events
/// included. The view's cpu is sampled against the process's own at every
/// hand-over, so the part of it inside a window -- whose bounds are process
/// cpu readings, `before_window_cpu_ns` and the window's end -- is found by
/// interpolating between samples, and taken out.
pub(crate) struct ViewCpu {
    /// The drawing thread's cpu clock (`pthread_getcpuclockid`), readable
    /// from this thread.
    pub(crate) drawing: Option<libc::clockid_t>,
    /// The hand-over's cpu on the runner's thread, a running total the
    /// drawing thread adds to its own figure.
    pub(crate) handover: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The drawing thread's last reading: once it has ended its clock reads
    /// nothing, and what it spent stays spent.
    pub(crate) drawn_ns: u64,
    /// (this process's cpu, the view's cpu so far), in the order taken.
    pub(crate) samples: Vec<(u64, u64)>,
}

impl ViewCpu {
    pub(crate) fn new(
        drawing: &std::thread::JoinHandle<std::io::Result<()>>,
        handover: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        use std::os::unix::thread::JoinHandleExt;
        let mut clock: libc::clockid_t = 0;
        // SAFETY: the thread is joined only after this value's last use, so
        // its handle names a thread that has not been reaped.
        let status = unsafe {
            libc::pthread_getcpuclockid(drawing.as_pthread_t() as libc::pthread_t, &mut clock)
        };
        let mut view_cpu = Self {
            drawing: (status == 0).then_some(clock),
            handover,
            drawn_ns: 0,
            samples: Vec::new(),
        };
        view_cpu.sample();
        view_cpu
    }

    fn handed_over(&mut self, ns: u64) {
        self.handover
            .fetch_add(ns, std::sync::atomic::Ordering::Relaxed);
    }

    /// What the view has spent so far: drawing, and handing over.
    fn spent(&mut self) -> (u64, u64) {
        if let Some(clock) = self.drawing {
            if let Some(now) = clock_ns(clock) {
                self.drawn_ns = self.drawn_ns.max(now);
            }
        }
        (
            self.drawn_ns,
            self.handover.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    fn sample(&mut self) {
        let (drawing, handover) = self.spent();
        self.samples
            .push((collect::process_cpu_ns(), drawing + handover));
    }

    /// The view's cpu when this process had spent `process_ns`.
    pub(crate) fn at(&self, process_ns: u64) -> u64 {
        let after = self.samples.partition_point(|(at, _)| *at <= process_ns);
        match (
            after.checked_sub(1).map(|one| self.samples[one]),
            self.samples.get(after),
        ) {
            (None, Some((_, first))) => {
                // Before the first sample: no more than the view had spent
                // by then, and taken as all of it, which leaves the
                // collectors the smaller figure only by what the view spent
                // before the window.
                *first
            }
            (Some((_, held)), None) => held,
            (Some((from, low)), Some((to, high))) => {
                let span = to.saturating_sub(from).max(1);
                low + ((high.saturating_sub(low)) as u128 * (process_ns - from) as u128
                    / span as u128) as u64
            }
            (None, None) => 0,
        }
    }

    /// Takes what the view spent out of one collection's account: its
    /// window's, and before and after it, so that the three still add up to
    /// what this process spent, less the view.
    pub(crate) fn take_out(&self, overhead: &mut collect::Overhead) {
        let opened = overhead.before_window_cpu_ns;
        let closed = opened + overhead.collector_cpu_ns;
        let handed = closed + overhead.after_window_cpu_ns;
        let before = self.at(opened).min(overhead.before_window_cpu_ns);
        let inside = self
            .at(closed)
            .saturating_sub(self.at(opened))
            .min(overhead.collector_cpu_ns);
        let after = self
            .at(handed)
            .saturating_sub(self.at(closed))
            .min(overhead.after_window_cpu_ns);
        if overhead.collector_cpu_ns > 0 {
            overhead.attributed_cpu_ns -= (overhead.attributed_cpu_ns as u128 * inside as u128
                / overhead.collector_cpu_ns as u128)
                as u64;
        }
        overhead.collector_cpu_ns -= inside;
        overhead.before_window_cpu_ns -= before;
        overhead.after_window_cpu_ns -= after;
    }
}

pub(crate) fn clock_ns(clock: libc::clockid_t) -> Option<u64> {
    let mut spec = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `spec` is a valid timespec the call writes into.
    (unsafe { libc::clock_gettime(clock, &mut spec) } == 0)
        .then(|| spec.tv_sec as u64 * 1_000_000_000 + spec.tv_nsec as u64)
}
