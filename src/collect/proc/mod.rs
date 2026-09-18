//! Collectors for the `proc` family.

mod status;

use super::Factory;

/// The collectors this family provides.
pub fn factories() -> Vec<Factory> {
    vec![
        Factory::new("proc.fd", "fs:proc", |_| {
            Ok(Box::new(status::Prepare(status::Counted::Descriptors)))
        }),
        Factory::new("proc.threads", "fs:proc", |_| {
            Ok(Box::new(status::Prepare(status::Counted::Threads)))
        }),
    ]
}
