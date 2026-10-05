//! Scenarios for the `gpu` family.
//!
//! A scenario is a workload with a property chosen in its own source, a session
//! that measures it, and a check of the answer against that property. What is
//! being tested here is not that the code returns what the code computed --
//! `tests/family_gpu.rs` and the unit tests already do that -- but that the
//! number the tool reports is the number the workload put there.
//!
//! The workload is `tests/gpu_workload.py`. It holds a chosen quantity of video
//! memory, keeps a compute kernel resident for a chosen window, or leaves the
//! device alone for one; each session in `examples/gpu-*.yml` names the
//! quantity it is about to check.
//!
//! Three things about this family shape every test below.
//!
//! **There is one mechanism.** Every `gpu` source this build collects goes
//! through the NVIDIA management library, so the backend comparison that
//! `--backend force:` exists for has nothing to compare: the comparison here is
//! with the refusal, and with a second reader of the same device.
//!
//! **A device figure is not a process figure.** Only `gpu.mem` attributes
//! anything to a process. For the other four a launched program sets the
//! window and nothing else, which means every reading is shared with whatever
//! else is holding a context -- a compositor, a browser, another test.
//!
//! **The device carries state between runs.** A GPU that boosted a second ago
//! is still boosted, still hot and still drawing, and NVML's occupancy figure
//! is averaged over a window up to a second wide. Two runs taken back to back
//! are not two independent samples. The tests that compare runs serialise on
//! `DEVICE` and settle the device first; `docs/scenarios/gpu.md` has what was
//! measured about that.
//!
//! A machine with no NVIDIA device skips the measuring tests with the reason
//! the tool gave. The refusals, the catalog sweep and the exit codes are
//! checked everywhere, because none of them needs hardware.

use std::process::{Command, Output};
use std::sync::{OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use serde_json::Value;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");
const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// The request is valid and this machine cannot serve it.
const EXIT_UNAVAILABLE: i32 = 1;
/// A request was refused because it cannot mean what it asks for.
const EXIT_REJECTED: i32 = 2;
/// The request is valid, but nothing in this build collects it.
const EXIT_NOT_IMPLEMENTED: i32 = 3;

/// The video memory `examples/gpu-vram-step.yml` tells the workload to hold.
/// The session file says the same number; if these two ever disagree the test
/// is checking a run that measured something else.
const ARENA_BYTES: i64 = 268_435_456;

/// One device, twenty-three tests, and readings that take seconds to decay.
///
/// A test that reads a *value* off the device -- an occupancy, a draw, a clock
/// -- needs nothing else in this binary touching the device while it does, so
/// it takes `exclusive()`. A test that reads a *shape* -- an exit code, an emit
/// name, a loss count -- does not care what the numbers are, and several of
/// those can run at once behind `shared()`. They still have to hold something:
/// a sweep firing thirty requests at the driver keeps the device awake, which
/// is enough to lift an idle baseline off its floor.
static DEVICE: RwLock<()> = RwLock::new(());

/// For a test that reads a number off the device.
fn exclusive() -> RwLockWriteGuard<'static, ()> {
    DEVICE.write().unwrap_or_else(|held| held.into_inner())
}

/// For a test that only reads the shape of an answer.
fn shared() -> RwLockReadGuard<'static, ()> {
    DEVICE.read().unwrap_or_else(|held| held.into_inner())
}

fn omn(args: &[&str]) -> Output {
    Command::new(BINARY)
        .current_dir(ROOT)
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

fn code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

/// True when a failure is this machine saying it has no GPU, rather than the
/// tool saying something is wrong.
fn is_a_missing_device(complaint: &str) -> bool {
    complaint.contains("libnvidia-ml")
        || complaint.contains("no viable backend")
        || complaint.contains("reports no device")
}

/// Whether this machine has a device these scenarios can measure, probed once.
fn device() -> Result<(), String> {
    static PROBE: OnceLock<Result<(), String>> = OnceLock::new();
    PROBE
        .get_or_init(|| {
            let output = omn(&[
                "gpu.temp", "--system", "--mode", "snapshot", "--emit", "raw", "--format", "json",
            ]);
            if output.status.success() {
                return Ok(());
            }
            let complaint = stderr(&output);
            if is_a_missing_device(&complaint) {
                Err(complaint
                    .lines()
                    .next()
                    .unwrap_or("no GPU on this machine")
                    .to_owned())
            } else {
                panic!("gpu.temp failed for a reason that is not a missing GPU: {complaint}");
            }
        })
        .clone()
}

/// Whether the workload itself can run here, probed once.
///
/// It needs `python3` and a loadable `libcuda.so.1`, and it says which is
/// absent. A scenario that cannot create the property it is about to check has
/// nothing to check, and that is a fact about the machine, not a failure.
fn workload() -> Result<(), String> {
    static PROBE: OnceLock<Result<(), String>> = OnceLock::new();
    PROBE
        .get_or_init(|| {
            let output = Command::new("python3")
                .current_dir(ROOT)
                .args(["tests/gpu_workload.py", "arena", "1048576", "0", "0"])
                .output()
                .map_err(|error| format!("python3 does not run here: {error}"))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .next()
                    .unwrap_or("the GPU workload will not start")
                    .to_owned())
            }
        })
        .clone()
}

/// Both of the above, for the tests that need a device *and* a program able to
/// put something on it.
fn device_and_workload() -> Result<(), String> {
    device()?;
    workload()
}

/// Whether this test has the board to itself, asked of the driver.
///
/// Two scenarios here compare a device figure against a workload they put on it
/// themselves, and neither means anything while a second process is using the
/// board: an allocation step is not the device's step if someone else is
/// allocating too, and an idle device does not read idle if someone else is
/// resident. Both failed exactly that way on the development machine, where the
/// author's editor holds 98 MiB of VRAM and keeps the device a few per cent
/// busy -- a permanent condition, not a Tuesday.
///
/// So it is detected rather than tolerated, and the numbers that show why go
/// into the skip. Loosening the bounds instead would turn a real second tenant
/// into slack, and the next reader would have no way to tell the difference
/// between a device that is idle and one that is merely quiet.
///
/// Probed once: a client that appears mid-suite is a different scenario, and
/// the `exclusive()` lock is what keeps this suite's own workloads apart.
fn device_to_ourselves() -> Result<(), String> {
    static PROBE: OnceLock<Result<(), String>> = OnceLock::new();
    PROBE.get_or_init(device_tenants).clone()
}

/// Who else is on the device right now, asked again every time: the answer
/// before a test is not the answer after it (flaky tally #24 -- the idle
/// baseline read 100 % because somebody else's kernel arrived meanwhile).
fn device_tenants() -> Result<(), String> {
    (|| {
        let output = Command::new("nvidia-smi")
            .args([
                "--query-compute-apps=pid,process_name,used_memory",
                "--format=csv,noheader,nounits",
            ])
            .output()
            .map_err(|error| format!("nvidia-smi does not run here: {error}"))?;
        if !output.status.success() {
            return Err("nvidia-smi cannot list the device's clients".to_owned());
        }
        let tenants: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| line.to_owned())
            .collect();
        if tenants.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "another process is using this GPU, so a device figure is not this \
                     workload's: {}",
                tenants.join("; ")
            ))
        }
    })()
}

/// Runs one of this family's sessions and returns its results as JSON.
///
/// `--format json` on the command line overrides the `format: text` in the
/// file, so the session a person reads and the session a test parses are the
/// same object rather than two that drift apart.
fn session(file: &str) -> Value {
    let path = format!("examples/{file}");
    let output = omn(&["-f", &path, "--format", "json"]);
    assert!(
        output.status.success(),
        "{path} did not run: {}",
        stderr(&output)
    );
    serde_json::from_str(&stdout(&output))
        .unwrap_or_else(|error| panic!("{path} produced output that is not JSON: {error}"))
}

fn result<'a>(run: &'a Value, label: &str) -> &'a Value {
    run["results"]
        .as_array()
        .expect("a run carries results")
        .iter()
        .find(|result| result["label"] == label)
        .unwrap_or_else(|| panic!("no request in this session is labelled `{label}`"))
}

fn output<'a>(result: &'a Value, emit: &str) -> &'a Value {
    result["outputs"]
        .as_array()
        .expect("a result carries outputs")
        .iter()
        .find(|output| output["emit"] == emit)
        .unwrap_or_else(|| panic!("this request did not emit `{emit}`"))
}

/// The attributes of one event, flattened to a lookup.
fn attr(event: &Value, name: &str) -> Value {
    event["attrs"]
        .as_array()
        .expect("an event carries attributes")
        .iter()
        .find(|attr| attr["name"] == name)
        .map(|attr| attr["value"].clone())
        .unwrap_or_else(|| panic!("no attribute named `{name}` on this event"))
}

fn events<'a>(result: &'a Value, emit: &str) -> &'a Vec<Value> {
    output(result, emit)["events"]
        .as_array()
        .expect("a raw output carries events")
}

fn points(result: &Value, emit: &str) -> Vec<f64> {
    output(result, emit)["series"][0]["points"]
        .as_array()
        .expect("a timeline carries points")
        .iter()
        .map(|point| point["value"].as_f64().expect("a point is a number"))
        .collect()
}

fn window_seconds(result: &Value) -> f64 {
    result["observed_ns"].as_f64().expect("a run has a window") / 1e9
}

/// What the vendor's own tool says. It is not this tool's mechanism -- both end
/// up in `libnvidia-ml` -- but it is a second reader of the same device, and on
/// hardware whose kernel interface exposes nothing it is the only independent
/// opinion available.
fn nvidia_smi(query: &str) -> Option<f64> {
    let output = Command::new("nvidia-smi")
        .args([
            &format!("--query-gpu={query}"),
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Waits for the board to come back down to its floor, and answers where it
/// got to.
///
/// A GPU does not return to its floor when the program using it exits, and it
/// does not glide there either. On the hardware this was written against the
/// draw comes down a staircase after a kernel -- about 94 W for 1.8 s, then
/// about 68 W for 1.8 s, then 40, then 26, reaching its 22 W floor around five
/// seconds later -- and each tread is flat enough that any "wait until the
/// reading stops falling" rule stops on one of them. That is how this function
/// was written the first time, and it returned 94 W.
///
/// So the bar is absolute: a fraction of what the board is rated to draw, which
/// the vendor's own tool will state. Without that tool there is nothing to take
/// a fraction of, and the staircase above is the reason for the number that
/// replaces it.
fn settle() -> f64 {
    let Some(limit) = nvidia_smi("power.limit") else {
        std::thread::sleep(Duration::from_secs(5));
        return f64::NAN;
    };
    let floor = limit * 0.15;
    let deadline = Instant::now() + Duration::from_secs(7);
    loop {
        let output = omn(&[
            "gpu.power",
            "--system",
            "--mode",
            "snapshot",
            "--emit",
            "raw",
            "--format",
            "json",
        ]);
        assert!(output.status.success(), "{}", stderr(&output));
        let run: Value = serde_json::from_str(&stdout(&output)).expect("json results");
        let watts = attr(&run["results"][0]["outputs"][0]["events"][0], "watts")
            .as_f64()
            .expect("the instantaneous draw rides beside the energy counter");
        if watts <= floor || Instant::now() > deadline {
            return watts;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

// ---------------------------------------------------------------------------
// Class A -- the capability sweep
// ---------------------------------------------------------------------------

/// Every source in the family, as the catalog has it.
fn catalog() -> Vec<Value> {
    let output = omn(&["list", "gpu", "--format", "json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    serde_json::from_str(&stdout(&output)).expect("the listing is a projection of the catalog")
}

fn strings(value: &Value, field: &str) -> Vec<String> {
    value[field]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .map(|item| {
            item.as_str()
                .expect("a catalog list holds names")
                .to_owned()
        })
        .collect()
}

/// A mode as the command line spells it. The catalog names the kind; the
/// periodic ones need a period.
fn spelled(mode: &str) -> String {
    match mode {
        "poll" => "poll@50ms".to_owned(),
        "sample" => "sample@997hz".to_owned(),
        other => other.to_owned(),
    }
}

/// The cheapest target this source declares, and the arguments that ask for it.
///
/// A launched `true` is preferred over a duration: it ends the run the moment
/// the child is reaped, where `--system` has to wait out an interval, and in
/// `count` mode that interval is a second.
fn target_args(entry: &Value) -> (Vec<String>, Vec<String>) {
    let targets = &entry["targets"];
    if targets.get("launch").is_some() {
        (Vec::new(), vec!["--".to_owned(), "true".to_owned()])
    } else if targets.get("system").is_some() {
        (
            vec!["--system".to_owned(), "-d".to_owned(), "120ms".to_owned()],
            Vec::new(),
        )
    } else {
        (
            vec!["--pid".to_owned(), std::process::id().to_string()],
            Vec::new(),
        )
    }
}

fn measure(entry: &Value, mode: &str, emit: &str, format: &str) -> Output {
    let source = entry["id"].as_str().expect("a source has an id").to_owned();
    let (before, after) = target_args(entry);
    let mut args = vec![
        source,
        "--mode".into(),
        spelled(mode),
        "--emit".into(),
        emit.into(),
    ];
    args.extend(before);
    args.extend(["--format".to_owned(), format.to_owned()]);
    args.extend(after);
    omn(&args.iter().map(String::as_str).collect::<Vec<_>>())
}

/// The three answers a source can give before it measures anything, which are
/// the three the brief keeps apart: this build collects it, this build does not
/// implement it, or this machine cannot serve it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Serves {
    Here,
    NoCollector,
    NotHere,
}

/// Asks the build what it can do with a source, rather than reading it off a
/// list written here -- a list would be the thing under test restated.
///
/// The first mode and emit the catalog declares are not always a pair the
/// validator allows (`gpu.engine_busy` declares `count` and `timeline`, and no
/// count can produce a timeline), so this walks pairs until one gets past
/// validation and answers about the backend instead. The pair it found comes
/// back with the answer, because every other test that needs "a request this
/// source actually serves" needs the same search.
fn probe(entry: &Value) -> (Serves, Option<(String, String)>) {
    for mode in strings(entry, "modes") {
        for emit in strings(entry, "emits") {
            let output = measure(entry, &mode, &emit, "json");
            if output.status.success() {
                return (Serves::Here, Some((mode, emit)));
            }
            let complaint = stderr(&output);
            if code(&output) == EXIT_NOT_IMPLEMENTED && complaint.contains("no collector") {
                return (Serves::NoCollector, None);
            }
            if complaint.contains("no viable backend") {
                return (Serves::NotHere, None);
            }
            // Anything else is the validator refusing this pair. Try the next.
        }
    }
    (Serves::NotHere, None)
}

/// A source, what the build can do with it, and a mode and emit it accepts.
type Probed = (Value, Serves, Option<(String, String)>);

/// The probe's answer for every source in the family, asked once.
fn probed() -> &'static Vec<Probed> {
    static PROBED: OnceLock<Vec<Probed>> = OnceLock::new();
    PROBED.get_or_init(|| {
        catalog()
            .into_iter()
            .map(|entry| {
                let (serves, pair) = probe(&entry);
                (entry, serves, pair)
            })
            .collect()
    })
}

/// The catalog offers a set of modes and a set of emits, and the CLI is a
/// projection of the catalog -- so every pair of the two is a promise. This
/// walks all of them for every source this build collects, and requires each
/// pair to be either served or refused **by name**: a pair that resolves and
/// then fails at attach time, or one that produces an output of the wrong
/// shape, is the promise broken in the one place nobody looks.
#[test]
fn every_mode_and_emit_a_collected_source_declares_is_served_or_refused_by_name() {
    if let Err(reason) = device() {
        return eprintln!("skipped: {reason}");
    }
    let _together = shared();
    let mut served = 0;
    let mut refused = 0;
    for (entry, serves, _) in probed() {
        let source = entry["id"].as_str().unwrap();
        if *serves != Serves::Here {
            // Vocabulary, or hardware this machine does not have. Both are
            // class D's subject and neither is a sweep.
            eprintln!("not swept: {source} is {serves:?} on this machine");
            continue;
        }
        for mode in strings(entry, "modes") {
            for emit in strings(entry, "emits") {
                let output = measure(entry, &mode, &emit, "json");
                if output.status.success() {
                    let run: Value =
                        serde_json::from_str(&stdout(&output)).unwrap_or_else(|error| {
                            panic!(
                                "{source} {mode}/{emit} produced output that is not JSON: {error}"
                            )
                        });
                    let produced = run["results"][0]["outputs"][0]["emit"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();
                    assert_eq!(
                        produced, emit,
                        "{source} was asked for `{emit}` and answered with `{produced}`"
                    );
                    served += 1;
                } else {
                    let complaint = stderr(&output);
                    assert_eq!(
                        code(&output),
                        EXIT_REJECTED,
                        "{source} {mode}/{emit} refused with something other than a rejection: \
                         {complaint}"
                    );
                    // The reason has to name the pair, because the pair is what
                    // the reader has to change. "invalid combination" sends
                    // them back to the catalog to guess which half was wrong.
                    assert!(
                        complaint.contains(&mode) && complaint.contains(&emit),
                        "the refusal of {source} {mode}/{emit} names neither: {complaint}"
                    );
                    refused += 1;
                }
            }
        }
        assert!(
            served > 0,
            "{source} declares modes and emits and served none"
        );
    }
    // Five sources, two modes each, three emits each: thirty pairs, of which
    // the validator refuses the five that no mode can produce. A build that
    // serves none of them has no collectors, and this test would be measuring
    // its own skip.
    assert!(
        served >= 20,
        "only {served} pairs were served across the family"
    );
    assert!(
        refused >= 1,
        "no pair was refused, which the validator should do"
    );
}

/// The other half of the sweep: a format either carries an emit or says it
/// cannot. There is no third answer -- a format that approximates an emit it
/// cannot express produces a file that opens in the wrong tool and is believed.
#[test]
fn every_output_format_either_carries_an_emit_or_refuses_it_by_name() {
    if let Err(reason) = device() {
        return eprintln!("skipped: {reason}");
    }
    let _together = shared();
    let entry = catalog()
        .into_iter()
        .find(|entry| entry["id"] == "gpu.temp")
        .expect("gpu.temp is in the catalog");
    let formats = [
        "text",
        "json",
        "yaml",
        "toml",
        "ndjson",
        "chrome",
        "folded",
        "speedscope",
        "pprof",
    ];
    let mut carried = 0;
    for emit in strings(&entry, "emits") {
        for format in formats {
            let output = measure(&entry, "poll", &emit, format);
            if output.status.success() {
                assert!(
                    !stdout(&output).trim().is_empty(),
                    "{format} accepted `{emit}` and wrote nothing"
                );
                if matches!(format, "json" | "chrome" | "speedscope") {
                    serde_json::from_str::<Value>(&stdout(&output)).unwrap_or_else(|error| {
                        panic!("{format} accepted `{emit}` and wrote invalid JSON: {error}")
                    });
                }
                carried += 1;
            } else {
                assert_eq!(code(&output), EXIT_REJECTED, "{}", stderr(&output));
                let complaint = stderr(&output);
                assert!(
                    complaint.contains(format) && complaint.contains(&emit),
                    "the refusal names neither the format nor the emit: {complaint}"
                );
            }
        }
    }
    assert!(
        carried >= 3,
        "at least the structured formats must carry something: {carried}"
    );
}

/// `gpu.engine_busy` is the one source in this family whose samples do not
/// happen when they are timestamped: NVML averages occupancy over a window it
/// chooses and does not report, up to a second wide, and the collector records
/// that as a one-second `ts_err_ns` on every event. No emit the source declares
/// serialises that field -- `raw` is the only one that does, and this source
/// does not declare it -- so a reader of a timeline saw points 200 ms apart
/// with nothing to say they might all be one driver sample.
///
/// The collector now states the error bar as a degradation of the run, which is
/// the one channel every emit and every format that carries a run's notes
/// carries. What it does not do is put the number on each point: that needs a
/// timeline point able to hold its own error bar, which is recorded under
/// "Needs from another stream" in `docs/open-findings.md`.
///
/// `ndjson` is deliberately not in the list. It is a stream of points and
/// carries no run-level object at all, so a degradation has nowhere to go in
/// it -- which is a property of the format rather than of this source.
#[test]
fn the_one_time_error_this_family_records_reaches_every_output_that_can_carry_it() {
    let entry = catalog()
        .into_iter()
        .find(|entry| entry["id"] == "gpu.engine_busy")
        .expect("gpu.engine_busy is in the catalog");
    let emits = strings(&entry, "emits");

    if let Err(reason) = device() {
        return eprintln!("skipped: {reason}");
    }
    let _together = shared();
    for emit in emits {
        let mut carried = 0;
        for format in ["text", "json", "chrome"] {
            // `rate` is reachable on this source only through `count`, and
            // `timeline` only through `poll`, so each emit is asked for in a
            // mode that can produce it rather than skipped where it cannot.
            let mut output = measure(&entry, "poll", &emit, format);
            if !output.status.success() {
                output = measure(&entry, "count", &emit, format);
            }
            if !output.status.success() {
                continue;
            }
            let rendered = stdout(&output);
            assert!(
                rendered.contains("averaged by the driver over a window it chooses"),
                "`{emit}` as `{format}` says nothing about how far its timestamps may be from \
                 the samples they carry:\n{rendered}"
            );
            // And it says how far, not merely that there is a window: an
            // uncertainty without a size is not one.
            assert!(
                rendered.contains("1000 ms"),
                "`{emit}` as `{format}` names no size for the window:\n{rendered}"
            );
            carried += 1;
        }
        // Not every emit reaches every format -- `chrome` carries what it can
        // draw and refuses the rest by name -- but each emit this source
        // declares has to reach at least one.
        assert!(
            carried > 0,
            "`{emit}` was refused by every format, so nothing was checked for it"
        );
    }
}

/// Every session this family ships parses, validates against the catalog, and
/// asks for combinations the catalog offers -- including the one this machine
/// cannot serve, which has to be *valid* even where it is unavailable.
#[test]
fn every_session_in_the_family_validates_against_the_catalog() {
    let mut checked = 0;
    for entry in std::fs::read_dir(format!("{ROOT}/examples")).expect("examples/ is there") {
        let path = entry.expect("a directory entry").path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.starts_with("gpu-") || !name.ends_with(".yml") {
            continue;
        }
        let output = omn(&["validate", "-f", &format!("examples/{name}")]);
        assert!(
            output.status.success(),
            "{name} does not validate: {}",
            stderr(&output)
        );
        checked += 1;
    }
    assert!(checked >= 7, "only {checked} gpu sessions were found");
}

// ---------------------------------------------------------------------------
// Class B -- the backend comparison
// ---------------------------------------------------------------------------

/// This family has no two implemented backends to compare, so the comparison is
/// with the refusal: forcing a backend the build does not implement must name
/// it, and never exit 2 like a request that cannot mean anything.
///
/// The distinction is the whole point of `--backend force:`. A reader who pins
/// `fs:sys:amdgpu` on a laptop needs to know whether they are missing hardware
/// or missing code, and only one of the two is worth waiting for. Where the
/// machine lacks something the backend needs, the hardware is the first answer
/// (exit 1), as the walk gives it; where it lacks nothing, the code is (exit 3)
/// -- the chain's order (`DECISIONS.md`, "one order for unbuilt and missing").
#[test]
fn forcing_a_backend_this_build_does_not_implement_names_it_in_the_chains_order() {
    let _together = shared();
    let mut forced = 0;
    let mut here = 0;
    // What the listing says nobody wrote, among the chains of the sources this
    // machine serves: each is pinned below and each has to exit 3. The number
    // is the build's and the machine's together -- the CI's runner has no GPU
    // and serves `gpu.api` alone, whose one backend is written -- so it is
    // counted from the listing, not assumed.
    let mut unwritten = 0;
    for (entry, serves, pair) in probed() {
        let Some((mode, emit)) = pair else { continue };
        if *serves != Serves::Here {
            continue;
        }
        here += 1;
        unwritten += entry["probes"]
            .as_array()
            .expect("the listing carries each backend's probes")
            .iter()
            .filter(|probes| probes["collector"] == Value::Bool(false))
            .count();
        let source = entry["id"].as_str().unwrap().to_owned();
        let mode = spelled(mode);
        for backend in entry["backends"].as_array().expect("a chain") {
            let id = backend["id"].as_str().expect("a backend has an id");
            let pinned = format!("force:{id}");
            let output = omn(&[
                &source,
                "--mode",
                &mode,
                "--emit",
                emit,
                "--backend",
                &pinned,
                "--",
                "true",
            ]);
            let complaint = stderr(&output);
            // Pinning is never silent: the resolver's choice was bypassed and
            // the run says so whether it then measures or refuses.
            assert!(
                complaint.contains("is pinned")
                    || complaint.contains(&format!("{id} (pinned with --backend force:")),
                "pinning {id} on {source} said nothing about it: {complaint}"
            );
            if output.status.success() {
                continue;
            }
            // What this machine lacks for the pinned backend, as the listing
            // probes it: the machine's gap is answered before the build's.
            let lacks: Vec<&str> = entry["probes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|probes| probes["backend"] == id)
                .flat_map(|probes| probes["requirements"].as_array().into_iter().flatten())
                .filter(|requirement| requirement["probe"]["state"] == "missing")
                .filter_map(|requirement| requirement["needs"].as_str())
                .collect();
            if lacks.is_empty() {
                assert_eq!(
                    code(&output),
                    EXIT_NOT_IMPLEMENTED,
                    "pinning {id} on {source} refused as something other than a missing \
                     collector: {complaint}"
                );
                assert!(
                    complaint.contains("no collector") && complaint.contains(id),
                    "the refusal does not name the backend that is missing: {complaint}"
                );
            } else {
                assert_eq!(
                    code(&output),
                    EXIT_UNAVAILABLE,
                    "pinning {id} on {source}, which lacks {lacks:?} here, is the machine's \
                     refusal before the build's: {complaint}"
                );
                assert!(
                    lacks
                        .iter()
                        .all(|needs| complaint.contains(&format!("{needs}: missing")))
                        && !complaint.contains("no collector"),
                    "the machine's refusal names what it lacks, not the collector: {complaint}"
                );
            }
            forced += 1;
        }
    }
    if here == 0 {
        // A machine with no GPU serves no source of this family, so no chain
        // is walked far enough to reach a backend nobody wrote.
        eprintln!(
            "skipped: this machine serves no gpu source (no NVIDIA driver, no DRM device), so \
             there is no chain to pin a backend of"
        );
        return;
    }
    if unwritten == 0 {
        eprintln!(
            "skipped: the {here} gpu source(s) this machine serves have a collector for every \
             backend of their chains, so there is no backend nobody wrote to pin"
        );
        return;
    }
    assert_eq!(
        forced, unwritten,
        "the listing names {unwritten} backend(s) nobody wrote in the chains this machine \
         serves, and pinning refused {forced} of them, as the machine's gap or the build's"
    );
}

/// A backend name that is in no chain is a different mistake and gets a
/// different answer: the request cannot mean anything, so it is rejected.
#[test]
fn forcing_a_backend_that_is_in_no_chain_is_a_rejection_rather_than_a_gap() {
    let output = omn(&[
        "gpu.temp",
        "--mode",
        "poll@50ms",
        "--emit",
        "raw",
        "--backend",
        "force:vendor:imaginary",
        "--",
        "true",
    ]);
    assert_eq!(code(&output), EXIT_REJECTED, "{}", stderr(&output));
    let complaint = stderr(&output);
    assert!(complaint.contains("vendor:imaginary"), "{complaint}");
    assert!(complaint.contains("gpu.temp"), "{complaint}");
}

/// The one comparison this family does have: two figures out of one read.
///
/// `gpu.mem` reports the process's own video memory and, riding beside it, the
/// whole device's. They are never mixed into one series and they are read from
/// two different places in the driver -- a process list and a memory-info
/// struct -- so when a program takes 256 MiB both must move by 256 MiB. A run
/// where they disagree means something else on the machine allocated inside the
/// window, and the two runs either side of it are not comparable.
#[test]
fn the_process_figure_and_the_device_figure_move_by_the_same_step() {
    if let Err(reason) = device_and_workload() {
        return eprintln!("skipped: {reason}");
    }
    if let Err(reason) = device_to_ourselves() {
        return eprintln!("skipped: {reason}");
    }
    let _alone = exclusive();

    let run = session("gpu-vram-step.yml");
    let events = events(result(&run, "vram"), "raw");
    let held: Vec<i64> = events
        .iter()
        .map(|event| attr(event, "bytes").as_i64().expect("bytes is an integer"))
        .collect();
    let device: Vec<i64> = events
        .iter()
        .map(|event| {
            attr(event, "device_used_bytes")
                .as_i64()
                .expect("the device total rides beside the process figure")
        })
        .collect();

    let rise = last_rise(&held).expect("the workload allocates, so the series rises");
    assert_eq!(
        device[rise] - device[rise - 1],
        held[rise] - held[rise - 1],
        "the process gained {} bytes and the device {}",
        held[rise] - held[rise - 1],
        device[rise] - device[rise - 1]
    );
}

/// The index of the last point at which the series rose.
///
/// The allocation is the workload's last act, so the last rise is it. Earlier
/// rises are the CUDA context arriving in the driver's process list, which
/// happens in two or three steps of its own.
fn last_rise(series: &[i64]) -> Option<usize> {
    (1..series.len()).rev().find(|&i| series[i] > series[i - 1])
}

// ---------------------------------------------------------------------------
// Ground truth -- the number the workload chose
// ---------------------------------------------------------------------------

/// The scenario this family is for: a program takes a quantity of video memory
/// that is written in its own source, and the tool has to find that quantity.
///
/// The check is on the **step**, not the level. A CUDA context reserves around
/// 156 MiB on the hardware this was written against before a single byte is
/// asked for, so the level measures the driver as much as the program. Both
/// plateaus are inside one run, so the reservation is in both and cancels.
#[test]
fn a_process_holds_exactly_the_video_memory_it_allocated() {
    if let Err(reason) = device_and_workload() {
        return eprintln!("skipped: {reason}");
    }
    let _alone = exclusive();

    let run = session("gpu-vram-step.yml");
    let result = result(&run, "vram");
    let held: Vec<i64> = events(result, "raw")
        .iter()
        .map(|event| attr(event, "bytes").as_i64().expect("bytes is an integer"))
        .collect();

    let rise = last_rise(&held).expect("the workload allocates, so the series rises");
    // Both plateaus have to have been observed, or the step is between two
    // moments of a driver still settling rather than between before and after.
    assert!(
        held.iter()
            .filter(|&&value| value == held[rise - 1])
            .count()
            >= 2,
        "the series never settled before the allocation: {held:?}"
    );
    assert!(
        held.iter().filter(|&&value| value == held[rise]).count() >= 2,
        "the series never settled after the allocation: {held:?}"
    );
    assert_eq!(
        held[rise] - held[rise - 1],
        ARENA_BYTES,
        "the workload allocated {ARENA_BYTES} bytes and the series rose by {}",
        held[rise] - held[rise - 1]
    );

    // And the figure is the process's, said so on every event rather than
    // remembered from the command line.
    for event in events(result, "raw") {
        assert_eq!(attr(event, "scope"), "process");
    }
}

/// Defect 9, fixed. A CUDA program under a wrapper -- here a shell that does
/// not `exec` it -- is a child of the launched process, and the driver lists
/// the child. `gpu.mem` read the launched pid alone and reported 0 for a tree
/// holding 412 MiB. It sums the tree now, so the same 256 MiB step the
/// unwrapped workload shows is found under the wrapper, by one process.
#[test]
fn a_program_under_a_wrapper_holds_the_video_memory_its_child_allocated() {
    if let Err(reason) = device_and_workload() {
        return eprintln!("skipped: {reason}");
    }
    let _alone = exclusive();
    let wrapped = format!("python3 tests/gpu_workload.py arena {ARENA_BYTES} 400 700; :");
    let output = omn(&[
        "gpu.mem",
        "--mode",
        "poll@50ms",
        "--emit",
        "raw",
        "--format",
        "json",
        "--",
        "sh",
        "-c",
        &wrapped,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let run: Value = serde_json::from_str(&stdout(&output)).expect("json results");
    let events = events(&run["results"][0], "raw");
    let held: Vec<i64> = events
        .iter()
        .map(|event| attr(event, "bytes").as_i64().expect("bytes is an integer"))
        .collect();
    let rise = last_rise(&held).unwrap_or_else(|| {
        panic!("the wrapped workload allocates, so the series rises: {held:?}")
    });
    assert_eq!(
        held[rise] - held[rise - 1],
        ARENA_BYTES,
        "the child allocated {ARENA_BYTES} bytes and the wrapper's series rose by {}: {held:?}",
        held[rise] - held[rise - 1]
    );
    assert_eq!(
        attr(&events[rise], "processes"),
        1,
        "one process of the tree holds the memory: the child, not the shell"
    );
}

/// A program that never opens a context holds no video memory, and the run says
/// zero rather than saying nothing. "Nothing was measured" and "the measurement
/// is zero" are different claims about a run and a reader cannot tell them
/// apart from an empty series.
#[test]
fn a_program_that_never_touches_the_device_holds_zero_rather_than_nothing() {
    if let Err(reason) = device() {
        return eprintln!("skipped: {reason}");
    }
    let _together = shared();
    let run = session("gpu-vram-of-a-process.yml");
    let result = result(&run, "vram");
    let events = events(result, "raw");
    assert!(
        events.len() >= 4,
        "a 600 ms program polled at 100 ms is read more than four times"
    );
    for event in events {
        assert_eq!(attr(event, "bytes"), 0);
        assert_eq!(attr(event, "held_as"), "none");
        assert_eq!(attr(event, "scope"), "process");
        // The device was not empty. A zero that could be read as "the device
        // held nothing" would be a different, false claim.
        assert!(
            attr(event, "device_used_bytes").as_i64().unwrap() > 0,
            "something is always holding VRAM on a machine with a display"
        );
    }
}

/// The draw in watts, as `--mode count --emit rate` gives it: the energy
/// counter's difference over the window it was read across. Ground truth is
/// the vendor's own tool, sampling the instantaneous draw every 200 ms over
/// the same window from another process. The two are different mechanisms
/// (an energy counter and a sampled draw), so the bar is separation rather
/// than agreement: a factor of two either way, which is what a rate of
/// millijoules, or a running total summed once a second, would each break.
#[test]
fn a_counted_energy_window_divided_by_its_length_is_the_boards_draw() {
    if let Err(reason) = device() {
        return eprintln!("skipped: {reason}");
    }
    if nvidia_smi("power.draw").is_none() {
        return eprintln!("skipped: no nvidia-smi to read the board's draw with");
    }
    let _together = shared();
    let sampling = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let sampler = {
        let sampling = sampling.clone();
        std::thread::spawn(move || {
            let mut draws = Vec::new();
            while sampling.load(std::sync::atomic::Ordering::Relaxed) {
                draws.extend(nvidia_smi("power.draw"));
                std::thread::sleep(Duration::from_millis(200));
            }
            draws
        })
    };
    let output = omn(&[
        "gpu.power",
        "--system",
        "--mode",
        "count",
        "--emit",
        "rate",
        "-d",
        "2s",
        "--format",
        "json",
    ]);
    sampling.store(false, std::sync::atomic::Ordering::Relaxed);
    let draws = sampler.join().expect("the sampler");
    assert!(output.status.success(), "{}", stderr(&output));
    let run: Value = serde_json::from_str(&stdout(&output)).expect("json results");
    let rate = &run["results"][0]["outputs"][0]["series"][0];
    assert_eq!(rate["events"], 1, "a counted window is one reading: {rate}");
    assert_eq!(rate["counted_window"], true, "{rate}");
    let window_s = rate["window_ns"].as_f64().expect("a window") / 1e9;
    assert!(
        (1.9..2.5).contains(&window_s),
        "a two-second window was counted over {window_s} s"
    );
    let watts = rate["per_second"].as_f64().expect("a rate");
    assert!(!draws.is_empty(), "nvidia-smi read nothing");
    let sampled = draws.iter().sum::<f64>() / draws.len() as f64;
    assert!(
        (sampled / 2.0..sampled * 2.0).contains(&watts),
        "the counted window says {watts:.1} W and nvidia-smi sampled {sampled:.1} W on average \
         over {} reads",
        draws.len()
    );
}

/// The pair that makes a device figure mean something: the same four sources,
/// the same window, the same machine, and a resident kernel as the only
/// difference.
///
/// Two properties are checked, and they are the two the workload chose. A
/// kernel was executing at the moment the busy window closed and nothing was at
/// the moment the idle one did, so the occupancy read at the end of each run
/// has to separate. And the board drew power for that kernel, so the energy per
/// second over the busy window has to be a multiple of the idle one's.
///
/// The end of the series rather than its maximum, deliberately: NVML averages
/// occupancy over a window up to a second wide, so the first points of any run
/// still carry whatever the device was doing before it started. That is
/// measured in `docs/scenarios/gpu.md`; here it is the reason for reading the
/// tail.
#[test]
fn a_resident_kernel_separates_from_an_idle_device_on_occupancy_and_on_power() {
    if let Err(reason) = device_and_workload() {
        return eprintln!("skipped: {reason}");
    }
    if let Err(reason) = device_to_ourselves() {
        return eprintln!("skipped: {reason}");
    }
    let _alone = exclusive();

    // The baseline first, and only once the device has stopped coming down
    // from whatever ran before this test.
    settle();
    let idle = session("gpu-idle-baseline.yml");
    // Asked between the two runs and after them as well: a tenant that arrived
    // during the baseline is in the baseline, and the device figures below are
    // the device's, not this workload's. The workload's own process has exited
    // by the time each is asked.
    if let Err(reason) = device_tenants() {
        return eprintln!("skipped: after the idle baseline, {reason}");
    }
    let busy = session("gpu-device-state.yml");
    if let Err(reason) = device_tenants() {
        return eprintln!("skipped: after the resident kernel, {reason}");
    }

    let idle_busy = points(result(&idle, "busy"), "timeline");
    let busy_busy = points(result(&busy, "busy"), "timeline");
    // Separation, not a threshold. The bar was 0.8 and the device reads 0.75
    // when the host is saturated: NVML averages occupancy over a window up to a
    // second wide, and a python process competing with the rest of this suite
    // for a core cannot keep the queue full across all of it. What the workload
    // chose is that a kernel was resident and nothing was, and that is a gap
    // between the two runs rather than a number either of them reaches.
    let busy_tail = *busy_busy.last().unwrap();
    let idle_tail = *idle_busy.last().unwrap();
    assert!(
        idle_tail <= 0.2,
        "nothing was on the device when this window closed and it read {idle_tail:?}"
    );
    assert!(
        busy_tail >= 0.5,
        "a kernel was resident when this window closed and the device read {busy_tail:?}"
    );
    assert!(
        busy_tail >= idle_tail + 0.5,
        "a device running a kernel and a device running nothing have to separate: {busy_tail} \
         against {idle_tail}"
    );

    let idle_watts = energy_rate(&idle);
    let busy_watts = energy_rate(&busy);
    // Twice, and not more, on purpose. The kernel draws around ten times the
    // floor on a quiet machine -- but the floor is not a floor: this is a
    // device figure, the compositor shares the device, and rendering this
    // suite's own output into a terminal window costs sixty watts on the
    // hardware this was written against. `docs/scenarios/gpu.md` has the
    // measurement, and it is why the ratio here is a direction rather than a
    // number.
    assert!(
        busy_watts >= idle_watts * 2.0,
        "the kernel drew {busy_watts:.1} W against a baseline of {idle_watts:.1} W, which \
         is not a separation at all -- either the baseline was taken before the device \
         settled, or something else was using the device throughout"
    );
}

/// Watts over the second half of a run's window.
///
/// The series is an energy counter rebased when the run started, so its slope
/// is power and its last point is the run's energy. The second half rather than
/// the whole: both workloads spend their first few hundred milliseconds
/// starting a Python interpreter and, for the kernel one, compiling a module,
/// and a window that includes that measures the startup as much as the work.
fn energy_rate(run: &Value) -> f64 {
    let result = result(run, "energy");
    let points = output(result, "timeline")["series"][0]["points"]
        .as_array()
        .expect("a timeline carries points");
    assert!(points.len() >= 4, "too few readings to take a slope from");
    let at = |point: &Value| {
        (
            point["offset_ns"].as_f64().expect("an offset"),
            point["value"].as_f64().expect("a value"),
        )
    };
    let (from_ns, from_j) = at(&points[points.len() / 2]);
    let (to_ns, to_j) = at(&points[points.len() - 1]);
    (to_j - from_j) / ((to_ns - from_ns) / 1e9)
}

/// Two roads to the same joules, inside one run.
///
/// `joules` is the device's own energy counter. `watts` is the instantaneous
/// draw at the moment of the read, and integrating those samples over the
/// window is a second estimate of the first. The catalog says in a caveat that
/// the second is not an energy measurement; this is that caveat with a number
/// on it.
///
/// What is asserted is the shape rather than the ratio -- the counter must be
/// monotone, both must be positive, and they must be within a factor of two of
/// each other. The measured ratio is in `docs/scenarios/gpu.md`, because it is
/// a property of this board and this poll interval, not of the tool.
#[test]
fn the_energy_counter_and_the_integral_of_the_sampled_draw_do_not_agree() {
    if let Err(reason) = device_and_workload() {
        return eprintln!("skipped: {reason}");
    }
    let _alone = exclusive();

    let run = session("gpu-energy-two-ways.yml");
    let result = result(&run, "energy");
    let events = events(result, "raw");
    assert!(
        events.len() >= 8,
        "a 1.2 s run polled at 100 ms reads often"
    );

    let readings: Vec<(f64, f64, f64)> = events
        .iter()
        .map(|event| {
            (
                event["ts_ns"].as_f64().expect("a timestamp"),
                attr(event, "joules").as_f64().expect("the energy counter"),
                attr(event, "watts").as_f64().expect("the sampled draw"),
            )
        })
        .collect();

    // An energy counter that goes backwards is not an energy counter.
    for pair in readings.windows(2) {
        assert!(
            pair[1].1 >= pair[0].1,
            "the energy counter fell from {} J to {} J",
            pair[0].1,
            pair[1].1
        );
    }

    let counted = readings.last().unwrap().1 - readings[0].1;
    let integrated: f64 = readings
        .windows(2)
        .map(|pair| (pair[0].2 + pair[1].2) / 2.0 * (pair[1].0 - pair[0].0) / 1e9)
        .sum();
    assert!(
        counted > 0.0 && integrated > 0.0,
        "{counted} J, {integrated} J"
    );
    let ratio = integrated / counted;
    assert!(
        (0.5..=2.0).contains(&ratio),
        "the sampled integral read {integrated:.1} J against a counter delta of \
         {counted:.1} J, a ratio of {ratio:.2}: the two roads have stopped being about \
         the same quantity"
    );
}

/// The clock is the figure this family cannot check against a workload -- it
/// has hysteresis longer than any scenario's window -- so it is checked against
/// the device's own statement of what it can do, and against a second reader.
#[test]
fn the_reported_clock_stays_inside_what_the_device_says_it_can_do() {
    if let Err(reason) = device() {
        return eprintln!("skipped: {reason}");
    }
    // A clock moves between two reads the way none of the other figures do:
    // this card idles at 405 MHz and boosts past 1900 MHz in milliseconds. The
    // two readers have to be looking at the same moment, so nothing else here
    // may be driving the device while they do.
    let _alone = exclusive();

    let before = nvidia_smi("clocks.sm");
    let output = omn(&[
        "gpu.freq", "--system", "--mode", "snapshot", "--emit", "raw", "--format", "json",
    ]);
    let after = nvidia_smi("clocks.sm");
    assert!(output.status.success(), "{}", stderr(&output));
    let run: Value = serde_json::from_str(&stdout(&output)).expect("json results");
    let event = &run["results"][0]["outputs"][0]["events"][0];

    let hz = attr(event, "hz").as_f64().unwrap();
    let memory_hz = attr(event, "memory_hz").as_f64().unwrap();
    assert!(hz > 0.0, "a device that answers is running at some clock");

    let Some(max_sm) = nvidia_smi("clocks.max.sm") else {
        return eprintln!("skipped the bound: nvidia-smi is not on this machine");
    };
    let max_mem = nvidia_smi("clocks.max.mem").expect("the same tool answers both");
    assert!(
        hz <= max_sm * 1e6,
        "the shader clock read {} MHz against a maximum of {max_sm} MHz",
        hz / 1e6
    );
    // The domains ride beside each other rather than in one series, and mixing
    // them would show here: a memory clock above the shader maximum.
    assert!(
        memory_hz <= max_mem * 1e6,
        "the memory clock read {} MHz against a maximum of {max_mem} MHz",
        memory_hz / 1e6
    );

    // Bracketed rather than compared to one reading: this tool's read happened
    // between the vendor's two, so its answer has to lie in the interval those
    // two span, widened by one boost bin for the moments in between.
    let (before, after) = (before.expect("bracketed"), after.expect("bracketed"));
    let ours = hz / 1e6;
    let bin = 120.0;
    let low = before.min(after) - bin;
    let high = before.max(after) + bin;
    assert!(
        (low..=high).contains(&ours),
        "this tool read {ours} MHz between two vendor readings of {before} and {after} MHz"
    );
}

// ---------------------------------------------------------------------------
// Class C -- the loss path
// ---------------------------------------------------------------------------

/// The way this family loses data.
///
/// There is no ring to overflow and no kernel to throttle these collectors:
/// every reading is a library call, and the only thing that can go wrong with
/// a library call in a loop is that it takes longer than the loop's period. Ask
/// `gpu.mem` -- the most expensive of them, because it walks the driver's
/// process list on every read -- for a reading every 200 microseconds, and
/// almost every tick starts late.
///
/// What is checked is that the run says so. A collector that falls behind and
/// reports a clean series has produced a timeline whose x axis is a fiction,
/// and nothing downstream can tell.
#[test]
fn a_poll_faster_than_the_driver_answers_counts_every_late_tick_and_says_so() {
    if let Err(reason) = device() {
        return eprintln!("skipped: {reason}");
    }
    let _together = shared();
    let run = session("gpu-poll-too-fast.yml");
    let result = result(&run, "too-fast");
    let losses = &result["losses"];
    let late = losses["late_ticks"]
        .as_u64()
        .expect("late ticks are counted");
    let seen = result["events_seen"].as_u64().expect("events are counted");
    assert!(
        late * 2 > seen,
        "a 200 us poll of a driver that answers in 250 us should be late on most ticks, \
         and was late on {late} of {seen}"
    );
    // Nothing else went wrong: the run was late, not broken. A run that
    // conflated the two would be telling the reader to look for a driver
    // problem that is not there.
    //
    // `dropped` is exactly zero because a poll has no ring to lose events
    // from. `failed_reads` is required to be negligible rather than zero: this
    // test holds the shared lock, so it runs beside the rest of the suite and
    // beside whatever else is on the board, and NVML does occasionally refuse
    // a read on a contended device. One refusal in several hundred does not
    // make the lateness a driver fault, and asserting zero asserts the machine
    // rather than the tool -- which is finding 11's whole subject.
    assert_eq!(losses["dropped"], 0);
    let failed = losses["failed_reads"]
        .as_u64()
        .expect("failed reads are counted");
    assert!(
        failed * 20 < late,
        "the run should be late rather than broken, and it reported {failed} failed reads \
         against {late} late ticks"
    );

    // And the count reaches a person, not only a JSON field. The text output is
    // what somebody running this at a terminal sees, and the loss leads it.
    let text = omn(&["-f", "examples/gpu-poll-too-fast.yml"]);
    assert!(text.status.success(), "{}", stderr(&text));
    assert!(
        stdout(&text).contains("LOSS:") && stdout(&text).contains("late ticks"),
        "the loss did not reach the terminal output: {}",
        stdout(&text)
    );
}

/// Defect 11, fixed. A `gpu.temp` poll costs its program its readings, a
/// tenth of a millisecond each, and not the clock state read beside them:
/// the end-of-run clock event reasons (17 to 32 ms of cpu on a board without
/// persistence mode) were read inside the window and made a two-read run on
/// a 150 ms program cost 36 to 45 ms, 22 to 27 % against its `low` band. They
/// are read after the window now, and that cpu is still reported, after it.
#[test]
fn a_temperature_poll_costs_its_program_its_readings_not_the_clock_state() {
    if let Err(reason) = device() {
        return eprintln!("skipped: {reason}");
    }
    let _together = shared();
    let output = omn(&[
        "gpu.temp",
        "--mode",
        "poll@100ms",
        "--emit",
        "summary",
        "--format",
        "json",
        "--",
        "python3",
        "-c",
        "import time\nt = time.process_time()\nwhile time.process_time() - t < 0.15: pass",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let run: Value = serde_json::from_str(&stdout(&output)).expect("json results");
    let overhead = &run["results"][0]["overhead"];
    let inside = overhead["collector_cpu_ns"].as_u64().expect("the cost is read");
    assert!(
        inside < 15_000_000,
        "two or three temperature readings cost the program {:.1} ms of cpu: {overhead}",
        inside as f64 / 1e6
    );
    // Not hidden: the end-of-run read is in the account after the window.
    assert!(
        overhead["after_window_cpu_ns"].as_u64().unwrap_or(0) > 0,
        "{overhead}"
    );
    // And the clock state still reaches the run.
    let notes = run["results"][0]["notes"].to_string();
    assert!(notes.contains("clock event reasons"), "{notes}");
}

/// The same source at a sane interval keeps up, which is what makes the
/// previous test a measurement rather than a collector that always complains.
///
/// A handful of late ticks is allowed and not a failure: a 100 ms poll on a
/// machine running the rest of this suite in parallel will occasionally be
/// scheduled late, and the run counting that is the behaviour under test, not
/// a defect. What has to hold is the contrast -- almost every tick late at
/// 200 us, almost none at 100 ms.
#[test]
fn the_same_source_polled_at_a_sane_interval_keeps_up_with_itself() {
    if let Err(reason) = device() {
        return eprintln!("skipped: {reason}");
    }
    let _together = shared();
    let run = session("gpu-vram-of-a-process.yml");
    let result = result(&run, "vram");
    let losses = &result["losses"];
    let late = losses["late_ticks"]
        .as_u64()
        .expect("late ticks are counted");
    let seen = result["events_seen"].as_u64().expect("events are counted");
    assert!(
        late * 4 < seen,
        "a 100 ms poll of a driver that answers in under a millisecond was late on \
         {late} of {seen} ticks"
    );
    // Zero, and it is a real assertion rather than an optimistic one: the two
    // calls NVML needs to list a device's processes race, and losing that race
    // used to be reported here as a failed read about once per suite run. The
    // collector retries with the size the library asks for, so a device whose
    // process list is merely changing no longer costs a reading. A failure that
    // survives that is a driver refusing, which is what this line is for.
    assert_eq!(losses["failed_reads"], 0, "{losses}");
    assert_eq!(losses["dropped"], 0, "{losses}");
}

/// The driver refreshes the energy counter at its own pace -- every 100 ms on
/// the board this was written on -- and a poll faster than that reads the
/// same driver sample again. The run counts those readings, marks each event
/// `driver_sample: repeat` or `new`, and says how often the counter actually
/// moved.
///
/// The ground truth is the pace itself, held as a separation rather than a
/// period: a 10 ms poll repeats on most readings, a 300 ms one on few. And the
/// note's count is the count of events marked as repeats, so the sentence a
/// person reads and the file a program reads say the same thing.
///
/// The fast poll also shows the counter's own defect (`docs/scenarios/gpu.md`,
/// defect 7): read that often, it under-counts, and the run says so with the
/// draw read beside it. This test holds `exclusive()`, because a fast poll of
/// the counter disturbs every other reader of the board.
#[test]
fn a_power_poll_faster_than_the_driver_refreshes_counts_its_repeats_and_says_so() {
    if let Err(reason) = device() {
        return eprintln!("skipped: {reason}");
    }
    let _alone = exclusive();
    let poll = |mode: &str| -> Value {
        let output = omn(&[
            "gpu.power",
            "--mode",
            mode,
            "--emit",
            "raw",
            "--format",
            "json",
            "--",
            "sleep",
            "1.5",
        ]);
        assert!(output.status.success(), "{}", stderr(&output));
        serde_json::from_str(&stdout(&output)).expect("json")
    };
    let repeats = |run: &Value| -> (usize, usize, Vec<String>) {
        let result = &run["results"][0];
        let events = events(result, "raw");
        let repeated = events
            .iter()
            .filter(|event| attr(event, "driver_sample") == "repeat")
            .count();
        let degradations = result["degradations"]
            .as_array()
            .map(|all| {
                all.iter()
                    .map(|note| note.as_str().unwrap_or_default().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        (repeated, events.len(), degradations)
    };

    let (fast_repeats, fast_reads, fast_notes) = repeats(&poll("poll@10ms"));
    let (slow_repeats, slow_reads, slow_notes) = repeats(&poll("poll@300ms"));
    let fast = fast_repeats as f64 / fast_reads as f64;
    let slow = slow_repeats as f64 / slow_reads.max(1) as f64;
    assert!(
        fast > 0.5 && slow < fast / 2.0,
        "polled every 10 ms the counter repeated on {fast_repeats} of {fast_reads} readings, \
         every 300 ms on {slow_repeats} of {slow_reads}"
    );

    note_agrees_with_events("energy counter", fast_repeats, fast_reads, &fast_notes);
    assert!(
        fast_notes
            .iter()
            .any(|note| note.contains("energy counter implies")),
        "a 10 ms poll starves the counter and the run did not say so: {fast_notes:?}"
    );
    // The slow run's note, or its absence, agrees with its own events too.
    note_agrees_with_events("energy counter", slow_repeats, slow_reads, &slow_notes);
}

/// The repeat note a run carries says exactly as many repeats as its events
/// are marked with, and a run none of whose events repeated says none.
fn note_agrees_with_events(figure: &str, repeats: usize, reads: usize, notes: &[String]) {
    let claim = format!("found the {figure} where the previous reading left it");
    let said: Vec<&String> = notes.iter().filter(|note| note.contains(&claim)).collect();
    if repeats == 0 {
        assert!(
            said.is_empty(),
            "a run with no repeat claims some: {notes:?}"
        );
    } else {
        assert_eq!(said.len(), 1, "{notes:?}");
        assert!(
            said[0].starts_with(&format!("vendor:nvml: {repeats} of {reads} readings")),
            "the note and the events disagree ({repeats} of {reads}): {}",
            said[0]
        );
    }
}

/// The occupancy's repeats are told by the driver's own sample timestamp, not
/// by the value: a board that is steadily idle reads 0.02 sample after sample,
/// and those are new samples. The ground truth is the driver's pace -- about a
/// second between samples on the board this was written on -- held as a
/// separation: a 200 ms poll reads most samples several times over and says
/// so, and a 1.5 s poll, slower than any refresh measured, reads a new sample
/// every time and claims no repeat, however many of its values are equal.
///
/// Before the timestamp, the repeat was the value seen twice, and an idle
/// board at the one-second default reported a third of its readings as stale
/// and a refresh period of 1.6 s.
#[test]
fn an_occupancy_repeat_is_a_driver_sample_seen_twice_not_a_value_seen_twice() {
    if let Err(reason) = device() {
        return eprintln!("skipped: {reason}");
    }
    let _together = shared();
    // The points, as (offset, value), and the run's degradations.
    let poll = |interval: &str, seconds: &str| -> (Vec<(u64, f64)>, Vec<String>) {
        let ran = omn(&[
            "gpu.engine_busy",
            "--mode",
            &format!("poll@{interval}"),
            "--emit",
            "timeline",
            "--format",
            "json",
            "--",
            "sleep",
            seconds,
        ]);
        assert!(ran.status.success(), "{}", stderr(&ran));
        let run: Value = serde_json::from_str(&stdout(&ran)).expect("json");
        let result = &run["results"][0];
        let points = output(result, "timeline")["series"][0]["points"]
            .as_array()
            .expect("a timeline carries points")
            .iter()
            .map(|point| {
                (
                    point["offset_ns"].as_u64().expect("a point has an offset"),
                    point["value"].as_f64().expect("a point is a number"),
                )
            })
            .collect();
        let notes = result["degradations"]
            .as_array()
            .map(|all| {
                all.iter()
                    .map(|note| note.as_str().unwrap_or_default().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        (points, notes)
    };
    // How many repeats a run's note claims; none when it has no note.
    let claimed = |notes: &[String]| -> usize {
        notes
            .iter()
            .find(|note| note.contains("found the occupancy where the previous reading left it"))
            .map(|said| {
                said.trim_start_matches("vendor:nvml: ")
                    .split(' ')
                    .next()
                    .and_then(|count| count.parse().ok())
                    .unwrap_or_else(|| panic!("the note does not start with its count: {said}"))
            })
            .unwrap_or(0)
    };

    let (fast, fast_notes) = poll("200ms", "3");
    let fast_repeats = claimed(&fast_notes);
    assert!(
        fast_repeats * 2 > fast.len(),
        "polled every 200 ms, only {fast_repeats} of {} readings repeated a sample the driver \
         refreshes about once a second: {fast_notes:?}",
        fast.len()
    );
    note_agrees_with_events("occupancy", fast_repeats, fast.len(), &fast_notes);

    // 4.4 s, so that the reading the run takes when its target exits lands
    // 1.4 s after the last scheduled one rather than a moment after it, which
    // would be a real repeat. The bound is still taken from the offsets: a
    // repeat may be claimed only across a gap shorter than any refresh seen
    // could explain, and here there is none.
    let (slow, slow_notes) = poll("1500ms", "4.4");
    let short_gaps = slow
        .windows(2)
        .filter(|pair| pair[1].0 - pair[0].0 < 1_200_000_000)
        .count();
    let equal = slow
        .windows(2)
        .filter(|pair| pair[0].1 == pair[1].1)
        .count();
    assert!(
        claimed(&slow_notes) <= short_gaps,
        "polled every 1.5 s, slower than the driver refreshes, {} readings ({equal} equal to the \
         one before, {short_gaps} under 1.2 s after it) were claimed as repeats: {slow_notes:?}",
        claimed(&slow_notes)
    );
    eprintln!(
        "polled every 1.5 s: {} readings, {equal} equal to the one before, {} claimed as repeats",
        slow.len(),
        claimed(&slow_notes)
    );
}

/// The other way a run of this family ends early: the process it was following
/// exits. That is not a loss and is not counted as one -- it is the end of the
/// window -- but the run has to say which process went, because a series that
/// stops before the requested duration and does not say why looks like a
/// collector that died.
#[test]
fn a_target_that_exits_before_the_duration_ends_the_run_and_names_it() {
    if let Err(reason) = device() {
        return eprintln!("skipped: {reason}");
    }
    let _together = shared();
    // Orphaned on purpose. A child of this test process becomes a zombie when
    // it exits and nothing reaps it until the test does -- and a zombie still
    // has a `/proc/<pid>`, which is the entry the collector checks. Handing the
    // sleep to a shell that exits immediately reparents it to init, which does
    // reap it, so the process really is gone. The other case is a defect and is
    // recorded in `docs/scenarios/gpu.md`.
    let spawned = Command::new("sh")
        .arg("-c")
        // The redirections are not tidiness: `output()` reads the shell's pipe
        // until every holder of its write end is gone, and the background sleep
        // inherits that end. Without them this call blocks for the whole second
        // and the target is already dead before the run starts.
        .arg("sleep 1 </dev/null >/dev/null 2>&1 & echo $!")
        .output()
        .expect("sh should run");
    let pid = String::from_utf8_lossy(&spawned.stdout).trim().to_owned();
    assert!(
        !pid.is_empty(),
        "the shell did not report the pid it started"
    );
    let output = omn(&[
        "gpu.mem",
        "--pid",
        &pid,
        "--mode",
        "poll@100ms",
        "--emit",
        "summary",
        "-d",
        "10s",
        "--format",
        "json",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let run: Value = serde_json::from_str(&stdout(&output)).expect("json results");
    let result = &run["results"][0];

    let stopped = result["stopped_because"].as_str().unwrap_or("");
    assert!(
        stopped.contains(&pid) && stopped.contains("exited"),
        "the run ended early and said `{stopped}`"
    );
    let window = window_seconds(result);
    assert!(
        (0.8..2.0).contains(&window),
        "the target slept one second and the window was {window:.2} s"
    );
    assert_eq!(
        result["losses"]["failed_reads"], 0,
        "a target that exits is the end of the window, not a failed read"
    );
}

// ---------------------------------------------------------------------------
// Class D -- the refusal
// ---------------------------------------------------------------------------

/// A request that cannot mean anything, several ways, each refused with the
/// reason that is actually the reason.
///
/// These are the messages a reader acts on. One that names the wrong axis costs
/// more than no message at all: it sends somebody to change the part of the
/// request that was right.
#[test]
fn a_request_that_cannot_mean_anything_is_rejected_and_names_what_cannot() {
    let cases: [(&[&str], &[&str]); 7] = [
        // A clock belongs to the device and there is no per-process share of
        // it. Answering with the device's number under a pid would be the worst
        // bug this tool could have, so the refusal names the target and says
        // what this source does take.
        (
            &[
                "gpu.freq",
                "--pid",
                "1",
                "--mode",
                "poll@100ms",
                "--emit",
                "raw",
            ],
            &["pid", "system"],
        ),
        // A temperature is not a stream of events, so there is nothing to trace.
        (
            &[
                "gpu.temp", "--mode", "trace", "--emit", "summary", "--", "true",
            ],
            &["gpu.temp", "trace"],
        ),
        // A histogram of a device temperature is not declared and not served.
        (
            &[
                "gpu.temp",
                "--mode",
                "poll@100ms",
                "--emit",
                "hist",
                "--",
                "true",
            ],
            &["gpu.temp", "hist"],
        ),
        // A single read cannot be a series.
        (
            &[
                "gpu.temp", "--mode", "snapshot", "--emit", "timeline", "--", "true",
            ],
            &["snapshot", "timeline"],
        ),
        // A device figure carries no task, so it cannot be joined on one, and
        // saying so now is cheaper than a join that matches nothing later.
        (
            &[
                "gpu.freq",
                "--keys",
                "task",
                "--mode",
                "poll@100ms",
                "--emit",
                "raw",
                "--",
                "true",
            ],
            &["gpu.freq", "task"],
        ),
        // A filter over a field this source has no idea about is false
        // everywhere, which would empty the series and look like a quiet run.
        (
            &[
                "gpu.temp",
                "--mode",
                "poll@100ms",
                "--emit",
                "raw",
                "--filter",
                "kernel == 'x'",
                "--",
                "true",
            ],
            &["gpu.temp", "kernel"],
        ),
        // Video memory is not measured per network interface.
        (
            &[
                "gpu.mem",
                "--iface",
                "lo",
                "--mode",
                "poll@100ms",
                "--emit",
                "raw",
                "-d",
                "1s",
            ],
            &["gpu.mem", "iface"],
        ),
    ];
    for (args, named) in cases {
        let output = omn(args);
        let complaint = stderr(&output);
        assert_eq!(
            code(&output),
            EXIT_REJECTED,
            "`{}` was not rejected: {complaint}",
            args.join(" ")
        );
        for word in named {
            assert!(
                complaint.contains(word),
                "the refusal of `{}` does not name `{word}`: {complaint}",
                args.join(" ")
            );
        }
    }
}

/// A request the catalog describes and this build does not implement is a
/// different answer, with a different exit code, and it names the backend that
/// is missing rather than the hardware.
///
/// Two of this family's nine sources are in that state since the ninth wave
/// gave `gpu.api` a collector, and both -- `gpu.kernel` and `gpu.counters` --
/// would be *available* here: the vendor libraries load. Nothing about the
/// machine is wrong; the code is not written.
#[test]
fn a_source_this_build_does_not_implement_exits_three_and_names_the_backend() {
    // `gpu.kernel` left this list in the nineteenth wave, when its CUPTI
    // collector was written (`tests/gpu_kernel.rs`), and `gpu.counters`
    // through CUPTI with it (`tests/gpu_counters.rs`): what is left is the
    // chain's AMD backend, pinned.
    for (source, mode, emit, backend) in [("gpu.counters", "trace", "raw", "vendor:rocprof")] {
        let pinned = format!("force:{backend}");
        let output = omn(&[
            source,
            "--mode",
            mode,
            "--emit",
            emit,
            "--backend",
            &pinned,
            "--",
            "true",
        ]);
        // Every backend of the chain is probed before the build is asked for a
        // collector, so a machine without the vendor's library answers first.
        if stderr(&output).contains("has no viable backend on this machine") {
            eprintln!("skipped: {}", stderr(&output));
            continue;
        }
        assert_eq!(
            code(&output),
            EXIT_NOT_IMPLEMENTED,
            "{source} answered with something other than a missing collector: {}",
            stderr(&output)
        );
        let complaint = stderr(&output);
        assert!(complaint.contains("no collector"), "{complaint}");
        assert!(
            complaint.contains(backend),
            "the refusal does not name the backend it would have needed: {complaint}"
        );
        assert!(
            complaint.contains("valid"),
            "a request nobody implemented is still a valid request, and the message \
             should distinguish that from a request that is wrong: {complaint}"
        );
    }
}

/// The third answer: the request is valid, this build implements it, and this
/// machine cannot serve it.
///
/// `gpu.job` reads the DRM scheduler's tracepoints, which the NVIDIA
/// proprietary driver does not expose. What matters is that the message names
/// the two tracepoints that are absent -- a refusal that sent the reader to
/// `CAP_BPF`, or to a sysctl, would cost more than silence.
///
/// The exit code is the part that is wrong, and it is recorded rather than
/// asserted away: see "Defects found" in `docs/scenarios/gpu.md`. This test
/// pins today's answer so that fixing it fails here and takes the entry out.
#[test]
fn a_source_this_machine_cannot_serve_names_the_tracepoints_that_are_absent() {
    let output = omn(&["-f", "examples/gpu-job-latency.yml"]);
    if output.status.success() {
        // A machine with amdgpu, i915 or xe: the session measures, which is why
        // it is kept in examples/ on machines that cannot run it.
        return;
    }
    let complaint = stderr(&output);
    assert!(
        complaint.contains("no viable backend"),
        "gpu.job failed for a reason other than the missing scheduler: {complaint}"
    );
    assert!(
        complaint.contains("gpu_scheduler:drm_run_job"),
        "the refusal must name the tracepoint that is absent: {complaint}"
    );
    assert!(
        complaint.contains("this machine that cannot serve it"),
        "the refusal must separate a machine's limit from a bad request: {complaint}"
    );
    assert_eq!(
        code(&output),
        EXIT_UNAVAILABLE,
        "a machine that cannot serve a valid request has its own exit code: it used to \
         share 2 with a request that cannot mean anything, which is defect 4 of \
         docs/scenarios/gpu.md, and a script that wants to skip what the machine cannot \
         do and stop on what the user got wrong could not tell them apart"
    );
}

/// The listing is a projection of the catalog crossed with this machine, and it
/// has to answer both questions separately: what is here, and what is written.
#[test]
fn the_listing_separates_what_this_machine_has_from_what_this_build_collects() {
    let output = omn(&["list", "gpu"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let listing = stdout(&output);
    for entry in catalog() {
        let id = entry["id"].as_str().unwrap();
        assert!(listing.contains(id), "{id} is missing from `omn list gpu`");
    }
    assert!(
        listing.contains("HERE is what this machine offers"),
        "the listing has to say that availability and implementation are two questions"
    );
}

// ---------------------------------------------------------------------------
// The defects, pinned
// ---------------------------------------------------------------------------
//
// Four more tests, and none of them asserts that the tool is right. Each pins
// behaviour that `docs/scenarios/gpu.md` records under "Defects found", so that
// the day one is fixed the suite says so and points at the entry to remove. A
// scenario suite that quietly worked around three bugs has measured nothing;
// one that leaves them unrecorded has measured them and then forgotten.

/// Defect 2, fixed. `gpu.engine_busy` is the only source in this family
/// declaring `count`, and a `count` names no cadence of its own -- so the run
/// used to fall back to the default poll interval of one second, notice the
/// launched program's exit at the next tick, and report a window that was the
/// program's life rounded up to the next second. Every emit that divides by a
/// window divided by that one.
///
/// The collector now supplies a tick of ten milliseconds in this mode while
/// keeping its reads a second apart, which is where the driver refreshes them.
/// The ground truth is the program: `sleep 0.3` lives 300 ms, and the window
/// the run reports has to be that and not a second.
///
/// Measured through `summary` rather than `rate`, because defect 3 took `rate`
/// off this entry in the same wave: the sum of two occupancies is not a
/// quantity. The window is what this scenario is about and it is reported
/// whatever the emit.
#[test]
fn a_count_mode_run_reports_the_window_of_the_program_it_watched() {
    if let Err(reason) = device() {
        return eprintln!("skipped: {reason}");
    }
    let _together = shared();
    let counted = omn(&[
        "gpu.engine_busy",
        "--mode",
        "count",
        "--emit",
        "summary",
        "--format",
        "json",
        "--",
        "sleep",
        "0.3",
    ]);
    assert!(counted.status.success(), "{}", stderr(&counted));
    let run: Value = serde_json::from_str(&stdout(&counted)).expect("json results");
    let result = &run["results"][0];
    let window = window_seconds(result);
    assert!(
        (0.28..0.45).contains(&window),
        "`sleep 0.3` lived 300 ms and the run reports a window of {window:.3} s; a window near \
         one second is the old behaviour, where the exit was noticed at the next tick"
    );
    assert!(
        result["stopped_because"]
            .as_str()
            .unwrap_or("")
            .contains("exited"),
        "the run has to end because its program did, not because a tick came round"
    );
    // And the program it watched really did live for about 0.3 s, so the gap
    // is the runner's tick and not the workload's.
    assert!(
        window <= 2.0,
        "a 300 ms program reported a {window:.3} s window, which is more than one \
         missed tick explains"
    );
}

/// Defect 3, fixed. `rate` sums the value attribute and divides by the window,
/// which is a quantity for a count, for bytes, for nanoseconds and for joules --
/// everything else in the catalog that declares it. `gpu.engine_busy` declared
/// it over a `ratio`, and the sum of two occupancies is not a quantity: the
/// device was three per cent busy throughout and `0.060/s` was the only number
/// offered for that.
///
/// The emit is out of the entry, and the check is kept as a fact about the
/// whole catalog rather than about this family: this was the only pairing of
/// its kind in a hundred sources, and the value of the scenario is that a
/// second one cannot arrive quietly. `src/validate.rs::check_catalog` enforces
/// the same rule over every unit that does not add up.
#[test]
fn no_source_in_the_catalog_declares_a_rate_over_a_ratio() {
    let output = omn(&["list", "--format", "json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let everything: Vec<Value> =
        serde_json::from_str(&stdout(&output)).expect("the whole catalog, as data");
    let offenders: Vec<String> = everything
        .iter()
        .filter(|entry| {
            entry["unit"] == "ratio" && strings(entry, "emits").contains(&"rate".into())
        })
        .map(|entry| entry["id"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        offenders.is_empty(),
        "a rate of a ratio is a quantity of nothing, and {offenders:?} declare one; \
         docs/scenarios/gpu.md records why there should be none"
    );

    // And the refusal a reader meets, with the caveat that says why.
    let refused = omn(&[
        "gpu.engine_busy",
        "--system",
        "--mode",
        "count",
        "--emit",
        "rate",
        "--duration",
        "200ms",
    ]);
    assert_eq!(code(&refused), 2, "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("`gpu.engine_busy` cannot emit `rate`"),
        "the refusal should name the source and the emit: {}",
        stderr(&refused)
    );
}

/// The loss report carries one reason for the late ticks, not one per tick.
///
/// `Losses::reasons` is documented as the reasons with how often each occurred.
/// The late-tick reason used to embed the nanoseconds that tick took, so every
/// occurrence was its own reason with a count of one and the table was a
/// transcript -- 1024 distinct reasons and 80 KB of JSON for a 300 ms run, which
/// is defect 5 of `docs/scenarios/gpu.md`. Same session, other answer.
#[test]
fn the_loss_report_carries_one_reason_for_all_the_late_ticks() {
    if let Err(reason) = device() {
        return eprintln!("skipped: {reason}");
    }
    let _together = shared();
    let run = session("gpu-poll-too-fast.yml");
    let losses = &result(&run, "too-fast")["losses"];
    let late = losses["late_ticks"]
        .as_u64()
        .expect("late ticks are counted");
    let reasons = losses["reasons"]
        .as_object()
        .expect("the reasons ride with the counts");
    assert!(late > 20, "this scenario needs a run that was late a lot");
    assert_eq!(
        reasons.len(),
        1,
        "{late} late ticks have to be one reason, not {}: {losses}",
        reasons.len()
    );
    assert_eq!(
        reasons.values().next().and_then(Value::as_u64),
        Some(late),
        "and the reason has to carry the count it explains: {losses}"
    );
    // The nanoseconds are not gone, they are beside the count: a magnitude
    // that aggregates rather than a key that cannot.
    let overrun = &losses["overrun"];
    assert!(
        overrun["worst_ns"].as_u64().unwrap() > overrun["tick_ns"].as_u64().unwrap(),
        "the worst overrun is what says how far off the cadence the series is: {losses}"
    );
}

/// Defect 6, fixed. `gpu.mem` ends a run when its process goes, and used to ask
/// `/proc/<pid>` whether it had. A zombie has one, so a child nobody had reaped
/// yet was followed to the end of the requested duration and reported as a
/// program holding no video memory -- which is the series of zeros the
/// collector's own comment says it exists to prevent.
///
/// The state letter in `/proc/<pid>/stat` is what tells the two apart, and the
/// ground truth is the child itself: it lives 200 ms, nothing reaps it, and the
/// run must stop there rather than at the 800 ms it was given.
#[test]
fn a_target_that_exits_without_being_reaped_ends_the_run() {
    if let Err(reason) = device() {
        return eprintln!("skipped: {reason}");
    }
    let _together = shared();
    // Not reaped on purpose: this handle is dropped without a `wait`, which is
    // exactly the state a supervisor or a test harness leaves a child in.
    let mut child = Command::new("sleep")
        .arg("0.2")
        .spawn()
        .expect("sleep should start");
    let pid = child.id().to_string();
    let output = omn(&[
        "gpu.mem",
        "--pid",
        &pid,
        "--mode",
        "poll@100ms",
        "--emit",
        "summary",
        "-d",
        "800ms",
        "--format",
        "json",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let run: Value = serde_json::from_str(&stdout(&output)).expect("json results");
    let result = &run["results"][0];

    let stopped = result["stopped_because"].as_str().unwrap_or("");
    assert!(
        stopped.contains("reaped"),
        "the run ended for `{stopped}`: an unreaped child has exited, and the run has to say \
         so rather than follow a process that is gone"
    );
    let window = window_seconds(result);
    assert!(
        window < 0.6,
        "the target lived 200 ms and the run observed {window:.3} s of the 800 ms it was \
         given, so it went on polling a process that had exited"
    );
    // And what it did report is the process while it existed, not a tail of
    // zeros for one that did not.
    let series = &result["outputs"][0]["series"][0];
    assert!(
        series["n"].as_u64().unwrap_or(0) <= 3,
        "a 200 ms process polled every 100 ms is two or three readings, and this run has \
         {}",
        series["n"]
    );
    let _ = child.wait();
}

// ===========================================================================
// The sixth wave: `gpu.job`
//
// The only collector in this wave that cannot be exercised on the machine it
// was written on, and the only one in this build on the classic tracepoint
// path. What can be checked here is checked here; what cannot is said in those
// words in `docs/families/gpu.md` rather than asserted away.
// ===========================================================================

/// The answer this machine gives has to stay "the machine", now that the build
/// has the collector.
///
/// Before the sixth wave `gpu.job` had no collector, and a machine without a
/// DRM scheduler and a build without a collector would have been two reasons
/// for the same silence. Now only one of them is true here, and the exit code
/// has to say which: exit 1 sends a reader to their hardware and exit 3 to
/// their build, and there is nothing to find in the second.
///
/// This is the class D refusal for this source, and it is the only class that
/// can be run here at all.
#[test]
fn the_collector_exists_and_this_machine_is_still_the_thing_that_cannot_serve_it() {
    let listed = stdout(&omn(&["list", "gpu.job"]));
    assert!(
        listed.contains("collector written"),
        "`gpu.job` has a collector in this build and the listing does not say so:\n{listed}"
    );

    let output = omn(&["-f", "examples/gpu-job-latency.yml"]);
    if output.status.success() {
        // A machine with amdgpu, i915 or xe, and a readable tracefs: the
        // session measures, which is the whole reason the collector exists.
        eprintln!("this machine has a DRM scheduler: the session measured rather than refusing");
        return;
    }
    let complaint = stderr(&output);
    assert_eq!(
        code(&output),
        EXIT_UNAVAILABLE,
        "the collector is written, so a refusal here is the machine's and not the build's; \
         exit {EXIT_NOT_IMPLEMENTED} would send a reader looking for a newer build that would \
         not help:\n{complaint}"
    );
    assert!(
        !complaint.contains("no collector is written"),
        "`gpu.job` has a collector and the refusal still says it does not:\n{complaint}"
    );
    assert!(
        complaint.contains("gpu_scheduler:drm_run_job"),
        "the refusal must name what is absent:\n{complaint}"
    );
}

/// What this machine cannot check, said with the numbers that show why.
///
/// A scenario that states its limit is worth more than one that hides it, and
/// this source's limit is total: there is no DRM scheduler under the NVIDIA
/// proprietary driver, so neither tracepoint exists, and tracefs is unreadable
/// by the uid the tool runs as, so even the field layout the collector reads at
/// startup cannot be looked at. Both are printed rather than described, because
/// the next person to read this will be on a different machine.
#[test]
fn what_this_machine_cannot_check_about_gpu_job_is_named_with_its_numbers() {
    let tracefs = ["/sys/kernel/tracing", "/sys/kernel/debug/tracing"]
        .iter()
        .map(std::path::PathBuf::from)
        .find(|root| root.join("events").is_dir());
    let readable = tracefs
        .as_ref()
        .is_some_and(|root| std::fs::read_dir(root.join("events/gpu_scheduler")).is_ok());
    let drivers: Vec<String> = std::fs::read_dir("/sys/class/drm")
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();

    eprintln!(
        "gpu.job on this machine: tracefs at {:?}, its `gpu_scheduler` events {}readable, \
         /sys/class/drm holds {:?}",
        tracefs.as_ref().map(|root| root.display().to_string()),
        if readable { "" } else { "not " },
        drivers
    );

    if readable {
        // A machine that can see the events can answer the one question this
        // test exists for: whether its kernel publishes the fence's identity
        // or only the pointer this build refuses to key on.
        let format = tracefs
            .expect("a root, since its events were readable")
            .join("events/gpu_scheduler/drm_sched_process_job/format");
        match std::fs::read_to_string(&format) {
            Ok(text) => eprintln!(
                "this kernel's `drm_sched_process_job` publishes the fence's identity: {}",
                text.contains("fence_context")
            ),
            Err(error) => eprintln!("{} cannot be read: {error}", format.display()),
        }
        return;
    }

    // The refusal is the only behaviour this machine can exercise, and it has
    // to send the reader to the right thing.
    let complaint = stderr(&omn(&[
        "gpu.job", "--system", "-m", "trace", "-e", "hist", "-d", "50ms",
    ]));
    assert!(
        complaint.contains("tracefs") || complaint.contains("gpu_scheduler"),
        "this machine cannot serve `gpu.job` and the refusal names neither the tracepoints nor \
         the filesystem they would be read through:\n{complaint}"
    );
    // Whether the capabilities are missing too is this binary's state, not the
    // machine's: `make` grants them and a plain `cargo build` or the CI's
    // unprivileged cell does not. The listing says which, and the refusal is
    // held to it -- naming them when `bpf` is missing is right, and naming them
    // when it is available sends the reader to a setcap that will not help.
    let listing = stdout(&omn(&["list", "gpu.job"]));
    let bpf_available = listing.lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next() == Some("needs")
            && words.next() == Some("bpf")
            && words.next() == Some("available")
    });
    eprintln!(
        "gpu.job: `bpf` is {} for this binary",
        if bpf_available {
            "available"
        } else {
            "missing"
        }
    );
    if bpf_available {
        assert!(
            !complaint.contains("CAP_BPF"),
            "the eBPF capabilities are not what is missing here, and a refusal that names them \
             sends the reader to a setcap that will not help:\n{complaint}"
        );
    } else {
        assert!(
            complaint.contains("CAP_BPF"),
            "this binary cannot load a program, and the refusal does not say so:\n{complaint}"
        );
    }
}

// ---------------------------------------------------------------------------
// The ninth wave: `gpu.api`, through the preload shim
//
// One C program calls the CUDA driver API or Vulkan through the dynamic
// linker a number of times its command line chose, and times every call
// itself. The shim is inside the call the program timed, so each of the
// tool's durations is nested in the program's: the counts are asserted equal,
// and each total is asserted no larger than the program's.
//
// This machine's proprietary NVIDIA driver makes both reachable headless --
// which the brief did not expect -- so neither is skipped here. Where a
// machine has no driver, no loader or no compiler, each test skips with what
// the build or the fixture said.
// ---------------------------------------------------------------------------

/// Launches and synchronizes in `examples/gpu-api-cuda.yml`.
const CUDA_LAUNCHES: u64 = 20;

/// Submissions in `examples/gpu-api-vulkan.yml`.
const VULKAN_SUBMITS: u64 = 100;

const GPU_FIXTURE_SOURCE: &str = r#"
/* BEGIN FIXTURE -- omn's GPU API fixture. It calls the CUDA driver API and
 * Vulkan through the dynamic linker, the way a program linked with `-lcuda`
 * and `-lvulkan` does, a number of times the command line chose, and times
 * every call itself with the clock the tool uses. It reports on stderr,
 * because stdout is where `omn` writes.
 *
 *   gpu cuda <launches> <spin iterations>   a kernel launched and waited for, each time
 *   gpu vulkan <submits>                    an empty submission and a fence wait, each time
 *
 * The CUDA prototypes are declared here rather than taken from `cuda.h`, so
 * the fixture needs the driver and not the toolkit. The Vulkan ones come from
 * the loader's headers. */
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static unsigned long long now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (unsigned long long)t.tv_sec * 1000000000ull + (unsigned long long)t.tv_nsec;
}

struct tally { const char *api; long calls; unsigned long long total_ns, max_ns; };

static void count(struct tally *tally, unsigned long long began) {
    unsigned long long spent = now_ns() - began;
    tally->calls++;
    tally->total_ns += spent;
    if (spent > tally->max_ns)
        tally->max_ns = spent;
}

static void say(const struct tally *tally) {
    fprintf(stderr, "fixture: api=%s calls=%ld total_ns=%llu max_ns=%llu\n", tally->api,
            tally->calls, tally->total_ns, tally->max_ns);
}

#ifdef WITH_CUDA
typedef int CUresult;
typedef int CUdevice;
typedef void *CUcontext;
typedef void *CUmodule;
typedef void *CUfunction;
typedef unsigned long long CUdeviceptr;
CUresult cuInit(unsigned);
CUresult cuDeviceGet(CUdevice *, int);
CUresult cuCtxCreate_v2(CUcontext *, unsigned, CUdevice);
CUresult cuCtxDestroy_v2(CUcontext);
CUresult cuModuleLoadData(CUmodule *, const void *);
CUresult cuModuleGetFunction(CUfunction *, CUmodule, const char *);
CUresult cuMemAlloc_v2(CUdeviceptr *, size_t);
CUresult cuMemFree_v2(CUdeviceptr);
CUresult cuMemcpyDtoH_v2(void *, CUdeviceptr, size_t);
CUresult cuLaunchKernel(CUfunction, unsigned, unsigned, unsigned, unsigned, unsigned, unsigned,
                        unsigned, void *, void **, void **);
CUresult cuCtxSynchronize(void);

/* The spin kernel of tests/gpu_workload.py: slow on purpose, and storing its
 * accumulator so the compiler cannot delete the loop. */
static const char SPIN_PTX[] =
    ".version 6.0\n.target sm_50\n.address_size 64\n"
    ".visible .entry spin(.param .u64 p_out, .param .u64 p_iters)\n{\n"
    "  .reg .u64 %rd<8>;\n  .reg .pred %p1;\n"
    "  ld.param.u64 %rd1, [p_out];\n  ld.param.u64 %rd2, [p_iters];\n"
    "  mov.u64 %rd3, 0;\n  mov.u64 %rd4, 0;\n"
    "LOOP:\n  add.u64 %rd4, %rd4, %rd3;\n  add.u64 %rd3, %rd3, 1;\n"
    "  setp.lt.u64 %p1, %rd3, %rd2;\n  @%p1 bra LOOP;\n"
    "  st.global.u64 [%rd1], %rd4;\n  ret;\n}\n";

static int check(const char *what, CUresult code) {
    if (code != 0) {
        fprintf(stderr, "fixture: unavailable=%s code=%d\n", what, code);
        exit(3);
    }
    return 0;
}

static int cuda(long launches, unsigned long long iterations) {
    CUdevice device;
    CUcontext context;
    CUmodule module;
    CUfunction spin;
    CUdeviceptr out;
    check("cuInit", cuInit(0));
    check("cuDeviceGet", cuDeviceGet(&device, 0));
    check("cuCtxCreate", cuCtxCreate_v2(&context, 0, device));
    check("cuModuleLoadData", cuModuleLoadData(&module, SPIN_PTX));
    check("cuModuleGetFunction", cuModuleGetFunction(&spin, module, "spin"));

    struct tally alloc = {"cuMemAlloc", 0, 0, 0}, launch = {"cuLaunchKernel", 0, 0, 0},
                 sync = {"cuCtxSynchronize", 0, 0, 0}, copy = {"cuMemcpyDtoH", 0, 0, 0},
                 release = {"cuMemFree", 0, 0, 0};
    unsigned long long began = now_ns();
    check("cuMemAlloc", cuMemAlloc_v2(&out, sizeof(unsigned long long)));
    count(&alloc, began);
    void *params[] = {&out, &iterations};
    for (long i = 0; i < launches; i++) {
        began = now_ns();
        check("cuLaunchKernel", cuLaunchKernel(spin, 1, 1, 1, 32, 1, 1, 0, NULL, params, NULL));
        count(&launch, began);
        began = now_ns();
        check("cuCtxSynchronize", cuCtxSynchronize());
        count(&sync, began);
    }
    unsigned long long result = 0;
    began = now_ns();
    check("cuMemcpyDtoH", cuMemcpyDtoH_v2(&result, out, sizeof result));
    count(&copy, began);
    began = now_ns();
    check("cuMemFree", cuMemFree_v2(out));
    count(&release, began);
    cuCtxDestroy_v2(context);
    say(&alloc);
    say(&launch);
    say(&sync);
    say(&copy);
    say(&release);
    return 0;
}
#endif

#ifdef WITH_VULKAN
#include <vulkan/vulkan.h>

static void vk_check(const char *what, VkResult code) {
    if (code != VK_SUCCESS) {
        fprintf(stderr, "fixture: unavailable=%s code=%d\n", what, (int)code);
        exit(3);
    }
}

static int vulkan(long submits) {
    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                             .apiVersion = VK_API_VERSION_1_1};
    VkInstanceCreateInfo instance_info = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                          .pApplicationInfo = &app};
    VkInstance instance;
    vk_check("vkCreateInstance", vkCreateInstance(&instance_info, NULL, &instance));
    uint32_t devices = 1;
    VkPhysicalDevice physical;
    VkResult listed = vkEnumeratePhysicalDevices(instance, &devices, &physical);
    if ((listed != VK_SUCCESS && listed != VK_INCOMPLETE) || devices == 0) {
        fprintf(stderr, "fixture: unavailable=vkEnumeratePhysicalDevices code=%d\n", (int)listed);
        return 3;
    }
    VkPhysicalDeviceProperties properties;
    vkGetPhysicalDeviceProperties(physical, &properties);
    fprintf(stderr, "fixture: device=%s\n", properties.deviceName);

    uint32_t families = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(physical, &families, NULL);
    VkQueueFamilyProperties family_properties[16];
    if (families > 16)
        families = 16;
    vkGetPhysicalDeviceQueueFamilyProperties(physical, &families, family_properties);
    uint32_t family = 0;
    while (family < families && !(family_properties[family].queueFlags & VK_QUEUE_COMPUTE_BIT))
        family++;
    if (family == families) {
        fprintf(stderr, "fixture: unavailable=compute-queue code=0\n");
        return 3;
    }
    float priority = 1.0f;
    VkDeviceQueueCreateInfo queue_info = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                          .queueFamilyIndex = family,
                                          .queueCount = 1,
                                          .pQueuePriorities = &priority};
    VkDeviceCreateInfo device_info = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                                      .queueCreateInfoCount = 1,
                                      .pQueueCreateInfos = &queue_info};
    VkDevice device;
    vk_check("vkCreateDevice", vkCreateDevice(physical, &device_info, NULL, &device));
    VkQueue queue;
    vkGetDeviceQueue(device, family, 0, &queue);
    VkFenceCreateInfo fence_info = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    VkFence fence;
    vk_check("vkCreateFence", vkCreateFence(device, &fence_info, NULL, &fence));

    struct tally submit = {"vkQueueSubmit", 0, 0, 0}, wait = {"vkWaitForFences", 0, 0, 0},
                 idle = {"vkQueueWaitIdle", 0, 0, 0};
    for (long i = 0; i < submits; i++) {
        /* An empty submission still goes to the queue and signals the fence:
         * what is timed is the host's side of submitting, which is what the
         * source measures. */
        unsigned long long began = now_ns();
        vk_check("vkQueueSubmit", vkQueueSubmit(queue, 0, NULL, fence));
        count(&submit, began);
        began = now_ns();
        vk_check("vkWaitForFences", vkWaitForFences(device, 1, &fence, VK_TRUE, UINT64_MAX));
        count(&wait, began);
        vk_check("vkResetFences", vkResetFences(device, 1, &fence));
    }
    unsigned long long began = now_ns();
    vk_check("vkQueueWaitIdle", vkQueueWaitIdle(queue));
    count(&idle, began);
    vkDestroyFence(device, fence, NULL);
    vkDestroyDevice(device, NULL);
    vkDestroyInstance(instance, NULL);
    say(&submit);
    say(&wait);
    say(&idle);
    return 0;
}
#endif

int main(int argc, char **argv) {
#ifdef WITH_CUDA
    if (argc == 4 && strcmp(argv[1], "cuda") == 0)
        return cuda(atol(argv[2]), strtoull(argv[3], NULL, 10));
#endif
#ifdef WITH_VULKAN
    if (argc == 3 && strcmp(argv[1], "vulkan") == 0)
        return vulkan(atol(argv[2]));
#endif
    fprintf(stderr, "usage: %s cuda <launches> <spin iterations> | vulkan <submits>\n", argv[0]);
    return 2;
}
/* END FIXTURE */
"#;

/// Builds one flavour of the fixture once per test process, or says why it
/// cannot: `cuda` against `libcuda.so.1`, `vulkan` against the loader.
fn gpu_fixture(flavour: &'static str) -> Result<std::path::PathBuf, String> {
    static CUDA: OnceLock<Result<std::path::PathBuf, String>> = OnceLock::new();
    static VULKAN: OnceLock<Result<std::path::PathBuf, String>> = OnceLock::new();
    let (cell, define, library) = match flavour {
        "cuda" => (&CUDA, "-DWITH_CUDA", "-l:libcuda.so.1"),
        _ => (&VULKAN, "-DWITH_VULKAN", "-lvulkan"),
    };
    cell.get_or_init(|| {
        let dir = std::path::Path::new(ROOT).join("target/omn-shim-fixture");
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        let path = dir.join(format!("gpu-{flavour}"));
        // Written and built under names of this process's own, and renamed:
        // the two flavours of one test process never race on one file, and a
        // half-written binary is never at the path the examples launch.
        let source = dir.join(format!("gpu-{flavour}.{}.c", std::process::id()));
        let partial = dir.join(format!("gpu-{flavour}.{}", std::process::id()));
        std::fs::write(&source, GPU_FIXTURE_SOURCE).map_err(|error| error.to_string())?;
        let built = Command::new("cc")
            .args(["-O1", define, "-o"])
            .arg(&partial)
            .arg(&source)
            .arg(library)
            .output();
        let _ = std::fs::remove_file(&source);
        let built = built
            .map_err(|error| format!("no C compiler (`cc`) to build the fixture with: {error}"))?;
        if !built.status.success() {
            return Err(format!(
                "the {flavour} fixture did not build, most likely because this machine has no \
                 {library} to link against:\n{}",
                String::from_utf8_lossy(&built.stderr)
            ));
        }
        std::fs::rename(&partial, &path).map_err(|error| error.to_string())?;
        Ok(path)
    })
    .clone()
}

/// Every `fixture: key=value` line the fixture printed on stderr.
fn gpu_fixture_said(output: &Output) -> Vec<std::collections::BTreeMap<String, String>> {
    stderr(output)
        .lines()
        .filter_map(|line| line.strip_prefix("fixture: "))
        .map(|line| {
            line.split_whitespace()
                .filter_map(|pair| pair.split_once('='))
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect()
        })
        .collect()
}

/// The fixture's own count and total for one entry point.
fn fixture_calls(said: &[std::collections::BTreeMap<String, String>], api: &str) -> (u64, u64) {
    let line = said
        .iter()
        .find(|line| line.get("api").map(String::as_str) == Some(api))
        .unwrap_or_else(|| panic!("the fixture never reported `{api}`: {said:?}"));
    (
        line["calls"].parse().expect("a count"),
        line["total_ns"].parse().expect("a total"),
    )
}

/// Runs a session whose fixture may find no usable device, and returns it --
/// unless the fixture said this machine could not serve it, in which case the
/// caller is told what the fixture was refused.
fn gpu_api_session(file: &str) -> Result<(Output, Value), String> {
    let output = omn(&["-f", &format!("examples/{file}"), "--format", "json"]);
    let said = gpu_fixture_said(&output);
    if let Some(refused) = said.iter().find(|line| line.contains_key("unavailable")) {
        return Err(format!(
            "the fixture could not use this machine's GPU: `{}` returned {}",
            refused["unavailable"], refused["code"]
        ));
    }
    assert!(
        output.status.success(),
        "{file} failed: {}",
        stderr(&output)
    );
    let run: Value = serde_json::from_str(&stdout(&output)).expect("a json document");
    Ok((output, run))
}

/// Every `raw` event of the first result.
fn raw_api_events(run: &Value) -> Vec<Value> {
    run["results"][0]["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .filter(|output| output["emit"] == "raw")
        .flat_map(|output| output["events"].as_array().expect("events").clone())
        .collect()
}

fn event_attr(event: &Value, name: &str) -> Value {
    event["attrs"]
        .as_array()
        .expect("attributes")
        .iter()
        .find(|attr| attr["name"] == name)
        .map(|attr| attr["value"].clone())
        .unwrap_or_else(|| panic!("an event without `{name}`: {event}"))
}

/// Every call the run reported, as (entry point, latency) pairs.
fn api_calls(run: &Value) -> Vec<(String, u64)> {
    raw_api_events(run)
        .iter()
        .map(|event| {
            (
                event_attr(event, "api")
                    .as_str()
                    .expect("a name")
                    .to_owned(),
                event_attr(event, "latency_ns")
                    .as_u64()
                    .expect("a duration"),
            )
        })
        .collect()
}

/// Asserts one entry point against the fixture -- the count exactly, the total
/// nested inside the fixture's own -- and returns the tool's count and total.
fn matches_the_fixture(
    calls: &[(String, u64)],
    said: &[std::collections::BTreeMap<String, String>],
    api: &str,
) -> (u64, u64) {
    let durations: Vec<u64> = calls
        .iter()
        .filter(|(name, _)| name == api)
        .map(|(_, ns)| *ns)
        .collect();
    let (count, total) = (durations.len() as u64, durations.iter().sum::<u64>());
    let (expected, program_total) = fixture_calls(said, api);
    assert_eq!(
        count, expected,
        "the program called {api} {expected} times and the run reports {count}"
    );
    assert!(
        total <= program_total,
        "the shim times inside the call the program timed, so {api}'s {total} ns cannot exceed \
         the program's {program_total} ns"
    );
    (count, total)
}

/// The CUDA driver API, called through the dynamic linker a known number of
/// times.
///
/// Ground truth is the program's own count of each call, which must match
/// exactly, and its own timing of each call, which must enclose the tool's.
/// And the separation that is this source's reason to exist: a launch returns
/// once the work is queued, and the kernel's time is paid by the synchronize
/// that waits for it.
#[test]
fn every_cuda_call_the_program_made_is_one_event_nested_in_its_own_timing() {
    if let Err(reason) = gpu_fixture("cuda") {
        return eprintln!("skipped: {reason}");
    }
    let _alone = exclusive();
    let (output, run) = match gpu_api_session("gpu-api-cuda.yml") {
        Ok(ran) => ran,
        Err(reason) => return eprintln!("skipped: {reason}"),
    };
    let said = gpu_fixture_said(&output);
    let calls = api_calls(&run);
    let (launches, launched_ns) = matches_the_fixture(&calls, &said, "cuLaunchKernel");
    let (syncs, synced_ns) = matches_the_fixture(&calls, &said, "cuCtxSynchronize");
    for api in ["cuMemAlloc", "cuMemcpyDtoH", "cuMemFree"] {
        matches_the_fixture(&calls, &said, api);
    }
    assert_eq!((launches, syncs), (CUDA_LAUNCHES, CUDA_LAUNCHES));
    assert_eq!(
        calls.len() as u64,
        2 * CUDA_LAUNCHES + 3,
        "the program made {} calls into interposed entry points and nothing else, and the run \
         reports {}: the driver calling its own exported symbols would look like this",
        2 * CUDA_LAUNCHES + 3,
        calls.len()
    );
    let (mean_launch, mean_sync) = (launched_ns / launches, synced_ns / syncs);
    eprintln!("cuLaunchKernel {mean_launch} ns on average, cuCtxSynchronize {mean_sync} ns");
    assert!(
        mean_sync > mean_launch.saturating_mul(10),
        "a launch is queued and returns, and the synchronize waits for the kernel; the run \
         reports {mean_launch} ns against {mean_sync} ns, which is not that"
    );
}

/// Vulkan, through the loader's exported entry points, headless.
#[test]
fn every_vulkan_submission_and_wait_is_one_event_nested_in_its_own_timing() {
    if let Err(reason) = gpu_fixture("vulkan") {
        return eprintln!("skipped: {reason}");
    }
    let _alone = exclusive();
    let (output, run) = match gpu_api_session("gpu-api-vulkan.yml") {
        Ok(ran) => ran,
        Err(reason) => return eprintln!("skipped: {reason}"),
    };
    let said = gpu_fixture_said(&output);
    let calls = api_calls(&run);
    let (submits, _) = matches_the_fixture(&calls, &said, "vkQueueSubmit");
    let (waits, _) = matches_the_fixture(&calls, &said, "vkWaitForFences");
    let (idles, _) = matches_the_fixture(&calls, &said, "vkQueueWaitIdle");
    assert_eq!((submits, waits, idles), (VULKAN_SUBMITS, VULKAN_SUBMITS, 1));
    assert_eq!(calls.len() as u64, 2 * VULKAN_SUBMITS + 1, "{calls:?}");
    assert!(
        raw_api_events(&run)
            .iter()
            .all(|event| event_attr(event, "family") == "vulkan"),
        "every call here is a Vulkan call"
    );
}

/// A program that drives the GPU through `ctypes` goes around the shim, and
/// the run says so by name rather than reporting an idle GPU.
///
/// The workload is the one every other scenario in this file uses, and
/// `gpu.engine_busy` sees its kernel: the device was busy, and this source
/// cannot see why. That is the most common shape of a real program, not an
/// edge case.
#[test]
fn a_program_that_fetched_its_own_driver_pointers_is_told_why_it_reported_nothing() {
    if let Err(reason) = workload() {
        return eprintln!("skipped: {reason}");
    }
    let _alone = exclusive();
    let output = omn(&["-f", "examples/gpu-api-unreachable.yml", "--format", "json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let run: Value = serde_json::from_str(&stdout(&output)).expect("a json document");
    assert_eq!(
        run["results"][0]["events_seen"].as_u64(),
        Some(0),
        "ctypes looks symbols up on the library's own handle, which never reaches a preloaded \
         one; a run that saw calls is reaching the shim some other way"
    );
    let everything = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        everything.contains("cuGetProcAddress") && everything.contains("ctypes"),
        "an empty result must say which ways around the shim there are:\n{everything}"
    );
}

/// `gpu.api` on a process already running is refused by the catalog, which
/// declares only a launch: a preloaded library is given at exec.
#[test]
fn gpu_api_on_a_running_process_is_rejected_for_its_target() {
    let output = omn(&[
        "gpu.api",
        "--mode",
        "trace",
        "--emit",
        "summary",
        "--pid",
        &std::process::id().to_string(),
        "-d",
        "50ms",
    ]);
    let complaint = stderr(&output);
    assert_eq!(code(&output), EXIT_REJECTED, "{complaint}");
    assert!(
        complaint.contains("pid") && complaint.contains("launch"),
        "the refusal must name the target it refused and the one it accepts: {complaint}"
    );
}
