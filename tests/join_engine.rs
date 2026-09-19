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
        via: KeyKind::Task,
        match_kind: MatchKind::Eq,
        opens: None,
        closes: None,
        waker: None,
        depth: None,
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
        via: KeyKind::Task,
        match_kind: MatchKind::Wakeup,
        opens: None,
        closes: None,
        waker: Some("waker_tid".to_owned()),
        depth: None,
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
    assert_eq!(
        correlation.matched + correlation.unmatched,
        switches.len() as u64,
        "every switch is either attributed or counted as unattributed"
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
