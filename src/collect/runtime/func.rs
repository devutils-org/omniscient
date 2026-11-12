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
//! the dynamic linker will map. A library the target maps later -- a plugin
//! opened with `dlopen` -- is found by reading its maps again every
//! [`RESCAN_INTERVAL`], and its functions are probed then: the calls between
//! its mapping and its probes are not timed, and the run says how long that
//! was at most. A launched program none of whose objects has the functions
//! at the start runs anyway, and fails the run if nothing it maps later has
//! them either.

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

/// How often a histogram's rows are read.
const ROWS_INTERVAL: StdDuration = StdDuration::from_millis(100);

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
    function.bare_name().to_owned()
}

/// Words a demangler writes that the mangled name does not spell: the
/// Itanium abbreviations (`St` is `std`, `Ss` a `basic_string`), builtin
/// types written as one letter, operators, and what it says of lambdas,
/// clones and anonymous namespaces; and Rust's equivalents. A term's word
/// that is part of one of these is not looked for in the mangled name.
const WRITTEN_BY_THE_DEMANGLER: &[&str] = &[
    "std",
    "allocator",
    "basic_string",
    "char_traits",
    "basic_istream",
    "basic_ostream",
    "basic_iostream",
    "string",
    "istream",
    "ostream",
    "iostream",
    "cxx11",
    "operator",
    "new",
    "delete",
    "sizeof",
    "alignof",
    "typeid",
    "throw",
    "noexcept",
    "decltype",
    "const",
    "volatile",
    "restrict",
    "unsigned",
    "signed",
    "char",
    "char8_t",
    "char16_t",
    "char32_t",
    "wchar_t",
    "short",
    "int",
    "long",
    "__int128",
    "float",
    "double",
    "__float128",
    "_Float16",
    "bool",
    "void",
    "auto",
    "nullptr",
    "true",
    "false",
    "anonymous",
    "namespace",
    "lambda",
    "unnamed",
    "clone",
    "abi",
    "vtable",
    "for",
    "VTT",
    "typeinfo",
    "name",
    "construction",
    "guard",
    "variable",
    "reference",
    "temporary",
    "non",
    "virtual",
    "thunk",
    "covariant",
    "return",
    "to",
    "transaction",
    "safe",
    "default",
    "arg",
    "string",
    "literal",
    "ellipsis",
    "impl",
    "dyn",
    "as",
    "mut",
    "fn",
    "unsafe",
    "extern",
    "closure",
    "shim",
    "str",
    "u8",
    "u16",
    "u32",
    "u64",
    "u128",
    "usize",
    "i8",
    "i16",
    "i32",
    "i64",
    "i128",
    "isize",
    "f32",
    "f64",
];

/// The words of a term a symbol must spell, as the file spells it, to be
/// worth demangling: [`Wanted::spelled`].
struct Spelled {
    words: Vec<String>,
    fold: bool,
}

impl Spelled {
    fn could(&self, symbol: &str) -> bool {
        if self.words.is_empty() {
            return true;
        }
        if self.fold {
            let symbol = symbol.to_lowercase();
            self.words.iter().all(|word| symbol.contains(word.as_str()))
        } else {
            self.words.iter().all(|word| symbol.contains(word.as_str()))
        }
    }
}

impl Wanted {
    /// What a symbol as the file spells it must hold to answer to this term
    /// once demangled, so that one which cannot is never demangled
    /// ([`crate::stack::functions_in_where`]).
    ///
    /// A demangled name spells its identifiers as the mangled one does --
    /// `_ZN3xla3cpu9CopyThunk7ExecuteEi` holds `CopyThunk` and `Execute`
    /// verbatim -- so every word of the term must be in the symbol, except a
    /// word the demangler may have written itself (part of one in
    /// [`WRITTEN_BY_THE_DEMANGLER`], or a number). `fold` compares without
    /// case, for the names offered when nothing matched.
    fn spelled(&self, fold: bool) -> Spelled {
        let words = self
            .term
            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
            .filter(|word| !word.is_empty())
            .filter(|word| !word.starts_with(|c: char| c.is_ascii_digit()))
            .filter(|word| {
                let lower = word.to_lowercase();
                !WRITTEN_BY_THE_DEMANGLER
                    .iter()
                    .any(|written| written.to_lowercase().contains(&lower))
            })
            .map(|word| {
                if fold {
                    word.to_lowercase()
                } else {
                    word.to_owned()
                }
            })
            .collect();
        Spelled { words, fold }
    }

    #[cfg(test)]
    fn could(&self, symbol: &str, fold: bool) -> bool {
        self.spelled(fold).could(symbol)
    }

    fn takes(&self, function: &FunctionSymbol) -> bool {
        if self.substring {
            function.name.contains(&self.term) || function.symbol.contains(&self.term)
        } else {
            function.answers_to(&self.term)
        }
    }
}

/// Evaluate function predicates against the same names used during discovery.
fn matches_call(filter: &FilterExpr, event: &Event, function: &FunctionSymbol) -> bool {
    match filter {
        FilterExpr::And(left, right) => {
            matches_call(left, event, function) && matches_call(right, event, function)
        }
        FilterExpr::Or(left, right) => {
            matches_call(left, event, function) || matches_call(right, event, function)
        }
        FilterExpr::Not(inner) => !matches_call(inner, event, function),
        FilterExpr::Compare {
            field,
            operator,
            value,
        } if field == "function" => {
            let matches = |term: &str, substring| {
                Wanted {
                    term: term.to_owned(),
                    substring,
                }
                .takes(function)
            };
            match (operator, value) {
                (CompareOp::Eq, Value::Str(term)) => matches(term, false),
                (CompareOp::Ne, Value::Str(term)) => !matches(term, false),
                (CompareOp::Matches, Value::Str(term)) => matches(term, true),
                (CompareOp::In, Value::List(terms)) => terms
                    .iter()
                    .any(|term| matches!(term, Value::Str(term) if matches(term, false))),
                _ => crate::filter::matches(filter, event),
            }
        }
        _ => crate::filter::matches(filter, event),
    }
}

/// What was searched for functions, one object a line: the name the target
/// maps it by, and why it could not be read when it could not. About symbol
/// tables, not probes -- a function is found in the first and has no use for
/// the second.
fn symbol_inventory(objects: &Objects) -> String {
    let mut lines: Vec<String> = objects
        .scanned
        .iter()
        .map(|scanned| match &scanned.sites {
            // The reason names the file already.
            Err(why) => format!("  {why}"),
            Ok(_) => format!("  {}", scanned.mapped.display()),
        })
        .collect();
    if !objects.unresolved.is_empty() {
        lines.push(format!(
            "  and needed libraries no search path resolved: {}",
            objects.unresolved.join(", ")
        ));
    }
    lines.join("\n")
}

/// One function the run times.
#[derive(Debug, Clone)]
struct Timed {
    object: String,
    function: FunctionSymbol,
    placed: Placed,
}

/// What a filter asks the symbol tables for, read once and asked of every
/// object: at the start, and of each library the target maps later.
struct Search {
    named: Vec<Wanted>,
    /// The object names the search is kept to; empty is every object.
    only: Vec<String>,
    spelled: Vec<Spelled>,
}

impl Search {
    fn of(filter: Option<&FilterExpr>) -> Result<Self, String> {
        let named = filter.map(wanted).unwrap_or_default();
        if named.is_empty() {
            return Err(cannot_mean_anything(
                "`runtime.func` times the functions you name, and this request names none. Name \
                 them through the filter, by `function` -- --filter \"function == 'compute'\", or \
                 \"function =~ 'CopyThunk'\" for every function whose name contains it",
            ));
        }
        Ok(Self {
            only: filter.map(objects_wanted).unwrap_or_default(),
            spelled: named.iter().map(|wanted| wanted.spelled(false)).collect(),
            named,
        })
    }

    /// The functions it names in the file opened at `path`, which the target
    /// maps as `mapped`: the name an `object ==` compares, and every event
    /// carries.
    fn in_object(&self, path: &Path, mapped: &Path) -> Vec<Timed> {
        let object = mapped.file_name().map_or_else(
            || mapped.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        if !self.only.is_empty() && !self.only.contains(&object) {
            return Vec::new();
        }
        crate::stack::functions_in_where(
            path,
            &|symbol| self.spelled.iter().any(|spelled| spelled.could(symbol)),
            &|function| self.named.iter().any(|wanted| wanted.takes(function)),
        )
        .into_iter()
        .map(|function| Timed {
            object: object.clone(),
            placed: Placed {
                object: path.to_path_buf(),
                offset: function.offset,
            },
            function,
        })
        .collect()
    }

    /// Why nothing was found in `objects`: the terms, what the files hold
    /// that resembles them, and what was read.
    fn not_found(&self, objects: &Objects) -> String {
        let terms: Vec<String> = self
            .named
            .iter()
            .map(|wanted| format!("`{}`", wanted.term))
            .collect();
        // What the files do hold that resembles it, for a name misspelt or
        // half remembered.
        let mut near: BTreeSet<String> = BTreeSet::new();
        let folded: Vec<Spelled> = self
            .named
            .iter()
            .map(|wanted| wanted.spelled(true))
            .collect();
        for scanned in &objects.scanned {
            for function in crate::stack::functions_in_where(
                &scanned.path,
                &|symbol| folded.iter().any(|spelled| spelled.could(symbol)),
                &|function| {
                    self.named.iter().any(|wanted| {
                        let lower = wanted.term.to_lowercase();
                        function.name.to_lowercase().contains(&lower)
                    })
                },
            ) {
                near.insert(bare(&function));
                if near.len() >= 10 {
                    break;
                }
            }
        }
        format!(
            "`runtime.func` found no function named {} in the {} object(s) the target maps{}{}. \
             What was read:\n{}",
            terms.join(" or "),
            objects.scanned.len(),
            if self.only.is_empty() {
                String::new()
            } else {
                format!(" (kept to {})", self.only.join(", "))
            },
            if near.is_empty() {
                String::new()
            } else {
                format!(
                    "; functions whose name contains it: {}",
                    near.into_iter().collect::<Vec<_>>().join(", ")
                )
            },
            symbol_inventory(objects)
        )
    }
}

/// More functions than [`MAX_FUNCTIONS`], refused with what to do instead.
fn too_many(count: usize) -> String {
    cannot_mean_anything(format!(
        "`runtime.func` selects {count} functions, and more than {MAX_FUNCTIONS} is a breakpoint \
         on most of the program: every call pays two traps into the kernel. Name them more \
         narrowly, or keep the search to one file with `object == '...'`"
    ))
}

/// Every function the search names, in every object read.
fn chosen(search: &Search, objects: &Objects) -> Vec<Timed> {
    objects
        .scanned
        .iter()
        .flat_map(|scanned| search.in_object(&scanned.path, &scanned.mapped))
        .collect()
}

// ------------------------------------------------------------ the prepare

/// Whether this request's calls are counted into rows in the kernel rather
/// than published one by one: `--emit hist` alone, in trace mode, keyed by
/// nothing but the thread, filtered only on what a row still says --
/// the function, as named or by its file, and the depth. Such a run has no
/// record per call to lose: what a function called millions of times a
/// second needs. Its events are rows, stamped when the drain read them.
pub(crate) fn builds_histogram_in_kernel(request: &crate::request::Request) -> bool {
    use crate::request::{Emit, HistScale, KeyKind};
    request.source.as_str() == "runtime.func"
        && request.mode == Mode::Trace
        && matches!(request.emit.as_slice(), [Emit::Hist(HistScale::Log2)])
        && request.keys.iter().all(|key| *key == KeyKind::Task)
        && request.filter.as_ref().is_none_or(|filter| {
            filter
                .fields()
                .iter()
                .all(|field| ["function", "signature", "object", "depth"].contains(field))
        })
}

pub struct Prepare {
    /// Calls counted into rows in the kernel ([`builds_histogram_in_kernel`]).
    pub histogram: bool,
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
        // Timed from the maps to the last symbol table, because at `--pid`
        // it is time the program runs before any probe is placed.
        let searching = std::time::Instant::now();
        let objects = match &self.target {
            Target::Launch(argv) => objects::of_launch(argv)?,
            _ => objects::of_process(process)?,
        };
        let search = Search::of(self.filter.as_ref())?;
        let timed = chosen(&search, &objects);
        // A launched program may name its functions in a library it has not
        // loaded yet -- a plugin it opens with `dlopen` -- so finding none
        // before it runs is a question for the end of the run, not a refusal.
        let pending = if timed.is_empty() {
            let refusal = search.not_found(&objects);
            if !launched {
                return Err(refusal);
            }
            Some(refusal)
        } else {
            None
        };
        if timed.len() > MAX_FUNCTIONS {
            return Err(too_many(timed.len()));
        }
        let symbol_time = searching.elapsed();
        let mut searched_paths = BTreeSet::new();
        let mut searched_files = BTreeSet::new();
        for scanned in &objects.scanned {
            let mapped = if launched {
                std::fs::canonicalize(&scanned.mapped).unwrap_or_else(|_| scanned.mapped.clone())
            } else {
                scanned.mapped.clone()
            };
            searched_paths.insert(mapped);
            if let Some(file) = file_identity(&scanned.path) {
                searched_files.insert(file);
            }
        }
        Ok(Box::new(FuncCollector {
            histogram: self.histogram,
            filter: self.filter,
            pid: process,
            only_tid,
            launched,
            timed,
            objects,
            symbol_time,
            search,
            searched_paths,
            searched_files,
            pending,
            late: Vec::new(),
            late_trouble: Vec::new(),
            started: None,
            last_rescan: None,
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

/// How often the target's maps are read for a library it mapped since:
/// the most a call of a function in a `dlopen`ed library can go untimed,
/// beyond the time it takes to search the library and place the probes.
const RESCAN_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// A file as `stat` names it, so the same file reached by two paths -- the
/// launch's resolution and the path the dynamic linker mapped -- is probed
/// once and its calls are not timed twice.
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino()))
}

/// A library the target mapped after the probes were first placed, and how
/// late its functions were probed.
struct Late {
    object: String,
    functions: Vec<String>,
    /// When its probes were placed, from the start of the collection.
    probed_after: StdDuration,
    /// The most time between its mapping and its probes: since the read of
    /// the maps before the one that found it.
    at_most: StdDuration,
}

// ----------------------------------------------------------- the collector

pub struct FuncCollector {
    /// Calls counted into rows in the kernel, drained as rows.
    histogram: bool,
    filter: Option<FilterExpr>,
    pid: i32,
    only_tid: Option<u32>,
    launched: bool,
    timed: Vec<Timed>,
    objects: Objects,
    symbol_time: StdDuration,
    search: Search,
    /// Every path the target maps that has been searched, by the target's
    /// name for it, and every file searched, by `stat`.
    searched_paths: BTreeSet<std::path::PathBuf>,
    searched_files: BTreeSet<(u64, u64)>,
    /// Why nothing was found before a launched program ran: its failure, if
    /// nothing it maps while it runs has the functions either.
    pending: Option<String>,
    late: Vec<Late>,
    /// Libraries mapped later whose functions could not be probed, and why.
    late_trouble: Vec<String>,
    started: Option<std::time::Instant>,
    last_rescan: Option<std::time::Instant>,
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
    /// Reads the target's maps for objects it mapped since the last read,
    /// and probes the functions the filter names in them: a library a
    /// program opens with `dlopen` is not mapped when the run starts.
    fn rescan(&mut self) {
        let now = std::time::Instant::now();
        if self
            .last_rescan
            .is_some_and(|last| now.duration_since(last) < RESCAN_INTERVAL)
        {
            return;
        }
        let previous = self.last_rescan.replace(now);
        if self.program.is_none() {
            return;
        }
        // A launched program is a fork of this tool until its `execve`, and
        // what it maps then is this tool's own.
        if self.launched
            && file_identity(Path::new(&format!("/proc/{}/exe", self.pid)))
                == file_identity(Path::new("/proc/self/exe"))
        {
            return;
        }
        let Ok(maps) = std::fs::read_to_string(format!("/proc/{}/maps", self.pid)) else {
            return;
        };
        let mappings = crate::bpf::usdt::parse_maps(&maps);
        let fresh: Vec<std::path::PathBuf> = mappings
            .iter()
            .map(|mapping| mapping.path.clone())
            .filter(|path| !path.to_string_lossy().ends_with(" (deleted)"))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|path| !self.searched_paths.contains(path))
            .collect();
        for mapped in fresh {
            self.searched_paths.insert(mapped.clone());
            let identities: Vec<crate::stack::MappedIdentity> = mappings
                .iter()
                .filter(|mapping| mapping.path == mapped)
                .map(|mapping| crate::stack::MappedIdentity {
                    device: mapping.device,
                    inode: mapping.inode,
                    start: mapping.start,
                    end: mapping.end,
                })
                .collect();
            let reached = crate::stack::reach_mapped(self.pid, &mapped, &identities);
            let path = reached.path().to_owned();
            let Some(file) = file_identity(&path) else {
                continue;
            };
            if !self.searched_files.insert(file) {
                continue;
            }
            let found = self.search.in_object(&path, &mapped);
            if found.is_empty() {
                continue;
            }
            let object = found[0].object.clone();
            if self.timed.len() + found.len() > MAX_FUNCTIONS {
                self.late_trouble.push(format!(
                    "{object}, mapped while the program ran, has {} more function(s) the filter \
                     names, which would make more than {MAX_FUNCTIONS}: not probed",
                    found.len()
                ));
                continue;
            }
            let placed: Vec<Placed> = found.iter().map(|timed| timed.placed.clone()).collect();
            let Some(program) = self.program.as_mut() else {
                return;
            };
            match program.add(&placed) {
                Ok(()) => {
                    let started = self.started.unwrap_or(now);
                    self.late.push(Late {
                        object,
                        functions: found.iter().map(|timed| bare(&timed.function)).collect(),
                        probed_after: std::time::Instant::now().duration_since(started),
                        at_most: std::time::Instant::now()
                            .duration_since(previous.unwrap_or(started).min(now)),
                    });
                    self.timed.extend(found);
                }
                Err(error) => self.late_trouble.push(format!(
                    "{object}, mapped while the program ran, has functions the filter names, \
                     and their probes could not be placed: {error}"
                )),
            }
        }
    }

    fn drain(&mut self, seq: u64) -> Vec<Event> {
        let Some(program) = self.program.as_mut() else {
            return Vec::new();
        };
        if program.histogram() {
            return self.drain_rows(seq);
        }
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
                if !matches_call(filter, &event, &timed.function) {
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

impl FuncCollector {
    /// The rows the kernel counted since the last drain, each one event that
    /// stands for its `count` calls: `lat_ns` its bucket's low bound,
    /// `total_ns` their exact sum.
    fn drain_rows(&mut self, seq: u64) -> Vec<Event> {
        let Some(program) = self.program.as_ref() else {
            return Vec::new();
        };
        let drained = match program.drain_rows() {
            Ok(drained) => drained,
            Err(reason) => {
                self.losses.failed_reads += 1;
                self.losses.note_times(reason, 1);
                return Vec::new();
            }
        };
        if drained.losses.vanished > 0 || drained.losses.truncated {
            self.losses.undecoded += drained.losses.vanished;
            self.losses.note(
                "rows of the function histogram went missing between the drain's listing and \
                 its read",
            );
        }
        let lost = program.lost();
        let full = lost.rows.saturating_sub(self.lost.rows);
        if full > 0 {
            self.losses.dropped += full;
            self.losses.note_times(
                "the kernel's function histogram was full in an interval: these calls were \
                 timed and counted into no row",
                full,
            );
        }
        let open =
            lost.open.saturating_sub(self.lost.open) + lost.stamp.saturating_sub(self.lost.stamp);
        if open > 0 {
            self.losses.dropped += open;
            self.losses.note_times(
                "more calls were open at once than the map of open calls holds, so these were \
                 never timed",
                open,
            );
        }
        self.lost = lost;
        let window_ns = drained.closed_ns.saturating_sub(drained.opened_ns);
        let mut events = Vec::with_capacity(drained.rows.len());
        let mut index = seq;
        for row in &drained.rows {
            if self.only_tid.is_some_and(|tid| tid != row.tid) {
                continue;
            }
            let Some(timed) = self.timed.get(row.op as usize) else {
                self.losses.undecoded += row.count;
                continue;
            };
            let low = row.bucket_low_ns();
            let event = Event {
                ts_ns: drained.closed_ns,
                ts_err_ns: 0,
                kind: "runtime.func".to_owned(),
                keys: vec![KeyValue::task(row.tid as i32)],
                attrs: vec![
                    Attr::uint("lat_ns", low),
                    Attr::uint("count", row.count),
                    Attr::uint(crate::emit::WEIGHT_ATTR, row.count),
                    Attr::uint(crate::emit::SUM_NS_ATTR, row.total_ns),
                    Attr::uint("bucket", u64::from(row.bucket)),
                    Attr::uint("window_ns", window_ns),
                    Attr::text("function", bare(&timed.function)),
                    Attr::text("signature", timed.function.name.clone()),
                    Attr::text("symbol", timed.function.symbol.clone()),
                    Attr::text("object", timed.object.clone()),
                    Attr::uint("depth", u64::from(row.aux)),
                    Attr::text("comm", row.comm.clone()),
                    Attr::uint("tid", u64::from(row.tid)),
                    Attr::uint("pid", u64::from(row.tgid)),
                ],
                collector_id: CollectorId::unstamped(),
                seq: index,
            };
            if let Some(filter) = &self.filter {
                if !matches_call(filter, &event, &timed.function) {
                    self.filtered_out += row.count;
                    continue;
                }
            }
            self.calls += row.count;
            // A bucket is [low, 2 * low): every call in it was shorter than
            // the floor only if its high bound is.
            if low.saturating_mul(2) <= TRAP_FLOOR_NS {
                self.short += row.count;
            }
            if row.aux > 1 {
                self.nested += row.count;
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
        let program = FuncProgram::load(&placed, self.pid, RING_BYTES, self.histogram)?;
        self.fallback = program.fallback.clone();
        self.program = Some(program);
        self.started = Some(std::time::Instant::now());
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        self.rescan();
        if let Some(program) = &self.program {
            if program.fallback.is_some() && self.fallback.is_none() {
                self.fallback = program.fallback.clone();
            }
        }
        Ok(self.drain(seq))
    }

    fn failure(&self) -> Option<String> {
        if !self.timed.is_empty() {
            return None;
        }
        self.pending.as_ref().map(|refusal| {
            format!(
                "{refusal}\nNor did any of the {} object(s) it mapped while it ran, read every {} \
                 ms for a library it loaded with dlopen",
                self.searched_paths
                    .len()
                    .saturating_sub(self.objects.scanned.len()),
                RESCAN_INTERVAL.as_millis()
            )
        })
    }

    fn stop(&mut self) -> Vec<Event> {
        let trailing = self.drain(u64::MAX / 2);
        self.open_at_end = self.program.as_ref().map_or(0, FuncProgram::open);
        self.program = None;
        trailing
    }

    fn tick(&self) -> Option<StdDuration> {
        // A row drain waits for a grace period; rows fill no faster for it.
        Some(if self.histogram {
            ROWS_INTERVAL
        } else {
            DRAIN_INTERVAL
        })
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        notes.push(format!(
            "finding the functions in the symbol tables of the {} object(s) searched took {}, \
             before any probe was placed{}",
            self.objects.scanned.len(),
            crate::triage::span(self.symbol_time.as_nanos() as f64),
            if self.launched {
                ""
            } else {
                ": the process ran unmeasured for that long"
            }
        ));
        if self.histogram {
            notes.push(
                "the histogram was built in the kernel: each call was counted into a row of \
                 (thread, function, depth, log2 bucket) where it returned, with its exact time \
                 added to the row's total, and the rows were read every 100 ms -- no record \
                 per call, so a call is lost only to a full histogram, which is counted"
                    .to_owned(),
            );
        }
        notes.push(
            "`depth` counts the calls of one function open at once on one thread, so two \
             different functions both at depth 1 can still be one inside the other; it is not \
             a depth in the call stack"
                .to_owned(),
        );
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
            match self
                .late
                .iter()
                .map(|late| late.functions.len())
                .sum::<usize>()
            {
                0 => String::new(),
                late => format!(" ({late} of them probed late, in a library mapped later)"),
            }
        ));
        for late in &self.late {
            notes.push(format!(
                "{} was mapped after the probes were first placed (a dlopen): {} probed {} into \
                 the run, at most {} after it was mapped, and calls in between were not timed",
                late.object,
                late.functions.join(", "),
                crate::triage::span(late.probed_after.as_nanos() as f64),
                crate::triage::span(late.at_most.as_nanos() as f64),
            ));
        }
        notes.extend(self.late_trouble.iter().cloned());
        notes.push(format!(
            "searched: {}",
            self.objects
                .scanned
                .iter()
                .map(|scanned| scanned.mapped.display().to_string())
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
        let lambda = function(
            "lambda",
            "xla::cpu::CopyThunk::Execute(int)::$_6::operator()(long)",
        );
        assert!(!exact("xla::cpu::CopyThunk::Execute").takes(&lambda));
        assert_eq!(
            bare(&lambda),
            "xla::cpu::CopyThunk::Execute(int)::$_6::operator()"
        );
        assert!(exact("xla::cpu::CopyThunk::Execute(int)::$_6::operator()").takes(&lambda));
    }

    /// The mangled-name prefilter refuses only symbols the filter would have
    /// refused after demangling: over every symbol of the C++ libraries this
    /// machine has, the search finds the same functions with it and without.
    #[test]
    fn the_prefilter_never_drops_a_symbol_the_filter_takes() {
        let libraries: Vec<&Path> = [
            "/usr/lib/libstdc++.so.6",
            "/usr/lib/x86_64-linux-gnu/libstdc++.so.6",
            "/usr/lib64/libstdc++.so.6",
        ]
        .iter()
        .map(Path::new)
        .filter(|path| path.exists())
        .collect();
        if libraries.is_empty() {
            eprintln!("skipped: no libstdc++ to read");
            return;
        }
        let terms = [
            ("std::vector", true),
            ("td::vec", true),
            ("basic_string", true),
            ("std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> >::append", false),
            ("operator()", true),
            ("operator new", true),
            ("(anonymous namespace)", true),
            ("unsigned long", true),
            ("~", true),
            ("ios_base::Init", true),
            ("std::locale::locale", false),
            ("_ZNSt6locale5classicEv", false),
            ("Locale", true),
            ("[clone", true),
            ("const&", true),
            ("char_traits<wchar_t>", true),
        ];
        for path in libraries {
            // Demangled once; each term is then asked both ways in memory.
            let every = crate::stack::functions_in(path, &|_| true);
            for (term, substring) in terms {
                let wanted = Wanted {
                    term: term.to_owned(),
                    substring,
                };
                let lower = term.to_lowercase();
                let near =
                    |function: &FunctionSymbol| function.name.to_lowercase().contains(&lower);
                for function in &every {
                    if wanted.takes(function) {
                        assert!(
                            wanted.could(&function.symbol, false),
                            "{term} in {}: the prefilter drops {}",
                            path.display(),
                            function.symbol
                        );
                    }
                    if near(function) {
                        assert!(
                            wanted.could(&function.symbol, true),
                            "{term} in {}, without case: the prefilter drops {}",
                            path.display(),
                            function.symbol
                        );
                    }
                }
            }
        }
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
        for expression in [
            "function == 'compute(int)' && depth == 1",
            "function == '_Z7computei'",
            "function in ('other', '_Z7computei')",
            "function =~ '_Z7compute'",
        ] {
            assert!(
                matches_call(&filter(expression), &event, &timed.function),
                "{expression}"
            );
        }
        assert!(!matches_call(
            &filter("function == 'compute(int)' && depth == 2"),
            &event,
            &timed.function
        ));
        assert!(!matches_call(
            &filter("function != '_Z7computei'"),
            &event,
            &timed.function
        ));
    }
}
