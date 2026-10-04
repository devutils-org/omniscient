//! Which block devices a filesystem's I/O lands on, as `io.bytes` keys them.
//!
//! A file event names the superblock's device -- on btrfs an anonymous `0:28`
//! that appears in no other file -- and `io.bytes` counts the devices nothing
//! is stacked on (`dev:259+0`, the disk, not the partition). The two meet only
//! through the mount source and `/sys`: the source is a partition, a
//! device-mapper or md device, or one member of a multi-device btrfs, and each
//! of those has to be followed down to the devices whose counters are their
//! own, by the same rule the diskstats collector uses to choose them (a
//! partition counts again on its disk, a stacked device on its members).
//!
//! The answer is a set: a btrfs over two disks, or an LVM volume striped over
//! three, writes to all of them, and which one a given byte went to is not
//! something the mount table can say. A source that is not a block device
//! (tmpfs, overlay, a network filesystem) maps to nothing, and the event then
//! carries no attribute rather than an empty one: "unknown" and "none" are the
//! same answer here, because neither names a device whose bytes `io.bytes`
//! counted.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Where the two `/sys` trees this reads are, so a test can lay out its own.
pub(super) struct Sysfs {
    /// `/sys/class/block`: one entry per block device, a symlink into
    /// `devices/`, whose parent directory is a partition's disk.
    pub class_block: PathBuf,
    /// `/sys/fs/btrfs`: one directory per filesystem, each listing its member
    /// devices under `devices/`.
    pub btrfs: PathBuf,
}

impl Sysfs {
    pub(super) fn machine() -> Self {
        Self {
            class_block: PathBuf::from("/sys/class/block"),
            btrfs: PathBuf::from("/sys/fs/btrfs"),
        }
    }

    /// The `major:minor` of every device under a mount source, sorted, or
    /// none when the source is not a block device `/sys` knows.
    ///
    /// `name` is the kernel's name for the source (`nvme0n1p6`, `dm-0`), not
    /// a `/dev` path: the caller resolves `/dev/mapper/root` to `dm-0`.
    pub(super) fn leaves_under(&self, name: &str) -> Vec<(u32, u32)> {
        let mut leaves = BTreeSet::new();
        for member in self.btrfs_members(name) {
            self.descend(&member, &mut leaves, 0);
        }
        leaves.into_iter().collect()
    }

    /// The members of the btrfs filesystem `name` belongs to, or `name` alone.
    ///
    /// The mount table names one device of a multi-device btrfs, whichever
    /// the mount was given; the filesystem's own directory names all of them.
    fn btrfs_members(&self, name: &str) -> Vec<String> {
        if let Ok(filesystems) = std::fs::read_dir(&self.btrfs) {
            for filesystem in filesystems.flatten() {
                let devices = filesystem.path().join("devices");
                if devices.join(name).exists() {
                    let mut members: Vec<String> = std::fs::read_dir(&devices)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|entry| entry.file_name().to_string_lossy().into_owned())
                        .collect();
                    members.sort();
                    return members;
                }
            }
        }
        vec![name.to_owned()]
    }

    fn descend(&self, name: &str, leaves: &mut BTreeSet<(u32, u32)>, depth: usize) {
        // A loop in `/sys` is not something the kernel builds; the bound is
        // here so that a fabricated tree cannot hang a drain.
        if depth > 16 {
            return;
        }
        let device = self.class_block.join(name);
        if !device.exists() {
            return;
        }
        if device.join("partition").exists() {
            let disk = device
                .canonicalize()
                .ok()
                .and_then(|path| Some(path.parent()?.file_name()?.to_string_lossy().into_owned()));
            if let Some(disk) = disk {
                self.descend(&disk, leaves, depth + 1);
            }
            return;
        }
        let slaves: Vec<String> = std::fs::read_dir(device.join("slaves"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        if !slaves.is_empty() {
            for slave in slaves {
                self.descend(&slave, leaves, depth + 1);
            }
            return;
        }
        if let Some(numbers) = read_major_minor(&device.join("dev")) {
            leaves.insert(numbers);
        }
    }
}

fn read_major_minor(path: &Path) -> Option<(u32, u32)> {
    let text = std::fs::read_to_string(path).ok()?;
    let (major, minor) = text.trim().split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// The kernel's name for a mount source, or `None` when it is not a block
/// device path: `/dev/nvme0n1p6` is `nvme0n1p6`, and `/dev/mapper/root` is
/// whatever `dm-N` it links to.
pub(super) fn kernel_name(source: &str) -> Option<String> {
    let path = Path::new(source);
    if !path.starts_with("/dev/") {
        return None;
    }
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    Some(resolved.file_name()?.to_string_lossy().into_owned())
}

/// The devices as the attribute carries them: each as the `dev` key
/// `io.bytes` puts on its series (`dev:259+0`), comma-separated.
pub(super) fn as_keys(leaves: &[(u32, u32)]) -> String {
    leaves
        .iter()
        .map(|(major, minor)| format!("dev:{major}+{minor}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("omn-underneath-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("class/block")).unwrap();
            std::fs::create_dir_all(root.join("fs/btrfs")).unwrap();
            Self { root }
        }

        fn sysfs(&self) -> Sysfs {
            Sysfs {
                class_block: self.root.join("class/block"),
                btrfs: self.root.join("fs/btrfs"),
            }
        }

        /// A device at `devices/<path>`, linked from `class/block/<last>`.
        fn device(&self, path: &str, numbers: &str) -> PathBuf {
            let real = self.root.join("devices").join(path);
            std::fs::create_dir_all(&real).unwrap();
            std::fs::write(real.join("dev"), format!("{numbers}\n")).unwrap();
            let name = Path::new(path).file_name().unwrap();
            std::os::unix::fs::symlink(&real, self.root.join("class/block").join(name)).unwrap();
            real
        }

        fn partition(&self, disk: &str, part: &str, numbers: &str) {
            let real = self.device(&format!("{disk}/{part}"), numbers);
            std::fs::write(real.join("partition"), "1\n").unwrap();
        }

        fn stacked(&self, name: &str, numbers: &str, on: &[&str]) {
            let real = self.device(name, numbers);
            for slave in on {
                std::fs::create_dir_all(real.join("slaves").join(slave)).unwrap();
            }
        }

        fn btrfs(&self, fsid: &str, members: &[&str]) {
            for member in members {
                std::fs::create_dir_all(
                    self.root
                        .join("fs/btrfs")
                        .join(fsid)
                        .join("devices")
                        .join(member),
                )
                .unwrap();
            }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The case `docs/view/flows.md` could not draw: a btrfs on a partition,
    /// whose superblock device `0:28` names nothing, lands on the disk
    /// `io.bytes` reports.
    #[test]
    fn a_partition_maps_to_the_disk_that_holds_it() {
        let tree = Tree::new("partition");
        tree.device("nvme0n1", "259:0");
        tree.partition("nvme0n1", "nvme0n1p6", "259:6");
        assert_eq!(tree.sysfs().leaves_under("nvme0n1p6"), vec![(259, 0)]);
    }

    /// A multi-device btrfs is mounted from one member and writes to all.
    #[test]
    fn every_member_of_a_btrfs_is_underneath_it() {
        let tree = Tree::new("btrfs");
        tree.device("nvme0n1", "259:0");
        tree.partition("nvme0n1", "nvme0n1p6", "259:6");
        tree.device("sda", "8:0");
        tree.partition("sda", "sda1", "8:1");
        tree.btrfs("43ce524a", &["nvme0n1p6", "sda1"]);
        assert_eq!(
            tree.sysfs().leaves_under("nvme0n1p6"),
            vec![(8, 0), (259, 0)]
        );
    }

    /// dm on md on two partitions: three layers down, the two disks.
    #[test]
    fn stacked_devices_are_followed_to_their_members() {
        let tree = Tree::new("stacked");
        tree.device("sda", "8:0");
        tree.partition("sda", "sda1", "8:1");
        tree.device("sdb", "8:16");
        tree.partition("sdb", "sdb1", "8:17");
        tree.stacked("md0", "9:0", &["sda1", "sdb1"]);
        tree.stacked("dm-0", "254:0", &["md0"]);
        assert_eq!(tree.sysfs().leaves_under("dm-0"), vec![(8, 0), (8, 16)]);
    }

    /// A disk with no partition table, mounted whole, is its own leaf; a name
    /// `/sys` does not know is nothing rather than a guess.
    #[test]
    fn a_whole_disk_is_itself_and_an_unknown_name_is_nothing() {
        let tree = Tree::new("whole");
        tree.device("vdb", "252:16");
        assert_eq!(tree.sysfs().leaves_under("vdb"), vec![(252, 16)]);
        assert!(tree.sysfs().leaves_under("nbd7").is_empty());
    }

    #[test]
    fn only_a_dev_path_names_a_block_device() {
        assert_eq!(kernel_name("tmpfs"), None);
        assert_eq!(kernel_name("server:/export"), None);
        assert_eq!(
            kernel_name("/dev/nonexistent9").as_deref(),
            Some("nonexistent9")
        );
    }

    #[test]
    fn the_attribute_spells_each_device_as_io_bytes_keys_it() {
        assert_eq!(as_keys(&[(8, 0), (259, 0)]), "dev:8+0,dev:259+0");
    }

    /// Against this machine: the root filesystem's mount source, followed
    /// down, lands on devices the diskstats collector reports as leaves.
    #[test]
    fn the_root_filesystem_lands_on_devices_this_machine_counts() {
        let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        let Some(source) = table.lines().find_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            let separator = fields.iter().position(|field| *field == "-")?;
            (fields.get(4) == Some(&"/"))
                .then(|| fields.get(separator + 2).map(|s| s.to_string()))?
        }) else {
            eprintln!("skipped: no root mount in /proc/self/mountinfo");
            return;
        };
        let Some(name) = kernel_name(&source) else {
            eprintln!("skipped: the root is mounted from `{source}`, not a block device");
            return;
        };
        let leaves = Sysfs::machine().leaves_under(&name);
        if leaves.is_empty() {
            eprintln!("skipped: /sys does not know `{name}` (the CI's `/dev/root` alias)");
            return;
        }
        for (major, minor) in leaves {
            let device = PathBuf::from(format!("/sys/dev/block/{major}:{minor}"));
            assert!(
                device.exists(),
                "{major}:{minor} under `{name}` is not a block device"
            );
            assert!(
                !device.join("partition").exists(),
                "{major}:{minor} under `{name}` is a partition, which io.bytes does not report"
            );
        }
    }
}
