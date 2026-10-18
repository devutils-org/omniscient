//! `mem.heap.alloc`: every allocation and free, as events.
//!
//! Each record becomes one event keyed by the range it covers, which is what
//! lets a page fault later be attributed to the allocation that owns its
//! address. The interval join in `examples/allocation-cost.yml` is what this
//! source exists for; on its own it is an allocation log.

use super::{state::LiveSet, Stream, OP_ALLOC};
use crate::collect::task::Comms;
use crate::collect::{Collector, Losses, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};

pub struct AllocCollector {
    id: CollectorId,
    stream: Stream,
    /// By tid: the shim reports the thread that allocated, and a thread pool
    /// that names its workers is exactly the case `comm` is filtered on.
    comms: Comms,
    /// What is live, so that a free can say how big the allocation it ends
    /// was: the shim writes no size on a free, and a join that closes only on
    /// large frees needs one.
    live: LiveSet,
}

impl AllocCollector {
    pub fn new(stream: Stream) -> Self {
        Self {
            id: CollectorId::new("mem.heap.alloc/preload"),
            stream,
            comms: Comms::new(),
            live: LiveSet::default(),
        }
    }

    fn events(&mut self, seq: u64) -> Vec<Event> {
        let records = self.stream.drain();
        let mut index = seq;
        records
            .into_iter()
            .map(|record| {
                let event = event_of(&record, &mut self.live, &mut self.comms, &self.id, index);
                index += 1;
                event
            })
            .collect()
    }
}

/// One allocation or free as an event, the live set kept up to date: shared
/// by the preload backend and the uprobe one (`super::bpf`), whose records
/// are the same facts.
pub(super) fn event_of(
    record: &super::Record,
    live: &mut LiveSet,
    comms: &mut Comms,
    id: &CollectorId,
    seq: u64,
) -> Event {
    let allocated = record.op == OP_ALLOC;
    // `bytes` stays what was allocated, zero on a free, because it is the
    // value every sum and mean of this source is taken over; the size a free
    // gave back rides beside it.
    let freed = if allocated {
        live.allocated_at(record.ptr, record.size, record.ts_ns);
        None
    } else {
        Some(
            live.freed_at(record.ptr, record.ts_ns)
                .map(|((start, end), lifetime)| (end - start, lifetime)),
        )
    };
    Event {
        ts_ns: record.ts_ns,
        ts_err_ns: 0,
        kind: "mem.heap.alloc".to_owned(),
        keys: vec![
            KeyValue::task(record.tid as i32),
            // The range this allocation owns. A free carries a one-byte range
            // at the same base, which is what closes the interval it opened.
            KeyValue::Mem {
                base: record.ptr,
                len: if allocated { record.size.max(1) } else { 1 },
            },
        ],
        attrs: {
            let mut attrs = vec![
                Attr::uint("bytes", if allocated { record.size } else { 0 }),
                Attr::text("op", if allocated { "alloc" } else { "free" }),
                Attr::uint("ptr", record.ptr),
            ];
            match freed {
                Some(Some((size, lifetime))) => {
                    attrs.push(Attr::uint("freed_bytes", size));
                    // How long the block it ends lived: the pair heaptrack's
                    // temporaries and dhat's lifetimes are made of.
                    if let Some(lifetime) = lifetime {
                        attrs.push(Attr::uint("lifetime_ns", lifetime));
                    }
                }
                // A free of something allocated before the run was
                // reporting, or whose record was lost: the size is unknown,
                // and saying so beats a zero.
                Some(None) => attrs.push(Attr::uint("freed_untracked", 1)),
                None => {}
            }
            if let Some(comm) = comms.get(record.tid as i32) {
                attrs.push(Attr::text("comm", comm));
            }
            attrs
        },
        collector_id: id.clone(),
        seq,
    }
}

impl Collector for AllocCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "bytes"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        Ok(self.events(seq))
    }

    fn stop(&mut self) -> Vec<Event> {
        // Whatever the shim wrote between the last drain and the exit still
        // belongs to this run.
        let trailing = self.events(u64::MAX / 2);
        self.stream.close();
        trailing
    }

    fn tick(&self) -> Option<std::time::Duration> {
        Some(super::DRAIN_INTERVAL)
    }

    fn notes(&self) -> Vec<String> {
        self.stream.notes()
    }

    fn losses(&self) -> Losses {
        self.stream.losses()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{piped, record, write};
    use super::*;
    use crate::request::KeyKind;

    #[test]
    fn an_allocation_becomes_an_event_keyed_by_the_range_it_owns() {
        let (writer, stream) = piped();
        write(writer, &record(OP_ALLOC, 0x1000, 4096));
        let mut collector = AllocCollector::new(stream);

        let events = collector.events(0);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].key(KeyKind::Mem),
            Some(&KeyValue::Mem {
                base: 0x1000,
                len: 4096
            })
        );
        assert_eq!(events[0].attr("bytes").unwrap().as_f64(), Some(4096.0));
        assert_eq!(events[0].attr("op").unwrap().to_string(), "alloc");
    }

    /// A free carries a one-byte range at the same base: it is what closes the
    /// interval the allocation opened, not a range of its own.
    #[test]
    fn a_free_closes_the_range_the_allocation_opened() {
        let (writer, stream) = piped();
        write(writer, &record(super::super::OP_FREE, 0x1000, 0));
        let mut collector = AllocCollector::new(stream);

        let events = collector.events(0);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].key(KeyKind::Mem),
            Some(&KeyValue::Mem {
                base: 0x1000,
                len: 1
            })
        );
        assert_eq!(events[0].attr("op").unwrap().to_string(), "free");
        assert_eq!(events[0].attr("bytes").unwrap().as_f64(), Some(0.0));
        assert!(
            events[0].attr("freed_bytes").is_none(),
            "no allocation of 0x1000 was seen, so its size is not known"
        );
        assert_eq!(
            events[0].attr("freed_untracked").unwrap().as_f64(),
            Some(1.0)
        );
    }

    /// A free says how big the allocation it ends was, so a join can close
    /// only on the large ones; `bytes`, the source's value, stays zero.
    #[test]
    fn a_free_carries_the_size_its_allocation_was_made_with() {
        let (writer, stream) = piped();
        write(writer, &record(OP_ALLOC, 0x1000, 8 << 20));
        write(writer, &record(OP_ALLOC, 0x9000_0000, 24));
        write(writer, &record(super::super::OP_FREE, 0x1000, 0));
        let mut collector = AllocCollector::new(stream);

        let events = collector.events(0);
        let free = &events[2];
        assert_eq!(free.attr("op").unwrap().to_string(), "free");
        assert_eq!(
            free.attr("freed_bytes").unwrap().as_f64(),
            Some((8u64 << 20) as f64)
        );
        assert_eq!(free.attr("bytes").unwrap().as_f64(), Some(0.0));
        assert!(
            events[0].attr("freed_bytes").is_none(),
            "an allocation frees nothing"
        );
    }
}
