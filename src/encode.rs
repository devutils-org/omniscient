//! Encoding results.
//!
//! Text is for reading, the structured formats are for tooling, and `ndjson` is
//! for a stream that does not fit in memory. Each format declares which emits it
//! can carry (see `format`), so this module never has to approximate: it either
//! knows how to write what it was handed, or it says which format would.

use std::fmt::Write as _;

use crate::catalog::{MatchKind, Unit};
use crate::emit::{Bucket, Output, Results, SeriesHist, StackCount};
use crate::error::ParseError;
use crate::event::KeyValue;
use crate::format::OutputFormat;
use crate::join::{Correlation, Outcome};
use crate::scalar::Duration;

/// Everything one run produced: what each source measured, and what the joins
/// made of it together.
///
/// It is one document rather than several because several would not be one:
/// concatenating two JSON documents produces a file no parser accepts, and a
/// consumer of the results needs the joins beside them anyway.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RunOutput {
    pub results: Vec<Results>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub joins: Vec<Correlation>,
}

/// Encodes a whole run.
pub fn run_output(format: OutputFormat, output: &RunOutput) -> Result<String, ParseError> {
    match format {
        OutputFormat::Text => {
            let mut text = String::new();
            for (index, results) in output.results.iter().enumerate() {
                if index > 0 {
                    text.push('\n');
                }
                text.push_str(&self::results(format, results)?);
            }
            for correlation in &output.joins {
                text.push('\n');
                text.push_str(&self::correlation(format, correlation)?);
            }
            Ok(text)
        }
        OutputFormat::Ndjson => {
            let mut text = String::new();
            for results in &output.results {
                text.push_str(&self::results(format, results)?);
            }
            for correlation in &output.joins {
                text.push_str(&self::correlation(format, correlation)?);
            }
            Ok(text)
        }
        OutputFormat::Folded => folded(output),
        OutputFormat::Speedscope => speedscope(output),
        format if format.renders_documents() => format.serialize_document(output),
        format => Err(ParseError::new(format!(
            "`{format}` has no encoder yet: {}",
            format.describe()
        ))),
    }
}

/// Encodes a join's result in the session's output format.
pub fn correlation(format: OutputFormat, correlation: &Correlation) -> Result<String, ParseError> {
    match format {
        OutputFormat::Text => Ok(correlation_text(correlation)),
        OutputFormat::Ndjson => {
            let line = serde_json::to_string(correlation)
                .map_err(|error| ParseError::new(format!("cannot encode the join: {error}")))?;
            Ok(format!("{line}\n"))
        }
        format if format.renders_documents() => format.serialize_document(correlation),
        format => Err(ParseError::new(format!(
            "`{format}` has no encoder for a join yet: {}",
            format.describe()
        ))),
    }
}

fn correlation_text(correlation: &Correlation) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "join {}  {} -> {}  evidence {}",
        correlation.rule, correlation.from, correlation.to, correlation.evidence
    );

    let total = correlation.matched + correlation.unmatched;
    let share = if total == 0 {
        0.0
    } else {
        100.0 * correlation.matched as f64 / total as f64
    };
    let _ = writeln!(
        out,
        "  {} of {} `{}` events attributed ({share:.1} %); {} {}",
        correlation.matched,
        total,
        correlation.to,
        correlation.unmatched,
        unmatched_reason(correlation.match_kind)
    );

    let states = &correlation.states;
    let _ = writeln!(
        out,
        "  states: {} opened, {} closed, {} still open at the end, peak {} held at once",
        states.opened, states.closed, states.still_open, states.peak_open
    );
    if states.is_clean() {
        let _ = writeln!(
            out,
            "  no eviction: nothing was dropped for age or for space"
        );
    } else {
        let _ = writeln!(
            out,
            "  EVICTED: {} expired past the window, {} dropped because the map was full. \
             Attributions are missing, not absent.",
            states.expired, states.overflowed
        );
    }

    if correlation.top.is_empty() {
        let _ = writeln!(out, "\n  nothing was attributed");
        return out;
    }

    // An `interval` state has an extent and the others do not, so the size
    // column exists for one match kind and would be a column of zeroes for the
    // other three -- which reads as "these allocations were empty" rather than
    // as "this join is not about ranges".
    let ranged = correlation.match_kind == MatchKind::Interval;
    let (heading, column) = table_words(correlation.match_kind);
    let _ = writeln!(out, "\n  {heading}");
    if ranged {
        let _ = writeln!(
            out,
            "    {column:<34} {:>10}  {:>8}  OUTCOME",
            "SIZE", "EVENTS"
        );
    } else {
        let _ = writeln!(out, "    {column:<44} {:>8}  OUTCOME", "EVENTS");
    }
    for attribution in &correlation.top {
        if ranged {
            let _ = writeln!(
                out,
                "    {:<34} {:>10}  {:>8}  {}",
                attribution.key,
                bytes(attribution.len as f64),
                attribution.matched,
                outcome(attribution.outcome, correlation.match_kind)
            );
        } else {
            let _ = writeln!(
                out,
                "    {:<44} {:>8}  {}",
                attribution.key,
                attribution.matched,
                outcome(attribution.outcome, correlation.match_kind)
            );
        }
    }
    if let Some(note) = counting_note(correlation.match_kind) {
        let _ = writeln!(out, "\n  {note}");
    }
    out
}

/// What an unattributed downstream event means, which is a different sentence
/// for each match kind.
fn unmatched_reason(kind: MatchKind) -> &'static str {
    match kind {
        MatchKind::Interval => "fell outside every open range",
        MatchKind::Eq => "carried a key no open state had",
        MatchKind::ScopedEq => "carried a key no open state had in that scope",
        MatchKind::Wakeup => "followed no wakeup this run saw",
    }
}

/// The heading over the attributions, and what the first column holds.
fn table_words(kind: MatchKind) -> (&'static str, &'static str) {
    match kind {
        MatchKind::Interval => ("heaviest ranges", "RANGE"),
        MatchKind::Eq | MatchKind::ScopedEq => ("heaviest keys", "KEY"),
        MatchKind::Wakeup => ("heaviest wakeup chains", "CHAIN"),
    }
}

/// The caveat under the table, where the counting has one.
///
/// Only the interval match does. Its `EVENTS` column counts page faults, and a
/// fault is not a page; the other three count events whose meaning the reader
/// already has from the source.
fn counting_note(kind: MatchKind) -> Option<&'static str> {
    match kind {
        MatchKind::Interval => Some(
            "EVENTS counts faults, not bytes, and on a kernel with multi-size transparent huge \
             pages it does not even rank by bytes: the kernel picks larger folios for larger \
             regions, so a 1 MiB and a 32 MiB allocation both fault a few hundred times. Read it \
             as where the faults happened, not as how much memory they cost; the THP mode is in \
             the environment.",
        ),
        _ => None,
    }
}

/// How a state left the engine, in the words of the thing it was.
///
/// `Closed` is the one that differs: a range closes because the program freed
/// it, a wakeup closes because the task it released actually ran. Calling the
/// second one "freed" is not a wording problem, it is a claim about memory in a
/// join that never looked at any.
fn outcome(outcome: Outcome, kind: MatchKind) -> &'static str {
    match (outcome, kind) {
        (Outcome::Closed, MatchKind::Interval) => "freed",
        (Outcome::Closed, MatchKind::Wakeup) => "ran",
        (Outcome::Closed, _) => "closed",
        (Outcome::Expired, _) => "expired",
        (Outcome::Overflowed, _) => "evicted",
        (Outcome::StillOpen, MatchKind::Wakeup) => "never ran",
        (Outcome::StillOpen, _) => "live",
    }
}

/// Encodes results in the session's output format.
pub fn results(format: OutputFormat, results: &Results) -> Result<String, ParseError> {
    match format {
        OutputFormat::Text => Ok(text(results)),
        OutputFormat::Ndjson => ndjson(results),
        // A profile format is a file, not a fragment: it has a header, a shared
        // frame table and a list of profiles. One result is a run of one.
        OutputFormat::Folded | OutputFormat::Speedscope => run_output(
            format,
            &RunOutput {
                results: vec![results.clone()],
                joins: Vec::new(),
            },
        ),
        format if format.renders_documents() => format.serialize_document(results),
        format => Err(ParseError::new(format!(
            "`{format}` has no encoder yet: {}",
            format.describe()
        ))),
    }
}

/// One JSON document per line, so a consumer can read a result as it arrives
/// rather than after it is complete.
fn ndjson(results: &Results) -> Result<String, ParseError> {
    let mut out = String::new();
    let header = serde_json::json!({
        "record": "run",
        "label": results.label,
        "source": results.source,
        "backend": results.backend,
        "unit": results.unit.as_str(),
        "events_seen": results.events_seen,
        "observed_ns": results.observed_ns,
        "stopped_because": results.stopped_because,
        "losses": results.losses,
    });
    let _ = writeln!(out, "{header}");

    for output in &results.outputs {
        match output {
            Output::Raw { events } => {
                for event in events {
                    let line = serde_json::to_string(event).map_err(|error| {
                        ParseError::new(format!("cannot encode event: {error}"))
                    })?;
                    let _ = writeln!(out, "{line}");
                }
            }
            Output::Timeline { series } => {
                for series in series {
                    for point in &series.points {
                        let line = serde_json::json!({
                            "record": "point",
                            "key": series.key,
                            "offset_ns": point.offset_ns,
                            "value": point.value,
                        });
                        let _ = writeln!(out, "{line}");
                    }
                }
            }
            Output::Rate { series } => {
                for rate in series {
                    let mut line = serde_json::to_value(rate)
                        .map_err(|error| ParseError::new(format!("cannot encode rate: {error}")))?;
                    line["record"] = serde_json::Value::from("rate");
                    let _ = writeln!(out, "{line}");
                }
            }
            Output::Summary { series } => {
                for summary in series {
                    let mut line = serde_json::to_value(summary).map_err(|error| {
                        ParseError::new(format!("cannot encode summary: {error}"))
                    })?;
                    line["record"] = serde_json::Value::from("summary");
                    let _ = writeln!(out, "{line}");
                }
            }
            Output::Hist { layout, series } => {
                for histogram in series {
                    let mut line = serde_json::to_value(histogram).map_err(|error| {
                        ParseError::new(format!("cannot encode histogram: {error}"))
                    })?;
                    line["record"] = serde_json::Value::from("hist");
                    line["layout"] = serde_json::Value::from(layout.to_string());
                    let _ = writeln!(out, "{line}");
                }
            }
            Output::TopK { k, series, rest } => {
                for (rank, total) in series.iter().enumerate() {
                    let mut line = serde_json::to_value(total).map_err(|error| {
                        ParseError::new(format!("cannot encode a ranking: {error}"))
                    })?;
                    line["record"] = serde_json::Value::from("topk");
                    line["rank"] = serde_json::Value::from(rank + 1);
                    line["k"] = serde_json::Value::from(*k);
                    let _ = writeln!(out, "{line}");
                }
                // Always written, even when nothing was left out: a consumer
                // reading the stream cannot tell a complete ranking from a
                // truncated one unless the answer is in it.
                let mut line = serde_json::to_value(rest)
                    .map_err(|error| ParseError::new(format!("cannot encode the rest: {error}")))?;
                line["record"] = serde_json::Value::from("topk_rest");
                let _ = writeln!(out, "{line}");
            }
            Output::Stack {
                stacks,
                unattributed,
            } => {
                for stack in stacks {
                    let line = serde_json::json!({
                        "record": "stack",
                        "stack": stack.stack,
                        "events": stack.events,
                        "total": stack.total,
                        "frames": results.stacks.get(&stack.stack),
                    });
                    let _ = writeln!(out, "{line}");
                }
                // Always written: a profile missing nine tenths of its samples
                // and a complete one are the same stream of records otherwise.
                let line = serde_json::json!({
                    "record": "stack_coverage",
                    "stacks": stacks.len(),
                    "unattributed": unattributed,
                });
                let _ = writeln!(out, "{line}");
            }
        }
    }
    Ok(out)
}

fn text(results: &Results) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}  via {}  ({})",
        results.label, results.backend, results.source
    );
    let _ = writeln!(
        out,
        "  {} events over {}, stopped because {}",
        results.events_seen,
        approximate(results.observed_ns),
        results.stopped_because
    );

    if results.losses.is_clean() {
        let _ = writeln!(out, "  no loss: every read succeeded and no tick ran late");
    } else {
        let _ = writeln!(
            out,
            "  LOSS: {} dropped, {} failed reads, {} late ticks",
            results.losses.dropped, results.losses.failed_reads, results.losses.late_ticks
        );
        for (reason, count) in &results.losses.reasons {
            let _ = writeln!(out, "    {count}x {reason}");
        }
    }

    // A filter that ran here is part of how the numbers came to be: the events
    // it removed were collected and paid for, and every series below is what
    // was left.
    if let Some(filtered) = &results.filtered {
        let _ = writeln!(
            out,
            "  filter `{}` kept {} of {} events, evaluated after reading",
            filtered.predicate,
            filtered.kept,
            filtered.kept + filtered.dropped
        );
        if !filtered.unseen_fields.is_empty() {
            let _ = writeln!(
                out,
                "    no event carried {}, so that part of the predicate was never true",
                filtered
                    .unseen_fields
                    .iter()
                    .map(|field| format!("`{field}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    for degradation in &results.degradations {
        let _ = writeln!(out, "  degraded: {degradation}");
    }
    for unverified in &results.unverified {
        let _ = writeln!(out, "  unverified requirement: {unverified}");
    }

    for output in &results.outputs {
        out.push('\n');
        match output {
            Output::Rate { series } => {
                let _ = writeln!(out, "  rate");
                for rate in series {
                    match rate.per_second {
                        Some(per_second) => {
                            let _ = writeln!(
                                out,
                                "    {:<14} {} over {}  =  {}/s",
                                rate.key,
                                quantity(rate.total, results.unit),
                                approximate(rate.window_ns),
                                quantity(per_second, results.unit),
                            );
                        }
                        None => {
                            let _ = writeln!(
                                out,
                                "    {:<14} {} over no measurable window, so there is no rate",
                                rate.key,
                                quantity(rate.total, results.unit),
                            );
                        }
                    }
                }
            }
            Output::Summary { series } => {
                let _ = writeln!(out, "  summary");
                for summary in series {
                    let _ = writeln!(
                        out,
                        "    {:<14} n={:<5} min {}  mean {}  max {}  last {}",
                        summary.key,
                        summary.count,
                        quantity(summary.min, results.unit),
                        quantity(summary.mean, results.unit),
                        quantity(summary.max, results.unit),
                        quantity(summary.last, results.unit),
                    );
                }
            }
            Output::Timeline { series } => {
                let _ = writeln!(out, "  timeline");
                for series in series {
                    let _ = writeln!(out, "    {}", series.key);
                    for point in &series.points {
                        let _ = writeln!(
                            out,
                            "      +{:<10} {}",
                            Duration::from_nanos(round_to_ms(point.offset_ns)).to_string(),
                            quantity(point.value, results.unit)
                        );
                    }
                }
            }
            Output::Hist { layout, series } => {
                let _ = writeln!(out, "  hist ({layout})");
                for histogram in series {
                    hist_text(&mut out, histogram, results.unit);
                }
            }
            Output::TopK { k, series, rest } => {
                let _ = writeln!(
                    out,
                    "  topk: the {} heaviest of {} series",
                    series.len(),
                    series.len() + rest.series
                );
                if series.len() < *k as usize && rest.series == 0 {
                    let _ = writeln!(
                        out,
                        "    ({k} were asked for and the run produced {})",
                        series.len()
                    );
                }
                for (rank, total) in series.iter().enumerate() {
                    let _ = writeln!(
                        out,
                        "    {:>2}. {:<20} {:>12}  over {} events",
                        rank + 1,
                        total.key,
                        quantity(total.total, results.unit),
                        total.events
                    );
                }
                // A ranking that does not say what it left out reads as a
                // statement about everything.
                if rest.is_empty() {
                    let _ = writeln!(out, "    nothing was left out: that is every series");
                } else {
                    let _ = writeln!(
                        out,
                        "    the other {} series: {} over {} events",
                        rest.series,
                        quantity(rest.total, results.unit),
                        rest.events
                    );
                }
            }
            Output::Stack {
                stacks,
                unattributed,
            } => {
                let _ = writeln!(out, "  stack");
                for stack in stacks {
                    let _ = writeln!(
                        out,
                        "    {:>12}  {:>7} events  {}",
                        quantity(stack.total, results.unit),
                        stack.events,
                        frames(results, stack).join(";")
                    );
                }
                if *unattributed > 0 {
                    let _ = writeln!(
                        out,
                        "    {unattributed} events carried no stack, so they are in no line above"
                    );
                }
                if stacks.is_empty() && *unattributed == 0 {
                    let _ = writeln!(out, "    no events");
                }
            }
            Output::Raw { events } => {
                let _ = writeln!(out, "  raw");
                for event in events {
                    let keys: Vec<String> = event.keys.iter().map(KeyValue::to_string).collect();
                    let attrs: Vec<String> = event
                        .attrs
                        .iter()
                        .map(|attr| format!("{}={}", attr.name, attr.value))
                        .collect();
                    let _ = writeln!(
                        out,
                        "    {:>14}  {}  {}  {}",
                        event.ts_ns,
                        event.kind,
                        keys.join(" "),
                        attrs.join(" ")
                    );
                }
            }
        }
    }
    out
}

/// Collapsed stacks: one line of `root;...;leaf weight` per distinct stack.
///
/// The weight is the summed value attribute, not the number of events, because
/// that is what the emit measured: nanoseconds blocked for `sched.offcpu`,
/// bytes for `mem.heap.alloc`, the sampling period for a profile. Counting
/// lines instead would make every sample worth the same, which is the one thing
/// a weighted profile exists not to do.
fn folded(output: &RunOutput) -> Result<String, ParseError> {
    let profiles: Vec<&Results> = output
        .results
        .iter()
        .filter(|results| {
            results
                .outputs
                .iter()
                .any(|output| matches!(output, Output::Stack { .. }))
        })
        .collect();

    // With one profile the file is the profile. With several, the label becomes
    // a root frame: merging two sources into one flame graph without saying
    // which frames came from where would produce a picture of neither.
    let label_as_root = profiles.len() > 1;
    let mut text = String::new();
    for results in &profiles {
        for output in &results.outputs {
            let Output::Stack { stacks, .. } = output else {
                continue;
            };
            for stack in stacks {
                let mut chain = frames(results, stack);
                if label_as_root {
                    chain.insert(0, results.label.clone());
                }
                let chain: Vec<String> = chain
                    .iter()
                    .map(|frame| frame.replace(['\n', '\r', ';'], " "))
                    .collect();
                let _ = writeln!(text, "{} {}", chain.join(";"), stack.total.round() as i64);
            }
        }
    }

    // An empty collapsed-stack file is a valid file: it opens as an empty flame
    // graph, which reads as a program that spent no time anywhere. Whatever the
    // reason there is nothing to write, saying it beats writing nothing.
    if text.is_empty() {
        return Err(ParseError::new(format!(
            "`folded` describes a profile and this run has none to write: {}",
            why_no_profile(output)
        )));
    }
    Ok(text)
}

/// Why a run carries no stacks, for a profile format that cannot write one.
///
/// The two reasons are different answers and only one of them is about the
/// program: a request that never asked for stacks, and a request that asked and
/// whose collector captured none.
fn why_no_profile(output: &RunOutput) -> String {
    let mut unattributed = 0;
    let mut asked = false;
    for results in &output.results {
        for out in &results.outputs {
            if let Output::Stack {
                stacks,
                unattributed: missing,
            } = out
            {
                asked = true;
                unattributed += missing;
                if !stacks.is_empty() {
                    return String::new();
                }
            }
        }
    }
    if !asked {
        return "it emitted no stacks; use --emit stack, or json for what was measured".to_owned();
    }
    format!(
        "`stack` was asked for and no event carried one, {unattributed} of them;          the backend that served this request captures no stacks"
    )
}

/// Speedscope's own JSON.
///
/// Two shapes, because speedscope has two and this tool produces both: a
/// *sampled* profile for aggregated stacks, whose axis is the weight, and an
/// *evented* profile per timeline series, whose axis is time. The second is
/// what makes speedscope worth offering for something that is not a profile --
/// a timeline opened in it is a strip of what the value was and when, which no
/// other format here can show.
fn speedscope(output: &RunOutput) -> Result<String, ParseError> {
    let mut frames_seen: Vec<String> = Vec::new();
    let mut index_of: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut intern = |name: String| -> usize {
        *index_of.entry(name.clone()).or_insert_with(|| {
            frames_seen.push(name);
            frames_seen.len() - 1
        })
    };

    let mut profiles = Vec::new();
    for results in &output.results {
        for out in &results.outputs {
            match out {
                Output::Stack { stacks, .. } => {
                    let mut samples = Vec::new();
                    let mut weights = Vec::new();
                    for stack in stacks {
                        samples.push(
                            frames(results, stack)
                                .into_iter()
                                .map(&mut intern)
                                .collect::<Vec<_>>(),
                        );
                        weights.push(stack.total);
                    }
                    if samples.is_empty() {
                        continue;
                    }
                    let end: f64 = weights.iter().sum();
                    profiles.push(serde_json::json!({
                        "type": "sampled",
                        "name": results.label,
                        "unit": speedscope_unit(results.unit),
                        "startValue": 0.0,
                        "endValue": end,
                        "samples": samples,
                        "weights": weights,
                    }));
                }
                Output::Timeline { series } => {
                    for series in series {
                        let Some(first) = series.points.first() else {
                            continue;
                        };
                        let last = series.points.last().expect("a first implies a last");
                        let mut events = Vec::new();
                        for (index, point) in series.points.iter().enumerate() {
                            // The value is in the frame's name and the time is
                            // the axis: a point holds until the next one, and
                            // the last holds for nothing, because nothing was
                            // observed after it.
                            let frame = intern(format!(
                                "{} = {}",
                                series.key,
                                quantity(point.value, results.unit)
                            ));
                            let until = series
                                .points
                                .get(index + 1)
                                .map(|next| next.offset_ns)
                                .unwrap_or(point.offset_ns);
                            events.push(serde_json::json!({"type": "O", "frame": frame, "at": point.offset_ns}));
                            events.push(
                                serde_json::json!({"type": "C", "frame": frame, "at": until}),
                            );
                        }
                        profiles.push(serde_json::json!({
                            "type": "evented",
                            "name": format!("{} {}", results.label, series.key),
                            "unit": "nanoseconds",
                            "startValue": first.offset_ns,
                            "endValue": last.offset_ns,
                            "events": events,
                        }));
                    }
                }
                _ => {}
            }
        }
    }

    if profiles.is_empty() {
        return Err(ParseError::new(format!(
            "`speedscope` carries stacks and timelines and this run has neither to write: {}",
            why_no_profile(output)
        )));
    }

    let document = serde_json::json!({
        "$schema": "https://www.speedscope.app/file-format-schema.json",
        "exporter": concat!("omn ", env!("CARGO_PKG_VERSION")),
        "name": output
            .results
            .iter()
            .map(|results| results.label.clone())
            .collect::<Vec<_>>()
            .join(", "),
        "activeProfileIndex": 0,
        "shared": {
            "frames": frames_seen
                .iter()
                .map(|name| serde_json::json!({"name": name}))
                .collect::<Vec<_>>(),
        },
        "profiles": profiles,
    });
    serde_json::to_string_pretty(&document)
        .map_err(|error| ParseError::new(format!("cannot encode speedscope JSON: {error}")))
}

/// Speedscope names the units it knows how to label an axis with; anything else
/// is `none`, which renders the number bare rather than as a wrong unit.
fn speedscope_unit(unit: Unit) -> &'static str {
    match unit {
        Unit::Ns => "nanoseconds",
        Unit::Bytes => "bytes",
        _ => "none",
    }
}

/// One histogram, with a bar per bucket.
///
/// The bar is the whole point of rendering a distribution in text: a column of
/// counts is a table, and the shape -- one mode, two modes, a long tail -- is
/// what a reader came for and what the mean could not tell them.
fn hist_text(out: &mut String, histogram: &SeriesHist, unit: Unit) {
    let mean = if histogram.count == 0 {
        0.0
    } else {
        histogram.sum / histogram.count as f64
    };
    let _ = writeln!(
        out,
        "    {}  n={}  min {}  mean {}  max {}",
        histogram.key,
        histogram.count,
        quantity(histogram.min, unit),
        quantity(mean, unit),
        quantity(histogram.max, unit),
    );

    const BAR: usize = 32;
    let peak = histogram
        .buckets
        .iter()
        .map(|bucket| bucket.count)
        .max()
        .unwrap_or(0);
    for bucket in &histogram.buckets {
        let filled = (bucket.count * BAR).checked_div(peak).unwrap_or(0);
        let _ = writeln!(
            out,
            "      {:<30} {:>8}  |{:<BAR$}|",
            bucket_range(bucket, unit),
            bucket.count,
            "#".repeat(filled),
        );
    }
    if histogram.unbucketable > 0 {
        let _ = writeln!(
            out,
            "      {} values are in no bucket: a histogram has no range below zero, and \
             putting them in the lowest one would report them as values they are not",
            histogram.unbucketable
        );
    }
}

/// `[low, high)` in the source's own unit, which is what makes a latency
/// histogram readable as milliseconds rather than as powers of two.
fn bucket_range(bucket: &Bucket, unit: Unit) -> String {
    match bucket.high {
        Some(high) => format!("[{}, {})", edge(bucket.low, unit), edge(high, unit)),
        None => format!("[{}, ...)", edge(bucket.low, unit)),
    }
}

/// A bucket boundary. Exact, unlike `quantity`: a boundary is a definition of
/// which values fall inside, and rounding one turns two adjacent buckets into
/// the same printed range.
fn edge(value: u64, unit: Unit) -> String {
    match unit {
        Unit::Bytes if value >= 1024 => exact_binary(value),
        Unit::Ns => Duration::from_nanos(value).to_string(),
        _ => value.to_string(),
    }
}

/// A power of two as `1 MiB` rather than `1048576`, and anything else as it is.
fn exact_binary(value: u64) -> String {
    const UNITS: [(u64, &str); 3] = [
        (1024 * 1024 * 1024, "GiB"),
        (1024 * 1024, "MiB"),
        (1024, "KiB"),
    ];
    for (factor, suffix) in UNITS {
        if value >= factor && value % factor == 0 {
            return format!("{} {suffix}", value / factor);
        }
    }
    value.to_string()
}

/// The frames of a stack, or a placeholder naming the identity that has none.
///
/// An unresolved stack is not an empty stack: a profile that silently dropped
/// the samples it could not symbolize would understate exactly the part of the
/// program the symbols were missing for.
fn frames(results: &Results, stack: &StackCount) -> Vec<String> {
    results
        .stacks
        .get(&stack.stack)
        .cloned()
        .unwrap_or_else(|| vec![format!("[unresolved stack {:#x}]", stack.stack)])
}

/// Renders a wall-clock span for a human: three significant digits, not the
/// exact nanosecond count, which nobody reads and which suggests a precision
/// the poll never had.
fn approximate(nanos: u64) -> String {
    const UNITS: [(f64, &str); 4] = [
        (3_600_000_000_000.0, "h"),
        (60_000_000_000.0, "m"),
        (1_000_000_000.0, "s"),
        (1_000_000.0, "ms"),
    ];
    let nanos = nanos as f64;
    for (factor, suffix) in UNITS {
        if nanos >= factor {
            return format!("{:.2}{suffix}", nanos / factor);
        }
    }
    Duration::from_nanos(nanos as u64).to_string()
}

/// Rounds an offset to the millisecond, so a timeline column stays readable
/// without pretending the poll was more precise than it was.
fn round_to_ms(offset_ns: u64) -> u64 {
    (offset_ns + 500_000) / 1_000_000 * 1_000_000
}

/// Renders a value in its unit. Bytes get binary prefixes because that is what
/// the kernel counts in; everything else stays a number with its unit.
fn quantity(value: f64, unit: Unit) -> String {
    match unit {
        Unit::Bytes => bytes(value),
        // Not the exact-unit form `Duration` renders for round trips: that
        // picks the largest unit a value divides evenly into, so 6654000 ns
        // reads as `6654us` while 7021500 ns reads as `7021500ns` in the very
        // same row. A reader comparing two numbers should not have to compare
        // their units first.
        Unit::Ns => duration(value),
        Unit::Ratio => format!("{value:.3}"),
        // A frequency reads as `3962428538 hz` and a tick's energy as
        // `0 joules` without this: the same problem the durations had, in the
        // units the `power` and `cpu` families report in.
        Unit::Hz => scaled(value, &[(1e9, "GHz"), (1e6, "MHz"), (1e3, "kHz")], "Hz"),
        Unit::Joules => scaled(value, &[(1e3, "kJ"), (1.0, "J"), (1e-3, "mJ")], "uJ"),
        other => format!("{value:.0} {other}"),
    }
}

/// Renders a value in the largest of `steps` it reaches, falling back to
/// `smallest` with three decimals for anything below them.
///
/// Without this a number is printed in its base unit whatever its size, and a
/// column of them cannot be compared at a glance -- which is the whole job of
/// the text output.
fn scaled(value: f64, steps: &[(f64, &str)], smallest: &str) -> String {
    let magnitude = value.abs();
    for (factor, suffix) in steps {
        if magnitude >= *factor {
            return format!("{:.2} {suffix}", value / factor);
        }
    }
    let base = steps.last().map(|(factor, _)| *factor).unwrap_or(1.0);
    format!("{:.3} {smallest}", value / (base / 1000.0))
}

/// Renders a duration by magnitude, so every value in a column reads in the
/// same unit as its neighbours.
fn duration(nanos: f64) -> String {
    const UNITS: [(f64, &str); 5] = [
        (60_000_000_000.0, "min"),
        (1_000_000_000.0, "s"),
        (1_000_000.0, "ms"),
        (1_000.0, "us"),
        (1.0, "ns"),
    ];
    let nanos = nanos.max(0.0);
    for (factor, suffix) in UNITS {
        if nanos >= factor {
            return format!("{:.2}{suffix}", nanos / factor);
        }
    }
    "0ns".to_owned()
}

fn bytes(value: f64) -> String {
    const UNITS: [(f64, &str); 4] = [
        (1024.0 * 1024.0 * 1024.0, "GiB"),
        (1024.0 * 1024.0, "MiB"),
        (1024.0, "KiB"),
        (1.0, "B"),
    ];
    for (factor, suffix) in UNITS {
        if value.abs() >= factor {
            return format!("{:.1} {suffix}", value / factor);
        }
    }
    format!("{value:.0} B")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::join::{Attribution, StateStats};

    use crate::collect::Losses;
    use crate::emit::{
        Bucket, Point, Rest, Series, SeriesHist, SeriesSummary, SeriesTotal, StackCount,
    };

    fn sample() -> Results {
        Results {
            label: "rss".into(),
            source: "mem.rss".into(),
            unit: Unit::Bytes,
            backend: "fs:proc:statm".into(),
            outputs: vec![
                Output::Summary {
                    series: vec![SeriesSummary {
                        key: "task:7".into(),
                        count: 2,
                        min: 1_048_576.0,
                        max: 2_097_152.0,
                        mean: 1_572_864.0,
                        last: 2_097_152.0,
                    }],
                },
                Output::Timeline {
                    series: vec![Series {
                        key: "task:7".into(),
                        points: vec![
                            Point {
                                offset_ns: 0,
                                value: 1_048_576.0,
                            },
                            Point {
                                offset_ns: 1_000_000_000,
                                value: 2_097_152.0,
                            },
                        ],
                    }],
                },
            ],
            events_seen: 2,
            observed_ns: 1_000_000_000,
            stopped_because: "the requested duration elapsed".into(),
            losses: Losses::default(),
            filtered: None,
            unverified: Vec::new(),
            degradations: vec!["fs:proc:statm: resident pages only".into()],
            stacks: BTreeMap::new(),
        }
    }

    #[test]
    fn frequencies_and_energies_are_scaled_like_every_other_quantity() {
        assert_eq!(quantity(3_962_428_538.0, Unit::Hz), "3.96 GHz");
        assert_eq!(quantity(2_400_000.0, Unit::Hz), "2.40 MHz");
        assert_eq!(quantity(145.5, Unit::Joules), "145.50 J");
        assert_eq!(quantity(1_450.0, Unit::Joules), "1.45 kJ");
        // The case that read as `0 joules`: a tick's worth of energy.
        assert_eq!(quantity(0.004, Unit::Joules), "4.00 mJ");
        assert_eq!(quantity(0.000_004, Unit::Joules), "4.000 uJ");
    }

    #[test]
    fn a_column_of_durations_reads_in_one_unit() {
        // The three values of one summary row, which must not arrive in three
        // different units because of how evenly they divide.
        assert_eq!(duration(6_654_000.0), "6.65ms");
        assert_eq!(duration(7_021_500.0), "7.02ms");
        assert_eq!(duration(7_341_000.0), "7.34ms");
        assert_eq!(duration(850.0), "850.00ns");
    }

    #[test]
    fn a_wall_clock_span_reads_as_a_human_would_write_it() {
        assert_eq!(approximate(1_100_744_889), "1.10s");
        assert_eq!(approximate(250_000_000), "250.00ms");
        assert_eq!(approximate(90_000_000_000), "1.50m");
    }

    #[test]
    fn text_output_states_the_backend_the_losses_and_the_degradation() {
        let text = results(OutputFormat::Text, &sample()).unwrap();
        assert!(text.contains("via fs:proc:statm"), "{text}");
        assert!(text.contains("no loss"), "{text}");
        assert!(text.contains("degraded:"), "{text}");
        assert!(text.contains("1.0 MiB"), "{text}");
    }

    #[test]
    fn ndjson_writes_one_document_per_line_and_every_line_parses() {
        let text = results(OutputFormat::Ndjson, &sample()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            1 + 1 + 2,
            "a run header, a summary and two points"
        );
        for line in lines {
            serde_json::from_str::<serde_json::Value>(line).expect("every line is a JSON document");
        }
    }

    #[test]
    fn structured_formats_round_trip_the_results() {
        let encoded = results(OutputFormat::Json, &sample()).unwrap();
        let decoded: Results = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, sample());
    }

    /// What a filter removed is part of how the numbers came to be, so the
    /// text output carries it beside the losses rather than leaving a reader to
    /// wonder why a poll that ran for a second produced three events.
    #[test]
    fn the_text_output_says_what_the_filter_removed() {
        let mut sample = sample();
        sample.filtered = Some(crate::collect::Filtered {
            predicate: "bytes >= 8388608".into(),
            kept: 5,
            dropped: 307,
            unseen_fields: Vec::new(),
        });
        let text = results(OutputFormat::Text, &sample).unwrap();
        assert!(text.contains("kept 5 of 312 events"), "{text}");
        assert!(text.contains("bytes >= 8388608"), "{text}");
    }

    /// The case that empties a series: a predicate over a field the collector
    /// never produces. The output names the field, because an empty series and
    /// a workload that did nothing look identical otherwise.
    #[test]
    fn the_text_output_names_a_field_no_event_carried() {
        let mut sample = sample();
        sample.filtered = Some(crate::collect::Filtered {
            predicate: "comm == 'sleep'".into(),
            kept: 0,
            dropped: 5,
            unseen_fields: vec!["comm".into()],
        });
        let text = results(OutputFormat::Text, &sample).unwrap();
        assert!(text.contains("kept 0 of 5 events"), "{text}");
        assert!(
            text.contains("no event carried `comm`"),
            "an empty series must explain itself: {text}"
        );
    }

    fn correlated_run() -> RunOutput {
        RunOutput {
            results: vec![sample()],
            joins: vec![crate::join::Correlation {
                rule: "pages-of-allocation".into(),
                from: "mem.heap.alloc".into(),
                to: "mem.faults".into(),
                match_kind: MatchKind::Interval,
                evidence: crate::join::Evidence::Key,
                matched: 12,
                unmatched: 3,
                states: crate::join::StateStats {
                    opened: 4,
                    closed: 3,
                    still_open: 1,
                    peak_open: 2,
                    ..Default::default()
                },
                top: vec![crate::join::Attribution {
                    key: "mem:[0x1000,0x2000)".into(),
                    base: 0x1000,
                    len: 0x1000,
                    opened_ns: 10,
                    closed_ns: Some(20),
                    matched: 12,
                    outcome: crate::join::Outcome::Closed,
                }],
                total_attributed: 12,
            }],
        }
    }

    /// A run is one document because two would not be one: concatenating a
    /// results document and a correlation produces a file no parser accepts.
    /// This is what that regression would look like.
    #[test]
    fn a_whole_run_is_one_document_in_every_structured_format() {
        let run = correlated_run();
        for format in [OutputFormat::Json, OutputFormat::Yaml, OutputFormat::Toml] {
            let encoded = run_output(format, &run).expect("encoding");
            let decoded: RunOutput = match format {
                OutputFormat::Json => serde_json::from_str(&encoded).expect("one json document"),
                OutputFormat::Yaml => serde_norway::from_str(&encoded).expect("one yaml document"),
                OutputFormat::Toml => toml::from_str(&encoded).expect("one toml document"),
                _ => unreachable!(),
            };
            assert_eq!(decoded, run, "a run did not survive {format}");
        }
    }

    #[test]
    fn every_ndjson_line_of_a_run_parses_on_its_own() {
        let encoded = run_output(OutputFormat::Ndjson, &correlated_run()).expect("encoding");
        let lines: Vec<&str> = encoded.lines().collect();
        assert!(
            lines.len() >= 4,
            "a run header, a summary, two points, a join"
        );
        for line in &lines {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|error| panic!("`{line}` is not a document on its own: {error}"));
        }
        assert!(
            lines.iter().any(|line| line.contains("\"rule\"")),
            "the join belongs in the stream too"
        );
    }

    /// Text is read by a person, so what a person needs is not the number but
    /// what it does and does not mean.
    #[test]
    fn a_correlation_states_its_evidence_and_what_was_not_attributed() {
        let text = correlation(OutputFormat::Text, &correlated_run().joins[0]).unwrap();
        assert!(text.contains("evidence KEY"), "{text}");
        assert!(text.contains("3 fell outside"), "{text}");
        assert!(text.contains("no eviction"), "{text}");
        assert!(
            text.contains("counts faults, not bytes"),
            "the fault count must not read as a quantity of memory: {text}"
        );
    }

    #[test]
    fn an_eviction_is_never_quietly_absent_from_the_output() {
        let mut run = correlated_run();
        run.joins[0].states.expired = 7;
        let text = correlation(OutputFormat::Text, &run.joins[0]).unwrap();
        assert!(text.contains("EVICTED"), "{text}");
        assert!(
            text.contains("missing, not absent"),
            "an attribution lost to eviction looks exactly like one that never happened: {text}"
        );
    }

    /// A profile: stacks aggregated by identity, with the frames beside them.
    fn profile() -> Results {
        let mut results = sample();
        results.label = "profile".into();
        results.source = "cpu.profile".into();
        results.unit = Unit::Ns;
        results.outputs = vec![Output::Stack {
            stacks: vec![
                StackCount {
                    stack: 1,
                    events: 300,
                    total: 3_000_000.0,
                },
                StackCount {
                    stack: 2,
                    events: 7,
                    total: 70_000.0,
                },
                // Seen, counted, and with no frames to show for it.
                StackCount {
                    stack: 3,
                    events: 1,
                    total: 10_000.0,
                },
            ],
            unattributed: 4,
        }];
        results.stacks = BTreeMap::from([
            (1, vec!["main".into(), "work".into(), "read".into()]),
            (2, vec!["main".into(), "idle".into()]),
        ]);
        results
    }

    fn histogram() -> Results {
        let mut results = sample();
        results.unit = Unit::Bytes;
        results.outputs = vec![Output::Hist {
            layout: crate::emit::BucketLayout::Log2,
            series: vec![SeriesHist {
                key: "task:7".into(),
                count: 102,
                min: 1024.0,
                max: 1_048_576.0,
                sum: 2_199_552.0,
                buckets: vec![
                    Bucket {
                        low: 1024,
                        high: Some(2048),
                        count: 100,
                    },
                    Bucket {
                        low: 2048,
                        high: Some(4096),
                        count: 0,
                    },
                    Bucket {
                        low: 1_048_576,
                        high: Some(2_097_152),
                        count: 2,
                    },
                ],
                unbucketable: 0,
            }],
        }];
        results
    }

    fn ranked() -> Results {
        let mut results = sample();
        results.outputs = vec![Output::TopK {
            k: 2,
            series: vec![
                SeriesTotal {
                    key: "task:2".into(),
                    events: 1,
                    total: 300.0,
                },
                SeriesTotal {
                    key: "task:3".into(),
                    events: 2,
                    total: 100.0,
                },
            ],
            rest: Rest {
                series: 2,
                events: 2,
                total: 11.0,
            },
        }];
        results
    }

    /// A histogram in text is the buckets *and* the shape: the empty bucket
    /// between two modes is the thing being read for, so it is printed.
    #[test]
    fn a_histogram_prints_the_empty_bucket_between_two_modes() {
        let text = results(OutputFormat::Text, &histogram()).unwrap();
        assert!(text.contains("hist (log2)"), "{text}");
        assert!(text.contains("[1 KiB, 2 KiB)"), "{text}");
        assert!(
            text.contains("[2 KiB, 4 KiB)"),
            "the empty bucket between the modes is the shape: {text}"
        );
        assert!(text.contains("[1 MiB, 2 MiB)"), "{text}");
    }

    /// A ranking that does not say what it left out reads as a statement about
    /// everything that happened.
    #[test]
    fn a_topk_says_what_it_left_out() {
        let text = results(OutputFormat::Text, &ranked()).unwrap();
        assert!(text.contains("the 2 heaviest of 4 series"), "{text}");
        assert!(text.contains("the other 2 series"), "{text}");
    }

    /// Collapsed stacks, one line each: `root;...;leaf weight`. Parsed back
    /// rather than string-matched, because what matters is that a consumer of
    /// the format gets the numbers that went in.
    #[test]
    fn folded_round_trips_through_the_format_flamegraph_reads() {
        let run = RunOutput {
            results: vec![profile()],
            joins: Vec::new(),
        };
        let encoded = run_output(OutputFormat::Folded, &run).expect("encoding");

        let parsed: Vec<(String, i64)> = encoded
            .lines()
            .map(|line| {
                let (chain, weight) = line.rsplit_once(' ').expect("a weight after the frames");
                (chain.to_owned(), weight.parse().expect("an integer weight"))
            })
            .collect();
        assert_eq!(
            parsed,
            vec![
                ("main;work;read".to_owned(), 3_000_000),
                ("main;idle".to_owned(), 70_000),
                ("[unresolved stack 0x3]".to_owned(), 10_000),
            ],
            "{encoded}"
        );
    }

    /// Two sources in one file would merge into a flame graph of neither, so
    /// each keeps its label as a root frame.
    #[test]
    fn two_profiles_in_one_folded_file_stay_distinguishable() {
        let mut second = profile();
        second.label = "offcpu".into();
        let run = RunOutput {
            results: vec![profile(), second],
            joins: Vec::new(),
        };
        let encoded = run_output(OutputFormat::Folded, &run).expect("encoding");
        assert!(encoded.contains("profile;main;work;read "), "{encoded}");
        assert!(encoded.contains("offcpu;main;work;read "), "{encoded}");
    }

    /// Speedscope indexes frames into a shared table, so an index that points
    /// past the table is a file that opens empty. The weights are the other
    /// half: `endValue` is what the tool claims the profile adds up to.
    #[test]
    fn every_speedscope_frame_index_resolves_and_the_weights_add_up() {
        let run = RunOutput {
            results: vec![profile()],
            joins: Vec::new(),
        };
        let encoded = run_output(OutputFormat::Speedscope, &run).expect("encoding");
        let document: serde_json::Value =
            serde_json::from_str(&encoded).expect("one json document");

        let frames = document["shared"]["frames"].as_array().expect("frames");
        let profiles = document["profiles"].as_array().expect("profiles");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0]["type"], "sampled");
        assert_eq!(profiles[0]["unit"], "nanoseconds");

        let mut total = 0.0;
        for (sample, weight) in profiles[0]["samples"]
            .as_array()
            .expect("samples")
            .iter()
            .zip(profiles[0]["weights"].as_array().expect("weights"))
        {
            for index in sample.as_array().expect("a stack of frame indices") {
                let index = index.as_u64().expect("an index") as usize;
                assert!(index < frames.len(), "frame {index} is past the table");
            }
            total += weight.as_f64().expect("a weight");
        }
        assert_eq!(profiles[0]["endValue"].as_f64(), Some(total));
        assert_eq!(total, 3_080_000.0);
    }

    /// The reason speedscope is offered for something that is not a profile: a
    /// timeline becomes an evented strip, whose axis is time. Speedscope
    /// requires the events in non-decreasing order and properly closed, and a
    /// file that breaks either opens as an error rather than as a chart.
    #[test]
    fn a_timeline_becomes_a_well_formed_evented_profile() {
        let run = RunOutput {
            results: vec![sample()],
            joins: Vec::new(),
        };
        let encoded = run_output(OutputFormat::Speedscope, &run).expect("encoding");
        let document: serde_json::Value =
            serde_json::from_str(&encoded).expect("one json document");
        let profile = &document["profiles"][0];
        assert_eq!(profile["type"], "evented");
        assert_eq!(profile["unit"], "nanoseconds");

        let mut open: Vec<u64> = Vec::new();
        let mut last_at = 0;
        for event in profile["events"].as_array().expect("events") {
            let at = event["at"].as_u64().expect("a time");
            assert!(at >= last_at, "events are out of order at {at}");
            last_at = at;
            let frame = event["frame"].as_u64().expect("a frame index");
            match event["type"].as_str() {
                Some("O") => open.push(frame),
                Some("C") => assert_eq!(
                    open.pop(),
                    Some(frame),
                    "a close that does not match the frame it closes"
                ),
                other => panic!("unknown event type {other:?}"),
            }
        }
        assert!(open.is_empty(), "a frame was left open");
        assert_eq!(profile["endValue"].as_u64(), Some(1_000_000_000));
    }

    /// A profile format has nothing to say about a run that produced no stacks,
    /// and saying nothing is worse than saying so: an empty folded file opens
    /// as an empty flame graph, which reads as a program that did nothing.
    #[test]
    fn a_profile_format_refuses_a_run_that_produced_no_profile() {
        let run = RunOutput {
            results: vec![ranked()],
            joins: Vec::new(),
        };
        let error = run_output(OutputFormat::Folded, &run).unwrap_err();
        assert!(error.message().contains("emitted no stacks"), "{error}");
    }

    /// The other reason there is no profile, and a different answer: stacks
    /// were asked for and the collector captured none. A reader who cannot tell
    /// the two apart does not know whether to change the request or the build.
    #[test]
    fn a_profile_of_events_that_carried_no_stack_says_which_of_the_two_happened() {
        let mut empty = profile();
        empty.outputs = vec![Output::Stack {
            stacks: Vec::new(),
            unattributed: 5057,
        }];
        empty.stacks = BTreeMap::new();
        let run = RunOutput {
            results: vec![empty],
            joins: Vec::new(),
        };
        let error = run_output(OutputFormat::Folded, &run).unwrap_err();
        assert!(
            error
                .message()
                .contains("no event carried one, 5057 of them"),
            "{error}"
        );
    }

    /// What a format claims about itself has to match what it does. A merge
    /// once put `ndjson` back to "planned" while it was encoding perfectly
    /// well, and nothing noticed: the claim and the encoder are in different
    /// files and only a test crosses them.
    ///
    /// Each format is handed a run carrying an output it declares it can
    /// express, so that a refusal is about the encoder and not about the emit.
    #[test]
    fn every_format_does_what_its_declared_status_says() {
        let mut run = correlated_run();
        run.results.push(profile());
        run.results.push(histogram());
        for format in OutputFormat::ALL {
            assert!(
                run.results.iter().any(|results| results
                    .outputs
                    .iter()
                    .any(|output| format.supports_emit(output.kind()))),
                "`{format}` was given nothing it declares it can carry"
            );
            let encoded = run_output(format, &run);
            match format.status() {
                crate::format::FormatStatus::Ready => assert!(
                    encoded.is_ok(),
                    "`{format}` is declared ready and cannot encode: {:?}",
                    encoded.err()
                ),
                crate::format::FormatStatus::Planned { .. } => assert!(
                    encoded.is_err(),
                    "`{format}` is declared unwritten and encoded anyway"
                ),
            }
        }
    }

    #[test]
    fn a_format_without_an_encoder_says_so() {
        let error = results(OutputFormat::Pprof, &sample()).unwrap_err();
        assert!(error.message().contains("no encoder yet"), "{error}");
    }

    /// Every new emit reaches the stream too, and a consumer reading it must be
    /// able to tell a complete ranking from a truncated one and a whole profile
    /// from a tenth of one.
    #[test]
    fn the_stream_carries_what_each_new_emit_left_out() {
        let ranked = results(OutputFormat::Ndjson, &ranked()).unwrap();
        for line in ranked.lines() {
            serde_json::from_str::<serde_json::Value>(line).expect("a document per line");
        }
        assert!(ranked.contains("\"record\":\"topk_rest\""), "{ranked}");

        let profiled = results(OutputFormat::Ndjson, &profile()).unwrap();
        for line in profiled.lines() {
            serde_json::from_str::<serde_json::Value>(line).expect("a document per line");
        }
        assert!(
            profiled.contains("\"unattributed\":4"),
            "a profile missing four events says so: {profiled}"
        );
        assert!(
            profiled.contains("\"frames\":[\"main\",\"work\",\"read\"]"),
            "{profiled}"
        );
    }

    fn correlated(match_kind: MatchKind, key: &str, len: u64) -> Correlation {
        Correlation {
            rule: "r".into(),
            from: "a".into(),
            to: "b".into(),
            match_kind,
            evidence: crate::join::Evidence::Key,
            matched: 3,
            unmatched: 1,
            states: StateStats {
                opened: 2,
                closed: 1,
                still_open: 1,
                peak_open: 2,
                ..StateStats::default()
            },
            top: vec![Attribution {
                key: key.into(),
                base: 0,
                len,
                opened_ns: 0,
                closed_ns: None,
                matched: 3,
                outcome: Outcome::Closed,
            }],
            total_attributed: 3,
        }
    }

    /// The interval join's own words, which every other kind used to borrow.
    #[test]
    fn an_interval_join_reads_as_ranges_and_bytes() {
        let text = correlation_text(&correlated(
            MatchKind::Interval,
            "mem:[0x1000,0x2000)",
            4096,
        ));
        assert!(text.contains("fell outside every open range"), "{text}");
        assert!(text.contains("heaviest ranges"), "{text}");
        assert!(text.contains("RANGE"), "{text}");
        assert!(text.contains("4.0 KiB"), "{text}");
        assert!(text.contains("freed"), "{text}");
        // The fault-counting caveat belongs to this kind and only this one.
        assert!(text.contains("multi-size transparent huge"), "{text}");
    }

    /// A wakeup join borrowed all of it: it reported chains between tasks as
    /// ranges of zero bytes that had been "freed", and printed the page-fault
    /// caveat about a join that never looked at memory.
    #[test]
    fn a_wakeup_join_speaks_of_chains_not_of_ranges() {
        let text = correlation_text(&correlated(MatchKind::Wakeup, "task:7 -> task:9", 0));
        assert!(text.contains("followed no wakeup this run saw"), "{text}");
        assert!(text.contains("heaviest wakeup chains"), "{text}");
        assert!(text.contains("CHAIN"), "{text}");
        assert!(text.contains("ran"), "{text}");
        assert!(!text.contains("RANGE"), "{text}");
        assert!(!text.contains("SIZE"), "{text}");
        assert!(!text.contains("freed"), "{text}");
        assert!(
            !text.contains("0 B"),
            "a chain has no extent to report: {text}"
        );
        assert!(!text.contains("transparent huge"), "{text}");
    }

    /// The two keyed kinds differ from each other only in what an unattributed
    /// event means, which is the one place the scope matters to a reader.
    #[test]
    fn the_keyed_joins_name_the_key_and_the_scope() {
        let plain = correlation_text(&correlated(MatchKind::Eq, "req:42", 0));
        assert!(plain.contains("carried a key no open state had"), "{plain}");
        assert!(plain.contains("heaviest keys"), "{plain}");
        assert!(plain.contains("closed"), "{plain}");

        let scoped = correlation_text(&correlated(MatchKind::ScopedEq, "blk:0x10@99", 0));
        assert!(scoped.contains("in that scope"), "{scoped}");
    }
}
