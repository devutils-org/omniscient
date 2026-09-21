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
    PROBE
        .get_or_init(|| {
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
        })
        .clone()
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
/// with the refusal: forcing a backend the build does not implement must exit 3
/// and name it, not exit 1 like a machine that cannot serve the request and not
/// exit 2 like a request that cannot mean anything.
///
/// The distinction is the whole point of `--backend force:`. A reader who pins
/// `fs:sys:amdgpu` on a laptop needs to know whether they are missing hardware
/// or missing code, and only one of the two is worth waiting for.
#[test]
fn forcing_a_backend_this_build_does_not_implement_exits_three_and_names_it() {
    let _together = shared();
    let mut forced = 0;
    for (entry, serves, pair) in probed() {
        let Some((mode, emit)) = pair else { continue };
        if *serves != Serves::Here {
            continue;
        }
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
                complaint.contains("is pinned"),
                "pinning {id} on {source} said nothing about it: {complaint}"
            );
            if output.status.success() {
                continue;
            }
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
            forced += 1;
        }
    }
    assert!(
        forced >= 5,
        "every collected source in this family declares a backend nobody wrote: {forced}"
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
    let busy = session("gpu-device-state.yml");

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
/// Three of this family's nine sources are in that state, and two of them --
/// `gpu.kernel` and `gpu.counters` -- would be *available* here: the vendor
/// libraries load. Nothing about the machine is wrong; the code is not written.
#[test]
fn a_source_this_build_does_not_implement_exits_three_and_names_the_backend() {
    for (source, mode, emit, backend) in [
        ("gpu.kernel", "trace", "summary", "vendor:cupti"),
        ("gpu.api", "trace", "summary", "preload"),
        ("gpu.counters", "snapshot", "summary", "vendor:ncu"),
    ] {
        let output = omn(&[source, "--mode", mode, "--emit", emit, "--", "true"]);
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
    assert!(
        !complaint.contains("CAP_BPF"),
        "the eBPF capabilities are not what is missing here, and a refusal that names them \
         sends the reader to a setcap that will not help:\n{complaint}"
    );
}
