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
    self, BPF_MAP_TYPE_HASH, BPF_MAP_TYPE_PERCPU_ARRAY, BPF_MAP_TYPE_STACK_TRACE,
};

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
}
