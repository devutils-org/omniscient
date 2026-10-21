//! `runtime.custom`, `runtime.gc` and `runtime.alloc` over USDT.
//!
//! One mechanism, three questions. `src/bpf/usdt.rs` reads the notes and
//! attaches the programs; this file decides *which* probes each source means,
//! and what a hit is once it comes back.
//!
//! - **`runtime.custom`** is the extension point: the user names the probes,
//!   by provider and name, through the filter. Nothing else in the catalog
//!   says which probe a custom request means, so a request that names none is
//!   refused rather than attached to everything the target carries.
//! - **`runtime.gc`** is a pause: a begin probe and an end probe on the same
//!   thread, from a table of the runtimes that publish them. The value is the
//!   time between the two.
//! - **`runtime.alloc`** is an object the runtime allocated and how large it
//!   was, from the runtimes that publish a size.
//!
//! **What a probe costs is not what the catalog's band suggests.** A uprobe
//! traps into the kernel on every hit: 740 ns per event against 132 ns for the
//! preload shim on the same event in isolation (finding 6); the shim's shared
//! ring brought a written record to about 70 ns (finding 47). USDT is the path
//! that reaches a
//! program this tool did not launch and cannot preload into, and one whose
//! events are named by the program itself -- not the cheap one. The notes of
//! every run say how many hits it paid for.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration as StdDuration;

use super::objects::{self, Objects};
use crate::bpf::usdt::{Hit, Shape, Site, UsdtProgram};
use crate::collect::{cannot_mean_anything, Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

/// How often the ring is drained. Probe hits are bursty -- a collector that
/// allocates a million objects in a loop -- and a drain is cheap.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// The ring every site of a run writes into. A record is 136 bytes, so this
/// holds about seven thousand hits between drains.
const RING_BYTES: usize = 1 << 20;

/// More sites than this is a request to put a breakpoint on most of a
/// runtime, and every one of them is a perf event and a loaded program.
const MAX_SITES: usize = 256;

/// Which of the three sources a collector serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavour {
    Custom,
    Gc,
    Alloc,
}

impl Flavour {
    fn source(self) -> &'static str {
        match self {
            Flavour::Custom => "runtime.custom",
            Flavour::Gc => "runtime.gc",
            Flavour::Alloc => "runtime.alloc",
        }
    }
}

// ----------------------------------------------------------- the pause table

/// A runtime's garbage-collection pause, as the two probes that bound it.
struct PauseProbes {
    provider: &'static str,
    begin: &'static str,
    end: &'static str,
    /// What the pause is called in the output, so that Ruby's two phases are
    /// not added up into one number.
    phase: &'static str,
    /// The begin probe's argument that is the generation collected.
    generation: Option<usize>,
    /// The end probe's argument that is how many objects were freed.
    collected: Option<usize>,
}

/// Every runtime this build knows publishes a pause, by the probe names in its
/// own source. A runtime built without `--with-dtrace` or its equivalent has
/// none of these, which is the normal case and not a quiet runtime.
const PAUSES: &[PauseProbes] = &[
    // CPython `Include/pydtrace.d`: gc__start(int generation), gc__done(long collected).
    PauseProbes {
        provider: "python",
        begin: "gc__start",
        end: "gc__done",
        phase: "collect",
        generation: Some(0),
        collected: Some(0),
    },
    // Node `src/node_provider.d`: gc__start(int type, int flags), gc__done.
    PauseProbes {
        provider: "node",
        begin: "gc__start",
        end: "gc__done",
        phase: "collect",
        generation: None,
        collected: None,
    },
    // HotSpot `hotspot.d`: gc__begin(bool full), gc__end.
    PauseProbes {
        provider: "hotspot",
        begin: "gc__begin",
        end: "gc__end",
        phase: "collect",
        generation: Some(0),
        collected: None,
    },
    // Ruby `probes.d`: the mark and sweep phases are separate pairs.
    PauseProbes {
        provider: "ruby",
        begin: "gc__mark__begin",
        end: "gc__mark__end",
        phase: "mark",
        generation: None,
        collected: None,
    },
    PauseProbes {
        provider: "ruby",
        begin: "gc__sweep__begin",
        end: "gc__sweep__end",
        phase: "sweep",
        generation: None,
        collected: None,
    },
];

/// A runtime's object allocation, with the argument that carries its size in
/// bytes.
struct AllocProbe {
    provider: &'static str,
    name: &'static str,
    bytes: usize,
}

/// HotSpot `object__alloc(int tid, char *class, int class_len, size_t size)`,
/// whose size `SharedRuntime::dtrace_object_alloc` passes in bytes. It fires
/// only under `-XX:+ExtendedDTraceProbes`. Ruby's `object__create` names a
/// class and no size, so it is not in this table: a source whose unit is bytes
/// cannot report an allocation that has none.
const ALLOCS: &[AllocProbe] = &[AllocProbe {
    provider: "hotspot",
    name: "object__alloc",
    bytes: 3,
}];

// --------------------------------------------------------- probe selection

/// The three fields a probe is selected by, which is what `runtime.custom`'s
/// filter pushes down: a probe the filter can never match is never attached,
/// which is the only placement under which a filtered-out hit costs nothing.
const SELECTING: [&str; 3] = ["provider", "probe", "object"];

/// Evaluates the parts of a filter that are about the probe, leaving the rest
/// unknown. `Some(false)` means no hit of this probe can pass; anything else
/// means it is attached and the full filter decides per hit.
fn could_match(filter: &FilterExpr, site: &Site) -> Option<bool> {
    match filter {
        FilterExpr::And(left, right) => match (could_match(left, site), could_match(right, site)) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        },
        FilterExpr::Or(left, right) => match (could_match(left, site), could_match(right, site)) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        },
        FilterExpr::Not(inner) => could_match(inner, site).map(|value| !value),
        FilterExpr::Compare { field, .. } if SELECTING.contains(&field.as_str()) => {
            let probe_only = Event {
                ts_ns: 0,
                ts_err_ns: 0,
                kind: String::new(),
                keys: Vec::new(),
                attrs: site_attributes(site),
                collector_id: CollectorId::unstamped(),
                seq: 0,
            };
            Some(crate::filter::matches(filter, &probe_only))
        }
        FilterExpr::Compare { .. } => None,
    }
}

fn site_attributes(site: &Site) -> Vec<Attr> {
    vec![
        Attr::text("provider", site.provider.clone()),
        Attr::text("probe", site.name.clone()),
        Attr::text("object", site.object.to_string_lossy()),
    ]
}

/// Which sites a request means, or why none.
fn select(
    flavour: Flavour,
    filter: Option<&FilterExpr>,
    objects: &Objects,
) -> Result<Vec<Site>, String> {
    let all = objects.sites();
    let source = flavour.source();
    let chosen: Vec<Site> = match flavour {
        Flavour::Custom => {
            let Some(filter) = filter.filter(|filter| {
                filter
                    .fields()
                    .iter()
                    .any(|field| *field == "provider" || *field == "probe")
            }) else {
                return Err(cannot_mean_anything(format!(
                    "`runtime.custom` is the probe you name, and this request names none. Select \
                     one through the filter, by `provider` and `probe` -- for example \
                     --filter \"provider == 'libc' && probe == 'setjmp'\". What the target \
                     carries:\n{}",
                    objects.inventory()
                )));
            };
            all.into_iter()
                .filter(|site| could_match(filter, site) != Some(false))
                .collect()
        }
        Flavour::Gc => all
            .into_iter()
            .filter(|site| {
                PAUSES.iter().any(|pause| {
                    site.provider == pause.provider
                        && (site.name == pause.begin || site.name == pause.end)
                })
            })
            .collect(),
        Flavour::Alloc => all
            .into_iter()
            .filter(|site| {
                ALLOCS
                    .iter()
                    .any(|alloc| site.provider == alloc.provider && site.name == alloc.name)
            })
            .collect(),
    };
    if chosen.is_empty() {
        let wanted = match flavour {
            Flavour::Custom => "a probe the filter selects".to_owned(),
            Flavour::Gc => PAUSES
                .iter()
                .map(|pause| format!("{}:{}", pause.provider, pause.begin))
                .collect::<Vec<_>>()
                .join(", "),
            Flavour::Alloc => ALLOCS
                .iter()
                .map(|alloc| format!("{}:{}", alloc.provider, alloc.name))
                .collect::<Vec<_>>()
                .join(", "),
        };
        let how = match flavour {
            Flavour::Custom => "Check the provider and probe names against the list below",
            Flavour::Gc => {
                "CPython and node need to be built --with-dtrace, and HotSpot with DTrace \
                 support"
            }
            Flavour::Alloc => "HotSpot fires object__alloc only under -XX:+ExtendedDTraceProbes",
        };
        return Err(format!(
            "`{source}` found no probe to attach to: the target carries none of {wanted}. A \
             runtime built without its probes publishes nothing, which is not the same as a \
             runtime that did nothing. {how}. What was read:\n{}",
            objects.inventory()
        ));
    }
    if chosen.len() > MAX_SITES {
        return Err(cannot_mean_anything(format!(
            "`{source}` selects {} probe sites, and more than {MAX_SITES} is a breakpoint on most \
             of the program: every hit is a trap into the kernel at about 740 ns. Narrow the \
             filter by `provider` and `probe`",
            chosen.len()
        )));
    }
    Ok(chosen)
}

// ------------------------------------------------------------ the prepare

pub struct Prepare {
    pub flavour: Flavour,
    pub mode: Mode,
    /// The request's own target, because at attach time a launched program is
    /// a pid that has not execed yet and its own map says nothing about what
    /// it will run.
    pub target: Target,
    /// The resolver reports every `bpf` backend's filter as pushed down, so
    /// this collector is the only thing that will ever apply it.
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let source = self.flavour.source();
        match (self.flavour, self.mode) {
            (_, Mode::Trace) | (Flavour::Custom, Mode::Count) => {}
            (_, mode) => {
                return Err(crate::collect::not_built(format!(
                    "`{source}` is collected in trace mode by this build; `{mode}` would \
                     aggregate the same hits on a clock, and nothing does that yet"
                )))
            }
        }
        let pids = match target {
            Target::Pid(pids) | Target::Tid(pids) => pids.clone(),
            other => {
                return Err(format!(
                    "`{source}` puts a probe in one process, and a {} target names none",
                    other.kind()
                ))
            }
        };
        let [pid] = pids[..] else {
            return Err(format!(
                "`{source}` follows one process per run in this build, and this target names {}",
                pids.len()
            ));
        };
        // A thread target is served by its process's probes, kept to the
        // thread's own hits: the breakpoint is in a file every thread shares.
        let (process, only_tid) = match &self.target {
            Target::Tid(_) => (
                crate::collect::task::tgid_of(pid)
                    .ok_or_else(|| format!("thread {pid} does not exist"))?,
                // Compared with the hits' tid, which the program wrote as the
                // kernel numbers it.
                Some(crate::bpf::pidns::kernel_tid(pid as u32)?),
            ),
            _ => (pid, None),
        };
        let launched = matches!(self.target, Target::Launch(_));
        let objects = match &self.target {
            Target::Launch(argv) => objects::of_launch(argv)?,
            _ => objects::of_process(process)?,
        };
        let sites = select(self.flavour, self.filter.as_ref(), &objects)?;
        Ok(Box::new(UsdtCollector::new(
            self.flavour,
            self.mode,
            self.filter,
            process,
            only_tid,
            launched,
            sites,
            objects,
        )))
    }
}

// ----------------------------------------------------------- the collector

pub struct UsdtCollector {
    flavour: Flavour,
    mode: Mode,
    filter: Option<FilterExpr>,
    pid: i32,
    only_tid: Option<u32>,
    launched: bool,
    sites: Vec<Site>,
    objects: Objects,
    program: Option<UsdtProgram>,
    /// Why the sites went through the older attach route, when they did.
    fallback: Option<String>,
    losses: Losses,
    dropped: u64,
    hits: u64,
    filtered_out: u64,
    /// Arguments read as unreadable, by probe and descriptor.
    unread: BTreeMap<String, u64>,
    /// `runtime.gc`: the begin of a pause still open, per thread and pair.
    open: HashMap<(u32, usize), (u64, Hit)>,
    unpaired_ends: u64,
    /// `runtime.custom` in count mode: hits per probe.
    counts: BTreeMap<String, (String, String, u64)>,
}

impl UsdtCollector {
    #[allow(clippy::too_many_arguments)]
    fn new(
        flavour: Flavour,
        mode: Mode,
        filter: Option<FilterExpr>,
        pid: i32,
        only_tid: Option<u32>,
        launched: bool,
        sites: Vec<Site>,
        objects: Objects,
    ) -> Self {
        Self {
            flavour,
            mode,
            filter,
            pid,
            only_tid,
            launched,
            sites,
            objects,
            program: None,
            fallback: None,
            losses: Losses::default(),
            dropped: 0,
            hits: 0,
            filtered_out: 0,
            unread: BTreeMap::new(),
            open: HashMap::new(),
            unpaired_ends: 0,
            counts: BTreeMap::new(),
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
            self.losses.note_times(
                "the USDT ring was full: probes fired faster than they were drained, and each \
                 lost hit is a probe the program did reach",
                dropped - self.dropped,
            );
            self.dropped = dropped;
        }

        let mut events = Vec::new();
        let mut index = seq;
        for hit in hits {
            if self.only_tid.is_some_and(|tid| tid != hit.tid) {
                continue;
            }
            let Some(site) = self.sites.get(hit.site as usize).cloned() else {
                self.losses.undecoded += 1;
                continue;
            };
            self.hits += 1;
            self.count_unread(&site, hit.unread);
            let event = match self.flavour {
                Flavour::Custom => self.custom_event(&site, &hit, index),
                Flavour::Gc => self.gc_event(&site, hit, index),
                Flavour::Alloc => self.alloc_event(&site, &hit, index),
            };
            let Some(event) = event else {
                continue;
            };
            if let Some(filter) = &self.filter {
                if !crate::filter::matches(filter, &event) {
                    self.filtered_out += 1;
                    continue;
                }
            }
            if self.mode == Mode::Count {
                let entry = self
                    .counts
                    .entry(site.qualified())
                    .or_insert_with(|| (site.provider.clone(), site.name.clone(), 0));
                entry.2 += 1;
                continue;
            }
            events.push(event);
            index += 1;
        }
        events
    }

    /// Counted in the kernel when nothing about a hit is needed but that it
    /// happened: `count` mode, and a filter with nothing left to evaluate once
    /// the probes are chosen. A predicate on `comm` needs the hit itself.
    fn shape(&self) -> Shape {
        let residual = self.filter.as_ref().is_some_and(|filter| {
            filter
                .fields()
                .iter()
                .any(|field| !SELECTING.contains(field))
        });
        if self.mode == Mode::Count && !residual {
            Shape::Count
        } else {
            Shape::Stream
        }
    }

    fn count_unread(&mut self, site: &Site, mask: u32) {
        for (slot, arg) in site.args.iter().enumerate() {
            if mask & (1 << slot) != 0 {
                *self
                    .unread
                    .entry(format!(
                        "{} argument {slot} (`{}`)",
                        site.qualified(),
                        arg.text
                    ))
                    .or_default() += 1;
            }
        }
    }

    fn base_attributes(&self, site: &Site, hit: &Hit) -> Vec<Attr> {
        let mut attrs = site_attributes(site);
        attrs.push(Attr::text("comm", hit.comm.clone()));
        attrs.push(Attr::uint("tid", u64::from(hit.tid)));
        attrs.push(Attr::uint("pid", u64::from(hit.tgid)));
        attrs
    }

    fn custom_event(&self, site: &Site, hit: &Hit, seq: u64) -> Option<Event> {
        let mut attrs = vec![Attr::uint("count", 1)];
        attrs.extend(self.base_attributes(site, hit));
        for (slot, arg) in site.args.iter().enumerate() {
            // An argument that could not be read is absent, not zero: a zero
            // is a value the program might have passed.
            if hit.unread & (1 << slot) == 0 {
                attrs.push(argument(&format!("arg{slot}"), arg.signed, hit.args[slot]));
            }
        }
        Some(self.event(hit.ts_ns, hit.tid, attrs, seq))
    }

    fn gc_event(&mut self, site: &Site, hit: Hit, seq: u64) -> Option<Event> {
        let (pair, is_begin) = PAUSES.iter().enumerate().find_map(|(index, pause)| {
            (site.provider == pause.provider)
                .then_some(())
                .and_then(|()| {
                    if site.name == pause.begin {
                        Some((index, true))
                    } else if site.name == pause.end {
                        Some((index, false))
                    } else {
                        None
                    }
                })
        })?;
        if is_begin {
            // A second begin with no end between them replaces the first: the
            // runtime does not nest these, so the first end was lost.
            if self
                .open
                .insert((hit.tid, pair), (hit.ts_ns, hit))
                .is_some()
            {
                self.unpaired_ends += 1;
            }
            return None;
        }
        let Some((began, begin)) = self.open.remove(&(hit.tid, pair)) else {
            // An end whose begin this run never saw: the pause was already
            // under way when the probes attached.
            self.unpaired_ends += 1;
            return None;
        };
        let pause = &PAUSES[pair];
        let mut attrs = vec![Attr::uint("pause_ns", hit.ts_ns.saturating_sub(began))];
        attrs.extend(self.base_attributes(site, &hit));
        // The pair's begin probe, which is what a reader filters a phase by.
        attrs[1] = Attr::text("provider", pause.provider);
        attrs[2] = Attr::text("probe", pause.begin);
        attrs.push(Attr::text("phase", pause.phase));
        if let Some(slot) = pause.generation {
            if begin.unread & (1 << slot) == 0 {
                attrs.push(Attr::int("generation", begin.args[slot] as i64));
            }
        }
        if let Some(slot) = pause.collected {
            if hit.unread & (1 << slot) == 0 {
                attrs.push(Attr::int("collected", hit.args[slot] as i64));
            }
        }
        Some(self.event(began, hit.tid, attrs, seq))
    }

    fn alloc_event(&self, site: &Site, hit: &Hit, seq: u64) -> Option<Event> {
        let alloc = ALLOCS
            .iter()
            .find(|alloc| site.provider == alloc.provider && site.name == alloc.name)?;
        if hit.unread & (1 << alloc.bytes) != 0 {
            return None;
        }
        let mut attrs = vec![Attr::uint("bytes", hit.args[alloc.bytes])];
        attrs.extend(self.base_attributes(site, hit));
        Some(self.event(hit.ts_ns, hit.tid, attrs, seq))
    }

    fn event(&self, ts_ns: u64, tid: u32, attrs: Vec<Attr>, seq: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: self.flavour.source().to_owned(),
            keys: vec![KeyValue::task(tid as i32)],
            attrs,
            collector_id: CollectorId::unstamped(),
            seq,
        }
    }
}

/// An argument as the descriptor declared it: signed ones as integers, so a
/// `-1` reads as `-1` and not as eighteen quintillion.
fn argument(name: &str, signed: bool, raw: u64) -> Attr {
    if signed {
        Attr::int(name, raw as i64)
    } else {
        Attr::uint(name, raw)
    }
}

impl Collector for UsdtCollector {
    fn value_attr(&self) -> &'static str {
        match self.flavour {
            Flavour::Custom => "count",
            Flavour::Gc => "pause_ns",
            Flavour::Alloc => "bytes",
        }
    }

    fn start(&mut self) -> Result<(), String> {
        let program = UsdtProgram::load(&self.sites, self.pid, self.shape(), RING_BYTES)?;
        self.fallback = program.fallback.clone();
        self.program = Some(program);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        Ok(self.drain(seq))
    }

    fn stop(&mut self) -> Vec<Event> {
        let mut trailing = self.drain(u64::MAX / 2);
        if let Some(program) = self.program.as_ref() {
            // Counted in the kernel: the totals are read once, now, while the
            // programs are still loaded.
            for (index, site) in self.sites.iter().enumerate() {
                let Some(count) = program.counted(index) else {
                    break;
                };
                self.hits += count;
                let entry = self
                    .counts
                    .entry(site.qualified())
                    .or_insert_with(|| (site.provider.clone(), site.name.clone(), 0));
                entry.2 += count;
            }
        }
        self.program = None;
        if self.mode == Mode::Count {
            let at = crate::clock::monotonic_ns();
            for (index, (provider, name, count)) in
                std::mem::take(&mut self.counts).into_values().enumerate()
            {
                trailing.push(Event {
                    ts_ns: at,
                    ts_err_ns: 0,
                    kind: self.flavour.source().to_owned(),
                    keys: vec![KeyValue::task(self.pid)],
                    attrs: vec![
                        Attr::uint("count", count),
                        Attr::text("provider", provider),
                        Attr::text("probe", name),
                    ],
                    collector_id: CollectorId::unstamped(),
                    seq: u64::MAX / 2 + index as u64,
                });
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
        let names: std::collections::BTreeSet<String> =
            self.sites.iter().map(Site::qualified).collect();
        let read: Vec<String> = self
            .objects
            .scanned
            .iter()
            .map(|scanned| scanned.path.display().to_string())
            .collect();
        notes.push(format!(
            "{} probe site{} attached ({}), out of the objects read: {}{}",
            self.sites.len(),
            if self.sites.len() == 1 { "" } else { "s" },
            names.into_iter().collect::<Vec<_>>().join(", "),
            read.join(", "),
            if self.launched {
                "; a library the program loads later with dlopen is not among them"
            } else {
                ""
            }
        ));
        if let Some(why) = &self.fallback {
            notes.push(why.clone());
            let guarded = self
                .sites
                .iter()
                .filter(|site| site.semaphore.is_some())
                .count();
            if guarded > 0 {
                notes.push(format!(
                    "{guarded} site{} guarded by a semaphore that nothing raised on this route, so \
                     the program never reached {}: a count of zero there is not a measurement",
                    if guarded == 1 { " is" } else { "s are" },
                    if guarded == 1 { "it" } else { "them" },
                ));
            }
        }
        for (what, count) in &self.unread {
            notes.push(format!(
                "{what} was unreadable on {count} hit{} and is absent from {} rather than zero",
                if *count == 1 { "" } else { "s" },
                if *count == 1 { "it" } else { "them" },
            ));
        }
        if self.filtered_out > 0 {
            notes.push(format!(
                "the filter removed {} of {} hits after they were taken: the probe was selected \
                 before the run and the rest of the predicate could only be evaluated per hit",
                self.filtered_out, self.hits
            ));
        }
        if self.flavour == Flavour::Gc {
            if self.unpaired_ends > 0 {
                notes.push(format!(
                    "{} pause{} had only one of {} two probes in this run and {} not reported",
                    self.unpaired_ends,
                    if self.unpaired_ends == 1 { "" } else { "s" },
                    if self.unpaired_ends == 1 {
                        "its"
                    } else {
                        "their"
                    },
                    if self.unpaired_ends == 1 { "is" } else { "are" },
                ));
            }
            if !self.open.is_empty() {
                notes.push(format!(
                    "{} pause{} still under way when the run ended and {} in no total",
                    self.open.len(),
                    if self.open.len() == 1 {
                        " was"
                    } else {
                        "s were"
                    },
                    if self.open.len() == 1 { "is" } else { "are" },
                ));
            }
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bpf::usdt::parse_arguments;
    use std::path::PathBuf;

    fn site(provider: &str, name: &str) -> Site {
        Site {
            object: PathBuf::from("/usr/lib/libc.so.6"),
            provider: provider.into(),
            name: name.into(),
            offset: 0,
            semaphore: None,
            args: parse_arguments("8@%rdi").unwrap(),
        }
    }

    #[test]
    fn the_filter_selects_probes_and_leaves_the_rest_of_itself_for_each_hit() {
        let filter: FilterExpr = "provider == 'libc' && probe == 'setjmp' && comm == 'x'"
            .parse()
            .unwrap();
        // The probe parts decide; the `comm` part cannot be known before a hit.
        assert_eq!(could_match(&filter, &site("libc", "setjmp")), None);
        assert_eq!(could_match(&filter, &site("libc", "longjmp")), Some(false));
        assert_eq!(could_match(&filter, &site("python", "setjmp")), Some(false));

        let either: FilterExpr = "probe == 'setjmp' || probe == 'longjmp'".parse().unwrap();
        assert_eq!(could_match(&either, &site("libc", "longjmp")), Some(true));
        assert_eq!(
            could_match(&either, &site("libc", "cond_wait")),
            Some(false)
        );

        let negated: FilterExpr = "provider == 'libc' && !(probe == 'setjmp')"
            .parse()
            .unwrap();
        assert_eq!(could_match(&negated, &site("libc", "setjmp")), Some(false));
        assert_eq!(could_match(&negated, &site("libc", "longjmp")), Some(true));
    }

    #[test]
    fn every_pause_pair_ends_differently_from_how_it_begins() {
        for pause in PAUSES {
            assert_ne!(pause.begin, pause.end, "{}", pause.provider);
        }
    }
}
