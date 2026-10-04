//! Asking for a measurement: what a session would cost and degrade, and --
//! when the person who started this server allowed it -- a bounded run of one.
//!
//! Validation is machine-independent and runs here, on the library, so its
//! rejections come back as fields rather than as stderr. Resolution depends on
//! the machine and on the privileges of whoever probes it, so it is asked of
//! the `omn` binary (`--dump-resolved`), which is also the binary a run goes
//! through.

use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use omn::catalog::Cost;
use omn::format::InputFormat;
use omn::request::{Budget, Session, TargetKind};
use omn::scalar::Duration;
use omn::Catalog;
use serde_json::{json, Value};

use crate::binary::Binary;

/// Where a session comes from: a file, text handed over in the call, or one
/// of the sessions the person who started the server approved.
pub enum Source {
    File(PathBuf),
    Text {
        text: String,
        format: InputFormat,
    },
    /// A session file read from the `--allow-launch` directory, read once:
    /// the text that was checked is the text that runs, from the file's own
    /// directory, and nothing in the call can change it.
    Approved {
        name: String,
        path: PathBuf,
        text: String,
        format: InputFormat,
    },
}

impl Source {
    /// The session a call names. `approved` names a file of the directory
    /// passed with `--allow-launch`, and is refused when the server was
    /// started without one.
    pub fn from_arguments(
        arguments: &Value,
        cwd: &Path,
        approved_dir: Option<&Path>,
    ) -> Result<Self, String> {
        let path = arguments.get("path").and_then(Value::as_str);
        let text = arguments.get("session").and_then(Value::as_str);
        let approved = arguments.get("approved").and_then(Value::as_str);
        let format = arguments.get("input_format").and_then(Value::as_str);
        match (path, text, approved) {
            (None, None, Some(name)) => match approved_dir {
                Some(dir) => Source::approved(dir, name),
                None => Err(format!(
                    "refused: `{name}` names an approved session, and this server was started \
                     without --allow-launch, so it approves none"
                )),
            },
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
                Err("pass one of `path`, `session` or `approved`".into())
            }
            (None, None, None) => Err(
                "a session is needed: `path` to a session file, `session` with its text, or \
                 `approved` naming one of the sessions the server was started to allow"
                    .into(),
            ),
            (Some(path), None, None) => Ok(Source::File(cwd.join(path))),
            (None, Some(text), None) => {
                let format = match format.unwrap_or("yaml") {
                    "yaml" | "yml" => InputFormat::Yaml,
                    "json" => InputFormat::Json,
                    "toml" => InputFormat::Toml,
                    other => {
                        return Err(format!(
                            "input_format `{other}` is not one of yaml, json or toml"
                        ))
                    }
                };
                Ok(Source::Text {
                    text: text.to_owned(),
                    format,
                })
            }
        }
    }

    /// One of the approved sessions, by its name in `dir`.
    ///
    /// The name is a file of `dir` or of a directory under it, and nothing
    /// else: an absolute path, a `..` anywhere in it, or a symbolic link --
    /// the file's or a directory's on the way -- that resolves outside `dir`
    /// is refused, because the directory is the whole of what its owner
    /// approved.
    pub fn approved(dir: &Path, name: &str) -> Result<Self, String> {
        let refuse = |why: &str| {
            Err(format!(
                "refused: `{name}` is not an approved session: {why}. The approved sessions are \
                 the files of {} (`list_approved` lists them)",
                dir.display()
            ))
        };
        let relative = Path::new(name);
        if name.is_empty() || relative.is_absolute() {
            return refuse("a name is relative to the approved directory");
        }
        if relative
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return refuse("a name may not leave the approved directory");
        }
        let root = dir
            .canonicalize()
            .map_err(|error| format!("the approved directory {}: {error}", dir.display()))?;
        let path = match root.join(relative).canonicalize() {
            Ok(path) => path,
            Err(_) => return refuse("there is no such file"),
        };
        if !path.starts_with(&root) {
            return refuse("it resolves outside the approved directory");
        }
        if !path.is_file() {
            return refuse("it is not a file");
        }
        let Some(format) = InputFormat::from_path(&path) else {
            return refuse("a session file is yaml, yml, json or toml");
        };
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Ok(Source::Approved {
            name: name.to_owned(),
            path,
            text,
            format,
        })
    }

    /// Every approved session under `dir`, by name, and why any file there
    /// is not one.
    pub fn every_approved(dir: &Path) -> Vec<(String, Result<Self, String>)> {
        let mut found = Vec::new();
        let mut pending = vec![PathBuf::new()];
        while let Some(relative) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(dir.join(&relative)) else {
                continue;
            };
            let mut names: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|entry| relative.join(entry.file_name()))
                .collect();
            names.sort();
            for name in names {
                let full = dir.join(&name);
                // A directory is walked only when it is one, not a link to
                // one: a link out would list what nobody approved.
                if full.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
                    pending.push(name);
                } else if InputFormat::from_path(&full).is_some() {
                    let name = name.display().to_string();
                    let source = Source::approved(dir, &name);
                    found.push((name, source));
                }
            }
        }
        found.sort_by(|left, right| left.0.cmp(&right.0));
        found
    }

    /// Whether the session is one its launch targets are allowed from.
    fn is_approved(&self) -> bool {
        matches!(self, Source::Approved { .. })
    }

    /// Where `omn` runs from: an approved session's own directory, so that
    /// a relative path in it means what its author meant.
    fn working_directory(&self) -> Option<&Path> {
        match self {
            Source::Approved { path, .. } => path.parent(),
            _ => None,
        }
    }

    /// Loads the session the way `omn` does: parsed, defaults pushed down into
    /// every request, the escalation block checked.
    pub fn load(&self) -> Result<Session, String> {
        let (text, format) = match self {
            Source::Approved { text, format, .. } => (text.clone(), *format),
            Source::File(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
                let format = InputFormat::from_path(path).ok_or_else(|| {
                    format!(
                        "cannot tell the format of {} from its name: yaml, yml, json or toml",
                        path.display()
                    )
                })?;
                (text, format)
            }
            Source::Text { text, format } => (text.clone(), *format),
        };
        let mut session: Session = format.parse(&text).map_err(|error| error.to_string())?;
        session.apply_defaults();
        session
            .check_escalation()
            .map_err(|error| error.to_string())?;
        Ok(session)
    }

    /// The arguments that hand this session to `omn -f`, and what to feed it.
    fn omn_arguments(&self) -> (Vec<String>, Option<String>) {
        match self {
            Source::File(path) => (vec!["-f".into(), path.display().to_string()], None),
            Source::Text { text, format } | Source::Approved { text, format, .. } => (
                vec![
                    "-f".into(),
                    "-".into(),
                    "--input-format".into(),
                    format.as_str().into(),
                ],
                Some(text.clone()),
            ),
        }
    }
}

/// Validation, here, on the library.
fn validation(catalog: &Catalog, session: &Session) -> (bool, Value) {
    match omn::validate::validate_session(catalog, session) {
        Ok(report) => (
            true,
            json!({
                "valid": true,
                "warnings": report.warnings().collect::<Vec<_>>(),
                "caveats": report.caveats().collect::<Vec<_>>(),
            }),
        ),
        Err(rejections) => (
            false,
            json!({
                "valid": false,
                "rejections": rejections.iter().map(|rejection| json!({
                    "request": rejection.request,
                    "message": rejection.message,
                    "hint": rejection.hint,
                })).collect::<Vec<_>>(),
            }),
        ),
    }
}

/// What a declared cost band means, and what it is not.
pub fn band(cost: Cost) -> Value {
    // The words are `omn`'s own (`src/main.rs::band`), and the ceiling is the
    // catalog's, held once in `Cost::worst_case_percent`. `extreme` is every
    // cost past 20 %: the bench measured `proc.syscall` at +296 % and the
    // preload shim at +600 % to +1000 % on an allocation-bound program, and
    // no band but this one holds them.
    let range = match cost {
        Cost::Free => "about zero, under 0.1 %",
        Cost::Low => "under 1 %",
        Cost::Med => "1 to 5 %",
        Cost::High => "5 to 20 %",
        Cost::Extreme => "over 20 %, with no bound above",
    };
    let worst = cost.worst_case_percent();
    json!({
        "band": cost.as_str(),
        "declared": range,
        "declared_worst_case_percent": if worst.is_finite() { json!(worst) } else { json!("unbounded") },
        "is": "a declaration in the catalog, not a measurement: a run measures its own cost \
               and writes it to cost.json",
        "not": "for an in-kernel collector, the cost is paid as the target's own dilation \
                rather than as the tool's cpu, and the band does not read it (finding 47)",
    })
}

/// `plan_session`: would this run, what would serve it, what would it give up.
pub fn plan(
    catalog: &Catalog,
    binary: &Binary,
    source: &Source,
    launch_allowed: bool,
) -> Result<Value, String> {
    let session = source.load()?;
    let (valid, validation) = validation(catalog, &session);
    let mut answer = json!({
        "requests": session.requests.iter().map(|request| json!({
            "label": request.label,
            "source": request.source.as_str(),
            "target": request.target.kind().as_str(),
        })).collect::<Vec<_>>(),
        "validation": validation,
        "through_run_session": match refuse_to_run(&session, source, launch_allowed) {
            Some(reason) => json!({"allowed": false, "why": reason}),
            None => json!({
                "allowed": true,
                "if": "the server was started with --allow-run, for at most the duration it allows",
            }),
        },
    });
    if !valid {
        answer["resolution"] =
            json!("not attempted: a session that does not validate is refused before the machine is asked");
        return Ok(answer);
    }

    let (mut args, stdin) = source.omn_arguments();
    args.extend(["--dump-resolved".into(), "--format".into(), "json".into()]);
    let output = binary.invoke_in(
        &args,
        stdin.as_deref(),
        StdDuration::from_secs(60),
        source.working_directory(),
    )?;
    let notes: Vec<&str> = output
        .stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    answer["answered_by"] = binary.answered_by();
    let mut missing = binary.capability_shortfall();
    match output.status {
        Some(0) => {
            let resolved: Value = serde_json::from_str(&output.stdout).map_err(|error| {
                format!("`omn --dump-resolved` printed something that is not JSON: {error}")
            })?;
            let mut per_request = Vec::new();
            for each in resolved.as_array().cloned().unwrap_or_default() {
                let cost: Option<Cost> =
                    serde_json::from_value(each["backend"]["cost"].clone()).ok();
                if !each["unverified"].as_array().is_none_or(Vec::is_empty) {
                    missing.push(format!(
                        "[{}] the selected backend has requirements no probe checked: {}",
                        each["request"]["label"]
                            .as_str()
                            .or(each["request"]["source"].as_str())
                            .unwrap_or("?"),
                        each["unverified"]
                    ));
                }
                per_request.push(json!({
                    "label": each["request"].get("label").cloned().unwrap_or(Value::Null),
                    "source": each["request"]["source"],
                    "backend": each["backend"]["id"],
                    "rank_in_chain": each["backend"]["rank"],
                    "cost": cost.map(band).unwrap_or(Value::Null),
                    "degradations": each.get("degradations").cloned().unwrap_or_else(|| json!([])),
                    "unverified": each.get("unverified").cloned().unwrap_or_else(|| json!([])),
                    "filter": each.get("filter").cloned().unwrap_or(Value::Null),
                    "chain": each["chain"],
                    "requirements": each["backend"]["requirements"],
                }));
            }
            answer["resolution"] = json!({
                "resolved": true,
                "requests": per_request,
                "said_by_omn": notes,
            });
        }
        status => {
            answer["resolution"] = json!({
                "resolved": false,
                "exit": status,
                "whose": match status {
                    Some(1) => "the machine: the request is valid and this machine cannot serve it",
                    Some(2) => "the request: it cannot mean anything",
                    Some(3) => "this build: the request is valid and nothing collects it yet",
                    _ => "unknown",
                },
                "said_by_omn": notes,
            });
        }
    }
    answer["missing"] = json!(missing);
    Ok(answer)
}

/// The bound a run is held to, set by whoever started the server.
#[derive(Debug, Clone, Copy)]
pub struct RunLimit {
    pub longest: Duration,
    /// The server was started with `--allow-launch`: an approved session
    /// may launch its program.
    pub launch_allowed: bool,
}

/// Why a session may not be run through this server, if it may not.
///
/// A launch target runs a program the session names -- any program, with this
/// user's rights -- which is acting on the machine rather than observing it:
/// boundary 5. Every other target kind names something that already exists.
/// The one exception is a session the person who started the server wrote
/// and put in the `--allow-launch` directory: its program, arguments and
/// working directory are theirs, as the file states them, and what a model
/// chooses is which of those sessions runs and when -- never what runs.
pub fn refuse_to_run(session: &Session, source: &Source, launch_allowed: bool) -> Option<String> {
    let launching: Vec<String> = session
        .requests
        .iter()
        .filter(|request| request.target.kind() == TargetKind::Launch)
        .map(|request| {
            request
                .label
                .clone()
                .unwrap_or_else(|| request.source.as_str().to_owned())
        })
        .collect();
    if launching.is_empty() || (source.is_approved() && launch_allowed) {
        return None;
    }
    let what = format!(
        "{} launch{} a program ({})",
        if launching.len() == 1 {
            "a request"
        } else {
            "requests"
        },
        if launching.len() == 1 { "es" } else { "" },
        launching.join(", ")
    );
    Some(if source.is_approved() {
        format!(
            "refused: {what}, and this server was started without --allow-launch, so no session \
             may launch one -- not even an approved one"
        )
    } else {
        format!(
            "refused: {what}, and running a program a caller named is changing the machine, not \
             observing it. This server runs a launching session only when its file is in the \
             directory the person who started the server passed with --allow-launch DIR, named \
             by `approved`; sessions over what already exists -- a pid, a tid, a cgroup, a cpu \
             set, an interface, a container, the system -- run from anywhere"
        )
    })
}

/// `run_session`: one bounded run through `omn`, written under `out`.
///
/// `budget` is the caller's, and optional: `omn` asked by a person carries
/// none unless the person asks for one, and a model asking the same question
/// through this server is asked nothing more. The bound this server holds a
/// run to is its duration, which is the person's who started it.
pub fn run(
    catalog: &Catalog,
    binary: &Binary,
    source: &Source,
    limit: RunLimit,
    duration: &str,
    budget: Option<&str>,
    out: &Path,
) -> Result<Value, String> {
    let duration: Duration = duration
        .parse()
        .map_err(|error| format!("duration `{duration}`: {error}"))?;
    if duration.as_nanos() == 0 || duration > limit.longest {
        return Err(format!(
            "refused: a duration of {duration} is outside what this server was started to allow \
             (more than zero, at most {}); the bound is the server's, set with --allow-run, and \
             not the caller's to raise",
            limit.longest
        ));
    }
    let budget: Option<Budget> = budget
        .map(|text| {
            text.parse()
                .map_err(|error| format!("budget `{text}`: {error}"))
        })
        .transpose()?;

    let session = source.load()?;
    if let Some(reason) = refuse_to_run(&session, source, limit.launch_allowed) {
        return Err(reason);
    }
    let (valid, validation) = validation(catalog, &session);
    if !valid {
        return Err(format!("refused before the run: {validation}"));
    }

    std::fs::create_dir_all(out.parent().unwrap_or(out))
        .map_err(|error| format!("cannot create {}: {error}", out.display()))?;
    let (mut args, stdin) = source.omn_arguments();
    args.extend([
        "--format".into(),
        "json".into(),
        "-o".into(),
        out.display().to_string(),
        "--duration".into(),
        duration.to_string(),
    ]);
    if let Some(budget) = budget {
        args.extend(["--budget".into(), budget.to_string()]);
    }
    let launches = session
        .requests
        .iter()
        .any(|request| request.target.kind() == TargetKind::Launch);
    // The duration is the run's own bound; this is the bound on the process,
    // for a run that does not end when it should.
    let bound = StdDuration::from_nanos(duration.as_nanos()) * 2 + StdDuration::from_secs(30);
    // A launched program's stderr is written to a file of its own, so that
    // `said_by_omn` is omn's alone.
    let program_stderr_file = out.join(PROGRAM_STDERR_FILE);
    let mut env = Vec::new();
    if launches {
        std::fs::create_dir_all(out)
            .map_err(|error| format!("cannot create {}: {error}", out.display()))?;
        env.push((
            omn::collect::PROGRAM_STDERR_VARIABLE,
            program_stderr_file.as_os_str(),
        ));
    }
    let output = binary.invoke_with(
        &args,
        stdin.as_deref(),
        bound,
        source.working_directory(),
        &env,
    )?;
    let mut answer = json!({
        "exit": output.status,
        "said_by_omn": output.stderr.lines().filter(|line| !line.trim().is_empty()).collect::<Vec<_>>(),
        "answered_by": binary.answered_by(),
        "validation": validation,
        "approved": match source {
            Source::Approved { name, path, .. } => json!({
                "name": name,
                "file": path.display().to_string(),
                "ran_from": source.working_directory().map(|dir| dir.display().to_string()),
            }),
            _ => Value::Null,
        },
        "bounds": {
            "duration": duration.to_string(),
            "budget": match budget {
                Some(budget) => json!(budget.to_string()),
                None => json!(NO_BUDGET),
            },
            "longest_this_server_allows": limit.longest.to_string(),
        },
        "budget_against": budget_against(out, budget.is_some()),
        "stopped_by_budget": stopped_by_budget(out),
    });
    if launches {
        answer["program_stdout"] = program_stdout(&output.stdout_bytes, out);
        answer["program_stderr"] = program_stderr(&program_stderr_file);
    }
    if let Some(after) = output.killed_after {
        answer["killed"] = json!(format!(
            "omn was still running {:.1} s after it started, against a {duration} run, and was \
             killed: what it wrote before that is all there is",
            after.as_secs_f64()
        ));
    }
    Ok(answer)
}

/// What `bounds.budget` reads when the call passed none.
const NO_BUDGET: &str = "none: the call asked for no ceiling, so the run was not stopped for what \
     it cost; what it cost is measured and written anyway (`budget_against`, `cost.json`)";

/// The most of the program's stdout handed back in the answer; the whole of
/// it is in the run's directory.
const PROGRAM_STDOUT_IN_ANSWER: usize = 64 * 1024;

/// The file of the run's directory that holds what the program printed.
const PROGRAM_STDOUT_FILE: &str = "program.stdout";

/// The file of the run's directory the program's stderr is written to.
const PROGRAM_STDERR_FILE: &str = "program.stderr";

/// What a launched program wrote on its stderr, which `omn` sent to a file
/// of its own ([`omn::collect::PROGRAM_STDERR_VARIABLE`]) rather than
/// mixing it into its own: the last [`PROGRAM_STDOUT_IN_ANSWER`] bytes, and
/// the file holding all of it.
pub fn program_stderr(file: &Path) -> Value {
    match std::fs::read(file) {
        Ok(bytes) => {
            let cut = bytes.len().saturating_sub(PROGRAM_STDOUT_IN_ANSWER);
            json!({
                "bytes": bytes.len(),
                "text": String::from_utf8_lossy(&bytes[cut..]),
                "left_out_before_text": cut,
                "file": file.display().to_string(),
            })
        }
        Err(error) => json!({
            "unread": format!(
                "{}: {error}; the program was not launched, or this omn predates a stderr of \
                 the program's own, in which case its lines are among `said_by_omn`",
                file.display()
            ),
        }),
    }
}

/// Every request whose budget ended the run, read from the run's
/// `cost.json`: `null` when none did. A stopped run's numbers are those of a
/// window shorter than the one asked for, and the answer says so as the CLI
/// does on its own line, so that a caller cannot take it for a complete one.
pub fn stopped_by_budget(out: &Path) -> Value {
    let Ok(Value::Array(costs)) = std::fs::read(out.join("cost.json"))
        .map_err(|_| ())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|_| ()))
    else {
        return Value::Null;
    };
    let stops: Vec<Value> = costs
        .iter()
        .filter(|cost| cost["stopped_by_budget"].as_bool().unwrap_or(false))
        .map(|cost| {
            json!({
                "label": cost["label"],
                "source": cost["source"],
                "budget_percent": cost["budget_percent"],
                "because": cost["stopped_because"],
                "reads": "the budget was exceeded and the run was stopped there, before its own \
                          end: every figure of this run covers a shorter window than the one \
                          asked for",
            })
        })
        .collect();
    if stops.is_empty() {
        Value::Null
    } else {
        json!(stops)
    }
}

/// What a launched program printed on stdout, told apart from `omn`'s own
/// document.
///
/// The program inherits `omn`'s stdout (`src/main.rs`, `shared_stdout_warning`),
/// so the two arrive on one pipe: whatever the program printed, and the run's
/// document, which `omn` writes once and also to `results.json`. The document
/// is found there byte for byte -- its last occurrence, since a program still
/// printing after `omn` wrote it would follow it -- and what is left is the
/// program's. Nothing is guessed: when the document is not found, the answer
/// says so and hands the whole stream back as it came.
///
/// The whole of it is written beside the run's documents, and the answer
/// carries its last [`PROGRAM_STDOUT_IN_ANSWER`] bytes, where a program's own
/// account of what it did usually is, with how many were left out before them.
pub fn program_stdout(stdout: &[u8], out: &Path) -> Value {
    let (program, told_apart): (Vec<u8>, Value) = match std::fs::read(out.join("results.json")) {
        Ok(document) if !document.is_empty() => match find_last(stdout, &document) {
            Some(at) => {
                let mut program = stdout[..at].to_vec();
                program.extend_from_slice(&stdout[at + document.len()..]);
                (
                    program,
                    json!(
                        "yes: omn's document, the bytes of results.json, was found on the \
                               shared stdout and taken out; the rest is the program's"
                    ),
                )
            }
            None => (
                stdout.to_vec(),
                json!(
                    "no: the bytes of results.json are not on the shared stdout, so this \
                           is the whole stream as it came, omn's document possibly in it"
                ),
            ),
        },
        _ if out.join("recording.json").is_file() => (
            stdout.to_vec(),
            json!(
                "yes: a recording writes each document to a file of its own under the run's \
                       directory and none to stdout, so all of it is the program's"
            ),
        ),
        _ => (
            stdout.to_vec(),
            json!(
                "no: omn wrote no results.json to compare with, so this is the whole stream \
                       as it came, and whether a document of omn's is in it was not checked"
            ),
        ),
    };
    let file = out.join(PROGRAM_STDOUT_FILE);
    let written = match std::fs::write(&file, &program) {
        Ok(()) => json!(file.display().to_string()),
        Err(error) => json!({"unwritten": format!("{}: {error}", file.display())}),
    };
    let cut = program.len().saturating_sub(PROGRAM_STDOUT_IN_ANSWER);
    json!({
        "bytes": program.len(),
        "text": String::from_utf8_lossy(&program[cut..]),
        "left_out_before_text": cut,
        "file": written,
        "told_apart": told_apart,
        "stderr": "apart: the program's stderr is written to a file of its own, in \
                   `program_stderr`, and is not among `said_by_omn`",
    })
}

/// Where `needle` last occurs in `haystack`.
fn find_last(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    if haystack.ends_with(needle) {
        return Some(haystack.len() - needle.len());
    }
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

/// What a budget is a percentage of, per request, read from the run's
/// `cost.json`.
///
/// `omn`'s budget is a share of the target's own cpu over the window. For a
/// target that spends its time waiting -- on a disk, on a child, on a socket
/// -- that is a small number, and a collector's cost that is small against
/// the window is large against it. Which of the two a run was is not guessed
/// here: each request says the target's cpu, the window's wall-clock time,
/// the share of the window the target was on a cpu, and what the run spent
/// against both denominators, the budget's and the window's.
pub fn budget_against(out: &Path, asked: bool) -> Value {
    let costs = match std::fs::read(out.join("cost.json")) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Array(costs)) => costs,
            Ok(_) | Err(_) => {
                return json!({"unread": "cost.json is not a list of measured costs"});
            }
        },
        Err(error) => return json!({"unread": format!("no cost.json: {error}")}),
    };
    let per_request: Vec<Value> = costs.iter().map(|cost| denominator(cost, asked)).collect();
    json!(per_request)
}

/// One request's [`budget_against`].
fn denominator(cost: &Value, asked: bool) -> Value {
    let overhead = &cost["overhead"];
    let ns = |field: &str| overhead[field].as_u64();
    let label = cost["label"].clone();
    let wall = ns("wall_ns").unwrap_or(0);
    // What the collector spent inside the target is unknown, never zero
    // (`in_target_cpu_ns: null`, with `in_target_from` saying why). Where it
    // is unknown, the run's spend is only known from below, and is said so.
    let attributed = ns("attributed_cpu_ns").unwrap_or(0);
    let inside = ns("in_target_cpu_ns");
    let budget = if asked {
        "the budget is a percentage of the target's own cpu over the window"
    } else {
        "no budget was asked; had one been, it would be a percentage of the target's own cpu \
         over the window"
    };
    let Some(observed) = ns("observed_cpu_ns") else {
        return json!({
            "label": label,
            "budget_is_a_share_of": budget,
            "target_cpu_ns": Value::Null,
            "why": format!(
                "the target's cpu was not measured ({}), so there is nothing for a budget to be \
                 a percentage of",
                overhead["observed_from"].as_str().unwrap_or("no reason given")
            ),
            "window_wall_ns": wall,
        });
    };
    let share = |part: u64, of: u64| (of > 0).then(|| 100.0 * part as f64 / of as f64);
    let mut answer = json!({
        "label": label,
        "budget_is_a_share_of": budget,
        "target_cpu_ns": observed,
        "target_cpu_from": overhead["observed_from"],
        "window_wall_ns": wall,
        "target_on_cpu_percent_of_window": share(observed, wall),
        "in_target_known": inside.is_some(),
        "in_target_cpu_ns": inside,
        "in_target_from": overhead["in_target_from"],
        "reads": "a target on a cpu for a small share of its window has little cpu to be a \
                  percentage of: the same collector costs far more of it than of the window. \
                  Both percentages are here, and neither is the other",
    });
    let fields = answer.as_object_mut().expect("built as an object");
    match inside {
        Some(inside) => {
            let spent = attributed + inside;
            fields.insert("run_spent_ns".into(), json!(spent));
            fields.insert(
                "run_spent_percent_of_target_cpu".into(),
                cost["measured_percent"].clone(),
            );
            fields.insert(
                "run_spent_percent_of_window_on_one_cpu".into(),
                json!(share(spent, wall)),
            );
        }
        None => {
            // Null where the figure is not known, and the part that is known
            // under a name that says it is a floor.
            fields.insert("run_spent_ns".into(), Value::Null);
            fields.insert("run_spent_percent_of_target_cpu".into(), Value::Null);
            fields.insert("run_spent_percent_of_window_on_one_cpu".into(), Value::Null);
            fields.insert("run_spent_at_least_ns".into(), json!(attributed));
            fields.insert(
                "run_spent_at_least_percent_of_target_cpu".into(),
                cost["measured_percent"].clone(),
            );
            fields.insert(
                "run_spent_at_least_percent_of_window_on_one_cpu".into(),
                json!(share(attributed, wall)),
            );
            fields.insert(
                "why_at_least".into(),
                json!(format!(
                    "what the collector spent inside the target is unknown ({}), so the run's \
                     spend is known only from below: the tool's own share",
                    overhead["in_target_from"]
                        .as_str()
                        .unwrap_or("no reason given")
                )),
            );
        }
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_in_target_cost_is_a_floor_and_not_a_zero() {
        let cost = |inside: Value| {
            json!({
                "label": "x",
                "measured_percent": 2.0,
                "overhead": {
                    "wall_ns": 1000, "observed_cpu_ns": 500, "observed_from": "rusage",
                    "attributed_cpu_ns": 10, "in_target_cpu_ns": inside,
                    "in_target_from": "program stats lost",
                },
            })
        };
        let unknown = denominator(&cost(Value::Null), false);
        assert_eq!(unknown["in_target_known"], false);
        assert_eq!(unknown["in_target_from"], "program stats lost");
        assert!(unknown["run_spent_ns"].is_null());
        assert!(unknown["run_spent_percent_of_target_cpu"].is_null());
        assert!(unknown["run_spent_percent_of_window_on_one_cpu"].is_null());
        assert_eq!(unknown["run_spent_at_least_ns"], 10);
        assert_eq!(
            unknown["run_spent_at_least_percent_of_window_on_one_cpu"],
            1.0
        );
        assert!(unknown["why_at_least"]
            .as_str()
            .unwrap()
            .contains("program stats lost"));

        let known = denominator(&cost(json!(40)), true);
        assert_eq!(known["in_target_known"], true);
        assert_eq!(known["run_spent_ns"], 50);
        assert_eq!(known["run_spent_percent_of_window_on_one_cpu"], 5.0);
        assert!(known.get("run_spent_at_least_ns").is_none());
    }

    #[test]
    fn the_document_is_found_where_it_last_is() {
        assert_eq!(find_last(b"ab{doc}", b"{doc}"), Some(2));
        assert_eq!(find_last(b"{doc}a{doc}b", b"{doc}"), Some(6));
        assert_eq!(find_last(b"ab", b"{doc}"), None);
        assert_eq!(find_last(b"{do", b"{doc}"), None);
    }

    #[test]
    fn a_program_printing_after_the_document_keeps_both_halves() {
        let out = std::env::temp_dir().join(format!("omn-mcp-split-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("results.json"), b"{\"r\":1}\n").unwrap();
        let answer = program_stdout(b"before\n{\"r\":1}\nafter\n", &out);
        assert_eq!(answer["text"], "before\nafter\n");
        assert!(answer["told_apart"].as_str().unwrap().starts_with("yes"));
        // Without the document on disk nothing is taken out, and that is said.
        std::fs::remove_file(out.join("results.json")).unwrap();
        let answer = program_stdout(b"before\n{\"r\":1}\n", &out);
        assert_eq!(answer["text"], "before\n{\"r\":1}\n");
        assert!(answer["told_apart"].as_str().unwrap().starts_with("no"));
        std::fs::remove_dir_all(&out).unwrap();
    }

    /// A run a budget stopped says so in the answer, by request, as the CLI
    /// says it on a line of its own; a run nothing stopped says null.
    #[test]
    fn a_run_the_budget_stopped_says_so_in_the_answer() {
        let out = std::env::temp_dir().join(format!("omn-mcp-stopped-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(
            out.join("cost.json"),
            r#"[{"label":"profile","source":"cpu.profile","budget_percent":5.0,
                 "stopped_by_budget":true,"stopped_because":"`cpu.profile` spent 9.1 % ..."},
                {"label":"rss","source":"mem.rss","stopped_because":"the same"}]"#,
        )
        .unwrap();
        let stops = stopped_by_budget(&out);
        assert_eq!(stops.as_array().map(Vec::len), Some(1), "{stops}");
        assert_eq!(stops[0]["source"], "cpu.profile");
        assert!(stops[0]["because"].as_str().unwrap().contains("9.1 %"));
        std::fs::write(out.join("cost.json"), r#"[{"label":"rss"}]"#).unwrap();
        assert!(stopped_by_budget(&out).is_null());
        std::fs::remove_dir_all(&out).unwrap();
    }
}
