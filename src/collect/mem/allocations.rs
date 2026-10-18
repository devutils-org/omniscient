//! `mem.page_alloc` and `mem.kmalloc` over raw tracepoints, bucketed in the
//! kernel.
//!
//! These are the two sources that say where a machine's memory actually goes.
//! Every other `mem` source reports a *level* -- resident size, cgroup charge,
//! swap in use -- and a level tells a reader what is held, never who asked for
//! it. `mem.page_alloc` is the page allocator's own ledger: every fault, page
//! table, network buffer and slab refill, with the size and the task that asked.
//! `mem.kmalloc` is the same for the kernel objects too small to be pages, and
//! it is the only source here that can show a kernel memory leak at all, since
//! slab memory appears in no process's footprint.
//!
//! **A row is not an allocation.** `src/bpf/mem_alloc.rs` counts in the kernel,
//! per `(task, cgroup, size bucket, direction)`, and this file drains what it
//! counted. So one event stands for as many allocations as its `count` says,
//! and it carries `emit::WEIGHT_ATTR` so the emit layer weighs it that way --
//! a histogram of these rows is a histogram of the workload's allocations and
//! not of the shape of a BPF map. The run says so in its own output, because
//! every other source in this tool makes one event one measurement.
//!
//! **A page free is an event when the request asks about direction.** Both
//! tracepoints are attached, and every event carries `direction` -- `alloc` or
//! `free` -- which is a filter field of `mem.page_alloc`. A request whose
//! predicate names it gets the frees as events, selected by that predicate; one
//! that does not gets allocations alone, because a histogram of allocations
//! and frees together is a histogram of neither, and the run says how many
//! frees it left out and how to ask for them.
//!
//! `mem.kmalloc` cannot do the same: `kfree` carries the pointer and the call
//! site and nothing that says how large the object was, and an event of a
//! source measured in bytes with no bytes is not one. `direction` is not a
//! filter field there, so the validator refuses a predicate over it, and the
//! run reports the count of frees.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration as StdDuration;

use super::cgroup::Cgroup;
use crate::bpf::{AllocProgram, AllocRow, Allocator};
use crate::clock::monotonic_ns;
use crate::collect::{cannot_mean_anything, not_built, Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{KeyKind, Mode, Target};

/// How often the rows are drained.
///
/// The cost of a drain is one syscall per row that saw an allocation in the
/// interval, not per allocation, which is the whole point of bucketing in the
/// kernel. Draining often also keeps the map far from the capacity at which it
/// would have to start refusing rows.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// Holds the request until the target exists.
pub struct Prepare {
    pub allocator: Allocator,
    pub mode: Mode,
    /// The keys the request asked to group by. Carried because one of them
    /// cannot be served by an aggregating collector and has to be refused
    /// rather than silently answered with no key at all.
    pub keys: Vec<KeyKind>,
    /// The request's predicate, which this collector applies itself; see
    /// [`AllocationCollector::predicate`].
    pub filter: Option<FilterExpr>,
}

impl Prepare {
    fn source(&self) -> &'static str {
        match self.allocator {
            Allocator::Pages => "mem.page_alloc",
            Allocator::Slab => "mem.kmalloc",
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(self.mode, Mode::Trace | Mode::Count) {
            return Err(cannot_mean_anything(format!(
                "`{}` mode cannot count allocations: an allocation is an instant, and sampling \
                 instants at a frequency counts how often the sampler looked rather than how \
                 often the allocator was called",
                self.mode
            )));
        }
        // The `mem` key is an individual allocation's address and length, and
        // the identity of an individual allocation is exactly what aggregating
        // in the kernel discards. Answering with no key at all would put every
        // task's allocations in one unnamed series and look like a machine that
        // allocates in one place.
        if self.keys.contains(&KeyKind::Mem) {
            return Err(not_built(format!(
                "`{}` counts allocations in the kernel, per task and per size, so it has no \
                 address to key a `mem:[base,len)` by: that key names one allocation and this \
                 source never carries one up. Group by `task` or by `cgroup` instead",
                self.source()
            )));
        }

        let cgroups = match (self.allocator, target) {
            (_, Target::System) => None,
            (Allocator::Pages, Target::Cgroup(_)) => {
                let root = Cgroup::resolve(target)?;
                Some(descendants(&root)?)
            }
            (Allocator::Pages, Target::Container(_)) => {
                return Err(not_built(
                    "a container has to be resolved to its cgroup first, which is not built \
                     yet; pass `--cgroup` with the path",
                ))
            }
            (_, other) => {
                return Err(cannot_mean_anything(format!(
                    "the {} allocator is a property of the machine, and a {} target names \
                     something it is not accounted per; use --system{}",
                    match self.allocator {
                        Allocator::Pages => "page",
                        Allocator::Slab => "slab",
                    },
                    other.kind(),
                    match self.allocator {
                        Allocator::Pages => " or --cgroup",
                        Allocator::Slab => "",
                    }
                )))
            }
        };

        Ok(Box::new(AllocationCollector::new(
            self.allocator,
            self.mode,
            cgroups,
            self.filter,
        )))
    }
}

/// Every cgroup id at or below a cgroup directory.
///
/// A `--cgroup` target means the accounting domain and everything inside it: a
/// container's own cgroup usually has one child per process manager, and a task
/// in a child is in the container by every definition a reader means. The ids
/// are inode numbers, which is what `bpf_get_current_cgroup_id` returns, so the
/// comparison needs nothing from the kernel that is not already in the row.
pub(super) fn descendants(root: &Cgroup) -> Result<BTreeSet<u64>, String> {
    use std::os::unix::fs::MetadataExt;

    let mut ids = BTreeSet::from([root.id]);
    let mut pending = vec![root.directory.clone()];
    while let Some(directory) = pending.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            // A cgroup removed while it is being walked is not an error: the
            // last process in it left, which is the ordinary end of a
            // container.
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if !kind.is_dir() {
                continue;
            }
            if let Ok(metadata) = entry.metadata() {
                ids.insert(metadata.ino());
            }
            pending.push(entry.path());
        }
    }
    Ok(ids)
}

pub struct AllocationCollector {
    id: CollectorId,
    allocator: Allocator,
    /// What the two declared modes differ by: `trace` publishes one event per
    /// row per drain, `count` adds the drains up and reports one total per row
    /// over the whole window.
    mode: Mode,
    /// The cgroups whose rows are kept, or none for the whole machine.
    cgroups: Option<BTreeSet<u64>>,
    program: Option<AllocProgram>,
    losses: Losses,
    /// The kernel's skipped runs of the program, cumulative, as last read.
    skipped: u64,
    /// The program's counter is cumulative, so what is new is the difference.
    dropped: u64,
    /// The end of the last interval, which is the start of this one.
    window_opened_ns: u64,
    /// When the whole window opened, which is the one `count` reports over.
    run_opened_ns: u64,
    /// The totals a `count` run accumulates, by the triple that keys a row.
    counted: BTreeMap<(u32, u64, u32), AllocRow>,
    /// Allocations this run counted, which is what the event count is not.
    allocations: u64,
    /// The rows those allocations were reported as, which is the event count.
    rows: u64,
    /// Frees, and what they freed where the tracepoint says: `mm_page_free`
    /// carries the order and `kfree` carries nothing but the pointer.
    frees: u64,
    freed: u64,
    /// Rows whose cgroup was not the one the request named.
    outside: u64,
    /// The request's predicate, applied to each row as it is drained.
    ///
    /// The resolver credits any backend whose id begins with `bpf` with pushing
    /// a filter into the kernel and then leaves its events alone. That credit is
    /// given from the backend's *name*, so a collector that does nothing with
    /// the predicate returns every event and nothing anywhere says the filter
    /// did not run. Applying it here is what makes the resolver's claim true of
    /// the numbers; what it is not true of is the cost, and the run says so.
    predicate: Option<FilterExpr>,
    filtered_out: u64,
    filtered_in: u64,
}

impl AllocationCollector {
    pub fn new(
        allocator: Allocator,
        mode: Mode,
        cgroups: Option<BTreeSet<u64>>,
        predicate: Option<FilterExpr>,
    ) -> Self {
        Self {
            id: CollectorId::new(match allocator {
                Allocator::Pages => "mem.page_alloc/bpf:raw_tp",
                Allocator::Slab => "mem.kmalloc/bpf:raw_tp",
            }),
            allocator,
            mode,
            cgroups,
            program: None,
            losses: Losses::default(),
            skipped: 0,
            dropped: 0,
            window_opened_ns: 0,
            run_opened_ns: 0,
            counted: BTreeMap::new(),
            allocations: 0,
            rows: 0,
            frees: 0,
            freed: 0,
            outside: 0,
            predicate: None,
            filtered_out: 0,
            filtered_in: 0,
        }
        .with_predicate(predicate)
    }

    fn with_predicate(mut self, predicate: Option<FilterExpr>) -> Self {
        self.predicate = predicate;
        self
    }

    fn kind(&self) -> &'static str {
        match self.allocator {
            Allocator::Pages => "mem.page_alloc",
            Allocator::Slab => "mem.kmalloc",
        }
    }

    /// Whether a free is an event in this run: a page free, asked about by a
    /// predicate that names `direction`. Without one, frees are counted and
    /// the run says so -- a default that merged them would be a histogram of
    /// neither.
    fn publishes_frees(&self) -> bool {
        self.allocator == Allocator::Pages
            && self
                .predicate
                .as_ref()
                .is_some_and(|predicate| predicate.fields().contains("direction"))
    }

    /// Whether this run reports one measurement at the end rather than a series.
    fn counting(&self) -> bool {
        self.mode == Mode::Count
    }

    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let closed_ns = monotonic_ns();
        let window_ns = closed_ns.saturating_sub(self.window_opened_ns);
        let (rows, drain_losses) = match program.drain() {
            Ok(drained) => drained,
            Err(reason) => {
                // A drain that failed is a window whose rows are still in the
                // map: they are not lost, they arrive with the next one. What
                // is lost is knowing which window they belonged to.
                self.losses.failed_reads += 1;
                self.losses.note_times(reason, 1);
                return;
            }
        };
        self.window_opened_ns = closed_ns;

        if drain_losses.vanished > 0 {
            self.losses.undecoded += drain_losses.vanished;
            self.losses
                .note("an allocation row was gone between being listed and being read");
        }
        if drain_losses.truncated {
            self.losses
                .note("the allocation histogram held more rows than one drain can walk");
        }

        let allocator = self.allocator;
        let mut index = 0;
        for row in rows {
            if let Some(wanted) = &self.cgroups {
                if !wanted.contains(&row.cgroup_id) {
                    self.outside += row.count;
                    continue;
                }
            }
            if row.freeing && !self.publishes_frees() {
                self.frees += row.count;
                self.freed += row.total_size(allocator);
                continue;
            }
            let event = Event {
                ts_ns: closed_ns,
                ts_err_ns: 0,
                kind: self.kind().to_owned(),
                keys: self.keys_of(&row),
                attrs: attributes(allocator, &row, window_ns),
                collector_id: self.id.clone(),
                seq: seq + index,
            };
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += row.count;
                    continue;
                }
                self.filtered_in += row.count;
            }
            self.allocations += row.count;
            if self.counting() {
                // The map is still drained -- leaving it to fill would lose
                // rows -- but what comes out is added to the window's total
                // rather than published as a slice of it.
                self.counted
                    .entry((row.tid, row.cgroup_id, row.bucket))
                    .and_modify(|held| {
                        held.count += row.count;
                        held.requested += row.requested;
                        held.comm.clone_from(&row.comm);
                    })
                    .or_insert(row);
                continue;
            }
            self.rows += 1;
            index += 1;
            events.push(event);
        }
    }

    fn keys_of(&self, row: &AllocRow) -> Vec<KeyValue> {
        let mut keys = vec![KeyValue::task(row.tid as i32)];
        // Only where the entry declares the key, and only where the kernel gave
        // one: a task outside every cgroup reports zero, and `cgroup:0` names
        // nothing.
        if self.allocator == Allocator::Pages && row.cgroup_id != 0 {
            keys.push(KeyValue::Cgroup { id: row.cgroup_id });
        }
        keys
    }

    /// Reads the program's own counter of what it could not record.
    ///
    /// A full map is the only way this source loses an allocation, and nothing
    /// else anywhere would record that it happened: the helper returns an error
    /// in the kernel and the allocation is simply never counted.
    fn absorb_drops(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        // The kernel's own count of runs it skipped, which no counter of the
        // program's can hold. It is the machine's -- the program runs on every
        // occurrence and narrows to the target after -- so it is an upper
        // bound on what the target lost, and the reason says so.
        let skipped = program.skipped();
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note_times(
                "the kernel skipped a run of this program because one was already running on \
                 that cpu, so an occurrence was never seen; the count is the machine's, an \
                 upper bound on the target's share, and every total here is a lower bound",
                skipped - self.skipped,
            );
            self.skipped = skipped;
        }
        let dropped = program.dropped();
        if dropped > self.dropped {
            self.losses.dropped += dropped - self.dropped;
            self.losses.note_times(
                format!(
                    "more distinct (task, cgroup, size, direction) rows appeared in one interval \
                 than the {} rows the histogram holds, so their allocations were never counted",
                    program.capacity()
                ),
                dropped - self.dropped,
            );
            self.dropped = dropped;
        }
    }
}

/// What an event carries beside its value.
///
/// Every field the catalog declares as a filter field is here: a predicate over
/// a field no event carries is false everywhere, which empties the series and
/// looks exactly like a machine that never allocated.
fn attributes(allocator: Allocator, row: &AllocRow, window_ns: u64) -> Vec<Attr> {
    // The low bound of the bucket rather than its mean, because it is the one
    // number that puts the row back in the same bucket when the emit layer
    // buckets it again: a histogram built in the kernel and one built in
    // userspace from these events agree exactly. For a page row it is not even
    // a bound -- every allocation behind the row is exactly this many pages.
    let size = row.size();
    let mut attrs = vec![
        // How many allocations this event stands for. Every number derived from
        // the value is this many times over, which is what the emit layer reads
        // out of `weight`; `count` carries the same number under the name a
        // reader and the filter grammar expect.
        Attr::uint("count", row.count),
        Attr::uint(crate::emit::WEIGHT_ATTR, row.count),
        Attr::uint("bucket", u64::from(row.bucket)),
        // The interval the row was collected over. Not a denominator: an
        // allocation is counted where it happened, and the emit layer's `rate`
        // divides by the run's own window.
        Attr::uint("window_ns", window_ns),
        Attr::uint("tid", u64::from(row.tid)),
        Attr::uint("pid", u64::from(row.tgid)),
        Attr::text("comm", row.comm.clone()),
        Attr::text("direction", if row.freeing { "free" } else { "alloc" }),
    ];
    match allocator {
        Allocator::Pages => {
            attrs.insert(0, Attr::uint("pages", size));
            attrs.push(Attr::uint("total_pages", row.total_size(allocator)));
            attrs.push(Attr::uint("bytes", size.saturating_mul(page_bytes())));
            // The allocation order, which is what the catalog offers as a
            // filter. The bucket is the order plus one, so this is exact and
            // not a bound.
            attrs.push(Attr::uint("order", u64::from(row.order())));
        }
        Allocator::Slab => {
            attrs.insert(0, Attr::uint("bytes", size));
            // The exact sums the bucket cannot give back: a log2 bucket is a
            // factor of two wide, so a total built from bounds inherits that
            // error and these do not.
            attrs.push(Attr::uint("total_bytes", row.total_size(allocator)));
            attrs.push(Attr::uint("requested_bytes", row.requested));
        }
    }
    if row.cgroup_id != 0 {
        attrs.push(Attr::uint("cgroup_id", row.cgroup_id));
    }
    attrs
}

/// The machine's page size, which is what turns a page count into bytes.
fn page_bytes() -> u64 {
    // SAFETY: `sysconf` takes an integer and returns one; there is no pointer
    // and no state involved.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as u64
    } else {
        4096
    }
}

impl Collector for AllocationCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        match self.allocator {
            Allocator::Pages => "pages",
            Allocator::Slab => "bytes",
        }
    }

    fn row_stamp(&self) -> Option<crate::collect::RowStamp> {
        use crate::collect::RowStamp;
        Some(RowStamp::DrainClose)
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(AllocProgram::load(self.allocator)?);
        self.run_opened_ns = monotonic_ns();
        self.window_opened_ns = self.run_opened_ns;
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        self.absorb_drops();
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
        self.absorb_drops();
        let allocator = self.allocator;
        if self.counting() {
            let closed_ns = monotonic_ns();
            let window_ns = closed_ns.saturating_sub(self.run_opened_ns);
            let seq = u64::MAX / 2;
            let counted = std::mem::take(&mut self.counted);
            self.rows += counted.len() as u64;
            for (index, row) in counted.into_values().enumerate() {
                trailing.push(Event {
                    ts_ns: closed_ns,
                    ts_err_ns: 0,
                    kind: self.kind().to_owned(),
                    keys: self.keys_of(&row),
                    attrs: attributes(allocator, &row, window_ns),
                    collector_id: self.id.clone(),
                    seq: seq + index as u64,
                });
            }
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
        if self.allocations > 0 {
            notes.push(format!(
                "{} allocations are reported as {} rows of a kernel-side log2 histogram, each \
                 carrying the allocations it stands for; every number derived from the value is \
                 weighted by that count",
                self.allocations, self.rows,
            ));
        }
        // The other half of the ledger, which is the first thing anyone asks
        // for and the one thing this source cannot publish as events.
        if self.frees > 0 {
            notes.push(match self.allocator {
                Allocator::Pages => format!(
                    "{} page frees totalling {} pages happened in the same window and are not in \
                     these events, because a histogram of allocations and frees together is a \
                     histogram of neither; a filter on `direction` (`direction == 'free'`) \
                     makes them the events",
                    self.frees, self.freed,
                ),
                Allocator::Slab => format!(
                    "{} kfree calls happened in the same window and are not events here, and \
                     their size is not reported at all: `kfree` carries the pointer and the call \
                     site and nothing that says how large the object was",
                    self.frees,
                ),
            });
        }
        if self.outside > 0 {
            notes.push(format!(
                "{} allocations were made outside the cgroup this run named and were dropped \
                 here, after the kernel had counted them: the cgroup is matched over the drained \
                 rows rather than in the program",
                self.outside,
            ));
        }
        if self.filtered_out > 0 || self.filtered_in > 0 {
            notes.push(format!(
                "the filter kept {} allocations and removed {}; it ran here, over the drained \
                 rows, not in the kernel, so the removed ones were counted before they were \
                 dropped",
                self.filtered_in, self.filtered_out,
            ));
        }
        if self.counting() {
            notes.push(
                "`count` reports one total per (task, cgroup, size) over the whole window, so \
                 there is no series here and `rate` over it is the window's mean; `--mode trace` \
                 is the one that reports a count per drain"
                    .to_owned(),
            );
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_row() -> AllocRow {
        AllocRow::for_tests(7, 7, 0, 10, false, 3, 0, 0, "toucher")
    }

    fn slab_row() -> AllocRow {
        AllocRow::for_tests(9, 9, 0, 12, false, 4, 9_000, 7_600, "sender")
    }

    fn of(attributes: &[Attr], name: &str) -> f64 {
        attributes
            .iter()
            .find(|attr| attr.name == name)
            .and_then(|attr| attr.value.as_f64())
            .unwrap_or_else(|| panic!("no `{name}` attribute"))
    }

    /// Every filter field the catalog offers must be on the events, or a
    /// predicate over it is false everywhere and silently empties the series.
    #[test]
    fn a_page_free_is_an_event_exactly_when_the_predicate_asks_about_direction() {
        let with = |allocator, filter: Option<&str>| {
            AllocationCollector::new(
                allocator,
                Mode::Trace,
                None,
                filter.map(|text| text.parse().expect("a predicate")),
            )
            .publishes_frees()
        };
        assert!(with(Allocator::Pages, Some("direction == 'free'")));
        assert!(with(
            Allocator::Pages,
            Some("direction == 'free' && order > 0")
        ));
        assert!(
            !with(Allocator::Pages, Some("comm == 'dd'")),
            "a merge nobody asked for"
        );
        assert!(!with(Allocator::Pages, None));
        assert!(
            !with(Allocator::Slab, Some("direction == 'free'")),
            "a kfree has no size, so it is never an event of a source in bytes"
        );
        // And the event says which side it is, so the predicate can select.
        let mut free = page_row();
        free.freeing = true;
        let attrs = attributes(Allocator::Pages, &free, 1);
        assert!(attrs
            .iter()
            .any(|attr| attr.name == "direction" && attr.value.to_string() == "free"));
    }

    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        for (source, allocator, row) in [
            ("mem.page_alloc", Allocator::Pages, page_row()),
            ("mem.kmalloc", Allocator::Slab, slab_row()),
        ] {
            let attributes = attributes(allocator, &row, 100_000);
            let declared = &catalog
                .get(&source.parse().expect("a source id"))
                .unwrap_or_else(|| panic!("{source} is in the catalog"))
                .filter_fields;
            assert!(!declared.is_empty(), "{source} declares filter fields");
            for field in declared {
                assert!(
                    attributes.iter().any(|attr| attr.name == *field),
                    "{source} offers a `{field}` filter and no event carries one"
                );
            }
        }
    }

    /// The value is the bucket's low bound, which for a page row is the exact
    /// size of every allocation behind it, and the total is exact either way.
    #[test]
    fn a_page_rows_value_is_its_exact_size_and_its_total_is_its_count_times_it() {
        let attributes = attributes(Allocator::Pages, &page_row(), 1);
        assert_eq!(of(&attributes, "order"), 9.0, "bucket 10 is order 9");
        assert_eq!(of(&attributes, "pages"), 512.0);
        assert_eq!(of(&attributes, "count"), 3.0);
        assert_eq!(of(&attributes, "total_pages"), 3.0 * 512.0);
        assert_eq!(of(&attributes, crate::emit::WEIGHT_ATTR), 3.0);
    }

    /// A slab row's value is a bound and its totals are not, which is the whole
    /// reason the kernel accumulates them: four allocations in `[2048, 4096)`
    /// are between 8192 and 16384 bytes, and the bound alone would say 8192.
    #[test]
    fn a_slab_rows_totals_are_exact_where_its_bucket_is_only_a_bound() {
        let attributes = attributes(Allocator::Slab, &slab_row(), 1);
        assert_eq!(of(&attributes, "bytes"), 2048.0, "bucket 12 starts at 2^11");
        assert_eq!(of(&attributes, "total_bytes"), 9_000.0);
        assert_eq!(of(&attributes, "requested_bytes"), 7_600.0);
        assert!(
            of(&attributes, "total_bytes") > of(&attributes, "bytes") * of(&attributes, "count"),
            "the exact total is what the bucket bounds cannot give back"
        );
    }

    /// A sampled allocation is an allocation nobody counted. Refused with the
    /// reason rather than served with a number of samples.
    #[test]
    fn a_sampled_mode_is_refused_with_the_reason() {
        let prepared = Box::new(Prepare {
            allocator: Allocator::Pages,
            mode: Mode::Sample {
                freq: "997hz".parse().expect("a frequency"),
            },
            keys: Vec::new(),
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::System) else {
            panic!("a sampled mode must be refused");
        };
        assert!(error.contains("cannot count allocations"), "{error}");
    }

    /// The `mem` key names one allocation, and aggregating in the kernel is
    /// exactly the discarding of that identity. Refused by name rather than
    /// answered with an unkeyed series.
    #[test]
    fn a_request_keyed_by_an_allocations_own_address_is_refused_by_name() {
        let prepared = Box::new(Prepare {
            allocator: Allocator::Slab,
            mode: Mode::Trace,
            keys: vec![KeyKind::Mem],
            filter: None,
        });
        let Err(error) = prepared.attach(&Target::System) else {
            panic!("a `mem` key must be refused");
        };
        assert!(error.contains("mem:[base,len)"), "{error}");
        assert!(error.contains("task"), "and it names what to use: {error}");
    }

    /// A target this source is not accounted per is refused by name, never
    /// answered with the machine's number under that target's label.
    #[test]
    fn a_target_the_allocator_is_not_accounted_per_is_refused_by_name() {
        for (allocator, target) in [
            (Allocator::Pages, Target::Pid(vec![1])),
            (Allocator::Slab, Target::Cgroup("/sys/fs/cgroup".into())),
        ] {
            let prepared = Box::new(Prepare {
                allocator,
                mode: Mode::Trace,
                keys: Vec::new(),
                filter: None,
            });
            let Err(error) = prepared.attach(&target) else {
                panic!("{target} must be refused for {allocator:?}");
            };
            assert!(error.contains("--system"), "{error}");
        }
    }

    /// The frees are half the ledger and are not events. A run that saw them
    /// has to say so, with the numbers, or a reader takes the allocation total
    /// for a growth.
    #[test]
    fn a_run_that_saw_frees_reports_them_even_though_they_are_not_events() {
        let mut collector = AllocationCollector::new(Allocator::Pages, Mode::Trace, None, None);
        assert!(collector.notes().is_empty(), "a run that saw nothing");
        collector.allocations = 90;
        collector.rows = 4;
        collector.frees = 80;
        collector.freed = 1024;
        let notes = collector.notes().join(" ");
        assert!(notes.contains("90 allocations"), "{notes}");
        assert!(notes.contains("80 page frees"), "{notes}");
        assert!(notes.contains("1024 pages"), "{notes}");

        // And the slab side says the harder thing: it does not know the size.
        let mut slab = AllocationCollector::new(Allocator::Slab, Mode::Trace, None, None);
        slab.frees = 12;
        let notes = slab.notes().join(" ");
        assert!(notes.contains("12 kfree"), "{notes}");
        assert!(notes.contains("how large the object was"), "{notes}");
    }

    /// A cgroup match made after the kernel counted the row is not the
    /// kernel-side filtering the resolver credits the backend with, and the run
    /// says which it was.
    #[test]
    fn a_cgroup_match_made_in_userspace_says_that_it_was() {
        let mut collector = AllocationCollector::new(
            Allocator::Pages,
            Mode::Trace,
            Some(BTreeSet::from([4242])),
            None,
        );
        collector.outside = 17;
        let notes = collector.notes().join(" ");
        assert!(notes.contains("17 allocations"), "{notes}");
        assert!(notes.contains("rather than in the program"), "{notes}");
    }
}
