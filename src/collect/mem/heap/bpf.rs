//! `mem.heap.alloc` through uprobes on the C library's allocator
//! (`src/bpf/mem_heap.rs`): the same events the preload backend makes, from
//! the same facts, and each allocation's user stack, which the shim cannot
//! take.
//!
//! A `realloc` is the two things it does: a free of the old block, when it
//! had one, and an allocation of the new one. A `calloc`'s bytes are its
//! count times its size.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration as StdDuration;

use super::alloc::event_of;
use super::state::LiveSet;
use super::{Record, OP_ALLOC, OP_FREE};
use crate::bpf::mem_heap::{HeapCall, HeapProgram, OP_FREE as CALL_FREE, OP_REALLOC};
use crate::collect::task::Comms;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::emit::STACK_ID_ATTR;
use crate::event::{Attr, CollectorId, Event};
use crate::request::{Emit, Mode, Target};
use crate::stack::{Frame, RawStack, Symbolizer};

/// 4 MiB of 48-byte records, drained every ten milliseconds.
const RING_BYTES: usize = 4 * 1024 * 1024;

pub struct Prepare {
    pub mode: Mode,
    pub emits: Vec<Emit>,
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
        Ok(Box::new(HeapUprobes {
            id: CollectorId::new("mem.heap.alloc/bpf:uprobe"),
            pid,
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
    id: CollectorId,
    pid: i32,
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
            self.losses.note(
                "the pending map was full: an allocation's size could not be kept to its return",
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
                    &self.id,
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
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "bytes"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(HeapProgram::load(self.pid, RING_BYTES)?);
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

    /// The events are the preload backend's: an allocation and the free that
    /// ends it, the free carrying the size it gave back.
    #[test]
    fn the_events_are_the_preload_backend_s() {
        let mut live = LiveSet::default();
        let mut comms = Comms::new();
        let id = CollectorId::new("t");
        let alloc = records_of(&call(OP_MALLOC, 0x4000, 4096, 0));
        let free = records_of(&call(CALL_FREE, 0x4000, 0, 0));
        let first = event_of(&alloc[0], &mut live, &mut comms, &id, 0);
        let second = event_of(&free[0], &mut live, &mut comms, &id, 1);
        assert_eq!(first.attr("bytes").unwrap().as_f64(), Some(4096.0));
        assert_eq!(second.attr("freed_bytes").unwrap().as_f64(), Some(4096.0));
    }
}
