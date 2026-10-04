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

use std::fmt;

use crate::capabilities::{Capabilities, Environment, Probe};
use crate::catalog::{Backend, Catalog, Cost, Entry, TargetSupport};
use crate::error::{one_of, ParseError};
use crate::filter::FilterExpr;
use crate::request::{KeyKind, ModeKind, Request, Target, TargetKind};

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
    /// How the stacks will be unwound and where their names come from, for a
    /// source whose collector unwinds through [`crate::stack::choose`]: the
    /// mode, whether it was forced, every mode considered with its verdict,
    /// and what it degrades -- said before the run, the way the backend is.
    /// Absent for every other source. A choice that is refused is not absent:
    /// the request is refused, [`ResolveError::StackRefused`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<crate::stack::StackChoice>,
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
    /// This machine can serve it and this build cannot: no collector is written
    /// for it yet. Passed over in favour of one that is, which is what keeps a
    /// newly granted privilege from taking a working source away.
    Unimplemented,
    /// Ruled out, with what was missing.
    Rejected(Vec<String>),
    /// Never looked at: a backend above it was selected.
    NotReached,
}

/// Where the filter predicates end up running.
///
/// The two variants are what the runner reads: `Userspace` is a predicate it
/// applies itself, over the events a collector handed up, and `Kernel` is one
/// it leaves alone because it was applied before the events reached it.
/// *Where* that was is [`Placed::by`], and it is what the resolved file says:
/// `"placement": "kernel"` only when the program itself evaluated every part
/// of the predicate ([`AppliedBy::Program`]), and `"userspace"` both when the
/// collector applied it to what the program delivered
/// ([`AppliedBy::Collector`]) and when the runner applies it after reading
/// ([`AppliedBy::Runner`]). Until the twelfth wave every `bpf` backend was
/// written down as `kernel`, and one comparison of one source ran there.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterPlacement {
    /// Applied before the runner sees an event.
    Kernel(Placed),
    /// Evaluated by the runner after the fact: every event is paid for, then
    /// discarded.
    Userspace(Placed),
}

impl FilterPlacement {
    /// Where the predicate is evaluated, and why.
    pub fn placed(&self) -> &Placed {
        match self {
            FilterPlacement::Kernel(placed) | FilterPlacement::Userspace(placed) => placed,
        }
    }

    /// Whether every part of the predicate ran in the kernel, so that an event
    /// it rejected was never paid for.
    pub fn in_program(&self) -> bool {
        self.placed().by == AppliedBy::Program
    }
}

/// Who evaluated a predicate, and the sentence that says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub by: AppliedBy,
    pub reason: String,
}

/// The three places a predicate can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppliedBy {
    /// Compared in the program: a rejected event is dropped in the kernel.
    Program,
    /// Applied by the collector to what the program delivered: every event it
    /// removes was delivered, and paid for, first.
    Collector,
    /// Applied by the runner to the events a collector handed up.
    Runner,
}

/// The sentence alone: the rendering names who applied the predicate in its
/// own heading, from `by`.
impl fmt::Display for Placed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

/// The resolved file's shape: `placement` is where the cost was paid, and
/// `applied_by` who applied it.
#[derive(Serialize, Deserialize)]
struct PlacementRecord {
    placement: PlacementKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    applied_by: Option<AppliedBy>,
    reason: String,
}

#[derive(Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum PlacementKind {
    Kernel,
    Userspace,
}

impl Serialize for FilterPlacement {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let placed = self.placed();
        PlacementRecord {
            placement: if placed.by == AppliedBy::Program {
                PlacementKind::Kernel
            } else {
                PlacementKind::Userspace
            },
            applied_by: Some(placed.by),
            reason: placed.reason.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FilterPlacement {
    /// A file written before `applied_by` existed said `kernel` for every
    /// `bpf` backend, and its runner left those events alone; it is read back
    /// as the program's, which keeps that behaviour, and as nothing more.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let record = PlacementRecord::deserialize(deserializer)?;
        let by = record.applied_by.unwrap_or(match record.placement {
            PlacementKind::Kernel => AppliedBy::Program,
            PlacementKind::Userspace => AppliedBy::Runner,
        });
        let placed = Placed {
            by,
            reason: record.reason,
        };
        Ok(match by {
            AppliedBy::Runner => FilterPlacement::Userspace(placed),
            AppliedBy::Program | AppliedBy::Collector => FilterPlacement::Kernel(placed),
        })
    }
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
    /// Every backend of the chain was ruled out by what it declares, not by
    /// what this machine has: the entry serves the target and the mode, and
    /// no one backend serves the two together. No machine could serve it, so
    /// it is the request's to change -- `sched.switches --cgroup -m trace`,
    /// where the one backend that serves a cgroup counts and does not trace.
    #[error(
        "`{source_id}` cannot serve a `{target}` target in `{mode}` mode: no backend of its \
         chain declares the two together:\n{}",
        .rejections.join("\n")
    )]
    NoBackendDeclares {
        source_id: String,
        target: TargetKind,
        mode: ModeKind,
        rejections: Vec<String>,
    },
    #[error("`{source_id}` has no backend `{backend}`")]
    UnknownBackend { source_id: String, backend: String },
    /// Whose gap it is follows from [`KeyElsewhere`], as for a key: the
    /// first backend that carries the field is named only for what stands
    /// between the request and it, and a `--backend force:` hint is given
    /// only for one this build collects and this machine can run.
    #[error(
        "`{source_id}` is filtered on `{field}`, which the selected backend `{backend}` does not \
         carry:\n  {}",
        .elsewhere.as_ref().map_or_else(
            || format!("no backend of `{source_id}` carries it"),
            |elsewhere| elsewhere.to_string(),
        )
    )]
    FilterFieldNotCarried {
        source_id: String,
        backend: String,
        field: String,
        /// The backend of the chain that does carry it, and whether it can be
        /// asked for here; `None` when no backend carries it. Boxed: it is
        /// the largest of the errors, and every `Result` here carries one.
        elsewhere: Option<Box<KeyElsewhere>>,
    },
    /// The twin of `FilterFieldNotCarried`, for a key: `--keys task` on a
    /// backend that attaches no task would produce events that join to
    /// nothing, which is refused rather than recorded.
    ///
    /// Whose gap it is follows from [`KeyElsewhere`]: a backend the request
    /// can be pinned to makes it the request's, one only the machine keeps out
    /// makes it the machine's, and one nothing collects in this build makes it
    /// the build's -- a `--backend force:` hint that the next run refuses as
    /// unwritten is not a hint.
    #[error(
        "`{source_id}` is keyed on `{key}`, which the selected backend `{backend}` does not \
         attach:\n  {}",
        .elsewhere.as_ref().map_or_else(
            || format!("no backend of `{source_id}` attaches it"),
            |elsewhere| elsewhere.to_string(),
        )
    )]
    KeyNotCarried {
        source_id: String,
        backend: String,
        key: KeyKind,
        /// The backend of the chain that does attach it, and whether it can be
        /// asked for here; `None` when no backend attaches it.
        elsewhere: Option<KeyElsewhere>,
    },
    /// The selected backend unwinds through [`crate::stack::choose`], and
    /// the choice is refused: a forced `--stack` mode this machine lacks what
    /// it needs for or this build does not read, `--stack auto` with nothing
    /// to pick, `--symbols debuginfod` with no server named. Refused here
    /// rather than left out of the resolved file, so that `--dump-resolved`
    /// refuses what the run would, with the same reason and the same code.
    #[error("`{source_id}` on `{backend}` cannot unwind as asked: {reason}")]
    StackRefused {
        source_id: String,
        backend: String,
        reason: String,
        whose: crate::collect::Whose,
    },
}

/// A backend of the chain that attaches a key, or carries a filter field, the
/// selected one does not, and what stands between the request and it.
///
/// The first backend that attaches the key is not necessarily one the request
/// can be moved to: it may serve neither the target nor the mode, this machine
/// may lack what it needs, or this build may have no collector for it. Naming
/// it in a `--backend force:` hint regardless sends the reader to a second
/// refusal. The resolver picks, in chain order, the first backend of the best
/// kind: one that can be forced, then one only the machine keeps out, then one
/// only the build does, then one the request's target or mode rules out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyElsewhere {
    /// Attaches the key, serves the target and mode, is collected by this
    /// build, and nothing it needs is missing here: pinning it is the fix.
    Forceable(String),
    /// Attaches the key, but this machine lacks something it needs, whether
    /// or not this build collects it.
    Missing { backend: String, reason: String },
    /// Attaches the key, nothing it needs is missing here, and this build has
    /// no collector for it.
    Unwritten(String),
    /// Attaches the key, and cannot serve the request's target or mode.
    Unfit { backend: String, reason: String },
}

impl KeyElsewhere {
    /// Whose gap the refusal is, per [`crate::collect::Whose`].
    pub fn whose(&self) -> crate::collect::Whose {
        use crate::collect::Whose;
        match self {
            KeyElsewhere::Forceable(_) | KeyElsewhere::Unfit { .. } => Whose::Request,
            KeyElsewhere::Missing { .. } => Whose::Machine,
            KeyElsewhere::Unwritten(_) => Whose::Build,
        }
    }

    /// Which carrier the hint names, lowest first, ties going to the chain's
    /// order. A carrier this machine can serve and nobody wrote outranks one
    /// the machine cannot serve, as on the walk: the walk passes over a
    /// backend that lacks something here and stops at the first one this
    /// machine could run, so its answer is the build's whenever such a
    /// backend exists, and the hint gives the same one (`DECISIONS.md`, "one
    /// order for unbuilt and missing"). A missing carrier may be unwritten as
    /// well, so naming it first would send a reader after privileges for a
    /// collector nobody wrote.
    fn rank(&self) -> u8 {
        match self {
            KeyElsewhere::Forceable(_) => 0,
            KeyElsewhere::Unwritten(_) => 1,
            KeyElsewhere::Missing { .. } => 2,
            KeyElsewhere::Unfit { .. } => 3,
        }
    }
}

impl fmt::Display for KeyElsewhere {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyElsewhere::Forceable(id) => {
                write!(
                    f,
                    "`{id}` does, and `--backend force:{id}` is how to ask for it"
                )
            }
            KeyElsewhere::Missing { backend, reason } => write!(
                f,
                "`{backend}` does, and this machine cannot run it: {reason}"
            ),
            KeyElsewhere::Unwritten(id) => {
                write!(f, "`{id}` does, and this build has no collector for it yet")
            }
            KeyElsewhere::Unfit { backend, reason } => {
                write!(
                    f,
                    "`{backend}` does, and cannot serve this request: {reason}"
                )
            }
        }
    }
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
///
/// Only a backend this build collects counts: *available means it attaches*,
/// and a backend with no collector attaches nowhere. The resolver passes such a
/// backend over for the next one that is written (`Verdict::Unimplemented`),
/// and the listing has to agree with it -- `cpu.cycles` on a cpu with no
/// hardware PMU read "usable through `pmu:sw`", the one backend of its chain
/// nobody wrote, and every run of it was refused (the CI's second run). Such a
/// source is out of reach here, and the listing's backend lines say which
/// requirement is missing and which backend has no collector.
pub fn availability(entry: &Entry, capabilities: &Capabilities) -> Availability {
    let mut unverified = None;
    for backend in &entry.backends {
        if !crate::collect::implements(entry.id.as_str(), &backend.id) {
            continue;
        }
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
        // A pin skips the walk, not the checks. A pinned backend that cannot
        // serve the target, or whose requirements probe missing, would be
        // accepted here, launch the target, and fail at attach -- finding 15's
        // shape reached through the pin instead of the probe. It is refused
        // now, before anything runs, with the probes' own reasons. A
        // requirement no probe covers is still accepted, as it is on the walk,
        // and the resolved file lists it as unverified.
        //
        // A backend both unwritten in this build and missing something here
        // is answered in the chain's order: the machine first, as the walk
        // does (`DECISIONS.md`, "one order for unbuilt and missing"). One
        // request then exits the same way whichever path reached it -- the
        // walk exited 1 and the pin 3 for `net.hwts` on a runner with no
        // readable PTP clock -- and on that machine no build would serve it
        // either. A pinned backend this build has no collector for and that
        // lacks nothing here is left to the runner, which refuses it before
        // the launch as the build's gap.
        let refuse = |reasons: Vec<String>| ResolveError::NoViableBackend {
            source_id: request.source.to_string(),
            rejections: vec![format!(
                "  {} (pinned with --backend force:, so no other backend was considered): {}",
                backend.id,
                reasons.join("; ")
            )],
        };
        if let Some(reason) = cannot_serve_target(entry, backend, request.target.kind()) {
            return Err(refuse(vec![reason]));
        }
        if let Some(reason) = cannot_serve_mode(entry, backend, request.mode.kind()) {
            return Err(refuse(vec![reason]));
        }
        check_filter_fields(entry, backend, request, capabilities)?;
        check_keys_carried(entry, backend, request, capabilities)?;
        let requirements = probe_all(capabilities, backend, Some(&request.target));
        let missing: Vec<String> = requirements
            .iter()
            .filter(|requirement| requirement.probe.is_missing())
            .map(|requirement| format!("{}: {}", requirement.needs, requirement.probe))
            .collect();
        if !missing.is_empty() {
            return Err(refuse(missing));
        }
        return build(
            entry,
            request,
            backend,
            rank,
            true,
            requirements,
            capabilities,
        );
    }

    let mut chain = Vec::new();
    let mut unverified_candidate: Option<(usize, &Backend, Vec<Requirement>)> = None;
    // A backend this machine can serve but this build cannot collect. Kept as a
    // fallback so that a chain with no implemented backend still resolves and
    // still reports "no collector is written", rather than claiming the machine
    // cannot serve it at all.
    let mut unimplemented_candidate: Option<(usize, &Backend, Vec<Requirement>)> = None;

    // Whether some backend of the chain declares the request's target and
    // mode together. None does: no machine could serve it, whatever it had.
    let mut some_backend_fits = false;
    for (index, backend) in entry.backends.iter().enumerate() {
        let unfit = cannot_serve_target(entry, backend, request.target.kind())
            .or_else(|| cannot_serve_mode(entry, backend, request.mode.kind()));
        if let Some(reason) = unfit {
            chain.push(ConsideredBackend {
                id: backend.id.clone(),
                verdict: Verdict::Rejected(vec![reason]),
            });
            continue;
        }
        some_backend_fits = true;

        let requirements = probe_all(capabilities, backend, Some(&request.target));
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
        let implemented = crate::collect::implements(request.source.as_str(), &backend.id);

        if all_checked && implemented {
            check_filter_fields(entry, backend, request, capabilities)?;
            check_keys_carried(entry, backend, request, capabilities)?;
            let mut resolved = build(
                entry,
                request,
                backend,
                index + 1,
                false,
                requirements,
                capabilities,
            )?;
            resolved.chain = finish_chain(chain, &backend.id, entry, index);
            return Ok(resolved);
        }

        if all_checked {
            // Viable here, unwritten in this build.
            if unimplemented_candidate.is_none() {
                unimplemented_candidate = Some((index, backend, requirements));
            }
            chain.push(ConsideredBackend {
                id: backend.id.clone(),
                verdict: Verdict::Unimplemented,
            });
            continue;
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

    // An unverified-but-implemented backend beats a verified one nothing can
    // collect: the first might work, the second certainly will not.
    for candidate in [unverified_candidate.take(), unimplemented_candidate.take()] {
        let Some((index, backend, requirements)) = candidate else {
            continue;
        };
        check_filter_fields(entry, backend, request, capabilities)?;
        check_keys_carried(entry, backend, request, capabilities)?;
        let mut resolved = build(
            entry,
            request,
            backend,
            index + 1,
            false,
            requirements,
            capabilities,
        )?;
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

    // Only a pair the entry itself declares, both halves: a mode or a target
    // the entry refuses is the validator's to answer, in its own words,
    // before any backend is looked at.
    let entry_declares = entry.modes.contains(&request.mode.kind())
        && entry.target_support(request.target.kind()) != TargetSupport::No;
    if !some_backend_fits && entry_declares {
        return Err(ResolveError::NoBackendDeclares {
            source_id: request.source.to_string(),
            target: request.target.kind(),
            mode: request.mode.kind(),
            rejections: chain
                .into_iter()
                .map(|considered| match considered.verdict {
                    Verdict::Rejected(reasons) => {
                        format!("  {}: {}", considered.id, reasons.join("; "))
                    }
                    _ => format!("  {}", considered.id),
                })
                .collect(),
        });
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

/// What a pin says beside the numbers: the user chose the backend, and the
/// resolver's choice was never asked.
fn pinned_note(backend: &str) -> String {
    format!(
        "backend `{backend}` was pinned, so the chain was not walked: this measures that \
         backend, not what the resolver would choose"
    )
}

/// Resolves a request onto one backend of its chain that the caller chose,
/// with every check a pin is given, for a caller that is not the user: a
/// budget's ladder stepping onto the next rung.
///
/// The result is not marked pinned and carries no pin's note. The user pinned
/// nothing, and "this measures that backend, not what the resolver would
/// choose" beside a budget's move would report the ladder's choice as theirs.
/// The request keeps whatever `--backend` the user did write.
pub fn resolve_onto(
    catalog: &Catalog,
    capabilities: &Capabilities,
    request: &Request,
    backend: &str,
) -> Result<ResolvedRequest, ResolveError> {
    let mut onto = request.clone();
    onto.backend =
        Some(
            format!("force:{backend}")
                .parse()
                .map_err(|_| ResolveError::UnknownBackend {
                    source_id: request.source.to_string(),
                    backend: backend.to_owned(),
                })?,
        );
    let mut resolved = resolve(catalog, capabilities, &onto)?;
    resolved.request.backend = request.backend.clone();
    resolved.backend.pinned = request
        .backend
        .as_ref()
        .is_some_and(|pinned| pinned.as_str() == backend);
    if !resolved.backend.pinned {
        let note = pinned_note(backend);
        resolved.degradations.retain(|said| *said != note);
    }
    Ok(resolved)
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
) -> Result<ResolvedRequest, ResolveError> {
    let stack = crate::stack::recorded_choice(request, &backend.id, capabilities.environment())
        .map_err(|refusal| ResolveError::StackRefused {
            source_id: request.source.to_string(),
            backend: backend.id.clone(),
            reason: refusal.reason,
            whose: refusal.whose,
        })?;
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
        degradations.push(pinned_note(&backend.id));
    }

    Ok(ResolvedRequest {
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
        filter: request
            .filter
            .as_ref()
            .map(|predicate| placement(backend, entry, predicate)),
        degradations,
        unverified,
        environment: crate::collect::gpu::with_clock_states(
            &entry.family,
            capabilities.environment().clone(),
        ),
        stack,
    })
}

/// Whether a backend of the chain serves the request's target and mode, so
/// that what could still rule it out is the machine's: its requirements.
pub fn serves(entry: &Entry, backend: &Backend, request: &Request) -> bool {
    cannot_serve_target(entry, backend, request.target.kind()).is_none()
        && cannot_serve_mode(entry, backend, request.mode.kind()).is_none()
}

/// Why a backend cannot serve a target, or `None` when it can.
///
/// Two separate refusals. A backend the entry marks `bpf_only` for this target
/// can only do the narrowing inside an eBPF program, so the chain's other
/// backends are out whatever else they can do. A backend whose own `targets`
/// map does not mention the kind cannot attribute at that granularity at all:
/// `/proc/diskstats` knows the machine and `io.stat` knows a cgroup, and
/// letting either answer for the other is the worst bug this tool can have --
/// a number about something else, presented as the one that was asked for.
fn cannot_serve_target(entry: &Entry, backend: &Backend, kind: TargetKind) -> Option<String> {
    match entry.backend_target_support(backend, kind) {
        TargetSupport::Native => None,
        TargetSupport::BpfOnly if backend.id.starts_with("bpf") => None,
        TargetSupport::BpfOnly => Some(format!(
            "a {kind} target needs in-program filtering, which `{}` cannot do",
            backend.id
        )),
        TargetSupport::No => Some(format!(
            "`{}` cannot attribute per {kind}: it would answer with {}",
            backend.id,
            served_instead(entry, backend)
        )),
    }
}

/// What a backend would have answered with, named by the targets it does
/// serve. "it would answer with a system-wide number" is the sentence that
/// makes the refusal worth reading.
fn served_instead(entry: &Entry, backend: &Backend) -> String {
    let serves: Vec<&str> = TargetKind::ALL
        .into_iter()
        .filter(|kind| entry.backend_target_support(backend, *kind) != TargetSupport::No)
        .map(|kind| kind.as_str())
        .collect();
    if serves.is_empty() {
        "nothing this request asked about".to_owned()
    } else {
        format!("a number scoped to {}", one_of(serves.into_iter()))
    }
}

/// Why a backend cannot serve the request's mode, or `None` when it can.
///
/// The entry's `modes` are what the chain can do between them; a backend that
/// declares its own serves only those. Handing a `trace` request to a file
/// that holds a total is P7 of `docs/portability.md`: the collector refused it
/// at attach, after the resolver had walked past the backend that could have
/// traced and the target had been launched.
fn cannot_serve_mode(entry: &Entry, backend: &Backend, mode: ModeKind) -> Option<String> {
    let serves = entry.backend_modes(backend);
    if serves.contains(&mode) {
        return None;
    }
    Some(format!(
        "`{}` serves {} mode, not `{mode}`",
        backend.id,
        one_of(
            serves
                .iter()
                .map(|mode| format!("`{mode}`"))
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str)
        )
    ))
}

/// Refuses a predicate over a field the selected backend never puts on an
/// event.
///
/// The userspace filter runs, so such a predicate is false on every event and
/// the run reports an empty series -- which reads as "nothing matched" when it
/// means "this could never have matched". An invalid combination is refused
/// with the reason, never answered with a plausible emptiness.
///
/// Only fields the entry itself offers are checked here. A field no backend of
/// the source has ever heard of is a question about the request rather than
/// about this machine, so it belongs to the validator, which answers it before
/// anything is probed and with the source's own vocabulary to suggest from.
fn check_filter_fields(
    entry: &Entry,
    backend: &Backend,
    request: &Request,
    capabilities: &Capabilities,
) -> Result<(), ResolveError> {
    let Some(filter) = &request.filter else {
        return Ok(());
    };
    let carried = entry.backend_filter_fields(backend);
    for field in filter.fields() {
        if carried.iter().any(|known| known == field)
            || !entry.filter_fields.iter().any(|known| known == field)
        {
            continue;
        }
        // Picked as a key's is: the first backend of the best kind, so that a
        // pin hint is never given for a backend nothing collects or this
        // machine cannot run -- the next run would refuse it.
        let elsewhere = entry
            .backends_carrying(field)
            .into_iter()
            .filter(|id| *id != backend.id)
            .filter_map(|id| entry.backends.iter().find(|other| other.id == id))
            .map(|other| key_elsewhere(entry, other, request, capabilities))
            .min_by_key(KeyElsewhere::rank);
        return Err(ResolveError::FilterFieldNotCarried {
            source_id: request.source.to_string(),
            backend: backend.id.clone(),
            field: field.to_owned(),
            elsewhere: elsewhere.map(Box::new),
        });
    }
    Ok(())
}

/// Refuses a request keyed on a key the selected backend does not attach.
///
/// The twin of [`check_filter_fields`], over the other list: `--keys task` on
/// `io.bytes` resolved onto `/proc/diskstats` would produce events with no
/// task on them, and a join written over that key would attribute nothing --
/// the expensive silence, found only when the session is joined later. A key
/// the entry itself does not offer is the validator's to refuse, before any
/// backend is chosen, and is let through here.
fn check_keys_carried(
    entry: &Entry,
    backend: &Backend,
    request: &Request,
    capabilities: &Capabilities,
) -> Result<(), ResolveError> {
    let attached = entry.backend_keys(backend);
    for key in &request.keys {
        if attached.contains(key) || !entry.keys.contains(key) {
            continue;
        }
        let elsewhere = entry
            .backends_carrying_key(*key)
            .into_iter()
            .filter_map(|id| entry.backends.iter().find(|other| other.id == id))
            .map(|other| key_elsewhere(entry, other, request, capabilities))
            .min_by_key(KeyElsewhere::rank);
        return Err(ResolveError::KeyNotCarried {
            source_id: request.source.to_string(),
            backend: backend.id.clone(),
            key: *key,
            elsewhere,
        });
    }
    Ok(())
}

/// What stands between the request and a backend that attaches its key. The
/// machine's gap is looked at before the build's, in the chain's order, as the
/// walk and the pinned path do (`DECISIONS.md`, "one order for unbuilt and
/// missing"): a backend this machine cannot serve is out of reach here
/// whoever writes its collector.
fn key_elsewhere(
    entry: &Entry,
    backend: &Backend,
    request: &Request,
    capabilities: &Capabilities,
) -> KeyElsewhere {
    let unfit = cannot_serve_target(entry, backend, request.target.kind())
        .or_else(|| cannot_serve_mode(entry, backend, request.mode.kind()));
    if let Some(reason) = unfit {
        return KeyElsewhere::Unfit {
            backend: backend.id.clone(),
            reason,
        };
    }
    let missing: Vec<String> = probe_all(capabilities, backend, Some(&request.target))
        .iter()
        .filter(|requirement| requirement.probe.is_missing())
        .map(|requirement| format!("{}: {}", requirement.needs, requirement.probe))
        .collect();
    if !missing.is_empty() {
        return KeyElsewhere::Missing {
            backend: backend.id.clone(),
            reason: missing.join("; "),
        };
    }
    if !crate::collect::implements(request.source.as_str(), &backend.id) {
        return KeyElsewhere::Unwritten(backend.id.clone());
    }
    KeyElsewhere::Forceable(backend.id.clone())
}

/// The comparisons of a predicate the backend's program carries: the parts of
/// its top-level conjunction that compare a field of `filter_in_program` with
/// one value, one per field. See [`Backend::filter_in_program`].
pub fn pushed_down<'a>(backend: &Backend, predicate: &'a FilterExpr) -> Vec<&'a FilterExpr> {
    let mut fields: Vec<&str> = Vec::new();
    let mut pushed = Vec::new();
    for part in predicate.conjuncts() {
        let Some(field) = part.constant_equality() else {
            continue;
        };
        if backend.filter_in_program.iter().any(|known| known == field) && !fields.contains(&field)
        {
            fields.push(field);
            pushed.push(part);
        }
    }
    pushed
}

/// Decides where a filter runs, from what the backend's program carries.
///
/// `kernel` only when every part of the predicate is a comparison the program
/// makes ([`pushed_down`]): that is the claim that an event it rejects is
/// never paid for, and it is checked against the collector's own count in
/// `tests/scenario_proc.rs`. An eBPF backend whose program carries less hands
/// the predicate to its collector, which applies it to what was delivered --
/// userspace, whatever the backend's id says. Any other backend returns whole
/// records and the runner filters them.
fn placement(backend: &Backend, entry: &Entry, predicate: &FilterExpr) -> FilterPlacement {
    let pushed = pushed_down(backend, predicate);
    let parts = predicate.conjuncts().len();
    let listed = |parts: &[&FilterExpr]| {
        one_of(
            parts
                .iter()
                .map(|part| format!("`{part}`"))
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str),
        )
    };
    if pushed.len() == parts {
        return FilterPlacement::Kernel(Placed {
            by: AppliedBy::Program,
            reason: format!(
                "`{}` compares {} in the program, so an event the predicate rejects is dropped \
                 in the kernel and never paid for",
                backend.id,
                listed(&pushed)
            ),
        });
    }
    if backend.id.starts_with("bpf") {
        let narrowed = if pushed.is_empty() {
            String::new()
        } else {
            format!(
                "; {} narrows the program first, and only that part is free",
                listed(&pushed)
            )
        };
        let carried = if backend.filter_in_program.is_empty() {
            format!(
                "`{}`'s program compares no field of a predicate",
                backend.id
            )
        } else {
            format!(
                "`{}`'s program compares only {}, with `==` and one value",
                backend.id,
                one_of(
                    backend
                        .filter_in_program
                        .iter()
                        .map(|field| format!("`{field}`"))
                        .collect::<Vec<_>>()
                        .iter()
                        .map(String::as_str)
                )
            )
        };
        return FilterPlacement::Kernel(Placed {
            by: AppliedBy::Collector,
            reason: format!(
                "{carried}, so the collector applies the predicate to what the program \
                 delivered: every event it removes was paid for first{narrowed}"
            ),
        });
    }
    FilterPlacement::Userspace(Placed {
        by: AppliedBy::Runner,
        reason: format!(
            "`{}` returns whole records, so `{}` is filtered after reading: every event costs \
             its full price before being discarded",
            backend.id, entry.id
        ),
    })
}

/// What a request asks of a perf backend beyond what its `pmu:` probes
/// answered: those open the event over this process, and a system, core or
/// cgroup target is one event per cpu, which the kernel guards separately. P9 of
/// `docs/portability.md`: without the capability, `perf_event_paranoid` at 2
/// lets a process count itself and no cpu, and ten sources resolved for
/// `--system` and refused at attach.
pub const PER_CPU_EVENTS: &str = "per-cpu perf events (the target is observed cpu by cpu)";

/// Answers every requirement of one backend on this machine, for a target
/// when there is one.
///
/// Without a target it is what `omn list` shows: each requirement asked about
/// this process, which is weaker than a request -- a system target adds
/// [`PER_CPU_EVENTS`] to a perf backend, and a `/proc/[pid]/...` path is
/// checked against the target's pid rather than this one.
pub fn probe_all(
    capabilities: &Capabilities,
    backend: &Backend,
    target: Option<&Target>,
) -> Vec<Requirement> {
    let pid = target.and_then(first_pid);
    let mut requirements: Vec<Requirement> = backend
        .requires
        .iter()
        .map(|needs| Requirement {
            needs: needs.clone(),
            probe: capabilities.probe(needs, pid),
        })
        .collect();
    // A package counter (`pmu:power/energy-pkg/`) is already probed the only
    // way it opens, on a cpu; every other `pmu:` requirement was asked about
    // this process.
    let per_task = backend.requires.iter().any(|needs| {
        needs
            .strip_prefix("pmu:")
            .is_some_and(|event| !event.contains('/') || event.starts_with("cpu/"))
    });
    // A cgroup is one event per cpu as well, filtered to the cgroup's tasks,
    // and the kernel asks it the same question (`perf_allow_cpu`).
    if per_task
        && matches!(
            target,
            Some(Target::System | Target::Cpu(_) | Target::Cgroup(_))
        )
    {
        requirements.push(Requirement {
            needs: PER_CPU_EVENTS.to_owned(),
            probe: match crate::perf::per_cpu_support() {
                Ok(()) => Probe::Available,
                Err(reason) => Probe::Missing(reason),
            },
        });
    }
    requirements
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
            symbols: None,
            backend: None,
            label: None,
            window: None,
        }
    }

    /// A key the pinned backend does not attach is refused, naming the backend
    /// that does and what stands between the request and it; one it attaches,
    /// or one the entry never offers (the validator's), is let through.
    #[test]
    fn a_key_the_selected_backend_does_not_attach_is_refused_naming_one_that_does() {
        let embedded = Catalog::embedded().unwrap();
        let capabilities = Capabilities::detect();
        let mut entry = embedded.get(&"io.bytes".parse().unwrap()).unwrap().clone();
        let proc = entry
            .backends
            .iter()
            .position(|backend| backend.id == "fs:proc")
            .unwrap();
        entry.backends[proc].keys = Some(vec![KeyKind::Dev]);
        let catalog = Catalog::from_entries(vec![entry]).unwrap();
        let entry = catalog.get(&"io.bytes".parse().unwrap()).unwrap();
        let mut request = fixture("io.bytes", Target::System);
        request.keys = vec![KeyKind::Task];
        let error =
            check_keys_carried(entry, &entry.backends[proc], &request, &capabilities).unwrap_err();
        let ResolveError::KeyNotCarried {
            source_id,
            backend,
            key,
            elsewhere,
        } = &error
        else {
            panic!("not a KeyNotCarried: {error}");
        };
        assert_eq!(
            (source_id.as_str(), backend.as_str(), *key),
            ("io.bytes", "fs:proc", KeyKind::Task)
        );
        // The chain's first backend attaches `task`; whether it is offered as
        // a pin depends on whether this build collects it there, and a pin
        // hint is never given for one it does not.
        let first = &entry.backends[0].id;
        let elsewhere = elsewhere.as_ref().expect("a backend attaches task");
        if crate::collect::implements("io.bytes", first) {
            assert!(
                matches!(elsewhere, KeyElsewhere::Forceable(id) if id == first)
                    || matches!(elsewhere, KeyElsewhere::Missing { backend, .. } if backend == first),
                "{elsewhere:?}"
            );
        } else {
            assert!(
                !matches!(elsewhere, KeyElsewhere::Forceable(_)),
                "{elsewhere:?}"
            );
            assert!(
                !error.to_string().contains("--backend force:"),
                "a pin hint for a backend nothing collects: {error}"
            );
        }
        request.keys = vec![KeyKind::Dev];
        assert_eq!(
            check_keys_carried(entry, &entry.backends[proc], &request, &capabilities),
            Ok(())
        );
        request.keys = vec![KeyKind::Req];
        assert_eq!(
            check_keys_carried(entry, &entry.backends[proc], &request, &capabilities),
            Ok(())
        );
    }

    /// Each of the four answers about a backend that attaches the key, held
    /// to what is independently true: `/proc/diskstats` exists on this
    /// machine (checked with `std::fs`), a path under a directory that does
    /// not exist does not, `fs:cgroup` declares no system target, and the
    /// collector registry is what `omn` itself refuses as unwritten.
    #[test]
    fn a_backend_that_attaches_the_key_is_named_for_what_keeps_the_request_from_it() {
        use crate::collect::Whose;
        let embedded = Catalog::embedded().unwrap();
        let capabilities = Capabilities::detect();
        let entry = embedded.get(&"io.bytes".parse().unwrap()).unwrap();
        let by_id = |id: &str| entry.backends.iter().find(|b| b.id == id).unwrap().clone();
        let request = fixture("io.bytes", Target::System);

        if std::fs::metadata("/proc/diskstats").is_ok() {
            let answer = key_elsewhere(entry, &by_id("fs:proc"), &request, &capabilities);
            assert_eq!(answer, KeyElsewhere::Forceable("fs:proc".into()));
            assert_eq!(answer.whose(), Whose::Request);
            assert!(answer.to_string().contains("--backend force:fs:proc"));
        } else {
            eprintln!("skipped the forceable half: no /proc/diskstats here");
        }

        let mut absent = by_id("fs:proc");
        let path = "/omn-no-such-directory/diskstats";
        assert!(std::fs::metadata(path).is_err());
        absent.requires = vec![path.to_owned()];
        let answer = key_elsewhere(entry, &absent, &request, &capabilities);
        assert!(
            matches!(&answer, KeyElsewhere::Missing { backend, reason }
                if backend == "fs:proc" && reason.contains(path)),
            "{answer:?}"
        );
        assert_eq!(answer.whose(), Whose::Machine);
        assert!(!answer.to_string().contains("--backend force:"), "{answer}");

        let answer = key_elsewhere(entry, &by_id("fs:cgroup"), &request, &capabilities);
        assert!(
            matches!(&answer, KeyElsewhere::Unfit { backend, .. } if backend == "fs:cgroup"),
            "{answer:?}"
        );
        assert_eq!(answer.whose(), Whose::Request);

        let unwritten = entry
            .backends
            .iter()
            .find(|b| !crate::collect::implements("io.bytes", &b.id));
        match unwritten {
            Some(backend) if serves(entry, backend, &request) => {
                let answer = key_elsewhere(entry, backend, &request, &capabilities);
                // The machine's gap first, in the chain's order: unwritten is
                // the answer only where nothing it needs is missing.
                let lacks = probe_all(&capabilities, backend, Some(&request.target))
                    .iter()
                    .any(|requirement| requirement.probe.is_missing());
                if lacks {
                    assert!(
                        matches!(&answer, KeyElsewhere::Missing { backend: id, .. }
                            if *id == backend.id),
                        "{answer:?}"
                    );
                    assert_eq!(answer.whose(), Whose::Machine);
                } else {
                    assert_eq!(answer, KeyElsewhere::Unwritten(backend.id.clone()));
                    assert_eq!(answer.whose(), Whose::Build);
                }
                assert!(!answer.to_string().contains("--backend force:"), "{answer}");
            }
            _ => eprintln!("skipped the unwritten half: every io.bytes backend is collected"),
        }
    }

    /// A filter field the selected backend does not carry names the backend
    /// that does for what keeps the request from it, as a key does: a pin hint
    /// only for a carrier this build collects and this machine can run.
    ///
    /// A catalog fixture, `io.latency` with its carriers' requirements and ids
    /// rewritten, held to what is independently true: `/proc/diskstats` exists
    /// here and a path under a directory that does not exist does not
    /// (`std::fs`), and the collector registry is what `omn` itself refuses as
    /// unwritten.
    #[test]
    fn a_filter_field_another_backend_carries_is_named_for_what_keeps_the_request_from_it() {
        use crate::collect::Whose;
        let embedded = Catalog::embedded().unwrap();
        let capabilities = Capabilities::detect();
        let original = embedded
            .get(&"io.latency".parse().unwrap())
            .unwrap()
            .clone();
        let proc = original
            .backends
            .iter()
            .position(|backend| backend.id == "fs:proc")
            .unwrap();
        let carriers: Vec<usize> = (0..original.backends.len())
            .filter(|index| {
                *index != proc
                    && original
                        .backend_filter_fields(&original.backends[*index])
                        .iter()
                        .any(|field| field == "rwbs")
            })
            .collect();
        assert!(!carriers.is_empty(), "io.latency carries `rwbs` somewhere");
        let collected: Vec<usize> = carriers
            .iter()
            .copied()
            .filter(|index| crate::collect::implements("io.latency", &original.backends[*index].id))
            .collect();
        let mut request = fixture("io.latency", Target::System);
        request.mode = Mode::Trace;
        request.emit = vec!["hist".parse().unwrap()];
        request.filter = Some("rwbs == 'R'".parse().unwrap());
        let refused = |entry: &Entry| {
            let error = check_filter_fields(entry, &entry.backends[proc], &request, &capabilities)
                .unwrap_err();
            let ResolveError::FilterFieldNotCarried {
                backend,
                field,
                elsewhere,
                ..
            } = &error
            else {
                panic!("not a FilterFieldNotCarried: {error}");
            };
            assert_eq!((backend.as_str(), field.as_str()), ("fs:proc", "rwbs"));
            (error.to_string(), elsewhere.clone().map(|boxed| *boxed))
        };

        // Every carrier needs a file this machine does not have.
        let absent = "/omn-no-such-directory/blk";
        assert!(std::fs::metadata(absent).is_err());
        let mut entry = original.clone();
        for index in &carriers {
            entry.backends[*index].requires = vec![absent.to_owned()];
        }
        let (message, elsewhere) = refused(&entry);
        let elsewhere = elsewhere.expect("a backend carries `rwbs`");
        // The machine's gap first, in the chain's order, whether or not this
        // build collects the carrier: the first carrier the chain names.
        let first = carriers[0];
        assert!(
            matches!(&elsewhere, KeyElsewhere::Missing { backend, reason }
                if *backend == entry.backends[first].id && reason.contains(absent)),
            "{elsewhere:?}"
        );
        assert_eq!(elsewhere.whose(), Whose::Machine);
        assert!(
            !message.contains("--backend force:"),
            "a pin hint for a backend this machine cannot run: {message}"
        );

        // Every carrier renamed to a tag nothing collects for this source.
        let mut entry = original.clone();
        for (n, index) in carriers.iter().enumerate() {
            let id = format!("fs:unwritten{n}");
            assert!(!crate::collect::implements("io.latency", &id));
            entry.backends[*index].id = id;
            entry.backends[*index].requires = vec!["/proc/diskstats".to_owned()];
        }
        let (message, elsewhere) = refused(&entry);
        assert_eq!(
            elsewhere,
            Some(KeyElsewhere::Unwritten("fs:unwritten0".into())),
            "{message}"
        );
        assert_eq!(elsewhere.unwrap().whose(), Whose::Build);
        assert!(!message.contains("--backend force:"), "{message}");

        // The same, but the chain's first carrier also lacks a file here: the
        // hint passes over it for a later one this machine could run and
        // nobody wrote, as the walk stops at the first backend it could run.
        // An unwritten carrier ranks above a missing one, whatever their order.
        assert!(
            carriers.len() >= 2,
            "io.latency carries `rwbs` on two backends or more"
        );
        entry.backends[carriers[0]].requires = vec![absent.to_owned()];
        let (message, elsewhere) = refused(&entry);
        assert_eq!(
            elsewhere,
            Some(KeyElsewhere::Unwritten("fs:unwritten1".into())),
            "{message}"
        );
        assert_eq!(elsewhere.unwrap().whose(), Whose::Build);

        // A collected carrier whose one requirement is a file this machine
        // has: the pin is the fix, and is named.
        let Some(first) = collected.first() else {
            eprintln!("skipped the forceable half: this build collects no io.latency carrier");
            return;
        };
        if std::fs::metadata("/proc/diskstats").is_err() {
            eprintln!("skipped the forceable half: no /proc/diskstats here");
            return;
        }
        let mut entry = original.clone();
        entry.backends[*first].requires = vec!["/proc/diskstats".to_owned()];
        let (message, elsewhere) = refused(&entry);
        let id = entry.backends[*first].id.clone();
        assert_eq!(
            elsewhere,
            Some(KeyElsewhere::Forceable(id.clone())),
            "{message}"
        );
        assert!(
            message.contains(&format!("--backend force:{id}")),
            "{message}"
        );
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

    /// A pin skips the walk and not the probes. `--backend force:pmu` on a
    /// cpu that does not publish the events used to be accepted, launch the
    /// target and fail at attach; it is refused here, before anything runs,
    /// as the machine's gap and with the probe's own reason.
    #[test]
    fn a_pinned_backend_whose_requirements_probe_missing_is_refused_before_the_launch() {
        let embedded = Catalog::embedded().unwrap();
        let mut entry = embedded
            .get(&"mem.rss".parse().unwrap())
            .expect("mem.rss is in the catalog")
            .clone();
        // A requirement no kernel meets, on the backend the pin names: the
        // probe that answers it is the real one, reading this machine.
        entry.backends[1].requires.push("kernel>=999.0".to_owned());
        let catalog = Catalog::from_entries(vec![entry]).unwrap();
        let capabilities = Capabilities::detect();
        let mut request = fixture("mem.rss", Target::Pid(vec![std::process::id() as i32]));
        request.backend = Some("force:fs:proc:statm".parse().unwrap());

        match resolve(&catalog, &capabilities, &request) {
            Err(ResolveError::NoViableBackend { rejections, .. }) => {
                let text = rejections.join("\n");
                assert!(text.contains("fs:proc:statm"), "{text}");
                assert!(text.contains("pinned"), "{text}");
                assert!(text.contains("kernel>=999.0"), "{text}");
                assert!(text.contains("older than"), "the probe's reason: {text}");
            }
            other => panic!("a pin past a missing requirement must be refused, got {other:?}"),
        }
    }

    /// A pinned backend nothing in this build collects and that lacks
    /// something here is answered in the chain's order, the machine first, so
    /// one request exits the same way by the walk and by the pin. Without the
    /// missing requirement it is the build's gap: the resolver accepts it, and
    /// the runner refuses it before the launch with "no collector is written".
    #[test]
    fn a_pinned_backend_this_build_lacks_is_refused_for_the_machine_first() {
        let embedded = Catalog::embedded().unwrap();
        let mut entry = embedded
            .get(&"proc.exec".parse().unwrap())
            .expect("proc.exec is in the catalog")
            .clone();
        let classic = entry
            .backends
            .iter_mut()
            .find(|backend| backend.id == "bpf:tp")
            .expect("proc.exec has a classic tracepoint backend");
        classic.requires.push("kernel>=999.0".to_owned());
        assert!(
            !crate::collect::implements("proc.exec", "bpf:tp"),
            "this test needs a backend with no collector"
        );
        let catalog = Catalog::from_entries(vec![entry]).unwrap();
        let capabilities = Capabilities::detect();
        let mut request = fixture("proc.exec", Target::System);
        request.mode = Mode::Trace;
        request.emit = vec!["summary".parse().unwrap()];
        request.backend = Some("force:bpf:tp".parse().unwrap());
        match resolve(&catalog, &capabilities, &request) {
            Err(ResolveError::NoViableBackend { rejections, .. }) => {
                let text = rejections.join("\n");
                assert!(
                    text.contains("kernel>=999.0"),
                    "the machine's reason: {text}"
                );
            }
            other => panic!("the machine's gap first, as the walk says it: {other:?}"),
        }
        // The walk, over the same chain, says the same thing for this backend.
        request.backend = None;
        if let Err(ResolveError::NoViableBackend { rejections, .. }) =
            resolve(&catalog, &capabilities, &request)
        {
            let line = rejections
                .iter()
                .find(|line| line.contains("bpf:tp"))
                .expect("the walk names the backend");
            assert!(line.contains("kernel>=999.0"), "{line}");
        }

        // And with nothing missing, the build's gap is the runner's to say.
        let mut entry = embedded
            .get(&"proc.exec".parse().unwrap())
            .expect("proc.exec is in the catalog")
            .clone();
        let classic = entry
            .backends
            .iter_mut()
            .find(|backend| backend.id == "bpf:tp")
            .expect("proc.exec has a classic tracepoint backend");
        classic.requires.clear();
        let catalog = Catalog::from_entries(vec![entry]).unwrap();
        request.backend = Some("force:bpf:tp".parse().unwrap());
        let resolved = resolve(&catalog, &capabilities, &request)
            .expect("an unwritten pinned backend this machine lacks nothing for is the runner's");
        assert!(resolved.backend.pinned);
    }

    /// And a pinned backend that cannot attribute to the target is refused for
    /// the reason the walk would have given, rather than answering a question
    /// about something else.
    #[test]
    fn a_pinned_backend_that_cannot_serve_the_target_is_refused_with_the_reason() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = Capabilities::detect();
        let mut request = fixture("io.latency", Target::Pid(vec![1]));
        request.mode = Mode::Trace;
        request.emit = vec!["hist".parse().unwrap()];
        let fs = catalog
            .get(&"io.latency".parse().unwrap())
            .unwrap()
            .backends
            .iter()
            .find(|backend| backend.id.starts_with("fs:"))
            .expect("io.latency has a procfs backend")
            .id
            .clone();
        request.backend = Some(format!("force:{fs}").parse().unwrap());
        match resolve(&catalog, &capabilities, &request) {
            Err(ResolveError::NoViableBackend { rejections, .. }) => {
                let text = rejections.join("\n");
                assert!(text.contains("cannot attribute per pid"), "{text}");
            }
            other => panic!("a pin that cannot serve a pid must be refused, got {other:?}"),
        }
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
    ///
    /// `net.tcp.rtt` is the case with nothing else in the way: the entry marks
    /// `cgroup` as `bpf_only` and neither backend declares targets of its own,
    /// so the only thing that can rule netlink out is the rule under test.
    #[test]
    fn a_bpf_only_target_rules_out_the_backends_that_cannot_filter_in_program() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = Capabilities::detect();
        let mut request = fixture("net.tcp.rtt", Target::Cgroup("/sys/fs/cgroup".into()));
        request.mode = Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        };

        match resolve(&catalog, &capabilities, &request) {
            Ok(resolved) => assert!(
                resolved.backend.id.starts_with("bpf"),
                "a cgroup target on net.tcp.rtt needs in-program filtering, got `{}`",
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

    /// The same request, refused for a stronger reason once the backend says
    /// what it can attribute: `/proc/diskstats` is not held back because it
    /// cannot filter in-program, it is held back because there is no
    /// per-process number in the file at all.
    #[test]
    fn a_backend_that_cannot_attribute_per_target_says_what_it_would_have_answered() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = Capabilities::detect();
        let mut request = fixture("io.latency", Target::Pid(vec![1]));
        request.mode = Mode::Trace;
        request.emit = vec!["hist".parse().unwrap()];

        match resolve(&catalog, &capabilities, &request) {
            Ok(resolved) => assert!(
                resolved.backend.id.starts_with("bpf"),
                "only the eBPF backend can attribute io.latency to a pid, got `{}`",
                resolved.backend.id
            ),
            Err(ResolveError::NoViableBackend { rejections, .. }) => {
                let diskstats = rejections
                    .iter()
                    .find(|rejection| rejection.contains("fs:proc"))
                    .unwrap_or_else(|| panic!("the chain has to say why: {rejections:?}"));
                assert!(
                    diskstats.contains("cannot attribute per pid")
                        && diskstats.contains("scoped to system"),
                    "the refusal has to name the number it would have answered with: {diskstats}"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// The CI's second run: on a machine that withholds an entry's written
    /// backend, the one left can be a backend nobody wrote. The request
    /// resolves there and the runner refuses it as this build's gap (exit 3)
    /// -- true: a collector for it would serve this machine -- with the chain
    /// saying what the machine withheld. The listing is what was wrong: it
    /// called the source usable through the unwritten backend, and available
    /// means it attaches. Out of reach now.
    ///
    /// `cpu.cycles` on `pmu:sw` was the pair until its collector landed. The
    /// unwritten backend is now `cpu.cycles`'s own `pmu:sw` fallback renamed
    /// to a tag no collector serves for this source, and asserted unwritten,
    /// so no collector written tomorrow can make the test's subject vanish
    /// again: it fails at that assertion, naming it. The machine is made to
    /// lack the first backend by asking for an event no machine publishes, so
    /// the collector registered for it is the real one.
    #[test]
    fn a_source_viable_only_through_a_backend_nobody_wrote_is_not_listed_usable() {
        const UNWRITTEN: &str = "fs:proc";
        let embedded = Catalog::embedded().unwrap();
        let mut entry = embedded
            .get(&"cpu.cycles".parse().unwrap())
            .expect("cpu.cycles is in the catalog")
            .clone();
        assert!(crate::collect::implements("cpu.cycles", "pmu"));
        assert!(
            !crate::collect::implements("cpu.cycles", UNWRITTEN),
            "cpu.cycles on {UNWRITTEN} has a collector now: pick another tag nobody wrote"
        );
        entry.backends[0].requires = vec!["pmu:omnnounit/cycles/".into()];
        let mut fallback = entry
            .backends
            .iter()
            .find(|b| b.id == "pmu:sw")
            .expect("cpu.cycles declares its pmu:sw fallback")
            .clone();
        fallback.id = UNWRITTEN.into();
        fallback.requires = Vec::new();
        entry.backends = vec![entry.backends[0].clone(), fallback];
        let catalog = Catalog::from_entries(vec![entry.clone()]).unwrap();
        let capabilities = Capabilities::detect();
        assert_eq!(
            availability(&entry, &capabilities),
            Availability::Unavailable
        );

        let mut request = fixture("cpu.cycles", Target::Pid(vec![std::process::id() as i32]));
        request.mode = Mode::Count;
        request.emit = vec!["summary".parse().unwrap()];
        let resolved = resolve(&catalog, &capabilities, &request)
            .unwrap_or_else(|e| panic!("resolves to {UNWRITTEN}: {e}"));
        assert_eq!(resolved.backend.id, UNWRITTEN);
        match &resolved.chain[0].verdict {
            Verdict::Rejected(reasons) => assert!(
                reasons.iter().any(|reason| reason.contains("omnnounit")),
                "{reasons:?}"
            ),
            other => panic!("the withheld backend has to say why: {other:?}"),
        }
    }

    /// P7 of `docs/portability.md`: a backend that holds a total cannot be
    /// handed a `trace` request because a sibling could serve one. It is
    /// passed over with the reason in the chain, and the walk goes on to the
    /// backend that traces -- here one no collector is written for, so the
    /// runner refuses it before the launch as the build's gap, which is the
    /// true answer, rather than the file's collector refusing it after.
    #[test]
    fn a_backend_whose_modes_leave_the_requests_out_is_passed_over_with_the_reason() {
        let embedded = Catalog::embedded().unwrap();
        let mut entry = embedded
            .get(&"cpu.oncpu_time".parse().unwrap())
            .expect("cpu.oncpu_time is in the catalog")
            .clone();
        for backend in &mut entry.backends {
            if backend.id.starts_with("fs:") {
                backend.modes = Some(vec![ModeKind::Poll]);
            }
        }
        let catalog = Catalog::from_entries(vec![entry]).unwrap();
        let capabilities = Capabilities::detect();
        let mut request = fixture(
            "cpu.oncpu_time",
            Target::Pid(vec![std::process::id() as i32]),
        );
        request.mode = crate::request::Mode::Trace;
        request.emit = vec!["summary".parse().unwrap()];

        let refusal = |chain: &[ConsideredBackend]| {
            chain
                .iter()
                .find(|considered| considered.id == "fs:proc")
                .map(|considered| considered.verdict.clone())
        };
        match resolve(&catalog, &capabilities, &request) {
            Ok(resolved) => {
                assert_ne!(resolved.backend.id, "fs:proc", "{:?}", resolved.chain);
                match refusal(&resolved.chain) {
                    Some(Verdict::Rejected(reasons)) => assert!(
                        reasons
                            .iter()
                            .any(|reason| reason.contains("serves `poll` mode, not `trace`")),
                        "{reasons:?}"
                    ),
                    other => panic!("fs:proc has to be rejected for the mode: {other:?}"),
                }
            }
            Err(ResolveError::NoViableBackend { rejections, .. }) => assert!(
                rejections
                    .iter()
                    .any(|line| line.contains("fs:proc") && line.contains("not `trace`")),
                "{rejections:?}"
            ),
            Err(other) => panic!("unexpected: {other}"),
        }

        // And the same request with the mode the file serves still reaches it.
        request.mode = crate::request::Mode::Poll {
            interval: crate::request::Mode::DEFAULT_POLL_INTERVAL,
        };
        let resolved = resolve(&catalog, &capabilities, &request).expect("a poll resolves");
        assert!(
            !matches!(refusal(&resolved.chain), Some(Verdict::Rejected(_))),
            "{:?}",
            resolved.chain
        );
    }

    /// A pin skips the walk and not the mode: forced onto a backend that does
    /// not serve the request's mode, the request is refused before anything
    /// runs, with that reason.
    #[test]
    fn a_pinned_backend_that_does_not_serve_the_mode_is_refused_with_the_reason() {
        let embedded = Catalog::embedded().unwrap();
        let mut entry = embedded
            .get(&"cpu.oncpu_time".parse().unwrap())
            .unwrap()
            .clone();
        let file = entry
            .backends
            .iter_mut()
            .find(|backend| backend.id == "fs:proc")
            .expect("cpu.oncpu_time reads /proc");
        file.modes = Some(vec![ModeKind::Poll]);
        let catalog = Catalog::from_entries(vec![entry]).unwrap();
        let mut request = fixture(
            "cpu.oncpu_time",
            Target::Pid(vec![std::process::id() as i32]),
        );
        request.mode = crate::request::Mode::Trace;
        request.backend = Some("force:fs:proc".parse().unwrap());
        match resolve(&catalog, &Capabilities::detect(), &request) {
            Err(ResolveError::NoViableBackend { rejections, .. }) => {
                let text = rejections.join("\n");
                assert!(text.contains("pinned"), "{text}");
                assert!(text.contains("not `trace`"), "{text}");
            }
            other => panic!("a pin past the mode must be refused, got {other:?}"),
        }
    }

    fn backend_of<'a>(catalog: &'a Catalog, source: &str, id: &str) -> (&'a Entry, &'a Backend) {
        let entry = catalog.get(&source.parse().unwrap()).unwrap();
        (entry, entry.backend(id).unwrap())
    }

    /// Stream S's request: `kernel` only for a predicate the program carries.
    /// `proc.syscall`'s program compares the syscall number and nothing else,
    /// with `==` and one value; every other shape over the same field, and
    /// every other field, is applied by the collector to what the program
    /// delivered.
    #[test]
    fn only_a_predicate_the_program_carries_is_placed_in_the_kernel() {
        let catalog = Catalog::embedded().unwrap();
        let (entry, backend) = backend_of(&catalog, "proc.syscall", "bpf:raw_tp");
        assert_eq!(backend.filter_in_program, vec!["syscall".to_owned()]);
        let place = |text: &str| placement(backend, entry, &text.parse().unwrap());

        for pushed in ["syscall == 'getppid'", "syscall == 110"] {
            let placed = place(pushed);
            assert!(placed.in_program(), "{pushed}: {placed:?}");
            assert!(matches!(placed, FilterPlacement::Kernel(_)), "{pushed}");
        }
        for not_pushed in [
            "syscall in (getppid)",
            "syscall != 'getppid'",
            "comm == 'python3'",
            "syscall == 'getppid' || comm == 'sh'",
            "!(syscall == 'getppid')",
            "syscall == 'getppid' && syscall == 'read'",
        ] {
            let placed = place(not_pushed);
            assert_eq!(
                placed.placed().by,
                AppliedBy::Collector,
                "{not_pushed}: {placed:?}"
            );
            // The runner leaves them alone: the collector applied them.
            assert!(matches!(placed, FilterPlacement::Kernel(_)), "{not_pushed}");
        }
        let partial = place("syscall == 'getppid' && comm == 'python3'");
        assert_eq!(partial.placed().by, AppliedBy::Collector);
        assert!(
            partial
                .placed()
                .reason
                .contains("narrows the program first"),
            "{partial:?}"
        );

        // A backend with no program comparisons says so, and a file is the
        // runner's.
        let (entry, backend) = backend_of(&catalog, "proc.exec", "bpf:raw_tp");
        let placed = placement(backend, entry, &"comm == 'sh'".parse().unwrap());
        assert_eq!(placed.placed().by, AppliedBy::Collector);
        assert!(
            placed.placed().reason.contains("compares no field"),
            "{placed:?}"
        );
        let (entry, backend) = backend_of(&catalog, "mem.rss", "fs:proc:statm");
        let placed = placement(backend, entry, &"comm == 'omn'".parse().unwrap());
        assert!(
            matches!(placed, FilterPlacement::Userspace(_)),
            "{placed:?}"
        );
    }

    /// The resolved file says `kernel` for the program's, `userspace` for the
    /// collector's and the runner's, and reads each back as it was -- the
    /// runner's decision to filter or not rides on the variant, so a file
    /// that came back as the other one would filter twice or not at all.
    #[test]
    fn a_placement_is_written_where_it_ran_and_read_back_as_it_was() {
        let catalog = Catalog::embedded().unwrap();
        let (entry, backend) = backend_of(&catalog, "proc.syscall", "bpf:raw_tp");
        let (file_entry, file) = backend_of(&catalog, "mem.rss", "fs:proc:statm");
        let cases = [
            (
                placement(backend, entry, &"syscall == 'read'".parse().unwrap()),
                "kernel",
                "program",
            ),
            (
                placement(backend, entry, &"comm == 'sh'".parse().unwrap()),
                "userspace",
                "collector",
            ),
            (
                placement(file, file_entry, &"comm == 'sh'".parse().unwrap()),
                "userspace",
                "runner",
            ),
        ];
        for (placed, written, by) in cases {
            let json = serde_json::to_value(&placed).unwrap();
            assert_eq!(json["placement"], written, "{json}");
            assert_eq!(json["applied_by"], by, "{json}");
            assert_eq!(
                serde_json::from_value::<FilterPlacement>(json).unwrap(),
                placed
            );
        }
        // A file from before `applied_by` keeps its runner's behaviour.
        let old: FilterPlacement =
            serde_json::from_str(r#"{"placement": "kernel", "reason": "old"}"#).unwrap();
        assert!(matches!(old, FilterPlacement::Kernel(_)));
        let old: FilterPlacement =
            serde_json::from_str(r#"{"placement": "userspace", "reason": "old"}"#).unwrap();
        assert!(matches!(old, FilterPlacement::Userspace(_)));
    }

    /// P9: a `pmu:` probe opens its event over this process, and a system or
    /// core target is one event per cpu, which the kernel guards on its own.
    /// So those targets carry one more requirement, answered by opening an
    /// event on a cpu -- and a process target does not.
    #[test]
    fn a_perf_backend_asked_about_whole_cpus_is_probed_for_whole_cpus() {
        let catalog = Catalog::embedded().unwrap();
        let capabilities = Capabilities::detect();
        let (_, backend) = backend_of(&catalog, "cpu.cycles", "pmu");
        let asks = |target: Option<&Target>| {
            probe_all(&capabilities, backend, target)
                .into_iter()
                .find(|requirement| requirement.needs == PER_CPU_EVENTS)
        };
        assert!(asks(Some(&Target::Pid(vec![1]))).is_none());
        assert!(asks(None).is_none(), "the listing asks about this process");
        for target in [Target::System, Target::Cpu("0".parse().unwrap())] {
            let requirement = asks(Some(&target)).expect("a machine target asks per cpu");
            match crate::perf::per_cpu_support() {
                Ok(()) => assert_eq!(requirement.probe, Probe::Available),
                Err(reason) => {
                    assert_eq!(requirement.probe, Probe::Missing(reason.clone()));
                    assert!(reason.contains("perf_event_paranoid"), "{reason}");
                }
            }
        }
        // An eBPF backend opens no perf event over a cpu.
        let (_, bpf) = backend_of(&catalog, "proc.exec", "bpf:raw_tp");
        assert!(probe_all(&capabilities, bpf, Some(&Target::System))
            .iter()
            .all(|requirement| requirement.needs != PER_CPU_EVENTS));
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
