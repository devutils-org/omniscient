//! Encoding results.
//!
//! Text is for reading, the structured formats are for tooling, and `ndjson` is
//! for a stream that does not fit in memory. Each format declares which emits it
//! can carry (see `format`), so this module never has to approximate: it either
//! knows how to write what it was handed, or it says which format would.

use std::fmt::Write as _;

use crate::catalog::{MatchKind, Unit};
use crate::collect::Losses;
use crate::emit::{Bucket, Output, Results, SeriesHist, SeriesKey, Shortfall, StackCount};
use crate::error::ParseError;
use crate::event::{Event, KeyValue};
use crate::format::OutputFormat;
use crate::join::{Breakdown, Correlation, DecompositionOutput, Evidence, Outcome, Row};
use crate::scalar::Duration;

/// Everything one run produced: what each source measured, what the joins
/// made of it together, and what the decompositions split it into.
///
/// It is one document rather than several because several would not be one:
/// concatenating two JSON documents produces a file no parser accepts, and a
/// consumer of the results needs the joins beside them anyway.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RunOutput {
    pub results: Vec<Results>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub joins: Vec<Correlation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decompositions: Vec<DecompositionOutput>,
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
            // What each level of evidence the run's tables use establishes,
            // and what it does not, said once, under the first table that
            // shows one: a bare `TEMPORAL` reads as a verdict.
            let levels = evidence_used(output);
            let mut explained = false;
            let mut explain = |text: &mut String, shown: bool| {
                if shown && !explained {
                    explained = true;
                    text.push_str(&evidence_text(&levels));
                }
            };
            for correlation in &output.joins {
                text.push('\n');
                text.push_str(&self::correlation(format, correlation)?);
                explain(&mut text, true);
            }
            for decomposition in &output.decompositions {
                text.push('\n');
                text.push_str(&self::decomposition(format, decomposition)?);
                let shown = decomposition
                    .tables
                    .iter()
                    .flat_map(|table| table.rows.iter().chain(std::iter::once(&table.whole)))
                    .any(|row| row.evidence.is_some());
                explain(&mut text, shown);
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
            for decomposition in &output.decompositions {
                text.push_str(&self::decomposition(format, decomposition)?);
            }
            Ok(text)
        }
        OutputFormat::Folded => folded(output),
        OutputFormat::Speedscope => speedscope(output),
        OutputFormat::ChromeTrace => chrome(output),
        // The one format whose file is not text. It is encoded all the same
        // and the bytes dropped, so that a run with nothing to profile hears
        // about the profile rather than about the pipe: a reader who asked for
        // `pprof` and captured no stacks has to change the request.
        format if format.is_binary() => {
            run_output_bytes(format, output)?;
            Err(ParseError::new(format!(
                "`{format}` is {}, which is not text: write it to a file with `--out <dir>`",
                format.describe()
            )))
        }
        format if format.renders_documents() => format.serialize_document(output),
        format => Err(ParseError::new(format!(
            "`{format}` has no encoder yet: {}",
            format.describe()
        ))),
    }
}

/// Every level of evidence a run's joins and decompositions show, weakest
/// first.
fn evidence_used(output: &RunOutput) -> Vec<crate::join::Evidence> {
    let mut shown: Vec<crate::join::Evidence> = Vec::new();
    for correlation in &output.joins {
        shown.push(correlation.evidence);
        shown.extend(correlation.within_key);
    }
    for decomposition in &output.decompositions {
        for table in &decomposition.tables {
            shown.extend(
                table
                    .rows
                    .iter()
                    .chain(std::iter::once(&table.whole))
                    .filter_map(|row| row.evidence),
            );
        }
    }
    crate::join::Evidence::LADDER
        .into_iter()
        .filter(|level| shown.contains(level))
        .collect()
}

/// The sentence of each level: what it establishes, and what it does not.
fn evidence_text(levels: &[crate::join::Evidence]) -> String {
    let mut out = String::new();
    if levels.is_empty() {
        return out;
    }
    out.push('\n');
    for level in levels {
        let _ = writeln!(
            out,
            "  evidence {level}: {}; it does not establish {}",
            level.establishes(),
            level.does_not_establish()
        );
    }
    out
}

/// Encodes a whole run in the format's own bytes.
///
/// The general entry point, and the only one that can carry every format:
/// `pprof` is a gzipped protobuf and a `String` cannot hold one. Every other
/// format is UTF-8 and comes through [`run_output`] unchanged, so a caller that
/// writes bytes writes all nine and a caller that prints text writes eight and
/// says why.
pub fn run_output_bytes(format: OutputFormat, output: &RunOutput) -> Result<Vec<u8>, ParseError> {
    match format {
        OutputFormat::Pprof => pprof(output),
        format => run_output(format, output).map(String::into_bytes),
    }
}

/// One document of a recording, as the structured formats carry it: the
/// trigger that wrote it beside the run output of its window, so the file
/// stands on its own.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Triggered<'a> {
    pub trigger: &'a crate::collect::Fired,
    #[serde(flatten)]
    pub run: &'a RunOutput,
}

/// Encodes one document of a recording.
///
/// The formats that describe documents carry the trigger inline: text as a
/// header above the window's results, `ndjson` as its first line, JSON, YAML
/// and TOML as a `trigger` field beside `results`. The four profile formats
/// have nowhere to put it -- a flame graph has frames, not fields -- and
/// carry the window alone; the recording's own record, `recording.json`,
/// names every trigger beside the document it wrote.
pub fn triggered_bytes(
    format: OutputFormat,
    fired: &crate::collect::Fired,
    run: &RunOutput,
) -> Result<Vec<u8>, ParseError> {
    match format {
        OutputFormat::Text => {
            let mut text = trigger_text(fired);
            text.push('\n');
            text.push_str(&run_output(format, run)?);
            Ok(text.into_bytes())
        }
        OutputFormat::Ndjson => {
            let line = serde_json::to_string(&serde_json::json!({ "trigger": fired }))
                .map_err(|error| ParseError::new(format!("cannot encode the trigger: {error}")))?;
            let mut text = format!("{line}\n");
            text.push_str(&run_output(format, run)?);
            Ok(text.into_bytes())
        }
        format if format.renders_documents() => format
            .serialize_document(&Triggered {
                trigger: fired,
                run,
            })
            .map(String::into_bytes),
        format => run_output_bytes(format, run),
    }
}

/// The header of a triggered document, in text.
pub fn trigger_text(fired: &crate::collect::Fired) -> String {
    let at = |ns: u64| ns.saturating_sub(fired.run_opened_ns) as f64 / 1e9;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "document {} of at most {}: `{}` fired on `{}` at {:.3} s into the run",
        fired.number,
        fired.of_at_most,
        fired.predicate,
        fired.on,
        at(fired.fired_ns)
    );
    let fields: Vec<String> = fired
        .fields
        .iter()
        .map(|(field, value)| match value.as_f64() {
            Some(number) => format!("{field} = {number}"),
            None => format!("{field} = {value:?}"),
        })
        .collect();
    let _ = writeln!(
        out,
        "  fired by the event at CLOCK_MONOTONIC {} ns{}",
        fired.event_ts_ns,
        if fired.fields.is_empty() {
            String::new()
        } else {
            format!(", with {}", fields.join(", "))
        }
    );
    let _ = writeln!(
        out,
        "  window {:.3} s to {:.3} s into the run (CLOCK_MONOTONIC {} to {} ns): retain {}, after {}",
        at(fired.opened_ns),
        at(fired.closed_ns),
        fired.opened_ns,
        fired.closed_ns,
        crate::scalar::Duration::from_nanos(fired.retain_ns),
        crate::scalar::Duration::from_nanos(fired.after_ns),
    );
    if let Some(why) = &fired.cut_short {
        let _ = writeln!(out, "  cut short: {why}");
    }
    if fired.fired_again > 0 {
        let _ = writeln!(
            out,
            "  fired again {} time(s) while this document was open: those rounds are in it",
            fired.fired_again
        );
    }
    for evicted in fired
        .evicted
        .iter()
        .filter(|one| one.events > 0 || one.rounds > 0)
    {
        let _ = writeln!(
            out,
            "  evicted before it: `{}`, {} event(s) over {} round(s), read and in no document",
            evicted.request, evicted.events, evicted.rounds
        );
    }
    out
}

/// What a recording did, in text: for the end of a run, on stderr.
pub fn recorded_text(recorded: &crate::collect::Recorded) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "recording: {} document(s) of at most {}, trigger `{}` on `{}`, retain {}; {}",
        recorded.documents.len(),
        recorded.documents_at_most,
        recorded.predicate,
        recorded.on,
        crate::scalar::Duration::from_nanos(recorded.retain_ns),
        recorded.stopped_because
    );
    if recorded.documents.is_empty() {
        let _ = writeln!(
            out,
            "recording: the trigger never fired in the {:.3} s the run lasted, so no document \
             was written",
            recorded.closed_ns.saturating_sub(recorded.opened_ns) as f64 / 1e9
        );
    }
    for unrecorded in recorded
        .unrecorded
        .iter()
        .filter(|one| one.events > 0 || one.rounds > 0)
    {
        let _ = writeln!(
            out,
            "recording: `{}` read {} event(s) over {} round(s) after the last document{}, and \
             they are in none",
            unrecorded.request,
            unrecorded.events,
            unrecorded.rounds,
            if recorded.documents.is_empty() {
                " -- that is, over the whole run"
            } else {
                ""
            }
        );
    }
    out
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
    match correlation.within_key {
        Some(within) => {
            let _ = writeln!(
                out,
                "join {}  {} -> {}  evidence {}, {within} within the key",
                correlation.rule, correlation.from, correlation.to, correlation.evidence
            );
            let _ = writeln!(
                out,
                "  the key says whose each event is; which of that key's successive states it \
                 went to, the clock decided: {} states were replaced by the next of the same key",
                correlation.states.replaced
            );
        }
        None => {
            let _ = writeln!(
                out,
                "join {}  {} -> {}  evidence {}",
                correlation.rule, correlation.from, correlation.to, correlation.evidence
            );
        }
    }
    if correlation.match_kind == MatchKind::Eq && correlation.evidence == Evidence::Temporal {
        let _ = writeln!(
            out,
            "  `eq` compares a recycled identifier without the scope that says which life of it \
             is meant, so an event lands on whichever life held it last: two events at one \
             address around the same time, which is TEMPORAL. `scoped_eq` tells the lives apart"
        );
    }
    if correlation.match_kind == MatchKind::Between {
        between_text(correlation, &mut out);
        return out;
    }
    if correlation.match_kind == MatchKind::Overlap {
        overlap_text(correlation, &mut out);
        return out;
    }

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
    // Outside the share above on purpose: an event with no value for the key
    // could never have matched, and folding it into the denominator would
    // make a rule look worse at the thing it does than it is.
    if correlation.keyless > 0 {
        let _ = writeln!(
            out,
            "  {} more `{}` events carried no value for the key at all, so no state could hold \
             them; counted apart, not refused",
            correlation.keyless, correlation.to
        );
    }
    outside_window_text(correlation, &mut out);

    let states = &correlation.states;
    let _ = writeln!(
        out,
        "  states: {} opened, {} closed, {} still open at the end, peak {} held at once",
        states.opened, states.closed, states.still_open, states.peak_open
    );
    window_closed_text(correlation, &mut out);
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
    let chains = correlation.match_kind == MatchKind::Wakeup;
    let (heading, column) = table_words(correlation.match_kind);
    let _ = writeln!(out, "\n  {heading}");
    // A chain is as wide as its hops: two of them with an interrupt at the far
    // end (`interrupt (hardirq) -> task:128611 -> task:128610`) is 46
    // characters, and a fixed column ran it into the next.
    let width = correlation
        .top
        .iter()
        .map(|attribution| attribution.key.chars().count())
        .max()
        .unwrap_or(0)
        .max(44);
    if chains {
        let _ = writeln!(
            out,
            "    {column:<width$} {:>8} {:>8}  LAST",
            "WAKEUPS", "EVENTS"
        );
        for attribution in &correlation.top {
            let _ = writeln!(
                out,
                "    {:<width$} {:>8} {:>8}  {}",
                attribution.key,
                attribution.wakeups.unwrap_or(1),
                attribution.matched,
                outcome(attribution.outcome, correlation.match_kind)
            );
        }
        return out;
    }
    if correlation.match_kind == MatchKind::Woke {
        // Each row is a waker and what it woke: how many closes it released,
        // and the time from each close to the wakeup, summed and per pair --
        // the amount a `paired` decomposition part reads.
        let _ = writeln!(
            out,
            "    {column:<width$} {:>8} {:>12} {:>12}  OUTCOME",
            "PAIRS", "WAITED", "MEAN"
        );
        for attribution in &correlation.top {
            let pairs = attribution.wakeups.unwrap_or(attribution.matched).max(1);
            let waited = attribution
                .change
                .as_ref()
                .map_or(0.0, |change| change.delta);
            let _ = writeln!(
                out,
                "    {:<width$} {:>8} {:>12} {:>12}  {}",
                attribution.key,
                pairs,
                duration(waited),
                duration(waited / pairs as f64),
                outcome(attribution.outcome, correlation.match_kind)
            );
        }
        let waited: f64 = correlation
            .top
            .iter()
            .filter_map(|attribution| attribution.change.as_ref())
            .map(|change| change.delta)
            .sum();
        let _ = writeln!(
            out,
            "\n  WAITED is from each close to the wakeup of its task that followed it, summed over \
             the rows shown: {} in all",
            duration(waited)
        );
        return out;
    }
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

/// The downstream events a join did not look up, because the `from` source
/// was not attached when they happened. Outside the share on purpose, as the
/// keyless ones are: the run was not watching for what would have matched
/// them, and counting them as unmatched would say it was.
fn outside_window_text(correlation: &Correlation, out: &mut String) {
    if correlation.outside_window > 0 {
        let _ = writeln!(
            out,
            "  {} more `{}` events fell outside `{}`'s window, when it was not attached: \
             neither matched nor unmatched, since the run was not watching for what would have \
             opened or closed a state for them",
            correlation.outside_window, correlation.to, correlation.from
        );
    }
}

/// The states the `from` source's detach ended, which are not evictions: the
/// rule's window did not run out, the run stopped watching.
fn window_closed_text(correlation: &Correlation, out: &mut String) {
    if correlation.states.window_closed > 0 {
        let _ = writeln!(
            out,
            "  {} more ended when `{}` was detached: nothing after it was credited to them",
            correlation.states.window_closed, correlation.from
        );
    }
}

/// What an unattributed downstream event means, which is a different sentence
/// for each match kind.
fn unmatched_reason(kind: MatchKind) -> &'static str {
    match kind {
        MatchKind::Interval => "fell outside every open range",
        MatchKind::Eq => "carried a key no open state had",
        MatchKind::ScopedEq => "carried a key no open state had in that scope",
        MatchKind::Wakeup => "followed no wakeup this run saw",
        MatchKind::Between => "moved in no interval where a state opened or closed",
        MatchKind::Woke => "were followed by no wakeup of their task",
        MatchKind::Overlap => "overlapped no reading's interval",
    }
}

/// The heading over the attributions, and what the first column holds.
fn table_words(kind: MatchKind) -> (&'static str, &'static str) {
    match kind {
        MatchKind::Interval => ("heaviest ranges", "RANGE"),
        MatchKind::Eq | MatchKind::ScopedEq => ("heaviest keys", "KEY"),
        MatchKind::Wakeup => ("heaviest wakeup chains", "CHAIN"),
        MatchKind::Between => ("heaviest intervals", "KEY"),
        MatchKind::Woke => ("heaviest wakers", "CHAIN"),
        MatchKind::Overlap => ("heaviest intervals", "KEY"),
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
            "EVENTS counts faults, not bytes, and does not even rank by bytes: how many faults a \
             range takes depends on how the kernel backs it -- huge pages, multi-size folios or \
             base pages -- and on where it lands, so two ranges of one size can differ by two \
             orders of magnitude. Read it as where the faults happened, not as how much memory \
             they cost; the THP mode is in the environment.",
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
        (Outcome::WindowClosed, _) => "source detached",
        // Not "never ran": a chain with events attributed to it did run, and
        // what is true of it is that no later wakeup ended it.
        (Outcome::StillOpen, MatchKind::Wakeup) => "still open at the end",
        (Outcome::StillOpen, _) => "live",
    }
}

/// The body of a `between` correlation, which counts readings and credits
/// changes rather than attributing events.
fn between_text(correlation: &Correlation, out: &mut String) {
    let total = correlation.matched + correlation.unmatched;
    let share = if total == 0 {
        0.0
    } else {
        100.0 * correlation.matched as f64 / total as f64
    };
    let _ = writeln!(
        out,
        "  {} of {} `{}` readings moved in an interval where a `{}` state opened or closed \
         ({share:.1} %)",
        correlation.matched, total, correlation.to, correlation.from
    );
    if let Some(readings) = &correlation.readings {
        let quiet = correlation.unmatched.saturating_sub(readings.baselines);
        let amount = |value: f64| magnitude(&readings.value, value);
        let _ = writeln!(
            out,
            "  the other {}: {} the first reading of its key, with nothing to differ from; {} \
             moved by {} with nothing opening or closing around them",
            correlation.unmatched,
            readings.baselines,
            quiet,
            amount(readings.uncredited)
        );
        let _ = writeln!(
            out,
            "  `{}` read as a {}; credited in all: {}",
            readings.value,
            readings.reading.as_str(),
            amount(readings.credited)
        );
        let states = &correlation.states;
        let _ = writeln!(
            out,
            "  states: {} opened or closed, {} taken into an interval, {} before the first \
             reading of their key, {} after the last",
            states.opened, states.closed, readings.before_first, states.still_open
        );
        outside_window_text(correlation, out);
        window_closed_text(correlation, out);
        if !states.is_clean() {
            let _ = writeln!(
                out,
                "  EVICTED: {} expired past the window, {} dropped because the map was full. \
                 Credits are missing, not absent.",
                states.expired, states.overflowed
            );
        }

        if correlation.top.is_empty() {
            let _ = writeln!(out, "\n  nothing was credited");
        } else {
            let origin = correlation
                .top
                .iter()
                .map(|attribution| attribution.opened_ns)
                .min()
                .unwrap_or(0);
            let ms = |ns: u64| ns.saturating_sub(origin) as f64 / 1e6;
            let _ = writeln!(
                out,
                "\n  heaviest intervals, in ms from the first shown\n    {:<24} {:>10} {:>10} \
                 {:>14} {:>7} {:>7}",
                "KEY", "FROM", "TO", "CHANGE", "OPENED", "CLOSED"
            );
            for attribution in &correlation.top {
                let Some(change) = &attribution.change else {
                    continue;
                };
                let _ = writeln!(
                    out,
                    "    {:<24} {:>10.1} {:>10.1} {:>14} {:>7} {:>7}",
                    attribution.key,
                    ms(attribution.opened_ns),
                    ms(attribution.closed_ns.unwrap_or(attribution.opened_ns)),
                    amount(change.delta),
                    change.opened,
                    change.closed
                );
            }
        }
    }
    let _ = writeln!(
        out,
        "\n  TEMPORAL, and nothing more: each change is credited to what opened or closed while \
         it happened. That is consistent with one moving the other, and it does not say which \
         state moved the total, or by how much."
    );
}

/// The body of an `overlap` correlation: each downstream event's value split
/// across the upstream readings' intervals it overlapped, by time.
fn overlap_text(correlation: &Correlation, out: &mut String) {
    let total = correlation.matched + correlation.unmatched;
    let share = if total == 0 {
        0.0
    } else {
        100.0 * correlation.matched as f64 / total as f64
    };
    let _ = writeln!(
        out,
        "  {} of {} `{}` events overlapped an interval of `{}`'s readings ({share:.1} %); {} {}",
        correlation.matched,
        total,
        correlation.to,
        correlation.from,
        correlation.unmatched,
        unmatched_reason(correlation.match_kind)
    );
    if correlation.keyless > 0 {
        let _ = writeln!(
            out,
            "  {} more `{}` events carried no value for the key at all, so no reading could hold \
             them; counted apart, not refused",
            correlation.keyless, correlation.to
        );
    }
    let Some(readings) = &correlation.readings else {
        outside_window_text(correlation, out);
        return;
    };
    let amount = |value: f64| magnitude(&readings.value, value);
    let _ = writeln!(
        out,
        "  `{}` split by the share of each event's span inside each interval: credited in all \
         {}, and {} fell before the first reading of its key or after the last",
        readings.value,
        amount(readings.credited),
        amount(readings.uncredited)
    );
    let _ = writeln!(
        out,
        "  {} readings were the first of their key, with nothing before them to cover",
        readings.baselines
    );
    outside_window_text(correlation, out);
    window_closed_text(correlation, out);

    if correlation.top.is_empty() {
        let _ = writeln!(out, "\n  nothing was credited");
        return;
    }
    let origin = correlation
        .top
        .iter()
        .map(|attribution| attribution.opened_ns)
        .min()
        .unwrap_or(0);
    let ms = |ns: u64| ns.saturating_sub(origin) as f64 / 1e6;
    let _ = writeln!(
        out,
        "\n  heaviest intervals, in ms from the first shown\n    {:<24} {:>10} {:>10} {:>14} {:>8}",
        "KEY", "FROM", "TO", "CREDITED", "EVENTS"
    );
    for attribution in &correlation.top {
        let credited = attribution
            .change
            .as_ref()
            .map_or(0.0, |change| change.delta);
        let _ = writeln!(
            out,
            "    {:<24} {:>10.1} {:>10.1} {:>14} {:>8}",
            attribution.key,
            ms(attribution.opened_ns),
            ms(attribution.closed_ns.unwrap_or(attribution.opened_ns)),
            amount(credited),
            attribution.matched
        );
    }
}

/// A signed amount of what an attribute holds, for reading only: the
/// structured formats carry the number as it was.
fn magnitude(attr: &str, value: f64) -> String {
    let sign = if value < 0.0 { "-" } else { "+" };
    if attr == "bytes" || attr.ends_with("_bytes") {
        format!("{sign}{}", bytes(value.abs()))
    } else if attr == "ns" || attr.ends_with("_ns") {
        format!("{sign}{}", duration(value.abs()))
    } else {
        format!("{value:+}")
    }
}

/// A signed amount in a decomposition's unit.
fn amount_in(unit: Unit, value: f64) -> String {
    let sign = if value < 0.0 { "-" } else { "" };
    match unit {
        Unit::Ns => format!("{sign}{}", duration(value.abs())),
        Unit::Bytes => format!("{sign}{}", bytes(value.abs())),
        _ => format!("{value}"),
    }
}

/// Encodes a decomposition in the session's output format.
///
/// Every format that carries a join carries this beside it, and for the same
/// reason: `OutputFormat::carries_correlations` says which, and the profile
/// and trace formats are not among them.
pub fn decomposition(
    format: OutputFormat,
    decomposition: &DecompositionOutput,
) -> Result<String, ParseError> {
    match format {
        OutputFormat::Text => Ok(decomposition_text(decomposition)),
        OutputFormat::Ndjson => {
            let line = serde_json::to_string(decomposition).map_err(|error| {
                ParseError::new(format!("cannot encode the decomposition: {error}"))
            })?;
            Ok(format!("{line}\n"))
        }
        format if format.renders_documents() => format.serialize_document(decomposition),
        format => Err(ParseError::new(format!(
            "`{format}` has no encoder for a decomposition: {}",
            format.describe()
        ))),
    }
}

fn decomposition_text(decomposition: &DecompositionOutput) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "decompose {}  per {}, in {}",
        decomposition.id, decomposition.by, decomposition.unit
    );
    if decomposition.tables.is_empty() {
        let _ = writeln!(
            out,
            "  no event of any part carried a `{}` key, so there is nothing to decompose",
            decomposition.by
        );
        return out;
    }
    if decomposition.keys_seen > decomposition.tables.len() {
        let _ = writeln!(
            out,
            "  the {} heaviest of {} keys the parts named",
            decomposition.tables.len(),
            decomposition.keys_seen
        );
    }
    for table in &decomposition.tables {
        breakdown_text(decomposition.unit, table, &mut out);
    }
    let _ = writeln!(
        out,
        "\n  A negative residue is something counted twice; a positive one is a state no row \
         covers."
    );
    out
}

fn breakdown_text(unit: Unit, table: &Breakdown, out: &mut String) {
    let amount = |row: &Row| match (row.amount, &row.short) {
        (Some(value), Some(_)) => format!("{} {}", Shortfall::MARK, amount_in(unit, value)),
        (Some(value), None) => amount_in(unit, value),
        (None, _) => "not read".to_owned(),
    };
    let resolution = |row: &Row| match row.resolution {
        Some(0.0) => "exact".to_owned(),
        Some(value) => format!("+-{}", amount_in(unit, value)),
        None => "-".to_owned(),
    };
    let share = |row: &Row| match (row.amount, table.whole.amount) {
        (Some(value), Some(whole)) if whole != 0.0 && row.state != "whole" => {
            format!("{:.1} %", 100.0 * value / whole)
        }
        _ => String::new(),
    };

    let _ = writeln!(out, "\n  {}", table.key);
    let _ = writeln!(
        out,
        "    {:<12} {:<18} {:<12} {:<8} {:<9} {:>14} {:>12} {:>7}",
        "STATE", "SOURCE", "VALUE", "KIND", "EVIDENCE", "AMOUNT", "RESOLUTION", "SHARE"
    );
    for row in table.rows.iter().chain(std::iter::once(&table.whole)) {
        if row.state == "whole" {
            let _ = writeln!(
                out,
                "    {:<12} {:>65}",
                "parts",
                table
                    .parts
                    .map_or("not read".to_owned(), |parts| amount_in(unit, parts))
            );
        }
        // What the row names, after its share, as the roadmap's tree prints
        // `-> holder tid=8821`. How many events agreed is said only when they
        // did not all agree: a name that is only the most frequent answer
        // reads differently from one every event gave.
        let named = row.named.as_ref().map_or(String::new(), |named| {
            if named.events == named.of {
                format!("  -> {}={}", named.attr, named.value)
            } else {
                format!(
                    "  -> {}={} ({} of {})",
                    named.attr, named.value, named.events, named.of
                )
            }
        });
        let _ = writeln!(
            out,
            "    {:<12} {:<18} {:<12} {:<8} {:<9} {:>14} {:>12} {:>7}{named}",
            row.state,
            row.source,
            row.value,
            row.kind.as_str(),
            row.evidence.map_or("-", |evidence| evidence.as_str()),
            amount(row),
            resolution(row),
            share(row)
        );
    }
    let residue = table.residue.map_or("not read".to_owned(), |residue| {
        let sign = if residue >= 0.0 { "+" } else { "" };
        format!("{sign}{}", amount_in(unit, residue))
    });
    let _ = writeln!(
        out,
        "    {:<12} {:<18} {:<12} {:<8} {:<9} {:>14} {:>12}",
        "RESIDUE",
        "whole - parts",
        "",
        "",
        "",
        residue,
        table
            .residue_resolution
            .map_or("-".to_owned(), |value| format!(
                "+-{}",
                amount_in(unit, value)
            ))
    );
    for row in table.rows.iter().chain(std::iter::once(&table.whole)) {
        if let Some(note) = &row.note {
            let _ = writeln!(out, "      {}: {note}", row.state);
        }
        if let Some(missing) = &row.missing {
            let _ = writeln!(out, "      {}: not read, {missing}", row.state);
        }
        if let Some(short) = &row.short {
            let _ = writeln!(
                out,
                "      {}: a lower bound, `{}` says: {}",
                row.state, row.source, short.note
            );
        }
    }
    let kinds: std::collections::BTreeSet<&str> =
        table.rows.iter().map(|row| row.kind.as_str()).collect();
    if kinds.len() > 1 {
        let _ = writeln!(
            out,
            "      the rows are {} numbers, which are not the same kind of number: a sample count is \
             a count times a period, a traced sum is every occurrence added up, a poll is short \
             by what came after its last reading",
            kinds.into_iter().collect::<Vec<_>>().join(" and ")
        );
    }
}

/// Encodes results in the session's output format.
pub fn results(format: OutputFormat, results: &Results) -> Result<String, ParseError> {
    match format {
        OutputFormat::Text => Ok(text(results)),
        OutputFormat::Ndjson => ndjson(results),
        // A profile format is a file, not a fragment: it has a header, a shared
        // frame table and a list of profiles. One result is a run of one, and
        // so is one trace.
        OutputFormat::Folded
        | OutputFormat::Speedscope
        | OutputFormat::Pprof
        | OutputFormat::ChromeTrace => run_output(
            format,
            &RunOutput {
                results: vec![results.clone()],
                joins: Vec::new(),
                decompositions: Vec::new(),
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
        "window": results.window,
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
            Output::Timeline { series, .. } => {
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
                ..
            } => {
                for stack in stacks {
                    let mut line = serde_json::json!({
                        "record": "stack",
                        "stack": stack.stack,
                        "events": stack.events,
                        "total": stack.total,
                        "frames": results.stacks.get(&stack.stack),
                    });
                    if let Some(series) = &stack.series {
                        line["series"] = serde_json::Value::from(series.to_string());
                    }
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

/// The one line that says what a run did not see.
///
/// All five counted kinds, named, in every format that has a line to put them
/// on. Three of them used to be printed and the other two were left to be
/// inferred from how many reason lines followed, which made the ordinary loss
/// of `/proc/diskstats` -- a line this build cannot decode -- announce `LOSS`
/// and then three zeros. A reader deciding whether to trust the numbers was
/// reading a headline that disagreed with the run.
///
/// The zeros are kept rather than elided. The line is only printed when
/// something was lost, and which of the five is zero is the part that says
/// where to look: a run losing only `undecoded` is a parsing gap, and one
/// losing only `throttled` is a kernel that declined to deliver. Printing the
/// non-zero ones alone would make the vocabulary itself depend on the run.
fn loss_headline(losses: &Losses) -> String {
    let mut line = format!(
        "LOSS: {} dropped, {} failed reads, {} late ticks, {} undecoded, {} throttled",
        losses.dropped, losses.failed_reads, losses.late_ticks, losses.undecoded, losses.throttled
    );
    // Only a recording evicts, and a sixth zero on every other run would be
    // a word about a mode it is not in.
    if losses.evicted > 0 {
        let _ = write!(line, ", {} evicted by the retention", losses.evicted);
    }
    line
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
    // The launched program's own life, which is a different number from the
    // run's window and the one a residue is a residue of. The bracket goes
    // beside the length because a reader given only the length would trust a
    // residue smaller than the bracket.
    if let Some(life) = &results.lifetime {
        let lived = life.exited_ns.saturating_sub(life.released_ns);
        let ending = if life.terminated {
            "stopped by the run".to_owned()
        } else {
            format!(
                "the exit bracketed to the last {}",
                approximate(life.exited_err_ns)
            )
        };
        let cpu = life
            .cpu_ns
            .map(|cpu| format!(", {} of cpu at the reap", approximate(cpu)))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "  the launched program ran {} (gate to exit; {ending}){cpu}",
            approximate(lived)
        );
    }

    if results.losses.is_clean() {
        let _ = writeln!(out, "  no loss: every read succeeded and no tick ran late");
    } else {
        let _ = writeln!(out, "  {}", loss_headline(&results.losses));
    }
    // A reason is printed whatever the counters say. A collector that explains
    // a result without having lost anything -- "no allocation was reported:
    // statically linked" -- noted it for the reader, and hiding it behind a
    // clean headline left a silent zero with its explanation one branch away.
    for (reason, count) in &results.losses.reasons {
        let _ = writeln!(out, "    {count}x {reason}");
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
    for note in &results.notes {
        let _ = writeln!(out, "  note: {note}");
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
                                "    {:<14} {}{} over {}{}  {}  {}",
                                rate.key,
                                bound(rate.short.as_ref()),
                                quantity(rate.total, results.unit),
                                approximate(rate.window_ns),
                                if rate.counted_window {
                                    " the counter was enabled"
                                } else {
                                    ""
                                },
                                comparison(rate.short.as_ref()),
                                per_second_quantity(per_second, results.unit),
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
                    if rate.unmeasured > 0 {
                        let _ = writeln!(
                            out,
                            "    {:<14} and {} events that carried no value, in no total",
                            "", rate.unmeasured
                        );
                    }
                }
                push_shortfall(&mut out, series.iter().find_map(|one| one.short.as_ref()));
            }
            Output::Summary { series } => {
                let _ = writeln!(out, "  summary");
                for summary in series {
                    // The mark goes on the three numbers a loss pulls
                    // downwards and on no others. `min` is the one part of the
                    // shape a lossy run does not move -- the events it is
                    // missing are the slow ones -- and `last` is a value, not a
                    // statistic over the run.
                    let short = comparison(summary.short.as_ref());
                    // The total where the values add up, marked as `n` is:
                    // a loss pulls it down the same way.
                    let total = summary.total.map_or_else(String::new, |total| {
                        format!(
                            "  total {}{}",
                            quantity(total, results.unit),
                            bound(summary.short.as_ref())
                        )
                    });
                    // The attributes asked added up beside the value, each a
                    // count of its own (`--sum errors`).
                    let sums: String = summary
                        .sums
                        .iter()
                        .map(|(name, sum)| format!("  {name} {sum}"))
                        .collect();
                    let _ = writeln!(
                        out,
                        "    {:<14} n{short}{:<5} min {}  mean {}{}  max {}{}  last {}{total}{sums}",
                        summary.key,
                        summary.count,
                        quantity(summary.min, results.unit),
                        bound(summary.short.as_ref()),
                        quantity(summary.mean, results.unit),
                        bound(summary.short.as_ref()),
                        quantity(summary.max, results.unit),
                        quantity(summary.last, results.unit),
                    );
                    if summary.unmeasured > 0 {
                        let _ = writeln!(
                            out,
                            "    {:<14} and {} events that carried no value, in none of the above",
                            "", summary.unmeasured
                        );
                    }
                }
                push_shortfall(&mut out, series.iter().find_map(|one| one.short.as_ref()));
            }
            Output::Timeline { series, .. } => {
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
                    if series.unmeasured > 0 {
                        let _ = writeln!(
                            out,
                            "      and {} events that carried no value, in no point",
                            series.unmeasured
                        );
                    }
                }
            }
            Output::Hist { layout, series } => {
                let _ = writeln!(out, "  hist ({layout})");
                for histogram in series {
                    hist_text(&mut out, histogram, results.unit);
                }
                push_shortfall(&mut out, series.iter().find_map(|one| one.short.as_ref()));
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
                        "    {:>2}. {:<20} {:>2}{:>12}  over {} events",
                        rank + 1,
                        total.key,
                        bound(total.short.as_ref()),
                        quantity(total.total, results.unit),
                        total.events
                    );
                    if total.unmeasured > 0 {
                        let _ = writeln!(
                            out,
                            "        and {} events that carried no value",
                            total.unmeasured
                        );
                    }
                }
                // A ranking that does not say what it left out reads as a
                // statement about everything.
                let counted_nothing = rest.is_empty()
                    && series.len() == 1
                    && series[0].events == 0
                    && series[0].unmeasured == 0
                    && series[0].total == 0.0;
                if counted_nothing {
                    let _ = writeln!(
                        out,
                        "    nothing was counted over the window: the one row is the target's, \
                         at zero, rather than no row"
                    );
                } else if rest.is_empty() {
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
                // Separate from the line above on purpose: that is what the
                // ranking measured and left out, this is what nothing measured.
                if let Some(short) = series.iter().find_map(|one| one.short.as_ref()) {
                    push_shortfall(&mut out, Some(short));
                    if short.bounds_below() {
                        let _ = writeln!(
                            out,
                            "    the totals are lower bounds, so the order above can be wrong"
                        );
                    }
                }
            }
            Output::Stack {
                stacks,
                unattributed,
                short,
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
                // Records that never arrived, as against the events above that
                // arrived without a stack: two facts, said as two lines.
                push_shortfall(&mut out, short.as_ref());
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
                ..
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
        "`stack` was asked for and no event carried one, {unattributed} of them; \
         the backend that served this request captured none"
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
                Output::Timeline { series, .. } => {
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

/// The mark that goes beside a value a lossy run has made a lower bound, and
/// nothing at all when the run lost nothing.
///
/// A clean run must gain nothing: not a character. A mark that appears on
/// every run is a mark readers learn to skip, and then the run that needed it
/// is the one nobody reads it on.
fn bound(short: Option<&Shortfall>) -> &'static str {
    match short {
        Some(short) if short.bounds_below() => Shortfall::MARK,
        _ => "",
    }
}

/// The same mark where a `=` would otherwise be, as in `n=102` against
/// `n>=102`. A count is written with its comparison rather than beside it.
fn comparison(short: Option<&Shortfall>) -> &'static str {
    match short {
        Some(short) if short.bounds_below() => Shortfall::MARK,
        _ => "=",
    }
}

/// The sentence under a marked distribution: what is missing, why no fraction
/// where none is known, and which way the bias runs.
///
/// Once per emit rather than once per series. The shortfall is the run's and
/// every series carries the same one, so printing it beside each of forty
/// would be saying one fact forty times.
fn push_shortfall(out: &mut String, short: Option<&Shortfall>) {
    match short {
        Some(short) if short.bounds_below() => {
            let _ = writeln!(out, "    {} {}", Shortfall::MARK, short.note);
        }
        Some(short) => {
            let _ = writeln!(out, "    evicted: {}", short.note);
        }
        None => {}
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
    // Every bucket count below is a lower bound too, and the highest ones are
    // the short ones. Marking each of sixty-five rows would say it sixty-five
    // times; the head line carries the mark and [`push_shortfall`] says which
    // end of the distribution the missing values were at.
    let short = histogram.short.as_ref();
    let _ = writeln!(
        out,
        "    {}  n{}{}  min {}  mean {}{}  max {}{}",
        histogram.key,
        comparison(short),
        histogram.count,
        quantity(histogram.min, unit),
        bound(short),
        quantity(mean, unit),
        bound(short),
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
    if histogram.unmeasured > 0 {
        let _ = writeln!(
            out,
            "      {} events carried no value and are in no bucket",
            histogram.unmeasured
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
/// A row's frames, root first, under its series when the request grouped
/// stacks by keys: the series becomes the root frame, so a flame of two
/// tasks is two towers rather than one that adds them up.
fn frames(results: &Results, stack: &StackCount) -> Vec<String> {
    let frames = results
        .stacks
        .get(&stack.stack)
        .cloned()
        .unwrap_or_else(|| vec![format!("[unresolved stack {:#x}]", stack.stack)]);
    match &stack.series {
        Some(series) => std::iter::once(series.to_string()).chain(frames).collect(),
        None => frames,
    }
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
        Unit::Watts => scaled(value, &[(1e3, "kW"), (1.0, "W"), (1e-3, "mW")], "uW"),
        other => format!("{value:.0} {other}"),
    }
}

/// A rate in the unit a reader names it by: energy per second is power, so a
/// rate over joules reads `21.09 W` and not `21.09 J/s`; every other unit keeps
/// its `/s`.
fn per_second_quantity(value: f64, unit: Unit) -> String {
    match unit {
        Unit::Joules => quantity(value, Unit::Watts),
        other => format!("{}/s", quantity(value, other)),
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

/// A gzipped `profile.proto`, the format `pprof`, Parca and Polar Signals read.
///
/// The whole format is a string table and indices into it, which is the same
/// interning [`Results::stacks`] already does; what this adds is the wire
/// encoding and a header that says what the numbers are. Nothing is aggregated
/// here that was not aggregated already.
///
/// **Several requests become one profile with several sample types**, which is
/// what pprof is for: a heap profile carries `alloc_space` and `alloc_objects`
/// side by side, and a run that asked for `cpu.profile` and `sched.offcpu`
/// carries both under their own names, each sample scoring in its own column
/// and zero in the others. `folded` cannot do that and has to prefix a root
/// frame instead.
///
/// **What is not written down is as deliberate as what is.** `time_nanos` is
/// the wall-clock start of a profile and this tool has no wall clock --
/// `CLOCK_MONOTONIC` is the only domain it admits -- so the field is left at
/// zero, which pprof reads as unknown, and a comment says so. `period` is the
/// sampling interval, which the results carry since the thirteenth wave
/// (`Results::period_ns`); it is written when every profile in the file was
/// sampled at one rate, and left out with a comment when they were not.
fn pprof(output: &RunOutput) -> Result<Vec<u8>, ParseError> {
    let profiles: Vec<(&Results, &[StackCount], usize)> = output
        .results
        .iter()
        .flat_map(|results| {
            results.outputs.iter().filter_map(move |out| match out {
                Output::Stack {
                    stacks,
                    unattributed,
                    ..
                } => Some((results, stacks.as_slice(), *unattributed)),
                _ => None,
            })
        })
        .collect();

    if profiles.iter().all(|(_, stacks, _)| stacks.is_empty()) {
        return Err(ParseError::new(format!(
            "`pprof` describes a profile and this run has none to write: {}",
            why_no_profile(output)
        )));
    }

    let mut strings = StringTable::default();
    let mut table = Symbols::default();

    // One sample type per request, named as the request was. Two requests of
    // the same source would otherwise produce two columns a reader cannot tell
    // apart, and `pprof -sample_index=` selects a column by name.
    let mut type_names: Vec<String> = Vec::new();
    for (results, _, _) in &profiles {
        let mut name = results.label.clone();
        let mut nth = 2;
        while type_names.contains(&name) {
            name = format!("{} #{nth}", results.label);
            nth += 1;
        }
        type_names.push(name);
    }

    let mut sample_types = Vec::new();
    for (index, (results, _, _)) in profiles.iter().enumerate() {
        let (unit, _) = pprof_unit(results.unit);
        let mut value_type = Vec::new();
        wire::int64(
            &mut value_type,
            1,
            strings.intern(&type_names[index]) as i64,
        );
        wire::int64(&mut value_type, 2, strings.intern(unit) as i64);
        sample_types.push(value_type);
    }

    let events_key = strings.intern("events") as i64;
    let count_unit = strings.intern("count") as i64;

    let mut samples = Vec::new();
    for (column, (results, stacks, _)) in profiles.iter().enumerate() {
        for stack in *stacks {
            // pprof orders a sample's locations leaf first; this tool carries
            // them root first, the way `folded` writes them.
            let ids: Vec<u64> = frames(results, stack)
                .iter()
                .rev()
                .map(|frame| table.location_of(frame, &mut strings))
                .collect();
            let mut values = vec![0i64; profiles.len()];
            values[column] = stack.total.round() as i64;

            // How many events went into a stack is a different number from what
            // they came to, and only one of the two fits in the value column.
            // A numeric label keeps the other rather than dropping it: `pprof
            // -tagshow events` reads it back.
            let mut label = Vec::new();
            wire::int64(&mut label, 1, events_key);
            wire::int64(&mut label, 3, stack.events as i64);
            wire::int64(&mut label, 6, count_unit);

            let mut sample = Vec::new();
            wire::packed_uint64(&mut sample, 1, &ids);
            wire::packed_int64(&mut sample, 2, &values);
            wire::message(&mut sample, 3, &label);
            samples.push(sample);
        }
    }

    // The sampling period, when every profile here was sampled at one: a
    // continuous-profiling pipeline compares two profiles taken at different
    // rates by it. Two rates in one file have no one period to give.
    let periods: std::collections::BTreeSet<Option<u64>> = profiles
        .iter()
        .map(|(results, _, _)| results.period_ns)
        .collect();
    let period_ns = match periods.iter().collect::<Vec<_>>().as_slice() {
        [Some(period)] => Some(*period),
        _ => None,
    };
    let mut comments = vec![
        format!("exporter: omn {}", env!("CARGO_PKG_VERSION")),
        "clock: CLOCK_MONOTONIC; time_nanos is left at zero because this tool has no wall clock"
            .to_owned(),
        match period_ns {
            Some(period) => format!(
                "period: {period} ns of cpu between samples, from the request's sampling rate"
            ),
            None => "period is unset: these profiles were not all sampled at one rate".to_owned(),
        },
    ];
    for (index, (results, stacks, unattributed)) in profiles.iter().enumerate() {
        comments.push(format!(
            "sample type {} `{}`: source {}, via {}, {} stacks over {}",
            index + 1,
            type_names[index],
            results.source,
            results.backend,
            stacks.len(),
            approximate(results.observed_ns),
        ));
        let (unit, known) = pprof_unit(results.unit);
        if !known {
            comments.push(format!(
                "  `{unit}` is not a unit pprof knows: the values are as measured, and no tool \
                 will scale or label them",
            ));
        }
        if *unattributed > 0 {
            comments.push(format!(
                "  {unattributed} events carried no stack and are not in this profile",
            ));
        }
        // Losses are counted and surfaced to the user, always -- including into
        // a format that has nowhere structural to put them.
        if !results.losses.is_clean() {
            comments.push(format!("  {}", loss_headline(&results.losses)));
        }
        for (reason, count) in &results.losses.reasons {
            comments.push(format!("    {count}x {reason}"));
        }
        for degradation in &results.degradations {
            comments.push(format!("  degraded: {degradation}"));
        }
        for note in &results.notes {
            comments.push(format!("  note: {note}"));
        }
        for unverified in &results.unverified {
            comments.push(format!("  unverified: {unverified}"));
        }
        if let Some(filtered) = &results.filtered {
            comments.push(format!(
                "  filtered: kept {} of {} events matching {}",
                filtered.kept,
                filtered.kept + filtered.dropped,
                filtered.predicate
            ));
        }
    }
    let comments: Vec<i64> = comments
        .iter()
        .map(|line| strings.intern(line) as i64)
        .collect();
    // `period_type` names what the period counts: cpu time, in nanoseconds,
    // which is what a `perf_event_open` sampling rate is a period of.
    let period = period_ns.map(|period| {
        let mut period_type = Vec::new();
        wire::int64(&mut period_type, 1, strings.intern("cpu") as i64);
        wire::int64(&mut period_type, 2, strings.intern("nanoseconds") as i64);
        (period_type, period as i64)
    });
    let default_type = strings.intern(&type_names[0]) as i64;
    let duration = profiles
        .iter()
        .map(|(results, _, _)| results.observed_ns)
        .max()
        .unwrap_or(0);

    let mut profile = Vec::new();
    for value_type in &sample_types {
        wire::message(&mut profile, 1, value_type);
    }
    for sample in &samples {
        wire::message(&mut profile, 2, sample);
    }
    for location in &table.locations {
        wire::message(&mut profile, 4, location);
    }
    for function in &table.functions {
        wire::message(&mut profile, 5, function);
    }
    // Written last, because interning a string is what the loops above were
    // doing: the table is only complete once nothing else will add to it.
    for entry in strings.entries() {
        wire::string(&mut profile, 6, entry);
    }
    wire::int64(&mut profile, 10, duration as i64);
    if let Some((period_type, period)) = &period {
        wire::message(&mut profile, 11, period_type);
        wire::int64(&mut profile, 12, *period);
    }
    wire::packed_int64(&mut profile, 13, &comments);
    wire::int64(&mut profile, 14, default_type);

    Ok(wire::gzip(&profile))
}

/// The unit string pprof will read, and whether it is one pprof knows.
///
/// pprof scales and labels an axis from a fixed set of unit names, and this
/// tool's units do not map onto them one to one. Three do. The rest are written
/// under their own name rather than under the nearest one pprof would
/// recognize: a ratio reported as `count` renders as a count, and a temperature
/// reported as anything renders as a lie. The caller writes a comment saying so.
fn pprof_unit(unit: Unit) -> (&'static str, bool) {
    match unit {
        Unit::Ns => ("nanoseconds", true),
        Unit::Bytes => ("bytes", true),
        Unit::Count => ("count", true),
        Unit::Ratio => ("ratio", false),
        Unit::Hz => ("hertz", false),
        Unit::Joules => ("joules", false),
        Unit::Watts => ("watts", false),
        Unit::Celsius => ("celsius", false),
        Unit::Cycles => ("cycles", false),
    }
}

/// pprof's location and function tables, which this tool fills one frame name
/// at a time.
///
/// One location per function and no addresses: the frames reaching this module
/// are already symbolized strings, produced by `stack` against the target's own
/// maps. Writing an address of zero on every frame would claim the profile
/// happened at one instruction, and writing a mapping would claim a build id
/// nothing here read.
#[derive(Debug, Default)]
struct Symbols {
    /// pprof numbers locations and functions from one; zero means absent.
    id_of: std::collections::HashMap<String, u64>,
    functions: Vec<Vec<u8>>,
    locations: Vec<Vec<u8>>,
}

impl Symbols {
    fn location_of(&mut self, name: &str, strings: &mut StringTable) -> u64 {
        if let Some(id) = self.id_of.get(name) {
            return *id;
        }
        let id = self.functions.len() as u64 + 1;

        // `system_name` is the mangled name and `name` the readable one. This
        // tool symbolizes once and has only the one, so writing it into both
        // would claim a demangling that did not happen.
        let mut function = Vec::new();
        wire::uint64(&mut function, 1, id);
        wire::int64(&mut function, 2, strings.intern(name) as i64);
        self.functions.push(function);

        let mut line = Vec::new();
        wire::uint64(&mut line, 1, id);
        let mut location = Vec::new();
        wire::uint64(&mut location, 1, id);
        wire::message(&mut location, 4, &line);
        self.locations.push(location);

        self.id_of.insert(name.to_owned(), id);
        id
    }
}

/// pprof's string table: every string in the file is an index into it, and
/// index zero is the empty string by definition of the format.
#[derive(Debug)]
struct StringTable {
    entries: Vec<String>,
    index_of: std::collections::HashMap<String, u64>,
}

impl Default for StringTable {
    fn default() -> Self {
        Self {
            entries: vec![String::new()],
            index_of: std::collections::HashMap::from([(String::new(), 0)]),
        }
    }
}

impl StringTable {
    fn intern(&mut self, text: &str) -> u64 {
        if let Some(index) = self.index_of.get(text) {
            return *index;
        }
        let index = self.entries.len() as u64;
        self.entries.push(text.to_owned());
        self.index_of.insert(text.to_owned(), index);
        index
    }

    fn entries(&self) -> &[String] {
        &self.entries
    }
}

/// Protocol-buffer and gzip encoding, which is all `pprof` needs from outside
/// this crate and all this module implements of either.
///
/// Writing it by hand rather than taking a dependency: the subset a profile
/// uses is varints, length-delimited fields and packed repeated integers, which
/// is under a hundred lines, and a code generator for one `.proto` is a build
/// step the rest of this project does not have.
mod wire {
    /// Base 128 varint, low group first.
    fn varint(out: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            out.push((value as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    fn tag(out: &mut Vec<u8>, field: u32, wire_type: u8) {
        varint(out, (u64::from(field) << 3) | u64::from(wire_type));
    }

    /// A varint field. Omitted when zero, which proto3 reads back as zero.
    pub fn uint64(out: &mut Vec<u8>, field: u32, value: u64) {
        if value != 0 {
            tag(out, field, 0);
            varint(out, value);
        }
    }

    /// A signed varint field, encoded the way protobuf encodes `int64`: two's
    /// complement, not zigzag. Every `int64` in `profile.proto` is an index or
    /// a count and none is negative, but a negative value has to round-trip
    /// rather than turn into a huge positive one.
    pub fn int64(out: &mut Vec<u8>, field: u32, value: i64) {
        if value != 0 {
            tag(out, field, 0);
            varint(out, value as u64);
        }
    }

    /// A repeated string entry, always written: index two of the string table
    /// is whatever was written third, so an empty entry that is skipped shifts
    /// every index after it.
    pub fn string(out: &mut Vec<u8>, field: u32, value: &str) {
        tag(out, field, 2);
        varint(out, value.len() as u64);
        out.extend_from_slice(value.as_bytes());
    }

    /// A nested message, always written even when its body is empty: a message
    /// whose every field is at its default still has to exist for the reader to
    /// count it.
    pub fn message(out: &mut Vec<u8>, field: u32, body: &[u8]) {
        tag(out, field, 2);
        varint(out, body.len() as u64);
        out.extend_from_slice(body);
    }

    pub fn packed_uint64(out: &mut Vec<u8>, field: u32, values: &[u64]) {
        if values.is_empty() {
            return;
        }
        let mut body = Vec::new();
        for value in values {
            varint(&mut body, *value);
        }
        message(out, field, &body);
    }

    pub fn packed_int64(out: &mut Vec<u8>, field: u32, values: &[i64]) {
        if values.is_empty() {
            return;
        }
        let mut body = Vec::new();
        for value in values {
            varint(&mut body, *value as u64);
        }
        message(out, field, &body);
    }

    /// A gzip stream, the container `profile.proto` is distributed in.
    ///
    /// The deflate payload is written as *stored* blocks -- the BTYPE 00 that
    /// the format defines as uncompressed -- so this is a conformant gzip file
    /// that every decoder reads, produced without a compressor. What it is not
    /// is smaller: a profile costs about five bytes per 64 KiB more than its
    /// protobuf. Compressing it would mean a deflate implementation or a
    /// dependency, and neither buys anything a reader of this format notices.
    pub fn gzip(data: &[u8]) -> Vec<u8> {
        // Magic, CM=8 (deflate), FLG=0, MTIME=0, XFL=0, OS=255.
        //
        // MTIME zero is the format's own "no time available", which is the same
        // answer `time_nanos` gives and for the same reason: this tool reads
        // one clock and it is not the wall.
        let mut out = vec![0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff];
        let mut chunks = data.chunks(0xffff).peekable();
        if chunks.peek().is_none() {
            out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
        }
        while let Some(chunk) = chunks.next() {
            let last = u8::from(chunks.peek().is_none());
            let len = chunk.len() as u16;
            out.push(last);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
        out.extend_from_slice(&crc32(data).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out
    }

    /// CRC-32 as gzip specifies it: the reflected IEEE polynomial, which is the
    /// same one `gzip -t` will check the trailer against.
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for byte in data {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let carry = crc & 1;
                crc >>= 1;
                if carry != 0 {
                    crc ^= 0xedb8_8320;
                }
            }
        }
        !crc
    }
}

/// Chrome Trace Event Format, which Perfetto UI and `chrome://tracing` read.
///
/// The first format here whose natural input is `timeline` rather than `stack`:
/// it carries *when* things had which value, and has no way to say where in a
/// program they happened. `--emit stack` is refused for it by the matrix in
/// `format`, which is the point of that matrix.
///
/// Two shapes, one per emit it accepts. A timeline series becomes a **counter
/// track** -- `ph: "C"`, one track per series, which is what Perfetto draws as
/// a line -- and a raw event becomes an **instant event** on a thread of its
/// own series. Each request is a process, so two requests in one run are two
/// rows rather than one interleaved strip.
///
/// **The axis.** Chrome's `ts` is microseconds from an arbitrary origin, and
/// this tool's origin is the first event of a collection. A run collects its
/// requests over one window against one target, so their origins are the same
/// to within the first tick; `otherData` says exactly that rather than leaving
/// a reader to assume it. Raw events, which carry an absolute
/// `CLOCK_MONOTONIC` timestamp, are placed against the earliest of them in the
/// run so that they share the axis instead of sitting at the machine's uptime.
fn chrome(output: &RunOutput) -> Result<String, ParseError> {
    let origin_ns = output
        .results
        .iter()
        .flat_map(|results| &results.outputs)
        .filter_map(|out| match out {
            Output::Raw { events } => events.iter().map(|event| event.ts_ns).min(),
            _ => None,
        })
        .min();

    let mut events: Vec<serde_json::Value> = Vec::new();
    let mut described: Vec<serde_json::Value> = Vec::new();
    let mut wrote_something = false;

    for (index, results) in output.results.iter().enumerate() {
        let carries = results
            .outputs
            .iter()
            .any(|out| matches!(out, Output::Timeline { .. } | Output::Raw { .. }));
        if !carries {
            continue;
        }
        let pid = index + 1;

        events.push(serde_json::json!({
            "ph": "M", "name": "process_name", "pid": pid, "tid": 0,
            "args": {"name": results.label},
        }));
        events.push(serde_json::json!({
            "ph": "M", "name": "process_labels", "pid": pid, "tid": 0,
            "args": {"labels": process_labels(results)},
        }));
        described.push(request_record(results));

        let mut tid_of: Vec<String> = Vec::new();

        for out in &results.outputs {
            match out {
                Output::Timeline { series, .. } => {
                    for series in series {
                        for point in &series.points {
                            events.push(serde_json::json!({
                                "ph": "C",
                                "name": series.key,
                                "pid": pid,
                                "ts": point.offset_ns as f64 / 1000.0,
                                "args": {results.label.as_str(): point.value},
                            }));
                            wrote_something = true;
                        }
                    }
                }
                Output::Raw { events: raw } => {
                    for event in raw {
                        let key = SeriesKey::of(event);
                        let thread = thread_of(key.as_str(), &mut tid_of);
                        let at = origin_ns
                            .map(|origin| event.ts_ns.saturating_sub(origin))
                            .unwrap_or(event.ts_ns);
                        events.push(serde_json::json!({
                            "ph": "i",
                            "s": "t",
                            "name": event.kind,
                            "pid": pid,
                            "tid": thread,
                            "ts": at as f64 / 1000.0,
                            "args": instant_args(event),
                        }));
                        wrote_something = true;
                    }
                }
                _ => {}
            }
        }

        for (index, name) in tid_of.iter().enumerate() {
            events.push(serde_json::json!({
                "ph": "M", "name": "thread_name", "pid": pid, "tid": index + 1,
                "args": {"name": name},
            }));
        }
    }

    if !wrote_something {
        return Err(ParseError::new(format!(
            "`chrome` carries a timeline and this run has none to write: {}",
            why_no_timeline(output)
        )));
    }

    // Chrome sorts by `ts` itself, but a trace whose events arrive in order
    // reads the same in a text diff as it does in the UI, and the metadata
    // events carry no time at all.
    let document = serde_json::json!({
        "displayTimeUnit": "ns",
        "traceEvents": events,
        "otherData": {
            "exporter": concat!("omn ", env!("CARGO_PKG_VERSION")),
            "clock": "CLOCK_MONOTONIC",
            "time_origin": "microseconds from the first event of the run; a run collects its \
                            requests over one window, so their tracks share this axis to within \
                            the first tick",
            "requests": described,
            "joins": output
                .joins
                .iter()
                .map(|join| format!(
                    "{}: {} -> {}, {} of {} attributed (this format carries no correlations; \
                     ask for json to read them)",
                    join.rule,
                    join.from,
                    join.to,
                    join.matched,
                    join.matched + join.unmatched
                ))
                .collect::<Vec<_>>(),
            "decompositions": output
                .decompositions
                .iter()
                .map(|decomposition| format!(
                    "{}: {} keys (this format carries no decompositions; ask for json to read \
                     them)",
                    decomposition.id, decomposition.keys_seen
                ))
                .collect::<Vec<_>>(),
        },
    });
    serde_json::to_string_pretty(&document)
        .map_err(|error| ParseError::new(format!("cannot encode the Chrome trace: {error}")))
}

/// The Chrome thread number a series key takes in one process, allocated in
/// the order the keys are seen and named by a metadata event afterwards.
///
/// `tid` is a number in this format and a series is keyed by a device, an
/// interface or a socket as often as by a thread, so the number is an index
/// into this trace and the key is what it means.
fn thread_of(key: &str, seen: &mut Vec<String>) -> usize {
    if let Some(index) = seen.iter().position(|known| known == key) {
        return index + 1;
    }
    seen.push(key.to_owned());
    seen.len()
}

/// The one-line provenance Chrome shows beside a process name.
fn process_labels(results: &Results) -> String {
    let mut label = format!(
        "{} via {} [{}], {} events over {}",
        results.source,
        results.backend,
        results.unit,
        results.events_seen,
        approximate(results.observed_ns)
    );
    if !results.losses.is_clean() {
        let _ = write!(label, " -- {}", loss_headline(&results.losses));
    }
    label
}

/// Everything about one request that the trace events themselves cannot say.
///
/// Losses are counted and surfaced to the user, always, and a trace viewer has
/// nowhere to draw them: they go here, where a reader who opens the file as
/// JSON finds them and a reader who opens it in Perfetto still has the summary
/// on the process label.
fn request_record(results: &Results) -> serde_json::Value {
    serde_json::json!({
        "label": results.label,
        "source": results.source,
        "backend": results.backend,
        "unit": results.unit.as_str(),
        "events_seen": results.events_seen,
        "observed_ns": results.observed_ns,
        "window": results.window,
        "stopped_because": results.stopped_because,
        "losses": results.losses,
        "degradations": results.degradations,
        "unverified": results.unverified,
        "filtered": results.filtered,
    })
}

/// What an instant event carries: what was measured, and what it can be joined
/// on. Both, because a raw trace is the emit a reader asks for when the
/// aggregate lost the thing they were looking for.
fn instant_args(event: &Event) -> serde_json::Value {
    let mut args = serde_json::Map::new();
    for attr in &event.attrs {
        args.insert(
            attr.name.clone(),
            serde_json::to_value(&attr.value).unwrap_or(serde_json::Value::Null),
        );
    }
    for key in &event.keys {
        args.insert(format!("key.{}", key.kind()), key.to_string().into());
    }
    args.insert("seq".to_owned(), event.seq.into());
    if event.ts_err_ns != 0 {
        // A timestamp converted from another domain, which is the only way this
        // is non-zero. It belongs next to the event rather than in a footnote:
        // the axis it was placed on is not the one it was taken on.
        args.insert("ts_err_ns".to_owned(), event.ts_err_ns.into());
    }
    serde_json::Value::Object(args)
}

/// Why a run carries nothing a trace viewer can draw.
///
/// The same distinction `why_no_profile` draws, for the other half of the
/// matrix: a request that asked for neither a timeline nor raw events, and one
/// that asked and whose window held none.
fn why_no_timeline(output: &RunOutput) -> String {
    let mut asked = false;
    for results in &output.results {
        for out in &results.outputs {
            match out {
                Output::Timeline { series, .. } => {
                    asked = true;
                    if series.iter().any(|series| !series.points.is_empty()) {
                        return String::new();
                    }
                }
                Output::Raw { events } => {
                    asked = true;
                    if !events.is_empty() {
                        return String::new();
                    }
                }
                _ => {}
            }
        }
    }
    if !asked {
        return "it emitted no timeline; use --emit timeline, or json for what was measured"
            .to_owned();
    }
    "a timeline was asked for and no event arrived in the window".to_owned()
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
                        unmeasured: 0,
                        sums: BTreeMap::new(),
                        count: 2,
                        min: 1_048_576.0,
                        max: 2_097_152.0,
                        mean: 1_572_864.0,
                        last: 2_097_152.0,
                        total: None,
                        short: None,
                    }],
                },
                Output::Timeline {
                    first_ts_ns: None,
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
                        unmeasured: 0,
                    }],
                },
            ],
            events_seen: 2,
            observed_ns: 1_000_000_000,
            window: Default::default(),
            stopped_because: "the requested duration elapsed".into(),
            losses: Losses::default(),
            filtered: None,
            lifetime: None,
            unverified: Vec::new(),
            degradations: vec!["fs:proc:statm: resident pages only".into()],
            stacks: BTreeMap::new(),
            notes: Vec::new(),
            overhead: Default::default(),
            served_by: Vec::new(),
            period_ns: None,
            by_ring: Vec::new(),
            value_attr: String::new(),
            row_stamp: None,
            drains: Vec::new(),
            fields: Default::default(),
            provenance: Default::default(),
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
    fn a_power_reads_in_watts_and_is_not_one_pprof_scales() {
        assert_eq!(quantity(215.0, Unit::Watts), "215.00 W");
        assert_eq!(quantity(1_500.0, Unit::Watts), "1.50 kW");
        assert_eq!(quantity(0.25, Unit::Watts), "250.00 mW");
        // A level, not an amount: pprof would sum it over a flame graph.
        assert_eq!(pprof_unit(Unit::Watts), ("watts", false));
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

    /// The heap collector's "statically linked, never calls malloc" was noted
    /// from the first wave and never once printed, because the reasons were
    /// only written under a non-zero counter.
    #[test]
    fn a_reason_noted_beside_clean_counters_is_printed() {
        let mut results = sample();
        results
            .losses
            .note("no allocation was reported: statically linked");
        assert!(results.losses.is_clean());
        let text = super::results(OutputFormat::Text, &results).unwrap();
        assert!(text.contains("no loss"), "{text}");
        assert!(text.contains("statically linked"), "{text}");
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
                keyless: 0,
                outside_window: 0,
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
                    change: None,
                    wakeups: None,
                }],
                total_attributed: 12,
                within_key: None,
                readings: None,
            }],
            decompositions: Vec::new(),
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
            short: None,
            stacks: vec![
                StackCount {
                    stack: 1,
                    series: None,
                    events: 300,
                    total: 3_000_000.0,
                },
                StackCount {
                    stack: 2,
                    series: None,
                    events: 7,
                    total: 70_000.0,
                },
                // Seen, counted, and with no frames to show for it.
                StackCount {
                    stack: 3,
                    series: None,
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
                unmeasured: 0,
                count: 102,
                min: 1024.0,
                max: 1_048_576.0,
                sum: 2_199_552.0,
                short: None,
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
                    short: None,
                    key: "task:2".into(),
                    unmeasured: 0,
                    events: 1,
                    total: 300.0,
                },
                SeriesTotal {
                    short: None,
                    key: "task:3".into(),
                    unmeasured: 0,
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
            decompositions: Vec::new(),
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

    /// A row keyed by a series (`--keys task`) roots its chain at the series,
    /// so two tasks on one stack are two towers in a flame graph.
    #[test]
    fn a_keyed_stack_row_is_rooted_at_its_series() {
        let mut keyed = profile();
        for output in &mut keyed.outputs {
            if let Output::Stack { stacks, .. } = output {
                stacks.truncate(1);
                stacks[0].series = Some("task:7".into());
            }
        }
        let run = RunOutput {
            results: vec![keyed],
            joins: Vec::new(),
            decompositions: Vec::new(),
        };
        let encoded = run_output(OutputFormat::Folded, &run).expect("encoding");
        assert_eq!(encoded.trim(), "task:7;main;work;read 3000000", "{encoded}");
        let lines = run_output(OutputFormat::Ndjson, &run).expect("encoding");
        assert!(lines.contains("\"series\":\"task:7\""), "{lines}");
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
            decompositions: Vec::new(),
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
            decompositions: Vec::new(),
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
            decompositions: Vec::new(),
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
            decompositions: Vec::new(),
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
            short: None,
            stacks: Vec::new(),
            unattributed: 5057,
        }];
        empty.stacks = BTreeMap::new();
        let run = RunOutput {
            results: vec![empty],
            joins: Vec::new(),
            decompositions: Vec::new(),
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
    /// The bytes entry point is the one asked, because it is the only one that
    /// can carry all nine: `pprof` is gzip and a `String` cannot hold it.
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
            let encoded = run_output_bytes(format, &run);
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

    /// The gzipped protobuf is the one format the text path cannot carry, and
    /// what it answers has to be the way out rather than a bare refusal.
    #[test]
    fn the_binary_format_says_how_to_get_it_rather_than_that_it_cannot() {
        let run = RunOutput {
            results: vec![profile()],
            joins: Vec::new(),
            decompositions: Vec::new(),
        };
        let error = run_output(OutputFormat::Pprof, &run).unwrap_err();
        assert!(error.message().contains("not text"), "{error}");
        assert!(error.message().contains("--out"), "{error}");
        assert!(run_output_bytes(OutputFormat::Pprof, &run).is_ok());
    }

    /// And a run with nothing to profile answers about the profile, not about
    /// the encoding: a reader who asked for `pprof` and has no stacks needs to
    /// change the request, and being told the file is binary would not say so.
    #[test]
    fn a_binary_format_with_nothing_to_write_still_answers_about_the_run() {
        let run = RunOutput {
            results: vec![ranked()],
            joins: Vec::new(),
            decompositions: Vec::new(),
        };
        let error = run_output(OutputFormat::Pprof, &run).unwrap_err();
        assert!(error.message().contains("emitted no stacks"), "{error}");
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

    /// Both profile formats draw the same distinction for the same reason, and
    /// `pprof` has to draw it too: a request that never asked for stacks and a
    /// collector that captured none are different answers.
    #[test]
    fn a_pprof_of_events_that_carried_no_stack_says_which_of_the_two_happened() {
        let mut empty = profile();
        empty.outputs = vec![Output::Stack {
            short: None,
            stacks: Vec::new(),
            unattributed: 91,
        }];
        empty.stacks = BTreeMap::new();
        let error = run_output_bytes(
            OutputFormat::Pprof,
            &RunOutput {
                results: vec![empty],
                joins: Vec::new(),
                decompositions: Vec::new(),
            },
        )
        .unwrap_err();
        assert!(
            error.message().contains("no event carried one, 91 of them"),
            "{error}"
        );
    }

    /// The trace format's half of the same distinction. A run that asked for a
    /// histogram has nothing a viewer can draw and never did; a run that polled
    /// and saw nothing is a measurement.
    #[test]
    fn a_chrome_trace_of_a_run_with_nothing_to_draw_says_which_of_the_two_happened() {
        let never = run_output(
            OutputFormat::ChromeTrace,
            &RunOutput {
                results: vec![histogram()],
                joins: Vec::new(),
                decompositions: Vec::new(),
            },
        )
        .unwrap_err();
        assert!(never.message().contains("emitted no timeline"), "{never}");

        let mut empty = sample();
        empty.outputs = vec![Output::Timeline {
            series: Vec::new(),
            first_ts_ns: None,
        }];
        let saw_nothing = run_output(
            OutputFormat::ChromeTrace,
            &RunOutput {
                results: vec![empty],
                joins: Vec::new(),
                decompositions: Vec::new(),
            },
        )
        .unwrap_err();
        assert!(
            saw_nothing.message().contains("no event arrived"),
            "{saw_nothing}"
        );
    }

    /// A trace viewer has nowhere to draw a dropped event, which is not a
    /// reason for the file not to carry it: losses are counted and surfaced to
    /// the user, always.
    #[test]
    fn a_chrome_trace_carries_the_losses_and_the_degradations_it_cannot_draw() {
        let mut lossy = sample();
        lossy.losses.dropped = 12;
        lossy.losses.late_ticks = 3;
        let encoded = run_output(
            OutputFormat::ChromeTrace,
            &RunOutput {
                results: vec![lossy],
                joins: Vec::new(),
                decompositions: Vec::new(),
            },
        )
        .expect("encoding");
        let document: serde_json::Value = serde_json::from_str(&encoded).expect("chrome JSON");

        let request = &document["otherData"]["requests"][0];
        assert_eq!(request["losses"]["dropped"], 12);
        assert_eq!(
            request["degradations"][0],
            "fs:proc:statm: resident pages only"
        );

        let labels = document["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["name"] == "process_labels")
            .expect("a process label");
        let labels = labels["args"]["labels"].as_str().unwrap();
        assert!(
            labels.contains("LOSS: 12 dropped"),
            "the one line a viewer does show has to carry it: {labels}"
        );
    }

    /// A run of several requests becomes one profile with one column each,
    /// which is what `pprof` is for and what `folded` has to fake with a root
    /// frame. Two requests of the same source would collide, so the second is
    /// numbered.
    #[test]
    fn two_requests_become_two_columns_that_can_be_told_apart() {
        let mut second = profile();
        second.label = "offcpu".into();
        let mut third = profile();
        third.label = "profile".into();
        let run = RunOutput {
            results: vec![profile(), second, third],
            joins: Vec::new(),
            decompositions: Vec::new(),
        };
        let bytes = run_output_bytes(OutputFormat::Pprof, &run).expect("encoding");
        // The names are in the string table, which is not compressed away: the
        // deflate blocks this writes are stored.
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(text.contains("offcpu"), "the second column is missing");
        assert!(
            text.contains("profile #2"),
            "two requests of one source are indistinguishable columns"
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
            keyless: 0,
            outside_window: 0,
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
                change: None,
                wakeups: None,
            }],
            total_attributed: 3,
            within_key: None,
            readings: None,
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
        // The fault-counting caveat belongs to this kind and only this one,
        // and it says what is true on every kernel rather than a mechanism:
        // `docs/scenarios/cross-memory.md` measured the counts predicted by
        // 2 MiB alignment on a machine with every multi-size size off (D4).
        assert!(text.contains("two orders of magnitude"), "{text}");
        assert!(!text.contains("picks larger folios"), "{text}");
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

    /// A correlation that credits within a key by recency says so on its
    /// first line, beside the evidence it has for the key itself.
    #[test]
    fn crediting_within_a_key_by_the_clock_is_said_beside_the_key() {
        let mut correlation = correlated(MatchKind::Eq, "task:7", 0);
        correlation.within_key = Some(Evidence::Temporal);
        correlation.states.replaced = 41;
        let text = correlation_text(&correlation);
        let first = text.lines().next().unwrap_or_default();
        assert!(
            first.contains("evidence KEY, TEMPORAL within the key"),
            "{text}"
        );
        assert!(text.contains("41 states were replaced"), "{text}");
    }

    /// A folded chain names how many wakeups it stands for, and a chain whose
    /// latest wakeup nothing ended is still open -- not "never ran", which is
    /// false of a chain with events attributed to it.
    #[test]
    fn a_wakeup_chain_counts_its_wakeups_and_is_still_open_rather_than_never_run() {
        let mut correlation = correlated(MatchKind::Wakeup, "task:7 -> task:9", 0);
        correlation.top[0].wakeups = Some(602);
        correlation.top[0].matched = 604;
        correlation.top[0].outcome = Outcome::StillOpen;
        let text = correlation_text(&correlation);
        assert!(text.contains("WAKEUPS"), "{text}");
        assert!(text.contains("602"), "{text}");
        assert!(text.contains("still open at the end"), "{text}");
        assert!(!text.contains("never ran"), "{text}");
    }

    /// A join across a detached source says what fell outside the source's
    /// window apart from the share, and which states the detach ended,
    /// without calling either an eviction.
    #[test]
    fn a_join_across_a_detach_says_what_fell_outside_and_what_it_ended() {
        let mut correlation = correlated(MatchKind::Wakeup, "task:7 -> task:9", 0);
        correlation.outside_window = 25;
        correlation.states.window_closed = 1;
        correlation.top[0].outcome = Outcome::WindowClosed;
        let text = correlation_text(&correlation);
        assert!(
            text.contains("25 more `b` events fell outside `a`'s window"),
            "{text}"
        );
        assert!(
            text.contains("1 more ended when `a` was detached"),
            "{text}"
        );
        assert!(text.contains("source detached"), "{text}");
        assert!(!text.contains("EVICTED"), "{text}");

        let plain = correlation_text(&correlated(MatchKind::Wakeup, "task:7 -> task:9", 0));
        assert!(
            !plain.contains("window"),
            "nothing said when nothing was detached: {plain}"
        );
    }

    /// A `between` correlation counts readings, says how much moved with
    /// nothing around it, and ends on what its evidence does not say.
    #[test]
    fn a_between_join_reads_as_intervals_and_says_what_temporal_does_not_claim() {
        let mut correlation = correlated(MatchKind::Between, "task:7", 0);
        correlation.evidence = Evidence::Temporal;
        correlation.top[0].change = Some(crate::join::Change {
            delta: -32.0 * 1024.0 * 1024.0,
            opened: 0,
            closed: 4,
        });
        correlation.readings = Some(crate::join::Readings {
            value: "bytes".into(),
            reading: crate::join::Reading::Level,
            baselines: 1,
            credited: -32.0 * 1024.0 * 1024.0,
            uncredited: 4096.0,
            before_first: 0,
        });
        let text = correlation_text(&correlation);
        assert!(text.contains("evidence TEMPORAL"), "{text}");
        assert!(text.contains("heaviest intervals"), "{text}");
        assert!(text.contains("-32.0 MiB"), "{text}");
        assert!(text.contains("the first reading of its key"), "{text}");
        assert!(text.contains("+4.0 KiB with nothing opening"), "{text}");
        assert!(text.contains("does not say which state"), "{text}");
        assert!(!text.contains("freed"), "{text}");
    }

    fn row(state: &str, kind: crate::join::Kind, amount: Option<f64>) -> Row {
        Row {
            state: state.into(),
            source: "sched.offcpu".into(),
            value: "total_ns".into(),
            kind,
            evidence: Some(Evidence::Key),
            amount,
            resolution: Some(0.0),
            readings: 3,
            short: None,
            missing: None,
            note: None,
            named: None,
        }
    }

    fn decomposed(residue: f64) -> DecompositionOutput {
        use crate::join::Kind;
        let mut whole = row("whole", Kind::Window, Some(100e6));
        whole.evidence = None;
        whole.resolution = None;
        DecompositionOutput {
            id: "where".into(),
            by: crate::request::KeyKind::Task,
            unit: Unit::Ns,
            tables: vec![Breakdown {
                key: "task:9".into(),
                whole,
                rows: vec![
                    row("on-cpu", Kind::Sampled, Some(60e6)),
                    row("blocked", Kind::Traced, Some(100e6 - 60e6 - residue)),
                ],
                parts: Some(100e6 - residue),
                residue: Some(residue),
                residue_resolution: Some(1e6),
            }],
            keys_seen: 2,
        }
    }

    /// The residue is printed signed, a negative one included, and the table
    /// says its rows are different kinds of number.
    #[test]
    fn a_decomposition_prints_a_signed_residue_and_the_kind_of_each_row() {
        let text = decomposition_text(&decomposed(-5e6));
        assert!(text.contains("RESIDUE"), "{text}");
        assert!(
            text.contains("-5.00ms"),
            "a negative residue keeps its sign: {text}"
        );
        assert!(text.contains("sampled"), "{text}");
        assert!(text.contains("traced"), "{text}");
        assert!(text.contains("sampled and traced"), "{text}");
        assert!(text.contains("counted twice"), "{text}");
        assert!(text.contains("the 1 heaviest of 2 keys"), "{text}");

        let positive = decomposition_text(&decomposed(2.5e6));
        assert!(positive.contains("+2.50ms"), "{positive}");
    }

    /// A row that could not be read says so and takes the residue with it,
    /// rather than reading as a zero that makes the rest add up.
    #[test]
    fn a_row_that_was_not_read_is_said_to_be_missing_rather_than_zero() {
        let mut output = decomposed(0.0);
        let table = &mut output.tables[0];
        table.rows[1].amount = None;
        table.rows[1].missing = Some("no `sched.offcpu` event carried `queued_ns`".into());
        table.parts = None;
        table.residue = None;
        let text = decomposition_text(&output);
        assert!(text.contains("not read"), "{text}");
        assert!(text.contains("carried `queued_ns`"), "{text}");
    }

    /// A board that used 42.18 J over two seconds drew 21.09 W on average, and
    /// the rate says so in watts; a rate over bytes keeps its `/s`.
    #[test]
    fn a_rate_over_joules_is_printed_in_watts() {
        let rate = |unit: Unit, total: f64| {
            let mut results = sample();
            results.unit = unit;
            results.outputs = vec![Output::Rate {
                series: vec![crate::emit::SeriesRate {
                    key: "card1".into(),
                    events: 20,
                    total,
                    per_second: Some(total / 2.0),
                    window_ns: 2_000_000_000,
                    unmeasured: 0,
                    counted_window: true,
                    short: None,
                }],
            }];
            text(&results)
        };
        let power = rate(Unit::Joules, 42.18);
        assert!(power.contains("42.18 J over"), "{power}");
        assert!(power.contains("21.09 W"), "{power}");
        assert!(!power.contains("J/s"), "{power}");
        let bytes = rate(Unit::Bytes, 4096.0);
        assert!(bytes.trim_end().ends_with("/s"), "{bytes}");
    }

    /// The text a person reads carries the same mark the structured formats
    /// do, on a rate, a ranking and a profile -- and a clean run gains none of
    /// it. `Rest` and `unattributed` keep their own lines: what a ranking left
    /// out and measured, and what arrived without a stack, are not what nothing
    /// measured at all.
    #[test]
    fn a_lossy_rate_ranking_and_profile_say_so_in_text_and_a_clean_one_does_not() {
        let losses = Losses {
            dropped: 10,
            ..Default::default()
        };
        let short = Shortfall::of(&losses, 90, false).expect("a dropped run is short");
        let build = |short: Option<Shortfall>| {
            let mut results = sample();
            results.outputs = vec![
                Output::Rate {
                    series: vec![crate::emit::SeriesRate {
                        key: "task:7".into(),
                        events: 90,
                        total: 900.0,
                        per_second: Some(900.0),
                        window_ns: 1_000_000_000,
                        unmeasured: 0,
                        counted_window: false,
                        short: short.clone(),
                    }],
                },
                Output::TopK {
                    k: 10,
                    series: vec![SeriesTotal {
                        key: "task:7".into(),
                        unmeasured: 0,
                        events: 90,
                        total: 900.0,
                        short: short.clone(),
                    }],
                    rest: Rest::default(),
                },
                Output::Stack {
                    stacks: Vec::new(),
                    unattributed: 3,
                    short: short.clone(),
                },
            ];
            text(&results)
        };

        let lossy = build(Some(short));
        assert!(
            lossy.matches(Shortfall::MARK).count() >= 3,
            "the rate, the ranking and the profile each carry the mark:\n{lossy}"
        );
        assert!(
            lossy.contains("the order above can be wrong"),
            "a ranking of lower bounds has to say its order is not certain:\n{lossy}"
        );
        assert!(
            lossy.contains("3 events carried no stack"),
            "`unattributed` keeps its own line beside the mark:\n{lossy}"
        );

        let clean = build(None);
        assert!(
            !clean.contains(Shortfall::MARK) && !clean.contains("can be wrong"),
            "a clean run gains nothing, or the mark becomes noise:\n{clean}"
        );
    }

    /// An `overlap` join prints what it credited: each interval's share of
    /// the downstream value, where the interval lies, and the readings'
    /// totals -- credited, uncredited, and the baselines that covered nothing.
    #[test]
    fn an_overlap_join_prints_each_interval_s_credited_amount() {
        let mut correlation = correlated(MatchKind::Overlap, "task:7", 0);
        correlation.evidence = Evidence::Temporal;
        correlation.readings = Some(crate::join::Readings {
            value: "total_ns".into(),
            reading: crate::join::Reading::Delta,
            baselines: 1,
            credited: 180e6,
            uncredited: 70e6,
            before_first: 0,
        });
        correlation.top[0].opened_ns = 1_000_000_000;
        correlation.top[0].closed_ns = Some(1_100_000_000);
        correlation.top[0].change = Some(crate::join::Change {
            delta: 80e6,
            opened: 0,
            closed: 0,
        });
        let text = correlation_text(&correlation);
        assert!(text.contains("CREDITED"), "{text}");
        assert!(text.contains("+80.00ms"), "the interval's share: {text}");
        assert!(text.contains("100.0"), "where the interval ends: {text}");
        assert!(
            text.contains("credited in all +180.00ms") && text.contains("+70.00ms fell before"),
            "the readings' totals: {text}"
        );
        assert!(text.contains("1 readings were the first"), "{text}");
        assert!(text.contains("overlapped no reading's interval"), "{text}");
    }

    /// A `woke` join prints, per waker, how many closes it released and the
    /// time from each close to the wakeup -- the amount a `paired` part reads.
    #[test]
    fn a_woke_join_prints_the_pairs_and_the_time_they_waited() {
        let mut correlation = correlated(MatchKind::Woke, "task:3 -> task:9", 0);
        correlation.evidence = Evidence::Wakeup;
        correlation.top[0].wakeups = Some(4);
        correlation.top[0].change = Some(crate::join::Change {
            delta: 4e6,
            opened: 4,
            closed: 4,
        });
        let text = correlation_text(&correlation);
        assert!(text.contains("PAIRS") && text.contains("WAITED"), "{text}");
        let line = text
            .lines()
            .find(|line| line.contains("task:3 -> task:9"))
            .expect("the waker's row");
        assert!(
            line.contains(" 4 ") && line.contains("4.00ms") && line.contains("1.00ms"),
            "four pairs, 4 ms in all, 1 ms each: {line}"
        );
        assert!(
            text.contains("followed by no wakeup of their task"),
            "{text}"
        );
    }

    /// A chain of two hops with an interrupt at its far end is wider than the
    /// column used to be, and the column grows rather than running into the
    /// next.
    #[test]
    fn a_two_hop_chain_does_not_run_into_the_next_column() {
        let chain = "interrupt (hardirq) -> task:128611 -> task:128610";
        let mut correlation = correlated(MatchKind::Wakeup, chain, 0);
        correlation.top[0].wakeups = Some(12);
        let text = correlation_text(&correlation);
        let line = text
            .lines()
            .find(|line| line.contains(chain))
            .expect("the row");
        let after = &line[line.find(chain).unwrap() + chain.len()..];
        assert!(after.starts_with(' '), "{line}");
        let header = text
            .lines()
            .find(|line| line.contains("WAKEUPS"))
            .expect("a header");
        assert_eq!(
            header.find("WAKEUPS").map(|at| at + "WAKEUPS".len()),
            line.find(" 12 ").map(|at| at + " 12".len()),
            "the count sits under its heading:\n{header}\n{line}"
        );
    }

    /// What each level a run's tables use establishes and does not, once per
    /// run and under the first table that shows a level, for every level
    /// shown and no other.
    #[test]
    fn a_run_says_once_what_each_level_it_shows_establishes() {
        let mut temporal = correlated(MatchKind::Eq, "req:1", 0);
        temporal.evidence = Evidence::Temporal;
        let run = RunOutput {
            results: vec![sample()],
            joins: vec![temporal.clone(), temporal],
            decompositions: vec![decomposed(0.0)],
        };
        let text = run_output(OutputFormat::Text, &run).unwrap();
        for level in [Evidence::Temporal, Evidence::Key] {
            let sentence = format!(
                "evidence {level}: {}; it does not establish {}",
                level.establishes(),
                level.does_not_establish()
            );
            assert_eq!(text.matches(&sentence).count(), 1, "{level}:\n{text}");
        }
        for level in [Evidence::Wakeup, Evidence::Identity] {
            assert!(
                !text.contains(&format!("evidence {level}:")),
                "{level} is in no table:\n{text}"
            );
        }
        let first_join_ends = text.find("heaviest keys").expect("the first join");
        let explained = text.find("evidence TEMPORAL:").expect("the sentence");
        let second_join = text.rfind("join r  a -> b").expect("the second join");
        assert!(
            first_join_ends < explained && explained < second_join,
            "under the first table that shows a level:\n{text}"
        );
        let bare = RunOutput {
            results: vec![sample()],
            joins: Vec::new(),
            decompositions: Vec::new(),
        };
        assert!(!run_output(OutputFormat::Text, &bare)
            .unwrap()
            .contains("it does not establish"));
    }
}
