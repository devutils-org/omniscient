//! Runs on disk: the directories `omn -o DIR` writes, read and never rewritten.
//!
//! A run is a directory holding `resolved.json`, and beside it `results.<ext>`
//! and `cost.json`. What this module hands out is those documents, verbatim,
//! regrouped per request so that a number never travels without what it is
//! missing: each request's result (its outputs with every `short` mark, its
//! losses, window, lifetime, degradations and why it stopped) beside its
//! resolved backend (degradations, unverified requirements, the filter's
//! placement, the chain) and its cost (declared and measured).
//!
//! Two things are added, and nothing is removed without saying so:
//!
//! - `evidence_means` beside every `evidence` field, from [`crate::evidence`];
//! - `elided` inside a `raw` output whose events were cut to `max_events`,
//!   with how many there were. `max_events: 0` hands every event over.
//!
//! Everything else is the document's own value, and `mcp/tests/` checks that
//! against the files, number by number.

use std::path::{Component, Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::evidence;

pub const ELIDED: &str = "elided";

/// Every run under `root`, `root` itself included, at most `depth` levels down.
pub fn find(root: &Path, depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    visit(root, depth, &mut found);
    found.sort();
    found
}

fn visit(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if dir.join("resolved.json").is_file() {
        found.push(dir.to_path_buf());
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Symbolic links are not followed: a run is what is under the root.
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            visit(&path, depth - 1, found);
        }
    }
}

/// The name a run is asked for by: its path relative to the root.
pub fn name(root: &Path, run: &Path) -> String {
    match run.strip_prefix(root) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".to_owned(),
        Ok(relative) => relative.display().to_string(),
        Err(_) => run.display().to_string(),
    }
}

/// Finds the run a name designates, refusing anything that leaves the root.
pub fn locate(root: &Path, name: &str) -> Result<PathBuf, String> {
    let relative = Path::new(name);
    if relative.is_absolute()
        || relative
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(format!(
            "`{name}` is not a run under {}: a run is named by its path relative to the \
             directory this server reads, without `..`",
            root.display()
        ));
    }
    let dir = root.join(relative);
    let (Ok(canonical), Ok(canonical_root)) = (dir.canonicalize(), root.canonicalize()) else {
        return Err(format!("no run `{name}` under {}", root.display()));
    };
    if !canonical.starts_with(&canonical_root) {
        return Err(format!(
            "`{name}` leads outside {} through a link",
            root.display()
        ));
    }
    if !canonical.join("resolved.json").is_file() {
        return Err(format!(
            "`{name}` holds no resolved.json, so it is not a run `omn -o` wrote; `list_runs` \
             names the ones there are"
        ));
    }
    Ok(canonical)
}

/// The files of one run, read as they are.
pub struct Run {
    pub dir: PathBuf,
    pub resolved: Value,
    pub results: Option<Results>,
    pub cost: Option<Value>,
    /// What is absent or unreadable, said once per thing.
    pub missing: Vec<String>,
}

pub enum Results {
    /// `results.json`, `.yaml` or `.toml`: a document, read as one.
    Document { file: String, value: Value },
    /// `results.txt` and the other renderings: text for a terminal.
    Rendered { file: String },
}

pub fn read(dir: &Path) -> Result<Run, String> {
    let resolved = read_json(&dir.join("resolved.json"))?;
    let mut missing = Vec::new();
    let cost = match read_json(&dir.join("cost.json")) {
        Ok(value) => Some(value),
        Err(error) => {
            missing.push(format!(
                "no cost: {error}, so what the run measured against what it declared is not here"
            ));
            None
        }
    };
    let results = read_results(dir, &mut missing);
    Ok(Run {
        dir: dir.to_path_buf(),
        resolved,
        results,
        cost,
        missing,
    })
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| format!("{} is not JSON: {error}", path.display()))
}

fn read_results(dir: &Path, missing: &mut Vec<String>) -> Option<Results> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        missing.push("the run's directory cannot be listed".into());
        return None;
    };
    let mut files: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|file| file.starts_with("results."))
        .collect();
    files.sort();
    let Some(file) = files.first().cloned() else {
        missing.push(
            "no results file: the run wrote its resolved requests and nothing it measured (it \
             was refused, or stopped before its first document)"
                .into(),
        );
        return None;
    };
    let path = dir.join(&file);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            missing.push(format!("cannot read {file}: {error}"));
            return None;
        }
    };
    let parsed = match file.rsplit('.').next() {
        Some("json") => serde_json::from_str::<Value>(&text).map_err(|e| e.to_string()),
        Some("yaml") => omn::format::InputFormat::Yaml
            .parse::<Value>(&text)
            .map_err(|e| e.to_string()),
        Some("toml") => omn::format::InputFormat::Toml
            .parse::<Value>(&text)
            .map_err(|e| e.to_string()),
        _ => {
            missing.push(format!(
                "the results are `{file}`, a rendering for a terminal and not a document: its \
                 numbers are not handed out as fields, because a field read back out of prose \
                 is a field whose shortfall may have been left behind. Rerun with `--format \
                 json`; the text itself is the resource omn://runs/<run>/{file}"
            ));
            return Some(Results::Rendered { file });
        }
    };
    match parsed {
        Ok(value) => Some(Results::Document { file, value }),
        Err(error) => {
            missing.push(format!("{file} does not parse: {error}"));
            None
        }
    }
}

/// A short line per run, for `list_runs`: what it asked, how it ended, and
/// whether anything in it is short.
pub fn outline(root: &Path, run: &Run) -> Value {
    let requests: Vec<Value> = as_array(&run.resolved)
        .iter()
        .map(|resolved| {
            json!({
                "label": resolved["request"].get("label").cloned().unwrap_or(Value::Null),
                "source": resolved["request"]["source"],
                "backend": resolved["backend"]["id"],
                "degraded": !as_array(&resolved["degradations"]).is_empty(),
                "unverified": !as_array(&resolved["unverified"]).is_empty(),
            })
        })
        .collect();
    let mut outline = json!({
        "run": name(root, &run.dir),
        "requests": requests,
        "missing": run.missing,
    });
    if let Some(Results::Document { file, value }) = &run.results {
        let lossy: Vec<Value> = as_array(&value["results"])
            .iter()
            .filter_map(|result| {
                let lost = nonzero_losses(&result["losses"]);
                let short = !shortfalls(result).is_empty();
                (!lost.is_empty() || short).then(|| {
                    json!({
                        "label": result["label"],
                        "losses": Value::Object(lost),
                        "carries_a_shortfall": short,
                    })
                })
            })
            .collect();
        outline["results"] = json!(file);
        outline["stopped_because"] = Value::Array(
            as_array(&value["results"])
                .iter()
                .map(|result| result["stopped_because"].clone())
                .collect(),
        );
        outline["short_or_lossy"] = Value::Array(lossy);
    }
    if let Some(Results::Rendered { file }) = &run.results {
        outline["results"] = json!(file);
    }
    outline
}

/// The run, regrouped per request, with nothing a number depends on left out.
pub fn bundle(root: &Path, run: &Run, label: Option<&str>, max_events: usize) -> Value {
    let mut missing = run.missing.clone();
    let resolved = as_array(&run.resolved);
    let cost = run.cost.as_ref().map(as_array).unwrap_or_default();
    let document = match &run.results {
        Some(Results::Document { value, .. }) => Some(value),
        _ => None,
    };
    let results = document
        .map(|value| as_array(&value["results"]))
        .unwrap_or_default();

    // Results, resolved requests and costs are written in request order, one
    // each. Paired by position only when every position names the same
    // source; otherwise the three are handed over apart and the answer says
    // they could not be paired, rather than pairing the wrong two.
    let paired = document.is_some()
        && results.len() == resolved.len()
        && results.iter().zip(&resolved).all(|(result, resolved)| {
            result["source"] == resolved["request"]["source"]
                && result["label"]
                    == resolved["request"]
                        .get("label")
                        .cloned()
                        .unwrap_or_else(|| resolved["request"]["source"].clone())
        })
        && (cost.is_empty()
            || (cost.len() == results.len()
                && cost
                    .iter()
                    .zip(&results)
                    .all(|(cost, result)| cost["label"] == result["label"])));

    let environment = resolved.first().map(|first| first["environment"].clone());
    let same_environment = resolved
        .iter()
        .all(|each| Some(&each["environment"]) == environment.as_ref());

    let mut requests = Vec::new();
    for (index, resolved) in resolved.iter().enumerate() {
        let request_label = resolved["request"]
            .get("label")
            .and_then(Value::as_str)
            .or_else(|| resolved["request"]["source"].as_str())
            .unwrap_or_default()
            .to_owned();
        if label.is_some_and(|wanted| wanted != request_label) {
            continue;
        }
        let mut resolved = resolved.clone();
        if same_environment {
            if let Some(object) = resolved.as_object_mut() {
                object.remove("environment");
            }
        }
        let mut entry = json!({"label": request_label, "resolved": resolved});
        if paired {
            let mut result = results[index].clone();
            elide(&mut result, max_events, &mut missing, &request_label);
            entry["what_it_is_missing"] = what_it_is_missing(&result, &entry["resolved"]);
            entry["result"] = result;
            if let Some(cost) = cost.get(index) {
                entry["cost"] = cost.clone();
            }
        }
        requests.push(entry);
    }
    if label.is_some() && requests.is_empty() {
        missing.push(format!(
            "no request labelled `{}` in this run",
            label.unwrap_or_default()
        ));
    }

    let mut answer = json!({
        "run": name(root, &run.dir),
        "requests": requests,
    });
    if same_environment {
        if let Some(environment) = environment {
            answer["environment"] = environment;
        }
    }
    if let Some(document) = document {
        if !paired {
            missing.push(
                "the results, the resolved requests and the costs do not name the same requests \
                 in the same order, so they are handed over apart and unpaired"
                    .into(),
            );
            let mut results = document["results"].clone();
            if let Value::Array(items) = &mut results {
                for result in items {
                    elide(result, max_events, &mut missing, "?");
                }
            }
            answer["unpaired"] = json!({"results": results, "cost": run.cost});
        }
        // Joins and decompositions belong to the run, not to a request.
        for key in ["joins", "decompositions"] {
            if let Some(value) = document.get(key) {
                if label.is_none() {
                    answer[key] = value.clone();
                }
            }
        }
        // Anything else the document carries, passed on under its own name,
        // so a field added to the results after this server was written is
        // not dropped for being unknown.
        if let Some(object) = document.as_object() {
            for (key, value) in object {
                if !matches!(key.as_str(), "results" | "joins" | "decompositions")
                    && label.is_none()
                {
                    answer[key.as_str()] = value.clone();
                }
            }
        }
    }
    let problems = evidence::annotate(&mut answer);
    missing.extend(problems);
    if answer.get("joins").is_some() || answer.get("decompositions").is_some() {
        answer["evidence_ladder"] = evidence::ladder();
    }
    answer["missing"] = json!(missing);
    answer
}

/// What one request's numbers lack, hoisted out of the fields that say so.
///
/// Nothing here is computed: each entry is a field of the result or of the
/// resolved request, restated at the top of the request so that it is the first
/// thing read and not a field deep in an output.
fn what_it_is_missing(result: &Value, resolved: &Value) -> Value {
    json!({
        "losses": Value::Object(nonzero_losses(&result["losses"])),
        "loss_reasons": result["losses"].get("reasons").cloned().unwrap_or(Value::Null),
        "shortfalls": shortfalls(result),
        "degradations": result.get("degradations").cloned().unwrap_or_else(|| json!([])),
        "unverified_requirements": resolved.get("unverified").cloned().unwrap_or_else(|| json!([])),
        "filter": resolved.get("filter").cloned().unwrap_or(Value::Null),
        "timestamp_error": timestamp_error(result),
        "stopped_because": result["stopped_because"],
    })
}

/// Every non-zero loss counter, as the document names it.
fn nonzero_losses(losses: &Value) -> Map<String, Value> {
    let mut lost = Map::new();
    if let Some(object) = losses.as_object() {
        for (key, value) in object {
            if value.as_u64().is_some_and(|count| count > 0)
                || value.as_f64().is_some_and(|count| count > 0.0)
            {
                lost.insert(key.clone(), value.clone());
            }
        }
    }
    lost
}

/// Every `short` mark in a result's outputs, with the series it is on.
fn shortfalls(result: &Value) -> Vec<Value> {
    let mut found = Vec::new();
    for output in as_array(&result["outputs"]) {
        for series in as_array(&output["series"]) {
            if let Some(short) = series.get("short") {
                found.push(json!({
                    "emit": output["emit"],
                    "key": series.get("key").cloned().unwrap_or(Value::Null),
                    "short": short,
                }));
            }
        }
        if let Some(short) = output.get("short") {
            found.push(json!({"emit": output["emit"], "short": short}));
        }
    }
    found
}

/// The largest `ts_err_ns` among the raw events handed over, and how many
/// carry one. Only `raw` serialises the field, and it is omitted when zero.
fn timestamp_error(result: &Value) -> Value {
    let mut raw_events = 0u64;
    let mut carrying = 0u64;
    let mut worst = 0u64;
    let mut has_raw = false;
    for output in as_array(&result["outputs"]) {
        if output["emit"] != "raw" {
            continue;
        }
        has_raw = true;
        for event in as_array(&output["events"]) {
            raw_events += 1;
            if let Some(error) = event.get("ts_err_ns").and_then(Value::as_u64) {
                carrying += 1;
                worst = worst.max(error);
            }
        }
    }
    if !has_raw {
        return json!({
            "said": "only a `raw` output carries each event's ts_err_ns; this result has none, so \
                     an error bar a collector stamped is here only if it said it as a degradation",
        });
    }
    json!({
        "raw_events_here": raw_events,
        "events_with_ts_err_ns": carrying,
        "worst_ts_err_ns": worst,
        "said": if carrying == 0 {
            "every event handed over was stamped in CLOCK_MONOTONIC at the source"
        } else {
            "events carrying ts_err_ns were converted from another clock and are known only to \
             within that many nanoseconds either side"
        },
    })
}

fn elide(result: &mut Value, max_events: usize, missing: &mut Vec<String>, label: &str) {
    if max_events == 0 {
        return;
    }
    let Some(outputs) = result.get_mut("outputs").and_then(Value::as_array_mut) else {
        return;
    };
    for output in outputs {
        let Some(object) = output.as_object_mut() else {
            continue;
        };
        let Some(Value::Array(events)) = object.get_mut("events") else {
            continue;
        };
        let total = events.len();
        if total <= max_events {
            continue;
        }
        events.truncate(max_events);
        object.insert(
            ELIDED.to_owned(),
            json!({
                "events_in_the_document": total,
                "events_here": max_events,
                "note": "the first events in document order; ask again with max_events: 0 for all",
            }),
        );
        missing.push(format!(
            "[{label}] {} of {total} raw events are not in this answer (max_events {max_events})",
            total - max_events
        ));
    }
}

fn as_array(value: &Value) -> Vec<Value> {
    value.as_array().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_that_climbs_out_is_refused() {
        let root = std::env::temp_dir();
        for name in ["../etc", "/etc", "a/../../b"] {
            assert!(locate(&root, name).is_err(), "{name}");
        }
    }

    #[test]
    fn elision_says_how_many_it_left_out() {
        let mut result = json!({"outputs": [{"emit": "raw", "events": [1, 2, 3, 4, 5]}]});
        let mut missing = Vec::new();
        elide(&mut result, 2, &mut missing, "x");
        assert_eq!(result["outputs"][0]["events"], json!([1, 2]));
        assert_eq!(result["outputs"][0][ELIDED]["events_in_the_document"], 5);
        assert_eq!(missing.len(), 1);
    }

    #[test]
    fn only_nonzero_losses_are_hoisted() {
        let lost = nonzero_losses(&json!({"dropped": 0, "throttled": 4, "reasons": {}}));
        assert_eq!(Value::Object(lost), json!({"throttled": 4}));
    }
}
