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
    let outcome = omn(&[
        "-f",
        &session("net-capability-sweep.yml"),
        "--format",
        "json",
    ]);
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);

    let expected: &[(&str, &[&str])] = &[
        ("volume", &["timeline", "summary"]),
        ("packets", &["timeline", "summary"]),
        ("links", &["raw", "timeline", "summary"]),
        ("accept-queue", &["raw", "timeline", "summary"]),
        ("round-trip", &["raw", "timeline", "summary"]),
        ("nic-clocks", &["raw", "timeline", "summary"]),
    ];
    for (label, wanted) in expected {
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
    // because it is both: the floor is the transfer and the ceiling allows the
    // doubling plus whatever else is talking to localhost.
    let bytes = rate_total(labelled(&run, "volume"), "iface:lo");
    assert!(
        bytes >= MOVED as f64,
        "{MOVED} bytes went through loopback and the counters saw {bytes}"
    );
    assert!(
        bytes <= 8.0 * MOVED as f64,
        "the counters saw {bytes} for a {MOVED} byte transfer, which is more than loopback \
         doubling explains"
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
    let outcome = omn(&["-f", &session("net-snapshot.yml"), "--format", "json"]);
    assert_eq!(code(&outcome), 0, "{}", stderr(&outcome));
    let run = results(&outcome);

    for label in ["links", "accept-queue", "nic-clocks"] {
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

    // And the refusal a reader actually meets, which now names the source and
    // the emits it does have. Against the old entry this said `mode `count`
    // cannot produce `topk``, which pointed at a rule in another file.
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
    assert_eq!(code(&outcome), 2, "{}", stderr(&outcome));
    assert!(
        stderr(&outcome).contains("`net.bytes` cannot emit `topk`"),
        "the refusal should name the source and the emit: {}",
        stderr(&outcome)
    );
    assert!(
        stderr(&outcome).contains("this source emits rate, timeline, summary"),
        "and the hint should be what the entry declares: {}",
        stderr(&outcome)
    );
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
            &["--pid", "1", "--mode", "snapshot", "--emit", "summary"],
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

    assert_eq!(
        seen("snapshotted"),
        1,
        "`snapshot` means one read, and the request beside it does not change that: \
         {document}"
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
        (
            // Not declared by the source at all, since the entry stopped
            // promising a ranking neither of its modes can reach.
            &[
                "net.bytes",
                "--iface",
                "lo",
                "--mode",
                "count",
                "--emit",
                "topk:5",
            ],
            "`net.bytes` cannot emit `topk`",
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
         \x20 - {label: ranked-volume, source: net.bytes, target: {iface: lo}, mode: count, emit: [topk:5], keys: [iface]}\n",
    )
    .expect("write the session");
    let outcome = omn(&["validate", "-f", path.to_str().expect("a path")]);
    assert_eq!(code(&outcome), 2, "{}", stdout(&outcome));
    let complaint = stderr(&outcome);
    assert!(
        complaint.matches("error:").count() >= 5,
        "only {} refusals were reported for four bad requests; a validator that stops at the          first turns one session into five runs:\n{complaint}",
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
#[test]
fn a_net_source_this_build_does_not_collect_exits_three_and_names_the_backend() {
    let outcome = omn(&["-f", &session("net-not-built.yml")]);
    assert_eq!(code(&outcome), 3, "{}", stderr(&outcome));
    let complaint = stderr(&outcome);
    assert!(
        complaint.contains("net.drops") && complaint.contains("bpf:tp"),
        "the refusal should name the source and the backend it needs: {complaint}"
    );
    assert!(
        complaint.contains("The request itself is valid"),
        "exit 3 is not a failure of the request and should not read like one: {complaint}"
    );

    // Each of the eight, on its own, for the message rather than the code.
    let unbuilt: &[(&str, &str, &str)] = &[
        ("net.tcp.connect", "trace", "bpf:fentry"),
        ("net.tcp.lifetime", "trace", "bpf:tp"),
        ("net.tcp.retransmit", "count", "bpf:tp"),
        ("net.drops", "count", "bpf:tp"),
        ("net.socket", "trace", "bpf:fentry"),
        ("net.qdisc", "trace", "bpf:tp"),
        ("net.softirq", "count", "bpf:tp"),
        ("net.xdp", "count", "bpf"),
    ];
    for (source, mode, backend) in unbuilt {
        let mut args: Vec<&str> = vec![source];
        if *source == "net.xdp" {
            args.extend_from_slice(&["--iface", "lo"]);
        } else {
            args.push("--system");
        }
        let emit = if *mode == "count" { "rate" } else { "raw" };
        args.extend_from_slice(&["--mode", mode, "--duration", "200ms", "--emit", emit]);
        let outcome = omn(&args);
        assert_eq!(
            code(&outcome),
            3,
            "`{source}` exited {} rather than 3:\n{}",
            code(&outcome),
            stderr(&outcome)
        );
        assert!(
            stderr(&outcome).contains(backend),
            "`{source}` should name `{backend}` as what is missing: {}",
            stderr(&outcome)
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
        let attr = |name: &str| {
            event["attrs"]
                .as_array()
                .expect("attributes")
                .iter()
                .find(|attr| attr["name"] == name)
                .and_then(|attr| attr["value"].as_u64())
                .unwrap_or_else(|| panic!("no `{name}` on {event}"))
        };
        drops += attr("drops");
        errors += attr("errors");
        assert_eq!(attr("tx_dropped"), attr("drops") - attr("rx_dropped"));
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

    // And the finding the scenario also produces: the measured value of this
    // source is errors, so every aggregating emit answers zero for a link that
    // just lost a thousand frames. That is still true -- errors and drops are
    // kept apart on purpose, and the catalog's caveat says so -- but it is no
    // longer silent.
    assert_eq!(
        rate_total(result, "iface:veth0"),
        0.0,
        "the rate stopped reporting errors alone; if drops are now in it, update the \
         \"Defects found\" section of docs/scenarios/net.md"
    );
    let said = result["degradations"]
        .as_array()
        .map(|lines| {
            lines
                .iter()
                .filter_map(|line| line.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    assert!(
        said.contains("veth0") && said.contains(&format!("{DATAGRAMS} drops")),
        "a rate of zero over {DATAGRAMS} dropped frames has to say what it is leaving out, and \
         the run says:\n{said}"
    );
    assert!(
        said.contains("every emit but `raw`"),
        "the note has to say where the number it is leaving out can be read:\n{said}"
    );
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
/// The address does not work the same way, and that is a finding rather than a
/// lesson. `daddr` carries the address *and* the port -- `127.0.0.1:40123` --
/// so an equality against an address matches nothing, forever, and the run
/// reports having kept zero of six hundred events without saying why. `=~`
/// finds them. Recorded in `docs/scenarios/net.md`.
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

    // The same connection, asked for by address. The attribute it compares
    // against is `127.0.0.1:<port>`, so this can never match.
    let by_address = kept("daddr == '127.0.0.1'");
    assert_eq!(
        by_address["events_seen"].as_u64(),
        Some(0),
        "`daddr ==` started matching; if the attribute is now the address alone, update this \
         assertion and the \"Defects found\" section of docs/scenarios/net.md"
    );

    // And the way that does work, which has to find at least the connection
    // above, since every loopback connection on the machine is in this set.
    let by_pattern = kept("daddr =~ '127.0.0.1'");
    assert!(
        by_pattern["events_seen"].as_u64().unwrap_or(0) >= events.len() as u64,
        "matching the address as a pattern found fewer connections than matching the port"
    );

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
/// has written it. The message says exactly that, and then the run exits 1,
/// which is the code for a failure of this run. Recorded in
/// `docs/scenarios/net.md` under "Defects found": the same refusal about a
/// *target* was moved to exit 3 and this one about a *mode* was not.
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
        complaint.contains("so_timestamping") && complaint.contains("not built"),
        "the refusal should name the backend a per-packet answer needs: {complaint}"
    );
    // `DECISIONS.md` settled that "valid, and not built is exit 3, wherever it
    // is discovered". A mode is the same answer and used to exit 1, which is
    // defect 7 of docs/scenarios/net.md; the refusal is raised by the factory,
    // before an interface has been touched, so it can only be about this build.
    assert_eq!(
        code(&outcome),
        3,
        "a mode that resolves and is then refused as not built is exit 3: {complaint}"
    );
}
