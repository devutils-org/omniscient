//! `omn trace`: a session read, resolved and checked, then measured.

use super::*;

/// Loads the catalog, from a directory when one is named, from the binary
/// otherwise.
pub(crate) fn catalog(explicit: Option<&Path>) -> anyhow::Result<Catalog> {
    let from_env = std::env::var_os("OMN_CATALOG_DIR").map(PathBuf::from);
    let dir = explicit.map(Path::to_path_buf).or(from_env);
    match dir {
        Some(dir) => {
            let catalog = Catalog::load_dir(&dir)?;
            // The unwind modes beside the entries, when the directory has
            // them: a catalog under iteration carries its modes too.
            crate::stack::unwind::use_table_in(&dir).map_err(anyhow::Error::msg)?;
            Ok(catalog)
        }
        None => Ok(Catalog::embedded()?),
    }
}

pub(crate) fn trace(args: TraceArgs) -> anyhow::Result<ExitCode> {
    let catalog = catalog(args.catalog.as_deref())?;
    if let Some(refusal) = crate::lazy::cost_refusal(&args) {
        eprintln!("error: {refusal}");
        return Ok(ExitCode::from(EXIT_REJECTED));
    }
    // A family where a source is expected: its default's band, kept for what
    // the backends turn out to declare once resolved.
    let mut band = None;

    let (session, defaulted) = match &args.file {
        Some(path) => {
            let (mut session, _origin) = session_file::load(path, args.input_format)?;
            // A format given on the command line overrides the file's, so one
            // session file can feed a terminal and a pipeline alike.
            if args.format != OutputFormat::default() {
                session.format = args.format;
            }
            if args.out.is_some() {
                session.out.clone_from(&args.out);
            }
            // And so do the other two things a `Session` holds for the whole of
            // itself. `--duration` used to be accepted here and dropped: the
            // run lasted as long as the file said, and `--dump-request` -- the
            // thing a reader checks when they want to know what they asked for
            // -- carried no duration at all. A flag that leaves no trace is the
            // silent degradation this project refuses everywhere else, so it is
            // an override now and the run says which requests it moved.
            let overridden = override_session_wide(&mut session, &args);
            // `--duration` on a recording replaces its longest duration, and
            // the bounds are asked again against the one that will run: a
            // retention the new duration no longer exceeds is refused as it
            // would be in the file.
            if let Err(error) = session.check_recording() {
                eprintln!("error: {error}");
                return Ok(ExitCode::from(EXIT_REJECTED));
            }
            (session, overridden)
        }
        None => match &args.family {
            Some(family) => {
                let composed = crate::lazy::compose(&args, &catalog, family)?;
                band = Some(composed.band);
                // Said first, before anything is resolved: the long form is
                // what the person reads to learn what was asked for them.
                for note in composed.defaulted.iter() {
                    eprintln!("note: {note}");
                }
                (composed.session, Defaulted::default())
            }
            None => cli::to_session(&args, &catalog)?,
        },
    };

    // A session's `launch` is the run's window, and a request's launch would
    // be a second program: the program is launched once, as one or the
    // other. Refused before anything is asked of the machine.
    if let Some(launched) = session.requests.iter().find(|request| {
        !session.launch.is_empty() && matches!(request.target, crate::request::Target::Launch(_))
    }) {
        eprintln!(
            "error: the session launches `{}` as its window, and `{}` launches a program of its \
             own: a run launches one program, as the session's `launch` or as a request's \
             target",
            session.launch.join(" "),
            launched
                .label
                .clone()
                .unwrap_or_else(|| launched.source.to_string())
        );
        return Ok(ExitCode::from(EXIT_REJECTED));
    }

    let report = match validate::validate_session(&catalog, &session) {
        Ok(report) => report,
        Err(mut rejections) => {
            // A program alone is the target; a source that cannot follow
            // one is measured for its life beside a target it accepts, and
            // the reader is told the spelling rather than left at "cannot".
            if args.file.is_none() && !args.program.is_empty() && session.launch.is_empty() {
                for rejection in &mut rejections {
                    if rejection.message.contains("cannot be measured per launch") {
                        let hint = format!(
                            "to measure it for the program's life, name the target beside the \
                             program: `omn trace {} --system -- {}`",
                            args.source
                                .iter()
                                .map(|source| source.as_str())
                                .collect::<Vec<_>>()
                                .join(","),
                            args.program.join(" ")
                        );
                        rejection.hint = Some(match rejection.hint.take() {
                            Some(first) => format!("{first}\n  hint: {hint}"),
                            None => hint,
                        });
                    }
                }
            }
            return Ok(reject(&rejections));
        }
    };

    // `-r` is refused before the machine is asked anything: a repetition
    // that cannot report a spread, or of a run with no single end, is a
    // request that cannot mean what it asks.
    if let Some(repeat) = args.repeat {
        let refusal = repeat::refusal(&session, repeat).or_else(|| {
            (repeat > 1 && !session.format.renders_documents()).then(|| {
                format!(
                    "-r writes each value's spread beside every run's results, which `{}` cannot \
                     carry: text, json, yaml or toml",
                    session.format
                )
            })
        });
        if let Some(refusal) = refusal {
            eprintln!("error: {refusal}");
            return Ok(ExitCode::from(EXIT_REJECTED));
        }
    }

    // A dump is a document about the request, and a results format cannot
    // hold one. Asked before the machine is, because the answer does not
    // depend on it: a stock kernel that refuses `cpu.profile` used to answer
    // "no viable backend" to a question about the request's spelling.
    if (args.dump_request || args.dump_resolved || args.no_resolve)
        && !session.format.renders_documents()
    {
        eprintln!(
            "error: `{}` describes results, not a request: a request dumps as text, json, yaml \
             or toml",
            session.format
        );
        return Ok(ExitCode::from(EXIT_REJECTED));
    }

    // A session written here for another machine is not resolved here: what
    // this machine can do says nothing about what that one can.
    if args.no_resolve {
        print_session(&catalog, &session, &report, &defaulted, &[])?;
        return Ok(ExitCode::SUCCESS);
    }

    // `--symbols debuginfod` reaches the collectors that name their stacks
    // through the unwinder; on any other source it would be accepted and
    // never asked, which is a debuginfod request silently not made. Refused
    // as this build's gap, before the machine is asked anything.
    if let Some(refusal) = symbols_not_read(&session) {
        eprintln!("error: {refusal}");
        return Ok(ExitCode::from(EXIT_NOT_IMPLEMENTED));
    }

    // Introspection happens once per invocation: every request is resolved
    // against the same view of the machine, so a session cannot straddle two.
    let capabilities = Capabilities::detect();
    let mut resolutions = Vec::new();
    let mut unserved: Vec<collect::Unserved> = Vec::new();
    let mut refused_at_resolution = Vec::new();
    for request in &session.requests {
        // A request held for the escalation (`window: on <layer>`) that this
        // machine cannot serve is not a reason to refuse the run: the session
        // asked for it only in case its layer were named, and the cheap pass
        // does not need it. It is set aside with the resolver's reason, and
        // the escalation says so if it names that layer.
        let held = request.window.is_some_and(|window| window.on.is_some());
        // A container is measured as the cgroup its runtime says it runs in,
        // which every collector that serves a cgroup can attach to. The
        // session keeps the container, as asked; the resolved request carries
        // the cgroup, as measured.
        let bound;
        let request = match &request.target {
            crate::request::Target::Container(name) => match crate::request::find_container(name) {
                Ok(container) => {
                    eprintln!(
                        "note: container `{name}` runs in cgroup {} (pid {}, from `{}`), which \
                         is what is measured",
                        container.cgroup.display(),
                        container.pid,
                        container.runtime
                    );
                    bound = crate::request::Request {
                        target: crate::request::Target::Cgroup(container.cgroup),
                        ..request.clone()
                    };
                    &bound
                }
                Err(reason) => {
                    eprintln!("error: {reason}");
                    said_which(Unavailable::Machine);
                    return Ok(ExitCode::FAILURE);
                }
            },
            _ => request,
        };
        match resolve::resolve(&catalog, &capabilities, request) {
            Ok(resolved) => resolutions.push(resolved),
            Err(error @ resolve::ResolveError::NoViableBackend { .. }) if held => {
                unserved.push(collect::Unserved {
                    request: request.clone(),
                    reason: error.to_string(),
                });
            }
            // Every request is resolved before the run is refused, so that
            // a session of several requests this machine cannot serve names
            // each of them rather than the first.
            Err(error) => refused_at_resolution.push(error),
        }
    }
    if !refused_at_resolution.is_empty() {
        for error in &refused_at_resolution {
            eprintln!("error: {error}");
            // The hint used to say "the request is valid; it is this machine
            // that cannot serve it" under every resolver failure and one line
            // above the code for a request that is *not* valid. Both halves
            // are said per case now, and the code is derived from the same
            // answer the sentence gives.
            if let Some(hint) = resolution_hint(error) {
                eprintln!("  hint: {hint}");
            }
        }
        if refused_at_resolution.len() > 1 {
            eprintln!(
                "error: {} of the session's {} requests cannot be resolved here",
                refused_at_resolution.len(),
                session.requests.len()
            );
        }
        return Ok(ExitCode::from(exit_for(first_to_fix(
            refused_at_resolution.iter().map(resolution_is),
        ))));
    }

    // A composed default's band chooses the backend where the chain has one
    // that keeps it; what still cannot is said.
    if let Some(band) = band {
        for note in crate::lazy::keep_band(&catalog, &capabilities, band, &mut resolutions) {
            eprintln!("note: {note}");
        }
    }

    // Requests whose backends cannot share the one launch they ask for are
    // the invalid combination too: asked once each request's backend is
    // known, since the rule is about the library that will serve it, and
    // before the program is started.
    let refused = launch_rejections(&resolutions);
    if !refused.is_empty() {
        return Ok(reject(&refused));
    }

    // A budget that could move a request onto a mode its emits cannot be
    // continued in is the invalid combination, refused before anything runs
    // and on the dumps alike: the request is what is wrong.
    if let Some(refusal) = resolutions
        .iter()
        .find_map(|resolved| budget_refusal(&catalog, resolved))
    {
        eprintln!("error: {refusal}");
        return Ok(ExitCode::from(EXIT_REJECTED));
    }

    // A budget that silently does nothing is worse than one that is refused,
    // so it is reported before anything is measured -- including on the dumps,
    // where the reader is deciding whether the request says what they meant.
    for note in resolutions
        .iter()
        .filter_map(|resolved| budget_note(&catalog, resolved))
    {
        eprintln!("note: {note}");
    }
    for note in band
        .map(|band| crate::lazy::band_notes(band, &resolutions))
        .unwrap_or_default()
    {
        eprintln!("note: {note}");
    }

    // Only after resolution can the trigger be checked against what the
    // chosen backend carries -- and still before anything is launched.
    if let Err(error) = session.check_trigger_fields(
        &catalog,
        session
            .record
            .as_ref()
            .and_then(|_| session.recorded_index().ok())
            .and_then(|index| resolutions.get(index))
            .map(|resolved| resolved.backend.id.as_str()),
    ) {
        eprintln!("error: {error}");
        return Ok(ExitCode::from(EXIT_REJECTED));
    }

    for one in &unserved {
        eprintln!(
            "note: `{}` is held for the escalation and this machine cannot serve it, so it is \
             set aside and never attached: {}",
            one.request
                .label
                .clone()
                .unwrap_or_else(|| one.request.source.to_string()),
            one.reason
        );
    }

    if args.dump_resolved {
        print_resolved(session.format, &resolutions)?;
        return Ok(ExitCode::SUCCESS);
    }

    if args.dump_request {
        print_session(&catalog, &session, &report, &defaulted, &resolutions)?;
        return Ok(ExitCode::SUCCESS);
    }

    // Every request held for the escalation may be one this machine cannot
    // serve; a run with nothing left to run is still refused, by the resolver.
    if resolutions.is_empty() {
        eprintln!("error: no request of this session can be served on this machine");
        return Ok(ExitCode::from(exit_for(crate::collect::Whose::Machine)));
    }

    if session.record.is_some() {
        return record(
            &catalog,
            &capabilities,
            &session,
            &report,
            &defaulted,
            &resolutions,
        );
    }

    let repeat = args.repeat.unwrap_or(1);
    if repeat > 1 {
        return measure_repeated(
            &catalog,
            &capabilities,
            &session,
            &report,
            &defaulted,
            &resolutions,
            &unserved,
            repeat,
        );
    }
    measure(
        &catalog,
        &capabilities,
        &session,
        &report,
        &defaulted,
        &resolutions,
        &unserved,
    )
}

/// Applies the flags a session file's own fields can be replaced by, and says
/// what was replaced.
///
/// Only the two a [`Session`] carries for the whole of itself. `--format` and
/// `--out` are the same kind of thing and were already handled where the file
/// is loaded; every other axis names one request, which a file may have many
/// of, and the parser refuses those beside `-f` rather than accepting them.
///
/// The replacement reaches the requests as well as the session, because
/// `Session::apply_defaults` has already run by the time the file is loaded:
/// setting the session field alone would leave every request holding the
/// file's value and change nothing, which is how this was silent in the first
/// place.
pub(crate) fn override_session_wide(session: &mut Session, args: &TraceArgs) -> Defaulted {
    let mut overridden = Defaulted::default();
    if let Some(duration) = args.duration {
        let replaced = session
            .requests
            .iter()
            .filter(|request| request.duration.is_some_and(|own| own != duration))
            .count();
        session.duration = Some(duration);
        for request in &mut session.requests {
            request.duration = Some(duration);
        }
        overridden.record(match replaced {
            0 => format!("--duration {duration} applied to every request of this session"),
            replaced => format!(
                "--duration {duration} replaced the duration that {replaced} request(s) of \
                 this session named for themselves"
            ),
        });
    }
    if let Some(budget) = args.budget {
        session.budget = Some(budget);
        for request in &mut session.requests {
            request.budget = Some(budget);
        }
        overridden.record(format!(
            "--budget {budget} applied to every request of this session"
        ));
    }
    overridden
}

/// The first request that asks for debuginfod names from a source whose
/// collector does not read `--symbols` in this build.
pub(crate) fn symbols_not_read(session: &Session) -> Option<String> {
    use crate::stack::unwind::SymbolSource;
    session.requests.iter().find_map(|request| {
        (request.symbols == Some(SymbolSource::Debuginfod)
            && !crate::stack::reads_symbols(request.source.as_str()))
        .then(|| {
            format!(
                "`--symbols debuginfod` is not implemented for `{}`: this build asks debuginfod \
                 only for the stacks of {}, and this source's are named from the files the \
                 target mapped. Leave `--symbols` out, or ask for it where it is read",
                request.source,
                crate::stack::symbol_readers()
                    .iter()
                    .map(|source| format!("`{source}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    })
}

/// Whose gap a resolver failure is.
///
/// `NoViableBackend` is the one that moved. Every backend of the chain was
/// probed against this machine and every one of them was rejected for something
/// this machine does not have -- a file, a counter, a capability. The request
/// itself is untouched and will run unchanged on a machine that has them, which
/// is a different answer from "this cannot mean anything" and needs a different
/// code: a CI harness running a suite of sessions across a fleet has to tell
/// *fix your session* from *this is the wrong machine for it*, and those need
/// opposite responses.
///
/// The others stay where they were. A source that is not in the catalog and a
/// backend that is not in its chain are misspellings; a filter field no backend
/// carries is a request that cannot be served anywhere, and the one the
/// selected backend does not carry names the `--backend force:` that would --
/// which is a change to the request, not to the machine. A key the selected
/// backend does not attach is whose the backend that would attach it says:
/// see [`resolve::KeyElsewhere::whose`].
/// Whose gap a session refused several ways is: the one the reader has to
/// deal with first -- a request to rewrite, then a build that lacks a part,
/// then a machine -- as [`collect::CollectError::whose`] orders them.
pub(crate) fn first_to_fix(
    each: impl Iterator<Item = crate::collect::Whose>,
) -> crate::collect::Whose {
    use crate::collect::Whose;
    each.min_by_key(|whose| match whose {
        Whose::Request => 0,
        Whose::Build => 1,
        Whose::Machine => 2,
    })
    .unwrap_or(Whose::Machine)
}

pub(crate) fn resolution_is(error: &resolve::ResolveError) -> crate::collect::Whose {
    use crate::collect::Whose;
    match error {
        resolve::ResolveError::NoViableBackend { .. } => Whose::Machine,
        // Whose it is depends on the backend that would attach the key: one
        // the request can be pinned to, one this machine cannot run, or one
        // this build has no collector for.
        resolve::ResolveError::KeyNotCarried {
            elsewhere: Some(elsewhere),
            ..
        } => elsewhere.whose(),
        // A filter field, the same way: the backend that carries it says.
        resolve::ResolveError::FilterFieldNotCarried {
            elsewhere: Some(elsewhere),
            ..
        } => elsewhere.whose(),
        resolve::ResolveError::StackRefused { whose, .. } => *whose,
        _ => Whose::Request,
    }
}

/// What to do about a resolver failure, in one line, said per case.
pub(crate) fn resolution_hint(error: &resolve::ResolveError) -> Option<String> {
    match error {
        resolve::ResolveError::NoViableBackend { .. } => {
            let mut hint = "the request is valid; it is this machine that cannot serve it. \
                            Use --no-resolve to write it down for one that can."
                .to_owned();
            if kernel_access_refused(&error.to_string()) {
                hint.push_str(KERNEL_ACCESS_HINT);
            }
            Some(hint)
        }
        resolve::ResolveError::UnknownSource { .. } => {
            Some("`omn list` names every source the catalog carries.".to_owned())
        }
        resolve::ResolveError::UnknownBackend { source_id, .. } => Some(format!(
            "`omn list {source_id}` names the backends of its chain."
        )),
        resolve::ResolveError::FilterFieldNotCarried { .. } => None,
        // The message names the backend that would attach it, and either the
        // `--backend force:` that asks for it or what keeps it out of reach.
        resolve::ResolveError::KeyNotCarried { .. } => None,
        // The reason is the unwind table's own, and says what it lacks.
        resolve::ResolveError::StackRefused { .. } => None,
        resolve::ResolveError::NoBackendDeclares { source_id, .. } => Some(format!(
            "no machine could serve this; `omn list {source_id}` says which target and mode \
             each backend serves."
        )),
    }
}

/// What to read when a refusal comes from the kernel's settings for tracing
/// rather than from a missing feature: atfq's card on them, with this
/// machine's values (devutils' atfq, a separate tool).
pub(crate) const KERNEL_ACCESS_HINT: &str =
    " The kernel settings and capabilities behind such a refusal \
     (perf_event_paranoid, CAP_PERFMON, CAP_BPF, lockdown, BTF, memlock) are explained, against \
     this machine, by `atfq kernel.tracing`.";

/// Whether a refusal names one of those settings.
pub(crate) fn kernel_access_refused(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    [
        "perf_event_paranoid",
        "cap_perfmon",
        "cap_bpf",
        "lockdown",
        "btf",
        "memlock",
        "unprivileged_bpf_disabled",
        "kptr_restrict",
    ]
    .iter()
    .any(|word| reason.contains(word))
}
