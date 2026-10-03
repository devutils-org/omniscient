//! `net.qdisc` over two raw tracepoints, bucketed in the kernel.
//!
//! The time a packet spent between being handed to a queueing discipline and
//! being given back to the driver. It is the one piece of transmit latency
//! nothing else in this tool can see: by the time a packet is in a qdisc the
//! application has already returned from `send`, and the device counters see it
//! only once it leaves.
//!
//! The programs are in `src/bpf/net_qdisc.rs`. This file is the part that
//! belongs to the family: what an event looks like, what is refused, and what
//! is counted as lost.
//!
//! **A row is keyed by the interface and by the discipline's handle**, and the
//! event carries the interface as its key. A device with a hierarchy of
//! disciplines -- a shaper above a fair queue -- is several rows, which is the
//! difference between "this link is slow" and "this class is slow". The catalog
//! entry declares `sock` as this source's key, which an aggregate over packets
//! cannot carry; the request to correct it is in `docs/open-findings.md`.
//!
//! **Loopback has no queueing discipline.** `lo` is `noqueue`: the transmit
//! path calls the driver directly and neither tracepoint fires. A run on a
//! machine whose only active link is loopback measures nothing, correctly, and
//! says so.

use std::time::Duration as StdDuration;

use super::interfaces;
use crate::bpf::{QdiscProgram, QdiscWait};
use crate::clock::monotonic_ns;
use crate::collect::{not_built, Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the rows are drained.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// Holds the request until the target exists.
pub struct Prepare {
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot measure queueing latency: a packet's wait is the gap between \
                 the enqueue and the dequeue, and a sample that catches one of them without the \
                 other has no duration to report",
                self.mode
            ));
        }
        let iface = match target {
            Target::System => None,
            Target::Iface(name) => Some((interfaces::index_of(name)?, name.clone())),
            other => {
                return Err(not_built(format!(
                    "a {} target cannot be served here: a queueing discipline belongs to a \
                     transmit queue of an interface, not to a process -- use --iface, or \
                     --system for every link",
                    other.kind()
                )))
            }
        };
        Ok(Box::new(QdiscCollector::new(iface, self.filter)))
    }
}

pub struct QdiscCollector {
    id: CollectorId,
    /// The interface a `--iface` target named, compared inside the program.
    iface: Option<(u32, String)>,
    program: Option<QdiscProgram>,
    /// Runs of the programs the kernel skipped, as last read.
    skipped: u64,
    losses: Losses,
    /// The programs' counters are cumulative, so what is new is the difference.
    dropped_queued: u64,
    dropped_rows: u64,
    unpaired: u64,
    still_queued: usize,
    window_opened_ns: u64,
    /// How many packets this run has bucketed, which is not the event count.
    packets: u64,
    rows: u64,
    predicate: Option<FilterExpr>,
    filtered_in: u64,
    filtered_out: u64,
}

impl QdiscCollector {
    fn new(iface: Option<(u32, String)>, predicate: Option<FilterExpr>) -> Self {
        Self {
            id: CollectorId::new("net.qdisc/bpf:raw_tp"),
            iface,
            program: None,
            losses: Losses::default(),
            dropped_queued: 0,
            dropped_rows: 0,
            skipped: 0,
            unpaired: 0,
            still_queued: 0,
            window_opened_ns: 0,
            packets: 0,
            rows: 0,
            predicate,
            filtered_in: 0,
            filtered_out: 0,
        }
    }

    /// When a row happened, as far as an aggregated source can say: the moment
    /// its window's first one began, or the drain that read it when the kernel
    /// recorded no beginning -- which falls back to the drain rather than to
    /// the beginning of time. Finding 44: a join that opens on a bucketed
    /// source would otherwise open at every window's close.
    fn stamp(first_ns: u64, closed_ns: u64) -> u64 {
        if first_ns == 0 {
            closed_ns
        } else {
            first_ns
        }
    }

    fn event(&self, row: &QdiscWait, window_ns: u64, ts_ns: u64, seq: u64) -> Event {
        // The interface is the series. A packet's wait belongs to the link it
        // was waiting to leave by, and a name the kernel no longer knows is
        // still the name the program read at the time -- which matters here,
        // because a queueing discipline may be in another network namespace
        // whose indices mean nothing in this one.
        let name = if row.ifname.is_empty() {
            interfaces::name_of(row.ifindex).unwrap_or_else(|| format!("if{}", row.ifindex))
        } else {
            row.ifname.clone()
        };
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "net.qdisc".to_owned(),
            keys: vec![KeyValue::iface(row.ifindex, name.clone())],
            attrs: attributes(row, &name, window_ns),
            collector_id: self.id.clone(),
            seq,
        }
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let closed_ns = monotonic_ns();
        let window_ns = closed_ns.saturating_sub(self.window_opened_ns);
        let drained = match program.drain() {
            Ok(drained) => drained,
            Err(reason) => {
                self.losses.failed_reads += 1;
                self.losses.note(reason);
                return;
            }
        };
        self.window_opened_ns = closed_ns;

        if drained.vanished > 0 {
            self.losses.undecoded += drained.vanished;
            self.losses
                .note("a qdisc row was gone between being listed and being read");
        }
        if drained.truncated {
            self.losses
                .note("the qdisc histogram held more rows than one drain can walk");
        }

        let mut index = 0;
        for row in drained.rows {
            let candidate = self.event(
                &row,
                window_ns,
                Self::stamp(row.first_ns, closed_ns),
                seq + index,
            );
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &candidate) {
                    self.filtered_out += row.count;
                    continue;
                }
                self.filtered_in += row.count;
            }
            self.packets += row.count;
            self.rows += 1;
            events.push(candidate);
            index += 1;
        }
    }

    fn absorb_counters(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let skipped = program.skipped();
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note(
                "the kernel skipped a run of these programs because one was already running on \
                 its cpu, so a packet went unseen; the count is the machine's, and an upper bound \
                 on this target's",
            );
            self.skipped = skipped;
        }
        let (queued, rows) = program.dropped();
        if queued > self.dropped_queued {
            self.losses.dropped += queued - self.dropped_queued;
            self.losses.note(
                "more packets were inside queueing disciplines at once than the map holds, so \
                 their waits were never timed",
            );
            self.dropped_queued = queued;
        }
        if rows > self.dropped_rows {
            self.losses.dropped += rows - self.dropped_rows;
            self.losses.note(
                "more distinct (interface, discipline, bucket) rows appeared in one interval \
                 than the histogram holds, so those waits were never counted",
            );
            self.dropped_rows = rows;
        }
        self.unpaired = program.unpaired();
    }
}

/// What an event carries beside its value.
///
/// `iface` is here because the catalog entry declares it as a filter field: a
/// predicate over a field no event carries is false everywhere, which empties
/// the series and looks exactly like a link with no queue.
fn attributes(row: &QdiscWait, name: &str, window_ns: u64) -> Vec<Attr> {
    vec![
        // The value: the low bound of the bucket, which is the number the emit
        // layer re-buckets into the bucket it came from.
        Attr::uint("wait_ns", row.bucket_low_ns()),
        Attr::uint("count", row.count),
        Attr::uint(crate::emit::WEIGHT_ATTR, row.count),
        // The exact sum, which a factor-of-two bucket cannot give back.
        Attr::uint("total_ns", row.total_ns),
        Attr::uint("bucket", u64::from(row.bucket)),
        Attr::uint("window_ns", window_ns),
        Attr::text("iface", name.to_owned()),
        Attr::uint("ifindex", u64::from(row.ifindex)),
        // The discipline's handle, as `tc` prints its major number. A device
        // with a shaper above a fair queue is two rows, and this is what tells
        // them apart.
        Attr::uint("handle", u64::from(row.handle)),
        Attr::text("qdisc", format!("{:x}:", row.handle >> 16)),
    ]
}

impl Collector for QdiscCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "wait_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(QdiscProgram::load(self.iface.as_ref().map(|(at, _)| *at))?);
        self.window_opened_ns = monotonic_ns();
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        self.absorb_counters();
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        self.absorb_counters();
        if let Some(program) = self.program.as_ref() {
            self.still_queued = program.still_queued();
        }
        self.program = None;
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.packets == 0 {
            // The commonest reason for an empty run, and one a reader will
            // otherwise spend a while on: a link with no queueing discipline
            // never enters one, so there is nothing to time. Loopback is
            // `noqueue` on every machine.
            notes.push(
                "no packet entered a queueing discipline during this run; a link whose qdisc is \
                 `noqueue` -- which loopback always is -- hands packets straight to the driver \
                 and neither tracepoint fires for it"
                    .to_owned(),
            );
        }
        if let Some((index, name)) = &self.iface {
            notes.push(format!(
                "only packets leaving by `{name}` (index {index}) were timed, compared in the \
                 kernel"
            ));
        }
        if self.unpaired > 0 {
            notes.push(format!(
                "{} packets left a queueing discipline that this run never saw enter one: they \
                 were already queued when it attached, so their waits have no beginning and are \
                 in no bucket",
                self.unpaired,
            ));
        }
        if self.still_queued > 0 {
            notes.push(format!(
                "{} packets were still inside a queueing discipline when the run ended; their \
                 waits have no end yet and are in no bucket",
                self.still_queued,
            ));
        }
        if self.filtered_out > 0 || self.filtered_in > 0 {
            notes.push(format!(
                "the filter kept {} packets and removed {}; it ran here, over the drained rows, \
                 not in the kernel",
                self.filtered_in, self.filtered_out,
            ));
        }
        if self.packets > 0 {
            notes.push(format!(
                "{} packet waits are reported as {} rows of a kernel-side log2 histogram, each \
                 carrying the waits it stands for; \
                 `wait_ns` is its bucket's low bound, a factor of two below the next: the buckets \
                 and `min` are bucketed and `max` is a lower bound, while the sum, mean, rate, \
                 topk and stack totals come from `total_ns`, the exact sum",
                self.packets, self.rows,
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> QdiscWait {
        QdiscWait {
            ifindex: 42,
            handle: 0x8001_0000,
            bucket: 26,
            count: 5,
            total_ns: 250_000_000,
            first_ns: 0,
            ifname: "dummy0".to_owned(),
        }
    }

    #[test]
    fn a_counted_mode_is_refused_with_the_reason() {
        let prepared = Box::new(Prepare {
            mode: Mode::Count,
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::System) else {
            panic!("a counted mode must be refused");
        };
        assert!(error.contains("gap between"), "{error}");
    }

    #[test]
    fn a_process_target_is_refused_and_names_what_to_ask_for() {
        let prepared = Box::new(Prepare {
            mode: Mode::Trace,
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::Pid(vec![1])) else {
            panic!("a queueing discipline belongs to an interface");
        };
        assert!(error.contains("--iface"), "{error}");
    }

    /// Every filter field the catalog offers must be on the events.
    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let attributes = attributes(&row(), "dummy0", 100_000);
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let declared = &catalog
            .get(&"net.qdisc".parse().expect("a source id"))
            .expect("net.qdisc is in the catalog")
            .filter_fields;
        assert!(!declared.is_empty(), "the entry declares filter fields");
        for field in declared {
            assert!(
                attributes.iter().any(|attr| attr.name == *field),
                "the catalog offers a `{field}` filter and no event carries one"
            );
        }
    }

    /// The name on the event is the one the program read in the kernel, not one
    /// this process looked up: a discipline in another network namespace has an
    /// index that means something else here.
    #[test]
    fn the_interface_name_is_the_one_the_program_read() {
        let collector = QdiscCollector::new(None, None);
        let event = collector.event(&row(), 1, 0, 0);
        assert_eq!(
            event.keys,
            vec![KeyValue::iface(42, "dummy0")],
            "the index and the name the kernel gave it travel together"
        );
    }

    /// The value is the bucket's low bound and the exact total is beside it.
    #[test]
    fn the_value_is_the_buckets_low_bound_and_the_total_is_exact() {
        let attributes = attributes(&row(), "dummy0", 1);
        let of = |name: &str| {
            attributes
                .iter()
                .find(|attr| attr.name == name)
                .and_then(|attr| attr.value.as_f64())
                .unwrap_or_else(|| panic!("no `{name}` attribute"))
        };
        assert_eq!(
            of("wait_ns"),
            (1u64 << 25) as f64,
            "bucket 26 starts at 2^25"
        );
        assert_eq!(of("count"), 5.0);
        assert_eq!(of("weight"), 5.0);
        assert_eq!(of("total_ns"), 250_000_000.0);
    }

    /// An empty run on a machine whose only link is loopback is the ordinary
    /// case and is the one a reader will otherwise misread as a broken
    /// collector, so the run says which it is.
    #[test]
    fn a_run_that_saw_no_queue_says_why_that_is_expected() {
        let collector = QdiscCollector::new(None, None);
        let notes = collector.notes().join(" ");
        assert!(notes.contains("noqueue"), "{notes}");
        assert!(notes.contains("loopback"), "{notes}");
    }
}
