//! `sched.mutex` over glibc's own USDT probes: the backend that reaches a
//! process already running.
//!
//! The preload shim is cheaper -- about 132 ns per event against a uprobe's
//! 740 ns (finding 6) -- and it reaches a program only at exec. A service that
//! is already up can only be read from outside, and glibc publishes the three
//! points that make a mutex wait measurable from outside:
//!
//! ```text
//!   pthread_mutex_lock ── libc:mutex_entry(mutex) ── ... ── libc:mutex_acquired(mutex)
//!   pthread_mutex_unlock ─ libc:mutex_release(mutex)
//!   pthread_cond_wait ─── libc:cond_wait(cond, mutex) ── (releases the mutex) ──
//! ```
//!
//! **One event per acquisition, the same event the shim writes.** Stamped when
//! the thread asked for the mutex, with the time until it had it as the value
//! (`wait_ns`) and the time until it let go beside it (`hold_ns`), published at
//! the release, which is the first moment both are known. The pairing is per
//! thread and per mutex, in userspace: a thread's hits reach the ring in the
//! order it made them.
//!
//! **What it cannot see, and says.** `pthread_mutex_trylock` has no probe, so a
//! successful trylock is not an event and its release arrives with no
//! acquisition before it; the reacquisition inside `pthread_cond_wait` has an
//! `acquired` with no `entry`, and is not an event either, as the catalog's
//! caveat says of the shim. Both are counted and named in the notes. A mutex
//! taken before the probes were attached is released without having been seen
//! taken, and is counted the same way.
//!
//! Three traps per lock-and-unlock is the price, and on a hot lock it
//! serialises what it measures: the catalog's degradation says so, and the
//! notes count the hits the run paid for.

use std::collections::HashMap;
use std::time::Duration as StdDuration;

use crate::bpf::usdt::{Hit, Shape, Site, UsdtProgram};
use crate::collect::runtime::objects;
use crate::collect::task::Comms;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Emit, Target};

/// A lock and an unlock are microseconds apart on a busy program, so the ring
/// is drained often and sized for a burst.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);
const RING_BYTES: usize = 4 << 20;

/// The probes this backend reads, all published by glibc under `libc`.
const PROVIDER: &str = "libc";
const ENTRY: &str = "mutex_entry";
const TIMED_ENTRY: &str = "mutex_timedlock_entry";
const ACQUIRED: &str = "mutex_acquired";
const RELEASE: &str = "mutex_release";
const COND_WAIT: &str = "cond_wait";

/// Which of the probes a site is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Point {
    Entry { timed: bool },
    Acquired,
    Release,
    CondWait,
}

impl Point {
    fn of(site: &Site) -> Option<Self> {
        if site.provider != PROVIDER {
            return None;
        }
        Some(match site.name.as_str() {
            ENTRY => Point::Entry { timed: false },
            TIMED_ENTRY => Point::Entry { timed: true },
            ACQUIRED => Point::Acquired,
            RELEASE => Point::Release,
            COND_WAIT => Point::CondWait,
            _ => return None,
        })
    }

    /// Which argument holds the mutex: the first everywhere but in
    /// `cond_wait(cond, mutex)`.
    fn mutex_argument(self) -> usize {
        match self {
            Point::CondWait => 1,
            _ => 0,
        }
    }
}

pub struct Prepare {
    pub target: Target,
    pub filter: Option<FilterExpr>,
    pub emits: Vec<Emit>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.emits.contains(&Emit::Stack) {
            return Err(crate::collect::not_built(
                "`sched.mutex` over USDT carries no stacks in this build: the probe program \
                 hands up the mutex and the thread, not a stack. Drop `stack`, or ask \
                 `sched.futex`, whose stacks show where a contended mutex blocked",
            ));
        }
        let pids = match target {
            Target::Pid(pids) | Target::Tid(pids) => pids.clone(),
            other => {
                return Err(format!(
                    "`sched.mutex` over USDT puts its probes in one process, and a {} target \
                     names none",
                    other.kind()
                ))
            }
        };
        let [pid] = pids[..] else {
            return Err(format!(
                "`sched.mutex` over USDT follows one process per run in this build, and this \
                 target names {}",
                pids.len()
            ));
        };
        // A thread target is served by its process's probes, kept to the
        // thread's own hits: the probes are in a library every thread shares.
        let (process, only_tid) = match &self.target {
            Target::Tid(_) => (
                crate::collect::task::tgid_of(pid)
                    .ok_or_else(|| format!("thread {pid} does not exist"))?,
                Some(pid as u32),
            ),
            _ => (pid, None),
        };
        let objects = match &self.target {
            Target::Launch(argv) => objects::of_launch(argv)?,
            _ => objects::of_process(process)?,
        };
        let sites: Vec<Site> = objects
            .sites()
            .into_iter()
            .filter(|site| Point::of(site).is_some())
            .collect();
        for needed in [ENTRY, ACQUIRED, RELEASE] {
            if !sites.iter().any(|site| site.name == needed) {
                return Err(format!(
                    "`{PROVIDER}:{needed}` is not among the probes this process carries, so a \
                     mutex wait cannot be timed from outside it. Glibc publishes its mutex \
                     probes only when built with `--enable-systemtap`, and a program that does \
                     not use glibc's pthreads has none; the preload backend needs no probe, and \
                     a program the tool launches. What was read: {}",
                    objects.inventory()
                ));
            }
        }
        Ok(Box::new(MutexUsdtCollector::new(
            process,
            only_tid,
            sites,
            self.filter,
        )))
    }
}

/// A mutex a thread is waiting for or holding, by thread and address.
type Slot = (u32, u64);

pub struct MutexUsdtCollector {
    id: CollectorId,
    pid: i32,
    only_tid: Option<u32>,
    sites: Vec<Site>,
    /// The resolver reports a `bpf` backend's filter as pushed down, so this
    /// collector is the only thing that applies it.
    filter: Option<FilterExpr>,
    program: Option<UsdtProgram>,
    fallback: Option<String>,
    comms: Comms,
    losses: Losses,
    dropped: u64,
    /// Asked for and not had yet: when, and whether through the timed lock.
    waiting: HashMap<Slot, (u64, bool)>,
    /// Had and not let go yet: when it was asked for, when it was had, how.
    /// `None` for the ask when the acquisition had no entry before it.
    held: HashMap<Slot, (Option<u64>, u64, bool)>,
    /// A `cond_wait` a thread entered and whose release has not been seen.
    in_cond_wait: HashMap<u32, u64>,
    hits: u64,
    unreadable: u64,
    /// Acquisitions with no entry before them: the reacquisition inside a
    /// condition wait, or a lock asked for before the probes were attached.
    unentered: u64,
    /// Releases of a mutex never seen taken: a successful trylock, which has
    /// no probe, or one taken before the probes were attached.
    unheld: u64,
    filtered_out: u64,
}

impl MutexUsdtCollector {
    fn new(pid: i32, only_tid: Option<u32>, sites: Vec<Site>, filter: Option<FilterExpr>) -> Self {
        Self {
            id: CollectorId::new("sched.mutex/bpf:usdt"),
            pid,
            only_tid,
            sites,
            filter,
            program: None,
            fallback: None,
            comms: Comms::new(),
            losses: Losses::default(),
            dropped: 0,
            waiting: HashMap::new(),
            held: HashMap::new(),
            in_cond_wait: HashMap::new(),
            hits: 0,
            unreadable: 0,
            unentered: 0,
            unheld: 0,
            filtered_out: 0,
        }
    }

    fn drain(&mut self, seq: u64) -> Vec<Event> {
        let Some(program) = self.program.as_mut() else {
            return Vec::new();
        };
        let mut hits = Vec::new();
        let mut undecoded = 0;
        program.drain(|hit| hits.push(hit), &mut undecoded);
        let dropped = program.dropped();
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the USDT ring was shorter than one hit");
        }
        if dropped > self.dropped {
            self.losses.dropped += dropped - self.dropped;
            self.losses.note(
                "the USDT ring was full: mutex probes fired faster than they were drained, so \
                 some acquisitions were never paired and are in no number here",
            );
            self.dropped = dropped;
        }

        let mut events = Vec::new();
        let mut index = seq;
        for hit in hits {
            if let Some(event) = self.step(&hit, index) {
                if let Some(filter) = &self.filter {
                    if !crate::filter::matches(filter, &event) {
                        self.filtered_out += 1;
                        continue;
                    }
                }
                events.push(event);
                index += 1;
            }
        }
        events
    }

    /// Advances one thread's state by one hit, and returns the event it
    /// completes, if it completes one.
    fn step(&mut self, hit: &Hit, seq: u64) -> Option<Event> {
        if self.only_tid.is_some_and(|tid| tid != hit.tid) {
            return None;
        }
        let point = Point::of(self.sites.get(hit.site as usize)?)?;
        self.hits += 1;
        let argument = point.mutex_argument();
        if hit.unread & (1 << argument) != 0 {
            self.unreadable += 1;
            return None;
        }
        let slot = (hit.tid, hit.args[argument]);
        match point {
            Point::Entry { timed } => {
                self.waiting.insert(slot, (hit.ts_ns, timed));
                None
            }
            Point::Acquired => {
                match self.waiting.remove(&slot) {
                    Some((asked, timed)) => {
                        self.held.insert(slot, (Some(asked), hit.ts_ns, timed));
                    }
                    None => {
                        self.unentered += 1;
                        self.held.insert(slot, (None, hit.ts_ns, false));
                    }
                }
                None
            }
            Point::CondWait => {
                self.in_cond_wait.insert(hit.tid, slot.1);
                None
            }
            Point::Release => {
                let by_cond_wait = self.in_cond_wait.get(&hit.tid) == Some(&slot.1);
                if by_cond_wait {
                    self.in_cond_wait.remove(&hit.tid);
                }
                let Some((asked, had, timed)) = self.held.remove(&slot) else {
                    self.unheld += 1;
                    return None;
                };
                let asked = asked?;
                Some(self.event(
                    slot,
                    asked,
                    had,
                    Some(hit.ts_ns),
                    timed,
                    if by_cond_wait { "cond_wait" } else { "unlock" },
                    seq,
                ))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn event(
        &mut self,
        (tid, mutex): Slot,
        asked: u64,
        had: u64,
        released: Option<u64>,
        timed: bool,
        released_by: &str,
        seq: u64,
    ) -> Event {
        let mut attrs = vec![
            Attr::uint("wait_ns", had.saturating_sub(asked)),
            Attr::uint("addr", mutex),
            Attr::uint("tid", u64::from(tid)),
            Attr::uint("pid", self.pid as u64),
            Attr::text("how", if timed { "timedlock" } else { "lock" }),
            Attr::text("released_by", released_by),
        ];
        // Absent rather than zero for a hold the run ended inside: a zero
        // would be a measurement of an instant release.
        if let Some(released) = released {
            attrs.push(Attr::uint("hold_ns", released.saturating_sub(had)));
        }
        if let Some(comm) = self.comms.get(tid as i32) {
            attrs.push(Attr::text("comm", comm));
        }
        Event {
            ts_ns: asked,
            ts_err_ns: 0,
            kind: "sched.mutex".to_owned(),
            keys: vec![KeyValue::task(tid as i32)],
            attrs,
            collector_id: self.id.clone(),
            seq,
        }
    }
}

impl Collector for MutexUsdtCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "wait_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        let program = UsdtProgram::load(&self.sites, self.pid, Shape::Stream, RING_BYTES)?;
        self.fallback = program.fallback.clone();
        self.program = Some(program);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        Ok(self.drain(seq))
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = self.drain(u64::MAX / 2);
        self.program = None;
        // A mutex still held when the run ends was acquired inside it and is
        // an acquisition like any other; only its hold is unknown.
        let mut held: Vec<_> = std::mem::take(&mut self.held).into_iter().collect();
        held.sort_by_key(|(_, (_, had, _))| *had);
        for (index, (slot, (asked, had, timed))) in held.into_iter().enumerate() {
            let Some(asked) = asked else { continue };
            let event = self.event(
                slot,
                asked,
                had,
                None,
                timed,
                "still_held",
                u64::MAX / 2 + 1 + index as u64,
            );
            if self
                .filter
                .as_ref()
                .is_none_or(|filter| crate::filter::matches(filter, &event))
            {
                trailing.push(event);
            }
        }
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
        let libraries: std::collections::BTreeSet<String> = self
            .sites
            .iter()
            .map(|site| site.object.display().to_string())
            .collect();
        notes.push(format!(
            "{} probe sites in {}, {} hits paid for at a uprobe's cost each",
            self.sites.len(),
            libraries.into_iter().collect::<Vec<_>>().join(", "),
            self.hits
        ));
        if self.unentered > 0 {
            notes.push(format!(
                "{} acquisitions had no entry before them and are not events: the \
                 reacquisition inside a condition wait, or a lock asked for before the probes \
                 were attached",
                self.unentered
            ));
        }
        if self.unheld > 0 {
            notes.push(format!(
                "{} releases were of a mutex never seen taken: a successful trylock, which has \
                 no probe, or a lock taken before the probes were attached",
                self.unheld
            ));
        }
        if !self.waiting.is_empty() {
            notes.push(format!(
                "{} threads were still waiting for a mutex when the run ended, and those waits \
                 are in no number here",
                self.waiting.len()
            ));
        }
        if self.unreadable > 0 {
            notes.push(format!(
                "{} hits carried a mutex address that could not be read and were not paired",
                self.unreadable
            ));
        }
        if self.filtered_out > 0 {
            notes.push(format!(
                "{} acquisitions did not match the filter",
                self.filtered_out
            ));
        }
        if let Some(fallback) = &self.fallback {
            notes.push(fallback.clone());
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bpf::usdt::MAX_ARGS;

    fn site(name: &str) -> Site {
        Site {
            object: "/usr/lib/libc.so.6".into(),
            provider: PROVIDER.to_owned(),
            name: name.to_owned(),
            offset: 0,
            semaphore: None,
            args: Vec::new(),
        }
    }

    fn collector() -> MutexUsdtCollector {
        let sites = [ENTRY, ACQUIRED, RELEASE, COND_WAIT, TIMED_ENTRY]
            .into_iter()
            .map(site)
            .collect();
        MutexUsdtCollector::new(1, None, sites, None)
    }

    fn hit(site: u32, tid: u32, ts_ns: u64, args: &[u64]) -> Hit {
        let mut all = [0u64; MAX_ARGS];
        all[..args.len()].copy_from_slice(args);
        Hit {
            ts_ns,
            tid,
            tgid: 1,
            site,
            unread: 0,
            comm: String::new(),
            args: all,
        }
    }

    const E: u32 = 0;
    const A: u32 = 1;
    const R: u32 = 2;
    const C: u32 = 3;
    const T: u32 = 4;

    fn attr(event: &Event, name: &str) -> Option<String> {
        event.attr(name).map(ToString::to_string)
    }

    #[test]
    fn an_acquisition_is_stamped_when_asked_for_and_carries_its_wait_and_its_hold() {
        let mut collector = collector();
        assert!(collector.step(&hit(E, 7, 1_000, &[0xaa]), 0).is_none());
        assert!(collector.step(&hit(A, 7, 1_400, &[0xaa]), 0).is_none());
        let event = collector
            .step(&hit(R, 7, 2_400, &[0xaa]), 0)
            .expect("an event");
        assert_eq!(event.ts_ns, 1_000);
        assert_eq!(attr(&event, "wait_ns").as_deref(), Some("400"));
        assert_eq!(attr(&event, "hold_ns").as_deref(), Some("1000"));
        assert_eq!(attr(&event, "how").as_deref(), Some("lock"));
        assert_eq!(attr(&event, "released_by").as_deref(), Some("unlock"));
        assert_eq!(event.keys, vec![KeyValue::task(7)]);
    }

    /// Two threads and two mutexes interleaved: each pair is its own.
    #[test]
    fn threads_and_mutexes_are_paired_separately() {
        let mut collector = collector();
        collector.step(&hit(E, 1, 100, &[0xa]), 0);
        collector.step(&hit(E, 2, 110, &[0xa]), 0);
        collector.step(&hit(A, 1, 120, &[0xa]), 0);
        let first = collector
            .step(&hit(R, 1, 200, &[0xa]), 0)
            .expect("thread 1");
        collector.step(&hit(A, 2, 210, &[0xa]), 0);
        let second = collector
            .step(&hit(R, 2, 260, &[0xa]), 0)
            .expect("thread 2");
        assert_eq!(attr(&first, "wait_ns").as_deref(), Some("20"));
        assert_eq!(attr(&second, "wait_ns").as_deref(), Some("100"));
        assert_eq!(attr(&second, "hold_ns").as_deref(), Some("50"));
    }

    /// The release inside `pthread_cond_wait` ends the hold as a condition
    /// wait, and the reacquisition it makes is not an event.
    #[test]
    fn a_condition_wait_ends_the_hold_and_its_reacquisition_is_not_an_event() {
        let mut collector = collector();
        collector.step(&hit(E, 3, 0, &[0xb]), 0);
        collector.step(&hit(A, 3, 5, &[0xb]), 0);
        collector.step(&hit(C, 3, 50, &[0xc0, 0xb]), 0);
        let event = collector
            .step(&hit(R, 3, 60, &[0xb]), 0)
            .expect("the hold ends");
        assert_eq!(attr(&event, "released_by").as_deref(), Some("cond_wait"));
        assert!(collector.step(&hit(A, 3, 900, &[0xb]), 0).is_none());
        assert!(collector.step(&hit(R, 3, 950, &[0xb]), 0).is_none());
        assert_eq!(collector.unentered, 1);
    }

    /// A trylock has no probe: its release is counted, not invented into a
    /// wait of zero.
    #[test]
    fn a_release_never_seen_taken_is_counted_and_not_an_event() {
        let mut collector = collector();
        assert!(collector.step(&hit(R, 4, 10, &[0xd]), 0).is_none());
        assert_eq!(collector.unheld, 1);
        assert!(collector
            .notes()
            .iter()
            .any(|note| note.contains("trylock")));
    }

    #[test]
    fn a_timed_lock_says_so() {
        let mut collector = collector();
        collector.step(&hit(T, 5, 0, &[0xe]), 0);
        collector.step(&hit(A, 5, 30, &[0xe]), 0);
        let event = collector.step(&hit(R, 5, 40, &[0xe]), 0).expect("an event");
        assert_eq!(attr(&event, "how").as_deref(), Some("timedlock"));
    }

    /// A hold the run ended inside is an acquisition with no hold, not a hold
    /// of zero.
    #[test]
    fn a_mutex_still_held_at_the_end_has_no_hold() {
        let mut collector = collector();
        collector.step(&hit(E, 6, 0, &[0xf]), 0);
        collector.step(&hit(A, 6, 10, &[0xf]), 0);
        let trailing = collector.stop();
        assert_eq!(trailing.len(), 1);
        assert_eq!(
            attr(&trailing[0], "released_by").as_deref(),
            Some("still_held")
        );
        assert!(trailing[0].attr("hold_ns").is_none());
        assert_eq!(attr(&trailing[0], "wait_ns").as_deref(), Some("10"));
    }

    #[test]
    fn an_address_that_could_not_be_read_is_not_paired() {
        let mut collector = collector();
        let mut unread = hit(E, 8, 0, &[0]);
        unread.unread = 1;
        assert!(collector.step(&unread, 0).is_none());
        assert_eq!(collector.unreadable, 1);
        assert!(collector.waiting.is_empty());
    }

    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let mut collector = collector();
        collector.step(&hit(E, 9, 0, &[0x1]), 0);
        collector.step(&hit(A, 9, 1, &[0x1]), 0);
        let event = collector.step(&hit(R, 9, 2, &[0x1]), 0).expect("an event");
        let catalog = crate::Catalog::embedded().expect("the catalog");
        let entry = catalog
            .entries()
            .iter()
            .find(|entry| entry.id.as_str() == "sched.mutex")
            .expect("sched.mutex");
        for field in &entry.filter_fields {
            // `comm` is read from /proc and there is no thread 9 to read.
            if field == "comm" {
                continue;
            }
            assert!(event.attr(field).is_some(), "`{field}` is not on the event");
        }
    }
}
