//! `omn`: the command line entry point.
//!
//! Everything here is dispatch and exit codes. The work lives in the library,
//! which is also what a future API would call.

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use serde::Serialize;

use omn::capabilities::Capabilities;
use omn::catalog::Catalog;
use omn::cli::{
    Cli, Command, Defaulted, FormatsArgs, ListArgs, TraceArgs, ValidateArgs, SUBCOMMANDS,
};
use omn::format::{FormatStatus, OutputFormat};
use omn::request::Session;
use omn::resolve::{self, ResolvedRequest};
use omn::validate::{Rejection, Report};
use omn::{collect, emit, encode, join, render, session as session_file, validate};

mod cli {
    pub use omn::cli::*;
}

/// The request was refused: it cannot mean what it asks for.
const EXIT_REJECTED: u8 = 2;
/// The request is valid, but the part of the tool that would answer it does not
/// exist yet.
const EXIT_NOT_IMPLEMENTED: u8 = 3;

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
            return ExitCode::from(u8::from(error.exit_code() != 0));
        }
    };

    match run(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
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
    }
}

/// Loads the catalog, from a directory when one is named, from the binary
/// otherwise.
fn catalog(explicit: Option<&Path>) -> anyhow::Result<Catalog> {
    let from_env = std::env::var_os("OMN_CATALOG_DIR").map(PathBuf::from);
    let dir = explicit.map(Path::to_path_buf).or(from_env);
    match dir {
        Some(dir) => Ok(Catalog::load_dir(&dir)?),
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
            (session, Defaulted::default())
        }
        None => cli::to_session(&args, &catalog)?,
    };

    let report = match validate::validate_session(&catalog, &session) {
        Ok(report) => report,
        Err(rejections) => return Ok(reject(&rejections)),
    };

    // A session written here for another machine is not resolved here: what
    // this machine can do says nothing about what that one can.
    if args.no_resolve {
        print_session(&catalog, &session, &report, &defaulted, &[])?;
        return Ok(ExitCode::SUCCESS);
    }

    // Introspection happens once per invocation: every request is resolved
    // against the same view of the machine, so a session cannot straddle two.
    let capabilities = Capabilities::detect();
    let mut resolutions = Vec::new();
    for request in &session.requests {
        match resolve::resolve(&catalog, &capabilities, request) {
            Ok(resolved) => resolutions.push(resolved),
            Err(error) => {
                eprintln!("error: {error}");
                eprintln!(
                    "  hint: the request is valid; it is this machine that cannot serve it. \
                     Use --no-resolve to write it down for one that can."
                );
                return Ok(ExitCode::from(EXIT_REJECTED));
            }
        }
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

    if args.dump_resolved {
        print_resolved(session.format, &resolutions)?;
        return Ok(ExitCode::SUCCESS);
    }

    if args.dump_request {
        print_session(&catalog, &session, &report, &defaulted, &resolutions)?;
        return Ok(ExitCode::SUCCESS);
    }

    measure(&catalog, &session, &report, &defaulted, &resolutions)
}

/// Whether a refusal means "this build does not do that yet" rather than
/// "this run failed".
///
/// The two exit differently on purpose. A caller that ran out of memory, lost
/// its target or could not open a counter has a problem with this run; a
/// caller that asked for a mechanism nobody has written has a problem with
/// this build, and retrying, raising privileges or changing the machine will
/// not help. `NoCollector` has always said so with exit 3. A target a
/// collector cannot serve *yet* -- the `yet` is in the message -- is the same
/// answer and used to exit 1, indistinguishable from a real failure.
fn not_built_yet(error: &collect::CollectError) -> bool {
    matches!(
        error,
        collect::CollectError::NoCollector { .. } | collect::CollectError::UnsupportedTarget { .. }
    )
}

/// What `--budget` did, which today is nothing, said out loud.
///
/// The flag is parsed, validated and carried into the resolved request, and
/// the resolver does not yet consult it. A reader who passes `--budget 1%` and
/// sees a measurement come back has been told, by omission, that the overhead
/// was under one percent -- so the omission is the bug, not the missing
/// enforcement. Until phase 8 measures what a backend actually costs, this
/// says what the declared costs imply and what enforcement would have to
/// change.
///
/// The comparison is against the *worst case* of the band a backend declares:
/// `med` means "1 to 5 %", so it fits a 5 % budget and cannot be promised
/// against a 2 % one. Erring the other way would let a budget look satisfied
/// by a backend that may well exceed it.
fn budget_note(catalog: &Catalog, resolved: &ResolvedRequest) -> Option<String> {
    let budget = resolved.request.budget?;
    let source = &resolved.request.source;
    let selected = &resolved.backend;
    let ceiling = budget.percent();

    if worst_case_percent(selected.cost) <= ceiling {
        return Some(format!(
            "[{source}] --budget {budget} is recorded and not yet enforced; `{}` declares \
             `{}` ({}), which is within it, so enforcing it would change nothing here",
            selected.id,
            selected.cost,
            band(selected.cost),
        ));
    }

    // Only the backends below the selected one are candidates: the ones above
    // were rejected by this machine, and a budget cannot make them reachable.
    let cheaper: Vec<String> = catalog
        .get(source)
        .into_iter()
        .flat_map(|entry| entry.backends.iter())
        .skip_while(|backend| backend.id != selected.id)
        .skip(1)
        .filter(|backend| worst_case_percent(backend.cost) <= ceiling)
        .map(|backend| format!("`{}` ({})", backend.id, backend.cost))
        .collect();

    Some(match cheaper.as_slice() {
        [] => format!(
            "[{source}] --budget {budget} is recorded and not yet enforced; `{}` declares `{}` \
             ({}), and nothing below it in the chain declares less, so enforcement could only \
             refuse this request",
            selected.id,
            selected.cost,
            band(selected.cost),
        ),
        candidates => format!(
            "[{source}] --budget {budget} is recorded and not yet enforced; `{}` declares `{}` \
             ({}), so enforcement would degrade onto {} -- which was never probed, because a \
             better backend was selected first",
            selected.id,
            selected.cost,
            band(selected.cost),
            candidates.join(" or "),
        ),
    })
}

/// The top of the overhead band a declared cost stands for, as `catalog::Cost`
/// documents it. `extreme` is "over 10x" and no percentage bounds it.
fn worst_case_percent(cost: omn::catalog::Cost) -> f64 {
    use omn::catalog::Cost;
    match cost {
        Cost::Free => 0.0,
        Cost::Low => 1.0,
        Cost::Med => 5.0,
        Cost::High => 20.0,
        Cost::Extreme => f64::INFINITY,
    }
}

fn band(cost: omn::catalog::Cost) -> &'static str {
    use omn::catalog::Cost;
    match cost {
        Cost::Free => "about zero",
        Cost::Low => "under 1 %",
        Cost::Med => "1 to 5 %",
        Cost::High => "5 to 20 %",
        Cost::Extreme => "over 10x",
    }
}

/// Runs the session's requests over one window and prints what they produced.
fn measure(
    catalog: &Catalog,
    session: &Session,
    report: &Report,
    defaulted: &Defaulted,
    resolved: &[ResolvedRequest],
) -> anyhow::Result<ExitCode> {
    for note in defaulted.iter() {
        eprintln!("note: {note}");
    }
    for warning in report.warnings() {
        eprintln!("warning: {warning}");
    }

    let collections = match collect::run_all(resolved) {
        Ok(collections) => collections,
        Err(error) if not_built_yet(&error) => {
            eprintln!("error: {error}");
            // The selected backend is often the last one standing, and why the
            // better ones were passed over is the answer the reader actually
            // needs: a counter that needs root reads as "no collector for
            // `pmu`" unless the chain's own verdicts come with it.
            if matches!(error, collect::CollectError::NoCollector { .. }) {
                for resolved in resolved {
                    for considered in &resolved.chain {
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
            eprintln!(
                "\nThe request itself is valid -- `--dump-request` shows how it would be \
                 obtained on a build that has it."
            );
            return Ok(ExitCode::from(EXIT_NOT_IMPLEMENTED));
        }
        Err(error) => anyhow::bail!(error),
    };

    // Every source's events go into one stream for the joins: they were
    // collected over one window against one target, which is what makes them
    // comparable in the first place.
    let mut merged = Vec::new();
    let mut run = encode::RunOutput {
        results: Vec::new(),
        joins: Vec::new(),
    };

    for (index, collection) in collections.into_iter().enumerate() {
        let resolved = &resolved[index];
        merged.extend(collection.events.iter().cloned());
        let unit = catalog
            .get(&resolved.request.source)
            .map(|entry| entry.unit)
            .unwrap_or(omn::catalog::Unit::Count);
        let value_attr = collection.value_attr.clone();
        run.results
            .push(emit::aggregate(resolved, collection, unit, &value_attr)?);
    }

    for rule in &session.joins {
        run.joins.push(join::apply(rule, &merged)?);
    }

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

    for caveat in report.caveats() {
        eprintln!("caveat: {caveat}");
    }

    if let Some(directory) = &session.out {
        write_artifacts(directory, session.format, resolved, &output)?;
        eprintln!(
            "\nwrote results and the resolved requests to {}",
            directory.display()
        );
    }

    Ok(ExitCode::SUCCESS)
}

/// Writes the numbers and, beside them, the record of how they were obtained.
///
/// The resolved requests are always written, in JSON, whatever the results
/// format is: results without them are numbers whose provenance is gone.
fn write_artifacts(
    directory: &Path,
    format: OutputFormat,
    resolved: &[ResolvedRequest],
    encoded: &[u8],
) -> anyhow::Result<()> {
    std::fs::create_dir_all(directory)?;
    std::fs::write(
        directory.join("resolved.json"),
        serde_json::to_string_pretty(resolved)?,
    )?;
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
            print!("{}", format.serialize_document(&selected)?);
        }
        format => anyhow::bail!(
            "`{format}` describes results; the catalog lists as text, json, yaml or toml"
        ),
    }
    Ok(ExitCode::SUCCESS)
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
}
