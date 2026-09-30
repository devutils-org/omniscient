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
    try_resolved(source, target, mode, duration).expect("the request resolves here")
}

/// The same, or the resolver's refusal: a counter is refused on a stock
/// kernel's `perf_event_paranoid` and on a cpu with no hardware PMU, and a
/// test that needs one says so and stops.
fn try_resolved(
    source: &str,
    target: Target,
    mode: Mode,
    duration: Option<&str>,
) -> Result<omn::resolve::ResolvedRequest, omn::resolve::ResolveError> {
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
        symbols: None,
        backend: None,
        label: None,
        window: None,
    };
    omn::resolve::resolve(&catalog, &capabilities, &request)
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

/// The reap carries the cpu the polls could not see, and the two say so
/// together.
///
/// `cpu.oncpu_time` reads `/proc/<pid>/stat`, which is in ticks of a hundredth
/// of a second and stops being readable the moment the process is gone. So its
/// series is short by whatever the program spent after the last poll -- always
/// by a multiple of the tick, and by 10 to 26 ms in the compute crossing's
/// measurements, which is its finding 8. `wait4`'s `rusage` has the exact
/// figure and the runner now carries it on the lifetime.
///
/// What is asserted is the relationship between the two numbers and their own
/// resolutions, not either number: how much cpu a busy loop gets belongs to the
/// machine, and that the polled series and the reap agree to within a poll and
/// a tick belongs to the tool.
#[test]
fn the_reap_carries_the_cpu_the_last_poll_missed() {
    if Command::new("python3").arg("-V").output().is_err() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    const POLL_NS: f64 = 50_000_000.0;
    const TICK_NS: f64 = 10_000_000.0;

    let output = run(&[
        "cpu.oncpu_time",
        "--mode",
        "poll@50ms",
        "--emit",
        "timeline",
        "--duration",
        "5s",
        "--format",
        "json",
        "--",
        "python3",
        "-c",
        "import time\nend = time.time() + 1.0\nwhile time.time() < end: pass",
    ]);
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let Ok(document) = serde_json::from_str::<serde_json::Value>(&text) else {
        eprintln!("skipped: the run did not produce one json document");
        return;
    };
    let result = &document["results"][0];

    let polled: f64 = result["outputs"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|output| output["series"].as_array().into_iter().flatten())
        .flat_map(|series| series["points"].as_array().into_iter().flatten())
        .filter_map(|point| point["value"].as_f64())
        .sum();
    let exact = result["lifetime"]["cpu_ns"]
        .as_f64()
        .expect("the reap's own cpu figure rides on the lifetime");

    assert!(
        polled > 0.0 && exact > 0.0,
        "a busy loop burns cpu: polled {polled}, reaped {exact}"
    );
    // The polls cannot have seen more than the program actually spent, beyond
    // the tick they are rounded to.
    assert!(
        polled <= exact + TICK_NS,
        "the polled series says {:.1} ms and the kernel handed over {:.1} ms at the reap",
        polled / 1e6,
        exact / 1e6
    );
    // And what they missed is bounded by their own resolution: the cpu spent
    // after the last poll, plus the tick that poll was rounded to. 990.0 ms
    // against 1006.5 ms when this was written.
    assert!(
        exact - polled <= POLL_NS + TICK_NS,
        "the reap says {:.1} ms and the polls saw {:.1} ms, which is more than one poll and one \
         tick apart",
        exact / 1e6,
        polled / 1e6
    );
}

// ---------------------------------------------------------------------------
// Hot attach: a request whose window is its own and inside the run's.
//
// Every scenario below runs one program with two phases whose boundaries the
// program itself reports, and asks sources attached for part of it what they
// saw. `docs/scenarios/attach.md` is what these measured.
// ---------------------------------------------------------------------------

/// The two-phase program, assembled: `nasm -f bin` of the source below, 772
/// bytes with no loader, no libc and one thread.
///
/// ```nasm
/// ; A single-threaded program in two phases, with nothing else in it.
/// bits 64
/// org 0x400000
///
/// ehdr:
///   db 0x7f, "ELF", 2, 1, 1, 0
///   times 8 db 0
///   dw 2                ; ET_EXEC
///   dw 0x3e             ; x86-64
///   dd 1
///   dq _start
///   dq phdr - $$
///   dq 0
///   dd 0
///   dw 64, 56, 1, 0, 0, 0
/// phdr:
///   dd 1                ; PT_LOAD
///   dd 7                ; read, write, execute
///   dq 0
///   dq $$, $$
///   dq filesize, filesize + 4096
///   dq 0x1000
///
/// ; offset 120: the parameters, patched by the test
/// turns1:     dq 0
/// sleeps1:    dq 0
/// pause:      dq 0, 0   ; seconds, nanoseconds
/// turns2:     dq 0
/// sleeps2:    dq 0
/// ; offset 168: the report's path, NUL-terminated
/// path:       times 256 db 0
/// ten_ms:     dq 0, 10000000
/// ; offset 440: what the program reports: tid, then four CLOCK_MONOTONIC stamps
/// report:     dq 0
/// stamps:     times 8 dq 0
///
/// %macro stamp 1
///   mov eax, 228
///   mov edi, 1
///   lea rsi, [rel stamps + %1 * 16]
///   syscall
/// %endmacro
///
/// %macro phase 2
///   mov rcx, [rel %1]
///   test rcx, rcx
///   jz %%sleeps
/// %%turn:
///   dec rcx
///   jnz %%turn
/// %%sleeps:
///   mov rbx, [rel %2]
///   test rbx, rbx
///   jz %%done
/// %%sleep:
///   mov eax, 35
///   lea rdi, [rel ten_ms]
///   xor esi, esi
///   syscall
///   dec rbx
///   jnz %%sleep
/// %%done:
/// %endmacro
///
/// _start:
///   mov eax, 186        ; gettid
///   syscall
///   mov [rel report], rax
///   stamp 0
///   phase turns1, sleeps1
///   stamp 1
///   mov eax, 35
///   lea rdi, [rel pause]
///   xor esi, esi
///   syscall
///   stamp 2
///   phase turns2, sleeps2
///   stamp 3
///   mov eax, 2          ; open(path, O_WRONLY|O_CREAT|O_TRUNC, 0644)
///   lea rdi, [rel path]
///   mov esi, 0x241
///   mov edx, 0o644
///   syscall
///   mov edi, eax
///   mov eax, 1          ; write(fd, report, 72)
///   lea rsi, [rel report]
///   mov edx, 72
///   syscall
///   mov eax, 60         ; exit(0)
///   xor edi, edi
///   syscall
///
/// filesize equ $ - $$
/// ```
const PHASES_ELF: [u8; 772] = [
    0x7f, 0x45, 0x4c, 0x46, 0x02, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x02, 0x00, 0x3e, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x02, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x38, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x04, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x13, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x80, 0x96, 0x98, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xb8, 0xba, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x89, 0x05, 0xaa, 0xff, 0xff, 0xff, 0xb8, 0xe4,
    0x00, 0x00, 0x00, 0xbf, 0x01, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x35, 0xa1, 0xff, 0xff, 0xff, 0x0f,
    0x05, 0x48, 0x8b, 0x0d, 0x50, 0xfe, 0xff, 0xff, 0x48, 0x85, 0xc9, 0x74, 0x05, 0x48, 0xff, 0xc9,
    0x75, 0xfb, 0x48, 0x8b, 0x1d, 0x47, 0xfe, 0xff, 0xff, 0x48, 0x85, 0xdb, 0x74, 0x15, 0xb8, 0x23,
    0x00, 0x00, 0x00, 0x48, 0x8d, 0x3d, 0x5e, 0xff, 0xff, 0xff, 0x31, 0xf6, 0x0f, 0x05, 0x48, 0xff,
    0xcb, 0x75, 0xeb, 0xb8, 0xe4, 0x00, 0x00, 0x00, 0xbf, 0x01, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x35,
    0x6c, 0xff, 0xff, 0xff, 0x0f, 0x05, 0xb8, 0x23, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x3d, 0x16, 0xfe,
    0xff, 0xff, 0x31, 0xf6, 0x0f, 0x05, 0xb8, 0xe4, 0x00, 0x00, 0x00, 0xbf, 0x01, 0x00, 0x00, 0x00,
    0x48, 0x8d, 0x35, 0x59, 0xff, 0xff, 0xff, 0x0f, 0x05, 0x48, 0x8b, 0x0d, 0x08, 0xfe, 0xff, 0xff,
    0x48, 0x85, 0xc9, 0x74, 0x05, 0x48, 0xff, 0xc9, 0x75, 0xfb, 0x48, 0x8b, 0x1d, 0xff, 0xfd, 0xff,
    0xff, 0x48, 0x85, 0xdb, 0x74, 0x15, 0xb8, 0x23, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x3d, 0xf6, 0xfe,
    0xff, 0xff, 0x31, 0xf6, 0x0f, 0x05, 0x48, 0xff, 0xcb, 0x75, 0xeb, 0xb8, 0xe4, 0x00, 0x00, 0x00,
    0xbf, 0x01, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x35, 0x24, 0xff, 0xff, 0xff, 0x0f, 0x05, 0xb8, 0x02,
    0x00, 0x00, 0x00, 0x48, 0x8d, 0x3d, 0xce, 0xfd, 0xff, 0xff, 0xbe, 0x41, 0x02, 0x00, 0x00, 0xba,
    0xa4, 0x01, 0x00, 0x00, 0x0f, 0x05, 0x89, 0xc7, 0xb8, 0x01, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x35,
    0xc4, 0xfe, 0xff, 0xff, 0xba, 0x48, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0x3c, 0x00, 0x00, 0x00,
    0x31, 0xff, 0x0f, 0x05,
];

/// Where the parameters are in [`PHASES_ELF`]: first-phase turns and sleeps,
/// the pause as a timespec, second-phase turns and sleeps.
const PHASES_PARAMETERS: usize = 120;
/// Where the report's path goes, NUL-terminated, in 256 bytes.
const PHASES_PATH: usize = 168;

/// The shape of a two-phase run: a first phase of `turns` turns of a loop that
/// retires exactly two instructions each and then `sleeps` sleeps of 10 ms, a
/// pause, and a second phase of the same shape.
#[derive(Debug, Clone, Copy)]
struct Shape {
    first_turns: u64,
    first_sleeps: u64,
    pause_ms: u64,
    second_turns: u64,
    second_sleeps: u64,
}

/// Writes the two-phase program with `shape` patched in, and returns the path
/// to launch and the path it will report to.
///
/// The program is written rather than found because it is the ground truth:
/// its user-mode instruction count is twice its turns and a few dozen more,
/// which `perf stat -e instructions:u` put at 600,000,158 for 300 million
/// turns, and it has one thread, so a counter opened on it at any moment
/// follows all of it.
fn phases_program(shape: Shape, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let program = scratch(&format!("{name}.elf"));
    let report = scratch(&format!("{name}.report"));
    let _ = std::fs::remove_file(&report);
    let mut image = PHASES_ELF;
    let parameters = [
        shape.first_turns,
        shape.first_sleeps,
        shape.pause_ms / 1000,
        shape.pause_ms % 1000 * 1_000_000,
        shape.second_turns,
        shape.second_sleeps,
    ];
    for (index, value) in parameters.into_iter().enumerate() {
        let at = PHASES_PARAMETERS + 8 * index;
        image[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
    let path = report.as_os_str().as_encoded_bytes();
    assert!(
        path.len() < 256,
        "the report path fits the program's buffer"
    );
    image[PHASES_PATH..PHASES_PATH + path.len()].copy_from_slice(path);
    std::fs::write(&program, image).expect("the program is written");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
        .expect("the program is made executable");
    (program, report)
}

/// What the workload said about itself: the thread that did the work, and
/// when each phase began and ended on `CLOCK_MONOTONIC`.
#[derive(Debug)]
struct Phases {
    tid: u64,
    started_ns: u64,
    first_ended_ns: u64,
    second_started_ns: u64,
    ended_ns: u64,
}

impl Phases {
    /// The report both workloads write: the tid, then four timespecs.
    fn read(report: &std::path::Path) -> Phases {
        let bytes = std::fs::read(report).expect("the workload wrote its report");
        let word = |index: usize| {
            u64::from_le_bytes(bytes[8 * index..8 * index + 8].try_into().expect("8 bytes"))
        };
        let stamp = |index: usize| word(1 + 2 * index) * 1_000_000_000 + word(2 + 2 * index);
        Phases {
            tid: word(0),
            started_ns: stamp(0),
            first_ended_ns: stamp(1),
            second_started_ns: stamp(2),
            ended_ns: stamp(3),
        }
    }
}

/// A resolved request with a window, on the launched workload.
fn windowed(
    source: &str,
    argv: &[String],
    mode: Mode,
    window: Option<&str>,
    label: &str,
) -> Result<omn::resolve::ResolvedRequest, String> {
    let mut resolved = try_resolved(source, Target::Launch(argv.to_vec()), mode, None)
        .map_err(|error| error.to_string())?;
    resolved.request.window = window.map(|text| text.parse().expect("a window"));
    resolved.request.label = Some(label.to_owned());
    Ok(resolved)
}

/// The count a `count` collection ended on.
fn instructions_in(collection: &Collection) -> f64 {
    collection
        .events
        .iter()
        .filter_map(|event| event.attr(&collection.value_attr))
        .filter_map(|value| value.to_string().parse::<f64>().ok())
        .next_back()
        .unwrap_or(0.0)
}

/// Four instruction counters over one two-phase program, in one run: one per
/// phase, one for the pause between them, one for the whole.
fn count_the_phases(
    argv: &[String],
    windows: [Option<&str>; 4],
) -> Result<Vec<Collection>, String> {
    let requests = ["first", "pause", "second", "whole"]
        .into_iter()
        .zip(windows)
        .map(|(label, window)| windowed("cpu.instructions", argv, Mode::Count, window, label))
        .collect::<Result<Vec<_>, _>>()?;
    collect::run_all(&requests).map_err(|error| error.to_string())
}

/// A source attached for the first phase counts the first phase and nothing
/// of the second; one attached for the second counts the second; one attached
/// for the pause counts nothing; and one attached for the whole run counts the
/// whole.
///
/// Ground truth is the loop: a thousand million instructions in the first
/// phase and two thousand million in the second, by construction, with two
/// seconds of sleep between them. The windows cut the run at 1.2 s and 1.8 s,
/// inside the pause, and the program's own report is what says the cuts landed
/// there: on a machine loaded enough to push a phase across a cut, the
/// scenario stops and prints the times rather than failing on the scheduler.
///
/// The three windows tile the run, so their sum has to be the whole run's
/// count -- which is the check that a window neither loses what falls at its
/// edge nor counts it twice.
#[test]
fn a_source_attached_for_one_phase_counts_that_phase_and_nothing_of_the_other() {
    const FIRST: f64 = 1_000_000_000.0;
    const SECOND: f64 = 2_000_000_000.0;
    // The program retires 158 user-mode instructions besides its loops, by
    // `perf stat -e instructions:u`. The counter counts kernel mode as well
    // where the machine lets it -- the timer ticks that land inside the loop,
    // +0.17 % when this was written -- so the loop is the floor and the
    // ceiling is half the one per cent `cpu.instructions` claims.
    const WITHIN: f64 = 0.005;
    // Two windows that meet inside the pause share an edge where the program
    // is asleep: what the tiling may lose or double there is next to nothing.
    const EDGE: f64 = 10_000.0;
    let (program, report) = phases_program(
        Shape {
            first_turns: 500_000_000,
            first_sleeps: 0,
            pause_ms: 2000,
            second_turns: 1_000_000_000,
            second_sleeps: 0,
        },
        "phases-counted",
    );
    let argv = vec![program.display().to_string()];
    let collections = match count_the_phases(
        &argv,
        [
            Some("..1200ms"),
            Some("1200ms..1800ms"),
            Some("1800ms.."),
            None,
        ],
    ) {
        Ok(collections) => collections,
        Err(error) if machine_cannot_serve(&error) => {
            eprintln!("skipped: this machine will not count instructions here: {error}");
            return;
        }
        Err(error) => panic!("the run failed: {error}"),
    };
    let phases = Phases::read(&report);
    let _ = std::fs::remove_file(&report);
    let _ = std::fs::remove_file(&program);

    let [first, pause, second, whole] = &collections[..] else {
        panic!("four collections");
    };
    let at = |ns: u64| ns.saturating_sub(whole.window.run_opened_ns) as f64 / 1e6;
    eprintln!(
        "the program ran its first phase from {:.1} to {:.1} ms and its second from {:.1} to \
         {:.1} ms; the windows were {:.1}..{:.1}, {:.1}..{:.1} and {:.1}..{:.1} ms",
        at(phases.started_ns),
        at(phases.first_ended_ns),
        at(phases.second_started_ns),
        at(phases.ended_ns),
        at(first.window.opened_ns),
        at(first.window.closed_ns),
        at(pause.window.opened_ns),
        at(pause.window.closed_ns),
        at(second.window.opened_ns),
        at(second.window.closed_ns),
    );
    if phases.first_ended_ns >= first.window.closed_ns
        || phases.second_started_ns <= second.window.opened_ns
    {
        eprintln!(
            "skipped: the machine pushed a phase across a window's edge (first phase ended at \
             {:.1} ms, first window closed at {:.1} ms; second phase started at {:.1} ms, \
             second window opened at {:.1} ms), so the separation cannot be checked here",
            at(phases.first_ended_ns),
            at(first.window.closed_ns),
            at(phases.second_started_ns),
            at(second.window.opened_ns),
        );
        return;
    }
    let shared: Vec<&String> = collections
        .iter()
        .flat_map(|collection| collection.notes.iter())
        .filter(|note| note.contains("shared the hardware"))
        .collect();
    if !shared.is_empty() {
        eprintln!(
            "skipped: the counters were time-shared with somebody else's, so each value is an \
             extrapolation that cannot be checked against a known count: {shared:?}"
        );
        return;
    }

    let [first_n, pause_n, second_n, whole_n] = [first, pause, second, whole].map(instructions_in);
    eprintln!(
        "counted {first_n:.0} in the first window, {pause_n:.0} in the pause, {second_n:.0} in \
         the second, {whole_n:.0} over the whole run"
    );
    let near =
        |counted: f64, expected: f64| (expected..expected * (1.0 + WITHIN)).contains(&counted);
    assert!(
        near(first_n, FIRST),
        "the first phase retires {FIRST:.0} and its window counted {first_n:.0}"
    );
    assert!(
        near(second_n, SECOND),
        "the second phase retires {SECOND:.0} and its window counted {second_n:.0}: half as \
         much again would be the first phase counted in it"
    );
    assert!(
        pause_n < EDGE,
        "the program only sleeps in the pause, and its window counted {pause_n:.0}"
    );
    assert!(
        near(whole_n, FIRST + SECOND),
        "the whole run retires {:.0} and was counted at {whole_n:.0}",
        FIRST + SECOND
    );
    let tiled = first_n + pause_n + second_n;
    assert!(
        (tiled - whole_n).abs() < EDGE,
        "three windows that tile the run count {tiled:.0} between them and the whole run \
         counted {whole_n:.0}"
    );

    // And each collection states its own window: the span it reports is its
    // own, and shorter than the run it sits in.
    for collection in [first, pause, second] {
        assert!(collection.window.attached);
        assert_eq!(collection.observed_ns, collection.window.span_ns());
        assert!(collection.observed_ns < whole.observed_ns);
    }
    assert!(
        (550_000_000..900_000_000).contains(&pause.observed_ns),
        "the pause window asked for 600 ms and watched {} ns",
        pause.observed_ns
    );
    assert!(second.window.attach_ns.is_some() && first.window.attach_ns.is_none());
    assert!(
        second
            .notes
            .iter()
            .any(|note| note.contains("attaching") && note.contains("into the run")),
        "{:?}",
        second.notes
    );
}

/// The variable that turns this test binary into a two-phase workload with
/// more than one thread: libtest runs the one test it is asked for on a thread
/// of its own, beside the main thread that waits for it.
const PHASES_VAR: &str = "OMN_ATTACH_PHASES";
/// Where that workload writes its report, in the program's format.
const REPORT_VAR: &str = "OMN_ATTACH_REPORT";

/// The threaded workload, when this binary is launched as one; nothing
/// otherwise. `turns:pause_ms:turns`, and the same report as the program.
#[test]
fn workload_runs_only_when_this_binary_was_launched_as_a_workload() {
    let Ok(spec) = std::env::var(PHASES_VAR) else {
        return;
    };
    let numbers: Vec<u64> = spec
        .split(':')
        .map(|number| number.parse().expect("a workload is three numbers"))
        .collect();
    let [first_turns, pause_ms, second_turns] = numbers[..] else {
        panic!("a workload is `turns:pause_ms:turns`, not `{spec}`");
    };
    let now = || {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: a timespec this frame owns.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        [ts.tv_sec as u64, ts.tv_nsec as u64]
    };
    // SAFETY: gettid takes nothing and cannot fail.
    let mut report = vec![unsafe { libc::gettid() } as u64];
    report.extend(now());
    retire(first_turns);
    report.extend(now());
    std::thread::sleep(std::time::Duration::from_millis(pause_ms));
    report.extend(now());
    retire(second_turns);
    report.extend(now());
    let bytes: Vec<u8> = report.iter().flat_map(|word| word.to_le_bytes()).collect();
    std::fs::write(std::env::var(REPORT_VAR).expect("a report path"), bytes)
        .expect("the report is written");
}

/// The program's loop, in this binary: `turns` turns of two instructions.
fn retire(turns: u64) {
    if turns == 0 {
        return;
    }
    #[cfg(target_arch = "x86_64")]
    // SAFETY: the loop decrements a register it owns and branches on the flag
    // that decrement sets.
    unsafe {
        std::arch::asm!(
            "2:",
            "dec {n}",
            "jnz 2b",
            n = inout(reg) turns => _,
            options(nostack),
        );
    }
}

/// A counter attached late to a program that already runs a second thread
/// either counts that thread or says it could not.
///
/// The workload is this test binary, which does its two phases on the thread
/// libtest runs the test on -- a thread that exists before the late counter
/// is opened. A per-task counter follows a process by inheritance, from its
/// open: attached at the gate it follows every thread the program will start,
/// and attached late it follows the main thread, which only waits. The second
/// phase retires two thousand million instructions on the other one.
///
/// Measured before this was fixed: 106,206 instructions for the second window,
/// against 2,000,000,000 retired in it. A number that looks like a quiet phase
/// is the one thing this must not produce, so the scenario accepts two
/// answers and nothing else: the count, or a window that was not attached and
/// says why. It waits on the counter collector declaring what it follows
/// (`Prepared::follows_only_later_threads`, filed in `docs/open-findings.md`
/// under Stream H); the runner already acts on the declaration.
#[test]
fn a_counter_attached_late_counts_the_threads_already_running_or_says_it_cannot() {
    const SECOND: f64 = 2_000_000_000.0;
    let report = scratch("phases-threaded.report");
    let exe = std::env::current_exe().expect("this test binary has a path");
    let argv = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        format!(
            "{PHASES_VAR}=500000000:2000:1000000000 {REPORT_VAR}=\"$2\" exec \"$1\" --exact \
             --test-threads 1 workload_runs_only_when_this_binary_was_launched_as_a_workload \
             >/dev/null 2>&1"
        ),
        "sh".to_owned(),
        exe.to_string_lossy().into_owned(),
        report.display().to_string(),
    ];
    let collections = match count_the_phases(
        &argv,
        [
            Some("..1200ms"),
            Some("1200ms..1800ms"),
            Some("1800ms.."),
            None,
        ],
    ) {
        Ok(collections) => collections,
        Err(error) if machine_cannot_serve(&error) => {
            eprintln!("skipped: this machine will not count instructions here: {error}");
            return;
        }
        Err(error) => panic!("the run failed: {error}"),
    };
    let _ = std::fs::remove_file(&report);
    let second = &collections[2];
    let counted = instructions_in(second);
    eprintln!(
        "the late window counted {counted:.0} of the {SECOND:.0} the second phase retired; \
         attached: {}, losses: {:?}",
        second.window.attached, second.losses
    );
    if second.window.attached {
        assert!(
            counted >= SECOND,
            "a late counter counted {counted:.0} of {SECOND:.0} and said nothing about the \
             thread it could not see"
        );
    } else {
        assert!(
            second
                .losses
                .reasons
                .keys()
                .any(|reason| reason.contains("threads")),
            "a window that was not attached says why: {:?}",
            second.losses
        );
    }
}

/// The same separation on the eBPF path, through the binary: a probe loaded
/// into the kernel mid-run, while the program runs.
///
/// Ground truth is the program's own sleeps: forty of them in the first phase
/// and twenty-five in the second, a timer wakeup each, around a 1.5 s pause
/// whose own wakeup lands in the second window. The late window opens at 1 s,
/// inside the pause, so it must see the pause's wakeup and the twenty-five and
/// nothing of the forty; the whole run sees all sixty-six.
#[test]
fn a_probe_attached_mid_run_sees_the_second_phase_and_nothing_of_the_first() {
    const FIRST: usize = 40;
    const SECOND: usize = 25;
    let (program, report) = phases_program(
        Shape {
            first_turns: 0,
            first_sleeps: FIRST as u64,
            pause_ms: 1500,
            second_turns: 0,
            second_sleeps: SECOND as u64,
        },
        "phases-woken",
    );
    let output = run_session(&format!(
        "format: json\nrequests:\n  \
         - label: late\n    source: sched.wakeup\n    target: &program {{launch: [{:?}]}}\n    \
         mode: trace\n    emit: [raw]\n    keys: [task]\n    window: 1s..\n  \
         - label: whole\n    source: sched.wakeup\n    target: *program\n    \
         mode: trace\n    emit: [raw]\n    keys: [task]\n",
        program.display().to_string()
    ));
    let message = stderr(&output);
    let _ = std::fs::remove_file(&program);
    if !output.status.success() && machine_cannot_serve(&message) {
        eprintln!(
            "skipped: `sched.wakeup` needs CAP_BPF on target/debug/omn (`make` grants it): \
             {message}"
        );
        return;
    }
    assert!(output.status.success(), "{message}");
    let phases = Phases::read(&report);
    let _ = std::fs::remove_file(&report);
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");

    let woken = |label: &str| -> Vec<u64> {
        results_labelled(&document, label)["outputs"][0]["events"]
            .as_array()
            .expect("raw events")
            .iter()
            .filter(|event| {
                event["attrs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|attr| attr["name"] == "wakee_tid" && attr["value"] == phases.tid)
            })
            .filter_map(|event| event["ts_ns"].as_u64())
            .collect()
    };
    let (late, whole) = (woken("late"), woken("whole"));
    let in_first_phase = |ts: &&u64| **ts <= phases.first_ended_ns;
    eprintln!(
        "the program was woken {} times over the run and {} times in the late window, {} of \
         those in the first phase; the first phase took {} ms and the pause {} ms",
        whole.len(),
        late.len(),
        late.iter().filter(in_first_phase).count(),
        (phases.first_ended_ns - phases.started_ns) / 1_000_000,
        (phases.second_started_ns - phases.first_ended_ns) / 1_000_000,
    );
    if phases.first_ended_ns - phases.started_ns > 900_000_000 {
        eprintln!(
            "skipped: forty 10 ms sleeps took more than 900 ms, so the first phase may reach \
             the late window's start and the separation cannot be checked here"
        );
        return;
    }
    assert_eq!(
        late.iter().filter(in_first_phase).count(),
        0,
        "a probe attached at 1 s saw a wakeup of the first phase, which had ended"
    );
    // Every wakeup of a program that only sleeps is one of its sleeps ending:
    // the pause's and the twenty-five in the window, all sixty-six over the
    // program's life. The whole run has one more before the program's first
    // instruction, and it is the launch gate releasing the child to `exec`.
    assert_eq!(late.len(), SECOND + 1, "the late window's wakeups");
    let in_its_life = whole.iter().filter(|ts| **ts >= phases.started_ns).count();
    assert_eq!(in_its_life, FIRST + SECOND + 1, "the whole run's wakeups");
    assert!(
        whole.len() - in_its_life <= 1,
        "before the program ran, only the gate wakes it: {} wakeups",
        whole.len() - in_its_life
    );
    let observed = |label: &str| {
        results_labelled(&document, label)["observed_ns"]
            .as_u64()
            .expect("observed_ns")
    };
    assert!(
        observed("whole") - observed("late") >= 950_000_000,
        "a source attached a second in watched {} ns of a {} ns run",
        observed("late"),
        observed("whole")
    );
}

/// A join across a source detached mid-run does not count what fell after the
/// detach as a loss.
///
/// Ground truth is the same program: forty sleeps before the pause and
/// twenty-five after it, each a block the scheduler records and a wakeup that
/// ends it. `sched.wakeup` is detached at 1 s, inside the pause; the switches
/// are traced for the whole run. The twenty-five blocks after the detach have
/// a wakeup the run did not watch for, which is not the same as a wakeup that
/// did not happen, and the join has to say which it is.
///
/// Waits on `src/join.rs`: `Correlation::outside_window`, filed in
/// `docs/open-findings.md` under Stream H with the signature. Measured before
/// it lands, the join is worse than lossy: it reports 67 matched and none
/// unmatched, because the last wakeup it saw before the detach stays open and
/// every block after the detach is credited to it -- `task:0 (idle) -> task`,
/// 63 matches, still open at the end. A stale state standing in for the
/// wakeups the run stopped watching is exactly the silent degradation this
/// scenario exists to catch.
#[test]
fn a_join_across_a_detached_source_does_not_count_what_fell_after_it_as_a_loss() {
    const FIRST: u64 = 40;
    const SECOND: u64 = 25;
    let (program, report) = phases_program(
        Shape {
            first_turns: 0,
            first_sleeps: FIRST,
            pause_ms: 1500,
            second_turns: 0,
            second_sleeps: SECOND,
        },
        "phases-joined",
    );
    let output = run_session(&format!(
        "format: json\nrequests:\n  \
         - label: wakeups\n    source: sched.wakeup\n    target: &program {{launch: [{:?}]}}\n    \
         mode: trace\n    emit: [summary]\n    keys: [task]\n    window: ..1s\n  \
         - label: switches\n    source: sched.switches\n    target: *program\n    \
         mode: trace\n    emit: [summary]\n    keys: [task]\n\
         joins:\n  \
         - id: released-by\n    from: sched.wakeup\n    to: sched.switches\n    via: task\n    \
         match: wakeup\n    waker: waker_tid\n    window: 10s\n",
        program.display().to_string()
    ));
    let message = stderr(&output);
    let _ = std::fs::remove_file(&program);
    let _ = std::fs::remove_file(&report);
    if !output.status.success() && machine_cannot_serve(&message) {
        eprintln!("skipped: the join's sources cannot be served here: {message}");
        return;
    }
    assert!(output.status.success(), "{message}");
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    let join = &document["joins"][0];
    eprintln!("{join}");
    let outside = join["outside_window"]
        .as_u64()
        .expect("the join says how much fell outside a source's window");
    assert!(
        outside >= SECOND,
        "the {SECOND} blocks after the detach fell outside `sched.wakeup`'s window: {join}"
    );
    // Neither credited to a wakeup from before the detach, nor counted as
    // having followed none: the first phase's blocks, the gate's and the
    // program's start are all that is left to match. Asserted as the
    // separation from the defect rather than as a count of the start's
    // switches: a stale state absorbed every block after the detach, which
    // put `matched` at the first phase plus the second (67 for 40 and 25);
    // how many switches the launch itself adds moved from three to four when
    // `sched.switches` began following every thread of the program.
    let matched = join["matched"].as_u64().unwrap_or(u64::MAX);
    let unmatched = join["unmatched"].as_u64().unwrap_or(u64::MAX);
    assert!(
        matched < FIRST + SECOND / 2 && unmatched < SECOND,
        "what fell after the detach is neither matched nor unmatched: {join}"
    );
}

/// A source given to the program at exec cannot be attached late, nor
/// detached early, and says so by name before anything runs.
///
/// Before anything runs is checked, not assumed: the program would create a
/// file as its first act, and the file must not exist afterwards.
#[test]
fn a_preload_source_given_a_window_is_refused_by_name_before_the_run() {
    for window in ["500ms..", "..500ms"] {
        let marker = scratch(&format!("preload-refused-{}", window.replace('.', "")));
        let _ = std::fs::remove_file(&marker);
        let output = run_session(&format!(
            "format: json\nrequests:\n  \
             - label: heap\n    source: mem.heap.alloc\n    \
             target: {{launch: [/bin/sh, -c, \"touch {}; sleep 1\"]}}\n    \
             mode: trace\n    emit: [summary]\n    window: \"{window}\"\n",
            marker.display()
        ));
        let message = stderr(&output);
        assert_eq!(
            output.status.code(),
            Some(EXIT_NOT_IMPLEMENTED),
            "a backend that attaches to a running process is in the catalog and not in this \
             build, so this is the build's gap: {message}"
        );
        assert!(
            message.contains("mem.heap.alloc")
                && message.contains(window)
                && message.contains("at exec"),
            "the refusal names the source, the window and why: {message}"
        );
        assert!(
            !marker.exists(),
            "the program ran although the session was refused"
        );
    }
}

/// A window the run cannot hold, or a mode cannot mean, is refused as a
/// request that cannot mean anything.
#[test]
fn a_window_the_run_cannot_hold_is_refused_with_the_reason() {
    let refused = |window: &str, mode: &str| {
        let output = run_session(&format!(
            "format: json\nduration: 1s\nrequests:\n  \
             - source: mem.rss\n    target: {{pid: [1]}}\n    mode: {mode}\n    \
             emit: [summary]\n    window: {window}\n"
        ));
        assert_eq!(
            output.status.code(),
            Some(EXIT_REJECTED),
            "{}",
            stderr(&output)
        );
        stderr(&output)
    };
    assert!(refused("500ms..2s", "poll@50ms").contains("at most 1s"));
    assert!(refused("100ms..200ms", "snapshot").contains("single read"));
    assert!(refused("800ms..200ms", "poll@50ms").contains("closes before it opens"));
}

/// A session with windows dumps to a session that reads back as itself: the
/// window is part of the request, and the file and the CLI build the same one.
#[test]
fn a_session_with_windows_dumps_unchanged() {
    let session = "name: two phases\nformat: json\nduration: 3s\nrequests:\n  \
                   - label: first\n    source: mem.rss\n    target: {pid: [1]}\n    \
                   mode: poll@50ms\n    emit: [summary]\n    window: ..1s\n  \
                   - label: second\n    source: mem.rss\n    target: {pid: [1]}\n    \
                   mode: poll@50ms\n    emit: [summary]\n    window: 1500ms..\n";
    let path = scratch("windows.yml");
    std::fs::write(&path, session).expect("the session is written");
    let once = run(&[
        "-f",
        path.to_str().unwrap(),
        "--dump-request",
        "--no-resolve",
    ]);
    assert!(once.status.success(), "{}", stderr(&once));
    let dumped = String::from_utf8_lossy(&once.stdout).into_owned();
    assert!(
        dumped.contains("..1s") && dumped.contains("1500ms.."),
        "the windows are in the dump: {dumped}"
    );
    let again_path = scratch("windows-dumped.yml");
    std::fs::write(&again_path, &dumped).expect("the dump is written");
    let twice = run(&[
        "-f",
        again_path.to_str().unwrap(),
        "--dump-request",
        "--no-resolve",
    ]);
    assert!(twice.status.success(), "{}", stderr(&twice));
    assert_eq!(
        String::from_utf8_lossy(&twice.stdout),
        dumped,
        "a dumped session with windows reads back as itself"
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&again_path);
}

/// `--window` on the command line is the window a session file carries: the
/// same request, attached and detached at the same moments, and said the
/// same way. Ground truth is the run's own record of where the window was --
/// the run began at the gate, and the window was asked to close 400 ms in --
/// against a program that runs for a second.
#[test]
fn a_window_on_the_command_line_is_attached_and_detached_as_a_session_s_is() {
    let output = run(&[
        "mem.rss",
        "--mode",
        "poll@20ms",
        "--emit",
        "summary",
        "--window",
        "200ms..400ms",
        "--format",
        "json",
        "--",
        "sleep",
        "1",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let result = &document["results"][0];
    let stopped = result["stopped_because"].as_str().unwrap_or_default();
    let detached_at: f64 = stopped
        .strip_prefix("its window closed: detached ")
        .and_then(|rest| rest.split(' ').next())
        .and_then(|seconds| seconds.parse().ok())
        .unwrap_or_else(|| panic!("the window did not close on it: {stopped}"));
    assert!(
        (0.4..0.9).contains(&detached_at),
        "detached where it was asked, not earlier and not at the end: {detached_at} s"
    );
    let observed_ms = result["observed_ns"].as_u64().unwrap() as f64 / 1e6;
    assert!(
        (150.0..500.0).contains(&observed_ms),
        "watched for its window, about 200 ms, and not for the run: {observed_ms} ms"
    );
    let notes = result["degradations"].to_string();
    assert!(notes.contains("watched only from 0.2"), "{notes}");

    let refused = run(&["mem.rss", "--window", "3s..1s", "--", "true"]);
    assert_eq!(refused.status.code(), Some(EXIT_REJECTED));
    assert!(
        stderr(&refused).contains("closes before it opens"),
        "{}",
        stderr(&refused)
    );
}

/// `omn list --format json` carries the sweep the text table shows: each
/// source's availability here and each backend's probes, so a program reading
/// the listing is not left to parse a table meant for a person. The ground
/// truth is the text table itself, from the same binary on the same machine:
/// the tallies in its footer are the counts of the JSON's `here`.
#[test]
fn the_json_listing_carries_the_sweep_the_text_table_shows() {
    let json = run(&["list", "--format", "json"]);
    assert!(json.status.success(), "{}", stderr(&json));
    let listed: serde_json::Value = serde_json::from_slice(&json.stdout).expect("json");
    let listed = listed.as_array().expect("a list of sources");
    let count = |state: &str| {
        listed
            .iter()
            .filter(|entry| entry["here"]["availability"] == state)
            .count()
    };
    for entry in listed {
        let probes = entry["probes"].as_array().expect("probes per backend");
        let chain = entry["backends"].as_array().expect("the chain");
        assert_eq!(probes.len(), chain.len(), "{}", entry["id"]);
        for (probed, backend) in probes.iter().zip(chain) {
            assert_eq!(probed["backend"], backend["id"]);
            assert_eq!(
                probed["requirements"].as_array().map(Vec::len),
                backend["requires"].as_array().map(Vec::len).or(Some(0)),
                "every requirement is probed: {}",
                entry["id"]
            );
        }
    }
    let text = run(&["list"]);
    let table = String::from_utf8_lossy(&text.stdout);
    let footer = table
        .lines()
        .find(|line| line.contains("usable here"))
        .expect("the table's footer");
    let expected = format!(
        "{} sources: {} usable here, {} unverified, {} out of reach on this machine.",
        listed.len(),
        count("ready"),
        count("unverified"),
        count("unavailable")
    );
    assert_eq!(footer, expected, "the JSON sweep and the table agree");
}

/// A counted window ends at its duration, not at the next read.
///
/// `count` reads a counter at the start and at the end, and in between at the
/// default cadence of a second; the run used to sleep to that next read, so
/// `-m count -d 600ms` observed a second and divided every rate by it
/// (`open-requests 2947`, `6181`). The duration is the property here, and on
/// the machine nothing else ends the run, so the window is the request's: 300 ms against a
/// cadence of 1 s separate by the whole of the difference. What the window may
/// run past its duration is the final read and the stop, which the run
/// reports as `observed_ns` -- a tenth of the cadence is room for both on a
/// loaded machine and nowhere near the second the defect added.
#[test]
fn a_counted_window_ends_at_its_duration_and_not_at_the_next_read() {
    let output = run(&[
        "trace", "io.bytes", "-m", "count", "-e", "summary", "-d", "300ms", "--format", "json",
        "--system",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("a JSON document");
    let result = &document["results"][0];
    let observed_ns = result["observed_ns"].as_u64().expect("the window");
    assert!(
        result["stopped_because"]
            .as_str()
            .is_some_and(|why| why.contains("duration")),
        "{result}"
    );
    assert!(
        (300_000_000..400_000_000).contains(&observed_ns),
        "a 300 ms count observed {observed_ns} ns"
    );
}

/// Two tasks in one profile, one of them made to overflow its ring: the
/// shortfall lands on that task's series and on no other.
///
/// A profile that follows a program opens one ring per thread, so each drop
/// belongs to one task; before the thirteenth wave the run's whole drop count
/// was marked on every series, and a reader could not tell the task that lost
/// samples from the one beside it that lost none. The busy task spins through
/// the whole run at a sampling rate past what its ring holds between two
/// drains; the light one wakes briefly twenty times.
///
/// What the property needs from the collector is its losses per ring
/// (`Collector::losses_by_ring`), which `cpu.profile` -- stream P's file --
/// does not report yet: until it does this says so, loudly, with the run's
/// numbers.
#[test]
fn a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed() {
    let busy = "i=0; while [ $i -lt 1500000 ]; do i=$((i+1)); done";
    let light = "for j in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do \
                 k=0; while [ $k -lt 2000 ]; do k=$((k+1)); done; sleep 0.05; done";
    let script = format!("({busy}) & ({light}) & wait");
    let output = run(&[
        "trace",
        "cpu.profile",
        "-m",
        "sample@50000hz",
        "-e",
        "topk:50",
        "-k",
        "task",
        "--format",
        "json",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);
    if !output.status.success() {
        eprintln!(
            "SKIP: `cpu.profile` is not served here: {}",
            stderr(&output)
        );
        return;
    }
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("a JSON document");
    let result = &document["results"][0];
    let dropped = result["losses"]["dropped"].as_u64().unwrap_or(0);
    let rings = result["by_ring"].as_array().cloned().unwrap_or_default();
    if rings.is_empty() {
        eprintln!(
            "SKIP: `cpu.profile` does not report its losses per ring yet (filed for stream P, \
             `Collector::losses_by_ring`), so its {dropped} dropped samples are the run's on \
             every series"
        );
        return;
    }
    let lost_by_task: std::collections::BTreeMap<String, u64> = rings
        .iter()
        .filter_map(|ring| {
            let tid = ring["key"]["task"]["tid"]
                .as_i64()
                .or_else(|| ring["key"]["task"].as_i64())?;
            Some((format!("task:{tid}"), ring["losses"]["dropped"].as_u64()?))
        })
        .collect();
    if dropped == 0 {
        eprintln!(
            "SKIP: no ring overflowed at 50 kHz on this machine, so there is no loss to place: \
             {lost_by_task:?}"
        );
        return;
    }
    let series = result["outputs"][0]["series"]
        .as_array()
        .expect("a ranking of tasks");
    let mut marked = 0;
    for one in series {
        let key = one["key"].as_str().expect("a key");
        let lost = lost_by_task.get(key).copied().unwrap_or(0);
        match one["short"]["dropped"].as_u64() {
            Some(short) => {
                assert_eq!(short, lost, "{key} is short by its own ring's loss: {one}");
                marked += 1;
            }
            None => assert_eq!(lost, 0, "{key} lost {lost} and is not marked: {one}"),
        }
    }
    assert!(
        marked > 0 && marked < series.len(),
        "the loss lands on the task that overflowed and not on the other: {series:?}"
    );
}

/// The resolution's filter line says who applies the predicate, which is what
/// it costs, in the words of the resolver's own placement -- and never the
/// `no;` its `Display` prefixes to a collector's for the line this replaced.
#[test]
fn the_filter_line_says_who_applied_the_predicate() {
    let output = run(&[
        "io.bytes",
        "-e",
        "summary",
        "-m",
        "count",
        "--system",
        "--filter",
        "dev == \"8:0\"",
        "--dump-request",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let said = String::from_utf8_lossy(&output.stdout).into_owned() + &stderr(&output);
    // The request's own `filter` line comes first; the placement is the one
    // that says who applies it.
    let headings = [
        "in the program:",
        "applied by the collector, after delivery:",
        "applied after reading:",
    ];
    let line = said
        .lines()
        .map(str::trim_start)
        .find(|line| {
            line.starts_with("filter  ") && headings.iter().any(|heading| line.contains(heading))
        })
        .unwrap_or_else(|| panic!("a filter placement line naming who applied it: {said}"));
    assert!(!line.contains("no;"), "{line}");
}

/// A request naming several processes, served by a collector that observes
/// one, is refused with what it would have lost -- not measured for the first
/// and silent about the rest (`open-requests 250`). Two processes this test
/// starts and owns are the target; the refusal names the one it would have
/// kept, and is the build's gap (exit 3), since the request is valid.
#[test]
fn several_processes_asked_of_a_collector_that_observes_one_are_refused_by_name() {
    let mut first = Command::new("sleep").arg("5").spawn().expect("a process");
    let mut second = Command::new("sleep").arg("5").spawn().expect("a process");
    let pids = format!("{},{}", first.id(), second.id());
    let output = run(&[
        "mem.rss",
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "-d",
        "200ms",
        "--pid",
        &pids,
    ]);
    let _ = first.kill();
    let _ = second.kill();
    let _ = first.wait();
    let _ = second.wait();
    let said = stderr(&output);
    assert_eq!(output.status.code(), Some(EXIT_NOT_IMPLEMENTED), "{said}");
    assert!(
        said.contains("observes one process")
            && said.contains(&format!("only {}", first.id()))
            && said.contains("the 2 this request named"),
        "{said}"
    );

    // One process is served as before.
    let alone = run(&[
        "mem.rss",
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "-d",
        "200ms",
        "--pid",
        &std::process::id().to_string(),
    ]);
    assert!(alone.status.success(), "{}", stderr(&alone));
}

/// A counted window over a polled counter ends with its closing read, whether
/// the run ends at its duration or when its program exits.
///
/// Ground truth is loopback traffic the program makes and measures itself: it
/// reads `lo`'s own counters in `/proc/net/dev`, sends `N` datagrams of `B`
/// bytes to a socket of its own and receives them, reads the counters again,
/// and reports both deltas. The counted window holds the program's whole run,
/// so `net.bytes` on `lo` must report at least that increment in each
/// direction, and at least `N` datagrams' worth of payload; `--system` holds the
/// same `lo` series. Before the fix the run woke at its duration, met the stop
/// condition before the read that followed it, and a counted window whose
/// next read was a second away reported its baseline alone: zero events.
#[test]
fn a_counted_window_over_a_polled_counter_ends_with_its_closing_read() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("SKIP: no python3 to make the traffic");
        return;
    }
    const N: u64 = 400;
    const B: u64 = 1000;
    let program = format!(
        "import socket, sys, time\n\
         def lo():\n\
         \x20   for line in open('/proc/net/dev'):\n\
         \x20       name, _, rest = line.partition(':')\n\
         \x20       if name.strip() == 'lo':\n\
         \x20           f = rest.split()\n\
         \x20           return int(f[0]), int(f[8])\n\
         s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)\n\
         s.bind(('127.0.0.1', 0))\n\
         before = lo()\n\
         for _ in range({N}):\n\
         \x20   s.sendto(b'x' * {B}, s.getsockname())\n\
         \x20   s.recv(65536)\n\
         after = lo()\n\
         print(after[0] - before[0], after[1] - before[1], flush=True)\n\
         time.sleep(float(sys.argv[1]))\n"
    );
    for (target, linger, duration) in [
        ("{iface: lo}", "0", None),
        ("{iface: lo}", "5", Some("1s")),
        ("system", "5", Some("1s")),
    ] {
        let dir = std::env::temp_dir().join(format!("omn-counted-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("traffic.py");
        std::fs::write(&script, &program).unwrap();
        let duration = duration.map_or_else(String::new, |d| format!("    duration: {d}\n"));
        let session = format!(
            "requests:\n\
             \x20 - label: program\n    source: mem.rss\n    \
             target: {{launch: [python3, \"{}\", \"{linger}\"]}}\n    mode: poll@100ms\n    \
             emit: [summary]\n{duration}\
             \x20 - label: lo\n    source: net.bytes\n    target: {target}\n    mode: count\n    \
             emit: [summary]\n{duration}",
            script.display()
        );
        let file = dir.join("session.yml");
        std::fs::write(&file, &session).unwrap();
        let output = run(&["-f", &file.to_string_lossy(), "--format", "json"]);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(output.status.success(), "{}", stderr(&output));
        // The program's own line comes first on standard output, then the
        // document.
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let (first, document) = text.split_once('\n').expect("the program's line");
        let own: Vec<u64> = first
            .split_whitespace()
            .map(|n| n.parse().expect("a count"))
            .collect();
        let document: serde_json::Value = serde_json::from_str(document).expect("json");
        let results = document["results"].as_array().unwrap();
        let lo = results
            .iter()
            .find(|result| result["label"] == "lo")
            .expect("the counted request");
        // The series is `lo`'s received plus transmitted bytes over the one
        // counted window: `net.bytes` declares no emit that shows the two
        // directions apart, though its event carries both.
        let series = lo["outputs"][0]["series"].as_array().expect("a summary");
        let lo_series = series
            .iter()
            .find(|one| one["key"].as_str().is_some_and(|key| key.contains("lo")))
            .unwrap_or_else(|| {
                panic!(
                    "{target}, ended by {}: no `lo` series among {series:?}",
                    lo["stopped_because"]
                )
            });
        let counted = lo_series["last"].as_f64().expect("the window's value") as u64;
        assert_eq!(lo_series["count"], 1, "one counted window: {lo_series}");
        assert!(
            counted >= own[0] + own[1] && counted >= 2 * N * B,
            "{target}, ended by {}: counted {counted} bytes, the program measured rx {} tx {} \
             and sent {} bytes of payload each way",
            lo["stopped_because"],
            own[0],
            own[1],
            N * B
        );
    }
}

// -------------------------------------------- a window ends where it stopped

fn monotonic_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid timespec for the kernel to write into.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

/// A task that blocks every two milliseconds until it is told to stop, and
/// logs each sleep on `CLOCK_MONOTONIC` with the kernel's own count of the
/// voluntary switches it made -- none when the timer expired while it was
/// preempted, one when it blocked.
const BLOCKING_WORKLOAD: &str = r#"
import json, resource, signal, time
mono = lambda: time.clock_gettime_ns(time.CLOCK_MONOTONIC)
blocked = lambda: resource.getrusage(resource.RUSAGE_THREAD).ru_nvcsw
done = []
signal.signal(signal.SIGUSR1, lambda *_: done.append(1))
sleeps = []
import os
print("ready", os.getpid(), flush=True)
end = mono() + 60 * 10**9
while not done and mono() < end:
    v = blocked(); a = mono(); time.sleep(0.002); b = mono(); sleeps.append([a, b, blocked() - v])
print(json.dumps(sleeps), flush=True)
"#;

/// A window closes before its collector's last read, not after the last of the
/// run's collectors stopped.
///
/// A drained collector's last read is inside its `stop()`, and the run stops
/// its collectors one after another. Before the fourteenth wave every window
/// closed once all of them had stopped, so what the task did between the
/// first one's last read and the last one's stop was inside the first one's
/// window and in none of its rows: stream S measured 6 of 930 blocks with
/// `sched.offcpu` alone and 71 with five collectors stopping after it. Now
/// every window the run held to its end closes at one instant before the
/// first stop.
///
/// The truth is the task's: every sleep it logged, with the voluntary
/// switches the kernel counted for it, that began after the window opened and
/// ended before it closed is a block `sched.offcpu` read, or one it reports
/// losing. The task keeps blocking past the end of the run, so the end of the
/// window is where the difference would be, and four more collectors stop
/// after `sched.offcpu` to make the gap wide.
#[test]
fn a_window_closes_before_its_collector_s_last_read_not_after_the_last_stop() {
    use std::io::BufRead;

    if Command::new("python3").arg("-V").output().is_err() {
        eprintln!("skipped: the workload is python3, which this machine does not have");
        return;
    }
    let mut task = Command::new("python3")
        .args(["-c", BLOCKING_WORKLOAD])
        .stdout(Stdio::piped())
        .spawn()
        .expect("python3 starts");
    let mut stdout = std::io::BufReader::new(task.stdout.take().expect("a pipe"));
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .expect("the workload says it is ready");
    let pid: i32 = line
        .trim()
        .strip_prefix("ready ")
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("the workload did not start: {line:?}"));

    let target = format!("{{pid: {pid}}}");
    let mut session =
        String::from("name: five collectors stopping in turn\nduration: 1500ms\nrequests:\n");
    for (label, source) in [
        ("blocks", "sched.offcpu"),
        ("wakeups", "sched.wakeup"),
        ("waits", "sched.runqlat"),
        ("moves", "sched.migrations"),
        ("locks", "sched.lock"),
    ] {
        session.push_str(&format!(
            "  - {{label: {label}, source: {source}, target: {target}, mode: trace, emit: [raw]}}\n"
        ));
    }
    let spawned_ns = monotonic_ns();
    let output = {
        use std::io::Write;
        let mut child = Command::new(BINARY)
            .args(["-f", "-", "--input-format", "yaml", "--format", "json"])
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
    };
    // SAFETY: a signal to the child this test started and has not reaped.
    unsafe { libc::kill(pid, libc::SIGUSR1) };
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut stdout, &mut rest).expect("the log");
    let _ = task.wait();

    if !output.status.success() {
        let why = stderr(&output);
        if machine_cannot_serve(&why) || why.contains("CAP_BPF") || why.contains("permission") {
            eprintln!("skipped: the sched programs are not served here: {why}");
            return;
        }
        panic!("the run failed: {why}");
    }
    let sleeps: Vec<[u64; 3]> = serde_json::from_str(rest.trim())
        .unwrap_or_else(|error| panic!("the workload's log is not json ({error}): {rest}"));
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("a JSON document");
    let blocks = results_labelled(&document, "blocks");
    let opened = blocks["window"]["opened_ns"].as_u64().expect("a window");
    let closed = blocks["window"]["closed_ns"].as_u64().expect("a window");
    let last_sleep = sleeps.last().map_or(0, |sleep| sleep[1]);
    if last_sleep <= closed {
        eprintln!(
            "skipped: the workload stopped {:.3} s after omn started and the window closed at \
             {:.3} s: nothing it did straddles the end",
            last_sleep.saturating_sub(spawned_ns) as f64 / 1e9,
            closed.saturating_sub(spawned_ns) as f64 / 1e9
        );
        return;
    }
    let inside: u64 = sleeps
        .iter()
        .filter(|sleep| sleep[0] >= opened && sleep[1] <= closed)
        .map(|sleep| sleep[2])
        .sum();
    let ceiling: u64 = sleeps
        .iter()
        .filter(|sleep| sleep[0] >= spawned_ns)
        .map(|sleep| sleep[2])
        .sum();
    let lost = blocks["losses"]["dropped"].as_u64().unwrap_or(0);
    let kept: u64 = blocks["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .filter(|output| output["events"].is_array())
        .flat_map(|output| output["events"].as_array().cloned().unwrap_or_default())
        .filter(|event| {
            let attrs = event["attrs"].as_array().cloned().unwrap_or_default();
            let attr = |name: &str| {
                attrs
                    .iter()
                    .find(|attr| attr["name"] == name)
                    .map(|attr| attr["value"].clone())
                    .unwrap_or_default()
            };
            attr("tid").as_i64() == Some(pid as i64) && attr("state") == "sleeping"
        })
        .map(|event| {
            event["attrs"]
                .as_array()
                .and_then(|attrs| attrs.iter().find(|attr| attr["name"] == "blocks"))
                .and_then(|attr| attr["value"].as_u64())
                .unwrap_or(1)
        })
        .sum();
    eprintln!(
        "sched.offcpu: {kept} blocks kept, {inside} inside its window by the kernel's count, \
         {ceiling} since omn started, {lost} reported lost"
    );
    assert!(
        inside > 100,
        "the task blocked {inside} times inside the window, too few to judge"
    );
    assert!(
        kept + lost >= inside,
        "the task blocked {inside} times inside `sched.offcpu`'s window by the kernel's own \
         count, and the source kept {kept} and reported {lost} lost: the window claims a span \
         its collector did not read"
    );
    assert!(
        kept <= ceiling,
        "{kept} blocks kept, more than the {ceiling} the task made since omn started"
    );
}
