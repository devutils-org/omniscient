//! Collectors for the `net` family.

mod backlog;
mod dev;
mod iface;
mod netlink;
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
    ]
}
