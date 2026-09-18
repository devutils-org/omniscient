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
