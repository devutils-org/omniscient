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

/// Where a session comes from: a file, or text handed over in the call.
pub enum Source {
    File(PathBuf),
    Text { text: String, format: InputFormat },
}

impl Source {
    pub fn from_arguments(arguments: &Value, cwd: &Path) -> Result<Self, String> {
        let path = arguments.get("path").and_then(Value::as_str);
        let text = arguments.get("session").and_then(Value::as_str);
        let format = arguments.get("input_format").and_then(Value::as_str);
        match (path, text) {
            (Some(_), Some(_)) => Err("pass `path` or `session`, not both".into()),
            (None, None) => Err(
                "a session is needed: `path` to a session file, or `session` with its text".into(),
            ),
            (Some(path), None) => Ok(Source::File(cwd.join(path))),
            (None, Some(text)) => {
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

    /// Loads the session the way `omn` does: parsed, defaults pushed down into
    /// every request, the escalation block checked.
    pub fn load(&self) -> Result<Session, String> {
        let (text, format) = match self {
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
            Source::Text { text, format } => (
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
    let (range, worst) = match cost {
        Cost::Free => ("a counter read, about zero", 0.1),
        Cost::Low => ("under 1 %", 1.0),
        Cost::Med => ("1 to 5 %", 5.0),
        Cost::High => ("5 to 20 %", 20.0),
        Cost::Extreme => ("over 10x: kernel replay", f64::INFINITY),
    };
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
pub fn plan(catalog: &Catalog, binary: &Binary, source: &Source) -> Result<Value, String> {
    let session = source.load()?;
    let (valid, validation) = validation(catalog, &session);
    let mut answer = json!({
        "requests": session.requests.iter().map(|request| json!({
            "label": request.label,
            "source": request.source.as_str(),
            "target": request.target.kind().as_str(),
        })).collect::<Vec<_>>(),
        "validation": validation,
        "through_run_session": match refuse_to_run(&session) {
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
    let output = binary.invoke(&args, stdin.as_deref(), StdDuration::from_secs(60))?;
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
}

/// Why a session may not be run through this server, if it may not.
///
/// A launch target runs a program the session names -- any program, with this
/// user's rights -- which is acting on the machine rather than observing it:
/// boundary 5. Every other target kind names something that already exists.
pub fn refuse_to_run(session: &Session) -> Option<String> {
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
    (!launching.is_empty()).then(|| {
        format!(
            "refused: {} launch{} a program ({}), and running a program a model named is \
             changing the machine, not observing it. This server runs sessions over what \
             already exists -- a pid, a tid, a cgroup, a cpu set, an interface, a container, \
             the system -- and a launch is for a person to run with `omn -f`",
            if launching.len() == 1 {
                "a request"
            } else {
                "requests"
            },
            if launching.len() == 1 { "es" } else { "" },
            launching.join(", ")
        )
    })
}

/// `run_session`: one bounded run through `omn`, written under `out`.
pub fn run(
    catalog: &Catalog,
    binary: &Binary,
    source: &Source,
    limit: RunLimit,
    duration: &str,
    budget: &str,
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
    let budget: Budget = budget
        .parse()
        .map_err(|error| format!("budget `{budget}`: {error}"))?;

    let session = source.load()?;
    if let Some(reason) = refuse_to_run(&session) {
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
        "--budget".into(),
        budget.to_string(),
    ]);
    // The duration is the run's own bound; this is the bound on the process,
    // for a run that does not end when it should.
    let bound = StdDuration::from_nanos(duration.as_nanos()) * 2 + StdDuration::from_secs(30);
    let output = binary.invoke(&args, stdin.as_deref(), bound)?;
    let mut answer = json!({
        "exit": output.status,
        "said_by_omn": output.stderr.lines().filter(|line| !line.trim().is_empty()).collect::<Vec<_>>(),
        "answered_by": binary.answered_by(),
        "validation": validation,
        "bounds": {
            "duration": duration.to_string(),
            "budget": budget.to_string(),
            "longest_this_server_allows": limit.longest.to_string(),
        },
    });
    if let Some(after) = output.killed_after {
        answer["killed"] = json!(format!(
            "omn was still running {:.1} s after it started, against a {duration} run, and was \
             killed: what it wrote before that is all there is",
            after.as_secs_f64()
        ));
    }
    Ok(answer)
}
