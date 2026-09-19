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

    measure(
        &catalog,
        &capabilities,
        &session,
        &report,
        &defaulted,
        &resolutions,
    )
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
            "[{source}] --budget {budget}: `{}` declares `{}` ({}), and nothing below it in the \
             chain declares less, so a run that exceeds the ceiling will stop rather than degrade",
            selected.id,
            selected.cost,
            band(selected.cost),
        ),
        candidates => format!(
            "[{source}] --budget {budget}: `{}` declares `{}` ({}), so a run that exceeds the \
             ceiling will degrade onto {} -- which was never probed, because a better backend \
             was selected first",
            selected.id,
            selected.cost,
            band(selected.cost),
            candidates.join(" or "),
        ),
    })
}

/// The backends a budget may fall back onto, resolved against this machine.
///
/// The runner knows it is over budget and nothing else. What is cheaper than
/// the backend in use is a question about the catalog, and whether this machine
/// can serve it is a question for the resolver; both live here, which is what
/// keeps `src/collect/` from reaching back into either.
///
/// The candidates are the backends *below* the selected one in the chain. The
/// ones above were rejected by this machine and a budget cannot make them
/// reachable; the ones below were never probed, because a better backend was
/// selected first -- so each is resolved for real before it is offered, and one
/// this build has no collector for is not offered at all.
struct Ladder<'a> {
    catalog: &'a Catalog,
    capabilities: &'a Capabilities,
    /// The resolved request each collection is being served by, replaced when a
    /// budget moves it. This, not the file written before the run, is what
    /// names the backend that produced the numbers.
    serving: Vec<ResolvedRequest>,
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
        }
    }

    /// The resolved request that was serving collection `index` when the run
    /// ended, which is the one to keep beside its numbers.
    ///
    /// A degradation that could not be started leaves the run on the backend it
    /// had, and `self.serving` has already moved on; the collection is the only
    /// thing that knows which backend actually produced the events, so it is
    /// the one asked.
    fn record(&self, index: usize, backend: &str, original: &ResolvedRequest) -> ResolvedRequest {
        if original.backend.id == backend {
            return original.clone();
        }
        if let Some(serving) = self.serving.get(index) {
            if serving.backend.id == backend {
                return serving.clone();
            }
        }
        let mut amended = original.clone();
        amended.backend.id = backend.to_owned();
        amended
    }

    /// Every backend below `from` in the chain whose declared band fits the
    /// ceiling, in chain order.
    fn candidates(&self, source: &omn::request::SourceId, from: &str, ceiling: f64) -> Vec<String> {
        self.catalog
            .get(source)
            .into_iter()
            .flat_map(|entry| entry.backends.iter())
            .skip_while(|backend| backend.id != from)
            .skip(1)
            .filter(|backend| worst_case_percent(backend.cost) <= ceiling)
            .map(|backend| backend.id.clone())
            .collect()
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
        for id in self.candidates(&source, &serving.backend.id, ceiling) {
            if !collect::implements(source.as_str(), &id) {
                continue;
            }
            let Ok(pinned) = format!("force:{id}").parse() else {
                continue;
            };
            let mut request = serving.request.clone();
            request.backend = Some(pinned);
            // Resolved for real: a backend further down the chain was never
            // probed, and offering one this machine cannot serve would turn a
            // budget into a failed run.
            let Ok(resolved) = resolve::resolve(self.catalog, self.capabilities, &request) else {
                continue;
            };
            self.serving[index] = resolved.clone();
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
    #[serde(skip_serializing_if = "Option::is_none")]
    measured_percent: Option<f64>,
    overhead: collect::Overhead,
    stopped_because: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
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
        format!(
            "{} ({}) via `{}` spent {} of cpu {against}, over {} of wall time; measuring that \
             cost {} across {} read(s)",
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
        Some(match (self.within_budget, self.measured_percent) {
            (Some(true), Some(percent)) => format!(
                "[{}] --budget {budget}% held: {percent:.2} % measured",
                self.source
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

/// The top of the overhead band a declared cost stands for, as `catalog::Cost`
/// documents it. `extreme` is "over 10x" and no percentage bounds it.
///
/// `free` is documented as "a counter read, about zero", which as a number has
/// to be *something*: at exactly zero the claim can never be met by any real
/// measurement, so every `free` backend would read as having broken its own
/// promise the moment the promise became checkable. A tenth of a percent is the
/// reading that makes "about zero" an assertion rather than a figure of speech.
fn worst_case_percent(cost: omn::catalog::Cost) -> f64 {
    use omn::catalog::Cost;
    match cost {
        Cost::Free => 0.1,
        Cost::Low => 1.0,
        Cost::Med => 5.0,
        Cost::High => 20.0,
        Cost::Extreme => f64::INFINITY,
    }
}

fn band(cost: omn::catalog::Cost) -> &'static str {
    use omn::catalog::Cost;
    match cost {
        Cost::Free => "about zero, under 0.1 %",
        Cost::Low => "under 1 %",
        Cost::Med => "1 to 5 %",
        Cost::High => "5 to 20 %",
        Cost::Extreme => "over 10x",
    }
}

/// Runs the session's requests over one window and prints what they produced.
fn measure(
    catalog: &Catalog,
    capabilities: &Capabilities,
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

    let mut ladder = Ladder::new(catalog, capabilities, resolved);
    let collections = match collect::run_all_within(resolved, &mut ladder) {
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
    let mut costs: Vec<MeasuredCost> = Vec::new();
    let mut served_by: Vec<ResolvedRequest> = Vec::new();

    for (index, collection) in collections.into_iter().enumerate() {
        // The record of how these events were obtained is the request that was
        // serving when they arrived, which a budget may have moved. Handing
        // `aggregate` the file written before the run would name a backend that
        // stopped producing half way through it.
        let serving = ladder.record(index, served_last(&collection), &resolved[index]);
        merged.extend(collection.events.iter().cloned());
        let unit = catalog
            .get(&serving.request.source)
            .map(|entry| entry.unit)
            .unwrap_or(omn::catalog::Unit::Count);
        let value_attr = collection.value_attr.clone();
        costs.push(measured_cost(catalog, &serving, &collection));
        run.results
            .push(emit::aggregate(&serving, collection, unit, &value_attr)?);
        served_by.push(serving);
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

    // What the measurement cost, on every run rather than only on the ones that
    // asked. A tool that reports what a program spends and never its own price
    // is asking to be trusted about the one number it never shows.
    for cost in &costs {
        eprintln!("cost: {}", cost.line());
        if let Some(verdict) = cost.verdict() {
            eprintln!("budget: {verdict}");
        }
    }
    if let Some(comparison) = compare_backends(&costs, &run.results) {
        eprint!("{comparison}");
    }

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
        within_budget: budget_percent
            .and_then(|ceiling| measured_percent.map(|percent| percent <= ceiling)),
        measured_percent,
        overhead: collection.overhead.clone(),
        stopped_because: collection.stopped_because.clone(),
        notes: collection.notes.clone(),
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
