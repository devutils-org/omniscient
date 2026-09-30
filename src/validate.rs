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

use crate::catalog::{Catalog, Entry, TargetSupport, Unit};
use crate::error::{nearest, one_of, DidYouMean};
use crate::format::{FormatStatus, OutputFormat};
use crate::request::{Emit, EmitKind, Mode, ModeKind, Request, Session, StackMode, TargetKind};

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
    check_pair(entry, request, &mut rejections);
    check_emits(entry, request, format, &mut rejections, &mut report);
    check_keys(entry, request, &mut rejections);
    check_filter(entry, request, &mut rejections);
    check_ranking(entry, request, &mut rejections);
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
            // No key: nothing to look for (the engine's `NoKey`).
            let Some(via) = rule.via else {
                continue;
            };
            if !entry.keys.contains(&via) {
                rejections.push(Rejection::with_hint(
                    format!(
                        "join `{}` matches on `{}`, which `{kind}` does not carry",
                        rule.id, via
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

        // What the rule means against these sources, collected this way:
        // the match kind's own refusals, found before anything is attached.
        let collected = [&rule.from, &rule.to]
            .iter()
            .all(|kind| present.contains(&kind.as_str()));
        if collected {
            if let Err(error) = crate::join::check_in(rule, &session.requests, catalog) {
                rejections.push(Rejection::new(error.to_string()));
            }
        }
    }

    // Against the session's joins: a `paired` part reads a join by its id, and
    // without them it was refused as a decomposition read out of context.
    for decomposition in &session.decompose {
        if let Err(error) = crate::join::check_decomposition_in(
            decomposition,
            &session.requests,
            &session.joins,
            catalog,
        ) {
            rejections.push(Rejection::new(error.to_string()));
        }
    }
}

/// Checks the catalog against itself, before any request exists.
///
/// The three checks above answer "can this request be served"; this one answers
/// "could any request have been". An entry that promises something no build can
/// keep is the quietest failure this tool has: the entry lists it, `omn list`
/// prints it, the help offers it, and the refusal when someone asks comes from a
/// rule that lives somewhere else entirely, so nothing ever points at the line
/// of YAML that was wrong.
///
/// Returns one complaint per broken promise, empty when the catalog keeps all
/// of them. `Catalog::from_entries` refuses a catalog with any, so a catalog
/// given with `--catalog <dir>` is held to the same promises as the embedded
/// one; `the_catalog_promises_nothing_it_cannot_produce` runs it over the
/// embedded catalog as well.
pub fn check_catalog(catalog: &Catalog) -> Vec<String> {
    let mut complaints = Vec::new();
    for entry in catalog.entries() {
        for emit in &entry.emits {
            if entry
                .modes
                .iter()
                .any(|mode| mode.valid_emits().contains(emit))
            {
                continue;
            }
            complaints.push(format!(
                "catalog entry `{}` declares the emit `{emit}`, which none of the modes it \
                 declares ({}) can produce: no request could ever reach it",
                entry.id,
                one_of(entry.modes.iter().map(|mode| mode.as_str()))
            ));
        }
        if entry.emits.contains(&EmitKind::Rate) && !adds_up(entry.unit) {
            complaints.push(format!(
                "catalog entry `{}` declares `rate` over a `{}`, which does not add up: a sum \
                 of them divided by a window is a quantity of nothing",
                entry.id, entry.unit
            ));
        }
        // A stack's weight is the sum of what its events carried, so it is the
        // same promise as `rate`: a profile of temperatures sums degrees into a
        // flame that is wide where the samples were many, not where it was hot.
        if entry.emits.contains(&EmitKind::Stack) && !adds_up(entry.unit) {
            complaints.push(format!(
                "catalog entry `{}` declares `stack` over a `{}`, which does not add up: a \
                 stack's weight is the sum of its events, and a sum of them is a quantity of \
                 nothing",
                entry.id, entry.unit
            ));
        }
    }
    complaints.extend(container_complaints(catalog));
    complaints
}

/// A container is measured as the cgroup its runtime names: `omn` binds
/// `--container` to that cgroup before resolution (`request::find_container`),
/// so a source, or a backend, that serves a cgroup serves the container too.
/// Saying `container: no` beside a served cgroup refuses, in the validator, a
/// request every collector would have served -- which is what eleven entries
/// did until the fifteenth wave. The key is left out, and the kind falls back
/// to `cgroup`.
fn container_complaints(catalog: &Catalog) -> Vec<String> {
    let refuses = |targets: &std::collections::BTreeMap<TargetKind, TargetSupport>| {
        targets.get(&TargetKind::Container) == Some(&TargetSupport::No)
            && targets
                .get(&TargetKind::Cgroup)
                .is_some_and(|support| *support != TargetSupport::No)
    };
    let mut complaints = Vec::new();
    for entry in catalog.entries() {
        if refuses(&entry.targets) {
            complaints.push(format!(
                "catalog entry `{}` serves a cgroup and declares `container: no`, but a \
                 container is measured as the cgroup its runtime names: drop the key",
                entry.id
            ));
        }
        for backend in &entry.backends {
            if backend.targets.as_ref().is_some_and(refuses) {
                complaints.push(format!(
                    "backend `{}` of `{}` serves a cgroup and declares `container: no`, but a \
                     container is measured as the cgroup its runtime names: drop the key",
                    backend.id, entry.id
                ));
            }
        }
    }
    complaints
}

/// Whether two readings of this unit may be added together.
///
/// `rate` sums an attribute and divides by the window, and `topk` ranks the
/// same sums, so both mean something for a quantity and nothing for a level.
/// Written as an exhaustive match so that a unit added tomorrow has to answer
/// the question rather than inherit an answer.
fn adds_up(unit: Unit) -> bool {
    match unit {
        Unit::Ns | Unit::Bytes | Unit::Count | Unit::Joules | Unit::Cycles => true,
        Unit::Ratio | Unit::Hz | Unit::Watts | Unit::Celsius => false,
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

/// Refuses a (target, mode) pair that the entry accepts one half at a time
/// and no backend of its chain declares together.
///
/// The entry's `targets` and `modes` are each the union over the chain, so a
/// request can pass both checks above and still name a pair no backend
/// serves: `sched.switches --cgroup -m trace`, where `pmu:sw` traces and does
/// not follow a cgroup, and the two programs follow a cgroup and only count.
/// The resolver used to walk that chain and answer "no viable backend on this
/// machine", exit 1 -- the machine's -- when every line under it was the
/// catalog's and no machine could serve it. It is the request's, exit 2,
/// before anything is resolved. A pinned backend is judged alone by
/// [`check_backend`], which is the pin's point.
fn check_pair(entry: &Entry, request: &Request, rejections: &mut Vec<Rejection>) {
    let kind = request.target.kind();
    let mode = request.mode.kind();
    if request.backend.is_some()
        || entry.target_support(kind) == TargetSupport::No
        || !entry.modes.contains(&mode)
    {
        return;
    }
    let serves_target = |backend: &&crate::catalog::Backend| {
        entry.backend_target_support(backend, kind) != TargetSupport::No
    };
    if entry
        .backends
        .iter()
        .filter(serves_target)
        .any(|backend| entry.backend_modes(backend).contains(&mode))
    {
        return;
    }
    let mut modes_for_target: Vec<ModeKind> = Vec::new();
    for backend in entry.backends.iter().filter(serves_target) {
        for served in entry.backend_modes(backend) {
            if !modes_for_target.contains(served) {
                modes_for_target.push(*served);
            }
        }
    }
    let targets_for_mode: Vec<&str> = TargetKind::ALL
        .into_iter()
        .filter(|candidate| {
            entry.backends.iter().any(|backend| {
                entry.backend_target_support(backend, *candidate) != TargetSupport::No
                    && entry.backend_modes(backend).contains(&mode)
            })
        })
        .map(|candidate| candidate.as_str())
        .collect();
    rejections.push(Rejection::with_hint(
        format!(
            "`{}` cannot be observed in `{mode}` mode per {kind}: no backend of its chain \
             declares both, so no machine could serve it ({})",
            entry.id,
            entry
                .backends
                .iter()
                .map(|backend| {
                    let per = if entry.backend_target_support(backend, kind) == TargetSupport::No {
                        format!("not per {kind}")
                    } else {
                        format!("per {kind}")
                    };
                    format!(
                        "`{}` {per}, in {}",
                        backend.id,
                        one_of(entry.backend_modes(backend).iter().map(|m| m.as_str()))
                    )
                })
                .collect::<Vec<_>>()
                .join("; ")
        ),
        format!(
            "per {kind} it serves {}; `{mode}` is served per {}",
            if modes_for_target.is_empty() {
                "no mode".to_owned()
            } else {
                one_of(modes_for_target.iter().map(|m| m.as_str()))
            },
            if targets_for_mode.is_empty() {
                "no target".to_owned()
            } else {
                one_of(targets_for_mode.into_iter())
            }
        ),
    ));
}

/// Whether this source's own collectors build the histogram in the kernel.
///
/// The validator runs before a backend is selected, so this asks the catalog
/// rather than the resolution: a source every one of whose implemented backends
/// buckets in the kernel cannot be told it is aggregating in userspace, whatever
/// the resolver later picks.
/// Whether a budget could move this source's histogram into the kernel.
///
/// Only when the chain has both: a written backend that buckets in a map
/// (`drained`), and a written one that streams every event to be bucketed
/// here, which the resolver may prefer until a budget turns it away. A source
/// whose every collector already buckets in the kernel (`sched.runqlat`) is not
/// waiting for a budget, and one with no such collector at all (a preload
/// source, whose only backend is the shim) has nowhere for a budget to send it.
/// Telling either to pass `--budget` sends the reader to a flag that changes
/// nothing.
fn budget_moves_hist(entry: &Entry) -> bool {
    let written: Vec<_> = entry
        .backends
        .iter()
        .filter(|backend| crate::collect::implements(entry.id.as_str(), &backend.id))
        .collect();
    written.iter().any(|backend| backend.drained) && written.iter().any(|backend| !backend.drained)
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
            // The hint has to be the intersection, not the matrix. A hint
            // listing what a mode can produce in the abstract sends a reader
            // to an emit this source refuses by name, and the second refusal
            // is the one that reads like the tool contradicting itself.
            let reachable: Vec<EmitKind> = mode
                .valid_emits()
                .iter()
                .copied()
                .filter(|candidate| entry.emits.contains(candidate))
                .collect();
            let hint = if reachable.is_empty() {
                format!(
                    "`{}` declares no emit that `{mode}` mode can produce; it emits {}",
                    entry.id,
                    one_of(entry.emits.iter().map(|emit| emit.as_str()))
                )
            } else {
                format!(
                    "in `{mode}` mode `{}` emits {}",
                    entry.id,
                    one_of(reachable.iter().map(|emit| emit.as_str()))
                )
            };
            rejections.push(Rejection::with_hint(
                format!("mode `{mode}` cannot produce `{kind}`"),
                hint,
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
        // Said only where a budget can change the answer; see
        // `budget_moves_hist` for the two shapes where it cannot.
        if let (Emit::Hist(_), Mode::Trace) = (emit, request.mode) {
            if request.budget.is_none() && budget_moves_hist(entry) {
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

/// A ranking by an attribute (`topk:N@<attr>`) names one the entry declares.
///
/// `filter_fields` is the catalog's one list of attribute names an event of the
/// source carries, which is what the filter check reads too. An attribute no
/// event carries ranked every event as `<attr>=?`, one row holding the whole
/// run, and the results only counted it: that is a request that cannot mean
/// anything, refused with the names it could have used.
fn check_ranking(entry: &Entry, request: &Request, rejections: &mut Vec<Rejection>) {
    for emit in &request.emit {
        let Emit::TopK { by: Some(by), .. } = emit else {
            continue;
        };
        let field = by.as_str();
        if entry.filter_fields.iter().any(|known| known == field) {
            continue;
        }
        let suggestion = nearest(field, entry.filter_fields.iter().map(String::as_str));
        let hint = if entry.filter_fields.is_empty() {
            format!(
                "`{}` declares no attribute to rank by; `topk:N` ranks by its keys",
                entry.id
            )
        } else {
            format!(
                "`{}` can be ranked by {}",
                entry.id,
                one_of(entry.filter_fields.iter().map(String::as_str))
            )
        };
        rejections.push(Rejection::with_hint(
            format!(
                "`{emit}` ranks `{}` by `{field}`, an attribute it does not declare{}",
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
    let Some(stack) = request.stack.filter(|stack| stack.is_concrete()) else {
        if request
            .emit
            .iter()
            .any(|emit| emit.kind() == EmitKind::Stack)
        {
            report.push("no --stack given (or `auto`), so the resolver picks the best unwind mode this machine and this target serve, and says why it passed over the others");
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
    // What the mode declares it costs and gives up, from the mode table the
    // chooser reads, so the validator and the resolved file say one thing.
    if let Some(declared) = crate::stack::unwind::entry(stack) {
        if declared.copies > 0 {
            report.push(format!(
                "`--stack {stack}` copies {} bytes of user stack with every sample (cost {}): expect more volume and overhead than frame pointers",
                declared.copies, declared.cost
            ));
        }
        if let Some(degraded) = &declared.degraded {
            report.push(format!("`--stack {stack}`: {degraded}"));
        }
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
    use crate::request::{Budget, ModeKind, SourceId, Target};

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
            symbols: None,
            backend: None,
            label: None,
            window: None,
        }
    }

    fn reject(request: &Request) -> Vec<Rejection> {
        validate_request(&catalog(), request, OutputFormat::Text).unwrap_err()
    }

    /// `topk:N@<attr>` over an attribute the entry declares is kept; over one
    /// it does not, it is refused naming the ones it could have used -- the
    /// filter check's answer, for a ranking.
    #[test]
    fn a_ranking_by_an_attribute_the_entry_does_not_declare_is_refused() {
        let mut ranked = request(
            "mem.heap.alloc",
            Target::Launch(vec!["/bin/true".into()]),
            "trace",
            "summary",
        );
        ranked.emit = vec!["topk:5@op".parse().unwrap()];
        assert!(
            validate_request(&catalog(), &ranked, OutputFormat::Text).is_ok(),
            "{:?}",
            validate_request(&catalog(), &ranked, OutputFormat::Text).err()
        );

        ranked.emit = vec!["topk:5@opp".parse().unwrap()];
        let rejections = reject(&ranked);
        let text = format!("{rejections:?}");
        assert!(text.contains("by `opp`"), "{text}");
        assert!(text.contains("did you mean `op`"), "{text}");
        assert!(text.contains("freed_bytes"), "{text}");

        // And a plain ranking, by the keys, asks for no attribute at all.
        ranked.emit = vec!["topk:5".parse().unwrap()];
        assert!(validate_request(&catalog(), &ranked, OutputFormat::Text).is_ok());
    }

    /// A decomposition part read from a join (`value: paired`) is checked
    /// against the session's joins: with the join declared the session
    /// validates, and with only another join the part is refused naming the
    /// join it reads. It used to be refused either way, as a table read
    /// without the joins.
    #[test]
    fn a_part_read_from_a_join_is_checked_against_the_sessions_joins() {
        let text = |joins: &str| {
            format!(
                "requests:\n\
                 \x20 - {{source: io.syscall, target: system, mode: trace, emit: [summary]}}\n\
                 \x20 - {{source: io.latency, target: system, mode: trace, emit: [summary]}}\n\
                 \x20 - {{source: sched.wakeup, target: system, mode: trace, emit: [summary]}}\n\
                 {joins}\
                 decompose:\n\
                 \x20 - id: one-read\n\
                 \x20   whole: {{from: io.syscall, value: total_ns}}\n\
                 \x20   parts:\n\
                 \x20     - {{state: device, from: io.latency, value: latency_ns}}\n\
                 \x20     - {{state: completion -> wakeup, from: completion-released, value: paired}}\n"
            )
        };
        let session = |joins: &str| {
            let mut session: Session =
                serde_norway::from_str(&text(joins)).expect("the session parses");
            session.apply_defaults();
            session
        };
        let joined = session(
            "joins:\n\
             \x20 - {id: completion-released, from: io.latency, to: sched.wakeup, via: task, \
             match: woke, woken: wakee_tid, waker: waker_tid, window: 1s}\n",
        );
        if let Err(rejections) = validate_session(&catalog(), &joined) {
            panic!("{rejections:?}");
        }
        // A join of another name is not the one the part reads.
        let other = session(
            "joins:\n\
             \x20 - {id: something-else, from: io.latency, to: sched.wakeup, via: task, \
             match: woke, woken: wakee_tid, waker: waker_tid, window: 1s}\n",
        );
        let rejections = validate_session(&catalog(), &other).unwrap_err();
        let text = format!("{rejections:?}");
        assert!(
            text.contains("`completion-released` names no join"),
            "{text}"
        );
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
    fn the_mode_hint_names_only_what_the_source_declares() {
        // `io.bytes` in `count` mode can produce `rate`, `summary` and `raw`
        // in the abstract, and declares all three. It does not declare `hist`
        // at all, and a hint that offered it would be advice leading to a
        // second refusal.
        let request = request("io.bytes", Target::System, "count", "timeline");
        let rejections = reject(&request);
        let hint = rejections
            .iter()
            .find_map(|rejection| rejection.hint.clone())
            .expect("a refused emit always says what the source does produce");
        assert!(
            hint.starts_with("in `count` mode `io.bytes` emits"),
            "the hint should name the source, not the matrix: {hint}"
        );
        let catalog = catalog();
        let entry = catalog.get(&"io.bytes".parse().unwrap()).unwrap();
        for offered in hint.rsplit("emits ").next().unwrap().split(", ") {
            let offered = offered.trim().trim_start_matches("and ");
            assert!(
                entry.emits.iter().any(|emit| emit.as_str() == offered),
                "the hint offered `{offered}`, which `io.bytes` refuses by name"
            );
            assert!(
                ModeKind::Count
                    .valid_emits()
                    .iter()
                    .any(|emit| emit.as_str() == offered),
                "the hint offered `{offered}`, which `count` mode cannot produce"
            );
        }
    }

    #[test]
    fn the_catalog_promises_nothing_it_cannot_produce() {
        let complaints = check_catalog(&catalog());
        assert!(
            complaints.is_empty(),
            "the catalog makes {} promise(s) no build can keep:\n  {}",
            complaints.len(),
            complaints.join("\n  ")
        );
    }

    /// `sched.switches --cgroup -m trace`: the entry follows a cgroup and
    /// traces, one backend at a time, and none of its three does both. The
    /// refusal is the request's, naming each backend's half, and the two
    /// pairs a backend does declare are not refused.
    #[test]
    fn a_target_and_mode_no_backend_declares_together_are_refused_as_the_request() {
        let cgroup = || Target::Cgroup("/sys/fs/cgroup".into());
        let traced = request("sched.switches", cgroup(), "trace", "raw");
        let rejections = reject(&traced);
        let said = rejections
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            said.contains("no backend of its chain declares both"),
            "{said}"
        );
        assert!(
            said.contains("`pmu:sw` not per cgroup") && said.contains("`bpf:raw_tp` per cgroup"),
            "{said}"
        );
        for served in [
            request("sched.switches", cgroup(), "count", "summary"),
            request("sched.switches", Target::Pid(vec![1]), "trace", "raw"),
        ] {
            if let Err(rejections) = validate_request(&catalog(), &served, OutputFormat::Text) {
                assert!(
                    !rejections
                        .iter()
                        .any(|r| r.message.contains("declares both")),
                    "{rejections:?}"
                );
            }
        }
    }

    /// Every pair the validator refuses as the catalog's is one the resolver,
    /// walking the chain on this machine, finds no backend for either: the
    /// refusal moves the answer earlier and to exit 2, and never refuses a
    /// request that would have resolved. The resolver's walk is the witness,
    /// written separately and asked separately.
    #[test]
    fn a_pair_refused_as_the_catalogs_would_not_have_resolved_on_this_machine() {
        let catalog = catalog();
        let capabilities = crate::capabilities::Capabilities::detect();
        let mut refused = 0;
        for entry in catalog.entries() {
            for kind in entry.supported_targets() {
                let target = match kind {
                    TargetKind::Launch => Target::Launch(vec!["/bin/true".into()]),
                    TargetKind::Pid => Target::Pid(vec![std::process::id() as i32]),
                    TargetKind::Tid => Target::Tid(vec![std::process::id() as i32]),
                    TargetKind::Cgroup => Target::Cgroup("/sys/fs/cgroup".into()),
                    TargetKind::System => Target::System,
                    TargetKind::Cpu => Target::Cpu("0".parse().unwrap()),
                    TargetKind::Iface => Target::Iface("lo".into()),
                    // Bound to a cgroup before resolution, never resolved as such.
                    TargetKind::Container => continue,
                };
                for mode in &entry.modes {
                    let spelled = match mode {
                        ModeKind::Poll => "poll@100ms".to_owned(),
                        ModeKind::Sample => "sample@99hz".to_owned(),
                        other => other.as_str().to_owned(),
                    };
                    let Some(emit) = entry
                        .emits
                        .iter()
                        .find(|emit| mode.valid_emits().contains(emit))
                    else {
                        continue;
                    };
                    let mut asked = request(entry.id.as_str(), target.clone(), &spelled, "raw");
                    asked.emit = vec![emit.as_str().parse().unwrap()];
                    let mut rejections = Vec::new();
                    check_pair(entry, &asked, &mut rejections);
                    if rejections.is_empty() {
                        continue;
                    }
                    refused += 1;
                    match crate::resolve::resolve(&catalog, &capabilities, &asked) {
                        Err(crate::resolve::ResolveError::NoViableBackend { .. }) => {}
                        other => panic!(
                            "`{}` per {kind} in `{mode}` is refused as the catalog's ({}), and \
                             the resolver answered {other:?}",
                            entry.id, rejections[0]
                        ),
                    }
                }
            }
        }
        eprintln!("{refused} (source, target, mode) triples are refused as the catalog's");
        assert!(
            refused > 0,
            "no pair of the catalog is refused, so this checked nothing"
        );
    }

    /// A container is its cgroup: every entry that serves a cgroup accepts a
    /// container at the validator, and a `container: no` beside a served
    /// cgroup is a catalog that does not load.
    #[test]
    fn a_container_is_accepted_wherever_its_cgroup_is_served() {
        let catalog = catalog();
        for entry in catalog.entries() {
            let cgroup = entry.target_support(TargetKind::Cgroup) != TargetSupport::No;
            let container = entry.target_support(TargetKind::Container) != TargetSupport::No;
            assert_eq!(cgroup, container, "`{}`", entry.id);
        }
        let mut entry = catalog
            .get(&"cpu.instructions".parse().unwrap())
            .unwrap()
            .clone();
        entry
            .targets
            .insert(TargetKind::Container, TargetSupport::No);
        let refusal = Catalog::from_entries(vec![entry])
            .expect_err("the loader refuses it")
            .to_string();
        assert!(
            refusal.contains("container: no") && refusal.contains("drop the key"),
            "{refusal}"
        );
    }

    #[test]
    fn an_emit_no_declared_mode_can_produce_is_a_broken_promise() {
        // `mem.rss` polls and snapshots, and neither produces a rate. (It was
        // `mem.swap` until the thirteenth wave gave that one a counted window.)
        let mut entry = catalog().get(&"mem.rss".parse().unwrap()).unwrap().clone();
        entry.emits.push(EmitKind::Rate);
        // The loader runs `check_catalog`, so the broken promise is a catalog
        // that does not load, with the complaint as its reason.
        let refusal = Catalog::from_entries(vec![entry])
            .expect_err("the loader refuses it")
            .to_string();
        assert_eq!(refusal.lines().count(), 1, "{refusal}");
        assert!(refusal.contains("none of the modes"), "{refusal}");
    }

    #[test]
    fn a_rate_over_a_unit_that_does_not_add_up_is_a_broken_promise() {
        let mut entry = catalog()
            .get(&"gpu.engine_busy".parse().unwrap())
            .unwrap()
            .clone();
        entry.emits.push(EmitKind::Rate);
        let refusal = Catalog::from_entries(vec![entry])
            .expect_err("the loader refuses it")
            .to_string();
        assert_eq!(refusal.lines().count(), 1, "{refusal}");
        assert!(refusal.contains("does not add up"), "{refusal}");
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

    /// A budget moves a histogram into the kernel only by choosing a backend
    /// that buckets there. `sched.mutex` has one written backend, the preload
    /// shim, and `sched.runqlat` buckets in the kernel whatever it is asked:
    /// neither is given advice a budget cannot act on.
    #[test]
    fn the_budget_advice_is_given_only_where_a_budget_can_act() {
        for source in ["sched.mutex", "sched.runqlat"] {
            let request = request(source, Target::Pid(vec![1]), "trace", "hist");
            let report = validate_request(&catalog(), &request, OutputFormat::Text)
                .unwrap_or_else(|rejections| panic!("{source}: {rejections:?}"));
            assert!(
                !report
                    .warnings()
                    .any(|warning| warning.contains("consider --budget")),
                "{source} was told to pass a budget that changes nothing: {report:?}"
            );
        }
    }
}
