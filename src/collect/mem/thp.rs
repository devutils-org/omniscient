//! `mem.thp` over `/sys/kernel/mm/transparent_hugepage` and `/proc/vmstat`.
//!
//! Transparent huge pages are the reason an RSS curve grows in two-megabyte
//! steps and the reason two runs of the same program are not comparable unless
//! the mode was the same for both. `khugepaged` promotes in the background, on
//! its own schedule, so a run can acquire huge pages it never asked for --
//! which is why the configured mode belongs in the environment capture beside
//! the clock frequencies, and why this source reports the mode next to the
//! counters rather than only the counters.
//!
//! Two backends, and the difference between them is the mode:
//!
//! - `fs:sys` reads the kernel's per-size statistics, and the `enabled` file
//!   that says which policy produced them. Those statistics arrived in 6.8;
//!   the requirement says so, so the resolver falls through rather than the
//!   collector failing at attach.
//! - `fs:proc` reads the same promotions out of `/proc/vmstat`, which every
//!   kernel has and which says nothing about how the machine is configured.
//!
//! **This source reports the PMD-sized huge page and says so on every event.**
//! Recent kernels also allocate smaller multi-size huge pages -- 16 KiB through
//! 1 MiB here -- and count each size separately. Adding them together would
//! produce a "number of huge pages" that is not a quantity of memory and not a
//! number of anything in particular, which is the same mistake as a fault count
//! read as a quantity of memory. One size, named on the event.

use std::path::{Path, PathBuf};

use super::cadence::Cadence;
use super::cgroup::{stat_field, stat_fields};
use crate::clock::monotonic_ns;
use crate::collect::poll::{CounterPoll, Cumulative, Interval, Reading};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event};
use crate::request::{Mode, Target};

const SYSFS: &str = "/sys/kernel/mm/transparent_hugepage";
const VMSTAT: &str = "/proc/vmstat";

/// Where the counters are read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The per-size statistics under sysfs, and the configured mode.
    Sys,
    /// The machine-wide promotion counters in `/proc/vmstat`.
    Proc,
}

impl Source {
    fn read_from(self) -> &'static str {
        match self {
            Source::Sys => SYSFS,
            Source::Proc => VMSTAT,
        }
    }
}

/// The counters one read found, in huge pages.
///
/// Each field is optional because the kernel grew them at different versions,
/// and a counter this kernel does not publish is absent rather than zero: zero
/// would say no page was ever split on a kernel that does not count splits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counters {
    ts_ns: u64,
    fault_alloc: Option<u64>,
    fault_fallback: Option<u64>,
    collapse_alloc: Option<u64>,
    split: Option<u64>,
    /// How many huge pages are fully mapped right now -- a level, not a flow.
    /// This is the number `/proc/meminfo` publishes as `AnonHugePages`, and
    /// both backends have to reach it or say they cannot.
    resident: Option<u64>,
    /// Huge pages that exist but have had subpages unmapped. Only the per-size
    /// statistics count them.
    partially_mapped: Option<u64>,
    /// Every anonymous huge folio of this size, however it is mapped: whole
    /// through one PMD, split into page-table entries that still map every
    /// subpage (an `mprotect` or `mremap` of part of it, a fork), or partially
    /// unmapped. Only the per-size statistics count it, and it is not
    /// `resident` minus anything this kernel publishes.
    huge_folios: Option<u64>,
}

impl Counters {
    /// Promotions are huge pages that came into being: allocated on a fault, or
    /// collapsed out of small pages by `khugepaged`. The two are different
    /// events with the same outcome, and both are carried separately as well.
    fn promotions(&self) -> Option<u64> {
        match (self.fault_alloc, self.collapse_alloc) {
            (None, None) => None,
            (fault, collapse) => Some(fault.unwrap_or(0) + collapse.unwrap_or(0)),
        }
    }
}

impl Cumulative for Counters {
    /// The flows' increase, and the levels as they stand at the later read.
    /// The promotions have to have grown; a flow the kernel does not count is
    /// absent at both ends, and absent from the increase.
    fn since(&self, earlier: &Self) -> Option<Self> {
        let delta = |after: Option<u64>, before: Option<u64>| after?.checked_sub(before?);
        delta(self.promotions(), earlier.promotions())?;
        Some(Counters {
            fault_alloc: delta(self.fault_alloc, earlier.fault_alloc),
            fault_fallback: delta(self.fault_fallback, earlier.fault_fallback),
            collapse_alloc: delta(self.collapse_alloc, earlier.collapse_alloc),
            split: delta(self.split, earlier.split),
            ..*self
        })
    }
}

/// Holds the mode and the backend until the run starts.
pub struct Prepare {
    mode: Mode,
    source: Source,
}

impl Prepare {
    pub fn new(source: Source, mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } | Mode::Snapshot => Ok(Self { mode, source }),
            other => Err(format!(
                "huge page promotions are counters that are read, not a stream of events: `{}` \
                 cannot be served from {}; poll it, or take one snapshot",
                other.kind(),
                source.read_from()
            )),
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(target, Target::System) {
            return Err(format!(
                "huge page promotion is counted for the machine, and a {} target names \
                 something narrower: the kernel publishes no per-process promotion counter, and \
                 answering a process with the machine's total would be attribution nobody \
                 measured",
                target.kind()
            ));
        }
        let interval = match self.mode {
            Mode::Poll { interval } => std::time::Duration::from_nanos(interval.as_nanos()),
            _ => std::time::Duration::ZERO,
        };
        match self.source {
            Source::Sys => {
                let size = pmd_size(Path::new(SYSFS)).ok_or_else(|| {
                    format!("{SYSFS}/hpage_pmd_size does not say how large a huge page is here")
                })?;
                let stats = stats_directory(Path::new(SYSFS), size);
                if !stats.exists() {
                    return Err(format!(
                        "{} does not exist: this kernel publishes no per-size huge page \
                         statistics, so the promotions can only be read from {VMSTAT} -- which \
                         is the `fs:proc` backend, reachable with `--backend force:fs:proc`",
                        stats.display()
                    ));
                }
                Ok(Box::new(Thp::new(Source::Sys, self.mode, size, interval)))
            }
            Source::Proc => Ok(Box::new(Thp::new(
                Source::Proc,
                self.mode,
                // `/proc/vmstat`'s counters are the PMD-sized ones, whatever
                // that size is on this architecture.
                pmd_size(Path::new(SYSFS)).unwrap_or(0),
                interval,
            ))),
        }
    }
}

/// Reads the huge page counters.
pub struct Thp {
    source: Source,
    mode: Mode,
    root: PathBuf,
    vmstat: PathBuf,
    /// The PMD huge page size in bytes, zero where the kernel would not say.
    hpage_bytes: u64,
    poll: CounterPoll<(), Counters>,
    cadence: Cadence,
    losses: Losses,
}

impl Thp {
    pub fn new(
        source: Source,
        mode: Mode,
        hpage_bytes: u64,
        interval: std::time::Duration,
    ) -> Self {
        Self {
            source,
            mode,
            root: PathBuf::from(SYSFS),
            vmstat: PathBuf::from(VMSTAT),
            hpage_bytes,
            poll: CounterPoll::new(mode),
            cadence: Cadence::new(interval),
            losses: Losses::default(),
        }
    }

    /// Points the collector at another tree, so the reader can be checked
    /// against a layout the test wrote itself.
    #[cfg(test)]
    fn rooted(mut self, root: PathBuf, vmstat: PathBuf) -> Self {
        self.root = root;
        self.vmstat = vmstat;
        self
    }

    fn read(&self) -> Result<Counters, SampleError> {
        match self.source {
            Source::Sys => self.read_sysfs(),
            Source::Proc => self.read_vmstat(),
        }
    }

    fn reading(&self) -> Result<Reading<(), Counters>, SampleError> {
        let counters = self.read()?;
        Ok(Reading {
            ts_ns: counters.ts_ns,
            counters: [((), counters)].into(),
        })
    }

    fn read_sysfs(&self) -> Result<Counters, SampleError> {
        let stats = stats_directory(&self.root, self.hpage_bytes);
        let field = |name: &str| {
            std::fs::read_to_string(stats.join(name))
                .ok()
                .and_then(|text| text.trim().parse::<u64>().ok())
        };
        // `nr_anon` counts every anonymous huge folio of this size, however
        // it is mapped; `nr_anon_transparent_hugepages` in /proc/vmstat --
        // which is what `/proc/meminfo` reports as `AnonHugePages` -- counts
        // the ones mapped by a PMD. Publishing the first under the second's
        // name made the two backends 8 % apart and said nothing. The fifteenth
        // wave subtracted `nr_anon_partially_mapped`, which matched on the
        // day (412 - 30 = 382, against vmstat's 382) and is not the kernel's
        // arithmetic: a folio whose PMD was split into page-table entries
        // that still map every subpage is in `nr_anon`, in neither of the
        // other two. Measured on this machine in the sixteenth wave: 494
        // folios, 3 partially mapped, 357 PMD-mapped -- 134 apart, every
        // round. The per-size statistics have no PMD-mapped count, so the
        // whole-page level is read where the kernel keeps it, and the folios
        // travel beside it under their own name.
        let allocated = field("nr_anon");
        let partially_mapped = field("nr_anon_partially_mapped");
        let pmd_mapped = std::fs::read_to_string(&self.vmstat)
            .ok()
            .and_then(|text| stat_field(&stat_fields(&text), "nr_anon_transparent_hugepages"));
        let counters = Counters {
            ts_ns: monotonic_ns(),
            fault_alloc: field("anon_fault_alloc"),
            fault_fallback: field("anon_fault_fallback"),
            collapse_alloc: field("collapse_alloc"),
            split: field("split"),
            // Absent rather than approximated where /proc/vmstat has no such
            // line: answering with `nr_anon`, or with `nr_anon` less anything,
            // would be the defect again under a different name.
            resident: pmd_mapped,
            partially_mapped,
            huge_folios: allocated,
        };
        if counters.promotions().is_none() {
            return Err(SampleError::Transient(format!(
                "{} holds no allocation counter",
                stats.display()
            )));
        }
        Ok(counters)
    }

    fn read_vmstat(&self) -> Result<Counters, SampleError> {
        let text = std::fs::read_to_string(&self.vmstat).map_err(|error| {
            SampleError::Transient(format!("{}: {error}", self.vmstat.display()))
        })?;
        let fields = stat_fields(&text);
        let counters = Counters {
            ts_ns: monotonic_ns(),
            fault_alloc: stat_field(&fields, "thp_fault_alloc"),
            fault_fallback: stat_field(&fields, "thp_fault_fallback"),
            collapse_alloc: stat_field(&fields, "thp_collapse_alloc"),
            split: stat_field(&fields, "thp_split_page"),
            resident: stat_field(&fields, "nr_anon_transparent_hugepages"),
            // /proc/vmstat has no equivalent of either, and this backend
            // declares that it sees less than the other rather than guessing.
            partially_mapped: None,
            huge_folios: None,
        };
        if counters.promotions().is_none() {
            return Err(SampleError::Transient(format!(
                "{} holds no `thp_fault_alloc` line, so this kernel has no transparent huge \
                 pages to count",
                self.vmstat.display()
            )));
        }
        Ok(counters)
    }

    /// The configured policy, which only the sysfs backend can see.
    ///
    /// `enabled` is written as a list with the active entry in brackets:
    /// `always [madvise] never`.
    fn mode_attrs(&self) -> Vec<Attr> {
        if self.source != Source::Sys {
            return Vec::new();
        }
        let mut attrs = Vec::new();
        for (file, name) in [("enabled", "thp_mode"), ("defrag", "defrag_mode")] {
            if let Some(selected) = selected_option(&self.root.join(file)) {
                attrs.push(Attr::text(name, selected));
            }
        }
        attrs
    }

    fn common_attrs(&self, counters: &Counters) -> Vec<Attr> {
        let mut attrs = vec![Attr::text("read_from", self.source.read_from())];
        if self.hpage_bytes > 0 {
            attrs.push(Attr::uint("hpage_bytes", self.hpage_bytes));
        }
        if let Some(resident) = counters.resident {
            attrs.push(Attr::uint("resident_huge_pages", resident));
            if self.hpage_bytes > 0 {
                attrs.push(Attr::uint(
                    "resident_huge_bytes",
                    resident * self.hpage_bytes,
                ));
            }
        }
        // Carried beside the comparable number rather than folded into it: a
        // partially mapped huge page is still a huge page the machine cannot
        // hand back, and only one of the two backends can see how many there
        // are.
        if let Some(partially_mapped) = counters.partially_mapped {
            attrs.push(Attr::uint("partially_mapped_huge_pages", partially_mapped));
        }
        if let Some(huge_folios) = counters.huge_folios {
            attrs.push(Attr::uint("huge_folios", huge_folios));
        }
        attrs.extend(self.mode_attrs());
        attrs
    }

    fn promotion_event(&mut self, interval: Interval<(), Counters>, seq: u64) -> Option<Event> {
        let interval_ns = interval.interval_ns;
        let after = interval.after.counters[&()];
        let Some((moved, promotions)) = interval
            .moved
            .first()
            .and_then(|&((), moved)| Some((moved, moved.promotions()?)))
        else {
            self.losses.undecoded += 1;
            self.losses.note_times(
                format!(
                "a huge page counter in {} went backwards, so this interval cannot be attributed",
                self.source.read_from()
            ),
                1,
            );
            return None;
        };

        let mut attrs = vec![
            Attr::uint("promotions", promotions),
            Attr::uint("interval_ns", interval_ns),
            Attr::text("basis", "the promotion counters' increase over this tick"),
        ];
        if self.hpage_bytes > 0 {
            attrs.push(Attr::uint("promoted_bytes", promotions * self.hpage_bytes));
        }
        for (name, value) in [
            ("fault_alloc", moved.fault_alloc),
            ("fault_fallback", moved.fault_fallback),
            ("collapse_alloc", moved.collapse_alloc),
            ("split", moved.split),
        ] {
            if let Some(value) = value {
                attrs.push(Attr::uint(name, value));
            }
        }
        attrs.extend(self.common_attrs(&after));
        Some(self.event(interval.ts_ns, attrs, seq))
    }

    fn snapshot_event(&self, counters: &Counters, seq: u64) -> Event {
        let mut attrs = vec![Attr::text(
            "basis",
            "the counters as they stand: a snapshot observes no window of its own",
        )];
        if let Some(promotions) = counters.promotions() {
            attrs.push(Attr::uint("promotions_since_boot", promotions));
        }
        attrs.extend(self.common_attrs(counters));
        self.event(counters.ts_ns, attrs, seq)
    }

    fn event(&self, ts_ns: u64, attrs: Vec<Attr>, seq: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "mem.thp".to_owned(),
            keys: Vec::new(),
            attrs,
            collector_id: CollectorId::unstamped(),
            seq,
        }
    }
}

impl Collector for Thp {
    fn value_attr(&self) -> &'static str {
        match self.mode {
            Mode::Snapshot => "promotions_since_boot",
            _ => "promotions",
        }
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if !self.cadence.due(monotonic_ns()) {
            return Ok(Vec::new());
        }
        if self.mode == Mode::Snapshot {
            let now = self.read()?;
            return Ok(vec![self.snapshot_event(&now, seq)]);
        }
        let mut poll = std::mem::replace(&mut self.poll, CounterPoll::new(self.mode));
        let ticked = poll.tick(|| self.reading());
        self.poll = poll;
        Ok(ticked?
            .and_then(|interval| self.promotion_event(interval, seq))
            .into_iter()
            .collect())
    }

    /// The baseline, before the observed program is released.
    fn start(&mut self) -> Result<(), String> {
        let mut poll = std::mem::replace(&mut self.poll, CounterPoll::new(self.mode));
        let started = poll.start(|| self.reading());
        self.poll = poll;
        started.map_err(|error| format!("{error:?}"))
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

/// How large a PMD-sized huge page is on this machine, in bytes.
fn pmd_size(root: &Path) -> Option<u64> {
    std::fs::read_to_string(root.join("hpage_pmd_size"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The statistics directory of one huge page size.
fn stats_directory(root: &Path, hpage_bytes: u64) -> PathBuf {
    root.join(format!("hugepages-{}kB", hpage_bytes / 1024))
        .join("stats")
}

/// The bracketed entry of a sysfs multiple-choice file.
fn selected_option(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.split_whitespace()
        .find_map(|word| word.strip_prefix('[')?.strip_suffix(']'))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poll() -> Mode {
        Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        }
    }

    fn tree(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "omn-thp-{}-{}-{name}",
            std::process::id(),
            monotonic_ns()
        ));
        std::fs::create_dir_all(path.join("hugepages-2048kB/stats")).expect("a scratch tree");
        std::fs::write(path.join("hpage_pmd_size"), "2097152\n").expect("a size");
        std::fs::write(path.join("enabled"), "always [madvise] never\n").expect("a mode");
        path
    }

    fn write_stats(root: &Path, fault_alloc: u64, collapse_alloc: u64) {
        let stats = root.join("hugepages-2048kB/stats");
        std::fs::write(stats.join("anon_fault_alloc"), format!("{fault_alloc}\n")).unwrap();
        std::fs::write(stats.join("collapse_alloc"), format!("{collapse_alloc}\n")).unwrap();
    }

    /// The resident count the two backends both answer with is the huge
    /// pages mapped whole, by a PMD, which is what `/proc/vmstat` and
    /// `/proc/meminfo` publish. `nr_anon` counts every huge folio, and neither
    /// it nor it less the partially mapped ones is that number: the three
    /// counts below are the ones this machine published in the sixteenth
    /// wave, 134 folios apart.
    #[test]
    fn the_per_size_statistics_report_whole_huge_pages_and_the_folios_apart() {
        let root = tree("resident");
        write_stats(&root, 1, 1);
        let stats = root.join("hugepages-2048kB/stats");
        std::fs::write(stats.join("nr_anon"), "494\n").unwrap();
        std::fs::write(stats.join("nr_anon_partially_mapped"), "3\n").unwrap();
        let vmstat = root.join("vmstat");
        std::fs::write(
            &vmstat,
            "thp_fault_alloc 1\nnr_anon_transparent_hugepages 357\n",
        )
        .unwrap();

        let mut collector = Thp::new(
            Source::Sys,
            Mode::Snapshot,
            2 * 1024 * 1024,
            std::time::Duration::ZERO,
        )
        .rooted(root.clone(), vmstat);
        let events = collector.sample(0).expect("one read");
        let event = &events[0];
        assert_eq!(
            event.attr("resident_huge_pages").unwrap().as_f64(),
            Some(357.0),
            "the whole huge pages are the PMD-mapped ones the kernel counts in \
             nr_anon_transparent_hugepages, not 494 folios less the 3 partially mapped"
        );
        assert_eq!(
            event.attr("partially_mapped_huge_pages").unwrap().as_f64(),
            Some(3.0),
            "the partially mapped ones travel beside it rather than inside it"
        );
        assert_eq!(
            event.attr("huge_folios").unwrap().as_f64(),
            Some(494.0),
            "and so does every folio, under its own name"
        );
        assert_eq!(
            event.attr("resident_huge_bytes").unwrap().as_f64(),
            Some(357.0 * 2.0 * 1024.0 * 1024.0)
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Where /proc/vmstat has no PMD-mapped count, the per-size statistics
    /// cannot say how many huge pages are whole. Answering with `nr_anon`
    /// there would be the same defect under a different kernel version, so
    /// the attribute is absent -- the rule this collector already follows for
    /// every counter a kernel does not publish -- and the folios still travel
    /// under their own name.
    #[test]
    fn a_kernel_that_does_not_count_whole_huge_pages_reports_no_resident_count() {
        let root = tree("no-pmd-count");
        write_stats(&root, 1, 1);
        std::fs::write(root.join("hugepages-2048kB/stats/nr_anon"), "412\n").unwrap();
        let vmstat = root.join("vmstat");
        std::fs::write(&vmstat, "thp_fault_alloc 1\n").unwrap();

        let mut collector = Thp::new(
            Source::Sys,
            Mode::Snapshot,
            2 * 1024 * 1024,
            std::time::Duration::ZERO,
        )
        .rooted(root.clone(), vmstat);
        let events = collector.sample(0).expect("one read");
        assert!(
            events[0].attr("resident_huge_pages").is_none(),
            "412 is every huge folio, not the whole ones: {:?}",
            events[0]
        );
        assert!(events[0].attr("resident_huge_bytes").is_none());
        assert_eq!(events[0].attr("huge_folios").unwrap().as_f64(), Some(412.0));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_configured_mode_is_the_bracketed_one() {
        let root = tree("mode");
        assert_eq!(
            selected_option(&root.join("enabled")).as_deref(),
            Some("madvise")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Promotion is two different events with one outcome, and both are
    /// carried: a page allocated on a fault is the program asking, a page
    /// collapsed by `khugepaged` is the kernel deciding.
    #[test]
    fn promotions_are_faults_and_collapses_and_both_are_reported_apart() {
        let root = tree("promotions");
        write_stats(&root, 100, 10);
        let mut collector = Thp::new(
            Source::Sys,
            poll(),
            2 * 1024 * 1024,
            std::time::Duration::ZERO,
        )
        .rooted(root.clone(), PathBuf::from(VMSTAT));
        assert!(collector.sample(0).expect("the first read").is_empty());
        write_stats(&root, 132, 14);
        let events = collector.sample(1).expect("a second read");

        let event = &events[0];
        assert_eq!(event.attr("promotions").unwrap().as_f64(), Some(36.0));
        assert_eq!(event.attr("fault_alloc").unwrap().as_f64(), Some(32.0));
        assert_eq!(event.attr("collapse_alloc").unwrap().as_f64(), Some(4.0));
        assert_eq!(
            event.attr("promoted_bytes").unwrap().as_f64(),
            Some(36.0 * 2.0 * 1024.0 * 1024.0)
        );
        assert_eq!(event.attr("thp_mode").unwrap().to_string(), "madvise");
        std::fs::remove_dir_all(&root).ok();
    }

    /// A counter this kernel does not publish is absent, not zero: reporting
    /// zero splits on a kernel that counts no splits is a measurement nobody
    /// took.
    #[test]
    fn a_counter_this_kernel_does_not_publish_is_absent_rather_than_zero() {
        let root = tree("absent");
        write_stats(&root, 1, 1);
        let vmstat = root.join("vmstat");
        std::fs::write(&vmstat, "thp_fault_alloc 1\n").unwrap();
        let mut collector = Thp::new(
            Source::Sys,
            Mode::Snapshot,
            2 * 1024 * 1024,
            std::time::Duration::ZERO,
        )
        .rooted(root.clone(), vmstat);
        let events = collector.sample(0).expect("one read");
        assert!(events[0].attr("split").is_none(), "{:?}", events[0]);
        assert!(events[0].attr("resident_huge_pages").is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    /// The `fs:proc` backend is declared as degraded because it cannot see the
    /// configured mode. It must not report one.
    #[test]
    fn the_vmstat_backend_reports_no_mode_because_it_cannot_see_one() {
        let mut collector = Thp::new(Source::Proc, Mode::Snapshot, 0, std::time::Duration::ZERO);
        let Ok(events) = collector.sample(0) else {
            eprintln!("skipped: this kernel has no transparent huge pages");
            return;
        };
        assert!(events[0].attr("thp_mode").is_none(), "{:?}", events[0]);
        assert!(events[0].attr("promotions_since_boot").is_some());
    }

    #[test]
    fn a_process_target_is_refused_rather_than_answered_machine_wide() {
        let refused = Box::new(Prepare::new(Source::Sys, Mode::Snapshot).expect("snapshot works"))
            .attach(&Target::Pid(vec![1]))
            .err()
            .expect("there is no per-process promotion counter");
        assert!(refused.contains("attribution nobody measured"), "{refused}");
    }

    /// Ground truth: huge pages are *caused* and then counted, twice over.
    ///
    /// A 2 MiB-aligned mapping is advised for huge pages and every huge page of
    /// it is touched, which is exactly what the kernel counts as an anonymous
    /// fault allocation. Two independent accountings then have to agree with
    /// what the test did:
    ///
    /// - the machine-wide counter has to rise by at least the number of huge
    ///   pages touched. At least, not exactly: it is the machine's counter and
    ///   every other process on it may promote during the same window.
    /// - the process's own `AnonHugePages:` in `smaps_rollup` has to rise by
    ///   about the size of the mapping, which is the same fact counted per
    ///   process by a different subsystem and cannot be inflated by anybody
    ///   else.
    #[test]
    fn a_mapping_advised_for_huge_pages_is_counted_as_promoted() {
        const HUGE_PAGES: u64 = 16;

        let root = PathBuf::from(SYSFS);
        let Some(hpage) = pmd_size(&root) else {
            eprintln!("skipped: this kernel has no transparent huge pages");
            return;
        };
        if !stats_directory(&root, hpage).exists() {
            eprintln!("skipped: this kernel publishes no per-size huge page statistics");
            return;
        }
        if selected_option(&root.join("enabled")).as_deref() == Some("never") {
            eprintln!("skipped: transparent huge pages are disabled on this machine");
            return;
        }

        let mut collector = Thp::new(Source::Sys, poll(), hpage, std::time::Duration::ZERO);
        collector.sample(0).expect("the baseline read");

        let before_anon = anon_huge_bytes();
        let size = (HUGE_PAGES * hpage) as usize;
        let span = size + hpage as usize;
        // SAFETY: an anonymous private mapping of our own, unmapped below.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                span,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED, "the mapping must succeed");
        let aligned = (base as usize).next_multiple_of(hpage as usize) as *mut libc::c_void;
        // SAFETY: advising and writing inside the mapping just made.
        unsafe {
            libc::madvise(aligned, size, libc::MADV_HUGEPAGE);
            // One byte per huge page is a fault per huge page.
            for offset in (0..size).step_by(hpage as usize) {
                aligned.cast::<u8>().add(offset).write_volatile(0x5A);
            }
        }

        let after_anon = anon_huge_bytes();
        let events = collector.sample(1).expect("a second read");
        // SAFETY: unmapping what was mapped above.
        unsafe { libc::munmap(base, span) };

        let promoted = events[0].attr("promotions").unwrap().as_f64().unwrap();
        let (Some(before_anon), Some(after_anon)) = (before_anon, after_anon) else {
            eprintln!("skipped: this kernel publishes no AnonHugePages to cross-check against");
            return;
        };
        let grew = after_anon.saturating_sub(before_anon);
        // The kernel may decline to promote under memory pressure, and a
        // machine that could not give this test huge pages has not served the
        // request rather than failed it. But if it did give them, the counter
        // this source reads has to have seen them.
        if grew < HUGE_PAGES * hpage / 2 {
            eprintln!(
                "skipped: the kernel promoted only {grew} bytes of the {size} advised, so there \
                 is nothing to check the counter against"
            );
            return;
        }
        let expected = (grew / hpage) as f64;
        assert!(
            promoted >= expected,
            "the process gained {grew} bytes of anonymous huge pages, which is {expected} huge \
             pages, and the machine's counter rose by only {promoted}"
        );
    }

    /// The process's own huge page residency, which the kernel maintains
    /// independently of the promotion counters.
    fn anon_huge_bytes() -> Option<u64> {
        let text = std::fs::read_to_string("/proc/self/smaps_rollup").ok()?;
        text.lines()
            .find_map(|line| line.strip_prefix("AnonHugePages:"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|kilobytes| kilobytes * 1024)
    }
}
