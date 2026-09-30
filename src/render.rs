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
use crate::collect;
use crate::format::OutputFormat;
use crate::request::{Request, Session};
use crate::resolve::{self, Availability, ResolvedRequest, Verdict};
use crate::style::Style;
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
    if let Some(record) = &session.record {
        let unset = || "(not set: refused)".to_owned();
        let _ = writeln!(
            out,
            "  record    until `{}` on {}, retain {}, after {}, at most {} document(s)",
            record
                .trigger
                .as_ref()
                .map_or_else(unset, ToString::to_string),
            record
                .on
                .as_deref()
                .map_or_else(|| "its one request".to_owned(), |on| format!("`{on}`")),
            record
                .retain
                .map_or_else(unset, |retain| retain.to_string()),
            record
                .after
                .map_or_else(|| "0s".to_owned(), |after| after.to_string()),
            record
                .documents
                .map_or_else(unset, |documents| documents.to_string()),
        );
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
            Verdict::Unimplemented => {
                "this machine can serve it and no collector is written for it yet".to_owned()
            }
            Verdict::Rejected(reasons) => format!("rejected: {}", reasons.join("; ")),
        };
        let _ = writeln!(out, "    chain   {:<34} {verdict}", considered.id);
    }
    // Who applies the predicate, which is what it costs: one the program
    // compares is dropped in the kernel, one the collector applies was
    // delivered first, and one applied after reading was delivered and read.
    // The reason is the placement's own, not its `Display`, which prefixes a
    // collector's with `no;` for a line that no longer exists.
    if let Some(placement) = &resolved.filter {
        let placed = placement.placed();
        let heading = match placed.by {
            resolve::AppliedBy::Program => "in the program",
            resolve::AppliedBy::Collector => "applied by the collector, after delivery",
            resolve::AppliedBy::Runner => "applied after reading",
        };
        let _ = writeln!(out, "    filter  {heading}: {}", placed.reason);
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
pub fn list(
    catalog: &Catalog,
    selection: Option<&str>,
    capabilities: &Capabilities,
    style: Style,
) -> String {
    let Some(name) = selection else {
        return table(
            &catalog.entries().iter().collect::<Vec<_>>(),
            None,
            capabilities,
            style,
        );
    };
    if let Some(entry) = name.parse().ok().and_then(|id| catalog.get(&id)) {
        return self::entry(entry, capabilities, style);
    }
    table(&catalog.family(name), Some(name), capabilities, style)
}

fn table(
    entries: &[&Entry],
    family: Option<&str>,
    capabilities: &Capabilities,
    style: Style,
) -> String {
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
        "{:<id_width$}  {:<modes_width$}  {:<5}  {:<6}  {:<4}  {:<9}  SUMMARY",
        "SOURCE", "MODES", "COST", "DET", "HERE", "COLLECTOR"
    );
    let coverage = collect::coverage();
    let mut counts = (0, 0, 0);
    let (mut sources_written, mut backends_written, mut backends_total) = (0, 0, 0);
    for entry in entries {
        let availability = resolve::availability(entry, capabilities);
        match availability {
            Availability::Ready(_) => counts.0 += 1,
            Availability::Unverified(_) => counts.1 += 1,
            Availability::Unavailable => counts.2 += 1,
        }
        let written = coverage.get(entry.id.as_str());
        backends_total += entry.backends.len();
        if let Some(written) = written {
            sources_written += 1;
            backends_written += written.len();
        }
        // The name is painted, not the row: a whole table in red reads as an
        // error, and this is a map of what has been built so far.
        let name = format!("{:<id_width$}", entry.id.as_str());
        let name = match written {
            Some(_) => style.present(&name),
            None => style.absent(&name),
        };
        let _ = writeln!(
            out,
            "{name}  {:<modes_width$}  {:<5}  {:<6}  {:<4}  {:<9}  {}",
            join(entry.modes.iter()),
            entry.best_cost().as_str(),
            entry.determinism.as_str(),
            availability.marker(),
            match written {
                Some(backends) if backends.len() == entry.backends.len() => "all".to_owned(),
                Some(backends) => format!("{} of {}", backends.len(), entry.backends.len()),
                None => "none".to_owned(),
            },
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
        "{sources_written} of them have a collector, covering {backends_written} of their \
         {backends_total} backends. The rest are vocabulary: they resolve, they validate, and \
         they say plainly that nothing collects them yet."
    );
    let _ = writeln!(
        out,
        "HERE is what this machine offers, probed now. `?` means a requirement no probe covers \
         yet, which is not the same as yes. COLLECTOR is what this build can actually measure, \
         which is a different question: a source can be written and out of reach here, or \
         available here with nothing written for it. COST is the cheapest backend in a chain; \
         DET is reproducibility between runs, never bit-exactness."
    );
    out
}

/// Renders one source in full: every backend, in order, with what it needs and
/// what it costs.
pub fn entry(entry: &Entry, capabilities: &Capabilities, style: Style) -> String {
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
    let written = collect::coverage();
    let written = written.get(entry.id.as_str());
    for (index, backend) in entry.backends.iter().enumerate() {
        let collector = match written.is_some_and(|ids| ids.contains(&backend.id.as_str())) {
            true => style.present("collector written"),
            false => style.absent("no collector yet"),
        };
        let _ = writeln!(
            out,
            "    {}. {:<22} cost {:<5}  {collector}",
            index + 1,
            backend.id,
            backend.cost.as_str()
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
        let text = list(&catalog, Some("sched"), &capabilities(), Style::never());
        assert!(text.contains("sched.wakeup"));
        assert!(!text.contains("io.latency"));
    }

    #[test]
    fn a_source_name_renders_the_detailed_entry() {
        let text = list(
            &catalog(),
            Some("io.latency"),
            &capabilities(),
            Style::never(),
        );
        assert!(text.contains("backends (ordered"));
        assert!(text.contains("blk:rq@open_ts"));
    }

    /// Colour is an accelerator, never the carrier of a fact. Strip the escapes
    /// from a coloured listing and it must be the uncoloured one, character for
    /// character -- otherwise the version that lands in a pipe, a file or an
    /// issue report is missing something the terminal had.
    #[test]
    fn colour_adds_nothing_the_plain_listing_does_not_say() {
        let catalog = catalog();
        let painted = list(&catalog, Some("mem"), &capabilities(), Style::always());
        let plain = list(&catalog, Some("mem"), &capabilities(), Style::never());
        assert_ne!(painted, plain, "`always` must actually paint something");
        assert_eq!(strip_escapes(&painted), plain);
    }

    /// What the colour is for: a source nothing collects yet is painted one way
    /// and a source with a collector the other, so a glance down the column
    /// answers "what does this build actually measure".
    #[test]
    fn a_source_without_a_collector_is_painted_as_absent() {
        let catalog = catalog();
        let text = list(&catalog, Some("mem"), &capabilities(), Style::always());
        let covered = crate::collect::coverage();
        assert!(
            covered.contains_key("mem.rss"),
            "this test reads the build's own coverage, and mem.rss is expected in it"
        );
        let line_of = |source: &str| {
            text.lines()
                .find(|line| line.contains(source))
                .unwrap_or_else(|| panic!("{source} should be listed"))
                .to_owned()
        };
        assert!(line_of("mem.rss").starts_with("\x1b[32m"), "{text}");

        // Whichever `mem` source has no collector in this build. Naming one
        // here would make this test fail the day that source is implemented,
        // which is a promise about the roadmap rather than about the rendering.
        let uncovered = catalog
            .entries()
            .iter()
            .filter(|entry| entry.family == "mem")
            .map(|entry| entry.id.to_string())
            .find(|id| !covered.contains_key(id.as_str()))
            .expect("some `mem` source is still waiting for its collector");
        assert!(line_of(&uncovered).starts_with("\x1b[31m"), "{text}");
        assert!(line_of(&uncovered).contains("none"));
    }

    /// The column is derived from the registry, and the table is written by
    /// hand around it. This checks every source in the catalog rather than the
    /// two the other tests happen to name: a source whose column says `none`
    /// while a collector exists for it is a build that understates itself, and
    /// the reverse is a build that claims a measurement it cannot take.
    #[test]
    fn the_collector_column_says_exactly_what_the_registry_holds() {
        let catalog = catalog();
        let text = list(&catalog, None, &capabilities(), Style::never());
        let coverage = crate::collect::coverage();
        for entry in catalog.entries() {
            let line = text
                .lines()
                .find(|line| line.split_whitespace().next() == Some(entry.id.as_str()))
                .unwrap_or_else(|| panic!("{} is missing from the listing", entry.id));
            let expected = match coverage.get(entry.id.as_str()) {
                None => "none".to_owned(),
                Some(written) if written.len() == entry.backends.len() => "all".to_owned(),
                Some(written) => format!("{} of {}", written.len(), entry.backends.len()),
            };
            assert!(
                line.contains(&expected),
                "{} should read `{expected}`: {line}",
                entry.id
            );
            for backend in coverage.get(entry.id.as_str()).into_iter().flatten() {
                assert!(
                    entry.backend(backend).is_some(),
                    "a collector is registered for `{}` on backend `{backend}`, which that \
                     source's chain does not contain",
                    entry.id
                );
            }
        }
    }

    /// The detail view marks each backend of a chain, which is finer than the
    /// table can be: `mem.faults` has a collector on one backend and none on
    /// the other, and "1 of 2" does not say which.
    #[test]
    fn the_detail_view_marks_the_backend_that_is_written() {
        let catalog = catalog();
        let entry = catalog
            .get(&"mem.faults".parse().unwrap())
            .expect("mem.faults");
        let text = self::entry(entry, &capabilities(), Style::never());
        let line_of = |backend: &str| {
            text.lines()
                .find(|line| line.contains(backend) && line.contains("cost"))
                .unwrap_or_else(|| panic!("{backend} should be listed"))
                .to_owned()
        };
        assert!(line_of("pmu:sw").contains("collector written"), "{text}");
        assert!(line_of("bpf:tp").contains("no collector yet"), "{text}");
    }

    /// A run whose output is not a terminal must carry no escape sequences,
    /// which is what a pipe and a redirect into a file both are.
    ///
    /// The style is stated rather than taken from this process: `cargo test`
    /// captures the print macros and not file descriptor one, so a suite
    /// launched from a terminal has a terminal on stdout inside its own tests.
    /// Asking `for_stdout()` here tested how the suite was started.
    #[test]
    fn nothing_is_painted_when_the_output_is_not_a_terminal() {
        let text = list(&catalog(), Some("mem"), &capabilities(), Style::never());
        assert!(!text.contains('\x1b'), "a pipe was given escape sequences");
    }

    /// Removes every escape sequence, which is what a pipe or a file sees.
    fn strip_escapes(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut characters = text.chars();
        while let Some(character) = characters.next() {
            if character != '\x1b' {
                out.push(character);
                continue;
            }
            // A colour sequence is `ESC [ ... m`; nothing else is emitted here.
            for character in characters.by_ref() {
                if character == 'm' {
                    break;
                }
            }
        }
        out
    }

    #[test]
    fn an_unknown_name_says_so_instead_of_panicking() {
        let text = list(
            &catalog(),
            Some("nonexistent"),
            &capabilities(),
            Style::never(),
        );
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
            symbols: None,
            backend: None,
            label: None,
            window: None,
        };
        let dumped = Session::single(request, OutputFormat::Text, None);
        let text = session(&catalog, &dumped, &Report::default(), &[]);
        assert!(text.contains("not resolved"), "{text}");
    }
}
