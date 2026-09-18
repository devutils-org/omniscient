//! The resolver: from a request to a decision.
//!
//! A `Request` says what the user wants. A `ResolvedRequest` says how it will be
//! obtained on *this* machine, and is the file to keep next to the numbers: two
//! measurements taken through different backends are not comparable, and without
//! this record nobody can tell that they were.
//!
//! The walk down a backend chain is ordinary. What matters is what it refuses to
//! do: it will not select a backend whose requirements it could not check, if a
//! fully verified one exists further down; when it has no choice but to select
//! one, it says so in the output rather than presenting a guess as a fact.

use serde::{Deserialize, Serialize};

use crate::capabilities::{Capabilities, Environment, Probe};
use crate::catalog::{Backend, Catalog, Cost, Entry, TargetSupport};
use crate::error::ParseError;
use crate::request::{Request, Target};

/// A request plus every decision made about it. The lockfile of a measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedRequest {
    pub request: Request,
    /// The backend that will be used.
    pub backend: SelectedBackend,
    /// The whole chain with a verdict each, so a surprising choice can be
    /// traced back to the requirement that caused it.
    pub chain: Vec<ConsideredBackend>,
    /// Where the filter predicates will run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<FilterPlacement>,
    /// What this backend gives up relative to the head of the chain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degradations: Vec<String>,
    /// Requirements that were assumed rather than checked. Empty is the goal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unverified: Vec<String>,
    /// The machine, as it was when this was resolved.
    pub environment: Environment,
}

impl ResolvedRequest {
    /// Whether every requirement of the selected backend was actually checked.
    pub fn fully_verified(&self) -> bool {
        self.unverified.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectedBackend {
    pub id: String,
    pub cost: Cost,
    /// Position in the chain, 1 being the preferred backend.
    pub rank: usize,
    /// True when the user pinned it and the chain was not walked.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    pub requirements: Vec<Requirement>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Requirement {
    pub needs: String,
    pub probe: Probe,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsideredBackend {
    pub id: String,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "verdict", content = "detail")]
pub enum Verdict {
    /// Chosen.
    Selected,
    /// Ruled out, with what was missing.
    Rejected(Vec<String>),
    /// Never looked at: a backend above it was selected.
    NotReached,
}

/// Where the filter predicates end up running.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "placement", content = "reason")]
pub enum FilterPlacement {
    /// Pushed down, so filtered-out events never cost anything.
    Kernel(String),
    /// Evaluated after the fact: every event is paid for, then discarded.
    Userspace(String),
}

/// Why a request could not be resolved at all.
/// `source` is spelled `source_id` because `thiserror` reserves a field named
/// `source` for the underlying error of a chain, which these are not.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ResolveError {
    #[error("`{source_id}` is not in the catalog")]
    UnknownSource { source_id: String },
    #[error("`{source_id}` has no viable backend on this machine:\n{}", .rejections.join("\n"))]
    NoViableBackend {
        source_id: String,
        rejections: Vec<String>,
    },
    #[error("`{source_id}` has no backend `{backend}`")]
    UnknownBackend { source_id: String, backend: String },
}

impl From<ResolveError> for ParseError {
    fn from(error: ResolveError) -> Self {
        ParseError::new(error.to_string())
    }
}

/// What a source can do on this machine, ignoring any particular request.
///
/// This is what `omn list` projects. It is weaker than resolving a request --
/// it knows nothing about the target, which can rule backends out on its own --
/// but it is enough to tell a source that is out of reach here from one that is
/// merely unwritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "availability", content = "backend")]
pub enum Availability {
    /// A backend whose every requirement was checked and found.
    Ready(String),
    /// The best backend has requirements no probe covers yet.
    Unverified(String),
    /// Every backend is missing something on this machine.
    Unavailable,
}

impl Availability {
    pub fn marker(&self) -> &'static str {
        match self {
            Availability::Ready(_) => "yes",
            Availability::Unverified(_) => "?",
            Availability::Unavailable => "no",
        }
    }
}

/// Asks what a source could do here, without a request to pin it down.
pub fn availability(entry: &Entry, capabilities: &Capabilities) -> Availability {
    let mut unverified = None;
    for backend in &entry.backends {
        let requirements = probe_all(capabilities, backend, None);
        if requirements
            .iter()
            .any(|requirement| requirement.probe.is_missing())
        {
            continue;
        }
        if requirements
            .iter()
            .all(|requirement| requirement.probe.is_available())
        {
            return Availability::Ready(backend.id.clone());
        }
        unverified.get_or_insert_with(|| backend.id.clone());
    }
    match unverified {
        Some(id) => Availability::Unverified(id),
        None => Availability::Unavailable,
    }
}

/// Decides how a request will be obtained on this machine.
pub fn resolve(
    catalog: &Catalog,
    capabilities: &Capabilities,
    request: &Request,
) -> Result<ResolvedRequest, ResolveError> {
    let entry = catalog
        .get(&request.source)
        .ok_or_else(|| ResolveError::UnknownSource {
            source_id: request.source.to_string(),
        })?;

    let pid = first_pid(&request.target);

    if let Some(pinned) = &request.backend {
        let backend =
            entry
                .backend(pinned.as_str())
                .ok_or_else(|| ResolveError::UnknownBackend {
                    source_id: request.source.to_string(),
                    backend: pinned.as_str().to_owned(),
                })?;
        let rank = entry
            .backends
            .iter()
            .position(|candidate| candidate.id == backend.id)
            .unwrap_or(0)
            + 1;
        let requirements = probe_all(capabilities, backend, pid);
        return Ok(build(
            entry,
            request,
            backend,
            rank,
            true,
            requirements,
            capabilities,
        ));
    }

    // A target the catalog marks `bpf_only` can only be filtered inside an eBPF
    // program, so the chain's non-eBPF backends cannot serve it whatever else
    // they can do.
    let needs_bpf = entry.target_support(request.target.kind()) == TargetSupport::BpfOnly;

    let mut chain = Vec::new();
    let mut unverified_candidate: Option<(usize, &Backend, Vec<Requirement>)> = None;

    for (index, backend) in entry.backends.iter().enumerate() {
        if needs_bpf && !backend.id.starts_with("bpf") {
            chain.push(ConsideredBackend {
                id: backend.id.clone(),
                verdict: Verdict::Rejected(vec![format!(
                    "a {} target needs in-program filtering, which `{}` cannot do",
                    request.target.kind(),
                    backend.id
                )]),
            });
            continue;
        }

        let requirements = probe_all(capabilities, backend, pid);
        let missing: Vec<String> = requirements
            .iter()
            .filter(|requirement| requirement.probe.is_missing())
            .map(|requirement| format!("{}: {}", requirement.needs, requirement.probe))
            .collect();

        if !missing.is_empty() {
            chain.push(ConsideredBackend {
                id: backend.id.clone(),
                verdict: Verdict::Rejected(missing),
            });
            continue;
        }

        let all_checked = requirements
            .iter()
            .all(|requirement| requirement.probe.is_available());
        if all_checked {
            let mut resolved = build(
                entry,
                request,
                backend,
                index + 1,
                false,
                requirements,
                capabilities,
            );
            resolved.chain = finish_chain(chain, &backend.id, entry, index);
            return Ok(resolved);
        }

        // Nothing is missing, but something could not be checked. Keep looking
        // for a backend that can be verified; come back to this one only if
        // none exists.
        if unverified_candidate.is_none() {
            unverified_candidate = Some((index, backend, requirements));
        }
        chain.push(ConsideredBackend {
            id: backend.id.clone(),
            verdict: Verdict::Rejected(vec![
                "held back: some requirements could not be checked".into()
            ]),
        });
    }

    if let Some((index, backend, requirements)) = unverified_candidate {
        let mut resolved = build(
            entry,
            request,
            backend,
            index + 1,
            false,
            requirements,
            capabilities,
        );
        resolved.chain = chain
            .into_iter()
            .map(|considered| {
                if considered.id == backend.id {
                    ConsideredBackend {
                        id: considered.id,
                        verdict: Verdict::Selected,
                    }
                } else {
                    considered
                }
            })
            .collect();
        return Ok(resolved);
    }

    Err(ResolveError::NoViableBackend {
        source_id: request.source.to_string(),
        rejections: chain
            .into_iter()
            .map(|considered| match considered.verdict {
                Verdict::Rejected(reasons) => {
                    format!("  {}: {}", considered.id, reasons.join("; "))
                }
                _ => format!("  {}", considered.id),
            })
            .collect(),
    })
}

fn finish_chain(
    mut chain: Vec<ConsideredBackend>,
    selected: &str,
    entry: &Entry,
    selected_index: usize,
) -> Vec<ConsideredBackend> {
    chain.push(ConsideredBackend {
        id: selected.to_owned(),
        verdict: Verdict::Selected,
    });
    for backend in entry.backends.iter().skip(selected_index + 1) {
        chain.push(ConsideredBackend {
            id: backend.id.clone(),
            verdict: Verdict::NotReached,
        });
    }
    chain
}

fn build(
    entry: &Entry,
    request: &Request,
    backend: &Backend,
    rank: usize,
    pinned: bool,
    requirements: Vec<Requirement>,
    capabilities: &Capabilities,
) -> ResolvedRequest {
    let unverified: Vec<String> = requirements
        .iter()
        .filter(|requirement| requirement.probe.is_unknown())
        .map(|requirement| requirement.needs.clone())
        .collect();

    let mut degradations = Vec::new();
    if let Some(degraded) = &backend.degraded {
        degradations.push(format!("{}: {degraded}", backend.id));
    }
    if pinned {
        degradations.push(format!(
            "backend `{}` was pinned, so the chain was not walked: this measures that backend, not what the resolver would choose",
            backend.id
        ));
    }

    ResolvedRequest {
        request: request.clone(),
        backend: SelectedBackend {
            id: backend.id.clone(),
            cost: backend.cost,
            rank,
            pinned,
            requirements,
        },
        chain: vec![ConsideredBackend {
            id: backend.id.clone(),
            verdict: Verdict::Selected,
        }],
        filter: request.filter.as_ref().map(|_| placement(backend, entry)),
        degradations,
        unverified,
        environment: capabilities.environment().clone(),
    }
}

/// Decides where a filter runs, from what the backend can do.
fn placement(backend: &Backend, entry: &Entry) -> FilterPlacement {
    if backend.id.starts_with("bpf") {
        FilterPlacement::Kernel(format!(
            "`{}` evaluates predicates in the program, so filtered-out events are never paid for",
            backend.id
        ))
    } else {
        FilterPlacement::Userspace(format!(
            "`{}` returns whole records, so `{}` is filtered after reading: every event costs its full price before being discarded",
            backend.id, entry.id
        ))
    }
}

fn probe_all(capabilities: &Capabilities, backend: &Backend, pid: Option<i32>) -> Vec<Requirement> {
    backend
        .requires
        .iter()
        .map(|needs| Requirement {
            needs: needs.clone(),
            probe: capabilities.probe(needs, pid),
        })
        .collect()
}

/// The pid a `/proc/[pid]/...` requirement should be checked against.
fn first_pid(target: &Target) -> Option<i32> {
    match target {
        Target::Pid(pids) | Target::Tid(pids) => pids.first().copied(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{Mode, SourceId};

    fn fixture(source: &str, target: Target) -> Request {
        Request {
            source: source.parse::<SourceId>().unwrap(),
            target,
            mode: Mode::Poll {
                interval: Mode::DEFAULT_POLL_INTERVAL,
            },
            emit: vec!["timeline".parse().unwrap()],
            keys: Vec::new(),
            filter: None,
            duration: None,
            budget: None,
            stack: None,
            backend: None,
            label: None,
        }
    }

    #[test]
    fn a_readable_source_resolves_to_its_preferred_backend() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = Capabilities::detect();
        let own_pid = std::process::id() as i32;
        let resolved = resolve(
            &catalog,
            &capabilities,
            &fixture("mem.rss", Target::Pid(vec![own_pid])),
        )
        .unwrap();
        assert_eq!(resolved.backend.rank, 1);
        assert!(resolved.fully_verified(), "{:?}", resolved.unverified);
        assert!(resolved.degradations.is_empty());
    }

    #[test]
    fn the_chain_records_why_each_backend_was_passed_over() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = Capabilities::detect();
        let resolved = resolve(
            &catalog,
            &capabilities,
            &fixture("mem.rss", Target::Pid(vec![1])),
        )
        .unwrap();
        assert_eq!(
            resolved.chain.len(),
            2,
            "both backends must appear in the record"
        );
        // Whatever was chosen, every entry of the chain carries a verdict.
        assert!(resolved
            .chain
            .iter()
            .any(|considered| considered.verdict == Verdict::Selected));
    }

    #[test]
    fn a_backend_whose_requirements_cannot_be_checked_is_reported_as_unverified() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = Capabilities::detect();
        // The uncore counter is answered now; the RDT fallback behind it is
        // not, so the resolver falls to a backend it could not check and has to
        // say it assumed rather than checked.
        let mut request = fixture("mem.bandwidth", Target::System);
        request.mode = Mode::Count;
        request.emit = vec!["rate".parse().unwrap()];
        let resolved = resolve(&catalog, &capabilities, &request).unwrap();
        assert!(!resolved.fully_verified());
        assert!(
            !resolved.unverified.is_empty(),
            "a backend selected with unchecked requirements has to list them"
        );
    }

    #[test]
    fn a_filter_on_a_procfs_backend_is_reported_as_running_in_userspace() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = Capabilities::detect();
        let mut request = fixture("mem.rss", Target::Pid(vec![std::process::id() as i32]));
        request.filter = Some("comm =~ 'omn'".parse().unwrap());
        let resolved = resolve(&catalog, &capabilities, &request).unwrap();
        assert!(
            matches!(resolved.filter, Some(FilterPlacement::Userspace(_))),
            "{:?}",
            resolved.filter
        );
    }

    /// Pinning exists for the comparison bench, which forces every backend of a
    /// chain in turn. It bypasses the resolver, and a result obtained that way
    /// is not the result the tool would have produced -- so the record has to
    /// say both that it was pinned and that the chain was never walked.
    #[test]
    fn a_pinned_backend_bypasses_the_chain_and_the_record_says_so() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = Capabilities::detect();
        let mut request = fixture("mem.rss", Target::Pid(vec![std::process::id() as i32]));
        // The second backend of the chain, which the resolver would not pick.
        request.backend = Some("force:fs:proc:statm".parse().unwrap());

        let resolved = resolve(&catalog, &capabilities, &request).unwrap();
        assert_eq!(resolved.backend.id, "fs:proc:statm");
        assert_eq!(resolved.backend.rank, 2, "it is not the preferred backend");
        assert!(resolved.backend.pinned);
        assert!(
            resolved
                .degradations
                .iter()
                .any(|degradation| degradation.contains("chain was not walked")),
            "{:?}",
            resolved.degradations
        );
        // Its own degradation is reported too, not replaced by the pin notice.
        assert!(
            resolved
                .degradations
                .iter()
                .any(|degradation| degradation.contains("resident pages only")),
            "{:?}",
            resolved.degradations
        );
    }

    #[test]
    fn pinning_a_backend_the_source_does_not_have_fails_by_name() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = Capabilities::detect();
        let mut request = fixture("mem.rss", Target::Pid(vec![1]));
        request.backend = Some("force:bpf:tp".parse().unwrap());
        match resolve(&catalog, &capabilities, &request) {
            Err(ResolveError::UnknownBackend { backend, .. }) => assert_eq!(backend, "bpf:tp"),
            other => panic!("expected the pin to be refused, got {other:?}"),
        }
    }

    /// A target the catalog marks `bpf_only` can only be filtered inside an
    /// eBPF program, so the chain's other backends cannot serve it whatever
    /// else they can do. The rejection has to name that, not a missing file.
    #[test]
    fn a_bpf_only_target_rules_out_the_backends_that_cannot_filter_in_program() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = Capabilities::detect();
        let mut request = fixture("io.latency", Target::Pid(vec![1]));
        request.mode = Mode::Trace;
        request.emit = vec!["hist".parse().unwrap()];

        match resolve(&catalog, &capabilities, &request) {
            Ok(resolved) => assert!(
                resolved.backend.id.starts_with("bpf"),
                "a pid target on io.latency needs in-program filtering, got `{}`",
                resolved.backend.id
            ),
            Err(ResolveError::NoViableBackend { rejections, .. }) => assert!(
                rejections
                    .iter()
                    .any(|rejection| rejection.contains("in-program filtering")),
                "{rejections:?}"
            ),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn a_resolved_request_survives_a_round_trip() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = Capabilities::detect();
        let resolved = resolve(
            &catalog,
            &capabilities,
            &fixture("mem.rss", Target::Pid(vec![std::process::id() as i32])),
        )
        .unwrap();
        let json = serde_json::to_string(&resolved).unwrap();
        assert_eq!(
            serde_json::from_str::<ResolvedRequest>(&json).unwrap(),
            resolved
        );
    }
}
