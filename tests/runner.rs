//! What the tool says about the things it does not do yet.
//!
//! Three answers are easy to confuse and cost a caller differently. A request
//! that cannot mean anything is refused (exit 2). A request that is valid and
//! that this build cannot serve is unimplemented (exit 3), and no privilege,
//! retry or different machine will change that. Anything else is a failure of
//! this run (exit 1).
//!
//! These are end-to-end on the binary, because the exit code and what goes to
//! stderr are the whole of the contract a script sees.
//!
//! `--budget` used to be the fourth answer -- parsed, carried and consulted by
//! nothing. It is enforced now, and what it says and does lives in
//! `tests/budget.rs`.

use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// A request was refused: it cannot mean what it asks for.
const EXIT_REJECTED: i32 = 2;
/// The request is valid, and this build has nothing that answers it.
const EXIT_NOT_IMPLEMENTED: i32 = 3;

fn run(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Ground truth for "the request is valid": the catalog, read independently of
/// the run being checked. `targets:` is what decides whether a target is a
/// legitimate thing to ask for, and the loader is the same one the binary
/// embeds.
fn catalog_declares_target(source: &str, target: &str) -> bool {
    let catalog = omn::catalog::Catalog::load_dir(std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/catalog"
    )))
    .expect("the catalog in this tree loads");
    catalog
        .get(&source.parse().expect("a source id"))
        .expect("the source is in the catalog")
        .supported_targets()
        .iter()
        .any(|kind| kind.as_str() == target)
}

/// `mem.heap.alloc --pid` is valid and unserveable here, and those are two
/// different facts.
///
/// The catalog declares `pid`, and it is right to: a uprobe or a USDT probe
/// attaches to a process that is already running. Neither is built, and the
/// backend that is -- a preloaded shim -- reaches a program only at exec. So
/// the answer is "this build does not do that", which used to exit 1 and was
/// indistinguishable from a counter that failed to open.
#[test]
fn a_target_this_build_cannot_serve_exits_as_unimplemented_not_as_a_failure() {
    assert!(
        catalog_declares_target("mem.heap.alloc", "pid"),
        "this test is about a *valid* request; if the catalog stopped declaring `pid` the \
         refusal moved to the validator and belongs at exit 2"
    );

    // Pid 1 exists on every Linux machine, so nothing here is about whether
    // the target was found.
    let output = run(&["mem.heap.alloc", "--pid", "1", "-e", "summary"]);

    assert_eq!(
        output.status.code(),
        Some(EXIT_NOT_IMPLEMENTED),
        "stderr was: {}",
        stderr(&output)
    );
    let complaint = stderr(&output);
    assert!(
        complaint.contains("at exec"),
        "the refusal has to name the mechanism, or the reader tries again with sudo: \
         {complaint}"
    );
}

/// The same source, over the target this build does serve, measures.
///
/// Without this the test above would pass just as well if `mem.heap.alloc`
/// were broken outright, and "not built" would be indistinguishable from
/// "built and failing".
#[test]
fn the_same_source_over_the_target_this_build_serves_measures() {
    let output = run(&[
        "mem.heap.alloc",
        "-e",
        "summary",
        "--",
        "/bin/sh",
        "-c",
        "echo hello > /dev/null",
    ]);
    assert!(
        output.status.success(),
        "a launch target is what the shim needs, and it has one: {}",
        stderr(&output)
    );
}

/// The classes must stay apart. A target the catalog refuses outright is not
/// "not built yet": no build will ever measure the resident size of a network
/// interface, and telling a caller to wait for one is wrong.
#[test]
fn a_request_that_cannot_mean_anything_is_still_refused_rather_than_deferred() {
    assert!(
        !catalog_declares_target("mem.rss", "iface"),
        "this test needs a target the catalog does not declare"
    );
    let output = run(&["mem.rss", "--iface", "lo", "-m", "snapshot", "-e", "raw"]);
    assert_eq!(
        output.status.code(),
        Some(EXIT_REJECTED),
        "stderr was: {}",
        stderr(&output)
    );
}

/// The environment travels with the numbers, and two `gpu` runs are comparable
/// only if the device and the driver were the same. Neither is visible in any
/// other field, so their absence made two runs on two cards look like one
/// series.
///
/// Ground truth is the kernel's own description, read here rather than through
/// the tool. A machine with no graphics device reports none, which is also
/// checked: the capture must look and find nothing, not fail to look.
#[test]
fn the_resolved_record_names_the_graphics_device_and_its_driver() {
    let output = run(&[
        "mem.rss",
        "--pid",
        "1",
        "-m",
        "snapshot",
        "-e",
        "raw",
        "--dump-resolved",
        "--format",
        "json",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let resolved: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--dump-resolved --format json is json");
    let captured = resolved[0]["environment"]["gpus"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let mut expected: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_card = name
                .strip_prefix("card")
                .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()));
            if is_card && entry.path().join("device/vendor").exists() {
                expected.push(name);
            }
        }
    }
    expected.sort();

    let captured_cards: Vec<String> = captured
        .iter()
        .map(|gpu| gpu["card"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        captured_cards, expected,
        "the record must name every card the kernel lists, and nothing else"
    );

    for gpu in &captured {
        let card = gpu["card"].as_str().expect("a card name");
        let device = std::path::Path::new("/sys/class/drm")
            .join(card)
            .join("device");
        let vendor = std::fs::read_to_string(device.join("vendor")).expect("a pci vendor");
        assert!(
            gpu["pci_id"]
                .as_str()
                .expect("a pci id")
                .starts_with(vendor.trim()),
            "the ids in the record must be the kernel's own: {gpu}"
        );
    }
}
