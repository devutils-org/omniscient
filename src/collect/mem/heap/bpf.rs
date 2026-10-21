//! `mem.heap.alloc` through uprobes on the C library's allocator
//! (`src/bpf/mem_heap.rs`): the same events the preload backend makes, from
//! the same facts, and each allocation's user stack, which the shim cannot
//! take.
//!
//! A `realloc` is the two things it does: a free of the old block, when it
//! had one, and an allocation of the new one. A `calloc`'s bytes are its
//! count times its size.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration as StdDuration;

use super::alloc::event_of;
use super::state::LiveSet;
use super::{Record, OP_ALLOC, OP_FREE};
use crate::bpf::mem_heap::{
    allocator_among, allocator_of, HeapCall, HeapProgram, OP_FREE as CALL_FREE, OP_REALLOC,
};
use crate::collect::runtime::objects;
use crate::collect::task::Comms;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::emit::STACK_ID_ATTR;
use crate::event::{Attr, Event};
use crate::request::{Emit, Mode, Target};
use crate::stack::{Frame, RawStack, Symbolizer};

/// 4 MiB of 48-byte records, drained every ten milliseconds.
const RING_BYTES: usize = 4 * 1024 * 1024;

pub struct Prepare {
    pub mode: Mode,
    pub emits: Vec<Emit>,
    /// The target as asked, before a launch is bound to its gate's pid: what
    /// a launched program will map is read from it.
    pub target: Target,
}

/// The file whose allocator the uprobes are placed on: for a launched
/// program, the C library it will map, or its own executable when it maps
/// none (a static binary), since at the gate its maps are still this tool's;
/// for a running process, the one it has mapped.
fn allocator(asked: &Target, pid: i32) -> Result<PathBuf, String> {
    match asked {
        Target::Launch(argv) => {
            let objects = objects::of_launch(argv)?;
            let paths: Vec<PathBuf> = objects
                .scanned
                .iter()
                .map(|scanned| scanned.path.clone())
                .collect();
            allocator_among(&paths)
                .ok_or_else(|| format!("`{}` names no file to place the uprobes in", argv[0]))
        }
        _ => allocator_of(pid),
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot see an allocation: each is a call, traced",
                self.mode
            ));
        }
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "the allocator's uprobes are placed in one process, and a {} target names none",
                target.kind()
            )
        })?;
        let library = allocator(&self.target, pid)?;
        Ok(Box::new(HeapUprobes {
            pid,
            library,
            wants_stacks: self.emits.contains(&Emit::Stack),
            program: None,
            live: LiveSet::default(),
            comms: Comms::new(),
            symbolizer: None,
            stacks: BTreeMap::new(),
            resolved: HashMap::new(),
            losses: Losses::default(),
            dropped: (0, 0),
        }))
    }
}

pub struct HeapUprobes {
    pid: i32,
    /// The file the uprobes are placed in.
    library: PathBuf,
    wants_stacks: bool,
    program: Option<HeapProgram>,
    live: LiveSet,
    comms: Comms,
    symbolizer: Option<Symbolizer>,
    /// The stacks named so far, root first, by the kernel's id.
    stacks: BTreeMap<u64, Vec<String>>,
    /// Kernel stack ids already named, and whether they could be.
    resolved: HashMap<u32, bool>,
    losses: Losses,
    dropped: (u64, u64),
}

/// The records a call is: a realloc's free of its old block, then its
/// allocation; every other call one record.
fn records_of(call: &HeapCall) -> Vec<Record> {
    let record = |op: u8, ptr: u64, size: u64| Record {
        ts_ns: call.ts_ns,
        ptr,
        size,
        extra: 0,
        tid: call.tid,
        op,
        flags: 0,
    };
    match call.op {
        CALL_FREE => vec![record(OP_FREE, call.ptr, 0)],
        OP_REALLOC if call.extra != 0 => vec![
            record(OP_FREE, call.extra, 0),
            record(OP_ALLOC, call.ptr, call.bytes()),
        ],
        _ => vec![record(OP_ALLOC, call.ptr, call.bytes())],
    }
}

impl HeapUprobes {
    /// Names a stack once, while the process's maps are still there.
    fn resolve(&mut self, id: u32) -> bool {
        if let Some(known) = self.resolved.get(&id) {
            return *known;
        }
        let Some(addresses) = self.program.as_ref().and_then(|program| program.stack(id)) else {
            self.resolved.insert(id, false);
            return false;
        };
        let symbolizer = self
            .symbolizer
            .get_or_insert_with(|| Symbolizer::for_kernel_process(self.pid));
        let stack = symbolizer.symbolize(&RawStack {
            kernel: Vec::new(),
            user: addresses,
            elsewhere: Vec::new(),
        });
        self.stacks.insert(
            u64::from(id),
            stack.frames.iter().rev().map(Frame::label).collect(),
        );
        self.resolved.insert(id, true);
        true
    }

    fn drain(&mut self, seq: u64) -> Vec<Event> {
        let Some(program) = self.program.as_mut() else {
            return Vec::new();
        };
        let mut calls = Vec::new();
        let mut undecoded = 0;
        program.drain(|call| calls.push(call), &mut undecoded);
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the heap ring was shorter than one call");
        }
        let (ring, pending) = program.dropped();
        if ring > self.dropped.0 {
            self.losses.dropped += ring - self.dropped.0;
            self.losses
                .note("the heap ring was full: allocations came faster than they were drained");
        }
        if pending > self.dropped.1 {
            self.losses.dropped += pending - self.dropped.1;
            self.losses.note_times(
                "the pending map was full: an allocation's size could not be kept to its return",
                pending - self.dropped.1,
            );
        }
        self.dropped = (ring, pending);
        let mut events = Vec::new();
        for call in calls {
            let stack = (self.wants_stacks && call.stack >= 0 && call.op != CALL_FREE)
                .then_some(call.stack as u32)
                .filter(|id| self.resolve(*id));
            for record in records_of(&call) {
                let mut event = event_of(
                    &record,
                    &mut self.live,
                    &mut self.comms,
                    seq + events.len() as u64,
                );
                if let (Some(id), true) = (stack, record.op == OP_ALLOC) {
                    event.attrs.push(Attr::uint(STACK_ID_ATTR, u64::from(id)));
                }
                events.push(event);
            }
        }
        events
    }
}

impl Collector for HeapUprobes {
    fn value_attr(&self) -> &'static str {
        "bytes"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(HeapProgram::load(
            self.pid,
            self.library.clone(),
            RING_BYTES,
        )?);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        Ok(self.drain(seq))
    }

    fn stop(&mut self) -> Vec<Event> {
        let events = self.drain(u64::MAX / 2);
        self.program = None;
        events
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(super::DRAIN_INTERVAL)
    }

    fn stacks(&self) -> BTreeMap<u64, Vec<String>> {
        self.stacks.clone()
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        self.program
            .as_ref()
            .map(|program| {
                vec![format!(
                    "uprobes on malloc, calloc, realloc and free in {}: a trap per call, so an \
                     allocation-heavy program runs slower under them",
                    program.library.display()
                )]
            })
            .unwrap_or_default()
    }
}

/// The allocator's uprobes as a [`super::Feed`]: what `mem.heap.live` and
/// `mem.heap.retained` read on `bpf:uprobe`, the same records as the shim's
/// and no stacks (a total has no use for one).
pub struct UprobeFeed {
    pid: i32,
    library: PathBuf,
    program: Option<HeapProgram>,
    losses: Losses,
    dropped: (u64, u64),
}

impl UprobeFeed {
    pub fn new(pid: i32, library: PathBuf) -> Self {
        Self {
            pid,
            library,
            program: None,
            losses: Losses::default(),
            dropped: (0, 0),
        }
    }
}

impl super::Feed for UprobeFeed {
    fn pid(&self) -> i32 {
        self.pid
    }

    fn backend(&self) -> &'static str {
        "bpf:uprobe"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(HeapProgram::load(
            self.pid,
            self.library.clone(),
            RING_BYTES,
        )?);
        Ok(())
    }

    fn drain(&mut self) -> Vec<Record> {
        let Some(program) = self.program.as_mut() else {
            return Vec::new();
        };
        let mut calls = Vec::new();
        let mut undecoded = 0;
        program.drain(|call| calls.push(call), &mut undecoded);
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the heap ring was shorter than one call");
        }
        let (ring, pending) = program.dropped();
        if ring + pending > self.dropped.0 + self.dropped.1 {
            self.losses.dropped += ring + pending - self.dropped.0 - self.dropped.1;
            self.losses.note_times(
                "the heap ring or its pending map was full: an allocation or a free is in no \
                 total",
                ring + pending - self.dropped.0 - self.dropped.1,
            );
        }
        self.dropped = (ring, pending);
        calls.iter().flat_map(records_of).collect()
    }

    fn close(&mut self) {
        self.program = None;
    }

    fn notes(&self) -> Vec<String> {
        Vec::new()
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

/// `mem.heap.live` and `mem.heap.retained` on `bpf:uprobe`.
pub struct LivePrepare {
    pub which: super::Which,
    pub mode: Mode,
    /// The target as asked, as [`Prepare::target`].
    pub target: Target,
}

impl Prepared for LivePrepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let interval = match self.mode {
            Mode::Poll { interval } => StdDuration::from_nanos(interval.as_nanos()),
            other => {
                return Err(format!(
                    "`{}` through the allocator's uprobes is a total kept from the attach on, \
                     read on a poll; `{other}` has no poll",
                    self.which.source_id()
                ))
            }
        };
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "the allocator's uprobes are placed in one process, and a {} target names none",
                target.kind()
            )
        })?;
        let feed = UprobeFeed::new(pid, allocator(&self.target, pid)?);
        match self.which {
            super::Which::Retained => super::retained::RetainedCollector::new(feed, interval)
                .map(|collector| collector as Box<dyn Collector>),
            _ => Ok(Box::new(super::live::LiveCollector::new(feed, interval))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bpf::mem_heap::{OP_CALLOC, OP_MALLOC};

    fn call(op: u32, ptr: u64, size: u64, extra: u64) -> HeapCall {
        HeapCall {
            ts_ns: 1,
            ptr,
            size,
            extra,
            tid: 7,
            tgid: 7,
            op,
            stack: -1,
        }
    }

    #[test]
    fn a_realloc_is_a_free_then_an_allocation_and_calloc_multiplies() {
        let moved = records_of(&call(OP_REALLOC, 0x2000, 64, 0x1000));
        assert_eq!(moved.len(), 2);
        assert_eq!((moved[0].op, moved[0].ptr), (OP_FREE, 0x1000));
        assert_eq!(
            (moved[1].op, moved[1].ptr, moved[1].size),
            (OP_ALLOC, 0x2000, 64)
        );
        assert_eq!(records_of(&call(OP_REALLOC, 0x2000, 64, 0)).len(), 1);
        assert_eq!(records_of(&call(OP_CALLOC, 0x3000, 8, 10))[0].size, 80);
        assert_eq!(records_of(&call(OP_MALLOC, 0x4000, 5, 0))[0].op, OP_ALLOC);
        assert_eq!(records_of(&call(CALL_FREE, 0x4000, 0, 0))[0].op, OP_FREE);
    }

    /// The live collector reads the uprobes' records as it reads the shim's:
    /// a realloc moves its block, a calloc counts its product, a free gives
    /// back what it ended.
    #[test]
    fn the_live_total_reads_the_uprobes_records() {
        struct Fake(Vec<Record>);
        impl super::super::Feed for Fake {
            fn pid(&self) -> i32 {
                std::process::id() as i32
            }
            fn backend(&self) -> &'static str {
                "bpf:uprobe"
            }
            fn drain(&mut self) -> Vec<Record> {
                std::mem::take(&mut self.0)
            }
            fn close(&mut self) {}
            fn notes(&self) -> Vec<String> {
                Vec::new()
            }
            fn losses(&self) -> Losses {
                Losses::default()
            }
        }
        let calls = [
            call(OP_MALLOC, 0x1000, 100, 0),
            call(OP_CALLOC, 0x2000, 8, 10),
            call(OP_REALLOC, 0x3000, 300, 0x1000),
            call(CALL_FREE, 0x2000, 0, 0),
        ];
        let records: Vec<Record> = calls.iter().flat_map(records_of).collect();
        let mut live =
            super::super::live::LiveCollector::new(Fake(records), StdDuration::from_secs(1));
        let last = live.stop();
        assert_eq!(last[0].attr("bytes").unwrap().as_f64(), Some(300.0));
        assert_eq!(last[0].attr("allocations").unwrap().as_f64(), Some(1.0));
    }

    /// The events are the preload backend's: an allocation and the free that
    /// ends it, the free carrying the size it gave back.
    #[test]
    fn the_events_are_the_preload_backend_s() {
        let mut live = LiveSet::default();
        let mut comms = Comms::new();
        let alloc = records_of(&call(OP_MALLOC, 0x4000, 4096, 0));
        let free = records_of(&call(CALL_FREE, 0x4000, 0, 0));
        let first = event_of(&alloc[0], &mut live, &mut comms, 0);
        let second = event_of(&free[0], &mut live, &mut comms, 1);
        assert_eq!(first.attr("bytes").unwrap().as_f64(), Some(4096.0));
        assert_eq!(second.attr("freed_bytes").unwrap().as_f64(), Some(4096.0));
    }
}
