//! Collectors for the `net` family.

mod afpacket;
mod backlog;
mod capture;
mod dev;
mod foreign_clock;
mod iface;
mod interfaces;
mod netlink;
mod packet;
mod procfs;
mod tcp_rtt;

use super::Factory;

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
        Factory::new("net.iface", "fs:proc", |_| Ok(Box::new(iface::Prepare))),
        Factory::new("net.backlog", "fs:proc", |_| Ok(Box::new(backlog::Prepare))),
        Factory::new("net.capture", "af_packet", |_| {
            Ok(Box::new(capture::Prepare))
        }),
    ]
}
