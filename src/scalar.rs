//! Scalar literals shared by every input surface: durations, frequencies and
//! sizes.
//!
//! The CLI, the session file and the filter grammar all accept the same
//! literals, so they all parse them here. A duration is always normalized to
//! nanoseconds and a size to bytes, which is the unit every downstream consumer
//! works in.

use std::fmt;
use std::str::FromStr;

use crate::error::ParseError;

/// A duration, normalized to nanoseconds.
///
/// Accepts `ns`, `us` (or `µs`), `ms`, `s`, `m`, `h`. A bare number is refused:
/// an unlabelled duration is exactly the kind of ambiguity this tool is meant to
/// eliminate. Decimal fractions are accepted when they represent a whole
/// number of nanoseconds; values are never rounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Duration {
    nanos: u64,
}

impl Duration {
    pub const fn from_nanos(nanos: u64) -> Self {
        Self { nanos }
    }

    pub const fn as_nanos(self) -> u64 {
        self.nanos
    }

    pub fn as_secs_f64(self) -> f64 {
        self.nanos as f64 / 1e9
    }
}

impl FromStr for Duration {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let split = s
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .ok_or_else(|| {
                ParseError::new(format!("duration `{s}` has no unit (try `30s`, `500ms`)"))
            })?;
        if split == 0 {
            return Err(ParseError::new(format!(
                "duration `{s}` does not start with a number"
            )));
        }
        let (value, unit) = s.split_at(split);
        let multiplier = match unit {
            "ns" => 1,
            "us" | "µs" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60 * 1_000_000_000,
            "h" => 3_600 * 1_000_000_000,
            other => {
                return Err(ParseError::new(format!(
                    "unknown duration unit `{other}` (expected ns, us, ms, s, m, h)"
                )))
            }
        };
        // Decimal arithmetic keeps integer nanoseconds exact, including near u64::MAX.
        let invalid = || {
            ParseError::new(format!(
                "duration `{s}` is invalid or overflows; use an exact number of nanoseconds"
            ))
        };
        let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
        if whole.is_empty() || (value.contains('.') && fraction.is_empty()) {
            return Err(invalid());
        }
        let whole: u64 = whole.parse().map_err(|_| invalid())?;
        let mut nanos = whole.checked_mul(multiplier).ok_or_else(invalid)?;
        if !fraction.is_empty() {
            let digits = fraction.trim_end_matches('0');
            if !digits.is_empty() {
                let numerator: u64 = digits.parse().map_err(|_| invalid())?;
                let denominator = 10u64
                    .checked_pow(digits.len().try_into().map_err(|_| invalid())?)
                    .ok_or_else(invalid)?;
                let scaled = u128::from(numerator) * u128::from(multiplier);
                if scaled % u128::from(denominator) != 0 {
                    return Err(invalid());
                }
                nanos = nanos
                    .checked_add((scaled / u128::from(denominator)) as u64)
                    .ok_or_else(invalid)?;
            }
        }
        Ok(Self::from_nanos(nanos))
    }
}

impl fmt::Display for Duration {
    /// Renders with the largest unit that keeps the value exact, so a duration
    /// survives a round trip through a session file unchanged.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [(u64, &str); 6] = [
            (3_600_000_000_000, "h"),
            (60_000_000_000, "m"),
            (1_000_000_000, "s"),
            (1_000_000, "ms"),
            (1_000, "us"),
            (1, "ns"),
        ];
        for (factor, suffix) in UNITS {
            if self.nanos >= factor && self.nanos % factor == 0 {
                return write!(f, "{}{}", self.nanos / factor, suffix);
            }
        }
        write!(f, "{}ns", self.nanos)
    }
}

/// A sampling frequency in hertz.
///
/// Accepts `997hz`, `1khz`, or a bare number read as hertz.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Frequency {
    hz: u64,
}

impl Frequency {
    pub const fn from_hz(hz: u64) -> Self {
        Self { hz }
    }

    pub const fn as_hz(self) -> u64 {
        self.hz
    }
}

impl FromStr for Frequency {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let lower = s.to_ascii_lowercase();
        let (digits, multiplier) = match lower.strip_suffix("khz") {
            Some(digits) => (digits, 1_000),
            None => (lower.strip_suffix("hz").unwrap_or(&lower), 1),
        };
        let value: u64 = digits.parse().map_err(|_| {
            ParseError::new(format!(
                "frequency `{s}` is not a number (try `997hz`, `1khz`)"
            ))
        })?;
        if value == 0 {
            return Err(ParseError::new("frequency must be greater than zero"));
        }
        value
            .checked_mul(multiplier)
            .map(Self::from_hz)
            .ok_or_else(|| ParseError::new(format!("frequency `{s}` overflows")))
    }
}

impl fmt::Display for Frequency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}hz", self.hz)
    }
}

/// Parses an integer literal with an optional size or duration suffix, used by
/// the filter grammar and by linear histogram widths.
///
/// Sizes are binary (`4KiB` = 4096) or decimal (`4KB` = 4000); durations are
/// normalized to nanoseconds; a bare number passes through unchanged.
pub fn parse_scalar(s: &str) -> Result<i64, ParseError> {
    let s = s.trim();
    if let Ok(value) = s.parse::<i64>() {
        return Ok(value);
    }
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .ok_or_else(|| ParseError::new(format!("`{s}` is not a number")))?;
    let (digits, unit) = s.split_at(split);
    let value: i64 = digits
        .parse()
        .map_err(|_| ParseError::new(format!("`{s}` does not start with a number")))?;
    let multiplier: i64 = match unit {
        "B" => 1,
        "KiB" | "Ki" => 1 << 10,
        "MiB" | "Mi" => 1 << 20,
        "GiB" | "Gi" => 1 << 30,
        "TiB" | "Ti" => 1i64 << 40,
        "KB" | "kB" | "k" | "K" => 1_000,
        "MB" | "M" => 1_000_000,
        "GB" | "G" => 1_000_000_000,
        "TB" | "T" => 1_000_000_000_000,
        _ => {
            return Duration::from_str(s)
                .map(|d| d.as_nanos() as i64)
                .map_err(|_| {
                    ParseError::new(format!(
                        "unknown suffix in `{s}` (expected a size like 4KiB or a duration like 5ms)"
                    ))
                })
        }
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| ParseError::new(format!("`{s}` overflows")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_durations_are_exact_and_never_rounded() {
        for (text, nanos) in [
            ("3.9s", 3_900_000_000),
            ("0.000000001s", 1),
            ("1.5m", 90_000_000_000),
            ("1.000000000000000000000s", 1_000_000_000),
            ("18446744073.709551615s", u64::MAX),
        ] {
            assert_eq!(
                text.parse::<Duration>().unwrap().as_nanos(),
                nanos,
                "{text}"
            );
        }
        for text in [
            ".9s",
            "3.s",
            "3..9s",
            "-1s",
            "1.1ns",
            "0.0000000001s",
            "18446744073.709551616s",
        ] {
            assert!(text.parse::<Duration>().is_err(), "{text}");
        }
        let window: crate::request::Window = "3.9s..4.1s".parse().unwrap();
        assert_eq!(window.start.unwrap().as_nanos(), 3_900_000_000);
        assert_eq!(window.stop.unwrap().as_nanos(), 4_100_000_000);
    }

    #[test]
    fn durations_round_trip_through_their_display_form() {
        for text in ["30s", "500ms", "1h", "250us", "7ns", "2m"] {
            let parsed: Duration = text.parse().unwrap();
            assert_eq!(parsed.to_string(), text, "round trip failed for {text}");
        }
    }

    #[test]
    fn a_duration_without_a_unit_is_refused() {
        assert!("30".parse::<Duration>().is_err());
    }

    #[test]
    fn frequencies_accept_bare_hertz_and_kilohertz() {
        assert_eq!("997".parse::<Frequency>().unwrap().as_hz(), 997);
        assert_eq!("997hz".parse::<Frequency>().unwrap().as_hz(), 997);
        assert_eq!("1kHz".parse::<Frequency>().unwrap().as_hz(), 1_000);
        assert!("0hz".parse::<Frequency>().is_err());
    }

    #[test]
    fn scalars_accept_binary_decimal_and_duration_suffixes() {
        assert_eq!(parse_scalar("4096").unwrap(), 4096);
        assert_eq!(parse_scalar("4KiB").unwrap(), 4096);
        assert_eq!(parse_scalar("4KB").unwrap(), 4000);
        assert_eq!(parse_scalar("5ms").unwrap(), 5_000_000);
        assert!(parse_scalar("4kittens").is_err());
    }
}

serde_as_string!(Duration);
serde_as_string!(Frequency);
