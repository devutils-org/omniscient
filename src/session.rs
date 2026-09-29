//! Session files: the same request, written down.
//!
//! A session file is not a second way of describing a measurement. It
//! deserializes into exactly the `Session` a command line builds, which is what
//! makes `omn --dump-request` able to print a runnable file and `omn -f` able to
//! replay it. If the two surfaces could express different things, one of them
//! would quietly become the real one.
//!
//! YAML, JSON and TOML are all accepted; the format is inferred from the
//! extension and can be forced. Reading from standard input requires the
//! format, since there is no extension to infer from.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::ParseError;
use crate::format::InputFormat;
use crate::request::Session;

/// Where a session came from, for error messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    File(PathBuf),
    Stdin,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Origin::File(path) => write!(f, "{}", path.display()),
            Origin::Stdin => f.write_str("<stdin>"),
        }
    }
}

/// Reads a session from a file, or from standard input when the path is `-`.
///
/// Session-wide defaults are pushed down into the requests before returning, so
/// every request that comes out is self-contained.
pub fn load(path: &Path, forced: Option<InputFormat>) -> Result<(Session, Origin), ParseError> {
    let (text, origin) = if path == Path::new("-") {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|error| ParseError::new(format!("cannot read standard input: {error}")))?;
        (text, Origin::Stdin)
    } else {
        let text = std::fs::read_to_string(path)
            .map_err(|error| ParseError::new(format!("cannot read {}: {error}", path.display())))?;
        (text, Origin::File(path.to_path_buf()))
    };

    let format = match (forced, &origin) {
        (Some(format), _) => format,
        (None, Origin::File(path)) => InputFormat::from_path(path).ok_or_else(|| {
            ParseError::new(format!(
                "cannot tell the format of {} from its name; pass --input-format yaml|json|toml",
                path.display()
            ))
        })?,
        (None, Origin::Stdin) => {
            return Err(ParseError::new(
                "reading a session from standard input needs --input-format yaml|json|toml",
            ))
        }
    };

    let mut session: Session = format
        .parse(&text)
        .map_err(|error| ParseError::new(format!("{origin}: {error}")))?;
    session.apply_defaults();
    session
        .check_escalation()
        .and_then(|()| session.check_recording())
        .map_err(|error| ParseError::new(format!("{origin}: {error}")))?;
    Ok((session, origin))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::OutputFormat;
    use crate::request::{Emit, HistScale, Mode, Target};

    const YAML: &str = r#"
name: web latency budget
duration: 30s
budget: 2%
format: yaml
requests:
  - label: block
    source: io.latency
    target: {pid: [4242]}
    mode: trace
    emit: [hist, topk:5]
    keys: [blk, task]
    filter: "bytes > 4KiB && rwbs == 'R'"
  - label: blocked-time
    source: sched.offcpu
    target: {pid: [4242]}
    mode: trace
    emit: [stack]
    stack: fp
    duration: 10s
"#;

    const JSON: &str = r#"{
      "duration": "30s",
      "requests": [
        {"source": "io.latency", "target": {"pid": [4242]}, "mode": "trace", "emit": ["hist"]}
      ]
    }"#;

    const TOML: &str = r#"
duration = "30s"
format = "json"

[[requests]]
source = "io.latency"
mode = "trace"
emit = ["hist"]
target = {pid = [4242]}
"#;

    fn parse(format: InputFormat, text: &str) -> Session {
        let mut session: Session = format.parse(text).expect("session should parse");
        session.apply_defaults();
        session
    }

    #[test]
    fn a_yaml_session_carries_every_axis_of_a_request() {
        let session = parse(InputFormat::Yaml, YAML);
        assert_eq!(session.name.as_deref(), Some("web latency budget"));
        assert_eq!(session.format, OutputFormat::Yaml);
        assert_eq!(session.requests.len(), 2);

        let block = &session.requests[0];
        assert_eq!(block.source.as_str(), "io.latency");
        assert_eq!(block.target, Target::Pid(vec![4242]));
        assert_eq!(block.mode, Mode::Trace);
        assert_eq!(
            block.emit,
            vec![Emit::Hist(HistScale::Log2), Emit::TopK { k: 5, by: None }]
        );
        assert_eq!(
            block.filter.as_ref().unwrap().to_string(),
            "bytes > 4KiB && rwbs == 'R'"
        );
    }

    #[test]
    fn session_defaults_reach_the_requests_that_omit_them() {
        let session = parse(InputFormat::Yaml, YAML);
        assert_eq!(session.requests[0].duration.unwrap().to_string(), "30s");
        // The second request sets its own duration and keeps it.
        assert_eq!(session.requests[1].duration.unwrap().to_string(), "10s");
        // Neither sets a budget, so both inherit the session's.
        assert_eq!(session.requests[1].budget.unwrap().to_string(), "2%");
    }

    #[test]
    fn the_three_input_formats_describe_the_same_request() {
        let from_yaml = parse(InputFormat::Yaml, YAML);
        let from_json = parse(InputFormat::Json, JSON);
        let from_toml = parse(InputFormat::Toml, TOML);
        assert_eq!(from_json.requests[0].source, from_toml.requests[0].source);
        assert_eq!(from_json.requests[0].target, from_toml.requests[0].target);
        assert_eq!(from_json.requests[0].emit, from_toml.requests[0].emit);
        assert_eq!(from_yaml.requests[0].source, from_json.requests[0].source);
    }

    #[test]
    fn a_session_survives_a_round_trip_through_every_format() {
        let session = parse(InputFormat::Yaml, YAML);
        for format in [OutputFormat::Yaml, OutputFormat::Json, OutputFormat::Toml] {
            let text = format.serialize_document(&session).expect("serializing");
            let input = match format {
                OutputFormat::Yaml => InputFormat::Yaml,
                OutputFormat::Json => InputFormat::Json,
                OutputFormat::Toml => InputFormat::Toml,
                _ => unreachable!(),
            };
            let round_tripped = parse(input, &text);
            assert_eq!(
                round_tripped, session,
                "round trip through {format} changed the session"
            );
        }
    }

    /// A window is part of the request, so it survives the dump: the file
    /// and the command line build the same `Request`, and a dumped session is
    /// the one that was run.
    #[test]
    fn a_window_survives_a_round_trip_through_every_format() {
        const WINDOWED: &str = r#"
duration: 3s
requests:
  - label: first phase
    source: cpu.instructions
    target: {launch: [./two-phases]}
    mode: count
    window: ..1s
  - label: second phase
    source: cpu.instructions
    target: {launch: [./two-phases]}
    mode: count
    window: 1500ms..
  - label: in between
    source: cpu.instructions
    target: {launch: [./two-phases]}
    mode: count
    window: 1s..1500ms
  - label: whole
    source: cpu.instructions
    target: {launch: [./two-phases]}
    mode: count
"#;
        let session = parse(InputFormat::Yaml, WINDOWED);
        let windows: Vec<String> = session
            .requests
            .iter()
            .map(|request| {
                request
                    .window
                    .map(|window| window.to_string())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(windows, ["..1s", "1500ms..", "1s..1500ms", ""]);
        for format in [OutputFormat::Yaml, OutputFormat::Json, OutputFormat::Toml] {
            let text = format.serialize_document(&session).expect("serializing");
            let input = match format {
                OutputFormat::Yaml => InputFormat::Yaml,
                OutputFormat::Json => InputFormat::Json,
                OutputFormat::Toml => InputFormat::Toml,
                _ => unreachable!(),
            };
            assert_eq!(
                parse(input, &text),
                session,
                "round trip through {format} changed a window:\n{text}"
            );
        }
        // A request with no window says nothing about one: a session written
        // before hot attach dumps as it did.
        let text = OutputFormat::Yaml.serialize_document(&session).unwrap();
        assert_eq!(text.matches("window:").count(), 3, "{text}");
    }

    #[test]
    fn a_malformed_window_is_refused_where_it_is_written() {
        let text = "requests: [{source: io.latency, target: system, mode: trace, window: 3s..1s}]";
        let error = InputFormat::Yaml.parse::<Session>(text).unwrap_err();
        assert!(
            error.message().contains("closes before it opens"),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_field_in_a_session_is_refused_rather_than_ignored() {
        let text = "requests: [{source: io.latency, target: system, mode: trace, emitt: [hist]}]";
        let error = InputFormat::Yaml.parse::<Session>(text).unwrap_err();
        assert!(error.message().contains("emitt"), "{error}");
    }

    const RECORDING: &str = r#"
duration: 30s
requests:
  - label: descriptors
    source: proc.fd
    target: {launch: [./bursts]}
    mode: poll@50ms
    emit: [timeline]
record:
  trigger: "value >= 64"
  on: descriptors
  retain: 1s
  after: 200ms
  documents: 2
"#;

    /// A recording is part of the session, so it survives the dump in every
    /// format: the file that was run is the file that is kept.
    #[test]
    fn a_recording_survives_a_round_trip_through_every_format() {
        let session = parse(InputFormat::Yaml, RECORDING);
        session
            .check_recording()
            .expect("a whole recording is accepted");
        let record = session.record.as_ref().expect("the block is read");
        assert_eq!(record.trigger.as_ref().unwrap().to_string(), "value >= 64");
        assert_eq!(record.retain_ns(), 1_000_000_000);
        assert_eq!(record.after_ns(), 200_000_000);
        assert_eq!(session.recorded_index().unwrap(), 0);
        for format in [OutputFormat::Yaml, OutputFormat::Json, OutputFormat::Toml] {
            let text = format.serialize_document(&session).expect("serializing");
            let input = match format {
                OutputFormat::Yaml => InputFormat::Yaml,
                OutputFormat::Json => InputFormat::Json,
                OutputFormat::Toml => InputFormat::Toml,
                _ => unreachable!(),
            };
            assert_eq!(parse(input, &text), session, "{format} changed it:\n{text}");
        }
    }

    /// Each bound is required, and each refusal says what the bound is for.
    #[test]
    fn a_recording_without_one_of_its_bounds_is_refused_by_name() {
        let refused = |from: &str, to: &str| {
            let text = RECORDING.replace(from, to);
            let session = parse(InputFormat::Yaml, &text);
            session
                .check_recording()
                .expect_err(&format!("`{from}` -> `{to}` should be refused"))
                .message()
                .to_owned()
        };
        assert!(refused("  retain: 1s\n", "").contains("needs `retain`"));
        assert!(refused("  documents: 2\n", "").contains("needs `documents`"));
        assert!(refused("  documents: 2\n", "  documents: 0\n").contains("needs `documents`"));
        assert!(refused("duration: 30s\n", "").contains("needs `duration`"));
        assert!(refused("  trigger: \"value >= 64\"\n", "").contains("names none"));
        assert!(refused("retain: 1s", "retain: 30s").contains("not shorter than the run"));
        assert!(refused("on: descriptors", "on: nothing").contains("names no request"));
        assert!(refused("mode: poll@50ms", "mode: count").contains("`count` mode"));
        assert!(
            refused("emit: [timeline]", "emit: [timeline]\n    window: 1s..")
                .contains("watched for the whole run")
        );
    }

    /// A malformed trigger is refused where it is written, by the filter
    /// grammar's own parser.
    #[test]
    fn a_trigger_the_filter_grammar_cannot_read_is_refused_at_parse() {
        let text = RECORDING.replace("value >= 64", "value = 64");
        let error = InputFormat::Yaml.parse::<Session>(&text).unwrap_err();
        assert!(error.message().contains("`==`"), "{error}");
    }

    /// The trigger reads `value`, `kind` and the fields the source declares,
    /// and nothing else: a field no event carries would never fire.
    #[test]
    fn a_trigger_over_a_field_the_source_does_not_emit_is_refused() {
        let catalog = crate::catalog::Catalog::embedded().unwrap();
        let session = parse(InputFormat::Yaml, RECORDING);
        session
            .check_trigger_fields(&catalog, None)
            .expect("`value` is readable");
        let text = RECORDING.replace("value >= 64", "bytes > 4096");
        let session = parse(InputFormat::Yaml, &text);
        let error = session.check_trigger_fields(&catalog, None).unwrap_err();
        assert!(
            error.message().contains("`bytes`") && error.message().contains("`value`"),
            "{error}"
        );
    }
}
