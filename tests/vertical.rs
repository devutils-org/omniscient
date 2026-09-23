//! The vertical case: one request's latency, decomposed boundary by boundary.
//!
//! `ROADMAP.md` phase 4, checked the way its exit criterion asks: a real server
//! whose delays the test injected, decomposed by the tool, and every row held
//! to what the server and its client measured on their own clocks. The tool
//! reads none of that; the server prints it on stderr, prefixed `vertical:`,
//! and this file is the only reader.
//!
//! The session is `examples/vertical-one-request.yml`, run once and shared by
//! every test below. Three requests:
//!
//! | id     | injected                                     | must land in                  |
//! |--------|----------------------------------------------|-------------------------------|
//! | `a001` | 40 ms waiting for a lock held under a sleep  | `lock_wait`, holder named     |
//! | `a002` | 64 pages read with `O_DIRECT`, cache dropped | `block_io`, 64 requests       |
//! | `a003` | 20 ms on the lock, then 32 pages             | both, and nowhere else        |
//!
//! What is independent of the tool, and what each check leans on:
//!
//! - **The holder's own clock** around its `nanosleep` under the mutex: the
//!   worker cannot have waited less than that for the lock, and cannot have
//!   waited more than its own clock around the handshake and the lock.
//! - **The worker's own clock** around its reads: the device cannot have spent
//!   longer on the request's block requests than the reads took.
//! - **The number of reads**, chosen: one page with `O_DIRECT`, from a file
//!   `fsync`ed and dropped from the cache, is one block request -- the I/O
//!   crossing's lesson, `docs/scenarios/cross-io.md`.
//! - **The client's clock** from before its send to the last byte of the
//!   response, which is the latency the roadmap's 10 % is measured against.
//! - **The thread ids** each role prints, which the holder the tool names must
//!   be, and which no request's thread may be but the worker's.
//!
//! The four states of `net.request` add up to its total by construction, and
//! nothing here treats that as evidence. The evidence is which row a delay
//! lands in, the block requests that carry the id, and the client's clock.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::OnceLock;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn omn_at(binary: &str, args: &[&str]) -> Output {
    // A binary copied a moment ago can be busy: another test thread that forked
    // while the copy was open holds its descriptor until that child execs.
    // `ETXTBSY` then says so, and the answer is to try again, not to fail.
    let mut attempts = 0;
    loop {
        match Command::new(binary).current_dir(root()).args(args).output() {
            Ok(output) => return output,
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && attempts < 50 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => panic!("omn should run: {error}"),
        }
    }
}

fn omn(args: &[&str]) -> Output {
    omn_at(BINARY, args)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

// ---------------------------------------------------------------- the server

/// The server's C source, between the two marker lines so that the example
/// can say how to build it by hand with `sed`.
const SERVER_SOURCE: &str = include_str!("../examples/vertical/server.c");

/// Where the server is built. The example launches it by this path, relative
/// to the repository root.
fn server_path() -> PathBuf {
    root().join("target/omn-vertical/server")
}

/// Builds the server once per test process, or says why it cannot.
fn server() -> Result<PathBuf, String> {
    static BUILT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let path = server_path();
            let dir = path.parent().expect("a directory");
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
            let source = dir.join("server.c");
            std::fs::write(&source, SERVER_SOURCE).map_err(|error| error.to_string())?;
            // Built beside and renamed, so a half-written binary is never at
            // the path the example launches.
            let partial = dir.join(format!("server.{}", std::process::id()));
            let built = Command::new("cc")
                .args(["-O0", "-pthread", "-o"])
                .arg(&partial)
                .arg(&source)
                .output()
                .map_err(|error| format!("no C compiler (`cc`) to build the server: {error}"))?;
            if !built.status.success() {
                return Err(format!(
                    "the server did not build -- most likely <sys/sdt.h> is missing, which \
                     systemtap's development headers provide:\n{}",
                    text(&built.stderr)
                ));
            }
            std::fs::rename(&partial, &path).map_err(|error| error.to_string())?;
            Ok(path)
        })
        .clone()
}

// ---------------------------------------------------------------- the run

/// A `vertical: key=value ...` line.
type Said = BTreeMap<String, String>;

/// The session, run once, as the tool's document and what the programs said.
struct Run {
    document: serde_json::Value,
    said: Vec<Said>,
}

/// Whether a failure is this machine declining, rather than the tool failing.
fn machine_cannot_serve(message: &str) -> bool {
    message.contains("CAP_BPF")
        || message.contains("no viable backend")
        || message.contains("O_DIRECT")
}

fn run() -> Option<&'static Run> {
    static RUN: OnceLock<Result<Run, String>> = OnceLock::new();
    let outcome = RUN.get_or_init(|| {
        server()?;
        let output = omn(&[
            "-f",
            "examples/vertical-one-request.yml",
            "--format",
            "json",
        ]);
        let (stdout, stderr) = (text(&output.stdout), text(&output.stderr));
        if !output.status.success() {
            if machine_cannot_serve(&stderr) {
                return Err(format!(
                    "this machine cannot serve the session: {}",
                    stderr.lines().next().unwrap_or_default()
                ));
            }
            panic!("the vertical session failed:\n{stdout}\n{stderr}");
        }
        let document = serde_json::from_str(&stdout).unwrap_or_else(|error| {
            panic!("the session is not one json document ({error}):\n{stdout}")
        });
        let said = stderr
            .lines()
            .filter_map(|line| line.strip_prefix("vertical: "))
            .map(|line| {
                line.split_whitespace()
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .collect()
            })
            .collect();
        Ok(Run { document, said })
    });
    match outcome {
        Ok(run) => Some(run),
        Err(reason) => {
            eprintln!("SKIPPED: {reason}");
            None
        }
    }
}

macro_rules! needs_run {
    () => {
        match run() {
            Some(run) => run,
            None => return,
        }
    };
}

fn field(said: &Said, name: &str) -> f64 {
    said.get(name)
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("the server said no `{name}`: {said:?}"))
}

/// The attributes of an event, by name.
fn attrs(event: &serde_json::Value) -> BTreeMap<&str, &serde_json::Value> {
    event["attrs"]
        .as_array()
        .expect("attrs")
        .iter()
        .map(|attr| (attr["name"].as_str().unwrap_or_default(), &attr["value"]))
        .collect()
}

fn number(event: &serde_json::Value, name: &str) -> f64 {
    attrs(event)
        .get(name)
        .and_then(|value| value.as_f64())
        .unwrap_or_else(|| panic!("no numeric `{name}` on {event}"))
}

fn key<'a>(event: &'a serde_json::Value, kind: &str) -> Option<&'a serde_json::Value> {
    event["keys"]
        .as_array()?
        .iter()
        .find_map(|key| key.get(kind))
}

fn request_of(event: &serde_json::Value) -> Option<String> {
    key(event, "req").and_then(|req| req["id"].as_str().map(str::to_owned))
}

/// One request, as everything that measured it saw it.
#[derive(Debug)]
struct Request<'a> {
    id: String,
    /// What the worker thread said about it.
    worker: &'a Said,
    /// What the client said about it.
    client: &'a Said,
    /// The `net.request` event.
    event: &'a serde_json::Value,
    /// The `io.latency` events that carry its id.
    blocks: Vec<&'a serde_json::Value>,
    /// Its table in the tool's decomposition of its life.
    table: &'a serde_json::Value,
    /// Its table in the decomposition of its block I/O, when it has one: the
    /// engine makes a table for each key its *parts* carry, so a request with
    /// no block request carrying its id has none.
    block_table: Option<&'a serde_json::Value>,
}

impl Run {
    fn raw(&self, label: &str) -> Vec<&serde_json::Value> {
        self.document["results"]
            .as_array()
            .and_then(|all| all.iter().find(|one| one["label"] == label))
            .unwrap_or_else(|| panic!("no request labelled `{label}`"))["outputs"]
            .as_array()
            .expect("outputs")
            .iter()
            .filter(|output| output["emit"] == "raw")
            .flat_map(|output| output["events"].as_array().expect("events"))
            .collect()
    }

    fn role(&self, role: &str) -> &Said {
        self.said
            .iter()
            .find(|said| {
                said.get("role").map(String::as_str) == Some(role) && !said.contains_key("req")
            })
            .unwrap_or_else(|| panic!("the server never introduced its `{role}`"))
    }

    fn decomposition(&self, id: &str) -> &serde_json::Value {
        self.document["decompositions"]
            .as_array()
            .and_then(|all| all.iter().find(|one| one["id"] == id))
            .unwrap_or_else(|| panic!("no decomposition `{id}`"))
    }

    fn table(&self, decomposition: &str, id: &str) -> Option<&serde_json::Value> {
        self.decomposition(decomposition)["tables"]
            .as_array()
            .expect("tables")
            .iter()
            .find(|table| table["key"] == format!("req:{id}"))
    }

    fn request(&self, id: &str) -> Request<'_> {
        let said = |role: &str| {
            self.said
                .iter()
                .find(|said| {
                    said.get("role").map(String::as_str) == Some(role)
                        && said.get("req").map(String::as_str) == Some(id)
                })
                .unwrap_or_else(|| panic!("the {role} said nothing about req {id}"))
        };
        let event = self
            .raw("requests")
            .into_iter()
            .find(|event| request_of(event).as_deref() == Some(id))
            .unwrap_or_else(|| panic!("no `net.request` event for req:{id}"));
        let blocks = self
            .raw("block")
            .into_iter()
            .filter(|event| request_of(event).as_deref() == Some(id))
            .collect();
        let table = self
            .table("one-request", id)
            .unwrap_or_else(|| panic!("no table for req:{id}"));
        let block_table = self.table("block-io", id);
        Request {
            id: id.to_owned(),
            worker: said("worker"),
            client: said("client"),
            event,
            blocks,
            table,
            block_table,
        }
    }
}

fn amount(table: &serde_json::Value, state: &str) -> f64 {
    table["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["state"] == state)
        .and_then(|row| row["amount"].as_f64())
        .unwrap_or_else(|| panic!("no `{state}` amount in {table}"))
}

impl Request<'_> {
    /// A row of the tool's table of the request's life, by state.
    fn row(&self, state: &str) -> f64 {
        amount(self.table, state)
    }

    fn parts(&self) -> f64 {
        self.table["parts"].as_f64().expect("the parts add up")
    }

    fn device_ns(&self) -> f64 {
        self.blocks
            .iter()
            .map(|event| number(event, "latency_ns"))
            .sum()
    }
}

const MS: f64 = 1e6;

// ---------------------------------------------------------------- scenarios

/// Each delay lands in the row it was injected into, and nowhere else.
///
/// The lock: the worker asks the holder to take the mutex, waits to hear that
/// it has, and then blocks on the mutex until the holder's `nanosleep` ends and
/// it unlocks. So the worker's interruptible sleep covers the holder's whole
/// sleep, less the few microseconds between the holder's signal and its sleep
/// starting, and cannot exceed the worker's own clock around the handshake and
/// the lock. One millisecond of slack on the low side, none on the high.
///
/// The reads: every block request the reads submitted is inside the worker's
/// own clock around them, so their device time cannot exceed it. And the
/// device's time is inside the request's wait for it, except for the moment
/// between each issue and the thread leaving the cpu -- a request is issued
/// on the cpu, so the device's time can overlap on-cpu time and nothing else.
///
/// And the absences, which are the half a plausible-looking tool gets wrong: a
/// request that took no lock has no lock wait, and one that read nothing has
/// no block request carrying its id.
#[test]
fn each_injected_delay_lands_in_the_row_it_was_injected_into() {
    let run = needs_run!();
    for id in ["a001", "a002", "a003"] {
        let request = run.request(id);
        let held: Option<f64> = (field(request.worker, "lock_ms") > 0.0).then(|| {
            // The holder reports each hold in order, one per locking request.
            let holds: Vec<f64> = run
                .said
                .iter()
                .filter(|said| said.get("role").map(String::as_str) == Some("holder"))
                .filter_map(|said| said.get("held_ns")?.parse().ok())
                .collect();
            let index = ["a001", "a003"].iter().position(|one| *one == id).unwrap();
            holds[index]
        });
        let lock_wait = request.row("lock_wait");
        let block_io = request.row("block_io");
        let device = request.device_ns();
        eprintln!(
            "req:{id}  lock_wait {:.3} ms (held {:?} ms, worker's own {:.3} ms)  block_io {:.3} \
             ms, the device {:.3} ms of it over {} requests (worker's reads {:.3} ms)",
            lock_wait / MS,
            held.map(|held| held / MS),
            field(request.worker, "lock_ns") / MS,
            block_io / MS,
            device / MS,
            request.blocks.len(),
            field(request.worker, "read_ns") / MS,
        );
        match held {
            Some(held) => {
                assert!(
                    lock_wait >= held - MS,
                    "req:{id}: the lock was held for {:.3} ms and the tool saw {:.3} ms of wait",
                    held / MS,
                    lock_wait / MS
                );
                assert!(
                    lock_wait <= field(request.worker, "lock_ns"),
                    "req:{id}: more lock wait than the worker's own clock around the lock"
                );
            }
            None => assert!(
                lock_wait < MS,
                "req:{id} took no lock and the tool put {:.3} ms in `lock_wait`",
                lock_wait / MS
            ),
        }
        if field(request.worker, "reads") > 0.0 {
            assert!(
                device > 0.0,
                "req:{id} read and no block request carries its id"
            );
            assert!(
                device <= field(request.worker, "read_ns"),
                "req:{id}: the device spent {:.3} ms on requests submitted inside reads the \
                 worker timed at {:.3} ms",
                device / MS,
                field(request.worker, "read_ns") / MS
            );
            assert!(
                device <= block_io + request.row("cpu_on"),
                "req:{id}: the device's {:.3} ms do not fit in the {:.3} ms the request waited \
                 for it and the {:.3} ms it ran",
                device / MS,
                block_io / MS,
                request.row("cpu_on") / MS
            );
        } else {
            assert!(
                request.blocks.is_empty(),
                "req:{id} read nothing and {} block requests carry its id",
                request.blocks.len()
            );
            assert!(
                block_io < MS,
                "req:{id} read nothing and waited {:.3} ms uninterruptibly",
                block_io / MS
            );
        }
    }
}

/// Every block request a request submitted carries its id, and no other does.
///
/// One page with `O_DIRECT` is one block request, so the count is exact: a
/// request that read 64 pages carries 64 block requests, at 64 different
/// sectors. And every one of them was created inside the request's own life --
/// `open_ts` is the kernel's own stamp on the block request, on the same clock
/// as the birth and the end -- with no slack, because the id is read from the
/// task at the issue and leaves the task at the end.
///
/// The worker also wrote and `fsync`ed the file before any request began, in
/// the same thread: those block requests were submitted by the same task and
/// must carry no id, which is the check that the identity is the request's
/// rather than the thread's.
#[test]
fn every_block_request_a_request_submitted_carries_its_id_and_no_other_does() {
    let run = needs_run!();
    let worker: f64 = field(run.role("worker"), "tid");
    for id in ["a001", "a002", "a003"] {
        let request = run.request(id);
        let reads = field(request.worker, "done_reads") as usize;
        let sectors: std::collections::BTreeSet<u64> = request
            .blocks
            .iter()
            .map(|event| number(event, "sector") as u64)
            .collect();
        assert_eq!(
            (request.blocks.len(), sectors.len()),
            (reads, reads),
            "req:{id} read {reads} pages with O_DIRECT, one block request each"
        );
        let born = number(request.event, "born_ns");
        let ended = request.event["ts_ns"].as_f64().expect("a timestamp");
        for block in &request.blocks {
            let created = number(block, "open_ts");
            assert!(
                born <= created && created <= ended,
                "req:{id} lived {born}..{ended} and a block request carrying its id was created \
                 at {created}"
            );
            assert_eq!(number(block, "tid"), worker, "submitted by the worker");
            assert!(
                attrs(block)["rwbs"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with('R'),
                "a read"
            );
        }
    }
    let unclaimed: Vec<&serde_json::Value> = run
        .raw("block")
        .into_iter()
        .filter(|event| request_of(event).is_none())
        .collect();
    assert!(
        !unclaimed.is_empty(),
        "the file's creation is the negative control: the worker wrote and synced it before any \
         request, and nothing reached the device"
    );
    let ids = ["a001", "a002", "a003"].map(|id| run.request(id));
    for event in &unclaimed {
        let created = number(event, "open_ts");
        for request in &ids {
            let born = number(request.event, "born_ns");
            let ended = request.event["ts_ns"].as_f64().unwrap();
            assert!(
                !(born <= created && created <= ended),
                "a block request created inside req:{}'s life carries no id: {event}",
                request.id
            );
        }
    }
}

/// The roadmap's exit criterion: the segments add up to the latency the client
/// measured, within 10 %.
///
/// The segments are the tool's own rows, and the client's clock is the only
/// thing here nobody in the server could have fudged: it runs from before the
/// request is sent to the last byte of the response, and includes everything
/// the server's rows cannot -- the loopback in both directions, the accept, the
/// parse before the birth, the client's own wakeups. What is left between the
/// two is printed, so a gap that grows is visible before it fails.
#[test]
fn the_segments_sum_to_the_latency_the_client_measured() {
    let run = needs_run!();
    for id in ["a001", "a002", "a003"] {
        let request = run.request(id);
        let client = field(request.client, "client_ns");
        let parts = request.parts();
        let gap = (client - parts) / client;
        eprintln!(
            "req:{id}  client {:.3} ms  parts {:.3} ms  whole {:.3} ms  gap {:+.2} %",
            client / MS,
            parts / MS,
            request.table["whole"]["amount"].as_f64().unwrap_or(0.0) / MS,
            gap * 100.0
        );
        assert!(
            gap.abs() < 0.10,
            "req:{id}: the rows add up to {:.3} ms and the client measured {:.3} ms",
            parts / MS,
            client / MS
        );
    }
}

/// The lock wait names the thread that held the lock.
///
/// The holder is the thread whose unlock woke the worker, read at
/// `sched_waking` in the waker's own context. The first version read it at
/// `sched_wakeup`, which runs where the wakee is queued, and named the idle
/// task on every request; this is the check that caught it.
#[test]
fn the_lock_wait_names_the_thread_that_held_the_lock() {
    let run = needs_run!();
    let holder = field(run.role("holder"), "tid");
    for id in ["a001", "a003"] {
        let request = run.request(id);
        assert_eq!(
            number(request.event, "holder_tid"),
            holder,
            "req:{id}'s longest interruptible sleep was on the lock"
        );
    }
}

/// No request is credited to a thread that was not serving one.
///
/// The client and the holder both sleep, block and run beside the worker, in
/// the same process, and neither declares a request: an identity that leaked
/// to either through a shared map, rather than living in the task, would show
/// up here as a request on the wrong thread.
#[test]
fn only_the_thread_that_declared_a_request_is_followed() {
    let run = needs_run!();
    let worker = field(run.role("worker"), "tid");
    let events = run.raw("requests");
    assert_eq!(events.len(), 3, "three requests declared, three ended");
    for event in events {
        assert_eq!(number(event, "tid"), worker, "{event}");
    }
}

/// The tables the tool printed are the decomposition of what it measured: one
/// per request, the first against the request's own life, the second against
/// its wait for its block I/O, whose one row is the device's time for exactly
/// the block requests carrying the id.
#[test]
fn the_tool_decomposes_each_request_against_its_own_life() {
    let run = needs_run!();
    for name in ["one-request", "block-io"] {
        let decomposition = run.decomposition(name);
        assert_eq!(decomposition["by"], "req", "{name}");
    }
    assert_eq!(run.decomposition("one-request")["keys_seen"], 3);
    for id in ["a001", "a002", "a003"] {
        let request = run.request(id);
        assert_eq!(
            request.table["whole"]["amount"].as_f64(),
            Some(number(request.event, "total_ns"))
        );
        match request.block_table {
            Some(table) => {
                assert_eq!(
                    table["whole"]["amount"].as_f64(),
                    Some(number(request.event, "disk_ns"))
                );
                assert_eq!(amount(table, "device"), request.device_ns());
            }
            // Filed against the engine in `docs/open-findings.md`: a key its
            // whole carries and no part does is not tabled, so a request that
            // waited on I/O no block request of its own explains would be
            // absent rather than shown with all of it in the residue. Here the
            // request read nothing, and the absence is asserted as today's
            // behaviour so that the day it changes is seen.
            None => assert!(
                request.blocks.is_empty(),
                "req:{id} has block requests and no `block-io` table"
            ),
        }
        // Against the server's clock around the probes, which starts a little
        // before the birth and ends a little after the end.
        let span = field(request.worker, "span_ns");
        let total = number(request.event, "total_ns");
        assert!(
            total <= span,
            "req:{id} lived {total} ns by the probes and {span} ns by the server's clock around \
             them"
        );
    }
}

/// A program that declares no request is refused with what it does carry.
#[test]
fn a_program_that_declares_no_request_is_refused_by_name() {
    let output = omn(&["net.request", "--emit", "raw", "--", "/bin/true"]);
    let stderr = text(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("request__start") && stderr.contains("request__done"),
        "the refusal names the two probes a program must declare: {stderr}"
    );
}

/// Asked to count, the source refuses by name rather than aggregating what it
/// cannot: a request's states are only known at its end.
#[test]
fn a_mode_that_cannot_follow_a_request_is_refused_by_name() {
    let Ok(server) = server() else {
        eprintln!("SKIPPED: no server to point the refusal at");
        return;
    };
    let output = omn(&[
        "net.request",
        "--mode",
        "sample",
        "--emit",
        "raw",
        "--",
        server.to_str().unwrap(),
        "target/omn-vertical/refused",
        "1:0",
    ]);
    assert!(!output.status.success());
    let stderr = text(&output.stderr);
    assert!(stderr.contains("sample"), "{stderr}");
}

/// Without the capabilities the session is refused with the advice, not run
/// into an empty table. A copy of the binary carries none: file capabilities do
/// not survive a copy, which is the same reason they do not survive a relink.
#[test]
fn without_the_capabilities_the_session_says_which_it_needs() {
    if server().is_err() {
        eprintln!("SKIPPED: no server for the session");
        return;
    }
    let copy = root().join("target/omn-vertical/omn-uncapped");
    std::fs::copy(BINARY, &copy).expect("a copy of the binary");
    let output = omn_at(
        copy.to_str().unwrap(),
        &[
            "-f",
            "examples/vertical-one-request.yml",
            "--format",
            "json",
        ],
    );
    let stderr = text(&output.stderr);
    assert!(
        !output.status.success(),
        "an uncapped run cannot follow a request: {stderr}"
    );
    assert!(
        stderr.contains("CAP_BPF"),
        "the refusal names the capability: {stderr}"
    );
}

/// A request that never ends is counted and named, and does not leak.
///
/// `examples/vertical-abandoned.yml`: the server declares a request and never
/// ends it, then serves an ordinary one on the same thread. The first has no
/// total and can be in no event; what it must be in is the run's account of
/// itself -- born two, ended one, which one did not end and in which thread,
/// and that the second birth replaced a live request. And the second request
/// must be whole: its lock wait is the holder's sleep, not the abandoned
/// request's time added to it.
#[test]
fn a_request_that_never_ends_is_counted_and_named_and_does_not_leak() {
    if let Err(reason) = server() {
        eprintln!("SKIPPED: {reason}");
        return;
    }
    let output = omn(&["-f", "examples/vertical-abandoned.yml", "--format", "json"]);
    let (stdout, stderr) = (text(&output.stdout), text(&output.stderr));
    if !output.status.success() && machine_cannot_serve(&stderr) {
        eprintln!("SKIPPED: {}", stderr.lines().next().unwrap_or_default());
        return;
    }
    assert!(output.status.success(), "{stderr}");
    let document: serde_json::Value = serde_json::from_str(&stdout).expect("one json document");
    let result = &document["results"][0];
    let said: Vec<String> = result["degradations"]
        .as_array()
        .expect("the run's account of itself")
        .iter()
        .filter_map(|note| note.as_str().map(str::to_owned))
        .collect();
    let worker = stderr
        .lines()
        .find_map(|line| line.strip_prefix("vertical: role=worker tid="))
        .and_then(|rest| rest.split_whitespace().next())
        .expect("the worker introduced itself")
        .to_owned();
    let holder_held: f64 = stderr
        .lines()
        .find_map(|line| line.strip_prefix("vertical: role=holder held_ns="))
        .and_then(|rest| rest.trim().parse().ok())
        .expect("the holder reported its one hold");

    assert!(
        said.iter()
            .any(|note| note.starts_with("2 requests were born and 1 ended")),
        "{said:#?}"
    );
    assert!(
        said.iter()
            .any(|note| note.contains("1 request never ended")
                && note.contains(&format!("req:a001 in task {worker}"))),
        "the request that never ended is named, with its thread: {said:#?}"
    );
    assert!(
        said.iter()
            .any(|note| note.contains("hit a task still carrying a request")),
        "the second birth replaced a live request, and says so: {said:#?}"
    );

    let events: Vec<&serde_json::Value> = result["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .filter(|output| output["emit"] == "raw")
        .flat_map(|output| output["events"].as_array().expect("events"))
        .collect();
    assert_eq!(events.len(), 1, "one request ended");
    let event = events[0];
    assert_eq!(request_of(event).as_deref(), Some("a002"));
    let sleep = number(event, "sleep_ns");
    let total = number(event, "total_ns");
    assert!(
        sleep >= holder_held - MS && total < holder_held + 5.0 * MS,
        "the second request waited {:.3} ms of a {:.3} ms life on a {:.3} ms hold: the \
         abandoned request's time is not in it",
        sleep / MS,
        total / MS,
        holder_held / MS
    );
}
