//! Ground-truth tests for the `net` family.
//!
//! A counter that returns a plausible number passes every test that only checks
//! the code against itself. These push a known amount of traffic through a known
//! interface and ask whether the measurement saw it.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output, Stdio};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Moves `bytes` over a loopback connection, and returns once both ends are
/// done so the counters have settled.
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

#[test]
fn a_known_loopback_transfer_shows_up_in_the_interface_counters() {
    const MOVED: usize = 16 * 1024 * 1024;

    let measuring = Command::new(BINARY)
        .args([
            "net.bytes",
            "--iface",
            "lo",
            // `count` is the mode for a counter aggregated over a window;
            // `poll` samples a level, and the validator keeps the two apart.
            "--mode",
            "count",
            "--duration",
            "2s",
            "--emit",
            "rate",
            "--format",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");

    // Let the first poll establish its baseline before any traffic moves.
    std::thread::sleep(std::time::Duration::from_millis(400));
    push_through_loopback(MOVED);

    let output = measuring.wait_with_output().expect("omn should finish");
    assert!(output.status.success(), "{}", stderr(&output));

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("json results");
    let series = run["results"][0]["outputs"][0]["series"]
        .as_array()
        .expect("a rate per interface");
    let loopback = series
        .iter()
        .find(|entry| entry["key"] == "iface:lo")
        .expect("the loopback series");
    let total = loopback["total"].as_f64().expect("total bytes");

    // Loopback counts every byte twice, once received and once transmitted, so
    // the floor is the transfer itself and the ceiling allows for the doubling
    // plus whatever else is talking to localhost.
    assert!(
        total >= MOVED as f64,
        "moved {MOVED} bytes over loopback but the counters saw {total}"
    );
    assert!(
        total <= 8.0 * MOVED as f64,
        "the counters saw {total} for a {MOVED} byte transfer, which is more than loopback doubling explains"
    );
}

/// Per-process network volume is real, but not from `/proc/net/dev`: that file
/// has no idea which process moved what. The honest outcome is to name the
/// backend that would answer, not to divide a machine-wide number by something.
#[test]
fn per_process_volume_names_the_backend_it_needs_instead_of_dividing_a_machine_wide_number() {
    let output = Command::new(BINARY)
        .args([
            "net.bytes",
            "--pid",
            "1",
            "--mode",
            "count",
            "--duration",
            "200ms",
            "--emit",
            "rate",
        ])
        .output()
        .expect("omn should run");
    assert!(!output.status.success());

    let complaint = stderr(&output);
    assert!(
        complaint.contains("bpf"),
        "the refusal should name the backend a per-process answer needs: {complaint}"
    );
    assert!(
        !stdout(&output).contains("iface:"),
        "a machine-wide number must not be presented for a process target"
    );
}

/// Ground truth for `net.backlog`.
///
/// A listener with a one-deep accept queue, and far more connections than it
/// will ever accept, is the situation those counters exist to name. The test
/// creates it and asks whether the measurement saw it -- which is the only way
/// to tell a counter that works from one that reports a plausible zero.
#[test]
fn an_overflowing_accept_queue_shows_up_in_the_backlog_counters() {
    let (listener, port) = listener_with_backlog(1);

    let measuring = Command::new(BINARY)
        .args([
            "net.backlog",
            "--system",
            "--mode",
            "count",
            "--duration",
            "2s",
            "--emit",
            "summary",
            "--format",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");

    std::thread::sleep(std::time::Duration::from_millis(400));

    // Connect far more times than a one-deep queue can hold, and never accept.
    // The handshakes complete, the queue fills, and the kernel starts counting.
    //
    // Each connect is bounded: once the queue is full the kernel stops
    // answering, and a plain `connect` then waits out the SYN retransmissions,
    // which is minutes. A refused or timed-out connection is the behaviour
    // being measured, not a failure of the test.
    let address: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().expect("address");
    let mut held = Vec::new();
    let mut refused = 0;
    for _ in 0..64 {
        match TcpStream::connect_timeout(&address, std::time::Duration::from_millis(50)) {
            Ok(stream) => held.push(stream),
            Err(_) => refused += 1,
        }
    }
    assert!(
        !held.is_empty() || refused > 0,
        "neither a connection nor a refusal: the listener was never reachable"
    );
    std::thread::sleep(std::time::Duration::from_millis(600));

    let output = measuring.wait_with_output().expect("omn should finish");
    assert!(output.status.success(), "{}", stderr(&output));
    drop(held);
    drop(listener);

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("json results");
    let series = run["results"][0]["outputs"][0]["series"]
        .as_array()
        .expect("a summary for the machine");
    let machine = series
        .iter()
        .find(|entry| entry["key"] == "system")
        .expect("a machine-wide series");

    // `drops` is the measured value; the kernel may instead answer an
    // overflowing queue with syncookies, so the raw events carry both. What
    // must not happen is every counter staying at zero while the queue was
    // demonstrably overflowing.
    let drops = machine["max"].as_f64().unwrap_or(0.0);
    assert!(
        drops > 0.0,
        "200 connections were made to a one-deep accept queue and the counters reported none: \
         {machine}"
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

// ---------------------------------------------------------------------------
// `net.capture`
// ---------------------------------------------------------------------------

/// Ground truth for `net.capture`.
///
/// A hundred datagrams of a hundred bytes go over loopback, and the capture has
/// to report a hundred frames of exactly the length those datagrams make on the
/// wire -- in each direction, because loopback both sends and receives every one
/// of them. Nothing here trusts the capture to check itself: the count, the
/// size, the port and the protocol were all chosen by this test.
///
/// The capture needs `CAP_NET_RAW`, which the build does not grant. Rather than
/// fall silent on a machine that could serve the request, the test creates a
/// user namespace where it holds the capability and a network namespace with a
/// loopback of its own, and runs itself again in there. A machine that will not
/// allow even that genuinely cannot serve the request, and only then does this
/// stop -- loudly.
#[test]
fn a_known_number_of_packets_over_loopback_is_captured_as_they_were_sent() {
    if std::env::var_os(IN_NAMESPACE).is_some() {
        bring_loopback_up();
        capture_a_hundred_datagrams();
        return;
    }
    if packet_socket_opens() {
        capture_a_hundred_datagrams();
        return;
    }
    run_this_test_again_inside_a_namespace(
        "a_known_number_of_packets_over_loopback_is_captured_as_they_were_sent",
    );
}

/// The capture is a privacy boundary, and the boundary is the kernel's rather
/// than this code's: the classic BPF filter on the socket caps what is copied at
/// the headers, so the payload never reaches the process.
///
/// The check is that a frame whose payload is a hundred bytes of a byte value
/// this test chose reports its full length on the wire and a captured length
/// well short of it -- and that the value itself appears nowhere in the output.
#[test]
fn a_capture_reports_the_whole_frame_and_holds_none_of_its_payload() {
    if std::env::var_os(IN_NAMESPACE).is_some() {
        bring_loopback_up();
        payload_never_reaches_the_output();
        return;
    }
    if packet_socket_opens() {
        payload_never_reaches_the_output();
        return;
    }
    run_this_test_again_inside_a_namespace(
        "a_capture_reports_the_whole_frame_and_holds_none_of_its_payload",
    );
}

/// How many datagrams the two capture tests send, and how big each one is.
const DATAGRAMS: usize = 100;
const PAYLOAD: usize = 100;
/// What each of them is on the wire: loopback's link header, an IPv4 header
/// with no options, a UDP header, and the payload.
const ON_THE_WIRE: u64 = 14 + 20 + 8 + PAYLOAD as u64;

fn capture_a_hundred_datagrams() {
    let events = capture_while_sending(0x5a);

    let mut ours = 0;
    let mut outgoing = 0;
    let mut received = 0;
    for event in &events {
        let attr = |name: &str| {
            event["attrs"]
                .as_array()
                .expect("attributes")
                .iter()
                .find(|attr| attr["name"] == name)
                .map(|attr| attr["value"].clone())
        };
        if attr("dport").and_then(|value| value.as_u64()) != Some(PORT as u64) {
            continue;
        }
        ours += 1;
        assert_eq!(
            attr("bytes").and_then(|value| value.as_u64()),
            Some(ON_THE_WIRE),
            "a {PAYLOAD}-byte datagram is {ON_THE_WIRE} bytes on the wire: {event}"
        );
        assert_eq!(
            attr("protocol").and_then(|value| value.as_str().map(str::to_owned)),
            Some("udp".to_owned())
        );
        assert_eq!(
            attr("daddr").and_then(|value| value.as_str().map(str::to_owned)),
            Some("127.0.0.1".to_owned())
        );
        // A frame from another clock domain never claims to be exact.
        assert!(
            event["ts_err_ns"].as_u64().unwrap_or(0) > 0,
            "a converted timestamp with no error: {event}"
        );
        match attr("direction").and_then(|value| value.as_str().map(str::to_owned)) {
            Some(direction) if direction == "outgoing" => outgoing += 1,
            Some(_) => received += 1,
            None => panic!("a captured frame with no direction: {event}"),
        }
    }

    assert_eq!(
        outgoing, DATAGRAMS,
        "{DATAGRAMS} datagrams were sent and the capture saw {outgoing} leave"
    );
    assert_eq!(
        received, DATAGRAMS,
        "{DATAGRAMS} datagrams were delivered over loopback and the capture saw {received} \
         arrive; loopback carries every frame in both directions"
    );
    assert_eq!(ours, outgoing + received);
}

fn payload_never_reaches_the_output() {
    // A byte value that would be unmistakable in the output if it got there.
    let events = capture_while_sending(0xa5);
    let ours = events
        .iter()
        .find(|event| {
            event["attrs"]
                .as_array()
                .expect("attributes")
                .iter()
                .any(|attr| attr["name"] == "dport" && attr["value"] == PORT as u64)
        })
        .expect("the datagrams this test sent");

    let captured = ours["attrs"]
        .as_array()
        .expect("attributes")
        .iter()
        .find(|attr| attr["name"] == "captured_bytes")
        .and_then(|attr| attr["value"].as_u64())
        .expect("how much of the frame was copied");

    assert!(
        captured < ON_THE_WIRE,
        "the whole frame was copied into this process ({captured} of {ON_THE_WIRE} bytes); the \
         filter that caps the copy is not doing its job"
    );
    assert!(
        captured >= 14 + 20 + 8,
        "the headers were cut short at {captured} bytes, so the ports below are guesses"
    );

    // The payload was a hundred repetitions of one byte. Nothing resembling it
    // can be in the output, because nothing resembling it was ever received.
    let text = serde_json::to_string(&events).expect("the events as text");
    assert!(
        !text.contains("165,165,165") && !text.to_lowercase().contains("a5a5a5"),
        "something that looks like the payload is in the output"
    );
}

const PORT: u16 = 39999;

/// Runs a capture on loopback and sends `DATAGRAMS` datagrams into it.
///
/// Returns the raw events the run produced.
fn capture_while_sending(fill: u8) -> Vec<serde_json::Value> {
    let capturing = Command::new(BINARY)
        .args([
            "net.capture",
            "--iface",
            "lo",
            "--mode",
            "trace",
            "--duration",
            "2s",
            "--emit",
            "raw",
            "--format",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");

    // The ring has to be open before anything is sent, or the frames go by
    // while the socket is still being set up.
    std::thread::sleep(std::time::Duration::from_millis(700));

    // Bound, so the datagrams are delivered rather than answered with an ICMP
    // port-unreachable that would show up in the capture as well.
    let receiver =
        std::net::UdpSocket::bind(("127.0.0.1", PORT)).expect("nothing else holds this port");
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").expect("an ephemeral port");
    let payload = vec![fill; PAYLOAD];
    for _ in 0..DATAGRAMS {
        sender
            .send_to(&payload, ("127.0.0.1", PORT))
            .expect("send over loopback");
    }
    drop(receiver);

    let output = capturing.wait_with_output().expect("omn should finish");
    assert!(output.status.success(), "{}", stderr(&output));

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("json results");
    let losses = &run["results"][0]["losses"];
    assert_eq!(
        losses["dropped"].as_u64().unwrap_or(0),
        0,
        "the capture dropped frames, so the counts below would mean nothing: {losses}"
    );
    run["results"][0]["outputs"][0]["events"]
        .as_array()
        .expect("the captured frames")
        .clone()
}

/// The environment variable that tells a re-executed test it is already inside
/// the namespace it asked for.
const IN_NAMESPACE: &str = "OMN_TEST_IN_NETNS";

/// Whether this process may open a packet socket at all.
fn packet_socket_opens() -> bool {
    // SAFETY: creating a socket; the arguments are constants.
    let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW | libc::SOCK_CLOEXEC, 0) };
    if fd >= 0 {
        // SAFETY: closing a descriptor just created here.
        unsafe { libc::close(fd) };
        return true;
    }
    false
}

/// Runs this same test again in a user and network namespace of its own.
///
/// `unshare -Ur -n` maps this user to root inside a new user namespace, which
/// carries `CAP_NET_RAW` in it, and gives the new network namespace a loopback
/// interface nothing else is using. That is the whole reason the counts above
/// can be exact: the only traffic on that loopback is this test's.
fn run_this_test_again_inside_a_namespace(name: &str) {
    let binary = std::env::current_exe().expect("the test binary");
    let outcome = Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "--"])
        .arg(&binary)
        .args(["--exact", name, "--nocapture", "--test-threads", "1"])
        .env(IN_NAMESPACE, "1")
        .output();

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => panic!(
            "this process may not open a packet socket, and `unshare` is not available to make \
             a namespace where it could ({error}). This machine cannot serve `net.capture`: \
             grant CAP_NET_RAW, or allow unprivileged user namespaces."
        ),
    };
    let complaint = stderr(&outcome);
    if !outcome.status.success()
        && (complaint.contains("Operation not permitted") || complaint.contains("unshare failed"))
        && !complaint.contains("panicked")
    {
        panic!(
            "this machine allows neither a packet socket nor an unprivileged user namespace, so \
             `net.capture` cannot be measured here at all: {complaint}"
        );
    }
    assert!(
        outcome.status.success(),
        "the capture failed inside its own namespace:\n{}\n{complaint}",
        stdout(&outcome)
    );
}

/// Brings the loopback interface of the current network namespace up.
///
/// A fresh network namespace has a loopback and it is down, so nothing is
/// delivered over it. This is `ip link set lo up`, done here rather than by
/// calling `ip`, because the test already holds `CAP_NET_ADMIN` inside the
/// namespace and one binary fewer is one dependency fewer.
fn bring_loopback_up() {
    /// `struct ifreq`: a name and a union, of which only the flags are used.
    #[repr(C)]
    struct IfReq {
        name: [u8; libc::IF_NAMESIZE],
        flags: libc::c_short,
        padding: [u8; 22],
    }

    const SIOCGIFFLAGS: libc::c_ulong = 0x8913;
    const SIOCSIFFLAGS: libc::c_ulong = 0x8914;

    // SAFETY: each call is checked, and the descriptor is closed once.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());

        let mut request = IfReq {
            name: [0; libc::IF_NAMESIZE],
            flags: 0,
            padding: [0; 22],
        };
        request.name[..2].copy_from_slice(b"lo");

        assert_eq!(
            libc::ioctl(fd, SIOCGIFFLAGS, &raw mut request),
            0,
            "reading the loopback's flags: {}",
            std::io::Error::last_os_error()
        );
        request.flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        assert_eq!(
            libc::ioctl(fd, SIOCSIFFLAGS, &raw const request),
            0,
            "bringing the loopback up: {}",
            std::io::Error::last_os_error()
        );
        libc::close(fd);
    }
}

// ---------------------------------------------------------------------------
// `net.hwts`
// ---------------------------------------------------------------------------

/// An interface with no hardware clock has no offset to report, and reporting
/// zero would be a number where there is no measurement.
///
/// Loopback is the case that is true on every machine: it is `ARPHRD_LOOPBACK`,
/// a software interface with no card, so it cannot have a card's clock. The
/// refusal has to say where its timestamps do come from, because "`lo` cannot
/// do this" sends a reader looking for a flag and the actual answer is that the
/// kernel stamped them from the realtime clock.
#[test]
fn an_interface_with_no_clock_of_its_own_is_refused_rather_than_answered_with_zero() {
    let kind = std::fs::read_to_string("/sys/class/net/lo/type").expect("every machine has one");
    assert_eq!(kind.trim(), "772", "ARPHRD_LOOPBACK");

    let output = Command::new(BINARY)
        .args([
            "net.hwts", "--iface", "lo", "--mode", "snapshot", "--emit", "summary",
        ])
        .output()
        .expect("omn should run");
    assert!(!output.status.success());

    let complaint = stderr(&output);
    assert!(
        complaint.contains("CLOCK_REALTIME"),
        "the refusal should name the clock loopback's timestamps come from: {complaint}"
    );
    assert!(
        !stdout(&output).contains("offset_ns"),
        "no offset may be presented for an interface that has no clock"
    );
}

/// `trace` on this source means a timestamp per packet, which is a different
/// backend and is not built. Answering it with a per-interface reading would be
/// the silent degradation the catalog exists to prevent.
#[test]
fn a_per_packet_request_names_the_backend_it_needs_instead_of_reading_an_interface() {
    let output = Command::new(BINARY)
        .args([
            "net.hwts",
            "--iface",
            "lo",
            "--mode",
            "trace",
            "--duration",
            "200ms",
            "--emit",
            "raw",
        ])
        .output()
        .expect("omn should run");
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("so_timestamping"),
        "the refusal should name the backend a per-packet answer needs: {}",
        stderr(&output)
    );
}

/// Ground truth for `net.hwts`: two clocks and a known interval.
///
/// A NIC's clock and `CLOCK_MONOTONIC` are independent oscillators. Polled over
/// a known window, the offset between them may drift -- a free-running clock is
/// allowed to -- but it cannot move by anything like the window itself, because
/// then one of the two is not ticking. That is a check against something this
/// code does not produce: the passage of time.
#[test]
fn a_polled_hardware_clock_keeps_step_with_ours_over_a_known_window() {
    const WINDOW_MS: u64 = 600;

    let output = Command::new(BINARY)
        .args([
            "net.hwts",
            "--system",
            "--mode",
            "poll@100ms",
            "--duration",
            &format!("{WINDOW_MS}ms"),
            "--emit",
            "timeline",
            "--format",
            "json",
        ])
        .output()
        .expect("omn should run");

    if !output.status.success() {
        let complaint = stderr(&output);
        assert!(
            complaint.contains("readable PTP hardware clock"),
            "net.hwts failed for a reason other than this machine having no clock: {complaint}"
        );
        // A machine whose interfaces keep no clock, or keep one this user may
        // not read, cannot serve this request at all. That is a fact about the
        // machine and is reported as one, rather than passed over in silence.
        eprintln!("this machine cannot serve `net.hwts --system`: {complaint}");
        return;
    }

    let run: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("json results");
    let series = run["results"][0]["outputs"][0]["series"]
        .as_array()
        .expect("a timeline per interface")
        .clone();
    assert!(
        !series.is_empty(),
        "the run succeeded, so at least one interface reported an offset"
    );

    for entry in &series {
        let points = entry["points"].as_array().expect("points");
        assert!(
            points.len() >= 2,
            "a {WINDOW_MS}ms poll at 100ms produced {} points for {}",
            points.len(),
            entry["key"]
        );
        let first = points.first().expect("a first point")["value"]
            .as_f64()
            .expect("an offset");
        let last = points.last().expect("a last point")["value"]
            .as_f64()
            .expect("an offset");
        let elapsed_ns = points.last().expect("a last point")["offset_ns"]
            .as_f64()
            .expect("when it was taken");

        let moved = (last - first).abs();
        assert!(
            moved < elapsed_ns / 100.0,
            "over {elapsed_ns} ns the offset to {}'s clock moved {moved} ns, which is more than \
             one part in a hundred: that clock is not ticking at our rate",
            entry["key"]
        );
    }
}
