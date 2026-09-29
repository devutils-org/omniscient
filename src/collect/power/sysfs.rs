//! Reading small files out of `/sys`, the way this family needs them.
//!
//! Every backend here is a handful of sysfs files read on a tick. The reads are
//! tiny and uninteresting; what matters is that a file which is *present but
//! unreadable* is reported differently from one that is absent, because one
//! calls for privileges and the other for different hardware. That distinction
//! is the resolver's rule, and a collector that collapsed the two at read time
//! would undo it at the last moment.

use std::path::{Path, PathBuf};

/// Why a sysfs read did not produce a number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The file is not there. Different hardware, or a different kernel.
    Absent(PathBuf),
    /// The file is there and this user may not read it.
    Restricted(PathBuf),
    /// It was read and did not hold what was expected.
    Unparsable { path: PathBuf, content: String },
    /// Anything else the kernel said.
    Other { path: PathBuf, error: String },
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Absent(path) => write!(f, "{} does not exist", path.display()),
            ReadError::Restricted(path) => write!(
                f,
                "{} exists but is not readable by this user",
                path.display()
            ),
            ReadError::Unparsable { path, content } => {
                write!(
                    f,
                    "{} holds `{content}`, which is not a number",
                    path.display()
                )
            }
            ReadError::Other { path, error } => write!(f, "{}: {error}", path.display()),
        }
    }
}

/// Reads a file and trims it.
pub fn text(path: &Path) -> Result<String, ReadError> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(content.trim().to_owned()),
        Err(error) => Err(match error.kind() {
            std::io::ErrorKind::NotFound => ReadError::Absent(path.to_path_buf()),
            std::io::ErrorKind::PermissionDenied => ReadError::Restricted(path.to_path_buf()),
            _ => ReadError::Other {
                path: path.to_path_buf(),
                error: error.to_string(),
            },
        }),
    }
}

/// Reads a file holding one unsigned number.
pub fn number(path: &Path) -> Result<u64, ReadError> {
    let content = text(path)?;
    content.parse().map_err(|_| ReadError::Unparsable {
        path: path.to_path_buf(),
        content,
    })
}

/// Reads a file holding one signed number, which temperatures are: a sensor
/// below zero is unusual, not impossible.
pub fn signed(path: &Path) -> Result<i64, ReadError> {
    let content = text(path)?;
    content.parse().map_err(|_| ReadError::Unparsable {
        path: path.to_path_buf(),
        content,
    })
}

/// The cpus the kernel currently has online, from `cpu/online`.
///
/// Read rather than assumed: a machine with a cpu offlined has a gap in the
/// numbering, and iterating `0..nproc` would read a directory that is not there
/// and call it a failure.
pub fn online_cpus(root: &Path) -> Result<Vec<u32>, ReadError> {
    parse_cpu_list(&text(&root.join("online"))?).ok_or(ReadError::Unparsable {
        path: root.join("online"),
        content: String::new(),
    })
}

/// `0-3,8,10-11` as the kernel writes cpu lists: the one reader, in
/// `capabilities.rs`.
pub use crate::capabilities::parse_cpu_list;

/// The entries of a directory, sorted, so a run does not depend on the order
/// the filesystem happens to return.
pub fn entries(directory: &Path) -> Result<Vec<PathBuf>, ReadError> {
    let mut found: Vec<PathBuf> = match std::fs::read_dir(directory) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|e| e.path())
            .collect(),
        Err(error) => {
            return Err(match error.kind() {
                std::io::ErrorKind::NotFound => ReadError::Absent(directory.to_path_buf()),
                std::io::ErrorKind::PermissionDenied => {
                    ReadError::Restricted(directory.to_path_buf())
                }
                _ => ReadError::Other {
                    path: directory.to_path_buf(),
                    error: error.to_string(),
                },
            })
        }
    };
    found.sort();
    Ok(found)
}

/// An attribute name a filter can name and a reader can read: lowercase, with
/// the spaces the kernel puts in sensor labels turned into underscores.
pub fn attribute_name(chip: &str, label: &str) -> String {
    let clean = |value: &str| {
        value
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    format!("{}.{}", clean(chip), clean(label))
}

/// A throwaway directory tree standing in for `/sys`, so the parsing of files
/// this machine does not have -- an Intel throttle counter, a wrapped RAPL
/// domain, an offline cpu -- can be exercised anywhere.
#[cfg(test)]
pub mod scratch {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// A directory that no other test in this process will pick.
    pub fn tree(name: &str) -> PathBuf {
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("omn-power-{}-{unique}-{name}", std::process::id()));
        std::fs::remove_dir_all(&path).ok();
        std::fs::create_dir_all(&path).expect("a scratch tree");
        path
    }

    /// Writes one sysfs-style file, creating the directories above it.
    pub fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("a scratch directory");
        std::fs::write(path, format!("{content}\n")).expect("a scratch file");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cpu_list_is_read_the_way_the_kernel_writes_it() {
        assert_eq!(parse_cpu_list("0-3").unwrap(), vec![0, 1, 2, 3]);
        assert_eq!(parse_cpu_list("0,2,4").unwrap(), vec![0, 2, 4]);
        assert_eq!(parse_cpu_list("0-1,4,6-7").unwrap(), vec![0, 1, 4, 6, 7]);
        assert!(parse_cpu_list("3-1").is_none());
    }

    /// The machine's own list, so the parser is held to what this kernel
    /// actually writes rather than to what the test author remembered.
    #[test]
    fn this_machines_online_cpus_parse_and_match_the_directories() {
        let root = Path::new("/sys/devices/system/cpu");
        let Ok(cpus) = online_cpus(root) else {
            return;
        };
        assert!(!cpus.is_empty());
        for cpu in cpus {
            assert!(
                root.join(format!("cpu{cpu}")).exists(),
                "cpu{cpu} is online but has no directory"
            );
        }
    }

    #[test]
    fn a_missing_file_and_an_unreadable_one_are_not_the_same_answer() {
        let absent = number(Path::new("/sys/class/omn-does-not-exist/value"));
        assert!(matches!(absent, Err(ReadError::Absent(_))), "{absent:?}");

        // Root-owned and mode 0400 on every machine that has RAPL at all.
        let restricted = Path::new("/sys/class/powercap/intel-rapl:0/energy_uj");
        if restricted.exists() {
            match number(restricted) {
                Ok(_) | Err(ReadError::Restricted(_)) => {}
                other => panic!("expected a value or a refusal, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_attribute_name_survives_the_spaces_the_kernel_writes() {
        assert_eq!(
            attribute_name("coretemp", "Package id 0"),
            "coretemp.package_id_0"
        );
        assert_eq!(attribute_name("k10temp", "Tctl"), "k10temp.tctl");
    }
}
