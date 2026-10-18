//! `gpu.counters`: what a kernel did inside, counted by the device, through
//! CUPTI's range profiler in the injection library (`cupti/src/profiler.rs`).
//!
//! Every launch is a range, replayed by CUPTI as many times as the counters
//! need passes, and every counter of every launch comes back as one frame.
//! This collector holds them per kernel and counter, and at the end of the
//! run emits **one event per kernel and counter**: the median over the
//! kernel's launches as `value`, the smallest and largest beside it, the
//! spread of the middle half (interquartile range over median), the
//! launches outside the band (`outliers`), and a verdict.
//!
//! **The verdict is measured, never declared.** A counter is reproducible
//! only when the workload dominates what it counts -- the DRAM bytes of a
//! memory-bound kernel repeat, those of a compute-bound kernel are whatever
//! else touched video memory -- so no list of good counters is kept: each
//! counter of each kernel is judged on its own launches in this run.
//! `reproducible here` when the spread is within [`REPRODUCIBLE_SPREAD`],
//! `not dominated by this workload` when it is not, `measured once` when the
//! kernel ran once and there is no spread to judge.
//!
//! **A launch disturbed by other work is found and left out.** The device
//! counts what its SMs and memory did, and another process's kernels running
//! in the same window are counted with the program's: measured here, a copy
//! kernel's instruction count went from 3.2 million to 286 million on every
//! launch of a run -- by the same amount each time, so the wrong count
//! repeated and a spread alone would have called it reproducible. So the
//! library always collects a witness, the blocks the SMs launched
//! ([`wire::WITNESS_METRIC`]), and compares it with the grid the launch call
//! asked for: a launch whose window held more blocks than its grid is
//! `disturbed`, counted and left out of every counter. Work that launched no
//! block in the window -- a long kernel started before it -- escapes the
//! witness, and shows as outliers or a spread; the compute processes NVML
//! lists on the device during the run, once a second, are in the facts.
//!
//! **Times are not taken.** Kernel replay runs each launch several times,
//! serialised, with CUPTI's save and restore between the runs: a run that
//! asks for counters and for `gpu.kernel` is refused, because the kernel
//! times would be those of replayed kernels.
//!
//! The shared part -- the socket of the launch, joined with `gpu.kernel`'s
//! and `gpu.profile`'s, and the frames every reader of it handles -- is
//! [`InsidePrepared`] and [`Common`], also used by `profile.rs`.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use super::kernel::{find_library, wire, KernelPrepared, Socket};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Request, Target};

/// The widest spread, interquartile range over median across a kernel's
/// launches, at which a counter is called reproducible here; and the band
/// around the median past which a launch is an outlier. Measured on this
/// machine: the DRAM bytes of a 64 MiB copy spread 0.5 to 5 % end to end,
/// those of a compute-bound kernel 180 to 340 %; instruction counts 0 % on
/// every launch nothing else disturbed.
pub const REPRODUCIBLE_SPREAD: f64 = 0.10;

/// How often the socket is drained, and NVML asked what else holds the
/// device.
const DRAIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// How often NVML is asked what else holds the device.
const LOOK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// What a reader inside kernels is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Inside {
    Counters,
    Profile,
}

impl Inside {
    fn report(self) -> u32 {
        match self {
            Inside::Counters => wire::REPORT_COUNTERS,
            Inside::Profile => wire::REPORT_PC_SAMPLING,
        }
    }

    pub(super) fn source(self) -> &'static str {
        match self {
            Inside::Counters => "gpu.counters",
            Inside::Profile => "gpu.profile",
        }
    }

    fn refuses_pid(self) -> &'static str {
        match self {
            Inside::Counters => {
                "CUPTI's range profiler works inside the process that owns the CUDA context, \
                 and is loaded there by the CUDA driver at `cuInit` from \
                 `CUDA_INJECTION64_PATH`, so this source needs `-- <program>`: a running \
                 process has initialised CUDA already and cannot be given a library at exec"
            }
            Inside::Profile => {
                "CUPTI's PC sampling works inside the process that owns the CUDA context, and \
                 is loaded there by the CUDA driver at `cuInit` from `CUDA_INJECTION64_PATH`, \
                 so this source needs `-- <program>`: a running process has initialised CUDA \
                 already and cannot be given a library at exec"
            }
        }
    }
}

pub fn prepare_counters() -> Result<Box<dyn Prepared>, String> {
    Ok(Box::new(InsidePrepared::new(Inside::Counters)?))
}

pub fn prepare_profile() -> Result<Box<dyn Prepared>, String> {
    Ok(Box::new(InsidePrepared::new(Inside::Profile)?))
}

/// The socket and the library, before the program exists: `gpu.kernel`'s,
/// with this reader's bit, so that every CUPTI source of a launch reads one
/// socket and the program is given one `CUDA_INJECTION64_PATH`.
pub(super) struct InsidePrepared {
    inner: KernelPrepared,
    kind: Inside,
}

impl InsidePrepared {
    /// For the unit tests: an inside reader of a library nobody loads.
    #[cfg(test)]
    fn new_at(kind: Inside) -> Self {
        Self {
            inner: KernelPrepared::new(std::path::PathBuf::from("/nonexistent"), kind.report())
                .unwrap(),
            kind,
        }
    }

    fn new(kind: Inside) -> Result<Self, String> {
        Ok(Self {
            inner: KernelPrepared::new(find_library()?, kind.report())?,
            kind,
        })
    }

    /// Why this reader cannot be attached beside what else the launch asks
    /// of the library, if it cannot.
    fn refusal(&self, report: u32) -> Option<String> {
        refusal(self.kind, report)
    }
}

/// Why `kind` cannot read a launch whose library is asked for `report`, if
/// it cannot: one rule, applied before the launch to a session's requests
/// ([`launch_refusals`]) and again at attach, to whatever reached it.
fn refusal(kind: Inside, report: u32) -> Option<String> {
    let source = kind.source();
    match kind {
        Inside::Counters if report & wire::REPORT_ACTIVITY != 0 => Some(format!(
            "{source} cannot be asked with gpu.kernel in one run: its kernel replay runs \
             every launch several times, serialised, with CUPTI's save and restore between \
             the runs, so the times gpu.kernel took would be those of replayed kernels \
             rather than of the program's. Ask for each in a run of its own"
        )),
        _ if report & wire::REPORT_COUNTERS != 0 && report & wire::REPORT_PC_SAMPLING != 0 => {
            Some(format!(
                "{source} cannot be asked with {} in one run: CUPTI's range profiler and \
                 its PC sampling cannot both run in one process. Ask for each in a run of \
                 its own",
                match kind {
                    Inside::Counters => "gpu.profile",
                    Inside::Profile => "gpu.counters",
                }
            ))
        }
        _ => None,
    }
}

/// What a request asks of the injection library, when it resolved onto it.
/// The backend is the one the resolver chose, not the one the request named:
/// an unpinned request served by another vendor's library asks nothing of
/// CUPTI, and is no reason to refuse its neighbours.
fn library_bit(request: &Request, backend: &str) -> Option<u32> {
    if backend != "vendor:cupti" {
        return None;
    }
    match request.source.as_str() {
        "gpu.kernel" => Some(wire::REPORT_ACTIVITY),
        "gpu.counters" => Some(wire::REPORT_COUNTERS),
        "gpu.profile" => Some(wire::REPORT_PC_SAMPLING),
        _ => None,
    }
}

/// The session's requests that cannot share their launch with the others,
/// by index, each with the reason: refused once resolved and before anything
/// is launched, as the request's error, rather than at attach as the
/// machine's. Each request comes with the backend it resolved onto, and only
/// those served by CUPTI count. Requests share a launch when they launch the
/// same command line.
pub fn launch_refusals(resolved: &[(&Request, &str)]) -> Vec<(usize, String)> {
    let mut refused = Vec::new();
    for (index, &(request, backend)) in resolved.iter().enumerate() {
        let kind = match request.source.as_str() {
            "gpu.counters" => Inside::Counters,
            "gpu.profile" => Inside::Profile,
            _ => continue,
        };
        if library_bit(request, backend).is_none() {
            continue;
        }
        let Target::Launch(argv) = &request.target else {
            continue;
        };
        let report = resolved
            .iter()
            .filter(|(other, _)| matches!(&other.target, Target::Launch(theirs) if theirs == argv))
            .filter_map(|&(other, backend)| library_bit(other, backend))
            .fold(0, |report, bit| report | bit);
        if let Some(reason) = refusal(kind, report) {
            refused.push((index, reason));
        }
    }
    refused
}

impl Prepared for InsidePrepared {
    fn launch_environment(&self) -> Vec<(String, String)> {
        self.inner.launch_environment()
    }

    /// Joins an earlier CUPTI source's socket -- `gpu.kernel`'s or another
    /// inside reader's, which [`InsidePrepared::shareable`] hands over as
    /// the `KernelPrepared` it is.
    fn share_launch(&mut self, earlier: &mut dyn Prepared) -> bool {
        self.inner.share_launch(earlier)
    }

    fn shareable(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(&mut self.inner)
    }

    fn requires_launch(&self) -> Option<&'static str> {
        Some(self.kind.refuses_pid())
    }

    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "{}; a {} target names none",
                self.kind.refuses_pid(),
                target.kind()
            )
        })?;
        let socket = self.inner.socket();
        // Every reader of the launch has added its bit by now: the program
        // has not been released, and refusing here kills it at the gate.
        if let Some(reason) = self.refusal(socket.borrow().report()) {
            return Err(reason);
        }
        socket.borrow_mut().close_write();
        let reader = self.inner.reader();
        Ok(match self.kind {
            Inside::Counters => Box::new(CountersCollector::new(socket, reader, pid)),
            Inside::Profile => Box::new(super::profile::ProfileCollector::new(socket, reader, pid)),
        })
    }
}

/// What a run says of a process that died inside the injection library's
/// exit handler: the tool ended the program, and the program's exit status
/// is the tool's doing, not the program's.
pub(super) fn died_in_exit(pid: u32, source: &str) -> String {
    format!(
        "pid {pid} died inside omn's injection library (libomn_cupti.so), in its exit handler, \
         after the program's own work had finished: {source} killed the program it measured, \
         so the program's exit status is the tool's failure, not the program's"
    )
}

/// What one process said about itself, for an inside reader.
#[derive(Debug, Default)]
pub(super) struct Sender {
    pub names: BTreeMap<u32, String>,
    pub hello: bool,
    pub ended: bool,
    /// The library's exit handler started ([`wire::OP_EXITING`]).
    pub exiting: bool,
    /// Inside records the library could not send, as last reported.
    unsent: u64,
    /// The process's `proc` key, read once, while it runs: at the end of the
    /// run it is gone.
    proc_key: Option<Option<KeyValue>>,
}

impl Sender {
    pub fn name(&self, id: u32) -> String {
        self.names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| "?".to_owned())
    }
}

/// The frames every inside reader handles alike: who said hello, the names,
/// the library's errors, the end, and what could not be sent.
pub(super) struct Common {
    pub kind: Inside,
    socket: Rc<RefCell<Socket>>,
    reader: usize,
    pub launched: i32,
    pub senders: BTreeMap<u32, Sender>,
    pub losses: Losses,
    pub errors: Vec<String>,
    /// Compute processes NVML listed on the device during the run that are
    /// not the program's, by pid, with their name.
    others: BTreeMap<u32, String>,
    nvml: Option<Result<super::nvml::Nvml, String>>,
    looked: Option<std::time::Instant>,
}

impl Common {
    pub fn new(kind: Inside, socket: Rc<RefCell<Socket>>, reader: usize, launched: i32) -> Self {
        Self {
            kind,
            socket,
            reader,
            launched,
            senders: BTreeMap::new(),
            losses: Losses::default(),
            errors: Vec::new(),
            others: BTreeMap::new(),
            nvml: None,
            looked: None,
        }
    }

    /// Every message waiting, as (sender pid, op, payload) for the frames
    /// the caller handles; the rest are handled here.
    pub fn take(&mut self) -> Vec<(u32, u8, Vec<u8>)> {
        let messages = self.socket.borrow_mut().take(self.reader);
        let mut mine = Vec::new();
        for message in messages {
            let Some((pid, frames)) = wire::parse(&message) else {
                self.losses.undecoded += 1;
                self.losses.note_times(
                    "a message from the injection library was not one this build reads: the \
                     library and `omn` are from different builds",
                    1,
                );
                continue;
            };
            for frame in frames {
                if !self.common(pid, frame.op, frame.payload) {
                    mine.push((pid, frame.op, frame.payload.to_vec()));
                }
            }
        }
        mine
    }

    /// Handles a frame every reader handles alike; answers whether it did.
    fn common(&mut self, pid: u32, op: u8, payload: &[u8]) -> bool {
        let source = self.kind.source();
        let sender = self.senders.entry(pid).or_default();
        let mut read = wire::Reader { payload, at: 0 };
        match op {
            wire::OP_HELLO => {
                sender.hello = true;
                sender
                    .proc_key
                    .get_or_insert_with(|| KeyValue::proc_of(pid as i32));
            }
            wire::OP_END => sender.ended = true,
            wire::OP_EXITING => sender.exiting = true,
            wire::OP_NAME if payload.len() >= 4 => {
                let id = read.u32();
                let raw = String::from_utf8_lossy(&payload[4..])
                    .trim_end_matches('\0')
                    .to_owned();
                let name = crate::stack::demangle(&raw).unwrap_or(raw);
                sender.names.insert(id, name);
            }
            wire::OP_ERROR => {
                let text = String::from_utf8_lossy(payload)
                    .trim_end_matches('\0')
                    .to_owned();
                // Another inside reader's errors are its own; the library's
                // own -- CUPTI not loadable -- are everybody's.
                let other = ["gpu.counters:", "gpu.profile:"]
                    .iter()
                    .any(|tag| text.starts_with(tag) && !tag.starts_with(source));
                if !other {
                    self.errors.push(format!("pid {pid}: {text}"));
                }
            }
            wire::OP_INSIDE_UNSENT if payload.len() >= 8 => {
                let unsent = read.u64();
                let new = unsent.saturating_sub(sender.unsent);
                sender.unsent = sender.unsent.max(unsent);
                if new > 0 {
                    self.losses.dropped += new;
                    self.losses.note_times(
                        "the injection library could not send these records to omn within its \
                         send timeout",
                        new,
                    );
                }
            }
            // `gpu.kernel`'s frames, on a socket it shares.
            wire::OP_CALIB
            | wire::OP_KERNEL
            | wire::OP_MEMCPY
            | wire::OP_MEMSET
            | wire::OP_DROPPED
            | wire::OP_LOST
            | wire::OP_UNKNOWN
            | wire::OP_INCOMPLETE
            | wire::OP_API
            | wire::OP_API_UNPAIRED => {}
            wire::OP_INSIDE_FIRST..=wire::OP_INSIDE_LAST => return false,
            _ => {
                self.losses.undecoded += 1;
                self.losses.note_times(
                    "a frame from the injection library was not one this build reads: the \
                     library and `omn` are from different builds",
                    1,
                );
            }
        }
        true
    }

    /// Counts what an [`wire::OP_INSIDE_LOST`] says was lost, with the
    /// reason for its kind.
    pub fn inside_lost(&mut self, payload: &[u8]) {
        let Some(mut read) = wire::Reader::checked(payload, 12) else {
            self.losses.undecoded += 1;
            return;
        };
        let count = read.u64();
        let kind = read.u32();
        let reason = match kind {
            wire::LOST_RANGES => {
                "CUPTI's decode dropped these ranges: their launches' counters are missing"
            }
            wire::LOST_UNPAIRED => {
                "the ranges of a batch of launches did not pair with its launches -- their \
                 counts differed (a launch through an entry point the library does not follow, \
                 such as a CUDA graph's), or a range launched fewer blocks than its launch's \
                 grid -- so none of the batch's counters was reported rather than any credited \
                 to the wrong kernel; counted here are its launches or ranges, whichever were \
                 more"
            }
            wire::LOST_UNDECODED => {
                "these launches' counters were never decoded: the range profiler was not set up \
                 on their context, or a decode failed (the notes say which)"
            }
            wire::LOST_SAMPLES => "CUPTI's PC sampling dropped these samples, by its own count",
            wire::LOST_HW_BUFFER_FULL => {
                "the PC sampling hardware buffer filled, this many times: the samples taken \
                 while it was full are lost and CUPTI does not count them"
            }
            wire::LOST_PCS => {
                "CUPTI still held these program counters when PC sampling was torn down, and \
                 they did not fit in the buffer it was handed"
            }
            _ => "the injection library lost these records for a reason this build does not know",
        };
        if kind == wire::LOST_HW_BUFFER_FULL {
            // A count of times, not of samples: how many samples each time
            // cost is not knowable, so it is not added to a record count.
            self.losses.throttled += count;
        } else {
            self.losses.dropped += count;
        }
        self.losses.note_times(reason, count);
    }

    /// Opens NVML before the program runs: its first load takes a large
    /// part of a tick, and a tick that outruns itself is a late tick.
    pub fn start(&mut self) {
        self.nvml.get_or_insert_with(super::nvml::Nvml::open);
    }

    /// Asks NVML which compute processes hold the devices, and keeps those
    /// that are not the program's: at most every [`LOOK_INTERVAL`], because
    /// one listing took up to 95 ms on a device shared by several programs,
    /// half a tick.
    pub fn look_for_others(&mut self) {
        let now = std::time::Instant::now();
        if self
            .looked
            .is_some_and(|looked| now.duration_since(looked) < LOOK_INTERVAL)
        {
            return;
        }
        self.looked = Some(now);
        self.look_now();
    }

    /// The same, now, whenever it last looked: at the end of the run.
    pub fn look_now(&mut self) {
        let nvml = self.nvml.get_or_insert_with(super::nvml::Nvml::open);
        let Ok(nvml) = nvml.as_ref() else {
            return;
        };
        let Ok(count) = nvml.device_count() else {
            return;
        };
        let mine: BTreeSet<u32> = self
            .senders
            .keys()
            .copied()
            .chain(std::iter::once(self.launched as u32))
            .collect();
        for index in 0..count {
            let Ok(device) = nvml.device(index) else {
                continue;
            };
            let Ok(processes) = nvml.processes(device) else {
                continue;
            };
            for (kind, process) in processes {
                if kind != super::nvml::ProcessKind::Compute || mine.contains(&process.pid) {
                    continue;
                }
                self.others.entry(process.pid).or_insert_with(|| {
                    std::fs::read_to_string(format!("/proc/{}/comm", process.pid))
                        .map(|comm| comm.trim().to_owned())
                        .unwrap_or_else(|_| "?".to_owned())
                });
            }
        }
    }

    /// The facts every inside reader carries: what else held the device.
    pub fn facts(&self) -> Vec<String> {
        let mut facts = Vec::new();
        match &self.nvml {
            Some(Ok(_)) if self.others.is_empty() => facts
                .push("no other compute process was seen on the device during the run".to_owned()),
            Some(Ok(_)) => {
                let others: Vec<String> = self
                    .others
                    .iter()
                    .map(|(pid, name)| format!("pid {pid} ({name})"))
                    .collect();
                facts.push(format!(
                    "other compute processes held the device during the run: {}; the device \
                     counts what its SMs and memory do, so their work in the same window is \
                     counted with the program's",
                    others.join(", ")
                ));
            }
            Some(Err(reason)) => facts.push(format!(
                "whether another process used the device during the run is not known: {reason}"
            )),
            None => {}
        }
        facts
    }

    /// The notes every inside reader carries: the library's errors, and a
    /// program that never loaded it.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = self.errors.clone();
        if self.senders.values().all(|sender| !sender.hello) {
            notes.push(format!(
                "pid {} never loaded the CUDA injection library: a program that never calls \
                 `cuInit` -- it uses no GPU, or reaches it through something other than the \
                 CUDA driver -- reports nothing from inside its kernels, and nothing is not zero",
                self.launched
            ));
        }
        notes
    }

    /// The first process the injection library killed in its exit handler,
    /// as [`Collector::failure`] says it.
    pub fn failure(&self) -> Option<String> {
        let source = self.kind.source();
        self.senders
            .iter()
            .find(|(_, sender)| sender.hello && !sender.ended && sender.exiting)
            .map(|(pid, _)| died_in_exit(*pid, source))
    }

    /// At the end: a process that said hello and never said it had finished
    /// lost what it had not sent; one that died inside the library's exit
    /// handler was ended by the tool, which is a failure, not a lost read.
    pub fn unfinished(&mut self) {
        let source = self.kind.source();
        for (pid, sender) in &self.senders {
            if sender.hello && !sender.ended {
                self.losses.failed_reads += 1;
                if sender.exiting {
                    self.errors.push(died_in_exit(*pid, source));
                    self.losses.note(format!(
                        "pid {pid} died inside omn's injection library as it exited, so what \
                         {source} had not yet read back from CUPTI is missing and was not counted"
                    ));
                } else {
                    self.losses.note(format!(
                        "pid {pid} never said it had finished: it was killed, or exited without \
                         running its exit handlers, so what {source} had not yet read back from \
                         CUPTI is missing and was not counted"
                    ));
                }
            }
        }
    }

    pub fn event(
        &mut self,
        pid: u32,
        ts_ns: u64,
        ts_err_ns: u64,
        attrs: Vec<Attr>,
        id: &CollectorId,
        seq: u64,
    ) -> Event {
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
            ts_err_ns: u32::try_from(ts_err_ns.max(1)).unwrap_or(u32::MAX),
            kind: self.kind.source().to_owned(),
            keys,
            attrs,
            collector_id: id.clone(),
            seq,
        }
    }
}

/// What the range profiler was set up with on one device of one process.
#[derive(Debug, Clone, Copy)]
struct Setup {
    passes: u32,
    chip: u32,
    metrics: u32,
}

/// One counter's values over one kernel's launches.
#[derive(Debug, Default)]
struct Series {
    /// The values of the launches the witness did not find disturbed.
    values: Vec<f64>,
    /// Launches whose window held other work, left out of `values`; and
    /// launches the witness could not judge, kept in it.
    disturbed: usize,
    unwitnessed: usize,
    device: u32,
    /// The first launch call's entry, and the last counters' arrival.
    first_ns: u64,
    last_ns: u64,
}

pub struct CountersCollector {
    id: CollectorId,
    common: Common,
    setups: BTreeMap<(u32, u32), Setup>,
    /// By (pid, kernel, counter).
    series: BTreeMap<(u32, String, String), Series>,
    launches: u64,
    disturbed: u64,
    unwitnessed: u64,
}

/// What a counter's launches say about it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Judgement {
    pub median: f64,
    pub min: f64,
    pub max: f64,
    /// The interquartile range over the median: how far apart the middle
    /// half of the launches are. None for a single launch, or for a median
    /// of zero with a middle half that is not all zero.
    pub spread: Option<f64>,
    /// Launches further than [`REPRODUCIBLE_SPREAD`] of the median from it:
    /// disturbed ones, when the rest agree.
    pub outliers: usize,
    pub verdict: &'static str,
}

/// The `q` quantile of sorted values, interpolated between the two nearest.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    let at = q * (sorted.len() - 1) as f64;
    let (low, high) = (at.floor() as usize, at.ceil() as usize);
    sorted[low] + (sorted[high] - sorted[low]) * (at - low as f64)
}

/// The verdict on a counter's launches.
///
/// The spread is that of the middle half, not of the extremes, because
/// what disturbs a counter on a shared device only ever adds to it, and
/// does so to a few launches: another process's kernel in the window of one
/// launch would otherwise turn an instruction count that repeats bit for bit
/// into a counter that does not repeat. The launches it disturbed are not
/// hidden: they are `outliers`, counted beside the verdict.
pub fn judge(values: &[f64]) -> Judgement {
    let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let (Some(&min), Some(&max)) = (sorted.first(), sorted.last()) else {
        return Judgement {
            median: 0.0,
            min: 0.0,
            max: 0.0,
            spread: None,
            outliers: 0,
            verdict: "measured once",
        };
    };
    let median = quantile(&sorted, 0.5);
    if sorted.len() < 2 {
        return Judgement {
            median,
            min,
            max,
            spread: None,
            outliers: 0,
            verdict: "measured once",
        };
    }
    let range = quantile(&sorted, 0.75) - quantile(&sorted, 0.25);
    let spread = if range == 0.0 {
        Some(0.0)
    } else if median > 0.0 {
        Some(range / median)
    } else {
        None
    };
    let outliers = sorted
        .iter()
        .filter(|value| (**value - median).abs() > REPRODUCIBLE_SPREAD * median.abs())
        .count();
    let verdict = match spread {
        Some(spread) if spread <= REPRODUCIBLE_SPREAD => "reproducible here",
        _ => "not dominated by this workload",
    };
    Judgement {
        median,
        min,
        max,
        spread,
        outliers,
        verdict,
    }
}

impl CountersCollector {
    fn new(socket: Rc<RefCell<Socket>>, reader: usize, launched: i32) -> Self {
        Self {
            id: CollectorId::new("gpu.counters/vendor:cupti"),
            common: Common::new(Inside::Counters, socket, reader, launched),
            setups: BTreeMap::new(),
            series: BTreeMap::new(),
            launches: 0,
            disturbed: 0,
            unwitnessed: 0,
        }
    }

    fn drain(&mut self) {
        let now = crate::clock::monotonic_ns();
        for (pid, op, payload) in self.common.take() {
            match op {
                wire::OP_COUNTER_SETUP => match wire::CounterSetup::decode(&payload) {
                    Some(setup) => {
                        self.setups.insert(
                            (pid, setup.device),
                            Setup {
                                passes: setup.passes,
                                chip: setup.chip,
                                metrics: setup.metrics,
                            },
                        );
                    }
                    None => self.common.losses.undecoded += 1,
                },
                wire::OP_COUNTER => match wire::Counter::decode(&payload) {
                    Some(counter) => {
                        let sender = self.common.senders.entry(pid).or_default();
                        let key = (
                            pid,
                            sender.name(counter.kernel),
                            sender.name(counter.metric),
                        );
                        // The launch's work ran after its call was entered
                        // and before its counters arrived here.
                        let launched = match counter.launched_ns {
                            0 => now,
                            at => at.min(now),
                        };
                        let series = self.series.entry(key).or_insert_with(|| Series {
                            first_ns: launched,
                            device: counter.device,
                            ..Default::default()
                        });
                        series.first_ns = series.first_ns.min(launched);
                        match counter.witness {
                            wire::WITNESS_DISTURBED => series.disturbed += 1,
                            wire::WITNESS_CLEAN => series.values.push(counter.value),
                            _ => {
                                series.unwitnessed += 1;
                                series.values.push(counter.value);
                            }
                        }
                        series.last_ns = now;
                    }
                    None => self.common.losses.undecoded += 1,
                },
                wire::OP_INSIDE_LOST => self.common.inside_lost(&payload),
                // Another inside reader's frames: not this collector's.
                _ => {}
            }
        }
    }

    fn events(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        let series = std::mem::take(&mut self.series);
        // Every counter of a launch carries the same witness, so any one
        // counter of a kernel says how many of its launches were what.
        let mut by_kernel: BTreeMap<(u32, &str), (usize, usize, usize)> = BTreeMap::new();
        for ((pid, kernel, _), values) in &series {
            by_kernel.entry((*pid, kernel)).or_insert((
                values.values.len() + values.disturbed,
                values.disturbed,
                values.unwitnessed,
            ));
        }
        self.launches = by_kernel.values().map(|counts| counts.0 as u64).sum();
        self.disturbed = by_kernel.values().map(|counts| counts.1 as u64).sum();
        self.unwitnessed = by_kernel.values().map(|counts| counts.2 as u64).sum();
        for (seq, ((pid, kernel, counter), values)) in series.iter().enumerate() {
            let Judgement {
                median,
                min,
                max,
                spread,
                outliers,
                verdict,
            } = judge(&values.values);
            let verdict = if values.values.is_empty() && values.disturbed > 0 {
                "disturbed on every launch"
            } else {
                verdict
            };
            let passes = self
                .setups
                .get(&(*pid, values.device))
                .map(|setup| u64::from(setup.passes))
                .unwrap_or(0);
            let mut attrs = vec![
                Attr::float("value", median),
                Attr::text("counter", counter.clone()),
                Attr::text("kernel", kernel.clone()),
                Attr::float("min", min),
                Attr::float("max", max),
                Attr::uint("launches", values.values.len() as u64),
                Attr::uint("disturbed", values.disturbed as u64),
                Attr::uint("outliers", outliers as u64),
                Attr::text("verdict", verdict),
                Attr::uint("passes", passes),
                Attr::uint("device", u64::from(values.device)),
                Attr::uint("pid", u64::from(*pid)),
            ];
            if let Some(spread) = spread {
                attrs.insert(4, Attr::float("spread", spread));
            }
            // The event stands for the kernel's launches: stamped when the
            // first launch call was entered, its error reaching to when the
            // last launch's counters arrived -- a window that holds all the
            // work measured, however late the library decoded it.
            let event = self.common.event(
                *pid,
                values.first_ns,
                values.last_ns.saturating_sub(values.first_ns),
                attrs,
                &self.id,
                seq as u64,
            );
            events.push(event);
        }
        events
    }
}

impl Collector for CountersCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "value"
    }

    fn start(&mut self) -> Result<(), String> {
        self.common.start();
        Ok(())
    }

    fn sample(&mut self, _seq: u64) -> Result<Vec<Event>, SampleError> {
        self.drain();
        self.common.look_for_others();
        Ok(Vec::new())
    }

    fn stop(&mut self) -> Vec<Event> {
        self.drain();
        self.common.look_now();
        self.common.unfinished();
        self.events()
    }

    fn tick(&self) -> Option<std::time::Duration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.common.losses.clone()
    }

    fn failure(&self) -> Option<String> {
        self.common.failure()
    }

    fn facts(&self) -> Vec<String> {
        let mut facts = Vec::new();
        for ((pid, device), setup) in &self.setups {
            let chip = self
                .common
                .senders
                .get(pid)
                .map(|sender| sender.name(setup.chip))
                .unwrap_or_else(|| "?".to_owned());
            facts.push(format!(
                "pid {pid}, device {device} ({chip}): {} counters by CUPTI's range profiler in \
                 kernel replay, every launch run {} time{} to collect them; its times are not \
                 the program's",
                setup.metrics,
                setup.passes,
                if setup.passes == 1 { "" } else { "s" }
            ));
        }
        if self.launches > 0 {
            facts.push(format!(
                "{} launches measured; a counter is `reproducible here` when the middle half \
                 of its kernel's launches is within {:.0} % of their median (interquartile \
                 range over median), `not dominated by this workload` when it is not; a launch \
                 further than that from the median is an outlier, counted beside the verdict",
                self.launches,
                REPRODUCIBLE_SPREAD * 100.0
            ));
        }
        if self.disturbed > 0 {
            facts.push(format!(
                "{} of {} launches were disturbed: the device launched more blocks in their \
                 window than their grids hold ({}), so other work was counted with theirs; they \
                 are left out of every counter, and counted as `disturbed`",
                self.disturbed,
                self.launches,
                wire::WITNESS_METRIC
            ));
        }
        if self.unwitnessed > 0 {
            facts.push(format!(
                "{} of {} launches could not be checked for other work in their window (no \
                 grid read from their launch call, or no {} on this chip): they are kept, \
                 and what else ran beside them is not known",
                self.unwitnessed,
                self.launches,
                wire::WITNESS_METRIC
            ));
        }
        facts.extend(self.common.facts());
        // Counters that follow the clock -- cycles, throughputs -- follow
        // it whether or not it was locked; say which it was.
        facts.extend(super::timing_clock_facts());
        facts
    }

    fn notes(&self) -> Vec<String> {
        self.common.notes()
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn a_counter_that_repeats_is_reproducible_and_one_that_wanders_is_not() {
        let same = judge(&[100.0, 100.0, 100.0]);
        assert_eq!(
            (same.median, same.min, same.max, same.spread),
            (100.0, 100.0, 100.0, Some(0.0))
        );
        assert_eq!(same.verdict, "reproducible here");

        let close = judge(&[1.0e6, 1.04e6, 0.98e6]);
        assert!(close.spread.unwrap() < REPRODUCIBLE_SPREAD);
        assert_eq!(close.verdict, "reproducible here");

        let wandering = judge(&[1_500.0, 3_000_000.0, 160.0, 1_200_000.0]);
        assert!(wandering.spread.unwrap() > 1.0);
        assert_eq!(wandering.verdict, "not dominated by this workload");
    }

    /// Two launches of twelve disturbed by another process's kernels: the
    /// count still repeats, and the two are counted, not hidden.
    #[test]
    fn a_few_disturbed_launches_are_outliers_beside_a_count_that_repeats() {
        let mut values = vec![3_167_488.0; 10];
        values.extend([7_006_142.0, 627_739_000.0]);
        let judged = judge(&values);
        assert_eq!(judged.median, 3_167_488.0);
        assert_eq!(judged.spread, Some(0.0));
        assert_eq!(judged.outliers, 2);
        assert_eq!(judged.verdict, "reproducible here");
        assert_eq!(judged.max, 627_739_000.0);
    }

    #[test]
    fn a_kernel_launched_once_has_no_spread_and_says_so() {
        let once = judge(&[42.0]);
        assert_eq!(once.median, 42.0);
        assert_eq!(once.spread, None);
        assert_eq!(once.verdict, "measured once");
    }

    /// Zero on every launch repeats; zero at the median and something on
    /// another launch has no spread to divide by, and is not reproducible.
    #[test]
    fn a_counter_at_zero_is_judged_without_dividing_by_it() {
        assert_eq!(judge(&[0.0, 0.0]).verdict, "reproducible here");
        let scattered = judge(&[0.0, 0.0, 256.0, 9_728.0]);
        assert_eq!(scattered.median, 128.0);
        let zero_median = judge(&[0.0, 0.0, 0.0, 640.0, 9_728.0, 4_000.0]);
        assert_eq!(zero_median.median, 320.0);
        let around_zero = judge(&[0.0, 0.0, 0.0, 0.0, 0.0, 9_728.0, 4_000.0]);
        assert_eq!(around_zero.median, 0.0);
        assert_eq!(around_zero.spread, None);
        assert_eq!(around_zero.verdict, "not dominated by this workload");
    }

    /// A socket pair as a launch would have it, the test holding the
    /// library's end: (that end, the socket, this reader).
    pub(in crate::collect::gpu) fn pair(kind: Inside) -> (i32, Rc<RefCell<Socket>>, usize) {
        let prepared = InsidePrepared {
            inner: KernelPrepared::new(std::path::PathBuf::from("/nonexistent"), kind.report())
                .unwrap(),
            kind,
        };
        let fd: i32 = prepared
            .launch_environment()
            .iter()
            .find(|(name, _)| name == wire::FD_VARIABLE)
            .and_then(|(_, value)| value.parse().ok())
            .unwrap();
        // SAFETY: duplicating a descriptor this test owns.
        let write = unsafe { libc::dup(fd) };
        let socket = prepared.inner.socket();
        socket.borrow_mut().close_write();
        (write, socket, prepared.inner.reader())
    }

    pub(in crate::collect::gpu) fn send(fd: i32, pid: u32, frames: &[(u8, Vec<u8>)]) {
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

    pub(in crate::collect::gpu) fn name(id: u32, text: &str) -> (u8, Vec<u8>) {
        let mut payload = id.to_ne_bytes().to_vec();
        payload.extend_from_slice(text.as_bytes());
        (wire::OP_NAME, payload)
    }

    pub(in crate::collect::gpu) fn lost(count: u64, kind: u32) -> (u8, Vec<u8>) {
        let mut fields = wire::Fields::<16>::default();
        fields.u64(count).u32(kind).u32(0);
        (wire::OP_INSIDE_LOST, fields.as_slice().to_vec())
    }

    fn counter(kernel: u32, metric: u32, value: f64) -> (u8, Vec<u8>) {
        let counter = wire::Counter {
            kernel,
            metric,
            correlation: 7,
            device: 0,
            value,
            witness: wire::WITNESS_CLEAN,
            pad: 0,
            launched_ns: 1_000,
        };
        (wire::OP_COUNTER, counter.encode().as_slice().to_vec())
    }

    fn disturbed(kernel: u32, metric: u32, value: f64) -> (u8, Vec<u8>) {
        let counter = wire::Counter {
            kernel,
            metric,
            correlation: 8,
            device: 0,
            value,
            witness: wire::WITNESS_DISTURBED,
            pad: 0,
            launched_ns: 1_000,
        };
        (wire::OP_COUNTER, counter.encode().as_slice().to_vec())
    }

    /// A launch whose window held more blocks than its grid is left out of
    /// every counter and counted; a kernel disturbed on every launch has no
    /// value of its own to judge.
    #[test]
    fn a_disturbed_launch_is_left_out_and_counted() {
        let (write, socket, reader) = pair(Inside::Counters);
        let mut collector = CountersCollector::new(socket, reader, 41);
        send(
            write,
            41,
            &[
                (wire::OP_HELLO, vec![0; 24]),
                name(1, "copy"),
                name(2, "smsp__inst_executed.sum"),
                name(3, "spin"),
                counter(1, 2, 3_167_488.0),
                disturbed(1, 2, 286_220_778.0),
                counter(1, 2, 3_167_488.0),
                disturbed(3, 2, 654_110_584.0),
                (wire::OP_END, vec![0; 24]),
            ],
        );
        collector.sample(0).unwrap();
        let events = collector.stop();
        let copy = events
            .iter()
            .find(|event| attr(event, "kernel") == "copy")
            .unwrap();
        assert_eq!(attr(copy, "value"), "3167488");
        assert_eq!(attr(copy, "launches"), "2");
        assert_eq!(attr(copy, "disturbed"), "1");
        assert_eq!(attr(copy, "verdict"), "reproducible here");
        let spin = events
            .iter()
            .find(|event| attr(event, "kernel") == "spin")
            .unwrap();
        assert_eq!(attr(spin, "verdict"), "disturbed on every launch");
        let facts = collector.facts().join("\n");
        assert!(facts.contains("2 of 4 launches were disturbed"), "{facts}");
    }

    fn attr(event: &Event, name: &str) -> String {
        event
            .attr(name)
            .map(|value| value.to_string())
            .unwrap_or_default()
    }

    /// Three launches of one kernel, two counters: one event per counter,
    /// its median, spread and verdict beside it.
    #[test]
    fn each_counter_of_each_kernel_is_one_event_with_its_verdict() {
        let (write, socket, reader) = pair(Inside::Counters);
        let mut collector = CountersCollector::new(socket, reader, 41);
        let setup = wire::CounterSetup {
            device: 0,
            passes: 2,
            chip: 4,
            metrics: 2,
        };
        send(
            write,
            41,
            &[
                (wire::OP_HELLO, vec![0; 24]),
                name(1, "copy"),
                name(2, "smsp__inst_executed.sum"),
                name(3, "dram__bytes_read.sum"),
                name(4, "TU102"),
                (wire::OP_COUNTER_SETUP, setup.encode().as_slice().to_vec()),
                counter(1, 2, 3_167_488.0),
                counter(1, 3, 67_110_592.0),
                counter(1, 2, 3_167_488.0),
                counter(1, 3, 1_500.0),
                counter(1, 2, 3_167_488.0),
                counter(1, 3, 3_000_000.0),
                (wire::OP_END, vec![0; 24]),
            ],
        );
        collector.sample(0).unwrap();
        let events = collector.stop();
        assert_eq!(events.len(), 2);
        let instructions = events
            .iter()
            .find(|event| attr(event, "counter") == "smsp__inst_executed.sum")
            .unwrap();
        assert_eq!(attr(instructions, "verdict"), "reproducible here");
        assert_eq!(attr(instructions, "launches"), "3");
        assert_eq!(attr(instructions, "passes"), "2");
        assert_eq!(attr(instructions, "kernel"), "copy");
        let dram = events
            .iter()
            .find(|event| attr(event, "counter") == "dram__bytes_read.sum")
            .unwrap();
        assert_eq!(attr(dram, "verdict"), "not dominated by this workload");
        // Stamped at the launches, not at the drain: the error reaches from
        // the first launch's call to the arrival of its counters.
        assert!(events.iter().all(|event| event.ts_ns == 1_000));
        assert!(events.iter().all(|event| event.ts_err_ns > 1_000));
        assert!(collector.losses().is_clean());
        assert!(collector.facts().iter().any(|fact| fact.contains("TU102")));
    }

    /// What the library could not report is counted with its reason, and
    /// frames meant for another reader of the socket are neither events nor
    /// losses.
    #[test]
    fn losses_are_counted_and_other_readers_frames_left_alone() {
        let (write, socket, reader) = pair(Inside::Counters);
        let mut collector = CountersCollector::new(socket, reader, 5);
        let kernel = wire::Kernel::default();
        send(
            write,
            5,
            &[
                (wire::OP_HELLO, vec![0; 24]),
                lost(3, wire::LOST_UNPAIRED),
                lost(2, wire::LOST_RANGES),
                (wire::OP_KERNEL, kernel.encode().as_slice().to_vec()),
                (wire::OP_PC, vec![0; wire::PC_SAMPLE_FIELDS]),
                (wire::OP_ERROR, b"gpu.profile: not mine".to_vec()),
                (wire::OP_ERROR, b"gpu.counters: mine".to_vec()),
                (wire::OP_INSIDE_UNSENT, 4u64.to_ne_bytes().to_vec()),
                (wire::OP_INSIDE_UNSENT, 6u64.to_ne_bytes().to_vec()),
            ],
        );
        collector.sample(0).unwrap();
        collector.stop();
        let losses = collector.losses();
        assert_eq!(losses.dropped, 3 + 2 + 6);
        assert_eq!(losses.undecoded, 0);
        assert_eq!(
            losses.failed_reads, 1,
            "no END: it never said it had finished"
        );
        let reasons = format!("{:?}", losses.reasons);
        assert!(reasons.contains("did not pair"), "{reasons}");
        let notes = collector.notes().join("\n");
        assert!(notes.contains("gpu.counters: mine"), "{notes}");
        assert!(!notes.contains("not mine"), "{notes}");
    }

    /// A process that died after the library's exit handler started, before
    /// its end, was killed by the tool: the run says so as a failure, beside
    /// the lost read, and a death without that frame stays a lost read.
    #[test]
    fn a_death_inside_the_library_s_exit_handler_is_the_tool_s_failure() {
        for exiting in [true, false] {
            let (write, socket, reader) = pair(Inside::Profile);
            let mut collector = CountersCollector::new(socket, reader, 5);
            let mut frames = vec![(wire::OP_HELLO, vec![0; 24])];
            if exiting {
                frames.push((wire::OP_EXITING, Vec::new()));
            }
            send(write, 5, &frames);
            collector.sample(0).unwrap();
            collector.stop();
            assert_eq!(collector.losses().failed_reads, 1);
            assert_eq!(collector.losses().undecoded, 0);
            let notes = collector.notes().join("\n");
            assert_eq!(
                notes.contains("killed the program it measured"),
                exiting,
                "{notes}"
            );
        }
    }

    /// The same rule, before the launch: a request that shares its launch
    /// with one it cannot is refused by index, and requests on different
    /// command lines, or resolved onto another backend, are left alone.
    #[test]
    fn a_session_s_requests_are_refused_before_their_shared_launch() {
        let session = |requests: &str| -> crate::request::Session {
            crate::format::InputFormat::Yaml
                .parse(&format!("requests:\n{requests}"))
                .expect("session should parse")
        };
        let request = |source: &str, argv: &str| {
            format!("  - {{source: {source}, target: {{launch: [{argv}]}}, mode: trace, emit: [raw]}}\n")
        };
        let on = |session: &crate::request::Session, backends: &[&'static str]| {
            let pairs: Vec<(&Request, &str)> = session
                .requests
                .iter()
                .zip(backends.iter().copied())
                .collect();
            launch_refusals(&pairs)
        };
        let cupti = "vendor:cupti";
        let both = session(&(request("gpu.counters", "a") + &request("gpu.kernel", "a")));
        let refused = on(&both, &[cupti, cupti]);
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].0, 0);
        assert!(refused[0].1.contains("with gpu.kernel"), "{}", refused[0].1);
        // The same pair served by another library, or with one of the two
        // served by another: nothing of CUPTI's replay is in the launch.
        assert!(on(&both, &["vendor:rocprof", "vendor:rocprof"]).is_empty());
        assert!(on(&both, &[cupti, "vendor:rocprof"]).is_empty());

        let inside = session(&(request("gpu.profile", "a") + &request("gpu.counters", "a")));
        let indexes: Vec<usize> = on(&inside, &[cupti, cupti])
            .iter()
            .map(|(index, _)| *index)
            .collect();
        assert_eq!(indexes, [0, 1]);

        let apart = session(&(request("gpu.counters", "a") + &request("gpu.kernel", "b")));
        assert!(on(&apart, &[cupti, cupti]).is_empty());
        let profile = session(&(request("gpu.profile", "a") + &request("gpu.kernel", "a")));
        assert!(on(&profile, &[cupti, cupti]).is_empty());
    }

    /// Counters replay kernels: refused beside gpu.kernel, and beside PC
    /// sampling, which cannot run in the same process; PC sampling beside
    /// gpu.kernel is a launch both read.
    #[test]
    fn the_combinations_cupti_cannot_serve_are_refused_with_the_reason() {
        let counters = InsidePrepared::new_at(Inside::Counters);
        let profile = InsidePrepared::new_at(Inside::Profile);
        let activity = wire::REPORT_ACTIVITY;
        let both = wire::REPORT_COUNTERS | wire::REPORT_PC_SAMPLING;
        let refused = counters.refusal(activity | wire::REPORT_COUNTERS).unwrap();
        assert!(refused.contains("replayed kernels"), "{refused}");
        assert!(counters.refusal(both).unwrap().contains("gpu.profile"));
        assert!(profile.refusal(both).unwrap().contains("gpu.counters"));
        assert_eq!(profile.refusal(activity | wire::REPORT_PC_SAMPLING), None);
        assert_eq!(counters.refusal(wire::REPORT_COUNTERS), None);
    }

    /// An inside reader joins gpu.kernel's socket whichever was asked first,
    /// and the program is given one set of variables naming both.
    #[test]
    fn an_inside_reader_and_gpu_kernel_share_one_socket_either_way() {
        let library = std::path::PathBuf::from("/lib/x.so");
        let mut kernel = KernelPrepared::new(library.clone(), wire::REPORT_ACTIVITY).unwrap();
        let mut profile = InsidePrepared {
            inner: KernelPrepared::new(library.clone(), wire::REPORT_PC_SAMPLING).unwrap(),
            kind: Inside::Profile,
        };
        assert!(profile.share_launch(&mut kernel));
        assert_eq!(kernel.launch_environment(), profile.launch_environment());
        assert!(kernel.launch_environment().contains(&(
            wire::REPORT_VARIABLE.to_owned(),
            (wire::REPORT_ACTIVITY | wire::REPORT_PC_SAMPLING).to_string()
        )));

        let mut first = InsidePrepared {
            inner: KernelPrepared::new(library.clone(), wire::REPORT_PC_SAMPLING).unwrap(),
            kind: Inside::Profile,
        };
        let mut later = KernelPrepared::new(library, wire::REPORT_ACTIVITY).unwrap();
        assert!(later.share_launch(&mut first));
        assert_eq!(first.launch_environment(), later.launch_environment());
    }
}
