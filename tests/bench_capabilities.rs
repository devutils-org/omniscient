//! What the bench needs from the environment capture, checked through the
//! binary a user runs: `src/capabilities.rs` is this stream's, and these are
//! the two things it added.
//!
//! - **The prefetcher state**, captured and never changed (`docs/ROADMAP.md` phase
//!   8, boundary 5), in every resolved file.
//! - **`dynamic-linking`**, stream S's request: every preload source requires
//!   it, and it read as unverified on every run of any of them. It is now
//!   answered by whether the shim is where the binary will look for it -- the
//!   ground truth being the file on disk, and a path that is certainly not
//!   one.

use std::path::PathBuf;
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn omn() -> PathBuf {
    root().join("target/debug/omn")
}

fn listing(source: &str, env: Option<(&str, &str)>) -> String {
    let mut command = Command::new(omn());
    command.args(["list", source, "--color", "never"]);
    if let Some((key, value)) = env {
        command.env(key, value);
    }
    let output = command.output().expect("omn runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn requirement_line<'a>(listing: &'a str, requirement: &str) -> &'a str {
    listing
        .lines()
        .find(|line| line.trim_start().starts_with("needs") && line.contains(requirement))
        .unwrap_or_else(|| panic!("no `{requirement}` line in:\n{listing}"))
}

#[test]
fn dynamic_linking_is_available_exactly_when_the_shim_is_on_disk() {
    let shim = root().join("target/debug/libomn_preload.so");
    for source in ["sched.mutex", "gpu.api"] {
        let text = listing(source, None);
        let line = requirement_line(&text, "dynamic-linking");
        assert!(
            !line.contains("unverified"),
            "{source}: still unanswered: {line}"
        );
        if shim.exists() {
            assert!(line.trim_end().ends_with("available"), "{source}: {line}");
        } else {
            assert!(line.contains("missing"), "{source}: {line}");
        }
    }
}

#[test]
fn a_shim_that_is_not_there_is_missing_and_named() {
    let nowhere = "/nonexistent/omn-bench/libomn_preload.so";
    let text = listing("sched.mutex", Some(("OMN_PRELOAD_LIB", nowhere)));
    let line = requirement_line(&text, "dynamic-linking");
    assert!(line.contains("missing"), "{line}");
    assert!(
        line.contains(nowhere),
        "the reason must name the path looked at: {line}"
    );
}

#[test]
fn every_resolved_file_carries_the_prefetcher_state() {
    // `proc.threads` reads `/proc`, which every machine serves: the property is
    // the resolved file's, of any request, and a counter was refused on the
    // CI's runners (paranoid 4, and no hardware PMU) before the file existed.
    let output = Command::new(omn())
        .args(["trace", "proc.threads", "-m", "poll", "-e", "summary"])
        .args(["--dump-resolved", "--", "true"])
        .output()
        .expect("omn runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let resolved: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let prefetchers = resolved
        .pointer("/environment/prefetchers")
        .unwrap_or_else(|| panic!("no prefetcher state in {resolved:#}"));
    let state = prefetchers["state"].as_str().expect("a state");
    assert!(
        ["read", "unreadable", "unknown"].contains(&state),
        "{prefetchers}"
    );
    if state != "read" {
        assert!(
            prefetchers["reason"]
                .as_str()
                .is_some_and(|r| !r.is_empty()),
            "a state that was not read says why: {prefetchers}"
        );
    }
    eprintln!("prefetchers here: {prefetchers}");
}
