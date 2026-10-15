//! The vertical case: one request's latency, decomposed boundary by boundary.
//!
//! `ROADMAP.md` phase 4, checked the way its exit criterion asks: a real server
//! whose delays the test injected, decomposed by the tool, and every row held
//! to what the server and its client measured on their own clocks. The tool
//! reads none of that; the server prints it on stderr, prefixed `vertical:`,
//! and this file is the only reader.
//!
//! The session is `examples/vertical-one-request.yml`, run once and shared by
//! every test below. Five requests:
//!
//! | id     | injected                                     | must land in                  |
//! |--------|----------------------------------------------|-------------------------------|
//! | `a001` | 40 ms waiting for a lock held under a sleep  | `lock_wait`, holder named     |
//! | `a002` | 64 pages read with `O_DIRECT`, cache dropped | `block_io`, 64 requests       |
//! | `a003` | 20 ms on the lock, then 32 pages             | both, and nowhere else        |
//! | `a004` | the client pauses 30 ms mid-request          | `net_rx`                      |
//! | `a005` | a client that reads 128 KiB slowly           | `net_tx`, not `lock_wait`     |
//!
//! The last two are injected by the client, which is why they need no root:
//! `tc netem` would put a delay where the tool never sees it happen, and a
//! client that pauses, or reads slowly through a small receive buffer, puts one
//! exactly where the tool does.
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
//! - **The client's own stamps**, on `CLOCK_MONOTONIC` like the kernel's: its
//!   two sends either side of the pause, and for the slow reader the start of
//!   the read after which less than a receive buffer was left (the earliest the
//!   response's last byte can have left the server) and the end of its last
//!   read (the latest). The tool's first-byte and last-byte stamps are held
//!   between them in absolute time, not as durations.
//! - **The socket's cookie**, which the server reads with `SO_COOKIE` on the
//!   connection it accepted, and which the tool's `sock` key must be.
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
    BUILT.get_or_init(|| build(server_path(), &[])).clone()
}

/// The same server with both probes behind a semaphore.
fn guarded_server() -> Result<PathBuf, String> {
    static BUILT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            build(
                root().join("target/omn-vertical/server-guarded"),
                &["-DGUARDED"],
            )
        })
        .clone()
}

fn build(path: PathBuf, flags: &[&str]) -> Result<PathBuf, String> {
    let dir = path.parent().expect("a directory");
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let source = dir.join(format!("{name}.c"));
    std::fs::write(&source, SERVER_SOURCE).map_err(|error| error.to_string())?;
    // Built beside and renamed, so a half-written binary is never at
    // the path the example launches.
    let partial = dir.join(format!("{name}.{}", std::process::id()));
    let built = Command::new("cc")
        .args(["-O0", "-pthread"])
        .args(flags)
        .arg("-o")
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

/// Why the machine declined, for a skip line: the refusal itself, not the
/// first line of stderr.
///
/// The first line is often a note -- `note: mode defaulted to trace, the only
/// one net.request supports` -- and a skip that gives it as its reason says
/// nothing about why, which is what `.github/scripts/skip-gate.sh` refuses. The
/// refusal is the `error:` line and the indented lines under it, where the
/// resolver puts each backend's missing requirement.
fn refusal(stderr: &str) -> String {
    let mut lines = stderr
        .lines()
        .skip_while(|line| !line.starts_with("error:"));
    let Some(first) = lines.next() else {
        return stderr.trim().to_owned();
    };
    std::iter::once(first)
        .chain(lines.take_while(|line| line.starts_with(' ')))
        .collect::<Vec<_>>()
        .join("\n")
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
                    refusal(&stderr)
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

/// Why a read of `path` issues no block request, if it issues none: the
/// filesystem it is on, found as the longest mount point above it in
/// `/proc/self/mountinfo`, is not backed by a `/dev` device -- virtiofs, NFS,
/// 9p, FUSE, a tmpfs. A checkout shared into a VM is one.
fn off_any_block_device(path: &std::path::Path) -> Option<String> {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let table = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let (_, fstype, source) = table
        .lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let point = left.split_whitespace().nth(4)?;
            let mut right = right.split_whitespace();
            let fstype = right.next()?;
            let source = right.next()?;
            path.starts_with(point)
                .then(|| (point.len(), fstype.to_owned(), source.to_owned()))
        })
        .max_by_key(|(length, _, _)| *length)?;
    (!source.starts_with("/dev/") || fstype == "tmpfs").then(|| {
        format!(
            "{} is on {fstype} ({source}), which is no block device: no read of it is a block \
             request",
            path.display()
        )
    })
}

/// Why the vertical case's reads reach no device here, if they do not.
fn data_off_block_device() -> Option<String> {
    off_any_block_device(&root().join("target/omn-vertical"))
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

/// Every request of the session, in the order the client sends them.
const IDS: [&str; 5] = ["a001", "a002", "a003", "a004", "a005"];

fn maybe(event: &serde_json::Value, name: &str) -> Option<f64> {
    attrs(event).get(name).and_then(|value| value.as_f64())
}

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
    for id in IDS {
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
        if field(request.worker, "reads") > 0.0 && data_off_block_device().is_some() {
            eprintln!(
                "req:{id}: its reads are not checked against the device: {}",
                data_off_block_device().unwrap_or_default()
            );
        } else if field(request.worker, "reads") > 0.0 {
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
    if let Some(reason) = data_off_block_device() {
        eprintln!("skipped: {reason}");
        return;
    }
    let worker: f64 = field(run.role("worker"), "tid");
    for id in IDS {
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
    let ids = IDS.map(|id| run.request(id));
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
/// The segments are the tool's own rows, six of them now, and the client's
/// clock is the only thing here nobody in the server could have fudged: it runs
/// from before the request is sent to the last byte of the response. What the
/// rows cannot hold is the client's own: its send before the first byte is
/// queued, its wakeups, and the reading of bytes that had already left the
/// server -- for the slow reader, the last receive buffer's worth. What is left
/// between the two is printed, so a gap that grows is visible before it fails.
#[test]
fn the_segments_sum_to_the_latency_the_client_measured() {
    let run = needs_run!();
    for id in IDS {
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
    assert_eq!(events.len(), 5, "five requests declared, five ended");
    for event in events {
        assert_eq!(number(event, "tid"), worker, "{event}");
    }
}

/// The tables the tool printed are the decomposition of what it measured: one
/// per request, the first against the request's span -- its first byte in to
/// its last byte out -- the second against its wait for its block I/O, whose
/// one row is the device's time for exactly the block requests carrying the id.
#[test]
fn the_tool_decomposes_each_request_against_its_own_life() {
    let run = needs_run!();
    for name in ["one-request", "block-io"] {
        let decomposition = run.decomposition(name);
        assert_eq!(decomposition["by"], "req", "{name}");
    }
    assert_eq!(run.decomposition("one-request")["keys_seen"], 5);
    for id in IDS {
        let request = run.request(id);
        let span = number(request.event, "span_ns");
        assert_eq!(request.table["whole"]["amount"].as_f64(), Some(span));
        // The span is the life with the network on either side of it.
        assert_eq!(
            span,
            number(request.event, "rx_queued_ns")
                + number(request.event, "total_ns")
                + number(request.event, "tx_drain_ns"),
            "req:{id}"
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

/// The pause a client makes in the middle of its request lands in `net_rx`.
///
/// `req:a004`'s client sends the first half of its request, sleeps 30 ms on
/// its own clock and sends the rest. The server cannot declare a request it has
/// not read, so the pause is between the request's first byte and its birth --
/// which is the stretch `net_rx` covers before the birth. Three references,
/// all the client's, on the same clock as the kernel's stamps:
///
/// - the first byte cannot have reached the socket before the client sent it,
///   nor after the pause that followed its half (causality, no slack);
/// - the birth cannot have come before the client sent the second half, so
///   `net_rx` is at least the second send less the first byte (no slack);
/// - and against the pause itself, measured by the client around its sleep,
///   `net_rx` holds all of it but for how late the first half was delivered,
///   which is printed.
///
/// And the absence: no other request paused, and none has a `net_rx` a tenth
/// the size of this one.
#[test]
fn a_pause_in_the_middle_of_the_request_lands_in_net_rx() {
    let run = needs_run!();
    let request = run.request("a004");
    let sent = field(request.client, "sent_ns");
    let first_end = field(request.client, "first_end_ns");
    let second = field(request.client, "second_ns");
    let paused = second - first_end;
    let first_byte = number(request.event, "first_byte_ns");
    let net_rx = request.row("net_rx");
    eprintln!(
        "req:a004  net_rx {:.3} ms against a {:.3} ms pause; the first byte stamped {:+.1} us \
         after the client's first send returned",
        net_rx / MS,
        paused / MS,
        (first_byte - first_end) / 1e3
    );
    assert!(
        sent <= first_byte && first_byte <= second,
        "the first byte was stamped at {first_byte}, and the client sent it at {sent} and paused \
         until {second}"
    );
    assert!(
        net_rx >= second - first_byte,
        "the request cannot have been born before its second half was sent: net_rx {net_rx} is \
         less than {} ns",
        second - first_byte
    );
    assert!(
        net_rx >= 0.9 * paused,
        "the client paused {:.3} ms mid-request and net_rx holds {:.3} ms",
        paused / MS,
        net_rx / MS
    );
    assert!(net_rx <= field(request.client, "client_ns"));
    for id in IDS.iter().filter(|id| **id != "a004") {
        let other = run.request(id).row("net_rx");
        assert!(
            other < paused / 10.0,
            "req:{id} did not pause and has {:.3} ms of net_rx",
            other / MS
        );
    }
}

/// A slow reader's response is held in `net_tx`, and not in `lock_wait`.
///
/// `req:a005`'s client asks for 128 KiB through a 4 KiB receive buffer and
/// sleeps a millisecond after every read; the server answers through a small
/// send buffer. The send blocks on the closed window -- an interruptible sleep
/// that `lock_wait` held before the socket was followed -- and returns with
/// the last send buffer's worth still queued, which leaves the stack well after
/// the server has declared the request done. `net_tx` is both.
///
/// The references, on the client's and the server's clocks:
///
/// - the last byte left the stack **before the client read it**, and **not
///   before the client made room for it**: before the read after which less
///   than a receive buffer was unread, more than a window of the response had
///   not been transmitted. Both are absolute stamps, with no slack;
/// - the waits inside the send are inside the server's own clock around it;
/// - from the send's start to the last byte leaving, the thread was either
///   waiting on the socket or on a cpu or runnable, so `net_tx` and those two
///   together cover it, and `net_tx` alone does not exceed it.
///
/// And the absence: every other request's response fits the window, and has
/// less than a millisecond of `net_tx`.
#[test]
fn a_slow_reader_holds_the_response_in_net_tx() {
    let run = needs_run!();
    let request = run.request("a005");
    let drained = number(request.event, "drained_ns");
    let room = field(request.client, "room_ns");
    let last = field(request.client, "last_ns");
    let send_start = field(request.worker, "send_start_ns");
    let tx_wait = number(request.event, "tx_wait_ns");
    let net_tx = request.row("net_tx");
    eprintln!(
        "req:a005  net_tx {:.3} ms ({:.3} ms blocked in the send, the server's own send {:.3} ms; \
         {:.3} ms draining after the end); the client slept {:.3} ms over {} reads; the last byte \
         left {:.3} ms after the client made room and {:.3} ms before its last read ended",
        net_tx / MS,
        tx_wait / MS,
        field(request.worker, "send_ns") / MS,
        number(request.event, "tx_drain_ns") / MS,
        field(request.client, "slept_ns") / MS,
        field(request.client, "reads"),
        (drained - room) / MS,
        (last - drained) / MS,
    );
    assert!(
        room <= drained && drained <= last,
        "the last byte left at {drained}; the client made room for it at {room} and had read it \
         by {last}"
    );
    assert!(tx_wait > 0.0, "the send never blocked on the window");
    assert!(
        tx_wait <= field(request.worker, "send_ns"),
        "more wait on the socket than the server's own clock around its send"
    );
    let covered = drained - send_start;
    assert!(
        net_tx <= covered,
        "net_tx {:.3} ms is more than the {:.3} ms from the send to the last byte",
        net_tx / MS,
        covered / MS
    );
    assert!(
        net_tx + request.row("cpu_on") + request.row("sched_wait") >= covered,
        "net_tx {:.3} ms, on a cpu and runnable leave part of the {:.3} ms from the send to the \
         last byte uncovered",
        net_tx / MS,
        covered / MS
    );
    assert!(
        request.row("lock_wait") < MS,
        "the wait on the window is not a lock"
    );
    for id in IDS.iter().filter(|id| **id != "a005") {
        let other = run.request(id).row("net_tx");
        assert!(
            other < MS,
            "req:{id} sent a response that fits the window and has {:.3} ms of net_tx",
            other / MS
        );
    }
}

/// The network rows are read by the socket the server accepted, and the ladder
/// says what that is worth.
///
/// Each event carries `sock:<cookie>` beside `req:<id>`, and the cookie is the
/// one the server read off its accepted connection with `SO_COOKIE` -- a
/// number the tool obtains through `bpf_get_socket_cookie` at the first byte,
/// on another thread's cpu, and the server obtains from the kernel on its own.
///
/// The evidence: the four rows read off the serving thread are `IDENTITY`, and
/// `net_rx` and `net_tx` are `KEY` -- the session reads them `via: sock`, and a
/// row is rated the weaker of the key it was carried through and the table's.
/// Rating them lower moves no amount, and that is checked here rather than
/// assumed: the six rows still tile the request's span to the nanosecond, and
/// still add up to the latency the client measured on its own clock, within
/// the 10 % `the_segments_sum_to_the_latency_the_client_measured` holds them to.
#[test]
fn the_network_rows_are_read_by_the_socket_the_server_accepted() {
    let run = needs_run!();
    for id in IDS {
        let request = run.request(id);
        let cookie = key(request.event, "sock")
            .and_then(|sock| sock["cookie"].as_f64())
            .unwrap_or_else(|| panic!("req:{id} carries no socket: {}", request.event));
        assert_eq!(
            cookie,
            field(request.worker, "cookie"),
            "req:{id}: the server's SO_COOKIE"
        );
        let evidence = |state: &str| {
            request.table["rows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["state"] == state)
                .map(|row| row["evidence"].as_str().unwrap_or_default().to_owned())
                .unwrap_or_default()
        };
        for state in ["cpu_on", "sched_wait", "lock_wait", "block_io"] {
            assert_eq!(evidence(state), "IDENTITY", "req:{id} {state}");
        }
        for state in ["net_rx", "net_tx"] {
            assert_eq!(evidence(state), "KEY", "req:{id} {state}");
        }
        let residue = request.table["residue"].as_f64().expect("a residue");
        assert!(
            residue.abs() < 1.0,
            "req:{id}: the six rows tile the span, and left {residue} ns"
        );
        let client = field(request.client, "client_ns");
        assert!(
            ((client - request.parts()) / client).abs() < 0.10,
            "req:{id}: the rows add up to {:.3} ms and the client measured {:.3} ms",
            request.parts() / MS,
            client / MS
        );
    }
}

/// The `block-io` table is held to its evidence level the way the one-request
/// table is.
///
/// Its one part is read by the request's id off every block request that
/// carries it, and the id is the one the serving thread held at the issue: so
/// the row is `IDENTITY`, and so is the whole, read off the request's own
/// event. The level is only worth what it counts, so the row is also held to
/// the server's own account: one block request per page it read with
/// `O_DIRECT` (`done_reads`), and a request that read nothing has no table.
/// And no row of either table goes without a level: the engine refuses to read
/// back a table one of whose rows has none, and here it is checked on the
/// document the run wrote.
#[test]
fn the_block_io_rows_carry_the_identity_the_block_requests_read_back() {
    let run = needs_run!();
    if let Some(reason) = data_off_block_device() {
        eprintln!("skipped: {reason}");
        return;
    }
    for name in ["one-request", "block-io"] {
        for table in run.decomposition(name)["tables"]
            .as_array()
            .expect("tables")
        {
            for row in table["rows"].as_array().expect("rows") {
                assert!(
                    row["evidence"].is_string(),
                    "{name} {}: a row with no evidence level: {row}",
                    table["key"]
                );
            }
        }
    }
    let mut tabled = 0;
    for id in IDS {
        let request = run.request(id);
        let reads = field(request.worker, "done_reads") as u64;
        let Some(table) = request.block_table else {
            assert_eq!(
                reads, 0,
                "req:{id} read {reads} pages and has no `block-io` table"
            );
            continue;
        };
        tabled += 1;
        assert_eq!(table["whole"]["evidence"], "IDENTITY", "req:{id} whole");
        let device = table["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["state"] == "device")
            .unwrap_or_else(|| panic!("req:{id}: no `device` row in {table}"));
        assert_eq!(device["evidence"], "IDENTITY", "req:{id} device");
        assert_eq!(
            device["readings"].as_u64(),
            Some(reads),
            "req:{id} read {reads} pages with O_DIRECT, one block request each"
        );
    }
    assert!(
        tabled >= 2,
        "a002 and a003 read, and {tabled} requests had a table"
    );
}

/// Without the socket, a wait on it is a sleep, and the run says so.
///
/// The same slow reader through `bpf:usdt:threads`, the backend for a kernel
/// that cannot trace the socket functions: the request has no network rows and
/// no `sock` key, the run says which backend would have them, and the send's
/// wait on the window is in `sleep_ns` -- with the client, whose reads opened
/// the window, named as the thread that "released" it. That is the misreading
/// the socket rows exist to prevent, shown on purpose.
#[test]
fn without_the_socket_a_wait_on_it_is_a_sleep_and_the_run_says_so() {
    let Ok(server) = server() else {
        eprintln!("SKIPPED: no server");
        return;
    };
    let output = omn(&[
        "net.request",
        "--backend",
        "force:bpf:usdt:threads",
        "--emit",
        "raw",
        "--format",
        "json",
        "--",
        server.to_str().unwrap(),
        "target/omn-vertical/threads",
        "0:0:0:131072",
    ]);
    let (stdout, stderr) = (text(&output.stdout), text(&output.stderr));
    if !output.status.success() && machine_cannot_serve(&stderr) {
        eprintln!("SKIPPED: {}", refusal(&stderr));
        return;
    }
    assert!(output.status.success(), "{stderr}");
    let document: serde_json::Value = serde_json::from_str(&stdout).expect("one json document");
    let result = &document["results"][0];
    let notes: Vec<&str> = result["degradations"]
        .as_array()
        .expect("notes")
        .iter()
        .filter_map(|note| note.as_str())
        .collect();
    assert!(
        notes.iter().any(|note| note.contains("does not follow one")
            && note.contains("`bpf:usdt` is the backend that does")),
        "{notes:#?}"
    );
    let event = &result["outputs"][0]["events"][0];
    assert!(key(event, "sock").is_none(), "{event}");
    for absent in ["net_rx_ns", "net_tx_ns", "span_ns"] {
        assert!(maybe(event, absent).is_none(), "{absent} on {event}");
    }
    let send_ns: f64 = stderr
        .lines()
        .find_map(|line| line.split("send_ns=").nth(1))
        .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
        .expect("the worker timed its send");
    let client_tid: f64 = stderr
        .lines()
        .find_map(|line| line.strip_prefix("vertical: role=client tid="))
        .and_then(|rest| rest.trim().parse().ok())
        .expect("the client introduced itself");
    let sleep = number(event, "sleep_ns");
    assert!(
        sleep >= 0.5 * send_ns,
        "the send blocked {:.3} ms and only {:.3} ms of sleep was seen",
        send_ns / MS,
        sleep / MS
    );
    assert_eq!(
        number(event, "holder_tid"),
        client_tid,
        "the reader's window update is what woke the send"
    );
}

/// A probe behind a semaphore fires, because the kernel raises it.
///
/// The server built with `-DGUARDED` skips both probe sites unless their
/// semaphores are non-zero, which is how a program makes a probe cost nothing
/// when nobody listens, and it prints the semaphore it saw at the birth. The
/// ninth wave refused such a probe; the link now hands the kernel each site's
/// semaphore to raise for as long as it is attached. The reference is the
/// server's own read of it: raised under the tool, and the request followed.
#[test]
fn a_probe_behind_a_semaphore_is_raised_and_followed() {
    let server = match guarded_server() {
        Ok(server) => server,
        Err(reason) => {
            eprintln!("SKIPPED: {reason}");
            return;
        }
    };
    let output = omn(&[
        "net.request",
        "--emit",
        "raw",
        "--format",
        "json",
        "--",
        server.to_str().unwrap(),
        "target/omn-vertical/guarded",
        "5:0",
    ]);
    let (stdout, stderr) = (text(&output.stdout), text(&output.stderr));
    if !output.status.success() && machine_cannot_serve(&stderr) {
        eprintln!("SKIPPED: {}", refusal(&stderr));
        return;
    }
    assert!(output.status.success(), "{stderr}");
    let guard = stderr
        .lines()
        .find_map(|line| line.split(" guard=").nth(1))
        .and_then(|rest| rest.split_whitespace().next())
        .expect("the worker reported its semaphore");
    assert_eq!(guard, "1", "the semaphore was raised: {stderr}");
    let document: serde_json::Value = serde_json::from_str(&stdout).expect("one json document");
    let events = document["results"][0]["outputs"][0]["events"]
        .as_array()
        .expect("events");
    assert_eq!(events.len(), 1, "the guarded request was followed");
    assert_eq!(request_of(&events[0]).as_deref(), Some("a001"));
}

/// A program that declares no request is refused with what it does carry.
#[test]
fn a_program_that_declares_no_request_is_refused_by_name() {
    let output = omn(&["net.request", "--emit", "raw", "--", "/bin/true"]);
    let stderr = text(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    // The probes are looked for once the resolver has chosen a backend. A
    // machine that cannot load the program refuses before that, for its own
    // reason, and that refusal is the right answer there -- not this one.
    if machine_cannot_serve(&stderr) && !stderr.contains("request__start") {
        eprintln!("SKIPPED: {}", refusal(&stderr));
        return;
    }
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
/// itself -- born three, ended two, which one did not end and in which thread,
/// and that the second birth replaced a live request. And the second request
/// must be whole: its lock wait is the holder's sleep, not the abandoned
/// request's time added to it.
///
/// The third is the network's loss path: it ends, and its client closes
/// without reading the answer, so the response never leaves the stack. The
/// request is an event with no `net_tx`, and the run counts it.
#[test]
fn a_request_that_never_ends_is_counted_and_named_and_does_not_leak() {
    if let Err(reason) = server() {
        eprintln!("SKIPPED: {reason}");
        return;
    }
    let output = omn(&["-f", "examples/vertical-abandoned.yml", "--format", "json"]);
    let (stdout, stderr) = (text(&output.stdout), text(&output.stderr));
    if !output.status.success() && machine_cannot_serve(&stderr) {
        eprintln!("SKIPPED: {}", refusal(&stderr));
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
            .any(|note| note.starts_with("3 requests were born and 2 ended")),
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
    assert_eq!(events.len(), 2, "two requests ended");
    // The third ended and its response never left the stack: the client closed
    // without reading it. Its event is whole but for `net_tx`, which is absent
    // rather than the part of a drain that was seen, and the run says why.
    let unread = events
        .iter()
        .find(|event| request_of(event).as_deref() == Some("a003"))
        .expect("the unread request ended");
    assert!(maybe(unread, "net_rx_ns").is_some(), "{unread}");
    for absent in ["net_tx_ns", "tx_drain_ns", "span_ns"] {
        assert!(maybe(unread, absent).is_none(), "{absent} on {unread}");
    }
    assert!(
        said.iter()
            .any(|note| note.contains("1 response had not left the stack when the run ended")),
        "{said:#?}"
    );
    let event = events
        .iter()
        .find(|event| request_of(event).as_deref() == Some("a002"))
        .expect("the second request ended");
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
