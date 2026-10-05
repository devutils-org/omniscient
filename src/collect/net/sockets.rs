//! `net.sockets` over `sock_diag`: every TCP and UDP socket, as `ss -tuan`
//! lists them.
//!
//! One event per socket: protocol, state, the two queues, the endpoints, the
//! owner's uid and, when this process can read the owner's descriptors, the
//! process that holds it (`ss -p`). Keyed by the socket's cookie, the key the
//! eBPF sources carry, so a socket here is the socket there.
//!
//! The queues mean what `ss` says they mean: on a listening socket the
//! receive queue is the accept queue's length and the send queue its bound
//! (the `backlog` given to `listen`); on any other socket, bytes not yet read
//! by the application and bytes not yet acknowledged by the peer.

use std::collections::BTreeMap;

use super::netlink::{Diag, Socket};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::Target;

/// `ss`'s names for the kernel's TCP states, by number (`include/net/tcp_states.h`).
fn state_name(state: u8) -> &'static str {
    match state {
        1 => "ESTAB",
        2 => "SYN-SENT",
        3 => "SYN-RECV",
        4 => "FIN-WAIT-1",
        5 => "FIN-WAIT-2",
        6 => "TIME-WAIT",
        7 => "UNCONN",
        8 => "CLOSE-WAIT",
        9 => "LAST-ACK",
        10 => "LISTEN",
        11 => "CLOSING",
        12 => "NEW-SYN-RECV",
        _ => "UNKNOWN",
    }
}

/// A socket's state as `ss` prints it: UDP has no connection states, and the
/// kernel's `TCP_ESTABLISHED` and `TCP_CLOSE` on a datagram socket mean
/// connected and not.
fn shown_state(protocol: &str, state: u8) -> &'static str {
    match (protocol, state) {
        ("udp", 1) => "ESTAB",
        ("udp", _) => "UNCONN",
        (_, state) => state_name(state),
    }
}

/// Who holds each socket inode: every process whose descriptors this process
/// can read. Another user's processes are unreadable without
/// `CAP_SYS_PTRACE`, which is counted, not guessed.
struct Owners {
    by_inode: BTreeMap<u32, (i32, String)>,
    unreadable: u64,
}

fn owners(only: Option<i32>) -> Owners {
    let mut owners = Owners {
        by_inode: BTreeMap::new(),
        unreadable: 0,
    };
    let pids: Vec<i32> = match only {
        Some(pid) => vec![pid],
        None => std::fs::read_dir("/proc")
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default(),
    };
    for pid in pids {
        let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            owners.unreadable += 1;
            continue;
        };
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|text| text.trim_end().to_owned())
            .unwrap_or_default();
        for entry in entries.flatten() {
            let Ok(link) = std::fs::read_link(entry.path()) else {
                continue;
            };
            if let Some(inode) = link
                .to_str()
                .and_then(|text| text.strip_prefix("socket:["))
                .and_then(|rest| rest.strip_suffix(']'))
                .and_then(|digits| digits.parse().ok())
            {
                owners.by_inode.entry(inode).or_insert((pid, comm.clone()));
            }
        }
    }
    owners
}

/// Why one process's sockets cannot be listed.
fn unreadable(pid: i32, error: &std::io::Error) -> String {
    format!(
        "cannot read /proc/{pid}/fd ({error}): process {pid} is another user's, and its \
         descriptors do not open without CAP_SYS_PTRACE, so which sockets it holds is unknown; \
         run as its user, or use --system, where its sockets are listed with their `uid` and \
         no `pid`"
    )
}

pub struct Prepare;

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let only = match target {
            Target::System => None,
            Target::Pid(pids) if pids.len() == 1 => {
                let pid = pids[0];
                // Which sockets a process holds is read off its descriptors:
                // where they cannot be read, the answer is unknown, and an
                // empty inventory would say "none".
                if let Err(error) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
                    if error.kind() == std::io::ErrorKind::PermissionDenied {
                        return Err(unreadable(pid, &error));
                    }
                }
                Some(pid)
            }
            other => {
                return Err(format!(
                    "the socket inventory is read machine-wide or for one process, and a {} \
                     target is neither; use --system or --pid",
                    other.kind()
                ))
            }
        };
        Ok(Box::new(SocketsCollector {
            id: CollectorId::new("net.sockets/netlink"),
            only,
            diag: None,
            losses: Losses::default(),
            unowned_note: None,
        }))
    }
}

pub struct SocketsCollector {
    id: CollectorId,
    /// One process: only the sockets it holds.
    only: Option<i32>,
    diag: Option<Diag>,
    losses: Losses,
    /// How many processes' descriptors could not be read, at the last read.
    unowned_note: Option<u64>,
}

impl SocketsCollector {
    fn event(
        &self,
        protocol: &str,
        socket: &Socket,
        owners: &Owners,
        ts_ns: u64,
        seq: u64,
    ) -> Event {
        let family = if socket.family == libc::AF_INET as u8 {
            "inet"
        } else {
            "inet6"
        };
        let mut attrs = vec![
            Attr::uint("recv_q", socket.receive_queue as u64),
            Attr::uint("send_q", socket.send_queue as u64),
            Attr::text("protocol", protocol),
            Attr::text("family", family),
            Attr::text("state", shown_state(protocol, socket.state)),
            Attr::text("saddr", socket.source.address(socket.family)),
            Attr::uint("sport", socket.source.port as u64),
            Attr::text("daddr", socket.destination.address(socket.family)),
            Attr::uint("dport", socket.destination.port as u64),
            Attr::uint("uid", socket.uid as u64),
            Attr::uint("inode", socket.inode as u64),
        ];
        if let Some((pid, comm)) = owners.by_inode.get(&socket.inode) {
            attrs.push(Attr::uint("pid", *pid as u64));
            attrs.push(Attr::text("comm", comm.clone()));
        }
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "net.sockets".to_owned(),
            keys: vec![KeyValue::Sock {
                cookie: socket.cookie,
            }],
            attrs,
            collector_id: self.id.clone(),
            seq,
        }
    }
}

impl Collector for SocketsCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "recv_q"
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
        if let Some(pid) = self.only {
            if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                return Err(SampleError::TargetGone(format!("process {pid} exited")));
            }
        }
        let mut dumped = Vec::new();
        for (protocol, number) in [("tcp", libc::IPPROTO_TCP), ("udp", libc::IPPROTO_UDP)] {
            for family in [libc::AF_INET as u8, libc::AF_INET6 as u8] {
                match diag.dump(family, number as u8) {
                    Ok(sockets) => {
                        dumped.extend(sockets.into_iter().map(|socket| (protocol, socket)))
                    }
                    Err(error) => {
                        self.losses.failed_reads += 1;
                        self.losses.note(format!(
                            "the sock_diag dump of {protocol} for family {family} failed: {error}"
                        ));
                    }
                }
            }
        }
        let owners = owners(self.only);
        if let (Some(pid), true) = (self.only, owners.unreadable > 0) {
            // Readable at attach and not now: a setuid exec, most likely.
            // Every socket it holds would be filtered out, which is not "it
            // holds none".
            let error = std::fs::read_dir(format!("/proc/{pid}/fd"))
                .err()
                .unwrap_or_else(|| std::io::Error::other("unreadable a moment ago"));
            return Err(SampleError::Transient(unreadable(pid, &error)));
        }
        let ts_ns = monotonic_ns();
        let mut events = Vec::new();
        for (protocol, socket) in &dumped {
            if self.only.is_some() && !owners.by_inode.contains_key(&socket.inode) {
                continue;
            }
            events.push(self.event(protocol, socket, &owners, ts_ns, seq + events.len() as u64));
        }
        self.unowned_note = Some(owners.unreadable);
        Ok(events)
    }

    fn notes(&self) -> Vec<String> {
        match self.unowned_note {
            Some(unreadable) if unreadable > 0 && self.only.is_none() => vec![format!(
                "{unreadable} processes' descriptors could not be read (another user's, without \
                 CAP_SYS_PTRACE), so the sockets they hold carry no `pid` or `comm`; `uid` is \
                 the kernel's and always there"
            )],
            _ => Vec::new(),
        }
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream, UdpSocket};

    /// Another user's process is refused with the reason at attach, and one
    /// that cannot be read at a sample is a failed read: in neither case an
    /// empty inventory, which would say it holds no socket.
    #[test]
    fn a_process_whose_descriptors_cannot_be_read_is_not_an_empty_inventory() {
        match std::fs::read_dir("/proc/1/fd") {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
            _ => {
                eprintln!("skipped: this process can read pid 1's descriptors");
                return;
            }
        }
        let Err(refusal) = Box::new(Prepare).attach(&Target::Pid(vec![1])) else {
            panic!("an unreadable process must be refused");
        };
        assert!(refusal.contains("/proc/1/fd"), "{refusal}");
        assert!(refusal.contains("unknown"), "{refusal}");

        let mut collector = SocketsCollector {
            id: CollectorId::new("net.sockets/netlink"),
            only: Some(1),
            diag: None,
            losses: Losses::default(),
            unowned_note: None,
        };
        if collector.start().is_err() {
            eprintln!("skipped: this machine refuses NETLINK_SOCK_DIAG");
            return;
        }
        match collector.sample(0) {
            Err(SampleError::Transient(reason)) => assert!(reason.contains("unknown"), "{reason}"),
            Ok(events) => panic!("{} events and no failed read", events.len()),
            Err(SampleError::TargetGone(reason)) => panic!("pid 1 is not gone: {reason}"),
        }
    }

    fn find(events: &[Event], what: impl Fn(&Event) -> bool) -> Option<&Event> {
        events.iter().find(|event| what(event))
    }

    fn text(event: &Event, name: &str) -> String {
        event
            .attr(name)
            .map(|value| value.to_string())
            .unwrap_or_default()
    }

    /// Ground truth is what this test opened: a listener with one connection
    /// waiting in its accept queue, the connection itself, and a bound UDP
    /// socket; each found with its state, its queues and this process as its
    /// owner.
    #[test]
    fn every_socket_this_test_opened_is_listed_with_its_state_and_owner() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let _client = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let udp = UdpSocket::bind("127.0.0.1:0").expect("bind udp");
        let udp_port = udp.local_addr().unwrap().port();

        let mut collector = Box::new(Prepare)
            .attach(&Target::Pid(vec![std::process::id() as i32]))
            .expect("one process");
        if collector.start().is_err() {
            eprintln!("skipped: this machine refuses NETLINK_SOCK_DIAG");
            return;
        }
        let events = collector.sample(0).expect("dump");
        let sport = |event: &Event, port: u16| {
            event.attr("sport").and_then(|value| value.as_f64()) == Some(port as f64)
        };
        let listening = find(&events, |event| {
            sport(event, port) && text(event, "state") == "LISTEN"
        })
        .expect("the listener");
        assert_eq!(
            listening.attr("recv_q").unwrap().as_f64(),
            Some(1.0),
            "one connection waits to be accepted"
        );
        assert_eq!(text(listening, "pid"), std::process::id().to_string());
        assert!(
            find(&events, |event| {
                event.attr("dport").and_then(|value| value.as_f64()) == Some(port as f64)
                    && text(event, "state") == "ESTAB"
            })
            .is_some(),
            "the client's end"
        );
        let datagram = find(&events, |event| sport(event, udp_port)).expect("the udp socket");
        assert_eq!(text(datagram, "protocol"), "udp");
        assert_eq!(text(datagram, "state"), "UNCONN");
        assert!(events
            .iter()
            .all(|event| text(event, "pid") == std::process::id().to_string()));
    }
}
