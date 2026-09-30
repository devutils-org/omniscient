//! `docs/mcp.md`'s walk-through, driven call by call against `examples/mcp/approved/`.
//!
//! The use case: *"benchmark my program, and tell me what dominates its
//! time"*, asked of an agent. The person starts the server over the directory
//! of sessions they approved; the agent lists them, runs each, and reads what
//! comes back. The ground truth has two halves:
//!
//! - **the run's own files.** Every number and loss the agent is handed --
//!   `run_session`'s read-back, and `read_run`'s answer for the same run --
//!   is found unchanged in the `results.json`, `resolved.json` and
//!   `cost.json` that `omn` wrote, with only the server's two documented
//!   additions taken out first.
//! - **the program's own account.** `examples/mcp/approved/program.py` computes, sleeps
//!   and computes again, and says on stderr -- which reaches the agent in
//!   `said_by_omn` -- when it ran on its own clock, how long it slept, and
//!   the kernel's `schedstat` for it. The life the tool hands over holds the
//!   program's own span, the blocked row holds its sleep, and the row that
//!   dominates is the one the program's own figures say dominates.
//!
//! The approved directory is copied into a scratch directory first, so a run
//! writes nothing into the repository.

mod common;

#[path = "../../examples/bench/harness.rs"]
mod harness;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use common::*;
use serde_json::{json, Value};

/// The sessions of `examples/mcp/approved/`, in the order the walk-through runs them.
const SESSIONS: [&str; 4] = [
    "benchmark.yml",
    "profile.yml",
    "decompose.yml",
    "triage.yml",
];

/// How long one run may last and what it may cost: the bounds the walk-through
/// starts the server with, and the ones each call passes.
const ALLOW_RUN: &str = "20s";
const BUDGET: &str = "25%";

/// `examples/mcp/approved/`, copied: the approved directory the server is started with.
fn approved_copy(scratch: &Path) -> PathBuf {
    let from = repo().join("examples/mcp/approved");
    let to = scratch.join("approved");
    std::fs::create_dir_all(&to).unwrap();
    for entry in std::fs::read_dir(&from).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            std::fs::copy(&path, to.join(path.file_name().unwrap())).unwrap();
        }
    }
    to
}

/// The server as the walk-through starts it.
fn walk_through_server(scratch: &Path) -> Client {
    let approved = approved_copy(scratch);
    let runs = scratch.join("runs");
    std::fs::create_dir_all(&runs).unwrap();
    Client::start(&[
        "--dir",
        runs.to_str().unwrap(),
        "--allow-run",
        ALLOW_RUN,
        "--allow-launch",
        approved.to_str().unwrap(),
    ])
}

fn has_python() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// What the program said of itself, read out of what `omn` said.
fn truth(answer: &Value) -> BTreeMap<String, f64> {
    let line = answer["said_by_omn"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find_map(|line| line.strip_prefix("omn-truth "))
        .unwrap_or_else(|| {
            panic!(
                "the program did not say what it did: {}",
                answer["said_by_omn"]
            )
        });
    line.split_whitespace()
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            Some((name.to_owned(), value.parse().ok()?))
        })
        .collect()
}

fn number(value: &Value) -> f64 {
    value
        .as_f64()
        .unwrap_or_else(|| panic!("not a number: {value}"))
}

/// The one request of an answer with this label.
fn request<'a>(answer: &'a Value, label: &str) -> &'a Value {
    answer["requests"]
        .as_array()
        .and_then(|requests| requests.iter().find(|request| request["label"] == label))
        .unwrap_or_else(|| panic!("no request `{label}` in {answer}"))
}

/// The program's life as the answer hands it over: released to reaped.
fn life(answer: &Value, label: &str) -> (f64, f64, f64) {
    let lifetime = &request(answer, label)["result"]["lifetime"];
    (
        number(&lifetime["released_ns"]),
        number(&lifetime["exited_ns"]),
        number(&lifetime["exited_err_ns"]),
    )
}

/// Which approved sessions this machine serves, as `list_approved` says --
/// and, for each it does not, the machine's own reason, said out loud.
fn served(client: &mut Client) -> BTreeMap<String, bool> {
    let listed = client.call("list_approved", json!({}));
    let sessions = listed["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("list_approved lists no sessions: {listed}"));
    let mut served = BTreeMap::new();
    for session in sessions {
        let name = session["approved"].as_str().unwrap_or_default().to_owned();
        let plan = &session["plan"];
        assert_eq!(
            plan["through_run_session"]["allowed"], true,
            "an approved session is one run_session takes: {session}"
        );
        assert_eq!(plan["validation"]["valid"], true, "{session}");
        let resolved = plan["resolution"]["resolved"] == true;
        if !resolved {
            let resolution = &plan["resolution"];
            assert_eq!(
                resolution["exit"], 1,
                "a valid session this machine does not serve is the machine's answer: {session}"
            );
            eprintln!(
                "SKIP ({name}): this machine withholds what it needs, so it is not run here. \
                 The machine's reason, as list_approved hands it over: {}",
                resolution["said_by_omn"]
            );
        }
        served.insert(name, resolved);
    }
    served
}

/// The walk-through, call by call, and every number it hands over found in
/// the run's own files.
#[test]
fn the_walk_through_hands_over_what_the_runs_wrote_and_what_the_program_did() {
    if !has_python() {
        eprintln!("SKIP: the approved program is a python3 one, and this machine has none");
        return;
    }
    let scratch = scratch("walkthrough");
    let mut client = walk_through_server(&scratch);

    // 1. What may run: the four sessions, each with its plan.
    let served = served(&mut client);
    let names: Vec<&str> = served.keys().map(String::as_str).collect();
    let mut expected = SESSIONS.to_vec();
    expected.sort_unstable();
    assert_eq!(names, expected, "list_approved lists the directory");
    assert!(
        served["benchmark.yml"],
        "the benchmark reads /proc and the reap's rusage, which every machine serves"
    );

    let mut compared = 0;
    for name in SESSIONS {
        if !served[name] {
            continue;
        }
        // 2. Run it.
        let answer = client.call(
            "run_session",
            json!({"approved": name, "duration": ALLOW_RUN, "budget": BUDGET}),
        );
        assert_eq!(answer["exit"], 0, "{name}: {answer}");
        assert!(answer.get("killed").is_none(), "{name}: {answer}");
        assert_eq!(answer["approved"]["name"], name);
        let run = answer["run"].as_str().expect("the run is named").to_owned();
        let dir = scratch.join("runs").join(&run);

        // 3. What it hands over is what the run wrote. `run_session`'s read-back
        // cuts raw events at 100; `read_run` with `max_events: 0` cuts none.
        let read_back = &answer["read_back"];
        compared += assert_run_unchanged(read_back, &dir);
        let again = client.call("read_run", json!({"run": run, "max_events": 0}));
        compared += assert_run_unchanged(&again, &dir);
        assert_eq!(
            again["missing"],
            json!([]),
            "{name}: read whole, a run leaves nothing out: {}",
            again["missing"]
        );
        // Each request's shortfall is stated first, and every loss in it is
        // one the file carries.
        let written = read_json(&dir.join("results.json"));
        for (index, each) in read_back["requests"].as_array().unwrap().iter().enumerate() {
            let losses = &written["results"][index]["losses"];
            for (counter, count) in losses.as_object().into_iter().flatten() {
                let Some(count) = count.as_u64() else {
                    continue;
                };
                let hoisted = &each["what_it_is_missing"]["losses"][counter];
                if count > 0 {
                    assert_eq!(hoisted, count, "{name}: loss `{counter}` restated first");
                } else {
                    assert!(
                        hoisted.is_null(),
                        "{name}: a zero `{counter}` is not hoisted"
                    );
                }
            }
        }

        // 4. And it is the program's: the life holds the program's own span.
        let truth = truth(&answer);
        let label = read_back["requests"][0]["label"]
            .as_str()
            .unwrap()
            .to_owned();
        let (released, exited, exited_err) = life(read_back, &label);
        assert!(
            released <= truth["started_ns"] && truth["ended_ns"] <= exited,
            "{name}: the program ran from {} to {} on its own clock, and the life handed over is \
             {released} to {exited}",
            truth["started_ns"],
            truth["ended_ns"]
        );
        eprintln!(
            "{name}: life {:.1} ms (exit within {:.1} ms), the program's own span {:.1} ms, \
             asleep {:.1} ms, on a cpu {:.1} ms by its schedstat",
            (exited - released) / 1e6,
            exited_err / 1e6,
            truth["wall_ns"] / 1e6,
            truth["slept_ns"] / 1e6,
            truth["oncpu_ns"] / 1e6
        );

        match name {
            "decompose.yml" => what_dominates(read_back, &truth),
            "triage.yml" => the_cheap_pass_named_the_cpu(read_back),
            "profile.yml" => {
                let profile = request(read_back, "profile");
                assert!(
                    number(&profile["result"]["events_seen"]) > 0.0,
                    "a profile of a program that spent {:.0} ms on a cpu sampled nothing",
                    truth["oncpu_ns"] / 1e6
                );
            }
            _ => {}
        }
    }
    assert!(
        compared > 500,
        "only {compared} numbers were compared: the runs are not the ones this test meant"
    );
}

/// The decomposition says what dominates, and the program's own figures agree.
fn what_dominates(answer: &Value, truth: &BTreeMap<String, f64>) {
    let table = &answer["decompositions"][0]["tables"][0];
    let rows: BTreeMap<&str, &Value> = table["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| (row["state"].as_str().unwrap(), row))
        .collect();
    let amount = |state: &str| number(&rows[state]["amount"]);
    let resolution = |state: &str| number(&rows[state]["resolution"]);
    for row in rows.values() {
        assert!(
            row["evidence_means"].is_object(),
            "every row's evidence comes with what it means: {row}"
        );
    }

    // The sleep is the program's one block: the row holds it, less nothing
    // but the queue wait after the wakeup, and a block of its own clock's
    // bracket is inside the kernel's.
    let blocked = amount("blocked");
    assert!(
        blocked >= truth["slept_ns"] - 1e6,
        "the program slept {:.3} ms by its own clock, and `blocked` holds {:.3} ms: {table}",
        truth["slept_ns"] / 1e6,
        blocked / 1e6
    );
    // On a cpu, within the row's stated resolution of the kernel's account
    // the program read of itself before it exited.
    let oncpu = amount("on-cpu");
    assert!(
        (oncpu - truth["oncpu_ns"]).abs() <= resolution("on-cpu") + 10e6,
        "the program's schedstat says {:.1} ms on a cpu, and `on-cpu` holds {:.1} ms with a \
         resolution of {:.1} ms: {table}",
        truth["oncpu_ns"] / 1e6,
        oncpu / 1e6,
        resolution("on-cpu") / 1e6
    );
    // What dominates, as the program's own figures order it.
    let dominant = rows
        .iter()
        .max_by(|a, b| number(&a.1["amount"]).total_cmp(&number(&b.1["amount"])))
        .map(|(state, _)| *state)
        .unwrap();
    let expected = if truth["oncpu_ns"] > truth["slept_ns"] {
        "on-cpu"
    } else {
        "blocked"
    };
    assert_eq!(
        dominant,
        expected,
        "the program spent {:.0} ms on a cpu and slept {:.0} ms: {table}",
        truth["oncpu_ns"] / 1e6,
        truth["slept_ns"] / 1e6
    );
    // The residue is said, signed, with its resolution beside it.
    assert!(table["residue"].is_number() && table["residue_resolution"].is_number());
}

/// The cheap pass ran the whole life; the layer it attached is the cpu's, and
/// the lock layer, never named, says it was never attached.
fn the_cheap_pass_named_the_cpu(answer: &Value) {
    for label in ["blocked", "on-cpu"] {
        assert_eq!(
            request(answer, label)["result"]["window"]["attached"],
            true,
            "the cheap pass runs for the whole life"
        );
    }
    let profile = &request(answer, "profile")["result"];
    let futex = &request(answer, "futex")["result"];
    let decisions = request(answer, "blocked")["result"]["degradations"].to_string();
    assert!(
        profile["window"]["attached"] == true && number(&profile["events_seen"]) > 0.0,
        "a program two thirds on a cpu escalates to the profile: {decisions}"
    );
    assert!(
        decisions.contains("named `cpu`"),
        "the decision is said, with the reading that made it: {decisions}"
    );
    assert_eq!(futex["window"]["attached"], false, "{futex}");
    assert!(
        futex["stopped_because"]
            .as_str()
            .unwrap_or_default()
            .starts_with("never attached"),
        "{futex}"
    );
}

/// The benchmark, repeated: the spread a model computes from N `run_session`
/// answers is the spread the bench's gate computes from the same runs' files,
/// with the bench's own statistics (`examples/bench/harness.rs`).
#[test]
fn a_repeated_benchmark_has_the_spread_the_bench_would_compute_from_its_files() {
    const RUNS: usize = 5;

    if !has_python() {
        eprintln!("SKIP: the approved program is a python3 one, and this machine has none");
        return;
    }
    let scratch = scratch("repeated");
    let mut client = walk_through_server(&scratch);

    let (mut handed, mut written, mut own) = (Vec::new(), Vec::new(), Vec::new());
    let (mut cpu_handed, mut cpu_written) = (Vec::new(), Vec::new());
    for _ in 0..RUNS {
        let answer = client.call(
            "run_session",
            json!({"approved": "benchmark.yml", "duration": ALLOW_RUN, "budget": BUDGET}),
        );
        assert_eq!(answer["exit"], 0, "{answer}");
        let (released, exited, _) = life(&answer["read_back"], "on-cpu");
        handed.push(exited - released);
        cpu_handed.push(number(
            &request(&answer["read_back"], "on-cpu")["result"]["lifetime"]["cpu_ns"],
        ));
        let truth = truth(&answer);
        own.push(truth["wall_ns"]);

        let dir = scratch.join("runs").join(answer["run"].as_str().unwrap());
        let results = read_json(&dir.join("results.json"));
        let lifetime = &results["results"][0]["lifetime"];
        written.push(number(&lifetime["exited_ns"]) - number(&lifetime["released_ns"]));
        cpu_written.push(number(&lifetime["cpu_ns"]));
    }

    let from_answers = harness::Stats::of(&handed).unwrap();
    let from_files = harness::Stats::of(&written).unwrap();
    let program = harness::Stats::of(&own).unwrap();
    let table = harness::gate_table(&[
        harness::Baseline {
            workload: "handed".into(),
            stats: from_answers,
        },
        harness::Baseline {
            workload: "files".into(),
            stats: from_files,
        },
        harness::Baseline {
            workload: "own clock".into(),
            stats: program,
        },
    ]);
    eprintln!("{RUNS} runs of benchmark.yml through run_session:\n{table}");

    // The same spread, to the bit: nothing between the files and the agent
    // rounded, reordered or dropped a run.
    assert_eq!(from_answers, from_files, "{table}");
    assert_eq!(
        harness::Stats::of(&cpu_handed).unwrap(),
        harness::Stats::of(&cpu_written).unwrap()
    );
    assert_eq!(from_answers.resolution(), from_files.resolution());
    // Each life holds the program's own span, so the tool's spread is of the
    // program: its median is no shorter than the program's own.
    for (life, span) in handed.iter().zip(&own) {
        assert!(
            life >= span,
            "a life of {life} ns around a span of {span} ns"
        );
    }
    assert!(from_answers.median >= program.median, "{table}");
    // The gate's arithmetic, restated: the resolution is 3.545 CV / sqrt(n).
    let expected = harness::RESOLUTION_FACTOR * from_answers.cv() / (RUNS as f64).sqrt();
    assert!((from_answers.resolution() - expected).abs() < 1e-12);
}
