//! The `power` family, through the surfaces a user meets.
//!
//! The collectors are checked against the kernel in their own unit tests: a
//! synthetic `/sys` for the machines this one is not, and `/proc/stat` and the
//! thermal zones for the parts it can be held against here. What is checked
//! here is the rest of the promise -- that the catalog entries are truthful,
//! that what cannot be measured is refused with the reason, and that this
//! family's particular failure mode is impossible: reporting a quiet zero where
//! nothing was measured.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use omn::Catalog;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// A request was refused because it cannot mean what it asks for.
const EXIT_REJECTED: i32 = 2;

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

const SOURCES: [&str; 5] = [
    "power.energy",
    "power.freq",
    "power.cstate",
    "power.temp",
    "power.throttle",
];

#[test]
fn every_source_of_this_family_has_a_collector_in_this_build() {
    // The source, not the backend: `power.energy` now also collects through
    // the `power` performance unit, which is the backend the resolver prefers
    // because it needs no root. Pinning the test to one backend id would have
    // made adding a better one look like a regression.
    let implemented = omn::collect::implemented().join(", ");
    for source in SOURCES {
        assert!(
            implemented.contains(&format!("{source} (")),
            "`{source}` has no collector: {implemented}"
        );
    }
}

#[test]
fn every_power_fallback_states_what_it_degrades() {
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    for entry in catalog.family("power") {
        for backend in entry.backends.iter().skip(1) {
            assert!(
                backend.degraded.is_some() || backend.equivalent,
                "`{}` falls back to `{}` without saying what that costs",
                entry.id,
                backend.id
            );
        }
    }
}

#[test]
fn backend_ids_are_unique_within_each_power_chain() {
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    for entry in catalog.family("power") {
        let mut seen = Vec::new();
        for backend in &entry.backends {
            assert!(
                !seen.contains(&backend.id),
                "`{}` lists `{}` twice",
                entry.id,
                backend.id
            );
            seen.push(backend.id.clone());
        }
    }
}

/// This family's numbers are all easy to misread -- a mean frequency across
/// cores, a temperature that is not the processor's, residency credited late --
/// so every entry has to say what its number is not.
#[test]
fn every_source_says_what_its_number_is_not() {
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    for source in SOURCES {
        let entry = catalog
            .get(&source.parse().expect("a source id"))
            .unwrap_or_else(|| panic!("`{source}` is in the catalog"));
        assert!(
            !entry.caveats.is_empty(),
            "`{source}` states no caveat, so nothing warns the reader what it is not"
        );
    }
}

/// RAPL measures a package. A per-process figure would be the package's energy
/// multiplied by that process's share of cycles, which is an estimate, and this
/// tool does not present an estimate as a measurement.
#[test]
fn energy_per_process_is_refused_rather_than_estimated() {
    let refused = run(&["power.energy", "--pid", "1", "-m", "poll", "-e", "timeline"]);
    assert_eq!(refused.status.code(), Some(EXIT_REJECTED));
    let message = stderr(&refused);
    assert!(message.contains("scoped to the package"), "{message}");
}

#[test]
fn a_mode_a_source_cannot_serve_is_refused_with_what_it_does() {
    let refused = run(&["power.temp", "--system", "-m", "trace", "-e", "raw"]);
    assert_eq!(refused.status.code(), Some(EXIT_REJECTED));
    let message = stderr(&refused);
    assert!(
        message.contains("cannot be observed in `trace` mode"),
        "{message}"
    );
    assert!(message.contains("poll, snapshot"), "{message}");
}

/// The failure this family exists to prevent, at the level of the whole tool:
/// on a machine that cannot see throttling, asking for it must produce an
/// explanation and a non-zero exit, never a series of zeroes.
#[test]
fn throttling_that_cannot_be_seen_is_never_reported_as_none() {
    let exposed = Path::new("/sys/devices/system/cpu/cpu0/thermal_throttle").exists();
    let attempt = run(&[
        "power.throttle",
        "--system",
        "-m",
        "poll@200ms",
        "-e",
        "timeline",
        "-d",
        "600ms",
    ]);
    if exposed {
        assert!(
            attempt.status.success(),
            "this machine exposes the counters, so the run should work: {}",
            stderr(&attempt)
        );
        return;
    }
    assert_ne!(attempt.status.code(), Some(0));
    let message = stderr(&attempt);
    assert!(
        message.contains("thermal_throttle"),
        "the refusal must name what is missing: {message}"
    );
    assert!(
        !stdout(&attempt).contains("throttles=0"),
        "a machine that cannot see throttling must not report none"
    );
}

/// The listing is the catalog seen through this machine, and it must not
/// promise what the collector then refuses.
#[test]
fn the_listing_agrees_with_what_this_machine_exposes() {
    // Per backend, not per source: a source is usable when *any* of its chain
    // is, and `power.throttle` also has an eBPF backend that a machine with
    // readable tracefs can serve. Asserting on the source would make gaining a
    // privilege look like the listing lying.
    let detail = stdout(&run(&["list", "power.throttle"]));
    let exposed = Path::new("/sys/devices/system/cpu/cpu0/thermal_throttle").exists();
    let sysfs_line = detail
        .lines()
        .find(|line| line.contains("thermal_throttle"))
        .expect("the sysfs requirement is listed");
    assert_eq!(
        sysfs_line.contains("available"),
        exposed,
        "the listing and the filesystem disagree about {}: {sysfs_line}",
        "/sys/devices/system/cpu/cpu0/thermal_throttle"
    );
}

#[test]
fn a_dumped_power_request_is_a_session_file_that_validates() {
    let dumped = run(&[
        "power.freq",
        "--system",
        "-m",
        "poll@500ms",
        "-e",
        "timeline",
        "-d",
        "5s",
        "--dump-request",
        "--format",
        "yaml",
    ]);
    let session = stdout(&dumped);
    assert!(session.contains("power.freq"), "{session}");

    let mut child = Command::new(BINARY)
        .args(["validate", "-f", "-", "--input-format", "yaml"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(session.as_bytes())
        .expect("writing the session");
    let validated = child.wait_with_output().expect("omn should finish");
    assert!(
        validated.status.success(),
        "a dumped request must validate: {}",
        stderr(&validated)
    );
}

#[test]
fn the_examples_this_family_ships_validate() {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut checked = 0;
    for entry in std::fs::read_dir(examples).expect("examples/ exists") {
        let path = entry.expect("a directory entry").path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.starts_with("power-") {
            continue;
        }
        let validated = run(&["validate", "-f", &path.to_string_lossy()]);
        assert!(
            validated.status.success(),
            "{name} does not validate: {}",
            stderr(&validated)
        );
        checked += 1;
    }
    assert!(checked > 0, "this family ships no example");
}

/// Whether a source can be served here, asked of the tool itself rather than
/// guessed from the filesystem a second time.
///
/// A backend whose requirements no probe covers still runs -- it is reported as
/// assumed rather than checked -- so only "no backend is usable" means there is
/// nothing to exercise. Reading this as "usable through" would have skipped
/// `power.temp` on the machine this was written on, which is exactly the kind
/// of test that passes by not running.
fn usable(source: &str) -> bool {
    !stdout(&run(&["list", source])).contains("no backend is usable")
}

/// The family end to end: resolve, attach, poll, aggregate, render. The unit
/// tests hold the arithmetic against a synthetic `/sys`; this holds the wiring,
/// which nothing else does.
#[test]
fn every_source_this_machine_can_serve_runs_and_reports_no_loss() {
    let mut ran = 0;
    for source in ["power.temp", "power.freq", "power.cstate"] {
        if !usable(source) {
            continue;
        }
        let attempt = run(&[
            source,
            "--system",
            "-m",
            "poll@100ms",
            "-e",
            "summary",
            "-d",
            "400ms",
        ]);
        let out = stdout(&attempt);
        assert!(attempt.status.success(), "{source}: {}", stderr(&attempt));
        assert!(out.contains("via fs:sys"), "{source}: {out}");
        assert!(
            out.contains("no loss"),
            "{source}: reading sysfs on an idle machine loses nothing: {out}"
        );
        let events: usize = out
            .lines()
            .find_map(|line| line.split_whitespace().next()?.parse().ok())
            .unwrap_or_else(|| panic!("{source} says how many events it saw: {out}"));
        assert!(events >= 2, "{source} saw only {events} events in 400 ms");
        ran += 1;
    }
    assert!(
        ran > 0,
        "this machine served none of the three sysfs sources, which no Linux does"
    );
}

/// The distinction the source exists for has to survive all the way to the
/// output: a number that does not say whether it is the effective or the
/// requested frequency cannot be compared with anything.
#[test]
fn a_frequency_run_says_which_frequency_it_measured() {
    if !usable("power.freq") {
        return;
    }
    let attempt = run(&[
        "power.freq",
        "--system",
        "-m",
        "poll@100ms",
        "-e",
        "raw",
        "-d",
        "250ms",
    ]);
    assert!(attempt.status.success(), "{}", stderr(&attempt));
    let out = stdout(&attempt);
    assert!(
        out.contains("measures=effective") || out.contains("measures=requested"),
        "every event says which of the two it is: {out}"
    );
    assert!(
        out.contains("basis=cpuinfo_avg_freq")
            || out.contains("basis=cpuinfo_cur_freq")
            || out.contains("basis=scaling_cur_freq"),
        "and which file it read: {out}"
    );
    assert!(
        out.contains("driver="),
        "and which driver produced it: {out}"
    );
}

/// A counter that is there and out of reach calls for privileges. Reporting it
/// as missing would send the reader looking for different hardware.
#[test]
fn an_unreadable_energy_counter_asks_for_privileges_not_for_hardware() {
    let rapl = Path::new("/sys/class/powercap/intel-rapl:0/energy_uj");
    if !rapl.exists() {
        return;
    }
    let readable = std::fs::read_to_string(rapl).is_ok();
    let attempt = run(&[
        "power.energy",
        "--system",
        "-m",
        "poll@100ms",
        "-e",
        "timeline",
        "-d",
        "300ms",
    ]);
    if readable {
        assert!(attempt.status.success(), "{}", stderr(&attempt));
        return;
    }

    // The sysfs file being out of reach does not mean the machine cannot
    // measure energy: the same RAPL domains open as a perf event wherever
    // `perf_event_paranoid` allows it, and the resolver takes that backend. A
    // run that succeeds here is the chain working, not the test's premise
    // failing -- what must never happen is a refusal that blames the hardware.
    if attempt.status.success() {
        return;
    }
    let message = stderr(&attempt);
    assert!(
        message.contains("root"),
        "the refusal has to name privileges: {message}"
    );
    assert!(
        !message.contains("does not exist"),
        "the file is there; saying otherwise sends the reader to the wrong place: {message}"
    );
}

/// Residency is cpu-time, and the value is one cpu's share of the tick. It is
/// allowed to exceed the tick, because the kernel credits the whole residency
/// at the moment a cpu *leaves* an idle state: a core that slept for a second
/// and woke inside a 200 ms tick reports a full second against that tick, for
/// a share of five.
///
/// The bound is therefore generous, and what it still catches is the failure
/// worth catching: microseconds left unconverted to nanoseconds is a factor of
/// a thousand, and no amount of late crediting reaches that over a tick this
/// long.
///
/// It used to be 1.5, which held only because the value was averaged over
/// every watched cpu -- twenty-four of them here -- and one core's late credit
/// divided by twenty-four is invisible. The events are per cpu now and the
/// smoothing is gone with them, which is the point: a machine reported as 90 %
/// idle while one core is pinned is the shape of the problem someone reads
/// this to find.
#[test]
fn an_idle_share_stays_within_reach_of_the_window_it_covers() {
    if !usable("power.cstate") {
        return;
    }
    let attempt = run(&[
        "power.cstate",
        "--system",
        "-m",
        "poll@200ms",
        "-e",
        "raw",
        "-d",
        "700ms",
    ]);
    assert!(attempt.status.success(), "{}", stderr(&attempt));
    let out = stdout(&attempt);
    let mut seen = 0;
    for line in out.lines() {
        let Some(field) = line
            .split_whitespace()
            .find(|f| f.starts_with("idle_ratio="))
        else {
            continue;
        };
        let ratio: f64 = field
            .trim_start_matches("idle_ratio=")
            .parse()
            .unwrap_or_else(|_| panic!("idle_ratio is a number: {line}"));
        assert!(
            (0.0..50.0).contains(&ratio),
            "an idle share of {ratio} is not late crediting, it is an error of scale: {line}"
        );
        seen += 1;
    }
    assert!(seen > 0, "the raw output carries the share: {out}");
}

/// The record written beside the numbers is what makes two runs comparable, and
/// for this family it is the run's own subject: governor, clocksource, and what
/// the machine was.
#[test]
fn the_record_beside_the_numbers_carries_the_machine_it_was_taken_on() {
    if !usable("power.temp") {
        return;
    }
    let directory =
        std::env::temp_dir().join(format!("omn-power-artifacts-{}", std::process::id()));
    std::fs::remove_dir_all(&directory).ok();
    let attempt = run(&[
        "power.temp",
        "--system",
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "-d",
        "300ms",
        "--out",
        &directory.to_string_lossy(),
    ]);
    assert!(attempt.status.success(), "{}", stderr(&attempt));

    let resolved = std::fs::read_to_string(directory.join("resolved.json"))
        .expect("the resolved request is written beside the results");
    let document: serde_json::Value =
        serde_json::from_str(&resolved).expect("the resolved record parses");
    let environment = &document[0]["environment"];
    assert!(environment["kernel_release"].is_string(), "{resolved}");
    assert!(
        environment.get("cpu_governor").is_some() || environment.get("clocksource").is_some(),
        "a power measurement that does not record the governor cannot be compared: {resolved}"
    );
    std::fs::remove_dir_all(&directory).ok();
}
