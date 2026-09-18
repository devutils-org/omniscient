//! Encoding results.
//!
//! Text is for reading, the structured formats are for tooling, and `ndjson` is
//! for a stream that does not fit in memory. Each format declares which emits it
//! can carry (see `format`), so this module never has to approximate: it either
//! knows how to write what it was handed, or it says which format would.

use std::fmt::Write as _;

use crate::catalog::Unit;
use crate::emit::{Output, Results};
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
        "  {} of {} `{}` events attributed ({share:.1} %); {} fell outside every open range",
        correlation.matched, total, correlation.to, correlation.unmatched
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

    let _ = writeln!(out, "\n  heaviest ranges");
    let _ = writeln!(
        out,
        "    {:<34} {:>10}  {:>8}  OUTCOME",
        "RANGE", "SIZE", "EVENTS"
    );
    for attribution in &correlation.top {
        let _ = writeln!(
            out,
            "    {:<34} {:>10}  {:>8}  {}",
            attribution.key,
            bytes(attribution.len as f64),
            attribution.matched,
            outcome(attribution.outcome)
        );
    }
    let _ = writeln!(
        out,
        "\n  EVENTS counts faults, not bytes, and on a kernel with multi-size transparent huge \
         pages it does not even rank by bytes: the kernel picks larger folios for larger regions, \
         so a 1 MiB and a 32 MiB allocation both fault a few hundred times. Read it as where the \
         faults happened, not as how much memory they cost; the THP mode is in the environment."
    );
    out
}

fn outcome(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Closed => "freed",
        Outcome::Expired => "expired",
        Outcome::Overflowed => "evicted",
        Outcome::StillOpen => "live",
    }
}

/// Encodes results in the session's output format.
pub fn results(format: OutputFormat, results: &Results) -> Result<String, ParseError> {
    match format {
        OutputFormat::Text => Ok(text(results)),
        OutputFormat::Ndjson => ndjson(results),
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
    use super::*;
    use crate::collect::Losses;
    use crate::emit::{Point, Series, SeriesSummary};

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

    /// What a format claims about itself has to match what it does. A merge
    /// once put `ndjson` back to "planned" while it was encoding perfectly
    /// well, and nothing noticed: the claim and the encoder are in different
    /// files and only a test crosses them.
    #[test]
    fn every_format_does_what_its_declared_status_says() {
        let run = correlated_run();
        for format in OutputFormat::ALL {
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
        let error = results(OutputFormat::Folded, &sample()).unwrap_err();
        assert!(error.message().contains("no encoder yet"), "{error}");
    }
}
