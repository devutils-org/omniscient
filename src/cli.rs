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
//!
//! The same rule decides what may be written beside `-f`. A session file can
//! hold many requests, so a flag that describes *one* request -- a target, a
//! mode, an emit, a filter, a key, a stack mode, a backend, a label -- cannot
//! mean anything next to a file, and is refused by name rather than accepted
//! and dropped. What is left is exactly what a `Session` carries for the whole
//! of itself: the duration, the budget, the output format and the output
//! directory. Those are overrides, and the run says so.

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
use crate::style::ColorChoice;

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
    /// Why is this slow: where one task's wall clock went, and what dominates.
    Latency(LatencyArgs),
    /// What does this queue for: utilization, saturation and errors per resource.
    Saturate(SaturateArgs),
    /// Two profiles and a subtraction: which function is the difference.
    Diff(DiffArgs),
}

/// The subcommand names, used to tell `omn io.latency` from `omn list`.
pub const SUBCOMMANDS: [&str; 8] = [
    "trace", "list", "formats", "validate", "latency", "saturate", "diff", "help",
];

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
    #[arg(
        long,
        value_name = "PID",
        value_delimiter = ',',
        group = "target",
        conflicts_with = "file"
    )]
    pub pid: Vec<i32>,

    /// Existing thread or threads.
    #[arg(
        long,
        value_name = "TID",
        value_delimiter = ',',
        group = "target",
        conflicts_with = "file"
    )]
    pub tid: Vec<i32>,

    /// A cgroup v2 path.
    #[arg(long, value_name = "PATH", group = "target", conflicts_with = "file")]
    pub cgroup: Option<PathBuf>,

    /// Cores, written as the kernel writes them: 0-3,8.
    #[arg(long, value_name = "SET", group = "target", conflicts_with = "file")]
    pub cpu: Option<CpuSet>,

    /// A network interface.
    #[arg(long, value_name = "NAME", group = "target", conflicts_with = "file")]
    pub iface: Option<String>,

    /// A container, measured as the cgroup its runtime says it runs in
    /// (docker, podman or nerdctl, or OMN_CONTAINER_RUNTIME).
    #[arg(long, value_name = "NAME", group = "target", conflicts_with = "file")]
    pub container: Option<String>,

    /// The whole machine.
    #[arg(long, group = "target", conflicts_with = "file")]
    pub system: bool,

    /// Program to launch and follow, after `--`.
    #[arg(
        last = true,
        value_name = "PROGRAM",
        group = "target",
        conflicts_with = "file"
    )]
    pub program: Vec<String>,

    /// How to observe: count, sample@997hz, trace, poll@100ms, snapshot.
    #[arg(short = 'm', long, value_name = "MODE", conflicts_with = "file")]
    pub mode: Option<Mode>,

    /// What comes out: raw, hist[:log2|:linear:WIDTH], rate, topk[:N], stack, timeline, summary.
    #[arg(
        short = 'e',
        long,
        value_name = "EMIT",
        value_delimiter = ',',
        conflicts_with = "file"
    )]
    pub emit: Vec<Emit>,

    /// Correlation keys to attach.
    #[arg(
        short = 'k',
        long,
        value_name = "KEY",
        value_delimiter = ',',
        conflicts_with = "file"
    )]
    pub keys: Vec<KeyKind>,

    /// Predicate, pushed into the kernel when the backend allows it.
    #[arg(
        long,
        value_name = "EXPR",
        allow_hyphen_values = true,
        conflicts_with = "file"
    )]
    pub filter: Option<FilterExpr>,

    /// How long to observe.
    #[arg(short = 'd', long, value_name = "DURATION")]
    pub duration: Option<Duration>,

    /// Overhead ceiling, as a percentage of the observed workload's own CPU time.
    /// Enforced against a measurement taken during the run: over it, the collection
    /// moves to a cheaper backend, then from trace to sample where the sampled events
    /// are weighed by their period, or the run stops, and says which.
    #[arg(short = 'b', long, value_name = "PERCENT")]
    pub budget: Option<Budget>,

    /// Stack unwinding mode.
    #[arg(long, value_name = "MODE", conflicts_with = "file")]
    pub stack: Option<StackMode>,

    /// Where stack names come from: `local` (default), or `debuginfod` -- the
    /// servers in DEBUGINFOD_URLS, asked by build id for a stripped file.
    #[arg(long, value_name = "SOURCE", conflicts_with = "file")]
    pub symbols: Option<crate::stack::unwind::SymbolSource>,

    /// Pin a backend instead of resolving one. For the comparison bench only.
    #[arg(long, value_name = "force:ID", conflicts_with = "file")]
    pub backend: Option<BackendOverride>,

    /// When, inside the run, this request is attached and detached: `1s..3s`,
    /// `1s..` or `..2s`. Absent is the whole run. Parsed by the same code as a
    /// session file's `window`, so the two refuse the same things in the same
    /// words.
    #[arg(long, value_name = "START..STOP", conflicts_with = "file")]
    pub window: Option<crate::request::Window>,

    /// Record until a predicate fires: the run keeps only what `--retain`
    /// holds, and writes one document each time this predicate, in the
    /// filter grammar, becomes true over a round of the request. `value` is
    /// the request's measured value. Needs --retain, --documents and
    /// --duration, the longest the recording may run.
    #[arg(
        long,
        value_name = "EXPR",
        allow_hyphen_values = true,
        conflicts_with = "file"
    )]
    pub until: Option<FilterExpr>,

    /// How far back a recorded document reaches from its trigger; what is
    /// older is evicted and counted.
    #[arg(
        long,
        value_name = "DURATION",
        requires = "until",
        conflicts_with = "file"
    )]
    pub retain: Option<Duration>,

    /// How long a recorded document goes on after its trigger before it is
    /// written. Zero when absent.
    #[arg(
        long,
        value_name = "DURATION",
        requires = "until",
        conflicts_with = "file"
    )]
    pub after: Option<Duration>,

    /// The most documents a recording writes; the run ends with the last.
    #[arg(
        long,
        value_name = "COUNT",
        requires = "until",
        conflicts_with = "file"
    )]
    pub documents: Option<u32>,

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
    #[arg(long, value_name = "NAME", conflicts_with = "file")]
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

    /// When to colour the listing: auto, always or never.
    #[arg(long, value_name = "WHEN", default_value_t = ColorChoice::Auto)]
    pub color: ColorChoice,

    /// Answer every raw-tracepoint requirement by attaching to it, not by
    /// reading the kernel's BTF. Slower by about a second, and the only way to
    /// know a backend will still be there once a target has been launched.
    #[arg(long)]
    pub verify_attach: bool,
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

/// `omn latency`: thread state analysis, asked as a question.
///
/// The objective is required and never defaulted: median latency and the tail
/// do not name the same culprit, and neither names the one throughput or
/// energy would.
#[derive(Debug, Args)]
pub struct LatencyArgs {
    /// What the answer is for: latency (the median run) or tail (the slowest run
    /// against the median). Required.
    #[arg(long, value_name = "OBJECTIVE")]
    pub objective: Option<crate::triage::Objective>,

    /// A process already running. Needs --duration.
    #[arg(long, value_name = "PID", group = "latency_target")]
    pub pid: Option<i32>,

    /// Program to launch and decompose, after `--`.
    #[arg(last = true, value_name = "PROGRAM", group = "latency_target")]
    pub program: Vec<String>,

    /// How many times to run a launched program. The tail needs at least 3.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub runs: u32,

    /// How long to observe: required with --pid, a ceiling on a launched program.
    #[arg(short = 'd', long, value_name = "DURATION")]
    pub duration: Option<Duration>,

    /// The on-cpu profile's rate. Its period is the on-cpu row's resolution.
    #[arg(long, value_name = "HZ", default_value_t = 997)]
    pub hz: u32,

    /// Where the stacks' names come from: `local` (default), or `debuginfod`
    /// -- the servers in DEBUGINFOD_URLS, asked by build id for a stripped
    /// file. Set on every request of the question that names its stacks
    /// through a collector reading it (`cpu.profile`'s today); the others
    /// keep their own names, and the verdict's notes say where each name
    /// came from.
    #[arg(long, value_name = "SOURCE")]
    pub symbols: Option<crate::stack::unwind::SymbolSource>,

    /// Output format: text, json or yaml.
    #[arg(long, value_name = "FORMAT", default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// Print the session this question is asked through, then exit. `omn -f`
    /// runs it and prints the tables the answer is read from.
    #[arg(long)]
    pub dump_session: bool,

    /// Load the catalog from this directory instead of the built-in one.
    #[arg(long, value_name = "DIR")]
    pub catalog: Option<PathBuf>,
}

/// `omn saturate`: the USE method, with Little's law as its arithmetic.
#[derive(Debug, Args)]
pub struct SaturateArgs {
    /// What the answer is for: throughput, latency, tail or energy. Required.
    #[arg(long, value_name = "OBJECTIVE")]
    pub objective: Option<crate::triage::Objective>,

    /// A process already running. Needs --duration.
    #[arg(long, value_name = "PID", group = "saturate_target")]
    pub pid: Option<i32>,

    /// Program to launch and follow, after `--`.
    #[arg(last = true, value_name = "PROGRAM", group = "saturate_target")]
    pub program: Vec<String>,

    /// How long to observe: required with --pid, a ceiling on a launched program.
    #[arg(short = 'd', long, value_name = "DURATION")]
    pub duration: Option<Duration>,

    /// Where stack names come from: `local` (default), or `debuginfod`. The
    /// same flag as `omn latency`'s, so one command line can ask both
    /// questions; set on every request of this mode whose collector reads it.
    /// The USE method names resources, not functions: no request it writes
    /// today names a stack, and the answer says so when this is given.
    #[arg(long, value_name = "SOURCE")]
    pub symbols: Option<crate::stack::unwind::SymbolSource>,

    /// Output format: text, json or yaml.
    #[arg(long, value_name = "FORMAT", default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// Print the session this question is asked through, then exit.
    #[arg(long)]
    pub dump_session: bool,

    /// Load the catalog from this directory instead of the built-in one.
    #[arg(long, value_name = "DIR")]
    pub catalog: Option<PathBuf>,
}

/// `omn diff`: a differential profile.
#[derive(Debug, Args)]
pub struct DiffArgs {
    /// The profile before: a run's `-o DIR`, its results.json, or a folded file.
    #[arg(value_name = "BEFORE")]
    pub before: PathBuf,

    /// The profile after, in any of the same three shapes.
    #[arg(value_name = "AFTER")]
    pub after: PathBuf,

    /// Output format: text, json, or folded -- the three-column
    /// `stack before after` file differential flame graphs are drawn from.
    #[arg(long, value_name = "FORMAT", default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// How many functions each table lists.
    #[arg(long, value_name = "N", default_value_t = 12)]
    pub top: usize,
}

/// What the CLI decided on the user's behalf, because the catalog left one
/// option. Reported with the request so the choice is never invisible.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Defaulted(Vec<String>);

impl Defaulted {
    /// Records something the command line decided that the request itself does
    /// not show: a default the catalog left no choice about, or a session-wide
    /// value a flag replaced.
    pub fn record(&mut self, note: impl Into<String>) {
        self.0.push(note.into());
    }

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
                    defaulted.record(format!(
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
                    defaulted.record(format!(
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
                defaulted.record(format!(
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
        symbols: args.symbols,
        backend: args.backend.clone(),
        label: args.label.clone(),
        window: args.window,
    };

    let mut session = Session::single(request, args.format, args.out.clone());
    // `--window on <layer>` parses, because the two surfaces share one
    // parser; it cannot mean anything on one request, which has no cheap
    // pass beside it to decide from, and is refused here in the words a
    // session file gets.
    session.check_escalation()?;
    // A recording on the command line is a session's `record:` block on its
    // one request, refused in the same words.
    if let Some(trigger) = &args.until {
        session.record = Some(crate::request::Recording {
            trigger: Some(trigger.clone()),
            on: None,
            retain: args.retain,
            after: args.after,
            documents: args.documents,
        });
        session.check_recording()?;
    }
    Ok((session, defaulted))
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
        EmitKind::TopK => Emit::TopK {
            k: Emit::DEFAULT_TOPK,
            by: None,
        },
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
    fn a_window_on_the_command_line_is_the_window_a_session_file_carries() {
        for text in ["1s..3s", "1s..", "..2s"] {
            let args = parse(&[
                "omn",
                "trace",
                "mem.rss",
                "--pid",
                "1",
                "--mode",
                "poll@100ms",
                "--emit",
                "summary",
                "--duration",
                "5s",
                "--window",
                text,
            ]);
            let (session, _) = to_session(&args, &catalog()).unwrap();
            let from_file: crate::request::Window = text.parse().unwrap();
            assert_eq!(session.requests[0].window, Some(from_file));
            assert_eq!(session.requests[0].window.unwrap().to_string(), text);
        }
    }

    #[test]
    fn a_window_the_command_line_cannot_mean_is_refused_in_the_session_file_s_words() {
        for text in ["3s..1s", "..", "2s"] {
            let from_file = text.parse::<crate::request::Window>().unwrap_err();
            let error =
                Cli::try_parse_from(["omn", "trace", "mem.rss", "--pid", "1", "--window", text])
                    .unwrap_err()
                    .to_string();
            assert!(error.contains(from_file.message()), "{error}");
        }
    }

    #[test]
    fn a_request_held_for_an_escalation_is_refused_on_its_own() {
        let args = parse(&[
            "omn",
            "trace",
            "sched.futex",
            "--mode",
            "trace",
            "--emit",
            "summary",
            "--window",
            "on lock",
            "--",
            "./prog",
        ]);
        let error = to_session(&args, &catalog()).unwrap_err();
        assert!(
            error.message().contains("held for an escalation"),
            "{error}"
        );
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

    /// `--symbols` is a flag of both questions, read into their arguments,
    /// and refuses a source nobody serves, as `omn trace`'s does.
    #[test]
    fn both_questions_take_symbols() {
        use crate::stack::unwind::SymbolSource;
        let latency = Cli::try_parse_from([
            "omn",
            "latency",
            "--objective",
            "latency",
            "--symbols",
            "debuginfod",
            "--",
            "true",
        ])
        .expect("arguments should parse");
        match latency.command {
            Command::Latency(args) => assert_eq!(args.symbols, Some(SymbolSource::Debuginfod)),
            other => panic!("expected latency, got {other:?}"),
        }
        let saturate = Cli::try_parse_from([
            "omn",
            "saturate",
            "--objective",
            "throughput",
            "--symbols",
            "local",
            "--",
            "true",
        ])
        .expect("arguments should parse");
        match saturate.command {
            Command::Saturate(args) => assert_eq!(args.symbols, Some(SymbolSource::Local)),
            other => panic!("expected saturate, got {other:?}"),
        }
        for command in ["latency", "saturate"] {
            assert!(Cli::try_parse_from([
                "omn",
                command,
                "--objective",
                "latency",
                "--symbols",
                "everywhere",
                "--",
                "true",
            ])
            .is_err());
        }
    }

    #[test]
    fn a_source_and_a_file_cannot_both_describe_the_session() {
        let result = Cli::try_parse_from(["omn", "trace", "io.latency", "-f", "session.yml"]);
        assert!(result.is_err());
    }
}
