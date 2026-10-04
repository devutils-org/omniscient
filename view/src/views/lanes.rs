//! The `lanes` view: threads side by side, one lane per thread and per GPU
//! stream, along one time axis.
//!
//! A thread's lane is rebuilt from two exact series of instants:
//! `sched.switches` says when the thread left a cpu, and `sched.wakeup` when
//! something woke it. A switch-out followed by a wakeup is a **block**, from
//! the one to the other; from a wakeup to the next switch-out the thread was
//! **awake** -- on a cpu, or queued for one, which these instants cannot tell
//! apart; a switch-out followed by another with no wakeup between was a
//! preemption, and the thread stayed awake (runnable) through it. Before a
//! thread's first instant and after its last nothing is drawn: nothing says
//! what it was doing.
//!
//! `sched.offcpu` does not give blocks one by one: its events are rows, one
//! per (thread, state, stack, log2 bucket) per drain window, stamped at the
//! drain. A block takes its state (sleeping, uninterruptible) and stack from
//! the row of its thread whose window holds its end and whose size is
//! nearest -- and says so when the match is not exact.
//!
//! A GPU stream's lane is `gpu.kernel`'s activities on it, each a start on
//! the device converted to `CLOCK_MONOTONIC` and a duration, so a kernel and
//! the host thread that waited for it are drawn one above the other.
//!
//! Every glyph is drawn in its source's trust style, a refused source's
//! reason where its lanes would be, and the view reads only the run it is
//! handed. `docs/view/lanes.md` says what it shows and what it does not.

use std::collections::BTreeMap;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};
use ratatui::Frame;

use super::{Action, Ctx, Focus, Need, Region, Scope, View};
use crate::model::{Data, Event, Refusal, Results};
use crate::trust::{self, palette, Trust};
use crate::units;

/// What this view needs measured.
pub const NEEDS: &[Need] = &[
    Need {
        source: "sched.switches",
        mode: "trace",
        emit: &["raw"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "each thread's switch-outs, the instant each block or preemption began",
    },
    Need {
        source: "sched.offcpu",
        mode: "trace",
        emit: &["raw", "stack"],
        keys: &[],
        stack: Some("fp"),
        scope: Scope::Target,
        why: "the state and stack of each block, from its drain window's rows",
    },
    Need {
        source: "gpu.kernel",
        mode: "trace",
        emit: &["raw", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "a lane per GPU stream",
    },
    Need {
        source: WAKEUP,
        mode: "trace",
        emit: &["raw"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "where each block ended, and who ended it",
    },
];

/// Where a block's end is read from: the same request as the wake view's,
/// so one collector serves both.
const WAKEUP: &str = "sched.wakeup";

/// The colours of the states, sixteen-colour names as the palette's.
const AWAKE: Color = Color::Green;
const SLEEPING: Color = Color::Blue;
const DISK: Color = Color::Yellow;
const KERNEL: Color = Color::Magenta;
const COPY: Color = Color::Cyan;

/// The name `omn`'s own threads inside a target start with (the injection
/// library's flush thread): drawn apart, since they are the tool's cost.
const OWN: &str = "omn-";

/// The width of a lane's name, and of the share beside it.
const LABEL: u16 = 22;
const SHARE: u16 = 11;
/// The rows of the detail under the lanes, at least.
const DETAIL: u16 = 5;

pub struct Lanes {
    /// The lane the keys act on, by its identity, so a lane keeps its
    /// selection while a live run adds lanes above it.
    selected: Option<LaneId>,
    /// The time the cursor is at; `None` until the first draw puts it on the
    /// run's most telling moment.
    cursor: Option<u64>,
    /// The span of time drawn; `None` is the whole run.
    window: Option<(u64, u64)>,
    /// The first lane drawn, when there are more lanes than rows.
    scroll: usize,
    /// The width of a cell at the last draw, in nanoseconds, for the keys.
    cell_ns: u64,
    cache: Option<(CacheKey, Model)>,
    /// The lanes as last drawn, each its index among the focus's lanes.
    regions: Vec<Region>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CacheKey {
    data: usize,
    closed_ns: u64,
    events: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum LaneId {
    Thread(i32),
    Gpu { pid: i32, device: u64, stream: u64 },
}

/// Everything the lanes draw, built once per run (and per live update).
#[derive(Debug, Default)]
struct Model {
    opened_ns: u64,
    closed_ns: u64,
    threads: Vec<Thread>,
    gpus: Vec<Stream>,
    /// Whether `sched.wakeup` was in the run: without it a switch-out cannot
    /// be told a block from a preemption.
    wakeups: bool,
    /// The instants' trust: `sched.switches` and `sched.wakeup`, which say
    /// where every span starts and ends.
    thread_trust: Trust,
    /// `sched.offcpu`'s, which says each block's state, stack and queueing.
    offcpu_trust: Trust,
    /// A block's: its ends from the instants, its colour from `sched.offcpu`.
    block_trust: Trust,
    gpu_trust: Trust,
    /// What each source holds, for the status line.
    held: Vec<Held>,
    /// `gpu.kernel`'s refusal, drawn where its lanes would be.
    gpu_refused: Option<Refusal>,
    /// The thread sources' refusals, said where the lanes would be.
    thread_refused: Vec<Refusal>,
}

#[derive(Debug)]
struct Held {
    source: &'static str,
    events: usize,
    trust: Trust,
    served: bool,
    /// The emits it was asked as, when it is in the run without `raw`.
    emits: Vec<String>,
}

#[derive(Debug, Default)]
struct Thread {
    tid: i32,
    pid: i32,
    comm: String,
    spans: Vec<Spell>,
    switches: u64,
    wakeups: u64,
    /// Where it blocked over the whole run, from `sched.offcpu`'s rows: by
    /// state and stack, the most time first.
    waits: Vec<Wait>,
}

/// One state and stack a thread blocked in, summed over the run.
#[derive(Debug, Clone)]
struct Wait {
    state: String,
    frames: Vec<String>,
    total_ns: u64,
    blocks: u64,
}

/// One stretch of a thread's time between two instants.
#[derive(Debug, Clone)]
struct Spell {
    start: u64,
    end: u64,
    state: State,
    /// The cpu the thread left at the start, for a span that starts at a
    /// switch-out.
    cpu: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    /// From a wakeup, or a preemption, to the next switch-out: on a cpu or
    /// queued for one.
    Awake,
    /// From a switch-out to the wakeup that ended it.
    Blocked(Box<Blocked>),
    /// Between two switch-outs, in a run without wakeups: a block or a
    /// preemption, which nothing here tells apart.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Blocked {
    /// Who woke it: a thread id, or 0 for an interrupt, with its context.
    waker_tid: Option<i64>,
    waker_context: String,
    waker_comm: String,
    /// The state and stack of the `sched.offcpu` row it was matched to.
    state: String,
    frames: Vec<String>,
    /// Whether the row is this block alone (one block, of this length, and
    /// no other row of its window could be).
    exact: bool,
    /// The other rows of its window that could hold it as well: near in
    /// size, so which one is this block cannot be told.
    rivals: usize,
    queued_ns: Option<u64>,
}

impl Blocked {
    fn disk(&self) -> bool {
        self.state == "uninterruptible"
    }

    fn colour(&self) -> Color {
        if self.disk() {
            DISK
        } else {
            SLEEPING
        }
    }
}

#[derive(Debug, Default)]
struct Stream {
    pid: i32,
    device: u64,
    stream: u64,
    ops: Vec<Op>,
    ts_err_ns: u64,
}

#[derive(Debug, Clone)]
struct Op {
    start: u64,
    end: u64,
    name: String,
    copy: bool,
    bytes: Option<u64>,
    grid: String,
    block: String,
}

impl Stream {
    /// The lane's name: the device, the stream and what took most of its time.
    fn label(&self) -> String {
        let mut time: BTreeMap<&str, u64> = BTreeMap::new();
        for op in &self.ops {
            let name = if op.copy { "copies" } else { op.name.as_str() };
            *time.entry(name).or_default() += op.end - op.start;
        }
        let top = time
            .iter()
            .max_by_key(|(name, ns)| (**ns, std::cmp::Reverse(**name)))
            .map(|(name, _)| *name)
            .unwrap_or("");
        format!("gpu{} s{} {top}", self.device, self.stream)
    }
}

impl Model {
    fn build(data: &Data) -> Model {
        let mut model = Model {
            opened_ns: data.opened_ns(),
            closed_ns: data.closed_ns(),
            ..Model::default()
        };
        let switches = data.for_need("sched.switches", "sched.switches");
        let offcpu = data.for_need("sched.offcpu", "sched.offcpu");
        let wakeups = data.for_need(WAKEUP, WAKEUP);
        let kernels = data.for_need("gpu.kernel", "gpu.kernel");
        model.wakeups = wakeups.iter().any(|results| has_raw(results));
        for (source, held) in [
            ("sched.switches", &switches),
            (WAKEUP, &wakeups),
            ("sched.offcpu", &offcpu),
            ("gpu.kernel", &kernels),
        ] {
            model.held.push(Held {
                source,
                events: held.iter().map(|results| results.events().len()).sum(),
                trust: Trust::joined(data, held.iter().copied()),
                served: held.iter().any(|results| has_raw(results)),
                emits: held
                    .iter()
                    .flat_map(|results| {
                        results
                            .outputs
                            .iter()
                            .map(|output| output.emit().to_owned())
                    })
                    .collect(),
            });
            if let Some(refusal) = data.refusal(source) {
                if source == "gpu.kernel" {
                    model.gpu_refused = Some(refusal.clone());
                } else {
                    model.thread_refused.push(refusal.clone());
                }
            }
        }
        model.thread_trust = Trust::joined(data, switches.iter().chain(wakeups.iter()).copied());
        model.offcpu_trust = Trust::joined(data, offcpu.iter().copied());
        model.block_trust = model.thread_trust.clone();
        model.block_trust.merge(model.offcpu_trust.clone());
        model.gpu_trust = Trust::joined(data, kernels.iter().copied());
        model.threads = threads(&switches, &wakeups, &offcpu);
        model.gpus = streams(&kernels);
        if model.closed_ns <= model.opened_ns {
            // A run whose windows say nothing: the lanes' own span.
            let instants = model
                .threads
                .iter()
                .flat_map(|thread| thread.spans.iter().flat_map(|span| [span.start, span.end]))
                .chain(
                    model
                        .gpus
                        .iter()
                        .flat_map(|gpu| gpu.ops.iter().flat_map(|op| [op.start, op.end])),
                );
            let (low, high) =
                instants.fold((u64::MAX, 0), |(low, high), t| (low.min(t), high.max(t)));
            if low < high {
                model.opened_ns = low;
                model.closed_ns = high;
            }
        }
        model
    }

    /// The lanes the focus leaves, process by process: its GPU streams
    /// first, right above the thread that most often launches on them, the
    /// main one; then its threads.
    fn lanes(&self, focus: &Focus) -> Vec<Lane<'_>> {
        let keep_pid = |pid: i32| focus.pid.is_none_or(|wanted| wanted == pid);
        let threads: Vec<&Thread> = self
            .threads
            .iter()
            .filter(|thread| keep_pid(thread.pid))
            .filter(|thread| focus.tid.is_none_or(|tid| tid == thread.tid))
            .collect();
        let gpus: Vec<&Stream> = self
            .gpus
            .iter()
            .filter(|gpu| keep_pid(gpu.pid))
            .filter(|gpu| focus.tid.is_none() || threads.iter().any(|thread| thread.pid == gpu.pid))
            .collect();
        let mut lanes = Vec::new();
        let mut placed = Vec::new();
        for thread in &threads {
            if !placed.contains(&thread.pid) {
                placed.push(thread.pid);
                lanes.extend(
                    gpus.iter()
                        .filter(|gpu| gpu.pid == thread.pid)
                        .map(|gpu| Lane::Gpu(gpu)),
                );
            }
            lanes.push(Lane::Thread(thread));
        }
        lanes.extend(
            gpus.iter()
                .filter(|gpu| !placed.contains(&gpu.pid))
                .map(|gpu| Lane::Gpu(gpu)),
        );
        lanes
    }
}

fn has_raw(results: &Results) -> bool {
    results.output("raw").is_some()
}

#[derive(Debug, Clone, Copy)]
enum Lane<'a> {
    Thread(&'a Thread),
    Gpu(&'a Stream),
}

impl Lane<'_> {
    fn id(&self) -> LaneId {
        match self {
            Lane::Thread(thread) => LaneId::Thread(thread.tid),
            Lane::Gpu(gpu) => LaneId::Gpu {
                pid: gpu.pid,
                device: gpu.device,
                stream: gpu.stream,
            },
        }
    }

    /// Every instant a span starts or ends at, in order.
    fn edges(&self) -> Vec<u64> {
        let mut edges: Vec<u64> = match self {
            Lane::Thread(thread) => thread
                .spans
                .iter()
                .flat_map(|span| [span.start, span.end])
                .collect(),
            Lane::Gpu(gpu) => gpu.ops.iter().flat_map(|op| [op.start, op.end]).collect(),
        };
        edges.sort_unstable();
        edges.dedup();
        edges
    }
}

fn text(event: &Event, name: &str) -> Option<String> {
    event.attr(name).map(|value| value.to_string())
}

fn uint(event: &Event, name: &str) -> Option<u64> {
    event
        .number(name)
        .filter(|value| *value >= 0.0)
        .map(|value| value as u64)
}

/// A `sched.offcpu` row, as the matching reads it: the blocks of one thread,
/// state, stack and size that ended between `opened` and `closed`.
struct Row<'a> {
    closed: u64,
    opened: u64,
    blocks: u64,
    blocked_ns: u64,
    queued_ns: u64,
    state: String,
    stack: Option<u64>,
    results: &'a Results,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Out,
    Woken,
}

struct Instant<'a> {
    ts: u64,
    kind: Kind,
    event: &'a Event,
}

/// The thread lanes: each thread's switch-outs and wakeups, in order, made
/// into spans, each block matched to its `sched.offcpu` row.
fn threads(switches: &[&Results], wakeups: &[&Results], offcpu: &[&Results]) -> Vec<Thread> {
    let mut by_tid: BTreeMap<i32, (Vec<Instant<'_>>, Thread)> = BTreeMap::new();
    for results in switches {
        for event in results.events() {
            let Some(tid) = event.tid() else { continue };
            let (instants, thread) = by_tid.entry(tid).or_default();
            instants.push(Instant {
                ts: event.ts_ns,
                kind: Kind::Out,
                event,
            });
            thread.switches += event
                .number("count")
                .map_or(1, |count| count.max(1.0) as u64);
            if let Some(pid) = uint(event, "pid") {
                thread.pid = pid as i32;
            }
        }
    }
    // A wakeup counts only for a thread whose switches are in the run: a
    // wakeup the target issued to a thread outside it is the wakee's, and
    // the wakee has no lane.
    for results in wakeups {
        for event in results.events() {
            let Some(tid) = event.tid() else { continue };
            if let Some((instants, thread)) = by_tid.get_mut(&tid) {
                instants.push(Instant {
                    ts: event.ts_ns,
                    kind: Kind::Woken,
                    event,
                });
                thread.wakeups += 1;
            }
        }
    }
    let rows = offcpu_rows(offcpu);
    let comms = comms(switches, wakeups);
    let have_wakeups = wakeups.iter().any(|results| has_raw(results));
    let none = Vec::new();
    let mut out = Vec::new();
    for (tid, (mut instants, mut thread)) in by_tid {
        instants.sort_by_key(|instant| (instant.ts, instant.kind));
        thread.tid = tid;
        if thread.pid == 0 {
            thread.pid = tid;
        }
        thread.comm = comms.get(&tid).cloned().unwrap_or_else(|| "?".to_owned());
        let rows = rows.get(&tid).unwrap_or(&none);
        thread.waits = waits(rows);
        let mut index = 0;
        while index + 1 < instants.len() {
            let here = &instants[index];
            match here.kind {
                Kind::Woken => {
                    // Awake until the next switch-out; a wakeup between is of
                    // a thread already awake.
                    match instants[index + 1..]
                        .iter()
                        .position(|instant| instant.kind == Kind::Out)
                    {
                        Some(offset) => {
                            let next = index + 1 + offset;
                            push(
                                &mut thread.spans,
                                here.ts,
                                instants[next].ts,
                                State::Awake,
                                None,
                            );
                            index = next;
                        }
                        None => break,
                    }
                }
                Kind::Out => {
                    let next = &instants[index + 1];
                    let cpu = uint(here.event, "cpu");
                    let state = match next.kind {
                        Kind::Woken => State::Blocked(Box::new(block(
                            here.ts, next.ts, next.event, rows, &comms,
                        ))),
                        Kind::Out if have_wakeups => State::Awake,
                        Kind::Out => State::Unknown,
                    };
                    push(&mut thread.spans, here.ts, next.ts, state, cpu);
                    index += 1;
                }
            }
        }
        out.push(thread);
    }
    // Process by process, its main thread first, then its threads by the
    // time they were awake.
    out.sort_by_key(|thread| {
        let awake: u64 = thread
            .spans
            .iter()
            .filter(|span| span.state == State::Awake)
            .map(|span| span.end - span.start)
            .sum();
        (
            thread.pid,
            thread.tid != thread.pid,
            std::cmp::Reverse(awake),
            std::cmp::Reverse(thread.switches),
            thread.tid,
        )
    });
    out
}

/// A thread's rows summed by state and stack.
fn waits(rows: &[Row<'_>]) -> Vec<Wait> {
    let mut summed: BTreeMap<(String, Vec<String>), (u64, u64)> = BTreeMap::new();
    for row in rows {
        let frames = row
            .stack
            .map(|stack| row.results.frames(stack).to_vec())
            .unwrap_or_default();
        let entry = summed.entry((row.state.clone(), frames)).or_default();
        entry.0 += row.blocked_ns + row.queued_ns;
        entry.1 += row.blocks;
    }
    let mut waits: Vec<Wait> = summed
        .into_iter()
        .map(|((state, frames), (total_ns, blocks))| Wait {
            state,
            frames,
            total_ns,
            blocks,
        })
        .collect();
    waits.sort_by_key(|wait| std::cmp::Reverse(wait.total_ns));
    waits
}

/// Adds a span, joining it to the last when both are awake and touch.
fn push(spans: &mut Vec<Spell>, start: u64, end: u64, state: State, cpu: Option<u64>) {
    if end <= start {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if last.end == start && last.state == State::Awake && state == State::Awake {
            last.end = end;
            return;
        }
    }
    spans.push(Spell {
        start,
        end,
        state,
        cpu,
    });
}

/// Each thread's last name: a thread named after it started is first seen
/// under its parent's.
fn comms(switches: &[&Results], wakeups: &[&Results]) -> BTreeMap<i32, String> {
    let mut named: BTreeMap<i32, (u64, String)> = BTreeMap::new();
    for results in switches.iter().chain(wakeups) {
        for event in results.events() {
            let (Some(tid), Some(comm)) = (event.tid(), text(event, "comm")) else {
                continue;
            };
            let entry = named.entry(tid).or_insert((0, String::new()));
            if event.ts_ns >= entry.0 {
                *entry = (event.ts_ns, comm);
            }
        }
    }
    named
        .into_iter()
        .map(|(tid, (_, comm))| (tid, comm))
        .collect()
}

fn offcpu_rows<'a>(offcpu: &[&'a Results]) -> BTreeMap<i32, Vec<Row<'a>>> {
    let mut rows: BTreeMap<i32, Vec<Row<'a>>> = BTreeMap::new();
    for results in offcpu {
        for event in results.events() {
            let Some(tid) = event.tid() else { continue };
            let window = uint(event, "window_ns").unwrap_or(0);
            rows.entry(tid).or_default().push(Row {
                closed: event.ts_ns,
                opened: event.ts_ns.saturating_sub(window),
                blocks: uint(event, "blocks").unwrap_or(1).max(1),
                blocked_ns: uint(event, "blocked_ns").unwrap_or(0),
                queued_ns: uint(event, "queued_ns").unwrap_or(0),
                state: text(event, "state").unwrap_or_default(),
                stack: uint(event, "stack_id"),
                results,
            });
        }
    }
    rows
}

/// A block from `start` to its wakeup, with its waker and the `sched.offcpu`
/// row that holds it: the thread's row whose window holds the block's end
/// (or the next window's, when the drain came between the wakeup and the
/// switch back in) and whose mean block is nearest in size.
fn block(
    start: u64,
    woken: u64,
    wakeup: &Event,
    rows: &[Row<'_>],
    comms: &BTreeMap<i32, String>,
) -> Blocked {
    let waker_tid = wakeup
        .number("waker_tid")
        .filter(|value| value.is_finite())
        .map(|value| value as i64);
    let waker_comm = waker_tid
        .filter(|tid| *tid > 0)
        .and_then(|tid| comms.get(&(tid as i32)).cloned())
        .unwrap_or_default();
    let mut blocked = Blocked {
        waker_tid,
        waker_context: text(wakeup, "waker_context").unwrap_or_default(),
        waker_comm,
        ..Blocked::default()
    };
    let length = (woken - start) as f64;
    let mut candidates: Vec<&Row<'_>> = rows
        .iter()
        .filter(|row| row.opened < woken && woken <= row.closed)
        .collect();
    if candidates.is_empty() {
        let next = rows
            .iter()
            .filter(|row| row.closed >= woken)
            .map(|row| row.closed)
            .min();
        candidates = rows.iter().filter(|row| Some(row.closed) == next).collect();
    }
    let distance = |row: &Row<'_>| {
        let mean = row.blocked_ns as f64 / row.blocks as f64;
        (mean.max(1.0).log2() - length.max(1.0).log2()).abs()
    };
    // A row that could be this block too: one block within an eighth of a
    // doubling of it, or several whose mean is within a doubling (a row's
    // blocks spread over its log2 bucket, so one of them may be this one).
    let could_hold = |row: &Row<'_>| {
        if row.blocks == 1 {
            distance(row) < 0.125
        } else {
            distance(row) < 1.0
        }
    };
    if let Some(row) = candidates
        .iter()
        .copied()
        .min_by(|one, two| distance(one).total_cmp(&distance(two)))
    {
        blocked.rivals = candidates
            .iter()
            .filter(|other| !std::ptr::eq(**other, row) && could_hold(other))
            .count();
        // Within an eighth of a doubling of the one block a row holds, and
        // no other row of the window could hold it: that block, as near as
        // the switch's clock and the wakeup's let it be.
        blocked.exact = row.blocks == 1 && distance(row) < 0.125 && blocked.rivals == 0;
        blocked.state = row.state.clone();
        blocked.frames = row
            .stack
            .map(|stack| row.results.frames(stack).to_vec())
            .unwrap_or_default();
        if blocked.exact {
            blocked.queued_ns = Some(row.queued_ns);
        }
    }
    blocked
}

/// The GPU lanes: one per process, device and stream.
fn streams(kernels: &[&Results]) -> Vec<Stream> {
    let mut by_stream: BTreeMap<(i32, u64, u64), Stream> = BTreeMap::new();
    for results in kernels {
        for event in results.events() {
            let pid = uint(event, "pid")
                .map(|pid| pid as i32)
                .or_else(|| event.pid())
                .unwrap_or(0);
            let device = uint(event, "device").unwrap_or(0);
            let stream = uint(event, "stream").unwrap_or(0);
            let lane = by_stream
                .entry((pid, device, stream))
                .or_insert_with(|| Stream {
                    pid,
                    device,
                    stream,
                    ..Stream::default()
                });
            let op = text(event, "op").unwrap_or_default();
            lane.ts_err_ns = lane.ts_err_ns.max(event.ts_err_ns);
            lane.ops.push(Op {
                start: event.ts_ns,
                end: event.ts_ns + uint(event, "duration_ns").unwrap_or(0),
                name: text(event, "kernel").unwrap_or_else(|| op.clone()),
                copy: op == "memcpy" || op == "memset",
                bytes: uint(event, "bytes"),
                grid: text(event, "grid").unwrap_or_default(),
                block: text(event, "block").unwrap_or_default(),
            });
        }
    }
    let mut lanes: Vec<Stream> = by_stream.into_values().collect();
    for lane in &mut lanes {
        lane.ops.sort_by_key(|op| op.start);
    }
    lanes
}

/// What one cell of a lane holds: the time of each state in it.
#[derive(Debug, Default, Clone, Copy)]
struct Cell {
    awake: u64,
    sleeping: u64,
    disk: u64,
    unknown: u64,
    kernel: u64,
    copy: u64,
}

fn overlap(start: u64, end: u64, low: u64, high: u64) -> u64 {
    end.min(high).saturating_sub(start.max(low))
}

/// A lane cut into `count` cells from `low` to `high`.
fn cells(lane: &Lane<'_>, low: u64, high: u64, count: usize) -> Vec<Cell> {
    let mut cells = vec![Cell::default(); count];
    if count == 0 || high <= low {
        return cells;
    }
    let width = (high - low) as f64 / count as f64;
    let column = |t: u64| ((t.saturating_sub(low) as f64 / width) as usize).min(count - 1);
    let mut add = |start: u64, end: u64, put: &dyn Fn(&mut Cell, u64)| {
        if end <= low || start >= high {
            return;
        }
        for (index, cell) in cells
            .iter_mut()
            .enumerate()
            .take(column(end.min(high) - 1) + 1)
            .skip(column(start.max(low)))
        {
            let cell_low = low + (index as f64 * width) as u64;
            let cell_high = low + ((index + 1) as f64 * width) as u64;
            // An activity shorter than a nanosecond's rounding still counts.
            put(cell, overlap(start, end, cell_low, cell_high).max(1));
        }
    };
    match lane {
        Lane::Thread(thread) => {
            for span in &thread.spans {
                add(span.start, span.end, &|cell, ns| match &span.state {
                    State::Awake => cell.awake += ns,
                    State::Blocked(blocked) if blocked.disk() => cell.disk += ns,
                    State::Blocked(_) => cell.sleeping += ns,
                    State::Unknown => cell.unknown += ns,
                });
            }
        }
        Lane::Gpu(gpu) => {
            for op in &gpu.ops {
                add(op.start, op.end.max(op.start + 1), &|cell, ns| {
                    if op.copy {
                        cell.copy += ns
                    } else {
                        cell.kernel += ns
                    }
                });
            }
        }
    }
    cells
}

const EIGHTHS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// A share of a cell as a bar's height: anything there is at least an eighth.
fn bar(share: f64) -> &'static str {
    let eighths = (share * 8.0).ceil().clamp(1.0, 8.0) as usize;
    EIGHTHS[eighths - 1]
}

/// The awake row of a tall lane: the share of the cell awake.
fn awake_glyph(cell: &Cell, width_ns: f64) -> (&'static str, Option<Color>) {
    if cell.awake > 0 {
        (bar(cell.awake as f64 / width_ns), Some(AWAKE))
    } else if cell.unknown > 0 {
        ("┄", None)
    } else {
        (" ", None)
    }
}

/// The blocked row of a tall lane: the share of the cell blocked, in the
/// colour of the state that held most of it.
fn blocked_glyph(cell: &Cell, width_ns: f64) -> (&'static str, Option<Color>) {
    let blocked = cell.sleeping + cell.disk;
    if blocked == 0 {
        return (" ", None);
    }
    let colour = if cell.disk >= cell.sleeping {
        DISK
    } else {
        SLEEPING
    };
    (bar(blocked as f64 / width_ns), Some(colour))
}

/// The glyph and colour of one cell of a one-row lane.
fn glyph(cell: &Cell, width_ns: f64) -> (&'static str, Option<Color>) {
    let share = |ns: u64| ns as f64 / width_ns;
    if cell.kernel + cell.copy > 0 {
        let colour = if cell.kernel >= cell.copy {
            KERNEL
        } else {
            COPY
        };
        return (bar(share(cell.kernel + cell.copy)), Some(colour));
    }
    // A sliver of awake time is still drawn: a thread that woke for 50 µs in
    // a 30 ms cell did wake.
    if cell.awake > 0 {
        return (bar(share(cell.awake)), Some(AWAKE));
    }
    if cell.disk > 0 && cell.disk >= cell.sleeping {
        return ("━", Some(DISK));
    }
    if cell.sleeping > 0 {
        return ("─", Some(SLEEPING));
    }
    if cell.unknown > 0 {
        return ("┄", None);
    }
    (" ", None)
}

/// A tick spacing of 1, 2 or 5 times a power of ten that keeps labels apart.
fn tick(span_ns: u64, columns: u16) -> u64 {
    let wanted = span_ns as f64 / (f64::from(columns) / 12.0).max(1.0);
    let mut step = 1u64;
    loop {
        for factor in [1, 2, 5] {
            if (step * factor) as f64 >= wanted {
                return step * factor;
            }
        }
        if step > u64::MAX / 100 {
            return step;
        }
        step *= 10;
    }
}

impl Lanes {
    fn model(&mut self, data: &Data) {
        let key = CacheKey {
            data: std::ptr::from_ref(data) as usize,
            closed_ns: data.closed_ns(),
            events: data
                .run
                .results
                .iter()
                .map(|results| results.events().len())
                .sum(),
        };
        if !matches!(&self.cache, Some((held, _)) if *held == key) {
            self.cache = Some((key, Model::build(data)));
        }
    }

    /// The span drawn: the zoom, or the whole run.
    fn span(&self, model: &Model) -> (u64, u64) {
        let (low, high) = self.window.unwrap_or((model.opened_ns, model.closed_ns));
        (low, high.max(low + 1))
    }

    /// Puts the cursor and the selection where the run says most, the first
    /// time: on a host thread's wait for the GPU -- a block that a kernel or a
    /// copy fills, start to end, the longest such -- or, where no GPU ran, on
    /// the longest block of the first lane, the program's main thread.
    fn settle(&mut self, focus: &Focus) {
        let Some((_, model)) = &self.cache else {
            return;
        };
        let lanes = model.lanes(focus);
        if self.cursor.is_some()
            && self
                .selected
                .as_ref()
                .is_some_and(|selected| lanes.iter().any(|lane| lane.id() == *selected))
        {
            return;
        }
        let ops: Vec<&Op> = lanes
            .iter()
            .filter_map(|lane| match lane {
                Lane::Gpu(gpu) => Some(gpu),
                Lane::Thread(_) => None,
            })
            .flat_map(|gpu| gpu.ops.iter())
            .collect();
        let blocks = |lane: &Lane<'_>| -> Vec<Spell> {
            match lane {
                Lane::Thread(thread) => thread
                    .spans
                    .iter()
                    .filter(|span| matches!(span.state, State::Blocked(_)))
                    .cloned()
                    .collect(),
                Lane::Gpu(_) => Vec::new(),
            }
        };
        // A wait for the GPU: the block ended as an activity did -- within
        // a tenth of its length, or a millisecond -- and the activity ran
        // through most of it.
        let waited = |span: &Spell| -> u64 {
            ops.iter()
                .filter(|op| op.end <= span.end)
                .filter(|op| span.end - op.end <= ((op.end - op.start) / 10).max(1_000_000))
                .map(|op| overlap(op.start, op.end, span.start, span.end))
                .filter(|held| *held * 5 >= (span.end - span.start) * 4)
                .max()
                .unwrap_or(0)
        };
        // The first lane, in the lanes' order, that waited for the GPU, at
        // its longest such wait: the program's main thread, most often.
        let gpu_wait = lanes.iter().find_map(|lane| {
            blocks(lane)
                .into_iter()
                .map(|span| (waited(&span), span))
                .filter(|(waited, _)| *waited > 0)
                .max_by_key(|(waited, _)| *waited)
                .map(|(_, span)| (lane.id(), span))
        });
        let chosen = gpu_wait.or_else(|| {
            let first = lanes.first()?;
            blocks(first)
                .into_iter()
                .max_by_key(|span| span.end - span.start)
                .map(|span| (first.id(), span))
        });
        match chosen {
            Some((id, span)) => {
                self.cursor = Some(span.start + (span.end - span.start) / 2);
                self.selected = Some(id);
            }
            None => {
                if self.cursor.is_none() {
                    self.cursor = Some(model.opened_ns + (model.closed_ns - model.opened_ns) / 2);
                }
                self.selected = lanes.first().map(Lane::id);
            }
        }
    }

    fn draw_lanes(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        self.model(ctx.data);
        self.settle(ctx.focus);
        let Some((_, model)) = &self.cache else {
            return;
        };
        let (low, high) = self.span(model);
        let lanes = model.lanes(ctx.focus);
        let cursor = self.cursor.unwrap_or(low);
        let selected = self
            .selected
            .as_ref()
            .and_then(|id| lanes.iter().position(|lane| lane.id() == *id));
        let dim = Style::new().add_modifier(Modifier::DIM);

        // The status, the ruler, the lanes, a rule, the detail, the legend.
        let status = status_line(model);
        let status_rows = if status.width() > usize::from(area.width) {
            2
        } else {
            1
        };
        let status = Paragraph::new(status).wrap(Wrap { trim: true });
        let fixed = status_rows + 1 + 1 + DETAIL + 1;
        if area.height <= fixed || area.width < LABEL + SHARE + 16 {
            frame.render_widget(
                Paragraph::new("too small for the lanes: widen or heighten the terminal"),
                area,
            );
            return;
        }
        let x_time = area.x + LABEL + 1;
        let columns = area.width - LABEL - SHARE - 2;
        let width_ns = (high - low) as f64 / f64::from(columns);
        let cursor_column = (cursor >= low && cursor < high)
            .then(|| (((cursor - low) as f64 / width_ns) as u16).min(columns - 1));

        let mut y = area.y;
        frame.render_widget(status, Rect::new(area.x, y, area.width, status_rows));
        y += status_rows;
        if lanes.is_empty() {
            let mut lines = vec![
                Line::default(),
                Line::from(Span::styled(
                    "No thread or GPU stream in this run to draw a lane of. What it holds:",
                    Style::new().add_modifier(Modifier::ITALIC),
                )),
            ];
            lines.extend(held_lines(model));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                if ctx.focus.is_empty() {
                    "A run `omn top` makes of a program (`omn top -- prog`, `--pid`) asks for \
                     each of them."
                        .to_owned()
                } else {
                    format!(
                        "The screen is narrowed to {}: `f` clears it.",
                        ctx.focus.describe()
                    )
                },
                Style::new().add_modifier(Modifier::DIM),
            )));
            for refusal in model.thread_refused.iter().chain(model.gpu_refused.iter()) {
                lines.push(Line::default());
                lines.extend(trust::refusal_lines(refusal));
            }
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: true }),
                Rect::new(area.x, y, area.width, area.y + area.height - y),
            );
            return;
        }
        let buffer = frame.buffer_mut();
        ruler(
            buffer,
            Rect::new(x_time, y, columns, 1),
            model.opened_ns,
            (low, high),
        );
        let scale = if self.window.is_some() {
            format!("zoom: {} a cell", units::duration(width_ns))
        } else {
            format!("{} a cell", units::duration(width_ns))
        };
        buffer.set_stringn(area.x, y, scale, usize::from(LABEL), dim);
        buffer.set_string(
            x_time + columns + 1,
            y,
            format!("{:>width$}", "of the span", width = usize::from(SHARE)),
            dim,
        );
        y += 1;

        // The refused sources take rows of their own under the lanes, and
        // the detail takes what the lanes leave.
        let refusals: Vec<&Refusal> = model
            .thread_refused
            .iter()
            .chain(model.gpu_refused.iter())
            .collect();
        let room = area.height - fixed;
        let refusal_rows = if refusals.is_empty() || room < 8 {
            0
        } else {
            (4 * refusals.len() as u16).min(room / 2)
        };
        // Two rows a thread -- awake above, blocked below -- for the threads
        // that did something (awake a hundredth of the span or more), as
        // many as the room allows, in the lanes' order; one row for the
        // rest, the larger state drawn: a thread asleep throughout is a line.
        let active = |lane: &Lane<'_>| {
            matches!(lane, Lane::Thread(_)) && lane_share(lane, low, high).0 >= 0.01
        };
        let threads = lanes.iter().filter(|lane| active(lane)).count() as u16;
        let free = room - refusal_rows;
        let tall = threads.min(free.saturating_sub(DETAIL + lanes.len() as u16));
        let wanted = lanes.len() as u16 + tall;
        let lane_rows = wanted.clamp(1, free);
        let detail_rows = DETAIL + (free - lane_rows);
        let rows = usize::from(lane_rows);
        if let Some(selected) = selected {
            if selected < self.scroll {
                self.scroll = selected;
            } else if selected >= self.scroll + rows {
                self.scroll = selected + 1 - rows;
            }
        }
        self.scroll = if tall > 0 {
            0
        } else {
            self.scroll.min(lanes.len().saturating_sub(rows))
        };
        // The thread that ended the block under the cursor, marked.
        let waker = selected.and_then(|index| match lanes[index] {
            Lane::Thread(thread) => thread.spans.iter().find_map(|span| match &span.state {
                State::Blocked(blocked) if span.start <= cursor && cursor < span.end => {
                    blocked.waker_tid.map(|tid| LaneId::Thread(tid as i32))
                }
                _ => None,
            }),
            Lane::Gpu(_) => None,
        });
        let mut row = y;
        for (index, lane) in lanes.iter().enumerate().skip(self.scroll) {
            let two = active(lane)
                && (lanes[..index].iter().filter(|lane| active(lane)).count() as u16) < tall;
            if row + u16::from(two) >= y + lane_rows {
                break;
            }
            let (name, trust, mut label_style) = match lane {
                Lane::Thread(thread) => (
                    format!("{:<14.14} {:>7}", thread.comm, thread.tid),
                    &model.thread_trust,
                    if thread.comm.starts_with(OWN) {
                        dim.add_modifier(Modifier::ITALIC)
                    } else {
                        Style::new()
                    },
                ),
                Lane::Gpu(gpu) => (gpu.label(), &model.gpu_trust, Style::new().fg(KERNEL)),
            };
            if Some(index) == selected {
                label_style = label_style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
            } else if Some(lane.id()) == waker {
                label_style = label_style
                    .fg(palette::ACCENT)
                    .add_modifier(Modifier::UNDERLINED);
            }
            buffer.set_stringn(
                area.x,
                row,
                format!("{name:<width$}", width = usize::from(LABEL)),
                usize::from(LABEL),
                label_style,
            );
            self.regions.push(Region::new(
                Rect::new(area.x, row, area.width, 1 + u16::from(two)),
                index,
            ));
            // The tool's own threads are its cost, not the program's: their
            // cells are drawn apart too, not only their name.
            let own = matches!(lane, Lane::Thread(thread) if thread.comm.starts_with(OWN));
            let lane_cells = cells(lane, low, high, usize::from(columns));
            let (share, of) = lane_share(lane, low, high);
            let share = if matches!(lane, Lane::Thread(_)) && !model.wakeups {
                format!("{of} ?")
            } else {
                format!("{of} {}", units::percent(share * 100.0))
            };
            let share = format!("{share:>width$}", width = usize::from(SHARE));
            // A cell's time error dims it only where it can move what the
            // cell draws (`Trust::dims_at`).
            let cell_ns = (high - low) as f64 / f64::from(columns.max(1));
            let mut paint = |row: u16, glyph: &dyn Fn(&Cell) -> (&'static str, Option<Color>)| {
                buffer.set_string(area.x + LABEL, row, "│", dim);
                for (column, cell) in lane_cells.iter().enumerate() {
                    let (symbol, colour) = glyph(cell);
                    // A block's colour is `sched.offcpu`'s word: its cells
                    // carry that source's trust as well as the instants'.
                    let mut style = match colour {
                        Some(SLEEPING | DISK) => model.block_trust.style_at(cell_ns),
                        _ => trust.style_at(cell_ns),
                    };
                    if own {
                        style = style.add_modifier(Modifier::DIM | Modifier::ITALIC);
                    }
                    if let Some(colour) = colour {
                        style = style.fg(colour);
                    }
                    if Some(column as u16) == cursor_column {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                    buffer[(x_time + column as u16, row)]
                        .set_symbol(symbol)
                        .set_style(style);
                }
                buffer.set_string(x_time + columns, row, "│", dim);
            };
            if two {
                paint(row, &|cell| awake_glyph(cell, width_ns));
                paint(row + 1, &|cell| blocked_glyph(cell, width_ns));
                if let Lane::Thread(thread) = lane {
                    let blocked: u64 = thread
                        .spans
                        .iter()
                        .filter(|span| matches!(span.state, State::Blocked(_)))
                        .map(|span| overlap(span.start, span.end, low, high))
                        .sum();
                    buffer.set_string(
                        x_time + columns + 1,
                        row + 1,
                        format!(
                            "{:>width$}",
                            format!(
                                "blkd {}",
                                units::percent(blocked as f64 * 100.0 / (high - low) as f64)
                            ),
                            width = usize::from(SHARE)
                        ),
                        trust.style(),
                    );
                    buffer.set_stringn(
                        area.x,
                        row + 1,
                        format!("{:>width$}", "off cpu ", width = usize::from(LABEL)),
                        usize::from(LABEL),
                        dim,
                    );
                }
            } else {
                paint(row, &|cell| glyph(cell, width_ns));
            }
            buffer.set_string(x_time + columns + 1, row, share, trust.style());
            row += 1 + u16::from(two);
        }
        if tall == 0 && lanes.len() > rows {
            buffer.set_stringn(
                area.x,
                y + lane_rows - 1,
                format!(
                    "{}-{} of {} lanes ↑/↓",
                    self.scroll + 1,
                    (self.scroll + rows).min(lanes.len()),
                    lanes.len()
                ),
                usize::from(LABEL),
                dim.add_modifier(Modifier::ITALIC),
            );
        }
        y += lane_rows;
        if refusal_rows > 0 {
            let each = refusal_rows / refusals.len() as u16;
            for refusal in &refusals {
                trust::render_refusal(frame, Rect::new(area.x, y, area.width, each), refusal);
                y += each;
            }
            y += refusal_rows - each * refusals.len() as u16;
        }

        // The cursor's instant on the rule, and the detail of the selected
        // lane at it.
        let buffer = frame.buffer_mut();
        buffer.set_string(area.x, y, "─".repeat(usize::from(area.width)), dim);
        if let Some(column) = cursor_column {
            let marker = format!("▲ {}", at(model, cursor));
            let x = (x_time + column).min(area.x + area.width - marker.chars().count() as u16);
            buffer.set_string(x, y, marker, Style::new().fg(palette::ACCENT));
        }
        y += 1;
        let detail = selected
            .map(|index| detail(model, &lanes, &lanes[index], cursor, area.width))
            .unwrap_or_default();
        Paragraph::new(detail)
            .wrap(Wrap { trim: false })
            .render(Rect::new(area.x, y, area.width, detail_rows), buffer);
        y += detail_rows;
        Paragraph::new(legend(model)).render(Rect::new(area.x, y, area.width, 1), buffer);
        self.cell_ns = (width_ns as u64).max(1);
    }
}

/// The time axis: ticks from the run's opening.
fn ruler(buffer: &mut Buffer, area: Rect, opened: u64, (low, high): (u64, u64)) {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let step = tick(high - low, area.width);
    let width_ns = (high - low) as f64 / f64::from(area.width);
    let mut mark = low.saturating_sub(opened).div_ceil(step) * step;
    let mut free_from = 0u16;
    while opened + mark < high {
        let column = ((opened + mark - low) as f64 / width_ns) as u16;
        let label = format!("╷{}", units::duration(mark as f64));
        let length = label.chars().count() as u16;
        if column >= free_from && column + length <= area.width {
            buffer.set_string(area.x + column, area.y, &label, dim);
            free_from = column + length + 1;
        }
        mark += step;
    }
}

/// The share of the drawn span a lane was awake (a thread) or busy (a GPU
/// stream), and which of the two it is.
fn lane_share(lane: &Lane<'_>, low: u64, high: u64) -> (f64, &'static str) {
    let span = (high - low).max(1) as f64;
    match lane {
        Lane::Thread(thread) => {
            let awake: u64 = thread
                .spans
                .iter()
                .filter(|spell| spell.state == State::Awake)
                .map(|spell| overlap(spell.start, spell.end, low, high))
                .sum();
            (awake as f64 / span, "awake")
        }
        Lane::Gpu(gpu) => {
            let busy: u64 = gpu
                .ops
                .iter()
                .map(|op| overlap(op.start, op.end, low, high))
                .sum();
            (busy as f64 / span, "busy")
        }
    }
}

/// What each source gave, with its trust, in one line.
fn status_line(model: &Model) -> Line<'static> {
    let mut spans = Vec::new();
    for held in &model.held {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("{} ", held.source),
            Style::new().add_modifier(Modifier::BOLD),
        ));
        if held.served {
            spans.extend(trust::figure(units::count(held.events as f64), &held.trust).spans);
        } else if model
            .thread_refused
            .iter()
            .chain(model.gpu_refused.iter())
            .any(|refusal| refusal.source == held.source)
        {
            spans.push(Span::styled("refused", Style::new().fg(palette::REFUSED)));
        } else {
            spans.push(Span::styled(
                if held.emits.is_empty() {
                    "not in this run"
                } else {
                    "no raw events"
                },
                Style::new().add_modifier(Modifier::DIM),
            ));
        }
    }
    Line::from(spans)
}

/// What each source holds, a line each, for a run with no lane.
fn held_lines(model: &Model) -> Vec<Line<'static>> {
    model
        .held
        .iter()
        .map(|held| {
            let mut spans = vec![Span::styled(
                format!("  {:<16}", held.source),
                Style::new().add_modifier(Modifier::BOLD),
            )];
            if held.served {
                spans.extend(
                    trust::figure(
                        format!("{} events", units::count(held.events as f64)),
                        &held.trust,
                    )
                    .spans,
                );
            } else if model
                .thread_refused
                .iter()
                .chain(model.gpu_refused.iter())
                .any(|refusal| refusal.source == held.source)
            {
                spans.push(Span::styled(
                    "refused: its reason is below",
                    Style::new().fg(palette::REFUSED),
                ));
            } else if held.emits.is_empty() {
                spans.push(Span::styled(
                    "not in this run",
                    Style::new().add_modifier(Modifier::DIM),
                ));
            } else {
                spans.push(Span::styled(
                    format!(
                        "in the run as {} only: a lane is drawn from each event, `raw`",
                        held.emits.join(", ")
                    ),
                    Style::new().add_modifier(Modifier::DIM),
                ));
            }
            Line::from(spans)
        })
        .collect()
}

fn legend(model: &Model) -> Line<'static> {
    let mut spans = vec![
        Span::styled("▆", Style::new().fg(AWAKE)),
        Span::raw(" awake: on a cpu or queued  "),
        Span::styled("─", Style::new().fg(SLEEPING)),
        Span::raw(" blocked  "),
        Span::styled("━", Style::new().fg(DISK)),
        Span::raw(" uninterruptible  "),
        Span::styled("▆", Style::new().fg(KERNEL)),
        Span::raw(" kernel  "),
        Span::styled("▆", Style::new().fg(COPY)),
        Span::raw(" copy  "),
    ];
    if model.wakeups {
        spans.push(Span::styled(
            "height: share of the cell",
            Style::new().add_modifier(Modifier::DIM),
        ));
    } else {
        spans.push(Span::raw("┄ blocked or not: no wakeups in this run"));
    }
    Line::from(spans)
}

fn at(model: &Model, t: u64) -> String {
    units::duration(t.saturating_sub(model.opened_ns) as f64)
}

fn interval(model: &Model, start: u64, end: u64) -> String {
    format!(
        "{}, {} → {}",
        units::duration((end - start) as f64),
        at(model, start),
        at(model, end)
    )
}

/// The frames of a stack, leaf first, as many as fit in `room` cells. A run
/// of unnamed frames of one object (`libc.so.6+0xa08f2`) is said once, with
/// how many: what tells one wait from another is the first name under them.
fn frames(frames: &[String], room: usize) -> String {
    let mut named: Vec<(String, usize)> = Vec::new();
    for frame in frames.iter().rev() {
        let name = frame.split(" (").next().unwrap_or(frame);
        let object = name
            .split_once("+0x")
            .map(|(object, _)| object.to_owned())
            .filter(|_| !name.contains(' '));
        match (object, named.last_mut()) {
            (Some(object), Some((last, count))) if *last == object && *count > 0 => *count += 1,
            (Some(object), _) => named.push((object, 1)),
            (None, _) => named.push((name.to_owned(), 0)),
        }
    }
    let mut out = String::new();
    for (name, count) in named {
        let name = if count > 1 {
            format!("{name} x{count}")
        } else {
            name
        };
        let piece = if out.is_empty() {
            name
        } else {
            format!(" ← {name}")
        };
        if out.chars().count() + piece.chars().count() + 4 > room {
            if !out.is_empty() {
                out.push_str(" ← …");
            }
            break;
        }
        out.push_str(&piece);
    }
    out
}

/// A figure of the detail in its source's trust style, over `base`.
fn trusted(text: impl Into<String>, base: Style, trust: &Trust) -> Span<'static> {
    Span::styled(text.into(), base.patch(trust.style()))
}

/// What the selected lane was doing at the cursor, and what every other lane
/// was doing at the same instant.
fn detail(
    model: &Model,
    lanes: &[Lane<'_>],
    lane: &Lane<'_>,
    t: u64,
    width: u16,
) -> Vec<Line<'static>> {
    let width = usize::from(width);
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let dim = Style::new().add_modifier(Modifier::DIM);
    let mut lines = Vec::new();
    let mut waits_lines = Vec::new();
    match lane {
        Lane::Thread(thread) => {
            let mut head = vec![Span::styled(
                format!(
                    "{} {} (pid {}) at {}: ",
                    thread.comm,
                    thread.tid,
                    thread.pid,
                    at(model, t)
                ),
                bold,
            )];
            let span = thread
                .spans
                .iter()
                .find(|span| span.start <= t && t < span.end);
            let mut more = Vec::new();
            match span.map(|span| (span, &span.state)) {
                None => head.push(Span::styled(
                    "nothing seen: before its first switch or wakeup, or after its last",
                    dim,
                )),
                Some((span, State::Awake)) => {
                    head.push(trusted(
                        "awake",
                        Style::new().fg(AWAKE),
                        &model.thread_trust,
                    ));
                    head.push(trusted(
                        format!(" {}", interval(model, span.start, span.end)),
                        Style::new(),
                        &model.thread_trust,
                    ));
                    head.push(Span::raw(", on a cpu or queued for one"));
                }
                Some((span, State::Unknown)) => head.push(trusted(
                    format!(
                        "off a cpu at {}, next off one {} later: a block or a preemption, \
                         which a run without wakeups cannot tell",
                        at(model, span.start),
                        units::duration((span.end - span.start) as f64)
                    ),
                    Style::new(),
                    &model.thread_trust,
                )),
                Some((span, State::Blocked(blocked))) => {
                    let state = if blocked.state.is_empty() {
                        "blocked".to_owned()
                    } else {
                        format!("blocked, {}", blocked.state)
                    };
                    // The state is `sched.offcpu`'s word, the interval the
                    // instants'.
                    let state_trust = if blocked.state.is_empty() {
                        &model.thread_trust
                    } else {
                        &model.block_trust
                    };
                    head.push(trusted(
                        state,
                        Style::new().fg(blocked.colour()),
                        state_trust,
                    ));
                    head.push(trusted(
                        format!(" {}", interval(model, span.start, span.end)),
                        Style::new(),
                        &model.thread_trust,
                    ));
                    if let Some(cpu) = span.cpu {
                        head.push(Span::styled(format!(", left cpu {cpu}"), dim));
                    }
                    let mut woken =
                        vec![Span::raw("  woken by "), Span::styled(waker(blocked), bold)];
                    match blocked.queued_ns {
                        // `sched.offcpu` writes a wait the kernel did not
                        // stamp as zero: a zero is not a measured zero.
                        Some(0) => woken.push(trusted(
                            ", then queued for a cpu: 0, or not stamped",
                            Style::new(),
                            &model.offcpu_trust,
                        )),
                        Some(queued) => woken.push(trusted(
                            format!(", then queued {} for a cpu", units::duration(queued as f64)),
                            Style::new(),
                            &model.offcpu_trust,
                        )),
                        None => {}
                    }
                    woken.push(Span::styled(
                        if blocked.state.is_empty() {
                            "; no sched.offcpu row holds it".to_owned()
                        } else if blocked.exact {
                            "; its sched.offcpu row is this block alone".to_owned()
                        } else if blocked.rivals > 0 {
                            format!(
                                "; state, stack: the nearest in size of {} sched.offcpu rows \
                                 of its window that could hold it",
                                blocked.rivals + 1
                            )
                        } else {
                            "; state, stack: its window's sched.offcpu row nearest in size"
                                .to_owned()
                        },
                        dim,
                    ));
                    more.push(Line::from(woken));
                    let stack = frames(&blocked.frames, width.saturating_sub(4));
                    if !stack.is_empty() {
                        more.push(Line::from(vec![
                            Span::styled("  in ", dim),
                            trusted(
                                stack,
                                Style::new().fg(blocked.colour()),
                                &model.offcpu_trust,
                            ),
                        ]));
                    }
                }
            }
            lines.push(Line::from(head));
            lines.extend(more);
            if !thread.waits.is_empty() {
                let total: u64 = thread.waits.iter().map(|wait| wait.total_ns).sum();
                let offcpu = &model.offcpu_trust;
                let mut header = vec![
                    Span::styled("  where it was off a cpu over the run, ", dim),
                    trusted(units::duration(total as f64), Style::new(), offcpu),
                    Span::styled(" in all (sched.offcpu, by state and stack)", dim),
                ];
                if !offcpu.marks.is_empty() {
                    header.push(Span::raw(" "));
                    header.extend(offcpu.badges());
                }
                header.push(Span::styled(":", dim));
                waits_lines = std::iter::once(Line::from(header))
                    .chain(thread.waits.iter().take(4).map(|wait| {
                        let colour = if wait.state == "uninterruptible" {
                            DISK
                        } else {
                            SLEEPING
                        };
                        Line::from(vec![
                            Span::raw("  "),
                            trusted(
                                format!(
                                    "{:>8} {:>5}",
                                    units::duration(wait.total_ns as f64),
                                    units::percent(
                                        wait.total_ns as f64 * 100.0 / total.max(1) as f64
                                    )
                                ),
                                Style::new(),
                                offcpu,
                            ),
                            Span::raw(" "),
                            trusted(
                                format!(
                                    "{:<9.9} ",
                                    if wait.state == "uninterruptible" {
                                        "uninterr."
                                    } else {
                                        wait.state.as_str()
                                    }
                                ),
                                Style::new().fg(colour),
                                offcpu,
                            ),
                            Span::styled(format!("{:>5} blocks  ", wait.blocks), dim),
                            trusted(
                                frames(&wait.frames, width.saturating_sub(44)),
                                Style::new(),
                                offcpu,
                            ),
                        ])
                    }))
                    .collect();
            }
            lines.push(Line::from(Span::styled(
                format!(
                    "  {} switch-outs and {} wakeups seen; awake {} of the run",
                    thread.switches,
                    thread.wakeups,
                    units::percent(lane_share(lane, model.opened_ns, model.closed_ns).0 * 100.0)
                ),
                dim,
            )));
        }
        Lane::Gpu(gpu) => {
            let mut head = vec![Span::styled(
                format!("{} (pid {}) at {}: ", gpu.label(), gpu.pid, at(model, t)),
                bold,
            )];
            let mut more = Vec::new();
            match gpu.ops.iter().find(|op| op.start <= t && t < op.end) {
                Some(op) => {
                    head.push(Span::styled(
                        op.name.clone(),
                        Style::new().fg(if op.copy { COPY } else { KERNEL }),
                    ));
                    head.push(trusted(
                        format!(" {}", interval(model, op.start, op.end)),
                        Style::new(),
                        &model.gpu_trust,
                    ));
                    let mut what = Vec::new();
                    if !op.grid.is_empty() {
                        what.push(format!("grid {}, block {}", op.grid, op.block));
                    }
                    if let Some(bytes) = op.bytes {
                        what.push(units::bytes(bytes as f64));
                    }
                    if !what.is_empty() {
                        more.push(Line::from(format!("  {}", what.join(", "))));
                    }
                }
                None => head.push(Span::styled("idle", dim)),
            }
            lines.push(Line::from(head));
            lines.extend(more);
            lines.push(Line::from(Span::styled(
                format!(
                    "  {} activities; their times converted from the device's clock, to within {}",
                    gpu.ops.len(),
                    units::duration(gpu.ts_err_ns as f64)
                ),
                dim,
            )));
        }
    }
    // What lines up: every other lane at the same instant, by what it was
    // doing.
    let mut groups: Vec<(&str, Color, Vec<Vec<Span<'static>>>)> = vec![
        ("awake", AWAKE, Vec::new()),
        ("blocked", SLEEPING, Vec::new()),
        ("uninterruptible", DISK, Vec::new()),
        ("on the GPU", KERNEL, Vec::new()),
    ];
    for other in lanes {
        if other.id() == lane.id() {
            continue;
        }
        match other {
            Lane::Thread(thread) => {
                let Some(span) = thread
                    .spans
                    .iter()
                    .find(|span| span.start <= t && t < span.end)
                else {
                    continue;
                };
                let group = match &span.state {
                    State::Awake => 0,
                    State::Blocked(blocked) if blocked.disk() => 2,
                    State::Blocked(_) => 1,
                    State::Unknown => continue,
                };
                groups[group]
                    .2
                    .push(vec![Span::raw(format!("{} {}", thread.comm, thread.tid))]);
            }
            Lane::Gpu(gpu) => {
                if let Some(op) = gpu.ops.iter().find(|op| op.start <= t && t < op.end) {
                    groups[3].2.push(vec![
                        Span::raw(format!("{} on s{} (", op.name, gpu.stream)),
                        trusted(
                            units::duration((op.end - op.start) as f64),
                            Style::new(),
                            &model.gpu_trust,
                        ),
                        Span::raw(")"),
                    ]);
                }
            }
        }
    }
    let mut others = vec![Span::styled("  meanwhile ", dim)];
    for (name, colour, members) in groups {
        if members.is_empty() {
            continue;
        }
        if others.len() > 1 {
            others.push(Span::styled(" · ", dim));
        }
        others.push(Span::styled(format!("{name}: "), Style::new().fg(colour)));
        let shown = members.len().min(3);
        for (index, member) in members[..shown].iter().enumerate() {
            if index > 0 {
                others.push(Span::raw(", "));
            }
            others.extend(member.iter().cloned());
        }
        if members.len() > shown {
            others.push(Span::raw(format!(" and {} more", members.len() - shown)));
        }
    }
    if others.len() > 1 {
        lines.push(Line::from(others));
    }
    lines.extend(waits_lines);
    lines
}

fn waker(blocked: &Blocked) -> String {
    match blocked.waker_tid {
        Some(0) if blocked.waker_context.is_empty() => "an interrupt".to_owned(),
        Some(0) => format!("an interrupt ({})", blocked.waker_context),
        Some(tid) if !blocked.waker_comm.is_empty() => format!("{} {tid}", blocked.waker_comm),
        Some(tid) => format!("tid {tid}, outside the target"),
        None => "a wakeup that does not say who".to_owned(),
    }
}

impl View for Lanes {
    fn name(&self) -> &'static str {
        "lanes"
    }

    fn title(&self) -> &'static str {
        "threads side by side: one lane per thread and per GPU stream, along time"
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        let block = Block::new().borders(Borders::ALL).title(format!(
            " {} -- {} ",
            self.name(),
            self.title()
        ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.regions.clear();
        self.draw_lanes(frame, inner, ctx);
    }

    fn regions(&self) -> &[Region] {
        &self.regions
    }

    fn select(&mut self, id: usize, ctx: &Ctx<'_>) -> Action {
        self.model(ctx.data);
        if let Some((_, model)) = &self.cache {
            if let Some(lane) = model.lanes(ctx.focus).get(id) {
                self.selected = Some(lane.id());
            }
        }
        Action::None
    }

    fn key(&mut self, key: KeyEvent, ctx: &Ctx<'_>) -> Action {
        self.model(ctx.data);
        self.settle(ctx.focus);
        let Some((_, model)) = &self.cache else {
            return Action::None;
        };
        let lanes = model.lanes(ctx.focus);
        let index = self
            .selected
            .as_ref()
            .and_then(|id| lanes.iter().position(|lane| lane.id() == *id));
        let lane = index.map(|index| lanes[index]);
        let (opened, closed) = (model.opened_ns, model.closed_ns.max(model.opened_ns + 1));
        let (low, high) = self.window.unwrap_or((opened, closed));
        let cursor = self.cursor.unwrap_or(low);
        let step = self.cell_ns.max(1);
        let mut selected = self.selected.clone();
        let mut to = None;
        let mut window = self.window;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(index) = index.filter(|index| *index > 0) {
                    selected = Some(lanes[index - 1].id());
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(index) = index.filter(|index| index + 1 < lanes.len()) {
                    selected = Some(lanes[index + 1].id());
                }
            }
            KeyCode::Char('h') => to = Some(cursor.saturating_sub(step)),
            KeyCode::Char('l') => to = Some(cursor + step),
            KeyCode::Char('H') => to = Some(cursor.saturating_sub(10 * step)),
            KeyCode::Char('L') => to = Some(cursor + 10 * step),
            KeyCode::Char('[') => {
                to =
                    lane.and_then(|lane| lane.edges().into_iter().rev().find(|edge| *edge < cursor))
            }
            KeyCode::Char(']') => {
                to = lane.and_then(|lane| lane.edges().into_iter().find(|edge| *edge > cursor))
            }
            KeyCode::Char('z') | KeyCode::Char('+') => {
                let width = ((high - low) / 2).max(10_000);
                let low = cursor
                    .saturating_sub(width / 2)
                    .clamp(opened, closed.saturating_sub(width).max(opened));
                window = Some((low, (low + width).min(closed)));
            }
            KeyCode::Char('Z') | KeyCode::Char('-') => {
                let width = (high - low).saturating_mul(2);
                window = if width >= closed - opened {
                    None
                } else {
                    let low = cursor
                        .saturating_sub(width / 2)
                        .clamp(opened, closed - width);
                    Some((low, low + width))
                };
            }
            KeyCode::Char('w') => {
                // To the thread that ended the block under the cursor, just
                // before it did: what the waker was doing as it woke it.
                if let Some(Lane::Thread(thread)) = lane {
                    let waker = thread.spans.iter().find_map(|span| match &span.state {
                        State::Blocked(blocked) if span.start <= cursor && cursor < span.end => {
                            Some((blocked.waker_tid?, span.end))
                        }
                        _ => None,
                    });
                    if let Some((tid, at)) = waker {
                        let id = LaneId::Thread(tid as i32);
                        if lanes.iter().any(|lane| lane.id() == id) {
                            selected = Some(id);
                            to = Some(at.saturating_sub(1));
                        }
                    }
                }
            }
            KeyCode::Enter => {
                return match lane {
                    Some(Lane::Thread(thread)) => Action::Focus(Focus {
                        pid: Some(thread.pid),
                        tid: Some(thread.tid),
                        request: None,
                    }),
                    Some(Lane::Gpu(gpu)) => Action::Focus(Focus {
                        pid: Some(gpu.pid),
                        tid: None,
                        request: None,
                    }),
                    None => Action::None,
                };
            }
            _ => {}
        }
        if let Some(t) = to {
            let t = t.clamp(opened, closed - 1);
            self.cursor = Some(t);
            // The window follows the cursor out of it.
            if let Some((low, high)) = window {
                if t < low || t >= high {
                    let width = high - low;
                    let low = t.saturating_sub(width / 2).clamp(opened, closed - width);
                    window = Some((low, low + width));
                }
            }
        }
        self.selected = selected;
        self.window = window;
        Action::None
    }

    fn keys(&self) -> &'static [(&'static str, &'static str)] {
        &[
            ("↑/↓", "lane"),
            ("h/l H/L", "cursor"),
            ("[/]", "prev/next change"),
            ("z/Z", "zoom"),
            ("w", "to waker"),
            ("enter", "focus"),
        ]
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(Lanes {
        selected: None,
        cursor: None,
        window: None,
        scroll: 0,
        cell_ns: 1,
        cache: None,
        regions: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::app::{App, Cost};
    use crate::model::{Attr, AttrValue, Key, Output, Window};
    use crate::snapshot::{self, HEIGHT, WIDTH};

    /// The recording this view was judged on: a host program feeding a GPU,
    /// `docs/view/gallery/lanes-gpu/` (its `workload.py` says what it does).
    fn recording() -> Data {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/view/gallery/lanes-gpu");
        let mut data = Data::load(&path).expect("the lanes recording");
        data.origin = "recording lanes-gpu".to_owned();
        data
    }

    fn app(data: Data) -> App {
        let mut app = App::new(data);
        app.cost = Cost::Snapshot;
        app.open("lanes").expect("the lanes view");
        app
    }

    fn press(app: &mut App, keys: &str) {
        for key in keys.chars() {
            app.key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        }
    }

    fn event(ts: u64, kind: &str, tid: i32, attrs: &[(&str, AttrValue)]) -> Event {
        Event {
            ts_ns: ts,
            ts_err_ns: 0,
            kind: kind.to_owned(),
            keys: vec![Key::Task { tid }],
            attrs: attrs
                .iter()
                .map(|(name, value)| Attr {
                    name: (*name).to_owned(),
                    value: value.clone(),
                })
                .collect(),
            collector_id: String::new(),
            seq: 0,
        }
    }

    fn results(source: &str, events: Vec<Event>) -> Results {
        Results {
            label: source.to_owned(),
            source: source.to_owned(),
            events_seen: events.len() as u64,
            outputs: vec![Output::Raw { events }],
            window: Window {
                run_opened_ns: 1_000,
                opened_ns: 1_000,
                closed_ns: 100_000_000,
                ..Window::default()
            },
            ..Results::default()
        }
    }

    fn out(ts: u64, tid: i32) -> Event {
        event(
            ts,
            "sched.switches",
            tid,
            &[
                ("pid", AttrValue::Uint(7)),
                ("comm", AttrValue::Text(format!("t{tid}"))),
                ("cpu", AttrValue::Uint(3)),
            ],
        )
    }

    fn woken(ts: u64, tid: i32, by: i64) -> Event {
        event(
            ts,
            "sched.wakeup",
            tid,
            &[
                ("waker_tid", AttrValue::Int(by)),
                ("waker_context", AttrValue::Text("task".into())),
            ],
        )
    }

    fn row(ts: u64, tid: i32, blocks: u64, blocked_ns: u64, state: &str) -> Event {
        event(
            ts,
            "sched.offcpu",
            tid,
            &[
                ("window_ns", AttrValue::Uint(50_000_000)),
                ("blocks", AttrValue::Uint(blocks)),
                ("blocked_ns", AttrValue::Uint(blocked_ns)),
                ("queued_ns", AttrValue::Uint(2_000)),
                ("state", AttrValue::Text(state.into())),
                ("stack_id", AttrValue::Uint(1)),
            ],
        )
    }

    /// One thread: off at 10 ms, woken at 30 ms (a block of 20 ms), off at
    /// 40 ms and again at 50 ms with no wakeup between (a preemption), woken
    /// by thread 9 at 70 ms; thread 9 itself only switches.
    fn synthetic(with_wakeups: bool) -> Data {
        let ms = 1_000_000;
        let mut offcpu = results(
            "sched.offcpu",
            vec![
                row(50 * ms, 5, 1, 20 * ms, "uninterruptible"),
                row(100 * ms, 5, 3, 3 * 20 * ms, "sleeping"),
            ],
        );
        offcpu.stacks.insert(1, vec!["main".into(), "fsync".into()]);
        let mut run = vec![
            results(
                "sched.switches",
                vec![
                    out(10 * ms, 5),
                    out(40 * ms, 5),
                    out(50 * ms, 5),
                    out(60 * ms, 9),
                ],
            ),
            offcpu,
        ];
        if with_wakeups {
            run.push(results(
                "sched.wakeup",
                vec![woken(30 * ms, 5, 0), woken(70 * ms, 5, 9)],
            ));
        }
        Data {
            run: crate::model::Run {
                results: run,
                ..Default::default()
            },
            ..Data::default()
        }
    }

    #[test]
    fn a_switch_then_a_wakeup_is_a_block_and_two_switches_a_preemption() {
        let model = Model::build(&synthetic(true));
        let thread = model.threads.iter().find(|thread| thread.tid == 5).unwrap();
        let ms = 1_000_000;
        let spans: Vec<(u64, u64, &str)> = thread
            .spans
            .iter()
            .map(|span| {
                (
                    span.start / ms,
                    span.end / ms,
                    match &span.state {
                        State::Awake => "awake",
                        State::Blocked(_) => "blocked",
                        State::Unknown => "unknown",
                    },
                )
            })
            .collect();
        // 30 -> 40 awake, then off at 40 and at 50 with no wakeup between:
        // still awake, the two joined.
        assert_eq!(
            spans,
            [(10, 30, "blocked"), (30, 50, "awake"), (50, 70, "blocked")]
        );
        let State::Blocked(first) = &thread.spans[0].state else {
            unreachable!()
        };
        // The 20 ms block ended in the first window, whose one row is it.
        assert!(first.exact, "{first:?}");
        assert_eq!(first.state, "uninterruptible");
        assert_eq!(first.frames, ["main", "fsync"]);
        assert_eq!(first.queued_ns, Some(2_000));
        assert_eq!(first.waker_tid, Some(0));
        let State::Blocked(second) = &thread.spans[2].state else {
            unreachable!()
        };
        // Three blocks in one row: the state, and no claim to be exact.
        assert!(!second.exact);
        assert_eq!(second.state, "sleeping");
        assert_eq!(second.waker_tid, Some(9));
        assert_eq!(second.waker_comm, "t9");
        assert_eq!(thread.waits.len(), 2);
    }

    /// Two one-block rows of one window near in size, with different stacks:
    /// a block between them is matched to the nearer, and not called exact.
    #[test]
    fn a_window_with_two_rows_near_in_size_is_not_an_exact_match() {
        let ms = 1_000_000;
        let one = |ts: u64, blocked_ns: u64, stack: u64| {
            event(
                ts,
                "sched.offcpu",
                5,
                &[
                    ("window_ns", AttrValue::Uint(50_000_000)),
                    ("blocks", AttrValue::Uint(1)),
                    ("blocked_ns", AttrValue::Uint(blocked_ns)),
                    ("queued_ns", AttrValue::Uint(0)),
                    ("state", AttrValue::Text("sleeping".into())),
                    ("stack_id", AttrValue::Uint(stack)),
                ],
            )
        };
        let mut offcpu = results(
            "sched.offcpu",
            vec![one(50 * ms, 5_050_000, 1), one(50 * ms, 5_100_000, 2)],
        );
        offcpu.stacks.insert(1, vec!["main".into(), "read".into()]);
        offcpu.stacks.insert(2, vec!["main".into(), "poll".into()]);
        let data = Data {
            run: crate::model::Run {
                results: vec![
                    results("sched.switches", vec![out(10 * ms, 5)]),
                    results("sched.wakeup", vec![woken(10 * ms + 5_080_000, 5, 0)]),
                    offcpu,
                ],
                ..Default::default()
            },
            ..Data::default()
        };
        let model = Model::build(&data);
        let thread = model.threads.iter().find(|thread| thread.tid == 5).unwrap();
        let State::Blocked(blocked) = &thread.spans[0].state else {
            panic!("{:?}", thread.spans)
        };
        assert!(!blocked.exact, "{blocked:?}");
        assert_eq!(blocked.rivals, 1);
        assert_eq!(blocked.frames, ["main", "poll"], "the nearer row's stack");
        assert_eq!(
            blocked.queued_ns, None,
            "no queueing said of an unsure match"
        );
    }

    #[test]
    fn without_wakeups_a_switch_says_neither_block_nor_preemption() {
        let model = Model::build(&synthetic(false));
        let thread = model.threads.iter().find(|thread| thread.tid == 5).unwrap();
        assert!(!model.wakeups);
        assert!(thread.spans.iter().all(|span| span.state == State::Unknown));
        let buffer = snapshot::draw(synthetic(false), "lanes", WIDTH, 24);
        let text = snapshot::text(&buffer);
        assert!(text.contains("sched.wakeup not in this run"), "{text}");
        assert!(text.contains("no wakeups in this run"), "{text}");
    }

    #[test]
    fn the_cursor_opens_on_the_main_thread_waiting_for_the_gpu() {
        let data = recording();
        let mut lanes = Lanes {
            selected: None,
            cursor: None,
            window: None,
            scroll: 0,
            cell_ns: 1,
            cache: None,
            regions: Vec::new(),
        };
        lanes.model(&data);
        lanes.settle(&Focus::default());
        let (_, model) = lanes.cache.as_ref().unwrap();
        let Some(LaneId::Thread(tid)) = lanes.selected.clone() else {
            panic!("no thread selected: {:?}", lanes.selected)
        };
        let thread = model
            .threads
            .iter()
            .find(|thread| thread.tid == tid)
            .unwrap();
        assert_eq!(thread.tid, thread.pid, "the main thread");
        let cursor = lanes.cursor.unwrap();
        let span = thread
            .spans
            .iter()
            .find(|span| span.start <= cursor && cursor < span.end)
            .unwrap();
        let State::Blocked(blocked) = &span.state else {
            panic!("the cursor is not on a block: {span:?}")
        };
        // Blocked inside the driver's synchronisation, the whole of a kernel
        // long, and woken by the driver's thread that took the interrupt.
        assert!(
            blocked
                .frames
                .iter()
                .any(|frame| frame.contains("cuCtxSynchronize")),
            "{:?}",
            blocked.frames
        );
        let spin = model
            .gpus
            .iter()
            .flat_map(|gpu| gpu.ops.iter())
            .find(|op| op.start <= cursor && cursor < op.end)
            .expect("a kernel under the cursor");
        assert_eq!(spin.name, "spin");
        assert!(span.end >= spin.end, "woken before the kernel ended");
        // The block is its sched.offcpu row alone, 132 ms long, and the
        // kernel covers it all but the wakeup's latency.
        assert!(blocked.exact, "{blocked:?}");
        assert_eq!(blocked.rivals, 0);
        let ms = 1_000_000;
        assert_eq!((span.end - span.start + ms / 2) / ms, 132, "{span:?}");
        let covered = overlap(spin.start, spin.end, span.start, span.end);
        assert!(
            covered * 100 >= (span.end - span.start) * 99,
            "the kernel covers {covered} ns of {span:?}"
        );
        assert_eq!(blocked.waker_comm, "cuda-EvtHandlr");
        // ... which a hardirq had woken a few tens of microseconds before.
        let waker_tid = blocked.waker_tid.unwrap() as i32;
        let waker = model
            .threads
            .iter()
            .find(|thread| thread.tid == waker_tid)
            .expect("the waker's lane");
        let its_block = waker
            .spans
            .iter()
            .filter(|its| its.end <= span.end)
            .max_by_key(|its| its.end)
            .unwrap();
        let State::Blocked(its) = &its_block.state else {
            panic!("the waker was not woken just before: {its_block:?}")
        };
        assert_eq!(its.waker_tid, Some(0), "{its:?}");
        assert_eq!(its.waker_context, "hardirq");
        let before = span.end - its_block.end;
        assert!((20_000..40_000).contains(&before), "{before} ns before");
    }

    #[test]
    fn w_goes_to_the_waker_and_enter_narrows_to_the_thread() {
        let mut app = app(recording());
        snapshot::draw_app(&mut app, WIDTH, HEIGHT);
        press(&mut app, "w");
        let text = snapshot::text(&snapshot::draw_app(&mut app, WIDTH, HEIGHT));
        assert!(text.contains("│cuda-EvtHandlr"), "{text}");
        assert!(
            text.lines()
                .any(|line| line.starts_with("│cuda-EvtHandlr") && line.contains(" at ")),
            "the detail is not the waker's:\n{text}"
        );
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.focus.tid.is_some());
        let text = snapshot::text(&snapshot::draw_app(&mut app, WIDTH, HEIGHT));
        assert!(
            !text.contains("loader"),
            "the focus left other threads:\n{text}"
        );
        assert!(text.contains("gpu0 s13 spin"), "{text}");
    }

    /// The recording, whole, and zoomed eight times on the cursor: the
    /// snapshots the view is judged on.
    #[test]
    fn the_recording() {
        let mut app = app(recording());
        snapshot::assert_snapshot(
            "lanes--gpu-feeder",
            &snapshot::draw_app(&mut app, WIDTH, HEIGHT),
        );
        press(&mut app, "zzz");
        snapshot::assert_snapshot(
            "lanes--gpu-feeder-zoom",
            &snapshot::draw_app(&mut app, WIDTH, HEIGHT),
        );
    }

    /// Half way through a live run, every key on a small terminal and on a
    /// run with nothing to draw: drawn, provisional, never a panic.
    #[test]
    fn live_small_and_empty_runs_draw() {
        let data = recording();
        let half = data.opened_ns() + (data.closed_ns() - data.opened_ns()) / 2;
        let mut live = app(crate::feed::as_live_until(&data, half));
        let text = snapshot::text(&snapshot::draw_app(&mut live, WIDTH, HEIGHT));
        assert!(text.contains("live"), "{text}");
        assert!(text.contains("│loader"), "{text}");
        for data in [recording(), synthetic(false), Data::default()] {
            let mut app = app(data);
            for (width, height) in [(WIDTH, HEIGHT), (60, 20), (40, 8)] {
                snapshot::draw_app(&mut app, width, height);
                for key in "jkhlHL[]zzzZZwj".chars() {
                    press(&mut app, &key.to_string());
                    snapshot::draw_app(&mut app, width, height);
                }
                for code in [KeyCode::Up, KeyCode::Down, KeyCode::Enter] {
                    app.key(KeyEvent::new(code, KeyModifiers::NONE));
                    snapshot::draw_app(&mut app, width, height);
                }
                app.focus = Focus::default();
            }
        }
    }
}
