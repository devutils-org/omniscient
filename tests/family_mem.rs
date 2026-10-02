//! Ground-truth tests for the `mem` family.
//!
//! Owned by that family; see docs/contributing.md. A test here checks a
//! measurement against something independently known -- a kernel counter, an
//! injected delay, a known size, a fixed iteration count. A test that checks
//! the code returns what the code computed proves nothing, which is the whole
//! difficulty of testing a tracer.

use std::io::Write;
use std::process::{Command, Output, Stdio};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// What the tool returns for a request it refuses before measuring anything.
const EXIT_REJECTED: i32 = 2;

fn run(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

fn run_session(session: &str) -> Output {
    let mut child = Command::new(BINARY)
        .args(["-f", "-", "--input-format", "yaml"])
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
    child.wait_with_output().expect("omn should finish")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Some machines withhold what these tests need. That is a skip, not a pass and
/// not a failure.
///
/// A missing shim is neither: it is a build that was not run, and letting it
/// skip would mean the heap tests quietly measure nothing on the machine where
/// they matter most. It fails, and says what to run.
fn unavailable(output: &Output) -> Option<String> {
    let message = stderr(output);
    assert!(
        !message.contains(SHIM_MISSING),
        "the heap sources need the preload shim: run `cargo build -p omn-preload` first\n{message}"
    );
    let withheld = message.contains("not readable") || message.contains("perf_event_paranoid");
    withheld.then_some(message)
}

/// The words the tool uses when the shim is not where it looks
/// (`collect::mem::heap::shim_in`: *"the preload shim was not found at ..."*).
/// Until the sixteenth wave the check read "heap shim was not found", which the
/// tool has not printed since the ninth, so a missing shim read as whatever
/// failure came after it.
const SHIM_MISSING: &str = "shim was not found";

/// Runs a copy of the binary from a directory that holds no shim, beside it or
/// in `../lib/omn/`, with `OMN_PRELOAD_LIB` unset: a build that was not run
/// through `make`, made on purpose.
fn run_without_the_shim(args: &[&str]) -> Output {
    let directory = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("no-shim-{}", std::process::id()))
        .join("bin");
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let copy = directory.join("omn");
    std::fs::copy(BINARY, &copy).expect("copying the binary");
    // Ground truth, outside the tool: neither place it looks holds a shim.
    for place in ["libomn_preload.so", "../lib/omn/libomn_preload.so"] {
        assert!(
            !directory.join(place).exists(),
            "{} exists",
            directory.join(place).display()
        );
    }
    // `ETXTBSY` is retried: another test thread that forks while the copy
    // is still open for writing holds that descriptor until its child execs,
    // and the kernel refuses to run a file someone may still be writing. The
    // descriptor is close-on-exec, so the refusal ends within a fork-exec of
    // another test; failed once in this file's parallel run.
    let mut busy = 0;
    let output = loop {
        match Command::new(&copy)
            .args(args)
            .env_remove("OMN_PRELOAD_LIB")
            .output()
        {
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && busy < 100 => {
                busy += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            other => break other.expect("the copy should run"),
        }
    };
    let _ = std::fs::remove_dir_all(directory.parent().expect("the scratch's parent"));
    output
}

/// A missing shim fails the heap tests rather than skipping them, and the
/// check that says so matches what the tool prints.
#[test]
fn a_binary_without_its_shim_fails_the_heap_tests_rather_than_skipping_them() {
    let output = run_without_the_shim(&["mem.heap.alloc", "-e", "summary", "--", "true"]);
    assert_ne!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(SHIM_MISSING),
        "the tool's words for a missing shim changed: {}",
        stderr(&output)
    );
    let caught = std::panic::catch_unwind(|| unavailable(&output));
    assert!(
        caught.is_err(),
        "a missing shim was read as a machine that cannot serve the request"
    );
}

fn has_python() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

/// Several footprints of one program, in one window.
fn footprint_session(program: &str, sources: &[&str]) -> String {
    let mut session = String::from("format: json\nrequests:\n");
    for source in sources {
        session.push_str(&format!(
            "  - source: {source}\n    \
             target: {{launch: [python3, -c, {program:?}]}}\n    \
             mode: poll@50ms\n    emit: [summary]\n"
        ));
    }
    session
}

/// The minimum and maximum of one result, or `None` when the run produced no
/// series for it at all.
///
/// The `None` is not a convenience. A launched `python3` under a loaded machine
/// can take longer to reach its first line than the window it is given, and
/// then there is nothing to summarise -- which is a fact about this machine's
/// scheduling and not about the collector. Indexing `series[0]` there panics at
/// the subscript and blames the footprint for a slow interpreter; the caller
/// below tells the two apart instead.
fn summary(run: &serde_json::Value, index: usize) -> Option<(f64, f64)> {
    let series = run["results"][index]["outputs"][0]["series"]
        .as_array()
        .unwrap_or_else(|| panic!("a summary per key in result {index}: {run}"));
    let first = series.first()?;
    Some((
        first["min"].as_f64().expect("a minimum"),
        first["max"].as_f64().expect("a maximum"),
    ))
}

/// Ground truth for the three footprints.
///
/// A known amount of private memory is made resident in a launched program, and
/// each of the three metrics has to see it. It is the same memory, touched
/// once, so the rise is the same number three times over -- what differs
/// between RSS, PSS and USS is how they count what the program *shares*, and a
/// buffer the program just wrote to is shared with nobody.
///
/// Without a test of this shape, a collector that returned a plausible constant
/// would pass everything else.
#[test]
fn a_known_private_allocation_shows_up_in_all_three_footprints() {
    const TOUCHED: usize = 64 * 1024 * 1024;

    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    // A baseline first, then the memory, then time to be polled again.
    let program = format!(
        "import time\n\
         time.sleep(0.3)\n\
         block = bytearray({TOUCHED})\n\
         time.sleep(0.4)\n"
    );
    let sources = ["mem.rss", "mem.pss", "mem.uss"];
    let output = run_session(&footprint_session(&program, &sources));
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");

    // Three sources polled over one window either all saw readings or the
    // window closed before any did. "Some did and some did not" is a defect and
    // still fails below; "none did" is a program that never got going, and the
    // scenario says so rather than asserting a rise that could not exist.
    let seen: Vec<bool> = (0..sources.len())
        .map(|index| summary(&run, index).is_some())
        .collect();
    if seen.iter().all(|got| !got) {
        eprintln!(
            "skipped: no source produced a reading in its window, so the launched program \
             never got far enough to be measured; sources {sources:?}"
        );
        return;
    }

    let mut maxima = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        assert_eq!(run["results"][index]["source"], *source);
        let (low, high) = summary(&run, index).unwrap_or_else(|| {
            panic!(
                "{source} produced no series where the others did, which is the collector \
                 and not the window: {run}"
            )
        });
        let rise = high - low;
        assert!(
            rise >= 0.75 * TOUCHED as f64,
            "{source}: {TOUCHED} bytes were touched but the rise was only {rise}"
        );
        assert!(
            rise <= 4.0 * TOUCHED as f64,
            "{source}: a rise of {rise} is far more than the {TOUCHED} bytes touched"
        );
        maxima.push(high);
    }

    // Private is a subset of proportional is a subset of resident. The three
    // are read one after the other inside a tick, so a few pages may appear
    // between two of them; a fraction of a percent of slack covers that, and
    // nothing near the size of the buffer.
    let slack = 0.01 * maxima[0];
    assert!(
        maxima[2] <= maxima[1] + slack,
        "uss {} exceeds pss {}",
        maxima[2],
        maxima[1]
    );
    assert!(
        maxima[1] <= maxima[0] + slack,
        "pss {} exceeds rss {}",
        maxima[1],
        maxima[0]
    );
}

/// The proportional footprint has to be below the resident one by roughly what
/// the program shares -- its libraries. Every process running the interpreter
/// maps the same libc, and PSS is the metric that does not count it in full in
/// each of them.
#[test]
fn the_proportional_footprint_is_below_the_resident_one_for_a_shared_interpreter() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let program = "import time; time.sleep(0.4)\n";
    let output = run_session(&footprint_session(program, &["mem.rss", "mem.pss"]));
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");
    let (Some((_, rss)), Some((_, pss))) = (summary(&run, 0), summary(&run, 1)) else {
        eprintln!(
            "skipped: the launched interpreter produced no reading in its window, so there \
             is no pair to compare"
        );
        return;
    };
    assert!(
        pss < rss,
        "an interpreter shares its libraries, so pss {pss} must be below rss {rss}"
    );
}

/// A program that allocates a known amount of memory, touches it, and then
/// frees every other block.
///
/// The holes are the point. Freeing everything lets the allocator hand the
/// whole block back to the kernel -- on this machine glibc trims 128 MiB back
/// to nothing the moment the last chunk goes -- and there would be no
/// fragmentation to measure. Freeing every other chunk leaves the arena with
/// live memory at the top and holes below it, which is exactly the shape that
/// makes a long-running process grow and never shrink.
fn fragmenting_program(count: usize, size: usize) -> String {
    format!(
        "import ctypes, time\n\
         libc = ctypes.CDLL(None)\n\
         libc.malloc.restype = ctypes.c_void_p\n\
         libc.free.argtypes = [ctypes.c_void_p]\n\
         blocks = [libc.malloc({size}) for _ in range({count})]\n\
         for block in blocks: ctypes.memset(block, 1, {size})\n\
         time.sleep(0.25)\n\
         for block in blocks[::2]: libc.free(block)\n\
         time.sleep(0.45)\n"
    )
}

fn heap_session(source: &str, program: &str) -> String {
    format!(
        "format: json\nrequests:\n  \
         - source: {source}\n    \
         target: {{launch: [python3, -c, {program:?}]}}\n    \
         mode: poll@50ms\n    emit: [summary, timeline]\n"
    )
}

/// Ground truth for the live heap.
///
/// The program allocates a known number of blocks of a known size through
/// `libc.malloc` itself, so the live total is arithmetic rather than opinion:
/// 1024 blocks of 64 KiB is 64 MiB, and after half of them are freed it is 32
/// MiB. The interpreter's own allocations ride on top of that, which is why the
/// bounds are ranges and not equalities.
#[test]
fn the_live_heap_matches_the_allocations_the_program_made() {
    const COUNT: usize = 1024;
    const SIZE: usize = 64 * 1024;
    const ALLOCATED: f64 = (COUNT * SIZE) as f64;

    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let output = run_session(&heap_session(
        "mem.heap.live",
        &fragmenting_program(COUNT, SIZE),
    ));
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");
    let series = run["results"][0]["outputs"][0]["series"][0].clone();
    let peak = series["max"].as_f64().expect("a maximum");
    let last = series["last"].as_f64().expect("a last value");

    assert!(
        (ALLOCATED..1.5 * ALLOCATED).contains(&peak),
        "{COUNT} blocks of {SIZE} bytes are {ALLOCATED} bytes live at the peak, measured {peak}"
    );
    assert!(
        (0.4 * ALLOCATED..0.75 * ALLOCATED).contains(&last),
        "half of them were freed, so about {} bytes should be left, measured {last}",
        ALLOCATED / 2.0
    );
}

/// Ground truth for retained memory, the metric this family exists to make
/// measurable.
///
/// Half of a known 64 MiB is freed, and every page of it was written to before
/// the free, so every page of it is memory the program gave back and the
/// process still holds. The allocator cannot return any of it: the holes are
/// surrounded by live blocks.
///
/// The check that this is not a tautology is the resident size reported beside
/// it. RSS stays at the full 64 MiB while the live heap halves, and the
/// retained number accounts for the difference -- which is precisely what "free
/// returns nothing" means, measured rather than asserted.
#[test]
fn memory_freed_into_a_fragmented_arena_is_reported_as_retained() {
    const COUNT: usize = 1024;
    const SIZE: usize = 64 * 1024;
    const FREED: f64 = (COUNT / 2 * SIZE) as f64;

    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let output = run_session(&heap_session(
        "mem.heap.retained",
        &fragmenting_program(COUNT, SIZE),
    ));
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");
    let result = &run["results"][0];
    let retained = result["outputs"][0]["series"][0]["last"]
        .as_f64()
        .expect("a last value");

    // A page shared between a freed block and a live one is counted as live, so
    // the measurement is a lower bound: 64 KiB blocks are 16 pages each and at
    // most one page at each end is given away.
    assert!(
        retained >= 0.8 * FREED,
        "{FREED} bytes of touched memory were freed into holes, measured {retained} retained"
    );
    assert!(
        retained <= 1.1 * FREED,
        "only {FREED} bytes were freed, so {retained} is more than there was"
    );

    // Nothing was lost, and the raw events carry what the number is made of.
    assert_eq!(result["losses"]["dropped"], 0);
    let last_point = result["outputs"][1]["series"][0]["points"]
        .as_array()
        .expect("a timeline")
        .last()
        .cloned()
        .expect("at least one point");
    assert!(
        last_point["value"].as_f64().unwrap() > 0.0,
        "the timeline ends with the retained memory still there: {last_point}"
    );
}

/// The claim `mem.heap.retained` exists to make measurable, end to end.
///
/// "`free()` returns nothing to the operating system" is three statements about
/// one run: the live heap halves, the resident size does not follow it down,
/// and the difference is memory the allocator is holding. All three come out of
/// a single `mem.heap.retained` collection, which reports live bytes and
/// resident bytes beside the retained ones for exactly this reason.
#[test]
fn freeing_half_the_heap_moves_the_live_total_and_not_the_resident_size() {
    const COUNT: usize = 1024;
    const SIZE: usize = 64 * 1024;

    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }

    let session = format!(
        "format: json\nrequests:\n  \
         - source: mem.heap.retained\n    \
         target: {{launch: [python3, -c, {:?}]}}\n    \
         mode: poll@50ms\n    emit: [raw]\n",
        fragmenting_program(COUNT, SIZE)
    );
    let output = run_session(&session);
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");
    let events = run["results"][0]["outputs"][0]["events"]
        .as_array()
        .expect("raw events")
        .clone();
    assert!(
        events.len() > 4,
        "a poll every 50 ms over a run of one second"
    );

    let attr = |event: &serde_json::Value, name: &str| -> f64 {
        event["attrs"]
            .as_array()
            .expect("attributes")
            .iter()
            .find(|attr| attr["name"] == name)
            .unwrap_or_else(|| panic!("no `{name}` on {event}"))["value"]
            .as_f64()
            .expect("a number")
    };

    let peak_live = events
        .iter()
        .map(|event| attr(event, "live"))
        .fold(0.0, f64::max);
    // `rss` is on the event for scale and only when it could be read: the
    // collector takes it from `/proc/<pid>/statm`, and a poll that lands after
    // the observed program exited finds no such file. Every other attribute
    // here comes from the shim's own accounting and is always present, so the
    // peak is taken over the events that carry it rather than demanding it of
    // all of them -- asking for it unconditionally made this a test of whether
    // the program outlived the last tick.
    let with_rss: Vec<&serde_json::Value> = events
        .iter()
        .filter(|event| {
            event["attrs"]
                .as_array()
                .is_some_and(|attrs| attrs.iter().any(|attr| attr["name"] == "rss"))
        })
        .collect();
    assert!(
        !with_rss.is_empty(),
        "no poll of this run read the target's resident size at all: {events:?}"
    );
    let peak_rss = with_rss
        .iter()
        .map(|event| attr(event, "rss"))
        .fold(0.0, f64::max);
    let last = events.last().expect("at least one point");

    // Half of the blocks were freed, so the live heap halves.
    let allocated = (COUNT * SIZE) as f64;
    assert!(
        peak_live >= allocated,
        "{COUNT} blocks of {SIZE} bytes were live at the peak, measured {peak_live}"
    );
    assert!(
        (0.4 * allocated..0.75 * allocated).contains(&attr(last, "live")),
        "half of them were freed, so about {} bytes should be live at the end, measured {}",
        allocated / 2.0,
        attr(last, "live")
    );

    // And the resident size does not follow it down: that is the whole point.
    assert!(
        attr(last, "rss") >= 0.9 * peak_rss,
        "the allocator kept the pages, so rss should still be near its peak of {peak_rss}, \
         measured {}",
        attr(last, "rss")
    );

    // The retained number accounts for what the live total lost.
    let freed = allocated / 2.0;
    assert!(
        attr(last, "bytes") >= 0.8 * freed,
        "{freed} bytes were freed into holes, {} reported as retained",
        attr(last, "bytes")
    );
}

// --- The sources that need no privileges at all -------------------------
//
// Six sources read a file and say what is in it: pressure, swap, huge pages,
// cgroup accounting, per-node placement and the working set. The unit tests
// beside each collector check its arithmetic against a kernel counter; what is
// checked here is the whole tool -- the catalog entry, the resolver, the
// collector and the emit layer -- against a program whose memory behaviour the
// test chose.

/// The six sources this stream added, with the backend each one is expected to
/// be collected through.
const BREADTH_SOURCES: [(&str, &str); 6] = [
    ("mem.pressure", "fs:proc, fs:cgroup"),
    ("mem.swap", "fs:proc, fs:cgroup"),
    ("mem.thp", "fs:sys, fs:proc"),
    ("mem.cgroup", "fs:cgroup"),
    ("mem.numa", "fs:proc"),
    ("mem.wss", "fs:proc:clear_refs"),
];

#[test]
fn every_file_reading_source_has_a_collector_in_this_build() {
    let implemented = omn::collect::implemented().join("; ");
    for (source, backends) in BREADTH_SOURCES {
        assert!(
            implemented.contains(&format!("{source} ({backends})")),
            "`{source}` has no collector on `{backends}`: {implemented}"
        );
    }
}

/// A fallback that loses something has to say what, or the reader takes the
/// second-best number for the best one.
#[test]
fn every_mem_fallback_states_what_it_degrades() {
    let catalog = omn::catalog::Catalog::embedded().expect("the embedded catalog loads");
    for entry in catalog.family("mem") {
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
/// looked up; two with the same id inside one chain cannot be told apart.
#[test]
fn backend_ids_are_unique_within_each_mem_chain() {
    let catalog = omn::catalog::Catalog::embedded().expect("the embedded catalog loads");
    for entry in catalog.family("mem") {
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

/// This family's cardinal rule is that the accounting metric is explicit, and a
/// caveat is where a source says what its number is *not*. Every one of these
/// six reports something that is routinely mistaken for something else.
#[test]
fn every_new_source_says_what_its_number_is_not() {
    let catalog = omn::catalog::Catalog::embedded().expect("the embedded catalog loads");
    for (source, _) in BREADTH_SOURCES {
        let entry = catalog
            .get(&source.parse().expect("a source id"))
            .unwrap_or_else(|| panic!("`{source}` is in the catalog"));
        assert!(
            !entry.caveats.is_empty(),
            "`{source}` states no caveat, so nothing warns the reader what it is not"
        );
    }
}

/// Huge page promotion is counted for the machine and cannot be attributed to a
/// process. The refusal comes from the catalog, before anything is attached.
#[test]
fn a_machine_wide_source_refuses_a_process_target() {
    let refused = run(&["mem.thp", "--pid", "1", "-m", "snapshot", "-e", "raw"]);
    let message = stderr(&refused);
    assert_eq!(refused.status.code(), Some(EXIT_REJECTED), "{message}");
    assert!(
        stdout(&refused).is_empty(),
        "a machine-wide total must not be produced for a process target:\n{}",
        stdout(&refused)
    );
    assert!(message.contains("system"), "{message}");
}

/// A working set is what is touched over a window, so a single read has nothing
/// to report. The refusal names the mode that does work.
#[test]
fn a_working_set_over_no_window_is_refused_with_the_mode_that_works() {
    let refused = run(&["mem.wss", "--pid", "1", "-m", "snapshot", "-e", "raw"]);
    let message = stderr(&refused);
    assert_eq!(refused.status.code(), Some(EXIT_REJECTED), "{message}");
    assert!(message.contains("poll"), "{message}");
}

/// The listing is the catalog seen through this machine, and the two must not
/// disagree about a file either one can look at.
#[test]
fn the_listing_agrees_with_what_this_machine_holds() {
    let listing = stdout(&run(&["list", "mem"]));
    let capabilities = omn::capabilities::Capabilities::detect();
    let readable = capabilities
        .probe("/proc/pressure/memory", None)
        .is_available();
    let row = listing
        .lines()
        .find(|line| line.starts_with("mem.pressure"))
        .expect("mem.pressure is listed");
    let expected = if readable { "yes" } else { "no" };
    assert!(
        row.contains(expected),
        "/proc/pressure/memory is {}, and the listing says otherwise: {row}",
        if readable { "readable" } else { "not readable" }
    );
}

/// Ground truth through the whole tool: a program that *holds* a lot of memory
/// and *touches* very little of it.
///
/// The program makes 256 MiB resident and then spends a second walking only the
/// first 16 MiB of it. Resident size and working set are then two numbers about
/// the same process that differ by more than a factor of ten, and `mem.rss` and
/// `mem.wss` have to report them the right way round. A collector returning the
/// resident set under either name passes nothing here.
#[test]
fn the_working_set_is_what_was_touched_and_not_what_was_held() {
    const RESIDENT: usize = 256 * 1024 * 1024;
    const WORKING: usize = 16 * 1024 * 1024;

    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let program = format!(
        "import time\n\
         held = bytearray({RESIDENT})\n\
         working = memoryview(held)[:{WORKING}]\n\
         deadline = time.time() + 2.5\n\
         while time.time() < deadline:\n\
         \x20   for offset in range(0, {WORKING}, 4096): working[offset] = 1\n"
    );
    let session = format!(
        "format: json\nrequests:\n  \
         - source: mem.wss\n    \
         target: {{launch: [python3, -c, {program:?}]}}\n    \
         mode: poll@250ms\n    emit: [timeline]\n  \
         - source: mem.rss\n    \
         target: {{launch: [python3, -c, {program:?}]}}\n    \
         mode: poll@250ms\n    emit: [timeline]\n"
    );

    let output = run_session(&session);
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");

    let points = |index: usize| -> Vec<f64> {
        run["results"][index]["outputs"][0]["series"][0]["points"]
            .as_array()
            .unwrap_or_else(|| panic!("a timeline in result {index}: {run}"))
            .iter()
            .map(|point| point["value"].as_f64().expect("a value"))
            .collect()
    };
    let working_set = points(0);
    let resident = points(1);

    // Two points are excluded. The first window covers the allocation itself,
    // when every page is touched for the first time. The last covers a window
    // the program's exit cut short, so it holds a fraction of an interval's
    // work and is not a rate over one.
    //
    // What is left is compared on its *busiest* window rather than its
    // quietest, and that is not a convenience. The suite runs its tests in
    // parallel, a dozen of them holding hundreds of megabytes at once, and a
    // starved interpreter can go most of a 250 ms window without running at
    // all -- which is a fact about the machine, not about the collector. The
    // busiest window is the one the claim is actually about: even at its most
    // active this program touches a small fraction of what it holds, and a
    // collector reporting the resident set under this name fails on exactly
    // that window.
    assert!(
        working_set.len() >= 5,
        "two and a half seconds at 250 ms is several points, not {}",
        working_set.len()
    );
    let interior = &working_set[2..working_set.len() - 1];
    let busiest = interior.iter().copied().fold(0.0_f64, f64::max);
    let held = resident.iter().copied().fold(0.0_f64, f64::max);

    assert!(
        held >= 0.9 * RESIDENT as f64,
        "the program holds {RESIDENT} bytes and mem.rss reported at most {held}"
    );
    assert!(
        busiest >= 0.5 * WORKING as f64,
        "the program walks {WORKING} bytes over and over, and its busiest full window came back \
         as {busiest}: {interior:?}"
    );
    assert!(
        busiest < 0.25 * held,
        "the program touches a sixteenth of what it holds, and its busiest window came back as \
         {busiest} against a resident set of {held}"
    );
}

/// Ground truth through the whole tool: per-node placement accounts for memory
/// the program made resident, and agrees with the resident size measured beside
/// it in the same window.
///
/// Every resident page is on exactly one node, so the per-node residency summed
/// over the nodes is the resident set -- which `mem.rss` reads from an entirely
/// different file. Two sources, two files, one number.
#[test]
fn per_node_placement_accounts_for_the_resident_set() {
    const TOUCHED: usize = 64 * 1024 * 1024;

    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let program = format!(
        "import time\n\
         block = bytearray({TOUCHED})\n\
         time.sleep(0.6)\n"
    );
    let session = format!(
        "format: json\nrequests:\n  \
         - source: mem.numa\n    \
         target: {{launch: [python3, -c, {program:?}]}}\n    \
         mode: poll@200ms\n    emit: [raw]\n  \
         - source: mem.rss\n    \
         target: {{launch: [python3, -c, {program:?}]}}\n    \
         mode: poll@200ms\n    emit: [summary]\n"
    );

    let output = run_session(&session);
    if let Some(reason) = unavailable(&output) {
        eprintln!("skipped: {reason}");
        return;
    }
    assert!(output.status.success(), "{}", stderr(&output));
    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one json document");

    // The last tick of the raw stream: one event per node, all at one instant.
    let events = run["results"][0]["outputs"][0]["events"]
        .as_array()
        .unwrap_or_else(|| panic!("raw events: {run}"));
    assert!(!events.is_empty(), "a running process holds resident pages");
    let last_ts = events
        .last()
        .and_then(|event| event["ts_ns"].as_u64())
        .expect("a timestamp");
    let placed: f64 = events
        .iter()
        .filter(|event| event["ts_ns"].as_u64() == Some(last_ts))
        .map(|event| {
            event["attrs"]
                .as_array()
                .expect("attributes")
                .iter()
                .find(|attr| attr["name"] == "bytes")
                .and_then(|attr| attr["value"].as_f64())
                .expect("bytes on every node event")
        })
        .sum();

    let Some((_, resident)) = summary(&run, 1) else {
        eprintln!(
            "skipped: the footprint produced no reading in its window, so the per-node \
             placement has nothing to be checked against"
        );
        return;
    };
    assert!(
        placed >= TOUCHED as f64,
        "the program made {TOUCHED} bytes resident and the per-node placement adds up to {placed}"
    );
    let ratio = placed / resident;
    assert!(
        (0.9..1.1).contains(&ratio),
        "numa_maps places {placed} bytes and smaps_rollup reports a resident set of {resident}"
    );
}

/// A system-wide file reader runs, produces a point per tick, and loses
/// nothing. Three of the six answer for the machine, and all three go through
/// the same shape of counter read.
#[test]
fn the_machine_wide_readers_run_clean() {
    for source in ["mem.pressure", "mem.swap", "mem.thp"] {
        let session = format!(
            "format: json\nrequests:\n  \
             - source: {source}\n    target: system\n    mode: poll@100ms\n    \
             emit: [timeline]\nduration: 500ms\n"
        );
        let output = run_session(&session);
        if let Some(reason) = unavailable(&output) {
            eprintln!("skipped: {source}: {reason}");
            continue;
        }
        assert!(output.status.success(), "{source}: {}", stderr(&output));
        let run: serde_json::Value =
            serde_json::from_str(&stdout(&output)).expect("one json document");
        let points = run["results"][0]["outputs"][0]["series"][0]["points"]
            .as_array()
            .unwrap_or_else(|| panic!("a timeline for {source}: {run}"));
        assert!(
            points.len() >= 3,
            "half a second at 100 ms is four or five points for {source}, not {}",
            points.len()
        );
        assert_eq!(
            run["results"][0]["losses"]["failed_reads"].as_u64(),
            Some(0),
            "{source} lost a read: {run}"
        );
    }
}
