//! `net.request`: one event per request a server declared, by state.
//!
//! `src/bpf/request.rs` holds the programs and the reasoning. What this file
//! decides is what a program has to declare, which probes are the request's,
//! and what the event says: the request is the series, by the id the server
//! chose, and the four states its serving task was in add up to its life.
//!
//! **Why the event is published at the end.** The states are only known once
//! the request is over, and a request's rows in a decomposition are read off
//! one event per request. The birth is seen too -- the kernel publishes it --
//! and is used here to say which requests began and never ended, rather than
//! to make an event of its own that half of every emit would have to skip.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use crate::bpf::usdt::{self, Site};
use crate::bpf::{request_key, RequestCounts, RequestProgram, RequestRecord};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// The probes a program declares its requests with.
pub const START: &str = "request__start";
pub const DONE: &str = "request__done";

const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);
/// One record per request and per birth, 128 bytes with its header.
const RING_BYTES: usize = 256 * 1024;

pub struct Prepare {
    pub mode: Mode,
    /// The request's own target: at attach time a launched program has not
    /// execed, and its pid says nothing about what it will run.
    pub target: Target,
    pub filter: Option<FilterExpr>,
}

/// The executable a target runs, which is where its probes are looked for.
fn executable(target: &Target, pid: i32) -> Result<PathBuf, String> {
    match target {
        Target::Launch(argv) => {
            let program = argv
                .first()
                .ok_or("a launch with no program names nothing to put a probe in")?;
            if program.contains('/') {
                return Ok(PathBuf::from(program));
            }
            std::env::var_os("PATH")
                .into_iter()
                .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
                .map(|dir| dir.join(program))
                .find(|candidate| candidate.is_file())
                .ok_or_else(|| format!("`{program}` is not on the PATH"))
        }
        _ => std::fs::read_link(format!("/proc/{pid}/exe"))
            .map_err(|error| format!("cannot read what process {pid} runs: {error}")),
    }
}

/// The start and done sites an object declares, or a refusal that names what
/// it does declare.
fn request_sites(path: &Path) -> Result<(Vec<Site>, Vec<Site>), String> {
    let object = usdt::Object::read(path)?;
    let sites = usdt::sites(&object)?;
    let named = |name: &str| -> Vec<Site> {
        sites
            .iter()
            .filter(|site| site.name == name)
            .cloned()
            .collect()
    };
    let (starts, ends) = (named(START), named(DONE));
    if starts.is_empty() || ends.is_empty() {
        let carried: Vec<String> = sites.iter().map(Site::qualified).collect();
        return Err(format!(
            "{} declares no request: `net.request` follows a request from a `{START}(id)` probe \
             to a `{DONE}(id)` probe the program fires itself, any provider, and this one {}. \
             Nothing is inferred from the connection instead -- see `docs/scenarios/vertical.md`",
            path.display(),
            if carried.is_empty() {
                "carries no USDT probe at all".to_owned()
            } else {
                format!("carries {}", carried.join(", "))
            }
        ));
    }
    Ok((starts, ends))
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot follow a request: its life is the gap between the server's two \
                 probes, and its states are only known at the end",
                self.mode
            ));
        }
        let pid = match target {
            Target::Pid(pids) => match pids[..] {
                [pid] => pid,
                _ => {
                    return Err(format!(
                        "`net.request` follows one process per run, and this target names {}",
                        pids.len()
                    ))
                }
            },
            other => {
                return Err(format!(
                    "`net.request` puts its probes in one process, and a {} target names none",
                    other.kind()
                ))
            }
        };
        let path = executable(&self.target, pid)?;
        let (starts, ends) = request_sites(&path)?;
        Ok(Box::new(RequestCollector {
            id: CollectorId::new("net.request/bpf:usdt"),
            pid,
            starts,
            ends,
            program: None,
            losses: Losses::default(),
            predicate: self.filter,
            counts: RequestCounts::default(),
            skipped: 0,
            open: BTreeMap::new(),
            ended: 0,
            mismatched: 0,
            anomalies: 0,
            filtered_out: 0,
            object: path,
        }))
    }
}

pub struct RequestCollector {
    id: CollectorId,
    pid: i32,
    starts: Vec<Site>,
    ends: Vec<Site>,
    object: PathBuf,
    program: Option<RequestProgram>,
    losses: Losses,
    predicate: Option<FilterExpr>,
    counts: RequestCounts,
    skipped: u64,
    /// Requests born and not yet ended, by id, with the thread and the birth.
    open: BTreeMap<u64, (u32, u64)>,
    ended: u64,
    mismatched: u64,
    anomalies: u64,
    filtered_out: u64,
}

/// The event for one ended request.
fn event(id: &CollectorId, record: &RequestRecord, seq: u64) -> Event {
    let total = record.done_ns.saturating_sub(record.born_ns);
    let mut attrs = vec![
        Attr::uint("total_ns", total),
        Attr::uint("oncpu_ns", record.oncpu_ns),
        Attr::uint("runq_ns", record.runq_ns),
        Attr::uint("sleep_ns", record.sleep_ns),
        Attr::uint("disk_ns", record.disk_ns),
        Attr::uint("born_ns", record.born_ns),
        Attr::uint("blocks", u64::from(record.blocks)),
        Attr::uint("preempted", u64::from(record.preempted)),
        Attr::uint("holder_ns", record.holder_ns),
        Attr::uint("holder_tid", u64::from(record.holder_tid)),
        Attr::uint("tid", u64::from(record.tid)),
        Attr::uint("pid", u64::from(record.tgid)),
        Attr::text("comm", record.comm()),
        Attr::uint("count", 1),
    ];
    if record.anomalies > 0 {
        // Stretches whose end the programs could not place: their time is in
        // no state, so the four do not add up to the total by that much.
        attrs.push(Attr::uint("anomalies", u64::from(record.anomalies)));
    }
    if record.flags & crate::bpf::ID_MISMATCH != 0 {
        attrs.push(Attr::text("declared", request_key(record.declared_id)));
    }
    Event {
        ts_ns: record.done_ns,
        ts_err_ns: 0,
        kind: "net.request".to_owned(),
        keys: vec![
            KeyValue::Req {
                id: request_key(record.id),
            },
            KeyValue::task(record.tid as i32),
        ],
        attrs,
        collector_id: id.clone(),
        seq,
    }
}

impl RequestCollector {
    fn drain_into(&mut self, events: &mut Vec<Event>, seq: u64) {
        let Some(program) = self.program.as_mut() else {
            return;
        };
        let mut records = Vec::new();
        let mut undecoded = 0;
        program.drain(|record| records.push(record), &mut undecoded);
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses
                .note("a record in the request ring was shorter than one request");
        }
        for record in records {
            if record.is_birth() {
                self.open.insert(record.id, (record.tid, record.born_ns));
                continue;
            }
            self.open.remove(&record.id);
            self.ended += 1;
            if record.flags & crate::bpf::ID_MISMATCH != 0 {
                self.mismatched += 1;
            }
            self.anomalies += u64::from(record.anomalies);
            let event = event(&self.id, &record, seq + events.len() as u64);
            if let Some(predicate) = &self.predicate {
                if !crate::filter::matches(predicate, &event) {
                    self.filtered_out += 1;
                    continue;
                }
            }
            events.push(event);
        }
    }

    fn absorb_counters(&mut self) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let counts = program.counts();
        let grew = |now: u64, before: u64| now.saturating_sub(before);
        let ring = grew(counts.ring_full, self.counts.ring_full);
        if ring > 0 {
            self.losses.dropped += ring;
            self.losses
                .note("the request ring was full: requests ended faster than they were drained");
        }
        let storage = grew(counts.no_storage, self.counts.no_storage);
        if storage > 0 {
            self.losses.dropped += storage;
            self.losses.note(
                "a request was born in a task the kernel would not give storage to, so it was \
                 never followed",
            );
        }
        let sleepers = grew(counts.sleepers_full, self.counts.sleepers_full);
        if sleepers > 0 {
            self.losses.dropped += sleepers;
            self.losses.note(
                "more serving tasks were asleep at once than the sleeper map holds, so a \
                 sleep's wakeup was not seen and the wait after it is counted asleep",
            );
        }
        self.counts = counts;
        let skipped = program.skipped();
        if skipped > self.skipped {
            self.losses.dropped += skipped - self.skipped;
            self.losses.note(
                "the kernel skipped a run of these programs on a cpu where one was already \
                 running: a switch nobody saw is a stretch whose state is unknown",
            );
            self.skipped = skipped;
        }
    }
}

impl Collector for RequestCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "total_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(RequestProgram::load(
            &self.starts,
            &self.ends,
            self.pid,
            RING_BYTES,
        )?);
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
        let mut notes = vec![format!(
            "{} request{} born and {} ended in {}, followed by the id the program declared at \
             `{START}` and carried in the serving task's own storage until `{DONE}`; a block \
             request submitted while serving one carries the id as `req:<id>` in `io.latency`",
            self.counts.births,
            if self.counts.births == 1 {
                " was"
            } else {
                "s were"
            },
            self.ended,
            self.object.display(),
        )];
        if !self.open.is_empty() {
            let named: Vec<String> = self
                .open
                .iter()
                .take(5)
                .map(|(id, (tid, _))| format!("req:{} in task {tid}", request_key(*id)))
                .collect();
            notes.push(format!(
                "{} request{} never ended in the run ({}{}); a request with no end has no total \
                 and is in no event",
                self.open.len(),
                if self.open.len() == 1 { "" } else { "s" },
                named.join(", "),
                if self.open.len() > named.len() {
                    ", ..."
                } else {
                    ""
                },
            ));
        }
        if self.counts.unborn > 0 {
            notes.push(format!(
                "{} `{DONE}` hit{} a task carrying no request: begun before the probes \
                 attached, or never declared at `{START}`",
                self.counts.unborn,
                if self.counts.unborn == 1 { "" } else { "s" },
            ));
        }
        if self.counts.restarted > 0 {
            notes.push(format!(
                "{} `{START}` hit{} a task still carrying a request that never ended; the old one \
                 was replaced and is among those that never ended",
                self.counts.restarted,
                if self.counts.restarted == 1 { "" } else { "s" },
            ));
        }
        if self.mismatched > 0 {
            notes.push(format!(
                "{} request{} ended under a different id from the one {} born with; each event \
                 carries the id it was born with as the key and the other as `declared`",
                self.mismatched,
                if self.mismatched == 1 { "" } else { "s" },
                if self.mismatched == 1 {
                    "it was"
                } else {
                    "they were"
                },
            ));
        }
        if self.anomalies > 0 {
            notes.push(format!(
                "{} off-cpu stretch{} could not be placed -- the cpu switched without the \
                 programs seeing it -- so {} time is in no state and the states add up to less \
                 than the total",
                self.anomalies,
                if self.anomalies == 1 { "" } else { "es" },
                if self.anomalies == 1 { "its" } else { "their" },
            ));
        }
        if self.filtered_out > 0 {
            notes.push(format!(
                "the filter removed {} requests, here over the drained records rather than in \
                 the kernel",
                self.filtered_out
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(flags: u32) -> RequestRecord {
        let mut bytes = [0u8; 120];
        let put = |bytes: &mut [u8; 120], at: usize, value: u64| {
            bytes[at..at + 8].copy_from_slice(&value.to_ne_bytes())
        };
        put(&mut bytes, 0, 0xa3f1);
        put(&mut bytes, 8, 1_000_000);
        put(&mut bytes, 16, 43_000_000);
        put(&mut bytes, 24, 2_000_000);
        put(&mut bytes, 32, 500_000);
        put(&mut bytes, 40, 40_000_000);
        put(&mut bytes, 48, 0);
        bytes[64..68].copy_from_slice(&8820u32.to_ne_bytes());
        bytes[72..76].copy_from_slice(&8821u32.to_ne_bytes());
        bytes[84..88].copy_from_slice(&flags.to_ne_bytes());
        put(&mut bytes, 88, 0xbeef);
        RequestRecord::from_bytes(&bytes).expect("a full record")
    }

    /// The request is the series and its thread the second key; the total is
    /// the end less the birth, and the states are carried as the kernel added
    /// them, not rescaled to fit it.
    #[test]
    fn an_ended_request_is_keyed_by_its_id_and_carries_its_states() {
        let event = event(&CollectorId::new("t"), &record(0), 0);
        assert_eq!(event.keys[0].to_string(), "req:a3f1");
        assert_eq!(event.keys[1], KeyValue::task(8820));
        let value = |name: &str| event.attr(name).and_then(|value| value.as_f64());
        assert_eq!(value("total_ns"), Some(42_000_000.0));
        assert_eq!(value("sleep_ns"), Some(40_000_000.0));
        assert_eq!(value("holder_tid"), Some(8821.0));
        assert_eq!(
            event.ts_ns, 43_000_000,
            "an ended request is stamped at its end"
        );
        assert!(event.attr("declared").is_none());
    }

    /// An end under another id says so, rather than folding the request into
    /// either.
    #[test]
    fn an_end_under_another_id_carries_both() {
        let event = event(&CollectorId::new("t"), &record(crate::bpf::ID_MISMATCH), 0);
        assert_eq!(event.keys[0].to_string(), "req:a3f1");
        assert_eq!(
            event.attr("declared").map(ToString::to_string),
            Some("beef".to_owned())
        );
    }

    /// Every filter field the catalog declares is on the event.
    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let entry = catalog
            .get(&"net.request".parse().expect("an id"))
            .expect("net.request is in the catalog");
        let event = event(&CollectorId::new("t"), &record(0), 0);
        for field in &entry.filter_fields {
            assert!(
                event.attr(field).is_some(),
                "`{field}` is declared and absent"
            );
        }
    }

    /// A program with no request probes is refused with what it does carry,
    /// and the refusal names the two probes it would need.
    #[test]
    fn a_program_that_declares_no_request_is_refused_with_what_it_carries() {
        let error = request_sites(Path::new("/bin/sh")).expect_err("no request probes");
        assert!(error.contains(START) && error.contains(DONE), "{error}");
        assert!(error.contains("/bin/sh"), "{error}");
    }
}
