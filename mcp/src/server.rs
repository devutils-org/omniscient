//! The protocol: `initialize`, the tools, the resources, and nothing else.

use std::path::{Path, PathBuf};

use omn::scalar::Duration;
use omn::Catalog;
use serde_json::{json, Value};

use crate::binary::Binary;
use crate::session::{self, RunLimit, Source};
use crate::{absolute, evidence, runs};

/// The protocol revisions this server speaks, newest first. A client asking
/// for one of them gets it; any other gets the newest.
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// How deep under `--dir` runs are looked for.
const RUN_DEPTH: usize = 3;

/// Raw events per output when a caller does not say.
const DEFAULT_MAX_EVENTS: usize = 100;

const INSTRUCTIONS: &str = "\
This server hands over what omn wrote, not a reading of it. Every number comes \
with the fields that say what it is missing, and an answer that is read without \
them is read wrong:

- `short` on a series: the run lost events, and every count, mean and maximum on \
it is a lower bound, not an estimate. `what_it_is_missing.shortfalls` lists them.
- `losses`: dropped, throttled, failed reads, late ticks, with the reasons.
- `degradations`: what the backend that served a request gives up against the \
head of its chain. `unverified`: requirements nobody could check.
- `evidence`: how two events are tied. The levels are a ladder, weakest first -- \
TEMPORAL (they overlapped in time: a hypothesis), WAKEUP, KEY, IDENTITY (an \
identity propagated and read back). They never mean the same thing; each one is \
annotated in place with `evidence_means`.
- `ts_err_ns`: a timestamp converted from another clock, known only to within it.
- `missing` on every answer: what this answer itself leaves out.

Nothing here changes the machine unless the person who started this server \
said so. `plan_session` says what a session would cost and degrade without \
running it; `run_session` runs one only with --allow-run, bounded by it. A \
session that launches a program runs only if that person wrote it and put it in \
the directory they passed with --allow-launch: `list_approved` lists those, and \
`approved` names one. What such a session runs is the file's, never the call's.";

pub struct Config {
    pub dir: PathBuf,
    pub omn: PathBuf,
    pub allow_run: Option<Duration>,
    /// The directory of session files the person who started the server
    /// approved, whose launch targets may run.
    pub allow_launch: Option<PathBuf>,
}

impl Config {
    pub fn from_args(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut dir = None;
        let mut omn = None;
        let mut allow_run = None;
        let mut allow_launch = None;
        while let Some(arg) = args.next() {
            let mut value = |flag: &str| args.next().ok_or_else(|| format!("{flag} needs a value"));
            match arg.as_str() {
                "--dir" => dir = Some(PathBuf::from(value("--dir")?)),
                "--omn" => omn = Some(PathBuf::from(value("--omn")?)),
                "--allow-run" => {
                    let text = value("--allow-run")?;
                    let duration: Duration = text
                        .parse()
                        .map_err(|error| format!("--allow-run {text}: {error}"))?;
                    if duration.as_nanos() == 0 {
                        return Err("--allow-run 0 would allow nothing; leave it out".into());
                    }
                    allow_run = Some(duration);
                }
                "--allow-launch" => {
                    allow_launch = Some(PathBuf::from(value("--allow-launch")?));
                }
                "-h" | "--help" => return Err("omn-mcp".into()),
                other => return Err(format!("unknown argument `{other}`")),
            }
        }
        let allow_launch = match (allow_launch.map(absolute), allow_run) {
            (Some(_), None) => {
                return Err(
                    "--allow-launch means something only with --allow-run: it says which \
                     sessions may launch a program, and without --allow-run none runs at all"
                        .into(),
                )
            }
            (Some(dir), Some(_)) if !dir.is_dir() => {
                return Err(format!("--allow-launch {}: not a directory", dir.display()))
            }
            (launch, _) => launch,
        };
        Ok(Self {
            dir: absolute(dir.unwrap_or_else(|| PathBuf::from("."))),
            omn: omn.map(absolute).unwrap_or_else(Binary::beside_this_server),
            allow_run,
            allow_launch,
        })
    }
}

pub struct Server {
    config: Config,
    catalog: Catalog,
    binary: Binary,
    runs_started: u64,
}

/// A JSON-RPC error, for a message the protocol itself cannot answer.
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }
}

impl Server {
    pub fn new(config: Config) -> Result<Self, String> {
        let catalog =
            Catalog::embedded().map_err(|error| format!("the built-in catalog: {error}"))?;
        let binary = Binary {
            path: config.omn.clone(),
        };
        Ok(Self {
            config,
            catalog,
            binary,
            runs_started: 0,
        })
    }

    /// Answers one line, or nothing for a notification.
    pub fn handle_line(&mut self, line: &str) -> Option<Value> {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(error) => {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {"code": -32700, "message": format!("not JSON: {error}")},
                }))
            }
        };
        if let Value::Array(batch) = &message {
            let replies: Vec<Value> = batch.iter().filter_map(|each| self.handle(each)).collect();
            return (!replies.is_empty()).then_some(Value::Array(replies));
        }
        self.handle(&message)
    }

    fn handle(&mut self, message: &Value) -> Option<Value> {
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str);
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let (Some(id), Some(method)) = (id, method) else {
            // A notification, or a response to nothing this server asked.
            return None;
        };
        let outcome = self.dispatch(method, &params);
        Some(match outcome {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": error.code, "message": error.message},
            }),
        })
    }

    fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "initialize" => Ok(self.initialize(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => self.call(params),
            "resources/list" => Ok(json!({"resources": self.resources()})),
            "resources/templates/list" => Ok(json!({"resourceTemplates": templates()})),
            "resources/read" => self.read_resource(params),
            "prompts/list" => Ok(json!({"prompts": []})),
            other => Err(RpcError {
                code: -32601,
                message: format!("no method `{other}`"),
            }),
        }
    }

    fn initialize(&self, params: &Value) -> Value {
        let asked = params.get("protocolVersion").and_then(Value::as_str);
        let version = asked
            .filter(|asked| PROTOCOL_VERSIONS.contains(asked))
            .unwrap_or(PROTOCOL_VERSIONS[0]);
        json!({
            "protocolVersion": version,
            "capabilities": {
                "tools": {"listChanged": false},
                "resources": {"listChanged": false, "subscribe": false},
            },
            "serverInfo": {"name": "omn-mcp", "version": env!("CARGO_PKG_VERSION")},
            "instructions": INSTRUCTIONS,
        })
    }

    fn call(&mut self, params: &Value) -> Result<Value, RpcError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("tools/call needs a `name`"))?;
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
        if !arguments.is_object() {
            return Err(RpcError::invalid_params("`arguments` is an object"));
        }
        let outcome = match name {
            "catalog" => self.catalog_tool(&arguments),
            "defaults" => self.defaults_tool(&arguments),
            "capabilities" => self.capabilities_tool(&arguments),
            "list_runs" => Ok(self.list_runs()),
            "read_run" => self.read_run(&arguments),
            "plan_session" => self.plan_session(&arguments),
            "list_approved" => self.list_approved(),
            "run_session" => self.run_session(&arguments),
            other => return Err(RpcError::invalid_params(format!("no tool `{other}`"))),
        };
        Ok(match outcome {
            Ok(value) => json!({
                "content": [{"type": "text", "text": value.to_string()}],
                "structuredContent": value,
                "isError": false,
            }),
            // A refusal is an answer, with its reason, not a protocol failure.
            Err(reason) => json!({
                "content": [{"type": "text", "text": reason}],
                "isError": true,
            }),
        })
    }

    fn catalog_tool(&self, arguments: &Value) -> Result<Value, String> {
        let entries = self.entries(
            arguments.get("source").and_then(Value::as_str),
            arguments.get("family").and_then(Value::as_str),
        )?;
        Ok(json!({
            "sources": entries,
            "missing": [
                "the catalog is what a source is and what each backend declares, not what this \
                 machine can serve: `capabilities` asks the omn binary that",
            ],
        }))
    }

    /// `defaults`: what `omn trace <family> -- PROGRAM` composes, per band or
    /// one band, from the catalog's own `default:` blocks.
    fn defaults_tool(&self, arguments: &Value) -> Result<Value, String> {
        let band = arguments
            .get("cost")
            .and_then(Value::as_str)
            .map(omn::lazy::parse_band)
            .transpose()?;
        let defaults: Vec<&omn::catalog::FamilyDefault> =
            match arguments.get("family").and_then(Value::as_str) {
                None => self.catalog.defaults().iter().collect(),
                Some(family) => vec![self.catalog.default_of(family).ok_or_else(|| {
                    format!(
                        "no family `{family}` has a default; the families are {}",
                        self.catalog
                            .defaults()
                            .iter()
                            .map(|default| default.family.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?],
            };
        let listed: Vec<Value> = defaults
            .into_iter()
            .map(|default| {
                let mut value = json!({"family": default.family});
                if let Some(question) = &default.question {
                    value["question"] = json!(question);
                }
                if let Some(none) = &default.none {
                    value["none"] = json!(none);
                    return value;
                }
                match band {
                    Some(band) => {
                        value["cost"] = json!(band.as_str());
                        value["requests"] = json!(default.band(band));
                    }
                    None => {
                        for (band, requests) in default.bands() {
                            value[band.as_str()] = json!(requests);
                        }
                    }
                }
                value
            })
            .collect();
        Ok(json!({
            "defaults": listed,
            "missing": [
                "a request here has no target: it takes the one the person gives, and is a \
                 session's request once one is added (`plan_session` says what it would cost \
                 here), unless it says `target: system`",
                "a band is the catalog's cost word for the backend the default is written \
                 against; what this machine serves it with, and at what cost, is \
                 `plan_session`'s answer",
            ],
        }))
    }

    fn entries(&self, source: Option<&str>, family: Option<&str>) -> Result<Value, String> {
        let selected: Vec<_> = match (source, family) {
            (Some(_), Some(_)) => return Err("pass `source` or `family`, not both".into()),
            (Some(source), None) => {
                let entry = source
                    .parse()
                    .ok()
                    .and_then(|id| self.catalog.get(&id))
                    .ok_or_else(|| {
                        format!("`{source}` is not in the catalog; `catalog` with no argument lists every source")
                    })?;
                vec![entry]
            }
            (None, Some(family)) => {
                let entries = self.catalog.family(family);
                if entries.is_empty() {
                    return Err(format!(
                        "no family `{family}`; the families are {}",
                        self.catalog.families().join(", ")
                    ));
                }
                entries
            }
            (None, None) => self.catalog.entries().iter().collect(),
        };
        serde_json::to_value(selected).map_err(|error| error.to_string())
    }

    fn capabilities_tool(&self, arguments: &Value) -> Result<Value, String> {
        let mut answer = self.binary.sweep(&self.catalog)?;
        if let Some(source) = arguments.get("source").and_then(Value::as_str) {
            if !source
                .parse()
                .ok()
                .is_some_and(|id| self.catalog.get(&id).is_some())
            {
                return Err(format!("`{source}` is not in the catalog"));
            }
            let row = answer["sources"]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["source"] == source))
                .cloned()
                .unwrap_or(Value::Null);
            answer = json!({
                "source": row,
                "listing": self.binary.listing(source)?,
            });
        }
        answer["answered_by"] = self.binary.answered_by();
        answer["missing"] = json!(self.binary.capability_shortfall());
        Ok(answer)
    }

    fn list_runs(&self) -> Value {
        let found = runs::find(&self.config.dir, RUN_DEPTH);
        let outlines: Vec<Value> = found
            .iter()
            .map(|dir| match runs::read(dir) {
                Ok(run) => runs::outline(&self.config.dir, &run),
                Err(error) => json!({
                    "run": runs::name(&self.config.dir, dir),
                    "unreadable": error,
                }),
            })
            .collect();
        json!({
            "dir": self.config.dir.display().to_string(),
            "runs": outlines,
            "missing": if found.is_empty() {
                json!([format!("no run under {} (a run is a directory `omn -o` wrote, holding resolved.json), {RUN_DEPTH} levels down", self.config.dir.display())])
            } else {
                json!([])
            },
        })
    }

    fn read_run(&self, arguments: &Value) -> Result<Value, String> {
        let name = arguments
            .get("run")
            .and_then(Value::as_str)
            .ok_or("`run` names the run to read; `list_runs` lists them")?;
        let label = arguments.get("label").and_then(Value::as_str);
        let max_events = match arguments.get("max_events") {
            None | Some(Value::Null) => DEFAULT_MAX_EVENTS,
            Some(value) => value
                .as_u64()
                .ok_or("`max_events` is a whole number, 0 for every event")?
                as usize,
        };
        let dir = runs::locate(&self.config.dir, name)?;
        let run = runs::read(&dir)?;
        let mut answer = runs::bundle(&self.config.dir, &run, label, max_events);
        if label.is_some() {
            if let Some(missing) = answer["missing"].as_array_mut() {
                missing.push(json!(
                    "joins and decompositions belong to the whole run and are not in an answer \
                     narrowed to one label"
                ));
            }
        }
        Ok(answer)
    }

    fn plan_session(&self, arguments: &Value) -> Result<Value, String> {
        let (source, composed) = self.source(arguments)?;
        let mut answer =
            session::plan(&self.catalog, &self.binary, &source, self.launch_allowed())?;
        if let Some(notes) = composed {
            answer["composed"] = json!(notes);
        }
        Ok(answer)
    }

    /// The session a call names: a family's default composed, or a file, a
    /// text or an approved session.
    fn source(&self, arguments: &Value) -> Result<(Source, Option<Vec<String>>), String> {
        if let Some((source, notes)) = session::composed(&self.catalog, arguments)? {
            return Ok((source, Some(notes)));
        }
        let source = Source::from_arguments(
            arguments,
            &std::env::current_dir().unwrap_or_default(),
            self.config.allow_launch.as_deref(),
        )?;
        Ok((source, None))
    }

    /// Whether an approved session may launch its program here.
    fn launch_allowed(&self) -> bool {
        self.config.allow_run.is_some() && self.config.allow_launch.is_some()
    }

    /// `list_approved`: the sessions of the `--allow-launch` directory, each
    /// with what `plan_session` says of it.
    fn list_approved(&self) -> Result<Value, String> {
        let Some(dir) = self.config.allow_launch.as_deref() else {
            return Err(
                "refused: this server was started without --allow-launch, so it approves no \
                 session; a session that launches a program is for a person to run with \
                 `omn -f`, or to put in the directory they pass with --allow-launch DIR"
                    .into(),
            );
        };
        let sessions: Vec<Value> = Source::every_approved(dir)
            .into_iter()
            .map(|(name, source)| match source {
                Ok(source) => {
                    match session::plan(&self.catalog, &self.binary, &source, self.launch_allowed())
                    {
                        Ok(plan) => json!({"approved": name, "plan": plan}),
                        Err(error) => json!({"approved": name, "unreadable": error}),
                    }
                }
                Err(error) => json!({"approved": name, "refused": error}),
            })
            .collect();
        Ok(json!({
            "dir": dir.display().to_string(),
            "sessions": sessions,
            "missing": if sessions.is_empty() {
                json!([format!("no session file (yaml, yml, json or toml) under {}", dir.display())])
            } else {
                json!([])
            },
        }))
    }

    fn run_session(&mut self, arguments: &Value) -> Result<Value, String> {
        let Some(longest) = self.config.allow_run else {
            return Err(
                "refused: this server was started without --allow-run, so it runs nothing. It \
                 reads what omn wrote and says what a session would cost (`plan_session`); a \
                 person runs it with `omn -f`, or starts this server with `--allow-run 10s` to \
                 let bounded sessions over existing targets through"
                    .into(),
            );
        };
        let duration = arguments
            .get("duration")
            .and_then(Value::as_str)
            .ok_or("`duration` is required: how long the run lasts, at most the server's bound")?;
        let budget = match arguments.get("budget") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(text.as_str()),
            Some(other) => {
                return Err(format!(
                    "`budget` is a percentage written as a string (`2%`), not {other}"
                ))
            }
        };
        let (source, _) = self.source(arguments)?;
        self.runs_started += 1;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis());
        let relative = PathBuf::from("mcp-runs").join(format!(
            "{stamp}-{}-{}",
            std::process::id(),
            self.runs_started
        ));
        let out = self.config.dir.join(&relative);
        let mut answer = session::run(
            &self.catalog,
            &self.binary,
            &source,
            RunLimit {
                longest,
                launch_allowed: self.launch_allowed(),
            },
            duration,
            budget,
            &out,
        )?;
        let name = relative.display().to_string();
        answer["run"] = json!(name);
        match runs::read(&out) {
            Ok(run) => {
                answer["read_back"] = runs::bundle(&self.config.dir, &run, None, DEFAULT_MAX_EVENTS)
            }
            Err(error) => answer["read_back"] = json!({"unreadable": error}),
        }
        Ok(answer)
    }

    fn resources(&self) -> Vec<Value> {
        let mut list = vec![
            resource(
                "omn://catalog",
                "catalog",
                "Every source, as the catalog declares it",
            ),
            resource(
                "omn://evidence",
                "evidence ladder",
                "What each evidence level establishes and does not, weakest first",
            ),
            resource(
                "omn://capabilities",
                "capability sweep",
                "What this machine can serve, asked of the omn binary that would run it",
            ),
            resource(
                "omn://runs",
                "runs",
                "The runs under the server's directory",
            ),
        ];
        for dir in runs::find(&self.config.dir, RUN_DEPTH) {
            let name = runs::name(&self.config.dir, &dir);
            for file in run_files(&dir) {
                list.push(resource(
                    &format!("omn://runs/{name}/{file}"),
                    &format!("{name}/{file}"),
                    "As omn wrote it, byte for byte",
                ));
            }
        }
        list
    }

    fn read_resource(&self, params: &Value) -> Result<Value, RpcError> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("resources/read needs a `uri`"))?;
        let not_found = |why: String| RpcError {
            code: -32002,
            message: why,
        };
        let (mime, text) = match uri {
            "omn://catalog" => (
                "application/json",
                self.entries(None, None).map_err(not_found)?.to_string(),
            ),
            "omn://evidence" => ("application/json", evidence::ladder().to_string()),
            "omn://capabilities" => (
                "application/json",
                self.capabilities_tool(&json!({}))
                    .map_err(not_found)?
                    .to_string(),
            ),
            "omn://runs" => ("application/json", self.list_runs().to_string()),
            _ => {
                if let Some(source) = uri.strip_prefix("omn://catalog/") {
                    (
                        "application/json",
                        self.entries(Some(source), None)
                            .map_err(not_found)?
                            .to_string(),
                    )
                } else if let Some(rest) = uri.strip_prefix("omn://runs/") {
                    let (name, file) = rest
                        .rsplit_once('/')
                        .ok_or_else(|| not_found(format!("`{uri}` names no file of a run")))?;
                    let dir = runs::locate(&self.config.dir, name).map_err(not_found)?;
                    if !run_files(&dir).iter().any(|each| each == file) {
                        return Err(not_found(format!(
                            "`{file}` is not one of the files omn writes in a run"
                        )));
                    }
                    let text = std::fs::read_to_string(dir.join(file))
                        .map_err(|error| not_found(format!("{uri}: {error}")))?;
                    let mime = if file.ends_with(".json") {
                        "application/json"
                    } else {
                        "text/plain"
                    };
                    (mime, text)
                } else {
                    return Err(not_found(format!("no resource `{uri}`")));
                }
            }
        };
        Ok(json!({"contents": [{"uri": uri, "mimeType": mime, "text": text}]}))
    }
}

/// The files of a run this server hands out: the ones omn writes.
fn run_files(dir: &Path) -> Vec<String> {
    let mut files: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|file| {
                    file == "resolved.json" || file == "cost.json" || file.starts_with("results.")
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

fn resource(uri: &str, name: &str, description: &str) -> Value {
    json!({
        "uri": uri,
        "name": name,
        "description": description,
        "mimeType": if uri.starts_with("omn://runs/") && !uri.ends_with(".json") {
            "text/plain"
        } else {
            "application/json"
        },
    })
}

fn templates() -> Vec<Value> {
    vec![
        json!({
            "uriTemplate": "omn://catalog/{source}",
            "name": "one source",
            "description": "One catalog entry: its backends in order, what each needs and degrades, its caveats",
            "mimeType": "application/json",
        }),
        json!({
            "uriTemplate": "omn://runs/{run}/{file}",
            "name": "a file of a run",
            "description": "resolved.json, results.<ext> or cost.json, as omn wrote it",
        }),
    ]
}

fn session_properties() -> Value {
    json!({
        "path": {
            "type": "string",
            "description": "A session file (yaml, yml, json or toml), relative to the server's working directory",
        },
        "session": {
            "type": "string",
            "description": "The session's text, in place of a path",
        },
        "input_format": {
            "type": "string",
            "enum": ["yaml", "json", "toml"],
            "description": "The format of `session`; yaml when absent",
        },
        "approved": {
            "type": "string",
            "description": "One of the approved sessions, by the name `list_approved` gives it: a file of the directory the server was started with --allow-launch. The only kind of session that may launch a program",
        },
        "family": {
            "type": "string",
            "description": "In place of a session: a family (`cpu`, `io`, ...), whose default is composed as `omn trace <family>` composes it, the band by `cost`. The answer's `composed` says what it decided, and `catalog` with `family` lists the sources to choose from instead",
        },
        "cost": {
            "type": "string",
            "enum": ["low", "med", "high"],
            "description": "Beside `family`: the band of its default; the cheapest it has when absent",
        },
        "pid": {
            "type": "array",
            "items": {"type": "integer"},
            "description": "Beside `family`: the processes to target",
        },
        "cgroup": {"type": "string", "description": "Beside `family`: a cgroup v2 path to target"},
        "system": {"type": "boolean", "description": "Beside `family`: the whole machine (give `duration`)"},
        "program": {
            "type": "array",
            "items": {"type": "string"},
            "description": "Beside `family`: a program to launch, for `plan_session`; `run_session` launches only approved sessions",
        },
    })
}

fn tools() -> Vec<Value> {
    let read_only = json!({"readOnlyHint": true, "destructiveHint": false, "openWorldHint": false});
    let mut run_properties = session_properties();
    run_properties["duration"] = json!({
        "type": "string",
        "description": "How long the run lasts (`2s`), at most what the server was started to allow",
    });
    run_properties["budget"] = json!({
        "type": "string",
        "description": "Optional: an overhead ceiling, a percentage of the target's own cpu over the window (`2%`), past which the run steps down or stops. A target that mostly waits has little cpu for it to be a percentage of; the answer's `budget_against` says, per request, what it was measured against. Absent, the run is not stopped for its cost, which is measured all the same",
    });
    vec![
        json!({
            "name": "catalog",
            "title": "The catalog",
            "description": "Sources as the catalog declares them: family, unit, modes, emits, targets, keys, filter fields, the backend chain in order with each backend's requirements, cost band and degradation, and the caveats that say what a number does not mean. Machine-independent.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": {"type": "string", "description": "One source, as <family>.<metric>"},
                    "family": {"type": "string", "description": "Every source of one family"},
                },
            },
            "annotations": read_only,
        }),
        json!({
            "name": "defaults",
            "title": "What a family's lazy mode composes",
            "description": "Each family's default, what `omn trace <family> -- PROGRAM` composes: the question it answers and the requests of each cost band (low, med, high), or why the family has no safe default. With `cost`, that band's requests alone. A request takes the target the person gives: add one and it is a session's request for `plan_session` or `run_session`. Machine-independent.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "family": {"type": "string", "description": "One family (`io`)"},
                    "cost": {"type": "string", "enum": ["low", "med", "high"], "description": "One cost band"},
                },
            },
            "annotations": read_only,
        }),
        json!({
            "name": "capabilities",
            "title": "What this machine can serve",
            "description": "`omn list`'s capability sweep, asked of the omn binary that would run a session, with the capabilities that binary carries: per source, available, unverified (never counted as available) or unavailable. With `source`, the full listing: every backend, every requirement, and the probe's answer and reason for each.",
            "inputSchema": {
                "type": "object",
                "properties": {"source": {"type": "string"}},
            },
            "annotations": read_only,
        }),
        json!({
            "name": "list_runs",
            "title": "The runs on disk",
            "description": "The runs under the server's directory -- each a directory `omn -o` wrote -- with their requests, backends, why each stopped, and which are lossy or carry a shortfall.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": read_only,
        }),
        json!({
            "name": "read_run",
            "title": "One run, per request",
            "description": "A run's results document, resolved file and cost, regrouped per request and otherwise as written: each result with its outputs, `short` marks, losses, window, lifetime and degradations; each resolved backend with its chain, degradations, unverified requirements and filter placement; each cost declared and measured; the joins and decompositions with their evidence, each annotated with what that level establishes. `what_it_is_missing` restates each request's losses, shortfalls, degradations and timestamp error first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "run": {"type": "string", "description": "The run's name, as list_runs gives it"},
                    "label": {"type": "string", "description": "Only the request with this label"},
                    "max_events": {"type": "integer", "minimum": 0, "description": "Raw events per output, 100 by default; 0 for every one. What is cut is counted in the answer"},
                },
                "required": ["run"],
            },
            "annotations": read_only,
        }),
        json!({
            "name": "plan_session",
            "title": "What a session would cost and degrade",
            "description": "Validates a session against the catalog and resolves it against this machine through the omn binary, without running anything: per request, the backend that would serve it, its rank in the chain, its declared cost band, what it degrades, what could not be verified, and where its filter would run -- or the rejection, with whose it is (the request, the machine, or this build).",
            "inputSchema": {"type": "object", "properties": session_properties()},
            "annotations": read_only,
        }),
        json!({
            "name": "list_approved",
            "title": "The sessions that may launch a program",
            "description": "The session files of the directory the person who started this server passed with --allow-launch, each with what `plan_session` says of it. These are the only sessions `run_session` lets launch a program, named by `approved`; what they run is the file's, and nothing in a call changes it.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": read_only,
        }),
        json!({
            "name": "run_session",
            "title": "Run one bounded session",
            "description": "Runs a session through the omn binary for a bounded duration, under a budget when one is given, and reads the run back. Only when the server was started with --allow-run, only up to the duration it allows. A session that launches a program runs only when it is `approved`: a file of the --allow-launch directory, run as the file states it, from its own directory; the answer then carries what the program printed on stdout, told apart from omn's document (`program_stdout`), and on stderr, which omn writes to a file of its own rather than among its lines (`program_stderr`). A run a budget stopped before its end says so, per request (`stopped_by_budget`; null when nothing stopped it). Every other session observes targets that already exist.",
            "inputSchema": {
                "type": "object",
                "properties": run_properties,
                "required": ["duration"],
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false},
        }),
    ]
}
