//! Naming and numbering the interfaces of this machine.
//!
//! The kernel identifies an interface by an index and people identify it by a
//! name, and the two are not interchangeable: a name can be reassigned to a
//! different device, and an index outlives the name it was given. `KeyValue::
//! Iface` carries both for that reason, so everything that produces one needs
//! to be able to go from either to the other.

use std::path::Path;

/// Where the kernel lists the interfaces that exist right now.
const SYSFS: &str = "/sys/class/net";

/// The index the kernel binds and reports on, for an interface named by a
/// person.
pub fn index_of(name: &str) -> Result<u32, String> {
    let Ok(c_name) = std::ffi::CString::new(name) else {
        return Err(format!("`{name}` is not a usable interface name"));
    };
    // SAFETY: passing a NUL-terminated string we own.
    let index = unsafe { libc::if_nametoindex(c_name.as_ptr()) };
    if index == 0 {
        return Err(format!(
            "this machine has no interface called `{name}`: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(index)
}

/// The name of an interface the kernel identified by index, when it still has
/// one: an interface removed between the two questions does not.
pub fn name_of(index: u32) -> Option<String> {
    let mut buffer = [0 as libc::c_char; libc::IF_NAMESIZE];
    // SAFETY: writing into a buffer we own, of the size the call requires.
    let resolved = unsafe { libc::if_indextoname(index, buffer.as_mut_ptr()) };
    if resolved.is_null() {
        return None;
    }
    // SAFETY: on success the kernel wrote a NUL-terminated name into `buffer`.
    let name = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
    Some(name.to_string_lossy().into_owned())
}

/// Every interface this machine has, in the order sysfs lists them.
///
/// Read from sysfs rather than from `/proc/net/dev`, because an interface that
/// is down still exists and still has a driver with something to say about it,
/// and this is the listing that includes it.
pub fn all() -> Result<Vec<String>, String> {
    let entries = std::fs::read_dir(Path::new(SYSFS)).map_err(|error| {
        format!("{SYSFS} cannot be read, so no interface can be named: {error}")
    })?;
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        // The directory is not only interfaces: `bonding_masters` appears there
        // once the bonding driver is loaded, and it is a control file. What
        // every real interface has is an `ifindex`, so that is what is asked.
        .filter(|name| Path::new(SYSFS).join(name).join("ifindex").exists())
        .collect();
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_has_the_index_sysfs_gives_it() {
        // Independently known: sysfs writes the index down, and the answer here
        // comes from a different kernel interface altogether.
        let sysfs =
            std::fs::read_to_string("/sys/class/net/lo/ifindex").expect("a loopback exists");
        let expected: u32 = sysfs.trim().parse().expect("an index");
        assert_eq!(index_of("lo"), Ok(expected));
        assert_eq!(name_of(expected).as_deref(), Some("lo"));
    }

    #[test]
    fn an_interface_that_does_not_exist_is_named_in_the_refusal() {
        let Err(error) = index_of("nosuchif0") else {
            panic!("this machine has no interface called nosuchif0");
        };
        assert!(error.contains("nosuchif0"), "{error}");
    }

    #[test]
    fn the_listing_holds_the_loopback_and_nothing_that_is_not_an_interface() {
        let names = all().expect("sysfs is readable");
        assert!(names.contains(&"lo".to_owned()), "{names:?}");
        for name in &names {
            assert!(
                index_of(name).is_ok(),
                "`{name}` was listed as an interface and the kernel does not know it"
            );
        }
    }
}
