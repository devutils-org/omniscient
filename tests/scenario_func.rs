//! `runtime.func`: each call of a named function, timed between a uprobe on
//! its entry and one on its return, held to what the program measured of the
//! same call on `CLOCK_MONOTONIC`.
//!
//! The fixture (built here) calls its functions in known ways and prints, for
//! each call, the time it measured around it: `fixture: call=hold ns=...`.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::OnceLock;

use serde_json::Value;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

const SOURCE: &str = r#"
#define _GNU_SOURCE
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/syscall.h>

static volatile uint64_t sink;

static uint64_t now(void)
{
	struct timespec at;
	clock_gettime(CLOCK_MONOTONIC, &at);
	return (uint64_t)at.tv_sec * 1000000000ull + (uint64_t)at.tv_nsec;
}

static void spin_ms(uint64_t ms)
{
	uint64_t end = now() + ms * 1000000ull;
	while (now() < end)
		sink++;
}

/* Sleeps MS: off the cpu for all of it. */
__attribute__((noinline)) int hold(int ms)
{
	struct timespec pause = { .tv_sec = ms / 1000, .tv_nsec = (ms % 1000) * 1000000L };
	nanosleep(&pause, NULL);
	return ms;
}

/* DEPTH calls of itself, the innermost spinning MS. */
__attribute__((noinline)) int recurse(int depth, int ms)
{
	if (depth > 1)
		return recurse(depth - 1, ms) + 1;
	spin_ms(ms);
	return 1;
}

__attribute__((noinline)) int tiny(int x)
{
	return x + 1;
}

static int calls_per_thread = 1, ms_per_call = 1;

static void *worker(void *arg)
{
	(void)arg;
	for (int i = 0; i < calls_per_thread; i++) {
		uint64_t start = now();
		hold(ms_per_call);
		printf("fixture: call=hold tid=%ld ns=%llu\n", (long)syscall(SYS_gettid),
		       (unsigned long long)(now() - start));
	}
	return NULL;
}

int main(int argc, char **argv)
{
	setvbuf(stdout, NULL, _IOLBF, 0);
	if (argc >= 4 && strcmp(argv[1], "hold") == 0) {
		int ms = atoi(argv[2]), n = atoi(argv[3]);
		for (int i = 0; i < n; i++) {
			uint64_t start = now();
			hold(ms);
			printf("fixture: call=hold ns=%llu\n", (unsigned long long)(now() - start));
		}
		return 0;
	}
	if (argc >= 4 && strcmp(argv[1], "recurse") == 0) {
		uint64_t start = now();
		recurse(atoi(argv[2]), atoi(argv[3]));
		printf("fixture: call=recurse ns=%llu\n", (unsigned long long)(now() - start));
		return 0;
	}
	if (argc >= 5 && strcmp(argv[1], "threads") == 0) {
		int threads = atoi(argv[2]);
		calls_per_thread = atoi(argv[3]);
		ms_per_call = atoi(argv[4]);
		pthread_t ids[16];
		for (int i = 0; i < threads && i < 16; i++)
			pthread_create(&ids[i], NULL, worker, NULL);
		for (int i = 0; i < threads && i < 16; i++)
			pthread_join(ids[i], NULL);
		return 0;
	}
	if (argc >= 3 && strcmp(argv[1], "tiny") == 0) {
		int n = atoi(argv[2]);
		for (int i = 0; i < n; i++)
			sink += tiny(i);
		return 0;
	}
	fprintf(stderr, "usage: fixture hold MS N | recurse DEPTH MS | threads T N MS | tiny N\n");
	return 2;
}
"#;

fn fixture() -> Result<PathBuf, String> {
    static BUILT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("omn-func-fixture");
            std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
            let source = dir.join("fixture.c");
            std::fs::write(&source, SOURCE).map_err(|error| error.to_string())?;
            let path = dir.join("fixture");
            let partial = dir.join(format!("fixture.{}", std::process::id()));
            let built = Command::new("cc")
                .args(["-O1", "-g", "-fno-inline", "-pthread", "-o"])
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
            Ok(path)
        })
        .clone()
}

fn omn(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

fn spoken(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

/// Times the functions `filter` names in the fixture run with `program`:
/// the document, and what the fixture printed; `None`, having said why,
/// where this machine places no uprobe.
fn timed(filter: &str, extra: &[&str], program: &[&str]) -> Option<(Value, Vec<String>)> {
    timed_as(&["raw", "summary"], filter, extra, program)
}

/// [`timed`], asking for `emits` instead of every call and a summary.
fn timed_as(
    emits: &[&str],
    filter: &str,
    extra: &[&str],
    program: &[&str],
) -> Option<(Value, Vec<String>)> {
    let fixture = match fixture() {
        Ok(path) => path,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return None;
        }
    };
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "omn-func-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let said = dir.join("program.stdout");
    let mut args = vec!["runtime.func", "-m", "trace"];
    for emit in emits {
        args.extend(["-e", emit]);
    }
    args.extend([
        "--filter",
        filter,
        "--format",
        "json",
        "--program-stdout",
        said.to_str().unwrap(),
    ]);
    args.extend_from_slice(extra);
    args.push("--");
    args.push(fixture.to_str().unwrap());
    args.extend_from_slice(program);
    let output = omn(&args);
    let text = spoken(&output);
    if !output.status.success()
        && (text.contains("CAP_BPF")
            || text.contains("CAP_PERFMON")
            || text.contains("has no viable backend on this machine"))
    {
        eprintln!("skipped: this machine places no uprobe here: {text}");
        return None;
    }
    assert!(output.status.success(), "{text}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("json");
    let printed = std::fs::read_to_string(&said)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.strip_prefix("fixture: ").map(str::to_owned))
        .collect();
    Some((document, printed))
}

fn rand_suffix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64
}

fn calls(document: &Value) -> Vec<Value> {
    let mut events: Vec<Value> = document["results"][0]["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|output| output["emit"] == "raw")
        .map(|raw| raw["events"].as_array().cloned().unwrap_or_default())
        .unwrap_or_default();
    events.sort_by_key(|event| event["ts_ns"].as_u64().unwrap_or(0));
    events
}

fn attr(event: &Value, name: &str) -> Value {
    event["attrs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attr| attr["name"] == name)
        .map(|attr| attr["value"].clone())
        .unwrap_or(Value::Null)
}

fn number(value: &Value) -> f64 {
    value
        .as_f64()
        .or_else(|| value["uint"].as_f64())
        .or_else(|| value["int"].as_f64())
        .unwrap_or_else(|| panic!("not a number: {value}"))
}

fn notes(document: &Value) -> String {
    document["results"][0]["degradations"].to_string()
}

fn measured(line: &str) -> f64 {
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("ns="))
        .and_then(|ns| ns.parse().ok())
        .unwrap_or_else(|| panic!("no ns= in {line}"))
}

/// Each call's time is the one the program measured around it, to within
/// what the two traps and the clock reads cost: a sleeping function's call
/// is timed whole, which a cpu profile would charge nothing.
#[test]
fn each_call_lasts_what_the_program_measured_around_it() {
    let Some((document, printed)) = timed("function == 'hold'", &[], &["hold", "20", "5"]) else {
        return;
    };
    let calls = calls(&document);
    assert_eq!(calls.len(), 5, "{document:#}");
    assert_eq!(printed.len(), 5, "{printed:?}");
    for (call, line) in calls.iter().zip(&printed) {
        let timed = number(&attr(call, "lat_ns"));
        let truth = measured(line);
        assert!(timed >= 20e6, "a 20 ms sleep timed {timed} ns");
        assert!(
            (truth - timed).abs() < 1e6,
            "timed {timed} ns, the program measured {truth} ns around the same call"
        );
        assert_eq!(attr(call, "function"), "hold");
        assert_eq!(
            attr(call, "depth")
                .as_u64()
                .or(attr(call, "depth")["uint"].as_u64()),
            Some(1)
        );
        assert_eq!(number(&attr(call, "ret")), 20.0, "hold returns its ms");
    }
}

/// A recursive function's calls are each timed, at their depth, nested in
/// one another: the outermost holds the inner ones, and the run says not to
/// add them.
#[test]
fn a_recursive_function_s_calls_are_timed_at_their_depth() {
    let Some((document, printed)) = timed("function == 'recurse'", &[], &["recurse", "3", "30"])
    else {
        return;
    };
    let calls = calls(&document);
    assert_eq!(calls.len(), 3, "{document:#}");
    let mut by_depth: Vec<(u64, f64)> = calls
        .iter()
        .map(|call| {
            (
                number(&attr(call, "depth")) as u64,
                number(&attr(call, "lat_ns")),
            )
        })
        .collect();
    by_depth.sort_by_key(|(depth, _)| *depth);
    assert_eq!(
        by_depth.iter().map(|(depth, _)| *depth).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(
        by_depth[0].1 >= by_depth[1].1 && by_depth[1].1 >= by_depth[2].1,
        "{by_depth:?}"
    );
    assert!(by_depth[2].1 >= 30e6, "{by_depth:?}");
    assert!(
        (measured(&printed[0]) - by_depth[0].1).abs() < 1e6,
        "{printed:?} {by_depth:?}"
    );
    assert!(
        notes(&document).contains("depth == 1"),
        "{}",
        notes(&document)
    );
}

/// Calls from several threads are each paired on their own thread.
#[test]
fn calls_from_several_threads_are_each_their_own_thread_s() {
    let Some((document, printed)) = timed("function == 'hold'", &[], &["threads", "3", "4", "10"])
    else {
        return;
    };
    let calls = calls(&document);
    assert_eq!(calls.len(), 12, "{document:#}");
    let mut per_tid: std::collections::BTreeMap<u64, usize> = Default::default();
    for call in &calls {
        *per_tid
            .entry(number(&attr(call, "tid")) as u64)
            .or_default() += 1;
        assert!(number(&attr(call, "lat_ns")) >= 10e6);
    }
    assert_eq!(per_tid.len(), 3, "{per_tid:?}");
    assert!(per_tid.values().all(|count| *count == 4), "{per_tid:?}");
    let program_tids: std::collections::BTreeSet<u64> = printed
        .iter()
        .filter_map(|line| {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix("tid="))
                .and_then(|tid| tid.parse().ok())
        })
        .collect();
    assert_eq!(
        per_tid
            .keys()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        program_tids
    );
}

/// A call still open when the run ends is counted and said, not timed.
#[test]
fn a_call_still_open_at_the_end_is_said_not_timed() {
    let Some((document, _)) = timed(
        "function == 'hold'",
        &["-d", "300ms"],
        &["hold", "3000", "1"],
    ) else {
        return;
    };
    assert!(calls(&document).is_empty(), "{document:#}");
    assert!(
        notes(&document).contains("never returned"),
        "{}",
        notes(&document)
    );
}

/// Calls shorter than the two traps that time them are said to be mostly
/// the measurement.
#[test]
fn calls_shorter_than_the_traps_are_said_to_be_mostly_the_measurement() {
    let Some((document, _)) = timed("function == 'tiny'", &[], &["tiny", "2000"]) else {
        return;
    };
    let calls = calls(&document);
    assert_eq!(calls.len(), 2000, "{}", notes(&document));
    assert!(
        notes(&document).contains("mostly the measurement"),
        "{}",
        notes(&document)
    );
}

/// A request that names no function means nothing; one whose name matches
/// none says what the files hold that resembles it.
#[test]
fn a_function_that_is_not_named_or_not_there_is_refused_by_name() {
    let Ok(fixture) = fixture() else { return };
    let nothing = omn(&[
        "runtime.func",
        "-m",
        "trace",
        "-e",
        "summary",
        "--",
        fixture.to_str().unwrap(),
        "tiny",
        "1",
    ]);
    assert!(!nothing.status.success());
    assert!(
        spoken(&nothing).contains("names none"),
        "{}",
        spoken(&nothing)
    );
    let missing = omn(&[
        "runtime.func",
        "-m",
        "trace",
        "-e",
        "summary",
        "--filter",
        "function == 'recur'",
        "--",
        fixture.to_str().unwrap(),
        "tiny",
        "1",
    ]);
    let text = spoken(&missing);
    if text.contains("CAP_BPF") || text.contains("has no viable backend") {
        eprintln!("skipped: {text}");
        return;
    }
    assert!(!missing.status.success(), "{text}");
    assert!(text.contains("found no function named `recur`"), "{text}");
    assert!(text.contains("recurse"), "the near name is offered: {text}");
}

/// A process in another mount namespace -- a container -- maps an
/// executable this tool's namespace does not have at that path. Its functions
/// are found, timed and named through `/proc/<pid>/root`, the way the target
/// sees its own files (the campaign's podman `kyavserver`, refused with
/// "cannot read /k/bin/kyavserver").
#[test]
fn a_process_whose_executable_only_its_own_mount_namespace_has_is_timed_and_named() {
    let Ok(fixture) = fixture() else { return };
    let hidden = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "omn-func-ns-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&hidden).unwrap();
    let inside = hidden.join("fixture");
    // A tmpfs over the directory, in a namespace of the child's own: the
    // copy is there for it and nowhere for this tool.
    let start = |program: &str| {
        Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount", "sh", "-c"])
            .arg(format!(
                "mount -t tmpfs none {dir} && cp {fixture} {inside} && exec {inside} {program}",
                dir = hidden.display(),
                fixture = fixture.display(),
                inside = inside.display()
            ))
            .stdout(std::process::Stdio::null())
            .spawn()
    };
    let wait_mapped = |pid: u32| {
        for _ in 0..400 {
            let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap_or_default();
            if maps.contains(inside.to_str().unwrap()) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    };
    let Ok(mut holding) = start("hold 20 400") else {
        eprintln!("skipped: no unshare");
        return;
    };
    if !wait_mapped(holding.id()) {
        let _ = holding.kill();
        let status = holding.wait().unwrap();
        eprintln!("skipped: no user and mount namespace here ({status})");
        return;
    }
    assert!(
        !inside.exists(),
        "the executable is outside this tool's namespace"
    );
    let pid = holding.id().to_string();
    let output = omn(&[
        "runtime.func",
        "-m",
        "trace",
        "-e",
        "summary",
        "--filter",
        "function == 'hold'",
        "--format",
        "json",
        "-d",
        "1s",
        "--pid",
        &pid,
    ]);
    let _ = holding.kill();
    let _ = holding.wait();
    let text = spoken(&output);
    if !output.status.success()
        && (text.contains("CAP_BPF")
            || text.contains("CAP_PERFMON")
            || text.contains("has no viable backend on this machine"))
    {
        eprintln!("skipped: this machine places no uprobe here: {text}");
        return;
    }
    assert!(output.status.success(), "{text}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("json");
    let summary = document["results"][0]["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|output| output["emit"] == "summary")
        .unwrap()
        .to_string();
    assert!(summary.contains("hold"), "{summary}");
    assert!(!summary.contains("\"n\":0,"), "calls were timed: {summary}");

    let Ok(mut spinning) = start("recurse 1 5000") else {
        return;
    };
    assert!(wait_mapped(spinning.id()));
    let pid = spinning.id().to_string();
    let output = omn(&["cpu.profile", "-e", "stack", "-d", "1s", "--pid", &pid]);
    let _ = spinning.kill();
    let _ = spinning.wait();
    std::fs::remove_dir_all(&hidden).ok();
    let text = spoken(&output);
    assert!(output.status.success(), "{text}");
    assert!(
        text.contains("recurse"),
        "the hidden executable's frames are named, and unwound past the leaf: {text}"
    );
    assert!(
        !text.contains("cannot read"),
        "every file the target maps was read through its root: {text}"
    );
}

const PLUGIN: &str = r#"
#include <time.h>
__attribute__((noinline)) int omn_plugin_step(int ms)
{
	struct timespec pause = { .tv_sec = 0, .tv_nsec = ms * 1000000L };
	nanosleep(&pause, NULL);
	return ms;
}
"#;

const LOADER: &str = r#"
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
int main(int argc, char **argv)
{
	/* Long enough that the run is past its start before the plugin exists. */
	struct timespec pause = { .tv_sec = 0, .tv_nsec = 300 * 1000000L };
	nanosleep(&pause, NULL);
	void *plugin = dlopen(argv[1], RTLD_NOW);
	if (!plugin) {
		fprintf(stderr, "dlopen: %s\n", dlerror());
		return 2;
	}
	int (*step)(int) = (int (*)(int))dlsym(plugin, "omn_plugin_step");
	int n = atoi(argv[2]);
	for (int i = 0; i < n; i++)
		step(10);
	return 0;
}
"#;

/// The plugin and the program that opens it, built once.
fn loader() -> Result<(PathBuf, PathBuf), String> {
    static BUILT: OnceLock<Result<(PathBuf, PathBuf), String>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("omn-func-dlopen");
            std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
            let build = |source: &str, name: &str, flags: &[&str]| -> Result<PathBuf, String> {
                let file = dir.join(format!("{name}.c"));
                std::fs::write(&file, source).map_err(|error| error.to_string())?;
                let out = dir.join(name);
                let partial = dir.join(format!("{name}.{}", std::process::id()));
                let built = Command::new("cc")
                    .args(["-O1", "-g"])
                    .args(flags)
                    .arg("-o")
                    .arg(&partial)
                    .arg(&file)
                    .output()
                    .map_err(|error| format!("no C compiler: {error}"))?;
                if !built.status.success() {
                    return Err(String::from_utf8_lossy(&built.stderr).into_owned());
                }
                std::fs::rename(&partial, &out).map_err(|error| error.to_string())?;
                Ok(out)
            };
            Ok((
                build(PLUGIN, "libomnplugin.so", &["-shared", "-fPIC"])?,
                build(LOADER, "loader", &["-ldl"])?,
            ))
        })
        .clone()
}

/// A launched program that opens the library its function is in only once
/// it runs -- the ZML plugin, `dlopen`ed by the runtime -- has that function
/// timed from the read of its maps that finds the library, and the run says
/// how late that was; a name nothing it ever maps has fails the run, saying
/// so, rather than timing nothing in silence.
#[test]
fn a_function_in_a_library_opened_later_is_timed_from_when_it_is_mapped() {
    let (plugin, program) = match loader() {
        Ok(built) => built,
        Err(reason) => {
            eprintln!("skipped: {reason}");
            return;
        }
    };
    let run = |function: &str| {
        omn(&[
            "runtime.func",
            "-m",
            "trace",
            "-e",
            "summary",
            "--filter",
            &format!("function == '{function}'"),
            "--format",
            "json",
            "--",
            program.to_str().unwrap(),
            plugin.to_str().unwrap(),
            "40",
        ])
    };
    let output = run("omn_plugin_step");
    let text = spoken(&output);
    if !output.status.success()
        && (text.contains("CAP_BPF")
            || text.contains("CAP_PERFMON")
            || text.contains("has no viable backend on this machine"))
    {
        eprintln!("skipped: this machine places no uprobe here: {text}");
        return;
    }
    assert!(output.status.success(), "{text}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("json");
    let said = notes(&document);
    assert!(
        said.contains("libomnplugin.so was mapped after the probes were first placed"),
        "{said}"
    );
    let summary = document["results"][0]["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|output| output["emit"] == "summary")
        .unwrap()["series"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let count: u64 = summary
        .iter()
        .map(|row| row["count"].as_u64().unwrap_or(0))
        .sum();
    // Forty calls of 10 ms, and the ones before the probes were placed are
    // not timed: the run says how long after the mapping that could be at
    // most -- the 100 ms rescan, the tick that runs it, the search and the
    // attach -- so at most that many tens of milliseconds' calls are missing.
    let at_most_ms = said
        .split("at most ")
        .nth(1)
        .and_then(|rest| rest.split(" after it was mapped").next())
        .and_then(|span| {
            span.strip_suffix("ms")
                .and_then(|ms| ms.parse::<f64>().ok())
                .or_else(|| {
                    span.strip_suffix('s')
                        .and_then(|s| s.parse::<f64>().ok())
                        .map(|s| s * 1e3)
                })
        })
        .unwrap_or_else(|| panic!("no `at most` delay in the notes: {said}"));
    let missed = (at_most_ms / 10.0).ceil() as u64 + 1;
    assert!(
        at_most_ms < 250.0 && (40 - missed.min(40)..=40).contains(&count),
        "{count} calls timed, at most {at_most_ms} ms unprobed: {summary:?}\n{said}"
    );

    let missing = run("omn_plugin_absent");
    let text = spoken(&missing);
    assert!(!missing.status.success(), "{text}");
    assert!(
        text.contains("found no function named `omn_plugin_absent`")
            && text.contains("while it ran"),
        "{text}"
    );
}

/// The histogram one series of a document holds, as `(count, sum)`.
fn histogram(document: &Value) -> (f64, f64) {
    let hist = document["results"][0]["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|output| output["emit"] == "hist")
        .unwrap_or_else(|| panic!("no hist: {document:#}"));
    let series = hist["series"].as_array().expect("series");
    (
        series.iter().map(|one| number(&one["count"])).sum(),
        series.iter().map(|one| number(&one["sum"])).sum(),
    )
}

/// `--emit hist` alone is built in the kernel: rows counted where the call
/// returns, no record per call, and every call of a function called two
/// hundred thousand times in a row is in a bucket.
#[test]
fn a_histogram_alone_is_counted_in_the_kernel_and_misses_no_call() {
    let Some((document, _)) = timed_as(&["hist"], "function == 'tiny'", &[], &["tiny", "200000"])
    else {
        return;
    };
    let (count, _) = histogram(&document);
    assert_eq!(count, 200_000.0, "{}", notes(&document));
    assert!(
        notes(&document).contains("built in the kernel"),
        "{}",
        notes(&document)
    );
    assert_eq!(
        document["results"][0]["losses"]["dropped"]
            .as_u64()
            .unwrap_or(0),
        0,
        "{document:#}"
    );
}

/// A row's exact sum travels beside its bucket: the histogram's sum is what
/// the program measured around its calls, not their buckets' low bounds.
#[test]
fn a_kernel_histogram_s_sum_is_the_calls_own() {
    let Some((document, printed)) =
        timed_as(&["hist"], "function == 'hold'", &[], &["hold", "3", "5"])
    else {
        return;
    };
    let (count, sum) = histogram(&document);
    assert_eq!(count, 5.0, "{document:#}");
    let measured: f64 = printed.iter().map(|line| measured(line)).sum();
    // Each call's time is inside what the program measured around it, short
    // of it by the clock reads and the traps: a few microseconds a call.
    assert!(
        sum <= measured && measured - sum < 5.0 * 50_000.0,
        "the histogram's sum {sum} against the program's {measured}"
    );
}
