//! Each advice rule's ground truth (`catalog/advice.yaml`, `truth:`): the
//! fixture (`tests/fixtures/advice/fixture.c`) runs a workload the rule fires
//! on, the test changes the setting the rule names -- in the fixture, or in
//! the systemd scope it runs in, never on the machine -- and runs it again
//! with `--against` the first answer. The rule holds only if the reading it
//! said would move moved that way.
//!
//! A machine without what a rule needs (a user systemd with delegated
//! memory, swap, the eBPF sources) skips, and says why.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde_json::Value;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");
const SOURCE: &str = include_str!("fixtures/advice/fixture.c");

/// One rule's runs at a time. The OOM victims are traced machine-wide (a run
/// observes one workload, and a launched program's tracepoint sees that
/// process alone), so a kill in another test is in this one's window; and the
/// cpu-bound runs are timed.
fn alone() -> MutexGuard<'static, ()> {
    static ALONE: Mutex<()> = Mutex::new(());
    ALONE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The built fixture, and beside it the two names `oomtwo` runs it by.
fn fixture() -> Result<PathBuf, String> {
    static BUILT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("omn-advice-fixture");
            std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
            let source = dir.join("fixture.c");
            std::fs::write(&source, SOURCE).map_err(|error| error.to_string())?;
            let path = dir.join("fixture");
            let partial = dir.join(format!("fixture.{}", std::process::id()));
            let built = Command::new("cc")
                .args(["-O1", "-pthread", "-o"])
                .arg(&partial)
                .arg(&source)
                .output()
                .map_err(|error| format!("no C compiler (`cc`) to build the fixture: {error}"))?;
            if !built.status.success() {
                return Err(format!(
                    "the fixture did not build:\n{}",
                    String::from_utf8_lossy(&built.stderr)
                ));
            }
            std::fs::rename(&partial, &path).map_err(|error| error.to_string())?;
            for name in ["keeper", "grower"] {
                let link = dir.join(name);
                let _ = std::fs::remove_file(&link);
                std::os::unix::fs::symlink(&path, &link).map_err(|error| error.to_string())?;
            }
            Ok(path)
        })
        .clone()
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("omn-advice-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Whether a user scope with these properties can be made here.
fn scope_works(properties: &[&str]) -> Result<(), String> {
    let mut command = Command::new("systemd-run");
    command.args(["--user", "--scope", "-q"]);
    for property in properties {
        command.args(["-p", property]);
    }
    let output = command
        .arg("true")
        .output()
        .map_err(|error| format!("no systemd-run: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "a user scope with {properties:?} cannot be made here: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Runs `omn advise`, in a user scope when `scope` names properties, and
/// answers the answer it saved.
fn advise(
    objective: &str,
    scope: &[&str],
    against: Option<&Path>,
    program: &[&str],
    save: &Path,
) -> Value {
    let mut command = if scope.is_empty() {
        Command::new(BINARY)
    } else {
        let mut command = Command::new("systemd-run");
        // OOMPolicy=continue: a scope is stopped whole after a kill
        // otherwise, omn with it.
        command.args(["--user", "--scope", "-q", "-p", "OOMPolicy=continue"]);
        for property in scope {
            command.args(["-p", property]);
        }
        command.arg(BINARY);
        command
    };
    command.args(["advise", "--objective", objective, "--save"]);
    command.arg(save);
    if let Some(before) = against {
        command.arg("--against").arg(before);
    }
    command.arg("--").args(program);
    let output = command.output().expect("omn advise runs");
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "omn advise failed:\n{text}");
    let saved =
        std::fs::read_to_string(save).unwrap_or_else(|_| panic!("no answer saved:\n{text}"));
    serde_json::from_str(&saved).expect("the saved answer is JSON")
}

fn fired(answer: &Value, rule: &str) -> bool {
    answer["advice"]
        .as_array()
        .unwrap()
        .iter()
        .any(|advice| advice["rule"] == rule)
}

/// The measures this answer could not take, by label.
fn dropped(answer: &Value) -> Vec<String> {
    answer["dropped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|one| format!("{}: {}", one["label"], one["reason"]))
        .collect()
}

/// The rule's own prediction, as `--against` checked it.
fn prediction<'a>(answer: &'a Value, rule: &str) -> &'a Value {
    answer["verification"]["checks"]
        .as_array()
        .expect("a verification")
        .iter()
        .find(|check| check["rule"] == rule && check.get("side_effect").is_none())
        .unwrap_or_else(|| panic!("no check of {rule}: {answer:#}"))
}

fn changed(answer: &Value, setting: &str) -> Option<(String, String)> {
    answer["verification"]["settings_changed"]
        .as_array()?
        .iter()
        .find(|change| change["setting"] == setting)
        .map(|change| {
            (
                change["before"].as_str().unwrap().to_owned(),
                change["after"].as_str().unwrap().to_owned(),
            )
        })
}

macro_rules! skip_unless {
    ($check:expr) => {
        if let Err(reason) = $check {
            eprintln!("skipped: {reason}");
            return;
        }
    };
}

fn swap_total_kib() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("SwapTotal:"))
                .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
        })
        .unwrap_or(0)
}

/// `cgroup.memory.swap-denied`: 300 MB in a 128 MB cgroup is killed at once
/// with its swap denied; with swap allowed, on a machine that has some, it
/// is not killed at all.
#[test]
fn swap_allowed_to_the_cgroup_ends_the_kill() {
    let _alone = alone();
    let fixture = match fixture() {
        Ok(path) => path,
        Err(reason) => return eprintln!("skipped: {reason}"),
    };
    skip_unless!(scope_works(&["MemoryMax=128M", "MemorySwapMax=0"]));
    if swap_total_kib() < 512 * 1024 {
        return eprintln!("skipped: the machine has less than 512 MiB of swap to allow");
    }
    let dir = scratch("swap");
    let program = [fixture.to_str().unwrap(), "grow", "300"];
    let before = advise(
        "latency",
        &["MemoryMax=128M", "MemorySwapMax=0"],
        None,
        &program,
        &dir.join("before.json"),
    );
    assert!(
        fired(&before, "cgroup.memory.swap-denied"),
        "the rule did not hold on a kill with swap denied: {before:#}"
    );
    let after = advise(
        "latency",
        &["MemoryMax=128M"],
        Some(&dir.join("before.json")),
        &program,
        &dir.join("after.json"),
    );
    assert_eq!(
        changed(&after, "cgroup.memory.swap.max"),
        Some(("0".to_owned(), "max".to_owned())),
        "{after:#}"
    );
    let check = prediction(&after, "cgroup.memory.swap-denied");
    assert_eq!(check["verdict"], "as_predicted", "{check:#}");
    assert!(
        check["after"]["number"].as_f64().unwrap() < 1.0,
        "{check:#}"
    );
}

/// `oom.victim-by-size`: of a 120 MB keeper and a grower, the kernel kills
/// the keeper by size; with the grower's oom_score_adj at 1000, the grower.
#[test]
fn an_adjustment_moves_the_oom_killer_to_the_other_process() {
    let _alone = alone();
    let fixture = match fixture() {
        Ok(path) => path,
        Err(reason) => return eprintln!("skipped: {reason}"),
    };
    let scope = ["MemoryMax=256M", "MemorySwapMax=0"];
    skip_unless!(scope_works(&scope));
    let dir = scratch("victim");
    let names = fixture.parent().unwrap().to_str().unwrap().to_owned();
    let program = |adj: &'static str| {
        [
            fixture.to_str().unwrap().to_owned(),
            "oomtwo".to_owned(),
            names.clone(),
            adj.to_owned(),
        ]
    };
    let first = program("0");
    let first: Vec<&str> = first.iter().map(String::as_str).collect();
    let before = advise("latency", &scope, None, &first, &dir.join("before.json"));
    let lost = dropped(&before);
    if lost.iter().any(|line| line.contains("mem.oom trace")) {
        return eprintln!("skipped: the OOM victims are not traced here: {lost:?}");
    }
    assert!(
        fired(&before, "oom.victim-by-size"),
        "the rule did not hold on a kill by size: {before:#}"
    );
    let second = program("1000");
    let second: Vec<&str> = second.iter().map(String::as_str).collect();
    let after = advise(
        "latency",
        &scope,
        Some(&dir.join("before.json")),
        &second,
        &dir.join("after.json"),
    );
    let check = prediction(&after, "oom.victim-by-size");
    assert_eq!(check["before"]["text"], "keeper", "{check:#}");
    assert_eq!(check["after"]["text"], "grower", "{check:#}");
    assert_eq!(check["verdict"], "as_predicted", "{check:#}");
}

/// `sched.shared-cpu`: four cpu-bound threads on one cpu wait for it; on
/// four, they do not, and the same work takes about a quarter of the time.
#[test]
fn a_wider_affinity_ends_the_wait_for_one_cpu() {
    let _alone = alone();
    let fixture = match fixture() {
        Ok(path) => path,
        Err(reason) => return eprintln!("skipped: {reason}"),
    };
    let online = std::thread::available_parallelism().map_or(1, |n| n.get());
    if online < 4 {
        return eprintln!("skipped: {online} cpus, and the rule needs four to widen onto");
    }
    let dir = scratch("affinity");
    let work = [fixture.to_str().unwrap(), "spin", "4", "200000000"];
    let pinned: Vec<&str> = ["taskset", "-c", "0"].into_iter().chain(work).collect();
    let before = advise("latency", &[], None, &pinned, &dir.join("before.json"));
    let lost = dropped(&before);
    if lost.iter().any(|line| line.contains("sched.runqlat")) {
        return eprintln!("skipped: run-queue latency is not measured here: {lost:?}");
    }
    assert!(
        fired(&before, "sched.shared-cpu"),
        "the rule did not hold on four threads pinned to one cpu: {before:#}"
    );
    let wide: Vec<&str> = ["taskset", "-c", "0-3"].into_iter().chain(work).collect();
    let after = advise(
        "latency",
        &[],
        Some(&dir.join("before.json")),
        &wide,
        &dir.join("after.json"),
    );
    assert_eq!(
        changed(&after, "process.cpus_allowed"),
        Some(("0".to_owned(), "0-3".to_owned())),
        "{after:#}"
    );
    let check = prediction(&after, "sched.shared-cpu");
    assert_eq!(check["verdict"], "as_predicted", "{check:#}");
    let (was, now) = (
        check["before"]["number"].as_f64().unwrap(),
        check["after"]["number"].as_f64().unwrap(),
    );
    assert!(now * 10.0 < was, "run-queue latency {was} -> {now}");
    assert!(!fired(&after, "sched.shared-cpu"), "{after:#}");
}

/// `io.readahead-refused`: half a cold file read 4 KiB at a time under
/// POSIX_FADV_RANDOM reads nothing ahead; without the hint, readahead fills
/// in front of the reader.
#[test]
fn readahead_refused_by_a_hint_returns_without_it() {
    let _alone = alone();
    let fixture = match fixture() {
        Ok(path) => path,
        Err(reason) => return eprintln!("skipped: {reason}"),
    };
    let dir = scratch("readahead");
    let file = dir.join("data.bin");
    // Made once, before either run, so neither pays for writing it.
    let made = Command::new(&fixture)
        .args(["readseq", file.to_str().unwrap(), "64", "normal"])
        .output()
        .unwrap();
    assert!(made.status.success());
    let program = |hint: &'static str| {
        vec![
            fixture.to_str().unwrap().to_owned(),
            "readseq".to_owned(),
            file.to_str().unwrap().to_owned(),
            "64".to_owned(),
            hint.to_owned(),
        ]
    };
    let random = program("random");
    let random: Vec<&str> = random.iter().map(String::as_str).collect();
    let before = advise("throughput", &[], None, &random, &dir.join("before.json"));
    let lost = dropped(&before);
    if lost
        .iter()
        .any(|line| line.contains("io.readahead") || line.contains("io.pagecache"))
    {
        return eprintln!("skipped: the page cache is not traced here: {lost:?}");
    }
    assert!(
        fired(&before, "io.readahead-refused"),
        "the rule did not hold on a hinted sequential reader: {before:#}"
    );
    let normal = program("normal");
    let normal: Vec<&str> = normal.iter().map(String::as_str).collect();
    let after = advise(
        "throughput",
        &[],
        Some(&dir.join("before.json")),
        &normal,
        &dir.join("after.json"),
    );
    let check = prediction(&after, "io.readahead-refused");
    assert_eq!(check["verdict"], "as_predicted", "{check:#}");
    assert!(
        check["after"]["number"].as_f64().unwrap() > 100.0,
        "{check:#}"
    );
    assert!(!fired(&after, "io.readahead-refused"), "{after:#}");
}

/// The victims `mem.oom` names for a launched program are its own tree's: a
/// child the kernel kills is named, and a kill elsewhere on the machine in
/// the same window is not.
#[test]
fn the_victims_are_the_launched_tree_s_and_no_one_else_s() {
    let _alone = alone();
    let fixture = match fixture() {
        Ok(path) => path,
        Err(reason) => return eprintln!("skipped: {reason}"),
    };
    let scope = ["OOMPolicy=continue", "MemoryMax=128M", "MemorySwapMax=0"];
    skip_unless!(scope_works(&scope));
    let victims =
        |system: bool, program: &[&str], beside: Option<&[&str]>| -> Option<Vec<String>> {
            let mut command = Command::new("systemd-run");
            command.args(["--user", "--scope", "-q"]);
            for property in scope {
                command.args(["-p", property]);
            }
            command
                .arg(BINARY)
                .args(["mem.oom", "-m", "trace", "-e", "raw", "--format", "json"])
                .args(if system { &["--system"][..] } else { &[][..] })
                .args(["--program-stdout", "/dev/null", "--"])
                .args(program);
            // A kill in another scope while this one runs.
            let elsewhere = beside.map(|other| {
                let mut command = Command::new("systemd-run");
                command.args(["--user", "--scope", "-q"]);
                for property in scope {
                    command.args(["-p", property]);
                }
                command
                    .args(other)
                    .spawn()
                    .expect("the other workload starts")
            });
            let output = command.output().expect("omn runs");
            if let Some(mut child) = elsewhere {
                let _ = child.wait();
            }
            let text = String::from_utf8_lossy(&output.stdout).into_owned();
            let said = text.clone() + &String::from_utf8_lossy(&output.stderr);
            if said.contains("has no viable backend") || said.contains("CAP_BPF") {
                eprintln!("skipped: the victims are not traced here: {said}");
                return None;
            }
            assert!(output.status.success(), "{said}");
            let document: Value = serde_json::from_str(&text).expect("json");
            let mut comms = Vec::new();
            for output in document["results"][0]["outputs"].as_array().unwrap() {
                for event in output["events"].as_array().into_iter().flatten() {
                    for attr in event["attrs"].as_array().unwrap() {
                        if attr["name"] == "comm" {
                            comms.push(attr["value"].as_str().unwrap().to_owned());
                        }
                    }
                }
            }
            Some(comms)
        };
    let names = fixture.parent().unwrap().to_str().unwrap().to_owned();
    let tree = [fixture.to_str().unwrap(), "oomtwo", names.as_str(), "0"];
    let Some(own) = victims(false, &tree, None) else {
        return;
    };
    assert_eq!(own.first().map(String::as_str), Some("keeper"), "{own:?}");

    // Started late enough to be killed inside the window, not before it.
    let late = format!("sleep 0.6; exec {} grow 300", fixture.display());
    let grow = ["sh", "-c", late.as_str()];
    let quiet = ["sleep", "1.5"];
    let Some(machine) = victims(true, &quiet, Some(&grow)) else {
        return;
    };
    assert!(
        machine.iter().any(|comm| comm == "fixture"),
        "the machine's trace did not see the kill beside it, so the next check would prove \
         nothing: {machine:?}"
    );
    let Some(none) = victims(false, &quiet, Some(&grow)) else {
        return;
    };
    assert!(
        none.is_empty(),
        "a kill outside the workload was named: {none:?}"
    );
}
