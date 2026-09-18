//! The `io` family, through the surfaces a user meets.
//!
//! The collectors are checked against the kernel's own counters in their own
//! unit tests, where a fake `/proc/diskstats` can be written and a known number
//! of bytes can be flushed to a real device. What is checked here is the rest
//! of the promise: that the catalog entries are truthful, that the sources this
//! build can collect say so, and that the combinations these backends cannot
//! mean are refused with the reason rather than answered.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use omn::capabilities::Capabilities;
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

/// The sources this family collects today, all of them through procfs.
const PROCFS_SOURCES: [&str; 5] = [
    "io.bytes",
    "io.iops",
    "io.merges",
    "io.inflight",
    "io.pressure",
];

#[test]
fn every_procfs_source_has_a_collector_in_this_build() {
    let implemented = omn::collect::implemented().join(", ");
    for source in PROCFS_SOURCES {
        assert!(
            implemented.contains(&format!("{source} (fs:proc")),
            "`{source}` has no collector: {implemented}"
        );
    }
}

/// A fallback that loses something has to say what, or the reader takes the
/// second-best number for the best one.
#[test]
fn every_io_fallback_states_what_it_degrades() {
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    for entry in catalog.family("io") {
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

/// Backend ids are how `--backend force:` names one and how a collector is
/// looked up. Two with the same id inside one chain cannot be told apart.
#[test]
fn backend_ids_are_unique_within_each_io_chain() {
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    for entry in catalog.family("io") {
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

/// Every source this family collects carries a caveat saying what its number
/// does *not* mean. For the block layer that is the whole difficulty: these
/// are the device's bytes, not the application's.
#[test]
fn every_collected_source_says_what_its_number_is_not() {
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    for source in PROCFS_SOURCES {
        let entry = catalog
            .get(&source.parse().expect("a source id"))
            .unwrap_or_else(|| panic!("`{source}` is in the catalog"));
        assert!(
            !entry.caveats.is_empty(),
            "`{source}` states no caveat, so nothing warns the reader what it is not"
        );
    }
}

/// The listing is the catalog seen through this machine. Where the file is
/// readable the source is usable, and the two must not disagree.
#[test]
fn the_listing_agrees_with_what_this_machine_holds() {
    let listing = stdout(&run(&["list", "io"]));
    let capabilities = Capabilities::detect();
    let readable = capabilities.probe("/proc/diskstats", None).is_available();
    let expected = if readable { "yes" } else { "no" };
    let row = listing
        .lines()
        .find(|line| line.starts_with("io.inflight"))
        .expect("io.inflight is listed");
    assert!(
        row.contains(expected),
        "/proc/diskstats is {}, but the listing says otherwise: {row}",
        if readable { "readable" } else { "not readable" }
    );
}

/// These counters are kept per device for the whole machine. Narrowing them to
/// a process would mean filtering inside the kernel, which only the eBPF
/// backend can do -- so the request is refused rather than answered with the
/// machine's total.
#[test]
fn a_per_process_request_is_refused_rather_than_answered_system_wide() {
    let refused = run(&["io.bytes", "--pid", "1", "-m", "poll", "-e", "timeline"]);
    let message = stderr(&refused);
    assert_ne!(refused.status.code(), Some(0), "{message}");
    assert!(
        message.contains("in-program filtering"),
        "the refusal must say why a procfs counter cannot serve a pid: {message}"
    );
}

/// A queue depth is read at an instant, so it counts over no window and cannot
/// produce a rate. The validator says so before anything is opened.
#[test]
fn a_mode_a_source_does_not_support_is_refused_with_what_it_does() {
    let refused = run(&["io.inflight", "--system", "-m", "count", "-e", "rate"]);
    assert_eq!(refused.status.code(), Some(EXIT_REJECTED));
    let message = stderr(&refused);
    assert!(
        message.contains("cannot be observed in `count` mode"),
        "{message}"
    );
    assert!(message.contains("poll, snapshot"), "{message}");
}

/// The dumped request is a session file, not a pretty-printed summary: what
/// the command line prototypes, a file replays.
#[test]
fn a_dumped_io_request_is_a_session_file_that_validates() {
    let dumped = run(&[
        "io.pressure",
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
    assert!(session.contains("io.pressure"), "{session}");

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

/// The examples are part of the documentation, and documentation that does not
/// parse is worse than none.
#[test]
fn the_examples_this_family_ships_validate() {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut checked = 0;
    for entry in std::fs::read_dir(examples).expect("examples/ exists") {
        let path = entry.expect("a directory entry").path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.starts_with("io-") {
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
