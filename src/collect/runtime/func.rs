//! `runtime.func`: how long each call of the functions a request names took,
//! through a uprobe on each one's entry and return (`src/bpf/func_latency.rs`).
//!
//! The functions are named through the filter, as `runtime.custom`'s probes
//! are: `--filter "function == 'compute'"`, `function =~ 'CopyThunk'`,
//! `function in ('a', 'b')`. A name answers to a function as the file spells it
//! (mangled), as it demangles, or as it demangles without its parameters -- the
//! name a C++ reader writes, which names every overload. `object == 'libfoo.so'`
//! keeps the search to one file. The objects searched are the target's: the
//! executable and the libraries it maps, or, for a launched program, the ones
//! the dynamic linker will map.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration as StdDuration;

use super::objects::{self, Objects};
use crate::bpf::func_latency::{Call, FuncProgram, Lost, Target as Placed, MAX_DEPTH};
use crate::collect::{cannot_mean_anything, Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::filter::{CompareOp, FilterExpr, Value};
use crate::request::{Mode, Target};
use crate::stack::FunctionSymbol;

/// How often the ring is drained.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(20);

/// The ring between the programs and this collector.
const RING_BYTES: usize = 1 << 20;

/// More functions than this is a breakpoint on most of a program: two traps a
/// call, everywhere.
const MAX_FUNCTIONS: usize = 32;

/// A call shorter than this is mostly the two traps that timed it: about a
/// microsecond of each timed call is theirs (measured on a call that does
/// nothing: 1.07 us).
const TRAP_FLOOR_NS: u64 = 2_000;

/// How a filter names functions: a term, and whether it is a substring.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wanted {
    term: String,
    substring: bool,
}

/// The function terms a filter names, anywhere in it: each alternative of an
/// `||` is a function to time.
fn wanted(filter: &FilterExpr) -> Vec<Wanted> {
    let mut found = Vec::new();
    collect(filter, "function", &mut found);
    found
}

/// The object names a filter keeps the search to.
fn objects_wanted(filter: &FilterExpr) -> Vec<String> {
    let mut found = Vec::new();
    collect(filter, "object", &mut found);
    found.into_iter().map(|wanted| wanted.term).collect()
}

fn collect(filter: &FilterExpr, name: &str, into: &mut Vec<Wanted>) {
    match filter {
        FilterExpr::And(left, right) | FilterExpr::Or(left, right) => {
            collect(left, name, into);
            collect(right, name, into);
        }
        // A function the request excludes is not one it names.
        FilterExpr::Not(_) => {}
        FilterExpr::Compare {
            field,
            operator,
            value,
        } if field == name => match (operator, value) {
            (CompareOp::Eq, Value::Str(term)) => into.push(Wanted {
                term: term.clone(),
                substring: false,
            }),
            (CompareOp::Matches, Value::Str(term)) => into.push(Wanted {
                term: term.clone(),
                substring: true,
            }),
            (CompareOp::In, Value::List(terms)) => {
                for term in terms {
                    if let Value::Str(term) = term {
                        into.push(Wanted {
                            term: term.clone(),
                            substring: false,
                        });
                    }
                }
            }
            _ => {}
        },
        FilterExpr::Compare { .. } => {}
    }
}

/// The name a function is filtered and reported by: its demangled name
/// without its parameter list.
fn bare(function: &FunctionSymbol) -> String {
    match function.name.split_once('(') {
        Some((bare, _)) if !bare.is_empty() => bare.trim_end().to_owned(),
        _ => function.name.clone(),
    }
}

impl Wanted {
    fn takes(&self, function: &FunctionSymbol) -> bool {
        if self.substring {
            function.name.contains(&self.term) || function.symbol.contains(&self.term)
        } else {
            function.answers_to(&self.term)
        }
    }
}

/// One function the run times.
#[derive(Debug, Clone)]
struct Timed {
    object: String,
    function: FunctionSymbol,
    placed: Placed,
}

/// The functions the filter names, in the objects it keeps.
fn select(filter: Option<&FilterExpr>, objects: &Objects) -> Result<Vec<Timed>, String> {
    let read: Vec<&Path> = objects
        .scanned
        .iter()
        .map(|scanned| scanned.path.as_path())
        .collect();
    let named = filter.map(wanted).unwrap_or_default();
    if named.is_empty() {
        return Err(cannot_mean_anything(
            "`runtime.func` times the functions you name, and this request names none. Name them \
             through the filter, by `function` -- --filter \"function == 'compute'\", or \
             \"function =~ 'CopyThunk'\" for every function whose name contains it",
        ));
    }
    let only: Vec<String> = filter.map(objects_wanted).unwrap_or_default();
    let mut chosen: Vec<Timed> = Vec::new();
    for path in &read {
        let object = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        if !only.is_empty() && !only.contains(&object) {
            continue;
        }
        for function in crate::stack::functions_in(path, &|function| {
            named.iter().any(|wanted| wanted.takes(function))
        }) {
            chosen.push(Timed {
                object: object.clone(),
                placed: Placed {
                    object: path.to_path_buf(),
                    offset: function.offset,
                },
                function,
            });
        }
    }
    if chosen.is_empty() {
        let terms: Vec<String> = named
            .iter()
            .map(|wanted| format!("`{}`", wanted.term))
            .collect();
        // What the files do hold that resembles it, for a name misspelt or
        // half remembered.
        let mut near: BTreeSet<String> = BTreeSet::new();
        for path in &read {
            for function in crate::stack::functions_in(path, &|function| {
                named.iter().any(|wanted| {
                    let lower = wanted.term.to_lowercase();
                    function.name.to_lowercase().contains(&lower)
                })
            }) {
                near.insert(bare(&function));
                if near.len() >= 10 {
                    break;
                }
            }
        }
        return Err(format!(
            "`runtime.func` found no function named {} in the {} object(s) the target maps{}{}. \
             What was read:\n{}",
            terms.join(" or "),
            read.len(),
            if only.is_empty() {
                String::new()
            } else {
                format!(" (kept to {})", only.join(", "))
            },
            if near.is_empty() {
                String::new()
            } else {
                format!(
                    "; functions whose name contains it: {}",
                    near.into_iter().collect::<Vec<_>>().join(", ")
                )
            },
            objects.inventory()
        ));
    }
    if chosen.len() > MAX_FUNCTIONS {
        return Err(cannot_mean_anything(format!(
            "`runtime.func` selects {} functions, and more than {MAX_FUNCTIONS} is a breakpoint \
             on most of the program: every call pays two traps into the kernel. Name them more \
             narrowly, or keep the search to one file with `object == '...'`",
            chosen.len()
        )));
    }
    Ok(chosen)
}

// ------------------------------------------------------------ the prepare

pub struct Prepare {
    pub mode: Mode,
    /// The request's own target: a launched program is a pid that has not
    /// execed yet at attach time, and what it will map is read from its argv.
    pub target: Target,
    pub filter: Option<FilterExpr>,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(crate::collect::not_built(format!(
                "`runtime.func` times each call in trace mode; `{}` would aggregate the same \
                 calls on a clock, and nothing does that yet: `-e summary` and `-e hist` sum the \
                 trace",
                self.mode
            )));
        }
        let pids = match target {
            Target::Pid(pids) | Target::Tid(pids) => pids.clone(),
            other => {
                return Err(format!(
                    "`runtime.func` puts its probes in one process, and a {} target names none",
                    other.kind()
                ))
            }
        };
        let [pid] = pids[..] else {
            return Err(format!(
                "`runtime.func` follows one process per run, and this target names {}",
                pids.len()
            ));
        };
        let (process, only_tid) = match &self.target {
            Target::Tid(_) => (
                crate::collect::task::tgid_of(pid)
                    .ok_or_else(|| format!("thread {pid} does not exist"))?,
                Some(crate::bpf::pidns::kernel_tid(pid as u32)?),
            ),
            _ => (pid, None),
        };
        let launched = matches!(self.target, Target::Launch(_));
        let objects = match &self.target {
            Target::Launch(argv) => objects::of_launch(argv)?,
            _ => objects::of_process(process)?,
        };
        let timed = select(self.filter.as_ref(), &objects)?;
        Ok(Box::new(FuncCollector {
            filter: self.filter,
            pid: process,
            only_tid,
            launched,
            timed,
            objects,
            program: None,
            fallback: None,
            losses: Losses::default(),
            lost: Lost::default(),
            calls: 0,
            short: 0,
            nested: 0,
            filtered_out: 0,
            open_at_end: 0,
        }))
    }
}

// ----------------------------------------------------------- the collector

pub struct FuncCollector {
    filter: Option<FilterExpr>,
    pid: i32,
    only_tid: Option<u32>,
    launched: bool,
    timed: Vec<Timed>,
    objects: Objects,
    program: Option<FuncProgram>,
    fallback: Option<String>,
    losses: Losses,
    /// The programs' counters as last read.
    lost: Lost,
    calls: u64,
    /// Calls shorter than [`TRAP_FLOOR_NS`].
    short: u64,
    /// Calls inside a call of the same function.
    nested: u64,
    filtered_out: u64,
    open_at_end: usize,
}

impl FuncCollector {
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
                .note("a record in the function ring was shorter than one call");
        }
        let lost = program.lost();
        let grew = |now: u64, then: u64| now.saturating_sub(then);
        let ring = grew(lost.ring, self.lost.ring);
        if ring > 0 {
            self.losses.dropped += ring;
            self.losses.note_times(
                "the function ring was full: calls returned faster than they were drained, and \
                 each lost one was a call the program made",
                ring,
            );
        }
        let open = grew(lost.open, self.lost.open) + grew(lost.stamp, self.lost.stamp);
        if open > 0 {
            self.losses.dropped += open;
            self.losses.note_times(
                "more calls were open at once than the map of open calls holds, so these were \
                 never timed",
                open,
            );
        }
        self.lost = lost;

        let mut events = Vec::with_capacity(calls.len());
        let mut index = seq;
        for call in calls {
            if self.only_tid.is_some_and(|tid| tid != call.tid) {
                continue;
            }
            let Some(timed) = self.timed.get(call.function as usize) else {
                self.losses.undecoded += 1;
                continue;
            };
            self.calls += 1;
            if call.duration_ns < TRAP_FLOOR_NS {
                self.short += 1;
            }
            if call.depth > 1 {
                self.nested += 1;
            }
            let event = event(timed, &call, index);
            if let Some(filter) = &self.filter {
                if !crate::filter::matches(filter, &event) {
                    self.filtered_out += 1;
                    continue;
                }
            }
            events.push(event);
            index += 1;
        }
        events
    }
}

/// One call as an event, stamped at its start and keyed by its thread.
fn event(timed: &Timed, call: &Call, seq: u64) -> Event {
    Event {
        ts_ns: call.start_ns,
        ts_err_ns: 0,
        kind: "runtime.func".to_owned(),
        keys: vec![KeyValue::task(call.tid as i32)],
        attrs: vec![
            Attr::uint("lat_ns", call.duration_ns),
            Attr::text("function", bare(&timed.function)),
            Attr::text("signature", timed.function.name.clone()),
            Attr::text("symbol", timed.function.symbol.clone()),
            Attr::text("object", timed.object.clone()),
            Attr::uint("depth", u64::from(call.depth)),
            Attr::int("ret", call.ret as i64),
            Attr::text("comm", call.comm.clone()),
            Attr::uint("tid", u64::from(call.tid)),
            Attr::uint("pid", u64::from(call.tgid)),
        ],
        collector_id: CollectorId::unstamped(),
        seq,
    }
}

impl Collector for FuncCollector {
    fn value_attr(&self) -> &'static str {
        "lat_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        let placed: Vec<Placed> = self
            .timed
            .iter()
            .map(|timed| timed.placed.clone())
            .collect();
        let program = FuncProgram::load(&placed, self.pid, RING_BYTES)?;
        self.fallback = program.fallback.clone();
        self.program = Some(program);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        Ok(self.drain(seq))
    }

    fn stop(&mut self) -> Vec<Event> {
        let trailing = self.drain(u64::MAX / 2);
        self.open_at_end = self.program.as_ref().map_or(0, FuncProgram::open);
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
        let mut by_object: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for timed in &self.timed {
            by_object
                .entry(timed.object.as_str())
                .or_default()
                .push(bare(&timed.function));
        }
        notes.push(format!(
            "{} call{} timed, of {} function{}: {}{}",
            self.calls,
            if self.calls == 1 { "" } else { "s" },
            self.timed.len(),
            if self.timed.len() == 1 { "" } else { "s" },
            by_object
                .iter()
                .map(|(object, names)| format!("{} in {object}", names.join(", ")))
                .collect::<Vec<_>>()
                .join("; "),
            if self.launched {
                "; a library the program loads later with dlopen is not searched"
            } else {
                ""
            }
        ));
        notes.push(format!(
            "searched: {}",
            self.objects
                .scanned
                .iter()
                .map(|scanned| scanned.path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        notes.push(
            "a call's time is the wall clock from its entry to its return, on its thread: time \
             it spent blocked or waiting for a cpu is in it, which a cpu profile does not see; \
             each call paid two traps into the kernel to be timed"
                .to_owned(),
        );
        if self.short > 0 {
            notes.push(format!(
                "{} of the calls lasted under {} us, and about 1 us of each timed call is the two \
                 traps that timed it: their times are mostly the measurement",
                self.short,
                TRAP_FLOOR_NS / 1000
            ));
        }
        if self.nested > 0 {
            notes.push(format!(
                "{} call{} ran inside a call of the same function (`depth` above 1): a nested \
                 call's time is also in its caller's, so add only `depth == 1` for the \
                 function's total",
                self.nested,
                if self.nested == 1 { "" } else { "s" }
            ));
        }
        if self.lost.unpaired > 0 {
            notes.push(format!(
                "{} return{} had no entry this run saw: calls already under way when the probes \
                 attached, not timed and not lost",
                self.lost.unpaired,
                if self.lost.unpaired == 1 { "" } else { "s" }
            ));
        }
        if self.lost.deep > 0 {
            notes.push(format!(
                "{} call{} nested deeper than {MAX_DEPTH} were counted and not timed",
                self.lost.deep,
                if self.lost.deep == 1 { "" } else { "s" }
            ));
        }
        if self.open_at_end > 0 {
            notes.push(format!(
                "{} call{} entered and never returned while the probes were attached: still \
                 running when the run ended, or left by a longjmp or an exception the return \
                 probe never saw",
                self.open_at_end,
                if self.open_at_end == 1 {
                    " was"
                } else {
                    "s were"
                }
            ));
        }
        if let Some(why) = &self.fallback {
            notes.push(why.clone());
        }
        if self.filtered_out > 0 {
            notes.push(format!(
                "the filter removed {} of {} calls after they were timed",
                self.filtered_out, self.calls
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(text: &str) -> FilterExpr {
        text.parse().unwrap()
    }

    fn function(symbol: &str, name: &str) -> FunctionSymbol {
        FunctionSymbol {
            symbol: symbol.to_owned(),
            name: name.to_owned(),
            offset: 0x1000,
            size: 16,
        }
    }

    /// Every way a filter names a function, and only those.
    #[test]
    fn the_filter_names_the_functions_to_time() {
        let terms = |text: &str| -> Vec<(String, bool)> {
            wanted(&filter(text))
                .into_iter()
                .map(|wanted| (wanted.term, wanted.substring))
                .collect()
        };
        assert_eq!(terms("function == 'a'"), [("a".to_owned(), false)]);
        assert_eq!(
            terms("function == 'a' || function =~ 'Copy'"),
            [("a".to_owned(), false), ("Copy".to_owned(), true)]
        );
        assert_eq!(
            terms("function in ('a', 'b') && comm == 'x'"),
            [("a".to_owned(), false), ("b".to_owned(), false)]
        );
        assert!(terms("!(function == 'a')").is_empty());
        assert_eq!(
            objects_wanted(&filter("object == 'libfoo.so'")),
            ["libfoo.so"]
        );
    }

    /// A C++ function answers to its readable name without parameters, its
    /// whole demangled name, and its mangled one.
    #[test]
    fn a_function_answers_to_the_name_a_reader_writes() {
        let copy = function(
            "_ZN3xla3cpu9CopyThunk7ExecuteERKNS0_5Thunk13ExecuteParamsE",
            "xla::cpu::CopyThunk::Execute(xla::cpu::Thunk::ExecuteParams const&)",
        );
        let exact = |term: &str| Wanted {
            term: term.to_owned(),
            substring: false,
        };
        assert!(exact("xla::cpu::CopyThunk::Execute").takes(&copy));
        assert!(exact("_ZN3xla3cpu9CopyThunk7ExecuteERKNS0_5Thunk13ExecuteParamsE").takes(&copy));
        assert!(!exact("xla::cpu::CopyThunk").takes(&copy));
        assert!(Wanted {
            term: "CopyThunk".to_owned(),
            substring: true
        }
        .takes(&copy));
        assert_eq!(bare(&copy), "xla::cpu::CopyThunk::Execute");
    }

    /// The event carries the name the filter matched, so the filter that
    /// chose the function also keeps its calls.
    #[test]
    fn a_call_s_event_is_kept_by_the_filter_that_chose_it() {
        let timed = Timed {
            object: "app".to_owned(),
            function: function("_Z7computei", "compute(int)"),
            placed: Placed {
                object: "/bin/app".into(),
                offset: 0x1000,
            },
        };
        let call = Call {
            start_ns: 10,
            duration_ns: 2_000_000,
            ret: 0,
            tid: 5,
            tgid: 5,
            function: 0,
            depth: 1,
            comm: "app".to_owned(),
        };
        let event = event(&timed, &call, 0);
        assert!(crate::filter::matches(
            &filter("function == 'compute'"),
            &event
        ));
        assert!(crate::filter::matches(
            &filter("function =~ 'comp' && depth == 1"),
            &event
        ));
    }
}
