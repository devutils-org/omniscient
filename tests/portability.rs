//! The portability matrix: `ROADMAP.md`'s two test-strategy rows nobody had
//! built, *Portability* (a kernel matrix: the floor, an LTS, the host's own)
//! and *Degradation* (no privileges, no BTF, no tracefs).
//!
//! A **cell** is one kernel under one configuration. Each cell is the
//! capability sweep run on that machine: every source's answer -- available,
//! missing or unverified, with the reason -- and every source *run*, briefly,
//! because available means it attaches. `docs/portability.md` says what the
//! cells found; `scripts/matrix.sh` boots them.
//!
//! Two halves, the way the bench is built:
//!
//! - **`sweep_this_machine`** is `#[ignore]`: it runs `omn selftest` on
//!   whatever machine it runs on (`src/selftest.rs`, the sweep made a
//!   command) and writes the cell's documents under `target/matrix/cells/`.
//!   `scripts/matrix.sh` runs it inside each guest; the CI runs it on the
//!   hosted runner, which is a cell of its own.
//! - **Everything else runs in the suite and is cheap**: it reads the cells on
//!   disk and judges them. The judges are checked first against cells built
//!   here with a planted defect, because a judge that passes everything proves
//!   nothing about the cells it passed.
//!
//! What the sweep records is what `omn` said and did, through the binary a
//! user runs: this test process carries no capabilities, and asking the
//! library from here would describe the test, not the tool.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde::{Deserialize, Serialize};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Where every cell's documents are written, one directory per cell.
fn cells_dir() -> PathBuf {
    std::env::var_os("OMN_MATRIX_CELLS")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("target/matrix/cells"))
}

// ------------------------------------------------------------ the documents

/// What a cell was: the kernel, the configuration, and what the tool itself
/// saw of the machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cell {
    /// `<kernel>-<config>`, the directory name.
    name: String,
    /// `/proc/sys/kernel/osrelease` inside the cell.
    kernel_release: String,
    /// `caps`, `root`, `nocaps`, `stock` (no capabilities, and the hosted
    /// image's `perf_event_paranoid` 4), `nobtf`, `notracefs`, or `host` for a sweep
    /// run outside a guest.
    config: String,
    uid: u32,
    /// Whether `/sys/kernel/btf/vmlinux` was hidden on purpose.
    btf_hidden: bool,
    /// Whether tracefs was unmounted on purpose.
    tracefs_hidden: bool,
    /// The environment the tool captured, from a resolved file: its own view
    /// of BTF, tracefs and `perf_event_paranoid`.
    environment: serde_json::Value,
    /// How long the sweep took.
    elapsed_s: f64,
    /// Which binary swept the cell: a hash of its bytes. Two cells swept by
    /// two builds can differ by what the build knows -- a probe written, a
    /// listing rule changed -- rather than by the machine, and only this says
    /// which. Absent from cells swept before it existed.
    #[serde(default)]
    build: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Here {
    Yes,
    Unverified,
    No,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Available,
    Missing,
    Unverified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Requirement {
    needs: String,
    state: State,
    /// What was looked for, or why it was not checked. Empty when available.
    detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Backend {
    id: String,
    collector: bool,
    /// The catalog's statement of what this backend loses, when it is a
    /// fallback that loses something.
    degraded: Option<String>,
    requires: Vec<Requirement>,
}

/// One short run of a source, as `omn trace` performed it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Run {
    /// `launch`, `system`, `cpu`, `iface`, `pid` or `cgroup`.
    target: String,
    args: Vec<String>,
    /// The backend `--dump-resolved` chose, when it resolved.
    backend: Option<String>,
    /// `None` when the run was killed by the watchdog.
    code: Option<i32>,
    elapsed_ms: u64,
    /// The head of standard output: the header lines, where `degraded:` is.
    stdout: String,
    stderr: String,
}

impl Run {
    fn attached(&self) -> bool {
        self.code == Some(0)
    }

    fn text(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Source {
    id: String,
    here: Here,
    /// The backend `omn list` named as usable, or as the unverified one.
    here_backend: Option<String>,
    backends: Vec<Backend>,
    runs: Vec<Run>,
    /// `omn list <source>` as the cell printed it. The fields above are
    /// parsed from it, and parsed again when the cell is judged, so a parser
    /// fixed on the host does not need every guest booted again.
    #[serde(default)]
    listing: String,
    /// The catalog's degradation text per backend id.
    #[serde(default)]
    degraded: BTreeMap<String, String>,
}

impl Source {
    /// Parses the listing again, with this build's parser.
    fn reparse(&mut self) {
        if self.listing.is_empty() {
            return;
        }
        let (here, here_backend, mut backends) = parse_listing(&self.listing);
        for backend in &mut backends {
            backend.degraded = self.degraded.get(&backend.id).cloned();
        }
        self.here = here;
        self.here_backend = here_backend;
        self.backends = backends;
    }

    fn backend(&self, id: &str) -> Option<&Backend> {
        self.backends.iter().find(|backend| backend.id == id)
    }

    fn requirements(&self) -> impl Iterator<Item = &Requirement> {
        self.backends.iter().flat_map(|backend| &backend.requires)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Sweep {
    cell: Cell,
    sources: Vec<Source>,
}

impl Sweep {
    fn kernel(&self) -> Option<(u32, u32)> {
        kernel_version(&self.cell.kernel_release)
    }
}

fn kernel_version(release: &str) -> Option<(u32, u32)> {
    let mut parts = release.split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

// ------------------------------------------------------------ reading `omn list <source>`

/// Reads one source's listing: the `here` line and every backend with its
/// requirements' answers.
///
/// Text, because `omn list --format json` projects the catalog and not the
/// probes; `docs/open-findings.md` asks for the probes in the document so this
/// parser can go.
fn parse_listing(text: &str) -> (Here, Option<String>, Vec<Backend>) {
    let mut here = Here::No;
    let mut here_backend = None;
    let mut backends: Vec<Backend> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("here ") {
            let rest = rest.trim();
            let quoted = rest.split('`').nth(1).map(str::to_owned);
            if rest.starts_with("usable through") {
                here = Here::Yes;
                here_backend = quoted;
            } else if rest.contains("no probe covers") {
                here = Here::Unverified;
                here_backend = quoted;
            }
        } else if let Some(rest) = trimmed.strip_prefix("needs ") {
            let rest = rest.trim_start();
            // A requirement may carry a prototype, which has spaces in it --
            // `raw_tracepoint:kfree(long unsigned int, void *)` -- so it ends at
            // the first space outside parentheses, not at the first space.
            // `fentry:do_fsync or vfs_fsync` names alternatives, and is one
            // requirement for as long as an ` or <name>` follows.
            let word_end = |from: usize| {
                let mut depth = 0usize;
                rest[from..]
                    .char_indices()
                    .find(|&(_, c)| {
                        match c {
                            '(' => depth += 1,
                            ')' => depth = depth.saturating_sub(1),
                            _ => {}
                        }
                        depth == 0 && c.is_whitespace()
                    })
                    .map_or(rest.len(), |(at, _)| from + at)
            };
            let mut end = word_end(0);
            while rest[end..].starts_with(" or ") {
                end = word_end(end + " or ".len());
            }
            let (needs, answer) = rest.split_at(end);
            let answer = answer.trim();
            let (state, detail) = if answer == "available" {
                (State::Available, String::new())
            } else if let Some(detail) = answer.strip_prefix("missing:") {
                (State::Missing, detail.trim().to_owned())
            } else if let Some(detail) = answer.strip_prefix("unverified:") {
                (State::Unverified, detail.trim().to_owned())
            } else {
                // An answer this parser does not know is recorded as
                // unverified with the text, so a judge sees it rather than a
                // parser swallowing it.
                (State::Unverified, format!("unparsed answer: {answer}"))
            };
            if let Some(backend) = backends.last_mut() {
                backend.requires.push(Requirement {
                    needs: needs.to_owned(),
                    state,
                    detail,
                });
            }
        } else if trimmed == "caveats" {
            // Nothing after this is about a backend, and a caveat may say
            // anything.
            break;
        } else if let Some((number, rest)) = trimmed.split_once(". ") {
            // Asked after `needs`: a requirement's reason is prose, and prose
            // has full stops -- "(... so bpf() needs CAP_BPF). Grant them" read
            // as a backend numbered "needs bpf missing: ..." lost every
            // capability refusal until a cell without capabilities showed it.
            if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) {
                let id = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                backends.push(Backend {
                    id,
                    collector: rest.contains("collector written"),
                    degraded: None,
                    requires: Vec::new(),
                });
            }
        }
    }
    (here, here_backend, backends)
}

// ------------------------------------------------------------ the sweep

fn omn() -> PathBuf {
    std::env::var_os("OMN_MATRIX_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("target/debug/omn"))
}

/// Measures whatever machine this runs on, and writes the cell.
///
/// `OMN_MATRIX_CELL` names the cell and `OMN_MATRIX_CONFIG` its
/// configuration; `scripts/vm/cell.sh` sets both inside a guest. Run bare, it
/// sweeps the host as `<release>-host`.
#[test]
#[ignore = "measures the machine it runs on; scripts/matrix.sh runs it in each guest"]
fn sweep_this_machine() {
    let config = std::env::var("OMN_MATRIX_CONFIG").unwrap_or_else(|_| "host".into());
    let name = std::env::var("OMN_MATRIX_CELL").unwrap_or_else(|_| format!("here-{config}"));
    let omn = omn();
    assert!(
        omn.exists(),
        "{} does not exist: run `make` first",
        omn.display()
    );
    // The sweep is `omn selftest` (`src/selftest.rs`), the same command a
    // distroproof guest or a user runs; this test only names the cell and
    // writes it where the judges read cells.
    let fixtures = root().join("target");
    let output = Command::new(&omn)
        .args([
            "selftest", "--format", "json", "--cell", &name, "--config", &config,
        ])
        .arg("--fixtures")
        .arg(&fixtures)
        .output()
        .expect("omn selftest runs");
    assert!(
        output.status.success(),
        "omn selftest failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut sweep: Sweep = serde_json::from_slice(&output.stdout).expect("the sweep is a cell");
    sweep.sources.iter_mut().for_each(Source::reparse);
    let dir = cells_dir().join(&name);
    fs::create_dir_all(&dir).expect("the cell's directory");
    let path = dir.join("sweep.json");
    fs::write(
        &path,
        serde_json::to_string_pretty(&sweep).expect("serializes"),
    )
    .expect("the sweep is written");
    let yes = sweep.sources.iter().filter(|s| s.here == Here::Yes).count();
    eprintln!(
        "matrix: {name}: {} sources, {yes} available, swept in {:.0}s; {}",
        sweep.sources.len(),
        sweep.cell.elapsed_s,
        path.display()
    );
}

// ------------------------------------------------------------ the judges

/// A known defect a cell exhibits, recorded rather than hidden: the judge
/// still requires the refusal to say what it says, so the entry fails -- and
/// has to be removed -- the day the defect is fixed.
struct Known {
    /// Where the defect is written down: `docs/portability.md`, by label.
    finding: &'static str,
    /// Whether the entry applies to a cell.
    applies: fn(&Sweep) -> bool,
    sources: &'static [&'static str],
    targets: &'static [&'static str],
    /// Text the refusal must contain.
    says: &'static str,
}

impl Known {
    fn covers(&self, sweep: &Sweep, source: &str, target: &str) -> bool {
        self.sources.contains(&source) && self.targets.contains(&target) && (self.applies)(sweep)
    }
}

/// Every refusal the matrix found and `docs/portability.md` records, with the
/// cells it applies to. Each is a request filed in `docs/open-findings.md`.
const KNOWN: &[Known] = KNOWN_DEFECTS;

/// Available means it attaches: every source `omn list` calls usable, run
/// through a backend that has a collector, must start. A refusal after the
/// probes said yes is finding 15's shape -- the target launched, and then the
/// attach discovered what the probe should have.
///
/// A source listed usable must also have *some* request that resolves. Not
/// every target kind: the listing answers for a source, and `mem.numa` is
/// usable here through `/proc` for a process and not for the machine, which
/// the resolver refuses with the reason before anything is launched.
fn refused_after_yes(sweep: &Sweep, known: &[Known]) -> Vec<String> {
    let mut out = Vec::new();
    for source in sweep.sources.iter().filter(|s| s.here == Here::Yes) {
        if !source.runs.is_empty() && source.runs.iter().all(|r| r.backend.is_none()) {
            out.push(format!(
                "{}: listed usable through `{}`, and no request of it resolved:\n{}",
                source.id,
                source.here_backend.as_deref().unwrap_or("?"),
                source.runs[0].stderr.trim()
            ));
        }
        for run in &source.runs {
            let Some(backend) = run.backend.as_deref() else {
                continue;
            };
            let collected = source.backend(backend).is_some_and(|b| b.collector);
            if run.attached() || !collected {
                continue;
            }
            if let Some(entry) = known
                .iter()
                .find(|k| k.covers(sweep, &source.id, &run.target))
            {
                if run.text().contains(entry.says) {
                    continue;
                }
                out.push(format!(
                    "{} ({}): refused, but not as {} records (expected `{}`):\n{}",
                    source.id,
                    run.target,
                    entry.finding,
                    entry.says,
                    run.text().trim()
                ));
                continue;
            }
            out.push(format!(
                "{} ({}) via `{backend}`: listed usable, refused at the run (code {}):\n  $ omn {}\n{}",
                source.id,
                run.target,
                run.code.map_or("killed".into(), |c| c.to_string()),
                run.args.join(" "),
                run.text().trim()
            ));
        }
    }
    // A recorded refusal that attached is stale: the defect is fixed, or the
    // cell no longer reaches it, and the table must say so. Only a run that
    // could have shown it counts -- a source this cell lists out of reach
    // proves nothing either way.
    for source in sweep.sources.iter().filter(|s| s.here == Here::Yes) {
        for run in source.runs.iter().filter(|r| r.attached()) {
            if let Some(entry) = known
                .iter()
                .find(|k| k.covers(sweep, &source.id, &run.target))
            {
                out.push(format!(
                    "{} ({}): {} is recorded as refusing here and it attached; narrow or remove \
                     the entry",
                    source.id, run.target, entry.finding
                ));
            }
        }
    }
    out
}

/// Every answer carries its reason: a source nobody can serve names, for each
/// backend, a requirement that is missing and what was looked for; an
/// unverified one names the requirement no probe covers; and unknown never
/// counts as available.
fn unexplained(sweep: &Sweep) -> Vec<String> {
    let mut out = Vec::new();
    for source in &sweep.sources {
        if source.backends.is_empty() {
            out.push(format!("{}: the listing names no backend", source.id));
            continue;
        }
        for requirement in source.requirements() {
            if requirement.state != State::Available && requirement.detail.trim().is_empty() {
                out.push(format!(
                    "{}: `{}` is {:?} with no reason",
                    source.id, requirement.needs, requirement.state
                ));
            }
        }
        match source.here {
            Here::Yes => {
                let named = source
                    .here_backend
                    .as_deref()
                    .and_then(|id| source.backend(id));
                match named {
                    // Available means it attaches, and a backend nobody wrote
                    // attaches nowhere: `cpu.cycles` on the CI's cpu with no
                    // hardware PMU was listed usable through `pmu:sw` and every
                    // run of it exited 3.
                    Some(backend) if !backend.collector => out.push(format!(
                        "{}: usable through `{}`, which has no collector in this build",
                        source.id, backend.id
                    )),
                    Some(backend)
                        if backend.requires.iter().all(|r| r.state == State::Available) => {}
                    Some(backend) => out.push(format!(
                        "{}: usable through `{}`, whose requirements are not all available",
                        source.id, backend.id
                    )),
                    None => out.push(format!(
                        "{}: usable through a backend the listing does not show",
                        source.id
                    )),
                }
            }
            Here::Unverified => {
                if !source.requirements().any(|r| r.state == State::Unverified) {
                    out.push(format!(
                        "{}: unverified, and no requirement is unverified",
                        source.id
                    ));
                }
            }
            Here::No => {
                // A backend with no collector is out of reach on every machine,
                // which the listing says beside it; its requirements may all be
                // here.
                for backend in source.backends.iter().filter(|b| b.collector) {
                    if !backend.requires.iter().any(|r| r.state == State::Missing) {
                        out.push(format!(
                            "{}: out of reach, but `{}` has nothing missing",
                            source.id, backend.id
                        ));
                    }
                }
            }
        }
        // Unknown never counts as available: a source whose only viable
        // backends are unverified is not listed as usable.
        if source.here == Here::Yes
            && !source.backends.iter().any(|b| {
                !b.requires.is_empty() && b.requires.iter().all(|r| r.state == State::Available)
                    || b.requires.is_empty()
            })
        {
            out.push(format!(
                "{}: usable, and no backend has every requirement available",
                source.id
            ));
        }
    }
    out
}

/// A source the machine cannot serve is refused with the reason, before
/// anything is launched -- never run through a different question.
fn refused_silently(sweep: &Sweep) -> Vec<String> {
    let mut out = Vec::new();
    for source in sweep.sources.iter().filter(|s| s.here == Here::No) {
        for run in &source.runs {
            if run.attached() {
                out.push(format!(
                    "{} ({}): listed out of reach, and the run went ahead:\n{}",
                    source.id,
                    run.target,
                    run.text().trim()
                ));
            } else if run.code.is_none() {
                out.push(format!(
                    "{} ({}): listed out of reach, and the run hung instead of refusing",
                    source.id, run.target
                ));
            } else if !run.stderr.contains("no viable backend")
                && !run.stderr.contains("no collector is written")
                && !run.stderr.contains("cannot")
                && !run.stderr.contains("missing")
            {
                out.push(format!(
                    "{} ({}): refused without saying why:\n{}",
                    source.id,
                    run.target,
                    run.stderr.trim()
                ));
            }
        }
    }
    out
}

/// A fallback that loses something says so in the run: a backend whose
/// catalog entry states a degradation, selected and run, prints it.
fn degraded_silently(sweep: &Sweep) -> Vec<String> {
    let mut out = Vec::new();
    for source in &sweep.sources {
        for run in source.runs.iter().filter(|r| r.attached()) {
            let Some(backend) = run.backend.as_deref().and_then(|id| source.backend(id)) else {
                continue;
            };
            if backend.degraded.is_some() && !run.text().contains("degraded") {
                out.push(format!(
                    "{} ({}): ran through `{}`, which the catalog says degrades, and the run \
                     did not say so",
                    source.id, run.target, backend.id
                ));
            }
        }
    }
    out
}

/// Every requirement of one kind, across the sweep.
fn requirements_of<'a>(
    sweep: &'a Sweep,
    kind: impl Fn(&str) -> bool + 'a,
) -> impl Iterator<Item = (&'a Source, &'a Requirement)> + 'a {
    sweep.sources.iter().flat_map(move |source| {
        source
            .requirements()
            .filter(|r| kind(&r.needs))
            .map(move |r| (source, r))
            .collect::<Vec<_>>()
    })
}

/// What a configuration removes is missing for that reason, by name.
fn configuration_reasons(sweep: &Sweep) -> Vec<String> {
    let mut out = Vec::new();
    let expect = |out: &mut Vec<String>, kind: &dyn Fn(&str) -> bool, says: &[&str], why: &str| {
        for (source, requirement) in requirements_of(sweep, kind) {
            let named = says.iter().any(|s| requirement.detail.contains(s));
            if requirement.state != State::Missing || !named {
                out.push(format!(
                    "{}: `{}` is {:?} ({}) in a cell where {why}",
                    source.id, requirement.needs, requirement.state, requirement.detail
                ));
            }
        }
    };
    if sweep.cell.btf_hidden {
        expect(
            &mut out,
            &|needs| needs == "btf",
            &["/sys/kernel/btf/vmlinux"],
            "BTF was hidden",
        );
        // A raw tracepoint or an fentry is looked up in BTF, so without it
        // the honest answer is that it cannot be checked -- unverified, with
        // the file named -- and the backend is out of reach through `btf`.
        for (source, requirement) in requirements_of(sweep, |needs| {
            needs.starts_with("raw_tracepoint:") || needs.starts_with("fentry:")
        }) {
            if requirement.state == State::Available || !requirement.detail.contains("vmlinux") {
                out.push(format!(
                    "{}: `{}` is {:?} ({}) in a cell where BTF was hidden, and it is looked up \
                     in BTF",
                    source.id, requirement.needs, requirement.state, requirement.detail
                ));
            }
        }
    }
    if sweep.cell.tracefs_hidden {
        expect(
            &mut out,
            &|needs| needs.starts_with("tracepoint:") || needs == "tracefs",
            &["tracefs is not mounted"],
            "tracefs was unmounted",
        );
    }
    if sweep.cell.config == "nocaps" || sweep.cell.config == "stock" {
        expect(
            &mut out,
            &|needs| needs == "bpf",
            &["CAP_BPF", "permission", "privilege", "capabilit"],
            "the binary carries no capability",
        );
    }
    // The hosted image as shipped: `perf_event_paranoid` 4, a Debian patch
    // that refuses every perf event to an unprivileged process. Every `pmu:`
    // requirement is then missing, and says why: the sysctl, or -- on a cpu
    // that publishes no such event -- that it is not there to be refused.
    if sweep.cell.config == "stock" {
        expect(
            &mut out,
            &|needs| needs.starts_with("pmu:"),
            &[
                "perf_event_paranoid",
                "publishes no",
                "not available on this cpu",
            ],
            "perf_event_paranoid refuses every perf event",
        );
    }
    out
}

/// The kernel floor is 6.1 (`DECISIONS.md`): below it, every requirement
/// written `kernel>=6.1` is missing and says which kernel it found.
fn floor_refused_by_name(sweep: &Sweep) -> Vec<String> {
    let Some(kernel) = sweep.kernel() else {
        return vec![format!(
            "the cell's kernel release `{}` does not parse",
            sweep.cell.kernel_release
        )];
    };
    let mut out = Vec::new();
    for (source, requirement) in requirements_of(sweep, |needs| needs.starts_with("kernel>=")) {
        let wanted = kernel_version(requirement.needs.trim_start_matches("kernel>="));
        let Some(wanted) = wanted else { continue };
        let should = if kernel >= wanted {
            State::Available
        } else {
            State::Missing
        };
        if requirement.state != should
            || (should == State::Missing && !requirement.detail.contains("older than"))
        {
            out.push(format!(
                "{}: `{}` on {}.{} is {:?} ({})",
                source.id,
                requirement.needs,
                kernel.0,
                kernel.1,
                requirement.state,
                requirement.detail
            ));
        }
    }
    out
}

/// uprobe-multi is Linux 6.6: below it a USDT source that attached took the
/// `uprobe` PMU and says so; from 6.6 on it says nothing of the kind.
fn uprobe_multi_by_kernel(sweep: &Sweep) -> Vec<String> {
    let Some(kernel) = sweep.kernel() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for source in &sweep.sources {
        for run in source.runs.iter().filter(|r| r.attached()) {
            if !run
                .backend
                .as_deref()
                .is_some_and(|b| b.starts_with("bpf:usdt"))
            {
                continue;
            }
            // The fallback's own words, not the phrase: a caveat that says a
            // source needs uprobe-multi is printed on every run of it.
            let fell_back = run.text().contains("refused a uprobe-multi link");
            // Below 6.6 a run that did not fall back is not judged: a vendor
            // kernel may carry uprobe-multi from a later release, and Rocky 9's
            // 5.14 does -- it attached through the multi link, without a word
            // of falling back, which is right. The tool asks the kernel, and a
            // judge that asks the version would be the one wrong.
            if kernel >= (6, 6) && fell_back {
                out.push(format!(
                    "{} ({}): fell back from uprobe-multi on {}.{}, which has it",
                    source.id, run.target, kernel.0, kernel.1
                ));
            }
        }
    }
    out
}

/// Every judge, over one cell, by name.
fn judge(sweep: &Sweep, known: &[Known]) -> BTreeMap<&'static str, Vec<String>> {
    let mut verdicts = BTreeMap::new();
    verdicts.insert(
        "available means it attaches",
        refused_after_yes(sweep, known),
    );
    verdicts.insert("every answer has its reason", unexplained(sweep));
    verdicts.insert("out of reach is refused, with why", refused_silently(sweep));
    verdicts.insert("a fallback says what it loses", degraded_silently(sweep));
    verdicts.insert(
        "a removed requirement is named",
        configuration_reasons(sweep),
    );
    verdicts.insert("the floor is refused by name", floor_refused_by_name(sweep));
    verdicts.insert("uprobe-multi is 6.6", uprobe_multi_by_kernel(sweep));
    verdicts
}

fn cells_on_disk() -> Vec<(PathBuf, Sweep)> {
    let Ok(entries) = fs::read_dir(cells_dir()) else {
        return Vec::new();
    };
    let mut cells: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("sweep.json"))
        .filter(|path| path.exists())
        .map(|path| {
            let text = fs::read_to_string(&path).expect("readable");
            let mut sweep: Sweep = serde_json::from_str(&text)
                .unwrap_or_else(|error| panic!("{}: not a sweep ({error})", path.display()));
            sweep.sources.iter_mut().for_each(Source::reparse);
            (path, sweep)
        })
        .collect();
    cells.sort_by(|a, b| a.0.cmp(&b.0));
    cells
}

// ------------------------------------------------------------ the cells on disk

// ------------------------------------------------------------ against envprobe

/// What distroproof's `envprobe` saw of the same guest, the facts `omn`'s
/// reasons are held against. Each is `None` where envprobe could not say.
#[derive(Debug, Default)]
struct Facts {
    btf: Option<bool>,
    paranoid: Option<i64>,
    unprivileged_bpf_disabled: Option<i64>,
    lockdown: Option<String>,
    tracefs_mounted: Option<bool>,
    cgroup_v2: Option<bool>,
    /// `CONFIG_FUNCTION_TRACER` is `y`: whether a trampoline has a call site.
    function_tracer: Option<bool>,
    /// The soft `RLIMIT_MEMLOCK`, in bytes; `None` where unlimited or unread.
    memlock_soft: Option<i64>,
    /// This process's seccomp mode: `filter` under a container's profile.
    seccomp: Option<String>,
}

impl Facts {
    /// From an envprobe report: `sections[].entries[]` of `key`, `status` and
    /// `value`, where `status` is `ok` for a value and anything else for none.
    fn of(report: &serde_json::Value) -> Facts {
        let mut entries = BTreeMap::new();
        for section in report["sections"].as_array().into_iter().flatten() {
            let name = section["name"].as_str().unwrap_or_default();
            for entry in section["entries"].as_array().into_iter().flatten() {
                let key = format!("{name}/{}", entry["key"].as_str().unwrap_or_default());
                entries.insert(key, (entry["status"].clone(), entry["value"].clone()));
            }
        }
        let ok = |key: &str| -> Option<String> {
            let (status, value) = entries.get(key)?;
            (status == "ok").then(|| match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            })
        };
        let number = |key: &str| ok(key).and_then(|v| v.trim().parse::<i64>().ok());
        Facts {
            btf: entries.get("btf/vmlinux").map(|(status, _)| status == "ok"),
            paranoid: number("sysctl/kernel.perf_event_paranoid"),
            unprivileged_bpf_disabled: number("sysctl/kernel.unprivileged_bpf_disabled"),
            lockdown: ok("security/lockdown"),
            tracefs_mounted: ok("tracing/tracefs").map(|v| v != "not mounted"),
            cgroup_v2: ok("cgroups/mode")
                .map(|mode| mode == "v2")
                .or_else(|| entries.contains_key("cgroups/mode").then_some(false)),
            function_tracer: ok("kernel/CONFIG_FUNCTION_TRACER").map(|v| v == "y"),
            memlock_soft: number("privileges/memlock_soft"),
            seccomp: ok("privileges/seccomp"),
        }
    }
}

/// Every reason `omn` gave in a cell agrees with what envprobe saw of the
/// same guest, both ways: a number it quotes is envprobe's number, a fact it
/// refuses for is a fact envprobe reports, and a fact envprobe reports is one
/// it refuses for. Two tools reading one machine independently; a
/// disagreement is a probe asking the wrong question, or a reason naming the
/// wrong cause.
fn reasons_disagree_with_envprobe(sweep: &Sweep, facts: &Facts) -> Vec<String> {
    let mut out = Vec::new();
    let texts: Vec<(String, String)> = sweep
        .sources
        .iter()
        .flat_map(|source| {
            let mut texts = vec![(source.id.clone(), source.listing.clone())];
            texts.extend(
                source
                    .runs
                    .iter()
                    .map(|run| (format!("{} ({})", source.id, run.target), run.text())),
            );
            texts
        })
        .collect();
    let quoted = |pattern: &str, fact: Option<i64>, name: &str, out: &mut Vec<String>| {
        let Some(fact) = fact else { return };
        for (whose, text) in &texts {
            for (at, _) in text.match_indices(pattern) {
                let rest = &text[at + pattern.len()..];
                let digits: String = rest
                    .chars()
                    .skip_while(|c| *c == ' ' || *c == '`')
                    .take_while(|c| c.is_ascii_digit() || *c == '-')
                    .collect();
                if let Ok(said) = digits.parse::<i64>() {
                    if said != fact {
                        out.push(format!(
                            "{whose}: says {name} is {said}, and envprobe read {fact}"
                        ));
                    }
                }
            }
        }
    };
    quoted(
        "perf_event_paranoid is",
        facts.paranoid,
        "perf_event_paranoid",
        &mut out,
    );
    quoted(
        "unprivileged_bpf_disabled` is",
        facts.unprivileged_bpf_disabled,
        "unprivileged_bpf_disabled",
        &mut out,
    );

    let requirement_states = |needs: &dyn Fn(&str) -> bool| -> Vec<(String, State, String)> {
        sweep
            .sources
            .iter()
            .flat_map(|source| {
                source
                    .requirements()
                    .filter(|r| needs(&r.needs))
                    .map(|r| (source.id.clone(), r.state, r.detail.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    if let Some(btf) = facts.btf {
        for (source, state, detail) in requirement_states(&|needs| needs == "btf") {
            match (btf, state) {
                (true, State::Missing) => out.push(format!(
                    "{source}: `btf` missing ({detail}), and envprobe found /sys/kernel/btf/vmlinux"
                )),
                (false, State::Available) => out.push(format!(
                    "{source}: `btf` available, and envprobe found no /sys/kernel/btf/vmlinux"
                )),
                _ => {}
            }
        }
    }
    if let Some(lockdown) = &facts.lockdown {
        let confidential = lockdown == "confidentiality";
        let said = texts
            .iter()
            .any(|(_, text)| text.contains("withholds bpf_probe_read_kernel"));
        if confidential && !said {
            out.push(
                "envprobe read lockdown=confidentiality, and no reason says the kernel withholds \
                 bpf_probe_read_kernel"
                    .to_owned(),
            );
        }
        if !confidential && said {
            out.push(format!(
                "a reason says the kernel withholds bpf_probe_read_kernel, and envprobe read \
                 lockdown={lockdown}"
            ));
        }
    }
    if let Some(mounted) = facts.tracefs_mounted {
        let said = texts
            .iter()
            .filter(|(_, text)| text.contains("tracefs is not mounted"))
            .map(|(whose, _)| whose.clone())
            .next();
        if let (true, Some(whose)) = (mounted, &said) {
            out.push(format!(
                "{whose}: says tracefs is not mounted, and envprobe found it mounted"
            ));
        }
        if !mounted {
            for (source, state, _) in requirement_states(&|needs| needs.starts_with("tracepoint:"))
            {
                if state == State::Available {
                    out.push(format!(
                        "{source}: a classic tracepoint answered available, and envprobe found \
                         tracefs not mounted"
                    ));
                }
            }
        }
    }
    if let Some(tracer) = facts.function_tracer {
        let said = texts
            .iter()
            .find(|(_, text)| text.contains("built without ftrace"))
            .map(|(whose, _)| whose.clone());
        match (tracer, &said) {
            (true, Some(whose)) => out.push(format!(
                "{whose}: says the kernel was built without ftrace, and envprobe read \
                 CONFIG_FUNCTION_TRACER=y"
            )),
            (false, None) => {
                for (source, state, _) in requirement_states(&|needs| needs.starts_with("fentry:"))
                {
                    if state == State::Available {
                        out.push(format!(
                            "{source}: an fentry requirement answered available, and envprobe \
                             read no CONFIG_FUNCTION_TRACER"
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    // A refusal blamed on a seccomp filter (a container's profile) is one
    // envprobe sees too: the rootless podman cells, where it first read
    // "perf_event_paranoid is 2".
    if let Some(mode) = &facts.seccomp {
        if let Some((whose, _)) = texts
            .iter()
            .find(|(_, text)| text.contains("runs under a seccomp filter"))
        {
            if mode != "filter" {
                out.push(format!(
                    "{whose}: says this process runs under a seccomp filter, and envprobe read \
                     seccomp={mode}"
                ));
            }
        }
    }
    if let Some(soft) = facts.memlock_soft {
        // "this process may lock 64 KiB": the limit omn names when it says the
        // kernel charges maps to RLIMIT_MEMLOCK.
        let pattern = "this process may lock ";
        for (whose, text) in &texts {
            for (at, _) in text.match_indices(pattern) {
                let kib: String = text[at + pattern.len()..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                if let Ok(kib) = kib.parse::<i64>() {
                    if kib * 1024 != soft {
                        out.push(format!(
                            "{whose}: says this process may lock {kib} KiB, and envprobe read \
                             a soft RLIMIT_MEMLOCK of {soft} bytes"
                        ));
                    }
                }
            }
        }
    }
    if let Some(v2) = facts.cgroup_v2 {
        let said = texts
            .iter()
            .find(|(_, text)| text.contains("cgroup v2 is not mounted"))
            .map(|(whose, _)| whose.clone());
        if let (true, Some(whose)) = (v2, &said) {
            out.push(format!(
                "{whose}: says cgroup v2 is not mounted, and envprobe read the unified hierarchy"
            ));
        }
    }
    out
}

/// Every distroproof cell's reasons agree with envprobe's report of the same
/// guest (`envprobe.json`, written beside the cell by
/// `scripts/distroproof/cells.py`).
#[test]
fn every_distroproof_cell_s_reasons_agree_with_envprobe() {
    let mut failures = Vec::new();
    let mut judged = 0;
    for (path, sweep) in cells_on_disk() {
        let Ok(text) = fs::read_to_string(path.with_file_name("envprobe.json")) else {
            continue;
        };
        let report: serde_json::Value = serde_json::from_str(&text).expect("envprobe's report");
        judged += 1;
        for disagreement in reasons_disagree_with_envprobe(&sweep, &Facts::of(&report)) {
            failures.push(format!("[{}] {disagreement}", sweep.cell.name));
        }
    }
    eprintln!("envprobe: {judged} distroproof cell(s) judged");
    assert!(
        failures.is_empty(),
        "{} disagreement(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The judge is checked first against a cell with planted disagreements: a
/// judge that passes everything proves nothing about the cells it passed.
#[test]
fn the_envprobe_judge_catches_a_reason_that_names_the_wrong_cause() {
    let mut cell = honest_cell("6.1", "caps");
    let facts = Facts {
        btf: Some(true),
        paranoid: Some(2),
        unprivileged_bpf_disabled: Some(2),
        lockdown: Some("none".into()),
        tracefs_mounted: Some(true),
        cgroup_v2: Some(true),
        function_tracer: Some(true),
        memlock_soft: Some(65536),
        seccomp: Some("none".into()),
    };
    assert!(reasons_disagree_with_envprobe(&cell, &facts).is_empty());
    source(&mut cell, "runtime.custom").runs[0].stderr =
        "perf_event_open was refused: perf_event_paranoid is 3; tracefs is not mounted; this \
         kernel was built without ftrace; this process may lock 8192 KiB; and this process runs \
         under a seccomp filter"
            .into();
    let found = reasons_disagree_with_envprobe(&cell, &facts);
    assert!(
        found.iter().any(|f| f.contains("without ftrace")),
        "{found:?}"
    );
    assert!(
        found.iter().any(|f| f.contains("may lock 8192 KiB")),
        "{found:?}"
    );
    assert!(
        found.iter().any(|f| f.contains("perf_event_paranoid is 3")),
        "{found:?}"
    );
    assert!(
        found.iter().any(|f| f.contains("tracefs is not mounted")),
        "{found:?}"
    );
    assert!(
        found.iter().any(|f| f.contains("seccomp=none")),
        "{found:?}"
    );
    let locked = Facts {
        lockdown: Some("confidentiality".into()),
        ..Facts::default()
    };
    assert!(
        reasons_disagree_with_envprobe(&cell, &locked)
            .iter()
            .any(|f| f.contains("lockdown=confidentiality")),
        "a lockdown no reason names is a disagreement"
    );
}

#[test]
fn every_cell_on_disk_holds_the_invariants() {
    let cells = cells_on_disk();
    if cells.is_empty() {
        eprintln!(
            "skipped: no matrix cell under {} -- `scripts/matrix.sh` boots and sweeps them, \
             `scripts/matrix.sh here` sweeps this machine alone",
            cells_dir().display()
        );
        return;
    }
    let mut failures = Vec::new();
    for (path, sweep) in &cells {
        let verdicts = judge(sweep, KNOWN);
        let broken: usize = verdicts.values().map(Vec::len).sum();
        let yes = sweep.sources.iter().filter(|s| s.here == Here::Yes).count();
        let runs = sweep.sources.iter().flat_map(|s| &s.runs).count();
        let attached = sweep
            .sources
            .iter()
            .flat_map(|s| &s.runs)
            .filter(|r| r.attached())
            .count();
        eprintln!(
            "matrix: {:<28} {:>3} sources, {yes:>3} available, {attached:>3} of {runs:>3} runs \
             attached, {broken} broken",
            sweep.cell.name,
            sweep.sources.len()
        );
        for (invariant, violations) in verdicts {
            for violation in violations {
                failures.push(format!(
                    "[{}] {invariant}: {violation}\n  (from {})",
                    sweep.cell.name,
                    path.display()
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} violation(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// A cell is swept against the catalog this build carries: a cell from an
/// older build has sources the catalog no longer has, or lacks new ones, and
/// is refused as stale rather than judged.
#[test]
fn every_cell_covers_the_whole_catalog() {
    let cells = cells_on_disk();
    if cells.is_empty() {
        eprintln!("skipped: no matrix cell on disk to compare with the catalog");
        return;
    }
    let catalog = Command::new(omn())
        .args(["list", "--format", "json"])
        .output()
        .expect("omn runs");
    let catalog: Vec<serde_json::Value> = serde_json::from_slice(&catalog.stdout).expect("JSON");
    let mut ids: Vec<&str> = catalog.iter().filter_map(|e| e["id"].as_str()).collect();
    ids.sort_unstable();
    for (path, sweep) in cells {
        let mut swept: Vec<&str> = sweep.sources.iter().map(|s| s.id.as_str()).collect();
        swept.sort_unstable();
        assert_eq!(
            swept,
            ids,
            "{} was swept against a different catalog; sweep it again",
            path.display()
        );
    }
}

// ------------------------------------------------------------ two cells, compared

/// Whether a source is listed usable through a backend with no collector: a
/// cell swept before the fourteenth wave's listing rule, which the invariant
/// judge refuses on its own. Not a difference of machines, so left out here.
fn listed_through_nobody(source: &Source) -> bool {
    source.here == Here::Yes
        && source
            .here_backend
            .as_deref()
            .and_then(|id| source.backend(id))
            .is_some_and(|backend| !backend.collector)
}

/// Whether a source's answer in a cell is the build's rather than the
/// machine's: every requirement available and the source still not usable (a
/// listing rule), or a requirement no probe was written for.
fn answered_by_the_build(source: &Source) -> bool {
    let requirements: Vec<&Requirement> = source.requirements().collect();
    (source.here != Here::Yes && requirements.iter().all(|r| r.state == State::Available))
        || requirements
            .iter()
            .any(|r| r.state != State::Available && r.detail.contains("no probe is written"))
}

/// Whether the difference between two cells' answers for one source is the
/// build's: a listing rule on either side, or a requirement no probe was
/// written for that the two answered differently. An unwritten probe that
/// answered alike on both sides cannot be what differs -- `net.hwts`'s
/// `so_timestamping` is unverified in every cell, and the gap is its `phc`.
fn differs_by_the_build(left: &Source, right: &Source) -> bool {
    let listing_rule = |source: &Source| {
        source.here != Here::Yes && source.requirements().all(|r| r.state == State::Available)
    };
    let unwritten_and_differs = |mine: &Source, theirs: &Source| {
        mine.backends.iter().any(|backend| {
            backend.requires.iter().any(|requirement| {
                requirement.state != State::Available
                    && requirement.detail.contains("no probe is written")
                    && theirs
                        .backend(&backend.id)
                        .and_then(|other| {
                            other.requires.iter().find(|r| r.needs == requirement.needs)
                        })
                        .is_none_or(|other| other.state != requirement.state)
            })
        })
    };
    listing_rule(left)
        || listing_rule(right)
        || unwritten_and_differs(left, right)
        || unwritten_and_differs(right, left)
}

/// Whether two cells were swept by the same binary, as far as they record.
fn same_build(a: &Sweep, b: &Sweep) -> Option<bool> {
    Some(a.cell.build.as_ref()? == b.cell.build.as_ref()?)
}

fn usable(sweep: &Sweep) -> std::collections::BTreeSet<&str> {
    sweep
        .sources
        .iter()
        .filter(|source| source.here == Here::Yes && !listed_through_nobody(source))
        .map(|source| source.id.as_str())
        .collect()
}

/// ROADMAP phase 2: "`omn list` shows different sources on two different
/// machines", held directly to two cells' listings.
///
/// The truth is the condition the matrix set up between the two cells of one
/// guest, and not anything the tool said: `nocaps` runs the binary without
/// the capabilities `caps` grants it, `nobtf` hides the BTF `root` has, and
/// `notracefs` unmounts what `root` mounts. So for every such pair on disk
/// the cell that withholds lists no source the one that grants does not, and
/// the two capability and BTF pairs list strictly fewer -- the listings
/// differ, and in the direction the condition predicts. Each source usable
/// only where the condition is granted says, in the withholding cell, a
/// reason that names the condition. A source the build answers (a listing
/// rule, a probe not written) is the build's difference, not the machine's,
/// and is held only between cells swept by the same binary.
#[test]
fn two_cells_list_different_sources_for_the_condition_between_them() {
    let cells = cells_on_disk();
    if cells.is_empty() {
        eprintln!(
            "skipped: no matrix cell under {} -- `scripts/matrix.sh` writes them",
            cells_dir().display()
        );
        return;
    }
    let by_name: BTreeMap<&str, &Sweep> = cells
        .iter()
        .map(|(_, sweep)| (sweep.cell.name.as_str(), sweep))
        .collect();
    // (withheld, granted, strictly fewer, what a withholding reason names)
    let pairs: [(&str, &str, bool, &[&str]); 4] = [
        ("nocaps", "caps", true, &["CAP_", "perf_event_paranoid"]),
        ("nobtf", "root", true, &["btf", "BTF"]),
        ("notracefs", "root", false, &["tracefs"]),
        ("caps", "root", false, &["root", "CAP_", "uid"]),
    ];
    let mut compared = 0;
    let mut failures = Vec::new();
    let mut builds = Vec::new();
    for (name, withheld) in &by_name {
        let Some((machine, config)) = name.rsplit_once('-') else {
            continue;
        };
        for (lo, hi, strictly, names) in pairs {
            if config != lo {
                continue;
            }
            let Some(granted) = by_name.get(format!("{machine}-{hi}").as_str()) else {
                continue;
            };
            compared += 1;
            let (few, many) = (usable(withheld), usable(granted));
            let only_withheld: Vec<&&str> = few.difference(&many).collect();
            if !only_withheld.is_empty() {
                failures.push(format!(
                    "{name} lists usable what {machine}-{hi} does not, though it only withholds: \
                     {only_withheld:?}"
                ));
            }
            if strictly && few.len() >= many.len() {
                failures.push(format!(
                    "{name} and {machine}-{hi} list {} and {} usable sources: withholding {lo} \
                     changed nothing",
                    few.len(),
                    many.len()
                ));
            }
            for id in many.difference(&few) {
                let source = withheld
                    .sources
                    .iter()
                    .find(|source| source.id == *id)
                    .expect("both cells swept the same catalog");
                if listed_through_nobody(source) {
                    // Listed usable through a backend with no collector: the
                    // invariant judge's to refuse, and no machine's answer.
                    continue;
                }
                if answered_by_the_build(source) {
                    if same_build(withheld, granted) == Some(true) {
                        failures.push(format!(
                            "{name}: `{id}` is usable in {machine}-{hi} and not here, and no \
                             requirement says why, under the same build"
                        ));
                    } else {
                        builds.push(format!("{name} / {machine}-{hi}: `{id}`"));
                    }
                    continue;
                }
                let said: Vec<&str> = source
                    .requirements()
                    .filter(|r| r.state != State::Available)
                    .map(|r| r.detail.as_str())
                    .collect();
                if !said
                    .iter()
                    .any(|detail| names.iter().any(|word| detail.contains(word)))
                {
                    failures.push(format!(
                        "{name}: `{id}` is usable only in {machine}-{hi}, and no reason here \
                         names what {lo} withholds ({names:?}): {said:?}"
                    ));
                }
            }
            eprintln!(
                "listings: {name:<22} {:>3} usable, {machine}-{hi:<10} {:>3}",
                few.len(),
                many.len()
            );
        }
    }
    if !builds.is_empty() {
        eprintln!(
            "not compared, the build's and not the machine's (cells swept by two binaries, or \
             before cells recorded theirs): {}",
            builds.join("; ")
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    if compared == 0 {
        eprintln!("skipped: no two cells of one machine differ by a condition the matrix sets");
    }
}

/// What a refusal says decided it, read from the probe's own words: the
/// kernel's version, or the one or two fields of a resolved record's
/// `environment` that stand for what the probe found missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cause {
    /// The requirement names a kernel version, and the refusing machine's is
    /// older.
    KernelBelow((u32, u32)),
    /// Any of these fields decides it: `perf_event_open` at paranoia 2 is
    /// refused by the sysctl and granted by CAP_PERFMON, so either explains.
    Fields(&'static [&'static str]),
    /// Two refusals in one reason, either of which a machine can lift: a
    /// uprobe-multi link that Linux 6.6 introduced, and the uprobe PMU it
    /// falls back to below that, which only CAP_SYS_ADMIN opens. A newer
    /// kernel or a privilege explains the gap.
    KernelBelowOr((u32, u32), &'static [&'static str]),
}

/// The version in "..., which Linux 6.6 introduced", the words a probe uses
/// for a kernel feature it found missing.
fn introduced_in(detail: &str) -> Option<(u32, u32)> {
    let (_, rest) = detail.split_once("which Linux ")?;
    let (version, _) = rest.split_once(" introduced")?;
    kernel_version(version)
}

/// The cause a probe names for a requirement it did not answer available, or
/// `None` when its words name nothing a resolved record carries.
///
/// The order matters where a reason names two things: "cannot be asked
/// without loading a program, which needs CAP_BPF" is a privilege, although
/// it also says "this kernel".
fn cause(needs: &str, detail: &str) -> Option<Cause> {
    if let Some(version) = needs.strip_prefix("kernel>=") {
        return kernel_version(version).map(Cause::KernelBelow);
    }
    let says = |words: &[&str]| words.iter().any(|w| detail.contains(w));
    if let Some(version) = introduced_in(detail).filter(|_| says(&["CAP_"])) {
        return Some(Cause::KernelBelowOr(version, &["privileges"]));
    }
    Some(Cause::Fields(if says(&["/sys/kernel/btf/vmlinux"]) {
        &["kernel_btf"]
    } else if says(&["tracefs is not mounted"]) {
        &["tracefs"]
    } else if says(&["tracefs is mounted but not readable"]) {
        &["tracefs", "privileges"]
    } else if says(&["perf_event_paranoid"]) {
        &["perf_event_paranoid", "privileges"]
    } else if says(&["CAP_", "not readable by uid"]) {
        &["privileges"]
    } else if says(&[
        "this kernel declares",
        "this kernel defines no",
        "this kernel refused",
    ]) {
        &["kernel_release"]
    } else if needs == "nvml" || says(&["libnvidia-ml"]) {
        &["gpus"]
    } else if says(&[
        "does not exist",
        "publishes no",
        "no chip under",
        "no interface on this machine",
    ]) {
        // Hardware the machine does not have: a guest's virtual cpu publishes
        // no cpufreq, no hwmon chip, no PMU of the host's vendor.
        &["virtualization"]
    } else {
        return None;
    }))
}

/// A field that a record written before it existed leaves out, and that an
/// absent key cannot be read as a value of.
const RECORDED_SINCE_THE_FIFTEENTH_WAVE: [&str; 2] = ["privileges", "virtualization"];

/// What comparing two cells' resolved records found, source by source.
#[derive(Debug, Default)]
struct Gaps {
    /// By the field that explains each difference.
    explained: BTreeMap<String, usize>,
    /// Differences only a field the records do not carry could explain.
    unrecorded: BTreeMap<String, usize>,
    /// Differences the build made, between cells that record no common build.
    builds: usize,
    unexplained: Vec<String>,
}

fn environment_field(sweep: &Sweep, field: &str) -> Option<serde_json::Value> {
    match sweep.cell.environment.get(field) {
        Some(value) => Some(value.clone()),
        // Serialized only when not empty.
        None if field == "gpus" => Some(serde_json::Value::Array(Vec::new())),
        None => None,
    }
}

/// How one refusal's cause stands against the two records.
enum Verdict {
    Explained(&'static str),
    Unrecorded(&'static str),
    Unexplained(String),
}

/// Whether the cause the refusing side's probe named is borne out by the two
/// records: the field it names differs, the way that refuses on the refusing
/// side (a BTF or a tracefs that side's record calls readable explains
/// nothing), or the kernel it names is newer than the refusing side's and not
/// newer than the other's.
fn judge_cause(
    refusing: &Sweep,
    other: &Sweep,
    other_available: bool,
    requirement: &Requirement,
) -> Verdict {
    match cause(&requirement.needs, &requirement.detail) {
        None => Verdict::Unexplained(format!(
            "`{}` refused with a reason that names nothing a resolved record carries: {}",
            requirement.needs, requirement.detail
        )),
        Some(Cause::KernelBelow(needed)) => {
            judge_kernel(refusing, other, other_available, requirement, needed)
        }
        Some(Cause::Fields(fields)) => judge_fields(refusing, other, requirement, fields),
        Some(Cause::KernelBelowOr(needed, fields)) => {
            match judge_kernel(refusing, other, other_available, requirement, needed) {
                explained @ Verdict::Explained(_) => explained,
                _ => judge_fields(refusing, other, requirement, fields),
            }
        }
    }
}

/// A kernel version explains a refusal when it is newer than the refusing
/// side's and, where the other side served it, not newer than the other's.
fn judge_kernel(
    refusing: &Sweep,
    other: &Sweep,
    other_available: bool,
    requirement: &Requirement,
    needed: (u32, u32),
) -> Verdict {
    let release = |sweep: &Sweep| {
        environment_field(sweep, "kernel_release")
            .and_then(|value| value.as_str().and_then(kernel_version))
    };
    match (release(refusing), release(other)) {
        (Some(here), Some(there)) if here < needed && (!other_available || there >= needed) => {
            Verdict::Explained("kernel_release")
        }
        (here, there) => Verdict::Unexplained(format!(
            "`{}` refused on kernel {here:?} and {} on {there:?}",
            requirement.needs,
            if other_available {
                "available"
            } else {
                "refused too"
            }
        )),
    }
}

/// A field explains a refusal when it differs between the two records the way
/// that refuses on the refusing side.
fn judge_fields(
    refusing: &Sweep,
    other: &Sweep,
    requirement: &Requirement,
    fields: &'static [&'static str],
) -> Verdict {
    for field in fields {
        let (Some(x), Some(y)) = (
            environment_field(refusing, field),
            environment_field(other, field),
        ) else {
            continue;
        };
        let refuses_here =
            !matches!(*field, "kernel_btf" | "tracefs") || x != serde_json::json!("readable");
        if x != y && refuses_here {
            return Verdict::Explained(field);
        }
    }
    if let Some(field) = fields.iter().find(|field| {
        RECORDED_SINCE_THE_FIFTEENTH_WAVE.contains(field)
            && (environment_field(refusing, field).is_none()
                || environment_field(other, field).is_none())
    }) {
        return Verdict::Unrecorded(field);
    }
    Verdict::Unexplained(format!(
        "`{}` refused ({}), and none of {fields:?} differs between the two records the way \
         that refuses",
        requirement.needs, requirement.detail
    ))
}

/// For each source whose availability differs between two cells, the
/// recorded field of their resolved records that explains it: for every
/// requirement answered differently, the cause the refusing side's probe
/// names, borne out by the two records.
fn gaps_between(a: &Sweep, b: &Sweep, gaps: &mut Gaps) {
    for left in &a.sources {
        let Some(right) = b.sources.iter().find(|source| source.id == left.id) else {
            continue;
        };
        if left.here == right.here || listed_through_nobody(left) || listed_through_nobody(right) {
            continue;
        }
        let where_ = format!("{} / {}: `{}`", a.cell.name, b.cell.name, left.id);
        if differs_by_the_build(left, right) {
            if same_build(a, b) == Some(true) {
                gaps.unexplained.push(format!(
                    "{where_} differs by what the build answers, under one build"
                ));
            } else {
                gaps.builds += 1;
            }
            continue;
        }
        let mut verdicts = Vec::new();
        for backend in &left.backends {
            let Some(other) = right.backend(&backend.id) else {
                continue;
            };
            for mine in &backend.requires {
                let Some(theirs) = other
                    .requires
                    .iter()
                    .find(|r| r.needs == mine.needs && r.state != mine.state)
                else {
                    continue;
                };
                // When both refuse, differently, the difference is the
                // side whose cause the records bear out: a BTF that one side
                // could not read explains its `unverified` against the other
                // side's `missing`, whatever the other side's reason.
                let mut judged: Vec<Verdict> =
                    [(a, mine, b, theirs.state), (b, theirs, a, mine.state)]
                        .into_iter()
                        .filter(|(_, requirement, _, _)| requirement.state != State::Available)
                        .map(|(refusing, requirement, other, other_state)| {
                            judge_cause(
                                refusing,
                                other,
                                other_state == State::Available,
                                requirement,
                            )
                        })
                        .collect();
                judged.sort_by_key(|verdict| match verdict {
                    Verdict::Explained(_) => 0,
                    Verdict::Unrecorded(_) => 1,
                    Verdict::Unexplained(_) => 2,
                });
                verdicts.extend(judged.into_iter().next());
            }
        }
        if verdicts.is_empty() {
            gaps.unexplained.push(format!(
                "{where_} is {:?} and {:?} with every requirement answered alike",
                left.here, right.here
            ));
            continue;
        }
        let mut unexplained: Vec<String> = Vec::new();
        let mut unrecorded = std::collections::BTreeSet::new();
        let mut explained = std::collections::BTreeSet::new();
        for verdict in verdicts {
            match verdict {
                Verdict::Explained(field) => {
                    explained.insert(field);
                }
                Verdict::Unrecorded(field) => {
                    unrecorded.insert(field);
                }
                Verdict::Unexplained(why) => unexplained.push(why),
            }
        }
        if !unexplained.is_empty() {
            unexplained.dedup();
            gaps.unexplained
                .push(format!("{where_}: {}", unexplained.join("; ")));
        } else if !unrecorded.is_empty() {
            let key = unrecorded.into_iter().collect::<Vec<_>>().join(" and ");
            *gaps.unrecorded.entry(key).or_default() += 1;
        } else {
            for field in explained {
                *gaps.explained.entry(field.to_owned()).or_default() += 1;
            }
        }
    }
}

/// ROADMAP phase 7: "the resolved file is enough to explain a gap between two
/// machines", over every two cells on disk.
///
/// For each source listed usable in one cell and not in the other, the
/// requirements that answered differently are found in the two listings. The
/// refusing side's probe names its cause in its own words (a BTF it could not
/// read, a capability, a sysctl, a kernel that defines no such tracepoint,
/// hardware the machine does not publish), and the field of the two resolved
/// records' `environment` that stands for that cause -- the kernel release,
/// the BTF, tracefs, `perf_event_paranoid`, the privileges, the
/// virtualization, the GPUs -- has to differ the way that refuses on the
/// refusing side; a `kernel>=` requirement has to be newer than the refusing
/// side's release and not newer than the other's. A field that merely differs
/// explains nothing. A gap whose cause the records do not bear out, or whose
/// reason names nothing a record carries, fails, naming the source and the two
/// cells. Fields a record written before the
/// fifteenth wave does not carry (`privileges`, `virtualization`) are counted
/// and said, not failed, until the matrix is swept again.
#[test]
fn every_gap_between_two_cells_is_explained_by_their_resolved_records() {
    // The cells of one userspace, the virtme-ng guests' and the host's: there
    // a gap is the kernel's or the configuration's, which the resolved records
    // carry. A distroproof cell (an `envprobe.json` beside it) differs from
    // another by its distribution too -- ftrace built or not, cgroup v2
    // mounted or not -- facts envprobe's report carries and these records do
    // not, so those are judged against envprobe (docs/distroproof.md).
    let cells: Vec<_> = cells_on_disk()
        .into_iter()
        .filter(|(path, _)| !path.with_file_name("envprobe.json").exists())
        .collect();
    if cells.len() < 2 {
        eprintln!(
            "skipped: {} matrix cell(s) under {}, and a gap needs two",
            cells.len(),
            cells_dir().display()
        );
        return;
    }
    let mut gaps = Gaps::default();
    for (index, (_, a)) in cells.iter().enumerate() {
        for (_, b) in &cells[index + 1..] {
            gaps_between(a, b, &mut gaps);
        }
    }
    eprintln!(
        "gaps between {} cells: explained by {:?}; the build's, between cells that record no \
         common build: {}",
        cells.len(),
        gaps.explained,
        gaps.builds
    );
    if !gaps.unrecorded.is_empty() {
        eprintln!(
            "NOT EXPLAINED YET, the records predate the field that would: {:?} -- sweep the \
             matrix again (`scripts/matrix.sh`)",
            gaps.unrecorded
        );
    }
    assert!(
        gaps.unexplained.is_empty(),
        "{} gap(s) no field of the resolved records explains:\n{}",
        gaps.unexplained.len(),
        gaps.unexplained.join("\n")
    );
    assert!(
        !gaps.explained.is_empty(),
        "no gap between {} cells was explained, so this compared nothing",
        cells.len()
    );
}

/// A binary that carries `cap_bpf,cap_perfmon` and runs where the kernel does
/// not grant them -- here under `no_new_privs`, which a sandbox sets and which
/// drops file capabilities at exec the way a `nosuid` mount does -- says why,
/// and does not tell the reader to run the `setcap` they already ran.
///
/// The truth is the kernel's: the flag is set on the child before it execs,
/// and `getcap` says what the file carries.
#[test]
fn a_binary_denied_its_file_capabilities_says_why_instead_of_advising_the_grant() {
    let omn = omn();
    let printed = Command::new("getcap")
        .arg(&omn)
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    if !printed.contains("cap_bpf") {
        eprintln!(
            "skipped: {} carries no CAP_BPF ({printed:?}), so nothing can be withheld from it -- \
             `make` grants it",
            omn.display()
        );
        return;
    }
    let mut command = Command::new(&omn);
    command.args(["list", "sched.offcpu", "--color", "never"]);
    // SAFETY: prctl is async-signal-safe, and the flag only narrows the child.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, || {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let output = command.output().expect("omn runs");
    let listing = String::from_utf8_lossy(&output.stdout);
    let bpf = listing
        .lines()
        .find(|line| line.trim_start().starts_with("needs") && line.contains(" bpf "))
        .unwrap_or_else(|| panic!("the listing names the `bpf` requirement:\n{listing}"));
    assert!(bpf.contains("missing"), "{bpf}");
    assert!(
        bpf.contains("carries CAP_BPF") && bpf.contains("no_new_privs") && !bpf.contains("setcap"),
        "{bpf}"
    );
}

// ------------------------------------------------------------ the judges, judged

/// A cell with one source of each shape, every answer honest. Each test below
/// breaks one thing in it and requires the judge responsible to say so -- and
/// the others to stay quiet, so a judge that flags everything fails too.
fn honest_cell(kernel: &str, config: &str) -> Sweep {
    let requirement = |needs: &str, state: State, detail: &str| Requirement {
        needs: needs.into(),
        state,
        detail: detail.into(),
    };
    let run = |target: &str, backend: Option<&str>, code: i32, stdout: &str, stderr: &str| Run {
        target: target.into(),
        args: vec!["trace".into()],
        backend: backend.map(Into::into),
        code: Some(code),
        elapsed_ms: 300,
        stdout: stdout.into(),
        stderr: stderr.into(),
    };
    Sweep {
        cell: Cell {
            name: format!("{kernel}-{config}"),
            kernel_release: format!("{kernel}.0-test"),
            config: config.into(),
            uid: 1000,
            btf_hidden: false,
            tracefs_hidden: false,
            environment: serde_json::Value::Null,
            elapsed_s: 1.0,
            build: Some("honest".into()),
        },
        sources: vec![
            Source {
                id: "sched.lock".into(),
                here: Here::Yes,
                here_backend: Some("bpf:raw_tp".into()),
                backends: vec![Backend {
                    id: "bpf:raw_tp".into(),
                    collector: true,
                    degraded: None,
                    requires: vec![
                        requirement("btf", State::Available, ""),
                        requirement("raw_tracepoint:contention_begin", State::Available, ""),
                        requirement("kernel>=6.1", State::Available, ""),
                    ],
                }],
                runs: vec![run(
                    "system",
                    Some("bpf:raw_tp"),
                    0,
                    "sched.lock via bpf:raw_tp",
                    "",
                )],
                listing: String::new(),
                degraded: BTreeMap::new(),
            },
            Source {
                id: "cpu.flow".into(),
                here: Here::No,
                here_backend: None,
                backends: vec![Backend {
                    id: "pmu:aux:intel_pt".into(),
                    collector: false,
                    degraded: None,
                    requires: vec![requirement(
                        "pmu:intel_pt",
                        State::Missing,
                        "this machine publishes no `intel_pt` performance unit",
                    )],
                }],
                runs: vec![run(
                    "launch",
                    None,
                    1,
                    "",
                    "error: `cpu.flow` has no viable backend on this machine",
                )],
                listing: String::new(),
                degraded: BTreeMap::new(),
            },
            Source {
                id: "cpu.cycles".into(),
                here: Here::Yes,
                here_backend: Some("pmu:sw".into()),
                backends: vec![
                    Backend {
                        id: "pmu".into(),
                        collector: true,
                        degraded: None,
                        requires: vec![requirement(
                            "pmu:PERF_COUNT_HW_CPU_CYCLES",
                            State::Missing,
                            "the kernel refused the event",
                        )],
                    },
                    Backend {
                        id: "pmu:sw".into(),
                        collector: true,
                        degraded: Some("CPU time, not cycles".into()),
                        requires: vec![requirement(
                            "pmu:PERF_COUNT_SW_CPU_CLOCK",
                            State::Available,
                            "",
                        )],
                    },
                ],
                runs: vec![run(
                    "launch",
                    Some("pmu:sw"),
                    0,
                    "cpu.cycles via pmu:sw\n  degraded: pmu:sw: CPU time, not cycles",
                    "",
                )],
                listing: String::new(),
                degraded: BTreeMap::new(),
            },
            Source {
                id: "runtime.custom".into(),
                here: Here::Yes,
                here_backend: Some("bpf:usdt".into()),
                backends: vec![Backend {
                    id: "bpf:usdt".into(),
                    collector: true,
                    degraded: None,
                    requires: vec![requirement("uprobe", State::Available, "")],
                }],
                runs: vec![run(
                    "launch",
                    Some("bpf:usdt"),
                    0,
                    "runtime.custom via bpf:usdt",
                    "",
                )],
                listing: String::new(),
                degraded: BTreeMap::new(),
            },
        ],
    }
}

fn violations(sweep: &Sweep, known: &[Known]) -> BTreeMap<&'static str, Vec<String>> {
    judge(sweep, known)
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .collect()
}

fn source<'a>(sweep: &'a mut Sweep, id: &str) -> &'a mut Source {
    sweep
        .sources
        .iter_mut()
        .find(|s| s.id == id)
        .expect("the honest cell has it")
}

#[test]
fn an_honest_cell_passes_every_judge() {
    let cell = honest_cell("7.2", "caps");
    assert!(
        violations(&cell, &[]).is_empty(),
        "{:#?}",
        violations(&cell, &[])
    );
}

#[test]
fn a_source_listed_usable_that_refuses_at_attach_fails_the_cell() {
    let mut cell = honest_cell("7.2", "caps");
    let run = &mut source(&mut cell, "sched.lock").runs[0];
    run.code = Some(1);
    run.stderr = "error: cannot start collecting `sched.lock`: EACCES".into();
    let found = violations(&cell, &[]);
    assert_eq!(
        found.keys().copied().collect::<Vec<_>>(),
        ["available means it attaches"]
    );
    assert!(found["available means it attaches"][0].contains("sched.lock (system)"));
}

#[test]
fn a_backend_without_a_collector_is_out_of_reach_and_not_an_attach_failure() {
    // Vocabulary: every requirement here, and nothing collects it. The listing
    // calls it out of reach and names the backend's missing collector; the run
    // is the build's refusal (exit 3), not the kernel's.
    let mut cell = honest_cell("7.2", "caps");
    let lock = source(&mut cell, "sched.lock");
    lock.here = Here::No;
    lock.here_backend = None;
    lock.backends[0].collector = false;
    lock.runs[0].backend = None;
    lock.runs[0].code = Some(3);
    lock.runs[0].stderr =
        "error: no collector is written for `sched.lock` on backend `bpf:raw_tp` yet".into();
    assert!(
        violations(&cell, &[]).is_empty(),
        "{:#?}",
        violations(&cell, &[])
    );
}

/// The CI's second run: `cpu.cycles` on a cpu with no hardware PMU was listed
/// usable through `pmu:sw`, the one backend of its chain with no collector, and
/// every run of it exited 3. Available means it attaches.
#[test]
fn a_source_listed_usable_through_a_backend_nobody_wrote_fails_the_cell() {
    let mut cell = honest_cell("7.2", "caps");
    let cycles = source(&mut cell, "cpu.cycles");
    cycles.backends[1].collector = false;
    cycles.runs[0].code = Some(3);
    cycles.runs[0].stdout = String::new();
    cycles.runs[0].stderr =
        "error: no collector is written for `cpu.cycles` on backend `pmu:sw` yet".into();
    let found = violations(&cell, &[]);
    assert!(
        found
            .get("every answer has its reason")
            .is_some_and(|v| v.iter().any(|v| v.contains("no collector in this build"))),
        "{found:#?}"
    );
}

/// The gap judge, judged: two cells where one hides the BTF. With records
/// that say nothing, the gap is unexplained and fails; with records that carry
/// `kernel_btf`, it is explained by that field and nothing else; and a gap
/// that only privileges could explain, between records written before they
/// were, is counted as unrecorded rather than failed or passed.
#[test]
fn a_gap_is_explained_only_by_a_recorded_field_that_differs() {
    let a = honest_cell("6.6", "root");
    let mut b = honest_cell("6.6", "nobtf");
    let lock = source(&mut b, "sched.lock");
    lock.here = Here::No;
    lock.here_backend = None;
    for requirement in lock
        .backends
        .iter_mut()
        .flat_map(|backend| &mut backend.requires)
    {
        if requirement.needs == "btf" {
            requirement.state = State::Missing;
            requirement.detail = "/sys/kernel/btf/vmlinux is absent".into();
        }
    }
    let mut silent = Gaps::default();
    gaps_between(&a, &b, &mut silent);
    assert_eq!(silent.unexplained.len(), 1, "{silent:?}");
    assert!(silent.unexplained[0].contains("sched.lock"), "{silent:?}");

    let (mut a, mut b) = (a, b);
    a.cell.environment = serde_json::json!({"kernel_btf": "readable", "tracefs": "readable"});
    b.cell.environment = serde_json::json!({"kernel_btf": "absent", "tracefs": "readable"});
    let mut recorded = Gaps::default();
    gaps_between(&a, &b, &mut recorded);
    assert!(recorded.unexplained.is_empty(), "{recorded:?}");
    assert_eq!(
        recorded.explained.get("kernel_btf"),
        Some(&1),
        "{recorded:?}"
    );

    // A `bpf` refusal, which only the privileges explain, between records
    // that do not carry them.
    let lock = source(&mut b, "sched.lock");
    for requirement in lock
        .backends
        .iter_mut()
        .flat_map(|backend| &mut backend.requires)
    {
        *requirement = if requirement.needs == "btf" {
            Requirement {
                needs: "bpf".into(),
                state: State::Missing,
                detail: "loading a tracing program needs CAP_BPF".into(),
            }
        } else {
            requirement.clone()
        };
    }
    for requirement in source(&mut a, "sched.lock")
        .backends
        .iter_mut()
        .flat_map(|backend| &mut backend.requires)
    {
        if requirement.needs == "btf" {
            requirement.needs = "bpf".into();
        }
    }
    b.cell.environment = a.cell.environment.clone();
    let mut unrecorded = Gaps::default();
    gaps_between(&a, &b, &mut unrecorded);
    assert!(unrecorded.unexplained.is_empty(), "{unrecorded:?}");
    assert_eq!(
        unrecorded.unrecorded.get("privileges"),
        Some(&1),
        "{unrecorded:?}"
    );
}

/// A field that differs is not the cause because it differs: the cause is the
/// one the refusing probe names, and the records have to bear it out. Each
/// case below was credited to the wrong field by a test that took the first
/// differing field in alphabetical order.
#[test]
fn a_gap_is_credited_to_the_cause_the_refusal_names_not_to_any_field_that_differs() {
    // One requirement of `sched.lock` refused on `b` with `detail`, between
    // two records.
    let pair = |kernels: (&str, &str), needs: &str, detail: &str, env: [serde_json::Value; 2]| {
        let mut a = honest_cell(kernels.0, "caps");
        let mut b = honest_cell(kernels.1, "nocaps");
        let [env_a, env_b] = env;
        a.cell.environment = env_a;
        b.cell.environment = env_b;
        for (sweep, state) in [(&mut a, State::Available), (&mut b, State::Missing)] {
            let lock = source(sweep, "sched.lock");
            let requires = &mut lock.backends[0].requires;
            requires.retain(|r| r.needs != needs);
            requires.push(Requirement {
                needs: needs.into(),
                state,
                detail: if state == State::Available {
                    String::new()
                } else {
                    detail.into()
                },
            });
            if state == State::Missing {
                lock.here = Here::No;
                lock.here_backend = None;
            }
        }
        let mut gaps = Gaps::default();
        gaps_between(&a, &b, &mut gaps);
        gaps
    };
    let env = |release: &str, gpus: bool| {
        let mut value = serde_json::json!({
            "kernel_release": release, "kernel_btf": "readable", "tracefs": "readable",
            "perf_event_paranoid": 2,
        });
        if gpus {
            value["gpus"] = serde_json::json!([{"card": "card1", "driver": "nvidia"}]);
        }
        value
    };

    // The capability config differs between two kernels: `bpf` refused for
    // CAP_BPF is the privileges', which these records do not carry, and not
    // the kernel release's, which differs too.
    let bpf = pair(
        ("6.1", "6.6"),
        "bpf",
        "loading a tracing program needs CAP_BPF and CAP_PERFMON",
        [env("6.1.0-test", false), env("6.6.0-test", false)],
    );
    assert!(bpf.explained.is_empty(), "{bpf:?}");
    assert_eq!(bpf.unrecorded.get("privileges"), Some(&1), "{bpf:?}");

    // No hwmon chip in a guest is its virtualization, which these records
    // do not carry, and not its GPUs, which differ too.
    let hwmon = pair(
        ("7.2", "7.2"),
        "/sys/class/hwmon",
        "no chip under /sys/class/hwmon publishes a temperature",
        [env("7.2.6-test", true), env("7.2.6-test", false)],
    );
    assert!(hwmon.explained.is_empty(), "{hwmon:?}");
    assert_eq!(
        hwmon.unrecorded.get("virtualization"),
        Some(&1),
        "{hwmon:?}"
    );

    // A kernel requirement refused on the newer kernel is not explained by
    // the release, although the releases differ.
    let backwards = pair(
        ("6.6", "5.15"),
        "kernel>=6.1",
        "kernel 6.6 is older than 6.1",
        [env("5.15.0-test", false), env("6.6.0-test", false)],
    );
    assert_eq!(backwards.unexplained.len(), 1, "{backwards:?}");
    let forwards = pair(
        ("6.6", "5.15"),
        "kernel>=6.1",
        "kernel 5.15 is older than 6.1",
        [env("6.6.0-test", false), env("5.15.0-test", false)],
    );
    assert_eq!(
        forwards.explained.get("kernel_release"),
        Some(&1),
        "{forwards:?}"
    );

    // A BTF that the refusing side's record calls readable explains nothing,
    // even where the other side's differs.
    let mut btf_backwards = [env("6.6.0-test", false), env("6.6.0-test", false)];
    btf_backwards[0]["kernel_btf"] = serde_json::json!("absent");
    let btf = pair(
        ("6.6", "6.6"),
        "btf",
        "/sys/kernel/btf/vmlinux is absent, so CO-RE is unavailable",
        btf_backwards,
    );
    assert_eq!(btf.unexplained.len(), 1, "{btf:?}");

    // A reason that names nothing a record carries fails, rather than being
    // credited to whatever differs.
    let unnamed = pair(
        ("6.6", "7.2"),
        "bpf",
        "something went wrong",
        [env("6.6.0-test", false), env("7.2.6-test", false)],
    );
    assert_eq!(unnamed.unexplained.len(), 1, "{unnamed:?}");
}

#[test]
fn a_known_refusal_passes_only_while_it_says_what_was_recorded() {
    let known = [Known {
        finding: "a test entry",
        applies: everywhere,
        sources: &["sched.lock"],
        targets: &["system"],
        says: "CAP_SYS_ADMIN",
    }];
    let mut cell = honest_cell("7.2", "caps");
    // Recorded, and it attached: the entry is stale.
    assert!(violations(&cell, &known)["available means it attaches"][0].contains("remove"));
    let run = &mut source(&mut cell, "sched.lock").runs[0];
    run.code = Some(1);
    run.stderr = "the uprobe PMU needs CAP_SYS_ADMIN".into();
    assert!(violations(&cell, &known).is_empty());
    source(&mut cell, "sched.lock").runs[0].stderr = "something else entirely".into();
    assert!(violations(&cell, &known)["available means it attaches"][0].contains("not as"));
}

#[test]
fn an_answer_without_its_reason_fails_the_cell() {
    let mut cell = honest_cell("7.2", "caps");
    source(&mut cell, "cpu.flow").backends[0].requires[0].detail = String::new();
    let found = violations(&cell, &[]);
    assert_eq!(
        found.keys().copied().collect::<Vec<_>>(),
        ["every answer has its reason"]
    );
}

#[test]
fn unknown_listed_as_usable_fails_the_cell() {
    let mut cell = honest_cell("7.2", "caps");
    let lock = source(&mut cell, "sched.lock");
    lock.backends[0].requires[1].state = State::Unverified;
    lock.backends[0].requires[1].detail = "no probe is written".into();
    let found = violations(&cell, &[]);
    assert!(found["every answer has its reason"]
        .iter()
        .any(|v| v.contains("not all available")));
}

#[test]
fn an_unavailable_source_that_runs_anyway_fails_the_cell() {
    let mut cell = honest_cell("7.2", "caps");
    let run = &mut source(&mut cell, "cpu.flow").runs[0];
    run.code = Some(0);
    run.stdout = "cpu.flow via something else".into();
    let found = violations(&cell, &[]);
    assert_eq!(
        found.keys().copied().collect::<Vec<_>>(),
        ["out of reach is refused, with why"]
    );
}

#[test]
fn a_fallback_that_does_not_say_what_it_loses_fails_the_cell() {
    let mut cell = honest_cell("7.2", "caps");
    source(&mut cell, "cpu.cycles").runs[0].stdout = "cpu.cycles via pmu:sw".into();
    let found = violations(&cell, &[]);
    assert_eq!(
        found.keys().copied().collect::<Vec<_>>(),
        ["a fallback says what it loses"]
    );
}

/// The hosted image as shipped refuses every perf event: a `pmu:` requirement
/// that reads available there, or missing without the sysctl's name, is a
/// probe that did not ask.
#[test]
fn a_stock_cell_must_show_every_perf_event_missing_by_the_sysctl() {
    let mut cell = honest_cell("6.17", "stock");
    let found = violations(&cell, &[]);
    assert!(
        found["a removed requirement is named"]
            .iter()
            .any(|v| v.contains("PERF_COUNT_SW_CPU_CLOCK") && v.contains("perf_event_paranoid")),
        "{found:#?}"
    );
    let cycles = source(&mut cell, "cpu.cycles");
    cycles.backends[0].requires[0].detail =
        "perf_event_open was refused for `PERF_COUNT_HW_CPU_CYCLES`: perf_event_paranoid is 4"
            .into();
    cycles.backends[1].requires[0].state = State::Missing;
    cycles.backends[1].requires[0].detail =
        "perf_event_open was refused for `PERF_COUNT_SW_CPU_CLOCK`: perf_event_paranoid is 4"
            .into();
    cycles.here = Here::No;
    cycles.here_backend = None;
    cycles.runs[0].backend = None;
    cycles.runs[0].code = Some(1);
    cycles.runs[0].stdout = String::new();
    cycles.runs[0].stderr = "error: `cpu.cycles` has no viable backend on this machine".into();
    assert!(
        !violations(&cell, &[]).contains_key("a removed requirement is named"),
        "{:#?}",
        violations(&cell, &[])
    );
}

#[test]
fn hiding_btf_must_show_as_btf_missing_by_name() {
    let mut cell = honest_cell("7.2", "nobtf");
    cell.cell.btf_hidden = true;
    // The honest cell's BTF requirement still reads available: a cell where
    // hiding the file changed nothing the tool says.
    let found = violations(&cell, &[]);
    assert!(found["a removed requirement is named"]
        .iter()
        .any(|v| v.contains("`btf`")));
    for requirement in &mut source(&mut cell, "sched.lock").backends[0].requires {
        if requirement.needs == "btf" || requirement.needs.starts_with("raw_tracepoint:") {
            requirement.state = State::Missing;
            requirement.detail =
                "/sys/kernel/btf/vmlinux is absent, so CO-RE is unavailable".into();
        }
    }
    let lock = source(&mut cell, "sched.lock");
    lock.here = Here::No;
    lock.here_backend = None;
    lock.runs[0].code = Some(1);
    lock.runs[0].stderr = "error: `sched.lock` has no viable backend on this machine".into();
    assert!(
        violations(&cell, &[]).is_empty(),
        "{:#?}",
        violations(&cell, &[])
    );
}

#[test]
fn below_the_floor_a_version_requirement_names_the_kernel() {
    let mut cell = honest_cell("5.15", "caps");
    // Its USDT source would be judged by the 6.6 rule; this test is the floor.
    cell.sources.retain(|s| s.id != "runtime.custom");
    // The honest cell says 6.1 is available: on 5.15 that is the defect.
    assert!(violations(&cell, &[]).contains_key("the floor is refused by name"));
    let lock = source(&mut cell, "sched.lock");
    lock.backends[0].requires[2].state = State::Missing;
    lock.backends[0].requires[2].detail = "kernel 5.15 is older than 6.1".into();
    lock.here = Here::No;
    lock.here_backend = None;
    lock.runs[0].code = Some(1);
    lock.runs[0].stderr = "error: `sched.lock` has no viable backend on this machine".into();
    assert!(
        violations(&cell, &[]).is_empty(),
        "{:#?}",
        violations(&cell, &[])
    );
}

/// Below 6.6 a USDT source may attach through the multi link without falling
/// back -- Rocky 9's 5.14 carries uprobe-multi backported -- or fall back and
/// say so; on 6.6 and later a fallback is the defect.
#[test]
fn a_usdt_source_falls_back_from_uprobe_multi_only_where_the_kernel_lacks_it() {
    let mut cell = honest_cell("6.1", "root");
    assert!(
        violations(&cell, &[]).is_empty(),
        "a backport is not a defect"
    );
    source(&mut cell, "runtime.custom").runs[0].stderr =
        "note: this kernel refused a uprobe-multi link (EINVAL), which Linux 6.6 introduced".into();
    assert!(violations(&cell, &[]).is_empty());
    // And the same words on 6.6 are the defect the other way.
    cell.cell.kernel_release = "6.6.0-test".into();
    assert!(violations(&cell, &[]).contains_key("uprobe-multi is 6.6"));
    // A caveat naming uprobe-multi is not a fallback.
    source(&mut cell, "runtime.custom").runs[0].stderr =
        "caveat: Linux 6.6 or later: every site is linked with a uprobe-multi link".into();
    assert!(violations(&cell, &[]).is_empty());
}

#[test]
fn the_listing_parser_reads_every_answer_kind() {
    let text = "\
sched.lock — Kernel lock contention
  here          usable through `bpf:raw_tp`

  backends (ordered; the resolver keeps the first viable one)
    1. bpf:raw_tp             cost med    collector written
       needs    bpf                                    missing: needs CAP_BPF (`unprivileged_bpf_disabled` is 2, so bpf() needs CAP_BPF). Grant them
       needs    kernel>=6.1                            available
       needs    raw_tracepoint:contention_end(void *, int) missing: this kernel declares `contention_end(void *)`
    2. bpf:tp                 cost med    no collector yet
       needs    tracepoint:lock:contention_begin       missing: tracefs is not mounted
       needs    symbols:x                              unverified: no probe is written for `symbols:x` yet
       degraded older kernels
";
    let (here, backend, backends) = parse_listing(text);
    assert_eq!(here, Here::Yes);
    assert_eq!(backend.as_deref(), Some("bpf:raw_tp"));
    assert_eq!(backends.len(), 2);
    assert!(backends[0].collector && !backends[1].collector);
    // A reason with a full stop in it is a reason, not a backend.
    assert_eq!(backends[0].requires.len(), 3);
    assert_eq!(backends[0].requires[0].state, State::Missing);
    assert!(backends[0].requires[0].detail.ends_with("Grant them"));
    // A prototype has spaces in it, and is still one requirement.
    assert_eq!(
        backends[0].requires[2].needs,
        "raw_tracepoint:contention_end(void *, int)"
    );
    assert_eq!(backends[0].requires[2].state, State::Missing);
    assert_eq!(backends[1].requires[0].state, State::Missing);
    assert_eq!(backends[1].requires[0].detail, "tracefs is not mounted");
    assert_eq!(backends[1].requires[1].state, State::Unverified);
    assert_eq!(backends[1].requires[1].needs, "symbols:x");
}

/// `omn selftest --format json` is a cell the judges read: the command a
/// distroproof guest or a user runs writes the document this file judges, so
/// the two cannot drift apart. One source, so it costs a second.
#[test]
fn a_selftest_of_one_source_is_a_cell_the_judges_read() {
    let output = Command::new(omn())
        .args([
            "selftest",
            "--format",
            "json",
            "--sources",
            "cpu.util",
            "-d",
            "200ms",
        ])
        .output()
        .expect("omn selftest runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut sweep: Sweep =
        serde_json::from_slice(&output.stdout).expect("the selftest's document is a cell");
    sweep.sources.iter_mut().for_each(Source::reparse);
    assert_eq!(sweep.cell.name, "here-host");
    assert_eq!(sweep.sources.len(), 1);
    let source = &sweep.sources[0];
    assert_eq!(source.id, "cpu.util");
    assert_eq!(source.here, Here::Yes, "{}", source.listing);
    assert!(
        source.runs.iter().any(Run::attached),
        "cpu.util reads /proc/stat and attaches anywhere: {:?}",
        source.runs
    );
    assert!(judge(&sweep, KNOWN).values().all(Vec::is_empty));
}

/// The parser is checked against the binary's real listing too: a change to
/// the listing's layout must fail here, not turn every cell into `no`.
#[test]
fn the_listing_parser_reads_the_real_listing() {
    let output = Command::new(omn())
        .args(["list", "sched.lock", "--color", "never"])
        .output()
        .expect("omn runs");
    let text = String::from_utf8_lossy(&output.stdout);
    let (_, _, backends) = parse_listing(&text);
    let catalog = Command::new(omn())
        .args(["list", "sched.lock", "--format", "json"])
        .output()
        .expect("omn runs");
    let catalog: Vec<serde_json::Value> = serde_json::from_slice(&catalog.stdout).expect("JSON");
    let declared: Vec<(String, usize)> = catalog[0]["backends"]
        .as_array()
        .expect("backends")
        .iter()
        .map(|b| {
            (
                b["id"].as_str().expect("id").to_owned(),
                b["requires"].as_array().map_or(0, Vec::len),
            )
        })
        .collect();
    let parsed: Vec<(String, usize)> = backends
        .iter()
        .map(|b| (b.id.clone(), b.requires.len()))
        .collect();
    assert_eq!(parsed, declared, "the listing:\n{text}");
}

#[test]
fn kernel_releases_parse_the_way_the_cells_write_them() {
    assert_eq!(kernel_version("6.1.188-0601188-generic"), Some((6, 1)));
    assert_eq!(kernel_version("7.2.6-arch2-1"), Some((7, 2)));
    assert_eq!(kernel_version("6.12.0"), Some((6, 12)));
    assert_eq!(kernel_version("garbage"), None);
}

// ------------------------------------------------------------ what the cells found

// Which cells an entry applies to. A guest cell is named after its kernel; the
// host's and the CI's are named `here-*` and `ci-*`.
fn everywhere(_: &Sweep) -> bool {
    true
}
fn in_a_guest(sweep: &Sweep) -> bool {
    sweep.cell.name.starts_with(|c: char| c.is_ascii_digit())
}

/// `docs/portability.md`, "What the cells found", has each label: what the
/// cells showed, why, and the request filed for it.
const KNOWN_DEFECTS: &[Known] = &[Known {
    finding: "P10 (the guest's own shape: one root cgroup, no cpufreq)",
    applies: in_a_guest,
    sources: &["mem.cgroup"],
    targets: &["launch"],
    says: "is the hierarchy root",
}];

#[test]
fn a_refusal_with_two_causes_is_explained_by_either() {
    let uprobe = "this kernel refused a uprobe-multi link (Invalid argument (os error 22)), \
                  which Linux 6.6 introduced; and the uprobe PMU it would fall back to refuses \
                  this process (Permission denied (os error 13)): the kernel opens a `uprobe` \
                  PMU event only for CAP_SYS_ADMIN, which CAP_BPF and CAP_PERFMON do not include";
    assert_eq!(
        cause("uprobe", uprobe),
        Some(Cause::KernelBelowOr((6, 6), &["privileges"]))
    );
    // A privilege alone stays a privilege.
    assert_eq!(
        cause("bpf", "needs CAP_BPF, which this kernel requires"),
        Some(Cause::Fields(&["privileges"]))
    );
}

#[test]
fn an_unwritten_probe_answered_alike_on_both_sides_is_not_the_build_s_difference() {
    let requirement = |needs: &str, state: State, detail: &str| Requirement {
        needs: needs.into(),
        state,
        detail: detail.into(),
    };
    let hwts = |phc: State, here: Here| Source {
        id: "net.hwts".into(),
        here,
        here_backend: None,
        degraded: BTreeMap::new(),
        listing: String::new(),
        runs: Vec::new(),
        backends: vec![Backend {
            id: "so_timestamping".into(),
            collector: false,
            degraded: None,
            requires: vec![
                requirement(
                    "so_timestamping",
                    State::Unverified,
                    "no probe is written for `so_timestamping` yet",
                ),
                requirement(
                    "phc",
                    phc,
                    "no interface on this machine keeps a readable PTP clock",
                ),
            ],
        }],
    };
    let (guest, host) = (
        hwts(State::Missing, Here::No),
        hwts(State::Available, Here::Yes),
    );
    assert!(!differs_by_the_build(&guest, &host));
    let mut written = host.clone();
    written.backends[0].requires[0].state = State::Available;
    assert!(differs_by_the_build(&guest, &written));
}
