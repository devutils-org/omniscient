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
    Cli, Command, Defaulted, DiffArgs, FormatsArgs, LatencyArgs, ListArgs, SaturateArgs, TopArgs,
    TraceArgs, ValidateArgs, SUBCOMMANDS,
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
        Whose::Machine => {
            said_which(Unavailable::Machine);
            EXIT_UNAVAILABLE
        }
        Whose::Build => EXIT_NOT_IMPLEMENTED,
    }
}

/// Which of the two answers exit 1 gives: the machine refused the run, or
/// the run failed. A caller deciding whether a pair is a skip (the machine
/// cannot) or a failure (it could, and something broke) reads it from the
/// last line of stderr, which is one of [`Unavailable::line`]'s, rather than
/// guess it from whether results were written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unavailable {
    /// Refused before anything ran: no backend here, a counter that would
    /// not open, a target that is not there. Nothing was measured.
    Machine,
    /// The run started and failed before it wrote its results.
    FailedUnwritten,
    /// The run reached its end and wrote its results, and a collector broke
    /// the program it measured on the way.
    FailedWritten,
}

impl Unavailable {
    fn line(self) -> &'static str {
        match self {
            Unavailable::Machine => "exit 1: the machine refused the run; nothing was measured",
            Unavailable::FailedUnwritten => {
                "exit 1: the run failed after it started, before writing its results"
            }
            Unavailable::FailedWritten => {
                "exit 1: the run failed after it started; what it read is written above"
            }
        }
    }
}

/// Whether a run has started: the collectors attached and the program
/// released. An error after it is a run that failed, not a refusal.
static RUN_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Says which exit 1 this is, once per process.
fn said_which(which: Unavailable) {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !SAID.swap(true, std::sync::atomic::Ordering::SeqCst) {
        eprintln!("{}", which.line());
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

mod repeat;

/// When this process started, on `CLOCK_MONOTONIC`: what a run attached to a
/// process already running says its attach came after.
static STARTED_NS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// What a run attached to processes already running says of when it
/// attached: what they did between the command and the attach is in no
/// reading, and `perf stat -p` and `perf record -p` attach within a few
/// milliseconds, so a reader comparing against them needs to know.
fn attached_late(resolved: &[ResolvedRequest], collections: &[collect::Collection]) {
    let Some(started_ns) = STARTED_NS.get() else {
        return;
    };
    for (resolved, collection) in resolved.iter().zip(collections) {
        let target = &resolved.request.target;
        if !matches!(
            target,
            omn::request::Target::Pid(_)
                | omn::request::Target::Tid(_)
                | omn::request::Target::Cgroup(_)
                | omn::request::Target::Container(_)
        ) || !collection.window.attached
        {
            continue;
        }
        let after_ns = collection.window.opened_ns.saturating_sub(*started_ns);
        eprintln!(
            "note: `{}` attached to {target} {} after omn started; what it did before that is \
             in no reading",
            resolved
                .request
                .label
                .as_deref()
                .unwrap_or(resolved.request.source.as_str()),
            span(after_ns)
        );
    }
}

fn main() -> ExitCode {
    let _ = STARTED_NS.set(omn::clock::monotonic_ns());
    restore_own_dumpability();
    // A launched program's orphans are reparented here, where a stop can
    // still end them (`collect::adopt_orphans`). This process starts nothing
    // else that outlives the call that started it.
    omn::collect::adopt_orphans();
    let cli = match Cli::try_parse_from(omn::lazy::spell_family(normalize(std::env::args_os()))) {
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
                None => {
                    said_which(if RUN_STARTED.load(std::sync::atomic::Ordering::SeqCst) {
                        Unavailable::FailedUnwritten
                    } else {
                        Unavailable::Machine
                    });
                    ExitCode::from(EXIT_UNAVAILABLE)
                }
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
    if let Some(refusal) = cli.program_stderr_refusal() {
        eprintln!("error: {refusal}");
        return Ok(ExitCode::from(EXIT_REJECTED));
    }
    if let Some(path) = &cli.program_stderr {
        // What the variable sets, and read where the variable is: the launch
        // takes it from this process's environment and does not pass it on.
        // Set before any thread is started.
        std::env::set_var(omn::collect::PROGRAM_STDERR_VARIABLE, path);
    }
    match cli.command {
        Command::Trace(args) => trace(*args),
        Command::List(args) => list(args),
        Command::Formats(args) => formats(args),
        Command::Validate(args) => validate_file(args),
        Command::Latency(args) => latency(args),
        Command::Saturate(args) => saturate(args),
        Command::Diff(args) => diff(args),
        Command::Top(args) => top(*args),
        Command::Join(args) => match omn::cli::join(&args) {
            Ok(out) => {
                print!("{out}");
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => {
                eprintln!("error: {error}");
                Ok(ExitCode::from(EXIT_REJECTED))
            }
        },
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
    if let Some(refusal) = omn::lazy::cost_refusal(&args) {
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
                let composed = omn::lazy::compose(&args, &catalog, family)?;
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
        !session.launch.is_empty() && matches!(request.target, omn::request::Target::Launch(_))
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
        .map(|band| omn::lazy::band_notes(band, &resolutions))
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
                omn::stack::symbol_readers()
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
fn first_to_fix(each: impl Iterator<Item = omn::collect::Whose>) -> omn::collect::Whose {
    use omn::collect::Whose;
    each.min_by_key(|whose| match whose {
        Whose::Request => 0,
        Whose::Build => 1,
        Whose::Machine => 2,
    })
    .unwrap_or(Whose::Machine)
}

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

/// Whether the collector of `source` on `backend` weighs each sampled event
/// by its period: declared by the collector in the registry
/// (`collect::Factory::weighing_its_samples`), and read here, so that the
/// ladder passes over a `trace` -> `sample` rung whose sampled events would
/// each count once where they stood for their period -- named, with the
/// reason, in the budget's note -- rather than taking it and flagging it
/// after the fact.
fn weighs_its_samples(source: &str, backend: &str) -> bool {
    collect::weighs_its_samples(source, backend)
}

/// The mode rungs a budget passes over because the collector's sampled events
/// would not be weighed, as one sentence for the budget's note.
fn unweighed_mode_rungs(catalog: &Catalog, resolved: &ResolvedRequest) -> Option<String> {
    unweighed_mode_rungs_given(catalog, resolved, weighs_its_samples)
}

/// [`unweighed_mode_rungs`], with which collectors weigh their samples given.
fn unweighed_mode_rungs_given(
    catalog: &Catalog,
    resolved: &ResolvedRequest,
    weighs: fn(&str, &str) -> bool,
) -> Option<String> {
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
                        && !weighs(resolved.request.source.as_str(), &backend.id)
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
    /// The budget of this source is what ended the run: the stop is said on
    /// a line of its own (`MeasuredCost::stop`), whatever the whole window's
    /// figure came to.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stopped_by_budget: bool,
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
        // A table loaded once inside the window is in the cpu said, and not
        // in the percentage, which is the rate a budget weighs.
        let once = if overhead.one_time_cpu_ns > 0 {
            format!(
                " ({} of it once, loading symbol tables, weighed apart from the percentage)",
                span(overhead.one_time_cpu_ns)
            )
        } else {
            String::new()
        };
        format!(
            "{} ({}) via `{}` spent {} of cpu{once}{inside} {against}, over {} of wall \
             time{before}; measuring that cost {} across {} read(s){unknown}",
            self.source,
            self.mode,
            self.served_by.join("` then `"),
            span(overhead.attributed_cpu_ns),
            span(overhead.wall_ns),
            span(overhead.measurement_ns),
            overhead.measurements,
        )
    }

    /// The line that says the budget ended the run, naming the source and
    /// the percentage that broke the ceiling.
    ///
    /// On its own line because the verdict is about the whole window, and a
    /// run stopped part way through can come out under the ceiling over the
    /// window, or not checkable whole, while the check that stopped it was
    /// over: a run ended 941 ms in was reported as `held` and `could not be
    /// checked whole` with nothing saying it had been stopped
    /// (`docs/fixes/w17-python.md`).
    fn stop(&self) -> Option<String> {
        let budget = self.budget_percent?;
        self.stopped_by_budget.then(|| {
            format!(
                "[{}] --budget {budget}% was exceeded, and the run was stopped there, before \
                 its own end: {}",
                self.source, self.stopped_because
            )
        })
    }

    /// The reason the run stopped, as the verdict's tail: nothing when the
    /// budget stopped it, which [`MeasuredCost::stop`] says on its own line.
    fn stopped_tail(&self) -> &str {
        if self.stopped_by_budget {
            "the run was stopped by this budget, as the line above says"
        } else {
            &self.stopped_because
        }
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
                    self.source,
                    self.stopped_tail()
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
                 window; {}",
                self.source,
                self.stopped_tail()
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
    let launches = !session.launch.is_empty()
        || session
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
/// ([`repeat::spread`]). Every run's costs and losses are said as a run's
/// are, numbered; every run's results are kept in the document, and under
/// `-o` in a directory of their own, `run-<i>`.
#[allow(clippy::too_many_arguments)]
fn measure_repeated(
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
                return Ok(refused_at_collect(catalog, &error, resolved));
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
        .filter(|(resolved, _)| matches!(resolved.request.target, omn::request::Target::Launch(_)))
        .map(|(_, results)| results.label.clone())
        .collect();
    let spread = repeat::spread(&runs, &launched);
    let document = repeat::Repeated {
        repeat,
        spread: &spread,
        runs: &runs,
    };
    let output = match session.format {
        OutputFormat::Text => repeat::text(repeat, &spread),
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
fn failures(collections: &[collect::Collection]) -> Vec<String> {
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
fn outcome(failures: &[String]) -> ExitCode {
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
    report_budgets(costs);
    // Every collection carries the one program's lifetime; said once.
    let left = run
        .results
        .iter()
        .find_map(|results| results.lifetime)
        .map_or(0, |life| life.left_running);
    if left > 0 {
        eprintln!(
            "warning: the stop left {left} process(es) the program started running: they \
             could not be signalled (a setuid child refuses this process's signals)"
        );
    }
    let lifetime = run.results.iter().find_map(|results| results.lifetime);
    let adopted = lifetime.map_or(0, |life| life.adopted_ended);
    if adopted > 0 {
        // Stopped, or exited on its own leaving them running: either way the
        // run ended them, and says which end it was.
        let end = if lifetime.is_some_and(|life| life.terminated) {
            "the stop also ended"
        } else {
            "the program exited, and the end of the run then ended"
        };
        eprintln!(
            "note: {end} {adopted} process(es) the program started that had left its tree \
             before it (orphaned, their parent gone first, and adopted by omn)"
        );
    }
    if let Some(comparison) = compare_backends(costs, &run.results) {
        eprint!("{comparison}");
    }
}

/// Each collection's cost and verdict, and the budget's stop on a line of its
/// own when the budget ended the run: said once, before the verdicts, however
/// many collections of the source that stopped it there were.
fn report_budgets(costs: &[MeasuredCost]) {
    let mut said: Vec<String> = Vec::new();
    for stop in costs.iter().filter_map(MeasuredCost::stop) {
        if !said.contains(&stop) {
            eprintln!("budget: {stop}");
            said.push(stop);
        }
    }
    for cost in costs {
        eprintln!("cost: {}", cost.line());
        if let Some(verdict) = cost.verdict() {
            eprintln!("budget: {verdict}");
        }
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

/// Whether a recording ended because the request of `source` under `ceiling`
/// was over its rolling budget.
fn over_budget(stopped_because: &str, source: &str, ceiling: f64) -> bool {
    collect::stopped_by_budget(stopped_because, source, ceiling)
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
    // Only the sources a join or a decomposition reads: every other
    // collection's events are consumed by `aggregate` and never cloned.
    let read = join::sources_read(&session.joins, &session.decompose);
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
        if read.contains(resolved[index].request.source.as_str()) {
            merged.extend(collection.events.iter().cloned());
        }
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
        stopped_by_budget: budget_percent.is_some_and(|ceiling| {
            collect::stopped_by_budget(
                &collection.stopped_because,
                resolved.request.source.as_str(),
                ceiling,
            )
        }),
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
            "{}{}",
            render::list(
                &catalog,
                args.selection.as_deref(),
                &capabilities,
                args.color.style(),
            ),
            omn::lazy::listing(&catalog, args.selection.as_deref())
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
        symbols: args.symbols,
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
        symbols: args.symbols,
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

// ---------------------------------------------------------------------------
// `omn top`
//
// The view is a projection, never a collector. Live, it builds a session --
// the sources its views need, or a session file's -- and runs it through the
// same resolver, ladder and runner as `omn trace`; between rounds a `Tap`
// hands what the runner already holds to the drawing thread. Nothing in the
// collectors changes for it, and a run without `omn top` never reaches here.

/// The longest a live `omn top` runs when no `--duration` is given.
const TOP_DEFAULT_DURATION: &str = "10m";

fn top(args: TopArgs) -> anyhow::Result<ExitCode> {
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
fn view_facts(catalog: &Catalog, data: &mut omn_view::model::Data) {
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
        let Ok(id) = source.parse::<omn::request::SourceId>() else {
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

fn top_load(catalog: &Catalog, path: &Path) -> anyhow::Result<omn_view::model::Data> {
    let mut data = omn_view::model::Data::load(path).map_err(|error| anyhow::anyhow!("{error}"))?;
    view_facts(catalog, &mut data);
    Ok(data)
}

fn top_app(args: &TopArgs, data: omn_view::model::Data) -> anyhow::Result<omn_view::app::App> {
    let mut app = omn_view::app::App::new(data);
    app.gallery = args.gallery.is_some();
    if let Some(name) = &args.view {
        app.open(name).map_err(|error| anyhow::anyhow!(error))?;
    }
    Ok(app)
}

fn top_size(args: &TopArgs) -> anyhow::Result<(u16, u16)> {
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

fn top_replay(catalog: &Catalog, args: &TopArgs, path: &Path) -> anyhow::Result<ExitCode> {
    let data = top_load(catalog, path)?;
    let against = args
        .against
        .as_deref()
        .map(|path| top_load(catalog, path))
        .transpose()?;
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
fn top_session(
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
        omn::request::Target::Launch(args.program.clone())
    } else if !args.pid.is_empty() {
        omn::request::Target::Pid(args.pid.clone())
    } else if let Some(cgroup) = &args.cgroup {
        omn::request::Target::Cgroup(cgroup.clone())
    } else {
        omn::request::Target::System
    };
    let mut requests = Vec::new();
    for need in omn_view::views::all_needs().map_err(|why| anyhow::anyhow!(why))? {
        let refuse = |reason: String| omn_view::model::Refusal {
            label: need.label.clone(),
            source: need.source.to_owned(),
            reason,
        };
        let built = (|| -> Result<omn::request::Request, String> {
            Ok(omn::request::Request {
                source: need.source.parse().map_err(|error| format!("{error}"))?,
                target: match need.scope {
                    omn_view::views::Scope::Target => target.clone(),
                    omn_view::views::Scope::System => omn::request::Target::System,
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

fn top_live(catalog: &Catalog, args: &TopArgs) -> anyhow::Result<ExitCode> {
    if args.headless && args.out.is_none() && !args.dump_session {
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
        return Ok(ExitCode::from(exit_for(omn::collect::Whose::Machine)));
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
            opened_ns: omn::clock::monotonic_ns(),
            now_ns: omn::clock::monotonic_ns(),
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
    let screen = if args.headless {
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
        match collect::run_all_escalating(&resolutions, &mut ladder, Some(&mut tap)) {
            Ok(collections) => break (collections, ladder),
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
    };
    beside.apply_ts_err(&mut ended);
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
    Ok(outcome(&failures))
}

/// Hands what the runner holds to the drawing thread, between rounds.
///
/// An [`collect::Escalate`] that never attaches anything: it is the one place
/// the runner shows a run in progress, and the view only reads. When the user
/// quits, it lets every request go, which ends the run the way a closed
/// window does -- and each collection says it was detached because the view
/// was quit.
struct Tap<'a> {
    /// How many events of each request were handed over already.
    sent: Vec<usize>,
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

    fn new(
        shell: omn_view::model::Data,
        sender: std::sync::mpsc::Sender<omn_view::feed::Update>,
        quit: std::sync::Arc<std::sync::atomic::AtomicBool>,
        view_cpu: Option<&'a mut ViewCpu>,
    ) -> Self {
        Self {
            sent: vec![0; shell.run.results.len()],
            sender,
            quit,
            last: None,
            unheard: view_cpu.is_none(),
            view_cpu,
            shell,
        }
    }

    /// The opening data with what the collectors say, when that changes it
    /// and nothing was handed over yet.
    ///
    /// A source attached after the first events, or drops placed after them,
    /// reach the view at the end: the feed has no update that sets them on a
    /// result already drawing.
    fn describe(&mut self, said: &[collect::Described<'_>]) -> Option<omn_view::model::Data> {
        if self.sent.iter().any(|sent| *sent > 0) {
            return None;
        }
        let mut changed = false;
        for (results, said) in self.shell.run.results.iter_mut().zip(said) {
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
                changed = true;
            }
        }
        changed.then(|| self.shell.clone())
    }

    /// Hands the events read since the last hand-over to the drawing thread.
    fn hand_over(&mut self, run: &collect::Progress<'_>) {
        for (index, events) in run.events.iter().enumerate() {
            let Some(sent) = self.sent.get_mut(index) else {
                continue;
            };
            // A buffer can only grow while the run goes; one that shrank was
            // let go by the runner, and what is in it now is all new.
            if events.len() < *sent {
                *sent = 0;
            }
            if events.len() == *sent {
                continue;
            }
            let fresh = &events[*sent..];
            *sent = events.len();
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
fn to_view_events(
    events: &[omn::event::Event],
) -> (Vec<omn_view::model::Event>, Option<(u64, String)>) {
    let convert = |events: &[omn::event::Event]| {
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
        if let Some(described) = self.describe(said) {
            let restart = omn_view::feed::Update::Restart(Box::new(described));
            if self.sender.send(restart).is_err() {
                self.unheard = true;
            }
        }
    }

    fn decide(&mut self, run: &collect::Progress<'_>) -> Vec<collect::Switch> {
        if self.quit.load(std::sync::atomic::Ordering::SeqCst) {
            return run
                .standing
                .iter()
                .enumerate()
                .filter(|(_, standing)| matches!(standing, collect::Standing::Attached))
                .map(|(index, _)| collect::Switch {
                    index,
                    attach: false,
                    why: "the view was quit".to_owned(),
                })
                .collect();
        }
        if self.unheard || self.last.is_some_and(|last| last.elapsed() < Self::EVERY) {
            return Vec::new();
        }
        self.last = Some(std::time::Instant::now());
        let opened = thread_cpu_ns();
        self.hand_over(run);
        if let Some(view_cpu) = self.view_cpu.as_mut() {
            view_cpu.sample();
            view_cpu.handed_over(thread_cpu_ns().saturating_sub(opened));
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
struct ViewCpu {
    /// The drawing thread's cpu clock (`pthread_getcpuclockid`), readable
    /// from this thread.
    drawing: Option<libc::clockid_t>,
    /// The hand-over's cpu on the runner's thread, a running total the
    /// drawing thread adds to its own figure.
    handover: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The drawing thread's last reading: once it has ended its clock reads
    /// nothing, and what it spent stays spent.
    drawn_ns: u64,
    /// (this process's cpu, the view's cpu so far), in the order taken.
    samples: Vec<(u64, u64)>,
}

impl ViewCpu {
    fn new(
        drawing: &std::thread::JoinHandle<std::io::Result<()>>,
        handover: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        use std::os::unix::thread::JoinHandleExt;
        let mut clock: libc::clockid_t = 0;
        // SAFETY: the thread is joined only after this value's last use, so
        // its handle names a thread that has not been reaped.
        let status = unsafe { libc::pthread_getcpuclockid(drawing.as_pthread_t(), &mut clock) };
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
        self.samples.push((process_cpu_ns(), drawing + handover));
    }

    /// The view's cpu when this process had spent `process_ns`.
    fn at(&self, process_ns: u64) -> u64 {
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
    fn take_out(&self, overhead: &mut collect::Overhead) {
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

fn clock_ns(clock: libc::clockid_t) -> Option<u64> {
    let mut spec = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `spec` is a valid timespec the call writes into.
    (unsafe { libc::clock_gettime(clock, &mut spec) } == 0)
        .then(|| spec.tv_sec as u64 * 1_000_000_000 + spec.tv_nsec as u64)
}

/// The calling thread's cpu.
fn thread_cpu_ns() -> u64 {
    clock_ns(libc::CLOCK_THREAD_CPUTIME_ID).unwrap_or(0)
}

/// This process's cpu, user and system, as `getrusage` -- the account every
/// collection's cost is read from -- gives it.
fn process_cpu_ns() -> u64 {
    // SAFETY: an all-zero rusage is valid, and the call writes into it.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` is a valid rusage the call writes into.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let of = |time: libc::timeval| time.tv_sec as u64 * 1_000_000_000 + time.tv_usec as u64 * 1_000;
    of(usage.ru_utime) + of(usage.ru_stime)
}

/// The resolved requests that cannot share their launch with the others,
/// as the request's error: a family's rule about the library that will
/// serve each one (`collect::gpu::launch_refusals`), so it is asked of the
/// backends the resolver chose rather than of the sources' names.
fn launch_rejections(resolutions: &[ResolvedRequest]) -> Vec<Rejection> {
    let pairs: Vec<(&omn::request::Request, &str)> = resolutions
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
    /// with `mem.faults`'s collector weighing (as `page_faults.rs` does), a
    /// `raw` list is refused up front and a `hist` is not; with it unweighed,
    /// the rung is passed over, nothing can mix, and the note names the rung
    /// and why.
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

        // The shipped collector weighs its samples: the same answers.
        assert!(weighs_its_samples("mem.faults", "pmu:sw"));
        assert_eq!(budget_refusal(&catalog, &raw), Some(refused));
        assert_eq!(budget_refusal(&catalog, &hist), None);
        assert!(
            rungs(&catalog, &hist, 0.01)
                .iter()
                .any(|rung| rung.mode != hist.request.mode),
            "a weighed collector offers the mode rung"
        );

        assert_eq!(unweighed_mode_rungs(&catalog, &hist), None);

        // An unweighed one: the rung is passed over, and the note says why.
        assert_eq!(budget_refusal_given(&catalog, &raw, |_, _| false), None);
        let passed = unweighed_mode_rungs_given(&catalog, &hist, |_, _| false)
            .expect("an unweighed collector's mode rung is passed over");
        assert!(
            passed.contains("`pmu:sw` in `sample@99hz`, is passed over")
                && passed.contains("carry no `weight`"),
            "{passed}"
        );
    }

    /// The view's cpu inside a collection's window is taken out of it, and
    /// what it spent before and after out of those: the three still add up
    /// to the process's account, less the view.
    #[test]
    fn the_view_s_cpu_is_taken_out_of_the_collectors_account() {
        let view_cpu = ViewCpu {
            drawing: None,
            handover: std::sync::Arc::default(),
            drawn_ns: 0,
            // At 100 ms of process cpu the view had spent 10; at 300, 50;
            // at 500, 90.
            samples: vec![(100, 10), (300, 50), (500, 90)],
        };
        assert_eq!(view_cpu.at(200), 30);
        assert_eq!(view_cpu.at(50), 10, "before the first sample: all of it");
        assert_eq!(view_cpu.at(900), 90);
        let mut overhead = collect::Overhead {
            before_window_cpu_ns: 100,
            collector_cpu_ns: 200,
            attributed_cpu_ns: 100,
            after_window_cpu_ns: 200,
            ..Default::default()
        };
        view_cpu.take_out(&mut overhead);
        assert_eq!(
            overhead.collector_cpu_ns, 160,
            "40 of the window was the view's"
        );
        assert_eq!(
            overhead.attributed_cpu_ns, 80,
            "the share keeps its proportion"
        );
        assert_eq!(overhead.before_window_cpu_ns, 90);
        assert_eq!(overhead.after_window_cpu_ns, 160);
    }

    /// A batch with one event that cannot be handed over loses that one,
    /// counted with the reason, and not the rest.
    #[test]
    fn one_event_that_does_not_convert_is_counted_and_the_rest_handed_over() {
        let event = |value: f64| omn::event::Event {
            ts_ns: 1,
            ts_err_ns: 0,
            kind: "x".to_owned(),
            keys: Vec::new(),
            attrs: vec![omn::event::Attr::float("value", value)],
            collector_id: omn::event::CollectorId::new("x"),
            seq: 0,
        };
        let (converted, unconverted) = to_view_events(&[event(1.0), event(f64::NAN), event(2.0)]);
        assert_eq!(converted.len(), 2);
        let (count, why) = unconverted.expect("the NaN is counted");
        assert_eq!(count, 1, "{why}");
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

    /// A collection as a run leaves it, carrying `failure` when given.
    fn collection(failure: Option<&str>) -> collect::Collection {
        let mut collection: collect::Collection = serde_json::from_value(serde_json::json!({
            "events": [],
            "value_attr": "value",
            "losses": collect::Losses::default(),
            "observed_ns": 0,
            "stopped_because": "the test",
        }))
        .expect("a bare collection deserializes");
        collection.failure = failure.map(str::to_owned);
        collection
    }

    /// A collector that broke the program it measured makes the run's
    /// outcome a failure, said once however many of its collections carry
    /// it; a run without one exits 0, and a collection without one writes no
    /// `failure` at all.
    #[test]
    fn a_collector_that_broke_its_program_fails_the_run_once() {
        let killed = "pid 7 died inside omn's injection library";
        let collections = [
            collection(None),
            collection(Some(killed)),
            collection(Some(killed)),
        ];
        assert_eq!(failures(&collections), vec![killed.to_owned()]);
        assert_eq!(outcome(&failures(&collections)), ExitCode::FAILURE);
        assert_eq!(outcome(&failures(&[collection(None)])), ExitCode::SUCCESS);
        let written = serde_json::to_value(collection(None)).unwrap();
        assert!(written.get("failure").is_none(), "{written}");
        let written = serde_json::to_value(collection(Some(killed))).unwrap();
        assert_eq!(written["failure"], killed);
    }

    /// A live view is given what each collector says of its events -- the
    /// value attribute, the row stamp, the drops at their drains -- while no
    /// event was handed over, once per change, and never after: a restart
    /// then would lose what it holds.
    #[test]
    fn a_live_view_is_told_how_to_read_its_results_before_its_first_events() {
        let mut shell = omn_view::model::Data::default();
        shell.run.results = vec![omn_view::model::Results::default(); 2];
        let (sender, _receiver) = std::sync::mpsc::channel();
        let quit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut tap = Tap::new(shell, sender, quit, None);
        let drains = [collect::DrainDrop {
            ts_ns: 40,
            dropped: 3,
        }];
        let said = [
            collect::Described {
                value_attr: "latency_ns",
                row_stamp: Some(collect::RowStamp::DrainClose),
                drains: &drains,
            },
            collect::Described {
                value_attr: "",
                row_stamp: None,
                drains: &[],
            },
        ];
        let described = tap.describe(&said).expect("the first description is given");
        let first = &described.run.results[0];
        assert_eq!(first.value_attr.as_deref(), Some("latency_ns"));
        assert_eq!(first.row_stamp.as_deref(), Some("drain_close"));
        assert_eq!(first.drains.len(), 1);
        assert_eq!(first.drains[0].dropped, 3);
        assert_eq!(described.run.results[1].value_attr, None);
        assert!(tap.describe(&said).is_none(), "an unchanged one is not");
        tap.sent[1] = 5;
        let mut later = said;
        later[1].value_attr = "bytes";
        assert!(
            tap.describe(&later).is_none(),
            "nor any once events went over"
        );
    }
}
