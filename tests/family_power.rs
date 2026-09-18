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
    let implemented = omn::collect::implemented().join(", ");
    for source in SOURCES {
        assert!(
            implemented.contains(&format!("{source} (fs:sys")),
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
    let listing = stdout(&run(&["list", "power"]));
    let exposed = Path::new("/sys/devices/system/cpu/cpu0/thermal_throttle").exists();
    let row = listing
        .lines()
        .find(|line| line.starts_with("power.throttle"))
        .expect("power.throttle is listed");
    let says_usable = row.split_whitespace().any(|word| word == "yes");
    assert_eq!(
        says_usable, exposed,
        "the listing and the filesystem disagree about throttling: {row}"
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
