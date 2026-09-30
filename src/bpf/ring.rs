// SPDX-License-Identifier: MIT OR GPL-2.0
//! The BPF ring buffer, consumed directly.
//!
//! The kernel publishes two pages and the data area through the map's file
//! descriptor: a consumer page this process writes, a producer page it reads,
//! and the records in between. The discipline is the same one `perf.rs` follows
//! for its ring -- read everything the producer has published, then publish the
//! consumer position once, after the batch has been copied out -- so a record
//! is never handed to a caller while the kernel may still be writing it.
//!
//! One thing differs from the perf ring and matters for honesty: this buffer
//! does not report its own overflows. When a program cannot reserve space, its
//! `bpf_ringbuf_output` returns an error and the event is gone, with nothing
//! recorded anywhere unless the program counts it. So every program in this
//! layer carries a drop counter and the collector reads it; a ring that quietly
//! swallowed a third of the wakeups would otherwise produce a plausible
//! correlation with a third of the evidence missing.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{fence, AtomicU64, Ordering};

/// Set while a record is still being written by the program that reserved it.
const BUSY_BIT: u32 = 1 << 31;
/// Set when the program reserved space and then threw the record away.
const DISCARD_BIT: u32 = 1 << 30;
const LENGTH_MASK: u32 = !(BUSY_BIT | DISCARD_BIT);

/// A mapped ring buffer map.
pub struct RingBuffer {
    _map: OwnedFd,
    consumer: *mut u8,
    producer: *mut u8,
    data: *mut u8,
    mask: u64,
    consumer_len: usize,
    producer_len: usize,
}

// The mappings belong to this struct alone and are only touched through it.
unsafe impl Send for RingBuffer {}

impl RingBuffer {
    /// Maps a ring buffer map. `bytes` is what the map was created with.
    pub fn map(map: OwnedFd, bytes: usize) -> io::Result<Self> {
        assert!(
            bytes.is_power_of_two(),
            "a ring buffer's size must be a power of two"
        );
        let page = page_size();
        let fd = map.as_raw_fd();

        let consumer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if consumer == libc::MAP_FAILED {
            return Err(io::Error::other(format!(
                "mmap of the ring's consumer page: {}",
                io::Error::last_os_error()
            )));
        }

        // The data area is mapped twice, back to back, by the kernel: a record
        // that straddles the end is contiguous in this mapping, so there is no
        // wrap to handle while reading.
        let producer_len = page + 2 * bytes;
        let producer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                producer_len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                page as i64,
            )
        };
        if producer == libc::MAP_FAILED {
            let error = io::Error::last_os_error();
            unsafe { libc::munmap(consumer, page) };
            return Err(io::Error::other(format!(
                "mmap of the ring's {bytes}-byte data area: {error}"
            )));
        }

        Ok(Self {
            _map: map,
            consumer: consumer as *mut u8,
            producer: producer as *mut u8,
            data: unsafe { (producer as *mut u8).add(page) },
            mask: (bytes - 1) as u64,
            consumer_len: page,
            producer_len,
        })
    }

    /// Hands every complete record to `handle`, then publishes the position.
    ///
    /// Returns how many records were consumed, so a caller can tell an empty
    /// drain from a busy one without counting events itself.
    pub fn drain(&mut self, mut handle: impl FnMut(&[u8])) -> usize {
        let consumer = unsafe { &*(self.consumer as *const AtomicU64) };
        let producer = unsafe { &*(self.producer as *const AtomicU64) };

        let from = consumer.load(Ordering::Acquire);
        let published = producer.load(Ordering::Acquire);
        // Acquire: a record's length is published last by the program that
        // wrote it, so seeing the producer position means the payloads behind
        // it are there.
        fence(Ordering::Acquire);

        // The kernel maps the data area twice, back to back, so a record that
        // straddles the end is contiguous here and the walk needs no wrap.
        let data = unsafe { std::slice::from_raw_parts(self.data, 2 * (self.mask as usize + 1)) };
        let (position, records) = walk(data, self.mask, from, published, &mut handle);

        // Release: the kernel must not reuse the space these records occupy
        // until the caller has been handed every one of them.
        consumer.store(position, Ordering::Release);
        records
    }
}

/// Walks the records between two positions, stopping at the first incomplete
/// one, and returns where the walk ended and how many records it yielded.
///
/// Separated from the mapping so it can be tested against a ring built by hand.
/// This is the part where an arithmetic slip does not crash: it hands out a
/// payload shifted by a few bytes, which decodes into plausible numbers.
fn walk(
    data: &[u8],
    mask: u64,
    from: u64,
    published: u64,
    handle: &mut impl FnMut(&[u8]),
) -> (u64, usize) {
    let mut position = from;
    let mut records = 0;
    while position < published {
        let at = (position & mask) as usize;
        let Some(bytes) = data.get(at..at + 4) else {
            // The header does not fit in the mapping, which cannot happen for a
            // ring the kernel produced. Stopping leaves the position where it
            // was, so nothing is silently skipped.
            break;
        };
        let header = u32::from_ne_bytes(bytes.try_into().expect("4 bytes"));
        if header & BUSY_BIT != 0 {
            // Still being written. Everything after it is newer, so the drain
            // stops here and resumes on the next tick.
            break;
        }
        let length = (header & LENGTH_MASK) as usize;
        if header & DISCARD_BIT == 0 {
            match data.get(at + 8..at + 8 + length) {
                Some(payload) => {
                    handle(payload);
                    records += 1;
                }
                None => break,
            }
        }
        // Records are eight-byte aligned, header included.
        position += ((length + 8 + 7) & !7) as u64;
    }
    (position, records)
}

impl Drop for RingBuffer {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.consumer as *mut libc::c_void, self.consumer_len);
            libc::munmap(self.producer as *mut libc::c_void, self.producer_len);
        }
    }
}

fn page_size() -> usize {
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as usize
    } else {
        4096
    }
}

/// Rounds a requested ring size up to something the kernel will accept: a
/// power of two, and a multiple of the page size.
pub fn ring_bytes(requested: usize) -> usize {
    let page = page_size();
    requested.max(page).next_power_of_two()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a ring by hand: records of `(payload, flags)` laid out the way a
    /// program writes them, in a buffer mirrored the way the kernel maps it.
    fn ring_of(size: usize, records: &[(&[u8], u32)]) -> (Vec<u8>, u64) {
        let mut data = vec![0u8; size];
        let mut position = 0usize;
        for (payload, flags) in records {
            let header = (payload.len() as u32) | flags;
            data[position..position + 4].copy_from_slice(&header.to_ne_bytes());
            data[position + 8..position + 8 + payload.len()].copy_from_slice(payload);
            position += (payload.len() + 8 + 7) & !7;
        }
        let published = position as u64;
        // The mirror: the second half of the mapping is the same memory again.
        let mirrored = [data.clone(), data].concat();
        (mirrored, published)
    }

    #[test]
    fn every_complete_record_comes_back_whole_and_in_order() {
        let (data, published) = ring_of(64, &[(b"first", 0), (b"second", 0)]);
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let (position, records) = walk(&data, 63, 0, published, &mut |payload| {
            seen.push(payload.to_vec())
        });
        assert_eq!(records, 2);
        assert_eq!(seen, vec![b"first".to_vec(), b"second".to_vec()]);
        assert_eq!(position, published, "a full walk consumes everything");
    }

    /// A record still being written must not be handed out, and the position
    /// must stay behind it: the next drain picks it up once it is complete.
    #[test]
    fn a_busy_record_stops_the_walk_without_consuming_it() {
        let (data, published) = ring_of(64, &[(b"done", 0), (b"writing", BUSY_BIT)]);
        let mut seen = 0;
        let (position, records) = walk(&data, 63, 0, published, &mut |_| seen += 1);
        assert_eq!((records, seen), (1, 1));
        assert_eq!(position, 16, "the walk stops at the busy record's header");
    }

    /// A discarded record occupies space and carries nothing. Skipping it must
    /// advance by its own length, or every record after it is read at the wrong
    /// offset -- which produces plausible numbers, not a crash.
    #[test]
    fn a_discarded_record_is_skipped_by_its_own_length() {
        let (data, published) = ring_of(64, &[(b"gone", DISCARD_BIT), (b"kept", 0)]);
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let (_, records) = walk(&data, 63, 0, published, &mut |payload| {
            seen.push(payload.to_vec())
        });
        assert_eq!(records, 1);
        assert_eq!(seen, vec![b"kept".to_vec()]);
    }

    /// The kernel's double mapping is what makes this legal: a record starting
    /// near the end runs past it into the mirror and is still contiguous.
    #[test]
    fn a_record_that_straddles_the_end_is_read_through_the_mirror() {
        let size = 64;
        let payload = b"straddles";
        let start = size - 8;
        let mut data = vec![0u8; size];
        data[start..start + 4].copy_from_slice(&(payload.len() as u32).to_ne_bytes());
        let mut mirrored = [data.clone(), data].concat();
        mirrored[start + 8..start + 8 + payload.len()].copy_from_slice(payload);

        let mut seen: Vec<Vec<u8>> = Vec::new();
        let published = start as u64 + ((payload.len() + 8 + 7) & !7) as u64;
        let (_, records) = walk(&mirrored, 63, start as u64, published, &mut |payload| {
            seen.push(payload.to_vec())
        });
        assert_eq!(records, 1);
        assert_eq!(seen[0], payload.to_vec());
    }

    /// A header claiming more payload than the mapping holds cannot come from
    /// a ring the kernel produced, so it means something is being read at the
    /// wrong offset. The walk stops without advancing, which loses nothing and
    /// hands out nothing, rather than slicing memory past the end.
    #[test]
    fn a_record_longer_than_the_mapping_stops_the_walk() {
        let mut data = vec![0u8; 64];
        data[0..4].copy_from_slice(&999u32.to_ne_bytes());
        let mirrored = [data.clone(), data].concat();
        let mut seen = 0;
        let (position, records) = walk(&mirrored, 63, 0, 64, &mut |_| seen += 1);
        assert_eq!((records, seen), (0, 0));
        assert_eq!(position, 0, "nothing was consumed, so nothing is skipped");
    }

    /// An empty ring is the common case on a quiet tick: no records, and the
    /// consumer position stays exactly where it was.
    #[test]
    fn an_empty_ring_consumes_nothing() {
        let (data, published) = ring_of(64, &[]);
        assert_eq!(published, 0);
        let (position, records) = walk(&data, 63, 0, published, &mut |_| unreachable!());
        assert_eq!((position, records), (0, 0));
    }

    #[test]
    fn a_ring_size_is_rounded_to_what_the_kernel_accepts() {
        let page = page_size();
        assert_eq!(ring_bytes(1), page);
        assert_eq!(ring_bytes(page + 1), (page * 2).next_power_of_two());
        assert!(ring_bytes(3 * page).is_power_of_two());
    }
}
