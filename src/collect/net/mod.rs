//! Collectors for the `net` family.

mod dev;

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
    ]
}
