//! The heap sources, and the shim every preload source shares.
//!
//! One preloaded library reports every allocation and every free of the
//! observed program through a pipe, and three sources read that same stream:
//! `mem.heap.alloc` turns each record into an event, `mem.heap.live` keeps the
//! running total of what has been allocated and not freed, and
//! `mem.heap.retained` asks the kernel how much of what was freed is still
//! resident. They differ in what they remember, not in what they observe.
//!
//! A preloaded library can only be given to a program at exec, so these sources
//! work on a program `omn` launches and on nothing else. Saying that plainly is
//! better than attaching to a running process and reporting an empty heap.
//!
//! Several of them on one launch read **one** pipe. A program has one
//! environment and the shim writes to the one descriptor `OMN_HEAP_FD` names,
//! so a pipe each would be two answers for one variable -- which is how asking
//! for allocations and retained memory of one program used to be refused.
//! Whichever collector drains first reads what the shim wrote and hands every
//! other collector of the launch its own copy of the bytes, so each decodes,
//! counts its losses and remembers as if the pipe were its own.
//!
//! **The shim is not only the heap's any more.** Since the ninth wave it also
//! reports mutex acquisitions (`sched.mutex`) and GPU API calls (`gpu.api`),
//! through the same pipe, so a launch that asks for allocations and lock waits
//! still gives the program one descriptor. Each source subscribes to a
//! *class*, the program is told the union in `OMN_SHIM_REPORT`, and each
//! stream decodes only its own class's records and counts only its own class's
//! drops. The pipe lives here because the heap was here first; the mutex and
//! GPU readers are in their own families and build on [`ShimPrepared`] and
//! [`Stream`].

mod alloc;
mod live;
mod pagemap;
mod retained;
mod state;

use std::cell::RefCell;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::rc::Rc;

use crate::collect::{Collector, Losses, Prepared};
use crate::request::{Mode, Target};

/// One record as the shim writes it: fixed size, native endian, both sides
/// being the same build on the same machine. `shim/src/lib.rs` asserts the
/// same layout from its side.
const RECORD_SIZE: usize = 40;

const OP_ALLOC: u8 = 1;
const OP_FREE: u8 = 2;
const OP_DROPPED: u8 = 3;
pub const OP_MUTEX: u8 = 4;
pub const OP_GPU_CALL: u8 = 5;

/// The classes the shim reports, as `OMN_SHIM_REPORT` names them.
const CLASS_HEAP: u8 = 1;
const CLASS_MUTEX: u8 = 2;
const CLASS_GPU: u8 = 4;

/// The pipe is sized up so a burst of allocations does not overflow it between
/// two drains. The shim drops rather than blocking, so this size is the
/// difference between a complete measurement and a partial one.
const WANTED_PIPE_BYTES: i32 = 1 << 20;

/// How often the pipe is drained, whatever the request's poll interval is.
///
/// These collectors have two cadences: the stream has to be read often enough
/// that the pipe never fills, and the value is published at the interval the
/// request asked for. Conflating them would mean either losing records or
/// reporting a series far denser than anyone asked for.
pub const DRAIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// One record, as it left the shim. What `ptr`, `size` and `extra` mean is the
/// operation's: an address and a size for the heap, a mutex, its wait and its
/// hold for a lock, an entry point, a duration and a return code for a GPU
/// call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub ts_ns: u64,
    pub ptr: u64,
    pub size: u64,
    pub extra: u64,
    pub tid: u32,
    pub op: u8,
    pub flags: u8,
}

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
            "the preload shim was not found at {}; build it with `make` (or \
             `cargo build -p omn-preload`), or point OMN_PRELOAD_LIB at it",
            beside.display()
        ))
    }
}

/// Which source is being served by this shim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Alloc,
    Live,
    Retained,
    Mutex,
    GpuApi,
}

impl Which {
    fn source_id(self) -> &'static str {
        match self {
            Which::Alloc => "mem.heap.alloc",
            Which::Live => "mem.heap.live",
            Which::Retained => "mem.heap.retained",
            Which::Mutex => "sched.mutex",
            Which::GpuApi => "gpu.api",
        }
    }

    /// The class of records this source reads, which is also what the program
    /// is asked to report on its behalf.
    fn class(self) -> u8 {
        match self {
            Which::Alloc | Which::Live | Which::Retained => CLASS_HEAP,
            Which::Mutex => CLASS_MUTEX,
            Which::GpuApi => CLASS_GPU,
        }
    }

    /// What the program did that this source is about, for the notes that say
    /// it did not happen or was lost.
    fn activity(self) -> &'static str {
        match self.class() {
            CLASS_HEAP => "allocations",
            CLASS_MUTEX => "mutex acquisitions",
            _ => "GPU API calls",
        }
    }
}

/// The shim's pipe, and every collector of one launch that reads it.
///
/// Bytes rather than records are fanned out, so that a record split across
/// two reads is reassembled by each reader exactly as it was with a pipe of
/// its own, and a drop the shim reports is counted by every source it cost.
struct Pipe {
    read_fd: RawFd,
    /// Open until the program has been forked with its copy, and then closed:
    /// the read end reports end-of-file only once no writer is left.
    write_fd: RawFd,
    /// Bytes read and not yet taken, one buffer per reader.
    unread: Vec<Vec<u8>>,
    /// What the program is asked to report: every class a collector of this
    /// launch reads.
    classes: u8,
}

impl Pipe {
    fn new(read_fd: RawFd, write_fd: RawFd) -> Self {
        Self {
            read_fd,
            write_fd,
            unread: Vec::new(),
            classes: 0,
        }
    }

    /// A new reader, which sees everything read from now on.
    fn subscribe(&mut self) -> usize {
        self.unread.push(Vec::new());
        self.unread.len() - 1
    }

    fn close_write(&mut self) {
        if self.write_fd >= 0 {
            // SAFETY: closing a descriptor this pipe owns, once.
            unsafe { libc::close(self.write_fd) };
            self.write_fd = -1;
        }
    }

    /// Reads whatever the shim has written, gives every reader a copy, and
    /// returns what `reader` had not taken yet.
    fn take(&mut self, reader: usize) -> Vec<u8> {
        let mut collected = Vec::new();
        let mut buffer = [0u8; 64 * 1024];
        if self.read_fd < 0 {
            return std::mem::take(&mut self.unread[reader]);
        }
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
        if !collected.is_empty() {
            for unread in &mut self.unread {
                unread.extend_from_slice(&collected);
            }
        }
        std::mem::take(&mut self.unread[reader])
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        self.close_write();
        if self.read_fd >= 0 {
            // SAFETY: closing a descriptor this pipe owns, once, when the
            // last reader of it is gone.
            unsafe { libc::close(self.read_fd) };
        }
    }
}

/// What a preload collector is built from once its program exists: the
/// stream of its class, and the interval its request asked for.
pub type Build = fn(Stream, std::time::Duration) -> Result<Box<dyn Collector>, String>;

/// The name the `mem` family's factories use, which predates the other
/// classes.
pub type HeapPrepared = ShimPrepared;

/// Holds the pipe and the shim path, before the observed program exists.
pub struct ShimPrepared {
    which: Which,
    interval: std::time::Duration,
    shim: PathBuf,
    /// Shared with every other preload collector of the same launch once
    /// [`Prepared::share_launch`] has been asked.
    pipe: Rc<RefCell<Pipe>>,
    build: Build,
}

impl ShimPrepared {
    /// A heap source.
    pub fn new(which: Which, mode: Mode) -> Result<Self, String> {
        // A snapshot is a single read taken as the run starts. For a program
        // the tool has to launch, that instant is before it has allocated
        // anything, so the answer would be zero and would look like a
        // measurement. The allocator-stats backend can answer a snapshot; this
        // one cannot, and says so rather than returning the number.
        let interval = match mode {
            Mode::Poll { interval } => std::time::Duration::from_nanos(interval.as_nanos()),
            Mode::Snapshot => {
                return Err(format!(
                    "`{}` through the preloaded shim watches a program from exec to exit, so a \
                     snapshot would be taken before it allocated anything; use `--mode poll`",
                    which.source_id()
                ))
            }
            _ => std::time::Duration::from_nanos(Mode::DEFAULT_POLL_INTERVAL.as_nanos()),
        };
        let build: Build = match which {
            Which::Alloc => |stream, _| Ok(Box::new(alloc::AllocCollector::new(stream))),
            Which::Live => {
                |stream, interval| Ok(Box::new(live::LiveCollector::new(stream, interval)))
            }
            Which::Retained => |stream, interval| {
                retained::RetainedCollector::new(stream, interval)
                    .map(|collector| collector as Box<dyn Collector>)
            },
            Which::Mutex | Which::GpuApi => {
                return Err(format!(
                "`{}` is not a heap source; its family builds it with `ShimPrepared::reporting`",
                which.source_id()
            ))
            }
        };
        Self::reporting(which, interval, build)
    }

    /// A source of any class, built by `build` once the program exists.
    pub fn reporting(
        which: Which,
        interval: std::time::Duration,
        build: Build,
    ) -> Result<Self, String> {
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
        let mut pipe = Pipe::new(read_fd, write_fd);
        pipe.classes = which.class();
        Ok(Self {
            which,
            interval,
            shim,
            pipe: Rc::new(RefCell::new(pipe)),
            build,
        })
    }
}

impl Prepared for ShimPrepared {
    /// The same three variables from every collector sharing one pipe, since
    /// they read them from the pipe rather than from themselves: the runner
    /// merges environments after sharing, and refuses two values for one
    /// variable.
    fn launch_environment(&self) -> Vec<(String, String)> {
        let pipe = self.pipe.borrow();
        vec![
            ("LD_PRELOAD".to_owned(), self.shim.display().to_string()),
            ("OMN_SHIM_FD".to_owned(), pipe.write_fd.to_string()),
            ("OMN_SHIM_REPORT".to_owned(), pipe.classes.to_string()),
        ]
    }

    fn share_launch(&mut self, earlier: &mut dyn Prepared) -> bool {
        let Some(earlier) = earlier
            .shareable()
            .and_then(|earlier| earlier.downcast_mut::<ShimPrepared>())
        else {
            return false;
        };
        // The same shim, so the same stream: what differs between the preload
        // sources is what they remember and which class they decode, not what
        // they read. This one's own pipe is closed as the last handle to it
        // goes.
        if earlier.shim != self.shim {
            return false;
        }
        earlier.pipe.borrow_mut().classes |= self.which.class();
        self.pipe = Rc::clone(&earlier.pipe);
        true
    }

    fn shareable(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    fn requires_launch(&self) -> Option<&'static str> {
        Some(
            "a preloaded shim can only be given to a program at exec, so this source needs \
             `-- <program>` rather than an already running process",
        )
    }

    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "the shim reports on one process, and a {} target names none",
                target.kind()
            )
        })?;
        // The child has its own copy now. Closing this one is what makes the
        // read end report end-of-file when the program exits.
        self.pipe.borrow_mut().close_write();
        let stream = Stream::reading(Rc::clone(&self.pipe), pid, self.which);
        (self.build)(stream, self.interval)
    }
}

/// The shim's side of the pipe, decoded into records.
///
/// Every heap collector holds one of these and differs only in what it does
/// with the records: the decoding, the reassembly of a record split across two
/// reads, and the accounting of what the shim had to drop belong here once.
pub struct Stream {
    pipe: Rc<RefCell<Pipe>>,
    reader: usize,
    pid: i32,
    which: Which,
    /// Bytes of a record that arrived split across two reads.
    pending: Vec<u8>,
    losses: Losses,
    /// Records of this stream's class, for the note that there were none.
    seen: u64,
    /// Why the run may have seen nothing, once it is over and did.
    silence: Option<String>,
}

impl Stream {
    /// A heap stream that is the only reader of `read_fd`.
    #[cfg(test)]
    fn new(read_fd: RawFd, pid: i32) -> Self {
        Self::only_reader(read_fd, pid, Which::Alloc)
    }

    /// A stream of `which`'s class that is the only reader of `read_fd`, for
    /// the tests of the collectors built on it.
    #[cfg(test)]
    pub fn only_reader(read_fd: RawFd, pid: i32, which: Which) -> Self {
        Self::reading(Rc::new(RefCell::new(Pipe::new(read_fd, -1))), pid, which)
    }

    fn reading(pipe: Rc<RefCell<Pipe>>, pid: i32, which: Which) -> Self {
        let reader = pipe.borrow_mut().subscribe();
        Self {
            pipe,
            reader,
            pid,
            which,
            pending: Vec::new(),
            losses: Losses::default(),
            seen: 0,
            silence: None,
        }
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Reads whatever the shim has written since the last call.
    pub fn drain(&mut self) -> Vec<Record> {
        let bytes = self.read_available();
        self.decode(&bytes)
    }

    fn read_available(&mut self) -> Vec<u8> {
        self.pipe.borrow_mut().take(self.reader)
    }

    fn decode(&mut self, bytes: &[u8]) -> Vec<Record> {
        self.pending.extend_from_slice(bytes);
        let mut records = Vec::new();
        let whole = self.pending.len() / RECORD_SIZE * RECORD_SIZE;

        for record in self.pending[..whole].chunks_exact(RECORD_SIZE) {
            let read_u64 =
                |at: usize| u64::from_ne_bytes(record[at..at + 8].try_into().expect("8 bytes"));
            let record = Record {
                ts_ns: read_u64(0),
                ptr: read_u64(8),
                size: read_u64(16),
                extra: read_u64(24),
                tid: u32::from_ne_bytes(record[32..36].try_into().expect("4 bytes")),
                op: record[36],
                flags: record[37],
            };

            let class = self.which.class();
            // Another class's records are another reader's, and are skipped
            // rather than counted as undecoded: they were decoded, by it.
            let mine = match record.op {
                OP_DROPPED => {
                    // A drop carries its class in `ptr`, and costs only the
                    // sources of that class.
                    if record.ptr as u8 & class != 0 {
                        self.losses.dropped += record.size;
                        self.losses.note(format!(
                            "the shim could not keep up: {} happened faster than the pipe \
                             drained",
                            self.which.activity()
                        ));
                    }
                    continue;
                }
                OP_ALLOC | OP_FREE => class == CLASS_HEAP,
                OP_MUTEX => class == CLASS_MUTEX,
                OP_GPU_CALL => class == CLASS_GPU,
                _ => {
                    self.losses.undecoded += 1;
                    continue;
                }
            };
            if !mine {
                continue;
            }
            if record.op != OP_FREE {
                self.seen += 1;
            }
            records.push(record);
        }

        self.pending.drain(..whole);
        records
    }

    /// Reports what the run could not see. The read end closes with the last
    /// stream reading it.
    ///
    /// The explanation is a note rather than a loss reason: nothing was lost,
    /// and a reason beside clean counters is never rendered -- which is how
    /// this sentence sat unprinted for every heap source until the ninth wave
    /// ran `gpu.api` on a program the shim cannot see and got a silent zero.
    pub fn close(&mut self) {
        // A program that reported nothing is almost never a program that did
        // nothing. Saying which explanations are plausible beats presenting an
        // empty result as a measurement.
        if self.seen == 0 {
            let why = match self.which.class() {
                CLASS_HEAP => {
                    "no allocation was reported for pid {pid}: the program may be statically \
                     linked, may use its own allocator, or may never call malloc"
                }
                CLASS_MUTEX => {
                    "no pthread mutex was taken by pid {pid}: the program may be statically \
                     linked, or its locks may not be pthread mutexes -- Rust's standard \
                     library, Go and most language runtimes wait on futexes directly, which \
                     `sched.futex` sees and this shim cannot"
                }
                _ => {
                    "no GPU API call was reported for pid {pid}: the shim sees only calls bound \
                     through the dynamic linker, so a program that reaches the driver through \
                     `dlopen` and `dlsym` (Python's ctypes), `cuGetProcAddress` (the CUDA \
                     runtime does, for every driver call) or `vkGetDeviceProcAddr` reports \
                     nothing here whatever it submitted"
                }
            };
            self.silence = Some(why.replace("{pid}", &self.pid.to_string()));
        }
    }

    /// What reaches the output beside the numbers: why there are none, when
    /// there are none.
    pub fn notes(&self) -> Vec<String> {
        self.silence.iter().cloned().collect()
    }

    pub fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A stream fed by a pipe this test writes into, which is the same path the
    /// shim takes: records arrive in whatever chunks the reader happens to get.
    pub(super) fn piped() -> (RawFd, Stream) {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `pipe` fills two descriptors into an array we own.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: the read end was just created by this process.
        unsafe { libc::fcntl(fds[0], libc::F_SETFL, libc::O_NONBLOCK) };
        (fds[1], Stream::new(fds[0], std::process::id() as i32))
    }

    pub fn write(fd: RawFd, bytes: &[u8]) {
        // SAFETY: writing bytes we own to a descriptor this test owns.
        let written =
            unsafe { libc::write(fd, bytes.as_ptr().cast::<libc::c_void>(), bytes.len()) };
        assert_eq!(written, bytes.len() as isize);
    }

    pub(super) fn record(op: u8, ptr: u64, size: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&7u64.to_ne_bytes()); // ts
        bytes.extend_from_slice(&ptr.to_ne_bytes());
        bytes.extend_from_slice(&size.to_ne_bytes());
        bytes.extend_from_slice(&0u64.to_ne_bytes()); // extra
        bytes.extend_from_slice(&42u32.to_ne_bytes()); // tid
        bytes.push(op);
        bytes.extend_from_slice(&[0, 0, 0]);
        assert_eq!(bytes.len(), RECORD_SIZE);
        bytes
    }

    /// A record of any operation, with every field chosen by the test.
    pub fn full_record(op: u8, ptr: u64, size: u64, extra: u64, tid: u32, flags: u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1_000u64.to_ne_bytes()); // ts
        bytes.extend_from_slice(&ptr.to_ne_bytes());
        bytes.extend_from_slice(&size.to_ne_bytes());
        bytes.extend_from_slice(&extra.to_ne_bytes());
        bytes.extend_from_slice(&tid.to_ne_bytes());
        bytes.push(op);
        bytes.push(flags);
        bytes.extend_from_slice(&[0, 0]);
        assert_eq!(bytes.len(), RECORD_SIZE);
        bytes
    }

    /// A pipe any test of a preload collector can write records into.
    pub fn pipe_pair() -> (RawFd, RawFd) {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `pipe` fills two descriptors into an array we own.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: the read end was just created by this process.
        unsafe { libc::fcntl(fds[0], libc::F_SETFL, libc::O_NONBLOCK) };
        (fds[0], fds[1])
    }

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
        let (writer, mut stream) = piped();
        let bytes = record(OP_ALLOC, 0x1000, 4096);

        // Half a record is not a record yet.
        write(writer, &bytes[..20]);
        assert!(stream.drain().is_empty());
        write(writer, &bytes[20..]);
        let records = stream.drain();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].ptr, 0x1000);
        assert_eq!(records[0].size, 4096);
    }

    #[test]
    fn every_reader_of_one_pipe_sees_every_record_once() {
        let (writer, first) = piped();
        let pipe = Rc::clone(&first.pipe);
        let mut first = first;
        let mut second = Stream::reading(pipe, 1, Which::Live);
        let bytes = record(OP_ALLOC, 0x1000, 4096);
        // Split across two drains, taken by different readers, so each has
        // to reassemble from the half it was handed.
        write(writer, &bytes[..20]);
        assert!(first.drain().is_empty());
        write(writer, &bytes[20..]);
        write(writer, &record(OP_DROPPED, u64::from(CLASS_HEAP), 3));
        let by_second = second.drain();
        let by_first = first.drain();
        assert_eq!(by_first, by_second);
        assert_eq!(by_first.len(), 1);
        assert_eq!(by_first[0].size, 4096);
        // A second drain finds nothing: fanned out, not replayed.
        assert!(first.drain().is_empty() && second.drain().is_empty());
        // What the shim dropped cost both sources.
        assert_eq!(first.losses().dropped, 3);
        assert_eq!(second.losses().dropped, 3);
    }

    /// Two preload sources of one launch give the program one pipe, and the
    /// program is asked for both classes: every collector reports the same
    /// three variables, or the runner would refuse the launch.
    #[test]
    fn sources_of_two_classes_share_one_pipe_and_ask_for_both() {
        let mut heap = ShimPrepared::new(Which::Alloc, Mode::Trace).expect("a heap source");
        let mut mutex = ShimPrepared::reporting(Which::Mutex, DRAIN_INTERVAL, |stream, _| {
            Ok(Box::new(alloc::AllocCollector::new(stream)))
        })
        .expect("a mutex source");
        assert!(mutex.share_launch(&mut heap), "the same shim is one stream");
        let first = heap.launch_environment();
        let second = mutex.launch_environment();
        assert_eq!(first, second);
        let report = first
            .iter()
            .find(|(name, _)| name == "OMN_SHIM_REPORT")
            .map(|(_, value)| value.as_str());
        assert_eq!(report, Some("3"), "heap (1) and mutex (2): {first:?}");
    }

    #[test]
    fn a_heap_source_alone_asks_for_the_heap_alone() {
        let heap = ShimPrepared::new(Which::Live, Mode::Trace).expect("a heap source");
        assert!(heap
            .launch_environment()
            .contains(&("OMN_SHIM_REPORT".to_owned(), "1".to_owned())));
    }

    #[test]
    fn a_dropped_record_becomes_a_reported_loss() {
        let (writer, mut stream) = piped();
        // The shim reports how many it dropped in the size field, and whose
        // they were in the pointer field.
        write(writer, &record(OP_DROPPED, u64::from(CLASS_HEAP), 17));
        assert!(stream.drain().is_empty());
        assert_eq!(stream.losses().dropped, 17);
        assert!(!stream.losses().is_clean());
    }

    #[test]
    fn a_snapshot_of_a_program_that_must_be_launched_is_refused() {
        let error = HeapPrepared::new(Which::Live, Mode::Snapshot)
            .err()
            .expect("a snapshot cannot be served by the shim");
        assert!(error.contains("before it allocated anything"), "{error}");
    }
}
