//! The layer above the engine: questions rather than measurements.
//!
//! `docs/ROADMAP.md` phase 10. A measurement request names a source and says what
//! to do with it; a triage request names a question and an objective, and this
//! layer works out which measurements answer it, asks the engine for them the
//! way any user would -- a [`Session`](crate::request::Session), run through
//! the same calls `omn -f` makes -- and reads the answer off the results.
//!
//! It never reaches under that surface. It does not edit what a collector
//! produces, does not add a join kind, does not change how a row of a
//! decomposition is computed: what it needs from those layers and cannot get is
//! filed in `docs/open-findings.md`, and what it computes itself -- a share, a
//! Little's-law product, a difference of two profiles -- is arithmetic on
//! numbers the engine already reported, with their resolution and their
//! shortfall carried through rather than summarised away.
//!
//! - [`objective`]: what an answer is for, required and never defaulted.
//! - [`latency`]: `omn latency`, thread state analysis -- *why is this slow*.
//! - [`saturate`]: `omn saturate`, USE with Little's law -- *what does it queue for*.
//! - [`diff`]: `omn diff`, two profiles and a subtraction.
//! - [`escalate`]: the escalation -- a run that attaches the detailed sources
//!   of the layer its own cheap pass names, and lets go of the rest.

pub mod diff;
pub mod engine;
pub mod escalate;
pub mod latency;
pub mod objective;
pub mod saturate;

use std::fmt;

pub use objective::{Method, Objective, Refusal};

use crate::collect::Whose;

/// Why a triage request produced no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriageError {
    /// The request cannot mean anything as asked: no objective, an objective
    /// the method does not answer for, a tail asked of one run.
    Refused(Refusal),
    /// The engine could not run what the method asked of it.
    Failed(engine::Failure),
}

impl TriageError {
    /// Whose gap it is, which is what the exit code is derived from.
    pub fn whose(&self) -> Whose {
        match self {
            TriageError::Refused(_) => Whose::Request,
            TriageError::Failed(failure) => failure.whose,
        }
    }
}

impl fmt::Display for TriageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TriageError::Refused(refusal) => refusal.fmt(f),
            TriageError::Failed(failure) => failure.fmt(f),
        }
    }
}

impl std::error::Error for TriageError {}

impl From<Refusal> for TriageError {
    fn from(refusal: Refusal) -> Self {
        TriageError::Refused(refusal)
    }
}

impl From<engine::Failure> for TriageError {
    fn from(failure: engine::Failure) -> Self {
        TriageError::Failed(failure)
    }
}

/// Nanoseconds a person can read, to the resolution the rows carry.
pub fn span(ns: f64) -> String {
    let magnitude = ns.abs();
    let sign = if ns < 0.0 { "-" } else { "" };
    if magnitude < 1e3 {
        format!("{sign}{magnitude:.0}ns")
    } else if magnitude < 1e6 {
        format!("{sign}{:.2}us", magnitude / 1e3)
    } else if magnitude < 1e9 {
        format!("{sign}{:.2}ms", magnitude / 1e6)
    } else {
        format!("{sign}{:.3}s", magnitude / 1e9)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_keeps_its_sign() {
        assert_eq!(span(-1_500_000.0), "-1.50ms");
        assert_eq!(span(999.0), "999ns");
        assert_eq!(span(2_000_000_000.0), "2.000s");
    }
}
