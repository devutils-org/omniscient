//! Scenarios for the `net` family.
//!
//! A scenario is a workload with a property this file chose, a session that
//! measures it, and a check of the answer against that property. The number
//! comes from outside the tool -- a delay injected by a network emulator, a
//! count of datagrams a loop sent, a link whose far end was never brought up --
//! and the question each test asks is whether the tool found it.
//!
//! The four classes of `BRIEF.md` are marked in the section headers below. What
//! this family cannot check, and why, is in `docs/scenarios/net.md`, together
//! with the defects these scenarios found.
//!
//! Several scenarios need a network of their own: a loopback nobody else is
//! using, a veth pair, a queueing discipline, or `CAP_NET_RAW`. They get one by
//! re-running themselves inside a user and network namespace, which is also
//! what makes their counts exact -- the only traffic there is theirs. A machine
//! that allows no unprivileged user namespace cannot serve them, and they say
//! so on stderr and stop rather than reporting a pass.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");
const EXAMPLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

fn omn(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

fn results(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output)).unwrap_or_else(|error| {
        panic!(
            "the run did not produce json ({error}):\n{}",
            stderr(output)
        )
    })
}

/// The path of one of this family's sessions.
fn session(name: &str) -> String {
    format!("{EXAMPLES}/{name}")
}

/// Every emit a result carries, in the order the run produced them.
fn emits(result: &serde_json::Value) -> Vec<String> {
    result["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .map(|output| output["emit"].as_str().expect("an emit name").to_owned())
        .collect()
}

/// The named result of a session run, by its label.
fn labelled<'a>(run: &'a serde_json::Value, label: &str) -> &'a serde_json::Value {
    run["results"]
        .as_array()
        .expect("results")
        .iter()
        .find(|result| result["label"] == label)
        .unwrap_or_else(|| panic!("no result labelled `{label}` in {run}"))
}

/// The total of a `rate` series, for one key.
fn rate_total(result: &serde_json::Value, key: &str) -> f64 {
    result["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .find(|output| output["emit"] == "rate")
        .unwrap_or_else(|| panic!("no rate in {result}"))["series"]
        .as_array()
        .expect("series")
        .iter()
        .find(|series| series["key"] == key)
        .unwrap_or_else(|| panic!("no series `{key}` in {result}"))["total"]
        .as_f64()
        .expect("a total")
}

/// Moves `bytes` over a loopback connection and returns once both ends are done.
///
/// The transfer is the ground truth of every volume scenario here: the size is
/// chosen in the caller, not read off anything.
fn push_through_loopback(bytes: usize) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("local address");

    let receiver = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut sink = vec![0u8; 64 * 1024];
        let mut received = 0usize;
        while received < bytes {
            match stream.read(&mut sink) {
                Ok(0) => break,
                Ok(read) => received += read,
                Err(_) => break,
            }
        }
        received
    });

    let mut stream = TcpStream::connect(address).expect("connect");
    let block = vec![0x5au8; 64 * 1024];
    let mut sent = 0usize;
    while sent < bytes {
        let chunk = block.len().min(bytes - sent);
        stream.write_all(&block[..chunk]).expect("write");
        sent += chunk;
    }
    stream.flush().expect("flush");
    drop(stream);

    let received = receiver.join().expect("receiver");
    assert!(received >= bytes, "the loopback transfer did not complete");
}

fn pause(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// Every byte loopback has carried since boot, received and transmitted.
///
/// The kernel's own counter, read straight out of the file the `fs:proc`
/// backend reads. It is what gives a scenario a ceiling that does not depend on
/// what the rest of the machine happens to be doing: a run's window is inside
/// the one two reads of this bound, so a run cannot report more than the
/// difference.
fn loopback_bytes() -> u64 {
    let text = std::fs::read_to_string("/proc/net/dev").expect("/proc/net/dev");
    for line in text.lines() {
        let Some((name, counters)) = line.split_once(':') else {
            continue;
        };
        if name.trim() != "lo" {
            continue;
        }
        let fields: Vec<u64> = counters
            .split_whitespace()
            .filter_map(|field| field.parse().ok())
            .collect();
        // Received bytes first, transmitted bytes ninth, as the file's own
        // header names them.
        return fields[0] + fields[8];
    }
    panic!("/proc/net/dev has no loopback");
}

// ---------------------------------------------------------------------------
// A network of one's own
// ---------------------------------------------------------------------------

/// Tells a re-executed test that it is already inside the namespace it asked
/// for.
const IN_NAMESPACE: &str = "OMN_NET_SCENARIO_IN_NETNS";

/// Whether this process is the copy running inside the namespace.
fn inside_a_namespace() -> bool {
    std::env::var_os(IN_NAMESPACE).is_some()
}

/// Runs a shell command inside the current namespace, and returns its output.
fn shell(script: &str) -> Output {
    Command::new("sh")
        .arg("-c")
        .arg(script)
        .output()
        .unwrap_or_else(|error| panic!("running `{script}`: {error}"))
}

/// Whether the tools these scenarios build their networks with are installed.
///
/// `ip` and `tc` are both iproute2. A machine without them cannot serve the
/// scenarios that need a link of their own, and that is a fact about the
/// machine rather than about the tool.
fn iproute2_is_installed() -> bool {
    Command::new("ip")
        .arg("-Version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Runs this same test again inside a user and network namespace of its own.
///
/// `unshare -Ur -n` maps this user to root in a new user namespace, which
/// carries `CAP_NET_RAW` and `CAP_NET_ADMIN` inside it, and gives the new
/// network namespace nothing but a loopback that is down. That is what makes
/// the counts in these scenarios exact: the only traffic on that network is the
/// scenario's own.
///
/// Returns `false` when the machine allows no such namespace, having said so.
#[must_use]
fn rerun_inside_a_namespace(name: &str) -> bool {
    let binary = std::env::current_exe().expect("the test binary");
    let outcome = Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "--"])
        .arg(&binary)
        .args(["--exact", name, "--nocapture", "--test-threads", "1"])
        .env(IN_NAMESPACE, "1")
        .output();

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!(
                "`{name}` needs a network namespace of its own and `unshare` is not installed \
                 ({error}); this machine cannot serve it"
            );
            return false;
        }
    };
    let complaint = stderr(&outcome);
    if !outcome.status.success() && !complaint.contains("panicked") {
        eprintln!(
            "`{name}` needs an unprivileged user namespace and this machine allows none: \
             {complaint}"
        );
        return false;
    }
    assert!(
        outcome.status.success(),
        "`{name}` failed inside its own namespace:\n{}\n{complaint}",
        stdout(&outcome)
    );
    true
}

/// Brings up the loopback of a fresh network namespace, which starts down.
fn bring_loopback_up() {
    let outcome = shell("ip link set lo up");
    assert!(
        outcome.status.success(),
        "bringing up the namespace's loopback: {}",
        stderr(&outcome)
    );
}

// ---------------------------------------------------------------------------
// Class A -- the capability sweep
// ---------------------------------------------------------------------------

/// The source the resolver said this machine cannot serve, when it said so:
/// exit 1 and *"`<source>` has no viable backend on this machine"*.
fn withheld_source(outcome: &Output) -> Option<String> {
    const WITHHELD: &str = "` has no viable backend on this machine";
    let said = stderr(outcome);
    if code(outcome) != 1 {
        return None;
    }
    let at = said.find(WITHHELD)?;
    let opened = said[..at].rfind('`')?;
    Some(said[opened + 1..at].to_owned())
}

/// A sweep session with every request for `source` taken out, written to a
/// scratch file: the requests are the `  - label:` blocks under `requests:`.
fn without_source(text: &str, source: &str) -> String {
    let (head, requests) = text
        .split_once("\nrequests:\n")
        .expect("a session with requests");
    let mut kept = format!("{head}\nrequests:\n");
    for block in requests
        .split_inclusive('\n')
        .fold(Vec::<String>::new(), |mut blocks, line| {
            if line.starts_with("  - label:") || blocks.is_empty() {
                blocks.push(String::new());
            }
            blocks.last_mut().expect("a block").push_str(line);
            blocks
        })
    {
        let names_it = block
            .lines()
            .any(|line| line.trim() == format!("source: {source}"));
        if !names_it {
            kept.push_str(&block);
        }
    }
    kept
}

/// Runs one leg of the sweep, leaving out each source the resolver said this
/// machine cannot serve, and printing it.
///
/// A sweep is about the axes, and a source the machine cannot serve is a
/// machine's limit -- the CI's runner keeps a PTP hardware clock only root may
/// read, so `net.hwts` has nothing to measure there. The rest of the leg is
/// still asked: skipping the whole leg for one withheld source left every
/// other source of it untested wherever that one was missing. Returns the
/// outcome and the sources left out.
fn run_leg(name: &str) -> (Output, Vec<String>) {
    let mut text = std::fs::read_to_string(session(name)).expect("the sweep session");
    let mut left_out = Vec::new();
    loop {
        let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("leg-{}", left_out.len()))
            .join(name);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("a scratch directory");
        std::fs::write(&path, &text).expect("the session");
        let outcome = omn(&["-f", path.to_str().expect("a path"), "--format", "json"]);
        let Some(source) = withheld_source(&outcome) else {
            if !left_out.is_empty() {
                eprintln!(
                    "note: `{name}` ran without {left_out:?}, which this machine cannot serve"
                );
            }
            return (outcome, left_out);
        };
        assert!(
            !left_out.contains(&source),
            "`{source}` was left out of `{name}` and still refused:\n{}",
            stderr(&outcome)
        );
        eprintln!(
            "note: this machine cannot serve `{source}`, left out of `{name}`:\n{}",
            stderr(&outcome)
        );
        text = without_source(&text, &source);
        left_out.push(source);
    }
}

/// Leaving a withheld source out of a sweep leg leaves every other request of
/// it, and a session that still validates: the pruning `run_leg` does where a
/// machine cannot serve `net.hwts`, done here where it can.
#[test]
fn a_leg_without_a_withheld_source_keeps_every_other_request() {
    for (name, requests) in [("net-capability-sweep.yml", 6), ("net-snapshot.yml", 3)] {
        let text = std::fs::read_to_string(session(name)).expect("the sweep session");
        let count = |text: &str| text.lines().filter(|l| l.starts_with("  - label:")).count();
        assert_eq!(count(&text), requests, "`{name}` changed its requests");
        let pruned = without_source(&text, "net.hwts");
        assert_eq!(
            count(&pruned),
            requests - 1,
            "`{name}` without net.hwts:\n{pruned}"
        );
        assert!(!pruned.contains("source: net.hwts"), "{pruned}");
        let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("pruned")
            .join(name);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("a scratch directory");
        std::fs::write(&path, &pruned).expect("the session");
        let outcome = omn(&["validate", "-f", path.to_str().expect("a path")]);
        assert_eq!(code(&outcome), 0, "{pruned}\n{}", stderr(&outcome));
    }
}

/// Every `net` session this family ships parses and validates.
///
/// Cheap, and it is the only check that an example a reader is told to run
/// still describes something the catalog offers. Every session here validates,
/// including `net-not-built.yml`: a request nothing collects is still a request
/// that means something, and the refusal comes later.
#[test]
fn the_sessions_this_family_ships_say_something_the_catalog_offers() {
    let mut seen = 0;
    for entry in std::fs::read_dir(EXAMPLES).expect("the examples directory") {
        let path = entry.expect("a directory entry").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        if !name.starts_with("net-") {
            continue;
        }
        seen += 1;
        let outcome = omn(&["validate", "-f", path.to_str().expect("a path")]);
        assert_eq!(
            code(&outcome),
            0,
            "`{name}` did not validate:\n{}",
            stderr(&outcome)
        );
    }
    assert!(seen >= 9, "only {seen} net sessions were found");
}

/// The `poll` leg of the sweep: six sources, every emit their `poll` mode
/// declares, over one window.
///
/// The check is that each request produced exactly the emits it asked for and
/// saw something. What the numbers mean is each source's own scenario below;
/// this one is about the axes, which is the class of lie nobody looks for --
/// a combination the catalog offers and the build cannot serve.
#[test]
fn the_poll_leg_of_the_sweep_answers_with_every_emit_it_promised() {
    let (outcome, left_out) = run_leg("net-capability-sweep.yml");
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);

    let expected: &[(&str, &str, &[&str])] = &[
        ("volume", "net.bytes", &["timeline", "summary"]),
        ("packets", "net.packets", &["timeline", "summary"]),
        ("links", "net.iface", &["raw", "timeline", "summary"]),
        (
            "accept-queue",
            "net.backlog",
            &["raw", "timeline", "summary"],
        ),
        ("round-trip", "net.tcp.rtt", &["raw", "timeline", "summary"]),
        ("nic-clocks", "net.hwts", &["raw", "timeline", "summary"]),
    ];
    for (label, source, wanted) in expected {
        if left_out.iter().any(|out| out == source) {
            continue;
        }
        let result = labelled(&run, label);
        assert_eq!(&emits(result), wanted, "`{label}` emitted the wrong set");
        assert!(
            result["losses"].is_object(),
            "`{label}` reported no loss accounting at all"
        );
    }

    // Three of the six answer for the machine whatever it is doing, so they are
    // the ones whose silence would be a defect rather than an idle machine.
    for label in ["volume", "packets", "links"] {
        let seen = labelled(&run, label)["events_seen"]
            .as_u64()
            .expect("an event count");
        assert!(
            seen > 0,
            "`{label}` polled every interface four times and produced nothing"
        );
    }
}

/// The `count` leg, with the ground truth attached: eight mebibytes pushed
/// through loopback while the session runs.
///
/// `count` is the only mode of these four sources that can produce a `rate`,
/// which is the whole reason the mode exists next to `poll`: a counter
/// aggregated over a window is a rate, and a level sampled at a cadence is not.
#[test]
fn the_count_leg_of_the_sweep_finds_the_traffic_the_scenario_pushed() {
    const MOVED: usize = 4 * 1024 * 1024;

    let moved_at_start = loopback_bytes();
    let measuring = Command::new(BINARY)
        .args(["-f", &session("net-counters.yml"), "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");

    // The first read of a counter source is a baseline and produces nothing, so
    // the traffic has to move after it.
    pause(400);
    push_through_loopback(MOVED);

    let outcome = measuring.wait_with_output().expect("omn should finish");
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);

    let expected: &[(&str, &[&str])] = &[
        ("volume", &["rate", "summary"]),
        ("packets", &["rate", "summary"]),
        ("links", &["rate", "raw", "summary"]),
        ("accept-queue", &["rate", "raw", "summary"]),
    ];
    for (label, wanted) in expected {
        assert_eq!(
            &emits(labelled(&run, label)),
            wanted,
            "`{label}` emitted the wrong set"
        );
    }

    // Loopback counts every byte twice, once received and once transmitted,
    // because it is both: the floor is the transfer this scenario made.
    //
    // The ceiling is the interface's own counter over a window that contains
    // this run, read here, and not a multiple of the transfer -- which is what
    // it was, and which made this test a statement about what the rest of the
    // machine was doing. Eight times the transfer is thirty-three megabytes,
    // and a desktop with a browser and a tunnel on it moves that through
    // loopback on its own often enough to fail this a few times an hour. The
    // bound below cannot be exceeded whatever else is running, because the
    // tool's window is inside the one the two reads bound.
    let bytes = rate_total(labelled(&run, "volume"), "iface:lo");
    assert!(
        bytes >= MOVED as f64,
        "{MOVED} bytes went through loopback and the counters saw {bytes}"
    );
    let crossed = (loopback_bytes() - moved_at_start) as f64;
    assert!(
        bytes <= crossed,
        "the run reports {bytes} bytes over loopback and the interface's own counters moved by \
         {crossed} over a window containing it, so the run counted something twice"
    );

    // Whatever the segment size was, that many bytes cannot have crossed in
    // fewer packets than the interface's own MTU allows.
    let packets = rate_total(labelled(&run, "packets"), "iface:lo");
    assert!(
        packets >= bytes / 65_536.0,
        "{bytes} bytes over loopback in {packets} packets is more than 64 KiB each"
    );
}

/// The `snapshot` leg: one read, now.
///
/// Every emit answers, and that is all this leg can be asked to prove. What the
/// numbers mean is
/// `a_snapshot_of_the_accept_queue_reports_the_queue_the_kernel_holds`: the two
/// counter sources here report the counters as they stand rather than an
/// increment over a microsecond window, which they used to and which
/// `docs/scenarios/net.md` records under "Defects found" 3.
#[test]
fn the_snapshot_leg_of_the_sweep_answers_with_every_emit_it_promised() {
    let (outcome, left_out) = run_leg("net-snapshot.yml");
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);

    for (label, source) in [
        ("links", "net.iface"),
        ("accept-queue", "net.backlog"),
        ("nic-clocks", "net.hwts"),
    ] {
        if left_out.iter().any(|out| out == source) {
            continue;
        }
        let result = labelled(&run, label);
        assert_eq!(
            emits(result),
            ["raw", "summary"],
            "`{label}` emitted the wrong set"
        );
        assert_eq!(
            result["stopped_because"], "snapshot: a single read",
            "`{label}` was asked for a snapshot and did something else"
        );
    }
}

/// Every emit a `net` source declares has to be reachable from a mode it
/// declares.
///
/// An emit is a promise the catalog makes and the mode x emit matrix can break
/// without anyone noticing: the entry lists it, `omn list` prints it, and every
/// request that could ask for it is refused. There is one such promise in this
/// family today, and this test pins it so that a second one cannot arrive
/// quietly -- and so that fixing the first is noticed here.
#[test]
fn every_emit_a_net_source_declares_is_reachable_from_a_mode_it_declares() {
    use omn::catalog::Catalog;

    /// The one the sweep found, now fixed: `net.bytes` declared `topk`, which
    /// comes from `sample` or `trace`, and declares neither. The emit is out of
    /// the entry, so this family promises nothing it cannot reach.
    ///
    /// It was removed rather than served because the ranking is expressible --
    /// the collector emits one event per interface, so the series exist -- and
    /// what refuses it is the mode x emit matrix, which is not this stream's.
    /// `docs/open-findings.md` asks for `count` to be able to rank what it
    /// counted; the day it can, `topk` comes back to `catalog/net.yaml` and
    /// this stays empty.
    const RECORDED: &[(&str, &str)] = &[];

    let catalog = Catalog::embedded().expect("the embedded catalog");
    let mut unreachable = Vec::new();
    for entry in catalog.family("net") {
        for emit in &entry.emits {
            let reachable = entry
                .modes
                .iter()
                .any(|mode| mode.valid_emits().contains(emit));
            if !reachable {
                unreachable.push((entry.id.as_str().to_owned(), emit.as_str().to_owned()));
            }
        }
    }

    let recorded: Vec<(String, String)> = RECORDED
        .iter()
        .map(|(source, emit)| ((*source).to_owned(), (*emit).to_owned()))
        .collect();
    assert_eq!(
        unreachable, recorded,
        "the set of `net` emits no mode can produce changed. If one was fixed, drop it from \
         RECORDED and from the \"Defects found\" section of docs/scenarios/net.md; if one \
         appeared, that is a promise the catalog now makes and no request can collect"
    );

    // And the ranking the entry dropped while `count` could not rank, which it
    // declares again since the thirteenth wave made a counted window rankable:
    // a reader asking for it is served, not refused.
    let outcome = omn(&[
        "net.bytes",
        "--iface",
        "lo",
        "--mode",
        "count",
        "--duration",
        "200ms",
        "--emit",
        "topk",
    ]);
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
}

/// The format axis: what a `net` result can be written as, and what it cannot.
///
/// `net` has no stacks and never will -- a frame off the wire has no call
/// stack -- so the three stack formats have to refuse by name rather than write
/// an empty profile. The timeline formats have to carry a timeline.
#[test]
fn a_net_result_is_written_in_every_format_that_can_carry_it_and_refused_by_the_rest() {
    let timeline = [
        "net.bytes",
        "--iface",
        "lo",
        "--mode",
        "poll@50ms",
        "--duration",
        "150ms",
        "--emit",
        "timeline",
        "--format",
    ];
    for format in ["json", "yaml", "toml", "ndjson", "chrome", "speedscope"] {
        let mut args = timeline.to_vec();
        args.push(format);
        let outcome = omn(&args);
        assert_eq!(
            code(&outcome),
            0,
            "`{format}` could not carry a timeline: {}",
            stderr(&outcome)
        );
        assert!(
            !stdout(&outcome).is_empty(),
            "`{format}` wrote nothing at all"
        );
    }

    // A rate is a number per window. Nothing collapses it into stacks, and the
    // refusal says which format was asked and what that format is for.
    let rate = [
        "net.bytes",
        "--iface",
        "lo",
        "--mode",
        "count",
        "--duration",
        "200ms",
        "--emit",
        "rate",
        "--format",
    ];
    for format in ["folded", "pprof"] {
        let mut args = rate.to_vec();
        args.push(format);
        let outcome = omn(&args);
        assert_eq!(code(&outcome), 2, "`{format}` accepted a rate");
        let complaint = stderr(&outcome);
        assert!(
            complaint.contains(format) && complaint.contains("rate"),
            "the refusal should name both the format and the emit: {complaint}"
        );
    }

    // The timeline formats have to carry the points, not merely be produced.
    let mut args = timeline.to_vec();
    args.push("chrome");
    let outcome = omn(&args);
    let trace: serde_json::Value = serde_json::from_str(&stdout(&outcome)).expect("chrome json");
    assert!(
        !trace["traceEvents"]
            .as_array()
            .expect("trace events")
            .is_empty(),
        "the chrome trace carried no events"
    );
}

// ---------------------------------------------------------------------------
// Class B -- the comparison
// ---------------------------------------------------------------------------

/// No `net` source has two implemented backends, so the comparison inside a
/// chain is with the refusal: forcing a backend nobody has written must say
/// that this *build* is missing it, and must not read like a machine that could
/// be persuaded.
///
/// The exit code carries that distinction on its own, which matters to a caller
/// that never reads the message: 3 is a problem with this build, 1 is a problem
/// with this run.
#[test]
fn forcing_a_net_backend_nothing_implements_names_it_and_exits_three() {
    // Every declared-and-unwritten backend in the family's chains, each asked
    // for in a way that source accepts -- a mode or a target it does not accept
    // is a different refusal, and is checked below.
    let declared: &[(&str, &str, &[&str])] = &[
        (
            "net.bytes",
            "netlink",
            &["--iface", "lo", "--mode", "count", "--emit", "rate"],
        ),
        (
            "net.bytes",
            "bpf:fentry",
            &["--system", "--mode", "count", "--emit", "rate"],
        ),
        (
            "net.packets",
            "netlink",
            &["--iface", "lo", "--mode", "count", "--emit", "rate"],
        ),
        (
            "net.iface",
            "netlink",
            &["--iface", "lo", "--mode", "snapshot", "--emit", "summary"],
        ),
        (
            "net.tcp.rtt",
            "bpf:fentry",
            &["--system", "--mode", "poll@50ms", "--emit", "summary"],
        ),
        (
            "net.hwts",
            "so_timestamping",
            // `trace`, the one mode the per-packet backend declares.
            &["--pid", "1", "--mode", "trace", "--emit", "summary"],
        ),
    ];

    for (source, backend, how) in declared {
        let pinned = format!("force:{backend}");
        let mut args = vec![*source];
        args.extend_from_slice(how);
        args.extend_from_slice(&["--duration", "200ms", "--backend", &pinned]);
        let outcome = omn(&args);
        let complaint = stderr(&outcome);
        assert_eq!(
            code(&outcome),
            3,
            "`{source}` forced onto `{backend}` exited {} rather than 3:\n{complaint}",
            code(&outcome)
        );
        assert!(
            complaint.contains(backend) && complaint.contains(source),
            "the refusal should name the source and the backend: {complaint}"
        );
        assert!(
            complaint.contains("pinned"),
            "pinning bypasses the resolver and the run should say so: {complaint}"
        );
    }

    // Pinning a backend that could never answer the target is the other
    // refusal, and it is a rejection: no build of anything would serve it.
    let outcome = omn(&[
        "net.bytes",
        "--iface",
        "lo",
        "--mode",
        "count",
        "--duration",
        "200ms",
        "--emit",
        "rate",
        "--backend",
        "force:bpf:fentry",
    ]);
    assert_eq!(code(&outcome), 2, "{}", stderr(&outcome));
    assert!(
        stderr(&outcome).contains("cannot attribute per iface"),
        "the refusal should say what the pinned backend cannot do: {}",
        stderr(&outcome)
    );
}

/// The comparison this family does have: two mechanisms, one wire.
///
/// `/proc/net/dev` is the kernel's own per-interface accounting and `AF_PACKET`
/// is every frame copied out of a ring. They are written by different people
/// for different reasons and they count the same traffic, which is what makes
/// the agreement worth something.
///
/// The workload chooses the number: two thousand datagrams of a hundred bytes,
/// each carried twice because loopback is both ends. Everything else follows
/// from constants -- 128 bytes of IP, UDP and payload per frame in the device
/// counters, and 14 more in the capture, which is the link header
/// `/proc/net/dev` does not count.
#[test]
fn two_mechanisms_measuring_one_wire_agree_to_the_link_header() {
    const NAME: &str = "two_mechanisms_measuring_one_wire_agree_to_the_link_header";
    const DATAGRAMS: u64 = 2000;
    const PAYLOAD: usize = 100;
    /// IP header, UDP header and payload: what `/proc/net/dev` counts on lo.
    const IN_THE_COUNTERS: u64 = 20 + 8 + PAYLOAD as u64;
    /// The same frame on the wire, with loopback's link header.
    const ON_THE_WIRE: u64 = 14 + IN_THE_COUNTERS;

    if !inside_a_namespace() {
        if !iproute2_is_installed() {
            eprintln!("`{NAME}` needs `ip` from iproute2 to raise a loopback of its own");
            return;
        }
        let _served = rerun_inside_a_namespace(NAME);
        return;
    }
    bring_loopback_up();

    let measuring = Command::new(BINARY)
        .args(["-f", &session("net-two-mechanisms.yml"), "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");

    // The ring and the baseline read both have to be in place before a frame
    // moves, or the comparison is between two different windows.
    pause(700);
    flood_loopback(40555, DATAGRAMS, PAYLOAD);

    let outcome = measuring.wait_with_output().expect("omn should finish");
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);

    let frames = labelled(&run, "frames");
    assert_eq!(
        frames["losses"]["dropped"].as_u64().unwrap_or(0),
        0,
        "the capture dropped frames, so nothing below would mean anything: {}",
        frames["losses"]
    );
    let captured = frames["events_seen"].as_u64().expect("a frame count");
    assert_eq!(
        captured,
        2 * DATAGRAMS,
        "{DATAGRAMS} datagrams over loopback are {} frames, and the capture saw {captured}",
        2 * DATAGRAMS
    );

    let packets = rate_total(labelled(&run, "packets"), "iface:lo");
    assert_eq!(
        packets,
        (2 * DATAGRAMS) as f64,
        "the device counters saw {packets} packets where the capture saw {captured} frames"
    );

    let bytes = rate_total(labelled(&run, "volume"), "iface:lo");
    assert_eq!(
        bytes,
        (2 * DATAGRAMS * IN_THE_COUNTERS) as f64,
        "the device counters should hold {IN_THE_COUNTERS} bytes for each of {} frames",
        2 * DATAGRAMS
    );

    let wire: u64 = frames["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .find(|output| output["emit"] == "raw")
        .expect("the frames themselves")["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| {
            event["attrs"]
                .as_array()
                .expect("attributes")
                .iter()
                .find(|attr| attr["name"] == "bytes")
                .and_then(|attr| attr["value"].as_u64())
                .expect("a length on the wire")
        })
        .sum();
    assert_eq!(
        wire,
        2 * DATAGRAMS * ON_THE_WIRE,
        "the capture should report {ON_THE_WIRE} bytes on the wire for each frame"
    );

    // The one difference between the two mechanisms, and it is a constant.
    assert_eq!(
        wire - bytes as u64,
        2 * DATAGRAMS * 14,
        "the gap between the capture and the device counters is loopback's link header, \
         fourteen bytes a frame, and nothing else"
    );
}

/// Sends `count` datagrams of `payload` bytes to a bound port on loopback.
///
/// Bound, so they are delivered rather than answered with an ICMP port
/// unreachable that would show up in a capture as traffic nobody sent.
fn flood_loopback(port: u16, count: u64, payload: usize) {
    let receiver =
        UdpSocket::bind(("127.0.0.1", port)).expect("nothing else holds this port in here");
    let sender = UdpSocket::bind("127.0.0.1:0").expect("an ephemeral port");
    let datagram = vec![b'x'; payload];
    for _ in 0..count {
        sender
            .send_to(&datagram, ("127.0.0.1", port))
            .expect("send over loopback");
    }
    drop(receiver);
}

// ---------------------------------------------------------------------------
// Class C -- the loss path
// ---------------------------------------------------------------------------

/// A capture driven past what its ring can hold has to account for every frame
/// it did not keep.
///
/// This is the class that justifies the project. A capture that loses three
/// percent and says nothing produces wrong conclusions that look right, and the
/// run is gone by the time anyone notices. So the arithmetic has to close:
/// twenty thousand datagrams over loopback are forty thousand frames, and what
/// the capture kept plus what it says the kernel dropped has to be exactly
/// that. How the two split moves from run to run because it depends on how the
/// drain races the sender; the total does not, and the total is the claim.
#[test]
fn a_capture_under_more_load_than_its_ring_accounts_for_every_frame() {
    const NAME: &str = "a_capture_under_more_load_than_its_ring_accounts_for_every_frame";
    const DATAGRAMS: u64 = 20_000;

    if !inside_a_namespace() {
        if !iproute2_is_installed() {
            eprintln!("`{NAME}` needs `ip` from iproute2 to raise a loopback of its own");
            return;
        }
        let _served = rerun_inside_a_namespace(NAME);
        return;
    }
    bring_loopback_up();

    let measuring = Command::new(BINARY)
        .args([
            "-f",
            &session("net-capture-accounting.yml"),
            "--format",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");

    pause(500);
    flood_loopback(39998, DATAGRAMS, 100);

    let outcome = measuring.wait_with_output().expect("omn should finish");
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let result = &results(&outcome)["results"][0];

    let kept = result["events_seen"].as_u64().expect("a frame count");
    let dropped = result["losses"]["dropped"].as_u64().expect("a drop count");
    // Twenty thousand rather than five: at five the drain sometimes keeps up
    // and nothing is lost, which is a fine outcome for a capture and a useless
    // one for a scenario about losing.
    assert!(
        dropped > 0,
        "{DATAGRAMS} datagrams went past a one-mebibyte ring and it says it lost none; \
         either the ring grew or the losses are not being read"
    );
    assert_eq!(
        kept + dropped,
        2 * DATAGRAMS,
        "{DATAGRAMS} datagrams over loopback are {} frames; the capture kept {kept} and admits \
         to {dropped}, which leaves {} unaccounted for",
        2 * DATAGRAMS,
        2 * DATAGRAMS - kept.min(2 * DATAGRAMS) - dropped.min(2 * DATAGRAMS),
    );

    // The loss has to reach the person, not merely the structure. Both states
    // the ring went through are named: running out of blocks is the moment
    // before a loss, and being full is the loss.
    let reasons = result["losses"]["reasons"]
        .as_object()
        .expect("the reasons behind the count");
    assert!(
        reasons
            .keys()
            .any(|reason| reason.contains("ring was full")),
        "the drops were counted and never explained: {reasons:?}"
    );
}

/// A `snapshot` written beside a polled request is still a single read.
///
/// The snapshot used to be a property of the *session*: the runner set it only
/// when every request was one, so a snapshot written next to a poll was read on
/// every tick of the shared window and became a series. The output was not a
/// lie -- `stopped_because` said what happened -- but the request's own mode
/// had been reinterpreted without a word, which is defect 4 of
/// `docs/scenarios/net.md`. Twenty-four entries across seven families declare
/// both modes, so every one of them was a session away from the same silence.
#[test]
fn a_snapshot_beside_a_poll_is_still_a_single_read() {
    let scratch = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("net-mixed-modes");
    std::fs::create_dir_all(&scratch).expect("a directory to write a session into");
    let path = scratch.join("a-snapshot-beside-a-poll.yml");
    std::fs::write(
        &path,
        "format: json\n\
         duration: 500ms\n\
         requests:\n\
         \x20 - label: polled\n\
         \x20   source: net.iface\n\
         \x20   target: {iface: lo}\n\
         \x20   mode: poll@50ms\n\
         \x20   emit: [raw]\n\
         \x20 - label: snapshotted\n\
         \x20   source: net.iface\n\
         \x20   target: {iface: lo}\n\
         \x20   mode: snapshot\n\
         \x20   emit: [raw]\n",
    )
    .expect("the test owns this file");

    let outcome = omn(&["-f", &path.to_string_lossy(), "--format", "json"]);
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let document = results(&outcome);
    let seen = |label: &str| {
        document["results"]
            .as_array()
            .expect("a run carries results")
            .iter()
            .find(|result| result["label"] == label)
            .unwrap_or_else(|| panic!("nothing in this session is labelled `{label}`"))
            ["events_seen"]
            .as_u64()
            .expect("an event count")
    };

    // One read of one interface, which is one event per counter kind -- the
    // seven series `net.iface` keeps apart, `missed` among them on a machine
    // whose sysfs has it for lo.
    let missed = std::path::Path::new("/sys/class/net/lo/statistics/rx_missed_errors").exists();
    assert_eq!(
        seen("snapshotted"),
        if missed { 7 } else { 6 },
        "`snapshot` means one read, and the request beside it does not change that"
    );
    assert!(
        seen("polled") >= 5,
        "and the polled request keeps its own 50 ms over a 500 ms window: {document}"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

/// A poll that cannot keep its own cadence has to say so.
///
/// A series is a claim about spacing as much as about values. Ten microseconds
/// is short enough that no machine can read a procfs file that fast, so the
/// scenario is the same everywhere and needs nothing of the machine it runs on:
/// essentially every tick must be late, and the run must count them rather than
/// present a series claiming a spacing it never had.
#[test]
fn a_poll_faster_than_its_own_read_counts_every_late_tick() {
    let outcome = omn(&["-f", &session("net-late-ticks.yml"), "--format", "json"]);
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let result = &results(&outcome)["results"][0];

    let reads = result["events_seen"].as_u64().expect("an event count");
    let late = result["losses"]["late_ticks"]
        .as_u64()
        .expect("a late tick count");
    assert!(reads > 10, "a 50 ms run at 10 us produced {reads} reads");
    assert!(
        late * 10 >= reads * 9,
        "{reads} reads of /proc/net/dev at a ten-microsecond cadence, and only {late} of them \
         were counted late; a read that fast is not possible and the ones that were not counted \
         are a series claiming a spacing it did not have"
    );

    // One reason, naming the cadence that was missed, carrying the count it
    // explains. It used to be one reason per late tick, because the duration
    // was in the key: five hundred lines for a fifty-millisecond run, growing
    // with the loss, which is defect 2 of docs/scenarios/net.md.
    let reasons = result["losses"]["reasons"]
        .as_object()
        .expect("the reasons behind the count");
    assert_eq!(
        reasons.len(),
        1,
        "{late} late ticks were explained in {} reasons: {reasons:?}",
        reasons.len()
    );
    let (reason, count) = reasons.iter().next().expect("just counted one");
    assert!(
        reason.contains("outran the 10.0us tick"),
        "a late tick blamed on something other than the cadence: {reason}"
    );
    assert_eq!(
        count.as_u64(),
        Some(late),
        "the reason has to carry the count it explains: {reasons:?}"
    );
    // And the measurement it used to be keyed by is beside it, where a reader
    // can ask how late the ticks were rather than reading them one by one.
    let overrun = &result["losses"]["overrun"];
    assert_eq!(overrun["tick_ns"].as_u64(), Some(10_000));
    assert!(
        overrun["worst_ns"].as_u64().unwrap_or_default() > 10_000,
        "a tick counted as late has to have overrun the tick: {overrun}"
    );

    // Nothing else is claimed. A tick that ran late lost no event -- the read
    // happened, it was simply not on time -- and saying otherwise would send a
    // reader looking for data that is all there.
    assert_eq!(result["losses"]["dropped"].as_u64(), Some(0));
    assert_eq!(result["losses"]["failed_reads"].as_u64(), Some(0));
}

/// A target that disappears mid-run ends the run by name.
///
/// An interface can be removed while something is watching it, and the two
/// wrong answers are both available: carry on reporting the last value, or stop
/// with a message about a file. The run has to say which interface went and how
/// much of the window it actually observed, because a series that is three
/// fifths as long as it was asked for is otherwise indistinguishable from one
/// that was quiet.
#[test]
fn an_interface_that_disappears_mid_run_ends_the_run_by_name() {
    const NAME: &str = "an_interface_that_disappears_mid_run_ends_the_run_by_name";

    if !inside_a_namespace() {
        if !iproute2_is_installed() {
            eprintln!("`{NAME}` needs `ip` from iproute2 to make a link it can then remove");
            return;
        }
        let _served = rerun_inside_a_namespace(NAME);
        return;
    }

    let made = shell("ip link add veth0 type veth peer name veth1 && ip link set veth0 up");
    if !made.status.success() {
        eprintln!(
            "`{NAME}` needs the veth driver, which this kernel does not offer: {}",
            stderr(&made)
        );
        return;
    }

    let measuring = Command::new(BINARY)
        .args([
            "net.bytes",
            "--iface",
            "veth0",
            "--mode",
            "poll@100ms",
            "--duration",
            "2s",
            "--emit",
            "timeline",
            "--format",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");

    pause(600);
    let removed = shell("ip link del veth0");
    assert!(removed.status.success(), "{}", stderr(&removed));

    let outcome = measuring.wait_with_output().expect("omn should finish");
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let result = &results(&outcome)["results"][0];

    let reason = result["stopped_because"].as_str().expect("a reason");
    assert!(
        reason.contains("veth0"),
        "the run ended because its interface went and did not say which: {reason}"
    );

    let observed = result["observed_ns"].as_u64().expect("a window");
    assert!(
        observed < 2_000_000_000,
        "the interface went after 600 ms and the run claims to have watched it for {observed} ns"
    );
    assert!(
        observed > 300_000_000,
        "the run stopped after {observed} ns, before the interface was removed"
    );
}

// ---------------------------------------------------------------------------
// Class D -- the refusal
// ---------------------------------------------------------------------------

/// A request that cannot mean anything is refused with the axis that is wrong.
///
/// Not merely refused: a refusal that sends a reader to the wrong flag costs
/// more than no message at all, so each of these has to name the axis and say
/// what the source does accept.
///
/// The four are asked twice, because the two ways of asking are different
/// answers to a reader. One at a time, a run refuses the one mistake it was
/// given. All four in a session, the validator answers for every one of them at
/// once -- five refusals, because the second request is wrong on two axes --
/// rather than turning four mistakes into four runs.
///
/// That session is written here rather than shipped in `examples/`, because
/// `tests/cli.rs` holds the invariant that every example in that directory
/// validates, and a file whose purpose is to be refused cannot. Its text is in
/// `docs/scenarios/net.md` for a reader who wants to run it.
#[test]
fn a_net_request_that_cannot_mean_anything_is_refused_with_the_axis_that_is_wrong() {
    // Each request, and the words the refusal has to contain.
    let refused: &[(&[&str], &str)] = &[
        (
            // The accept queue belongs to the machine, not to a link.
            &[
                "net.backlog",
                "--iface",
                "lo",
                "--mode",
                "count",
                "--emit",
                "rate",
            ],
            "`net.backlog` cannot be measured per iface",
        ),
        (
            // A counter is read, not sampled.
            &[
                "net.bytes",
                "--iface",
                "lo",
                "--mode",
                "sample@997hz",
                "--emit",
                "hist",
            ],
            "`net.bytes` cannot be observed in `sample` mode",
        ),
        (
            // A capture is frames; there is no single number to summarise them
            // into.
            &[
                "net.capture",
                "--iface",
                "lo",
                "--mode",
                "trace",
                "--emit",
                "summary",
            ],
            "`net.capture` cannot emit `summary`",
        ),
    ];

    for (how, expected) in refused {
        let mut args = how.to_vec();
        args.extend_from_slice(&["--duration", "200ms"]);
        let outcome = omn(&args);
        assert_eq!(
            code(&outcome),
            2,
            "`{}` exited {} rather than 2",
            how.join(" "),
            code(&outcome)
        );
        assert!(
            stderr(&outcome).contains(expected),
            "the refusal did not say `{expected}`:\n{}",
            stderr(&outcome)
        );
        assert!(
            stderr(&outcome).contains("hint:"),
            "a refusal that does not say what the source does accept sends a reader guessing: {}",
            stderr(&outcome)
        );
    }

    // The same five as one session. The validator answers before anything is
    // attached, and answers for all of them.
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("net-refusals.yml");
    std::fs::write(
        &path,
        "name: four requests that cannot mean anything\n\
         duration: 1s\n\
         format: text\n\
         requests:\n\
         \x20 - {label: queue-per-link, source: net.backlog, target: {iface: lo}, mode: count, emit: [rate]}\n\
         \x20 - {label: sampled-counter, source: net.bytes, target: {iface: lo}, mode: sample@997hz, emit: [hist], keys: [iface]}\n\
         \x20 - {label: summarised-capture, source: net.capture, target: {iface: lo}, mode: trace, emit: [summary], keys: [iface]}\n\
         \x20 - {label: counted-histogram, source: net.bytes, target: {iface: lo}, mode: count, emit: [hist], keys: [iface]}\n",
    )
    .expect("write the session");
    let outcome = omn(&["validate", "-f", path.to_str().expect("a path")]);
    assert_eq!(code(&outcome), 2, "{}", stdout(&outcome));
    let complaint = stderr(&outcome);
    assert!(
        complaint.matches("error:").count() >= 5,
        "only {} refusals were reported for four bad requests; a validator that stops \
         at the first turns one session into five runs:\n{complaint}",
        complaint.matches("error:").count()
    );
    std::fs::remove_file(&path).ok();

    // And the one refusal that is not the validator's, because it depends on
    // the backend the resolver chose: a filter on a field this backend does not
    // carry, answered with the backend that does.
    let outcome = omn(&[
        "net.bytes",
        "--iface",
        "lo",
        "--mode",
        "count",
        "--duration",
        "200ms",
        "--emit",
        "rate",
        "--filter",
        "comm == 'curl'",
    ]);
    assert_ne!(code(&outcome), 0);
    let complaint = stderr(&outcome);
    assert!(
        complaint.contains("comm") && complaint.contains("bpf:fentry"),
        "a filter the selected backend cannot carry should name the one that can: {complaint}"
    );
}

/// A machine that cannot serve a request names the privilege, not the build.
///
/// Two of them, and they are different. `net.capture` is written, and this
/// machine will not let this user open a packet socket: that is a privilege to
/// grant. `net.hwts --iface lo` asks for the offset between an interface's own
/// clock and ours, and loopback has no clock: no privilege makes that true, so
/// the message has to say where loopback's timestamps come from instead.
#[test]
fn a_machine_that_cannot_serve_a_net_request_names_what_it_is_missing() {
    let outcome = omn(&[
        "net.capture",
        "--iface",
        "lo",
        "--mode",
        "trace",
        "--duration",
        "200ms",
        "--emit",
        "raw",
    ]);
    if code(&outcome) == 0 {
        // A machine that grants CAP_NET_RAW serves it, and has nothing to say.
        return;
    }
    let complaint = stderr(&outcome);
    assert!(
        complaint.contains("CAP_NET_RAW"),
        "the refusal should name the capability: {complaint}"
    );
    assert!(
        complaint.contains("privilege to grant") && complaint.contains("the request is valid"),
        "the refusal should separate a machine's limit from a bad request: {complaint}"
    );
    // The code says what the sentence beside it says. It used to be 2 -- the
    // code for a request that cannot mean anything -- which is defect 8 of
    // docs/scenarios/net.md, and the distinction is the one that matters to a
    // caller that never reads the message: raising a privilege fixes this one
    // and nothing fixes the other.
    assert_eq!(
        code(&outcome),
        1,
        "a valid request this machine cannot serve has its own exit code: {complaint}"
    );

    let outcome = omn(&[
        "net.hwts", "--iface", "lo", "--mode", "snapshot", "--emit", "summary",
    ]);
    assert_ne!(code(&outcome), 0);
    let complaint = stderr(&outcome);
    assert!(
        complaint.contains("CLOCK_REALTIME"),
        "an interface with no clock of its own should be told where its timestamps do come \
         from, not merely refused: {complaint}"
    );
    assert!(
        !stdout(&outcome).contains("offset_ns"),
        "no offset may be presented for an interface that has none"
    );
}

/// A source this build does not collect exits 3 and names the backend.
///
/// Eight of the fifteen `net` sources have no collector, and the answer for
/// them is neither a failure nor a rejection: the request resolves, it
/// validates, it dumps, and nothing in this build can serve it. A caller that
/// cannot tell that from exit 1 raises privileges against a collector nobody
/// has written.
///
/// Which of the eight can demonstrate that depends on the machine, and the
/// scenario says so rather than assuming. A source is exit 3 only where its
/// best backend is one the machine serves; where the machine cannot serve any
/// of them, exit 1 is the honest answer and this scenario is not what proves
/// it. So the session asks for the two `fentry` sources, whose requirement is
/// BTF, and the sweep below requires each source to give one of the two answers
/// with the message that belongs to it -- never the third answer, which would
/// say the request cannot mean anything.
#[test]
fn a_net_source_this_build_does_not_collect_exits_three_and_names_the_backend() {
    let outcome = omn(&["-f", &session("net-not-built.yml")]);
    assert_eq!(code(&outcome), 3, "{}", stderr(&outcome));
    let complaint = stderr(&outcome);
    assert!(
        complaint.contains("net.tcp.connect") && complaint.contains("bpf:kprobe"),
        "the refusal should name the source and the backend it was pinned to: {complaint}"
    );
    assert!(
        complaint.contains("The request itself is valid"),
        "exit 3 is not a failure of the request and should not read like one: {complaint}"
    );

    // Each of the three left, on its own, for the message rather than the code.
    //
    // Five sources have come off this list: `net.tcp.lifetime`,
    // `net.tcp.retransmit`, `net.drops`, `net.qdisc` and `net.softirq` all
    // collect now, through the `bpf:raw_tp` backend the catalog pass put ahead
    // of their classic one, and the eighth wave took two more off it:
    // `net.tcp.connect` and `net.socket` collect through `bpf:fentry`. What is
    // left is their kprobe backend, pinned, and `net.xdp`, which needs an XDP
    // program -- the same shape of gap, one mechanism further on.
    let unbuilt: &[(&str, &str, &str, Option<&str>)] = &[
        (
            "net.tcp.connect",
            "trace",
            "bpf:kprobe",
            Some("force:bpf:kprobe"),
        ),
        (
            "net.socket",
            "trace",
            "bpf:kprobe",
            Some("force:bpf:kprobe"),
        ),
        ("net.xdp", "count", "bpf", None),
    ];
    for (source, mode, backend, pinned) in unbuilt {
        let mut args: Vec<&str> = vec![source];
        if *source == "net.xdp" {
            args.extend_from_slice(&["--iface", "lo"]);
        } else {
            args.push("--system");
        }
        if let Some(pinned) = pinned {
            args.extend_from_slice(&["--backend", pinned]);
        }
        let emit = if *mode == "count" { "rate" } else { "raw" };
        args.extend_from_slice(&["--mode", mode, "--duration", "200ms", "--emit", emit]);
        let outcome = omn(&args);
        let complaint = stderr(&outcome);
        match code(&outcome) {
            3 => assert!(
                complaint.contains("no collector is written"),
                "`{source}` exited 3, which is this build's gap, and must say so: {complaint}"
            ),
            1 => assert!(
                complaint.contains("has no viable backend on this machine"),
                "`{source}` exited 1, which is this machine's gap, and must say so: {complaint}"
            ),
            other => panic!(
                "`{source}` exited {other}: an unwritten collector is the build's gap or the \
                 machine's, never a request that cannot mean anything:\n{complaint}"
            ),
        }
        assert!(
            complaint.contains(backend),
            "`{source}` should name `{backend}` either way: {complaint}"
        );
    }
}

// ---------------------------------------------------------------------------
// Ground truth: numbers chosen outside the tool
// ---------------------------------------------------------------------------

/// An injected round trip is the number the stack reports.
///
/// This is the strongest statement this family can make about its own numbers.
/// A network emulator on loopback delays every packet by twenty-five
/// milliseconds in each direction, so a round trip is fifty -- a number chosen
/// in this test, produced by the kernel's queueing discipline, and reached by
/// `net.tcp.rtt` through a netlink dump that knows nothing about any of it.
///
/// Both modes the source declares are asked, over one window, and both have to
/// land on the same fifty milliseconds.
#[test]
fn an_injected_round_trip_is_the_number_the_stack_reports() {
    const NAME: &str = "an_injected_round_trip_is_the_number_the_stack_reports";
    /// What the emulator adds in each direction.
    const ONE_WAY_MS: f64 = 25.0;
    const PORT: u16 = 40123;

    if !inside_a_namespace() {
        if !iproute2_is_installed() {
            eprintln!("`{NAME}` needs `tc` from iproute2 to inject a delay it can then look for");
            return;
        }
        let _served = rerun_inside_a_namespace(NAME);
        return;
    }
    bring_loopback_up();

    let emulated = shell("tc qdisc add dev lo root netem delay 25ms");
    if !emulated.status.success() {
        eprintln!(
            "`{NAME}` needs the netem queueing discipline, which this kernel does not offer: {}",
            stderr(&emulated)
        );
        return;
    }

    // A connection that keeps talking, so the kernel keeps an estimate for it.
    // Nothing here measures the delay; the connection only has to exist.
    let chatter = std::thread::spawn(|| {
        let listener = TcpListener::bind(("127.0.0.1", PORT)).expect("bind the chosen port");
        let mut client = TcpStream::connect(("127.0.0.1", PORT)).expect("connect");
        let (mut server, _) = listener.accept().expect("accept");
        let mut buffer = [0u8; 64];
        for _ in 0..40 {
            client.write_all(&[b'x'; 64]).expect("write");
            server.read_exact(&mut buffer).expect("read");
            server.write_all(&[b'y'; 64]).expect("write back");
            client.read_exact(&mut buffer).expect("read back");
            std::thread::sleep(Duration::from_millis(50));
        }
    });

    // Two round trips have to have happened before the kernel has an estimate.
    pause(1500);
    let outcome = omn(&["-f", &session("net-injected-rtt.yml"), "--format", "json"]);
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);

    for label in ["polled", "traced"] {
        let result = labelled(&run, label);
        let series = result["outputs"]
            .as_array()
            .expect("outputs")
            .iter()
            .find(|output| output["emit"] == "summary")
            .expect("a summary")["series"]
            .as_array()
            .expect("series");
        assert_eq!(
            series.len(),
            1,
            "the filter names one port and the run found {} connections on it",
            series.len()
        );
        let mean_ns = series[0]["mean"].as_f64().expect("a mean round trip");
        let mean_ms = mean_ns / 1e6;
        assert!(
            (mean_ms - 2.0 * ONE_WAY_MS).abs() < 5.0,
            "`{label}` reported a {mean_ms:.2} ms round trip over a path this test delayed by \
             {ONE_WAY_MS} ms in each direction"
        );
    }

    chatter.join().expect("the chatter");
}

/// Every frame a down peer refuses is counted as a drop, and none as an error.
///
/// A veth pair whose far end was never brought up drops everything handed to
/// it, and a static neighbour entry keeps address resolution out of the way, so
/// the count of drops is the count of datagrams this test sent. Exactly --
/// there is nothing else on that link.
///
/// The second half is the family's own rule: a bad frame and a full queue are
/// different failures, counted apart everywhere, and nothing here was
/// malformed.
#[test]
fn every_frame_a_down_peer_refuses_is_counted_as_a_drop_and_none_as_an_error() {
    const NAME: &str = "every_frame_a_down_peer_refuses_is_counted_as_a_drop_and_none_as_an_error";
    const DATAGRAMS: u64 = 1000;

    if !inside_a_namespace() {
        if !iproute2_is_installed() {
            eprintln!("`{NAME}` needs `ip` from iproute2 to make a link whose far end is down");
            return;
        }
        let _served = rerun_inside_a_namespace(NAME);
        return;
    }

    let built = shell(
        "ip link add veth0 type veth peer name veth1 \
         && ip addr add 10.9.0.1/24 dev veth0 \
         && ip link set veth0 up \
         && ip neigh replace 10.9.0.2 dev veth0 nud permanent lladdr \
              $(ip -o link show veth1 | sed -n 's/.*link.ether \\([0-9a-f:]*\\).*/\\1/p')",
    );
    if !built.status.success() {
        eprintln!(
            "`{NAME}` needs the veth driver, which this kernel does not offer: {}",
            stderr(&built)
        );
        return;
    }

    let measuring = Command::new(BINARY)
        .args([
            "-f",
            &session("net-interface-drops.yml"),
            "--format",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");

    // After the baseline read, or the drops are in the interval nobody measures.
    pause(500);
    let sender = UdpSocket::bind("10.9.0.1:0").expect("a local address on the link");
    for _ in 0..DATAGRAMS {
        sender
            .send_to(&[b'x'; 100], ("10.9.0.2", 9))
            .expect("send into a link whose far end is down");
    }

    let outcome = measuring.wait_with_output().expect("omn should finish");
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let result = &results(&outcome)["results"][0];

    let mut drops = 0u64;
    let mut errors = 0u64;
    for event in result["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .find(|output| output["emit"] == "raw")
        .expect("the counters themselves")["events"]
        .as_array()
        .expect("events")
    {
        let attrs = event["attrs"].as_array().expect("attributes");
        let text = |name: &str| {
            attrs
                .iter()
                .find(|attr| attr["name"] == name)
                .and_then(|attr| attr["value"].as_str())
                .unwrap_or_else(|| panic!("no `{name}` on {event}"))
                .to_owned()
        };
        let number = |name: &str| {
            attrs
                .iter()
                .find(|attr| attr["name"] == name)
                .and_then(|attr| attr["value"].as_u64())
                .unwrap_or_else(|| panic!("no `{name}` on {event}"))
        };
        assert_eq!(
            text("iface"),
            "veth0",
            "the filter keeps veth0 alone: {event}"
        );
        match text("counter").as_str() {
            "drops" => {
                drops += number("frames");
                assert_eq!(number("frames"), number("rx") + number("tx"));
            }
            "errors" => errors += number("frames"),
            _ => {}
        }
    }

    assert_eq!(
        drops, DATAGRAMS,
        "{DATAGRAMS} datagrams were handed to a link whose far end is down, and the counters \
         report {drops} drops"
    );
    assert_eq!(
        errors, 0,
        "nothing sent here was malformed, and the link reports {errors} errors; a drop counted \
         as an error sends a reader looking for a cable"
    );

    // And every aggregate reaches them now: each counter kind is its own
    // series, so the rate over the drops series is the datagrams, exactly,
    // and the errors series beside it is zero. It used to be one series whose
    // value was errors, and a link that had just lost a thousand frames read
    // `0 count/s` (net 5 in `docs/scenarios/net.md`).
    assert_eq!(
        rate_total(result, "iface:veth0/drops"),
        DATAGRAMS as f64,
        "the rate over veth0's drops"
    );
    assert_eq!(rate_total(result, "iface:veth0/errors"), 0.0);
}

/// A snapshot of the accept queue reports the counters the kernel holds.
///
/// The workload is the ground truth and it is unambiguous: a listener one deep,
/// and far more connections than it will ever accept. `count` finds the drops
/// over its window. The same source in `snapshot` mode used to report zero --
/// a counter source reports the increment between two reads, and a snapshot's
/// two reads were a hundred microseconds apart, so the answer was structurally
/// zero however hard the queue was being hammered.
///
/// A snapshot now reports what a single read can honestly report: the counters
/// as they stand, under names that say so. The check is two-sided, and neither
/// side is the tool's own arithmetic -- the totals since boot must contain the
/// drops `count` measured over its second, and they must be far larger than any
/// increment over the snapshot's own microsecond window could be.
#[test]
fn a_snapshot_of_the_accept_queue_reports_the_queue_the_kernel_holds() {
    let (listener, port) = listener_with_backlog(1);
    let overflowing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let keep_going = overflowing.clone();

    // Connections nobody will ever accept, for as long as this test runs. Each
    // is bounded: once the queue is full the kernel stops answering, and a
    // plain connect then waits out the SYN retransmissions, which is minutes.
    //
    // A connect every other millisecond, rather than as fast as the loop can
    // go. A one-deep queue overflows on the second one either way, and a thread
    // spinning through a whole suite run is cpu the scheduler and budget
    // families are measuring at the same time.
    let hammering = std::thread::spawn(move || {
        let address: std::net::SocketAddr =
            format!("127.0.0.1:{port}").parse().expect("an address");
        let mut held = Vec::new();
        while keep_going.load(std::sync::atomic::Ordering::Relaxed) {
            if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_millis(20)) {
                held.push(stream);
            }
            if held.len() > 256 {
                held.drain(..128);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    });

    let counted = omn(&[
        "net.backlog",
        "--system",
        "--mode",
        "count",
        "--duration",
        "1s",
        "--emit",
        "summary",
        "--format",
        "json",
    ]);
    assert_eq!(code(&counted), 0, "{}", stderr(&counted));
    let over_a_second = results(&counted)["results"][0]["outputs"][0]["series"]
        .as_array()
        .expect("a series for the machine")
        .iter()
        .find(|series| series["key"] == "system")
        .expect("the machine's own series")["max"]
        .as_f64()
        .unwrap_or(0.0);
    assert!(
        over_a_second > 0.0,
        "a one-deep accept queue was hammered for a second and `count` saw no drops at all, so \
         the workload is not doing what this test assumes"
    );

    let snapshot = omn(&[
        "net.backlog",
        "--system",
        "--mode",
        "snapshot",
        "--emit",
        "raw",
        "--format",
        "json",
    ]);
    assert_eq!(code(&snapshot), 0, "{}", stderr(&snapshot));
    let result = &results(&snapshot)["results"][0];
    let window_ns = result["observed_ns"].as_u64().expect("a window");
    let event = &result["outputs"][0]["events"][0];
    let attr = |name: &str| {
        event["attrs"]
            .as_array()
            .expect("attributes")
            .iter()
            .find(|attr| attr["name"] == name)
            .and_then(|attr| attr["value"].as_u64())
    };
    let drops = attr("drops_since_boot").expect("a snapshot names what it reports");

    overflowing.store(false, std::sync::atomic::Ordering::Relaxed);
    hammering.join().expect("the hammering");
    drop(listener);

    // A snapshot is still one read: the window it observes is microseconds,
    // and that is the point -- there is no interval in it to report.
    assert!(
        window_ns < 5_000_000,
        "a snapshot observed {window_ns} ns, which is not the microsecond window this is about"
    );
    assert!(
        attr("drops").is_none(),
        "an interval's name on a number that is not one is the lie this fixed: {event}"
    );

    // The kernel's own counter is the reference. The drops `count` measured
    // over its second happened before this read, so the totals since boot
    // contain them...
    assert!(
        drops as f64 >= over_a_second,
        "`count` measured {over_a_second} drops over its second and the snapshot taken after \
         it reports {drops} since boot, which cannot contain them"
    );
    // ...and they are far more than any increment over a microsecond window
    // could be, which is what the old answer was and why it was always zero.
    let an_increment_could_be = over_a_second * (window_ns as f64 / 1e9) + 2.0;
    assert!(
        drops as f64 > an_increment_could_be,
        "the snapshot reported {drops}, which an increment over its own {window_ns} ns window \
         could account for; it is supposed to be reporting the counter, not an interval"
    );
}

/// Builds a listener with a chosen accept-queue depth.
///
/// `TcpListener::bind` asks for a backlog of 128, which is far too deep to
/// overflow on purpose, so the socket is made by hand.
fn listener_with_backlog(backlog: i32) -> (OwnedListener, u16) {
    // SAFETY: each call is checked, and the descriptor is owned by the returned
    // value from the moment it exists.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());

        let mut address: libc::sockaddr_in = std::mem::zeroed();
        address.sin_family = libc::AF_INET as libc::sa_family_t;
        address.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
        address.sin_port = 0; // the kernel chooses

        let bound = libc::bind(
            fd,
            (&raw const address).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );
        assert_eq!(bound, 0, "bind: {}", std::io::Error::last_os_error());
        assert_eq!(
            libc::listen(fd, backlog),
            0,
            "listen: {}",
            std::io::Error::last_os_error()
        );

        let mut assigned: libc::sockaddr_in = std::mem::zeroed();
        let mut length = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        assert_eq!(
            libc::getsockname(
                fd,
                (&raw mut assigned).cast::<libc::sockaddr>(),
                &mut length
            ),
            0
        );
        let port = u16::from_be(assigned.sin_port);
        (OwnedListener(fd), port)
    }
}

/// Closes the listening socket when the test is done with it.
struct OwnedListener(i32);

impl Drop for OwnedListener {
    fn drop(&mut self) {
        // SAFETY: closing a descriptor this value owns, once.
        unsafe { libc::close(self.0) };
    }
}

/// The filter fields this family declares, against a connection this test made.
///
/// The ground truth is the port: the kernel handed it out, this test knows it,
/// and no other socket on the machine has it. So `dport == <port>` has to
/// select exactly one connection, and every event it keeps has to carry that
/// port.
///
/// The address has to work the same way. `daddr` used to carry the address
/// *and* the port -- `127.0.0.1:40123` -- so an equality against an address
/// matched nothing, forever, and the run reported having kept zero of six
/// hundred events without saying why (net 6 in `docs/scenarios/net.md`). It is
/// the address alone now, and the equality has to find at least the
/// connection the port found.
#[test]
fn a_filter_on_a_declared_field_selects_the_connection_the_scenario_made() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local address").port();
    let mut client = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let (mut server, _) = listener.accept().expect("accept");
    // One exchange, so the kernel has a round-trip estimate to report.
    client.write_all(b"ping").expect("write");
    let mut buffer = [0u8; 4];
    server.read_exact(&mut buffer).expect("read");
    server.write_all(b"pong").expect("write back");
    client.read_exact(&mut buffer).expect("read back");

    let kept = |filter: &str| -> serde_json::Value {
        let outcome = omn(&[
            "net.tcp.rtt",
            "--system",
            "--mode",
            "poll@100ms",
            "--duration",
            "250ms",
            "--emit",
            "raw",
            "--filter",
            filter,
            "--format",
            "json",
        ]);
        assert_eq!(code(&outcome), 0, "`{filter}`: {}", stderr(&outcome));
        results(&outcome)["results"][0].clone()
    };

    let by_port = kept(&format!("dport == {port}"));
    let events = by_port["outputs"][0]["events"]
        .as_array()
        .expect("the events the filter kept")
        .clone();
    assert!(
        !events.is_empty(),
        "a connection on port {port} was open for the whole window and the filter kept nothing"
    );
    for event in &events {
        let dport = event["attrs"]
            .as_array()
            .expect("attributes")
            .iter()
            .find(|attr| attr["name"] == "dport")
            .and_then(|attr| attr["value"].as_u64())
            .expect("a destination port");
        assert_eq!(
            dport,
            u64::from(port),
            "the filter kept an event for another connection: {event}"
        );
    }

    // The same connection, asked for by address: every loopback connection on
    // the machine is in this set, so it contains at least the one above.
    let by_address = kept("daddr == '127.0.0.1'");
    assert!(
        by_address["events_seen"].as_u64().unwrap_or(0) >= events.len() as u64,
        "an equality against the address found fewer connections than the port did: {}",
        by_address["events_seen"]
    );
    for event in by_address["outputs"][0]["events"]
        .as_array()
        .expect("the events the filter kept")
    {
        let daddr = event["attrs"]
            .as_array()
            .expect("attributes")
            .iter()
            .find(|attr| attr["name"] == "daddr")
            .and_then(|attr| attr["value"].as_str())
            .expect("a destination address");
        assert_eq!(daddr, "127.0.0.1", "the address alone, no port: {event}");
    }

    drop(client);
    drop(server);
}

/// The `trace` leg of the sweep, for the two sources that declare it outside a
/// namespace of their own.
///
/// `net.tcp.rtt` answers it: the netlink backend has no per-update hook, so it
/// reads at the window's cadence and the mode says what the numbers mean rather
/// than how they were obtained. Every emit `trace` adds over `poll` -- a
/// histogram of round trips -- has to come out.
///
/// `net.hwts` cannot: a timestamp per packet is a different backend and nobody
/// has written it. The interface backend declares `poll` and `snapshot` only,
/// so a `trace` request of an interface is refused at resolution, naming the
/// modes the backend does serve -- before the fifteenth wave it resolved onto
/// that backend and failed at the start, exit 3, which is what the matrix's
/// `here-caps` cell caught: a source listed usable whose first mode did not
/// run. The same request of a launched program resolves onto the per-packet
/// backend and is refused as not built, exit 3.
#[test]
fn the_trace_leg_of_the_sweep_answers_or_names_the_backend_it_would_need() {
    let outcome = omn(&[
        "net.tcp.rtt",
        "--system",
        "--mode",
        "trace",
        "--duration",
        "200ms",
        "--emit",
        "raw",
        "--emit",
        "hist",
        "--emit",
        "timeline",
        "--emit",
        "summary",
        "--format",
        "json",
    ]);
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let result = &results(&outcome)["results"][0];
    assert_eq!(emits(result), ["raw", "hist", "timeline", "summary"]);
    assert_eq!(result["backend"], "netlink");

    let outcome = omn(&[
        "net.hwts",
        "--iface",
        "lo",
        "--mode",
        "trace",
        "--duration",
        "200ms",
        "--emit",
        "raw",
    ]);
    let complaint = stderr(&outcome);
    assert!(
        // The resolver's wording, or the validator's once a (target, mode)
        // pair no backend declares is refused before resolution.
        complaint.contains("`netlink:ethtool` serves `poll`, `snapshot` mode, not `trace`")
            || complaint.contains("`netlink:ethtool` per iface, in poll, snapshot"),
        "the interface backend should be passed over for its modes, by name: {complaint}"
    );
    // Refused before anything starts: never exit 3 ("started, then found not
    // built"), which is the listing and the run disagreeing. 1 today; 2 once a
    // (target, mode) pair no backend declares is the request's (filed to P in
    // the fourteenth wave).
    assert!(
        matches!(code(&outcome), 1 | 2),
        "a trace of an interface is refused at resolution, not at the start: {complaint}"
    );

    let outcome = omn(&[
        "net.hwts",
        "--mode",
        "trace",
        "--duration",
        "200ms",
        "--emit",
        "raw",
        "--",
        "true",
    ]);
    let complaint = stderr(&outcome);
    // Its pinned twin: the same request with the per-packet backend forced
    // exits the same way the walk does, on either branch below. The walk said
    // "the machine" and the pin "the build" on a runner with no readable PTP
    // clock until the eighteenth wave (`DECISIONS.md`, "one order for unbuilt
    // and missing").
    let pinned = omn(&[
        "net.hwts",
        "--mode",
        "trace",
        "--duration",
        "200ms",
        "--emit",
        "raw",
        "--backend",
        "force:so_timestamping",
        "--",
        "true",
    ]);
    let pinned_complaint = stderr(&pinned);
    assert_eq!(
        code(&pinned),
        code(&outcome),
        "pinning the backend the walk refused exits another way:\nwalk: {complaint}\npin: \
         {pinned_complaint}"
    );
    // The per-packet backend needs a PTP hardware clock this process can
    // read, as well as a collector. Where none can be read the machine is the
    // first answer, exit 1, with the clock named against the backend -- the
    // CI's runners keep one, `/dev/ptp1`, root's alone. Ground truth for which
    // of the two this machine is: opening the character devices ourselves.
    if let Some(why) = no_readable_ptp_clock() {
        eprintln!(
            "note: {why}, so the per-packet backend is this machine's to refuse before it \
             is the build's"
        );
        assert!(
            complaint.contains("so_timestamping: phc: missing"),
            "the refusal should name the backend a per-packet answer needs and the clock it \
             lacks here: {complaint}"
        );
        assert_eq!(
            code(&outcome),
            1,
            "a machine with no readable PTP clock cannot serve it: {complaint}"
        );
        assert!(
            pinned_complaint.contains("so_timestamping (pinned with --backend force:")
                && pinned_complaint.contains("phc: missing"),
            "the pin names the clock it lacks here, as the walk does: {pinned_complaint}"
        );
        return;
    }
    assert!(
        complaint.contains("so_timestamping") && complaint.contains("no collector is written"),
        "the refusal should name the backend a per-packet answer needs: {complaint}"
    );
    // `DECISIONS.md` settled that "valid, and not built is exit 3, wherever it
    // is discovered" (defect 7 of docs/scenarios/net.md).
    assert_eq!(
        code(&outcome),
        3,
        "valid and not built is exit 3: {complaint}"
    );
    assert!(
        pinned_complaint.contains("so_timestamping")
            && pinned_complaint.contains("no collector is written"),
        "the pin is the build's gap where the walk's is: {pinned_complaint}"
    );
}

/// Why this process can read no interface's PTP hardware clock, or `None`
/// when it can read one.
///
/// The clocks are found the way sysfs links them to a card
/// (`/sys/class/net/<iface>/device/ptp/ptp<N>`), not the way the tool asks
/// (ethtool's netlink family), and each is opened here, outside the tool. A
/// clock no interface is wired to -- a hypervisor's -- stamps no packet and is
/// left out, as the tool leaves it out.
fn no_readable_ptp_clock() -> Option<String> {
    let mut clocks = Vec::new();
    for iface in std::fs::read_dir("/sys/class/net")
        .into_iter()
        .flatten()
        .flatten()
    {
        let wired = iface.path().join("device/ptp");
        for clock in std::fs::read_dir(&wired).into_iter().flatten().flatten() {
            clocks.push((iface.file_name(), clock.file_name()));
        }
    }
    if clocks.is_empty() {
        return Some("no interface here is wired to a PTP hardware clock".to_owned());
    }
    let mut refused = Vec::new();
    for (iface, clock) in &clocks {
        let device = std::path::Path::new("/dev").join(clock);
        match std::fs::File::open(&device) {
            Ok(_) => return None,
            Err(error) => refused.push(format!(
                "{} keeps {}: {error}",
                iface.to_string_lossy(),
                device.display()
            )),
        }
    }
    Some(format!(
        "no interface's PTP clock opens for reading ({})",
        refused.join("; ")
    ))
}

/// A source listed usable attaches in every mode that resolves.
///
/// The matrix's `here-caps` cell found `net.hwts` listed usable through
/// `netlink:ethtool` and refused at the start in `trace`, its first mode: the
/// resolver chose the interface backend for a mode only the per-packet one
/// makes. The listing, the resolver and the run have to agree, so this asks
/// every `net` source `omn list` calls ready here, in each of its modes and on
/// the machine (or `lo`, or a launched `sleep`), what the sweep asks: resolve
/// it, and if it resolves onto a backend this build collects, run it. A run
/// that resolved and then did not start is the defect, whatever the source.
///
/// The ground truth is the probe's own verdict, independent of the collector:
/// what it called ready has to start. One refusal is the target's and not the
/// machine's: `net.request` follows the probes a program carries, and `sleep`
/// carries none, which it says by name.
#[test]
fn every_usable_net_source_starts_in_every_mode_that_resolves() {
    let listing = omn(&["list", "--format", "json"]);
    assert_eq!(code(&listing), 0, "{}", stderr(&listing));
    let catalog: Vec<serde_json::Value> =
        serde_json::from_str(&stdout(&listing)).expect("the catalog is JSON");
    let strings = |value: &serde_json::Value| -> Vec<String> {
        value
            .as_array()
            .map(|all| {
                all.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut ran = 0;
    let mut ready = 0;
    let mut failures = Vec::new();
    for entry in catalog.iter().filter(|e| e["family"] == "net") {
        let id = entry["id"].as_str().expect("an id");
        if entry["here"]["availability"] != "ready" {
            continue;
        }
        ready += 1;
        // Every (target, mode) pair this source refused, and how many
        // resolved onto a backend this build collects. A source called ready
        // for which none does is the listing and the run disagreeing as much
        // as a run that fails to start, so it is a failure too.
        let mut refusals: Vec<String> = Vec::new();
        let mut resolved_here = 0;
        let collected: Vec<String> = entry["probes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|probe| probe["collector"] == true)
            .filter_map(|probe| probe["backend"].as_str().map(str::to_owned))
            .collect();
        let declared = entry["targets"].as_object().cloned().unwrap_or_default();
        let mut targets: Vec<Vec<&str>> = Vec::new();
        if declared.contains_key("system") {
            targets.push(vec!["--system"]);
        } else if declared.contains_key("iface") {
            targets.push(vec!["--iface", "lo"]);
        }
        if declared.contains_key("launch") {
            targets.push(vec!["--", "sleep", "0.3"]);
        }
        let emits = strings(&entry["emits"]);
        let emit = ["summary", "hist", "rate", "timeline", "topk", "raw"]
            .into_iter()
            .find(|e| emits.iter().any(|x| x == e))
            .unwrap_or("raw");
        for target in &targets {
            for mode in strings(&entry["modes"]) {
                let head = [id, "-m", &mode, "-e", emit, "-d", "300ms"];
                let resolve: Vec<&str> = head
                    .iter()
                    .copied()
                    .chain(["--dump-resolved", "--format", "json"])
                    .chain(target.iter().copied())
                    .collect();
                let resolved = omn(&resolve);
                if code(&resolved) != 0 {
                    refusals.push(format!(
                        "`-m {mode} {}` exited {}: {}",
                        target.join(" "),
                        code(&resolved),
                        stderr(&resolved).lines().next().unwrap_or_default()
                    ));
                    continue;
                }
                let resolved: serde_json::Value =
                    serde_json::from_str(&stdout(&resolved)).expect("the resolution is JSON");
                let backend = resolved[0]["backend"]["id"]
                    .as_str()
                    .or_else(|| resolved["backend"]["id"].as_str())
                    .unwrap_or_default()
                    .to_owned();
                if !collected.contains(&backend) {
                    refusals.push(format!(
                        "`-m {mode} {}` resolved onto `{backend}`, which this build does not collect",
                        target.join(" ")
                    ));
                    continue;
                }
                let run: Vec<&str> = head.iter().copied().chain(target.iter().copied()).collect();
                let outcome = omn(&run);
                ran += 1;
                resolved_here += 1;
                let complaint = stderr(&outcome);
                if code(&outcome) == 0
                    || (id == "net.request" && complaint.contains("declares no request"))
                {
                    continue;
                }
                failures.push(format!(
                    "omn {} resolved onto `{backend}` and exited {}:\n{}",
                    run.join(" "),
                    code(&outcome),
                    complaint.lines().next().unwrap_or_default()
                ));
            }
        }
        if resolved_here == 0 {
            failures.push(format!(
                "`{id}` is listed ready and no declared mode resolves onto a collected backend \
                 on {} target(s):\n  {}",
                targets.len(),
                refusals.join("\n  ")
            ));
        }
    }
    if ready == 0 {
        eprintln!("skipped: `omn list` calls no net source ready on this machine");
        return;
    }
    assert!(
        failures.is_empty(),
        "{} of {ran} runs a usable source resolved did not start:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

// ---------------------------------------------------------------------------
// The five sources that go through the kernel
// ---------------------------------------------------------------------------
//
// `net.tcp.lifetime`, `net.tcp.retransmit`, `net.drops`, `net.qdisc` and
// `net.softirq`, all over raw tracepoints. Every one of them needs `CAP_BPF`
// and `CAP_PERFMON`, which `make` puts on the binary and every `cargo build`
// takes off again; a machine that does not grant them cannot serve any of
// these and the tests say so and stop, which is the only thing allowed to stop
// one quietly.
//
// Three of them are measured against a property injected from outside the
// tool: a queueing discipline that delays every packet by a chosen amount, a
// link that drops everything, a count of datagrams sent somewhere nothing is
// listening. Two are measured against a counter the kernel keeps for its own
// reasons -- `/proc/softirqs` and `/proc/net/snmp` -- which is the strongest
// check available for a number nobody can inject.
//
// The namespaced scenarios run **`omn` outside** the namespace and the workload
// inside it, which is the opposite of the older ones in this file. A raw
// tracepoint is not namespaced, so a program loaded on the machine sees packets
// queued on a device this process cannot name -- and `bpf()` checks `CAP_BPF`
// against the initial user namespace, so a program cannot be loaded from inside
// an unprivileged one at all. Measuring from outside is the only arrangement
// that works, not a convenience.

/// Whether a run was refused for want of the privileges the eBPF backends need.
/// A machine's limit, and the only thing in this section allowed to stop a test
/// quietly.
/// The inode `/proc/self/fd` names a socket by, `socket:[n]`: the kernel's own
/// account, read by a route that shares nothing with the eBPF programs.
fn socket_inode(socket: &impl std::os::fd::AsRawFd) -> u64 {
    let link = std::fs::read_link(format!("/proc/self/fd/{}", socket.as_raw_fd()))
        .expect("a descriptor this process holds");
    let link = link.to_string_lossy();
    link.strip_prefix("socket:[")
        .and_then(|rest| rest.strip_suffix(']'))
        .and_then(|number| number.parse().ok())
        .unwrap_or_else(|| panic!("`{link}` is not a socket"))
}

fn unprivileged(text: &str) -> bool {
    text.contains("CAP_BPF") || text.contains("CAP_PERFMON")
}

/// Whether this machine will let an unprivileged user build a network of its
/// own, which three of these scenarios inject their ground truth with.
fn can_build_a_network() -> bool {
    iproute2_is_installed()
        && Command::new("unshare")
            .args(["--user", "--map-root-user", "--net", "--", "true"])
            .output()
            .is_ok_and(|outcome| outcome.status.success())
}

/// Runs a script inside a user and network namespace of its own, in the
/// background, and returns the child.
///
/// Unlike `rerun_inside_a_namespace`, nothing here is measured *inside* the
/// namespace: the script is the workload and `omn` watches it from outside.
fn workload_in_a_namespace(script: &str) -> std::process::Child {
    Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--net",
            "--",
            "sh",
            "-c",
            script,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("unshare should run")
}

/// Every attribute of an event, by name.
fn attrs(event: &serde_json::Value) -> BTreeMap<String, serde_json::Value> {
    event["attrs"]
        .as_array()
        .expect("attrs")
        .iter()
        .map(|attr| {
            (
                attr["name"].as_str().expect("a name").to_owned(),
                attr["value"].clone(),
            )
        })
        .collect()
}

/// The raw events of a labelled result.
fn raw_events<'a>(run: &'a serde_json::Value, label: &str) -> &'a Vec<serde_json::Value> {
    labelled(run, label)["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .find(|output| output["emit"] == "raw")
        .unwrap_or_else(|| panic!("no raw output in `{label}`"))["events"]
        .as_array()
        .expect("events")
}

/// One counter out of a `/proc/net/snmp` block, by its protocol and name.
///
/// The file is pairs of lines: a header naming the columns and a row of values
/// under it, which is why this cannot just split on whitespace and count.
fn snmp(protocol: &str, counter: &str) -> u64 {
    let text = std::fs::read_to_string("/proc/net/snmp").expect("/proc/net/snmp");
    let lines: Vec<&str> = text.lines().collect();
    for pair in lines.windows(2) {
        let (header, values) = (pair[0], pair[1]);
        if !header.starts_with(protocol) || !values.starts_with(protocol) {
            continue;
        }
        let names: Vec<&str> = header.split_whitespace().collect();
        let Some(at) = names.iter().position(|name| *name == counter) else {
            continue;
        };
        return values
            .split_whitespace()
            .nth(at)
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("`{protocol} {counter}` is not a number"));
    }
    panic!("/proc/net/snmp has no `{protocol} {counter}`")
}

/// How many times a softirq vector's handler has run since boot, over all cpus.
///
/// The row's position in the file is the vector number the tracepoint reports,
/// which is the same fact the collector reads; this reads it by name because a
/// test that hard-coded the number would be checking the tool against itself.
fn softirq_runs(vector: &str) -> u64 {
    let text = std::fs::read_to_string("/proc/softirqs").expect("/proc/softirqs");
    for line in text.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let Some(label) = fields.next() else { continue };
        if label.trim_end_matches(':') != vector {
            continue;
        }
        return fields.filter_map(|count| count.parse::<u64>().ok()).sum();
    }
    panic!("/proc/softirqs has no `{vector}` row")
}

// ------------------------------------------------------ Class A: the sweep

/// The `trace` leg of the sweep for the five sources that go through the
/// kernel: every emit each one declares, over one window, with traffic running.
///
/// What this catches is the promise nobody looks at -- a combination the
/// catalog offers and the build cannot serve. It is not about the numbers; each
/// source's own scenario below is.
#[test]
fn the_ebpf_leg_of_the_sweep_answers_with_every_emit_it_promised() {
    let measuring = Command::new(BINARY)
        .args(["-f", &session("net-bpf-sweep.yml"), "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");

    // The window is half a second and the programs attach first; the traffic
    // has to happen inside it or three of the five answer for an idle machine.
    //
    // Sixty-four kibibytes and no more, deliberately. What this leg needs is a
    // connection that opens and closes and a receive softirq that carries it,
    // not volume -- and `the_count_leg_of_the_sweep_finds_the_traffic_the_
    // scenario_pushed` is reading this machine's loopback counters at the same
    // time, in another thread of the same suite. A megabyte here is a megabyte
    // in its total.
    pause(150);
    push_through_loopback(64 * 1024);

    let outcome = measuring.wait_with_output().expect("omn should finish");
    if unprivileged(&stderr(&outcome)) {
        eprintln!(
            "skipped: every source in this sweep needs CAP_BPF and CAP_PERFMON, which this \
             machine does not grant"
        );
        return;
    }
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);

    let expected: &[(&str, &[&str])] = &[
        (
            "connections",
            &["hist", "topk", "raw", "timeline", "summary"],
        ),
        ("retransmits", &["rate", "topk", "raw", "summary"]),
        ("drops", &["rate", "topk", "raw", "summary"]),
        ("queueing", &["hist", "raw", "summary", "timeline"]),
        ("softirq", &["hist", "rate", "raw", "summary"]),
    ];
    for (label, wanted) in expected {
        let result = labelled(&run, label);
        assert_eq!(&emits(result), wanted, "`{label}` emitted the wrong set");
        assert_eq!(
            result["backend"], "bpf:raw_tp",
            "`{label}` was served by something other than the raw tracepoint path"
        );
        assert!(
            result["losses"].is_object(),
            "`{label}` reported no loss accounting at all"
        );
    }

    // Two of the five answer for any machine that moved a byte: four mebibytes
    // through loopback is a connection that opened and closed, and the receive
    // softirq that carried it. The other three are a machine's own business --
    // a quiet link retransmits nothing, drops nothing, and loopback has no
    // queueing discipline at all.
    for label in ["connections", "softirq"] {
        let seen = labelled(&run, label)["events_seen"]
            .as_u64()
            .expect("an event count");
        assert!(
            seen > 0,
            "`{label}` watched four mebibytes cross loopback and produced nothing"
        );
    }
}

/// The pairs the catalog implies and the mode x emit matrix refuses.
///
/// An entry lists its modes and its emits as two flat lists, so it promises
/// their cross product; the matrix is narrower than that, and the difference is
/// a promise nobody can collect. `net.softirq`'s histogram over a counted
/// window is a correct refusal -- one measurement per series is no
/// distribution -- and it is pinned so that another cannot arrive quietly.
///
/// Two more were recorded here while `count` could not rank: `topk` over
/// `net.tcp.retransmit` and `net.drops` in `count`. Stream R's thirteenth wave
/// lets it (`ModeKind::Count` admits `topk`), so the two are expected exactly
/// when the build's own matrix still refuses them, and the test holds on both
/// sides of that merge.
#[test]
fn the_pairs_these_entries_imply_and_the_matrix_refuses_are_the_ones_recorded() {
    use omn::catalog::Catalog;
    let catalog = Catalog::embedded().expect("the embedded catalog");
    let counting_ranks = catalog
        .get(&"net.drops".parse().expect("a source id"))
        .expect("net.drops is in the catalog")
        .modes
        .iter()
        .find(|mode| mode.as_str() == "count")
        .expect("net.drops counts")
        .valid_emits()
        .iter()
        .any(|emit| emit.as_str() == "topk");
    let mut recorded: Vec<(&str, &str, &str)> = Vec::new();
    if !counting_ranks {
        recorded.push(("net.tcp.retransmit", "count", "topk"));
        recorded.push(("net.drops", "count", "topk"));
    }
    // The stacks of drops are each drop's, which a counted window of totals
    // does not keep: `-e stack` goes with `trace`.
    recorded.push(("net.drops", "count", "stack"));
    recorded.push(("net.softirq", "count", "hist"));
    recorded.sort();

    for (source, mode, emit) in &recorded {
        let outcome = omn(&[
            source,
            "--system",
            "--mode",
            mode,
            "--duration",
            "150ms",
            "--emit",
            emit,
        ]);
        assert_eq!(
            code(&outcome),
            2,
            "`{source} --mode {mode} --emit {emit}` is refused as a request that cannot mean \
             anything, which is exit 2: {}",
            stderr(&outcome)
        );
        assert!(
            stderr(&outcome).contains(&format!("mode `{mode}` cannot produce `{emit}`")),
            "the refusal should name the pair: {}",
            stderr(&outcome)
        );
    }

    // And every other pair these five entries imply is one a request can
    // actually ask for. A new refusal appearing here is a promise the catalog
    // now makes and no request can collect.
    let mut refused = Vec::new();
    for id in [
        "net.tcp.lifetime",
        "net.tcp.retransmit",
        "net.drops",
        "net.qdisc",
        "net.softirq",
    ] {
        let entry = catalog
            .get(&id.parse().expect("a source id"))
            .unwrap_or_else(|| panic!("{id} is in the catalog"));
        for mode in &entry.modes {
            for emit in &entry.emits {
                if !mode.valid_emits().contains(emit) {
                    refused.push((id, mode.as_str().to_owned(), emit.as_str().to_owned()));
                }
            }
        }
    }
    let recorded: Vec<(&str, String, String)> = recorded
        .iter()
        .map(|(source, mode, emit)| (*source, (*mode).to_owned(), (*emit).to_owned()))
        .collect();
    refused.sort();
    assert_eq!(
        refused, recorded,
        "the set of pairs these entries imply and the matrix refuses changed; \
         docs/scenarios/net.md lists them"
    );
}

// ------------------------------------------- Class: ground truth, per source

/// A connection whose length, volume and local port the scenario chose, found
/// by both of its ends.
///
/// Three numbers come from the workload and nowhere else: it lives 300 ms, the
/// client writes exactly 4096 bytes, and the client's local port is read off
/// the socket. `net.tcp.lifetime` has to find all three.
///
/// The fourth check is the identity. A socket cookie is generated lazily -- the
/// first time anything asks the kernel for one -- so the session polls
/// `net.tcp.rtt` beside it, and a `sock_diag` dump is exactly the thing that
/// asks. Without it both events carry no `sock` key and can be joined to
/// nothing; with it the two sources name the same connection by the same
/// number, which is what this family's whole key discipline is for.
#[test]
fn a_connection_of_a_known_length_and_volume_is_found_by_both_of_its_ends() {
    const HELD_MS: u64 = 300;
    const SENT: usize = 4096;

    let measuring = Command::new(BINARY)
        .args([
            "-f",
            &session("net-connection-life.yml"),
            "--format",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");

    // Three connections in a row rather than one, and the check asks for any
    // of them. A connection that was already established when the programs
    // attached has no beginning for them to measure and produces no event at
    // all, so a single connection after a fixed pause is a test that passes
    // when the machine is idle and fails when it is busy -- which is what this
    // did. Every one of the three carries the same chosen properties, so
    // finding one of them is the same statement.
    pause(150);
    let mut made = Vec::new();
    for _ in 0..3 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let server_port = listener.local_addr().expect("local address").port();
        let sink = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = vec![0u8; 64 * 1024];
            let mut received = 0usize;
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => received += read,
                    Err(_) => break,
                }
            }
            received
        });
        let mut stream = TcpStream::connect(("127.0.0.1", server_port)).expect("connect");
        let client_port = stream.local_addr().expect("local address").port();
        let client_inode = socket_inode(&stream);
        stream.write_all(&vec![0x5au8; SENT]).expect("write");
        stream.flush().expect("flush");
        // The connection is established for this long and no less. The far end
        // closes as soon as it sees the client's EOF, so both sockets reach
        // TCP_CLOSE within a few milliseconds of each other.
        pause(HELD_MS);
        drop(stream);
        assert_eq!(sink.join().expect("the receiver"), SENT);
        made.push((client_port, server_port, client_inode));
    }

    let outcome = measuring.wait_with_output().expect("omn should finish");
    if unprivileged(&stderr(&outcome)) {
        eprintln!(
            "skipped: net.tcp.lifetime needs CAP_BPF and CAP_PERFMON, which this machine does \
             not grant"
        );
        return;
    }
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);

    // Any one of the three, with both of its ends.
    let closed: BTreeMap<u64, (&serde_json::Value, BTreeMap<String, serde_json::Value>)> =
        raw_events(&run, "connections")
            .iter()
            .filter_map(|event| {
                let attributes = attrs(event);
                attributes["sport"]
                    .as_u64()
                    .map(|port| (port, (event, attributes)))
            })
            .collect();
    let found = made
        .iter()
        .find(|(client, server, _)| {
            closed.contains_key(&u64::from(*client)) && closed.contains_key(&u64::from(*server))
        })
        .unwrap_or_else(|| {
            panic!(
                "none of the three connections the scenario opened on ports {made:?} is among \
                 the {} this run saw close: {:?}",
                closed.len(),
                closed.keys().collect::<Vec<_>>()
            )
        });
    let (client_event, client_attrs) = &closed[&u64::from(found.0)];
    let (_, server_attrs) = &closed[&u64::from(found.1)];

    // The length. The connection was held open for 300 ms after the transfer
    // and closed once; anything under that is a duration measured from the
    // wrong moment, and the ceiling is generous because the close is a
    // scheduling event on a machine running a test suite.
    let held_ns = client_attrs["duration_ns"].as_u64().expect("a duration");
    assert!(
        held_ns >= HELD_MS * 1_000_000,
        "the connection was held open for at least {HELD_MS} ms and this says {} ms",
        held_ns / 1_000_000
    );
    assert!(
        held_ns < 3 * HELD_MS * 1_000_000,
        "and not three times that: {} ms",
        held_ns / 1_000_000
    );

    // The volume, which is exact: `bytes_sent` counts payload, so it is the
    // number the workload wrote and not one byte more.
    assert_eq!(
        client_attrs["bytes_sent"].as_u64(),
        Some(SENT as u64),
        "the client sent exactly {SENT} bytes"
    );
    // And the other end received them, plus one: a FIN occupies a sequence
    // number, and `bytes_received` is the kernel's reading of the sequence
    // space rather than a count of payload the application saw.
    assert_eq!(
        server_attrs["bytes_received"].as_u64(),
        Some(SENT as u64 + 1),
        "the server received the payload and the FIN behind it"
    );
    assert_eq!(client_attrs["saddr"].as_str(), Some("127.0.0.1"));

    // The identity. `net.tcp.rtt` dumped the connection while it was alive,
    // which is what made the kernel generate a cookie for it at all.
    let keyed = client_event["keys"]
        .as_array()
        .and_then(|keys| keys.first())
        .and_then(|key| key["sock"]["cookie"].as_u64());
    assert_eq!(
        keyed,
        client_attrs["cookie"].as_u64(),
        "the event's key and its cookie attribute are the same number"
    );
    assert!(
        keyed.is_some_and(|cookie| cookie != 0),
        "a connection dumped by `net.tcp.rtt` during its life must carry the cookie that dump \
         generated, which is what a `sock:cookie` join is made of: {client_event}"
    );

    // The inode, against the kernel's own name for the client's socket: the
    // `socket:[n]` its descriptor showed while it was open. The client's end
    // had its file from before the handshake, so the inode read when the
    // connection was established is the one; the close is too late for it,
    // because `sock_orphan` clears it first. Newer kernels read it from
    // `sock.sk_ino`, 6.6 and older through `sk_socket` -- one assertion for
    // both, and the portability matrix is where the second runs.
    assert_eq!(
        client_attrs["inode"].as_u64(),
        Some(found.2),
        "the client's connection carries the inode its descriptor named: {client_event}"
    );

    // The peer, read out of `sock_common`'s anonymous unions: each end names
    // the other. The client's peer is the listener's port, the server's is the
    // client's, and both addresses are loopback.
    assert_eq!(client_attrs["daddr"].as_str(), Some("127.0.0.1"));
    assert_eq!(
        client_attrs["dport"].as_u64(),
        Some(u64::from(found.1)),
        "the client's peer is the port it connected to"
    );
    assert_eq!(
        server_attrs["dport"].as_u64(),
        Some(u64::from(found.0)),
        "the server's peer is the port the client connected from"
    );
}

/// Every datagram sent to a port nothing is listening on is a drop, with one
/// reason, and the kernel's own counter agrees.
///
/// Two independent checks on one workload. The count is chosen here -- five
/// hundred and twelve datagrams -- and `Udp: NoPorts` in `/proc/net/snmp` is
/// the kernel counting the same thing by another route. Both are lower bounds
/// rather than equalities, and deliberately: this machine is shared with a test
/// suite and whatever else is running, and a scenario that asserted an exact
/// total would be asserting what the machine was doing.
///
/// The contrast is the point of landing this source at all: `net.iface` watches
/// the same window and reports nothing. The frames arrived perfectly and the
/// stack discarded them, so a reader with only the device counters concludes
/// the machine lost nothing.
#[test]
fn every_datagram_sent_to_a_closed_port_is_a_drop_with_one_reason() {
    const SENT: u64 = 512;
    /// A port nothing on a developer machine binds, and nothing here binds
    /// either: the scenario needs the kernel to have no socket for it.
    const NOWHERE: u16 = 59321;

    // Up to three windows, because a window that lost records cannot answer
    // this question and a lossy one is not this scenario's fault: another test
    // in this suite floods a socket until its receive buffer overflows, and
    // twenty thousand of its drops in one tick fill the ring this reads. The
    // run says when that happened, so the answer is to take another window
    // rather than to assert a count that is known to be short.
    let mut attempts = Vec::new();
    for _ in 0..3 {
        let before = snmp("Udp", "NoPorts");
        let measuring = Command::new(BINARY)
            .args(["-f", &session("net-closed-port.yml"), "--format", "json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("omn should run");

        pause(200);
        let sender = UdpSocket::bind("127.0.0.1:0").expect("a socket to send from");
        for _ in 0..SENT {
            // A previous datagram's ICMP port-unreachable comes back as an
            // error on the next send of a connected socket; this one is
            // unconnected, so every send is delivered to the stack and dropped
            // there.
            let _ = sender.send_to(&[0x5au8; 32], ("127.0.0.1", NOWHERE));
        }

        let outcome = measuring.wait_with_output().expect("omn should finish");
        if unprivileged(&stderr(&outcome)) {
            eprintln!(
                "skipped: net.drops needs CAP_BPF and CAP_PERFMON, which this machine does not \
                 grant"
            );
            return;
        }
        assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
        let after = snmp("Udp", "NoPorts");
        let run = results(&outcome);
        let lost = labelled(&run, "drops")["losses"]["dropped"]
            .as_u64()
            .expect("a loss count");
        if lost == 0 {
            attempts.push((before, after, run));
            break;
        }
        attempts.push((before, after, run));
    }
    let (before, after, run) = attempts.pop().expect("at least one window");
    let lost = labelled(&run, "drops")["losses"]["dropped"]
        .as_u64()
        .expect("a loss count");
    if lost > 0 {
        eprintln!(
            "skipped: three windows in a row lost drops to a full ring -- the last {lost} of \
             them -- so none of their counts can be checked against the {SENT} datagrams the \
             scenario sent; `a_drop_storm_reports_what_the_ring_could_not_hold` is the scenario \
             for that case"
        );
        return;
    }

    assert!(
        after - before >= SENT,
        "the kernel's own Udp:NoPorts moved by {}, and the scenario sent {SENT} datagrams to a \
         port nothing was listening on",
        after - before
    );

    // Every drop, by reason. A drop to a closed port has exactly one, and it
    // has to account for at least the datagrams the scenario sent -- the rest
    // of the machine's drops are other reasons or more of this one.
    let mut by_reason: BTreeMap<String, u64> = BTreeMap::new();
    let mut counted = 0;
    for event in raw_events(&run, "drops") {
        let attributes = attrs(event);
        let drops = attributes["drops"].as_u64().expect("a count");
        *by_reason
            .entry(
                attributes["reason"]
                    .as_str()
                    .expect("a reason, by name")
                    .to_owned(),
            )
            .or_default() += drops;
        counted += drops;
    }
    assert!(counted >= SENT, "only {counted} drops were reported");

    // `NO_SOCKET` specifically, and not "the commonest reason", which is what
    // this asserted first and is a fact about the machine rather than about
    // the tool: nine hundred tests share it, and the rest of this file's own
    // scenarios flood a socket whose receive buffer overflows, which is
    // another reason and outnumbers the scenario's own drops twenty to one.
    //
    // A packet nobody was listening for is `SKB_DROP_REASON_NO_SOCKET` in the
    // kernel's source, and the name is read from the running kernel's BTF --
    // so a kernel that renumbers the enum still says `NO_SOCKET` here, and a
    // table of numbers in the tool would have said something else.
    assert!(
        by_reason.get("NO_SOCKET").copied().unwrap_or(0) >= SENT,
        "the scenario sent {SENT} datagrams to a port nothing was listening on, and \
         `NO_SOCKET` accounts for {:?} of the {counted} drops this run saw: {by_reason:?}",
        by_reason.get("NO_SOCKET")
    );

    // And the contrast. The device counted no drops at all, because nothing
    // went wrong at the device.
    let links = labelled(&run, "links");
    assert!(
        links["events_seen"].as_u64().unwrap_or(0) > 0,
        "net.iface read the interface counters and produced nothing"
    );
}

/// The kernel's own softirq counter bounds what this run bucketed.
///
/// There is no way to inject a number of `NET_RX` handlers: they happen when
/// the driver decides, and how many a transfer costs depends on coalescing. So
/// the check is against the only other party that counts them, `/proc/softirqs`,
/// which the kernel maintains for its own reasons and which this run reads
/// around -- once strictly before, once strictly after.
///
/// That makes the relationship exact in one direction, which is the direction
/// that matters: the window this run observed is *inside* the window the two
/// reads bound, so it cannot have seen more handlers than happened. A run that
/// reports more is double-counting, and this is the only thing that would catch
/// it.
#[test]
fn the_kernels_own_softirq_counter_bounds_what_this_run_bucketed() {
    let before = softirq_runs("NET_RX");
    let measuring = Command::new(BINARY)
        .args(["-f", &session("net-softirq-cost.yml"), "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");

    // A quarter of a mebibyte is thousands of `NET_RX` handlers and is small
    // enough not to show up in the loopback counters another test in this file
    // is reading over the same seconds. The bound below is what this scenario
    // checks, and it does not need volume to be meaningful.
    pause(200);
    push_through_loopback(256 * 1024);

    let outcome = measuring.wait_with_output().expect("omn should finish");
    let after = softirq_runs("NET_RX");
    if unprivileged(&stderr(&outcome)) {
        eprintln!(
            "skipped: net.softirq needs CAP_BPF and CAP_PERFMON, which this machine does not \
             grant"
        );
        return;
    }
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);

    // Every event stands for the handler runs in its bucket, which is what the
    // `count` attribute and `emit::WEIGHT_ATTR` both carry. Counting the events
    // instead would count buckets.
    // `NET_RX` alone, because that is the row this test reads out of
    // `/proc/softirqs`. The source measures both network vectors, and summing
    // them against one of the kernel's two counters is how this first failed:
    // it reported more handlers than had happened, which is exactly the shape
    // of the defect the bound exists to catch.
    let mut handlers = 0u64;
    let mut exact_total_ns = 0u64;
    let mut bucketed_low_ns = 0u64;
    let mut vectors: BTreeMap<String, u64> = BTreeMap::new();
    for event in raw_events(&run, "softirq") {
        let attributes = attrs(event);
        let count = attributes["count"].as_u64().expect("a count");
        assert_eq!(
            attributes["weight"].as_u64(),
            Some(count),
            "a kernel-built bucket has to carry the runs it stands for, or a histogram comes \
             out the height of its bucket count"
        );
        let vector = attributes["vec_name"]
            .as_str()
            .expect("a vector name")
            .to_owned();
        *vectors.entry(vector.clone()).or_default() += count;
        exact_total_ns += attributes["total_ns"].as_u64().expect("a total");
        bucketed_low_ns += count * attributes["duration_ns"].as_u64().expect("a value");
        if vector == "NET_RX" {
            handlers += count;
        }
    }
    assert!(
        vectors
            .keys()
            .all(|name| name == "NET_RX" || name == "NET_TX"),
        "a network source must measure the network vectors and no others: {vectors:?}"
    );

    assert!(
        handlers > 0,
        "a quarter of a mebibyte crossed loopback and no NET_RX softirq ran: {vectors:?}"
    );
    let kernel_ran = after - before;
    assert!(
        handlers <= kernel_ran,
        "this run bucketed {handlers} NET_RX handlers and the kernel counted {kernel_ran} over a \
         window containing it, so something here counted twice"
    );

    // The exact total and the bucketed one are different numbers, and the
    // second is a lower bound: a log2 bucket is a factor of two wide, so a sum
    // built from its low bounds is between half the truth and the truth.
    assert!(
        exact_total_ns >= bucketed_low_ns,
        "the exact total {exact_total_ns} is below the sum of the bucket floors \
         {bucketed_low_ns}, which cannot happen"
    );
    assert!(
        exact_total_ns < 2 * bucketed_low_ns.max(1),
        "and cannot be more than twice it: {exact_total_ns} against {bucketed_low_ns}"
    );

    // The run says which vectors it measured and how it compares against the
    // kernel's own count, because "network softirq time" is a choice this
    // collector made rather than something the kernel reports.
    let notes = labelled(&run, "softirq")["degradations"].to_string();
    assert!(
        notes.contains("NET_RX") && notes.contains("/proc/softirqs"),
        "the run has to name the vectors it measured and where their numbers came from: {notes}"
    );
    // And the kernel's own count is a fact the run states, not something it
    // gave up: it reaches `notes`, and not `degraded:`.
    let facts = labelled(&run, "softirq")["notes"].to_string();
    assert!(
        facts.contains("the kernel's own /proc/softirqs counted"),
        "the kernel's count of the same handler runs is a fact of the run: {facts}"
    );
    assert!(
        !notes.contains("the kernel's own /proc/softirqs counted"),
        "and not a degradation: {notes}"
    );
}

/// A queueing discipline that delays every packet by fifty milliseconds,
/// measured as fifty milliseconds.
///
/// The number is injected: `netem` holds each packet for a fixed time, chosen
/// here and produced by the kernel, and the whole of `net.qdisc` is the gap
/// between the enqueue and the dequeue that the delay sits in. Two hundred
/// packets are sent and all two hundred have to be found.
///
/// The device is inside a network namespace and `omn` runs outside it, which is
/// the only arrangement that works: a raw tracepoint is not namespaced and sees
/// every packet queued on the machine, while `bpf()` checks `CAP_BPF` against
/// the initial user namespace and would refuse to load anything from inside an
/// unprivileged one.
#[test]
fn a_queue_that_delays_every_packet_is_measured_as_that_delay() {
    const PACKETS: u64 = 200;
    const DELAY_MS: u64 = 50;

    if !can_build_a_network() {
        eprintln!(
            "skipped: this scenario needs iproute2 and an unprivileged user namespace to build a \
             device with a queueing discipline; loopback is `noqueue` on every machine, so there \
             is nothing else here to measure a queue on"
        );
        return;
    }
    if !Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|outcome| outcome.status.success())
    {
        eprintln!("skipped: this machine has no `python3`, which the sending workload is in");
        return;
    }

    let measuring = Command::new(BINARY)
        .args(["-f", &session("net-queue-delay.yml"), "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");

    // A dummy device, a discipline that delays, an address to send to and a
    // static neighbour so that address resolution never happens: what is left
    // on the wire is exactly the scenario's own packets.
    let workload = workload_in_a_namespace(&format!(
        "set -e
         ip link add dummy0 type dummy
         tc qdisc add dev dummy0 root netem delay {DELAY_MS}ms limit 100000
         ip addr add 10.99.0.1/24 dev dummy0
         ip link set dummy0 up
         ip neigh add 10.99.0.2 lladdr 02:00:00:00:00:02 dev dummy0
         sleep 0.4
         python3 -c 'import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
for _ in range({PACKETS}): s.sendto(b\"x\" * 64, (\"10.99.0.2\", 9999))'
         sleep 0.6"
    ));
    let built = workload
        .wait_with_output()
        .expect("the workload should run");
    let outcome = measuring.wait_with_output().expect("omn should finish");
    if unprivileged(&stderr(&outcome)) {
        eprintln!(
            "skipped: net.qdisc needs CAP_BPF and CAP_PERFMON, which this machine does not grant"
        );
        return;
    }
    if !built.status.success() {
        eprintln!(
            "skipped: this machine would not build the device the scenario measures: {}",
            stderr(&built)
        );
        return;
    }
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);

    let mut packets = 0u64;
    let mut total_ns = 0u64;
    for event in raw_events(&run, "queueing") {
        let attributes = attrs(event);
        if attributes["iface"].as_str() != Some("dummy0") {
            continue;
        }
        packets += attributes["count"].as_u64().expect("a count");
        total_ns += attributes["total_ns"].as_u64().expect("a total");
    }
    assert_eq!(
        packets, PACKETS,
        "the scenario put {PACKETS} packets through a queueing discipline and this found \
         {packets}"
    );

    // The exact mean, which is what `total_ns` is beside the bucket for: a log2
    // bucket holding fifty milliseconds is `[33.55ms, 67.11ms)`, and a mean
    // taken from those bounds could say anything in between.
    let mean_ms = total_ns as f64 / packets as f64 / 1e6;
    assert!(
        (mean_ms - DELAY_MS as f64).abs() < 5.0,
        "the discipline was told to hold every packet for {DELAY_MS} ms and the exact mean of \
         what it held is {mean_ms:.2} ms"
    );

    // The interface's name came out of the kernel, in a namespace this process
    // cannot name an index in. Resolving it here afterwards would have found
    // someone else's device or nothing at all.
    let keys = raw_events(&run, "queueing")
        .iter()
        .filter_map(|event| event["keys"][0]["iface"]["name"].as_str())
        .any(|name| name == "dummy0");
    assert!(keys, "the event's key carries the name the program read");
}

/// Every segment the kernel resent is one this run saw, and the kernel is the
/// one that counted them.
///
/// There is no way to choose a number of retransmissions: the stack resends on
/// its own timer, and how many fit in a window is an accident of `TCP_RTO_MIN`
/// and exponential backoff. What the scenario chooses is the *fault* -- a
/// queueing discipline added after the handshake, so an established connection
/// finds its path gone -- and what it checks against is `Tcp: RetransSegs` in
/// that namespace's own `/proc/net/snmp`, which the kernel maintains by a
/// completely different route.
///
/// The namespace makes that counter exact: the only connection on that network
/// is the scenario's, so its whole `RetransSegs` delta belongs to the one
/// socket, whose local port the workload prints. `omn` watches from outside and
/// sees the machine's retransmissions as well as the scenario's, which is why
/// the check is over the events of that one port.
#[test]
fn every_segment_the_kernel_resent_is_one_this_run_saw() {
    if !can_build_a_network() {
        eprintln!(
            "skipped: this scenario needs iproute2 and an unprivileged user namespace to build a \
             link it can break; breaking this machine's own is not an option a test has"
        );
        return;
    }
    if !Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|outcome| outcome.status.success())
    {
        eprintln!("skipped: this machine has no `python3`, which the workload is written in");
        return;
    }

    let measuring = Command::new(BINARY)
        .args([
            "-f",
            &session("net-retransmissions.yml"),
            "--format",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");

    // The discipline is added after the handshake, so the connection is
    // established and then the path disappears under it. The workload prints
    // the two numbers this scenario is checked against.
    let workload = workload_in_a_namespace(
        "set -e
         ip link set lo up
         sleep 0.4
         python3 -c '
import socket, struct, subprocess, time
def retrans():
    lines = open(\"/proc/net/snmp\").read().splitlines()
    for at, line in enumerate(lines):
        if line.startswith(\"Tcp:\") and \"RetransSegs\" in line:
            names = line.split()
            return int(lines[at + 1].split()[names.index(\"RetransSegs\")])
    raise SystemExit(\"no Tcp: RetransSegs\")
l = socket.socket(); l.bind((\"127.0.0.1\", 0)); l.listen(1)
c = socket.create_connection(l.getsockname()); s, _ = l.accept()
subprocess.run([\"tc\", \"qdisc\", \"add\", \"dev\", \"lo\", \"root\", \"netem\", \"loss\", \"100%\"], check=True)
before = retrans()
c.sendall(b\"x\" * 100)
time.sleep(1.1)
port = c.getsockname()[1]
c.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack(\"ii\", 1, 0))
c.close()
print(port, l.getsockname()[1], retrans() - before)'",
    );
    let built = workload
        .wait_with_output()
        .expect("the workload should run");
    let outcome = measuring.wait_with_output().expect("omn should finish");
    if unprivileged(&stderr(&outcome)) {
        eprintln!(
            "skipped: net.tcp.retransmit needs CAP_BPF and CAP_PERFMON, which this machine does \
             not grant"
        );
        return;
    }
    if !built.status.success() {
        eprintln!(
            "skipped: this machine would not build the broken link the scenario measures: {}",
            stderr(&built)
        );
        return;
    }
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));

    let spoken = stdout(&built);
    let mut said = spoken.split_whitespace();
    let port: u64 = said
        .next()
        .and_then(|field| field.parse().ok())
        .unwrap_or_else(|| panic!("the workload did not print its port: {spoken}"));
    let listener: u64 = said
        .next()
        .and_then(|field| field.parse().ok())
        .unwrap_or_else(|| panic!("the workload did not print the listener's port: {spoken}"));
    let kernel_counted: u64 = said
        .next()
        .and_then(|field| field.parse().ok())
        .unwrap_or_else(|| panic!("the workload did not print its RetransSegs: {spoken}"));

    if kernel_counted == 0 {
        eprintln!(
            "skipped: the kernel resent nothing over the scenario's window, so there is nothing \
             to check this against; the connection was established and its path removed, and a \
             machine slow enough to miss its own retransmission timer can do that"
        );
        return;
    }

    let mine: Vec<BTreeMap<String, serde_json::Value>> =
        raw_events(&results(&outcome), "retransmits")
            .iter()
            .map(attrs)
            // The connection by both of its ports, which the peer's end on
            // the event makes possible: a system target sees every namespace,
            // and another namespace's connection may have the same local
            // port, but not the same pair.
            .filter(|attributes| {
                attributes["sport"].as_u64() == Some(port)
                    && attributes["dport"].as_u64() == Some(listener)
            })
            .collect();
    // The kernel's count is the namespace's own `RetransSegs`, read after the
    // connection was aborted -- `SO_LINGER` of zero stops its retransmission
    // timer -- so no resend can land between the read and the end of the
    // connection, which is what reading it before the close allowed (the
    // flaky tally's #16: "4 against 3").
    assert_eq!(
        mine.len() as u64,
        kernel_counted,
        "the kernel counted {kernel_counted} retransmitted segments on {port} -> {listener} and \
         this run saw {}",
        mine.len()
    );

    // Every one of them from an established connection, which is what makes
    // the count readable: the same number from `SYN_SENT` would be a handshake
    // nobody answered, and it is a different fault.
    for attributes in &mine {
        assert_eq!(
            attributes["state_name"].as_str(),
            Some("ESTABLISHED"),
            "the path was removed after the handshake, so every resend is of data: {attributes:?}"
        );
    }
}

// --------------------------------------------------------- Class: the loss path

/// More drops than the ring can hold, and a run that says its own count is
/// short.
///
/// The BPF ring buffer does not report its own overflows: when a program cannot
/// reserve space the helper returns an error, the event is gone, and nothing
/// anywhere records that it existed. So the program counts it in a per-cpu
/// array beside the ring, and this checks that the count reaches the user.
///
/// It is the class this project exists for. A collector that loses a third of
/// its events and says nothing produces wrong numbers that look right, and by
/// the time anyone notices the run is over.
#[test]
fn a_drop_storm_reports_what_the_ring_could_not_hold() {
    /// Six senders, because one cannot outrun the drain: a single thread
    /// manages about two hundred thousand datagrams a second, and the ring
    /// holds nine thousand records between ten-millisecond drains. Six of them
    /// clear a million a second between them, which is more than that.
    ///
    /// Forty thousand each and no more, because this storm is shared: it runs
    /// in the same suite as `every_datagram_sent_to_a_closed_port_is_a_drop_
    /// with_one_reason`, whose window it can fill, and it saturates a machine
    /// that eight hundred other tests are measuring things on.
    const SENDERS: usize = 6;
    const EACH: usize = 40_000;
    const NOWHERE: u16 = 59321;

    let measuring = Command::new(BINARY)
        .args(["-f", &session("net-drop-storm.yml"), "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");

    pause(200);
    let senders: Vec<std::thread::JoinHandle<()>> = (0..SENDERS)
        .map(|_| {
            std::thread::spawn(move || {
                let socket = UdpSocket::bind("127.0.0.1:0").expect("a socket to send from");
                socket
                    .connect(("127.0.0.1", NOWHERE))
                    .expect("a destination nothing is listening on");
                for _ in 0..EACH {
                    // A previous datagram's ICMP port-unreachable comes back as
                    // an error on the next send of a connected socket. It is
                    // not a failure of the workload: the datagram before it
                    // reached the stack and was dropped, which is the whole
                    // point.
                    let _ = socket.send(&[0x5au8; 16]);
                }
            })
        })
        .collect();
    for sender in senders {
        sender.join().expect("a sender");
    }

    let outcome = measuring.wait_with_output().expect("omn should finish");
    if unprivileged(&stderr(&outcome)) {
        eprintln!(
            "skipped: net.drops needs CAP_BPF and CAP_PERFMON, which this machine does not grant"
        );
        return;
    }
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);
    let result = labelled(&run, "drops");

    let dropped = result["losses"]["dropped"].as_u64().expect("a loss count");
    let seen = result["events_seen"].as_u64().expect("an event count");
    if dropped == 0 {
        eprintln!(
            "skipped: this machine drained {seen} drops without ever filling the ring, so the \
             loss path was not exercised; a faster machine needs a bigger storm than \
             {} datagrams",
            SENDERS * EACH
        );
        return;
    }

    assert!(
        seen > 0,
        "the run lost {dropped} drops and reported none at all, which is a ring that never \
         delivered rather than a ring that overflowed"
    );

    // The count is a lower bound and the run has to say which way it is wrong.
    let reasons = result["losses"]["reasons"].to_string();
    assert!(
        reasons.contains("the drop ring was full"),
        "the loss has to reach the output with what it was: {reasons}"
    );
    assert!(
        reasons.contains("short"),
        "and with which way the number is wrong, because a count of what the machine threw away \
         that is itself short is the worst shape this source could take: {reasons}"
    );
}

// --------------------------------------------------------- Class: the refusal

/// The three different things these five sources say no to, and the three
/// different codes that go with them.
///
/// They must not be confused, because each one sends a reader somewhere else. A
/// request that cannot mean anything has to be rewritten; a machine that cannot
/// serve one may be swapped or given a privilege; a build that has not written
/// something is neither, and retrying it forever is what a reader who cannot
/// tell them apart does.
#[test]
fn the_three_answers_an_ebpf_net_request_can_be_refused_with() {
    // 2 -- the request cannot mean anything. A connection's lifetime is the gap
    // between two state changes, and there is no such thing as a sampled gap:
    // a sample that catches one end without the other has no duration at all.
    let outcome = omn(&[
        "net.tcp.lifetime",
        "--system",
        "--mode",
        "sample@99hz",
        "--duration",
        "200ms",
        "--emit",
        "raw",
    ]);
    let complaint = stderr(&outcome);
    assert_eq!(code(&outcome), 2, "{complaint}");
    assert!(
        complaint.contains("net.tcp.lifetime") && complaint.contains("sample"),
        "the rejection should name the source and the mode: {complaint}"
    );

    // Also 2, and by a different route: `net.drops` declares only a system
    // target, so a pid never reaches a collector at all. The two are worth
    // keeping apart -- a source the catalog says nothing about for a process,
    // and a source it does and this build cannot serve -- because only the
    // second is a gap anybody can close.
    let outcome = omn(&[
        "net.drops",
        "--pid",
        &std::process::id().to_string(),
        "--mode",
        "trace",
        "--duration",
        "200ms",
        "--emit",
        "raw",
    ]);
    let complaint = stderr(&outcome);
    assert_eq!(code(&outcome), 2, "{complaint}");
    assert!(
        complaint.contains("declares no `pid` support"),
        "a target the catalog never offered is a request that cannot mean anything: {complaint}"
    );

    // Still 2 for a target the entry does not offer. The seventh wave took
    // `pid` off `net.tcp.lifetime`, the task current when a connection is
    // established being whichever one softirq landed on; the twenty-fourth
    // gave it back, attributed to the process that connected or closed it
    // first, so a pid now runs and a cgroup is the target refused before the
    // machine is touched.
    let outcome = omn(&[
        "net.tcp.lifetime",
        "--cgroup",
        "/sys/fs/cgroup",
        "--mode",
        "trace",
        "--duration",
        "200ms",
        "--emit",
        "raw",
    ]);
    let complaint = stderr(&outcome);
    assert_eq!(
        code(&outcome),
        2,
        "a target the catalog never offers is a request that cannot mean anything: {complaint}"
    );
    assert!(
        complaint.contains("declares no `cgroup` support") && complaint.contains("system"),
        "the refusal should say what the source does accept: {complaint}"
    );

    // 3 -- this build has not written it. `net.socket` pinned to its
    // `bpf:kprobe` backend, which this machine serves and no collector does,
    // and the answer says so in those words: not "this machine cannot", which
    // would send a reader to change a kernel that is already capable. It was
    // the unpinned request until the eighth wave wrote `bpf:fentry`.
    let outcome = omn(&[
        "net.socket",
        "--system",
        "--mode",
        "trace",
        "--duration",
        "200ms",
        "--emit",
        "raw",
        "--backend",
        "force:bpf:kprobe",
    ]);
    let complaint = stderr(&outcome);
    assert_eq!(
        code(&outcome),
        3,
        "a backend this build has not written is the build's gap: {complaint}"
    );
    assert!(
        complaint.contains("no collector is written") && complaint.contains("net.socket"),
        "the refusal should name the source and say it is this build: {complaint}"
    );
    assert!(
        complaint.contains("The request itself is valid"),
        "exit 3 is not a failure of the request and should not read like one: {complaint}"
    );

    // 1 -- this machine cannot serve it. Every one of these five has a
    // `bpf:tp` backend behind its `bpf:raw_tp` one, and a classic tracepoint's
    // numeric event id lives in a file under tracefs that a normal user cannot
    // read. Forcing it is the backend comparison this family can make: the
    // second road exists, and on this machine it is closed.
    let outcome = omn(&[
        "net.softirq",
        "--system",
        "--mode",
        "trace",
        "--duration",
        "200ms",
        "--emit",
        "hist",
        "--backend",
        "force:bpf:tp",
    ]);
    let complaint = stderr(&outcome);
    match code(&outcome) {
        1 => assert!(
            complaint.contains("tracefs") || complaint.contains("tracepoint:irq:softirq_entry"),
            "a machine that cannot serve the classic path should say what it is missing: \
             {complaint}"
        ),
        3 => assert!(
            complaint.contains("no collector is written"),
            "a machine that can serve the classic path has only this build's gap left, and the \
             message has to say so rather than reading like a failure: {complaint}"
        ),
        other => panic!(
            "forcing `bpf:tp` exited {other}: it is this machine's gap or this build's, never a \
             request that cannot mean anything:\n{complaint}"
        ),
    }
}

/// Forcing the backend a chain's second road is, on each of the five.
///
/// `--backend force:` is what a backend comparison is made with, and this
/// family has none to make: every one of these sources has exactly one
/// implemented backend and one declared behind it. So the comparison is with
/// the refusal, and what it establishes is that the chain is real -- the
/// resolver knows about the other road, names it, and says which kind of
/// obstacle is in the way.
#[test]
fn the_second_road_of_every_ebpf_net_chain_is_named_when_it_is_forced() {
    let chains: &[(&str, &str, &str, &str)] = &[
        ("net.tcp.lifetime", "trace", "raw", "bpf:tp"),
        ("net.tcp.retransmit", "trace", "raw", "bpf:tp"),
        ("net.drops", "trace", "raw", "bpf:tp"),
        ("net.qdisc", "trace", "raw", "bpf:tp"),
        ("net.softirq", "trace", "raw", "bpf:tp"),
    ];
    for (source, mode, emit, backend) in chains {
        let forced = format!("force:{backend}");
        let outcome = omn(&[
            source,
            "--system",
            "--mode",
            mode,
            "--duration",
            "150ms",
            "--emit",
            emit,
            "--backend",
            &forced,
        ]);
        let complaint = stderr(&outcome);
        assert!(
            matches!(code(&outcome), 1 | 3),
            "`{source}` forced onto `{backend}` exited {}, and the second road of a chain is \
             either unreachable here or unwritten, never a request that cannot mean anything:\n\
             {complaint}",
            code(&outcome)
        );
        assert!(
            complaint.contains(backend),
            "`{source}` should name `{backend}` when it is the backend that was asked for: \
             {complaint}"
        );

        // And the road that is open answers, which is what makes the refusal
        // above a comparison rather than a pair of failures.
        let outcome = omn(&[
            source,
            "--system",
            "--mode",
            mode,
            "--duration",
            "150ms",
            "--emit",
            emit,
            "--backend",
            "force:bpf:raw_tp",
            "--format",
            "json",
        ]);
        if unprivileged(&stderr(&outcome)) {
            eprintln!(
                "skipped: `{source}` needs CAP_BPF and CAP_PERFMON, which this machine does not \
                 grant"
            );
            return;
        }
        assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
        assert_eq!(
            results(&outcome)["results"][0]["backend"],
            "bpf:raw_tp",
            "`{source}` was served by something other than the backend it was forced onto"
        );
    }
}

// ---------------------------------------------------------------------------
// The two sources over fentry: `net.tcp.connect` and `net.socket`
// ---------------------------------------------------------------------------
//
// The eighth wave's attach kind. Ground truth here is the kernel's own answer
// for the workload's own sockets: `SO_COOKIE` is `getsockopt`, asked by the
// program of the socket it holds, and it is the number both sources key their
// series on. A count of handshakes and sends the program made, the port it
// connected to, the errno it was given, and a wait it timed itself complete
// the reference; none of it comes from the tool.
//
// The four classes: the sweep (`examples/net-fentry-sweep.yml`), the ground
// truth below, the loss path (a pending map and a histogram driven past their
// capacity on purpose), and the refusal. `docs/families/net.md` has what each
// measured.

/// Where a workload and its account live: inside the build directory, on the
/// repository's own filesystem, like every other scratch file of this suite.
fn fx_scratch(name: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the scratch directory");
    path
}

/// Runs `omn` with a launched Python workload, and reads back what the
/// workload says it did.
fn fx_launch(args: &[&str], name: &str, body: &str) -> (Output, serde_json::Value) {
    let workload = fx_scratch(&format!("{name}.py"));
    std::fs::write(&workload, body).expect("the workload");
    let account = fx_scratch(&format!("{name}.json"));
    let _ = std::fs::remove_file(&account);
    let mut full: Vec<&str> = args.to_vec();
    full.extend_from_slice(&["--format", "json", "--", "python3"]);
    let workload = workload.to_str().expect("a path").to_owned();
    let account_arg = account.to_str().expect("a path").to_owned();
    full.push(&workload);
    full.push(&account_arg);
    let output = omn(&full);
    let did = std::fs::read_to_string(&account)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null);
    (output, did)
}

/// A raw event's attributes, by name.
fn fx_attrs(event: &serde_json::Value) -> BTreeMap<String, serde_json::Value> {
    event["attrs"]
        .as_array()
        .expect("attributes")
        .iter()
        .map(|attr| {
            (
                attr["name"].as_str().expect("a name").to_owned(),
                attr["value"].clone(),
            )
        })
        .collect()
}

fn fx_raw(result: &serde_json::Value) -> Vec<BTreeMap<String, serde_json::Value>> {
    result["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .find(|output| output["emit"] == "raw")
        .expect("a raw emit")["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(fx_attrs)
        .collect()
}

fn fx_u64(event: &BTreeMap<String, serde_json::Value>, name: &str) -> u64 {
    event
        .get(name)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| panic!("an event carries `{name}`: {event:?}"))
}

fn fx_text<'a>(event: &'a BTreeMap<String, serde_json::Value>, name: &str) -> &'a str {
    event
        .get(name)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("an event carries `{name}`: {event:?}"))
}

/// Whether this build can attach an fentry program, and why not.
fn fx_no_fentry_here() -> Option<String> {
    let outcome = omn(&[
        "net.socket",
        "--system",
        "--mode",
        "trace",
        "--duration",
        "100ms",
        "--emit",
        "summary",
    ]);
    if code(&outcome) == 0 {
        return None;
    }
    Some(format!(
        "this build cannot attach an fentry program: {}",
        stderr(&outcome).lines().next().unwrap_or("").trim()
    ))
}

/// The sweep for the two fentry sources: every emit each declares, in every
/// mode the matrix lets it have, over one window with traffic running.
#[test]
fn the_fentry_leg_of_the_sweep_answers_with_every_emit_it_promised() {
    if let Some(reason) = fx_no_fentry_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let measuring = Command::new(BINARY)
        .args(["-f", &session("net-fentry-sweep.yml"), "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    // Traffic for as long as the run lasts, rather than once at a guessed
    // moment: attaching fentry programs rebuilds trampolines, which the kernel
    // serialises machine-wide, and under this suite a run's attach has been
    // measured at more than two seconds. A single burst sent a quarter of a
    // second in lands before the window opens on exactly the runs that matter.
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let traffic = {
        let done = done.clone();
        std::thread::spawn(move || {
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                push_through_loopback(64 * 1024);
                pause(50);
            }
        })
    };
    let outcome = measuring.wait_with_output().expect("omn should finish");
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    traffic.join().expect("the traffic thread");
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);
    for (label, wanted) in [
        ("handshakes", &["hist", "topk", "raw", "summary"][..]),
        (
            "sends-and-receives",
            &["hist", "topk", "raw", "summary"][..],
        ),
        ("counted", &["raw", "summary"][..]),
    ] {
        let result = labelled(&run, label);
        assert_eq!(emits(result), wanted, "`{label}` emitted the wrong set");
        assert_eq!(result["backend"], "bpf:fentry", "`{label}`");
        let seen = result["events_seen"].as_u64().expect("an event count");
        assert!(
            seen > 0,
            "`{label}` watched a connection open, carry 64 KiB and close, and saw nothing"
        );
    }
}

const FX_SOCKETS: &str = r#"
import json, os, socket, struct, sys, threading, time
account = sys.argv[1]
SO_COOKIE = 57
cookie = lambda s: struct.unpack("Q", s.getsockopt(socket.SOL_SOCKET, SO_COOKIE, 8))[0]
listener = socket.socket(); listener.bind(("127.0.0.1", 0)); listener.listen(1)
port = listener.getsockname()[1]
state = {}
def serve():
    conn, _ = listener.accept()
    state["server_cookie"] = cookie(conn)
    total = 0
    while total < 2 * 1000 * 4096:
        data = conn.recv(65536)
        if not data: break
        total += len(data)
    state["received"] = total
    # The injected wait: the client's next receive blocks until this sends.
    time.sleep(0.3)
    conn.send(b"!")
    conn.recv(1)
    conn.close()
server = threading.Thread(target=serve); server.start()
client = socket.create_connection(("127.0.0.1", port))
block = b"x" * 4096
for _ in range(1000):
    client.send(block)
for _ in range(1000):
    os.write(client.fileno(), block)
started = time.monotonic_ns()
client.recv(1)
waited = time.monotonic_ns() - started
client.send(b".")
server.join()
# The byte that ends the exchange is a send too: 2001, and one byte more.
json.dump({"cookie": cookie(client), "port": port, "sends": 2001, "bytes": 2000 * 4096 + 1,
           "waited_ns": waited, **state}, open(account, "w"))
client.close()
"#;

/// Two thousand sends of 4096 bytes on one socket, found on that socket's own
/// cookie, byte for byte; the server's receives, found on its; and a receive
/// that waited three hundred milliseconds for its peer, found at the length
/// the program timed.
///
/// Every reference is the program's: the cookies are `getsockopt(SO_COOKIE)`
/// on the sockets it holds, the sends are its loop bound, and the wait is its
/// own clock around the call. The half of the sends made with `write(2)` are
/// the check that the send path is the family's `sendmsg` and not
/// `sock_sendmsg`: `src/bpf/net_socket.rs` has why, and a program attached to
/// the latter counts none of these.
#[test]
fn every_send_and_receive_is_found_on_the_socket_that_made_it() {
    if let Some(reason) = fx_no_fentry_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let (outcome, did) = fx_launch(
        &["net.socket", "--mode", "trace", "--emit", "raw"],
        "fx-sockets",
        FX_SOCKETS,
    );
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);
    let result = &run["results"][0];
    let events = fx_raw(result);
    let client = did["cookie"].as_u64().expect("the client's cookie");
    let server = did["server_cookie"].as_u64().expect("the server's cookie");
    let on = |cookie: u64, op: &str| -> (u64, u64, Vec<u64>) {
        let mine: Vec<_> = events
            .iter()
            .filter(|event| {
                event.get("cookie").and_then(serde_json::Value::as_u64) == Some(cookie)
                    && fx_text(event, "op") == op
            })
            .collect();
        (
            mine.iter().map(|event| fx_u64(event, "count")).sum(),
            mine.iter().map(|event| fx_u64(event, "bytes")).sum(),
            mine.iter().map(|event| fx_u64(event, "total_ns")).collect(),
        )
    };
    let dropped = result["losses"]["dropped"].as_u64().expect("a drop count");
    if dropped > 0 {
        eprintln!(
            "note: the run counted {dropped} lost call(s): {}",
            result["losses"]["reasons"]
        );
    }

    // The client's sends: the loop bound, the bytes, exactly -- or short by no
    // more than what the run counted as lost.
    let (sends, sent, _) = on(client, "send");
    let wanted = did["sends"].as_u64().expect("sends");
    assert!(
        sends <= wanted && sends + dropped >= wanted,
        "{sends} sends on the client's socket {client} for the {wanted} it made, {dropped} lost"
    );
    if dropped == 0 {
        assert_eq!(sent, did["bytes"].as_u64().expect("bytes"));
    }
    // The port the client connected to, on every one of its rows.
    let port = did["port"].as_u64().expect("the port");
    assert!(events
        .iter()
        .filter(|event| event.get("cookie").and_then(serde_json::Value::as_u64) == Some(client))
        .all(|event| fx_u64(event, "dport") == port && fx_u64(event, "family") == 2));
    // And the interface its route leaves by: a connection to 127.0.0.1 is
    // routed over loopback, and that is the key `net.bytes` carries for it.
    for cookie in [client, server] {
        let rows: Vec<_> = events
            .iter()
            .filter(|event| event.get("cookie").and_then(serde_json::Value::as_u64) == Some(cookie))
            .collect();
        assert!(!rows.is_empty());
        assert!(
            rows.iter().all(|event| fx_text(event, "iface") == "lo"),
            "socket {cookie}'s rows name the interface {:?}",
            rows.iter()
                .map(|event| fx_text(event, "iface"))
                .collect::<Vec<_>>()
        );
    }

    // The server's receives carry every byte the client sent: the stream the
    // server counted itself, and the closing byte its last receive took.
    let (_, received, _) = on(server, "recv");
    if dropped == 0 {
        assert_eq!(
            received,
            did["received"].as_u64().expect("received") + 1,
            "the server's receives on its socket {server}"
        );
    }

    // The injected wait: the client's one receive, at the length the program
    // timed around it -- inside that interval, and within a millisecond of it.
    let waited = did["waited_ns"].as_u64().expect("the program's own clock");
    assert!(waited >= 300_000_000, "the server slept {waited} ns");
    let (receives, _, totals) = on(client, "recv");
    if receives == 0 && dropped > 0 {
        eprintln!("skipped the wait: the client's receive is in the {dropped} counted as lost");
        return;
    }
    assert_eq!(receives, 1, "the client received once");
    let timed = totals[0];
    assert!(
        timed <= waited && waited - timed < 1_000_000,
        "the receive was timed at {timed} ns inside a call the program timed at {waited} ns"
    );
}

const FX_CONNECTS: &str = r#"
import json, socket, struct, sys, time
account = sys.argv[1]
SO_COOKIE = 57
listener = socket.socket(); listener.bind(("127.0.0.1", 0)); listener.listen(64)
port = listener.getsockname()[1]
spare = socket.socket(); spare.bind(("127.0.0.1", 0)); closed = spare.getsockname()[1]; spare.close()
cookies = []
for _ in range(20):
    c = socket.create_connection(("127.0.0.1", port))
    cookies.append(struct.unpack("Q", c.getsockopt(socket.SOL_SOCKET, SO_COOKIE, 8))[0])
    listener.accept()[0].close()
    c.close()
refused = 0
for _ in range(5):
    try:
        socket.create_connection(("127.0.0.1", closed))
    except ConnectionRefusedError:
        refused += 1
# A listener whose accept queue is full drops the next SYN, and the client
# resends it after the kernel's initial retransmission timeout, one second.
# With a backlog of zero, one established connection is a full queue.
full = socket.socket(); full.bind(("127.0.0.1", 0)); full.listen(0)
full_port = full.getsockname()[1]
queued = socket.create_connection(("127.0.0.1", full_port), timeout=5)
import threading
def drain():
    time.sleep(0.5)
    for _ in range(2):
        full.accept()[0].close()
threading.Thread(target=drain).start()
started = time.monotonic_ns()
slow = socket.create_connection(("127.0.0.1", full_port), timeout=5)
slow_ns = time.monotonic_ns() - started
slow_cookie = struct.unpack("Q", slow.getsockopt(socket.SOL_SOCKET, SO_COOKIE, 8))[0]
json.dump({"port": port, "closed": closed, "cookies": cookies, "refused": refused,
           "full_port": full_port, "slow_ns": slow_ns, "slow_cookie": slow_cookie},
          open(account, "w"))
"#;

/// Twenty handshakes that succeed, five that are refused, and one whose first
/// SYN is dropped: each found with its outcome, its port, its errno and the
/// cookie the program's own socket reports.
///
/// The one lost SYN is the injected delay. A listener with a full accept queue
/// drops the SYN, and the client resends it after the kernel's initial
/// retransmission timeout -- one second, `TCP_TIMEOUT_INIT` -- so that
/// handshake must come back at least a second long and the twenty on an idle
/// listener must come back three orders of magnitude shorter. The separation is
/// asserted, not a calibrated threshold: the slow one is above a second, and
/// every fast one is below a tenth of it.
#[test]
fn every_handshake_is_found_with_its_outcome_and_the_lost_syn_takes_a_second() {
    if let Some(reason) = fx_no_fentry_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let (outcome, did) = fx_launch(
        &["net.tcp.connect", "--mode", "trace", "--emit", "raw"],
        "fx-connects",
        FX_CONNECTS,
    );
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);
    let result = &run["results"][0];
    let events = fx_raw(result);
    let port = did["port"].as_u64().expect("the port");
    let closed = did["closed"].as_u64().expect("the closed port");
    let full_port = did["full_port"].as_u64().expect("the full listener's port");
    let dropped = result["losses"]["dropped"].as_u64().expect("a drop count");
    assert_eq!(dropped, 0, "{}", result["losses"]);

    let established: Vec<_> = events
        .iter()
        .filter(|event| fx_u64(event, "dport") == port)
        .collect();
    assert_eq!(established.len(), 20, "twenty handshakes to {port}");
    assert!(established
        .iter()
        .all(|event| fx_text(event, "outcome") == "established"));
    let cookies: std::collections::BTreeSet<u64> = did["cookies"]
        .as_array()
        .expect("cookies")
        .iter()
        .filter_map(serde_json::Value::as_u64)
        .collect();
    let found: std::collections::BTreeSet<u64> = established
        .iter()
        .map(|event| fx_u64(event, "cookie"))
        .collect();
    assert_eq!(
        found, cookies,
        "each handshake carries the cookie SO_COOKIE gives its socket"
    );

    let refused: Vec<_> = events
        .iter()
        .filter(|event| fx_u64(event, "dport") == closed)
        .collect();
    assert_eq!(
        refused.len() as u64,
        did["refused"].as_u64().expect("refused")
    );
    assert!(refused.iter().all(|event| {
        fx_text(event, "outcome") == "refused"
            && event.get("errno").and_then(serde_json::Value::as_i64) == Some(111)
    }));

    let to_full: Vec<_> = events
        .iter()
        .filter(|event| fx_u64(event, "dport") == full_port)
        .collect();
    let slow = to_full
        .iter()
        .find(|event| {
            event.get("cookie").and_then(serde_json::Value::as_u64) == did["slow_cookie"].as_u64()
        })
        .expect("the handshake whose SYN was dropped");
    let slow_ns = fx_u64(slow, "lat_ns");
    assert!(
        slow_ns >= 1_000_000_000 && slow_ns <= did["slow_ns"].as_u64().expect("its own clock"),
        "a handshake behind one dropped SYN took {slow_ns} ns; the kernel resends after one \
         second, and the program timed {} ns around it",
        did["slow_ns"]
    );
    let slowest_fast = established
        .iter()
        .map(|event| fx_u64(event, "lat_ns"))
        .max()
        .expect("twenty");
    assert!(
        slowest_fast < 100_000_000,
        "an idle loopback handshake took {slowest_fast} ns, which is not apart from a lost SYN"
    );
}

const FX_HANDSHAKE_STORM: &str = r#"
import json, resource, socket, sys, time
account = sys.argv[1]
soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
resource.setrlimit(resource.RLIMIT_NOFILE, (min(hard, 16384), hard))
full = socket.socket(); full.bind(("127.0.0.1", 0)); full.listen(0)
port = full.getsockname()[1]
sockets = []
for _ in range(5000):
    s = socket.socket(); s.setblocking(False)
    s.connect_ex(("127.0.0.1", port))
    sockets.append(s)
time.sleep(0.2)
json.dump({"connects": len(sockets), "port": port}, open(account, "w"))
"#;

/// Five thousand handshakes in flight at once against a listener that drops
/// their SYNs is more than the pending map holds, and every one that found no
/// room is counted.
///
/// The number closes: the map holds 4096, the listener's queue lets a couple
/// through before it is full, and the rest stay in `SYN_SENT` for the whole
/// run. So the drops are five thousand less the map's capacity, less the few
/// that established and freed a slot -- and the run must say its handshake
/// count is short, and that the stamps still in the map are handshakes still in
/// flight rather than lost.
#[test]
fn more_handshakes_in_flight_than_the_map_holds_are_counted_as_lost() {
    if let Some(reason) = fx_no_fentry_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let (outcome, did) = fx_launch(
        &["net.tcp.connect", "--mode", "trace", "--emit", "summary"],
        "fx-handshake-storm",
        FX_HANDSHAKE_STORM,
    );
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);
    let result = &run["results"][0];
    let connects = did["connects"].as_u64().expect("connects");
    const CAPACITY: u64 = 4096;
    let dropped = result["losses"]["dropped"].as_u64().expect("a drop count");
    assert!(
        dropped <= connects - CAPACITY && dropped + 16 >= connects - CAPACITY,
        "{connects} handshakes in flight and a map of {CAPACITY}: {dropped} dropped, where \
         {} is the arithmetic",
        connects - CAPACITY
    );
    let reasons = result["losses"]["reasons"].to_string();
    assert!(reasons.contains("in flight at once"), "{reasons}");
    let notes = result["degradations"].to_string();
    assert!(
        notes.contains("still in flight"),
        "the stamps left in the map are handshakes still in SYN_SENT, and the run says so: \
         {notes}"
    );
}

const FX_MANY_SOCKETS: &str = r#"
import json, socket, struct, sys, threading
account = sys.argv[1]
SO_COOKIE = 57
pairs, threads = 1000, 100
ends = [socket.socketpair() for _ in range(pairs)]
cookies = [struct.unpack("Q", a.getsockopt(socket.SOL_SOCKET, SO_COOKIE, 8))[0] for a, _ in ends]
start = threading.Barrier(threads)
def sender():
    start.wait()
    for a, _ in ends:
        a.send(b"x")
workers = [threading.Thread(target=sender) for _ in range(threads)]
for w in workers: w.start()
for w in workers: w.join()
json.dump({"cookies": cookies, "sends": pairs * threads}, open(account, "w"))
"#;

/// A hundred threads each sending one byte on a thousand sockets is a hundred
/// thousand rows, three times what the histogram holds per drain: every send is
/// in a row or in the drop count, and none is in both.
#[test]
fn more_socket_rows_than_the_histogram_holds_are_counted_as_lost_and_the_rest_are_exact() {
    if let Some(reason) = fx_no_fentry_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let (outcome, did) = fx_launch(
        &["net.socket", "--mode", "trace", "--emit", "raw"],
        "fx-many-sockets",
        FX_MANY_SOCKETS,
    );
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);
    let result = &run["results"][0];
    let cookies: std::collections::BTreeSet<u64> = did["cookies"]
        .as_array()
        .expect("cookies")
        .iter()
        .filter_map(serde_json::Value::as_u64)
        .collect();
    let sends = did["sends"].as_u64().expect("sends");
    let counted: u64 = fx_raw(result)
        .iter()
        .filter(|event| {
            fx_text(event, "op") == "send"
                && event
                    .get("cookie")
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|cookie| cookies.contains(&cookie))
        })
        .map(|event| fx_u64(event, "count"))
        .sum();
    let dropped = result["losses"]["dropped"].as_u64().expect("a drop count");
    if dropped == 0 {
        eprintln!(
            "skipped: {counted} of {sends} sends counted and none dropped, so this machine did \
             not fill a drain's worth of rows and the loss path was not created"
        );
        assert_eq!(counted, sends);
        return;
    }
    assert!(
        counted + dropped >= sends && counted <= sends,
        "{counted} counted and {dropped} dropped for {sends} sends"
    );
    assert!(result["losses"]["reasons"]
        .to_string()
        .contains("lower bound"));
}

/// Each refuses what it cannot serve, with the exit code that belongs to it: a
/// mode it cannot mean (2), a cgroup target this build cannot narrow to (3),
/// and a binary without the capabilities (1) -- a copy of this one, which is
/// what a relink produces.
#[test]
fn the_fentry_net_sources_refuse_with_the_reason_that_is_actually_missing() {
    let polled = omn(&[
        "net.socket",
        "--system",
        "--mode",
        "poll@100ms",
        "--duration",
        "200ms",
        "--emit",
        "summary",
    ]);
    assert_eq!(code(&polled), 2, "{}", stderr(&polled));
    let counted = omn(&[
        "net.tcp.connect",
        "--system",
        "--mode",
        "count",
        "--duration",
        "200ms",
        "--emit",
        "summary",
    ]);
    assert_eq!(code(&counted), 2, "{}", stderr(&counted));
    for source in ["net.socket", "net.tcp.connect"] {
        let cgroup = omn(&[
            source,
            "--cgroup",
            "/sys/fs/cgroup",
            "--mode",
            "trace",
            "--duration",
            "100ms",
            "--emit",
            "summary",
        ]);
        let complaint = stderr(&cgroup);
        if code(&cgroup) == 1 {
            eprintln!("skipped the cgroup refusal of {source}: {complaint}");
        } else {
            assert_eq!(code(&cgroup), 3, "{source}: {complaint}");
            assert!(
                complaint.contains("bpf_get_current_cgroup_id"),
                "{complaint}"
            );
        }
    }
    let bare = fx_scratch("omn-net-without-capabilities");
    std::fs::copy(BINARY, &bare).expect("a copy of the binary");
    for source in ["net.socket", "net.tcp.connect"] {
        let output = Command::new(&bare)
            .args([
                source,
                "--system",
                "--mode",
                "trace",
                "--duration",
                "100ms",
                "--emit",
                "summary",
            ])
            .output()
            .expect("the copy runs");
        if output.status.success() {
            eprintln!("skipped the capability refusal of {source}: the copy collected");
            continue;
        }
        let complaint = stderr(&output);
        assert_eq!(code(&output), 1, "{source}: {complaint}");
        assert!(
            complaint.contains("CAP_BPF") && complaint.contains("setcap"),
            "{source}: {complaint}"
        );
    }
}

/// A connection opened in one source and closed in another is joined by its
/// cookie with nothing else in the session.
///
/// Until `net.tcp.connect` existed this join needed `net.tcp.rtt` beside it,
/// for no reason but that its `sock_diag` poll was what made the kernel assign
/// cookies (`docs/scenarios/net.md` item 11). `net.tcp.connect` asks for the
/// cookie at the handshake, so the field `net.tcp.lifetime` reads at the close
/// is set. The reference is the connections this test makes, identified by the
/// local ports the kernel gave their client sockets: each of them must close
/// with a cookie and be attributed to its handshake.
#[test]
fn a_connection_is_joined_from_its_handshake_to_its_close_by_the_cookie_alone() {
    if let Some(reason) = fx_no_fentry_here() {
        eprintln!("skipped: {reason}");
        return;
    }
    let measuring = Command::new(BINARY)
        .args([
            "-f",
            &session("net-handshake-to-close.yml"),
            "--format",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    // Connections for as long as the run lasts, for the reason the fentry
    // sweep gives: an attach under this suite can take seconds.
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let traffic = {
        let done = done.clone();
        std::thread::spawn(move || {
            let mut ports = Vec::new();
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            let listening = listener.local_addr().expect("an address").port();
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                let mut client = TcpStream::connect(listener.local_addr().expect("an address"))
                    .expect("connect");
                ports.push(client.local_addr().expect("a local address").port());
                let (mut server, _) = listener.accept().expect("accept");
                client.write_all(&[7u8; 4096]).expect("send");
                let mut sink = [0u8; 4096];
                server.read_exact(&mut sink).expect("receive");
                drop(client);
                drop(server);
                pause(20);
            }
            (listening, ports)
        })
    };
    let outcome = measuring.wait_with_output().expect("omn should finish");
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let (listening, ports) = traffic.join().expect("the traffic thread");
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);
    let closed = fx_raw(labelled(&run, "closed"));
    let opened = fx_raw(labelled(&run, "opened"));
    // One of this test's connections, by both of its ports: the client's
    // port and the listener's. Both sources are machine-wide, and another
    // loopback connection in the window may reuse a client port -- which is
    // how matching on `sport` alone compared two connections' cookies (the
    // flaky tally's #17) -- but not with this listener at the other end.
    let ours = |event: &BTreeMap<String, serde_json::Value>, port: u16| {
        fx_u64(event, "sport") == u64::from(port) && fx_u64(event, "dport") == u64::from(listening)
    };
    // The connections that both opened and closed inside the window.
    let mine: Vec<u16> = ports
        .iter()
        .copied()
        .filter(|port| {
            opened.iter().any(|event| ours(event, *port))
                && closed.iter().any(|event| ours(event, *port))
        })
        .collect();
    assert!(
        mine.len() >= 3,
        "{} connections made, {} of them seen at both ends inside the window",
        ports.len(),
        mine.len()
    );
    for port in &mine {
        let open = opened
            .iter()
            .find(|event| ours(event, *port))
            .expect("the handshake");
        let close = closed
            .iter()
            .find(|event| ours(event, *port))
            .expect("the close");
        assert_eq!(
            open.get("cookie"),
            close.get("cookie"),
            "the connection from port {port} carries one cookie from its handshake to its close"
        );
    }
    let joins = run["joins"].to_string();
    assert!(
        joins.contains("handshake-to-close") && joins.contains("KEY"),
        "the join is reported, with KEY evidence: {joins}"
    );
}

/// Every packet a program sent through a queueing discipline is in a row or a
/// counted loss, on the collector's own 100 ms tick -- the drain race stream K
/// closed in `sched`, `proc` and `mem`, closed here by moving `net_qdisc.rs`
/// onto `map::Rows`.
///
/// Four senders in a namespace of their own push small datagrams through a
/// `netem` with no delay on a dummy device for a second and a half -- named
/// `omnq0`, because `a_queue_that_delays_every_packet_is_measured_as_that_delay`
/// may be running beside it on a `dummy0` of its own, and a raw tracepoint sees
/// every namespace's -- each
/// counting what the kernel accepted. `netem` queues every packet -- a `pfifo`
/// would not: an empty one lets a packet bypass it, and the tracepoints never
/// see it -- so each accepted datagram is one enqueue and one dequeue, and the
/// dequeues land in rows drained fifteen times while they are written. Exact,
/// not close: the senders' count is theirs, and a packet in neither a row nor
/// `losses` is the defect.
#[test]
fn every_packet_through_a_discipline_is_in_a_row_or_a_counted_loss_on_a_short_tick() {
    if !can_build_a_network() {
        eprintln!(
            "skipped: this scenario needs iproute2 and an unprivileged user namespace to build a \
             device with a queueing discipline; loopback is `noqueue` on every machine"
        );
        return;
    }
    if !Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|outcome| outcome.status.success())
    {
        eprintln!("skipped: this machine has no `python3`, which the sending workload is in");
        return;
    }
    let measuring = Command::new(BINARY)
        .args([
            "net.qdisc",
            "--system",
            "-d",
            "3s",
            "-m",
            "trace",
            "-e",
            "raw",
            "--format",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    // A device name of this process's own. The tracepoints see every
    // namespace, and another checkout's suite running this same scenario at
    // the same moment makes an `omnq0` of its own: measured, 708253 dequeues
    // counted for 632000 sent, and alone it passes.
    let device = format!("omnq{}", std::process::id() % 100_000);
    let workload = workload_in_a_namespace(
        &"set -e
         ip link add omnq0 type dummy
         tc qdisc add dev omnq0 root netem limit 1000000
         ip addr add 10.99.0.1/24 dev omnq0
         ip link set omnq0 up
         ip neigh add 10.99.0.2 lladdr 02:00:00:00:00:02 dev omnq0
         sleep 0.5
         python3 -c 'import os, socket, time
reader, writer = os.pipe()
for _ in range(4):
    if os.fork() == 0:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sent, until = 0, time.monotonic() + 1.5
        while time.monotonic() < until:
            for _ in range(100):
                try:
                    s.sendto(b\"x\" * 16, (\"10.99.0.2\", 9999))
                    sent += 1
                except OSError:
                    pass
        os.write(writer, (str(sent) + \"\\n\").encode())
        os._exit(0)
for _ in range(4):
    os.wait()
os.close(writer)
print(\"omn-sent\", sum(int(line) for line in os.fdopen(reader).read().split()))'"
            .replace("omnq0", &device),
    );
    let built = workload
        .wait_with_output()
        .expect("the workload should run");
    let outcome = measuring.wait_with_output().expect("omn should finish");
    if unprivileged(&stderr(&outcome)) {
        eprintln!(
            "skipped: net.qdisc needs CAP_BPF and CAP_PERFMON, which this machine does not grant"
        );
        return;
    }
    if !built.status.success() {
        eprintln!(
            "skipped: this machine would not build the device the scenario measures: {}",
            stderr(&built)
        );
        return;
    }
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let sent: u64 = String::from_utf8_lossy(&built.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("omn-sent "))
        .and_then(|count| count.trim().parse().ok())
        .unwrap_or_else(|| panic!("the senders say what they sent: {built:?}"));
    let run = results(&outcome);
    let counted: u64 = raw_events(&run, "net.qdisc")
        .iter()
        .map(attrs)
        .filter(|attributes| attributes["iface"].as_str() == Some(device.as_str()))
        .map(|attributes| attributes["count"].as_u64().expect("a count"))
        .sum();
    let losses = &labelled(&run, "net.qdisc")["losses"];
    let lost: u64 = ["dropped", "undecoded", "throttled"]
        .iter()
        .filter_map(|field| losses[*field].as_u64())
        .sum();
    eprintln!("sent {sent}, counted {counted}, lost {lost}");
    assert!(sent > 10_000, "the senders sent only {sent} datagrams");
    assert!(
        counted <= sent,
        "{counted} dequeues counted for {sent} datagrams sent: one was counted twice"
    );
    assert!(
        counted + lost >= sent,
        "{sent} datagrams went through the discipline, {counted} are in rows and {lost} are \
         counted as lost: {} vanished without a trace; {losses}",
        sent - counted - lost
    );
}

// ---------------------------------------------- Class: a request's own bytes
//
// `net.request`'s `rx_bytes`, `tx_bytes` and `holder_since_ns`, held to what a
// server and its client did rather than to anything the tool decoded. The
// server is the vertical case's (`examples/vertical/server.c`), built here
// under a name of its own so this file never races `tests/vertical.rs` for the
// binary; it declares each request with `omn:request__start(id, fd)` and
// `omn:request__done(id, fd)`, and its client sends one request per
// connection.

/// The vertical case's server, built for this file once per process.
fn usdt_server() -> Result<std::path::PathBuf, String> {
    static BUILT: std::sync::OnceLock<Result<std::path::PathBuf, String>> =
        std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            const SOURCE: &str = include_str!("../examples/vertical/server.c");
            let dir =
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/omn-scenario-net");
            std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
            let source = dir.join("server.c");
            std::fs::write(&source, SOURCE).map_err(|error| error.to_string())?;
            let path = dir.join("server");
            let partial = dir.join(format!("server.{}", std::process::id()));
            let built = Command::new("cc")
                .args(["-O0", "-pthread", "-o"])
                .arg(&partial)
                .arg(&source)
                .output()
                .map_err(|error| format!("no C compiler (`cc`): {error}"))?;
            if !built.status.success() {
                return Err(format!(
                    "the server did not build, most likely for want of <sys/sdt.h>: {}",
                    stderr(&built)
                ));
            }
            std::fs::rename(&partial, &path).map_err(|error| error.to_string())?;
            Ok(path)
        })
        .clone()
}

/// One `vertical: key=value ...` line the server printed.
fn said(text: &str) -> Vec<BTreeMap<String, String>> {
    text.lines()
        .filter_map(|line| line.strip_prefix("vertical: "))
        .map(|line| {
            line.split_whitespace()
                .filter_map(|pair| pair.split_once('='))
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect()
        })
        .collect()
}

/// Each request's `rx_bytes` is the length of the request the client sent,
/// `tx_bytes` the length of the response the worker wrote, and the start of
/// its longest sleep is where the worker began waiting for the lock.
///
/// The ground truth is chosen here and printed by the programs, never read off
/// the tool: the request line is the client's format with this plan's numbers
/// in it, so its length is known before the run (four lengths, one of them
/// sent in two halves 20 ms apart); the response is the server's header and a
/// body of a length the plan chose, and the worker prints how many bytes its
/// `send` calls returned. The lock's request holds `holder_since_ns` between
/// the worker's own birth stamp and the end of its own clock around the lock,
/// less the holder's own clock around its sleep under it.
///
/// An arithmetic slip in `copied_seq` or `write_seq`, or a start claimed from
/// the wrong arrival, is off by a request's length or more; nothing here has a
/// tolerance on a byte count.
#[test]
fn a_requests_bytes_are_what_its_client_sent_and_its_worker_wrote() {
    let server = match usdt_server() {
        Ok(server) => server,
        Err(reason) => {
            eprintln!("skipped: this machine cannot serve a USDT server: {reason}");
            return;
        }
    };
    let data = server.with_file_name("data");
    // lock_ms:reads:gap_ms:body. A body of 0 is the server's two-byte answer
    // read at once; any other is read through a small receive buffer.
    const PLAN: [(u64, u64, u64, u64); 5] = [
        (0, 0, 0, 0),
        (0, 0, 0, 7),
        (0, 0, 20, 300),
        (0, 0, 0, 20_000),
        (40, 0, 0, 0),
    ];
    let plan: Vec<String> = PLAN
        .iter()
        .map(|(lock, reads, gap, body)| format!("{lock}:{reads}:{gap}:{body}"))
        .collect();
    let mut args = vec![
        "net.request",
        "--mode",
        "trace",
        "--emit",
        "raw",
        "--keys",
        "req",
        "--format",
        "json",
        "--",
        server.to_str().expect("a utf-8 path"),
        data.to_str().expect("a utf-8 path"),
    ];
    args.extend(plan.iter().map(String::as_str));
    let outcome = omn(&args);
    let complaint = stderr(&outcome);
    if unprivileged(&complaint) {
        eprintln!(
            "skipped: net.request needs CAP_BPF and CAP_PERFMON, which this machine does not \
             grant"
        );
        return;
    }
    assert_eq!(code(&outcome), 0, "{complaint}");
    let run = results(&outcome);
    let events = raw_events(&run, "net.request");
    let said = said(&complaint);
    let worker = |id: &str| {
        said.iter()
            .find(|line| {
                line.get("role").map(String::as_str) == Some("worker")
                    && line.get("req").map(String::as_str) == Some(id)
            })
            .unwrap_or_else(|| panic!("the worker said nothing about req {id}:\n{complaint}"))
    };
    let number = |line: &BTreeMap<String, String>, name: &str| -> u64 {
        line.get(name)
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("the server said no `{name}`: {line:?}"))
    };

    for (index, (lock, reads, _, body)) in PLAN.iter().enumerate() {
        let id = format!("{:x}", 0xa001 + index);
        let event = events
            .iter()
            .find(|event| {
                event["keys"].as_array().is_some_and(|keys| {
                    keys.iter()
                        .any(|key| key["req"]["id"].as_str() == Some(id.as_str()))
                })
            })
            .unwrap_or_else(|| panic!("no net.request event for req:{id} in {events:?}"));
        let attributes = attrs(event);
        let count = |name: &str| {
            attributes
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_else(|| panic!("req:{id} carries no `{name}`: {attributes:?}"))
        };

        let request =
            format!("GET /?lock={lock}&reads={reads}&body={body} HTTP/1.1\r\nHost: omn\r\n\r\n");
        assert_eq!(
            count("rx_bytes"),
            request.len() as u64,
            "req:{id}'s client sent a {}-byte request",
            request.len()
        );

        let length = if *body == 0 { 2 } else { *body };
        let header =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n");
        let wrote = number(worker(&id), "sent");
        assert_eq!(
            wrote,
            header.len() as u64 + length,
            "the worker's own count of req:{id}'s response"
        );
        assert_eq!(
            count("tx_bytes"),
            wrote,
            "req:{id}'s worker wrote {wrote} bytes"
        );

        if *lock == 0 {
            continue;
        }
        let born = number(worker(&id), "born_ns");
        let lock_ns = number(worker(&id), "lock_ns");
        let held_ns = number(
            said.iter()
                .find(|line| line.contains_key("held_ns"))
                .expect("the holder says how long it held the lock"),
            "held_ns",
        );
        let since = count("holder_since_ns");
        // The worker cannot have begun waiting before its birth, and the wait
        // on the mutex lasted at least the holder's sleep under it, so it began
        // that long before the worker's own clock around the lock ended. Five
        // milliseconds of slack for the worker's wake-up after the holder's
        // signal, against a 40 ms sleep.
        const SLACK_NS: u64 = 5_000_000;
        assert!(
            since >= born,
            "req:{id}'s longest sleep began {} ns before its birth",
            born - since
        );
        assert!(
            since + held_ns <= born + lock_ns + SLACK_NS,
            "req:{id}'s longest sleep began {} ns after its birth, too late to have lasted the \
             holder's {held_ns} ns under the lock within the worker's {lock_ns} ns around it",
            since - born
        );
    }
    let fields = &labelled(&run, "net.request")["fields"];
    for name in ["never_ended", "unborn", "replaced", "filtered_out"] {
        assert_eq!(fields[name], 0, "`{name}` in {fields}");
    }
}
