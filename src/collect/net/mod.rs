//! Collectors for the `net` family.

mod afpacket;
mod backlog;
mod capture;
mod dev;
mod ethtool;
mod foreign_clock;
mod hwts;
mod iface;
mod interfaces;
mod netlink;
mod packet;
mod procfs;
mod tcp_rtt;

use super::Factory;
use crate::request::Mode;

/// The collectors this family provides.
pub fn factories() -> Vec<Factory> {
    vec![
        Factory::new("net.bytes", "fs:proc", |_| {
            Ok(Box::new(dev::Prepare(dev::Counted::Bytes)))
        }),
        Factory::new("net.packets", "fs:proc", |_| {
            Ok(Box::new(dev::Prepare(dev::Counted::Packets)))
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
