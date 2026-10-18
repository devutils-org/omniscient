//! `omn selftest`: what this machine serves, and whether what is listed
//! usable attaches.
//!
//! Every source is listed (`omn list <source>`, the reasons verbatim) and
//! **run**, briefly, on a launched program and on the machine -- the two
//! targets that need no setup -- because available means it attaches, and only
//! a run says so. It is the portability matrix's sweep (`docs/portability.md`),
//! made a command so that a machine needs the release and nothing else:
//! `scripts/matrix.sh`'s guests, distroproof's (`docs/distroproof.md`), and a
//! user asking what their machine gives them.
//!
//! Every answer comes from this same binary run as a separate program
//! (`current_exe`), never from the library in this process: the sweep has to
//! describe what a user meets, with the file capabilities the binary carries,
//! through the command line a user types. The document it writes is the
//! matrix's cell, which `tests/portability.rs` judges.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

/// What one sweep is asked.
pub struct Options {
    /// The cell's name; `here-<config>` when not given.
    pub cell: Option<String>,
    /// What the machine was made to be: `host` when not given.
    pub config: String,
    /// How long each run lasts.
    pub duration: Duration,
    /// Only these sources, when given.
    pub sources: Vec<String>,
    /// Where the repository's fixtures are (`omn-usdt-fixture/fixture`,
    /// `omn-vertical/server`): a checkout's `target/`. Without them the
    /// sources about a program's own probes run on the plain workload, and
    /// say which probes they did not find.
    pub fixtures: Option<PathBuf>,
}

/// The cell: what the machine was and who swept it.
#[derive(Debug, Serialize)]
pub struct Cell {
    pub name: String,
    pub kernel_release: String,
    pub config: String,
    pub uid: u32,
    pub btf_hidden: bool,
    pub tracefs_hidden: bool,
    /// The environment the tool captured in a resolved file: its own view of
    /// BTF, tracefs and `perf_event_paranoid`.
    pub environment: serde_json::Value,
    pub elapsed_s: f64,
    /// A hash of the binary's bytes: two sweeps by two builds can differ by
    /// the build rather than the machine.
    pub build: Option<String>,
}

/// One short run, as `omn trace` performed it.
#[derive(Debug, Serialize)]
pub struct Run {
    pub target: String,
    pub args: Vec<String>,
    /// The backend `--dump-resolved` chose, when it resolved.
    pub backend: Option<String>,
    /// `None` when the watchdog had to kill it.
    pub code: Option<i32>,
    pub elapsed_ms: u64,
    /// The head of standard output, where the `degraded:` lines are.
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn attached(&self) -> bool {
        self.code == Some(0)
    }

    /// The first line that says why a run did not attach.
    fn reason(&self) -> String {
        let text = format!("{}\n{}", self.stderr, self.stdout);
        let line = text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with("note:"))
            .unwrap_or("no output");
        match self.code {
            None => format!("killed by the watchdog: {line}"),
            Some(code) => format!("exit {code}: {line}"),
        }
    }
}

/// One source: its listing and its runs. `here` is the listing's answer; the
/// judges parse the backends out of `listing` themselves.
#[derive(Debug, Serialize)]
pub struct Source {
    pub id: String,
    pub here: &'static str,
    pub here_backend: Option<String>,
    pub backends: Vec<serde_json::Value>,
    pub runs: Vec<Run>,
    pub listing: String,
    pub degraded: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct Sweep {
    pub cell: Cell,
    pub sources: Vec<Source>,
}

impl Sweep {
    /// Sources listed usable here with a run that did not attach: what "available
    /// means it attaches" forbids.
    pub fn refused_after_yes(&self) -> Vec<&Source> {
        self.sources
            .iter()
            .filter(|source| source.here == "yes" && !source.runs.iter().any(Run::attached))
            .filter(|source| !source.runs.is_empty())
            .collect()
    }

    /// The sweep as a person reads it: one line a source, then the totals.
    pub fn text(&self) -> String {
        let cell = &self.cell;
        let mut out = format!(
            "omn selftest on {} ({}, uid {}), {:.0} s\n\n",
            cell.kernel_release, cell.config, cell.uid, cell.elapsed_s
        );
        let width = self.sources.iter().map(|s| s.id.len()).max().unwrap_or(0);
        for source in &self.sources {
            let listed = match source.here {
                "yes" => "usable",
                _ => "not here",
            };
            let runs: Vec<String> = source
                .runs
                .iter()
                .map(|run| {
                    if run.attached() {
                        format!("{} attaches", run.target)
                    } else {
                        format!("{}: {}", run.target, run.reason())
                    }
                })
                .collect();
            out.push_str(&format!(
                "{:width$}  {:8}  {}\n",
                source.id,
                listed,
                runs.join("; "),
                width = width
            ));
        }
        let usable = self.sources.iter().filter(|s| s.here == "yes").count();
        let attached = self
            .sources
            .iter()
            .filter(|s| s.runs.iter().any(Run::attached))
            .count();
        let refused = self.refused_after_yes();
        out.push_str(&format!(
            "\n{} sources: {usable} listed usable here, {attached} attached on at least one \
             target; {} listed usable and refused at start{}\n",
            self.sources.len(),
            refused.len(),
            if refused.is_empty() {
                String::new()
            } else {
                format!(
                    " ({})",
                    refused
                        .iter()
                        .map(|s| s.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        ));
        out
    }
}

/// Runs a command with a watchdog: its exit code (`None` when it had to be
/// killed), its output, and how long it took.
fn run_bounded(command: &mut Command, limit: Duration) -> (Option<i32>, String, String, u64) {
    let started = Instant::now();
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return (Some(-1), String::new(), error.to_string(), 0),
    };
    // Both pipes read on threads, so a chatty child cannot fill one and block
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
            Err(_) => break None,
        }
    };
    let out = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
    let err = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
    (code, out, err, started.elapsed().as_millis() as u64)
}

/// The program a launched run starts: a little cpu, a few files, a sleep --
/// something for every launch-capable collector to attach to, and long enough
/// that the run's duration ends it rather than its exit.
const WORKLOAD: [&str; 3] = [
    "/bin/sh",
    "-c",
    "i=0; while [ $i -lt 3000 ]; do i=$((i+1)); done; cat /proc/self/status >/dev/null; \
     ls / >/dev/null; sleep 2",
];

/// One way to run a source: the target kind, what goes before the options the
/// sweep chooses (`trace <id>` and any filter), and the target, which goes
/// last because a launched program's `--` ends the options.
struct Request {
    target: String,
    head: Vec<String>,
    tail: Vec<String>,
}

/// The requests a source is run with: one per target kind that needs no setup.
///
/// A few sources are about a property of the target rather than the machine
/// -- USDT probes a program carries -- and are given the repository's fixtures
/// when they are there.
fn requests(entry: &serde_json::Value, fixtures: Option<&Path>) -> Vec<Request> {
    let id = entry["id"].as_str().unwrap_or_default();
    let targets = entry["targets"].as_object().cloned().unwrap_or_default();
    let fixture = |relative: &str| {
        fixtures
            .map(|dir| dir.join(relative))
            .filter(|path| path.exists())
            .map(|path| path.to_string_lossy().into_owned())
    };
    let workload: Vec<String> = WORKLOAD.iter().map(|s| s.to_string()).collect();
    let usdt = fixture("omn-usdt-fixture/fixture");
    let (launch, extra): (Vec<String>, Vec<String>) = match (id, usdt) {
        ("runtime.gc", Some(f)) => (vec![f, "gc".into(), "5".into(), "20".into()], vec![]),
        ("runtime.alloc", Some(f)) => (vec![f, "alloc".into(), "50".into()], vec![]),
        ("runtime.custom", Some(f)) => (
            vec![f, "ticks".into(), "50".into()],
            vec![
                "--filter".into(),
                "provider == 'omn_fixture' && probe == 'tick'".into(),
            ],
        ),
        ("net.request", _) => match fixture("omn-vertical/server") {
            Some(server) => (
                vec![server, "/tmp/omn-matrix-server-data".into(), "0:0".into()],
                vec![],
            ),
            None => (workload, vec![]),
        },
        _ => (workload, vec![]),
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
    // Launch and system reach different code -- the gate, and per-cpu events
    // -- and need nothing set up. A source that takes neither is run on the
    // first target it does take.
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

/// The mode and emit a request runs with: the first combination the validator
/// accepts that this machine also resolves, or else the first the validator
/// accepts, so that the refusal is recorded.
///
/// `omn list` answers for a source, not for a mode -- `mem.numa` is usable
/// through `/proc` and its first mode needs a PMU -- and the claim under test
/// is that a source listed usable has *some* request that runs.
fn choose(omn: &Path, entry: &serde_json::Value, request: &Request, duration: &str) -> Vec<String> {
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
        ["-m", mode, "-e", emit, "-d", duration]
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

/// A 64-bit FNV-1a hash of a file, in hex: enough to tell two builds apart.
fn build_of(binary: &Path) -> Option<String> {
    let bytes = std::fs::read(binary).ok()?;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    Some(format!("{hash:016x}"))
}

/// Sweeps this machine with the binary at `omn`.
pub fn sweep(omn: &Path, options: &Options) -> Result<Sweep, String> {
    let started = Instant::now();
    let duration = format!("{}ms", options.duration.as_millis());
    let catalog = Command::new(omn)
        .args(["list", "--format", "json"])
        .output()
        .map_err(|error| format!("cannot run {}: {error}", omn.display()))?;
    if !catalog.status.success() {
        return Err(format!(
            "`omn list --format json` failed: {}",
            String::from_utf8_lossy(&catalog.stderr).trim()
        ));
    }
    let catalog: Vec<serde_json::Value> = serde_json::from_slice(&catalog.stdout)
        .map_err(|error| format!("the catalog listing is not JSON: {error}"))?;
    let unknown: Vec<&String> = options
        .sources
        .iter()
        .filter(|id| !catalog.iter().any(|entry| entry["id"] == id.as_str()))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "no source named {}",
            unknown
                .iter()
                .map(|id| format!("`{id}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let mut sources = Vec::new();
    let mut environment = serde_json::Value::Null;
    for entry in &catalog {
        let id = entry["id"].as_str().unwrap_or_default().to_owned();
        if !options.sources.is_empty() && !options.sources.contains(&id) {
            continue;
        }
        let listing = Command::new(omn)
            .args(["list", &id, "--color", "never"])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .unwrap_or_default();
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
        for request in requests(entry, options.fixtures.as_deref()) {
            let chosen = choose(omn, entry, &request, &duration);
            let args: Vec<String> = request
                .head
                .iter()
                .chain(&chosen)
                .chain(&request.tail)
                .cloned()
                .collect();
            // Resolved first, for the backend's id: the run names it too, in
            // prose.
            let resolve: Vec<String> = request
                .head
                .iter()
                .chain(&chosen)
                .cloned()
                .chain(["--dump-resolved".into(), "--format".into(), "json".into()])
                .chain(request.tail.iter().cloned())
                .collect();
            let (code, out, _, _) =
                run_bounded(Command::new(omn).args(&resolve), Duration::from_secs(20));
            let resolved: Option<serde_json::Value> = (code == Some(0))
                .then(|| serde_json::from_str(&out).ok())
                .flatten();
            let backend = resolved
                .as_ref()
                .and_then(|r| r[0]["backend"]["id"].as_str().map(str::to_owned));
            if environment.is_null() {
                if let Some(resolved) = &resolved {
                    environment = resolved[0]["environment"].clone();
                }
            }
            let (code, stdout, stderr, elapsed_ms) =
                run_bounded(Command::new(omn).args(&args), Duration::from_secs(30));
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
        let here = match entry["here"]["availability"].as_str() {
            Some("ready") => "yes",
            Some("unavailable") => "no",
            _ => "unverified",
        };
        sources.push(Source {
            id,
            here,
            here_backend: None,
            backends: Vec::new(),
            runs,
            listing,
            degraded,
        });
    }

    let kernel_release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .trim()
        .to_owned();
    let flag = |name: &str| std::env::var(name).is_ok_and(|v| v == "1");
    Ok(Sweep {
        cell: Cell {
            name: options
                .cell
                .clone()
                // `here-<config>`: a cell named after a kernel release is a
                // guest's (`scripts/matrix.sh`), and the judges tell the two
                // apart by that.
                .unwrap_or_else(|| format!("here-{}", options.config)),
            kernel_release,
            config: options.config.clone(),
            // SAFETY: getuid cannot fail.
            uid: unsafe { libc::getuid() },
            btf_hidden: flag("OMN_MATRIX_BTF_HIDDEN"),
            tracefs_hidden: flag("OMN_MATRIX_TRACEFS_HIDDEN"),
            environment,
            elapsed_s: started.elapsed().as_secs_f64(),
            build: build_of(omn),
        },
        sources,
    })
}
