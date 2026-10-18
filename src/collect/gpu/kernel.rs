//! `gpu.kernel`: every kernel, copy and memset the device ran for a launched
//! program, with its time on the device, through CUPTI.
//!
//! NVIDIA's driver has no kernel interface below it, so the device's own
//! account of what ran comes from CUPTI's activity API, and that API works
//! only inside the process that owns the CUDA context. So `omn` hands the
//! program an injection library at exec (`CUDA_INJECTION64_PATH`), which the
//! driver loads inside `cuInit`; the library subscribes to kernel, memcpy and
//! memset activity and sends every record back over a socket made for this
//! launch (`cupti/src/lib.rs` is the other half, `cupti/src/wire.rs` the
//! format both sides compile). A running process cannot be given a library at
//! exec, so a `pid` target is refused with that reason.
//!
//! **One event per activity**, stamped at its start on the device converted
//! to `CLOCK_MONOTONIC`, carrying its duration on the device (`duration_ns`),
//! what it was (`op`: `kernel`, `memcpy` or `memset`), its name (`kernel`:
//! the kernel's demangled name, or `[memcpy HtoD]` and the like), its device,
//! stream, launch geometry and the bytes a copy moved.
//!
//! **Every timestamp carries its error.** CUPTI's clock is not
//! `CLOCK_MONOTONIC`; the library brackets it between two monotonic reads as
//! the run starts, at every flush and as it ends, and [`Calibration`] turns
//! the brackets into the map and its measured error, which every event
//! carries as `ts_err_ns`.
//!
//! **Lost records are counted and said.** CUPTI drops records when it has no
//! buffer to put them in, and says how many only if asked: the library asks
//! after every buffer, and the count is this run's `dropped`. Records the
//! library could not send, records handed back before their activity
//! finished, and a run whose program never said it had flushed everything are
//! each counted beside it.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::clock::{Bracket, Calibration};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::Target;

#[allow(dead_code)]
#[path = "../../../cupti/src/wire.rs"]
pub(super) mod wire;

/// Where the injection library is looked for, relative to the directory of
/// the `omn` binary: beside it (the build tree and the release's flat
/// `bin/`), then `<prefix>/lib/omn/`, as for the preload shim.
const LIBRARY_PLACES: [&str; 2] = ["libomn_cupti.so", "../lib/omn/libomn_cupti.so"];

/// How often the socket is drained. The library sends a batch per CUPTI
/// buffer and per flush, so this only bounds how long a batch waits.
const DRAIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

/// The most activity events one run keeps, as for the preload sources: past
/// it, what arrives is read, dropped and counted, so what `omn` holds does
/// not grow with the program it watches.
const HELD_EVENTS: u64 = 1 << 21;

const REFUSES_PID: &str = "CUPTI records the activity of the process it is loaded in, and it is \
     loaded by the CUDA driver at `cuInit` from `CUDA_INJECTION64_PATH`, so this source needs \
     `-- <program>`: a running process has initialised CUDA already and cannot be given a \
     library at exec";

/// Locates the injection library: `OMN_CUPTI_INJECTION` when it is set,
/// otherwise beside the binary or in `<exe dir>/../lib/omn/`.
pub fn find_library() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("OMN_CUPTI_INJECTION") {
        let path = PathBuf::from(path);
        return if path.exists() {
            Ok(path)
        } else {
            Err(format!(
                "OMN_CUPTI_INJECTION points at {}, which does not exist",
                path.display()
            ))
        };
    }
    let executable =
        std::env::current_exe().map_err(|error| format!("cannot find this executable: {error}"))?;
    let directory = executable
        .parent()
        .ok_or_else(|| "this executable has no directory".to_owned())?;
    library_in(directory)
}

fn library_in(directory: &Path) -> Result<PathBuf, String> {
    let places: Vec<PathBuf> = LIBRARY_PLACES
        .iter()
        .map(|place| directory.join(place))
        .collect();
    if let Some(found) = places.iter().find(|place| place.exists()) {
        return Ok(found.clone());
    }
    Err(format!(
        "the CUDA injection library was not found at {} or {}; it ships beside the binary, so \
         reinstall, or build it with `make` (or `cargo build -p omn-cupti`) in a checkout, or \
         point OMN_CUPTI_INJECTION at it",
        places[0].display(),
        places[1].display()
    ))
}

pub fn prepare() -> Result<Box<dyn Prepared>, String> {
    Ok(Box::new(KernelPrepared::new(
        find_library()?,
        wire::REPORT_ACTIVITY,
    )?))
}

/// The channel of one launch: the socket, the library the program is given,
/// what it is asked to report, and the messages read and not yet taken, one
/// queue per reader.
///
/// A program has one environment, so every collector of a launch that goes
/// through the injection library reads **one** socket: whichever drains
/// first hands every other reader its own copy of each message, as the
/// preload shim's pipe does for its readers. A later CUPTI source --
/// counters, PC sampling -- joins by adding its `REPORT_*` bit to `report`
/// and subscribing ([`KernelPrepared::share_launch`]).
pub(super) struct Socket {
    library: PathBuf,
    /// `omn`'s end: non-blocking, closed on exec.
    read_fd: RawFd,
    /// The program's end, inherited across exec and closed once the program
    /// has been forked with its copy.
    write_fd: RawFd,
    report: u32,
    unread: Vec<Vec<Vec<u8>>>,
    /// Every process holding the other end is gone.
    closed: bool,
}

impl Socket {
    fn new(library: PathBuf, report: u32) -> Result<Self, String> {
        let (read_fd, write_fd) = socket_pair()?;
        Ok(Self {
            library,
            read_fd,
            write_fd,
            report,
            unread: Vec::new(),
            closed: false,
        })
    }

    /// A new reader, which sees every message read from now on.
    pub(super) fn subscribe(&mut self) -> usize {
        self.unread.push(Vec::new());
        self.unread.len() - 1
    }

    /// What the program is asked to report: every reader's bit.
    pub(super) fn report(&self) -> u32 {
        self.report
    }

    pub(super) fn close_write(&mut self) {
        close(&mut self.write_fd);
    }

    /// Reads every message waiting, gives every reader a copy, and returns
    /// what `reader` had not taken yet.
    pub(super) fn take(&mut self, reader: usize) -> Vec<Vec<u8>> {
        let mut buffer = vec![0u8; wire::MESSAGE_MAX + 4096];
        while self.read_fd >= 0 {
            // SAFETY: reading into a buffer we own from a descriptor we own.
            let read = unsafe {
                libc::recv(
                    self.read_fd,
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if read > 0 {
                let message = buffer[..read as usize].to_vec();
                for unread in &mut self.unread {
                    unread.push(message.clone());
                }
                continue;
            }
            if read == 0 {
                close(&mut self.read_fd);
                self.closed = true;
            }
            break;
        }
        std::mem::take(&mut self.unread[reader])
    }

    fn launch_environment(&self) -> Vec<(String, String)> {
        vec![
            (
                "CUDA_INJECTION64_PATH".to_owned(),
                self.library.display().to_string(),
            ),
            (wire::FD_VARIABLE.to_owned(), self.write_fd.to_string()),
            (wire::REPORT_VARIABLE.to_owned(), self.report.to_string()),
        ]
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        close(&mut self.read_fd);
        close(&mut self.write_fd);
    }
}

/// The socket and the library, before the program exists.
pub struct KernelPrepared {
    socket: Rc<RefCell<Socket>>,
    reader: usize,
}

impl KernelPrepared {
    pub(super) fn new(library: PathBuf, report: u32) -> Result<Self, String> {
        let mut socket = Socket::new(library, report)?;
        let reader = socket.subscribe();
        Ok(Self {
            socket: Rc::new(RefCell::new(socket)),
            reader,
        })
    }

    /// The channel this collector reads, for a CUPTI source of the same
    /// launch that joins it (`counters.rs`).
    pub(super) fn socket(&self) -> Rc<RefCell<Socket>> {
        Rc::clone(&self.socket)
    }

    /// This collector's reader of the channel.
    pub(super) fn reader(&self) -> usize {
        self.reader
    }
}

/// A sequenced-packet pair: message boundaries kept, and two senders -- a
/// program that forked after `cuInit` -- never interleaved within one.
fn socket_pair() -> Result<(RawFd, RawFd), String> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `socketpair` fills two descriptors into an array we own.
    let made =
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) };
    if made != 0 {
        return Err(format!(
            "cannot create the CUPTI report socket: {}",
            std::io::Error::last_os_error()
        ));
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    let wanted: libc::c_int = 4 << 20;
    // SAFETY: setting flags and options on two descriptors this process just
    // made. A buffer size the kernel caps lower is a slower drain, not an
    // error: the library waits, bounded, rather than dropping at once.
    unsafe {
        libc::fcntl(read_fd, libc::F_SETFL, libc::O_NONBLOCK);
        libc::fcntl(read_fd, libc::F_SETFD, libc::FD_CLOEXEC);
        for (fd, option) in [(read_fd, libc::SO_RCVBUF), (write_fd, libc::SO_SNDBUF)] {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                option,
                (&wanted as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
    Ok((read_fd, write_fd))
}

fn close(fd: &mut RawFd) {
    if *fd >= 0 {
        // SAFETY: closing a descriptor this collector owns, once.
        unsafe { libc::close(*fd) };
        *fd = -1;
    }
}

impl Prepared for KernelPrepared {
    /// The same three variables from every collector sharing one socket, as
    /// they are read from the socket rather than from the collector.
    fn launch_environment(&self) -> Vec<(String, String)> {
        self.socket.borrow().launch_environment()
    }

    fn share_launch(&mut self, earlier: &mut dyn Prepared) -> bool {
        let Some(earlier) = earlier
            .shareable()
            .and_then(|earlier| earlier.downcast_mut::<KernelPrepared>())
        else {
            return false;
        };
        let mine = self.socket.borrow();
        let mut theirs = earlier.socket.borrow_mut();
        if mine.library != theirs.library {
            return false;
        }
        theirs.report |= mine.report;
        let reader = theirs.subscribe();
        drop(theirs);
        drop(mine);
        self.socket = Rc::clone(&earlier.socket);
        self.reader = reader;
        true
    }

    fn shareable(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    fn requires_launch(&self) -> Option<&'static str> {
        Some(REFUSES_PID)
    }

    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let pid = target
            .first_pid()
            .ok_or_else(|| format!("{REFUSES_PID}; a {} target names none", target.kind()))?;
        // The program has its copy. Closing ours is what makes the socket
        // report end-of-file once every process holding it is gone.
        self.socket.borrow_mut().close_write();
        Ok(Box::new(KernelCollector::new(
            Rc::clone(&self.socket),
            self.reader,
            pid,
        )))
    }
}

/// What one process said about itself.
#[derive(Debug, Default)]
struct Sender {
    names: HashMap<u32, String>,
    calibration: Calibration,
    hello: Option<Hello>,
    ended: bool,
    /// The library's exit handler started ([`wire::OP_EXITING`]).
    exiting: bool,
    /// Records the library could not send, as it last reported them.
    lost: u64,
    /// The process's `proc` key, read once: two `/proc` reads per event
    /// made a drain of 20 000 kernels outrun its tick by 290 ms.
    proc_key: Option<Option<KeyValue>>,
}

#[derive(Debug, Clone, Copy)]
struct Hello {
    version: u32,
    buffer_bytes: u64,
    buffers: u64,
}

pub struct KernelCollector {
    id: CollectorId,
    socket: Rc<RefCell<Socket>>,
    reader: usize,
    launched: i32,
    senders: BTreeMap<u32, Sender>,
    losses: Losses,
    errors: Vec<String>,
    held: u64,
    /// The largest error any event of the run carried.
    widest_err_ns: u64,
    /// Activities that reached `omn`, by op.
    by_op: BTreeMap<&'static str, u64>,
}

impl KernelCollector {
    fn new(socket: Rc<RefCell<Socket>>, reader: usize, launched: i32) -> Self {
        Self {
            id: CollectorId::new("gpu.kernel/vendor:cupti"),
            socket,
            reader,
            launched,
            senders: BTreeMap::new(),
            losses: Losses::default(),
            errors: Vec::new(),
            held: 0,
            widest_err_ns: 0,
            by_op: BTreeMap::new(),
        }
    }

    fn drain(&mut self, seq: u64) -> Vec<Event> {
        let mut events = Vec::new();
        let messages = self.socket.borrow_mut().take(self.reader);
        for message in messages {
            self.message(&message, seq, &mut events);
        }
        events
    }

    fn message(&mut self, message: &[u8], seq: u64, events: &mut Vec<Event>) {
        let Some((pid, frames)) = wire::parse(message) else {
            self.losses.undecoded += 1;
            self.losses.note(
                "a message from the injection library was not one this build reads: the library \
                 and `omn` are from different builds",
            );
            return;
        };
        for frame in frames {
            self.frame(pid, frame, seq + events.len() as u64, events);
        }
    }

    fn frame(&mut self, pid: u32, frame: wire::Frame<'_>, seq: u64, events: &mut Vec<Event>) {
        let sender = self.senders.entry(pid).or_default();
        let payload = frame.payload;
        let mut read = wire::Reader { payload, at: 0 };
        match frame.op {
            wire::OP_HELLO if payload.len() >= 24 => {
                let version = read.u32();
                let _report = read.u32();
                sender.hello = Some(Hello {
                    version,
                    buffer_bytes: read.u64(),
                    buffers: read.u64(),
                });
            }
            wire::OP_CALIB if payload.len() >= 24 => sender.calibration.push(Bracket {
                before: read.u64(),
                other: read.u64(),
                after: read.u64(),
            }),
            wire::OP_NAME if payload.len() >= 4 => {
                let id = read.u32();
                let raw = String::from_utf8_lossy(&payload[4..])
                    .trim_end_matches('\0')
                    .to_owned();
                let name = crate::stack::demangle(&raw).unwrap_or(raw);
                sender.names.insert(id, name);
            }
            wire::OP_KERNEL => match wire::Kernel::decode(payload) {
                Some(kernel) => {
                    if let Some(event) = self.kernel(pid, &kernel, seq) {
                        events.push(event);
                    }
                }
                None => self.losses.undecoded += 1,
            },
            wire::OP_MEMCPY | wire::OP_MEMSET => match wire::Copy::decode(payload) {
                Some(copy) => {
                    if let Some(event) = self.copy(pid, frame.op, &copy, seq) {
                        events.push(event);
                    }
                }
                None => self.losses.undecoded += 1,
            },
            wire::OP_DROPPED if payload.len() >= 8 => {
                let dropped = read.u64();
                self.losses.dropped += dropped;
                self.losses.note(
                    "CUPTI had no buffer to put these records in and dropped them: the program \
                     launched faster than its buffers were handed back",
                );
            }
            wire::OP_LOST if payload.len() >= 8 => {
                // Cumulative, per process: only what is new is a new loss.
                let lost = read.u64();
                let new = lost.saturating_sub(sender.lost);
                sender.lost = sender.lost.max(lost);
                self.losses.dropped += new;
                if new > 0 {
                    self.losses.note(
                        "the injection library could not send these records to omn within its \
                         send timeout -- or, in the batches it could not send, the counts of \
                         records CUPTI had dropped or handed back unfinished",
                    );
                }
            }
            wire::OP_INCOMPLETE if payload.len() >= 8 => {
                let incomplete = read.u64();
                self.losses.dropped += incomplete;
                self.losses.note(
                    "CUPTI handed these records back before their activity had finished, \
                     without the times to report: the program exited with work still queued \
                     on the device",
                );
            }
            wire::OP_ERROR => {
                let text = String::from_utf8_lossy(payload)
                    .trim_end_matches('\0')
                    .to_owned();
                self.errors.push(format!("pid {pid}: {text}"));
            }
            wire::OP_END if payload.len() >= 24 => {
                sender.ended = true;
                let _seen = read.u64();
                let lost = read.u64();
                let unknown = read.u64();
                let new = lost.saturating_sub(sender.lost);
                sender.lost = sender.lost.max(lost);
                if new > 0 {
                    self.losses.dropped += new;
                    self.losses.note(
                        "the injection library could not send these records to omn within its \
                         send timeout -- or, in the batches it could not send, the counts of \
                         records CUPTI had dropped or handed back unfinished",
                    );
                }
                if unknown > 0 {
                    self.losses.undecoded += unknown;
                    self.losses.note(
                        "CUPTI returned activity records of a kind the injection library does \
                         not decode",
                    );
                }
            }
            wire::OP_UNKNOWN => {}
            // `gpu.api`'s, through the callback API, on a socket it shares.
            wire::OP_API | wire::OP_API_UNPAIRED => {}
            wire::OP_EXITING => sender.exiting = true,
            // What the library learns inside kernels, for the readers of
            // `gpu.counters` and `gpu.profile` on the same socket: theirs.
            wire::OP_INSIDE_FIRST..=wire::OP_INSIDE_LAST => {}
            _ => {
                self.losses.undecoded += 1;
                self.losses.note(
                    "a frame from the injection library was not one this build reads: the \
                     library and `omn` are from different builds",
                );
            }
        }
    }

    /// Whether one more event may be kept, counting it as dropped if not.
    fn admit(&mut self) -> bool {
        if self.held >= HELD_EVENTS {
            self.losses.dropped += 1;
            self.losses.note(format!(
                "the program ran more than {HELD_EVENTS} device activities: past them, each one \
                 was read, dropped and counted rather than kept, so what omn holds does not \
                 grow with the program it watches"
            ));
            return false;
        }
        self.held += 1;
        true
    }

    fn stamp(&mut self, pid: u32, start: u64, end: u64) -> Option<(u64, u64, u64)> {
        let sender = self.senders.get(&pid)?;
        let map = sender.calibration.map()?;
        // The map as it stands when the record arrives: a record past the
        // last bracket so far is extrapolated, and carries what that adds.
        let err_ns = map.err_at(start).max(map.err_at(end));
        self.widest_err_ns = self.widest_err_ns.max(err_ns);
        Some((map.to_monotonic(start), map.span(start, end), err_ns))
    }

    /// A record before any calibration bracket has no monotonic time; the
    /// library sends one before enabling anything, so this is a library from
    /// another build or a message lost before it.
    fn uncalibrated(&mut self) {
        self.losses.undecoded += 1;
        self.losses.note(
            "a device activity arrived before any clock calibration from its process, so it has \
             no time in CLOCK_MONOTONIC",
        );
    }

    fn kernel(&mut self, pid: u32, kernel: &wire::Kernel, seq: u64) -> Option<Event> {
        let Some((ts_ns, duration_ns, ts_err_ns)) = self.stamp(pid, kernel.start, kernel.end)
        else {
            self.uncalibrated();
            return None;
        };
        if !self.admit() {
            return None;
        }
        *self.by_op.entry("kernel").or_default() += 1;
        let name = self
            .senders
            .get(&pid)
            .and_then(|sender| sender.names.get(&kernel.name))
            .cloned()
            .unwrap_or_else(|| "?".to_owned());
        let geometry = |three: [i32; 3]| format!("{}x{}x{}", three[0], three[1], three[2]);
        let attrs = vec![
            Attr::uint("duration_ns", duration_ns),
            Attr::text("op", "kernel"),
            Attr::text("kernel", name),
            Attr::uint("device", u64::from(kernel.device)),
            Attr::uint("stream", u64::from(kernel.stream)),
            Attr::uint("context", u64::from(kernel.context)),
            Attr::uint("correlation", u64::from(kernel.correlation)),
            Attr::text("grid", geometry(kernel.grid)),
            Attr::text("block", geometry(kernel.block)),
            Attr::uint("registers", u64::from(kernel.registers)),
            Attr::uint("shared_bytes", u64::from(kernel.shared)),
            Attr::uint("pid", u64::from(pid)),
        ];
        Some(self.event(pid, ts_ns, ts_err_ns, attrs, seq))
    }

    fn copy(&mut self, pid: u32, op: u8, copy: &wire::Copy, seq: u64) -> Option<Event> {
        let Some((ts_ns, duration_ns, ts_err_ns)) = self.stamp(pid, copy.start, copy.end) else {
            self.uncalibrated();
            return None;
        };
        if !self.admit() {
            return None;
        }
        let mut attrs = vec![Attr::uint("duration_ns", duration_ns)];
        if op == wire::OP_MEMCPY {
            *self.by_op.entry("memcpy").or_default() += 1;
            let direction = copy_kind(copy.detail);
            attrs.push(Attr::text("op", "memcpy"));
            attrs.push(Attr::text("kernel", format!("[memcpy {direction}]")));
            attrs.push(Attr::text("copy", direction));
        } else {
            *self.by_op.entry("memset").or_default() += 1;
            attrs.push(Attr::text("op", "memset"));
            attrs.push(Attr::text("kernel", "[memset]"));
            attrs.push(Attr::uint("value", u64::from(copy.detail)));
        }
        attrs.extend([
            Attr::uint("bytes", copy.bytes),
            Attr::uint("device", u64::from(copy.device)),
            Attr::uint("stream", u64::from(copy.stream)),
            Attr::uint("context", u64::from(copy.context)),
            Attr::uint("correlation", u64::from(copy.correlation)),
            Attr::uint("pid", u64::from(pid)),
        ]);
        Some(self.event(pid, ts_ns, ts_err_ns, attrs, seq))
    }

    fn event(&mut self, pid: u32, ts_ns: u64, ts_err_ns: u64, attrs: Vec<Attr>, seq: u64) -> Event {
        let mut keys = vec![KeyValue::task(pid as i32)];
        let sender = self.senders.entry(pid).or_default();
        keys.extend(
            sender
                .proc_key
                .get_or_insert_with(|| KeyValue::proc_of(pid as i32))
                .clone(),
        );
        Event {
            ts_ns,
            // An error past four seconds is not a timestamp any more; it is
            // saturated rather than wrapped, and the facts line says what
            // it really was.
            ts_err_ns: u32::try_from(ts_err_ns).unwrap_or(u32::MAX),
            kind: "gpu.kernel".to_owned(),
            keys,
            attrs,
            collector_id: self.id.clone(),
            seq,
        }
    }
}

/// `CUpti_ActivityMemcpyKind`, as a direction a person reads.
fn copy_kind(kind: u32) -> &'static str {
    match kind {
        1 => "HtoD",
        2 => "DtoH",
        3 => "HtoA",
        4 => "AtoH",
        5 => "AtoA",
        6 => "AtoD",
        7 => "DtoA",
        8 => "DtoD",
        9 => "HtoH",
        10 => "PtoP",
        _ => "unknown",
    }
}

impl Collector for KernelCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "duration_ns"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        Ok(self.drain(seq))
    }

    fn stop(&mut self) -> Vec<Event> {
        // The program has exited, and its closing flush is on the socket; a
        // child that inherited the descriptor may still hold it, so this
        // reads what is there rather than waiting for end-of-file.
        let events = self.drain(u64::MAX / 2);
        for (pid, sender) in &self.senders {
            if sender.hello.is_some() && !sender.ended {
                self.losses.failed_reads += 1;
                if sender.exiting {
                    self.errors
                        .push(super::counters::died_in_exit(*pid, "gpu.kernel"));
                    self.losses.note(format!(
                        "pid {pid} died inside omn's injection library as it exited, so the \
                         activity of its last buffer is missing and was not counted"
                    ));
                } else {
                    self.losses.note(format!(
                        "pid {pid} never said it had flushed CUPTI: it was killed, or exited \
                         without running its exit handlers, so the activity of its last buffer \
                         is missing and was not counted"
                    ));
                }
            }
        }
        events
    }

    fn tick(&self) -> Option<std::time::Duration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn failure(&self) -> Option<String> {
        self.senders
            .iter()
            .find(|(_, sender)| sender.hello.is_some() && !sender.ended && sender.exiting)
            .map(|(pid, _)| super::counters::died_in_exit(*pid, "gpu.kernel"))
    }

    fn facts(&self) -> Vec<String> {
        let mut facts = Vec::new();
        for (pid, sender) in &self.senders {
            let Some(hello) = sender.hello else {
                continue;
            };
            let calibration = match sender.calibration.map() {
                Some(map) => format!(
                    "{} brackets map CUPTI's clock onto CLOCK_MONOTONIC to within {} ns, at a \
                     rate of {:.9}",
                    sender.calibration.len(),
                    map.err_ns,
                    map.slope
                ),
                None => "no bracket mapped CUPTI's clock".to_owned(),
            };
            facts.push(format!(
                "pid {pid}: CUPTI API {}, {} buffers of {} KiB at most; {calibration}",
                hello.version,
                hello.buffers,
                hello.buffer_bytes / 1024
            ));
        }
        if !self.by_op.is_empty() {
            let counts: Vec<String> = self
                .by_op
                .iter()
                .map(|(op, count)| format!("{count} {op}"))
                .collect();
            facts.push(format!(
                "device activities reported: {}; the widest timestamp error was {} ns",
                counts.join(", "),
                self.widest_err_ns
            ));
        }
        // A timing on unlocked clocks depends on what ran before it: say
        // which clocks these were taken on.
        facts.extend(super::timing_clock_facts());
        facts
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = self.errors.clone();
        if self.senders.values().all(|sender| sender.hello.is_none()) {
            notes.push(format!(
                "pid {} never loaded the CUDA injection library: a program that never calls \
                 `cuInit` -- it uses no GPU, or reaches it through something other than the \
                 CUDA driver -- reports no device activity, and none is not zero",
                self.launched
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connected pair, the test holding the library's end.
    fn collector() -> (RawFd, KernelCollector) {
        let prepared = KernelPrepared::new(PathBuf::from("/nonexistent"), 1).unwrap();
        let write = std::mem::replace(&mut prepared.socket.borrow_mut().write_fd, -1);
        let collector = KernelCollector::new(
            prepared.socket(),
            prepared.reader,
            std::process::id() as i32,
        );
        (write, collector)
    }

    fn send(fd: RawFd, pid: u32, frames: &[(u8, Vec<u8>)]) {
        let mut buffer = vec![0u8; wire::MESSAGE_MAX];
        let mut writer = wire::Writer::new(&mut buffer, pid);
        for (op, payload) in frames {
            writer.frame(*op, payload);
        }
        let message = writer.finish().to_vec();
        // SAFETY: sending bytes we own on a socket we own.
        let sent = unsafe { libc::send(fd, message.as_ptr().cast(), message.len(), 0) };
        assert_eq!(sent as usize, message.len());
    }

    fn calib(before: u64, other: u64, after: u64) -> (u8, Vec<u8>) {
        let mut fields = wire::Fields::<24>::default();
        fields.u64(before).u64(other).u64(after);
        (wire::OP_CALIB, fields.as_slice().to_vec())
    }

    fn hello() -> (u8, Vec<u8>) {
        let mut fields = wire::Fields::<24>::default();
        fields.u32(130401).u32(1).u64(262144).u64(256);
        (wire::OP_HELLO, fields.as_slice().to_vec())
    }

    fn name(id: u32, text: &str) -> (u8, Vec<u8>) {
        let mut payload = id.to_ne_bytes().to_vec();
        payload.extend_from_slice(text.as_bytes());
        (wire::OP_NAME, payload)
    }

    fn kernel(start: u64, end: u64, name: u32) -> (u8, Vec<u8>) {
        let record = wire::Kernel {
            start,
            end,
            device: 0,
            context: 1,
            stream: 7,
            correlation: 42,
            grid: [68, 1, 1],
            block: [256, 1, 1],
            name,
            registers: 16,
            shared: 0,
        };
        (wire::OP_KERNEL, record.encode().as_slice().to_vec())
    }

    fn count(op: u8, value: u64) -> (u8, Vec<u8>) {
        (op, value.to_ne_bytes().to_vec())
    }

    fn end(seen: u64, lost: u64) -> (u8, Vec<u8>) {
        let mut fields = wire::Fields::<24>::default();
        fields.u64(seen).u64(lost).u64(0);
        (wire::OP_END, fields.as_slice().to_vec())
    }

    #[test]
    fn a_kernel_is_converted_to_the_monotonic_clock_and_carries_the_error() {
        let (writer, mut collector) = collector();
        send(
            writer,
            77,
            &[
                hello(),
                calib(1_000, 9_000_000, 1_100),
                name(1, "_Z4spinPmm"),
                kernel(9_000_500, 9_002_500, 1),
                end(1, 0),
            ],
        );
        let events = collector.drain(0);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.ts_ns, 1_550);
        // Half the bracket, and the kernel ends 2.5 us past the only
        // bracket: 500 ppm of that, rounded up, for the extrapolation.
        assert_eq!(event.ts_err_ns, 50 + 2);
        assert_eq!(event.attr("duration_ns").unwrap().as_f64(), Some(2_000.0));
        assert_eq!(
            event.attr("kernel").unwrap().to_string(),
            "spin(unsigned long*, unsigned long)"
        );
        assert_eq!(event.attr("op").unwrap().to_string(), "kernel");
        assert_eq!(event.attr("grid").unwrap().to_string(), "68x1x1");
        assert_eq!(event.keys[0], KeyValue::task(77));
        assert!(collector.losses().is_clean());
    }

    #[test]
    fn every_timestamp_carries_a_non_zero_error() {
        let (writer, mut collector) = collector();
        send(
            writer,
            5,
            &[
                hello(),
                calib(100, 100, 100),
                name(1, "tiny"),
                kernel(200, 300, 1),
            ],
        );
        let events = collector.drain(0);
        assert!(events.iter().all(|event| event.ts_err_ns > 0));
    }

    /// Every way a record goes missing is a count in the run's losses, with
    /// its reason: CUPTI's drops, the library's unsent batches, records with
    /// no time, and a program that never said it had flushed.
    #[test]
    fn every_loss_is_counted_with_its_reason() {
        let (writer, mut collector) = collector();
        send(
            writer,
            9,
            &[
                hello(),
                calib(0, 0, 10),
                count(wire::OP_DROPPED, 1_258),
                count(wire::OP_LOST, 3),
                count(wire::OP_LOST, 5),
                count(wire::OP_INCOMPLETE, 2),
            ],
        );
        collector.drain(0);
        collector.stop();
        let losses = collector.losses();
        assert_eq!(losses.dropped, 1_258 + 5 + 2);
        assert_eq!(losses.failed_reads, 1, "no closing frame");
        let reasons = format!("{:?}", losses.reasons);
        assert!(reasons.contains("CUPTI had no buffer"), "{reasons}");
        assert!(reasons.contains("never said it had flushed"), "{reasons}");
    }

    #[test]
    fn a_record_before_any_calibration_is_counted_rather_than_placed() {
        let (writer, mut collector) = collector();
        send(writer, 3, &[hello(), name(1, "k"), kernel(10, 20, 1)]);
        assert!(collector.drain(0).is_empty());
        assert_eq!(collector.losses().undecoded, 1);
    }

    #[test]
    fn a_frame_this_build_does_not_know_is_counted() {
        let (writer, mut collector) = collector();
        send(writer, 3, &[(200, vec![0; 8])]);
        collector.drain(0);
        assert_eq!(collector.losses().undecoded, 1);
    }

    #[test]
    fn a_program_that_never_loaded_the_library_says_so() {
        let (_writer, mut collector) = collector();
        collector.stop();
        let notes = collector.notes().join("\n");
        assert!(
            notes.contains("never loaded the CUDA injection library"),
            "{notes}"
        );
        assert!(collector.losses().is_clean());
    }

    #[test]
    fn a_copy_is_named_by_its_direction_and_carries_its_bytes() {
        let (writer, mut collector) = collector();
        let copy = wire::Copy {
            start: 1_000,
            end: 3_000,
            bytes: 1 << 20,
            device: 0,
            context: 1,
            stream: 7,
            correlation: 3,
            detail: 1,
        };
        send(
            writer,
            3,
            &[
                hello(),
                calib(0, 0, 2),
                (wire::OP_MEMCPY, copy.encode().as_slice().to_vec()),
            ],
        );
        let events = collector.drain(0);
        assert_eq!(
            events[0].attr("kernel").unwrap().to_string(),
            "[memcpy HtoD]"
        );
        assert_eq!(events[0].attr("bytes").unwrap().as_f64(), Some(1048576.0));
    }

    /// The wire is one file compiled on both sides; what is pinned here is
    /// CUPTI's own layout, measured with `offsetof` against its headers.
    #[test]
    fn the_cupti_offsets_are_the_ones_its_headers_give() {
        use wire::cupti::{copy, kernel};
        assert_eq!(
            (kernel::START, kernel::END, kernel::DEVICE, kernel::STREAM),
            (16, 24, 40, 48)
        );
        assert_eq!(
            (
                kernel::GRID,
                kernel::BLOCK,
                kernel::CORRELATION,
                kernel::NAME
            ),
            (52, 64, 92, 104)
        );
        assert_eq!(
            (copy::BYTES, copy::START, copy::DEVICE, copy::STREAM),
            (8, 16, 32, 40)
        );
    }

    /// Two CUPTI collectors of one launch give the program one socket and
    /// one set of variables, asking for the union of what they report, and
    /// each reads every message.
    #[test]
    fn two_collectors_of_one_launch_share_one_socket_and_each_read_it_all() {
        let mut first = KernelPrepared::new(PathBuf::from("/lib/x.so"), 1).unwrap();
        let mut second = KernelPrepared::new(PathBuf::from("/lib/x.so"), 2).unwrap();
        assert!(second.share_launch(&mut first));
        assert_eq!(first.launch_environment(), second.launch_environment());
        assert!(first
            .launch_environment()
            .contains(&(wire::REPORT_VARIABLE.to_owned(), "3".to_owned())));
        let write = std::mem::replace(&mut first.socket.borrow_mut().write_fd, -1);
        let mut one = KernelCollector::new(first.socket(), first.reader, 1);
        let mut two = KernelCollector::new(second.socket(), second.reader, 1);
        send(
            write,
            4,
            &[hello(), calib(0, 0, 2), name(1, "k"), kernel(5, 9, 1)],
        );
        assert_eq!(one.drain(0).len(), 1);
        assert_eq!(two.drain(0).len(), 1);
        close(&mut { write });

        let mut other = KernelPrepared::new(PathBuf::from("/lib/y.so"), 1).unwrap();
        assert!(
            !other.share_launch(&mut first),
            "another library is another launch"
        );
    }

    #[test]
    fn the_library_is_looked_for_beside_the_binary_and_named_when_absent() {
        let empty = std::env::temp_dir().join(format!("omn-no-cupti-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        let error = library_in(&empty).unwrap_err();
        assert!(error.contains("libomn_cupti.so"), "{error}");
        assert!(error.contains("cargo build -p omn-cupti"), "{error}");
        std::fs::remove_dir_all(&empty).ok();
    }
}
