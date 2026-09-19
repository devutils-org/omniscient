//! Input and output formats.
//!
//! Two different jobs share this module. On the way in, a session file may be
//! written as YAML, JSON or TOML; all three deserialize into the same `Session`,
//! so the choice is a matter of what generates the file, never of what the tool
//! can express.
//!
//! On the way out there are two kinds of payload. A *document* is a request, a
//! resolved request or a catalog listing: structured, small, and expressible in
//! any of the structured formats. A *result* is what a collector produces, and
//! there the format has to match the emit: collapsed stacks describe a profile
//! and nothing else, a Chrome trace describes a timeline and nothing else. The
//! matrix below is what keeps `--emit hist --format folded` from producing a
//! file that parses but means nothing.
//!
//! Result encoders are declared here before they exist. They are reported as
//! not yet implemented, with the phase that brings them, rather than silently
//! falling back to another format.

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::{one_of, ParseError};
use crate::request::EmitKind;

/// The format of a session file on the way in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    Yaml,
    Json,
    Toml,
}

impl InputFormat {
    pub const ALL: [InputFormat; 3] = [InputFormat::Yaml, InputFormat::Json, InputFormat::Toml];

    pub fn as_str(self) -> &'static str {
        match self {
            InputFormat::Yaml => "yaml",
            InputFormat::Json => "json",
            InputFormat::Toml => "toml",
        }
    }

    /// Infers the format from a file extension.
    ///
    /// Returns nothing rather than guessing when the extension is unknown: the
    /// caller then asks for `--input-format` instead of parsing a file as
    /// something it is not.
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "yaml" | "yml" => Some(InputFormat::Yaml),
            "json" => Some(InputFormat::Json),
            "toml" => Some(InputFormat::Toml),
            _ => None,
        }
    }

    pub fn parse<T: DeserializeOwned>(self, text: &str) -> Result<T, ParseError> {
        match self {
            InputFormat::Yaml => serde_norway::from_str(text).map_err(to_parse_error("YAML")),
            InputFormat::Json => serde_json::from_str(text).map_err(to_parse_error("JSON")),
            InputFormat::Toml => toml::from_str(text).map_err(to_parse_error("TOML")),
        }
    }
}

fn to_parse_error<E: fmt::Display>(format: &'static str) -> impl Fn(E) -> ParseError {
    move |error| ParseError::new(format!("invalid {format}: {error}"))
}

impl FromStr for InputFormat {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let name = s.trim().to_ascii_lowercase();
        let name = if name == "yml" {
            "yaml".to_owned()
        } else {
            name
        };
        InputFormat::ALL
            .into_iter()
            .find(|format| format.as_str() == name)
            .ok_or_else(|| {
                ParseError::new(format!(
                    "unknown input format `{s}` (expected {})",
                    one_of(InputFormat::ALL.iter().map(|format| format.as_str()))
                ))
            })
    }
}

impl fmt::Display for InputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The format of what the tool writes out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-readable, the default. Aligned for a terminal, not for a parser.
    #[default]
    Text,
    Json,
    Yaml,
    Toml,
    /// One JSON document per line: the shape an event stream takes.
    Ndjson,
    /// Collapsed stacks, `main;work;read 1234`. The input of flamegraph.pl,
    /// inferno and speedscope.
    Folded,
    /// Speedscope's own JSON, which carries sampled profiles and evented
    /// timelines alike.
    Speedscope,
    /// Gzipped profile.proto, read by pprof, Parca and Polar Signals.
    Pprof,
    /// Chrome Trace Event Format, read by Perfetto UI and chrome://tracing.
    ChromeTrace,
}

impl OutputFormat {
    pub const ALL: [OutputFormat; 9] = [
        OutputFormat::Text,
        OutputFormat::Json,
        OutputFormat::Yaml,
        OutputFormat::Toml,
        OutputFormat::Ndjson,
        OutputFormat::Folded,
        OutputFormat::Speedscope,
        OutputFormat::Pprof,
        OutputFormat::ChromeTrace,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            OutputFormat::Text => "text",
            OutputFormat::Json => "json",
            OutputFormat::Yaml => "yaml",
            OutputFormat::Toml => "toml",
            OutputFormat::Ndjson => "ndjson",
            OutputFormat::Folded => "folded",
            OutputFormat::Speedscope => "speedscope",
            OutputFormat::Pprof => "pprof",
            OutputFormat::ChromeTrace => "chrome",
        }
    }

    /// One line for `omn formats` and for error messages.
    pub fn describe(self) -> &'static str {
        match self {
            OutputFormat::Text => "human-readable, aligned for a terminal",
            OutputFormat::Json => "structured, one document",
            OutputFormat::Yaml => "structured, re-readable as a session file",
            OutputFormat::Toml => "structured, re-readable as a session file",
            OutputFormat::Ndjson => "one JSON document per line, for event streams",
            OutputFormat::Folded => "collapsed stacks for flamegraph.pl, inferno, speedscope",
            OutputFormat::Speedscope => "speedscope JSON, sampled profiles and evented timelines",
            OutputFormat::Pprof => "gzipped profile.proto, for pprof and Parca",
            OutputFormat::ChromeTrace => "Chrome Trace Event Format, for Perfetto UI",
        }
    }

    /// Whether the format can carry a request, a resolved request or a catalog
    /// listing.
    pub fn renders_documents(self) -> bool {
        matches!(
            self,
            OutputFormat::Text | OutputFormat::Json | OutputFormat::Yaml | OutputFormat::Toml
        )
    }

    /// Whether the format can carry results produced by this emit.
    ///
    /// A profile format cannot express a histogram and a trace format cannot
    /// express a rate; the pairing is refused instead of being approximated.
    pub fn supports_emit(self, emit: EmitKind) -> bool {
        match self {
            OutputFormat::Text
            | OutputFormat::Json
            | OutputFormat::Yaml
            | OutputFormat::Toml
            | OutputFormat::Ndjson => true,
            OutputFormat::Folded | OutputFormat::Pprof => emit == EmitKind::Stack,
            OutputFormat::Speedscope => matches!(emit, EmitKind::Stack | EmitKind::Timeline),
            OutputFormat::ChromeTrace => matches!(emit, EmitKind::Timeline | EmitKind::Raw),
        }
    }

    /// Whether this format's encoder exists yet.
    ///
    /// Formats are declared before their encoder lands so that the surface is
    /// stable, but asking for one that is not written yet fails loudly.
    pub fn status(self) -> FormatStatus {
        match self {
            OutputFormat::Text | OutputFormat::Json | OutputFormat::Yaml | OutputFormat::Toml => {
                FormatStatus::Ready
            }
            // ndjson landed with the first collector: it encodes any record
            // stream, so it needed no emit-specific work.
            OutputFormat::Ndjson => FormatStatus::Ready,
            // Written with the aggregation layer, not with the first
            // stack-producing collector: `folded` renders an aggregate of stack
            // identities and `speedscope` renders that or a timeline, and
            // neither needed a collector to exist. Where the stacks come from
            // is a separate question from whether this can write them down.
            OutputFormat::Folded | OutputFormat::Speedscope => FormatStatus::Ready,
            OutputFormat::Pprof | OutputFormat::ChromeTrace => FormatStatus::Planned {
                phase: "phase 4, with the correlated vertical case",
            },
        }
    }

    pub fn serialize_document<T: Serialize>(self, value: &T) -> Result<String, ParseError> {
        match self {
            OutputFormat::Json => {
                serde_json::to_string_pretty(value).map_err(to_parse_error("JSON output"))
            }
            OutputFormat::Yaml => {
                serde_norway::to_string(value).map_err(to_parse_error("YAML output"))
            }
            OutputFormat::Toml => {
                toml::to_string_pretty(value).map_err(to_parse_error("TOML output"))
            }
            OutputFormat::Text => Err(ParseError::new(
                "text output is rendered by the caller, not serialized",
            )),
            other => Err(ParseError::new(format!(
                "`{other}` describes results, not documents; use text, json, yaml or toml"
            ))),
        }
    }
}

impl FromStr for OutputFormat {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let name = s.trim().to_ascii_lowercase();
        let name = match name.as_str() {
            "yml" => "yaml",
            "chrome-trace" | "trace-event" | "perfetto" => "chrome",
            other => other,
        };
        OutputFormat::ALL
            .into_iter()
            .find(|format| format.as_str() == name)
            .ok_or_else(|| {
                ParseError::new(format!(
                    "unknown output format `{s}` (expected {})",
                    one_of(OutputFormat::ALL.iter().map(|format| format.as_str()))
                ))
            })
    }
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for OutputFormat {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for OutputFormat {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Whether a format's encoder is written yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatStatus {
    Ready,
    Planned { phase: &'static str },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_formats_are_inferred_from_the_extension_only_when_known() {
        assert_eq!(
            InputFormat::from_path(Path::new("s.yml")),
            Some(InputFormat::Yaml)
        );
        assert_eq!(
            InputFormat::from_path(Path::new("s.json")),
            Some(InputFormat::Json)
        );
        assert_eq!(
            InputFormat::from_path(Path::new("s.toml")),
            Some(InputFormat::Toml)
        );
        assert_eq!(InputFormat::from_path(Path::new("session")), None);
    }

    #[test]
    fn profile_formats_refuse_emits_they_cannot_express() {
        assert!(OutputFormat::Folded.supports_emit(EmitKind::Stack));
        assert!(!OutputFormat::Folded.supports_emit(EmitKind::Hist));
        assert!(OutputFormat::Speedscope.supports_emit(EmitKind::Timeline));
        assert!(!OutputFormat::ChromeTrace.supports_emit(EmitKind::Stack));
    }

    #[test]
    fn result_formats_are_not_document_formats() {
        assert!(OutputFormat::Yaml.renders_documents());
        assert!(!OutputFormat::Folded.renders_documents());
        assert!(OutputFormat::Pprof.serialize_document(&1).is_err());
    }

    #[test]
    fn format_names_round_trip() {
        for format in OutputFormat::ALL {
            assert_eq!(format.as_str().parse::<OutputFormat>().unwrap(), format);
        }
    }
}
