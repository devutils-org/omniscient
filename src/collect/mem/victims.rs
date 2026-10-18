//! `mem.oom` in trace mode: the victims, over `oom:mark_victim`.
//!
//! The counting backends (`oom.rs`) say how many tasks the kernel killed; this
//! one says which, with what each held when it was chosen. The program is
//! `src/bpf/mem_oom.rs`; what belongs here is what an event carries and what
//! is counted as lost.
//!
//! Sizes are the tracepoint's own, pages turned into bytes: `total_vm` is the
//! address space, `anon_rss`, `file_rss` and `shmem_rss` the resident pages by
//! kind (what the kernel log's "Killed process" line prints), and `swap` the
//! entries it had swapped out. `rss` is the three resident kinds added up,
//! which is what `ps` and the log call RSS.

use std::time::Duration as StdDuration;

use crate::bpf::{Victim, VictimProgram, Whose, NO_UID};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// A kill is rare: the drain is for a stop that comes soon after one.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// Eighty-eight bytes a victim: a few hundred kills between drains.
const RING_BYTES: usize = 64 * 1024;

pub struct Prepare {
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot name OOM victims: a kill is an event, recorded when the \
                 kernel picks it; count kills with `-m count` or `-m poll`, which the counting \
                 backends serve",
                self.mode
            ));
        }
        let whose = Whose::of(target).map_err(|reason| format!("`mem.oom`'s victims: {reason}"))?;
        Ok(Box::new(Victims {
            id: CollectorId::new("mem.oom/bpf:raw_tp"),
            whose,
            filter: self.filter,
            program: None,
            losses: Losses::default(),
            dropped: 0,
            skipped: 0,
            removed: 0,
            kills: 0,
        }))
    }
}

pub struct Victims {
    id: CollectorId,
    whose: Whose,
    filter: Option<FilterExpr>,
    program: Option<VictimProgram>,
    losses: Losses,
    dropped: u64,
    skipped: u64,
    /// Victims the predicate left out.
    removed: u64,
    kills: u64,
}

fn page_size() -> u64 {
    // SAFETY: `sysconf` takes an integer and returns one; no memory is touched.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as u64
    } else {
        4096
    }
}

/// What a victim event carries, sizes in bytes.
fn attributes(victim: &Victim, page: u64) -> Vec<Attr> {
    let bytes = |pages: u64| pages.saturating_mul(page);
    let mut attrs = vec![
        Attr::uint("count", 1),
        Attr::uint("pid", u64::from(victim.tgid)),
        Attr::uint("tid", u64::from(victim.tid)),
        Attr::text("comm", victim.comm()),
        Attr::int("oom_score_adj", i64::from(victim.oom_score_adj())),
        Attr::uint("total_vm", bytes(victim.total_vm)),
        Attr::uint(
            "rss",
            bytes(
                victim
                    .anon_rss
                    .saturating_add(victim.file_rss)
                    .saturating_add(victim.shmem_rss),
            ),
        ),
        Attr::uint("anon_rss", bytes(victim.anon_rss)),
        Attr::uint("file_rss", bytes(victim.file_rss)),
        Attr::uint("shmem_rss", bytes(victim.shmem_rss)),
        Attr::uint("swap", bytes(victim.swap_ents)),
        Attr::uint("cpu", u64::from(victim.cpu)),
    ];
    if victim.uid != NO_UID {
        attrs.push(Attr::uint("uid", u64::from(victim.uid)));
    }
    attrs
}

fn event(victim: &Victim, page: u64, id: &CollectorId, seq: u64) -> Event {
    Event {
        ts_ns: victim.ts_ns,
        ts_err_ns: 0,
        kind: "mem.oom".to_owned(),
        keys: vec![
            KeyValue::task(victim.tid as i32),
            KeyValue::comm(victim.comm()),
        ],
        attrs: attributes(victim, page),
        collector_id: id.clone(),
        seq,
    }
}

impl Victims {
    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_mut() else {
            return;
        };
        let page = page_size();
        let id = self.id.clone();
        let filter = &self.filter;
        let mut index = seq;
        let mut undecoded = 0;
        let mut removed = 0;
        let mut kills = 0;
        program.drain(
            |victim| {
                let event = event(&victim, page, &id, index);
                if filter
                    .as_ref()
                    .is_some_and(|expression| !crate::filter::matches(expression, &event))
                {
                    removed += 1;
                    return;
                }
                kills += 1;
                events.push(event);
                index += 1;
            },
            &mut undecoded,
        );
        self.removed += removed;
        self.kills += kills;
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the OOM victim ring was shorter than one victim".to_owned());
        }
        let (dropped, skipped) = (program.dropped(), program.skipped());
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note(
                "the kernel skipped a run of the OOM victim program because one was already \
                 running on that cpu, so a victim was never seen; the count is the machine's"
                    .to_owned(),
            );
            self.skipped = skipped;
        }
        if dropped > self.dropped {
            self.losses.dropped += dropped - self.dropped;
            self.losses
                .note("the OOM victim ring was full: victims were not drained in time".to_owned());
            self.dropped = dropped;
        }
    }
}

impl Collector for Victims {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "count"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(VictimProgram::load(self.whose, RING_BYTES)?);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let mut events = Vec::new();
        self.drain_into(&mut events, seq);
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = Vec::new();
        self.drain_into(&mut trailing, u64::MAX / 2);
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
        if let Some(expression) = &self.filter {
            notes.push(format!(
                "the predicate `{expression}` was applied here, over the drained victims: it \
                 removed {} and kept {}",
                self.removed, self.kills
            ));
        }
        if !VictimProgram::uid_is_passed() {
            notes.push(
                "this kernel's `mark_victim` does not pass the victim's uid, so its events carry \
                 none"
                    .to_owned(),
            );
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn victim() -> Victim {
        let mut bytes = [0u8; 88];
        bytes[8..12].copy_from_slice(&41u32.to_ne_bytes());
        bytes[12..16].copy_from_slice(&40u32.to_ne_bytes());
        bytes[16..20].copy_from_slice(&NO_UID.to_ne_bytes());
        bytes[24..27].copy_from_slice(b"hog");
        bytes[48..56].copy_from_slice(&10u64.to_ne_bytes());
        bytes[56..64].copy_from_slice(&100u64.to_ne_bytes());
        bytes[72..80].copy_from_slice(&1u64.to_ne_bytes());
        // `Victim::from_bytes` is the program's; the test builds the record the
        // way the ring hands it over.
        unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<Victim>()) }
    }

    #[test]
    fn a_victim_carries_its_sizes_in_bytes_and_no_uid_it_was_not_given() {
        let attrs = attributes(&victim(), 4096);
        let get = |name: &str| {
            attrs
                .iter()
                .find(|attr| attr.name == name)
                .map(|attr| &attr.value)
        };
        assert_eq!(get("rss"), Some(&crate::event::AttrValue::Uint(111 * 4096)));
        assert_eq!(
            get("anon_rss"),
            Some(&crate::event::AttrValue::Uint(100 * 4096))
        );
        assert_eq!(get("pid"), Some(&crate::event::AttrValue::Uint(40)));
        assert!(get("uid").is_none());
    }

    #[test]
    fn every_declared_filter_field_is_on_an_event() {
        let catalog = crate::catalog::Catalog::embedded().expect("the catalog");
        let entry = catalog
            .get(&"mem.oom".parse().expect("an id"))
            .expect("mem.oom");
        let backend = entry
            .backends
            .iter()
            .find(|backend| backend.id == "bpf:raw_tp")
            .expect("the victim backend");
        let attrs = attributes(
            &Victim {
                uid: 1000,
                ..victim()
            },
            4096,
        );
        for field in entry.backend_filter_fields(backend) {
            assert!(
                attrs.iter().any(|attr| attr.name == *field),
                "`{field}` is declared and not carried"
            );
        }
    }

    #[test]
    fn a_mode_on_a_clock_is_refused() {
        let prepared = Box::new(Prepare {
            mode: Mode::Count,
            filter: None,
        });
        let refused = prepared.attach(&Target::System).err().expect("refused");
        assert!(refused.contains("-m count"), "{refused}");
    }
}
