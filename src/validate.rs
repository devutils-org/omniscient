//! Request validation.
//!
//! Everything here runs before anything is attached. The point is not to be
//! strict for its own sake: it is that the alternative to refusing an invalid
//! request is answering it with a number that means something else. Returning a
//! system-wide value while presenting it as per-process attribution is the worst
//! bug this tool could have, so the combination is refused instead.
//!
//! Availability is *not* checked here. Whether a tracepoint exists on this
//! machine is the resolver's question; this module only checks what the catalog
//! declares, which makes it machine-independent and testable.

use std::fmt;

use crate::catalog::{Catalog, Entry, TargetSupport};
use crate::error::{nearest, one_of, DidYouMean};
use crate::format::{FormatStatus, OutputFormat};
use crate::request::{Emit, EmitKind, Mode, Request, Session, StackMode, TargetKind};

/// A rejected request, with the reason and, where one exists, the way out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// Which request in the session, for a session with several.
    pub request: Option<String>,
    pub message: String,
    /// What the user can do about it, when there is something to do.
    pub hint: Option<String>,
}

impl Rejection {
    fn new(message: impl Into<String>) -> Self {
        Self {
            request: None,
            message: message.into(),
            hint: None,
        }
    }

    fn with_hint(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            request: None,
            message: message.into(),
            hint: Some(hint.into()),
        }
    }
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(request) = &self.request {
            write!(f, "[{request}] ")?;
        }
        f.write_str(&self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, "\n  hint: {hint}")?;
        }
        Ok(())
    }
}

/// What a request carries besides its own validity.
///
/// Warnings are about this request: something legal that is likely to mislead,
/// such as an unwind mode that will cost ten times what the user expects.
/// Caveats belong to the source itself and are true of every request that uses
/// it. They are kept apart because mixing them buries the first kind under the
/// second, and the first kind is the one that is actionable right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    warnings: Vec<String>,
    caveats: Vec<String>,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.warnings.is_empty() && self.caveats.is_empty()
    }

    pub fn warnings(&self) -> impl Iterator<Item = &str> {
        self.warnings.iter().map(String::as_str)
    }

    pub fn caveats(&self) -> impl Iterator<Item = &str> {
        self.caveats.iter().map(String::as_str)
    }

    fn push(&mut self, warning: impl Into<String>) {
        self.warnings.push(warning.into());
    }

    fn push_caveat(&mut self, caveat: impl Into<String>) {
        self.caveats.push(caveat.into());
    }

    /// Merges another request's report in, prefixing each line with its label.
    fn absorb(&mut self, label: &str, other: Report) {
        self.warnings.extend(
            other
                .warnings
                .into_iter()
                .map(|line| format!("[{label}] {line}")),
        );
        self.caveats.extend(
            other
                .caveats
                .into_iter()
                .map(|line| format!("[{label}] {line}")),
        );
    }
}

/// Checks one request against the catalog.
pub fn validate_request(
    catalog: &Catalog,
    request: &Request,
    format: OutputFormat,
) -> Result<Report, Vec<Rejection>> {
    let mut rejections = Vec::new();
    let mut report = Report::default();

    let Some(entry) = catalog.get(&request.source) else {
        let suggestion = nearest(request.source.as_str(), catalog.ids());
        return Err(vec![Rejection::with_hint(
            format!(
                "unknown source `{}`{}",
                request.source,
                DidYouMean(suggestion)
            ),
            "run `omn list` to see every source the catalog declares",
        )]);
    };

    check_target(entry, request, &mut rejections);
    check_mode(entry, request, &mut rejections);
    check_emits(entry, request, format, &mut rejections, &mut report);
    check_keys(entry, request, &mut rejections);
    check_filter(entry, request, &mut rejections);
    check_stack(entry, request, &mut rejections, &mut report);
    check_backend(entry, request, &mut rejections, &mut report);
    carry_caveats(entry, &mut report);

    if rejections.is_empty() {
        Ok(report)
    } else {
        Err(rejections)
    }
}

/// Checks every request in a session, reporting all failures at once rather
/// than stopping at the first.
pub fn validate_session(catalog: &Catalog, session: &Session) -> Result<Report, Vec<Rejection>> {
    let mut rejections = Vec::new();
    let mut report = Report::default();

    if session.requests.is_empty() {
        rejections.push(Rejection::new("session declares no request"));
    }

    for (index, request) in session.requests.iter().enumerate() {
        let label = request
            .label
            .clone()
            .unwrap_or_else(|| format!("{} #{}", request.source, index + 1));
        match validate_request(catalog, request, session.format) {
            Ok(request_report) => report.absorb(&label, request_report),
            Err(request_rejections) => {
                rejections.extend(request_rejections.into_iter().map(|mut rejection| {
                    rejection.request = Some(label.clone());
                    rejection
                }));
            }
        }
    }

    check_joins(catalog, session, &mut rejections, &mut report);

    if rejections.is_empty() {
        Ok(report)
    } else {
        Err(rejections)
    }
}

/// Checks that a declared join can be applied to the sources in the session.
///
/// A rule naming a source the session does not collect, or a key the source
/// does not carry, would produce an empty correlation that looks like "nothing
/// was related" instead of "this rule could never have matched".
fn check_joins(
    catalog: &Catalog,
    session: &Session,
    rejections: &mut Vec<Rejection>,
    report: &mut Report,
) {
    let present: Vec<&str> = session
        .requests
        .iter()
        .map(|request| request.source.as_str())
        .collect();

    for rule in &session.joins {
        for (role, kind) in [("from", &rule.from), ("to", &rule.to)] {
            if !present.contains(&kind.as_str()) {
                rejections.push(Rejection::with_hint(
                    format!(
                        "join `{}` reads `{kind}` as its `{role}` side, but this session does not collect it",
                        rule.id
                    ),
                    format!("the session collects {}", one_of(present.iter().copied())),
                ));
                continue;
            }
            let Some(entry) = kind.parse().ok().and_then(|id| catalog.get(&id)) else {
                continue;
            };
            if !entry.keys.contains(&rule.via) {
                rejections.push(Rejection::with_hint(
                    format!(
                        "join `{}` matches on `{}`, which `{kind}` does not carry",
                        rule.id, rule.via
                    ),
                    format!(
                        "`{kind}` carries {}",
                        one_of(entry.keys.iter().map(|key| key.as_str()))
                    ),
                ));
            }
        }

        if rule.window.is_none() {
            report.push(format!(
                "join `{}` has no window, so a range that is never closed is held until the run ends",
                rule.id
            ));
        }
    }
}

fn check_target(entry: &Entry, request: &Request, rejections: &mut Vec<Rejection>) {
    let kind = request.target.kind();
    if entry.target_support(kind) != TargetSupport::No {
        return;
    }
    let supported = entry.supported_targets();
    let reason = target_rejection_reason(entry, kind);
    rejections.push(Rejection::with_hint(
        format!("`{}` cannot be measured per {kind}: {reason}", entry.id),
        format!(
            "this source accepts {}",
            one_of(supported.iter().map(|kind| kind.as_str()))
        ),
    ));
}

/// Explains *why* a target is refused, using what the entry declares rather
/// than a generic "unsupported".
fn target_rejection_reason(entry: &Entry, kind: TargetKind) -> String {
    let uncore = entry.backends.iter().any(|backend| {
        backend
            .requires
            .iter()
            .any(|req| req.contains("uncore") || req.contains("rapl"))
    });
    match kind {
        TargetKind::Pid | TargetKind::Tid | TargetKind::Launch | TargetKind::Container
            if uncore =>
        {
            "the counter is scoped to the package, so no per-process number exists to return"
                .to_owned()
        }
        TargetKind::Iface => format!("`{}` is not a network interface source", entry.id),
        TargetKind::Cpu => "this source is not attributable to a single core".to_owned(),
        _ => format!("the catalog declares no `{kind}` support for this source"),
    }
}

fn check_mode(entry: &Entry, request: &Request, rejections: &mut Vec<Rejection>) {
    let kind = request.mode.kind();
    if entry.modes.contains(&kind) {
        return;
    }
    rejections.push(Rejection::with_hint(
        format!("`{}` cannot be observed in `{kind}` mode", entry.id),
        format!(
            "this source supports {}",
            one_of(entry.modes.iter().map(|mode| mode.as_str()))
        ),
    ));
}

fn check_emits(
    entry: &Entry,
    request: &Request,
    format: OutputFormat,
    rejections: &mut Vec<Rejection>,
    report: &mut Report,
) {
    if request.emit.is_empty() {
        rejections.push(Rejection::with_hint(
            format!("`{}` was requested without an emit", entry.id),
            format!(
                "add --emit with one of {}",
                one_of(entry.emits.iter().map(|emit| emit.as_str()))
            ),
        ));
        return;
    }

    let mode = request.mode.kind();
    for emit in &request.emit {
        let kind = emit.kind();
        if !entry.emits.contains(&kind) {
            rejections.push(Rejection::with_hint(
                format!("`{}` cannot emit `{kind}`", entry.id),
                format!(
                    "this source emits {}",
                    one_of(entry.emits.iter().map(|emit| emit.as_str()))
                ),
            ));
            continue;
        }
        if !mode.valid_emits().contains(&kind) {
            rejections.push(Rejection::with_hint(
                format!("mode `{mode}` cannot produce `{kind}`"),
                format!(
                    "in `{mode}` mode the valid emits are {}",
                    one_of(mode.valid_emits().iter().map(|emit| emit.as_str()))
                ),
            ));
            continue;
        }
        if !format.supports_emit(kind) {
            rejections.push(Rejection::with_hint(
                format!("output format `{format}` cannot carry a `{kind}`"),
                format!("`{format}` is {}", format.describe()),
            ));
            continue;
        }
        if let (Emit::Hist(_), Mode::Trace) = (emit, request.mode) {
            if request.budget.is_none() {
                report.push(format!(
                    "`trace` with `{kind}` aggregates in userspace unless a budget lets the resolver \
                     move the histogram into the kernel; consider --budget"
                ));
            }
        }
    }

    if let FormatStatus::Planned { phase } = format.status() {
        report.push(format!(
            "output format `{format}` is declared but its encoder lands in {phase}"
        ));
    }
}

fn check_keys(entry: &Entry, request: &Request, rejections: &mut Vec<Rejection>) {
    for key in &request.keys {
        if entry.keys.contains(key) {
            continue;
        }
        let hint = if entry.keys.is_empty() {
            format!("`{}` carries no correlation key", entry.id)
        } else {
            format!(
                "`{}` carries {}",
                entry.id,
                one_of(entry.keys.iter().map(|key| key.as_str()))
            )
        };
        rejections.push(Rejection::with_hint(
            format!(
                "`{}` does not carry the `{key}` key, so it cannot be joined on it",
                entry.id
            ),
            hint,
        ));
    }
}

fn check_filter(entry: &Entry, request: &Request, rejections: &mut Vec<Rejection>) {
    let Some(filter) = &request.filter else {
        return;
    };
    for field in filter.fields() {
        if entry.filter_fields.iter().any(|known| known == field) {
            continue;
        }
        let suggestion = nearest(field, entry.filter_fields.iter().map(String::as_str));
        let hint = if entry.filter_fields.is_empty() {
            format!("`{}` exposes no filterable field", entry.id)
        } else {
            format!(
                "`{}` exposes {}",
                entry.id,
                one_of(entry.filter_fields.iter().map(String::as_str))
            )
        };
        rejections.push(Rejection::with_hint(
            format!(
                "`{}` has no field `{field}` to filter on{}",
                entry.id,
                DidYouMean(suggestion)
            ),
            hint,
        ));
    }
}

fn check_stack(
    entry: &Entry,
    request: &Request,
    rejections: &mut Vec<Rejection>,
    report: &mut Report,
) {
    let Some(stack) = request.stack else {
        if request
            .emit
            .iter()
            .any(|emit| emit.kind() == EmitKind::Stack)
        {
            report.push("no --stack given, so the resolver picks an unwind mode: frame pointers where the target has them, none otherwise");
        }
        return;
    };
    if stack == StackMode::None {
        if request
            .emit
            .iter()
            .any(|emit| emit.kind() == EmitKind::Stack)
        {
            rejections.push(Rejection::with_hint(
                "`--emit stack` with `--stack none` asks for stacks and forbids unwinding them",
                "drop --stack none, or emit something other than stacks",
            ));
        }
        return;
    }
    let mode = request.mode.kind();
    if !matches!(request.mode, Mode::Sample { .. } | Mode::Trace) {
        rejections.push(Rejection::with_hint(
            format!("`{mode}` mode captures no stack, so --stack has nothing to unwind"),
            "stacks come from sampling or tracing; a poll or a snapshot reads a value, not a call site",
        ));
        return;
    }
    if !entry.emits.contains(&EmitKind::Stack)
        && !request
            .emit
            .iter()
            .any(|emit| emit.kind() == EmitKind::Stack)
    {
        report.push(format!(
            "--stack was given but nothing emits stacks, so `{}` is only paying to unwind them",
            entry.id
        ));
    }
    if stack == StackMode::Dwarf {
        report.push(
            "dwarf unwinding copies the stack on every sample: expect far more volume and overhead than frame pointers",
        );
    }
}

fn check_backend(
    entry: &Entry,
    request: &Request,
    rejections: &mut Vec<Rejection>,
    report: &mut Report,
) {
    let Some(pinned) = &request.backend else {
        return;
    };
    let Some(backend) = entry.backend(pinned.as_str()) else {
        let suggestion = nearest(
            pinned.as_str(),
            entry.backends.iter().map(|b| b.id.as_str()),
        );
        rejections.push(Rejection::with_hint(
            format!(
                "`{}` has no backend `{}`{}",
                entry.id,
                pinned.as_str(),
                DidYouMean(suggestion)
            ),
            format!(
                "its chain is {}",
                one_of(entry.backends.iter().map(|b| b.id.as_str()))
            ),
        ));
        return;
    };

    // Pinning bypasses the resolver, which is the one place that would
    // otherwise have refused a backend that cannot attribute at the requested
    // granularity. Without this check the bench could pin `/proc/diskstats`
    // against a cgroup and be handed the whole machine's I/O under the
    // cgroup's name -- and a pin exists to compare backends, which is only
    // meaningful between backends that answer the same question.
    let kind = request.target.kind();
    if entry.backend_target_support(backend, kind) == TargetSupport::No {
        let serves: Vec<&str> = TargetKind::ALL
            .into_iter()
            .filter(|candidate| {
                entry.backend_target_support(backend, *candidate) != TargetSupport::No
            })
            .map(|candidate| candidate.as_str())
            .collect();
        rejections.push(Rejection::with_hint(
            format!(
                "backend `{}` of `{}` cannot attribute per {kind}",
                backend.id, entry.id
            ),
            if serves.is_empty() {
                format!("`{}` serves no target this source declares", backend.id)
            } else {
                format!("`{}` serves {}", backend.id, one_of(serves.into_iter()))
            },
        ));
        return;
    }

    report.push(format!(
        "backend `{}` is pinned, so the resolver's choice is bypassed; this is for the comparison bench, not for measuring",
        backend.id
    ));
}

/// Surfaces the entry's caveats with the request, not in documentation nobody
/// reads at the moment it matters.
fn carry_caveats(entry: &Entry, report: &mut Report) {
    for caveat in &entry.caveats {
        report.push_caveat(squash(caveat));
    }
}

/// Collapses the line wrapping a YAML folded scalar leaves in a caveat.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{Budget, SourceId, Target};

    fn catalog() -> Catalog {
        Catalog::embedded().unwrap()
    }

    fn request(source: &str, target: Target, mode: &str, emit: &str) -> Request {
        Request {
            source: source.parse::<SourceId>().unwrap(),
            target,
            mode: mode.parse().unwrap(),
            emit: vec![emit.parse().unwrap()],
            keys: Vec::new(),
            filter: None,
            duration: None,
            budget: None,
            stack: None,
            backend: None,
            label: None,
        }
    }

    fn reject(request: &Request) -> Vec<Rejection> {
        validate_request(&catalog(), request, OutputFormat::Text).unwrap_err()
    }

    #[test]
    fn a_valid_request_passes() {
        let request = request("io.latency", Target::System, "trace", "hist");
        assert!(validate_request(&catalog(), &request, OutputFormat::Text).is_ok());
    }

    #[test]
    fn a_system_wide_source_refuses_a_pid_and_says_why() {
        let request = request("mem.bandwidth", Target::Pid(vec![1]), "count", "rate");
        let rejections = reject(&request);
        assert_eq!(rejections.len(), 1);
        assert!(
            rejections[0].message.contains("scoped to the package"),
            "{}",
            rejections[0]
        );
    }

    #[test]
    fn an_impossible_mode_is_refused_with_the_supported_ones() {
        let request = request("mem.rss", Target::Pid(vec![1]), "trace", "raw");
        let rejections = reject(&request);
        assert!(
            rejections
                .iter()
                .any(|r| r.message.contains("cannot be observed in `trace`")),
            "{rejections:?}"
        );
    }

    #[test]
    fn the_mode_emit_matrix_is_enforced() {
        let request = request("cpu.oncpu_time", Target::Pid(vec![1]), "poll@1s", "hist");
        let rejections = reject(&request);
        assert!(
            rejections
                .iter()
                .any(|r| r.message.contains("cannot produce")),
            "{rejections:?}"
        );
    }

    #[test]
    fn an_unknown_source_suggests_the_closest_one() {
        let request = request("io.latancy", Target::System, "trace", "hist");
        let rejections = reject(&request);
        assert!(
            rejections[0].message.contains("io.latency"),
            "{}",
            rejections[0]
        );
    }

    #[test]
    fn a_filter_on_an_absent_field_is_refused() {
        let mut request = request("io.latency", Target::System, "trace", "hist");
        request.filter = Some("bites > 4096".parse().unwrap());
        let rejections = reject(&request);
        assert!(rejections[0].message.contains("bytes"), "{}", rejections[0]);
    }

    #[test]
    fn a_profile_format_refuses_a_histogram() {
        let request = request("io.latency", Target::System, "trace", "hist");
        let rejections = validate_request(&catalog(), &request, OutputFormat::Folded).unwrap_err();
        assert!(
            rejections[0].message.contains("cannot carry"),
            "{}",
            rejections[0]
        );
    }

    #[test]
    fn stacks_and_no_unwinding_contradict_each_other() {
        let mut request = request("cpu.profile", Target::Pid(vec![1]), "sample@99hz", "stack");
        request.stack = Some(StackMode::None);
        let rejections = reject(&request);
        assert!(
            rejections[0].message.contains("forbids unwinding"),
            "{}",
            rejections[0]
        );
    }

    #[test]
    fn caveats_reach_the_user_with_the_request() {
        let mut request = request("io.latency", Target::System, "trace", "hist");
        request.budget = Some("2%".parse::<Budget>().unwrap());
        let report = validate_request(&catalog(), &request, OutputFormat::Text).unwrap();
        assert!(
            report.caveats().any(|caveat| caveat.contains("writeback")),
            "expected the write-attribution caveat, got {report:?}"
        );
        assert!(
            !report
                .warnings()
                .any(|warning| warning.contains("writeback")),
            "a source caveat must not be filed as a warning about this request"
        );
    }
}
