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
//! - **`sweep_this_machine`** is `#[ignore]`: it measures whatever machine it
//!   runs on and writes the cell's documents under `target/matrix/cells/`.
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
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
            let mut depth = 0usize;
            let end = rest
                .char_indices()
                .find(|&(_, c)| {
                    match c {
                        '(' => depth += 1,
                        ')' => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                    depth == 0 && c.is_whitespace()
                })
                .map_or(rest.len(), |(at, _)| at);
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

/// Runs a command with a watchdog, and returns its exit code (`None` when it
/// had to be killed), its output and how long it took.
fn run_bounded(command: &mut Command, limit: Duration) -> (Option<i32>, String, String, u64) {
    let started = Instant::now();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("cannot run {command:?}: {error}"));
    // Read both pipes on threads, so a chatty child cannot fill one and block
    // while the watchdog waits on the other.
    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let out = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = out.read_to_end(&mut buffer);
        buffer
    });
    let err = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = err.read_to_end(&mut buffer);
        buffer
    });
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if started.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => panic!("waiting on {command:?}: {error}"),
        }
    };
    let out = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
    let err = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
    (code, out, err, started.elapsed().as_millis() as u64)
}

/// The program a launched run starts: a little cpu, a few files, a sleep --
/// enough for every launch-capable collector to have something to attach to,
/// and long enough that the run's duration ends it rather than its exit.
const WORKLOAD: [&str; 3] = [
    "/bin/sh",
    "-c",
    "i=0; while [ $i -lt 3000 ]; do i=$((i+1)); done; cat /proc/self/status >/dev/null; \
     ls / >/dev/null; sleep 2",
];

/// One way to run a source: the target kind, what goes before the options
/// the sweep chooses (`trace <id>` and any filter), and the target itself,
/// which goes last because a launched program's `--` ends the options.
struct Request {
    target: String,
    head: Vec<String>,
    tail: Vec<String>,
}

/// The requests a source is run with: one per target kind that needs no
/// setup.
///
/// A few sources are about a property of the target rather than the machine
/// -- USDT probes a program has to carry -- and are given the repository's own
/// fixtures, which `scripts/matrix.sh` builds. Without the fixture, the run
/// refuses by naming the probes it looked for, and the judge reports it.
fn requests(entry: &serde_json::Value) -> Vec<Request> {
    let id = entry["id"].as_str().unwrap_or_default();
    let targets = entry["targets"].as_object().cloned().unwrap_or_default();

    let fixture = root().join("target/omn-usdt-fixture/fixture");
    let fixture = fixture.to_string_lossy().into_owned();
    let server = root().join("target/omn-vertical/server");
    let server = server.to_string_lossy().into_owned();
    let (launch, extra): (Vec<String>, Vec<String>) = match id {
        "runtime.gc" => (vec![fixture, "gc".into(), "5".into(), "20".into()], vec![]),
        "runtime.alloc" => (vec![fixture, "alloc".into(), "50".into()], vec![]),
        "runtime.custom" => (
            vec![fixture, "ticks".into(), "50".into()],
            vec![
                "--filter".into(),
                "provider == 'omn_fixture' && probe == 'tick'".into(),
            ],
        ),
        "net.request" => (
            vec![server, "/tmp/omn-matrix-server-data".into(), "0:0".into()],
            vec![],
        ),
        _ => (WORKLOAD.iter().map(|s| s.to_string()).collect(), vec![]),
    };
    let head: Vec<String> = ["trace".to_owned(), id.to_owned()]
        .into_iter()
        .chain(extra)
        .collect();

    let mut out = Vec::new();
    let mut push = |target: &str, tail: Vec<String>| {
        out.push(Request {
            target: target.to_owned(),
            head: head.clone(),
            tail,
        })
    };
    // Launch and system are the two targets every machine can offer without
    // setting anything up, and they reach different code: a launched program
    // goes through the gate, a system target through per-cpu events. A
    // source that declares neither is run on the first target it does declare.
    if targets.contains_key("launch") {
        push(
            "launch",
            std::iter::once("--".to_owned()).chain(launch).collect(),
        );
    }
    if targets.contains_key("system") {
        push("system", vec!["--system".to_owned()]);
    }
    if !targets.contains_key("launch") && !targets.contains_key("system") {
        let fallback = [
            ("cpu", vec!["--cpu".to_owned(), "0".into()]),
            ("iface", vec!["--iface".to_owned(), "lo".into()]),
            ("pid", vec!["--pid".to_owned(), "1".into()]),
            ("cgroup", vec!["--cgroup".to_owned(), "/".into()]),
        ];
        if let Some((kind, tail)) = fallback
            .into_iter()
            .find(|(kind, _)| targets.contains_key(*kind))
        {
            push(kind, tail);
        }
    }
    out
}

/// The mode and emit a request is run with: the first combination the
/// validator accepts that this machine also resolves, or failing that the
/// first the validator accepts, so the refusal is recorded.
///
/// Validation is asked without the machine (`--no-resolve`): which
/// combinations mean something is the catalog's question. Resolution is then
/// asked per combination because `omn list` answers for a source, not for a
/// mode -- `mem.numa` is usable here through `/proc` and its first mode,
/// `sample`, needs a PMU -- and the claim under test is that a source listed
/// usable has *some* request that runs.
fn choose(omn: &PathBuf, entry: &serde_json::Value, request: &Request) -> Vec<String> {
    let list = |key: &str| -> Vec<String> {
        entry[key]
            .as_array()
            .map(|all| {
                all.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let emits = list("emits");
    let mut preferred: Vec<String> = [
        "summary", "hist", "rate", "timeline", "topk", "raw", "stack",
    ]
    .into_iter()
    .map(str::to_owned)
    .filter(|e| emits.contains(e))
    .collect();
    let rest: Vec<String> = emits
        .into_iter()
        .filter(|e| !preferred.contains(e))
        .collect();
    preferred.extend(rest);
    let modes = list("modes");
    let options = |mode: &str, emit: &str| -> Vec<String> {
        ["-m", mode, "-e", emit, "-d", "300ms"]
            .map(str::to_owned)
            .to_vec()
    };
    let mut valid = None;
    for mode in &modes {
        for emit in &preferred {
            let args: Vec<String> = request
                .head
                .iter()
                .cloned()
                .chain(options(mode, emit))
                .chain(["--no-resolve".into(), "--dump-request".into()])
                .chain(request.tail.iter().cloned())
                .collect();
            let (code, _, _, _) =
                run_bounded(Command::new(omn).args(&args), Duration::from_secs(20));
            if code != Some(0) {
                continue;
            }
            valid.get_or_insert_with(|| options(mode, emit));
            let resolve: Vec<String> = request
                .head
                .iter()
                .cloned()
                .chain(options(mode, emit))
                .chain(["--dump-resolved".into()])
                .chain(request.tail.iter().cloned())
                .collect();
            let (code, _, _, _) =
                run_bounded(Command::new(omn).args(&resolve), Duration::from_secs(20));
            if code == Some(0) {
                return options(mode, emit);
            }
            // One emit is enough to learn whether this mode resolves.
            break;
        }
    }
    valid.unwrap_or_else(|| {
        options(
            modes.first().map_or("trace", String::as_str),
            preferred.first().map_or("summary", String::as_str),
        )
    })
}

fn sweep(name: &str, config: &str) -> Sweep {
    let started = Instant::now();
    let omn = omn();
    assert!(
        omn.exists(),
        "{} does not exist: run `make` first",
        omn.display()
    );
    let catalog = Command::new(&omn)
        .args(["list", "--format", "json"])
        .output()
        .expect("omn list runs");
    assert!(catalog.status.success(), "omn list --format json failed");
    let catalog: Vec<serde_json::Value> =
        serde_json::from_slice(&catalog.stdout).expect("the catalog is JSON");

    let mut sources = Vec::new();
    let mut environment = serde_json::Value::Null;
    for entry in &catalog {
        let id = entry["id"].as_str().expect("an id").to_owned();
        let listing = Command::new(&omn)
            .args(["list", &id, "--color", "never"])
            .output()
            .expect("omn list <source> runs");
        let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
        let degraded: BTreeMap<String, String> = entry["backends"]
            .as_array()
            .map(|all| {
                all.iter()
                    .filter_map(|b| {
                        Some((
                            b["id"].as_str()?.to_owned(),
                            b["degraded"].as_str()?.to_owned(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut runs = Vec::new();
        for request in requests(entry) {
            let options = choose(&omn, entry, &request);
            let args: Vec<String> = request
                .head
                .iter()
                .chain(&options)
                .chain(&request.tail)
                .cloned()
                .collect();
            // Resolve first, for the backend's id: a run's own output names
            // it too, but in prose.
            let resolve: Vec<String> = request
                .head
                .iter()
                .chain(&options)
                .cloned()
                .chain(["--dump-resolved".into(), "--format".into(), "json".into()])
                .chain(request.tail.iter().cloned())
                .collect();
            let (code, out, _, _) =
                run_bounded(Command::new(&omn).args(&resolve), Duration::from_secs(20));
            let resolved: Option<serde_json::Value> = (code == Some(0))
                .then(|| serde_json::from_str(&out).ok())
                .flatten();
            let backend = resolved
                .as_ref()
                .and_then(|r| r[0]["backend"]["id"].as_str().map(str::to_owned));
            if environment.is_null() {
                if let Some(r) = &resolved {
                    environment = r[0]["environment"].clone();
                }
            }
            let (code, stdout, stderr, elapsed_ms) =
                run_bounded(Command::new(&omn).args(&args), Duration::from_secs(30));
            runs.push(Run {
                target: request.target,
                args,
                backend,
                code,
                elapsed_ms,
                stdout: stdout.lines().take(40).collect::<Vec<_>>().join("\n"),
                stderr: stderr.chars().take(6000).collect(),
            });
        }
        let mut source = Source {
            id,
            here: Here::No,
            here_backend: None,
            backends: Vec::new(),
            runs,
            listing,
            degraded,
        };
        source.reparse();
        sources.push(source);
    }

    let kernel_release = fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .trim()
        .to_owned();
    let flag = |name: &str| std::env::var(name).is_ok_and(|v| v == "1");
    Sweep {
        cell: Cell {
            name: name.to_owned(),
            kernel_release,
            config: config.to_owned(),
            uid: unsafe { libc::getuid() },
            btf_hidden: flag("OMN_MATRIX_BTF_HIDDEN"),
            tracefs_hidden: flag("OMN_MATRIX_TRACEFS_HIDDEN"),
            environment,
            elapsed_s: started.elapsed().as_secs_f64(),
        },
        sources,
    }
}

/// Measures whatever machine this runs on, and writes the cell.
///
/// `OMN_MATRIX_CELL` names the cell and `OMN_MATRIX_CONFIG` its
/// configuration; `scripts/vm/cell.sh` sets both inside a guest. Run bare, it
/// sweeps the host as `<release>-host`.
#[test]
#[ignore = "measures the machine it runs on; scripts/matrix.sh runs it in each guest"]
fn sweep_this_machine() {
    let release = fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .trim()
        .to_owned();
    let config = std::env::var("OMN_MATRIX_CONFIG").unwrap_or_else(|_| "host".into());
    let name = std::env::var("OMN_MATRIX_CELL").unwrap_or_else(|_| format!("{release}-{config}"));
    let sweep = sweep(&name, &config);
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
            if kernel < (6, 6) && !fell_back {
                out.push(format!(
                    "{} ({}): attached through USDT on {}.{} without saying it fell back from \
                     uprobe-multi:\n{}",
                    source.id,
                    run.target,
                    kernel.0,
                    kernel.1,
                    run.text().trim()
                ));
            }
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

#[test]
fn a_usdt_source_below_six_six_must_say_it_fell_back() {
    let mut cell = honest_cell("6.1", "root");
    assert!(violations(&cell, &[]).contains_key("uprobe-multi is 6.6"));
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
