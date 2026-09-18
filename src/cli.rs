//! The command line surface.
//!
//! The CLI builds a [`Session`] and hands it on. It attaches nothing, opens no
//! perf event and loads no program: that keeps this file honest about what it
//! is, which is one of the two ways a request can be written down. The other is
//! a session file, and both produce the same object.
//!
//! Where the catalog leaves no choice, the CLI makes none: if a source declares
//! exactly one mode, one emit or one target kind, omitting the flag picks it and
//! says so. Where the catalog offers several, the flag is required. Guessing
//! between two legitimate answers is how a tool ends up reporting one number
//! while the user reads it as another.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::catalog::Catalog;
use crate::error::{one_of, ParseError};
use crate::filter::FilterExpr;
use crate::format::{InputFormat, OutputFormat};
use crate::request::{
    BackendOverride, Budget, CpuSet, Emit, KeyKind, Mode, Request, Session, SourceId, StackMode,
    Target,
};
use crate::scalar::Duration;

#[derive(Debug, Parser)]
#[command(
    name = "omn",
    version,
    about = "Omniscient: one vocabulary for system observability",
    long_about = "Omniscient describes a measurement along five axes -- source, target, mode, emit \
                  and keys -- and refuses the combinations that cannot mean anything.\n\n\
                  A bare `omn <source> ...` is short for `omn trace <source> ...`.",
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Describe a measurement, from the command line or from a session file.
    Trace(Box<TraceArgs>),
    /// Project the catalog: every source, or one family, or one source in full.
    List(ListArgs),
    /// Show the input and output formats, and what each one can carry.
    Formats(FormatsArgs),
    /// Check a session file against the catalog without running it.
    Validate(ValidateArgs),
}

/// The subcommand names, used to tell `omn io.latency` from `omn list`.
pub const SUBCOMMANDS: [&str; 5] = ["trace", "list", "formats", "validate", "help"];

#[derive(Debug, Args)]
pub struct TraceArgs {
    /// Source to measure, as <family>.<metric>, for instance io.latency.
    #[arg(value_name = "SOURCE", required_unless_present = "file")]
    pub source: Option<SourceId>,

    /// Read the session from a file, or from standard input with `-`.
    #[arg(short = 'f', long, value_name = "PATH", conflicts_with = "source")]
    pub file: Option<PathBuf>,

    /// Format of the session file, when the extension does not say.
    #[arg(long, value_name = "FORMAT", requires = "file")]
    pub input_format: Option<InputFormat>,

    /// Existing process or processes.
    #[arg(long, value_name = "PID", value_delimiter = ',', group = "target")]
    pub pid: Vec<i32>,

    /// Existing thread or threads.
    #[arg(long, value_name = "TID", value_delimiter = ',', group = "target")]
    pub tid: Vec<i32>,

    /// A cgroup v2 path.
    #[arg(long, value_name = "PATH", group = "target")]
    pub cgroup: Option<PathBuf>,

    /// Cores, written as the kernel writes them: 0-3,8.
    #[arg(long, value_name = "SET", group = "target")]
    pub cpu: Option<CpuSet>,

    /// A network interface.
    #[arg(long, value_name = "NAME", group = "target")]
    pub iface: Option<String>,

    /// A container, resolved to a cgroup plus a pid namespace.
    #[arg(long, value_name = "NAME", group = "target")]
    pub container: Option<String>,

    /// The whole machine.
    #[arg(long, group = "target")]
    pub system: bool,

    /// Program to launch and follow, after `--`.
    #[arg(last = true, value_name = "PROGRAM", group = "target")]
    pub program: Vec<String>,

    /// How to observe: count, sample@997hz, trace, poll@100ms, snapshot.
    #[arg(short = 'm', long, value_name = "MODE")]
    pub mode: Option<Mode>,

    /// What comes out: raw, hist[:log2|:linear:WIDTH], rate, topk[:N], stack, timeline, summary.
    #[arg(short = 'e', long, value_name = "EMIT", value_delimiter = ',')]
    pub emit: Vec<Emit>,

    /// Correlation keys to attach.
    #[arg(short = 'k', long, value_name = "KEY", value_delimiter = ',')]
    pub keys: Vec<KeyKind>,

    /// Predicate, pushed into the kernel when the backend allows it.
    #[arg(long, value_name = "EXPR", allow_hyphen_values = true)]
    pub filter: Option<FilterExpr>,

    /// How long to observe.
    #[arg(short = 'd', long, value_name = "DURATION")]
    pub duration: Option<Duration>,

    /// Overhead ceiling; the resolver degrades to stay under it and reports what it degraded.
    #[arg(short = 'b', long, value_name = "PERCENT")]
    pub budget: Option<Budget>,

    /// Stack unwinding mode.
    #[arg(long, value_name = "MODE")]
    pub stack: Option<StackMode>,

    /// Pin a backend instead of resolving one. For the comparison bench only.
    #[arg(long, value_name = "force:ID")]
    pub backend: Option<BackendOverride>,

    /// Where results and the resolved file are written.
    #[arg(short = 'o', long, value_name = "DIR")]
    pub out: Option<PathBuf>,

    /// Output format.
    #[arg(long, value_name = "FORMAT", default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// Print the request and how it resolves, then exit without measuring.
    #[arg(long)]
    pub dump_request: bool,

    /// Print the resolved request -- the record to keep beside the numbers --
    /// then exit without measuring.
    #[arg(long, conflicts_with = "no_resolve")]
    pub dump_resolved: bool,

    /// Do not consult this machine. For writing a session meant to run
    /// somewhere else, where what this machine can do says nothing.
    #[arg(long)]
    pub no_resolve: bool,

    /// Name this request in the output.
    #[arg(long, value_name = "NAME")]
    pub label: Option<String>,

    /// Load the catalog from this directory instead of the built-in one.
    #[arg(long, value_name = "DIR", global = true)]
    pub catalog: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// A family (`io`) or a source (`io.latency`). Everything, when omitted.
    #[arg(value_name = "FAMILY|SOURCE")]
    pub selection: Option<String>,

    /// Output format.
    #[arg(long, value_name = "FORMAT", default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// Load the catalog from this directory instead of the built-in one.
    #[arg(long, value_name = "DIR")]
    pub catalog: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct FormatsArgs {
    /// Output format.
    #[arg(long, value_name = "FORMAT", default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct ValidateArgs {
    /// Session file to check, or `-` for standard input.
    #[arg(short = 'f', long, value_name = "PATH")]
    pub file: PathBuf,

    /// Format of the session file, when the extension does not say.
    #[arg(long, value_name = "FORMAT")]
    pub input_format: Option<InputFormat>,

    /// Output format for the checked session.
    #[arg(long, value_name = "FORMAT", default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// Load the catalog from this directory instead of the built-in one.
    #[arg(long, value_name = "DIR")]
    pub catalog: Option<PathBuf>,
}

/// What the CLI decided on the user's behalf, because the catalog left one
/// option. Reported with the request so the choice is never invisible.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Defaulted(Vec<String>);

impl Defaulted {
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Turns command line arguments into the same session a file would produce.
pub fn to_session(args: &TraceArgs, catalog: &Catalog) -> Result<(Session, Defaulted), ParseError> {
    let source = args
        .source
        .clone()
        .ok_or_else(|| ParseError::new("no source given"))?;
    let entry = catalog.get(&source);
    let mut defaulted = Defaulted::default();

    let target = match target_from(args) {
        Some(target) => target,
        None => {
            let Some(entry) = entry else {
                return Err(ParseError::new(format!(
                    "`{source}` needs a target, and it is not in the catalog to suggest one"
                )));
            };
            let supported = entry.supported_targets();
            match supported.as_slice() {
                [only] => {
                    defaulted.0.push(format!(
                        "target defaulted to `{only}`, the only one `{source}` accepts"
                    ));
                    match only {
                        crate::request::TargetKind::System => Target::System,
                        other => {
                            return Err(ParseError::new(format!(
                                "`{source}` is measured per {other}: name one with --{other}"
                            )))
                        }
                    }
                }
                _ => {
                    return Err(ParseError::new(format!(
                        "`{source}` needs a target: one of {}",
                        one_of(supported.iter().map(|kind| kind.as_str()))
                    )))
                }
            }
        }
    };

    let mode = match args.mode {
        Some(mode) => mode,
        None => {
            let Some(entry) = entry else {
                return Err(ParseError::new(format!("`{source}` needs --mode")));
            };
            match entry.modes.as_slice() {
                [only] => {
                    let mode = fill_mode(*only);
                    defaulted.0.push(format!(
                        "mode defaulted to `{mode}`, the only one `{source}` supports"
                    ));
                    mode
                }
                modes => {
                    return Err(ParseError::new(format!(
                        "`{source}` needs --mode: one of {}",
                        one_of(modes.iter().map(|mode| mode.as_str()))
                    )))
                }
            }
        }
    };

    let emit = if !args.emit.is_empty() {
        args.emit.clone()
    } else {
        let Some(entry) = entry else {
            return Err(ParseError::new(format!("`{source}` needs --emit")));
        };
        let usable: Vec<_> = entry
            .emits
            .iter()
            .copied()
            .filter(|emit| mode.kind().valid_emits().contains(emit))
            .collect();
        match usable.as_slice() {
            [only] => {
                let emit = fill_emit(*only);
                defaulted.0.push(format!(
                    "emit defaulted to `{emit}`, the only one `{source}` produces in `{}` mode",
                    mode.kind()
                ));
                vec![emit]
            }
            _ => Vec::new(),
        }
    };

    let request = Request {
        source,
        target,
        mode,
        emit,
        keys: args.keys.clone(),
        filter: args.filter.clone(),
        duration: args.duration,
        budget: args.budget,
        stack: args.stack,
        backend: args.backend.clone(),
        label: args.label.clone(),
    };

    Ok((
        Session::single(request, args.format, args.out.clone()),
        defaulted,
    ))
}

fn target_from(args: &TraceArgs) -> Option<Target> {
    if !args.program.is_empty() {
        return Some(Target::Launch(args.program.clone()));
    }
    if !args.pid.is_empty() {
        return Some(Target::Pid(args.pid.clone()));
    }
    if !args.tid.is_empty() {
        return Some(Target::Tid(args.tid.clone()));
    }
    if let Some(path) = &args.cgroup {
        return Some(Target::Cgroup(path.clone()));
    }
    if let Some(set) = &args.cpu {
        return Some(Target::Cpu(set.clone()));
    }
    if let Some(name) = &args.iface {
        return Some(Target::Iface(name.clone()));
    }
    if let Some(name) = &args.container {
        return Some(Target::Container(name.clone()));
    }
    args.system.then_some(Target::System)
}

/// Gives a bare mode kind its default argument.
fn fill_mode(kind: crate::request::ModeKind) -> Mode {
    use crate::request::ModeKind;
    match kind {
        ModeKind::Count => Mode::Count,
        ModeKind::Trace => Mode::Trace,
        ModeKind::Snapshot => Mode::Snapshot,
        ModeKind::Sample => Mode::Sample {
            freq: Mode::DEFAULT_SAMPLE_FREQ,
        },
        ModeKind::Poll => Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        },
    }
}

/// Gives a bare emit kind its default argument.
fn fill_emit(kind: crate::request::EmitKind) -> Emit {
    use crate::request::EmitKind;
    match kind {
        EmitKind::Raw => Emit::Raw,
        EmitKind::Hist => Emit::Hist(crate::request::HistScale::Log2),
        EmitKind::Rate => Emit::Rate,
        EmitKind::TopK => Emit::TopK(Emit::DEFAULT_TOPK),
        EmitKind::Stack => Emit::Stack,
        EmitKind::Timeline => Emit::Timeline,
        EmitKind::Summary => Emit::Summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn catalog() -> Catalog {
        Catalog::embedded().unwrap()
    }

    fn parse(args: &[&str]) -> TraceArgs {
        let cli = Cli::try_parse_from(args).expect("arguments should parse");
        match cli.command {
            Command::Trace(args) => *args,
            other => panic!("expected a trace command, got {other:?}"),
        }
    }

    #[test]
    fn a_full_command_line_becomes_one_request() {
        let args = parse(&[
            "omn",
            "trace",
            "io.latency",
            "--pid",
            "42,43",
            "--mode",
            "trace",
            "--emit",
            "hist,topk:5",
            "--keys",
            "blk,task",
            "--filter",
            "bytes > 4KiB",
            "--duration",
            "30s",
            "--budget",
            "2%",
        ]);
        let (session, defaulted) = to_session(&args, &catalog()).unwrap();
        let request = &session.requests[0];
        assert_eq!(request.target, Target::Pid(vec![42, 43]));
        assert_eq!(request.emit.len(), 2);
        assert_eq!(request.keys, vec![KeyKind::Blk, KeyKind::Task]);
        assert_eq!(request.filter.as_ref().unwrap().to_string(), "bytes > 4KiB");
        assert!(defaulted.is_empty());
    }

    #[test]
    fn a_program_after_the_separator_is_a_launch_target() {
        let args = parse(&[
            "omn",
            "trace",
            "cpu.profile",
            "--mode",
            "sample",
            "--emit",
            "stack",
            "--",
            "./bench",
            "-n",
            "5",
        ]);
        let (session, _) = to_session(&args, &catalog()).unwrap();
        assert_eq!(
            session.requests[0].target,
            Target::Launch(vec!["./bench".into(), "-n".into(), "5".into()])
        );
    }

    #[test]
    fn a_single_possible_mode_is_filled_in_and_reported() {
        // `cpu.profile` only samples, so there is nothing to choose between.
        let args = parse(&[
            "omn",
            "trace",
            "cpu.profile",
            "--pid",
            "1",
            "--emit",
            "stack",
        ]);
        let (session, defaulted) = to_session(&args, &catalog()).unwrap();
        assert_eq!(
            session.requests[0].mode,
            Mode::Sample {
                freq: Mode::DEFAULT_SAMPLE_FREQ
            }
        );
        assert!(
            defaulted.iter().any(|note| note.contains("mode defaulted")),
            "{defaulted:?}"
        );
    }

    #[test]
    fn a_source_with_several_modes_asks_instead_of_choosing() {
        let args = parse(&[
            "omn", "trace", "mem.rss", "--pid", "1", "--emit", "timeline",
        ]);
        let error = to_session(&args, &catalog()).unwrap_err();
        assert!(error.message().contains("poll, snapshot"), "{error}");
    }

    #[test]
    fn an_ambiguous_mode_is_asked_for_rather_than_guessed() {
        let args = parse(&["omn", "trace", "io.latency", "--system", "--emit", "hist"]);
        let error = to_session(&args, &catalog()).unwrap_err();
        assert!(error.message().contains("needs --mode"), "{error}");
        assert!(error.message().contains("trace"), "{error}");
    }

    #[test]
    fn two_targets_at_once_are_refused_by_the_parser() {
        let result = Cli::try_parse_from(["omn", "trace", "io.latency", "--pid", "1", "--system"]);
        assert!(result.is_err());
    }

    #[test]
    fn a_missing_target_lists_the_ones_the_source_accepts() {
        let args = parse(&[
            "omn",
            "trace",
            "io.latency",
            "--mode",
            "trace",
            "--emit",
            "hist",
        ]);
        let error = to_session(&args, &catalog()).unwrap_err();
        assert!(error.message().contains("needs a target"), "{error}");
    }

    #[test]
    fn a_source_and_a_file_cannot_both_describe_the_session() {
        let result = Cli::try_parse_from(["omn", "trace", "io.latency", "-f", "session.yml"]);
        assert!(result.is_err());
    }
}
