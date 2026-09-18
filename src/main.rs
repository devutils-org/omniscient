//! `omn`: the command line entry point.
//!
//! Everything here is dispatch and exit codes. The work lives in the library,
//! which is also what a future API would call.

use std::ffi::OsString;
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

fn main() -> ExitCode {
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
        Err(error @ collect::CollectError::NoCollector { .. }) => {
            eprintln!("error: {error}");
            // The selected backend is often the last one standing, and why the
            // better ones were passed over is the answer the reader actually
            // needs: a counter that needs root reads as "no collector for
            // `pmu`" unless the chain's own verdicts come with it.
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
                "\nCollectors exist for: {}.\n\
                 The request itself is valid -- `--dump-request` shows how it would be obtained.",
                collect::implemented().join(", ")
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

    let output = encode::run_output(session.format, &run)?;
    print!("{output}");

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
    encoded: &str,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(directory)?;
    std::fs::write(
        directory.join("resolved.json"),
        serde_json::to_string_pretty(resolved)?,
    )?;
    let extension = match format {
        OutputFormat::Text => "txt",
        other => other.as_str(),
    };
    std::fs::write(directory.join(format!("results.{extension}")), encoded)?;
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
