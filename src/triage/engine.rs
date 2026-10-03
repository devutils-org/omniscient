//! The engine, called the way any other user calls it.
//!
//! A triage method writes a [`Session`] and hands it here; what comes back is
//! the same [`RunOutput`] `omn -f` would have printed, and the merged events
//! the joins read. Nothing in this file reaches under the library calls
//! `main.rs` makes -- validate, resolve, collect, aggregate, join, decompose --
//! so a triage answer is never computed from anything a session file could not
//! also have asked for, and `--dump-session` prints that file.

use std::fmt;

use crate::capabilities::Capabilities;
use crate::catalog::{Catalog, Unit};
use crate::collect::{self, Whose};
use crate::emit;
use crate::encode::RunOutput;
use crate::event::Event;
use crate::join;
use crate::request::{Request, Session};
use crate::resolve::{self, ResolvedRequest};
use crate::validate;

/// Why a session a method wrote could not be run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Whose gap it is, which is what the exit code is derived from.
    pub whose: Whose,
    pub message: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

impl Failure {
    fn request(message: impl Into<String>) -> Self {
        Self {
            whose: Whose::Request,
            message: message.into(),
        }
    }
}

/// A request a method would like and can do without, and why it was left out.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Dropped {
    pub label: String,
    pub source: String,
    pub reason: String,
}

/// One session, run.
#[derive(Debug, Clone)]
pub struct Run {
    pub output: RunOutput,
    /// Every request's events in one stream, as the joins read them.
    pub events: Vec<Event>,
    /// The resolved request that served each result, in the same order.
    pub served: Vec<ResolvedRequest>,
    /// What the validator had to say that is not a refusal.
    pub warnings: Vec<String>,
    pub caveats: Vec<String>,
    /// What each request cost the target, in the order of `served`.
    pub costs: Vec<Cost>,
}

/// What one request cost, as the runner measured it: the collectors' cpu and
/// whatever they spent inside the target, against the target's own cpu.
#[derive(Debug, Clone, PartialEq)]
pub struct Cost {
    pub label: String,
    pub source: String,
    pub backend: String,
    /// The band the catalog declares for the backend that served.
    pub declared: crate::catalog::Cost,
    /// Of the target's cpu. Absent when the target's cpu was not measured.
    pub percent: Option<f64>,
    /// Which account the target's cpu was read from.
    pub observed_from: String,
}

impl Run {
    /// The result of the request labelled `label`.
    pub fn result(&self, label: &str) -> Option<&emit::Results> {
        self.output
            .results
            .iter()
            .find(|results| results.label == label)
    }

    /// The events of one source, in time order.
    pub fn events_of<'a>(&'a self, source: &'a str) -> impl Iterator<Item = &'a Event> + 'a {
        self.events.iter().filter(move |event| event.kind == source)
    }
}

/// Resolves every request of `session`, and leaves out the ones named in
/// `optional` that this machine or this build cannot serve, saying why.
///
/// A required request that cannot be served fails the whole session: a method
/// whose inputs are not there has no answer to give, and a partial one would
/// read as a complete one.
pub fn prune(
    catalog: &Catalog,
    capabilities: &Capabilities,
    session: &mut Session,
    optional: &[&str],
) -> Result<Vec<Dropped>, Failure> {
    let mut dropped = Vec::new();
    let mut kept = Vec::new();
    for request in session.requests.drain(..) {
        let label = label_of(&request);
        let reason = match resolve::resolve(catalog, capabilities, &request) {
            Ok(resolved) if collect::implements(request.source.as_str(), &resolved.backend.id) => {
                None
            }
            Ok(resolved) => Some((
                Whose::Build,
                format!(
                    "this build has no collector for `{}` through `{}`",
                    request.source, resolved.backend.id
                ),
            )),
            Err(error) => {
                let whose = match &error {
                    resolve::ResolveError::NoViableBackend { .. } => Whose::Machine,
                    // A target and a mode no one backend declares together
                    // is the request's, as `omn` answers it (exit 2).
                    resolve::ResolveError::NoBackendDeclares { .. } => Whose::Request,
                    resolve::ResolveError::KeyNotCarried {
                        elsewhere: Some(elsewhere),
                        ..
                    } => elsewhere.whose(),
                    resolve::ResolveError::FilterFieldNotCarried {
                        elsewhere: Some(elsewhere),
                        ..
                    } => elsewhere.whose(),
                    // No backend attaches the key or carries the field: the
                    // request's to change, as `omn` answers it (exit 2).
                    resolve::ResolveError::KeyNotCarried { .. }
                    | resolve::ResolveError::FilterFieldNotCarried { .. } => Whose::Request,
                    resolve::ResolveError::StackRefused { whose, .. } => *whose,
                    // A source the catalog does not carry is a catalog this
                    // build was not given, not a request anyone wrote wrong.
                    _ => Whose::Build,
                };
                Some((whose, error.to_string()))
            }
        };
        match reason {
            None => kept.push(request),
            Some((_, reason)) if optional.contains(&label.as_str()) => dropped.push(Dropped {
                label,
                source: request.source.to_string(),
                reason,
            }),
            Some((whose, reason)) => {
                return Err(Failure {
                    whose,
                    message: format!(
                        "`{label}` ({}) is an input this method cannot do without, and: {reason}",
                        request.source
                    ),
                })
            }
        }
    }
    session.requests = kept;
    Ok(dropped)
}

fn label_of(request: &Request) -> String {
    request
        .label
        .clone()
        .unwrap_or_else(|| request.source.to_string())
}

/// Validates, resolves and runs `session`, and reads what it produced.
///
/// The run is held to no budget: a triage run pays for the sources its method
/// needs, and what they cost is measured and carried on every result as it is
/// for any run.
pub fn run(
    catalog: &Catalog,
    capabilities: &Capabilities,
    session: &Session,
) -> Result<Run, Failure> {
    let report = validate::validate_session(catalog, session).map_err(|rejections| {
        Failure::request(
            rejections
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    })?;

    let mut resolved = Vec::new();
    for request in &session.requests {
        match resolve::resolve(catalog, capabilities, request) {
            Ok(one) => resolved.push(one),
            Err(error) => {
                let whose = match &error {
                    resolve::ResolveError::NoViableBackend { .. } => Whose::Machine,
                    resolve::ResolveError::KeyNotCarried {
                        elsewhere: Some(elsewhere),
                        ..
                    } => elsewhere.whose(),
                    resolve::ResolveError::FilterFieldNotCarried {
                        elsewhere: Some(elsewhere),
                        ..
                    } => elsewhere.whose(),
                    resolve::ResolveError::StackRefused { whose, .. } => *whose,
                    _ => Whose::Request,
                };
                return Err(Failure {
                    whose,
                    message: error.to_string(),
                });
            }
        }
    }

    let collections = collect::run_all_within(&resolved, &mut collect::StopWhenOverBudget)
        .map_err(|error| Failure {
            whose: error.whose(),
            message: error.to_string(),
        })?;

    let mut output = RunOutput {
        results: Vec::new(),
        joins: Vec::new(),
        decompositions: Vec::new(),
    };
    let mut events = Vec::new();
    let mut costs = Vec::new();
    for (serving, collection) in resolved.iter().zip(collections) {
        costs.push(Cost {
            label: label_of(&serving.request),
            source: serving.request.source.to_string(),
            backend: serving.backend.id.clone(),
            declared: serving.backend.cost,
            percent: collection.overhead.percent(),
            observed_from: collection.overhead.observed_from.clone(),
        });
        events.extend(collection.events.iter().cloned());
        let unit = catalog
            .get(&serving.request.source)
            .map(|entry| entry.backend_unit(&serving.backend.id))
            .unwrap_or(Unit::Count);
        let value_attr = collection.value_attr.clone();
        let results = emit::aggregate(serving, collection, unit, &value_attr)
            .map_err(|error| Failure::request(error.to_string()))?;
        output.results.push(results);
    }

    for rule in &session.joins {
        output
            .joins
            .push(join::apply(rule, &events).map_err(|error| Failure::request(error.to_string()))?);
    }
    let requests: Vec<Request> = resolved.iter().map(|one| one.request.clone()).collect();
    for decomposition in &session.decompose {
        // `join::apply` watches everything, so no window closes early.
        output.decompositions.push(join::decompose_within(
            decomposition,
            &requests,
            &output.results,
            &events,
            &session.joins,
            &std::collections::BTreeMap::new(),
        ));
    }

    Ok(Run {
        output,
        events,
        served: resolved,
        warnings: report.warnings().map(str::to_owned).collect(),
        caveats: report.caveats().map(str::to_owned).collect(),
        costs,
    })
}
