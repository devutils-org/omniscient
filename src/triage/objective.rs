//! What a triage answer is for.
//!
//! "What is this workload limited by" has no answer until someone says *limited
//! for what*. Median latency, the tail, throughput and energy do not name the
//! same culprit, and improving one routinely degrades another: a program that
//! waits on one synchronous write at a time is limited by the disk for its
//! latency and by nothing at all for its throughput, because the disk is idle
//! most of the time it is waited on. So a triage request carries its objective
//! the way a measurement request carries its emit, and a request that does not
//! say which is refused with the list rather than defaulted -- a layer that
//! picked one silently would report a confident wrong suspect, which is the
//! failure this project is built against, one level up.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// What the answer is optimising for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Objective {
    /// How long one run of the work takes, typically: the median run.
    Latency,
    /// How long the slowest runs take: what makes a slow run slow.
    Tail,
    /// How much work gets done per second: what the work queues for.
    Throughput,
    /// How much energy the work costs: what burns the energy the machine meters.
    Energy,
}

serde_as_string!(Objective);

impl Objective {
    pub const ALL: [Objective; 4] = [
        Objective::Latency,
        Objective::Tail,
        Objective::Throughput,
        Objective::Energy,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Objective::Latency => "latency",
            Objective::Tail => "tail",
            Objective::Throughput => "throughput",
            Objective::Energy => "energy",
        }
    }

    /// What minimising it means, in one line.
    pub fn describe(self) -> &'static str {
        match self {
            Objective::Latency => "how long a typical run takes: the median run, decomposed",
            Objective::Tail => {
                "how long the slowest runs take: what the slowest run spent that the median did not"
            }
            Objective::Throughput => {
                "how much work gets done per second: the resource the work queues for"
            }
            Objective::Energy => {
                "how much energy the work costs: what burns the energy this machine meters"
            }
        }
    }
}

impl fmt::Display for Objective {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Objective {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Objective::ALL
            .into_iter()
            .find(|objective| objective.as_str() == text)
            .ok_or_else(|| {
                format!(
                    "`{text}` is not an objective. {}",
                    list(&Objective::ALL).trim_end()
                )
            })
    }
}

/// The two question-oriented modes, which is what an objective is checked
/// against: not every method can answer for every objective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// `omn latency`: thread state analysis, *why is this slow*.
    Latency,
    /// `omn saturate`: utilization, saturation and errors per resource.
    Saturate,
}

impl Method {
    pub fn command(self) -> &'static str {
        match self {
            Method::Latency => "omn latency",
            Method::Saturate => "omn saturate",
        }
    }

    /// The objectives this method answers for.
    pub fn answers(self) -> &'static [Objective] {
        match self {
            Method::Latency => &[Objective::Latency, Objective::Tail],
            Method::Saturate => &[Objective::Throughput, Objective::Tail, Objective::Energy],
        }
    }

    /// Why this method cannot answer for `objective`, and which one can.
    fn cannot(self, objective: Objective) -> String {
        match (self, objective) {
            (Method::Latency, Objective::Throughput) => format!(
                "`{}` decomposes where one task's wall clock went, and throughput is not a wall \
                 clock: a program limited by the disk for its latency can be limited by nothing \
                 at all for its throughput, when the disk is idle most of the time it is waited \
                 on. What the work queues for is `omn saturate --objective throughput`",
                self.command()
            ),
            (Method::Latency, Objective::Energy) => format!(
                "`{}` decomposes a wall clock, and a nanosecond spent blocked costs the cpu no \
                 energy while a nanosecond on it does: the rows of a latency are not the rows of \
                 an energy bill. `omn saturate --objective energy` reads the energy counters",
                self.command()
            ),
            (Method::Saturate, Objective::Latency) => format!(
                "`{}` walks resources across every task of a workload, and a latency is one \
                 task's wall clock: a lock one thread waits on and a cpu another thread spins on \
                 are both busy, and adding them up names neither. Where one task's time went is \
                 `omn latency --objective latency`",
                self.command()
            ),
            _ => format!("`{}` does not answer for `{objective}`", self.command()),
        }
    }

    /// The objective a request named, checked against what this method answers
    /// for. No objective is a refusal with the list, never a default.
    pub fn require(self, objective: Option<Objective>) -> Result<Objective, Refusal> {
        let Some(objective) = objective else {
            return Err(Refusal(format!(
                "`{}` needs an objective, and does not pick one: median latency, the tail, \
                 throughput and energy do not name the same culprit, and an answer for the wrong \
                 one is a confident wrong suspect. Say which with --objective:\n{}",
                self.command(),
                list(self.answers())
            )));
        };
        if self.answers().contains(&objective) {
            return Ok(objective);
        }
        Err(Refusal(format!(
            "{}.\n`{}` answers for:\n{}",
            self.cannot(objective),
            self.command(),
            list(self.answers())
        )))
    }
}

/// A triage request that cannot mean anything as asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

/// The objectives, one per line, each with what it means.
pub fn list(objectives: &[Objective]) -> String {
    objectives
        .iter()
        .map(|objective| format!("  {:<11} {}\n", objective.as_str(), objective.describe()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_objective_reads_back_as_itself() {
        for objective in Objective::ALL {
            assert_eq!(objective.as_str().parse::<Objective>(), Ok(objective));
        }
    }

    #[test]
    fn an_unknown_objective_is_refused_with_the_list() {
        let error = "cost".parse::<Objective>().unwrap_err();
        for objective in Objective::ALL {
            assert!(error.contains(objective.as_str()), "{error}");
        }
    }

    #[test]
    fn no_objective_is_refused_with_the_list_rather_than_defaulted() {
        for method in [Method::Latency, Method::Saturate] {
            let refusal = method.require(None).unwrap_err().to_string();
            assert!(refusal.contains("needs an objective"), "{refusal}");
            for objective in method.answers() {
                assert!(refusal.contains(objective.as_str()), "{refusal}");
            }
        }
    }

    #[test]
    fn an_objective_a_method_cannot_answer_names_the_one_that_can() {
        let refusal = Method::Latency
            .require(Some(Objective::Throughput))
            .unwrap_err()
            .to_string();
        assert!(
            refusal.contains("omn saturate --objective throughput"),
            "{refusal}"
        );
        let refusal = Method::Latency
            .require(Some(Objective::Energy))
            .unwrap_err()
            .to_string();
        assert!(
            refusal.contains("omn saturate --objective energy"),
            "{refusal}"
        );
        let refusal = Method::Saturate
            .require(Some(Objective::Latency))
            .unwrap_err()
            .to_string();
        assert!(
            refusal.contains("omn latency --objective latency"),
            "{refusal}"
        );
    }

    #[test]
    fn each_method_accepts_what_it_lists() {
        for method in [Method::Latency, Method::Saturate] {
            for objective in method.answers() {
                assert_eq!(method.require(Some(*objective)), Ok(*objective));
            }
        }
    }
}
