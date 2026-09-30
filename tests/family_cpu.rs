//! End-to-end checks on the `cpu` family.
//!
//! The unit tests in `src/collect/cpu/counters.rs` check the counter against a
//! loop whose instruction count is known by construction. These check the same
//! thing through the binary, where the request, the resolver, the launch and
//! the aggregation are all in the way -- and check that a request this family
//! cannot serve is refused with the reason rather than answered with the wrong
//! mechanism.

use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

fn run(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

/// Feeds a session file on standard input, which is what `-f -` reads.
fn run_with_stdin(args: &[&str], stdin: &str) -> Output {
    use std::io::Write;
    let mut child = Command::new(BINARY)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("omn should run");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(stdin.as_bytes())
        .expect("writing the session");
    child.wait_with_output().expect("omn should finish")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The result of a run that was supposed to measure something, as JSON.
///
/// `None` means this machine cannot serve the request -- no counter, no
/// permission -- which is a legitimate reason to stop. A request the tool
/// *refused* is not: it is a test asking for something that cannot mean
/// anything, and it must fail loudly rather than pass by skipping. That
/// distinction was learned here: a test asked `cpu.ipc` for an emit the source
/// does not declare and quietly proved nothing for several runs.
fn measured(args: &[&str]) -> Option<serde_json::Value> {
    let output = run(args);
    if output.status.success() {
        return Some(serde_json::from_str(&stdout(&output)).expect("the json output parses"));
    }
    let message = stderr(&output);
    assert!(
        machine_cannot_serve(&message),
        "the request was refused, which is a broken test rather than a limited machine: {message}"
    );
    eprintln!("skipped: {message}");
    None
}

/// Whether a failure is this machine's limit rather than a refusal of the
/// request itself. Only the first is a reason for a test to stop quietly.
fn machine_cannot_serve(message: &str) -> bool {
    message.contains("no viable backend")
        || message.contains("cannot start collecting")
        || message.contains("perf_event_paranoid")
}

/// A shell loop of `iterations` turns, which is a program whose work scales
/// linearly with a number this test chooses.
fn counting_loop(iterations: u32) -> String {
    format!("i=0; while [ $i -lt {iterations} ]; do i=$((i+1)); done")
}

/// The count of `cpu.instructions` for one run of that loop, or nothing when
/// this machine will not open the counter.
fn instructions_for(iterations: u32) -> Option<f64> {
    let document = measured(&[
        "cpu.instructions",
        "-m",
        "count",
        "-e",
        "summary",
        "--format",
        "json",
        "--",
        "/bin/sh",
        "-c",
        &counting_loop(iterations),
    ])?;
    let summary = &document["results"][0]["outputs"][0]["series"][0]["last"];
    Some(summary.as_f64().expect("a counted value"))
}

/// Ground truth through the whole tool: twice the work is twice the
/// instructions. The absolute count depends on this machine's shell, but the
/// ratio depends only on the counter being a counter.
#[test]
fn twice_the_work_retires_about_twice_the_instructions() {
    let Some(once) = instructions_for(100_000) else {
        return;
    };
    let Some(twice) = instructions_for(200_000) else {
        return;
    };
    let ratio = twice / once;
    assert!(
        (1.9..2.1).contains(&ratio),
        "doubling the loop changed the instruction count by a factor of {ratio:.3} \
         ({once} then {twice})"
    );
}

/// `cpu.ipc` is derived, and a derived number that hides its terms cannot be
/// checked. Both counters have to be on the event.
#[test]
fn a_ratio_is_published_with_the_counters_it_came_from() {
    let Some(document) = measured(&[
        "cpu.ipc",
        "-m",
        "count",
        "-e",
        "raw",
        "--format",
        "json",
        "--",
        "/bin/sh",
        "-c",
        &counting_loop(50_000),
    ]) else {
        return;
    };
    let event = &document["results"][0]["outputs"][0]["events"][0];
    let attrs = event["attrs"].as_array().expect("the event carries attrs");
    let named = |name: &str| -> f64 {
        attrs
            .iter()
            .find(|attr| attr["name"] == name)
            .unwrap_or_else(|| panic!("`{name}` is on the event"))["value"]
            .as_f64()
            .expect("numeric")
    };
    let (ipc, instructions, cycles) = (named("ipc"), named("instructions"), named("cycles"));
    assert!((ipc - instructions / cycles).abs() < 1e-9);
    assert!(
        (0.01..20.0).contains(&ipc),
        "an ipc of {ipc} is outside anything a cpu can do"
    );
}

/// Counting mode is not a cheap sampling mode, and the refusal has to say which
/// mechanism is missing rather than return a count labelled as a sample.
#[test]
fn sampling_a_counter_is_refused_with_the_mechanism_it_would_need() {
    let output = run(&[
        "cpu.cycles",
        "-m",
        "sample",
        "-e",
        "hist",
        "--",
        "/bin/true",
    ]);
    assert!(!output.status.success(), "{}", stdout(&output));
    let message = stderr(&output);
    // The collector's refusal is reached only through a backend the machine
    // offers; `perf_event_paranoid` above 2 withholds every counter.
    if message.contains("has no viable backend on this machine") {
        eprintln!("skipped: {message}");
        return;
    }
    assert!(message.contains("sampling path"), "{message}");
    assert!(message.contains("--mode count"), "{message}");
}

/// The record kept beside the numbers has to name the counter that produced
/// them: two runs through different backends are not comparable.
#[test]
fn the_resolved_record_names_the_backend_that_counts() {
    let Some(document) = measured(&[
        "cpu.instructions",
        "-m",
        "count",
        "-e",
        "summary",
        "--dump-resolved",
        "--format",
        "json",
        "--",
        "/bin/true",
    ]) else {
        return;
    };
    let resolved = &document[0];
    assert_eq!(resolved["backend"]["id"], "pmu");
    assert_eq!(
        resolved["backend"]["requirements"][0]["needs"],
        "pmu:PERF_COUNT_HW_INSTRUCTIONS"
    );
    // The counter's meaning depends on the machine as much as on the event.
    assert!(resolved["environment"]["smt"].is_string());
    assert!(resolved["environment"]["cpu_governor"].is_string());
}

/// The machine-wide sources answer without a pid, and their shares have to be
/// shares: a utilisation outside zero to one is a parsing bug wearing a
/// plausible number.
#[test]
fn utilisation_polls_the_machine_and_stays_a_share() {
    let Some(document) = measured(&[
        "cpu.util",
        "--system",
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "--format",
        "json",
        "-d",
        "400ms",
    ]) else {
        return;
    };
    let summary = &document["results"][0]["outputs"][0]["series"][0];
    let count = summary["count"].as_u64().expect("a polled series");
    assert!(count >= 2, "a 400ms run at 100ms produced {count} reads");
    for bound in ["min", "max"] {
        let value = summary[bound].as_f64().expect("a share");
        assert!(
            (0.0..=1.0).contains(&value),
            "`{bound}` utilisation is {value}, which is not a share of anything"
        );
    }
}

/// `--cpu` is how someone asks about a core rather than about a mean, so the
/// answer has to be per core: one series each, keyed on the number. A mean
/// across cpus is a number that describes no cpu, and the emit layer can build
/// one from these while nothing can recover a core from an average.
#[test]
fn per_cpu_utilisation_reports_one_series_for_each_core_asked_for() {
    let Some(document) = measured(&[
        "cpu.util",
        "--cpu",
        "0,1",
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "--format",
        "json",
        "-d",
        "400ms",
    ]) else {
        return;
    };
    let series = document["results"][0]["outputs"][0]["series"]
        .as_array()
        .expect("one series per cpu");
    assert_eq!(series.len(), 2, "two cpus were asked for");
    for (entry, cpu) in series.iter().zip([0, 1]) {
        assert_eq!(entry["key"], format!("cpu:{cpu}"));
        for bound in ["min", "max"] {
            let value = entry[bound].as_f64().expect("a share");
            assert!(
                (0.0..=1.0).contains(&value),
                "`{bound}` utilisation of cpu {cpu} is {value}, which is not a share of anything"
            );
        }
    }
}

/// A cpu with no line in `/proc/stat` is not a cpu that is doing nothing, and
/// the refusal has to say which of the two it is.
#[test]
fn utilisation_of_a_cpu_that_is_not_online_is_refused_rather_than_reported_as_idle() {
    let output = run(&[
        "cpu.util",
        "--cpu",
        "4095",
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "-d",
        "200ms",
    ]);
    assert!(!output.status.success(), "{}", stdout(&output));
    let message = stderr(&output);
    assert!(message.contains("not online"), "{message}");
}

/// Every counting source in the family, through the binary, on a program that
/// does a little work. A source whose event id or backend name is wrong in the
/// table resolves and then finds no collector, which this catches; a source
/// whose counter does not exist on this cpu never reaches the table at all.
#[test]
fn every_counting_source_produces_one_value_for_a_program_that_runs() {
    const SOURCES: [&str; 7] = [
        "cpu.cycles",
        "cpu.instructions",
        "cpu.ipc",
        "cpu.branch_misses",
        "cpu.cache_misses",
        "cpu.context_switches",
        "cpu.migrations",
    ];
    for source in SOURCES {
        let Some(document) = measured(&[
            source,
            "-m",
            "count",
            "-e",
            "summary",
            "--format",
            "json",
            "--",
            "/bin/sh",
            "-c",
            &counting_loop(20_000),
        ]) else {
            continue;
        };
        let results = &document["results"][0];
        assert_eq!(results["source"], source);
        let series = &results["outputs"][0]["series"][0];
        assert_eq!(
            series["count"], 1,
            "{source} reported {} values for one counted window",
            series["count"]
        );
        let value = series["last"].as_f64().expect("a number");
        assert!(
            value.is_finite() && value >= 0.0,
            "{source} reported {value}"
        );
        // A counter nobody counted is a loss, and a loss is never silent.
        assert_eq!(results["losses"]["failed_reads"], 0, "{source}");
    }
}

/// Several sources over one window is the point of a session file: run one
/// after another and the numbers describe different moments of the program.
#[test]
fn one_session_counts_several_sources_over_one_window() {
    let session = format!(
        r#"
name: three counters, one window
format: json
requests:
  - source: cpu.instructions
    target: {{launch: [/bin/sh, -c, "{loop}"]}}
    mode: count
    emit: [summary]
  - source: cpu.ipc
    target: {{launch: [/bin/sh, -c, "{loop}"]}}
    mode: count
    emit: [summary]
  - source: cpu.context_switches
    target: {{launch: [/bin/sh, -c, "{loop}"]}}
    mode: count
    emit: [summary]
"#,
        loop = counting_loop(20_000)
    );
    let output = run_with_stdin(&["-f", "-", "--input-format", "yaml"], &session);
    if !output.status.success() {
        let message = stderr(&output);
        assert!(machine_cannot_serve(&message), "{message}");
        eprintln!("skipped: {message}");
        return;
    }
    let document: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("the json output parses");
    let results = document["results"].as_array().expect("three results");
    assert_eq!(results.len(), 3);

    // One launch, one window: the sources cannot have observed the program for
    // different lengths of time.
    let windows: Vec<u64> = results
        .iter()
        .map(|result| result["observed_ns"].as_u64().expect("a window"))
        .collect();
    assert!(
        windows.iter().all(|window| *window == windows[0]),
        "the sources report different windows: {windows:?}"
    );
}

/// A collector only learns at open time that it cannot measure what was asked
/// -- here, that the cgroup has no quota to be throttled by. That has to reach
/// the output, not stay inside the collector.
#[test]
fn what_a_collector_discovers_at_open_time_reaches_the_output() {
    let Ok(own) = std::fs::read_to_string("/proc/self/cgroup") else {
        eprintln!("skipped: no cgroup v2 here");
        return;
    };
    let Some(path) = own.lines().next().and_then(|line| line.rsplit(':').next()) else {
        eprintln!("skipped: /proc/self/cgroup is not the shape this expects");
        return;
    };
    // Two shapes have nothing to be throttled by, and each is disclosed: the
    // cpu controller not enabled (no `cpu.max`), and `cpu.max` at `max`. Only
    // a real quota leaves nothing to say. (This read the presence of
    // `nr_throttled` as a quota, and skipped on the CI's delegated cgroup,
    // which has the counters and no quota.)
    let quota = std::fs::read_to_string(format!("/sys/fs/cgroup{path}/cpu.max")).ok();
    if let Some(quota) = quota.filter(|quota| !quota.starts_with("max")) {
        eprintln!(
            "skipped: this session's cgroup has a cpu.max quota ({}), so there is nothing to \
             disclose",
            quota.trim()
        );
        return;
    }

    let output = run(&[
        "cpu.throttle",
        "--cgroup",
        path,
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "-d",
        "200ms",
    ]);
    if !output.status.success() {
        let message = stderr(&output);
        assert!(machine_cannot_serve(&message), "{message}");
        eprintln!("skipped: {message}");
        return;
    }
    let text = stdout(&output);
    assert!(text.contains("degraded"), "{text}");
    assert!(text.contains("cpu.max"), "{text}");
    // The distinction the whole path exists for.
    assert!(text.contains("not the same as not having been"), "{text}");
}

/// The examples are documentation that runs. One that stopped validating would
/// be worse than none, because it is the first thing anyone copies.
#[test]
fn the_families_examples_validate() {
    let mut checked = 0;
    for entry in std::fs::read_dir("examples").expect("the examples directory") {
        let path = entry.expect("an entry").path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if !name.starts_with("cpu-") {
            continue;
        }
        let output = run(&["validate", "-f", &path.to_string_lossy()]);
        assert!(
            output.status.success(),
            "{name} does not validate: {}",
            stderr(&output)
        );
        checked += 1;
    }
    assert!(checked >= 2, "only {checked} cpu examples were found");
}

/// The cpus of a per-cpu count are kept apart, and each event says which cpu it
/// describes.
///
/// A sum over twenty-four cores describes no core, so the collector never makes
/// one; what this checks is that the parts it does produce are separable, which
/// is what lets the emit layer build the sum for a reader who wants it.
#[test]
fn counting_a_set_of_cpus_reports_each_of_them_separately() {
    let Some(document) = measured(&[
        "cpu.instructions",
        "--cpu",
        "0,1",
        "-m",
        "count",
        "-e",
        "raw",
        "--format",
        "json",
        "-d",
        "200ms",
    ]) else {
        return;
    };
    let events = document["results"][0]["outputs"][0]["events"]
        .as_array()
        .expect("events");
    assert_eq!(events.len(), 2, "two cpus were asked for");
    for (event, cpu) in events.iter().zip([0, 1]) {
        assert_eq!(event["keys"][0]["cpu"]["cpu"], cpu);
        let attrs = event["attrs"].as_array().expect("attrs");
        let named = |name: &str| {
            attrs
                .iter()
                .find(|attr| attr["name"] == name)
                .unwrap_or_else(|| panic!("`{name}` is on the event"))["value"]
                .as_f64()
                .expect("numeric")
        };
        // `cpu` as a key is for a join; `cpu` as an attribute is for a filter,
        // which the catalog declares and a predicate can read.
        assert_eq!(named("cpu"), f64::from(cpu));
        assert!(named("enabled_ns") > 0.0);
    }
}

/// `--system` for a counter is every online cpu, because there is no
/// machine-wide counter in the interface to ask instead.
#[test]
fn counting_the_machine_opens_one_event_per_online_cpu() {
    let online = std::fs::read_to_string("/sys/devices/system/cpu/online")
        .expect("this kernel publishes its online cpus");
    let expected: usize = online
        .trim()
        .split(',')
        .map(|part| match part.split_once('-') {
            Some((first, last)) => {
                last.parse::<usize>().unwrap() - first.parse::<usize>().unwrap() + 1
            }
            None => 1,
        })
        .sum();

    let Some(document) = measured(&[
        "cpu.cycles",
        "--system",
        "-m",
        "count",
        "-e",
        "summary",
        "--format",
        "json",
        "-d",
        "200ms",
    ]) else {
        return;
    };
    let series = document["results"][0]["outputs"][0]["series"]
        .as_array()
        .expect("one series per cpu");
    assert_eq!(
        series.len(),
        expected,
        "this machine has {expected} online cpus and the run produced {} series",
        series.len()
    );
}

/// A cpu number this machine does not have is not a permission problem, and the
/// refusal must not send the reader to `perf_event_paranoid`.
#[test]
fn counting_a_cpu_that_does_not_exist_says_so_rather_than_blaming_privileges() {
    let output = run(&[
        "cpu.cycles",
        "--cpu",
        "4095",
        "-m",
        "count",
        "-e",
        "summary",
        "-d",
        "100ms",
    ]);
    assert!(!output.status.success(), "{}", stdout(&output));
    let message = stderr(&output);
    // A machine that withholds every counter refuses before the cpu is looked
    // at, and rightly names the sysctl: there, it is the answer.
    // So does one that has no cycle counter at all, or lets no process count
    // a cpu (the CI's paranoid-2 cell): the refusal is about the machine, and
    // about cpu 0 rather than the cpu asked for.
    if message.contains("has no viable backend on this machine") && !message.contains("4095") {
        eprintln!("skipped: {message}");
        return;
    }
    assert!(message.contains("not online"), "{message}");
    assert!(!message.contains("perf_event_paranoid"), "{message}");
}

/// `cpu.oncpu_time` against the wall clock of the program it watched: a shell
/// loop is on a cpu for its whole life, so almost all of the window has to come
/// back as consumed time.
#[test]
fn cpu_time_of_a_program_that_never_blocks_is_most_of_its_wall_clock() {
    let Some(document) = measured(&[
        "cpu.oncpu_time",
        "-m",
        "poll@50ms",
        "-e",
        "summary",
        "--format",
        "json",
        "--",
        "/bin/sh",
        "-c",
        &counting_loop(400_000),
    ]) else {
        return;
    };
    let result = &document["results"][0];
    let observed = result["observed_ns"].as_f64().expect("a window");
    let series = &result["outputs"][0]["series"][0];
    let reads = series["count"].as_f64().expect("a polled series");
    let mean = series["mean"].as_f64().expect("a mean");
    let consumed = mean * reads;
    assert!(
        consumed > observed * 0.5,
        "a shell loop ran for {:.0} ms of wall clock and is reported as having consumed \
         {:.0} ms of cpu",
        observed / 1e6,
        consumed / 1e6
    );
    // The tick granularity the backend declares cannot exceed the window
    // either: more cpu time than there was time is a parsing bug.
    assert!(
        consumed < observed * 1.5,
        "{:.0} ms of cpu time in a {:.0} ms window from a single-threaded program",
        consumed / 1e6,
        observed / 1e6
    );
}

/// The three `mem.cache_*` sources are rows in this family's counters table --
/// the mechanism decides who writes a source, not the first word of its id --
/// so they are exercised here, beside the counters they are built from.
#[test]
fn every_cache_source_counts_and_publishes_what_its_misses_are_out_of() {
    for (source, accesses) in [
        ("mem.cache_l1", Some("l1d_loads")),
        ("mem.cache_llc", None),
        ("mem.cache_tlb", None),
    ] {
        let Some(document) = measured(&[
            source,
            "-m",
            "count",
            "-e",
            "raw",
            "--format",
            "json",
            "--",
            "/bin/sh",
            "-c",
            &counting_loop(50_000),
        ]) else {
            continue;
        };
        let event = &document["results"][0]["outputs"][0]["events"][0];
        let attrs = event["attrs"].as_array().expect("attrs");
        let named = |name: &str| -> Option<f64> {
            attrs.iter().find(|attr| attr["name"] == name)?["value"].as_f64()
        };
        let misses = named("count").unwrap_or_else(|| panic!("{source} reported no count"));
        assert!(misses >= 0.0, "{source} reported {misses}");
        if let Some(accesses) = accesses {
            let accesses =
                named(accesses).unwrap_or_else(|| panic!("{source} did not publish `{accesses}`"));
            assert!(
                accesses >= misses,
                "{source} missed {misses} times in {accesses} accesses"
            );
        }
        assert_eq!(
            document["results"][0]["losses"]["failed_reads"], 0,
            "{source}"
        );
    }
}

/// `cpu.topdown` is the one source of this family that needs hardware most
/// machines here do not have. Either it measures -- and then its four
/// categories are a partition of the issue slots, which is the whole claim of
/// the method -- or it is refused by naming the events this cpu does not
/// publish. Both are answers; neither is a skip.
#[test]
fn topdown_either_partitions_the_slots_or_names_what_this_cpu_lacks() {
    let output = run(&[
        "cpu.topdown",
        "-m",
        "count",
        "-e",
        "raw",
        "--format",
        "json",
        "--",
        "/bin/sh",
        "-c",
        &counting_loop(200_000),
    ]);
    if !output.status.success() {
        let message = stderr(&output);
        assert!(message.contains("no viable backend"), "{message}");
        assert!(message.contains("slots"), "{message}");
        eprintln!("skipped, and the refusal checked: this cpu has no issue-slot counter");
        return;
    }
    let document: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("the json output parses");
    let attrs = document["results"][0]["outputs"][0]["events"][0]["attrs"]
        .as_array()
        .expect("attrs")
        .clone();
    let named = |name: &str| -> f64 {
        attrs
            .iter()
            .find(|attr| attr["name"] == name)
            .unwrap_or_else(|| panic!("`{name}` is on the event"))["value"]
            .as_f64()
            .expect("numeric")
    };
    let accounted = named("accounted");
    assert!(
        (accounted - 1.0).abs() < 0.02,
        "the four categories account for {accounted} of the issue slots"
    );
    for category in [
        "retiring",
        "bad_speculation",
        "frontend_bound",
        "backend_bound",
    ] {
        let fraction = named(category);
        assert!(
            (0.0..=1.0).contains(&fraction),
            "`{category}` is {fraction}, which is not a share of anything"
        );
    }
}
