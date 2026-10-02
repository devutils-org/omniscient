//! `omn`: the command line entry point.
//!
//! Everything here is dispatch and exit codes. The work lives in the library,
//! which is also what a future API would call.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use serde::Serialize;

use omn::capabilities::Capabilities;
use omn::catalog::Catalog;
use omn::cli::{
    Cli, Command, Defaulted, DiffArgs, FormatsArgs, LatencyArgs, ListArgs, SaturateArgs, TraceArgs,
    ValidateArgs, SUBCOMMANDS,
};
use omn::format::{FormatStatus, OutputFormat};
use omn::request::Session;
use omn::resolve::{self, ResolvedRequest};
use omn::validate::{Rejection, Report};
use omn::{collect, emit, encode, join, render, session as session_file, validate};

mod cli {
    pub use omn::cli::*;
}

/// Something went wrong while running -- a counter that would not open, a
/// target that went away, a file that could not be written -- or this machine
/// cannot serve the request at all.
///
/// One code because they are one answer: *this run cannot happen here*. Which
/// of them it was is in the message; what a caller does next -- try another
/// machine, or stop -- is the same either way, and is not what it does about
/// the other two codes.
const EXIT_UNAVAILABLE: u8 = 1;
/// The request was refused: it cannot mean what it asks for.
const EXIT_REJECTED: u8 = 2;
/// The request is valid, but the part of the tool that would answer it does not
/// exist yet.
const EXIT_NOT_IMPLEMENTED: u8 = 3;

/// The exit code for a refusal, from whose gap it is.
///
/// One function, so that the three answers cannot drift apart again: they did,
/// and nine families found it independently. `man omn.1` and `GUIDE.md` carry
/// the same table in words.
fn exit_for(whose: omn::collect::Whose) -> u8 {
    use omn::collect::Whose;
    match whose {
        Whose::Request => EXIT_REJECTED,
        Whose::Machine => EXIT_UNAVAILABLE,
        Whose::Build => EXIT_NOT_IMPLEMENTED,
    }
}

/// Undoes the `dumpable = 0` that comes with carrying file capabilities.
///
/// A binary that gains privileges through `setcap` is marked non-dumpable at
/// exec, and the kernel then reassigns its own `/proc/<pid>` entries to root.
/// The collectors that read this process's procfs -- the heap shim's, which
/// walk `pagemap` and `smaps_rollup` -- get EACCES on files belonging to the
/// user who started the run.
///
/// The symptom is the one worth naming, because it is backwards: granting
/// `CAP_BPF` and `CAP_PERFMON` to unlock the eBPF backends *removes*
/// `mem.heap.alloc`, `.live` and `.retained`, which needed no privileges at
/// all. Two identical binaries, one with capabilities, disagree about what the
/// machine can serve.
///
/// This does not give anything away. `ptrace` of a task holding capabilities
/// the tracer lacks is refused by the capability subset check, which is a
/// separate barrier and still stands; dumpability governs core dumps and the
/// ownership of procfs entries, and this process has no business hiding either
/// from the user running it. The same call is made in every launched child,
/// for the same reason and with the same comment.
fn restore_own_dumpability() {
    // Safety: `prctl` with this option takes no pointers and cannot fail in a
    // way that matters -- a kernel that refuses it leaves the old behaviour,
    // which is what this build had before.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0);
    }
}

fn main() -> ExitCode {
    restore_own_dumpability();
    let cli = match Cli::try_parse_from(normalize(std::env::args_os())) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            // `--help` and `--version` are not failures. Everything else the
            // parser refuses -- a flag that does not exist, a value that does
            // not parse, two flags that cannot both be meant -- is a request
            // that cannot mean anything, which is the same answer the validator
            // gives one layer in and now carries the same code. It used to be
            // flattened to 1, so `--mode` misspelled and `--mode` missing
            // exited differently while being the same mistake.
            return match error.exit_code() {
                0 => ExitCode::SUCCESS,
                _ => ExitCode::from(EXIT_REJECTED),
            };
        }
    };

    match run(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            // A request missing an axis the catalog cannot fill in -- no
            // `--mode` where a source declares two, no target where it accepts
            // several -- cannot mean anything, which is exit 2. It used to fall
            // through here to the generic failure code and tell a script to go
            // and look at the machine. `--emit`, raised one layer up by the
            // validator, has always exited 2, and the two are the same answer.
            match error.downcast_ref::<omn::error::ParseError>() {
                Some(_) => ExitCode::from(EXIT_REJECTED),
                None => ExitCode::from(EXIT_UNAVAILABLE),
            }
        }
    }
}

/// Inserts the implicit `trace` subcommand, so `omn io.latency --pid 1` and
/// `omn trace io.latency --pid 1` are the same command.
fn normalize(args: impl Iterator<Item = OsString>) -> Vec<OsString> {
    let mut args: Vec<OsString> = args.collect();
    let Some(first) = args.get(1).and_then(|arg| arg.to_str()) else {
        return args;
    };
    let is_subcommand = SUBCOMMANDS.contains(&first);
    let is_global_flag = matches!(first, "-h" | "--help" | "-V" | "--version");
    if !is_subcommand && !is_global_flag {
        args.insert(1, OsString::from("trace"));
    }
    args
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Trace(args) => trace(*args),
        Command::List(args) => list(args),
        Command::Formats(args) => formats(args),
        Command::Validate(args) => validate_file(args),
        Command::Latency(args) => latency(args),
        Command::Saturate(args) => saturate(args),
        Command::Diff(args) => diff(args),
    }
}

/// Loads the catalog, from a directory when one is named, from the binary
/// otherwise.
fn catalog(explicit: Option<&Path>) -> anyhow::Result<Catalog> {
    let from_env = std::env::var_os("OMN_CATALOG_DIR").map(PathBuf::from);
    let dir = explicit.map(Path::to_path_buf).or(from_env);
    match dir {
        Some(dir) => {
            let catalog = Catalog::load_dir(&dir)?;
            // The unwind modes beside the entries, when the directory has
            // them: a catalog under iteration carries its modes too.
            omn::stack::unwind::use_table_in(&dir).map_err(anyhow::Error::msg)?;
            Ok(catalog)
        }
        None => Ok(Catalog::embedded()?),
    }
}

fn trace(args: TraceArgs) -> anyhow::Result<ExitCode> {
    let catalog = catalog(args.catalog.as_deref())?;

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
        None => cli::to_session(&args, &catalog)?,
    };

    let report = match validate::validate_session(&catalog, &session) {
        Ok(report) => report,
        Err(rejections) => return Ok(reject(&rejections)),
    };

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
            omn::request::Target::Container(name) => match omn::request::find_container(name) {
                Ok(container) => {
                    eprintln!(
                        "note: container `{name}` runs in cgroup {} (pid {}, from `{}`), which \
                         is what is measured",
                        container.cgroup.display(),
                        container.pid,
                        container.runtime
                    );
                    bound = omn::request::Request {
                        target: omn::request::Target::Cgroup(container.cgroup),
                        ..request.clone()
                    };
                    &bound
                }
                Err(reason) => {
                    eprintln!("error: {reason}");
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
            Err(error) => {
                eprintln!("error: {error}");
                // The hint used to say "the request is valid; it is this
                // machine that cannot serve it" under every resolver failure
                // and one line above the code for a request that is *not*
                // valid. Both halves are said per case now, and the code is
                // derived from the same answer the sentence gives.
                let whose = resolution_is(&error);
                if let Some(hint) = resolution_hint(&error) {
                    eprintln!("  hint: {hint}");
                }
                return Ok(ExitCode::from(exit_for(whose)));
            }
        }
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
        return Ok(ExitCode::from(exit_for(omn::collect::Whose::Machine)));
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
fn override_session_wide(session: &mut Session, args: &TraceArgs) -> Defaulted {
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
fn symbols_not_read(session: &Session) -> Option<String> {
    use omn::stack::unwind::SymbolSource;
    session.requests.iter().find_map(|request| {
        (request.symbols == Some(SymbolSource::Debuginfod)
            && !omn::stack::reads_symbols(request.source.as_str()))
        .then(|| {
            format!(
                "`--symbols debuginfod` is not implemented for `{}`: this build asks debuginfod \
                 only for the stacks of {}, and this source's are named from the files the \
                 target mapped. Leave `--symbols` out, or ask for it where it is read",
                request.source,
                omn::stack::UNWOUND_BY_CHOICE
                    .iter()
                    .map(|(source, _)| format!("`{source}`"))
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
fn resolution_is(error: &resolve::ResolveError) -> omn::collect::Whose {
    use omn::collect::Whose;
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
fn resolution_hint(error: &resolve::ResolveError) -> Option<String> {
    match error {
        resolve::ResolveError::NoViableBackend { .. } => Some(
            "the request is valid; it is this machine that cannot serve it. \
             Use --no-resolve to write it down for one that can."
                .to_owned(),
        ),
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

/// One step a budget can take below the backend and mode serving a request:
/// a backend of its chain, and the mode it would go on in.
#[derive(Debug, Clone, PartialEq)]
struct Rung {
    backend: String,
    cost: omn::catalog::Cost,
    mode: omn::request::Mode,
}

impl Rung {
    /// How the rung reads in a note, beside the mode the series began in.
    fn describe(&self, from: omn::request::Mode) -> String {
        if self.mode == from {
            format!("`{}` ({})", self.backend, self.cost)
        } else {
            format!(
                "`{}` in `{}` ({}, every event then standing for its period)",
                self.backend, self.mode, self.cost
            )
        }
    }
}

/// The rungs below what serves `resolved`, in the order a budget takes them.
///
/// First the backends below the serving one in the chain, in the same mode,
/// whose declared band fits the ceiling: each keeps every event. Then each
/// cheaper mode ([`omn::request::Mode::cheaper`], phase 6's `trace` ->
/// `sample`) on the serving backend and on those below it that fit, when the
/// request's emits can take a series made of the two modes
/// ([`omn::request::mode_move`]), and only on a backend whose collector
/// weighs each sampled event by its period ([`weighs_its_samples`]). The
/// serving backend is offered in the
/// cheaper mode whatever its band: the band is the backend's, and a sampled
/// mode of it costs less than the traced one it was measured over.
///
/// The ones above were rejected by this machine and a budget cannot make them
/// reachable; the ones below were never probed, because a better backend was
/// selected first -- so each is resolved for real before it is offered.
fn rungs(catalog: &Catalog, resolved: &ResolvedRequest, ceiling: f64) -> Vec<Rung> {
    let Some(entry) = catalog.get(&resolved.request.source) else {
        return Vec::new();
    };
    let mode = resolved.request.mode;
    let serving = entry.backend(&resolved.backend.id);
    let below: Vec<&omn::catalog::Backend> = entry
        .backends
        .iter()
        .skip_while(|backend| backend.id != resolved.backend.id)
        .skip(1)
        .filter(|backend| worst_case_percent(backend.cost) <= ceiling)
        .collect();
    let rung = |backend: &omn::catalog::Backend, mode| Rung {
        backend: backend.id.clone(),
        cost: backend.cost,
        mode,
    };
    let mut rungs: Vec<Rung> = below
        .iter()
        .filter(|backend| entry.backend_modes(backend).contains(&mode.kind()))
        .map(|backend| rung(backend, mode))
        .collect();
    for cheaper in mode.cheaper() {
        if omn::request::mode_move(mode.kind(), cheaper.kind(), &resolved.request.emit).is_err() {
            continue;
        }
        rungs.extend(
            serving
                .into_iter()
                .chain(below.iter().copied())
                .filter(|backend| entry.backend_modes(backend).contains(&cheaper.kind()))
                .filter(|backend| weighs_its_samples(resolved.request.source.as_str(), &backend.id))
                .map(|backend| rung(backend, cheaper)),
        );
    }
    rungs
}

/// The collectors whose sampled events each carry the period they stand for
/// as [`omn::emit::WEIGHT_ATTR`], by source and backend.
///
/// A series the budget moves from `trace` to `sample` is one answer only if
/// every sampled event after the move is weighed by its period; one that
/// counts once where it stood for a hundred makes every total after the move
/// short by that factor. Such a rung is passed over -- named, with the
/// reason, in the budget's note -- rather than taken and flagged after the
/// fact. `mem.faults` on `pmu:sw`, the only source whose chain declares both
/// `trace` and `sample`, weighs each sampled fault by its period
/// (`src/collect/mem/page_faults.rs`). A collector that does is added here,
/// with the test that holds its weighed series to a workload's own count.
const WEIGHS_ITS_SAMPLES: &[(&str, &str)] = &[("mem.faults", "pmu:sw")];

/// Whether the collector of `source` on `backend` weighs each sampled event
/// by its period ([`WEIGHS_ITS_SAMPLES`]).
fn weighs_its_samples(source: &str, backend: &str) -> bool {
    WEIGHS_ITS_SAMPLES
        .iter()
        .any(|(weighed, by)| *weighed == source && *by == backend)
}

/// The mode rungs a budget passes over because the collector's sampled events
/// would not be weighed, as one sentence for the budget's note.
fn unweighed_mode_rungs(catalog: &Catalog, resolved: &ResolvedRequest) -> Option<String> {
    let entry = catalog.get(&resolved.request.source)?;
    let mode = resolved.request.mode;
    let passed: Vec<String> = mode
        .cheaper()
        .into_iter()
        .filter(|cheaper| {
            omn::request::mode_move(mode.kind(), cheaper.kind(), &resolved.request.emit).is_ok()
        })
        .flat_map(|cheaper| {
            entry
                .backends
                .iter()
                .skip_while(|backend| backend.id != resolved.backend.id)
                .filter(move |backend| entry.backend_modes(backend).contains(&cheaper.kind()))
                .filter(|backend| {
                    collect::implements(resolved.request.source.as_str(), &backend.id)
                        && !weighs_its_samples(resolved.request.source.as_str(), &backend.id)
                })
                .map(move |backend| format!("`{}` in `{cheaper}`", backend.id))
        })
        .collect();
    (!passed.is_empty()).then(|| {
        format!(
            "{}, is passed over: its sampled events carry no `{}`, so each would count once \
             where it stood for its period and every total after the move would be short by \
             that factor",
            passed.join(" and "),
            omn::emit::WEIGHT_ATTR
        )
    })
}

/// Refuses a budget that could move a request onto a mode its emits cannot
/// be continued in, before anything runs.
///
/// A `raw` list -- or a `timeline`, or a `stack` count -- of a series that is
/// traced until the budget moves it and sampled after would present two kinds
/// of event as one: a traced event is an occurrence, a sampled one stands for
/// a period. The destination mode produces the emit, so the rung would be
/// taken; the combination is what is invalid, and it is refused with the
/// reason rather than degraded silently at the move. An emit the cheaper mode
/// cannot produce at all only takes that rung out of the ladder, which
/// [`budget_note`] says.
fn budget_refusal(catalog: &Catalog, resolved: &ResolvedRequest) -> Option<String> {
    budget_refusal_given(catalog, resolved, weighs_its_samples)
}

/// [`budget_refusal`], with which collectors weigh their samples given: a
/// sampled rung whose events are not weighed is passed over
/// ([`weighs_its_samples`]), so it cannot mix two kinds of event and there is
/// nothing to refuse.
fn budget_refusal_given(
    catalog: &Catalog,
    resolved: &ResolvedRequest,
    weighs: fn(&str, &str) -> bool,
) -> Option<String> {
    let budget = resolved.request.budget?;
    let entry = catalog.get(&resolved.request.source)?;
    let mode = resolved.request.mode;
    let collected = |backend: &omn::catalog::Backend| {
        collect::implements(resolved.request.source.as_str(), &backend.id)
    };
    for cheaper in mode.cheaper() {
        let Err(why @ omn::request::ModeMove::Mixed(emit)) =
            omn::request::mode_move(mode.kind(), cheaper.kind(), &resolved.request.emit)
        else {
            continue;
        };
        let Some(onto) = entry
            .backends
            .iter()
            .skip_while(|backend| backend.id != resolved.backend.id)
            .find(|backend| {
                entry.backend_modes(backend).contains(&cheaper.kind())
                    && collected(backend)
                    && weighs(resolved.request.source.as_str(), &backend.id)
            })
        else {
            continue;
        };
        return Some(format!(
            "[{}] --budget {budget} may move this collection from `{mode}` to `{cheaper}` on \
             `{}`: {why}. Drop `{emit}` or the budget, or ask for `-m {}` from the start",
            resolved.request.source,
            onto.id,
            cheaper.kind(),
        ));
    }
    None
}

/// What `--budget` implies before anything has been measured.
///
/// The flag is now enforced against a measurement, and the measurement does not
/// exist until the run is over -- so this is the part that can be said in
/// advance: what the selected backend *declares* it costs, whether that claim
/// fits the ceiling, and what the run will do if the measurement agrees. It is
/// printed on the dumps too, where the reader is deciding whether the request
/// says what they meant.
///
/// The comparison is against the *worst case* of the declared band: `med` means
/// "1 to 5 %", so it fits a 5 % budget and cannot be promised against a 2 % one.
/// Erring the other way would let a budget look satisfied by a backend that may
/// well exceed it.
fn budget_note(catalog: &Catalog, resolved: &ResolvedRequest) -> Option<String> {
    let budget = resolved.request.budget?;
    let source = &resolved.request.source;
    let selected = &resolved.backend;
    let ceiling = budget.percent();

    if worst_case_percent(selected.cost) <= ceiling {
        return Some(format!(
            "[{source}] --budget {budget}: `{}` declares `{}` ({}), which is within it. \
             What it actually cost is measured over the run and reported when it ends",
            selected.id,
            selected.cost,
            band(selected.cost),
        ));
    }

    let rungs = rungs(catalog, resolved, ceiling);
    let others = rungs.iter().any(|rung| rung.backend != selected.id);
    let passed = unweighed_mode_rungs(catalog, resolved)
        .map(|why| format!(". The ladder's cheaper mode, {why}"))
        .unwrap_or_default();
    let cheaper: Vec<String> = rungs
        .iter()
        .map(|rung| rung.describe(resolved.request.mode))
        .collect();

    Some(match cheaper.as_slice() {
        [] => format!(
            "[{source}] --budget {budget}: `{}` declares `{}` ({}), and nothing below it in the \
             chain declares less, so a run that exceeds the ceiling will stop rather than \
             degrade{passed}",
            selected.id,
            selected.cost,
            band(selected.cost),
        ),
        candidates => format!(
            "[{source}] --budget {budget}: `{}` declares `{}` ({}), so a run that exceeds the \
             ceiling will degrade onto {}{}{}",
            selected.id,
            selected.cost,
            band(selected.cost),
            candidates.join(" or "),
            // A rung on the serving backend was probed with it; one below it
            // was not, because a better backend was selected first.
            if others {
                " -- which was never probed, because a better backend was selected first"
            } else {
                ""
            },
            passed,
        ),
    })
}

/// The rungs a budget may step onto, resolved against this machine.
///
/// The runner knows it is over budget and nothing else. What is cheaper than
/// the backend and mode in use is a question about the catalog, and whether
/// this machine can serve it is a question for the resolver; both live here,
/// which is what keeps `src/collect/` from reaching back into either.
///
/// See [`rungs`] for the order. Each rung is resolved for real before it is
/// offered, and one this build has no collector for is not offered at all.
struct Ladder<'a> {
    catalog: &'a Catalog,
    capabilities: &'a Capabilities,
    /// The resolved request each collection is being served by, replaced when a
    /// budget moves it. This, not the file written before the run, is what
    /// names the backend that produced the numbers.
    serving: Vec<ResolvedRequest>,
    /// Every request offered per collection, in order. A move that is taken
    /// adds a backend to the collection's `served_by`, and a move that cannot
    /// be started ends the run, so the `n`th offer is what served after the
    /// `n`th move -- which a backend id alone cannot tell when the move kept
    /// the backend and changed the mode.
    offered: Vec<Vec<ResolvedRequest>>,
}

impl<'a> Ladder<'a> {
    fn new(
        catalog: &'a Catalog,
        capabilities: &'a Capabilities,
        resolved: &[ResolvedRequest],
    ) -> Self {
        Self {
            catalog,
            capabilities,
            serving: resolved.to_vec(),
            offered: vec![Vec::new(); resolved.len()],
        }
    }

    /// The resolved request that was serving collection `index` when the run
    /// ended, which is the one to keep beside its numbers.
    ///
    /// `served_by` is the collection's own record of the backends it ran
    /// through: the only thing that knows how many moves were taken, since a
    /// move that could not be started leaves the run where it was.
    fn record(
        &self,
        index: usize,
        served_by: &[String],
        original: &ResolvedRequest,
    ) -> ResolvedRequest {
        let moves = served_by.len().saturating_sub(1);
        let backend = served_by.last().map(String::as_str).unwrap_or_default();
        if moves == 0 && (backend.is_empty() || original.backend.id == backend) {
            return original.clone();
        }
        if let Some(offered) = self
            .offered
            .get(index)
            .and_then(|offered| offered.get(moves.checked_sub(1)?))
        {
            if offered.backend.id == backend {
                return offered.clone();
            }
        }
        let mut amended = original.clone();
        amended.backend.id = backend.to_owned();
        amended
    }

    /// The modes a collection ran in, in order, as the cost line names them:
    /// one mode, or `trace then sample@99hz` across a move between modes.
    fn modes(&self, index: usize, served_by: &[String], original: &ResolvedRequest) -> String {
        let moves = served_by.len().saturating_sub(1);
        let mut modes = vec![original.request.mode];
        for offered in self.offered.get(index).into_iter().flatten().take(moves) {
            if modes.last() != Some(&offered.request.mode) {
                modes.push(offered.request.mode);
            }
        }
        modes
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" then ")
    }
}

impl collect::Cheaper for Ladder<'_> {
    fn next(
        &mut self,
        index: usize,
        _spent: &collect::Overhead,
        ceiling: f64,
    ) -> Option<ResolvedRequest> {
        let serving = self.serving.get(index)?.clone();
        let source = serving.request.source.clone();
        for rung in rungs(self.catalog, &serving, ceiling) {
            if !collect::implements(source.as_str(), &rung.backend) {
                continue;
            }
            let mut request = serving.request.clone();
            request.mode = rung.mode;
            // Resolved for real: a backend further down the chain was never
            // probed, and offering one this machine cannot serve would turn a
            // budget into a failed run. Onto the rung, not pinned to it: the
            // ladder chose it, and the user pinned nothing.
            let Ok(mut resolved) =
                resolve::resolve_onto(self.catalog, self.capabilities, &request, &rung.backend)
            else {
                continue;
            };
            if rung.mode != serving.request.mode {
                resolved.degradations.push(format!(
                    "the budget moved this collection from `{}` to `{}` on `{}`: up to the move \
                     every event is one occurrence, and after it each stands for its period, \
                     which the emits weigh by the event's `{}`",
                    serving.request.mode,
                    rung.mode,
                    rung.backend,
                    omn::emit::WEIGHT_ATTR
                ));
            }
            self.serving[index] = resolved.clone();
            self.offered[index].push(resolved.clone());
            return Some(resolved);
        }
        None
    }
}

/// What one collection cost, as the record written beside the numbers.
#[derive(Serialize)]
struct MeasuredCost {
    label: String,
    source: String,
    /// How the source was observed. It belongs beside the price because it is
    /// most of what sets it: the same backend polled every millisecond and
    /// every second costs a thousandfold different, and only one of those
    /// numbers can be what a band declared per backend meant.
    mode: String,
    /// The backend that produced the events, which is not the one the resolved
    /// file names when a budget moved the collection mid-run.
    backend: String,
    /// Every backend the collection ran through, in order. Longer than one only
    /// when a budget degraded it part way through the window.
    served_by: Vec<String>,
    /// The band the catalog declares for that backend, so the claim and the
    /// measurement sit side by side.
    declared: &'static str,
    declared_worst_case_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_percent: Option<f64>,
    /// Whether the measurement came in under the ceiling. Absent when there was
    /// no ceiling, or no workload to be a percentage of.
    #[serde(skip_serializing_if = "Option::is_none")]
    within_budget: Option<bool>,
    /// What the run cost as a share of the work the target did of its own:
    /// the tool's cpu and what the collector spent inside the target
    /// (`overhead.in_target_cpu_ns`), when that is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    measured_percent: Option<f64>,
    /// The tool's own cpu alone, as a share of the target's.
    #[serde(skip_serializing_if = "Option::is_none")]
    own_percent: Option<f64>,
    overhead: collect::Overhead,
    stopped_because: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
    /// Set for a recording, whose budget is weighed over a rolling span
    /// rather than the whole window: that span, in nanoseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    weighed_over_ns: Option<u64>,
}

impl MeasuredCost {
    /// The one line a reader gets for free at the end of every run.
    fn line(&self) -> String {
        let overhead = &self.overhead;
        let against = match (overhead.observed_cpu_ns, self.measured_percent) {
            (Some(observed), Some(percent)) => format!(
                "against the target's {} ({percent:.2} %, declared `{}`)",
                span(observed),
                self.declared
            ),
            (Some(_), None) => format!(
                "against a target that burned no cpu at all, so there is nothing for it to be \
                 a percentage of ({})",
                overhead.observed_from
            ),
            (None, _) => format!("against nothing measurable: {}", overhead.observed_from),
        };
        let inside = match (overhead.in_target_cpu_ns, overhead.in_target_runs) {
            (Some(0), None) => String::new(),
            (Some(ns), Some(runs)) if overhead.in_target_left_out > 0 => format!(
                ", and {} inside the target over {runs} run(s) of its own code, {} of them \
                 priced at the mean of the others because a preemption fell in their timed \
                 span",
                span(ns),
                overhead.in_target_left_out
            ),
            (Some(ns), Some(runs)) => format!(
                ", and {} inside the target over {runs} run(s) of its own code",
                span(ns)
            ),
            (Some(ns), None) => format!(", and {} inside the target", span(ns)),
            (None, _) => " and an unknown amount inside the target".to_owned(),
        };
        let unknown = if overhead.in_target_known() {
            String::new()
        } else {
            format!(
                ". What it spent inside the target is not in that percentage: {}",
                overhead.in_target_from
            )
        };
        // What this process spent before the window opened -- its startup,
        // the probes, the tables a run reads first, such as the kernel's
        // symbols -- dilated nothing and is in no percentage, and once it was
        // a second against a window's 80 ms with nothing on this line to say
        // so. Named when it outweighs the window. It is the process's cpu at
        // this collection's opening, so a collection attached mid-run also
        // holds the other collections' windows: it is said per collection,
        // and never added up across them.
        let before = if overhead.before_window_cpu_ns > overhead.attributed_cpu_ns {
            format!(
                "; and {} before this collection's window opened (startup, probes, the tables \
                 the run reads first), which is in no percentage here",
                span(overhead.before_window_cpu_ns)
            )
        } else {
            String::new()
        };
        format!(
            "{} ({}) via `{}` spent {} of cpu{inside} {against}, over {} of wall time{before}; \
             measuring that cost {} across {} read(s){unknown}",
            self.source,
            self.mode,
            self.served_by.join("` then `"),
            span(overhead.attributed_cpu_ns),
            span(overhead.wall_ns),
            span(overhead.measurement_ns),
            overhead.measurements,
        )
    }

    /// What the ceiling made of it, when there was one.
    fn verdict(&self) -> Option<String> {
        let budget = self.budget_percent?;
        if let Some(rolling_ns) = self.weighed_over_ns {
            let over = span(rolling_ns);
            return Some(match (self.within_budget, self.measured_percent) {
                (Some(false), _) => format!(
                    "[{}] --budget {budget}% was exceeded over a {over} span, which is what a \
                     recording is weighed over: {}",
                    self.source, self.stopped_because
                ),
                (_, Some(percent)) => format!(
                    "[{}] --budget {budget}% held over every {over} span the run checked; \
                     {percent:.2} % over the whole window",
                    self.source
                ),
                (_, None) => format!(
                    "[{}] --budget {budget}% could not be checked: {}",
                    self.source, self.overhead.observed_from
                ),
            });
        }
        Some(match (self.within_budget, self.measured_percent) {
            (Some(true), Some(percent)) => format!(
                "[{}] --budget {budget}% held: {percent:.2} % measured",
                self.source
            ),
            (None, Some(percent)) if !self.overhead.in_target_known() => format!(
                "[{}] --budget {budget}% could not be checked whole: the tool's own cpu came to \
                 {percent:.2} %, and what the collector spent inside the target is unknown: {}",
                self.source, self.overhead.in_target_from
            ),
            (Some(false), Some(percent)) => format!(
                "[{}] --budget {budget}% was exceeded: {percent:.2} % measured over the whole \
                 window. {}",
                self.source, self.stopped_because
            ),
            _ => format!(
                "[{}] --budget {budget}% could not be checked: {}",
                self.source, self.overhead.observed_from
            ),
        })
    }
}

/// Nanoseconds a human can read, down to the microsecond: what a budget check
/// costs is measured there and rounding it away would hide the answer.
fn span(ns: u64) -> String {
    match ns {
        0 => "nothing".to_owned(),
        ns if ns < 1_000 => format!("{ns}ns"),
        ns if ns < 1_000_000 => format!("{:.1}us", ns as f64 / 1e3),
        ns if ns < 1_000_000_000 => format!("{:.1}ms", ns as f64 / 1e6),
        ns => format!("{:.2}s", ns as f64 / 1e9),
    }
}

/// The top of the overhead band a declared cost stands for: the catalog's
/// own answer, which `omn-mcp` reads too.
fn worst_case_percent(cost: omn::catalog::Cost) -> f64 {
    cost.worst_case_percent()
}

fn band(cost: omn::catalog::Cost) -> &'static str {
    use omn::catalog::Cost;
    match cost {
        Cost::Free => "about zero, under 0.1 %",
        Cost::Low => "under 1 %",
        Cost::Med => "1 to 5 %",
        Cost::High => "5 to 20 %",
        Cost::Extreme => "over 20 %, with no bound above",
    }
}

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
fn shared_stdout_warning(session: &Session) -> Option<String> {
    // Text is left out although it "renders documents": it is a report for a
    // person, who can see what came from where, and it is the format every run
    // launching a program in a terminal uses.
    let structured = session.format != OutputFormat::Text;
    let launches = session
        .requests
        .iter()
        .any(|request| matches!(request.target, omn::request::Target::Launch(_)));
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
fn refused_at_collect(
    catalog: &Catalog,
    error: &collect::CollectError,
    resolved: &[ResolvedRequest],
) -> ExitCode {
    use omn::collect::Whose;

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
                if let omn::resolve::Verdict::Rejected(reasons) = &considered.verdict {
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
struct PassedOver {
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
fn passed_over_first(
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
            omn::resolve::Verdict::Rejected(reasons) => Some((considered.id.as_str(), reasons)),
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
fn measure(
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
    let collections = match collect::run_all_within_unserved(resolved, &mut ladder, unserved) {
        Ok(collections) => collections,
        Err(error) => return Ok(refused_at_collect(catalog, &error, resolved)),
    };
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

    Ok(ExitCode::SUCCESS)
}

/// What is said on stderr before any run: the defaults taken, the
/// validator's warnings, and a stdout the program would share.
fn announce(session: &Session, report: &Report, defaulted: &Defaulted) {
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

/// What the measurement cost, on every run rather than only on the ones that
/// asked. A tool that reports what a program spends and never its own price
/// is asking to be trusted about the one number it never shows.
fn report_costs(costs: &[MeasuredCost], run: &encode::RunOutput) {
    for cost in costs {
        eprintln!("cost: {}", cost.line());
        if let Some(verdict) = cost.verdict() {
            eprintln!("budget: {verdict}");
        }
    }
    if let Some(comparison) = compare_backends(costs, &run.results) {
        eprint!("{comparison}");
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
fn record(
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
    let (collections, recorded) =
        match collect::record_within(resolved, &mut cheaper, &mut recorder) {
            Ok(done) => done,
            Err(error) => return Ok(refused_at_collect(catalog, &error, resolved)),
        };
    drop(recorder);

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
            if cost.budget_percent.is_some() {
                cost.within_budget = Some(!over_budget(&recorded.stopped_because, &cost.source));
            }
            cost
        })
        .collect();
    for cost in &costs {
        eprintln!("cost: {}", cost.line());
        if let Some(verdict) = cost.verdict() {
            eprintln!("budget: {verdict}");
        }
    }
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
    Ok(ExitCode::SUCCESS)
}

/// Whether a recording ended because `source` was over its rolling budget.
fn over_budget(stopped_because: &str, source: &str) -> bool {
    stopped_because.starts_with(&format!("`{source}` spent"))
        && stopped_because.contains(collect::ROLLING_BUDGET)
}

/// The budget's ladder, shared with a recording's documents.
struct Shared<'l, 'a>(&'l std::cell::RefCell<Ladder<'a>>);

impl collect::Cheaper for Shared<'_, '_> {
    fn next(
        &mut self,
        index: usize,
        spent: &collect::Overhead,
        ceiling: f64,
    ) -> Option<ResolvedRequest> {
        self.0.borrow_mut().next(index, spent, ceiling)
    }
}

/// Turns a run's collections into its output: every request's results, the
/// joins across them and the decompositions, with what each cost and the
/// request that was serving it when it ended.
fn assemble(
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
    let mut run = encode::RunOutput {
        results: Vec::new(),
        joins: Vec::new(),
        decompositions: Vec::new(),
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
        merged.extend(collection.events.iter().cloned());
        bounds.push(merged.len());
        let unit = catalog
            .get(&serving.request.source)
            .map(|entry| entry.backend_unit(&serving.backend.id))
            .unwrap_or(omn::catalog::Unit::Count);
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
    let streams: Vec<&[omn::event::Event]> =
        bounds.windows(2).map(|at| &merged[at[0]..at[1]]).collect();
    for rule in &session.joins {
        run.joins
            .push(join::apply_merging(rule, &streams, &windows)?);
    }
    // The served requests rather than the session's: a budget can lower a
    // sampling rate, and a sample stands for the period it was taken at.
    let served: Vec<omn::request::Request> = served_by
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
/// `weight` ([`omn::request::EmitKind::weighs_each_event`]). A collector whose
/// sampled events carry none has each of them count once where it stood for a
/// period, and the totals from the move on are short by that factor -- the one
/// thing about the move the events themselves can show, so it is read off
/// them rather than assumed.
fn unweighed_after_a_mode_move(
    original: &ResolvedRequest,
    serving: &ResolvedRequest,
    collection: &collect::Collection,
) -> Option<String> {
    if original.request.mode.kind() == serving.request.mode.kind() {
        return None;
    }
    let weighed = collection.events.iter().any(|event| {
        event
            .attr(omn::emit::WEIGHT_ATTR)
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
            omn::emit::WEIGHT_ATTR
        )
    })
}

/// The backend a collection ended on, which is the one its numbers came from.
fn served_last(collection: &collect::Collection) -> &str {
    collection
        .served_by
        .last()
        .map(String::as_str)
        .unwrap_or_default()
}

/// Pairs what one collection cost with what its backend claimed it would.
fn measured_cost(
    catalog: &Catalog,
    resolved: &ResolvedRequest,
    collection: &collect::Collection,
) -> MeasuredCost {
    let declared = catalog
        .get(&resolved.request.source)
        .and_then(|entry| entry.backend(served_last(collection)))
        .map(|backend| backend.cost)
        .unwrap_or(resolved.backend.cost);
    let measured_percent = collection.overhead.percent();
    let budget_percent = resolved.request.budget.map(|budget| budget.percent());
    MeasuredCost {
        label: resolved
            .request
            .label
            .clone()
            .unwrap_or_else(|| resolved.request.source.to_string()),
        source: resolved.request.source.to_string(),
        mode: resolved.request.mode.to_string(),
        backend: served_last(collection).to_owned(),
        served_by: collection.served_by.clone(),
        declared: declared.as_str(),
        declared_worst_case_percent: worst_case_percent(declared),
        budget_percent,
        // Over the ceiling on what is known is over it; under it is "held"
        // only when nothing spent inside the target is missing from the sum.
        within_budget: budget_percent.and_then(|ceiling| {
            measured_percent
                .map(|percent| percent <= ceiling)
                .filter(|within| !within || collection.overhead.in_target_known())
        }),
        measured_percent,
        own_percent: collection.overhead.own_percent(),
        overhead: collection.overhead.clone(),
        stopped_because: collection.stopped_because.clone(),
        notes: collection.notes.clone(),
        weighed_over_ns: None,
    }
}

/// The self-benchmark: one source measured through several backends at once,
/// and what each of them cost.
///
/// Running one backend, then another, and comparing the two is *not* this. Two
/// sequential runs observe two different workloads, and most of the difference
/// between them is the difference between the moments. A session that names the
/// same source twice with `--backend force:` puts both backends on the same
/// target, in the same window, against the same clock -- which is the only
/// arrangement in which "one cost more" and "they disagreed" mean anything.
fn compare_backends(costs: &[MeasuredCost], results: &[emit::Results]) -> Option<String> {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fmt::Write;

    let mut by_source: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, cost) in costs.iter().enumerate() {
        by_source
            .entry(cost.source.as_str())
            .or_default()
            .push(index);
    }

    let mut out = String::new();
    for (source, indices) in by_source {
        let backends: BTreeSet<&str> = indices
            .iter()
            .map(|index| costs[*index].backend.as_str())
            .collect();
        if backends.len() < 2 {
            continue;
        }
        let _ = writeln!(
            out,
            "self-benchmark: `{source}` through {} backends, one target, one window",
            backends.len()
        );
        let _ = writeln!(
            out,
            "  {:<24} {:>8} {:>10} {:>8} {:>14}",
            "BACKEND", "DECLARED", "MEASURED", "EVENTS", "MEAN"
        );
        for index in &indices {
            let cost = &costs[*index];
            let _ = writeln!(
                out,
                "  {:<24} {:>8} {:>10} {:>8} {:>14}",
                cost.backend,
                cost.declared,
                cost.measured_percent
                    .map_or_else(|| "--".to_owned(), |percent| format!("{percent:.3} %")),
                results[*index].events_seen,
                summary_mean(&results[*index])
                    .map_or_else(|| "--".to_owned(), |mean| format!("{mean:.4e}")),
            );
        }

        let means: Vec<f64> = indices
            .iter()
            .filter_map(|index| summary_mean(&results[*index]))
            .collect();
        match means.as_slice() {
            [] | [_] => {
                let _ = writeln!(
                    out,
                    "  whether they agree was not checked: that needs `--emit summary` on each"
                );
            }
            means => {
                let low = means.iter().cloned().fold(f64::INFINITY, f64::min);
                let high = means.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let spread = if high.abs() > 0.0 {
                    (high - low) / high.abs() * 100.0
                } else {
                    0.0
                };
                let _ = writeln!(
                    out,
                    "  the backends agree to within {spread:.2} % on the value they report"
                );
            }
        }

        // The declared band is a claim; this is the line that checks it.
        for index in &indices {
            let cost = &costs[*index];
            if let Some(percent) = cost.measured_percent {
                if percent > cost.declared_worst_case_percent {
                    let _ = writeln!(
                        out,
                        "  `{}` declares `{}` and measured {percent:.3} % at {}: a band is \
                         declared per backend, and what a poll costs is set by how often it is \
                         asked",
                        cost.backend, cost.declared, cost.mode
                    );
                }
            }
        }
    }

    (!out.is_empty()).then_some(out)
}

/// One number for a whole result, so two backends can be asked whether they
/// agree: the count-weighted mean over every series a `summary` produced.
fn summary_mean(results: &emit::Results) -> Option<f64> {
    let series = results.outputs.iter().find_map(|output| match output {
        emit::Output::Summary { series } => Some(series),
        _ => None,
    })?;
    let count: usize = series.iter().map(|one| one.count).sum();
    (count > 0).then(|| {
        series
            .iter()
            .map(|one| one.mean * one.count as f64)
            .sum::<f64>()
            / count as f64
    })
}

/// Writes the numbers and, beside them, the record of how they were obtained.
///
/// The resolved requests are always written, in JSON, whatever the results
/// format is: results without them are numbers whose provenance is gone.
fn write_artifacts(
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

fn print_resolved(format: OutputFormat, resolutions: &[ResolvedRequest]) -> anyhow::Result<()> {
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

fn print_session(
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

fn list(args: ListArgs) -> anyhow::Result<ExitCode> {
    let catalog = catalog(args.catalog.as_deref())?;
    // Before `detect`, which is what probes: the switch changes how the
    // question is asked and must be set before anything asks it.
    if args.verify_attach {
        omn::bpf::verify_attach();
    }
    let capabilities = Capabilities::detect();
    match args.format {
        OutputFormat::Text => print!(
            "{}",
            render::list(
                &catalog,
                args.selection.as_deref(),
                &capabilities,
                args.color.style(),
            )
        ),
        format if format.renders_documents() => {
            let selected: Vec<_> = match args.selection.as_deref() {
                None => catalog.entries().iter().collect(),
                Some(name) => match name.parse().ok().and_then(|id| catalog.get(&id)) {
                    Some(entry) => vec![entry],
                    None => catalog.family(name),
                },
            };
            // The same sweep the text table's `HERE` and `COLLECTOR` columns
            // show, so a program reading the listing has what a person does:
            // what this machine serves, through which backend, what each
            // backend's requirements answered, and what this build collects.
            let coverage = collect::coverage();
            let listed: Vec<Listed<'_>> = selected
                .into_iter()
                .map(|entry| Listed {
                    here: resolve::availability(entry, &capabilities),
                    probes: entry
                        .backends
                        .iter()
                        .map(|backend| BackendProbes {
                            backend: backend.id.clone(),
                            collector: coverage
                                .get(entry.id.as_str())
                                .is_some_and(|built| built.contains(&backend.id.as_str())),
                            // The resolver's own sweep, so the listing and
                            // a request can never ask the question twice two
                            // different ways.
                            requirements: resolve::probe_all(&capabilities, backend, None),
                        })
                        .collect(),
                    entry,
                })
                .collect();
            print!("{}", format.serialize_document(&listed)?);
        }
        format => anyhow::bail!(
            "`{format}` describes results; the catalog lists as text, json, yaml or toml"
        ),
    }
    Ok(ExitCode::SUCCESS)
}

/// One catalog entry as `omn list` gives it to a program: the entry, and
/// what this machine and this build make of it.
#[derive(Serialize)]
struct Listed<'a> {
    #[serde(flatten)]
    entry: &'a omn::catalog::Entry,
    /// What the text table's `HERE` column says, with the backend it names.
    here: resolve::Availability,
    /// Every backend of the chain in order, with what each requirement's
    /// probe answered here and whether this build has a collector for it.
    probes: Vec<BackendProbes>,
}

#[derive(Serialize)]
struct BackendProbes {
    backend: String,
    collector: bool,
    requirements: Vec<resolve::Requirement>,
}

/// The machine-readable view of one output format.
#[derive(Serialize)]
struct FormatInfo {
    name: &'static str,
    description: &'static str,
    renders_documents: bool,
    emits: Vec<&'static str>,
    implemented: bool,
    planned_in: Option<&'static str>,
}

#[derive(Serialize)]
struct FormatsInfo {
    input: Vec<&'static str>,
    output: Vec<FormatInfo>,
}

fn formats(args: FormatsArgs) -> anyhow::Result<ExitCode> {
    match args.format {
        OutputFormat::Text => print!("{}", render::formats()),
        format if format.renders_documents() => {
            let info = FormatsInfo {
                input: omn::format::InputFormat::ALL
                    .iter()
                    .map(|f| f.as_str())
                    .collect(),
                output: OutputFormat::ALL
                    .iter()
                    .map(|output| FormatInfo {
                        name: output.as_str(),
                        description: output.describe(),
                        renders_documents: output.renders_documents(),
                        emits: omn::request::EmitKind::ALL
                            .iter()
                            .filter(|emit| output.supports_emit(**emit))
                            .map(|emit| emit.as_str())
                            .collect(),
                        implemented: output.status() == FormatStatus::Ready,
                        planned_in: match output.status() {
                            FormatStatus::Planned { phase } => Some(phase),
                            FormatStatus::Ready => None,
                        },
                    })
                    .collect(),
            };
            print!("{}", format.serialize_document(&info)?);
        }
        format => anyhow::bail!("`{format}` describes results, not the format listing"),
    }
    Ok(ExitCode::SUCCESS)
}

fn validate_file(args: ValidateArgs) -> anyhow::Result<ExitCode> {
    let catalog = catalog(args.catalog.as_deref())?;
    let (mut session, origin) = session_file::load(&args.file, args.input_format)?;
    if args.format != OutputFormat::default() {
        session.format = args.format;
    }

    match validate::validate_session(&catalog, &session) {
        Ok(report) => {
            print_session(&catalog, &session, &report, &Defaulted::default(), &[])?;
            eprintln!(
                "\n{origin}: {} request(s) valid against the catalog. \
                 Availability on this machine is the resolver's call, not this one's.",
                session.requests.len()
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(rejections) => {
            eprintln!("{origin}:");
            Ok(reject(&rejections))
        }
    }
}

/// A triage request refused before anything ran: it cannot mean anything as
/// asked, which is the validator's answer and carries its code.
fn refuse_triage(refusal: &omn::triage::Refusal) -> ExitCode {
    eprintln!("error: {refusal}");
    ExitCode::from(EXIT_REJECTED)
}

/// Prints a triage answer in the format asked for. Text is the answer's own
/// report; the structured formats are the answer itself, every row included.
fn print_answer<T: Serialize>(
    format: OutputFormat,
    value: &T,
    text: impl FnOnce() -> String,
) -> anyhow::Result<()> {
    match format {
        OutputFormat::Text => print!("{}", text()),
        format if format.renders_documents() => print!("{}", format.serialize_document(value)?),
        format => anyhow::bail!(
            "`{format}` describes a measurement's results, not a triage answer: use text, json, \
             yaml or toml, and `omn -f` on the --dump-session file for the results themselves"
        ),
    }
    Ok(())
}

/// Prints the session a triage question is asked through.
fn print_triage_session(format: OutputFormat, session: &Session) -> anyhow::Result<()> {
    let format = match format {
        OutputFormat::Text => OutputFormat::Yaml,
        other => other,
    };
    print!("{}", format.serialize_document(session)?);
    Ok(())
}

fn latency(args: LatencyArgs) -> anyhow::Result<ExitCode> {
    use omn::request::Target;
    use omn::triage::{latency, Method, TriageError};

    let objective = match Method::Latency.require(args.objective) {
        Ok(objective) => objective,
        Err(refusal) => return Ok(refuse_triage(&refusal)),
    };
    let target =
        match (args.pid, args.program.is_empty()) {
            (Some(pid), _) => Target::Pid(vec![pid]),
            (None, false) => Target::Launch(args.program.clone()),
            (None, true) => return Ok(refuse_triage(&omn::triage::Refusal(
                "`omn latency` needs a program: `-- PROGRAM [ARGS]` to launch one, or `--pid PID` \
                 with --duration for one already running"
                    .to_owned(),
            ))),
        };
    let ask = latency::Ask {
        objective,
        target,
        runs: args.runs,
        duration: args.duration,
        hz: args.hz,
    };
    if let Err(refusal) = ask.check() {
        return Ok(refuse_triage(&refusal));
    }
    if args.dump_session {
        print_triage_session(args.format, &ask.session())?;
        return Ok(ExitCode::SUCCESS);
    }
    let catalog = catalog(args.catalog.as_deref())?;
    let capabilities = Capabilities::detect();
    match latency::answer(&catalog, &capabilities, &ask) {
        Ok(answer) => {
            print_answer(args.format, &answer, || answer.text())?;
            Ok(ExitCode::SUCCESS)
        }
        Err(TriageError::Refused(refusal)) => Ok(refuse_triage(&refusal)),
        Err(error) => {
            eprintln!("error: {error}");
            Ok(ExitCode::from(exit_for(error.whose())))
        }
    }
}

fn saturate(args: SaturateArgs) -> anyhow::Result<ExitCode> {
    use omn::request::Target;
    use omn::triage::{saturate, Method, TriageError};

    let objective = match Method::Saturate.require(args.objective) {
        Ok(objective) => objective,
        Err(refusal) => return Ok(refuse_triage(&refusal)),
    };
    let target = match (args.pid, args.program.is_empty()) {
        (Some(pid), _) => Target::Pid(vec![pid]),
        (None, false) => Target::Launch(args.program.clone()),
        (None, true) => {
            return Ok(refuse_triage(&omn::triage::Refusal(
                "`omn saturate` needs a workload: `-- PROGRAM [ARGS]` to launch one, or \
                 `--pid PID` with --duration for one already running"
                    .to_owned(),
            )))
        }
    };
    let ask = saturate::Ask {
        objective,
        target,
        duration: args.duration,
    };
    if let Err(refusal) = ask.check() {
        return Ok(refuse_triage(&refusal));
    }
    if args.dump_session {
        print_triage_session(args.format, &ask.session())?;
        return Ok(ExitCode::SUCCESS);
    }
    let catalog = catalog(args.catalog.as_deref())?;
    let capabilities = Capabilities::detect();
    match saturate::answer(&catalog, &capabilities, &ask) {
        Ok(answer) => {
            print_answer(args.format, &answer, || answer.text())?;
            Ok(ExitCode::SUCCESS)
        }
        Err(TriageError::Refused(refusal)) => Ok(refuse_triage(&refusal)),
        Err(error) => {
            eprintln!("error: {error}");
            Ok(ExitCode::from(exit_for(error.whose())))
        }
    }
}

fn diff(args: DiffArgs) -> anyhow::Result<ExitCode> {
    use omn::triage::diff::{self, Profile};

    let load = |path: &Path| Profile::load(path);
    let (before, after) = match (load(&args.before), load(&args.after)) {
        (Ok(before), Ok(after)) => (before, after),
        (Err(error), _) | (_, Err(error)) => {
            eprintln!("error: {error}");
            return Ok(ExitCode::from(EXIT_REJECTED));
        }
    };
    let result = diff::diff(before, after);
    match args.format {
        OutputFormat::Folded => print!("{}", result.folded()),
        format => print_answer(format, &result, || result.text(args.top))?,
    }
    Ok(ExitCode::SUCCESS)
}

fn reject(rejections: &[Rejection]) -> ExitCode {
    eprint!("{}", render::rejections(rejections));
    ExitCode::from(EXIT_REJECTED)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalized(args: &[&str]) -> Vec<String> {
        normalize(args.iter().map(OsString::from))
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// `mem.faults` traced under a budget, resolved on this machine, or why it
    /// cannot be.
    fn traced_faults(emit: &str) -> Result<(Catalog, ResolvedRequest), String> {
        let catalog = Catalog::embedded().expect("the embedded catalog loads");
        let request = omn::request::Request {
            source: "mem.faults".parse().unwrap(),
            target: omn::request::Target::Pid(vec![std::process::id() as i32]),
            mode: omn::request::Mode::Trace,
            emit: vec![emit.parse().unwrap()],
            keys: Vec::new(),
            filter: None,
            duration: None,
            budget: Some("0.01%".parse().unwrap()),
            stack: None,
            symbols: None,
            backend: None,
            label: None,
            window: None,
        };
        let capabilities = Capabilities::detect();
        resolve::resolve(&catalog, &capabilities, &request)
            .map(|resolved| (catalog, resolved))
            .map_err(|error| error.to_string())
    }

    /// The rung `trace` -> `sample` is offered only on a collector that
    /// weighs its samples, and `raw` is refused only when it could be taken:
    /// with `mem.faults`'s collector weighing (as the filed change to
    /// `page_faults.rs` would make it), a `raw` list is refused up front and a
    /// `hist` is not; with it as it is, the rung is passed over, nothing can
    /// mix, and the note names the rung and why.
    #[test]
    fn a_sampled_rung_whose_events_are_not_weighed_is_passed_over_not_taken() {
        let (catalog, raw) = match traced_faults("raw") {
            Ok(resolved) => resolved,
            Err(why) => {
                eprintln!("SKIP: `mem.faults` traced is not served here: {why}");
                return;
            }
        };
        let (_, hist) = traced_faults("hist").expect("hist resolves where raw does");
        if raw.backend.id != "pmu:sw" {
            eprintln!(
                "SKIP: `mem.faults` traced is served by `{}`, not `pmu:sw`",
                raw.backend.id
            );
            return;
        }
        let refused = budget_refusal_given(&catalog, &raw, |_, _| true)
            .expect("a weighing collector makes the move reachable, and raw cannot cross it");
        assert!(
            refused.contains("`raw` would hold the events of both modes as one kind"),
            "{refused}"
        );
        assert_eq!(budget_refusal_given(&catalog, &hist, |_, _| true), None);

        assert!(!weighs_its_samples("mem.faults", "pmu:sw"));
        assert_eq!(budget_refusal(&catalog, &raw), None);
        assert!(
            rungs(&catalog, &hist, 0.01)
                .iter()
                .all(|rung| rung.mode == hist.request.mode),
            "no mode rung on an unweighed collector"
        );
        let note = budget_note(&catalog, &hist).expect("a budget has a note");
        assert!(
            note.contains("`pmu:sw` in `sample@99hz`, is passed over")
                && note.contains("carry no `weight`"),
            "{note}"
        );
    }

    #[test]
    fn a_bare_source_gets_the_implicit_trace_subcommand() {
        assert_eq!(
            normalized(&["omn", "io.latency", "--pid", "1"]),
            ["omn", "trace", "io.latency", "--pid", "1"]
        );
    }

    #[test]
    fn real_subcommands_and_global_flags_are_left_alone() {
        assert_eq!(normalized(&["omn", "list", "io"]), ["omn", "list", "io"]);
        assert_eq!(normalized(&["omn", "--help"]), ["omn", "--help"]);
        assert_eq!(normalized(&["omn"]), ["omn"]);
    }
    #[test]
    fn a_key_the_backend_cannot_attach_is_the_request_s_to_change() {
        let error = resolve::ResolveError::KeyNotCarried {
            source_id: "io.bytes".into(),
            backend: "fs:proc".into(),
            key: omn::request::KeyKind::Task,
            elsewhere: Some(resolve::KeyElsewhere::Forceable("tracepoint".into())),
        };
        assert_eq!(resolution_is(&error), omn::collect::Whose::Request);
        // No hint: the message already names what would serve it.
        assert_eq!(resolution_hint(&error), None);
        let message = error.to_string();
        assert!(message.contains("`task`"), "{message}");
        assert!(message.contains("--backend force:tracepoint"), "{message}");

        let nowhere = resolve::ResolveError::KeyNotCarried {
            source_id: "io.bytes".into(),
            backend: "fs:proc".into(),
            key: omn::request::KeyKind::Task,
            elsewhere: None,
        };
        assert!(
            nowhere
                .to_string()
                .contains("no backend of `io.bytes` attaches it"),
            "{nowhere}"
        );
        assert_eq!(resolution_is(&nowhere), omn::collect::Whose::Request);

        // A backend nothing collects is the build's gap, and one the machine
        // cannot run is the machine's; neither is offered as a pin.
        let unwritten = resolve::ResolveError::KeyNotCarried {
            source_id: "io.bytes".into(),
            backend: "fs:proc".into(),
            key: omn::request::KeyKind::Task,
            elsewhere: Some(resolve::KeyElsewhere::Unwritten("bpf:raw_tp".into())),
        };
        assert_eq!(resolution_is(&unwritten), omn::collect::Whose::Build);
        assert!(
            !unwritten.to_string().contains("--backend force:"),
            "{unwritten}"
        );
        let missing = resolve::ResolveError::KeyNotCarried {
            source_id: "io.bytes".into(),
            backend: "fs:proc".into(),
            key: omn::request::KeyKind::Task,
            elsewhere: Some(resolve::KeyElsewhere::Missing {
                backend: "bpf:raw_tp".into(),
                reason: "bpf: missing".into(),
            }),
        };
        assert_eq!(resolution_is(&missing), omn::collect::Whose::Machine);
        assert!(
            !missing.to_string().contains("--backend force:"),
            "{missing}"
        );
    }
}
