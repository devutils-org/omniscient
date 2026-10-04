//! End-to-end checks for the `gpu` family.
//!
//! Most machines have no GPU this tool can read, and on those every measuring
//! test here skips with the reason rather than failing: a red test on a laptop
//! with an integrated Intel chip says nothing about the code. What does not
//! depend on hardware -- that a device-wide source refuses a per-process
//! target, that a source with no collector says so -- is checked everywhere.

use std::process::{Command, Output, Stdio};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// A request was refused because it cannot mean what it asks for.
const EXIT_REJECTED: i32 = 2;
/// The request is valid, but nothing collects it yet.
const EXIT_NOT_IMPLEMENTED: i32 = 3;

fn run(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Runs a device-state request over a short launched program, or says why this
/// machine cannot.
fn measure(source: &str, emit: &str) -> Result<serde_json::Value, String> {
    measure_with(source, emit, None)
}

fn measure_with(
    source: &str,
    emit: &str,
    filter: Option<&str>,
) -> Result<serde_json::Value, String> {
    let mut args = vec![
        source,
        "--mode",
        "poll@100ms",
        "--emit",
        emit,
        "--format",
        "json",
    ];
    if let Some(filter) = filter {
        args.extend_from_slice(&["--filter", filter]);
    }
    args.extend_from_slice(&["--", "sleep", "0.4"]);
    let output = run(&args);
    if output.status.success() {
        return serde_json::from_slice(&output.stdout).map_err(|error| error.to_string());
    }
    let complaint = stderr(&output);
    // No device, no driver, or a driver this user may not talk to: all three
    // are facts about the machine, and the message already says which.
    if complaint.contains("libnvidia-ml")
        || complaint.contains("no viable backend")
        || complaint.contains("reports no device")
    {
        Err(complaint.lines().next().unwrap_or("no GPU here").to_owned())
    } else {
        panic!("{source} failed for a reason that is not a missing GPU: {complaint}");
    }
}

fn attr(event: &serde_json::Value, name: &str) -> Option<serde_json::Value> {
    event["attrs"]
        .as_array()?
        .iter()
        .find(|attr| attr["name"] == name)
        .map(|attr| attr["value"].clone())
}

/// What the vendor's own tool says, for the one cross-check this hardware
/// allows. Omniscient never runs `nvidia-smi` -- it calls the library that
/// tool calls -- but a second reader is the only independent opinion available
/// on a GPU whose kernel interface exposes nothing.
fn nvidia_smi(query: &str) -> Option<String> {
    let output = Command::new("nvidia-smi")
        .args([
            &format!("--query-gpu={query}"),
            "--format=csv,noheader,nounits",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(|line| line.trim().to_owned())
}

#[test]
fn a_device_reading_says_which_device_it_is_and_what_it_is_a_figure_for() {
    let run = match measure("gpu.temp", "raw") {
        Ok(run) => run,
        Err(reason) => return eprintln!("skipped: {reason}"),
    };
    let events = run["results"][0]["outputs"][0]["events"]
        .as_array()
        .expect("raw emits events")
        .clone();
    assert!(!events.is_empty(), "a poll over 400 ms reads at least once");
    for event in &events {
        assert_eq!(event["kind"], "gpu.temp");
        // The scope is on every event because a device figure taken while one
        // program ran is not that program's figure, and the file has to say so
        // without a reader who remembers the command line.
        assert_eq!(attr(event, "scope").unwrap(), "device");
        assert!(attr(event, "device").is_some());
        assert!(attr(event, "device_uuid").is_some());
        let celsius = attr(event, "celsius").unwrap().as_f64().unwrap();
        assert!((1.0..=125.0).contains(&celsius), "{celsius} C");
    }
}

/// Ground truth, as far as this hardware offers one: the number must match
/// what an independent reader of the same device says.
#[test]
fn the_temperature_matches_what_the_vendors_own_tool_reports() {
    let run = match measure("gpu.temp", "summary") {
        Ok(run) => run,
        Err(reason) => return eprintln!("skipped: {reason}"),
    };
    let Some(reference) = nvidia_smi("temperature.gpu").and_then(|text| text.parse::<f64>().ok())
    else {
        return eprintln!("skipped: nvidia-smi is not on this machine");
    };

    let ours = run["results"][0]["outputs"][0]["series"][0]["last"]
        .as_f64()
        .expect("a summary carries the last reading");
    assert!(
        (ours - reference).abs() <= 3.0,
        "this tool read {ours} C and nvidia-smi read {reference} C moments later"
    );
}

/// The same check for VRAM, where the two readers must agree on the device
/// total exactly: it is a constant of the hardware, not a measurement.
#[test]
fn the_device_memory_total_matches_the_vendors_own_tool() {
    let run = match measure("gpu.mem", "raw") {
        Ok(run) => run,
        Err(reason) => return eprintln!("skipped: {reason}"),
    };
    let Some(reference) = nvidia_smi("memory.total").and_then(|text| text.parse::<f64>().ok())
    else {
        return eprintln!("skipped: nvidia-smi is not on this machine");
    };

    let event = run["results"][0]["outputs"][0]["events"][0].clone();
    let total = attr(&event, "device_total_bytes")
        .and_then(|value| value.as_f64())
        .expect("the device total rides along with a process figure");
    // nvidia-smi reports mebibytes, rounded to the unit.
    let ours_mib = total / (1024.0 * 1024.0);
    assert!(
        (ours_mib - reference).abs() <= 1.0,
        "this tool read {ours_mib} MiB of VRAM and nvidia-smi read {reference} MiB"
    );
    assert_eq!(attr(&event, "scope").unwrap(), "process");
}

/// A launched program that never touches the GPU holds no VRAM, and the run
/// says so with zeros rather than with an empty series: "nothing was measured"
/// and "the measurement is zero" are different claims.
#[test]
fn a_program_that_never_touches_the_gpu_reports_zero_rather_than_nothing() {
    let run = match measure("gpu.mem", "raw") {
        Ok(run) => run,
        Err(reason) => return eprintln!("skipped: {reason}"),
    };
    let events = run["results"][0]["outputs"][0]["events"]
        .as_array()
        .expect("raw emits events")
        .clone();
    assert!(!events.is_empty());
    for event in &events {
        assert_eq!(attr(event, "bytes").unwrap(), 0);
        assert_eq!(attr(event, "held_as").unwrap(), "none");
    }
}

/// A device figure has no process to belong to, so `--system` is its natural
/// target: the run is bounded by a duration rather than by a program's life.
#[test]
fn a_system_target_measures_the_device_and_says_that_is_what_it_did() {
    let output = run(&[
        "gpu.mem", "--system", "--mode", "snapshot", "--emit", "raw", "--format", "json",
    ]);
    if !output.status.success() {
        let complaint = stderr(&output);
        assert!(
            complaint.contains("libnvidia-ml")
                || complaint.contains("no viable backend")
                || complaint.contains("reports no device"),
            "{complaint}"
        );
        return eprintln!("skipped: no GPU on this machine");
    }

    let run: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json results");
    let event = run["results"][0]["outputs"][0]["events"][0].clone();
    // Without a process, the figure is the device's own VRAM and the event says
    // so: the same number under `scope=process` would be an attribution nobody
    // measured.
    assert_eq!(attr(&event, "scope").unwrap(), "device");
    assert!(event["keys"].as_array().is_none_or(|keys| keys.is_empty()));
    let used = attr(&event, "bytes").unwrap().as_f64().unwrap();
    let total = attr(&event, "device_total_bytes")
        .unwrap()
        .as_f64()
        .unwrap();
    assert!(used <= total, "{used} bytes held of {total}");
}

#[test]
fn a_device_wide_source_refuses_a_per_process_target() {
    // A clock is the device's, and there is no per-process share of it to
    // return. Refusing is the point; answering with the device's number under
    // a pid would be the worst bug this tool could have.
    let output = run(&[
        "gpu.freq",
        "--pid",
        "1",
        "--mode",
        "poll@100ms",
        "--dump-request",
    ]);
    assert_eq!(output.status.code(), Some(EXIT_REJECTED), "{output:?}");
    let complaint = stderr(&output);
    assert!(complaint.contains("pid"), "{complaint}");
    assert!(
        complaint.contains("system") || complaint.contains("launch"),
        "the refusal should name what this source does accept: {complaint}"
    );
}

#[test]
fn a_source_whose_collector_is_not_written_says_so_instead_of_measuring() {
    // `gpu.counters` through `vendor:rocprof` has no collector (the
    // nineteenth wave wrote `gpu.kernel`'s and `gpu.counters`' CUPTI ones,
    // which this test used to name), so the backend is pinned. Whether this
    // machine has the SDK or not, the answer must name the gap rather than
    // produce numbers from somewhere else.
    let output = run(&[
        "gpu.counters",
        "--mode",
        "trace",
        "-e",
        "raw",
        "--backend",
        "force:vendor:rocprof",
        "--",
        "true",
    ]);
    assert!(!output.status.success());
    let complaint = stderr(&output);
    if output.status.code() == Some(EXIT_NOT_IMPLEMENTED) {
        assert!(complaint.contains("no collector"), "{complaint}");
    } else {
        assert!(complaint.contains("no viable backend"), "{complaint}");
    }
}

#[test]
fn the_listing_marks_what_this_machine_can_serve() {
    let output = run(&["list", "gpu"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let listing = stdout(&output);
    for source in [
        "gpu.job",
        "gpu.engine_busy",
        "gpu.freq",
        "gpu.power",
        "gpu.temp",
        "gpu.mem",
        "gpu.kernel",
        "gpu.api",
        "gpu.counters",
        "gpu.profile",
    ] {
        assert!(
            listing.contains(source),
            "{source} is missing from `omn list gpu`"
        );
    }
    // On a machine with no DRM scheduler there is nothing to trace, and the
    // listing has to say so rather than leave the question open.
    let drm_scheduler = std::path::Path::new("/sys/kernel/tracing/events/gpu_scheduler").exists();
    if !drm_scheduler {
        let job = listing
            .lines()
            .find(|line| line.starts_with("gpu.job"))
            .expect("gpu.job is listed");
        assert!(
            job.contains(" no "),
            "without gpu_scheduler tracepoints gpu.job cannot be served: {job}"
        );
    }
}

/// The filter has to reach the events of a source that declares the field, and
/// the run has to say what it removed. A GPU is the case that made this
/// visible: `device` is the field that matters on a machine with two cards, and
/// a predicate over it that quietly kept everything would average them.
#[test]
fn a_filter_on_the_device_keeps_that_device_and_reports_the_rest() {
    let kept = match measure_with("gpu.temp", "raw", Some("device == 0")) {
        Ok(run) => run,
        Err(reason) => return eprintln!("skipped: {reason}"),
    };
    let events = kept["results"][0]["outputs"][0]["events"]
        .as_array()
        .expect("raw emits events")
        .clone();
    assert!(
        !events.is_empty(),
        "device 0 exists on any machine with a GPU"
    );
    for event in &events {
        assert_eq!(attr(event, "device").unwrap(), 0);
    }
    let filtered = &kept["results"][0]["filtered"];
    assert_eq!(filtered["predicate"], "device == 0");
    assert_eq!(filtered["kept"].as_u64().unwrap(), events.len() as u64);

    // And a device that is not there keeps nothing, without the run pretending
    // the numbers it did collect answer the question.
    let dropped = match measure_with("gpu.temp", "raw", Some("device == 99")) {
        Ok(run) => run,
        Err(reason) => return eprintln!("skipped: {reason}"),
    };
    let none = dropped["results"][0]["outputs"][0]["events"]
        .as_array()
        .expect("raw emits events")
        .clone();
    assert!(none.is_empty(), "no machine here has a hundredth GPU");
    let filtered = &dropped["results"][0]["filtered"];
    assert_eq!(filtered["kept"].as_u64().unwrap(), 0);
    assert!(
        filtered["dropped"].as_u64().unwrap() > 0,
        "the events that were collected and discarded have to be counted: {filtered}"
    );
    // `device` was on every event, so the empty series is the predicate's doing
    // and not a field nothing carried.
    assert!(filtered["unseen_fields"].is_null(), "{filtered}");
}

/// Every run of a device source carries the device's clock state in its
/// notes: whether the shader clock is locked, the application clocks,
/// persistence mode and the clock event reasons, read before the run and never
/// changed. Persistence mode is held to the vendor's own tool, the one
/// independent reader this hardware allows, because it does not move while
/// another program uses the board.
#[test]
fn a_device_run_records_the_state_of_the_clocks_it_read_and_never_set() {
    let run = match measure("gpu.temp", "summary") {
        Ok(run) => run,
        Err(reason) => return eprintln!("skipped: {reason}"),
    };
    let notes: Vec<String> = run["results"][0]["notes"]
        .as_array()
        .expect("a device run states the clocks it found")
        .iter()
        .map(|note| note.as_str().unwrap_or_default().to_owned())
        .collect();
    let state = notes
        .iter()
        .find(|note| note.contains("shader clock"))
        .unwrap_or_else(|| panic!("no note states the clocks: {notes:?}"));
    assert!(state.starts_with("vendor:nvml: device 0 ("), "{state}");
    assert!(state.contains("clock event reasons"), "{state}");
    match nvidia_smi("persistence_mode").as_deref() {
        Some("Enabled") => assert!(state.contains("persistence mode on"), "{state}"),
        Some("Disabled") => assert!(state.contains("persistence mode off"), "{state}"),
        other => eprintln!("persistence not compared: nvidia-smi answered {other:?}"),
    }
}

/// `gpu.profile`'s stacks are the device's -- kernel, line, stall -- and
/// nothing is unwound: the resolver's unwinding warning is not raised for
/// it, and still is for a host profile in the same session.
#[test]
fn a_device_stack_raises_no_unwinding_warning() {
    let session = std::env::temp_dir().join(format!("omn-gpu-unwind-{}.yml", std::process::id()));
    std::fs::write(
        &session,
        "requests:\n  \
         - {source: gpu.profile, target: {launch: [x]}, mode: trace, emit: [stack]}\n  \
         - {source: cpu.profile, target: {launch: [x]}, mode: sample, emit: [stack]}\n",
    )
    .unwrap();
    let output = run(&["validate", "-f", session.to_str().unwrap()]);
    let _ = std::fs::remove_file(&session);
    let text = stdout(&output);
    assert!(output.status.success(), "{}", stderr(&output));
    let warned: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("no --stack given"))
        .collect();
    assert_eq!(warned.len(), 1, "{text}");
    assert!(warned[0].contains("[cpu.profile #2]"), "{text}");
}
