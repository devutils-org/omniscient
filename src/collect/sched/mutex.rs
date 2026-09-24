//! `sched.mutex`: pthread mutex waits and holds, through the preload shim.
//!
//! One event per acquisition, stamped when the thread *asked* for the mutex,
//! carrying how long it waited to get it (`wait_ns`, the value) and how long it
//! then held it (`hold_ns`). The shim reports both when the mutex is released,
//! which is the first moment both are known; `shim/src/mutex.rs` is the other
//! half.
//!
//! **What each of the three lock sources sees.** This one sees every
//! acquisition of a pthread mutex, contended or not, because it is in the call:
//! an uncontended `pthread_mutex_lock` is an atomic exchange that never reaches
//! the kernel, and it is here with a wait of a few tens of nanoseconds.
//! `sched.futex` sees the syscall a *contended* mutex ends in -- and every
//! other futex wait, a condition variable, a semaphore, a Rust or Go lock that
//! is not a pthread mutex at all -- but not the uncontended majority, and not
//! which mutex object it was when a lock keeps its futex word elsewhere.
//! `sched.lock` is the kernel's own locks and nothing in user space.
//!
//! The shim costs about 70 ns a record since it writes into a shared ring
//! (finding 47; 132 ns in finding 6's isolated measurement, 560 through the
//! pipe) against a uprobe's 740 ns for the same one, and
//! `pthread_mutex_lock` fires far more often than an allocation: that is why
//! this backend is first and the uprobe is the fallback.

use crate::collect::mem::heap::{Record, ShimPrepared, Stream, Which, OP_MUTEX};
use crate::collect::task::Comms;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::Emit;

/// The flags the shim sets, from `shim/src/mutex.rs`.
const FLAG_TRY: u8 = 1;
const FLAG_COND: u8 = 2;
const FLAG_HOLD_UNSEEN: u8 = 4;

/// Builds the preload collector, refusing what it cannot serve by name.
pub fn prepare(emits: &[Emit]) -> Result<Box<dyn Prepared>, String> {
    // The catalog declares `stack` for this source because the uprobe backend
    // can take one in the kernel. The shim cannot: walking a stack inside a
    // lock hook costs more than the lock, and needs frame pointers the program
    // was probably not built with. Refused rather than rendered empty.
    if emits.contains(&Emit::Stack) {
        return Err(
            "`sched.mutex` through the preload shim carries no stacks: walking one inside \
             every lock would cost more than the lock. Drop `stack`, or ask `sched.futex`, \
             whose stacks show where a contended mutex blocked"
                .to_owned(),
        );
    }
    Ok(Box::new(ShimPrepared::reporting(
        Which::Mutex,
        std::time::Duration::from_nanos(crate::request::Mode::DEFAULT_POLL_INTERVAL.as_nanos()),
        |stream, _| Ok(Box::new(MutexCollector::new(stream))),
    )?))
}

pub struct MutexCollector {
    id: CollectorId,
    stream: Stream,
    comms: Comms,
}

impl MutexCollector {
    fn new(stream: Stream) -> Self {
        Self {
            id: CollectorId::new("sched.mutex/preload"),
            stream,
            comms: Comms::new(),
        }
    }

    fn events(&mut self, seq: u64) -> Vec<Event> {
        let pid = self.stream.pid();
        let records = self.stream.drain();
        let mut events = Vec::with_capacity(records.len());
        for (offset, record) in records.into_iter().enumerate() {
            if record.op != OP_MUTEX {
                continue;
            }
            events.push(self.event(record, pid, seq + offset as u64));
        }
        events
    }

    fn event(&mut self, record: Record, pid: i32, seq: u64) -> Event {
        let mut attrs = vec![
            Attr::uint("wait_ns", record.size),
            Attr::uint("addr", record.ptr),
            Attr::uint("tid", u64::from(record.tid)),
            Attr::uint("pid", pid as u64),
            Attr::text(
                "how",
                if record.flags & FLAG_TRY != 0 {
                    "trylock"
                } else {
                    "lock"
                },
            ),
            // Where the hold ended. A hold that ended in a condition wait is
            // the time before the wait, and the reacquisition inside it is not
            // an event: its wait for the mutex and its wait for the condition
            // cannot be told apart.
            Attr::text(
                "released_by",
                if record.flags & FLAG_HOLD_UNSEEN != 0 {
                    "unseen"
                } else if record.flags & FLAG_COND != 0 {
                    "cond_wait"
                } else {
                    "unlock"
                },
            ),
        ];
        // An absent hold and a zero one say different things: the first is a
        // thread that held more mutexes at once than the shim keeps track of.
        if record.flags & FLAG_HOLD_UNSEEN == 0 {
            attrs.push(Attr::uint("hold_ns", record.extra));
        }
        if let Some(comm) = self.comms.get(record.tid as i32) {
            attrs.push(Attr::text("comm", comm));
        }
        Event {
            ts_ns: record.ts_ns,
            ts_err_ns: 0,
            kind: "sched.mutex".to_owned(),
            keys: vec![KeyValue::task(record.tid as i32)],
            attrs,
            collector_id: self.id.clone(),
            seq,
        }
    }
}

impl Collector for MutexCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "wait_ns"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        Ok(self.events(seq))
    }

    fn stop(&mut self) -> Vec<Event> {
        let trailing = self.events(u64::MAX / 2);
        self.stream.close();
        trailing
    }

    fn tick(&self) -> Option<std::time::Duration> {
        Some(crate::collect::mem::heap::DRAIN_INTERVAL)
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
    use super::*;
    use crate::collect::mem::heap::tests::{full_record, pipe_pair, write};
    use crate::collect::mem::heap::OP_GPU_CALL;

    fn collector() -> (std::os::fd::RawFd, MutexCollector) {
        let (read, write) = pipe_pair();
        let stream = Stream::only_reader(read, std::process::id() as i32, Which::Mutex);
        (write, MutexCollector::new(stream))
    }

    #[test]
    fn an_acquisition_carries_its_wait_as_the_value_and_its_hold_beside_it() {
        let (writer, mut collector) = collector();
        write(writer, &full_record(OP_MUTEX, 0xbeef, 1_500, 20_000, 7, 0));
        let events = collector.events(0);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.kind, "sched.mutex");
        assert_eq!(event.ts_ns, 1_000, "stamped when the thread asked");
        assert_eq!(event.attr("wait_ns").unwrap().as_f64(), Some(1_500.0));
        assert_eq!(event.attr("hold_ns").unwrap().as_f64(), Some(20_000.0));
        assert_eq!(
            event.attr("addr").unwrap().as_f64(),
            Some(f64::from(0xbeef))
        );
        assert_eq!(event.attr("how").unwrap().to_string(), "lock");
        assert_eq!(event.attr("released_by").unwrap().to_string(), "unlock");
        assert_eq!(event.keys, vec![KeyValue::task(7)]);
    }

    #[test]
    fn the_flags_say_how_the_mutex_was_taken_and_how_the_hold_ended() {
        let (writer, mut collector) = collector();
        write(writer, &full_record(OP_MUTEX, 1, 0, 5, 7, FLAG_TRY));
        write(writer, &full_record(OP_MUTEX, 1, 9, 5, 7, FLAG_COND));
        write(writer, &full_record(OP_MUTEX, 1, 9, 0, 7, FLAG_HOLD_UNSEEN));
        let events = collector.events(0);
        let how: Vec<String> = events
            .iter()
            .map(|e| e.attr("how").unwrap().to_string())
            .collect();
        let released: Vec<String> = events
            .iter()
            .map(|e| e.attr("released_by").unwrap().to_string())
            .collect();
        assert_eq!(how, ["trylock", "lock", "lock"]);
        assert_eq!(released, ["unlock", "cond_wait", "unseen"]);
        assert!(
            events[2].attr("hold_ns").is_none(),
            "a hold that was never timed is absent, not zero"
        );
    }

    /// The pipe carries every class; a GPU call is not a lock, and is not
    /// counted as something this reader failed to decode either.
    #[test]
    fn another_class_on_the_same_pipe_is_neither_an_event_nor_a_loss() {
        let (writer, mut collector) = collector();
        write(writer, &full_record(OP_GPU_CALL, 0, 10, 0, 7, 0));
        write(writer, &full_record(1, 0x1000, 64, 0, 7, 0)); // an allocation
        write(writer, &full_record(OP_MUTEX, 2, 3, 4, 7, 0));
        assert_eq!(collector.events(0).len(), 1);
        assert!(collector.losses().is_clean(), "{:?}", collector.losses());
    }

    #[test]
    fn a_drop_of_mutex_records_is_this_sources_loss_and_a_heap_drop_is_not() {
        let (writer, mut collector) = collector();
        write(writer, &full_record(3, 1, 11, 0, 7, 0)); // heap class
        write(writer, &full_record(3, 2, 5, 0, 7, 0)); // mutex class
        collector.events(0);
        assert_eq!(collector.losses().dropped, 5);
    }

    #[test]
    fn asking_for_stacks_is_refused_by_name() {
        let error = prepare(&[Emit::Stack]).err().expect("stacks are refused");
        assert!(error.contains("no stacks"), "{error}");
    }
}
