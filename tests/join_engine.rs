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
                     no real two-task stream to attribute: {message}"
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
                 unattributed, and that no chain names a task nothing woke: {errors}"
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
                stderr.lines().next().unwrap_or_default()
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
    let result = run.result("retained");
    let dropped = result["losses"]["dropped"].as_u64().unwrap_or(0);
    let last_reading = result["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .filter(|output| output["emit"] == "raw")
        .flat_map(|output| output["events"].as_array().into_iter().flatten())
        .filter_map(|event| event["ts_ns"].as_u64())
        .max()
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
    assert!(
        (live - held).abs() <= INTERPRETER,
        "the program ends holding {held} bytes it asked for; the live row says {live}"
    );
    assert!(
        (retained - whole_pages).abs() <= INTERPRETER,
        "the freed blocks cover {whole_pages} bytes of whole pages; the retained row says \
         {retained}"
    );
    let predicted = freed - whole_pages;
    assert!(
        residue >= -INTERPRETER,
        "a residue of {residue} means more was accounted for than is resident"
    );
    assert!(
        residue <= predicted + HUGE_PAGE,
        "a residue of {residue} is more than the {predicted} the program predicts plus a huge \
         page: something resident is in neither row"
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
    if machine_cannot_serve(&said) || said.contains("CAP_BPF") {
        eprintln!("skipped: this machine cannot serve the three sources: {said}");
        return;
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
             {said}"
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
             only the `eq` refusal was checked. The machine's reason:\n{said}"
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
             {cpus} series was not checked. The machine's reason:\n{said}"
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
