//! `mem.heap.alloc` through a preloaded shim.
//!
//! The shim lives in the observed program and writes one fixed-size record per
//! allocation and per free into a pipe. This end reads the pipe and turns the
//! records into events keyed by the range they cover, which is what lets a page
//! fault later be attributed to the allocation that owns its address.
//!
//! A preloaded library can only be given to a program at exec, so this source
//! works on a program `omn` launches and on nothing else. Saying that plainly
//! is better than attaching to a running process and reporting an empty heap.

use std::os::fd::RawFd;
use std::path::PathBuf;

use crate::collect::{Attachment, Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};

/// One record as the shim writes it: fixed size, native endian, both sides
/// being the same build on the same machine.
const RECORD_SIZE: usize = 32;

const OP_ALLOC: u8 = 1;
const OP_FREE: u8 = 2;
const OP_DROPPED: u8 = 3;

/// The pipe is sized up so a burst of allocations does not overflow it between
/// two drains. The shim drops rather than blocking, so this size is the
/// difference between a complete measurement and a partial one.
const WANTED_PIPE_BYTES: i32 = 1 << 20;

/// Locates the shim, which must sit next to the binary that loads it.
pub fn find_shim() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("OMN_PRELOAD_LIB") {
        let path = PathBuf::from(path);
        return if path.exists() {
            Ok(path)
        } else {
            Err(format!(
                "OMN_PRELOAD_LIB points at {}, which does not exist",
                path.display()
            ))
        };
    }
    let executable =
        std::env::current_exe().map_err(|error| format!("cannot find this executable: {error}"))?;
    let beside = executable
        .parent()
        .map(|directory| directory.join("libomn_preload.so"))
        .ok_or_else(|| "this executable has no directory".to_owned())?;
    if beside.exists() {
        Ok(beside)
    } else {
        Err(format!(
            "the heap shim was not found at {}; build it with `cargo build -p omn-preload`, \
             or point OMN_PRELOAD_LIB at it",
            beside.display()
        ))
    }
}

/// Holds the pipe and the shim path, before the observed program exists.
pub struct HeapPrepared {
    shim: PathBuf,
    read_fd: RawFd,
    write_fd: RawFd,
}

impl HeapPrepared {
    pub fn new() -> Result<Self, String> {
        let shim = find_shim()?;
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `pipe` fills two descriptors into an array we own.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(format!(
                "cannot create the report pipe: {}",
                std::io::Error::last_os_error()
            ));
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // SAFETY: both descriptors were just created by this process.
        unsafe {
            // Non-blocking on both ends, for opposite reasons: the reader must
            // not stall the run, and the writer must not stall the program
            // being measured. The shim counts what it could not write.
            libc::fcntl(read_fd, libc::F_SETFL, libc::O_NONBLOCK);
            libc::fcntl(write_fd, libc::F_SETFL, libc::O_NONBLOCK);
            libc::fcntl(write_fd, libc::F_SETPIPE_SZ, WANTED_PIPE_BYTES);
        }
        Ok(Self {
            shim,
            read_fd,
            write_fd,
        })
    }
}

impl Prepared for HeapPrepared {
    fn launch_environment(&self) -> Vec<(String, String)> {
        vec![
            ("LD_PRELOAD".to_owned(), self.shim.display().to_string()),
            ("OMN_HEAP_FD".to_owned(), self.write_fd.to_string()),
        ]
    }

    fn requires_launch(&self) -> Option<&'static str> {
        Some(
            "a preloaded shim can only be given to a program at exec, so this source needs \
             `-- <program>` rather than an already running process",
        )
    }

    fn attach(self: Box<Self>, attachment: Attachment) -> Result<Box<dyn Collector>, String> {
        // `requires_launch` already refuses anything but a launched program, so
        // reaching here without a process would mean the runner changed shape.
        let pid = attachment
            .pid()
            .ok_or_else(|| attachment.needs_a_process("mem.heap.alloc"))?;
        // The child has its own copy now. Closing this one is what makes the
        // read end report end-of-file when the program exits.
        // SAFETY: closing a descriptor this process owns, once.
        unsafe { libc::close(self.write_fd) };
        Ok(Box::new(HeapCollector {
            id: CollectorId::new("mem.heap.alloc/preload"),
            pid,
            read_fd: self.read_fd,
            pending: Vec::new(),
            losses: Losses::default(),
            allocations_seen: 0,
        }))
    }
}

/// Reads the shim's records and turns them into events.
pub struct HeapCollector {
    id: CollectorId,
    pid: i32,
    read_fd: RawFd,
    /// Bytes of a record that arrived split across two reads.
    pending: Vec<u8>,
    losses: Losses,
    allocations_seen: u64,
}

impl HeapCollector {
    fn read_available(&mut self) -> Vec<u8> {
        let mut collected = Vec::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            // SAFETY: reading into a buffer we own, from a descriptor we own.
            let read = unsafe {
                libc::read(
                    self.read_fd,
                    buffer.as_mut_ptr().cast::<libc::c_void>(),
                    buffer.len(),
                )
            };
            if read > 0 {
                collected.extend_from_slice(&buffer[..read as usize]);
                continue;
            }
            // Zero means the program is gone; a negative return with EAGAIN
            // means there is simply nothing more right now.
            break;
        }
        collected
    }

    fn decode(&mut self, bytes: &[u8], seq: u64) -> Vec<Event> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut index = seq;
        let whole = self.pending.len() / RECORD_SIZE * RECORD_SIZE;

        for record in self.pending[..whole].chunks_exact(RECORD_SIZE) {
            let read_u64 =
                |at: usize| u64::from_ne_bytes(record[at..at + 8].try_into().expect("8 bytes"));
            let ts_ns = read_u64(0);
            let ptr = read_u64(8);
            let size = read_u64(16);
            let tid = u32::from_ne_bytes(record[24..28].try_into().expect("4 bytes"));
            let op = record[28];

            if op == OP_DROPPED {
                self.losses.dropped += size;
                self.losses.note(
                    "the shim could not keep up: allocations happened faster than the pipe drained",
                );
                continue;
            }

            let (kind, length) = match op {
                OP_ALLOC => {
                    self.allocations_seen += 1;
                    ("alloc", size.max(1))
                }
                OP_FREE => ("free", 1),
                _ => {
                    self.losses.undecoded += 1;
                    continue;
                }
            };

            events.push(Event {
                ts_ns,
                ts_err_ns: 0,
                kind: "mem.heap.alloc".to_owned(),
                keys: vec![
                    KeyValue::task(tid as i32),
                    // The range this allocation owns. A free carries a
                    // one-byte range at the same base, which is what closes
                    // the interval it opened.
                    KeyValue::Mem {
                        base: ptr,
                        len: length,
                    },
                ],
                attrs: vec![
                    Attr::uint("bytes", if op == OP_ALLOC { size } else { 0 }),
                    Attr::text("op", kind),
                    Attr::uint("ptr", ptr),
                ],
                collector_id: self.id.clone(),
                seq: index,
            });
            index += 1;
        }

        self.pending.drain(..whole);
        events
    }
}

impl Collector for HeapCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "bytes"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let bytes = self.read_available();
        Ok(self.decode(&bytes, seq))
    }

    fn stop(&mut self) -> Vec<Event> {
        let bytes = self.read_available();
        let trailing = self.decode(&bytes, u64::MAX / 2);
        // SAFETY: closing a descriptor this collector owns, once.
        unsafe { libc::close(self.read_fd) };

        // A program that reported nothing is almost never a program that
        // allocated nothing. Saying which explanations are plausible beats
        // presenting an empty heap as a measurement.
        if self.allocations_seen == 0 {
            self.losses.note(format!(
                "no allocation was reported for pid {}: the program may be statically linked, \
                 may use its own allocator, or may never call malloc",
                self.pid
            ));
        }
        trailing
    }

    fn tick(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_millis(10))
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shim_is_built_next_to_the_binary_that_loads_it() {
        // The test binary lives in the same directory as the shim, so if the
        // workspace was built this must resolve.
        match find_shim() {
            Ok(path) => assert!(path.exists()),
            Err(message) => assert!(message.contains("cargo build"), "{message}"),
        }
    }

    #[test]
    fn records_split_across_reads_are_reassembled() {
        let mut collector = HeapCollector {
            id: CollectorId::new("test"),
            pid: 1,
            read_fd: -1,
            pending: Vec::new(),
            losses: Losses::default(),
            allocations_seen: 0,
        };

        let mut record = Vec::new();
        record.extend_from_slice(&7u64.to_ne_bytes()); // ts
        record.extend_from_slice(&0x1000u64.to_ne_bytes()); // ptr
        record.extend_from_slice(&4096u64.to_ne_bytes()); // size
        record.extend_from_slice(&42u32.to_ne_bytes()); // tid
        record.push(OP_ALLOC);
        record.extend_from_slice(&[0, 0, 0]);
        assert_eq!(record.len(), RECORD_SIZE);

        // Half a record is not an event yet.
        assert!(collector.decode(&record[..20], 0).is_empty());
        let events = collector.decode(&record[20..], 0);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].key(crate::request::KeyKind::Mem),
            Some(&KeyValue::Mem {
                base: 0x1000,
                len: 4096
            })
        );
        assert_eq!(events[0].attr("bytes").unwrap().as_f64(), Some(4096.0));
    }

    #[test]
    fn a_dropped_record_becomes_a_reported_loss() {
        let mut collector = HeapCollector {
            id: CollectorId::new("test"),
            pid: 1,
            read_fd: -1,
            pending: Vec::new(),
            losses: Losses::default(),
            allocations_seen: 0,
        };
        let mut record = Vec::new();
        record.extend_from_slice(&0u64.to_ne_bytes());
        record.extend_from_slice(&0u64.to_ne_bytes());
        record.extend_from_slice(&17u64.to_ne_bytes()); // how many were dropped
        record.extend_from_slice(&0u32.to_ne_bytes());
        record.push(OP_DROPPED);
        record.extend_from_slice(&[0, 0, 0]);

        assert!(collector.decode(&record, 0).is_empty());
        assert_eq!(collector.losses().dropped, 17);
        assert!(!collector.losses().is_clean());
    }
}
