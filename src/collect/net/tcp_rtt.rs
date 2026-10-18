//! `net.tcp.rtt` over `sock_diag`.
//!
//! The kernel maintains a smoothed round-trip estimate for every established
//! connection, along with the congestion window and the congestion state. Those
//! three together answer a question a packet capture cannot: not what the
//! network did to one packet, but what the stack currently believes about the
//! path.
//!
//! Each connection is its own series, keyed by the socket cookie -- the
//! identity that survives softirq processing, and the one an eBPF backend will
//! carry too, so the two can be joined when that backend exists.

use std::collections::BTreeSet;

use super::netlink::{Diag, Socket, TcpInfoExtra};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::Target;

/// Only an established connection has a round-trip estimate. A listening or
/// closing socket reports zero, and reporting that as a measured latency of
/// zero would be a lie with a number attached.
const TCP_ESTABLISHED: u8 = 1;

pub struct Prepare;

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let only_pid = match target {
            Target::System => None,
            Target::Pid(pids) | Target::Tid(pids) => Some(pids[0]),
            Target::Launch(_) => None,
            other => {
                return Err(format!(
                    "connection state is read per process or machine-wide, and a {} target is \
                     neither",
                    other.kind()
                ))
            }
        };
        Ok(Box::new(TcpRttCollector {
            id: CollectorId::new("net.tcp.rtt/netlink"),
            only_pid,
            losses: Losses::default(),
            diag: None,
        }))
    }
}

pub struct TcpRttCollector {
    id: CollectorId,
    /// When set, only sockets this process holds open are reported.
    only_pid: Option<i32>,
    losses: Losses,
    diag: Option<Diag>,
}

impl TcpRttCollector {
    /// The socket inodes a process currently holds.
    ///
    /// `sock_diag` reports every socket on the machine; this is what narrows it
    /// to one process. The mapping is a directory of symlinks, so it is a
    /// snapshot: a connection opened and closed between two ticks is not seen,
    /// which is a property of polling rather than a loss.
    fn sockets_of(pid: i32) -> Result<BTreeSet<u32>, SampleError> {
        let directory = format!("/proc/{pid}/fd");
        let entries = std::fs::read_dir(&directory).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                SampleError::TargetGone(format!("process {pid} exited"))
            } else {
                SampleError::Transient(format!("{directory}: {error}"))
            }
        })?;

        let mut inodes = BTreeSet::new();
        for entry in entries.flatten() {
            let Ok(link) = std::fs::read_link(entry.path()) else {
                // A descriptor closed while the directory was being walked.
                continue;
            };
            let text = link.to_string_lossy();
            if let Some(inode) = text
                .strip_prefix("socket:[")
                .and_then(|rest| rest.strip_suffix(']'))
                .and_then(|digits| digits.parse().ok())
            {
                inodes.insert(inode);
            }
        }
        Ok(inodes)
    }

    fn event(&self, socket: &Socket, ts_ns: u64, seq: u64) -> Option<Event> {
        let info = socket.info?;
        Some(Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "net.tcp.rtt".to_owned(),
            keys: vec![KeyValue::Sock {
                cookie: socket.cookie,
            }],
            attrs: vec![
                // The kernel reports microseconds; the catalog's unit is
                // nanoseconds, and converting here means nothing downstream has
                // to know which source used which.
                Attr::uint("rtt_ns", info.rtt_us as u64 * 1_000),
                Attr::uint("rtt_var_ns", info.rtt_var_us as u64 * 1_000),
                Attr::uint("cwnd", info.send_congestion_window as u64),
                Attr::uint("ssthresh", info.send_slow_start_threshold as u64),
                Attr::uint("retransmits", info.retransmits as u64),
                Attr::uint("congestion_state", info.congestion_state as u64),
                Attr::text("saddr", socket.source.address(socket.family)),
                Attr::text("daddr", socket.destination.address(socket.family)),
                Attr::uint("sport", socket.source.port as u64),
                Attr::uint("dport", socket.destination.port as u64),
            ]
            .into_iter()
            .chain(extra_attrs(&info.extra))
            .chain(
                socket
                    .congestion
                    .clone()
                    .map(|name| Attr::text("congestion", name)),
            )
            .collect(),
            collector_id: self.id.clone(),
            seq,
        })
    }
}

/// The rest of `tcp_info` that `ss -ti` prints, each under a name that says
/// its unit: times in nanoseconds (the kernel's microseconds converted), rates
/// in bytes per second (`ss` prints bits), the rest counts of segments or
/// bytes. A field the kernel's `tcp_info` ends before is absent, not zero.
fn extra_attrs(extra: &TcpInfoExtra) -> Vec<Attr> {
    let us = |value: Option<u32>| value.map(|us| us as u64 * 1_000);
    [
        ("rto_ns", us(extra.rto_us)),
        ("ato_ns", us(extra.ato_us)),
        ("min_rtt_ns", us(extra.min_rtt_us)),
        ("rcv_rtt_ns", us(extra.rcv_rtt_us)),
        ("busy_ns", extra.busy_time_us.map(|us| us * 1_000)),
        (
            "rwnd_limited_ns",
            extra.rwnd_limited_us.map(|us| us * 1_000),
        ),
        (
            "sndbuf_limited_ns",
            extra.sndbuf_limited_us.map(|us| us * 1_000),
        ),
        ("pacing_rate_bytes_per_s", extra.pacing_rate),
        ("delivery_rate_bytes_per_s", extra.delivery_rate),
        ("bytes_acked", extra.bytes_acked),
        ("bytes_received", extra.bytes_received),
        ("bytes_sent", extra.bytes_sent),
        ("bytes_retrans", extra.bytes_retrans),
        ("snd_mss", extra.snd_mss.map(u64::from)),
        ("rcv_mss", extra.rcv_mss.map(u64::from)),
        ("pmtu", extra.pmtu.map(u64::from)),
        ("unacked", extra.unacked.map(u64::from)),
        ("sacked", extra.sacked.map(u64::from)),
        ("lost", extra.lost.map(u64::from)),
        ("retrans", extra.retrans.map(u64::from)),
        ("total_retrans", extra.total_retrans.map(u64::from)),
        ("segs_out", extra.segs_out.map(u64::from)),
        ("segs_in", extra.segs_in.map(u64::from)),
        ("delivered", extra.delivered.map(u64::from)),
        ("notsent_bytes", extra.notsent_bytes.map(u64::from)),
        ("rcv_space", extra.rcv_space.map(u64::from)),
        ("snd_wnd", extra.snd_wnd.map(u64::from)),
        ("rcv_wnd", extra.rcv_wnd.map(u64::from)),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.map(|value| Attr::uint(name, value)))
    .collect()
}

impl Collector for TcpRttCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "rtt_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        self.diag = Some(Diag::open().map_err(|error| {
            format!("cannot open a NETLINK_SOCK_DIAG socket ({error}); the kernel may be built without it")
        })?);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let Some(diag) = self.diag.as_ref() else {
            return Err(SampleError::Transient(
                "the netlink socket is closed".into(),
            ));
        };

        let owned = match self.only_pid {
            Some(pid) => Some(Self::sockets_of(pid)?),
            None => None,
        };

        let ts_ns = monotonic_ns();
        let mut events = Vec::new();
        let mut index = seq;

        for family in [libc::AF_INET as u8, libc::AF_INET6 as u8] {
            let sockets = match diag.dump_tcp(family) {
                Ok(sockets) => sockets,
                Err(error) => {
                    // One family failing is not the run failing: a machine
                    // without IPv6 still has IPv4 connections.
                    self.losses.failed_reads += 1;
                    self.losses.note_times(
                        format!("the sock_diag dump for family {family} failed: {error}"),
                        1,
                    );
                    continue;
                }
            };
            for socket in sockets {
                if socket.state != TCP_ESTABLISHED {
                    continue;
                }
                if owned
                    .as_ref()
                    .is_some_and(|inodes| !inodes.contains(&socket.inode))
                {
                    continue;
                }
                if let Some(event) = self.event(&socket, ts_ns, index) {
                    events.push(event);
                    index += 1;
                }
            }
        }
        Ok(events)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn an_established_loopback_connection_reports_a_plausible_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let mut client = TcpStream::connect(address).expect("connect");
        let (mut server, _) = listener.accept().expect("accept");
        client.write_all(b"ping").expect("write");
        let _ = server.write_all(b"pong");

        let mut collector = Box::new(Prepare)
            .attach(&Target::Pid(vec![std::process::id() as i32]))
            .expect("a process target is supported");
        if collector.start().is_err() {
            eprintln!("skipped: this machine refuses NETLINK_SOCK_DIAG");
            return;
        }

        let events = collector.sample(0).expect("dump");
        let ours = events
            .iter()
            .find(|event| {
                event.attr("dport").and_then(|value| value.as_f64()) == Some(address.port() as f64)
            })
            .unwrap_or_else(|| panic!("the connection this test opened was not reported"));

        let rtt_ns = ours
            .attr("rtt_ns")
            .and_then(|value| value.as_f64())
            .unwrap();
        assert!(
            rtt_ns > 0.0 && rtt_ns < 100_000_000.0,
            "loopback round-trip reported as {rtt_ns} ns"
        );
        assert!(
            matches!(ours.key(crate::request::KeyKind::Sock), Some(KeyValue::Sock { cookie }) if *cookie != 0),
            "each connection is keyed by its socket cookie"
        );
    }

    /// The rest of `tcp_info`, against a volume this test sent and the
    /// algorithm the machine says new connections get.
    #[test]
    fn a_connection_reports_the_bytes_it_moved_and_its_congestion_algorithm() {
        use std::io::Read;
        const VOLUME: usize = 256 * 1024;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let mut client = TcpStream::connect(address).expect("connect");
        let (mut server, _) = listener.accept().expect("accept");
        client.write_all(&vec![1u8; VOLUME]).expect("write");
        let mut received = vec![0u8; VOLUME];
        server.read_exact(&mut received).expect("read");

        let mut collector = Box::new(Prepare)
            .attach(&Target::Pid(vec![std::process::id() as i32]))
            .expect("a process target is supported");
        if collector.start().is_err() {
            eprintln!("skipped: this machine refuses NETLINK_SOCK_DIAG");
            return;
        }
        let events = collector.sample(0).expect("dump");
        let number = |event: &Event, name: &str| {
            event
                .attr(name)
                .and_then(|value| value.as_f64())
                .unwrap_or_else(|| panic!("no `{name}`: {event:?}"))
        };
        let client_side = events
            .iter()
            .find(|event| number(event, "dport") == address.port() as f64)
            .expect("the client's end");
        let server_side = events
            .iter()
            .find(|event| number(event, "sport") == address.port() as f64)
            .expect("the server's end");
        // `bytes_sent` counts retransmissions too, which a loaded machine's
        // loopback does make.
        assert_eq!(
            number(client_side, "bytes_sent") - number(client_side, "bytes_retrans"),
            VOLUME as f64
        );
        // RFC 4898's octets acked counts the SYN's sequence number too.
        assert_eq!(number(client_side, "bytes_acked"), VOLUME as f64 + 1.0);
        assert_eq!(number(server_side, "bytes_received"), VOLUME as f64);
        assert!(
            number(client_side, "rto_ns") >= 200_000_000.0,
            "the kernel's floor"
        );
        assert!(number(client_side, "min_rtt_ns") > 0.0);
        let algorithm = std::fs::read_to_string("/proc/sys/net/ipv4/tcp_congestion_control")
            .expect("the default algorithm");
        assert_eq!(
            client_side
                .attr("congestion")
                .map(|value| value.to_string()),
            Some(algorithm.trim().to_owned())
        );
    }

    #[test]
    fn only_the_sockets_a_process_holds_are_reported_for_that_process() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let _client = TcpStream::connect(address).expect("connect");
        let (_server, _) = listener.accept().expect("accept");

        let mut all = Box::new(Prepare).attach(&Target::System).expect("system");
        let mut mine = Box::new(Prepare)
            .attach(&Target::Pid(vec![std::process::id() as i32]))
            .expect("pid");
        if all.start().is_err() || mine.start().is_err() {
            return;
        }

        let everything = all.sample(0).expect("dump").len();
        let ours = mine.sample(0).expect("dump").len();
        assert!(ours >= 2, "this test holds both ends of a connection open");
        assert!(
            ours <= everything,
            "a process cannot hold more sockets than the machine has"
        );
    }
}
