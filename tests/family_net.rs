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
