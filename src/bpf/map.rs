//! Maps a program and this process share, and how a map is drained.
//!
//! The ring buffer in `ring.rs` carries one record per event, which is the
//! right shape when the events are rare and each one matters. It is the wrong
//! shape for a tracepoint that fires two million times in a run: the cost of
//! the measurement then scales with the workload, and a switch-heavy program
//! measures a load test rather than itself.
//!
//! The answer is to count in the kernel. A hash map keyed by whatever the
//! series is keyed by -- a task, a stack, a device -- turns those two million
//! events into a few hundred entries, and userspace reads the entries instead
//! of the events. That is what makes `sched.offcpu` affordable, and it is the
//! mechanism `--budget` will degrade a trace onto.
//!
//! Two things about a map are not obvious and both change what the numbers
//! mean:
//!
//! **A drain is not a snapshot.** There is no cursor and no atomic iteration.
//! The walk asks the kernel for the key after this one until there are no
//! more, and the program keeps writing throughout. So a drain takes what it
//! finds and leaves the rest for the next pass, and nothing here pretends to
//! have seen the map at an instant.
//!
//! **Reading and removing must be one operation.** A lookup followed by a
//! delete loses whatever the program added in between, silently and in
//! proportion to how busy the workload is. `BPF_MAP_LOOKUP_AND_DELETE_ELEM`
//! is one operation and is what `drain` uses.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use super::syscall::{
    self, TypedMap, BPF_F_NO_PREALLOC, BPF_MAP_TYPE_HASH, BPF_MAP_TYPE_PERCPU_ARRAY,
    BPF_MAP_TYPE_STACK_TRACE, BPF_MAP_TYPE_TASK_STORAGE,
};

/// A map this layer could not create, said in a way a reader can act on.
///
/// Every program here attaches `permission_advice` to its `prog_load`, and a
/// program load is not the first privileged thing a collector does -- **map
/// creation is**, and it fails first. So what somebody without `CAP_BPF`
/// actually saw, from every collector written before the seventh wave, was
///
/// ```text
/// cannot create the `sched_process_exec` ring buffer: Operation not permitted (os error 1)
/// ```
///
/// which names neither the capability nor the way to get it. Nobody noticed for
/// four waves because the message only appears on a machine without the
/// capabilities, which is not the machine any of this was written on.
///
/// `what` is the map, without its article: "off-cpu in-flight map".
pub fn map_failure(what: &str, error: io::Error) -> String {
    if error.kind() == io::ErrorKind::PermissionDenied {
        super::permission_advice(&format!("creating the {what}"))
    } else {
        format!("cannot create the {what}: {error}")
    }
}

/// A `BPF_MAP_TYPE_HASH` shared with a program, addressed as bytes.
///
/// Key and value are byte strings here rather than types, because the layer
/// does not know what any particular program counts. The program that creates
/// one owns the `#[repr(C)]` structs on both sides and the test that says they
/// agree.
pub struct Hash {
    fd: OwnedFd,
    key_bytes: usize,
    value_bytes: usize,
    max_entries: u32,
}

/// What one drain found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Drained {
    pub entries: Vec<(Vec<u8>, Vec<u8>)>,
    /// Keys the walk returned and the read no longer found.
    ///
    /// Nothing but a drain removes from these maps, so this should be zero. It
    /// is counted rather than ignored because if it is ever not zero, an entry
    /// was dropped between being seen and being read, and a total short by one
    /// entry looks exactly like a total.
    pub vanished: u64,
    /// Set when the walk stopped at the map's own capacity rather than at its
    /// end, which means the kernel kept handing back keys.
    pub truncated: bool,
}

impl Hash {
    pub fn create(
        key_bytes: usize,
        value_bytes: usize,
        max_entries: u32,
        name: &str,
    ) -> io::Result<Self> {
        let fd = syscall::map_create(
            BPF_MAP_TYPE_HASH,
            key_bytes as u32,
            value_bytes as u32,
            max_entries,
            name,
        )?;
        Ok(Self {
            fd,
            key_bytes,
            value_bytes,
            max_entries,
        })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Every key the map holds right now, in whatever order the kernel walks
    /// its buckets.
    pub fn keys(&self) -> io::Result<(Vec<Vec<u8>>, bool)> {
        let fd = self.fd.as_raw_fd();
        let key_bytes = self.key_bytes;
        walk(self.max_entries as usize, |from| {
            syscall::map_next_key(fd, from, key_bytes)
        })
    }

    /// Reads every entry and removes it, so the next drain sees only what
    /// happened since this one.
    ///
    /// Removing is what keeps the map bounded on a long run and what makes
    /// each drain a delta rather than a running total; the caller adds them up
    /// if it wants the total, and can say which window each one covers.
    pub fn drain(&self) -> io::Result<Drained> {
        let (keys, truncated) = self.keys()?;
        let mut drained = Drained {
            truncated,
            ..Default::default()
        };
        for key in keys {
            match syscall::map_take(self.fd.as_raw_fd(), &key, self.value_bytes)? {
                Some(value) => drained.entries.push((key, value)),
                None => drained.vanished += 1,
            }
        }
        Ok(drained)
    }

    /// How many entries the map holds, without reading or removing any.
    ///
    /// For a map of in-flight state -- tasks that went off cpu and have not
    /// come back -- this is the count still open when the run ended, which is
    /// a fact about the measurement rather than a loss.
    pub fn len(&self) -> io::Result<usize> {
        Ok(self.keys()?.0.len())
    }
}

/// Walks a map's keys, stopping at the end or at `capacity`.
///
/// Separated from the syscall so the termination can be tested. A walk that
/// the kernel never ends -- keys reappearing because the program keeps
/// inserting into buckets the walk has already passed -- would otherwise be an
/// unbounded loop inside a drain that runs on a tick, which is a hang rather
/// than a wrong number. The capacity is the map's own, so reaching it means
/// something is wrong and the caller is told rather than left to notice.
fn walk(
    capacity: usize,
    mut next: impl FnMut(Option<&[u8]>) -> io::Result<Option<Vec<u8>>>,
) -> io::Result<(Vec<Vec<u8>>, bool)> {
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut from: Option<Vec<u8>> = None;
    loop {
        let Some(key) = next(from.as_deref())? else {
            return Ok((keys, false));
        };
        if keys.len() >= capacity {
            return Ok((keys, true));
        }
        keys.push(key.clone());
        from = Some(key);
    }
}

/// A one-per-cpu counter array, which is how this layer counts what a program
/// could not record.
///
/// Per-cpu so that incrementing it costs no atomic on a path that runs tens of
/// thousands of times a second, and summed on the way out. Every program in
/// the layer carries one of these, because neither the ring buffer nor a full
/// hash map reports its own overflow: the helper returns an error, the event
/// is gone, and this is the only place that records it happened.
pub struct Counters {
    fd: OwnedFd,
    cpus: usize,
}

impl Counters {
    pub fn create(slots: u32, name: &str) -> io::Result<Self> {
        let fd = syscall::map_create(BPF_MAP_TYPE_PERCPU_ARRAY, 4, 8, slots, name)?;
        Ok(Self {
            fd,
            cpus: syscall::possible_cpus(),
        })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// One slot, summed over the cpus that incremented it.
    ///
    /// A map that cannot be read reads as zero, which is the only answer that
    /// does not invent a loss; the counter is read while the program is still
    /// loaded, so the case does not arise in practice.
    pub fn read(&self, slot: u32) -> u64 {
        syscall::map_lookup_percpu_u64(self.fd.as_raw_fd(), slot, self.cpus)
            .map(|values| values.iter().sum())
            .unwrap_or(0)
    }
}

/// The stacks a program interned with `bpf_get_stackid`.
///
/// A `BPF_MAP_TYPE_STACK_TRACE` is the kernel's own side table: the program
/// asks for an id, the kernel walks the stack, hashes it and hands back a
/// number, and the same stack always gets the same number. That is what makes
/// a stack affordable on a path that runs tens of thousands of times a second
/// -- the walk happens once per distinct stack, and what travels with each
/// measurement is four bytes.
///
/// Entries are read and left in place. Many measurements resolve to the same
/// stack, and taking one out on the first sighting would leave every later one
/// unresolvable.
pub struct StackTraces {
    fd: OwnedFd,
    depth: u32,
}

impl StackTraces {
    /// `depth` is how many frames the kernel will walk before it gives up. The
    /// value size is fixed at creation, so a stack deeper than this comes back
    /// truncated rather than failing, and the truncation is invisible in the
    /// map: what says so is the last slot being full.
    pub fn create(slots: u32, depth: u32, name: &str) -> io::Result<Self> {
        let fd = syscall::map_create(
            BPF_MAP_TYPE_STACK_TRACE,
            4,
            depth * std::mem::size_of::<u64>() as u32,
            slots,
            name,
        )?;
        Ok(Self { fd, depth })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// The addresses behind one id, leaf first, with the zero padding removed.
    ///
    /// `None` when the id is not in the map: the kernel evicts under pressure,
    /// and an id whose stack is gone is a measurement that keeps its number and
    /// loses its explanation, which is worth reporting as such rather than as
    /// an empty stack.
    pub fn get(&self, id: u32) -> io::Result<Option<Vec<u64>>> {
        let bytes = self.depth as usize * std::mem::size_of::<u64>();
        let Some(value) = syscall::map_lookup(self.fd.as_raw_fd(), &id.to_ne_bytes(), bytes)?
        else {
            return Ok(None);
        };
        let addresses: Vec<u64> = value
            .chunks_exact(std::mem::size_of::<u64>())
            .map(|chunk| u64::from_ne_bytes(chunk.try_into().expect("eight bytes")))
            // The kernel zero-fills the slots it did not walk into. A zero is
            // not an address, so the first one ends the stack.
            .take_while(|address| *address != 0)
            .collect();
        Ok(Some(addresses))
    }
}

/// A `BPF_MAP_TYPE_TASK_STORAGE`: one value per task, kept in the task itself.
///
/// This is the one map in the layer that is not keyed by anything a program
/// computes. A program asks for *the current task's* slot, and the kernel hands
/// back the value that lives in that `task_struct` -- so whatever one program
/// wrote there while a thread was doing something is readable by any other
/// program that later runs in the same thread, on any hook, with no key to
/// agree on. That is what carries a request's identity from where it enters to
/// where it touches the disk: a read is submitted in the reader's context, so
/// the block layer's issue finds the id the request's birth wrote.
///
/// Two things the kernel insists on and nothing else in this layer needs:
///
/// - **A BTF description of the key and the value.** The value lives inside
///   the task, and `bpf_local_storage_map_alloc_check` refuses a map it is not
///   told the type of. The key must be a 32-bit int -- from userspace it is a
///   pidfd -- and the value is described as an array of `int` of the right
///   size, which is all the check reads. [`btf_blob`] builds exactly that.
/// - **No preallocation, and no capacity.** An entry exists when a program
///   creates one for a task and goes when the task does, so `max_entries` is
///   zero and `BPF_F_NO_PREALLOC` is required.
///
/// An entry dies with its task. A thread that exits mid-request leaves no
/// stale identity for a recycled tid to inherit, which a hash keyed by tid
/// would.
pub struct TaskStorage {
    fd: OwnedFd,
    /// The BTF object the map was described with. The map holds its own
    /// reference; this one is kept so the description outlives nothing it
    /// describes.
    _btf: OwnedFd,
}

/// The BTF type ids [`btf_blob`] assigns.
const BTF_INT_ID: u32 = 1;
const BTF_VALUE_ID: u32 = 2;

/// The smallest BTF that describes a local-storage map: `int` for the key, and
/// an array of `int` as long as the value for the value.
///
/// Laid out as `include/uapi/linux/btf.h` gives it: a 24-byte header, the type
/// section, then the string section, whose first byte must be the empty
/// string. The value's element type does not matter to the kernel -- it checks
/// the size and looks for special fields, which an array of `int` has none of
/// -- so no struct is described and no member name is owed.
fn btf_blob(value_bytes: usize) -> Vec<u8> {
    const MAGIC: u16 = 0xeb9f;
    const KIND_INT: u32 = 1;
    const KIND_ARRAY: u32 = 3;
    const INT_SIGNED: u32 = 1;

    let strings: &[u8] = b"\0int\0";
    let int_name = 1u32;

    let mut types: Vec<u8> = Vec::new();
    let word = |types: &mut Vec<u8>, value: u32| types.extend_from_slice(&value.to_le_bytes());
    // [1] int: name, info (kind in bits 24..28), size, then the encoding word:
    // signed, at bit offset zero, thirty-two bits wide.
    word(&mut types, int_name);
    word(&mut types, KIND_INT << 24);
    word(&mut types, 4);
    word(&mut types, (INT_SIGNED << 24) | 32);
    // [2] int[value_bytes / 4]: no name, the array kind, a zero size field,
    // then element type, index type, element count.
    word(&mut types, 0);
    word(&mut types, KIND_ARRAY << 24);
    word(&mut types, 0);
    word(&mut types, BTF_INT_ID);
    word(&mut types, BTF_INT_ID);
    word(&mut types, (value_bytes / 4) as u32);

    let mut blob = Vec::with_capacity(24 + types.len() + strings.len());
    blob.extend_from_slice(&MAGIC.to_le_bytes());
    blob.push(1); // version
    blob.push(0); // flags
    blob.extend_from_slice(&24u32.to_le_bytes()); // hdr_len
    blob.extend_from_slice(&0u32.to_le_bytes()); // type_off
    blob.extend_from_slice(&(types.len() as u32).to_le_bytes()); // type_len
    blob.extend_from_slice(&(types.len() as u32).to_le_bytes()); // str_off
    blob.extend_from_slice(&(strings.len() as u32).to_le_bytes()); // str_len
    blob.extend_from_slice(&types);
    blob.extend_from_slice(strings);
    blob
}

impl TaskStorage {
    /// `value_bytes` must be a multiple of four, which every value this layer
    /// lays out is: the kernel is told the value is an array of `int`.
    pub fn create(value_bytes: usize, name: &str) -> io::Result<Self> {
        assert!(
            value_bytes > 0 && value_bytes % 4 == 0,
            "a task-storage value is described as an array of int"
        );
        let btf = syscall::btf_load(&btf_blob(value_bytes)).map_err(|refused| {
            if refused.verifier_log.is_empty() {
                refused.error
            } else {
                io::Error::new(
                    refused.error.kind(),
                    format!(
                        "{}; the kernel said: {}",
                        refused.error, refused.verifier_log
                    ),
                )
            }
        })?;
        let fd = syscall::map_create_typed(&TypedMap {
            map_type: BPF_MAP_TYPE_TASK_STORAGE,
            key_size: 4,
            value_size: value_bytes as u32,
            max_entries: 0,
            map_flags: BPF_F_NO_PREALLOC,
            btf_fd: btf.as_raw_fd(),
            btf_key_type_id: BTF_INT_ID,
            btf_value_type_id: BTF_VALUE_ID,
            name,
        })?;
        Ok(Self { fd, _btf: btf })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// One task's value, read from userspace: the key is a pidfd for the
    /// task, which is how the kernel lets a process name a task it may not
    /// share an address space with. `None` when the task holds no entry.
    ///
    /// Nothing in the build reads a task's value from userspace -- the
    /// programs publish what they account -- so this is here for the test
    /// that the map answers the way the kernel documents.
    #[cfg(test)]
    pub fn lookup(&self, pidfd: RawFd, value_bytes: usize) -> io::Result<Option<Vec<u8>>> {
        syscall::map_lookup(self.fd.as_raw_fd(), &pidfd.to_ne_bytes(), value_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A walk over a map the kernel serves normally: every key once, in the
    /// order it hands them back, and an end that is an end.
    #[test]
    fn a_walk_yields_every_key_once_and_stops_at_the_end() {
        let map = [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()];
        let (keys, truncated) = walk(16, |from| {
            Ok(match from {
                None => Some(map[0].clone()),
                Some(key) => map
                    .iter()
                    .position(|candidate| candidate == key)
                    .and_then(|at| map.get(at + 1))
                    .cloned(),
            })
        })
        .expect("a walk over a map that answers");
        assert_eq!(keys, map);
        assert!(!truncated);
    }

    #[test]
    fn an_empty_map_walks_to_no_keys() {
        let (keys, truncated) = walk(16, |_| Ok(None)).expect("an empty walk");
        assert!(keys.is_empty() && !truncated);
    }

    /// A program inserting into buckets the walk has already passed can make
    /// the kernel hand back keys forever. Inside a drain on a tick loop that
    /// is a hang, which is worse than a wrong number because nothing is ever
    /// reported at all -- so the walk stops at the map's own capacity and says
    /// it did.
    #[test]
    fn a_walk_that_never_ends_stops_at_the_maps_capacity() {
        let mut issued = 0u32;
        let (keys, truncated) = walk(8, |_| {
            issued += 1;
            Ok(Some(issued.to_ne_bytes().to_vec()))
        })
        .expect("a walk over a map that keeps answering");
        assert_eq!(keys.len(), 8, "no more than the map can hold");
        assert!(truncated, "and the caller is told the walk was cut short");
    }

    /// An error from the kernel is not an empty map. Returning the keys found
    /// so far would report a partial drain as a complete one.
    #[test]
    fn a_walk_that_fails_is_an_error_not_a_short_answer() {
        let mut calls = 0;
        let result = walk(16, |_| {
            calls += 1;
            if calls == 1 {
                Ok(Some(b"first".to_vec()))
            } else {
                Err(io::Error::from_raw_os_error(libc::EPERM))
            }
        });
        assert!(result.is_err());
    }

    /// Without the capabilities there is no map to create, and that is the
    /// answer rather than a failure -- but the refusal must be the kernel's.
    #[test]
    fn a_hash_map_is_created_or_the_kernel_says_why() {
        match Hash::create(4, 8, 16, "omn_test_hash") {
            Ok(map) => {
                let (keys, truncated) = map.keys().expect("an empty map walks");
                assert!(keys.is_empty() && !truncated, "a new map holds nothing");
                assert_eq!(map.len().expect("a length"), 0);

                syscall::map_update(map.as_raw_fd(), &7u32.to_ne_bytes(), &42u64.to_ne_bytes())
                    .expect("a write into a map this process owns");
                assert_eq!(map.len().expect("a length"), 1);

                // The drain must both read and remove: a second one finding
                // the same entry would double every total built from it.
                let drained = map.drain().expect("a drain");
                assert_eq!(drained.entries.len(), 1);
                assert_eq!(drained.entries[0].0, 7u32.to_ne_bytes());
                assert_eq!(
                    u64::from_ne_bytes(drained.entries[0].1[..8].try_into().unwrap()),
                    42
                );
                assert_eq!((drained.vanished, drained.truncated), (0, false));
                assert!(
                    map.drain().expect("a second drain").entries.is_empty(),
                    "a drained entry must not come back"
                );
            }
            Err(error) => {
                assert!(
                    error.raw_os_error().is_some(),
                    "a refusal must carry the kernel's errno, not a message invented here"
                );
                eprintln!("skipped: this machine does not let this process create a map: {error}");
            }
        }
    }

    /// The BTF handed to the kernel is checked byte by byte on the other side,
    /// and a field one word out of place is a refused map with a log line as
    /// its only diagnosis. The layout is `include/uapi/linux/btf.h`'s.
    #[test]
    fn the_task_storage_btf_is_laid_out_as_the_kernel_reads_it() {
        let blob = btf_blob(64);
        let word = |at: usize| u32::from_le_bytes(blob[at..at + 4].try_into().unwrap());
        assert_eq!(u16::from_le_bytes([blob[0], blob[1]]), 0xeb9f);
        assert_eq!((blob[2], blob[3]), (1, 0), "version one, no flags");
        assert_eq!(word(4), 24, "the header is 24 bytes");
        let (type_off, type_len, str_off, str_len) = (word(8), word(12), word(16), word(20));
        assert_eq!(type_off, 0);
        assert_eq!(type_len, 40, "an int of four words and an array of six");
        assert_eq!(str_off, type_len, "the strings follow the types");
        let strings = &blob[24 + str_off as usize..][..str_len as usize];
        assert_eq!(
            strings[0], 0,
            "the string section starts with the empty string"
        );
        let types = 24 + type_off as usize;
        // The key: `int`, four bytes, signed, thirty-two bits.
        assert_eq!(&strings[word(types) as usize..][..4], b"int\0");
        assert_eq!(word(types + 4) >> 24, 1, "an INT");
        assert_eq!(word(types + 8), 4);
        assert_eq!(word(types + 12) & 0xff, 32);
        // The value: an array of that int, as long as the value.
        assert_eq!(word(types + 20) >> 24, 3, "an ARRAY");
        assert_eq!(word(types + 28), BTF_INT_ID, "of int");
        assert_eq!(word(types + 36), 16, "sixty-four bytes of int");
        assert_eq!(blob.len(), 24 + 40 + str_len as usize);
    }

    /// Created, or refused with the kernel's own errno. Not created by an
    /// in-process test on a normal machine: the test binary carries no
    /// capabilities, and `tests/vertical.rs` is where the map is exercised.
    #[test]
    fn a_task_storage_map_is_created_or_the_kernel_says_why() {
        match TaskStorage::create(64, "omn_test_task") {
            Ok(storage) => {
                let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
                assert!(pidfd >= 0, "pidfd_open: {}", io::Error::last_os_error());
                assert_eq!(
                    storage
                        .lookup(pidfd as RawFd, 64)
                        .expect("a lookup by pidfd"),
                    None,
                    "no program has written into this task"
                );
                unsafe { libc::close(pidfd as RawFd) };
            }
            Err(error) => {
                assert!(error.raw_os_error().is_some(), "{error}");
                eprintln!("skipped: this process may not create a task-storage map: {error}");
            }
        }
    }

    /// The advice has to arrive on the first privileged call a collector makes,
    /// which is this one and not the program load. A reader whose kernel
    /// refuses gets the capability and the command, not `os error 1`.
    #[test]
    fn a_map_refused_on_privilege_grounds_names_the_capability() {
        let refused = io::Error::from(io::ErrorKind::PermissionDenied);
        let message = map_failure("off-cpu in-flight map", refused);
        assert!(message.contains("CAP_BPF"), "{message}");
        assert!(message.contains("setcap"), "{message}");
        assert!(message.contains("off-cpu in-flight map"), "{message}");
    }

    /// And a failure that is not about privilege says what it was, rather than
    /// sending a reader to grant a capability they already have. A full map
    /// table and a refused one are different problems.
    #[test]
    fn a_map_refused_for_any_other_reason_is_reported_as_itself() {
        let full = io::Error::from(io::ErrorKind::OutOfMemory);
        let message = map_failure("syscall histogram map", full);
        assert!(!message.contains("CAP_BPF"), "{message}");
        assert!(
            message.starts_with("cannot create the syscall histogram map"),
            "{message}"
        );
    }
}
