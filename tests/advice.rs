//! `omn advise` as a command: what it refuses, what it never does, and that
//! every rule names a ground truth that exists.

use std::process::Command;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");
const RULES: &str = include_str!("../catalog/advice.yaml");
const TRUTH: &str = include_str!("advice_truth.rs");

fn omn(args: &[&str]) -> std::process::Output {
    Command::new(BINARY).args(args).output().expect("omn runs")
}

fn spoken(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

/// Every `truth:` of the table names a test of `tests/advice_truth.rs`: a
/// rule nobody has seen move its reading is not one the table carries.
#[test]
fn every_rule_names_a_ground_truth_test_that_exists() {
    let truths: Vec<&str> = RULES
        .lines()
        .filter_map(|line| line.trim().strip_prefix("truth:"))
        .map(str::trim)
        .collect();
    let rules = RULES
        .lines()
        .filter(|line| line.starts_with("- id:"))
        .count();
    assert_eq!(truths.len(), rules, "one truth per rule");
    for truth in truths {
        let name = truth
            .strip_prefix("advice_truth::")
            .unwrap_or_else(|| panic!("`{truth}` is not in tests/advice_truth.rs"));
        assert!(
            TRUTH.contains(&format!("fn {name}()")),
            "`{truth}` names no test in tests/advice_truth.rs"
        );
    }
}

#[test]
fn advice_without_an_objective_is_refused_with_the_list() {
    let output = omn(&["advise", "--", "true"]);
    assert_eq!(output.status.code(), Some(2), "{}", spoken(&output));
    let text = spoken(&output);
    assert!(text.contains("needs an objective"), "{text}");
    for objective in ["latency", "tail", "throughput", "energy"] {
        assert!(text.contains(objective), "{text}");
    }
}

#[test]
fn a_running_process_needs_a_duration_and_a_saved_answer_must_be_one() {
    let output = omn(&["advise", "--objective", "latency", "--pid", "1"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        spoken(&output).contains("--duration"),
        "{}",
        spoken(&output)
    );

    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let bogus = dir.join(format!("omn-advice-bogus-{}.json", std::process::id()));
    std::fs::write(&bogus, "{}").unwrap();
    let output = omn(&[
        "advise",
        "--objective",
        "latency",
        "--against",
        bogus.to_str().unwrap(),
        "--",
        "true",
    ]);
    assert_eq!(output.status.code(), Some(2), "{}", spoken(&output));
    assert!(
        spoken(&output).contains("is not an answer"),
        "{}",
        spoken(&output)
    );
}

/// The session a question is asked through lists the settings beside the
/// measures, and every measure of the objective's rules once.
#[test]
fn the_session_reads_the_settings_beside_every_measure() {
    let output = omn(&[
        "advise",
        "--objective",
        "latency",
        "--dump-session",
        "--",
        "true",
    ]);
    assert!(output.status.success(), "{}", spoken(&output));
    let text = String::from_utf8_lossy(&output.stdout);
    for source in [
        "conf.machine",
        "conf.process",
        "conf.changed",
        "mem.oom",
        "sched.runqlat",
    ] {
        assert!(
            text.contains(&format!("source: {source}")),
            "{source}:\n{text}"
        );
    }
    let labels: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("label:"))
        .collect();
    let mut unique = labels.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(labels.len(), unique.len(), "a measure asked twice:\n{text}");
}

/// Boundary 5, as a test: a whole `omn advise` run, traced, opens no setting
/// for writing and calls nothing that changes another process.
#[test]
fn advice_changes_nothing() {
    let probe = Command::new("strace").arg("-V").output();
    if !probe.is_ok_and(|output| output.status.success()) {
        return eprintln!("skipped: no strace to watch the run with");
    }
    let log = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("omn-advice-strace-{}.log", std::process::id()));
    // Under ptrace the binary's file capabilities are not granted, so the
    // eBPF measures are left out: the settings and the rest of the run are
    // what is watched, and they are where a write would be.
    let output = Command::new("strace")
        .args(["-f", "-qq", "-o"])
        .arg(&log)
        .args([
            "-e",
            "trace=openat,open,creat,truncate,sched_setaffinity,sched_setattr,sched_setscheduler,\
             setpriority,ioprio_set,prlimit64,setrlimit,set_mempolicy,mbind,migrate_pages,\
             move_pages,process_madvise",
        ])
        .arg(BINARY)
        .args(["advise", "--objective", "latency", "--", "sleep", "0.3"])
        .output()
        .expect("strace runs");
    assert!(output.status.success(), "{}", spoken(&output));
    let trace = std::fs::read_to_string(&log).unwrap();
    // The watch saw the settings being read, so an empty list below means
    // nothing was written rather than nothing was watched.
    assert!(
        trace.contains("\"/proc/sys/vm/swappiness\"") && trace.contains("oom_score_adj"),
        "the trace does not show the settings being read:\n{}",
        &trace[..trace.len().min(4000)]
    );
    let mut written = Vec::new();
    for line in trace.lines() {
        let call = line.split_once(' ').map_or(line, |(_, rest)| rest);
        let opens =
            call.starts_with("openat(") || call.starts_with("open(") || call.starts_with("creat(");
        let setting_path = [
            "\"/proc/sys",
            "\"/sys/",
            "oom_score_adj\"",
            "\"/proc/self/attr",
            "/memory.",
            "/cpu.",
            "/io.",
            "/pids.",
        ]
        .iter()
        .any(|path| call.contains(path));
        let for_writing = call.contains("O_WRONLY")
            || call.contains("O_RDWR")
            || call.contains("O_CREAT")
            || call.starts_with("creat(");
        if opens && setting_path && for_writing {
            written.push(line.to_owned());
        }
        // Calls on the run's own process (0) are its own business; any
        // other first argument is another process.
        for changer in [
            "sched_setaffinity(",
            "sched_setattr(",
            "sched_setscheduler(",
            "ioprio_set(",
            "set_mempolicy(",
            "mbind(",
            "migrate_pages(",
            "move_pages(",
            "process_madvise(",
        ] {
            if call.starts_with(changer) && !call.starts_with(&format!("{changer}0,")) {
                written.push(line.to_owned());
            }
        }
        if call.starts_with("setpriority(") && !call.contains(", 0, ") {
            written.push(line.to_owned());
        }
        // prlimit64(pid, resource, new, old): a new limit on another process.
        if let Some(arguments) = call.strip_prefix("prlimit64(") {
            let mut parts = arguments.splitn(3, ", ");
            let pid = parts.next().unwrap_or("");
            let new = parts.nth(1).unwrap_or("");
            if pid != "0" && !new.starts_with("NULL") {
                written.push(line.to_owned());
            }
        }
    }
    assert!(
        written.is_empty(),
        "omn advise changed something:\n{}",
        written.join("\n")
    );
}
