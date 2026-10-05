//! Collectors for the `net` family.

mod accept;
mod afpacket;
mod backlog;
mod capture;
mod connect;
mod dev;
mod drops;
mod ethtool;
mod foreign_clock;
mod hwts;
mod iface;
mod interfaces;
mod lifetime;
mod netlink;
mod packet;
mod procfs;
mod qdisc;
mod request;
mod retransmit;
mod snmp;
mod socket;
mod sockets;
mod softirq;
mod softirqs;
mod tcp_rtt;

use super::Factory;
use crate::request::Mode;

/// The probes behind the `phc` and `ethtool` requirements, answered by the
/// code that serves them and remembered, since `omn list` asks once per
/// backend: `None` for any other requirement.
pub fn probe(requirement: &str) -> Option<Result<(), String>> {
    use std::sync::OnceLock;
    static PHC: OnceLock<Result<(), String>> = OnceLock::new();
    static ETHTOOL: OnceLock<Result<(), String>> = OnceLock::new();
    match requirement {
        "phc" => Some(PHC.get_or_init(hwts::clock_support).clone()),
        "ethtool" => Some(
            ETHTOOL
                .get_or_init(|| {
                    ethtool::Ethtool::open().map(drop).map_err(|error| {
                        format!(
                            "the `ethtool` generic-netlink family could not be resolved: {error}"
                        )
                    })
                })
                .clone(),
        ),
        _ => None,
    }
}

/// The collectors this family provides.
pub fn factories() -> Vec<Factory> {
    vec![
        Factory::new("net.bytes", "fs:proc", |resolved| {
            let by_direction = resolved
                .request
                .keys
                .contains(&crate::request::KeyKind::Dir);
            Ok(Box::new(dev::Prepare(dev::Counted::Bytes, by_direction)))
        }),
        Factory::new("net.packets", "fs:proc", |resolved| {
            let by_direction = resolved
                .request
                .keys
                .contains(&crate::request::KeyKind::Dir);
            Ok(Box::new(dev::Prepare(dev::Counted::Packets, by_direction)))
        }),
        Factory::new("net.tcp.rtt", "netlink", |_| Ok(Box::new(tcp_rtt::Prepare))),
        // Both of these read counters, and both declare `snapshot`. The mode
        // reaches the collector because a single read of a counter cannot be an
        // increment: what it can report is the counter itself, under a name
        // that says so.
        Factory::new("net.iface", "fs:proc", |resolved| {
            Ok(Box::new(iface::Prepare(resolved.request.mode)))
        }),
        Factory::new("net.backlog", "fs:proc", |resolved| {
            Ok(Box::new(backlog::Prepare(resolved.request.mode)))
        }),
        Factory::new("net.sockets", "netlink", |_| Ok(Box::new(sockets::Prepare))),
        Factory::new("net.snmp", "fs:proc", |resolved| {
            Ok(Box::new(snmp::Prepare(resolved.request.mode)))
        }),
        Factory::new("net.tcp.lifetime", "bpf:raw_tp", |resolved| {
            Ok(Box::new(lifetime::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("net.tcp.retransmit", "bpf:raw_tp", |resolved| {
            Ok(Box::new(retransmit::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("net.qdisc", "bpf:raw_tp", |resolved| {
            Ok(Box::new(qdisc::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        Factory::new("net.drops", "bpf:raw_tp", |resolved| {
            Ok(Box::new(drops::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
                wants_stacks: resolved.request.emit.contains(&crate::request::Emit::Stack),
            }))
        }),
        Factory::new("net.softirq", "bpf:raw_tp", |resolved| {
            Ok(Box::new(softirq::Prepare {
                mode: resolved.request.mode,
                // Carried into the collector because the resolver credits every
                // `bpf` backend with filtering in the kernel and then leaves
                // its events alone; see `softirq::Prepare`.
                filter: resolved.request.filter.clone(),
            }))
        }),
        // The handshake, over fentry: opened by the caller, closed by softirq.
        Factory::new("net.tcp.connect", "bpf:fentry", |resolved| {
            Ok(Box::new(connect::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        // The other end of a handshake: the server taking it off its queue.
        Factory::new("net.tcp.accept", "bpf:fentry", |resolved| {
            Ok(Box::new(accept::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        // Every send and receive, over fentry and bucketed in the kernel.
        Factory::new("net.socket", "bpf:fentry", |resolved| {
            Ok(Box::new(socket::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        // A request the server declared, followed in the task serving it --
        // and, on the first backend, on the socket it named.
        Factory::new("net.request", "bpf:usdt", |resolved| {
            Ok(Box::new(request::Prepare {
                mode: resolved.request.mode,
                target: resolved.request.target.clone(),
                filter: resolved.request.filter.clone(),
                sockets: true,
            }))
        }),
        Factory::new("net.request", "bpf:usdt:threads", |resolved| {
            Ok(Box::new(request::Prepare {
                mode: resolved.request.mode,
                target: resolved.request.target.clone(),
                filter: resolved.request.filter.clone(),
                sockets: false,
            }))
        }),
        Factory::new("net.capture", "af_packet", |_| {
            Ok(Box::new(capture::Prepare))
        }),
        Factory::new("net.hwts", "netlink:ethtool", |resolved| {
            // The entry declares `trace` because the chain's first backend is a
            // timestamp per packet. This one reads an interface, which is a
            // snapshot or a series of them -- and answering a request for
            // per-packet stamps with a per-interface reading would be the
            // silent degradation the catalog exists to prevent.
            if resolved.request.mode == Mode::Trace {
                return Err(
                    "`trace` on net.hwts means a timestamp per packet, which is the \
                     `so_timestamping` backend and is not built; this backend reads what an \
                     interface can stamp and how far its clock is from ours, so ask for \
                     `--mode snapshot` or `--mode poll`"
                        .to_owned(),
                );
            }
            Ok(Box::new(hwts::Prepare))
        }),
    ]
}
