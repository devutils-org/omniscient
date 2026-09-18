//! Human-readable rendering.
//!
//! The text format is for a terminal and makes no promise of being parsed; the
//! structured formats exist for that. What text does promise is that everything
//! the tool knows about a request is visible: the caveats of the source, the
//! warnings, and what has not been decided yet.
//!
//! That last part matters. A request is printed with the backend the resolver
//! picked, the requirements it checked, and the ones it could not check -- and
//! when it was printed without consulting the machine at all, it says so rather
//! than letting the reader assume a backend was chosen.

use std::fmt::Write as _;

use crate::capabilities::Capabilities;
use crate::catalog::{Catalog, Entry};
use crate::format::OutputFormat;
use crate::request::{Request, Session};
use crate::resolve::{self, Availability, FilterPlacement, ResolvedRequest, Verdict};
use crate::validate::{Rejection, Report};

/// Renders a session the way `--dump-request` prints it.
pub fn session(
    catalog: &Catalog,
    session: &Session,
    report: &Report,
    resolutions: &[ResolvedRequest],
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "session {}",
        session.name.as_deref().unwrap_or("<unnamed>")
    );
    let _ = writeln!(out, "  format    {}", session.format);
    if let Some(path) = &session.out {
        let _ = writeln!(out, "  out       {}", path.display());
    }
    if let Some(duration) = session.duration {
        let _ = writeln!(out, "  duration  {duration}");
    }
    if let Some(budget) = session.budget {
        let _ = writeln!(out, "  budget    {budget}");
    }

    for (index, request) in session.requests.iter().enumerate() {
        let label = request
            .label
            .clone()
            .unwrap_or_else(|| format!("#{}", index + 1));
        out.push('\n');
        let _ = writeln!(out, "request {label}");
        push_request(&mut out, catalog, request);
        if let Some(resolved) = resolutions.get(index) {
            push_resolution(&mut out, resolved);
        }
    }

    if let Some(resolved) = resolutions.first() {
        out.push('\n');
        push_environment(&mut out, resolved);
    }

    let mut warnings = report.warnings().peekable();
    if warnings.peek().is_some() {
        out.push('\n');
        let _ = writeln!(out, "warnings (about this request)");
        for warning in warnings {
            let _ = writeln!(out, "  ! {warning}");
        }
    }

    let mut caveats = report.caveats().peekable();
    if caveats.peek().is_some() {
        out.push('\n');
        let _ = writeln!(
            out,
            "caveats (true of these sources, whatever you ask of them)"
        );
        for caveat in caveats {
            let _ = writeln!(out, "  - {caveat}");
        }
    }

    if resolutions.is_empty() {
        out.push('\n');
        let _ = writeln!(
            out,
            "not resolved: no backend was selected, so nothing here says how these numbers \
             would be obtained."
        );
    }
    out
}

/// Prints how a request will actually be obtained on this machine.
fn push_resolution(out: &mut String, resolved: &ResolvedRequest) {
    let _ = writeln!(
        out,
        "  backend   {} (rank {} of {}, cost {}){}",
        resolved.backend.id,
        resolved.backend.rank,
        resolved.chain.len(),
        resolved.backend.cost,
        if resolved.backend.pinned {
            ", pinned"
        } else {
            ""
        }
    );
    for requirement in &resolved.backend.requirements {
        let _ = writeln!(
            out,
            "    needs   {:<34} {}",
            requirement.needs, requirement.probe
        );
    }
    for considered in &resolved.chain {
        if considered.id == resolved.backend.id {
            continue;
        }
        let verdict = match &considered.verdict {
            Verdict::Selected => "selected".to_owned(),
            Verdict::NotReached => "not reached".to_owned(),
            Verdict::Rejected(reasons) => format!("rejected: {}", reasons.join("; ")),
        };
        let _ = writeln!(out, "    chain   {:<34} {verdict}", considered.id);
    }
    match &resolved.filter {
        Some(FilterPlacement::Kernel(reason)) => {
            let _ = writeln!(out, "    filter  pushed into the kernel: {reason}");
        }
        Some(FilterPlacement::Userspace(reason)) => {
            let _ = writeln!(out, "    filter  userspace: {reason}");
        }
        None => {}
    }
    for degradation in &resolved.degradations {
        let _ = writeln!(out, "    degraded {degradation}");
    }
    for unverified in &resolved.unverified {
        let _ = writeln!(
            out,
            "    UNVERIFIED {unverified}: no probe exists for it, so this backend was assumed, not checked"
        );
    }
}

/// Prints the environment captured with the run, which is what makes two
/// measurements comparable.
fn push_environment(out: &mut String, resolved: &ResolvedRequest) {
    let environment = &resolved.environment;
    let _ = writeln!(out, "environment");
    let _ = writeln!(
        out,
        "  kernel {} ({}), {} cpus",
        environment.kernel_release, environment.arch, environment.cpus
    );
    let optional = [
        ("clocksource", environment.clocksource.as_deref()),
        ("governor", environment.cpu_governor.as_deref()),
        ("thp", environment.transparent_hugepage.as_deref()),
        ("smt", environment.smt.as_deref()),
    ];
    let captured: Vec<String> = optional
        .iter()
        .filter_map(|(name, value)| value.map(|value| format!("{name} {value}")))
        .collect();
    if !captured.is_empty() {
        let _ = writeln!(out, "  {}", captured.join(", "));
    }
    let _ = writeln!(
        out,
        "  btf {}, cgroup2 {}, tracefs {}{}",
        environment.kernel_btf,
        environment.cgroup2,
        environment.tracefs,
        environment
            .perf_event_paranoid
            .map(|value| format!(", perf_event_paranoid {value}"))
            .unwrap_or_default()
    );
    if !environment.not_captured.is_empty() {
        let _ = writeln!(
            out,
            "  not captured yet: {}",
            environment.not_captured.join(", ")
        );
    }
}

fn push_request(out: &mut String, catalog: &Catalog, request: &Request) {
    let summary = catalog
        .get(&request.source)
        .map(|entry| entry.summary.as_str())
        .unwrap_or("<not in catalog>");
    let _ = writeln!(
        out,
        "  source    {:<22} {summary}",
        request.source.to_string()
    );
    let _ = writeln!(out, "  target    {}", request.target);
    let _ = writeln!(out, "  mode      {}", request.mode);
    if !request.emit.is_empty() {
        let _ = writeln!(out, "  emit      {}", join(request.emit.iter()));
    }
    if !request.keys.is_empty() {
        let _ = writeln!(out, "  keys      {}", join(request.keys.iter()));
    }
    if let Some(filter) = &request.filter {
        let _ = writeln!(out, "  filter    {filter}");
    }
    if let Some(duration) = request.duration {
        let _ = writeln!(out, "  duration  {duration}");
    }
    if let Some(budget) = request.budget {
        let _ = writeln!(out, "  budget    {budget}");
    }
    if let Some(stack) = request.stack {
        let _ = writeln!(out, "  stack     {stack}");
    }
    if let Some(backend) = &request.backend {
        let _ = writeln!(out, "  backend   {backend}  (pinned, resolver bypassed)");
    }
}

/// Renders the catalog as a table, or one entry in full.
///
/// The selection is a source id when it names one, a family otherwise. Nothing
/// else is guessed: an unknown name lists nothing and says so.
pub fn list(catalog: &Catalog, selection: Option<&str>, capabilities: &Capabilities) -> String {
    let Some(name) = selection else {
        return table(
            &catalog.entries().iter().collect::<Vec<_>>(),
            None,
            capabilities,
        );
    };
    if let Some(entry) = name.parse().ok().and_then(|id| catalog.get(&id)) {
        return self::entry(entry, capabilities);
    }
    table(&catalog.family(name), Some(name), capabilities)
}

fn table(entries: &[&Entry], family: Option<&str>, capabilities: &Capabilities) -> String {
    let mut out = String::new();
    if entries.is_empty() {
        let _ = writeln!(
            out,
            "no source matches `{}`",
            family.unwrap_or("<everything>")
        );
        return out;
    }

    let id_width = entries
        .iter()
        .map(|e| e.id.as_str().len())
        .max()
        .unwrap_or(10)
        .max(6);
    let modes_width = entries
        .iter()
        .map(|e| join(e.modes.iter()).len())
        .max()
        .unwrap_or(5)
        .max(5);

    let _ = writeln!(
        out,
        "{:<id_width$}  {:<modes_width$}  {:<5}  {:<6}  {:<4}  SUMMARY",
        "SOURCE", "MODES", "COST", "DET", "HERE"
    );
    let mut counts = (0, 0, 0);
    for entry in entries {
        let availability = resolve::availability(entry, capabilities);
        match availability {
            Availability::Ready(_) => counts.0 += 1,
            Availability::Unverified(_) => counts.1 += 1,
            Availability::Unavailable => counts.2 += 1,
        }
        let _ = writeln!(
            out,
            "{:<id_width$}  {:<modes_width$}  {:<5}  {:<6}  {:<4}  {}",
            entry.id.as_str(),
            join(entry.modes.iter()),
            entry.best_cost().as_str(),
            entry.determinism.as_str(),
            availability.marker(),
            entry.summary
        );
    }
    let _ = writeln!(
        out,
        "\n{} sources: {} usable here, {} unverified, {} out of reach on this machine.",
        entries.len(),
        counts.0,
        counts.1,
        counts.2
    );
    let _ = writeln!(
        out,
        "HERE is what this machine offers, probed now. `?` means a requirement no probe covers \
         yet, which is not the same as yes. COST is the cheapest backend in a chain; DET is \
         reproducibility between runs, never bit-exactness."
    );
    out
}

/// Renders one source in full: every backend, in order, with what it needs and
/// what it costs.
pub fn entry(entry: &Entry, capabilities: &Capabilities) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{} — {}", entry.id, entry.summary);
    let _ = writeln!(out, "  family        {}", entry.family);
    let _ = writeln!(out, "  unit          {}", entry.unit);
    let _ = writeln!(out, "  determinism   {}", entry.determinism);
    if !entry.primitive {
        let _ = writeln!(out, "  derived from  {}", join(entry.derived_from.iter()));
    }
    let _ = writeln!(out, "  modes         {}", join(entry.modes.iter()));
    let _ = writeln!(out, "  emits         {}", join(entry.emits.iter()));
    let targets: Vec<String> = entry
        .supported_targets()
        .into_iter()
        .map(|kind| format!("{kind}={}", entry.target_support(kind)))
        .collect();
    let _ = writeln!(out, "  targets       {}", targets.join(" "));
    if !entry.keys.is_empty() {
        let _ = writeln!(out, "  keys          {}", join(entry.keys.iter()));
    }
    if !entry.filter_fields.is_empty() {
        let _ = writeln!(out, "  filter fields {}", entry.filter_fields.join(", "));
    }

    let availability = resolve::availability(entry, capabilities);
    let _ = writeln!(
        out,
        "  here          {}",
        match &availability {
            Availability::Ready(id) => format!("usable through `{id}`"),
            Availability::Unverified(id) => format!(
                "`{id}` has requirements no probe covers yet, so it would be assumed, not checked"
            ),
            Availability::Unavailable => "no backend is usable on this machine".to_owned(),
        }
    );

    let _ = writeln!(
        out,
        "\n  backends (ordered; the resolver keeps the first viable one)"
    );
    for (index, backend) in entry.backends.iter().enumerate() {
        let _ = writeln!(
            out,
            "    {}. {:<22} cost {}",
            index + 1,
            backend.id,
            backend.cost
        );
        for requirement in &backend.requires {
            let _ = writeln!(
                out,
                "       needs    {:<38} {}",
                requirement,
                capabilities.probe(requirement, None)
            );
        }
        if let Some(join_rule) = &backend.join {
            let ttl = join_rule
                .ttl
                .map(|ttl| format!(", ttl {ttl}"))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "       join     {} ({}{ttl})",
                join_rule.key, join_rule.match_kind
            );
        }
        if let Some(degraded) = &backend.degraded {
            let _ = writeln!(out, "       degraded {degraded}");
        }
    }

    if !entry.caveats.is_empty() {
        let _ = writeln!(out, "\n  caveats");
        for caveat in &entry.caveats {
            let _ = writeln!(out, "    - {}", squash(caveat));
        }
    }
    out
}

/// Lists the input and output formats, with what each output format can carry.
pub fn formats() -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "input formats (session files; inferred from the extension, forced with --input-format)"
    );
    for format in crate::format::InputFormat::ALL {
        let _ = writeln!(out, "  {:<12}", format);
    }
    let _ = writeln!(out, "\noutput formats (--format)");
    let width = OutputFormat::ALL
        .iter()
        .map(|f| f.as_str().len())
        .max()
        .unwrap_or(10);
    for format in OutputFormat::ALL {
        let status = match format.status() {
            crate::format::FormatStatus::Ready => String::new(),
            crate::format::FormatStatus::Planned { phase } => {
                format!("   [encoder lands in {phase}]")
            }
        };
        let _ = writeln!(
            out,
            "  {:<width$}  {}{status}",
            format.as_str(),
            format.describe()
        );
    }
    let _ = writeln!(
        out,
        "\nResult formats only accept the emits they can express: `folded` and `pprof` carry \
         stacks, `speedscope` carries stacks and timelines, `chrome` carries timelines. \
         A mismatch is refused rather than approximated."
    );
    out
}

/// Renders rejections as the CLI prints them before exiting.
pub fn rejections(rejections: &[Rejection]) -> String {
    let mut out = String::new();
    for rejection in rejections {
        let _ = writeln!(out, "error: {rejection}");
    }
    out
}

fn join<T: std::fmt::Display>(values: impl Iterator<Item = T>) -> String {
    values
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Collapses the newlines a YAML folded string leaves in a caveat.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{Mode, SourceId, Target};

    fn catalog() -> Catalog {
        Catalog::embedded().unwrap()
    }

    fn capabilities() -> Capabilities {
        Capabilities::detect()
    }

    #[test]
    fn the_listing_covers_every_entry_of_a_family() {
        let catalog = catalog();
        let text = list(&catalog, Some("sched"), &capabilities());
        assert!(text.contains("sched.wakeup"));
        assert!(!text.contains("io.latency"));
    }

    #[test]
    fn a_source_name_renders_the_detailed_entry() {
        let text = list(&catalog(), Some("io.latency"), &capabilities());
        assert!(text.contains("backends (ordered"));
        assert!(text.contains("blk:rq@open_ts"));
    }

    #[test]
    fn an_unknown_name_says_so_instead_of_panicking() {
        let text = list(&catalog(), Some("nonexistent"), &capabilities());
        assert!(text.contains("no source matches"), "{text}");
    }

    #[test]
    fn a_dumped_session_says_it_is_not_resolved() {
        let catalog = catalog();
        let request = Request {
            source: "io.latency".parse::<SourceId>().unwrap(),
            target: Target::System,
            mode: Mode::Trace,
            emit: vec!["hist".parse().unwrap()],
            keys: Vec::new(),
            filter: None,
            duration: None,
            budget: None,
            stack: None,
            backend: None,
            label: None,
        };
        let dumped = Session::single(request, OutputFormat::Text, None);
        let text = session(&catalog, &dumped, &Report::default(), &[]);
        assert!(text.contains("not resolved"), "{text}");
    }
}
