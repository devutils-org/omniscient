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
use crate::scalar::Duration;

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
                    let _ = writeln!(
                        out,
                        "    {:<14} {} over {}  =  {}/s",
                        rate.key,
                        quantity(rate.total, results.unit),
                        approximate(rate.window_ns),
                        quantity(rate.per_second, results.unit),
                    );
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
        Unit::Ns => Duration::from_nanos(value.max(0.0) as u64).to_string(),
        Unit::Ratio => format!("{value:.3}"),
        other => format!("{value:.0} {other}"),
    }
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
            unverified: Vec::new(),
            degradations: vec!["fs:proc:statm: resident pages only".into()],
        }
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

    #[test]
    fn a_format_without_an_encoder_says_so() {
        let error = results(OutputFormat::Folded, &sample()).unwrap_err();
        assert!(error.message().contains("no encoder yet"), "{error}");
    }
}
