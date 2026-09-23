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

// ---------------------------------------------------------------------------
// The run model: a workload beside the machine, the program's own lifetime,
// and one shim stream per launch. `docs/families/runner.md` is what these
// measured.
// ---------------------------------------------------------------------------

use std::process::Stdio;

use omn::collect::{self, Collection};
use omn::request::{Emit, Mode, Request, Target};

fn run_session(session: &str) -> Output {
    use std::io::Write;

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
        .expect("the session is written");
    child.wait_with_output().expect("omn should finish")
}

/// Whether a refusal is this machine's limit rather than the request's. Only
/// the first is a reason for a scenario to stop, and it stops loudly.
fn machine_cannot_serve(message: &str) -> bool {
    message.contains("no viable backend")
        || message.contains("cannot start collecting")
        || message.contains("perf_event_paranoid")
}

fn results_labelled<'a>(document: &'a serde_json::Value, label: &str) -> &'a serde_json::Value {
    document["results"]
        .as_array()
        .expect("a results array")
        .iter()
        .find(|results| results["label"] == label)
        .unwrap_or_else(|| panic!("no result labelled `{label}` in {document}"))
}

/// A shell loop of `iterations` turns: work that scales with a number this
/// test chooses, and that retires the same instructions however it is
/// scheduled.
fn counting_loop(iterations: u32) -> String {
    format!("i=0; while [ $i -lt {iterations} ]; do i=$((i+1)); done")
}

/// The last cpu this process may run on, which is where the scenario puts both
/// the program and the stranger beside it.
fn a_core_of_ours() -> usize {
    // SAFETY: a zeroed cpu set filled by the kernel for this process.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: the set is owned by this frame and sized by `size_of`.
    let got = unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&set), &mut set) };
    assert_eq!(got, 0, "sched_getaffinity on this process");
    (0..libc::CPU_SETSIZE as usize)
        .rev()
        // SAFETY: reading a bit of a set this frame owns.
        .find(|cpu| unsafe { libc::CPU_ISSET(*cpu, &set) })
        .expect("this process may run somewhere")
}

/// A process spinning on one core for as long as the guard lives: the
/// stranger a core-wide counter cannot tell from the program.
struct Stranger(std::process::Child);

impl Stranger {
    fn on(core: usize) -> Stranger {
        Stranger(
            Command::new("taskset")
                .args(["-c", &core.to_string(), "sh", "-c", "while :; do :; done"])
                .spawn()
                .expect("taskset runs"),
        )
    }
}

impl Drop for Stranger {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn instructions_session(core: usize, iterations: u32, with_core: bool) -> String {
    let mut session = format!(
        "name: the program beside its core\nformat: json\nrequests:\n  \
         - label: program\n    source: cpu.instructions\n    \
         target: &program {{launch: [taskset, -c, \"{core}\", /bin/sh, -c, \"{}\"]}}\n    \
         mode: count\n    emit: [summary]\n",
        counting_loop(iterations)
    );
    if with_core {
        session.push_str(&format!(
            "  - label: core\n    source: cpu.instructions\n    target: {{cpu: \"{core}\"}}\n    \
             mode: count\n    emit: [summary]\n"
        ));
    }
    session
}

fn counted(document: &serde_json::Value, label: &str) -> f64 {
    results_labelled(document, label)["outputs"][0]["series"][0]["last"]
        .as_f64()
        .unwrap_or_else(|| panic!("a counted value for `{label}` in {document}"))
}

/// The confounder check in one run, which is what the machine crossing could
/// not do: the program's instructions, and the instructions of the core it ran
/// on, with a stranger spinning on that core the whole time.
///
/// Ground truth is the program's own count taken alone first. The stranger's
/// work must be in the core's count and must not be in the program's: the two
/// requests are about different targets and the run keeps them apart. And the
/// program ends the run -- the core has no end of its own.
///
/// What is asserted is the separation, never the stranger's share. The core
/// counts the program plus whatever else ran there; how much the stranger got
/// is the scheduler's business. The only bound used is that a spinning
/// process pinned beside a spinning program gets a quarter of what the program
/// retires, where CFS gives it half.
#[test]
fn a_program_and_the_core_it_ran_on_are_counted_apart_in_one_run() {
    const ITERATIONS: u32 = 300_000;
    let core = a_core_of_ours();

    let alone = run_session(&instructions_session(core, ITERATIONS, false));
    if !alone.status.success() && machine_cannot_serve(&stderr(&alone)) {
        eprintln!(
            "skipped: this machine will not count instructions: {}",
            stderr(&alone)
        );
        return;
    }
    assert!(alone.status.success(), "{}", stderr(&alone));
    let alone: serde_json::Value = serde_json::from_slice(&alone.stdout).expect("json");
    let reference = counted(&alone, "program");

    let stranger = Stranger::on(core);
    let output = run_session(&instructions_session(core, ITERATIONS, true));
    drop(stranger);
    let message = stderr(&output);
    if !output.status.success() && machine_cannot_serve(&message) {
        eprintln!(
            "skipped: a per-cpu counter needs CAP_PERFMON on target/debug/omn (`make` grants \
             it), and this binary could not open one: {message}"
        );
        return;
    }
    assert!(
        output.status.success(),
        "a program beside a core is one workload beside a machine target, which a run \
         accepts: {message}"
    );
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    let program = counted(&document, "program");
    let on_core = counted(&document, "core");

    let ratio = program / reference;
    eprintln!(
        "core {core}: the program retired {program:.0} beside the stranger and {reference:.0} \
         alone ({ratio:.4}); the core retired {on_core:.0}, {:.2}x the program",
        on_core / program
    );
    assert!(
        (0.95..1.05).contains(&ratio),
        "the program's count must not carry the stranger: {program} beside it, {reference} \
         alone ({ratio:.3})"
    );
    assert!(
        on_core > 1.25 * program,
        "the core's count must carry the stranger: {on_core} on core {core} against \
         {program} for the program"
    );
    for label in ["program", "core"] {
        let stopped = results_labelled(&document, label)["stopped_because"]
            .as_str()
            .expect("a reason");
        assert!(
            stopped.contains("the launched program exited"),
            "the workload ends the run for every request in it, `{label}` included: {stopped}"
        );
    }
}

/// Two workloads are still one too many. Whose exit would end the run, and
/// which program would a launch mean, have no single answer.
#[test]
fn two_workloads_in_one_run_are_refused_by_name() {
    let output = run_session(
        "format: json\nrequests:\n  \
         - source: mem.rss\n    target: {launch: [/bin/true]}\n    mode: poll@50ms\n    \
         emit: [summary]\n  \
         - source: mem.rss\n    target: {pid: [1]}\n    mode: poll@50ms\n    emit: [summary]\n",
    );
    assert_eq!(
        output.status.code(),
        Some(EXIT_REJECTED),
        "{}",
        stderr(&output)
    );
    let message = stderr(&output);
    assert!(
        message.contains("one workload") && message.contains("pid"),
        "the refusal names both workloads and says what a run may hold: {message}"
    );
}

/// A resolved request for `source` on `target`, from the catalog this tree
/// embeds and this machine as it is.
fn resolved(
    source: &str,
    target: Target,
    mode: Mode,
    duration: Option<&str>,
) -> omn::resolve::ResolvedRequest {
    let catalog = omn::catalog::Catalog::embedded().expect("the embedded catalog loads");
    let capabilities = omn::capabilities::Capabilities::detect();
    let request = Request {
        source: source.parse().expect("a source id"),
        target,
        mode,
        emit: vec![Emit::Summary],
        keys: Vec::new(),
        filter: None,
        duration: duration.map(|text| text.parse().expect("a duration")),
        budget: None,
        stack: None,
        backend: None,
        label: None,
    };
    omn::resolve::resolve(&catalog, &capabilities, &request).expect("the request resolves here")
}

fn poll_every(text: &str) -> Mode {
    Mode::Poll {
        interval: text.parse().expect("an interval"),
    }
}

/// A program that writes its own `CLOCK_MONOTONIC` at its first line and at
/// its last, which is the independent clock the lifetime is checked against.
fn self_timing(report: &std::path::Path, sleep_s: f64) -> Vec<String> {
    vec![
        "python3".to_owned(),
        "-c".to_owned(),
        format!(
            "import time, sys\n\
             start = time.clock_gettime_ns(time.CLOCK_MONOTONIC)\n\
             time.sleep({sleep_s})\n\
             end = time.clock_gettime_ns(time.CLOCK_MONOTONIC)\n\
             open(sys.argv[1], 'w').write(f'{{start}} {{end}}')\n"
        ),
        report.display().to_string(),
    ]
}

fn scratch(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("omn-runner-{}-{name}", std::process::id()))
}

/// The launched program's lifetime, against the program's own clock.
///
/// The program reads `CLOCK_MONOTONIC` as its first statement and again just
/// before it exits. So the gate must have opened before the first read, and
/// the exit the run reports must be at or after the last one: those are the
/// two inequalities that hold whatever the machine is doing.
///
/// The collectors here poll once a second and the program lives a fifth of
/// that, which is the separation that shows the exit is taken from the pidfd:
/// without it the run notices the exit at its next tick, most of a second
/// late. And a machine target beside the program carries the same lifetime and
/// ends with it, because the program is the run's workload and the machine is
/// not.
#[test]
fn the_launched_program_s_lifetime_brackets_its_own_clock() {
    if Command::new("python3").arg("-V").output().is_err() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let report = scratch("lifetime");
    let tick = "1s";
    let requests = vec![
        resolved(
            "mem.rss",
            Target::Launch(self_timing(&report, 0.2)),
            poll_every(tick),
            None,
        ),
        resolved("cpu.util", Target::System, poll_every(tick), None),
    ];
    let collections: Vec<Collection> = collect::run_all(&requests).expect("the run completes");
    let (start_ns, end_ns) = {
        let text = std::fs::read_to_string(&report).expect("the program wrote its clock");
        let _ = std::fs::remove_file(&report);
        let mut fields = text
            .split_whitespace()
            .map(|field| field.parse::<u64>().unwrap());
        (fields.next().unwrap(), fields.next().unwrap())
    };

    let lifetime = collections[0]
        .lifetime
        .expect("a run that launched a program says when it ran");
    assert!(!lifetime.terminated, "the program ended by itself");
    assert!(
        lifetime.released_ns <= start_ns,
        "the gate opened at {} and the program read its clock at {start_ns}, before it",
        lifetime.released_ns
    );
    assert!(
        end_ns <= lifetime.exited_ns,
        "the program read its clock at {end_ns} and the run says it had exited by {}",
        lifetime.exited_ns
    );
    assert!(
        lifetime.exited_err_ns <= lifetime.exited_ns - lifetime.released_ns,
        "the exit's uncertainty cannot be longer than the program lived: {lifetime:?}"
    );
    let late_ns = lifetime.exited_ns - end_ns;
    assert!(
        late_ns < 500_000_000,
        "with a {tick} tick the exit was seen {late_ns} ns after the program's last read: \
         the run waited for its tick rather than for the program"
    );
    eprintln!(
        "lifetime: released {} ns before the program's first read; exit seen {late_ns} ns after \
         its last, bracketed to {} ns",
        start_ns - lifetime.released_ns,
        lifetime.exited_err_ns
    );

    let machine = &collections[1];
    assert_eq!(
        machine.lifetime,
        Some(lifetime),
        "the machine target beside the program shares its window, so it carries its whole"
    );
    for collection in &collections {
        assert!(
            collection
                .stopped_because
                .contains("the launched program exited"),
            "the program ends the run for the machine target too: {}",
            collection.stopped_because
        );
    }
}

/// A program the run had to stop says so, and its end is the tool's deadline
/// rather than anything it did.
#[test]
fn a_program_the_run_outlived_is_marked_terminated() {
    let requests = vec![resolved(
        "mem.rss",
        Target::Launch(vec!["sleep".to_owned(), "30".to_owned()]),
        poll_every("50ms"),
        Some("300ms"),
    )];
    let collections = collect::run_all(&requests).expect("the run completes");
    let lifetime = collections[0].lifetime.expect("the program was launched");
    assert!(lifetime.terminated, "{lifetime:?}");
    let lived_ns = lifetime.exited_ns - lifetime.released_ns;
    assert!(
        lived_ns >= 300_000_000,
        "it was stopped when 300 ms had elapsed, not before: {lived_ns} ns"
    );
    assert!(
        collections[0].stopped_because.contains("duration"),
        "{}",
        collections[0].stopped_because
    );
}

/// A run that launched nothing has no lifetime to report, and says nothing
/// rather than zero.
#[test]
fn a_run_that_launched_nothing_has_no_lifetime() {
    let requests = vec![resolved(
        "mem.rss",
        Target::Pid(vec![std::process::id() as i32]),
        poll_every("20ms"),
        Some("60ms"),
    )];
    let collections = collect::run_all(&requests).expect("the run completes");
    assert_eq!(collections[0].lifetime, None);
}

/// A program that allocates a known number of blocks of a known size through
/// `libc.malloc` itself, frees half, and exits.
fn known_allocations(count: usize, size: usize) -> String {
    format!(
        "import ctypes, time\n\
         libc = ctypes.CDLL(None)\n\
         libc.malloc.restype = ctypes.c_void_p\n\
         libc.free.argtypes = [ctypes.c_void_p]\n\
         blocks = [libc.malloc({size}) for _ in range({count})]\n\
         for block in blocks: ctypes.memset(block, 1, {size})\n\
         time.sleep(0.2)\n\
         for block in blocks[::2]: libc.free(block)\n\
         time.sleep(0.2)\n"
    )
}

fn heap_session(program: &str, with_live: bool) -> String {
    let mut session = format!(
        "format: json\nrequests:\n  \
         - label: alloc\n    source: mem.heap.alloc\n    \
         target: &program {{launch: [python3, -c, {program:?}]}}\n    \
         mode: trace\n    emit: [raw]\n"
    );
    if with_live {
        session.push_str(
            "  - label: live\n    source: mem.heap.live\n    target: *program\n    \
             mode: poll@50ms\n    emit: [summary]\n",
        );
    }
    session
}

/// How many allocations of exactly `size` bytes the `alloc` result saw.
fn allocations_of(document: &serde_json::Value, size: usize) -> usize {
    results_labelled(document, "alloc")["outputs"][0]["events"]
        .as_array()
        .expect("raw events")
        .iter()
        .filter(|event| {
            event["attrs"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|attr| attr["name"] == "op" && attr["value"] == "alloc")
                && event["attrs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|attr| attr["name"] == "bytes" && attr["value"] == size)
        })
        .count()
}

/// Two shim-backed sources on one launch read one pipe, and each sees all of
/// it.
///
/// This was refused before it started: each source opened a pipe of its own
/// and both wanted `OMN_HEAP_FD` to name it. Ground truth is the program's
/// arithmetic -- 512 blocks of 96 KiB, half freed -- and the same allocation
/// source run alone: sharing the stream must not cost the allocation source a
/// record, and the live source beside it must see the same heap it would see
/// alone.
#[test]
fn two_heap_sources_on_one_launch_share_the_shim_and_each_sees_all_of_it() {
    const COUNT: usize = 512;
    const SIZE: usize = 96 * 1024;
    const ALLOCATED: f64 = (COUNT * SIZE) as f64;
    if Command::new("python3").arg("-V").output().is_err() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let program = known_allocations(COUNT, SIZE);

    let alone = run_session(&heap_session(&program, false));
    assert!(alone.status.success(), "{}", stderr(&alone));
    let alone: serde_json::Value = serde_json::from_slice(&alone.stdout).expect("json");

    let output = run_session(&heap_session(&program, true));
    assert!(
        output.status.success(),
        "two heap sources on one program share one pipe: {}",
        stderr(&output)
    );
    let together: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");

    let by_alone = allocations_of(&alone, SIZE);
    let by_together = allocations_of(&together, SIZE);
    assert!(
        by_alone >= COUNT && by_together >= COUNT,
        "the program made {COUNT} allocations of {SIZE} bytes; alone the source saw \
         {by_alone}, beside another it saw {by_together}"
    );
    assert_eq!(
        by_alone, by_together,
        "sharing the pipe must not change what the allocation source sees"
    );
    for label in ["alloc", "live"] {
        assert_eq!(
            results_labelled(&together, label)["losses"]["dropped"],
            0,
            "the shim dropped nothing for `{label}`"
        );
    }

    let live = &results_labelled(&together, "live")["outputs"][0]["series"][0];
    let peak = live["max"].as_f64().expect("a peak");
    let last = live["last"].as_f64().expect("a last value");
    assert!(
        (ALLOCATED..1.5 * ALLOCATED).contains(&peak),
        "{ALLOCATED} bytes were live at the peak, measured {peak}"
    );
    assert!(
        (0.4 * ALLOCATED..0.75 * ALLOCATED).contains(&last),
        "half were freed, so about {} bytes are left, measured {last}",
        ALLOCATED / 2.0
    );
}

/// The lifetime reaches the results document, and not only the collection.
///
/// It is the *whole* a decomposition's parts are parts of: a residue is a
/// statement about a total, and a total nobody carries cannot be checked. The
/// run model stamps it; this asserts that it survives the emit layer into the
/// four structured formats, where a reader and the next layer can find it.
///
/// The bracket travels with it on purpose. `exited_err_ns` is how long before
/// `exited_ns` the program was last seen alive, and a reader given only the
/// length would over-trust a residue smaller than the bracket.
#[test]
fn the_lifetime_reaches_the_results_document_with_its_bracket() {
    if Command::new("python3").arg("-V").output().is_err() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let output = run(&[
        "mem.rss",
        "--mode",
        "poll@50ms",
        "--emit",
        "raw",
        "--duration",
        "2s",
        "--format",
        "json",
        "--",
        "python3",
        "-c",
        "import time; time.sleep(0.3)",
    ]);
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let document: serde_json::Value = match serde_json::from_str(&text) {
        Ok(document) => document,
        Err(error) => {
            // The launched program shares this stdout, so a run that printed
            // something is not this test's business to parse.
            eprintln!("skipped: the run did not produce one json document ({error})");
            return;
        }
    };
    let result = &document["results"][0];
    let lifetime = &result["lifetime"];
    assert!(
        !lifetime.is_null(),
        "a run that launched a program must carry when it ran: {text}"
    );

    let released = lifetime["released_ns"].as_u64().expect("a gate moment");
    let exited = lifetime["exited_ns"].as_u64().expect("an exit moment");
    let bracket = lifetime["exited_err_ns"]
        .as_u64()
        .expect("the exit's own uncertainty, which is what makes it honest");
    assert!(
        exited > released,
        "the program cannot have exited before the gate opened: {lifetime}"
    );
    // The program sleeps 300 ms, and the whole is bounded below by that and
    // above by the run's own duration. Not a calibrated threshold: a machine
    // that takes a second to start python is still inside it.
    let lived = exited - released;
    assert!(
        lived >= 300_000_000,
        "the program slept 300 ms and the run says it lived {lived} ns"
    );
    assert!(
        bracket > 0,
        "an exit this tool learned about by asking is never exact, and saying so is the point: \
         {lifetime}"
    );
    assert!(
        bracket < lived,
        "the bracket cannot be wider than the life it brackets: {lifetime}"
    );
}
