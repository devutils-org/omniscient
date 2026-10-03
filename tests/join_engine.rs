//! Ground-truth tests for the join engine.
//!
//! A join engine is hard to test honestly, because its input is a stream of
//! events and the easiest stream to produce is one written to match the answer
//! expected from it. So the streams here come from somewhere else: real
//! processes read through the real collectors, and real threads blocked on a
//! real pipe. What the engine has to recover is a fact the operating system
//! guarantees -- that a descriptor count belongs to the process it was read
//! from, that a reader blocked on an empty pipe is released by the writer and
//! by nothing else -- and the expected counts are computed from the input
//! rather than from what the engine produced.
//!
//! The eBPF test skips, loudly, on a machine that does not grant CAP_BPF and
//! CAP_PERFMON, because the wakeup chain is not readable without them: tracefs
//! is root-only, `unprivileged_bpf_disabled` is the default, and there is no
//! lesser answer to fall back to. Every other test here runs anywhere.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::thread;
use std::time::Duration as StdDuration;

use omn::catalog::MatchKind;
use omn::clock::monotonic_ns;
use omn::emit::Output;
use omn::encode::RunOutput;
use omn::event::{Attr, CollectorId, Event, KeyValue};
use omn::join::{self, Evidence, Rule};
use omn::request::KeyKind;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// A machine that cannot serve a request and a request that cannot mean
/// anything are different answers, and only the first is a reason to stop
/// quietly. This is the first.
fn machine_cannot_serve(message: &str) -> bool {
    message.contains("no viable backend")
        || message.contains("cannot start collecting")
        || message.contains("perf_event_paranoid")
        || message.contains("CAP_BPF")
        || message.contains("cap_bpf")
}

/// The machine's reason for a skip, laid on the skip line itself.
///
/// A run's stderr opens with whatever it warned about first -- a caveat, an
/// unwinder it lacks -- and a skip line that quotes the whole of it puts the
/// reason several lines down, unindented, where a reader of skip lines does not
/// look. So the lines that say why the machine cannot serve come first, the
/// rest follow indented under them, and a stderr with no such line is quoted
/// whole, still on one logical line.
fn machine_s_reason(said: &str) -> String {
    let lines: Vec<&str> = said
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let (reasons, rest): (Vec<&str>, Vec<&str>) = lines
        .iter()
        .partition(|line| machine_cannot_serve(line) || line.contains("error"));
    let mut laid = String::new();
    for line in reasons.iter().chain(rest.iter()) {
        if laid.is_empty() {
            laid.push_str(line.trim());
        } else {
            laid.push_str("\n    ");
            laid.push_str(line.trim_end());
        }
    }
    laid
}

/// A process that exists and does nothing, so that reading its procfs entries
/// is a measurement of something rather than a race against its exit.
fn helper() -> Child {
    Command::new("/bin/sh")
        .args(["-c", "sleep 30"])
        .spawn()
        .expect("a helper process")
}

/// Runs one source against one process and hands back the events it produced.
///
/// The events are the collector's own, read back through the tool's own
/// encoding: real timestamps, real task ids, real counts. Nothing here decides
/// what the join should answer.
fn observe(source: &str, pid: u32) -> Result<Vec<Event>, String> {
    let output = Command::new(BINARY)
        .args([
            source,
            "--pid",
            &pid.to_string(),
            "--mode",
            "poll@20ms",
            "--duration",
            "300ms",
            "--emit",
            "raw",
            "--format",
            "json",
        ])
        .output()
        .expect("omn should run");
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let errors = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        return Err(format!("{text}{errors}"));
    }
    let run: RunOutput =
        serde_json::from_str(&text).map_err(|error| format!("{error} in:\n{text}"))?;
    let events: Vec<Event> = run
        .results
        .iter()
        .flat_map(|results| &results.outputs)
        .filter_map(|output| match output {
            Output::Raw { events } => Some(events.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    Ok(events)
}

fn eq_rule() -> Rule {
    Rule {
        id: "handles-of-a-thread".to_owned(),
        from: "proc.threads".to_owned(),
        to: "proc.fd".to_owned(),
        via: Some(KeyKind::Task),
        match_kind: MatchKind::Eq,
        opens: None,
        closes: None,
        waker: None,
        woken: None,
        span: None,
        stamped: None,
        enclosing: None,
        depth: None,
        value: None,
        reading: None,
        window: None,
        capacity: 1000,
        top: 10,
    }
}

/// Ground truth for `eq`: two processes, four real collections, and a count
/// per task that is known before the engine runs.
///
/// Both processes are measured by both sources, so the merged stream holds two
/// tasks whose events interleave by kind. Every descriptor reading belongs to
/// the process it was read from -- procfs cannot report one process's table
/// under another's pid -- so the number of events that must land under each
/// task is the number that source produced for it, and nothing may land under
/// the other. An engine that attributed to whichever state happened to be open
/// passes a single-process test and fails this one.
#[test]
fn an_equal_key_sends_every_event_to_its_own_task_and_none_to_the_other() {
    let mut first = helper();
    let mut second = helper();
    // Let both reach `sleep`, so the first reading is of a live process.
    thread::sleep(StdDuration::from_millis(100));
    let (left, right) = (first.id(), second.id());

    let collected = [
        observe("proc.threads", left),
        observe("proc.threads", right),
        observe("proc.fd", left),
        observe("proc.fd", right),
    ];
    let _ = first.kill();
    let _ = second.kill();
    let _ = first.wait();
    let _ = second.wait();

    let mut events = Vec::new();
    for outcome in collected {
        match outcome {
            Ok(some) => events.extend(some),
            Err(message) if machine_cannot_serve(&message) => {
                eprintln!(
                    "skipped: this machine cannot read a process's procfs entries, so there is \
                     no real two-task stream to attribute: {}",
                    machine_s_reason(&message)
                );
                return;
            }
            Err(message) => panic!("a procfs source must be collectable here: {message}"),
        }
    }

    // What has to come out, counted off the input. Each reading is filed under
    // the task it was read from, and the collections run one after another, so
    // every descriptor reading follows every thread reading of the same task.
    let readings = |kind: &str, tid: i32| {
        events
            .iter()
            .filter(|event| {
                event.kind == kind && event.key(KeyKind::Task) == Some(&KeyValue::task(tid))
            })
            .count() as u64
    };
    let (left, right) = (left as i32, right as i32);
    for tid in [left, right] {
        assert!(
            readings("proc.threads", tid) > 0 && readings("proc.fd", tid) > 0,
            "task {tid} produced nothing to join"
        );
    }

    let correlation = join::apply(&eq_rule(), &events).expect("the rule fits these sources");

    assert_eq!(
        correlation.matched + correlation.unmatched,
        readings("proc.fd", left) + readings("proc.fd", right),
        "every downstream event is either attributed or counted as unattributed"
    );
    assert_eq!(
        correlation.unmatched, 0,
        "both tasks had a state open before their descriptors were read"
    );

    let attributed = |tid: i32| -> u64 {
        correlation
            .top
            .iter()
            .filter(|attribution| attribution.key == format!("task:{tid}"))
            .map(|attribution| attribution.matched)
            .sum()
    };
    assert_eq!(
        attributed(left),
        readings("proc.fd", left),
        "task {left} must be credited with exactly its own readings: {:?}",
        correlation.top
    );
    assert_eq!(
        attributed(right),
        readings("proc.fd", right),
        "task {right} must be credited with exactly its own readings: {:?}",
        correlation.top
    );
    assert_eq!(correlation.evidence, Evidence::Key);
}

// -- wakeup -----------------------------------------------------------------

fn gettid() -> i32 {
    // SAFETY: `gettid` takes no arguments, touches no memory and cannot fail.
    unsafe { libc::gettid() }
}

fn wakeup_rule() -> Rule {
    Rule {
        id: "who-released-this".to_owned(),
        from: "sched.wakeup".to_owned(),
        to: "sched.switches".to_owned(),
        via: Some(KeyKind::Task),
        match_kind: MatchKind::Wakeup,
        opens: None,
        closes: None,
        waker: Some("waker_tid".to_owned()),
        woken: None,
        span: None,
        stamped: None,
        enclosing: None,
        depth: None,
        value: None,
        reading: None,
        window: None,
        capacity: 1000,
        top: 10,
    }
}

fn wakeup_event(ts_ns: u64, wakee: i32, waker: i32) -> Event {
    Event {
        ts_ns,
        ts_err_ns: 0,
        kind: "sched.wakeup".to_owned(),
        keys: vec![KeyValue::task(wakee)],
        attrs: vec![
            Attr::uint("count", 1),
            Attr::uint("wakee_tid", wakee as u64),
            Attr::uint("waker_tid", waker as u64),
        ],
        collector_id: CollectorId::new("sched.wakeup/bpf:raw_tp"),
        seq: 0,
    }
}

fn switch_event(ts_ns: u64, tid: i32) -> Event {
    Event {
        ts_ns,
        ts_err_ns: 0,
        kind: "sched.switches".to_owned(),
        keys: vec![KeyValue::task(tid)],
        attrs: vec![Attr::uint("count", 1)],
        collector_id: CollectorId::new("sched.switches/pmu:sw"),
        seq: 0,
    }
}

/// Ground truth for `wakeup`: a reader blocked on a pipe, the writer that
/// releases it, and a third task that has nothing to do with either.
///
/// The causal fact is the operating system's, not the test's: a thread blocked
/// reading an empty socket pair runs again when something is written to it and
/// at no other time, so the work the reader does afterwards is work the writer
/// released. Everything fed to the engine is measured -- real thread ids, real
/// `CLOCK_MONOTONIC` timestamps, a resumption the test asserts happened after
/// the write -- and the bystander runs through the same window on purpose: an
/// engine that attributed by time rather than by task would take its work too.
///
/// What this does not check is the collector that reports who woke whom, which
/// needs CAP_BPF; the test below it does that where the machine allows.
#[test]
fn work_done_after_a_block_is_attributed_to_the_task_that_ended_it() {
    const WORK: usize = 40;
    const NOISE: usize = 40;

    let (mut reading_end, mut writing_end) = UnixStream::pair().expect("a socket pair");

    let reader = thread::spawn(move || {
        let tid = gettid();
        let mut byte = [0u8; 1];
        reading_end
            .read_exact(&mut byte)
            .expect("the writer writes");
        let mut work = Vec::with_capacity(WORK);
        for _ in 0..WORK {
            work.push(monotonic_ns());
            thread::sleep(StdDuration::from_millis(2));
        }
        (tid, work)
    });

    let writer = thread::spawn(move || {
        let tid = gettid();
        // Long enough that the reader is certainly blocked in `read` rather
        // than still being scheduled for the first time.
        thread::sleep(StdDuration::from_millis(80));
        let at = monotonic_ns();
        writing_end.write_all(b"g").expect("the pipe takes a byte");
        (tid, at)
    });

    // Runs the whole time, touches neither end, and must not be attributed.
    let bystander = thread::spawn(|| {
        let tid = gettid();
        let mut work = Vec::with_capacity(NOISE);
        for _ in 0..NOISE {
            work.push(monotonic_ns());
            thread::sleep(StdDuration::from_millis(4));
        }
        (tid, work)
    });

    let (reader_tid, reader_work) = reader.join().expect("the reader finishes");
    let (writer_tid, wrote_at) = writer.join().expect("the writer finishes");
    let (bystander_tid, bystander_work) = bystander.join().expect("the bystander finishes");

    assert!(
        reader_work[0] > wrote_at,
        "the reader cannot have resumed at {} before the write at {wrote_at} that released it",
        reader_work[0]
    );
    let overlapping = bystander_work
        .iter()
        .filter(|ts_ns| **ts_ns > wrote_at)
        .count();
    assert!(
        overlapping >= 5,
        "the bystander has to be running while the reader works, or it proves nothing: \
         {overlapping} of {NOISE} of its events fall after the wakeup"
    );

    let mut events = vec![wakeup_event(wrote_at, reader_tid, writer_tid)];
    events.extend(reader_work.iter().map(|ts| switch_event(*ts, reader_tid)));
    events.extend(
        bystander_work
            .iter()
            .map(|ts| switch_event(*ts, bystander_tid)),
    );

    let correlation = join::apply(&wakeup_rule(), &events).expect("the rule fits these sources");

    assert_eq!(correlation.evidence, Evidence::Wakeup);
    assert_eq!(
        correlation.matched, WORK as u64,
        "everything the reader did after the write belongs to the writer"
    );
    assert_eq!(
        correlation.unmatched, NOISE as u64,
        "nobody woke the bystander, so none of its work is anybody's"
    );
    assert_eq!(correlation.top.len(), 1, "{:?}", correlation.top);
    assert_eq!(
        correlation.top[0].key,
        format!("task:{writer_tid} -> task:{reader_tid}"),
        "the chain has to name the writer as the one that released the reader"
    );
}

/// The same question through the whole tool: the real eBPF collector, the real
/// scheduler, and a program whose blocking is the point.
///
/// This is the one test here that needs privileges. Without CAP_BPF the waker
/// is not readable on this machine by any means -- tracefs is root-only and
/// unprivileged eBPF is off by default -- so there is no lesser measurement to
/// fall back to and the test says what it would have checked.
///
/// What it checks is not a particular waker, which is not predictable: a timer
/// expiry is credited to whatever task the softirq interrupted. It is that the
/// engine accounts for every switch it was given, and that every chain it
/// reports ends at a task the wakeup stream actually recorded as woken.
#[test]
fn a_blocking_program_is_attributed_through_the_real_wakeup_collector() {
    let directory = std::env::temp_dir().join(format!("omn-join-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("a directory to write the session into");
    let path = directory.join("wakeup.yml");
    let session = "\
name: who releases this program
format: json
duration: 2s
requests:
  - label: wakeups
    source: sched.wakeup
    target:
      launch: [\"/bin/sh\", \"-c\", \"for i in 1 2 3 4 5 6 7 8; do sleep 0.05; done\"]
    mode: trace
    emit: [raw]
  - label: switches
    source: sched.switches
    target:
      launch: [\"/bin/sh\", \"-c\", \"for i in 1 2 3 4 5 6 7 8; do sleep 0.05; done\"]
    mode: trace
    emit: [raw]
joins:
  - id: who-released-this
    from: sched.wakeup
    to: sched.switches
    via: task
    match: wakeup
    waker: waker_tid
    window: 1s
";
    std::fs::write(&path, session).expect("the session file");

    let output = Command::new(BINARY)
        .args(["-f", path.to_str().expect("a utf-8 path")])
        .output()
        .expect("omn should run");
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let errors = String::from_utf8_lossy(&output.stderr).into_owned();
    let _ = std::fs::remove_dir_all(&directory);

    if !output.status.success() {
        if machine_cannot_serve(&errors) || machine_cannot_serve(&text) {
            eprintln!(
                "skipped: this machine does not grant CAP_BPF, so who woke whom cannot be read \
                 at all. It would have checked that every context switch of a blocking program \
                 is either attributed to a recorded wakeup of that task or counted as \
                 unattributed, and that no chain names a task nothing woke: {}",
                machine_s_reason(&errors)
            );
            return;
        }
        panic!("{text}{errors}");
    }

    let run: RunOutput =
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{error} in:\n{text}"));
    let correlation = run.joins.first().expect("the session declares one join");

    let raw = |source: &str| -> Vec<Event> {
        run.results
            .iter()
            .filter(|results| results.source == source)
            .flat_map(|results| &results.outputs)
            .filter_map(|output| match output {
                Output::Raw { events } => Some(events.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    };
    let switches = raw("sched.switches");
    let wakeups = raw("sched.wakeup");
    assert!(
        !wakeups.is_empty() && !switches.is_empty(),
        "a program that sleeps eight times both blocks and is woken"
    );

    assert_eq!(correlation.evidence, Evidence::Wakeup);
    // Every switch is attributed, counted as unattributed, or counted apart:
    // one with no task key, or one stamped outside the wakeup source's
    // window, which is not a switch that followed no wakeup. The run says how
    // many of each.
    assert_eq!(
        correlation.matched
            + correlation.unmatched
            + correlation.keyless
            + correlation.outside_window,
        switches.len() as u64,
        "every switch is either attributed or counted as unattributed or outside the window: \
         {} + {} + {} + {}",
        correlation.matched,
        correlation.unmatched,
        correlation.keyless,
        correlation.outside_window
    );

    // A chain ends at the task that was woken. The engine may not report one
    // ending at a task the wakeup stream never named, which is what inventing
    // a state would look like.
    let woken: Vec<String> = wakeups
        .iter()
        .filter_map(|event| event.key(KeyKind::Task).map(|key| key.to_string()))
        .collect();
    for attribution in &correlation.top {
        let tail = attribution
            .key
            .rsplit(" -> ")
            .next()
            .expect("a chain names at least one task");
        assert!(
            woken.iter().any(|task| task == tail),
            "`{}` ends at a task no wakeup named",
            attribution.key
        );
    }
}

// -- the eighth wave: TEMPORAL, chains, refusals, and the decomposition -----
//
// Every workload below reports its own state on standard error -- what it
// wrote and when, what the kernel says it did, which task released it -- and
// the tool reads none of that. What the engine says is held to it.

const MIB: f64 = (1u64 << 20) as f64;

/// What `mem.rss` can move by without the program doing anything: khugepaged
/// collapsing a range into one huge page, the caveat the catalog writes on the
/// source. The memory crossing uses the same bound.
const HUGE_PAGE: f64 = 2.0 * MIB;

/// What the interpreter allocates, frees and faults on its own between two
/// readings: measured by the memory crossing at under 25 KiB, and sixteen
/// pages here so that a slower interpreter is not mistaken for a wrong tool.
const INTERPRETER: f64 = 16.0 * 4096.0;

fn has(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// One run of a shipped session as JSON: the document, and standard error,
/// where the workload reports itself.
struct Session {
    document: serde_json::Value,
    stderr: String,
}

/// Runs `examples/<file>`, or says loudly why this machine cannot and returns
/// nothing. A shipped session the tool refuses is a failure, not a skip.
fn session(file: &str, would_have_checked: &str) -> Option<Session> {
    let output = Command::new(BINARY)
        .args(["-f", &format!("examples/{file}"), "--format", "json"])
        .output()
        .expect("omn should run");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        if machine_cannot_serve(&stderr) || machine_cannot_serve(&stdout) {
            eprintln!(
                "SKIPPED: {file} needs what this machine does not grant ({}). It would have \
                 checked {would_have_checked}",
                machine_s_reason(&stderr)
            );
            return None;
        }
        panic!("{file} did not run cleanly:\n{stdout}{stderr}");
    }
    let document = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("{file} is not one json document ({error}):\n{stdout}"));
    Some(Session { document, stderr })
}

impl Session {
    fn join(&self, rule: &str) -> &serde_json::Value {
        self.document["joins"]
            .as_array()
            .and_then(|joins| joins.iter().find(|join| join["rule"] == rule))
            .unwrap_or_else(|| panic!("no join `{rule}` in {}", self.document))
    }

    fn decomposition(&self, id: &str) -> &serde_json::Value {
        self.document["decompositions"]
            .as_array()
            .and_then(|all| all.iter().find(|one| one["id"] == id))
            .unwrap_or_else(|| panic!("no decomposition `{id}` in {}", self.document))
    }

    fn result(&self, label: &str) -> &serde_json::Value {
        self.document["results"]
            .as_array()
            .and_then(|all| all.iter().find(|one| one["label"] == label))
            .unwrap_or_else(|| panic!("no request labelled `{label}`"))
    }

    /// The fields of every line of standard error that starts with `prefix`.
    fn reported(&self, prefix: &str) -> Vec<Vec<String>> {
        self.stderr
            .lines()
            .filter_map(|line| line.strip_prefix(prefix))
            .map(|rest| rest.split_whitespace().map(str::to_owned).collect())
            .collect()
    }

    /// `name=value` pairs from the workload's `omn-truth` line.
    fn truth(&self, name: &str) -> f64 {
        self.reported("omn-truth ")
            .first()
            .and_then(|fields| {
                fields.iter().find_map(|pair| {
                    let (key, value) = pair.split_once('=')?;
                    (key == name).then(|| value.parse().ok()).flatten()
                })
            })
            .unwrap_or_else(|| panic!("the workload reported no `{name}`:\n{}", self.stderr))
    }
}

fn number(value: &serde_json::Value) -> f64 {
    value.as_f64().unwrap_or(0.0)
}

/// The rows of a decomposition's table for one key, by state.
fn rows(table: &serde_json::Value) -> BTreeMap<&str, &serde_json::Value> {
    table["rows"]
        .as_array()
        .expect("a table has rows")
        .iter()
        .map(|row| (row["state"].as_str().unwrap_or_default(), row))
        .collect()
}

/// `between` against a program that says what it wrote, and when.
///
/// Three phases each open large blocks and write a known number of bytes
/// inside one interval, and the resident size's change across that interval --
/// with whatever a phase that straddled a reading left in the next one -- is
/// held to what the program wrote, to a huge page. That is the arithmetic.
///
/// The fourth phase is the point, and it is the test that TEMPORAL is not a
/// synonym for KEY. The program allocates one 1 MiB block and never writes it,
/// and in the same instant maps and writes 16 MiB the allocation tracker cannot
/// see. `between` credits the interval's 16 MiB to the one block that opened
/// in it -- sixteen times more than the block can hold -- because that is what
/// crediting by the clock does, and the correlation says TEMPORAL. The same
/// streams joined by `eq` on the task say KEY for the association and TEMPORAL
/// for which state each reading landed on.
#[test]
fn between_credits_what_moved_together_and_says_temporal_even_when_it_is_wrong() {
    if !has("python3") {
        eprintln!("SKIPPED: this machine has no python3 to launch the workload");
        return;
    }
    let Some(run) = session(
        "join-between-residency.yml",
        "that each phase's written bytes are credited to the blocks that opened with it, and \
         that a decoy's unrelated 16 MiB is credited to an unwritten block at TEMPORAL",
    ) else {
        return;
    };

    let residency = run.join("residency");
    assert_eq!(residency["match_kind"], "between");
    assert_eq!(residency["evidence"], "TEMPORAL", "{residency}");
    assert!(residency.get("within_key").is_none(), "{residency}");

    let by_task = run.join("by-task");
    assert_eq!(by_task["evidence"], "KEY", "the two sides do share a task");
    assert_eq!(
        by_task["within_key"], "TEMPORAL",
        "every allocator call replaced the state before it, so which one a reading landed on \
         was the clock's choice: {by_task}"
    );

    // The resident size readings, to find the one after an interval.
    let readings: Vec<(u64, f64)> = run.result("resident")["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .filter(|output| output["emit"] == "raw")
        .flat_map(|output| output["events"].as_array().cloned().unwrap_or_default())
        .map(|event| {
            let bytes = event["attrs"]
                .as_array()
                .and_then(|attrs| attrs.iter().find(|attr| attr["name"] == "bytes"))
                .map(|attr| number(&attr["value"]))
                .unwrap_or(0.0);
            (event["ts_ns"].as_u64().unwrap_or(0), bytes)
        })
        .collect();
    let after = |closed_ns: u64| -> f64 {
        let position = readings.iter().position(|(ts, _)| *ts == closed_ns);
        match position {
            Some(at) if at + 1 < readings.len() => readings[at + 1].1 - readings[at].1,
            _ => 0.0,
        }
    };
    let lossy = number(&run.result("allocations")["losses"]["dropped"]) > 0.0;

    let intervals = residency["top"].as_array().expect("intervals");
    for phase in run.reported("PHASE ") {
        let (name, at, written, blocks) = (
            phase[0].as_str(),
            phase[1].parse::<u64>().expect("an instant"),
            phase[2].parse::<f64>().expect("bytes"),
            phase[3].parse::<u64>().expect("a count"),
        );
        if blocks == 0 {
            // The freed phase: a free carries no size, so this session has no
            // `closes` and the change is credited to nothing. It is counted
            // among the readings that moved with nothing around them.
            continue;
        }
        // The interval the phase's allocations fell in: the one reaching past
        // the instant the phase began, and starting before its writes ended.
        let interval = intervals
            .iter()
            .find(|interval| {
                interval["closed_ns"].as_u64().unwrap_or(0) >= at
                    && interval["opened_ns"].as_u64().unwrap_or(u64::MAX) <= at + 20_000_000
            })
            .unwrap_or_else(|| panic!("no credited interval holds the {name} phase: {residency}"));
        let change = &interval["change"];
        if !lossy {
            assert_eq!(
                change["opened"].as_u64(),
                Some(blocks),
                "the {name} phase allocated {blocks} large blocks: {interval}"
            );
        }
        let delta = number(&change["delta"]);
        let spill = after(interval["closed_ns"].as_u64().unwrap_or(0));
        assert!(
            delta >= -INTERPRETER,
            "the {name} phase only added memory, and its interval moved by {delta}"
        );
        assert!(
            (delta + spill - written).abs() <= HUGE_PAGE + INTERPRETER,
            "the {name} phase wrote {written} bytes; its interval moved by {delta} and the \
             reading after it by {spill}"
        );
        if name == "decoy" {
            assert!(
                delta + spill > 8.0 * MIB,
                "the unwritten 1 MiB block was credited with {} MiB: more than it can hold, \
                 which is what TEMPORAL evidence allows and KEY would not",
                (delta + spill) / MIB
            );
        }
    }
    assert_eq!(
        run.reported("PHASE ").len(),
        4,
        "the workload reports four phases:\n{}",
        run.stderr
    );
}

/// The memory crossing's decomposition, said by the tool: the rise in the
/// resident size split into live and retained bytes, with a residue the
/// program predicts most of.
#[test]
fn the_resident_size_decomposes_into_live_and_retained_with_the_residue_the_program_predicts() {
    if !has("python3") {
        eprintln!("SKIPPED: this machine has no python3 to launch the workload");
        return;
    }
    let Some(run) = session(
        "join-decompose-memory.yml",
        "that live and retained bytes are what the program holds and frees, and that the \
         residue is the freed bytes on pages shared with live blocks",
    ) else {
        return;
    };
    let arena = &run.reported("ARENA ")[0];
    let (freed, whole_pages) = (
        arena[1].parse::<f64>().expect("bytes"),
        arena[2].parse::<f64>().expect("bytes"),
    );

    // docs/flaky.md #26: the two things that can move a row by more than the
    // interpreter's allowance without the tool being wrong are a last poll
    // that came before the frees had all returned, and a shim ring that
    // dropped records under a slow reader. The run says both, so the test
    // reads both before it asserts anything that depends on them.
    let freed_at = run.reported("FREED ")[0][0]
        .parse::<u64>()
        .expect("a timestamp");
    // The interpreter's own startup is the third: a first reading taken
    // before it was ready (a slow VM, CI's `defaults` cell) saw part of it,
    // and the rise in `live` then includes what it allocated after that
    // reading -- at most what glibc says it holds once ready.
    let ready = &run.reported("READY ")[0];
    let ready_at = ready[0].parse::<u64>().expect("a timestamp");
    let in_use_when_ready = ready[1].parse::<f64>().expect("bytes");
    let free_when_ready = ready[2].parse::<f64>().expect("bytes");
    let resident_when_ready = ready[3].parse::<f64>().expect("bytes");
    let result = run.result("retained");
    let dropped = result["losses"]["dropped"].as_u64().unwrap_or(0);
    let readings: Vec<(u64, f64)> = result["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .filter(|output| output["emit"] == "raw")
        .flat_map(|output| output["events"].as_array().into_iter().flatten())
        .filter_map(|event| {
            let live = event["attrs"]
                .as_array()?
                .iter()
                .find(|attr| attr["name"] == "live")?["value"]
                .as_f64()?;
            Some((event["ts_ns"].as_u64()?, live))
        })
        .collect();
    let last_reading = readings.iter().map(|(ts, _)| *ts).max().unwrap_or(0);
    // The rows' own filter, `live > 0`: the first reading they start from.
    let first_reading = readings
        .iter()
        .filter(|(_, live)| *live > 0.0)
        .map(|(ts, _)| *ts)
        .min()
        .unwrap_or(0);
    eprintln!(
        "last reading {:+.1} ms from the last free; the shim dropped {dropped}",
        (last_reading as f64 - freed_at as f64) / 1e6
    );
    if last_reading < freed_at {
        eprintln!(
            "not asserted: the last reading was taken {:.1} ms before the program's last free \
             returned, so the table measured the state before the end, and the program's \
             figures describe the end",
            (freed_at - last_reading) as f64 / 1e6
        );
        return;
    }

    let decomposition = run.decomposition("where-the-memory-went");
    assert_eq!(decomposition["unit"], "bytes");
    let table = &decomposition["tables"][0];
    let rows = rows(table);
    for row in rows.values() {
        assert_eq!(row["kind"], "polled", "{row}");
        assert_eq!(row["evidence"], "KEY", "{row}");
    }

    let live = number(&rows["live"]["amount"]);
    let retained = number(&rows["retained"]["amount"]);
    let residue = number(&table["residue"]);
    // Four kept 8 MiB mappings, and the 512 arena blocks less those freed.
    let held = 4.0 * 8.0 * MIB + 512.0 * 65536.0 - freed;
    if dropped > 0 {
        eprintln!(
            "not asserted: live and retained against the program's figures, because the shim \
             dropped {dropped} records and a lost free or allocation moves `live` by a whole \
             block (live {live}, held {held}; retained {retained}, whole pages {whole_pages})"
        );
        return;
    }
    // What the startup after the first reading can add to each row, by the
    // program's own figures once ready; nothing when the first reading came
    // after it.
    let early = first_reading < ready_at;
    let (startup_live, startup_retained, startup_resident) = if early {
        (in_use_when_ready, free_when_ready, resident_when_ready)
    } else {
        (0.0, 0.0, 0.0)
    };
    if !early {
        assert!(
            (live - held).abs() <= INTERPRETER,
            "the program ends holding {held} bytes it asked for; the live row says {live}"
        );
    } else {
        if in_use_when_ready < 0.0 {
            eprintln!(
                "not asserted: the first reading came {:.1} ms before the interpreter was \
                 ready, and this glibc has no mallinfo2 to say how much of the startup the \
                 rows include",
                (ready_at - first_reading) as f64 / 1e6
            );
            return;
        }
        eprintln!(
            "the first reading came {:.1} ms before the interpreter was ready: the live row \
             may include up to the {in_use_when_ready} bytes glibc says it held then",
            (ready_at - first_reading) as f64 / 1e6
        );
        assert!(
            live >= held - INTERPRETER && live <= held + in_use_when_ready + INTERPRETER,
            "the program ends holding {held} bytes it asked for, and held {in_use_when_ready} \
             when it was ready, after the first reading; the live row says {live}"
        );
    }
    assert!(
        (retained - whole_pages).abs() <= startup_retained + INTERPRETER,
        "the freed blocks cover {whole_pages} bytes of whole pages (and glibc kept \
         {startup_retained} free from a startup the rows began inside); the retained row says \
         {retained}"
    );
    let predicted = freed - whole_pages;
    assert!(
        residue >= -INTERPRETER - startup_live - startup_retained,
        "a residue of {residue} means more was accounted for than is resident"
    );
    assert!(
        residue <= predicted + HUGE_PAGE + startup_resident,
        "a residue of {residue} is more than the {predicted} the program predicts plus a huge \
         page (and the {startup_resident} resident bytes of a startup the rows began inside): \
         something resident is in neither row"
    );
    assert!(
        table.get("residue_resolution").is_none(),
        "a level of bytes has no resolution in bytes, and the table must not invent one"
    );
}

/// One task's wall clock through the tool's own decomposition, each row held
/// to the kernel's accounting of the same task, and the wakeup join beside it
/// ranked by chain.
#[test]
fn a_half_blocked_task_decomposes_as_the_kernel_says_and_its_releaser_heads_the_chains() {
    const ROUND_TRIPS: f64 = 600.0;
    let Some(run) = session(
        "join-decompose-compute.yml",
        "each row of one task's decomposition against /proc/self/schedstat, and that the \
         responder that released it 600 times is one chain at the top of the wakeup table",
    ) else {
        return;
    };
    let task = run.truth("tid") as u64;
    let (wall, on_cpu, runq) = (
        run.truth("wall_ns"),
        run.truth("oncpu_ns"),
        run.truth("runq_ns"),
    );
    let slices = run.truth("slices");

    let decomposition = run.decomposition("where-the-time-went");
    assert_eq!(decomposition["unit"], "ns");
    let table = &decomposition["tables"][0];
    assert_eq!(
        table["key"],
        format!("task:{task}"),
        "the heaviest table is the task the workload says it is"
    );
    let rows = rows(table);

    // On a cpu: a sample count times a period, with a count's resolution.
    let period = 1e9 / 997.0;
    let sampled = &rows["on-cpu"];
    assert_eq!(sampled["kind"], "sampled");
    assert!(
        (number(&sampled["resolution"]) - period).abs() < 1.0,
        "{sampled}"
    );
    let sampled_ns = number(&sampled["amount"]);
    assert!(
        (sampled_ns - on_cpu).abs() <= 0.05 * sampled_ns + 3.0 * period,
        "the sampled row says {sampled_ns} ns on cpu and the kernel says {on_cpu}"
    );

    // Waiting for a cpu: every wait, summed. 5 us per wait is the per-wait
    // difference the compute crossing measured between the tracer and
    // `run_delay` on an idle cpu (finding 7).
    let runnable = &rows["runnable"];
    assert_eq!(runnable["kind"], "traced");
    assert_eq!(number(&runnable["resolution"]), 0.0);
    let runnable_ns = number(&runnable["amount"]);
    assert!(
        (runnable_ns - runq).abs() <= 5_000.0 * 2.0 * slices,
        "the runnable row says {runnable_ns} ns and the kernel's run_delay is {runq}"
    );

    // Blocked: every block, summed, less the queue wait after each wakeup,
    // which `sched.offcpu` carries as `queued_ns` and the runnable row also
    // holds. It still holds the shell's blocks outside the task's own clock, so
    // the part of the clock that was neither on a cpu nor queued is a lower
    // bound on it.
    let blocked_ns = number(&rows["blocked"]["amount"]);
    let slack = 0.05 * sampled_ns + 3.0 * period;
    assert!(
        blocked_ns >= wall - on_cpu - runq - slack,
        "the task's own clock has {} ns neither on a cpu nor queued, and the blocked row says \
         {blocked_ns}",
        wall - on_cpu - runq
    );

    // The parts bracket the task's clock from below, and account for the
    // program's life from above. This used to be a bound against the run's
    // window plus the double count the runnable row could hold; both are gone.
    // The whole is the program's life, gate to reap, and the queue wait is out
    // of the blocked row -- so the parts are held to the whole by the residue's
    // own resolution, which carries the exit bracket, and the one sample the
    // profile can land on either side of the exit.
    let parts = number(&table["parts"]);
    let window = number(&table["whole"]["amount"]);
    assert_eq!(table["whole"]["kind"], "lifetime");
    assert!(
        parts >= wall - slack,
        "the parts add to {parts} ns, less than the task's own clock of {wall}"
    );
    let resolution = number(&table["residue_resolution"]);
    assert!(
        (window - parts).abs() <= resolution + slack,
        "the parts add to {parts} ns against a life of {window} ns, which is further apart than \
         the {resolution} ns the residue can be trusted to"
    );
    eprintln!(
        "task:{task}: on cpu {:.1} ms (kernel {:.1}), runnable {:.2} ms (kernel {:.2}), blocked \
         {:.1} ms; parts {:.1} ms against a clock of {:.1} and a life of {:.1}: residue {:+.1} ms",
        sampled_ns / 1e6,
        on_cpu / 1e6,
        runnable_ns / 1e6,
        runq / 1e6,
        blocked_ns / 1e6,
        parts / 1e6,
        wall / 1e6,
        window / 1e6,
        number(&table["residue"]) / 1e6
    );

    // The chain that released the task once per round trip is one row, at
    // the top, naming the responder.
    let join = run.join("what-released-each-run");
    assert_eq!(join["evidence"], "WAKEUP");
    let top = &join["top"][0];
    let chain = top["key"].as_str().unwrap_or_default();
    let responder = run.truth("responder") as u64;
    assert!(
        chain.ends_with(&format!("task:{responder} -> task:{task}")),
        "the responder releases the driver every round trip, and the top chain is `{chain}`: \
         {join}"
    );
    // The slack is three plus whatever either side says it lost: a wakeup the
    // kernel skipped because the program was already running on that cpu is
    // counted as dropped since the tenth wave, and a chain one short for a
    // counted loss is the run being honest, not the join being wrong.
    //
    // The responder answers 600 times, and releases the driver only when the
    // driver is already asleep on the fifo: on a loaded machine an answer can
    // arrive before the driver has blocked, and then no wakeup is needed. So
    // the count is bracketed by the workload's own two figures -- the answers
    // from above, and from below the driver's voluntary switches (its blocks,
    // by the kernel's count) less the ones that are not on the fifo: waiting
    // for its six children and on its two fifo opens.
    const OTHER_BLOCKS: f64 = 20.0;
    let wakeups = number(&top["wakeups"]);
    let lost = number(&run.result("released-by")["losses"]["dropped"])
        + number(&run.result("switched-out")["losses"]["dropped"]);
    let voluntary = run.truth("voluntary");
    assert!(
        wakeups <= ROUND_TRIPS + 3.0 + lost
            && wakeups >= voluntary.min(ROUND_TRIPS) - OTHER_BLOCKS - 3.0 - lost,
        "the responder answered {ROUND_TRIPS} times, the driver blocked {voluntary} times by the \
         kernel's count, and the responder's chain folds {wakeups} releases, with {lost} counted \
         lost"
    );
    assert!(
        number(&join["top"][1]["wakeups"]) < ROUND_TRIPS / 10.0,
        "no other chain comes near: {join}"
    );
}

/// Writes a session into a scratch file and validates it, returning the exit
/// code and everything said. Validation needs no machine: these refusals are
/// about what a request means.
fn validate(name: &str, session: &str) -> (Option<i32>, String) {
    let directory = std::env::temp_dir().join(format!("omn-join-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let path = directory.join("session.yml");
    std::fs::write(&path, session).expect("the session file");
    let output = Command::new(BINARY)
        .args(["validate", "-f", path.to_str().expect("a utf-8 path")])
        .output()
        .expect("omn should run");
    let _ = std::fs::remove_dir_all(&directory);
    (
        output.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

const LAUNCH: &str = "{launch: [/bin/sh, -c, 'sleep 0.1']}";

/// A wakeup join onto a drained total matched, attributed, and said WAKEUP:
/// the compute crossing's finding 5. It is refused before the run now, with
/// the source to use instead.
#[test]
fn a_wakeup_join_onto_a_drained_total_is_refused_before_anything_is_attached() {
    let (code, said) = validate(
        "drained",
        &format!(
            "requests:\n\
             \x20 - {{source: sched.wakeup, target: {LAUNCH}, mode: trace, emit: [raw]}}\n\
             \x20 - {{source: sched.offcpu, target: {LAUNCH}, mode: trace, emit: [raw]}}\n\
             joins:\n\
             \x20 - {{id: released, from: sched.wakeup, to: sched.offcpu, via: task, \
             match: wakeup, waker: waker_tid}}\n"
        ),
    );
    assert_eq!(code, Some(2), "{said}");
    assert!(said.contains("drained"), "{said}");
    assert!(said.contains("sched.switches"), "{said}");
}

/// `between` onto a traced source has no reading before an event to differ
/// from, and onto a poll it has to be told what a reading is.
#[test]
fn between_is_refused_onto_what_is_not_a_reading_and_onto_an_unstated_poll() {
    let traced = format!(
        "requests:\n\
         \x20 - {{source: mem.heap.alloc, target: {LAUNCH}, mode: trace, emit: [raw]}}\n\
         \x20 - {{source: mem.faults, target: {LAUNCH}, mode: trace, emit: [raw]}}\n\
         joins:\n\
         \x20 - {{id: moved, from: mem.heap.alloc, to: mem.faults, via: task, match: between, \
         value: count}}\n"
    );
    let (code, said) = validate("between-traced", &traced);
    assert_eq!(code, Some(2), "{said}");
    assert!(said.contains("no reading before it"), "{said}");

    let unstated = format!(
        "requests:\n\
         \x20 - {{source: mem.heap.alloc, target: {LAUNCH}, mode: trace, emit: [raw]}}\n\
         \x20 - {{source: mem.rss, target: {LAUNCH}, mode: poll@100ms, emit: [raw]}}\n\
         joins:\n\
         \x20 - {{id: moved, from: mem.heap.alloc, to: mem.rss, via: task, match: between, \
         value: bytes}}\n"
    );
    let (code, said) = validate("between-unstated", &unstated);
    assert_eq!(code, Some(2), "{said}");
    assert!(said.contains("reading: level"), "{said}");

    let (code, said) = validate(
        "between-stated",
        &unstated.replace("value: bytes}", "value: bytes, reading: level}"),
    );
    assert_eq!(code, Some(0), "a stated level is a valid rule: {said}");
}

/// A decomposition whose parts are not what the session collects is refused
/// before the run, by name.
#[test]
fn a_decomposition_that_cannot_mean_anything_is_refused_before_the_run() {
    let (code, said) = validate(
        "decompose-missing",
        &format!(
            "requests:\n\
             \x20 - {{source: sched.offcpu, target: {LAUNCH}, mode: trace, emit: [raw]}}\n\
             decompose:\n\
             \x20 - id: where\n\
             \x20   whole: window\n\
             \x20   parts:\n\
             \x20     - {{state: on-cpu, from: cpu.profile, value: samples}}\n\
             \x20     - {{state: blocked, from: sched.offcpu, value: total_ns}}\n"
        ),
    );
    assert_eq!(code, Some(2), "{said}");
    assert!(said.contains("does not collect"), "{said}");

    let (code, said) = validate(
        "decompose-other-target",
        &format!(
            "requests:\n\
             \x20 - {{source: sched.offcpu, target: {LAUNCH}, mode: trace, emit: [raw]}}\n\
             \x20 - {{source: sched.runqlat, target: {{pid: [1]}}, mode: trace, emit: [raw]}}\n\
             decompose:\n\
             \x20 - id: where\n\
             \x20   whole: window\n\
             \x20   parts:\n\
             \x20     - {{state: blocked, from: sched.offcpu, value: total_ns}}\n\
             \x20     - {{state: runnable, from: sched.runqlat, value: total_ns}}\n"
        ),
    );
    assert_eq!(code, Some(2), "{said}");
    assert!(said.contains("different program"), "{said}");
}

/// `format.rs` claims which formats carry a join and a decomposition; the
/// encoders are held to the claim in both directions.
#[test]
fn the_formats_that_claim_to_carry_correlations_carry_them_and_the_others_refuse() {
    use omn::format::OutputFormat;
    let decomposition: omn::join::DecompositionOutput = serde_json::from_value(serde_json::json!({
        "id": "where-the-time-went", "by": "task", "unit": "ns", "keys_seen": 1,
        "tables": [{
            "key": "task:7",
            "whole": {"state": "whole", "source": "the run's window", "value": "observed_ns",
                      "kind": "window", "amount": 10.0, "readings": 0},
            "rows": [{"state": "blocked", "source": "sched.offcpu", "value": "total_ns",
                      "kind": "traced", "evidence": "KEY", "amount": 7.0, "resolution": 0.0,
                      "readings": 2}],
            "parts": 7.0, "residue": 3.0, "residue_resolution": 0.0
        }]
    }))
    .expect("a decomposition");
    let correlation = join::apply(
        &Rule {
            match_kind: MatchKind::Eq,
            ..eq_rule()
        },
        &[],
    )
    .expect("an empty join");

    for format in OutputFormat::ALL {
        let decomposed = omn::encode::decomposition(format, &decomposition);
        let joined = omn::encode::correlation(format, &correlation);
        if format.carries_correlations() {
            let text = decomposed.unwrap_or_else(|error| panic!("{format}: {error}"));
            assert!(text.contains("where-the-time-went"), "{format}: {text}");
            let text = joined.unwrap_or_else(|error| panic!("{format}: {error}"));
            assert!(text.contains("handles-of-a-thread"), "{format}: {text}");
        } else {
            assert!(
                decomposed.is_err() && joined.is_err(),
                "`{format}` claims it cannot carry correlations and encoded one"
            );
        }
    }
}

// ------------------------------------------------ the lifetime as the whole

/// Runs a session given as text, the way a person would write one.
fn written_session(text: &str, extra: &[&str]) -> std::process::Output {
    let mut child = Command::new(BINARY)
        .args(["-f", "-"])
        .args(extra)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("omn starts");
    child
        .stdin
        .take()
        .expect("a stdin")
        .write_all(text.as_bytes())
        .expect("the session is written");
    child.wait_with_output().expect("omn finishes")
}

/// A lifetime is the life of a launched program, and a session that launches
/// nothing has none to be the whole of. Refused at validation, with what to use
/// instead, rather than decomposed against a whole that does not exist.
#[test]
fn a_lifetime_whole_is_refused_where_nothing_was_launched() {
    const SESSION: &str = "\
name: a machine-wide decomposition cannot have a program's life as its whole
requests:
  - source: sched.runqlat
    target: system
    mode: trace
    emit: [summary]
decompose:
  - id: nothing-launched
    whole: lifetime
    parts:
      - {state: runnable, from: sched.runqlat, value: total_ns}
";
    let output = written_session(SESSION, &["--input-format", "yaml", "--no-resolve"]);
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(2), "{said}");
    assert!(
        said.contains("lifetime") && said.contains("launches"),
        "the refusal says why a lifetime needs a launched program: {said}"
    );
    assert!(said.contains("window"), "and what to use instead: {said}");
}

/// The whole of a time decomposition is the program's life, and the residue
/// is then small enough to be about something.
///
/// Measured when this landed, on the compute session: with the run's window as
/// the whole the residue was +97 ms, almost all of it the drain arriving late;
/// with the lifetime it is -0.55 ms against a resolution of 1.58 ms. What is
/// asserted is that relationship -- the residue lies inside its own resolution
/// plus the one sample's error the on-cpu row always carries -- and that the
/// whole's bracket is counted in that resolution, not how long anything took.
#[test]
fn a_program_s_life_is_a_whole_its_three_states_account_for() {
    if Command::new("python3").arg("-V").output().is_err() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    let output = Command::new(BINARY)
        .args([
            "-f",
            "examples/join-decompose-compute.yml",
            "--format",
            "json",
        ])
        .output()
        .expect("omn runs");
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    // A run that succeeds still warns -- the perf_event_paranoid caveat is on
    // every run's stderr -- so only a failed run is a machine that cannot serve.
    if !output.status.success() {
        if machine_cannot_serve(&said) {
            eprintln!(
                "skipped: this machine cannot serve the three sources: {}",
                machine_s_reason(&said)
            );
            return;
        }
        panic!("the compute decomposition did not run: {said}");
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let Some(start) = text.find('{') else {
        panic!("no json document: {said}");
    };
    let document: serde_json::Value =
        serde_json::from_str(&text[start..]).unwrap_or_else(|error| panic!("{error}: {said}"));

    let table = &document["decompositions"][0]["tables"][0];
    let whole = &table["whole"];
    assert_eq!(whole["kind"], "lifetime", "{table}");
    let bracket = whole["resolution"]
        .as_f64()
        .expect("the exit bracket is the whole's resolution");
    let residue = table["residue"].as_f64().expect("a residue");
    let resolution = table["residue_resolution"]
        .as_f64()
        .expect("the residue says how far it can be trusted");

    assert!(
        resolution >= bracket,
        "the residue's resolution {resolution} has to include the whole's own bracket {bracket}"
    );
    // One period of slack beyond the stated resolution, for the one sample the
    // profile can land on either side of the exit. Nothing here is calibrated
    // to a machine: the residue is compared with the tool's own uncertainty.
    let period = 1_003_000.0;
    assert!(
        residue.abs() <= resolution + period,
        "the three states should account for the program's life to within their own \
         resolution: residue {residue} ns against {resolution} ns\n{table}"
    );
}

// ------------------------------------- a filtered part that matched nothing

/// A part whose filter nothing can pass is a measured zero, and the table
/// still adds up.
///
/// The program sleeps 300 ms and exits, so no block of it can outlast its
/// life, and a part that keeps only blocks longer than 1000 s keeps none --
/// known from the program, not from the tool. `sched.offcpu` ran, lost
/// nothing, and had events for the task; the filter kept none of them. Before
/// the eleventh wave the row read `not read` -- the filter had emptied the
/// source, so the engine concluded no event carried `total_ns` -- and took
/// `parts` and the residue with it, which is what made every residue of
/// `omn latency`'s split by state `not read`.
///
/// Checked against the same clock: the unfiltered row holds the sleep, at
/// least the 300 ms the program asked for, and at most its life.
#[test]
fn a_filtered_part_that_matched_nothing_is_a_zero_and_the_table_adds_up() {
    const SESSION: &str = "\
name: a part nothing can pass
format: json
requests:
  - source: sched.offcpu
    target: {launch: [/bin/sleep, '0.3']}
    mode: trace
    emit: [summary]
decompose:
  - id: blocked
    whole: lifetime
    parts:
      - {state: blocked, from: sched.offcpu, value: total_ns}
      - {state: impossible, from: sched.offcpu, value: total_ns, filter: 'total_ns > 1000000000000'}
";
    let output = written_session(SESSION, &["--input-format", "yaml"]);
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() && machine_cannot_serve(&said) {
        eprintln!(
            "SKIPPED: this machine cannot serve `sched.offcpu`, so the zero it would have \
             checked -- a part keeping blocks longer than the program lived -- is not checked: \
             {}",
            machine_s_reason(&said)
        );
        return;
    }
    assert!(output.status.success(), "{said}");
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!("{error}: {said}"));
    let losses = &document["results"][0]["losses"];
    let table = &document["decompositions"][0]["tables"][0];
    let rows = rows(table);
    let life = number(&table["whole"]["amount"]);

    let impossible = rows["impossible"];
    assert_eq!(
        impossible["amount"].as_f64(),
        Some(0.0),
        "no block outlasts a {life} ns life, and the row says so rather than `not read` \
         (losses {losses}): {table}"
    );
    assert!(impossible.get("missing").is_none(), "{table}");
    assert!(
        impossible["note"]
            .as_str()
            .unwrap_or_default()
            .contains("passed the filter"),
        "the row says how it knows: {table}"
    );

    let blocked = number(&rows["blocked"]["amount"]);
    assert!(
        (300e6..=life).contains(&blocked),
        "the program slept 300 ms of its {life} ns life, and the unfiltered row holds {blocked} \
         ns: {table}"
    );
    assert_eq!(
        table["parts"].as_f64(),
        Some(blocked),
        "the zero adds to the parts instead of taking them away: {table}"
    );
    assert!(table["residue"].as_f64().is_some(), "{table}");
}

// -- the thirteenth wave: an interrupt named, the new match kinds ----------

/// Finding 48's other half, read through the join: a waker of 0 from an
/// interrupt is the interrupt, and a task that released a task is a task.
///
/// The workload's main thread sleeps twenty times -- each `nanosleep` ended by
/// its hrtimer's hard interrupt -- then reads a pipe twenty times, each read
/// released by a helper thread's write after the helper's own sleep. It says
/// both thread ids on stderr. So the main thread has at least twenty wakeups
/// whose chain is the timer and nothing else, and at least twenty whose chain
/// runs through the helper; and no chain may call the timer the idle task,
/// which is what every one of the first twenty read as before the chain knew
/// `waker_context`.
#[test]
fn a_timer_is_named_as_an_interrupt_and_a_thread_as_a_task() {
    if !has("python3") {
        eprintln!("SKIPPED: the workload is a python3 program, and this machine has none");
        return;
    }
    let Some(run) = session(
        "join-interrupt-or-task.yml",
        "that twenty sleeps are released by `interrupt (hardirq)` and twenty reads by the helper",
    ) else {
        return;
    };
    let (main, helper) = (run.truth("main") as i64, run.truth("helper") as i64);
    let (sleeps, releases) = (run.truth("sleeps"), run.truth("releases"));
    let join = run.join("timer-or-thread");
    assert_eq!(join["evidence"], "WAKEUP");
    let rows = join["top"].as_array().expect("a join has rows");
    let wakeups = |matches: &dyn Fn(&str) -> bool| -> f64 {
        rows.iter()
            .filter(|row| matches(row["key"].as_str().unwrap_or_default()))
            .map(|row| number(&row["wakeups"]))
            .sum()
    };
    let (main_task, helper_task) = (format!("task:{main}"), format!("task:{helper}"));
    let by_timer = wakeups(&|key| key == format!("interrupt (hardirq) -> {main_task}"));
    let by_helper = wakeups(&|key| key.ends_with(&format!("{helper_task} -> {main_task}")));
    let rendered: Vec<&str> = rows.iter().filter_map(|row| row["key"].as_str()).collect();
    eprintln!("main {main}, helper {helper}: {rendered:#?}");

    assert!(
        by_timer >= sleeps,
        "{sleeps} sleeps end in {sleeps} timer interrupts, and {by_timer} of the main thread's \
         wakeups were named `interrupt (hardirq)`: {rendered:?}"
    );
    assert!(
        by_helper >= releases,
        "{releases} reads were released by the helper's writes, and {by_helper} wakeups of the \
         main thread name it: {rendered:?}"
    );
    assert!(
        !rendered.iter().any(|key| key.contains("task:0 (idle)")),
        "a timer was called the idle task: {rendered:?}"
    );
    // Two hops deep, the helper was released by its own timer just before it
    // wrote: the far end of a chain is named as the interrupt too.
    assert!(
        rendered
            .iter()
            .any(|key| *key == format!("interrupt (hardirq) -> {helper_task} -> {main_task}")),
        "no chain reached back through the helper to its timer: {rendered:?}"
    );
}

/// `overlap` against a program that says when each of its waits began and
/// ended.
///
/// Four waits of 250 ms each lie across polls 100 ms apart, so every wait is
/// split between two or three intervals. What each interval should hold is
/// computed here from the program's own brackets -- the overlap of each
/// `[began, ended]` with the interval -- and the tool's credit must match it
/// to the gap between a bracket and the kernel's span of the same wait (the
/// interpreter's own call around the syscall), plus whatever futex waits of
/// the task lie outside every bracket, which the run itself reports. The
/// separation that matters is the one a naive join fails: a 250 ms wait
/// credited to the one reading it ended in puts more in a 100 ms interval
/// than the interval holds.
#[test]
fn a_wait_across_several_readings_is_split_by_its_overlap_with_each() {
    waits_are_split_by_their_overlap(
        "join-overlap-waits.yml",
        "that four 250 ms waits are split across 100 ms readings by their overlap with each",
        "waiting-per-interval",
        None,
    );
}

/// `overlap` with no key, against the same program: the machine's memory
/// pressure is one series with no task on it, and each of the program's waits
/// is laid across its intervals as across the task's own. The ground truth is
/// the program's brackets and the intervals the readings state; the rows are
/// the machine's, keyed `machine`, and nothing is counted keyless.
#[test]
fn a_wait_is_split_across_the_machines_readings_with_no_key() {
    waits_are_split_by_their_overlap(
        "join-overlap-machine.yml",
        "that four 250 ms waits are split across the machine's 100 ms pressure readings",
        "waiting-per-pressure-interval",
        Some(omn::join::MACHINE),
    );
}

/// The four waits of `join-overlap-*.yml`, each interval's credit held to what
/// the program's brackets put in it. `series` is the rows' key: the waiting
/// task's when `None`.
fn waits_are_split_by_their_overlap(file: &str, what: &str, id: &str, series: Option<&str>) {
    /// The gap between the program's bracket and the kernel's span, per wait:
    /// an interpreted call on either side of the syscall. Measured here at
    /// 20 to 60 us; ten times that, so a slow interpreter is not taken for a
    /// wrong tool, and still three orders of magnitude under a 250 ms wait.
    const BRACKET_NS: f64 = 500_000.0;

    if !has("python3") {
        eprintln!("SKIPPED: the workload is a python3 program, and this machine has none");
        return;
    }
    let Some(run) = session(file, what) else {
        return;
    };
    let waits: Vec<(i64, f64, f64)> = run
        .reported("omn-wait ")
        .iter()
        .map(|fields| {
            (
                fields[0].parse().expect("a tid"),
                fields[1].parse().expect("a start"),
                fields[2].parse().expect("an end"),
            )
        })
        .collect();
    assert_eq!(
        waits.len(),
        4,
        "the workload waits four times:\n{}",
        run.stderr
    );
    let task = series.map_or_else(|| format!("task:{}", waits[0].0), str::to_owned);

    let join = run.join(id);
    assert_eq!(join["evidence"], "TEMPORAL");
    if series.is_some() {
        assert_eq!(
            join["keyless"].as_u64().unwrap_or(0),
            0,
            "with no key, no event can lack it: {join}"
        );
    }
    let intervals: Vec<(f64, f64, f64)> = join["top"]
        .as_array()
        .expect("rows")
        .iter()
        .filter(|row| row["key"] == task.as_str())
        .map(|row| {
            (
                number(&row["opened_ns"]),
                number(&row["closed_ns"]),
                number(&row["change"]["delta"]),
            )
        })
        .collect();

    // What the program's own brackets put in each interval.
    let overlap = |start: f64, end: f64| -> f64 {
        waits
            .iter()
            .map(|(_, began, ended)| (ended.min(end) - began.max(start)).max(0.0))
            .sum()
    };
    // Futex waits of the task that lie in no bracket: the interpreter's own,
    // at start and exit. Whatever of them the tool credits is slack here.
    let strays: f64 = run.result("waits")["outputs"][0]["events"]
        .as_array()
        .expect("raw waits")
        .iter()
        .filter(|event| {
            let ts = number(&event["ts_ns"]);
            !waits
                .iter()
                .any(|(_, began, ended)| ts >= began - BRACKET_NS && ts <= *ended)
        })
        .filter_map(|event| {
            event["attrs"]
                .as_array()?
                .iter()
                .find(|attr| attr["name"] == "total_ns")
                .map(|attr| number(&attr["value"]))
        })
        .sum();

    let mut split = 0;
    for (start, end, credited) in &intervals {
        let expected = overlap(*start, *end);
        let slack = 2.0 * BRACKET_NS + strays;
        eprintln!(
            "  ({start}, {end}]  {:>8.3} ms long  credited {:>8.3} ms  the brackets hold {:>8.3} ms",
            (end - start) / 1e6,
            credited / 1e6,
            expected / 1e6
        );
        assert!(
            (credited - expected).abs() <= slack,
            "an interval of {:.3} ms was credited {:.3} ms of waiting, and the program's own \
             brackets put {:.3} ms in it (slack {:.3} ms)",
            (end - start) / 1e6,
            credited / 1e6,
            expected / 1e6,
            slack / 1e6
        );
        assert!(
            *credited <= end - start + slack,
            "an interval of {:.3} ms holds {:.3} ms of waiting: a wait was credited whole",
            (end - start) / 1e6,
            credited / 1e6
        );
        split += usize::from(expected > 0.0);
    }
    assert!(
        split >= 2 * waits.len(),
        "four 250 ms waits across 100 ms readings touch at least eight intervals; the tool \
         credited {split}"
    );
}

/// A rule with no key is `overlap`'s alone. Written for `eq`, it is refused
/// before anything runs, and the refusal names `via`; the same session with
/// `overlap` validates and resolves. Both are asked with `--dump-resolved`,
/// which validates and resolves and runs nothing.
#[test]
fn a_rule_with_no_key_is_refused_on_every_match_but_overlap() {
    let session = |kind: &str| {
        format!(
            "name: a machine-wide join
requests:
  - {{label: pressure, source: mem.pressure, target: system, mode: poll@100ms, emit: [raw]}}
  - {{label: waits, source: sched.futex, target: {LAUNCH}, mode: trace, emit: [raw]}}
joins:
  - {{id: no-key, from: mem.pressure, to: sched.futex, match: {kind}, value: total_ns}}
"
        )
    };
    let output = written_session(
        &session("eq"),
        &["--input-format", "yaml", "--dump-resolved"],
    );
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(2), "an `eq` with no key: {said}");
    assert!(
        said.contains("names no key") && said.contains("via"),
        "the refusal says what is missing: {said}"
    );

    let output = written_session(
        &session("overlap"),
        &["--input-format", "yaml", "--dump-resolved"],
    );
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() && output.status.code() == Some(1) && machine_cannot_serve(&said) {
        eprintln!(
            "SKIP (overlap half): the session is valid and this machine does not serve it, so \
             only the `eq` refusal was checked. The machine's reason: {}",
            machine_s_reason(&said)
        );
        return;
    }
    assert!(
        output.status.success(),
        "`overlap` with no key is a valid session, and it was refused: {said}"
    );
    assert!(
        !said.contains("names no key") && !said.contains("does not carry"),
        "`overlap` with no key is warned about as if it needed one: {said}"
    );
    // One resolved request per JSON document, each with the backend chosen.
    let resolved: Vec<serde_json::Value> = serde_json::Deserializer::from_slice(&output.stdout)
        .into_iter()
        .collect::<Result<_, _>>()
        .expect("--dump-resolved writes JSON");
    let labels: Vec<&str> = resolved
        .iter()
        .filter(|each| each["backend"]["id"].is_string())
        .filter_map(|each| each["request"]["label"].as_str())
        .collect();
    assert_eq!(
        labels,
        ["pressure", "waits"],
        "both requests resolve: {resolved:?}"
    );
}

/// The CPUs this machine has online, as the kernel lists them in
/// `/sys/devices/system/cpu/online` (`0-23`, `0,2-5`): the ground truth for a
/// per-cpu source's series, read without `omn`.
fn online_cpus() -> Option<usize> {
    let listed = std::fs::read_to_string("/sys/devices/system/cpu/online").ok()?;
    let mut count = 0;
    for range in listed.trim().split(',') {
        let (first, last) = match range.split_once('-') {
            Some((first, last)) => (first.parse::<usize>().ok()?, last.parse::<usize>().ok()?),
            None => {
                let only = range.parse::<usize>().ok()?;
                (only, only)
            }
        };
        count += last - first + 1;
    }
    Some(count)
}

/// With no key, every reading of `from` is one series; a source that carries
/// one per CPU is refused by name, with the number of series it carried. The
/// ground truth is the kernel's own list of online CPUs: `power.freq` polled
/// for the system reads one series per CPU, so the refusal counts exactly as
/// many, and names the key they differ by.
#[test]
fn with_no_key_a_per_cpu_poll_is_refused_with_as_many_series_as_the_machine_has_cpus() {
    let Some(cpus) = online_cpus() else {
        eprintln!("SKIP: /sys/devices/system/cpu/online is unreadable here, so there is no count to hold the refusal to");
        return;
    };
    if cpus < 2 {
        eprintln!(
            "SKIP: this machine has {cpus} CPU online, so a per-cpu poll is one series and \
             there is nothing to refuse"
        );
        return;
    }
    let output = written_session(
        &format!(
            "name: a per-cpu poll with no key
requests:
  - {{label: freq, source: power.freq, target: system, mode: poll@100ms, emit: [raw]}}
  - {{label: waits, source: sched.futex, target: {LAUNCH}, mode: trace, emit: [raw]}}
joins:
  - {{id: no-key, from: power.freq, to: sched.futex, match: overlap, value: total_ns}}
"
        ),
        &["--input-format", "yaml", "--format", "json"],
    );
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    let refusal = said
        .lines()
        .find(|line| line.contains("names no key, so it reads every"));
    if refusal.is_none() && output.status.code() == Some(1) && machine_cannot_serve(&said) {
        eprintln!(
            "SKIP: this machine does not serve power.freq or sched.futex, so the refusal of \
             {cpus} series was not checked. The machine's reason: {}",
            machine_s_reason(&said)
        );
        return;
    }
    let refusal = refusal.unwrap_or_else(|| {
        panic!("a keyless overlap over {cpus} CPUs' readings was not refused: {said}")
    });
    assert_eq!(output.status.code(), Some(1), "{said}");
    assert!(
        refusal.contains(&format!("carried {cpus} of them")),
        "the machine has {cpus} CPUs online, and the refusal counts otherwise: {refusal}"
    );
    assert!(
        refusal.contains("cpu:") && refusal.contains("via"),
        "the refusal names the key the series differ by and what to write: {refusal}"
    );
}

// ------------------------------------------------------------ the load test

/// Live heap bytes, and the most there ever were since the last reset, for the
/// load test's bound on memory. Counted for the whole test binary, which is
/// why the load test runs itself in a child process of its own: the other
/// tests' allocations would otherwise be counted as the engine's.
mod heap {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub struct Counting;

    static LIVE: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc(layout) };
            if !pointer.is_null() {
                let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
                PEAK.fetch_max(live, Ordering::Relaxed);
            }
            pointer
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            unsafe { System.dealloc(pointer, layout) };
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
    }

    pub fn live() -> usize {
        LIVE.load(Ordering::Relaxed)
    }

    /// Starts a new peak from what is live now.
    pub fn reset_peak() {
        PEAK.store(live(), Ordering::Relaxed);
    }

    pub fn peak() -> usize {
        PEAK.load(Ordering::Relaxed)
    }
}

#[global_allocator]
static HEAP: heap::Counting = heap::Counting;

/// What the load stream did, counted by the generator as it wrote each event.
///
/// Every number here is known by construction, before the engine runs: the
/// stream is built of segments in each of which exactly one thing can happen
/// to a state, and every segment but the last leaves the map empty.
#[derive(Debug, Default, Clone, PartialEq)]
struct Expected {
    events: u64,
    opened: u64,
    closed: u64,
    replaced: u64,
    expired: u64,
    overflowed: u64,
    matched: u64,
    unmatched: u64,
    keyless: u64,
    /// States open when the `from` source's window closed, and the `to`
    /// events that came after it for them.
    left_open: u64,
}

/// The load test's constants: a window of 10 ms, a map of 1000 states, and a
/// stream that advances one microsecond per event, so that no segment lasts as
/// long as the window and every expiry is the one a segment asked for.
const LOAD_WINDOW_NS: u64 = 10_000_000;
const LOAD_CAPACITY: u64 = 1000;
const LOAD_TICK_NS: u64 = 1_000;
/// States per segment, and how far past the map the overflow segment goes.
const LOAD_LIVES: u64 = 500;
const LOAD_OVER: u64 = 200;
/// How many pointers the block requests are drawn from: fewer than the lives
/// of one cycle, so every pointer is reissued, cycle after cycle.
const LOAD_POINTERS: u64 = LOAD_CAPACITY + LOAD_OVER;

fn load_rule() -> Rule {
    serde_norway::from_str(&format!(
        "{{id: load, from: blk.rq, to: blk.seg, via: blk, match: scoped_eq, \
           opens: \"op == 'issue'\", closes: \"op == 'done'\", \
           window: {}ms, capacity: {LOAD_CAPACITY}}}",
        LOAD_WINDOW_NS / 1_000_000
    ))
    .expect("the load rule")
}

struct Stream {
    events: Vec<Event>,
    ts_ns: u64,
    expected: Expected,
    /// Distinguishes the lives of one pointer: the scope a block request is
    /// stamped with. Never repeats, so no two lives collapse.
    lives: u64,
}

impl Stream {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            ts_ns: 1_000_000_000,
            expected: Expected::default(),
            lives: 0,
        }
    }

    fn pointer(index: u64) -> u64 {
        0xffff_8881_0000_0000 + (index % LOAD_POINTERS) * 0x200
    }

    fn push(&mut self, kind: &str, key: Option<(u64, u64)>, attrs: Vec<Attr>) {
        self.ts_ns += LOAD_TICK_NS;
        self.expected.events += 1;
        let seq = self.expected.events;
        self.events.push(Event {
            ts_ns: self.ts_ns,
            ts_err_ns: 0,
            kind: kind.to_owned(),
            keys: key
                .map(|(ptr, open_ts)| vec![KeyValue::Blk { ptr, open_ts }])
                .unwrap_or_default(),
            attrs,
            collector_id: CollectorId::new(kind),
            seq,
        });
    }

    /// A new life of a pointer: its issue, with a scope no life had before.
    fn issue(&mut self, ptr: u64) -> (u64, u64) {
        self.lives += 1;
        let key = (ptr, self.lives);
        self.push("blk.rq", Some(key), vec![Attr::text("op", "issue")]);
        self.expected.opened += 1;
        key
    }

    fn done(&mut self, key: (u64, u64)) {
        self.push("blk.rq", Some(key), vec![Attr::text("op", "done")]);
    }

    fn segment(&mut self, key: (u64, u64)) {
        self.push("blk.seg", Some(key), vec![Attr::uint("bytes", 4096)]);
    }

    /// One of each segment, starting the pointers at `first`.
    fn cycle(&mut self, first: u64) {
        let lives = |from: u64| (from..from + LOAD_LIVES).map(Stream::pointer);

        // Closed: issued, one segment each, done.
        let keys: Vec<_> = lives(first).map(|ptr| self.issue(ptr)).collect();
        for key in &keys {
            self.segment(*key);
        }
        for key in &keys {
            self.done(*key);
        }
        self.expected.closed += LOAD_LIVES;
        self.expected.matched += LOAD_LIVES;

        // Reissued: each pointer issued again before it was done, which ends
        // the first life; a segment of the first life then finds the second
        // open under its pointer and is not its, and one of the second is.
        let old: Vec<_> = lives(first + 7).map(|ptr| self.issue(ptr)).collect();
        let new: Vec<_> = lives(first + 7).map(|ptr| self.issue(ptr)).collect();
        for key in &old {
            self.segment(*key);
        }
        for key in &new {
            self.segment(*key);
        }
        for key in &new {
            self.done(*key);
        }
        self.expected.closed += 2 * LOAD_LIVES;
        self.expected.replaced += LOAD_LIVES;
        self.expected.unmatched += LOAD_LIVES;
        self.expected.matched += LOAD_LIVES;

        // Expired: issued and never done, then one segment of a pointer no
        // request holds, two windows later, is the first event past them.
        for ptr in lives(first + 13) {
            self.issue(ptr);
        }
        self.ts_ns += 2 * LOAD_WINDOW_NS;
        self.segment((0xdead_0000, 0));
        self.expected.expired += LOAD_LIVES;
        self.expected.unmatched += 1;

        // Overflowed: more requests in flight than the map holds, so the
        // oldest are evicted; their segments find nothing, the rest are done.
        let keys: Vec<_> = (first..first + LOAD_CAPACITY + LOAD_OVER)
            .map(|index| self.issue(Stream::pointer(index)))
            .collect();
        let (evicted, kept) = keys.split_at(LOAD_OVER as usize);
        for key in evicted.iter().chain(kept) {
            self.segment(*key);
        }
        for key in kept {
            self.done(*key);
        }
        self.expected.overflowed += LOAD_OVER;
        self.expected.unmatched += LOAD_OVER;
        self.expected.matched += LOAD_CAPACITY;
        self.expected.closed += LOAD_CAPACITY;

        // Keyless: segments of no block request.
        for _ in 0..LOAD_OVER {
            self.push("blk.seg", None, vec![Attr::uint("bytes", 512)]);
        }
        self.expected.keyless += LOAD_OVER;
    }

    /// A stream of at least `events` events, ending on requests still in
    /// flight when the `from` source's window closes, and segments of theirs
    /// after it. Returns the stream and the instant the window closed.
    fn load(events: u64) -> (Self, u64) {
        let mut stream = Stream::new();
        let mut first = 0;
        while stream.expected.events < events {
            stream.cycle(first);
            first += 311;
        }
        let keys: Vec<_> = (0..LOAD_OVER)
            .map(|index| stream.issue(Stream::pointer(first + index)))
            .collect();
        let closed_ns = stream.ts_ns;
        for key in keys {
            stream.segment(key);
        }
        stream.expected.left_open = LOAD_OVER;
        (stream, closed_ns)
    }
}

fn balances(states: &join::StateStats) -> bool {
    states.opened
        == states.closed
            + states.expired
            + states.overflowed
            + states.window_closed
            + states.still_open
}

/// The load's windows: the requests' source detached before the end, the
/// segments' watched to it.
fn load_windows(closed_ns: u64) -> BTreeMap<String, omn::collect::Watched> {
    let mut windows = BTreeMap::new();
    windows.insert(
        "blk.rq".to_owned(),
        omn::collect::Watched {
            run_opened_ns: 0,
            opened_ns: 0,
            closed_ns,
            attached: true,
            attach_ns: None,
        },
    );
    windows.insert(
        "blk.seg".to_owned(),
        omn::collect::Watched {
            run_opened_ns: 0,
            opened_ns: 0,
            closed_ns: u64::MAX,
            attached: true,
            attach_ns: None,
        },
    );
    windows
}

/// The load as a run hands it over: one collection per source, each in
/// order, and the joins' input either those collections walked apart
/// (`join::apply_merging`) or laid end to end into one stream that is not in
/// order (`join::apply_within` over it, which is what `assemble` in
/// `src/main.rs` does today). Returns the heap each needed at its peak beyond
/// its input, after holding both answers to the generator's counts.
fn split_load(events: u64) -> (usize, usize) {
    let rule = load_rule();
    let (stream, closed_ns) = Stream::load(events);
    let expected = stream.expected;
    let windows = load_windows(closed_ns);
    let (requests, segments): (Vec<Event>, Vec<Event>) = stream
        .events
        .into_iter()
        .partition(|event| event.kind == "blk.rq");

    let before = heap::live();
    heap::reset_peak();
    let apart =
        join::apply_merging(&rule, &[&requests, &segments], &windows).expect("the rule applies");
    let merged_peak = heap::peak() - before;

    let laid: Vec<Event> = requests.into_iter().chain(segments).collect();
    let before = heap::live();
    heap::reset_peak();
    let end_to_end = join::apply_within(&rule, &laid, &windows).expect("the rule applies");
    let laid_peak = heap::peak() - before;

    for (name, correlation) in [("apart", &apart), ("end to end", &end_to_end)] {
        assert!(
            balances(&correlation.states),
            "{name}: {:?}",
            correlation.states
        );
        assert_eq!(correlation.states.opened, expected.opened, "{name}: opened");
        assert_eq!(correlation.matched, expected.matched, "{name}: matched");
        assert_eq!(
            correlation.unmatched, expected.unmatched,
            "{name}: unmatched"
        );
        assert_eq!(
            correlation.outside_window, expected.left_open,
            "{name}: outside the window"
        );
    }
    eprintln!(
        "load as a run hands it over: {} events in two collections; walked apart, peak {:.1} \
         MiB beyond them; laid end to end, {:.1} MiB",
        expected.events,
        merged_peak as f64 / (1 << 20) as f64,
        laid_peak as f64 / (1 << 20) as f64
    );
    (merged_peak, laid_peak)
}

/// One load: the stream built, applied with the `from` source's window
/// closing before the end and without, and every count held to the
/// generator's. Returns the heap the two applications needed at their peak,
/// beyond the stream itself.
fn one_load(events: u64) -> usize {
    let rule = load_rule();
    let started = std::time::Instant::now();
    let (stream, closed_ns) = Stream::load(events);
    let built = started.elapsed();
    let expected = &stream.expected;
    let windows = load_windows(closed_ns);

    let before = heap::live();
    heap::reset_peak();
    let within = join::apply_within(&rule, &stream.events, &windows).expect("the rule applies");
    let whole = join::apply(&rule, &stream.events).expect("the rule applies");
    let peak = heap::peak() - before;
    let applied = started.elapsed() - built;

    for (name, correlation) in [("within", &within), ("whole", &whole)] {
        let states = &correlation.states;
        assert!(
            balances(states),
            "{name}: the states do not balance: {states:?}"
        );
        assert_eq!(states.opened, expected.opened, "{name}: opened");
        assert_eq!(states.closed, expected.closed, "{name}: closed");
        assert_eq!(states.replaced, expected.replaced, "{name}: replaced");
        assert_eq!(
            states.scope_collapsed, 0,
            "{name}: every life has its own scope"
        );
        assert_eq!(states.expired, expected.expired, "{name}: expired");
        assert_eq!(states.overflowed, expected.overflowed, "{name}: overflowed");
        assert_eq!(
            states.peak_open, LOAD_CAPACITY,
            "{name}: the map filled, and never held more than it may"
        );
        assert_eq!(
            correlation.unmatched, expected.unmatched,
            "{name}: unmatched"
        );
        assert_eq!(correlation.keyless, expected.keyless, "{name}: keyless");
    }
    // Watching to the end, the last requests are still open and their
    // segments land on them; with the window closed under them, they end at
    // the close and their segments are outside it.
    assert_eq!(
        (whole.states.still_open, whole.states.window_closed),
        (expected.left_open, 0)
    );
    assert_eq!(whole.matched, expected.matched + expected.left_open);
    assert_eq!(whole.outside_window, 0);
    assert_eq!(
        (within.states.still_open, within.states.window_closed),
        (0, expected.left_open)
    );
    assert_eq!(within.matched, expected.matched);
    assert_eq!(within.outside_window, expected.left_open);

    // Nothing the engine built outlives its answer but the answer, whose
    // attributions are cut to the rule's `top`.
    drop((within, whole));
    let after = heap::live();
    assert!(
        after <= before + 64 * 1024,
        "the engine left {} bytes live after returning",
        after.saturating_sub(before)
    );
    eprintln!(
        "load: {} events, {} states, peak {:.1} MiB beyond the stream's {:.1} MiB; built in \
         {built:.1?}, applied twice in {applied:.1?}",
        expected.events,
        expected.opened,
        peak as f64 / (1 << 20) as f64,
        before as f64 / (1 << 20) as f64
    );
    peak
}

/// Runs one load test in a child process of this binary, alone, so that the
/// heap it counts is its own.
fn in_a_child(test: &str, env: &[(&str, &str)]) {
    if std::env::var_os("OMN_JOIN_LOAD_CHILD").is_some() {
        return;
    }
    let status = Command::new(std::env::current_exe().expect("this test binary"))
        .args([
            test,
            "--exact",
            "--test-threads=1",
            "--nocapture",
            "--include-ignored",
        ])
        .env("OMN_JOIN_LOAD_CHILD", "1")
        .envs(env.iter().copied())
        .status()
        .expect("the test binary runs again");
    assert!(
        status.success(),
        "the load test failed in its child: {status}"
    );
}

fn is_child() -> bool {
    std::env::var_os("OMN_JOIN_LOAD_CHILD").is_some()
}

/// ROADMAP.md phase 5: no open-state leak, under load.
///
/// A million and a half synthetic block-request events of recycled pointers, through a
/// `scoped_eq` rule with a window and a map smaller than what is in flight:
/// requests done, reissued before they were done, never done and expired,
/// evicted from a full map, segments of no request, and requests in flight
/// when the `from` source's window closes. Every count is held to what the
/// generator wrote, the open-state balance included, and the map to its
/// capacity. The memory: the engine holds at most `capacity` open states and
/// the rule's `top` attributions, and walks a stream already in order where it
/// lies, so what it needs does not grow with the stream: measured, 97.2 MiB
/// beyond the stream for the million events before the sixteenth wave (an
/// index of the input and a record of every state that ended), 0.7 MiB after.
/// A twice longer stream may take no more than the shorter's peak and a
/// margin, every peak stays under [`LOAD_BOUND`], and nothing is left live
/// afterwards. And the input as a run hands it over -- one collection per
/// source, each in order -- walked apart by `apply_merging` stays under the
/// bound too, while the same collections laid end to end, which is what
/// `assemble` gives the engine today, cost an index of the whole run.
#[test]
fn a_join_under_load_ends_every_state_it_opened_and_holds_no_more_than_its_map() {
    in_a_child(
        "a_join_under_load_ends_every_state_it_opened_and_holds_no_more_than_its_map",
        &[],
    );
    if !is_child() {
        return;
    }
    let short = one_load(500_000);
    let long = one_load(1_000_000);
    for (events, peak) in [(500_000, short), (1_000_000, long)] {
        assert!(
            peak < LOAD_BOUND,
            "{events} events took {peak} bytes beyond the stream, more than the {LOAD_BOUND} a \
             map of {LOAD_CAPACITY} states and its top need"
        );
    }
    assert!(
        long < short + LOAD_BOUND / 4,
        "twice the stream took {long} bytes against {short}: the engine's memory grows with \
         the run"
    );

    // The collections of a run, each in order and their concatenation not:
    // walked apart they cost what one ordered stream does; laid end to end
    // the engine indexes the whole run, eight bytes an event at least.
    let (apart, laid) = split_load(1_000_000);
    assert!(
        apart < LOAD_BOUND,
        "two collections walked apart took {apart} bytes beyond them, more than the \
         {LOAD_BOUND} one ordered stream may"
    );
    assert!(
        laid >= 8 * 1_000_000,
        "one stream out of order took only {laid} bytes beyond it: the index of a million \
         references the engine sorts it by is eight megabytes"
    );
}

/// What one application of the load rule may hold beyond its input: the map
/// of [`LOAD_CAPACITY`] states with their age index and labels, and the
/// buffer of attributions it cuts back to the rule's `top`, with room for the
/// allocator's rounding. Not a calibrated figure: two orders of magnitude
/// under what the engine held before it was bounded.
const LOAD_BOUND: usize = 4 << 20;

/// The same for an hour, by hand (`OMN_JOIN_LOAD_SECS` shortens it):
///
/// ```console
/// cargo test --release --test join_engine -- --ignored an_hour_of_joins
/// OMN_JOIN_LOAD_SECS=300 cargo test --release --test join_engine -- --ignored an_hour_of_joins
/// ```
///
/// Rounds of two million events for as long as asked, each held to its own
/// counts, and the heap after every round held to the first's: an engine that
/// leaked open states, or anything else, across an hour of rounds grows.
#[test]
#[ignore = "an hour long; run by hand"]
fn an_hour_of_joins_leaks_no_state() {
    in_a_child("an_hour_of_joins_leaks_no_state", &[]);
    if !is_child() {
        return;
    }
    let seconds: u64 = std::env::var("OMN_JOIN_LOAD_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3600);
    let deadline = std::time::Instant::now() + StdDuration::from_secs(seconds);
    let baseline = heap::live();
    let mut rounds = 0u64;
    let mut first_peak = None;
    while std::time::Instant::now() < deadline {
        let peak = one_load(2_000_000);
        let first = *first_peak.get_or_insert(peak);
        assert!(
            peak < first * 2 && peak < LOAD_BOUND,
            "round {rounds}: the peak grew from {first} to {peak} bytes (bound {LOAD_BOUND})"
        );
        assert!(
            heap::live() <= baseline + 64 * 1024,
            "round {rounds}: {} bytes live after the round",
            heap::live().saturating_sub(baseline)
        );
        rounds += 1;
    }
    eprintln!("{rounds} rounds of two million events in {seconds} s, nothing left live");
}

// ------------------------------------------- a boundary is a rule, end to end

/// Two per-cpu polls of the machine that no test or session had joined, with
/// the join written here and nowhere else. `fs:sys` serves both with no
/// privilege on a machine with cpufreq and cpuidle.
fn cpu_join_session(rule: &str, poll: &str) -> String {
    format!(
        "name: a boundary no test had joined
requests:
  - {{label: freq, source: power.freq, target: system, mode: poll@{poll}, emit: [raw]}}
  - {{label: idle, source: power.cstate, target: system, mode: poll@{poll}, emit: [raw]}}
joins:
  - {{id: freq-idle, from: power.freq, to: power.cstate, via: cpu, match: eq, top: 4096{rule}}}
"
    )
}

/// Runs a session from a file, writing its results to `-o`, and hands back
/// the directory, or the machine's reason when it cannot serve the sources.
fn run_to_dir(name: &str, session: &str, format: &str) -> Result<(PathBuf, String), String> {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("join-engine-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let file = directory.join("session.yml");
    std::fs::write(&file, session).expect("the session is written");
    let out = directory.join("out");
    let output = Command::new(BINARY)
        .args(["-f"])
        .arg(&file)
        .args(["--duration", "500ms", "--format", format, "-o"])
        .arg(&out)
        .output()
        .expect("omn runs");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        return Err(format!("{stdout}{stderr}"));
    }
    Ok((out, stdout))
}

/// Which cpus a run's raw events of one request carried, and how many events.
fn cpus_of(document: &serde_json::Value, label: &str) -> (std::collections::BTreeSet<u64>, u64) {
    let mut cpus = std::collections::BTreeSet::new();
    let mut events = 0;
    let results = document["results"].as_array().expect("results");
    let result = results
        .iter()
        .find(|result| result["label"] == label)
        .unwrap_or_else(|| panic!("no request `{label}`"));
    for output in result["outputs"].as_array().expect("outputs") {
        if output["emit"] != "raw" {
            continue;
        }
        for event in output["events"].as_array().expect("events") {
            events += 1;
            for key in event["keys"].as_array().into_iter().flatten() {
                if let Some(cpu) = key["cpu"]["cpu"].as_u64() {
                    cpus.insert(cpu);
                }
            }
        }
    }
    (cpus, events)
}

fn online_set() -> Option<std::collections::BTreeSet<u64>> {
    let listed = std::fs::read_to_string("/sys/devices/system/cpu/online").ok()?;
    let mut set = std::collections::BTreeSet::new();
    for range in listed.trim().split(',') {
        let (first, last) = match range.split_once('-') {
            Some((first, last)) => (first.parse::<u64>().ok()?, last.parse::<u64>().ok()?),
            None => {
                let only = range.parse::<u64>().ok()?;
                (only, only)
            }
        };
        set.extend(first..=last);
    }
    Some(set)
}

/// ROADMAP.md phase 5: adding a boundary means adding a rule, zero lines of
/// engine.
///
/// Both halves. The engine names no source: not one catalog id is a string
/// in `src/join.rs` outside its tests, so nothing in it can be a case for a
/// pair of sources. And a join between two sources no test or session had
/// joined -- `power.freq` to `power.cstate`, by cpu -- is written in the
/// session and runs: every attribution is a cpu the kernel lists online
/// (`/sys/devices/system/cpu/online`, read here), every cpu both sources read
/// is attributed, and each is credited exactly the `power.cstate` readings of
/// its own cpu that came after its first frequency reading -- counted here off
/// the raw events, not off the join.
#[test]
fn a_new_boundary_is_a_rule_in_a_session_and_no_line_of_engine() {
    let engine = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/join.rs"))
        .expect("the engine's source");
    let engine = engine
        .split("#[cfg(test)]")
        .next()
        .expect("the engine before its tests");
    let catalog = omn::Catalog::embedded().expect("the embedded catalog loads");
    let named: Vec<&str> = catalog
        .entries()
        .iter()
        .map(|entry| entry.id.as_str())
        .filter(|id| engine.contains(&format!("\"{id}\"")))
        .collect();
    assert!(
        named.is_empty(),
        "the engine names sources, so a boundary between them is code: {named:?}"
    );

    let Some(online) = online_set() else {
        eprintln!("SKIPPED the run: /sys/devices/system/cpu/online is unreadable here");
        return;
    };
    let (out, _) = match run_to_dir("boundary", &cpu_join_session("", "100ms"), "json") {
        Ok(ran) => ran,
        Err(said) if machine_cannot_serve(&said) => {
            eprintln!(
                "SKIPPED the run: this machine serves neither power.freq nor power.cstate \
                 through /sys: {}",
                machine_s_reason(&said)
            );
            return;
        }
        Err(said) => panic!("the session did not run: {said}"),
    };
    let document: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(out.join("results.json")).expect("results.json"),
    )
    .expect("results.json parses");
    let (freq_cpus, _) = cpus_of(&document, "freq");
    let (idle_cpus, _) = cpus_of(&document, "idle");
    let both: std::collections::BTreeSet<u64> =
        freq_cpus.intersection(&idle_cpus).copied().collect();
    assert!(!both.is_empty(), "no cpu was read by both sources");
    assert!(
        freq_cpus.is_subset(&online) && idle_cpus.is_subset(&online),
        "a cpu the kernel does not list online: {freq_cpus:?} {idle_cpus:?} against {online:?}"
    );

    // What each cpu must be credited: its idle readings from its first
    // frequency reading on, the opening side first at an equal instant.
    let raw = |label: &str| -> Vec<(u64, u64)> {
        let result = document["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|result| result["label"] == label)
            .unwrap();
        result["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|output| output["emit"] == "raw")
            .flat_map(|output| output["events"].as_array().unwrap())
            .filter_map(|event| {
                let cpu = event["keys"]
                    .as_array()?
                    .iter()
                    .find_map(|key| key["cpu"]["cpu"].as_u64())?;
                Some((cpu, event["ts_ns"].as_u64()?))
            })
            .collect()
    };
    let first_freq: BTreeMap<u64, u64> =
        raw("freq")
            .into_iter()
            .fold(BTreeMap::new(), |mut first, (cpu, ts)| {
                let at = first.entry(cpu).or_insert(ts);
                *at = (*at).min(ts);
                first
            });
    let mut expected: BTreeMap<String, u64> = BTreeMap::new();
    let mut before = 0u64;
    for (cpu, ts) in raw("idle") {
        match first_freq.get(&cpu) {
            Some(first) if *first <= ts => *expected.entry(format!("cpu:{cpu}")).or_default() += 1,
            _ => before += 1,
        }
    }

    let join = &document["joins"][0];
    assert_eq!(join["rule"], "freq-idle");
    let mut credited: BTreeMap<String, u64> = BTreeMap::new();
    for attribution in join["top"].as_array().expect("attributions") {
        *credited
            .entry(attribution["key"].as_str().expect("a key").to_owned())
            .or_default() += attribution["matched"].as_u64().expect("matched");
    }
    assert_eq!(
        credited, expected,
        "each cpu is credited its own idle readings after its first frequency reading"
    );
    assert_eq!(join["unmatched"].as_u64(), Some(before), "{join}");
    assert_eq!(
        credited.len(),
        both.len(),
        "every cpu both sources read is attributed"
    );
    let states: join::StateStats =
        serde_json::from_value(join["states"].clone()).expect("the states");
    assert!(balances(&states), "{states:?}");
}

/// ROADMAP.md phase 5: evictions appear in the user-visible output.
///
/// The same boundary, with a map of two states and a window of five
/// milliseconds, over a machine with more cpus than that: each poll reads
/// every cpu in turn, so each reading evicts the oldest of the two held and
/// no state is ever replaced by its own cpu's next reading, and the two left
/// at the end of a poll outlive the window before the next one, 100 ms
/// later. So, from the kernel's count of online cpus and the rule alone:
/// nothing closed, the map full at two, both kinds of eviction counted, and
/// the states balancing. The counts are the ones `results.json` holds, and
/// the text the tool renders from that document says the same numbers; a run
/// with the text format says them too, balanced against its own opened count.
#[test]
fn a_join_that_evicts_says_how_many_in_results_json_and_in_the_text() {
    let Some(online) = online_set() else {
        eprintln!("SKIPPED: /sys/devices/system/cpu/online is unreadable here");
        return;
    };
    if online.len() < 3 {
        eprintln!(
            "SKIPPED: {} cpus online, so a map of two holds every cpu and nothing is evicted",
            online.len()
        );
        return;
    }
    let session = cpu_join_session(", capacity: 2, window: 5ms", "100ms");
    let (out, _) = match run_to_dir("evicts", &session, "json") {
        Ok(ran) => ran,
        Err(said) if machine_cannot_serve(&said) => {
            eprintln!(
                "SKIPPED: this machine serves neither power.freq nor power.cstate through /sys: \
                 {}",
                machine_s_reason(&said)
            );
            return;
        }
        Err(said) => panic!("the session did not run: {said}"),
    };
    let text = std::fs::read_to_string(out.join("results.json")).expect("results.json");
    let document: serde_json::Value = serde_json::from_str(&text).expect("results.json parses");
    let (freq_cpus, freq_events) = cpus_of(&document, "freq");
    let join = &document["joins"][0];
    let states: join::StateStats =
        serde_json::from_value(join["states"].clone()).expect("the states");
    eprintln!("{} cpus read; {states:?}", freq_cpus.len());
    assert!(freq_cpus.len() > 2, "{freq_cpus:?}");
    assert_eq!(
        states.opened, freq_events,
        "every frequency reading opens a state"
    );
    assert!(balances(&states), "{states:?}");
    assert_eq!(
        states.closed, 0,
        "no cpu is read twice while its state is held"
    );
    assert_eq!(states.peak_open, 2);
    assert!(states.overflowed > 0, "{states:?}");
    assert!(
        states.expired > 0 || freq_events <= freq_cpus.len() as u64,
        "more than one poll ran and nothing outlived a 5 ms window between polls 100 ms apart: \
         {states:?}"
    );

    // The text the tool renders from the same document.
    let output: RunOutput = serde_json::from_str(&text).expect("results.json is a run");
    let rendered = omn::encode::run_output(omn::format::OutputFormat::Text, &output)
        .expect("the run renders as text");
    let said = format!(
        "EVICTED: {} expired past the window, {} dropped because the map was full",
        states.expired, states.overflowed
    );
    assert!(rendered.contains(&said), "`{said}` is not in:\n{rendered}");

    // And a run in the text format says it, against its own opened count.
    let (out, stdout) = run_to_dir("evicts-text", &session, "text").expect("the text run");
    let written = std::fs::read_to_string(out.join("results.txt")).expect("results.txt");
    for text in [&stdout, &written] {
        let number = |line: &str, before: &str| -> u64 {
            let at = line
                .find(before)
                .unwrap_or_else(|| panic!("no `{before}` in {line}"));
            line[..at]
                .rsplit(|c: char| !c.is_ascii_digit())
                .find(|word| !word.is_empty())
                .and_then(|word| word.parse().ok())
                .unwrap_or_else(|| panic!("no count before `{before}` in {line}"))
        };
        let line = |needle: &str| {
            text.lines()
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("no `{needle}` line in:\n{text}"))
        };
        let counts = line("states:");
        let evicted = line("EVICTED:");
        let opened = number(counts, " opened");
        let closed = number(counts, " closed");
        let still = number(counts, " still open");
        let expired = number(evicted, " expired past the window");
        let overflowed = number(evicted, " dropped because the map was full");
        assert!(overflowed > 0, "{evicted}");
        assert_eq!(
            opened,
            closed + still + expired + overflowed,
            "the text's counts balance: {counts} / {evicted}"
        );
    }
}

/// What loopback has received and transmitted, in bytes, as the kernel counts
/// it in `/sys/class/net/lo/statistics`.
fn loopback_bytes() -> f64 {
    ["rx_bytes", "tx_bytes"]
        .iter()
        .map(|file| {
            std::fs::read_to_string(format!("/sys/class/net/lo/statistics/{file}"))
                .expect("every Linux machine has lo")
                .trim()
                .parse::<f64>()
                .expect("a counter")
        })
        .sum()
}

/// Two sources of one interface, joined by it: `net.iface`'s `lo/drops` and
/// `net.bytes`'s `lo` are one key, by the kernel's index, and the bytes lo
/// moved while both watched are credited -- at least what the kernel counted
/// across this test's own burst of datagrams, and at most what it counted
/// across the whole run. Before the key compared the index, the two names
/// matched nothing and every reading was uncredited.
#[test]
fn two_sources_of_one_interface_join_by_it_and_credit_what_lo_moved() {
    const DATAGRAMS: usize = 2000;
    let before_run = loopback_bytes();
    let run = Command::new(BINARY)
        .args(["-f", "examples/join-iface-loopback.yml", "--format", "json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("omn should run");

    // Well inside the run's two seconds: after the first readings of both
    // sources, and long before the last.
    thread::sleep(StdDuration::from_millis(700));
    let receiver = std::net::UdpSocket::bind("127.0.0.1:0").expect("a loopback socket");
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").expect("a loopback socket");
    let to = receiver.local_addr().expect("an address");
    receiver
        .set_read_timeout(Some(StdDuration::from_millis(50)))
        .expect("a timeout");
    let before_burst = loopback_bytes();
    let mut buffer = [0u8; 2048];
    for _ in 0..DATAGRAMS {
        sender.send_to(&[7u8; 1000], to).expect("a datagram");
        let _ = receiver.recv_from(&mut buffer);
    }
    let burst = loopback_bytes() - before_burst;

    let output = run.wait_with_output().expect("omn should finish");
    let moved = loopback_bytes() - before_run;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        assert!(
            machine_cannot_serve(&stderr),
            "the session failed for a reason that is not the machine's:\n{stderr}"
        );
        eprintln!(
            "SKIPPED: {}. It would have checked that two sources of lo join by its index",
            machine_s_reason(&stderr)
        );
        return;
    }
    let document: serde_json::Value = serde_json::from_str(&stdout).expect("a JSON document");
    let join = document["joins"]
        .as_array()
        .and_then(|joins| {
            joins
                .iter()
                .find(|join| join["rule"] == "lo-moved-while-watched")
        })
        .expect("the session's join");
    let credited = number(&join["readings"]["credited"]);
    let uncredited = number(&join["readings"]["uncredited"]);
    eprintln!(
        "lo moved {burst} bytes in the burst and {moved} over the run; credited {credited}, \
         uncredited {uncredited}"
    );
    assert_eq!(join["evidence"], "TEMPORAL");
    assert!(
        burst >= (DATAGRAMS * 2 * 1000) as f64,
        "the kernel counted {burst} bytes for {DATAGRAMS} datagrams of 1000 bytes, sent and received"
    );
    assert!(
        join["matched"].as_u64().unwrap_or(0) > 0,
        "no `net.bytes` reading of lo met a `net.iface` state of lo: the key is still \
         compared by name\n{join}"
    );
    assert!(
        credited >= burst,
        "the kernel counted {burst} bytes on lo across the burst, inside the run; the join \
         credited {credited}"
    );
    assert!(
        credited + uncredited <= moved,
        "the join read {} bytes on lo; the kernel counted {moved} across the whole run",
        credited + uncredited
    );
}

/// What the kernel says lo transmitted, in packets.
fn loopback_tx_packets() -> f64 {
    std::fs::read_to_string("/sys/class/net/lo/statistics/tx_packets")
        .expect("every Linux machine has lo")
        .trim()
        .parse::<f64>()
        .expect("a counter")
}

/// Two producers of one interface that name it two ways: `net.qdisc` carries
/// lo's index as the kernel read it off each packet, `net.iface` only the
/// name `/proc/net/dev` prints, in one series per counter kind (`lo/drops`).
/// They are one key by the index, so every packet the discipline saw on lo is
/// laid across lo's readings -- credited to the reading whose interval holds
/// its drained row, or counted uncredited, never dropped for a key that did
/// not match -- and at least half of what the kernel counted transmitted on lo
/// across this test's own burst is credited.
///
/// Loopback is `noqueue` unless someone gave it a discipline, and then no
/// packet of it passes one: the test skips with that reason, which is the
/// machine's (`tc qdisc replace dev lo root netem limit 100000`, as root,
/// makes it run; a fifo is bypassed while empty and queues nothing).
#[test]
fn a_queued_packet_on_lo_joins_lo_s_link_counters_by_its_index() {
    const DATAGRAMS: usize = 2000;
    let run = Command::new(BINARY)
        .args(["-f", "examples/join-iface-qdisc.yml", "--format", "json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("omn should run");

    // After the first two readings of lo, and well before the last.
    thread::sleep(StdDuration::from_millis(1200));
    let receiver = std::net::UdpSocket::bind("127.0.0.1:0").expect("a loopback socket");
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").expect("a loopback socket");
    let to = receiver.local_addr().expect("an address");
    receiver
        .set_read_timeout(Some(StdDuration::from_millis(50)))
        .expect("a timeout");
    let before_burst = loopback_tx_packets();
    let mut buffer = [0u8; 2048];
    for _ in 0..DATAGRAMS {
        sender.send_to(&[7u8; 100], to).expect("a datagram");
        let _ = receiver.recv_from(&mut buffer);
    }
    let burst = loopback_tx_packets() - before_burst;

    let output = run.wait_with_output().expect("omn should finish");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        assert!(
            machine_cannot_serve(&stderr),
            "the session failed for a reason that is not the machine's:\n{stderr}"
        );
        eprintln!(
            "SKIPPED: {}. It would have checked that a kernel-indexed and a named producer of \
             lo join by its index",
            machine_s_reason(&stderr)
        );
        return;
    }
    let document: serde_json::Value = serde_json::from_str(&stdout).expect("a JSON document");
    let queued: Vec<&serde_json::Value> = document["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|result| result["label"] == "queued")
        .flat_map(|result| result["outputs"].as_array().into_iter().flatten())
        .filter(|output| output["emit"] == "raw")
        .flat_map(|output| output["events"].as_array().into_iter().flatten())
        .collect();
    let packets: f64 = queued
        .iter()
        .map(|event| {
            event["attrs"]
                .as_array()
                .and_then(|attrs| attrs.iter().find(|attr| attr["name"] == "count"))
                .and_then(|attr| attr["value"].as_f64())
                .unwrap_or(0.0)
        })
        .sum();
    if packets == 0.0 {
        eprintln!(
            "SKIPPED: no packet of lo was queued on this machine: lo is `noqueue` by default, \
             and a fifo is bypassed while it is empty, so none of the {burst} packets the kernel \
             counted on lo entered a discipline and `net.qdisc` has nothing to key. \
             `tc qdisc replace dev lo root netem limit 100000` (as root; `tc qdisc del dev lo \
             root` undoes it) gives it one that queues. It would have checked that a \
             kernel-indexed and a named producer of lo join by its index"
        );
        return;
    }
    let join = document["joins"]
        .as_array()
        .and_then(|joins| {
            joins
                .iter()
                .find(|join| join["rule"] == "queued-per-link-reading")
        })
        .expect("the session's join");
    let credited = number(&join["readings"]["credited"]);
    let uncredited = number(&join["readings"]["uncredited"]);
    eprintln!(
        "lo transmitted {burst} packets in the burst; net.qdisc saw {packets} on lo; credited \
         {credited}, uncredited {uncredited}"
    );
    assert_eq!(join["evidence"], "TEMPORAL");
    assert!(
        burst >= DATAGRAMS as f64,
        "the kernel counted {burst} packets transmitted on lo for {DATAGRAMS} datagrams"
    );
    assert_eq!(
        join["keyless"].as_u64().unwrap_or(0),
        0,
        "every row of lo carries lo: {join}"
    );
    assert!(
        (credited + uncredited - packets).abs() < 0.5,
        "net.qdisc saw {packets} packets on lo and the join laid {credited} across lo's readings \
         and left {uncredited}: a row whose key matched nothing would be in neither"
    );
    assert!(
        packets >= burst,
        "the kernel transmitted {burst} packets on lo across the burst, and its discipline saw \
         {packets}"
    );
    assert!(
        credited >= burst / 2.0,
        "the kernel counted {burst} packets on lo across the burst, inside the run; the join \
         credited {credited} to lo's readings ({uncredited} uncredited): the two producers' \
         keys of lo did not meet\n{join}"
    );
    for row in join["top"].as_array().into_iter().flatten() {
        assert!(
            row["key"] == "iface:lo",
            "a row of lo's readings is lo's: {row}"
        );
    }
}

/// The script a sender runs inside `unshare -rn`: a discipline that queues on
/// its own lo, the namespace's inode on the first line, then 2000 datagrams
/// over that lo once the session has taken its first readings.
const OTHER_NAMESPACE_SENDER: &str = r#"
ip link set lo up || exit 9
tc qdisc replace dev lo root netem limit 100000 || exit 9
readlink /proc/self/ns/net
sleep 1
python3 -c 'import socket
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); r=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); r.bind(("127.0.0.1",0))
for _ in range(2000): s.sendto(b"x"*64, r.getsockname())' || exit 9
tc -s qdisc show dev lo
"#;

/// Runs `session` while a sender in another network namespace queues 2000
/// packets on that namespace's lo. `None` when the machine cannot make such a
/// namespace, with the reason printed; otherwise the session's JSON and the
/// inode of the sender's namespace.
fn run_beside_another_namespaces_lo(session: &str) -> Option<(serde_json::Value, u64)> {
    let run = Command::new(BINARY)
        .args(["-f", session, "--format", "json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("omn should run");
    let sender = Command::new("unshare")
        .args(["-rn", "sh", "-c", OTHER_NAMESPACE_SENDER])
        .output();
    let output = run.wait_with_output().expect("omn should finish");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let sender = match sender {
        Ok(sender) if sender.status.success() => sender,
        Ok(sender) => {
            eprintln!(
                "SKIPPED: this machine cannot queue packets on lo in an unprivileged network \
                 namespace (`unshare -rn` with `tc qdisc ... netem`; the user namespace may be \
                 restricted, or `sch_netem` not loaded): {}. It would have checked that \
                 another namespace's lo is not this one's",
                String::from_utf8_lossy(&sender.stderr).trim()
            );
            return None;
        }
        Err(error) => {
            eprintln!(
                "SKIPPED: `unshare` could not run: {error}. It would have checked that another \
                 namespace's lo is not this one's"
            );
            return None;
        }
    };
    if !output.status.success() {
        assert!(
            machine_cannot_serve(&stderr),
            "the session failed for a reason that is not the machine's:\n{stderr}"
        );
        eprintln!(
            "SKIPPED: {}. It would have checked that another namespace's lo is not this one's",
            machine_s_reason(&stderr)
        );
        return None;
    }
    let said = String::from_utf8_lossy(&sender.stdout);
    let inode: u64 = said
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("net:["))
        .and_then(|rest| rest.strip_suffix(']'))
        .and_then(|inode| inode.parse().ok())
        .unwrap_or_else(|| panic!("the sender names its namespace first: {said}"));
    assert!(
        said.contains("Sent ") && said.contains(" 2000 pkt"),
        "the discipline in the sender's namespace counted its 2000 packets: {said}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    Some((
        serde_json::from_str(&stdout).expect("a JSON document"),
        inode,
    ))
}

/// An interface index is per network namespace, and every namespace has a lo
/// of index 1. A sender in an unprivileged namespace (`unshare -rn`) queues
/// 2000 packets on its own lo while this one's lo is read.
///
/// With `net.qdisc` on the whole machine, the discipline sees the 2000 packets
/// and keys them by the device's own namespace: none is credited to this
/// namespace's lo, all 2000 are counted uncredited, and every series of them
/// says whose lo it is (`iface:lo@netns:<inode>`). Before the namespace was in
/// the key, all 2000 were credited here. And the session as shipped, which
/// targets lo, has the kernel drop them by their namespace: no row at all.
#[test]
fn another_namespaces_lo_is_not_credited_to_this_ones() {
    let shipped = std::fs::read_to_string("examples/join-iface-qdisc.yml").expect("the session");
    let narrowed = "    source: net.qdisc\n    target: {iface: lo}\n";
    assert!(
        shipped.contains(narrowed),
        "the session targets lo for net.qdisc"
    );
    let whole = shipped.replace(narrowed, "    source: net.qdisc\n    target: system\n");
    let session =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("join-iface-qdisc-system.yml");
    std::fs::write(&session, whole).expect("a session file");

    let Some((document, inode)) = run_beside_another_namespaces_lo(session.to_str().unwrap())
    else {
        return;
    };
    let join = document["joins"]
        .as_array()
        .and_then(|joins| {
            joins
                .iter()
                .find(|join| join["rule"] == "queued-per-link-reading")
        })
        .expect("the session's join");
    let credited = number(&join["readings"]["credited"]);
    let uncredited = number(&join["readings"]["uncredited"]);
    let theirs = format!("iface:lo@netns:{inode}");
    let queued: Vec<String> = document["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|result| result["label"] == "queued")
        .flat_map(|result| result["outputs"].as_array().into_iter().flatten())
        .filter(|output| output["emit"] == "raw")
        .flat_map(|output| output["events"].as_array().into_iter().flatten())
        .filter_map(|event| {
            let attrs = event["attrs"].as_array()?;
            let netns = attrs.iter().find(|attr| attr["name"] == "netns")?;
            Some(netns["value"].to_string())
        })
        .collect();
    eprintln!(
        "their namespace {inode}: credited {credited}, uncredited {uncredited}, {} rows of theirs",
        queued
            .iter()
            .filter(|netns| **netns == inode.to_string())
            .count()
    );
    assert_eq!(
        credited, 0.0,
        "a packet on another namespace's lo was credited to this one's: {join}"
    );
    assert!(
        uncredited >= 2000.0,
        "the 2000 packets their discipline counted are counted uncredited here: {join}"
    );
    assert!(
        join["top"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|row| row["key"] != "iface:lo"),
        "no row of their lo is labelled as this one's: {join}"
    );
    assert!(
        join.to_string().contains(&theirs)
            || queued.iter().any(|netns| *netns == inode.to_string()),
        "their rows carry their namespace, {theirs}: {join}"
    );

    // As shipped, the target is this namespace's lo, and the kernel compares
    // the namespace: none of theirs is a row.
    let Some((document, _)) = run_beside_another_namespaces_lo("examples/join-iface-qdisc.yml")
    else {
        return;
    };
    let join = document["joins"]
        .as_array()
        .and_then(|joins| {
            joins
                .iter()
                .find(|join| join["rule"] == "queued-per-link-reading")
        })
        .expect("the session's join");
    assert_eq!(
        number(&join["readings"]["credited"]) + number(&join["readings"]["uncredited"]),
        0.0,
        "a target of this namespace's lo kept a packet of another namespace's: {join}"
    );
}
