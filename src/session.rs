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
        assert_eq!(block.emit, vec![Emit::Hist(HistScale::Log2), Emit::TopK(5)]);
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

    #[test]
    fn an_unknown_field_in_a_session_is_refused_rather_than_ignored() {
        let text = "requests: [{source: io.latency, target: system, mode: trace, emitt: [hist]}]";
        let error = InputFormat::Yaml.parse::<Session>(text).unwrap_err();
        assert!(error.message().contains("emitt"), "{error}");
    }
}
