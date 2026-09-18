//! Error types shared across the input surfaces.

use std::fmt;

/// A textual component of a request could not be parsed.
///
/// Parse errors are user-facing: they are printed verbatim, so the message
/// names what was refused and what the accepted form looks like.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ParseError(String);

impl ParseError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    pub fn message(&self) -> &str {
        &self.0
    }
}

/// Suggests the closest candidate to a misspelled name, for error messages.
///
/// Returns nothing when no candidate is close enough, so an unrelated typo does
/// not produce a misleading "did you mean".
pub fn nearest<'a>(needle: &str, haystack: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let max_distance = (needle.len() / 3).max(2);
    haystack
        .into_iter()
        .map(|candidate| (edit_distance(needle, candidate), candidate))
        .filter(|(distance, _)| *distance <= max_distance)
        .min_by_key(|(distance, candidate)| (*distance, candidate.len()))
        .map(|(_, candidate)| candidate)
}

/// Levenshtein distance, two rows at a time.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != cb);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

/// Formats a list of accepted values for an error message.
pub fn one_of<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    values.into_iter().collect::<Vec<_>>().join(", ")
}

/// Writes `did you mean` when a near match exists, nothing otherwise.
pub struct DidYouMean<'a>(pub Option<&'a str>);

impl fmt::Display for DidYouMean<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(candidate) => write!(f, "; did you mean `{candidate}`?"),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_finds_a_plausible_typo() {
        assert_eq!(
            nearest("io.latancy", ["io.latency", "io.queue"]),
            Some("io.latency")
        );
    }

    #[test]
    fn nearest_refuses_to_guess_when_nothing_is_close() {
        assert_eq!(nearest("gpu.job", ["io.latency", "mem.rss"]), None);
    }
}
