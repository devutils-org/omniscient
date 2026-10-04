//! The `flame` view: a flamegraph in the terminal: on-cpu and off-cpu stacks, and two runs' difference.
//!
//! Two icicles side by side -- where the target spent its cpu (`cpu.profile`)
//! and where it waited (`sched.offcpu`) -- root at the top, the heaviest
//! callee leftmost, each frame as wide as its share. A frame can be selected,
//! zoomed into and out of; a name can be searched, and the share of the
//! profile it accounts for is said. Off cpu, where the run kept the blocks
//! themselves, the icicle can be split by thread and narrowed to a focused
//! one.
//!
//! With `omn top --replay A --against B` each icicle is the difference of the
//! two runs: red where B spent more, blue where it spent less, grey where the
//! two differ by no more than a sample count's noise. The arithmetic is the
//! triage's (`omn diff`, `src/triage/diff.rs`), re-stated here because the
//! view links nothing of `omn`: absolute rather than shares, in samples at one
//! rate and in time at two, a difference counted only beyond three standard
//! deviations of Poisson noise, the losses of both runs said. `d` switches to
//! comparing the two profiles' shapes -- the second scaled to the first's
//! total -- which is what separates "the same program doing more work" from
//! "a different program".
//!
//! Every figure is drawn in the trust style: the icicle is dimmed with its
//! source's badges in the pane's title, and the source's own caveats (a
//! frameless leaf whose caller is lost, names that could not be read) are
//! said under it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Sparkline, Wrap};
use ratatui::Frame;

use super::{Action, Ctx, Focus, Need, Scope, View};
use crate::model::{Data, Output, Refusal, Results, StackCount};
use crate::trust::{self, palette, Trust};
use crate::units;

/// What this view needs measured.
pub const NEEDS: &[Need] = &[
    Need {
        source: "cpu.profile",
        mode: "sample@997hz",
        emit: &["stack"],
        keys: &[],
        stack: Some("fp"),
        scope: Scope::Target,
        why: "the on-cpu icicle",
    },
    Need {
        source: "sched.offcpu",
        mode: "trace",
        emit: &["stack"],
        keys: &[],
        stack: Some("fp"),
        scope: Scope::Target,
        why: "the off-cpu icicle",
    },
];

/// The view's keys, for the help line.
const KEYS: &[(&str, &str)] = &[
    ("↑↓hl", "move"),
    ("⏎⌫", "zoom"),
    ("/*", "find"),
    ("om", "pane"),
    ("t", "threads"),
    ("d", "shape"),
];

/// How many standard deviations of a sample count's noise a difference has
/// to exceed to be one: the triage's `SIGMAS`.
pub const SIGMAS: f64 = 3.0;

/// The name of the root every icicle hangs from.
const ALL: &str = "all";

/// The two panes, by index.
const ON: usize = 0;
const OFF: usize = 1;

/// The view's state: what is selected and zoomed in each pane, the search,
/// and the toggles.
#[derive(Default)]
pub struct Flame {
    panes: [PaneState; 2],
    /// The pane the keys move in.
    active: usize,
    /// The active pane alone, full width.
    alone: bool,
    /// A pattern being typed after `/`.
    typing: Option<String>,
    /// The pattern searched for.
    search: Option<String>,
    /// In a comparison: the second run scaled to the first's total.
    shape: bool,
    /// Off cpu, split by thread under the root.
    by_thread: bool,
    /// The width the view was last drawn at, for moving from a key.
    last_width: u16,
    /// Whether each pane said its caveats in its own room, on the last draw.
    caveats_said: [bool; 2],
    cache: Option<(Fingerprint, [Pane; 2])>,
}

/// Where a pane is: its zoom and selection as paths of frame names from the
/// root, so they hold across a rebuild (a live update, a toggle).
#[derive(Default, Clone)]
struct PaneState {
    zoom: Vec<String>,
    selected: Vec<String>,
    scroll: usize,
}

/// What a built pane was built from: when any of it changes, it is built
/// again.
#[derive(Clone, PartialEq)]
struct Fingerprint {
    data: usize,
    events: u64,
    now_ns: Option<u64>,
    against: Option<usize>,
    shape: bool,
    by_thread: bool,
    focus: Focus,
}

impl Fingerprint {
    fn of(ctx: &Ctx<'_>, shape: bool, by_thread: bool) -> Fingerprint {
        Fingerprint {
            data: ctx.data as *const Data as usize,
            events: ctx.data.run.results.iter().map(|r| r.events_seen).sum(),
            now_ns: ctx.data.live.as_ref().map(|live| live.now_ns),
            against: ctx.against.map(|against| against as *const Data as usize),
            shape,
            by_thread,
            focus: ctx.focus.clone(),
        }
    }
}

/// One pane, built from the run.
#[derive(Clone)]
struct Pane {
    /// `on cpu` or `off cpu`.
    title: &'static str,
    source: &'static str,
    trust: Trust,
    /// The source's own caveats: its degradations, then its notes.
    caveats: Vec<String>,
    /// Something this pane could not do as asked, said under it.
    aside: Option<String>,
    body: Body,
}

#[derive(Clone)]
enum Body {
    Refused(Refusal),
    /// Not in this run at all.
    Missing,
    /// A live run: the runner names stacks at its end, so until then the
    /// pane says what has arrived, over time.
    Live {
        events: u64,
        stacks: usize,
        /// When each event arrived, as a share of the run so far.
        arrivals: Vec<f32>,
    },
    /// Served, with nothing in it.
    Empty(String),
    Tree(Tree),
}

/// What a value is in.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Unit {
    /// Samples, each worth `period_ns` when known.
    Samples(Option<u64>),
    /// Nanoseconds off cpu.
    Nanos,
}

impl Unit {
    fn of(results: &Results) -> Unit {
        if results.unit == "ns" {
            Unit::Nanos
        } else {
            Unit::Samples(results.period_ns)
        }
    }

    /// A value, as a person reads it: samples with the time they stand for.
    fn say(&self, value: f64) -> String {
        match self {
            Unit::Samples(Some(period)) => format!(
                "{} samples ≈ {}",
                units::count(value),
                units::duration(value * *period as f64)
            ),
            Unit::Samples(None) => format!("{} samples", units::count(value)),
            Unit::Nanos => units::duration(value),
        }
    }

    /// A value in a few cells.
    fn short(&self, value: f64) -> String {
        match self {
            Unit::Samples(_) => units::count(value),
            Unit::Nanos => units::duration(value),
        }
    }
}

/// One frame of a tree: its name, and its weight in each run (`[this,
/// other]`; the second is zero outside a comparison).
#[derive(Debug, Clone)]
struct Node {
    name: String,
    parent: usize,
    children: Vec<usize>,
    value: [f64; 2],
    /// What ends here: the weight whose leaf this frame is.
    own: [f64; 2],
    events: [u64; 2],
}

/// The arithmetic of a comparison.
#[derive(Debug, Clone)]
struct Compare {
    /// What each run's values are multiplied by to be comparable: a period
    /// when the rates differ, and the shapes' scale.
    scale: [f64; 2],
    /// Whether a sample count's noise applies: samples do, time off cpu does
    /// not.
    poisson: bool,
    /// What the comparable values are in.
    unit: &'static str,
    totals: [f64; 2],
    /// The functions whose own cost moved beyond the noise, largest first.
    difference: Vec<Change>,
    /// How many functions there are, in either run.
    functions: usize,
    notes: Vec<String>,
}

#[derive(Debug, Clone)]
struct Change {
    function: String,
    delta: f64,
    ratio: Option<f64>,
    sigmas: f64,
}

impl Compare {
    /// Both runs' values of a node, comparable.
    fn pair(&self, value: [f64; 2]) -> (f64, f64) {
        (value[0] * self.scale[0], value[1] * self.scale[1])
    }

    /// How many standard deviations of noise `value`'s difference is, and
    /// whether it counts as one. Time off cpu has no noise model: it counts
    /// when trust's own difference colour says so and it is not a speck.
    fn judge(&self, value: [f64; 2]) -> (f64, bool) {
        let (a, b) = self.pair(value);
        if self.poisson {
            // The variance of a count times a scale is the count times the
            // scale squared.
            let variance = value[0] * self.scale[0].powi(2) + value[1] * self.scale[1].powi(2);
            let sigmas = if variance > 0.0 {
                (b - a).abs() / variance.sqrt()
            } else {
                0.0
            };
            (sigmas, sigmas > SIGMAS)
        } else {
            let ratio = if a > 0.0 { b / a } else { f64::INFINITY };
            let speck = (b - a).abs() < 0.0025 * self.totals[0].max(self.totals[1]);
            (
                f64::NAN,
                !speck && trust::difference_colour(ratio).is_some(),
            )
        }
    }

    fn amount(&self, value: f64) -> String {
        match self.unit {
            "ns" => format!(
                "{}{}",
                if value >= 0.0 { "+" } else { "-" },
                units::duration(value.abs())
            ),
            _ => format!("{value:+.0}"),
        }
    }

    /// The comparison in one sentence: the triage's.
    fn sentence(&self, shape: bool) -> String {
        let mode = if shape { "shapes" } else { "absolute" };
        let noise = if self.poisson {
            format!("beyond {SIGMAS}σ of a sample count's noise")
        } else {
            "by over 5 % (time off cpu has no noise model)".to_owned()
        };
        match self.difference.first() {
            None => format!("{mode}: no function's own cost moved {noise}"),
            Some(first) => {
                let ratio = first
                    .ratio
                    .map_or("new".to_owned(), |ratio| format!("x{ratio:.2}"));
                let sigmas = if first.sigmas.is_finite() {
                    format!(", {:.1}σ", first.sigmas)
                } else {
                    String::new()
                };
                let rest = match self.difference.len() {
                    1 => "; nothing else beyond the noise".to_owned(),
                    n => format!("; {} more of {} moved", n - 1, self.functions),
                };
                format!(
                    "{mode}: the difference is `{}` {} own ({ratio}{sigmas}){rest}",
                    display_name(&first.function),
                    self.amount(first.delta),
                )
            }
        }
    }
}

/// A tree of frames, merged over every stack of a pane's results (of two
/// runs, in a comparison).
#[derive(Debug, Clone)]
struct Tree {
    nodes: Vec<Node>,
    unit: Unit,
    compare: Option<Compare>,
    /// A frame's node by its caller's and its name, so the two runs of a
    /// comparison land on the same nodes.
    index: HashMap<(usize, String), usize>,
}

impl Tree {
    fn new(unit: Unit) -> Tree {
        Tree {
            nodes: vec![Node {
                name: ALL.to_owned(),
                parent: 0,
                children: Vec::new(),
                value: [0.0; 2],
                own: [0.0; 2],
                events: [0; 2],
            }],
            unit,
            compare: None,
            index: HashMap::new(),
        }
    }

    /// Adds one stack's weight, root first, to run `run` (0 or 1).
    fn add<'a>(
        &mut self,
        frames: impl IntoIterator<Item = &'a str>,
        weight: f64,
        events: u64,
        run: usize,
    ) {
        let mut at = 0;
        self.nodes[0].value[run] += weight;
        self.nodes[0].events[run] += events;
        for frame in frames {
            let next = match self.index.get(&(at, frame.to_owned())) {
                Some(&next) => next,
                None => {
                    let next = self.nodes.len();
                    self.nodes.push(Node {
                        name: frame.to_owned(),
                        parent: at,
                        children: Vec::new(),
                        value: [0.0; 2],
                        own: [0.0; 2],
                        events: [0; 2],
                    });
                    self.nodes[at].children.push(next);
                    self.index.insert((at, frame.to_owned()), next);
                    next
                }
            };
            self.nodes[next].value[run] += weight;
            self.nodes[next].events[run] += events;
            at = next;
        }
        self.nodes[at].own[run] += weight;
    }

    /// The weight a node is drawn with: its value, or in a comparison the
    /// mean of the two comparable values -- linear, so a parent is never
    /// narrower than its children.
    fn weight(&self, node: usize) -> f64 {
        let value = self.nodes[node].value;
        match &self.compare {
            None => value[0],
            Some(compare) => {
                let (a, b) = compare.pair(value);
                (a + b) / 2.0
            }
        }
    }

    /// Children heaviest first, ties by name, so the hot path is leftmost and
    /// the order is the same every time.
    fn sort(&mut self) {
        let weights: Vec<f64> = (0..self.nodes.len()).map(|n| self.weight(n)).collect();
        for node in 0..self.nodes.len() {
            let mut children = std::mem::take(&mut self.nodes[node].children);
            children.sort_by(|&x, &y| {
                weights[y]
                    .total_cmp(&weights[x])
                    .then_with(|| self.nodes[x].name.cmp(&self.nodes[y].name))
            });
            self.nodes[node].children = children;
        }
    }

    fn path(&self, mut node: usize) -> Vec<String> {
        let mut path = Vec::new();
        while node != 0 {
            path.push(self.nodes[node].name.clone());
            node = self.nodes[node].parent;
        }
        path.reverse();
        path
    }

    /// The node at a path of names, or as far down it as the tree goes.
    fn find(&self, path: &[String]) -> usize {
        let mut at = 0;
        for name in path {
            match self.nodes[at]
                .children
                .iter()
                .find(|&&child| self.nodes[child].name == *name)
            {
                Some(&child) => at = child,
                None => break,
            }
        }
        at
    }

    fn is_under(&self, mut node: usize, ancestor: usize) -> bool {
        loop {
            if node == ancestor {
                return true;
            }
            if node == 0 {
                return false;
            }
            node = self.nodes[node].parent;
        }
    }

    /// Each function's own cost in each run, by the frame a sample ends in.
    fn own_by_function(&self) -> BTreeMap<&str, [f64; 2]> {
        let mut by: BTreeMap<&str, [f64; 2]> = BTreeMap::new();
        for node in &self.nodes[1..] {
            if node.own != [0.0; 2] {
                let entry = by.entry(node.name.as_str()).or_default();
                entry[0] += node.own[0];
                entry[1] += node.own[1];
            }
        }
        by
    }

    /// How many frames a search pattern matches, and the weight they account
    /// for: a frame matching under another matching one is counted once.
    fn matched(&self, pattern: &str) -> (usize, f64) {
        let mut frames = 0;
        let mut weight = 0.0;
        let mut stack = vec![(0usize, false)];
        while let Some((node, inside)) = stack.pop() {
            let hit = node != 0 && matches(&self.nodes[node].name, pattern);
            if hit {
                frames += 1;
                if !inside {
                    weight += self.weight(node);
                }
            }
            for &child in &self.nodes[node].children {
                stack.push((child, inside || hit));
            }
        }
        (frames, weight)
    }
}

fn matches(name: &str, pattern: &str) -> bool {
    !pattern.is_empty() && name.to_lowercase().contains(&pattern.to_lowercase())
}

fn refusal_of<'a>(data: &'a Data, need: &Need) -> Option<&'a Refusal> {
    let label = need.label();
    data.refused
        .iter()
        .find(|refusal| refusal.label == label)
        .or_else(|| data.refusal(need.source))
}

/// The stack emits of a result, with the samples that had no stack.
fn stack_outputs(results: &Results) -> impl Iterator<Item = (&[StackCount], u64)> + '_ {
    results.outputs.iter().filter_map(|output| match output {
        Output::Stack {
            stacks,
            unattributed,
            ..
        } => Some((stacks.as_slice(), *unattributed)),
        _ => None,
    })
}

/// How a pane is read from its results.
struct Reading<'a> {
    by_thread: bool,
    focus: &'a Focus,
}

/// Adds one run's results to a tree. Off cpu, from the blocks themselves
/// when the run kept them and a thread split or a focus asks for them;
/// otherwise from the stack emit. Returns what could not be done as asked.
fn fill(tree: &mut Tree, results: &[&Results], run: usize, reading: &Reading<'_>) -> Vec<String> {
    let mut asides = Vec::new();
    let narrowed = reading.focus.pid.is_some() || reading.focus.tid.is_some();
    let wants_events = reading.by_thread || narrowed;
    let named = |results: &Results, stack: u64| -> Vec<String> {
        let frames = results.frames(stack);
        if frames.is_empty() {
            vec![format!("[stack {stack}, unnamed]")]
        } else {
            frames.to_vec()
        }
    };
    for one in results {
        let events = one.events();
        let from_events = wants_events
            && events
                .iter()
                .any(|event| event.number("stack_id").is_some());
        if from_events {
            for event in events {
                if reading
                    .focus
                    .tid
                    .is_some_and(|tid| event.tid() != Some(tid))
                {
                    continue;
                }
                let pid = event
                    .pid()
                    .or_else(|| event.number("pid").map(|pid| pid as i32));
                if reading.focus.pid.is_some() && pid != reading.focus.pid {
                    continue;
                }
                let weight = event
                    .number("total_ns")
                    .or_else(|| event.number("weight"))
                    .unwrap_or(1.0);
                let mut path = Vec::new();
                if reading.by_thread {
                    let comm = event
                        .attr("comm")
                        .and_then(|value| value.as_str())
                        .unwrap_or("?");
                    let tid = event.tid().map_or("?".to_owned(), |tid| tid.to_string());
                    path.push(format!("[{comm} {tid}]"));
                }
                match event.number("stack_id") {
                    Some(stack) => path.extend(named(one, stack as u64)),
                    None => path.push("[no stack]".to_owned()),
                }
                tree.add(path.iter().map(String::as_str), weight, 1, run);
            }
            if one.losses.evicted > 0 {
                asides.push(format!(
                    "{} blocks let go before the end",
                    units::count(one.losses.evicted as f64)
                ));
            }
            if narrowed {
                asides.push("narrowed to the focus".to_owned());
            }
        } else {
            if reading.by_thread {
                asides.push(
                    "not split by thread: the run kept the stacks, not the blocks".to_owned(),
                );
            }
            if narrowed {
                asides.push("not narrowed to the focus: these stacks carry no thread".to_owned());
            }
            for (stacks, unattributed) in stack_outputs(one) {
                for stack in stacks {
                    let frames = named(one, stack.stack);
                    tree.add(
                        frames.iter().map(String::as_str),
                        stack.total,
                        stack.events,
                        run,
                    );
                }
                if unattributed > 0 && tree.unit != Unit::Nanos {
                    tree.add(["[no stack]"], unattributed as f64, unattributed, run);
                }
            }
        }
    }
    asides.sort();
    asides.dedup();
    asides
}

/// Builds one pane from the run, and from the run it is compared against.
fn build(
    data: &Data,
    against: Option<&Data>,
    need: &Need,
    title: &'static str,
    shape: bool,
    reading: &Reading<'_>,
) -> Pane {
    let held = data.for_need(&need.label(), need.source);
    let mut pane = Pane {
        title,
        source: need.source,
        trust: Trust::joined(data, held.iter().copied()),
        caveats: Vec::new(),
        aside: None,
        body: Body::Missing,
    };
    if held.is_empty() {
        if let Some(refusal) = refusal_of(data, need) {
            pane.body = Body::Refused(refusal.clone());
        }
        return pane;
    }
    for one in &held {
        pane.caveats.extend(one.degradations.iter().cloned());
    }
    for one in &held {
        pane.caveats.extend(
            one.notes
                .iter()
                .filter(|note| !note.starts_with("stacks unwound by"))
                .cloned(),
        );
    }
    if data.live.is_some() {
        pane.body = live_body(data, &held);
        return pane;
    }
    let unit = Unit::of(held[0]);
    let mut tree = Tree::new(unit);
    let mut asides = fill(&mut tree, &held, 0, reading);
    let other = against
        .filter(|against| against.live.is_none())
        .map(|against| against.for_need(&need.label(), need.source))
        .filter(|other| !other.is_empty());
    if against.is_some() && other.is_none() {
        asides.push("the other run has none: drawn alone".to_owned());
    }
    if let (Some(against), Some(other)) = (against, &other) {
        // The other run is read the same way; what it could not do is the
        // same as this one's.
        let _ = fill(&mut tree, other, 1, reading);
        pane.trust
            .merge(Trust::joined(against, other.iter().copied()));
        tree.compare = Some(compare(&tree, held[0], other[0], shape));
    }
    pane.aside = (!asides.is_empty()).then(|| asides.join("; "));
    if tree.nodes[0].value == [0.0; 2] {
        pane.body = Body::Empty(match unit {
            Unit::Nanos => format!(
                "no block was recorded: the target never waited, or never long enough for \
                 `{}` to see it ({} events)",
                need.source,
                held.iter().map(|one| one.events_seen).sum::<u64>()
            ),
            _ => format!("no sample was recorded by `{}`", need.source),
        });
        return pane;
    }
    tree.sort();
    pane.body = Body::Tree(tree);
    pane
}

/// The comparison of a tree's two runs: the triage's arithmetic.
fn compare(tree: &Tree, first: &Results, second: &Results, shape: bool) -> Compare {
    let mut notes = Vec::new();
    let poisson = tree.unit != Unit::Nanos;
    let (unit, mut scale) = match (tree.unit, Unit::of(second)) {
        (Unit::Nanos, _) => ("ns", [1.0, 1.0]),
        (Unit::Samples(Some(a)), Unit::Samples(Some(b))) if a == b => ("samples", [1.0, 1.0]),
        (Unit::Samples(Some(a)), Unit::Samples(Some(b))) => {
            notes.push(format!(
                "the two runs were sampled at two rates ({:.0} Hz and {:.0} Hz): compared in time",
                1e9 / a as f64,
                1e9 / b as f64
            ));
            ("ns", [a as f64, b as f64])
        }
        _ => {
            notes.push(
                "a sampling rate is unknown: both are taken to be one rate, compared in samples"
                    .to_owned(),
            );
            ("samples", [1.0, 1.0])
        }
    };
    let raw = tree.nodes[0].value;
    if shape && raw[1] > 0.0 {
        scale[1] *= raw[0] * scale[0] / (raw[1] * scale[1]);
    }
    for (which, results) in [("this run", first), ("the other", second)] {
        let missing = results.losses.missing();
        if missing > 0 {
            notes.push(format!(
                "{which} lost {}: part of the difference is in what each lost",
                units::count(missing as f64)
            ));
        }
    }
    let mut compare = Compare {
        scale,
        poisson,
        unit,
        totals: [raw[0] * scale[0], raw[1] * scale[1]],
        difference: Vec::new(),
        functions: 0,
        notes,
    };
    let own = tree.own_by_function();
    compare.functions = own.len();
    let mut difference: Vec<Change> = own
        .into_iter()
        .filter_map(|(function, value)| {
            let (sigmas, significant) = compare.judge(value);
            let (a, b) = compare.pair(value);
            significant.then(|| Change {
                function: function.to_owned(),
                delta: b - a,
                ratio: (a > 0.0).then(|| b / a),
                sigmas,
            })
        })
        .collect();
    difference.sort_by(|x, y| {
        y.delta
            .abs()
            .total_cmp(&x.delta.abs())
            .then_with(|| x.function.cmp(&y.function))
    });
    compare.difference = difference;
    compare
}

/// A live pane: what has arrived, over time. The runner names stacks only
/// when the run ends.
fn live_body(data: &Data, held: &[&Results]) -> Body {
    let opened = data.opened_ns();
    let now = data.closed_ns().max(opened + 1);
    let mut arrivals = Vec::new();
    let mut stacks = BTreeSet::new();
    let mut events = 0;
    for one in held {
        for event in one.events() {
            events += 1;
            if let Some(stack) = event.number("stack_id") {
                stacks.insert(stack as u64);
            }
            arrivals
                .push((event.ts_ns.saturating_sub(opened) as f64 / (now - opened) as f64) as f32);
        }
    }
    Body::Live {
        events,
        stacks: stacks.len(),
        arrivals,
    }
}

/// One box on screen: a chain of frames from `start` down to `node`, folded
/// into one because nothing else under it would be a cell wide, and the
/// cells it covers.
#[derive(Debug, Clone, Copy)]
struct Cell {
    start: usize,
    node: usize,
    x0: u16,
    x1: u16,
}

/// A tree laid out from its zoom root into `width` cells, one row per box
/// below the one above it. A frame whose heaviest callee holds all of it to
/// within a cell is folded with that callee into one box -- a call chain
/// that never branches visibly takes one row, not one per frame -- so every
/// column goes as deep as it branches, not as deep as its stack.
struct Icicle {
    rows: Vec<Vec<Cell>>,
}

/// Where a chain that starts at `start` ends: as far down its heaviest
/// callees as nothing else is a cell wide.
fn fold(tree: &Tree, start: usize, cell: f64) -> usize {
    let mut at = start;
    while let Some(&heaviest) = tree.nodes[at].children.first() {
        if tree.weight(at) - tree.weight(heaviest) >= cell {
            break;
        }
        at = heaviest;
    }
    at
}

fn lay_out(tree: &Tree, zoom: usize, width: u16) -> Icicle {
    let total = tree.weight(zoom).max(f64::MIN_POSITIVE);
    let cell = total / f64::from(width.max(1));
    let top = fold(tree, zoom, cell);
    let mut rows = vec![vec![Cell {
        start: zoom,
        node: top,
        x0: 0,
        x1: width,
    }]];
    let scale = f64::from(width) / total;
    let mut level = vec![(top, 0.0f64)];
    for _ in 0..512 {
        let mut next = Vec::new();
        let mut row = Vec::new();
        for &(parent, start) in &level {
            let mut x = start;
            for &child in &tree.nodes[parent].children {
                let w = tree.weight(child) * scale;
                let x0 = x.round() as u16;
                let x1 = (x + w).round().min(f64::from(width)) as u16;
                if x1 > x0 {
                    let end = fold(tree, child, cell);
                    row.push(Cell {
                        start: child,
                        node: end,
                        x0,
                        x1,
                    });
                    next.push((end, x));
                }
                x += w;
            }
        }
        if row.is_empty() {
            break;
        }
        rows.push(row);
        level = next;
    }
    Icicle { rows }
}

impl Icicle {
    /// The row and position of the box a node is drawn in, if it is.
    fn locate(&self, tree: &Tree, node: usize) -> Option<(usize, usize)> {
        self.rows.iter().enumerate().find_map(|(r, row)| {
            row.iter()
                .position(|cell| tree.is_under(cell.node, node) && tree.is_under(node, cell.start))
                .map(|i| (r, i))
        })
    }

    /// The box the selection is in: the selected frame's, or the first one.
    fn selected(&self, tree: &Tree, node: usize) -> (usize, usize) {
        if node == 0 {
            return (0, 0);
        }
        self.locate(tree, node).unwrap_or((0, 0))
    }

    /// How many frames deep the deepest box goes.
    fn depth(&self, tree: &Tree) -> usize {
        let depth = |mut node: usize| {
            let mut depth = 0;
            while node != 0 {
                depth += 1;
                node = tree.nodes[node].parent;
            }
            depth
        };
        self.rows
            .iter()
            .flatten()
            .map(|cell| depth(cell.node))
            .max()
            .unwrap_or(0)
    }
}

/// The frames a box folds, from its first to its last.
fn chain(tree: &Tree, cell: &Cell) -> Vec<usize> {
    let mut frames = Vec::new();
    let mut at = cell.node;
    loop {
        frames.push(at);
        if at == cell.start || at == 0 {
            break;
        }
        at = tree.nodes[at].parent;
    }
    frames.reverse();
    frames
}

/// A box's label in `width` cells: the longest of what fits, from the chain
/// named whole with its share, down to the last frame's offset alone.
fn label(tree: &Tree, cell: &Cell, share: Option<String>, width: usize) -> String {
    let frames: Vec<String> = chain(tree, cell)
        .into_iter()
        .map(|node| {
            if node == 0 {
                ALL.to_owned()
            } else {
                display_name(&tree.nodes[node].name)
            }
        })
        .collect();
    let last = frames.last().cloned().unwrap_or_default();
    let named = match frames.len() {
        1 => last.clone(),
        2 => format!("{} › {}", frames[0], last),
        n => format!("{} › ⋯{}⋯ › {}", frames[0], n - 2, last),
    };
    // Shorter and shorter: the chain, its last frame, the last frame's
    // offset alone (a module's unnamed frames all share the module).
    let mut forms = vec![named];
    if frames.len() > 1 {
        forms.push(format!("⋯{last}"));
    }
    forms.push(last.clone());
    if let Some((_, offset)) = last.rsplit_once("+0x") {
        forms.push(format!("+0x{offset}"));
    }
    let share = share.map(|share| format!(" {share}"));
    let mut candidates: Vec<String> = Vec::new();
    if let Some(share) = &share {
        candidates.extend(forms.iter().map(|form| format!("{form}{share}")));
    }
    candidates.extend(forms);
    candidates
        .into_iter()
        .find(|candidate| candidate.chars().count() <= width)
        .unwrap_or_else(|| fit(&last, width))
}

/// A frame's name in few cells: a `module+0xoffset` frame loses its
/// module's version suffix.
fn display_name(name: &str) -> String {
    if let Some((module, offset)) = name.rsplit_once("+0x") {
        if let Some(stem) = module.find(".so").map(|at| &module[..at]) {
            return format!("{stem}+0x{offset}");
        }
    }
    name.to_owned()
}

/// Fits a text into `width` cells, with an ellipsis when it was cut.
fn fit(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_owned();
    }
    match width {
        0 => String::new(),
        1..=3 => text.chars().take(width).collect(),
        _ => {
            let mut cut: String = text.chars().take(width - 1).collect();
            cut.push('…');
            cut
        }
    }
}

/// Fits a text into `width` cells, cutting from its start: the end of a
/// path is the part that says where one is.
fn fit_start(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut cut = String::from("…");
    cut.extend(text.chars().skip(count - (width - 1)));
    cut
}

/// The colour of a frame: warm on cpu, cool off it, by a hash of the name so
/// a function has one colour wherever it is -- and never the colour of the
/// frame just left of it, so two frames stay two.
fn frame_colour(pane: usize, name: &str, avoid: Option<Color>) -> Color {
    const WARM: [Color; 3] = [Color::Yellow, Color::LightYellow, Color::LightRed];
    const COOL: [Color; 3] = [Color::LightBlue, Color::Cyan, Color::LightCyan];
    let colours = if pane == ON { &WARM } else { &COOL };
    let hash = name.bytes().fold(2_166_136_261u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16_777_619)
    });
    let mut pick = colours[hash as usize % colours.len()];
    if Some(pick) == avoid {
        pick = colours[(hash as usize + 1) % colours.len()];
    }
    pick
}

/// The colour of a frame in a comparison: red where the second run spent
/// more, blue where less -- strong at half again, or half, or a frame one run
/// lacks -- grey within the noise.
fn difference_colour(compare: &Compare, value: [f64; 2]) -> Color {
    let (_, significant) = compare.judge(value);
    let (a, b) = compare.pair(value);
    let ratio = if a > 0.0 { b / a } else { f64::INFINITY };
    let strong = !(0.5..=1.5).contains(&ratio);
    match (significant, b > a, strong) {
        (false, _, _) => Color::DarkGray,
        (true, true, true) => palette::WORSE,
        (true, true, false) => Color::LightRed,
        (true, false, true) => palette::BETTER,
        (true, false, false) => Color::LightBlue,
    }
}

fn text_on(background: Color) -> Color {
    match background {
        Color::Red | Color::Blue | Color::DarkGray | Color::Magenta | Color::Green => Color::White,
        _ => Color::Black,
    }
}

impl Flame {
    /// The panes, built anew when what they are built from changed.
    fn panes(&mut self, ctx: &Ctx<'_>) -> [Pane; 2] {
        let fingerprint = Fingerprint::of(ctx, self.shape, self.by_thread);
        if let Some((held, panes)) = &self.cache {
            if *held == fingerprint {
                return panes.clone();
            }
        }
        let on = build(
            ctx.data,
            ctx.against,
            &NEEDS[ON],
            "on cpu",
            self.shape,
            &Reading {
                by_thread: false,
                focus: ctx.focus,
            },
        );
        let off = build(
            ctx.data,
            ctx.against,
            &NEEDS[OFF],
            "off cpu",
            self.shape,
            &Reading {
                by_thread: self.by_thread,
                focus: ctx.focus,
            },
        );
        let panes = [on, off];
        self.cache = Some((fingerprint, panes.clone()));
        panes
    }

    /// The pane the keys move in: the one asked, unless only the other has a
    /// tree.
    fn active_pane(&self, panes: &[Pane; 2]) -> usize {
        let tree = |pane: &Pane| matches!(pane.body, Body::Tree(_));
        if tree(&panes[self.active]) || !tree(&panes[1 - self.active]) {
            self.active
        } else {
            1 - self.active
        }
    }

    /// Whether the two panes are drawn side by side at this width.
    fn side_by_side(&self, panes: &[Pane; 2], width: u16) -> bool {
        !self.alone && width >= 80 && panes.iter().all(|pane| matches!(pane.body, Body::Tree(_)))
    }

    /// The width the active pane's icicle was drawn at, inside its borders.
    fn icicle_width(&self, panes: &[Pane; 2], active: usize) -> u16 {
        let total = self.last_width.max(4);
        if self.side_by_side(panes, total) {
            let left = total / 2;
            if active == ON {
                left - 2
            } else {
                total - left - 2
            }
        } else {
            total - 2
        }
    }

    fn move_in(&mut self, tree: &Tree, active: usize, width: u16, code: KeyCode) {
        let state = &mut self.panes[active];
        let zoom = tree.find(&state.zoom);
        let icicle = lay_out(tree, zoom, width);
        let (row, index) = icicle.selected(tree, tree.find(&state.selected));
        let here = icicle.rows[row][index];
        let next = match code {
            KeyCode::Down | KeyCode::Char('j') => icicle
                .rows
                .get(row + 1)
                .and_then(|below| {
                    below
                        .iter()
                        .find(|cell| tree.nodes[cell.start].parent == here.node)
                })
                .map(|cell| cell.node),
            KeyCode::Up | KeyCode::Char('k') if row > 0 => {
                let parent = tree.nodes[here.start].parent;
                icicle
                    .locate(tree, parent)
                    .map(|(r, i)| icicle.rows[r][i].node)
            }
            KeyCode::Char('h') | KeyCode::Char('[') if index > 0 => {
                Some(icicle.rows[row][index - 1].node)
            }
            KeyCode::Char('l') | KeyCode::Char(']') => {
                icicle.rows[row].get(index + 1).map(|cell| cell.node)
            }
            KeyCode::Enter => {
                state.zoom = tree.path(here.start);
                state.scroll = 0;
                Some(here.node)
            }
            KeyCode::Backspace | KeyCode::Char('u') => {
                // Out to the nearest ancestor that shows more than this one,
                // by at least a cell.
                let cell = tree.weight(zoom) / f64::from(width.max(1));
                let mut out = zoom;
                while out != 0 {
                    out = tree.nodes[out].parent;
                    if tree.weight(out) - tree.weight(zoom) >= cell {
                        break;
                    }
                }
                state.zoom = tree.path(out);
                None
            }
            _ => None,
        };
        if let Some(next) = next {
            state.selected = tree.path(next);
        }
    }
}

impl View for Flame {
    fn name(&self) -> &'static str {
        "flame"
    }

    fn title(&self) -> &'static str {
        "a flamegraph in the terminal: on-cpu and off-cpu stacks, and two runs' difference"
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn keys(&self) -> &'static [(&'static str, &'static str)] {
        KEYS
    }

    fn key(&mut self, key: KeyEvent, ctx: &Ctx<'_>) -> Action {
        if let Some(typed) = self.typing.as_mut() {
            match key.code {
                KeyCode::Enter => {
                    let pattern = self.typing.take().unwrap_or_default();
                    self.search = (!pattern.is_empty()).then_some(pattern);
                }
                KeyCode::Esc => self.typing = None,
                KeyCode::Backspace => {
                    typed.pop();
                }
                KeyCode::Char(c) => typed.push(c),
                _ => {}
            }
            return Action::None;
        }
        let panes = self.panes(ctx);
        let active = self.active_pane(&panes);
        self.active = active;
        match key.code {
            KeyCode::Char('/') => self.typing = Some(String::new()),
            KeyCode::Char('*') => {
                if let Body::Tree(tree) = &panes[active].body {
                    let node = tree.find(&self.panes[active].selected);
                    if node != 0 {
                        self.search = Some(tree.nodes[node].name.clone());
                    }
                }
            }
            KeyCode::Esc => {
                self.search = None;
                self.panes[active].zoom.clear();
            }
            KeyCode::Char('o') => {
                if matches!(panes[1 - active].body, Body::Tree(_)) {
                    self.active = 1 - active;
                }
            }
            KeyCode::Char('m') => self.alone = !self.alone,
            KeyCode::Char('d') if ctx.against.is_some() => self.shape = !self.shape,
            KeyCode::Char('t') => self.by_thread = !self.by_thread,
            code => {
                if let Body::Tree(tree) = &panes[active].body {
                    let width = self.icicle_width(&panes, active);
                    self.move_in(tree, active, width, code);
                }
            }
        }
        Action::None
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        // The view's name and title on a rule, as every view says what it is.
        let rule = Block::new()
            .borders(Borders::TOP)
            .border_style(Style::new().add_modifier(Modifier::DIM))
            .title(Span::styled(
                format!(" {} -- {} ", self.name(), self.title()),
                Style::new().add_modifier(Modifier::BOLD),
            ));
        let area = {
            let inner = rule.inner(area);
            frame.render_widget(rule, area);
            inner
        };
        self.last_width = area.width;
        self.caveats_said = [false; 2];
        let panes = self.panes(ctx);
        let active = self.active_pane(&panes);
        self.active = active;
        let [top, middle, detail, bottom] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .areas(area);
        frame.render_widget(Paragraph::new(self.summary(&panes)), top);

        let is_tree = |pane: &Pane| matches!(pane.body, Body::Tree(_));
        if self.alone && is_tree(&panes[active]) {
            self.draw_pane(frame, middle, &panes[active], active, true);
        } else if self.side_by_side(&panes, middle.width) {
            let left = middle.width / 2;
            let [one, two] =
                Layout::horizontal([Constraint::Length(left), Constraint::Min(0)]).areas(middle);
            self.draw_pane(frame, one, &panes[ON], ON, active == ON);
            self.draw_pane(frame, two, &panes[OFF], OFF, active == OFF);
        } else if panes.iter().any(is_tree) {
            // The pane with a tree takes the room; the other says in a strip
            // why it has none.
            let (big, small) = if is_tree(&panes[active]) {
                (active, 1 - active)
            } else {
                (1 - active, active)
            };
            let strip = if is_tree(&panes[small]) {
                middle.height / 2
            } else {
                4
            };
            let [one, two] =
                Layout::vertical([Constraint::Min(0), Constraint::Length(strip)]).areas(middle);
            self.draw_pane(frame, one, &panes[big], big, true);
            self.draw_pane(frame, two, &panes[small], small, false);
        } else {
            let [one, two] =
                Layout::vertical([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).areas(middle);
            self.draw_pane(frame, one, &panes[ON], ON, false);
            self.draw_pane(frame, two, &panes[OFF], OFF, false);
        }
        frame.render_widget(Paragraph::new(self.detail(&panes[active], active)), detail);
        frame.render_widget(Paragraph::new(self.bottom(&panes[active], active)), bottom);
    }
}

impl Flame {
    /// The line above the panes: the search's share while there is one; in a
    /// comparison, the triage's sentence; otherwise each pane's heaviest own
    /// function.
    fn summary(&self, panes: &[Pane; 2]) -> Line<'static> {
        let mut spans = Vec::new();
        let bold = Style::new().add_modifier(Modifier::BOLD);
        if let Some(pattern) = &self.search {
            spans.push(Span::styled(
                format!("/{pattern}/ "),
                Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            ));
            let mut first = true;
            for pane in panes {
                if let Body::Tree(tree) = &pane.body {
                    let (frames, weight) = tree.matched(pattern);
                    if !first {
                        spans.push(Span::raw(" │ "));
                    }
                    first = false;
                    spans.push(Span::styled(format!("{}: ", pane.title), bold));
                    spans.push(Span::styled(
                        format!(
                            "{frames} frames, {} of it",
                            units::percent(100.0 * weight / tree.weight(0).max(f64::MIN_POSITIVE))
                        ),
                        pane.trust.style(),
                    ));
                }
            }
            return Line::from(spans);
        }
        let active = self.active_pane(panes);
        if let Body::Tree(tree) = &panes[active].body {
            if let Some(compare) = &tree.compare {
                let ratio = compare.totals[1] / compare.totals[0].max(f64::MIN_POSITIVE);
                spans.push(Span::styled(format!("{}: ", panes[active].title), bold));
                spans.push(Span::raw(format!(
                    "{} → {} (x{ratio:.2}) · ",
                    tree.unit.short(tree.nodes[0].value[0]),
                    tree.unit.short(tree.nodes[0].value[1]),
                )));
                spans.push(Span::raw(compare.sentence(self.shape)));
                return Line::from(spans);
            }
        }
        for (index, pane) in panes.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw(" │ "));
            }
            spans.push(Span::styled(format!("{}: ", pane.title), bold));
            match &pane.body {
                Body::Tree(tree) => {
                    let total = tree.weight(0).max(f64::MIN_POSITIVE);
                    let hottest = tree
                        .own_by_function()
                        .into_iter()
                        .max_by(|x, y| x.1[0].total_cmp(&y.1[0]).then_with(|| y.0.cmp(x.0)));
                    spans.push(Span::styled(
                        format!("{} · ", tree.unit.short(tree.weight(0))),
                        pane.trust.style(),
                    ));
                    if let Some((name, own)) = hottest {
                        spans.push(Span::raw("hottest own "));
                        spans.push(Span::styled(display_name(name), bold));
                        spans.push(Span::styled(
                            format!(" {}", units::percent(100.0 * own[0] / total)),
                            pane.trust.style(),
                        ));
                    }
                }
                Body::Live { events, .. } => spans.push(Span::styled(
                    format!("{} so far", units::count(*events as f64)),
                    pane.trust.style(),
                )),
                Body::Empty(_) => spans.push(Span::raw("nothing recorded")),
                Body::Refused(_) => {
                    spans.push(Span::styled("refused", Style::new().fg(palette::REFUSED)))
                }
                Body::Missing => spans.push(Span::raw("not in this run")),
            }
        }
        Line::from(spans)
    }

    /// The two lines under the panes: the selected frame, whole.
    fn detail(&self, pane: &Pane, index: usize) -> Vec<Line<'static>> {
        let Body::Tree(tree) = &pane.body else {
            return Vec::new();
        };
        let state = &self.panes[index];
        let zoom = tree.find(&state.zoom);
        let icicle = lay_out(tree, zoom, self.icicle_width_for(index));
        let (row, at) = icicle.selected(tree, tree.find(&state.selected));
        let cell = icicle.rows[row][at];
        let node = cell.node;
        let one = &tree.nodes[node];
        let style = pane.trust.style();
        let mut first = vec![
            Span::styled(
                format!("{} ▸ ", pane.title),
                Style::new()
                    .fg(palette::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(one.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
        ];
        let folded = chain(tree, &cell);
        if folded.len() > 1 {
            first.push(Span::styled(
                format!(
                    "  the last of {} frames called in one chain from {}",
                    folded.len(),
                    display_name(&tree.nodes[cell.start].name)
                ),
                Style::new().add_modifier(Modifier::DIM),
            ));
        }
        let total = tree.weight(0).max(f64::MIN_POSITIVE);
        let mut second = Vec::new();
        match &tree.compare {
            None => {
                let of_zoom = if zoom != 0 && node != zoom {
                    format!(
                        ", {} of the zoom",
                        units::percent(
                            100.0 * one.value[0] / tree.weight(zoom).max(f64::MIN_POSITIVE)
                        )
                    )
                } else {
                    String::new()
                };
                second.push(Span::styled(
                    format!(
                        "{} · {} of all{of_zoom} · own {} · {} {}",
                        tree.unit.say(one.value[0]),
                        units::percent(100.0 * one.value[0] / total),
                        tree.unit.short(one.own[0]),
                        units::count(one.events[0] as f64),
                        if tree.unit == Unit::Nanos {
                            "blocks"
                        } else {
                            "events"
                        },
                    ),
                    style,
                ));
            }
            Some(compare) => {
                let (a, b) = compare.pair(one.value);
                let (sigmas, significant) = compare.judge(one.value);
                let say = |v: f64| match compare.unit {
                    "ns" => units::duration(v),
                    _ => units::count(v),
                };
                let ratio = if a > 0.0 {
                    format!("x{:.2}", b / a)
                } else {
                    "new".to_owned()
                };
                let colour = if !significant {
                    Color::Reset
                } else if b > a {
                    palette::WORSE
                } else {
                    palette::BETTER
                };
                second.push(Span::styled(
                    format!(
                        "this run {} → the other {}{}",
                        say(a),
                        say(b),
                        if compare.unit == "ns" { "" } else { " samples" }
                    ),
                    style,
                ));
                second.push(Span::raw(" ("));
                second.push(Span::styled(
                    format!("{}, {ratio}", compare.amount(b - a)),
                    style.fg(colour),
                ));
                if sigmas.is_finite() {
                    second.push(Span::styled(
                        format!(
                            ", {sigmas:.1}σ{}",
                            if significant {
                                ""
                            } else {
                                ": within the noise"
                            }
                        ),
                        style,
                    ));
                }
                second.push(Span::raw(")"));
                let (own_a, own_b) = compare.pair(one.own);
                if own_a > 0.0 || own_b > 0.0 {
                    second.push(Span::styled(
                        format!(" · own {} → {}", say(own_a), say(own_b)),
                        style,
                    ));
                }
            }
        }
        vec![Line::from(first), Line::from(second)]
    }

    /// The icicle width of a pane, from the last draw.
    fn icicle_width_for(&self, index: usize) -> u16 {
        match &self.cache {
            Some((_, panes)) => self.icicle_width(panes, index),
            None => self.last_width.saturating_sub(2).max(1),
        }
    }

    /// The last line: the search being typed, or the comparison's notes and
    /// the active pane's caveats, the first of them in full.
    fn bottom(&self, pane: &Pane, index: usize) -> Line<'static> {
        if let Some(typed) = &self.typing {
            return Line::from(vec![
                Span::styled(
                    format!("/{typed}█"),
                    Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "  enter searches, esc cancels",
                    Style::new().add_modifier(Modifier::DIM),
                ),
            ]);
        }
        let mut said: Vec<String> = Vec::new();
        if let Body::Tree(tree) = &pane.body {
            if let Some(compare) = &tree.compare {
                said.extend(compare.notes.iter().cloned());
            }
        }
        if !self.caveats_said[index] {
            said.extend(pane.caveats.iter().cloned());
        }
        match said.split_first() {
            None => Line::from(Span::styled(
                "⏎ zooms into the selected box, ⌫ out · / finds a name, * the selected one · o \
                 switches pane, m shows it alone · t splits off cpu by thread",
                Style::new().add_modifier(Modifier::DIM),
            )),
            Some((first, rest)) => {
                let more = if rest.is_empty() {
                    String::new()
                } else {
                    format!(" (+{} more)", rest.len())
                };
                Line::from(vec![
                    Span::styled(
                        format!("{}{more}: ", pane.source),
                        Style::new().fg(palette::LOSS),
                    ),
                    Span::styled(first.clone(), Style::new().add_modifier(Modifier::DIM)),
                ])
            }
        }
    }

    fn draw_pane(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        pane: &Pane,
        index: usize,
        active: bool,
    ) {
        let border = if active {
            Style::new().fg(palette::ACCENT)
        } else {
            Style::new().add_modifier(Modifier::DIM)
        };
        let mut title = vec![
            Span::styled(
                format!(" {} ", pane.title),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} ", pane.source),
                Style::new().add_modifier(Modifier::DIM),
            ),
        ];
        if let Body::Tree(tree) = &pane.body {
            let total = tree.nodes[0].value;
            let said = match &tree.compare {
                None => tree.unit.short(total[0]),
                Some(_) => format!(
                    "{} vs {}",
                    tree.unit.short(total[0]),
                    tree.unit.short(total[1])
                ),
            };
            title.push(Span::styled(format!("{said} "), pane.trust.style()));
            title.extend(pane.trust.badges());
            title.push(Span::raw(" "));
        }
        let mut block = Block::new()
            .borders(Borders::ALL)
            .border_style(border)
            .title(Line::from(title));
        if let Some(aside) = &pane.aside {
            block = block.title_bottom(Line::from(Span::styled(
                format!(" {aside} "),
                Style::new().fg(palette::LOSS),
            )));
        }
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let said = |lines: Vec<Line<'static>>| Paragraph::new(lines).wrap(Wrap { trim: true });
        match &pane.body {
            Body::Refused(refusal) => {
                frame.render_widget(said(trust::refusal_lines(refusal)), inner)
            }
            Body::Missing => frame.render_widget(
                said(vec![Line::from(Span::styled(
                    format!(
                        "{} is not in this run: the session that made it did not ask for it",
                        pane.source
                    ),
                    Style::new().add_modifier(Modifier::DIM),
                ))]),
                inner,
            ),
            Body::Empty(why) => {
                frame.render_widget(said(vec![trust::figure(why.clone(), &pane.trust)]), inner)
            }
            Body::Live {
                events,
                stacks,
                arrivals,
            } => {
                let bins = usize::from(inner.width.max(1));
                let mut spark = vec![0u64; bins];
                for at in arrivals {
                    spark[((*at * bins as f32) as usize).min(bins - 1)] += 1;
                }
                let [words, chart] =
                    Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(inner);
                frame.render_widget(
                    said(vec![
                        trust::figure(
                            format!(
                                "{} {} on {} stacks so far",
                                units::count(*events as f64),
                                if pane.source == "sched.offcpu" {
                                    "blocks"
                                } else {
                                    "samples"
                                },
                                units::count(*stacks as f64)
                            ),
                            &pane.trust,
                        ),
                        Line::from(Span::styled(
                            "the icicle is drawn when the run ends: the runner names the stacks \
                             then. Below, what arrived over time.",
                            Style::new().add_modifier(Modifier::ITALIC | Modifier::DIM),
                        )),
                    ]),
                    words,
                );
                frame.render_widget(
                    Sparkline::default()
                        .data(&spark)
                        .style(pane.trust.style().fg(palette::LIVE)),
                    chart,
                );
            }
            Body::Tree(tree) => self.draw_tree(frame, inner, pane, tree, index, active),
        }
    }

    fn draw_tree(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        pane: &Pane,
        tree: &Tree,
        index: usize,
        active: bool,
    ) {
        if area.width < 4 || area.height < 2 {
            return;
        }
        let pattern = self.search.clone();
        let state = &mut self.panes[index];
        let zoom = tree.find(&state.zoom);
        let icicle = lay_out(tree, zoom, area.width);
        let (selected_row, selected_index) = icicle.selected(tree, tree.find(&state.selected));
        let selected = icicle.rows[selected_row][selected_index];

        // The breadcrumb: where the zoom is, and how deep the tree goes.
        let [crumb, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        let right = format!(" {} frames deep ", icicle.depth(tree));
        let room = usize::from(area.width).saturating_sub(right.chars().count());
        let crumb_text = if zoom == 0 {
            format!(
                "width ∝ {} · heaviest left",
                match (tree.unit, &tree.compare) {
                    (_, Some(_)) => "the mean of the two runs",
                    (Unit::Nanos, None) => "time off cpu",
                    (Unit::Samples(_), None) => "samples",
                }
            )
        } else {
            let mut path: Vec<String> = vec![ALL.to_owned()];
            path.extend(tree.path(zoom).iter().map(|name| display_name(name)));
            format!(
                "zoom: {}",
                fit_start(&path.join(" › "), room.saturating_sub(6))
            )
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{:<room$}", fit(&crumb_text, room)),
                    Style::new().fg(palette::ACCENT),
                ),
                Span::styled(right, Style::new().add_modifier(Modifier::DIM)),
            ])),
            crumb,
        );

        // The icicle takes the rows it needs; what is left under it lists
        // the heaviest functions on their own, then the source's caveats.
        let rows = icicle.rows.len() as u16;
        let (height, under) = if body.height >= rows + 5 {
            (rows, body.height - rows - 1)
        } else {
            (body.height, 0)
        };
        let height = usize::from(height);
        if selected_row < state.scroll {
            state.scroll = selected_row;
        } else if selected_row >= state.scroll + height {
            state.scroll = selected_row + 1 - height;
        }
        state.scroll = state.scroll.min(icicle.rows.len().saturating_sub(height));
        let scroll = state.scroll;
        let dim = pane.trust.dims();
        let difference: BTreeSet<&str> = tree
            .compare
            .as_ref()
            .map(|compare| {
                compare
                    .difference
                    .iter()
                    .map(|change| change.function.as_str())
                    .collect()
            })
            .unwrap_or_default();
        let total = tree.weight(zoom).max(f64::MIN_POSITIVE);
        let buffer = frame.buffer_mut();
        for (r, row) in icicle.rows.iter().enumerate().skip(scroll).take(height) {
            let y = body.y + (r - scroll) as u16;
            let mut left: Option<Color> = None;
            for cell in row {
                let node = &tree.nodes[cell.node];
                let width = usize::from(cell.x1 - cell.x0);
                let mut background = match &tree.compare {
                    Some(compare) => difference_colour(compare, node.value),
                    None => frame_colour(index, &node.name, left),
                };
                left = Some(background);
                let mut modifier = Modifier::empty();
                let hit = pattern.as_deref().is_some_and(|pattern| {
                    chain(tree, cell)
                        .iter()
                        .any(|&n| n != 0 && matches(&tree.nodes[n].name, pattern))
                });
                if hit {
                    background = Color::Magenta;
                    modifier |= Modifier::BOLD;
                }
                if cell.start == selected.start && cell.node == selected.node {
                    background = if active { palette::ACCENT } else { Color::Gray };
                    modifier |= Modifier::BOLD;
                }
                if difference.contains(node.name.as_str()) {
                    modifier |= Modifier::UNDERLINED;
                }
                if dim {
                    modifier |= Modifier::DIM;
                }
                // The top box says how much there is; the others, their share.
                let share = if r == 0 {
                    tree.unit.short(tree.weight(cell.node))
                } else {
                    units::percent(100.0 * tree.weight(cell.node) / total)
                };
                // A box's first cell is its edge, so two boxes of one colour
                // stay two.
                let text = if width >= 2 {
                    let words = label(tree, cell, Some(share), width - 1);
                    format!("▏{words:<w$}", w = width - 1)
                } else {
                    " ".repeat(width)
                };
                let style = Style::new()
                    .bg(background)
                    .fg(text_on(background))
                    .add_modifier(modifier);
                buffer.set_stringn(body.x + cell.x0, y, &text, width, style);
            }
        }
        if icicle.rows.len() > scroll + height {
            let more = icicle.rows.len() - scroll - height;
            let text = format!(" ↓{more} rows ");
            let x = body.x + body.width.saturating_sub(text.chars().count() as u16);
            buffer.set_string(
                x,
                body.y + height as u16 - 1,
                text,
                Style::new().add_modifier(Modifier::REVERSED),
            );
        }
        if under > 0 {
            let rest = Rect {
                y: body.y + height as u16 + 1,
                height: under,
                ..body
            };
            let (lines, said) = self.ranking(pane, tree, zoom, rest);
            self.caveats_said[index] = said;
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rest);
        }
    }

    /// The functions heaviest on their own under the zoom -- where the
    /// samples end, which an icicle spreads over every path that reaches
    /// them -- with a bar each; in a comparison, the largest movements. Then
    /// the source's caveats, as room allows.
    fn ranking(
        &self,
        pane: &Pane,
        tree: &Tree,
        zoom: usize,
        area: Rect,
    ) -> (Vec<Line<'static>>, bool) {
        let mut own: BTreeMap<&str, [f64; 2]> = BTreeMap::new();
        let mut stack = vec![zoom];
        while let Some(node) = stack.pop() {
            let one = &tree.nodes[node];
            if node != 0 && one.own != [0.0; 2] {
                let entry = own.entry(one.name.as_str()).or_default();
                entry[0] += one.own[0];
                entry[1] += one.own[1];
            }
            stack.extend(one.children.iter().copied());
        }
        let total = tree.weight(zoom).max(f64::MIN_POSITIVE);
        let width = usize::from(area.width);
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let style = pane.trust.style();
        let mut lines = Vec::new();
        let room = usize::from(area.height).saturating_sub(1);
        let entries = room.min(12);
        match &tree.compare {
            None => {
                lines.push(Line::from(vec![
                    Span::styled("heaviest on their own", bold),
                    Span::styled(
                        fit(
                            " (where it ends, every path summed)",
                            width.saturating_sub(21),
                        ),
                        Style::new().add_modifier(Modifier::DIM),
                    ),
                ]));
                let mut ranked: Vec<(&str, f64)> =
                    own.iter().map(|(name, value)| (*name, value[0])).collect();
                ranked.sort_by(|x, y| y.1.total_cmp(&x.1).then_with(|| x.0.cmp(y.0)));
                let max = ranked.first().map_or(1.0, |first| first.1);
                // A lossy run's figures are lower bounds: the bar says how
                // much more each could be.
                let lost = pane.trust.marks.iter().find_map(|mark| match mark {
                    trust::Mark::Lost {
                        fraction: Some(fraction),
                        ..
                    } => Some(*fraction),
                    _ => None,
                });
                for (name, value) in ranked.into_iter().take(entries) {
                    let high = lost.map_or(value, |fraction| value / (1.0 - fraction).max(0.01));
                    let mut spans = vec![Span::styled(
                        format!("{:>6} ", units::percent(100.0 * value / total)),
                        style,
                    )];
                    let bar = trust::error_bar(value, value, high, max, 12);
                    spans.extend(
                        bar.spans
                            .into_iter()
                            .map(|span| span.patch_style(Style::new().fg(palette_for(pane)))),
                    );
                    spans.push(Span::styled(
                        format!(" {}", fit(&display_name(name), width.saturating_sub(20))),
                        style,
                    ));
                    lines.push(Line::from(spans));
                }
            }
            Some(compare) => {
                lines.push(Line::from(vec![
                    Span::styled("moved on their own", bold),
                    Span::styled(
                        " -- this run → the other",
                        Style::new().add_modifier(Modifier::DIM),
                    ),
                ]));
                let mut ranked: Vec<(&str, f64, f64, f64, bool)> = own
                    .iter()
                    .map(|(name, value)| {
                        let (a, b) = compare.pair(*value);
                        let (sigmas, significant) = compare.judge(*value);
                        (*name, a, b, sigmas, significant)
                    })
                    .collect();
                ranked.sort_by(|x, y| {
                    (y.2 - y.1)
                        .abs()
                        .total_cmp(&(x.2 - x.1).abs())
                        .then_with(|| x.0.cmp(y.0))
                });
                for (name, a, b, sigmas, significant) in ranked.into_iter().take(entries) {
                    let colour = match (significant, b > a) {
                        (false, _) => Color::Reset,
                        (true, true) => palette::WORSE,
                        (true, false) => palette::BETTER,
                    };
                    let ratio = if a > 0.0 {
                        format!("x{:.2}", b / a)
                    } else {
                        "new".to_owned()
                    };
                    let noise = if sigmas.is_finite() {
                        format!("{sigmas:>5.1}σ")
                    } else {
                        "      ".to_owned()
                    };
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{:>9} {ratio:>6}", compare.amount(b - a)),
                            style.fg(colour),
                        ),
                        Span::styled(format!(" {noise} "), style),
                        Span::styled(
                            fit(&display_name(name), width.saturating_sub(24)),
                            if significant {
                                style.add_modifier(Modifier::UNDERLINED)
                            } else {
                                style
                            },
                        ),
                    ]));
                }
            }
        }
        let said = !pane.caveats.is_empty() && lines.len() + 2 <= usize::from(area.height);
        if said {
            lines.push(Line::default());
            for caveat in &pane.caveats {
                lines.push(Line::from(vec![
                    Span::styled("‼ ", Style::new().fg(palette::LOSS)),
                    Span::styled(caveat.clone(), Style::new().add_modifier(Modifier::DIM)),
                ]));
            }
        }
        (lines, said)
    }
}

/// The colour of a pane's bars: its icicle's.
fn palette_for(pane: &Pane) -> Color {
    if pane.source == "sched.offcpu" {
        Color::Cyan
    } else {
        Color::Yellow
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(Flame::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, Cost};
    use crate::model::Run;
    use crate::snapshot::{assert_snapshot, draw_app, fixture, text, HEIGHT, WIDTH};
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::KeyModifiers;

    fn app(name: &str) -> App {
        let mut app = App::new(fixture(name));
        app.cost = Cost::Snapshot;
        app.open("flame").unwrap();
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        app.key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn draw(app: &mut App) -> Buffer {
        draw_app(app, WIDTH, HEIGHT)
    }

    fn tree(data: &Data, against: Option<&Data>, pane: usize, shape: bool) -> Tree {
        let focus = Focus::default();
        let built = build(
            data,
            against,
            &NEEDS[pane],
            "",
            shape,
            &Reading {
                by_thread: false,
                focus: &focus,
            },
        );
        match built.body {
            Body::Tree(tree) => tree,
            _ => panic!("no tree for {}", NEEDS[pane].source),
        }
    }

    fn stack_total(data: &Data, source: &str) -> f64 {
        data.of_source(source)
            .flat_map(|results| stack_outputs(results).collect::<Vec<_>>())
            .map(|(stacks, unattributed)| {
                stacks.iter().map(|stack| stack.total).sum::<f64>() + unattributed as f64
            })
            .sum()
    }

    /// A profile of named stacks at 1 kHz, as `omn` writes one.
    fn profile(stacks: &[(&[&str], u64)]) -> Data {
        let mut results = Results {
            label: "cpu.profile".into(),
            source: "cpu.profile".into(),
            unit: "count".into(),
            period_ns: Some(1_000_000),
            ..Results::default()
        };
        let mut counts = Vec::new();
        for (id, (frames, count)) in stacks.iter().enumerate() {
            results
                .stacks
                .insert(id as u64, frames.iter().map(|f| f.to_string()).collect());
            counts.push(StackCount {
                stack: id as u64,
                events: *count,
                total: *count as f64,
            });
        }
        results.outputs = vec![Output::Stack {
            stacks: counts,
            unattributed: 0,
            short: None,
        }];
        Data {
            run: Run {
                results: vec![results],
                ..Run::default()
            },
            ..Data::default()
        }
    }

    #[test]
    fn every_sample_is_in_the_tree_once() {
        for name in [
            "cpython-hot",
            "http-server",
            "sqlite-commits",
            "sqlite-latency",
        ] {
            let data = fixture(name);
            let on = tree(&data, None, ON, false);
            assert_eq!(
                on.nodes[0].value[0],
                stack_total(&data, "cpu.profile"),
                "{name}"
            );
            let own: f64 = on.nodes.iter().map(|node| node.own[0]).sum();
            assert_eq!(
                own, on.nodes[0].value[0],
                "{name}: every sample ends somewhere"
            );
            for (index, node) in on.nodes.iter().enumerate() {
                let children: f64 = node.children.iter().map(|&c| on.nodes[c].value[0]).sum();
                assert!(
                    children <= node.value[0] + 1e-6,
                    "{name}: {} is narrower than its callees",
                    on.nodes[index].name
                );
            }
        }
    }

    #[test]
    fn off_cpu_is_weighed_in_time_and_the_commits_wait_in_fdatasync() {
        let data = fixture("sqlite-commits");
        let off = tree(&data, None, OFF, false);
        assert_eq!(off.unit, Unit::Nanos);
        assert_eq!(off.nodes[0].value[0], stack_total(&data, "sched.offcpu"));
        let own = off.own_by_function();
        let (hottest, value) = own
            .iter()
            .max_by(|x, y| x.1[0].total_cmp(&y.1[0]))
            .expect("a block");
        assert_eq!(*hottest, "fdatasync", "the commits block in fdatasync");
        assert!(value[0] > 0.9 * off.nodes[0].value[0]);
    }

    #[test]
    fn a_run_against_itself_differs_in_nothing() {
        let data = fixture("cpython-hot");
        let same = fixture("cpython-hot");
        let on = tree(&data, Some(&same), ON, false);
        let compare = on.compare.as_ref().expect("a comparison");
        assert!(compare.difference.is_empty(), "{:?}", compare.difference);
        assert!(compare
            .sentence(false)
            .contains("no function's own cost moved"));
    }

    /// The triage's own test, held here to the view's re-statement of its
    /// arithmetic: one function doubled is the only difference, and noise is
    /// none.
    #[test]
    fn one_function_doubled_is_the_only_difference_and_noise_is_none() {
        let before = profile(&[(&["main", "f"], 1000), (&["main", "g"], 3000)]);
        let after = profile(&[(&["main", "f"], 2000), (&["main", "g"], 3030)]);
        let on = tree(&before, Some(&after), ON, false);
        let compare = on.compare.as_ref().unwrap();
        let named: Vec<&str> = compare
            .difference
            .iter()
            .map(|change| change.function.as_str())
            .collect();
        assert_eq!(named, ["f"], "g moved by 30 of 6030: under 1σ");
        assert!(compare.sentence(false).contains("`f` +1000 own (x2.00"));
        // The caller carried it, and is coloured for it; g is grey.
        let main = on.find(&["main".to_owned()]);
        let g = on.find(&["main".to_owned(), "g".to_owned()]);
        assert_eq!(
            difference_colour(compare, on.nodes[main].value),
            Color::LightRed
        );
        assert_eq!(
            difference_colour(compare, on.nodes[g].value),
            Color::DarkGray
        );
    }

    #[test]
    fn twice_the_work_moves_everything_absolutely_and_little_in_shape() {
        let first = fixture("cpython-hot");
        let second = fixture("cpython-hot-heavier");
        let absolute = tree(&first, Some(&second), ON, false);
        let shape = tree(&first, Some(&second), ON, true);
        let moved = |tree: &Tree| tree.compare.as_ref().unwrap().difference.len();
        let up = absolute
            .compare
            .as_ref()
            .unwrap()
            .difference
            .iter()
            .filter(|change| change.delta > 0.0)
            .count();
        assert_eq!(
            up,
            moved(&absolute),
            "the heavier run spends more everywhere"
        );
        assert!(
            moved(&absolute) >= 4 * moved(&shape).max(1),
            "absolutely {} functions moved, in shape {}: the heavier run is the same program \
             doing more, which only the shape comparison says",
            moved(&absolute),
            moved(&shape)
        );
        let totals = shape.compare.as_ref().unwrap().totals;
        assert!((totals[0] - totals[1]).abs() < 1e-6, "scaled to one total");
    }

    #[test]
    fn a_comparison_with_a_source_the_other_run_lacks_is_drawn_alone_and_says_so() {
        let data = fixture("http-server");
        let other = fixture("cpython-hot");
        let focus = Focus::default();
        let built = build(
            &data,
            Some(&other),
            &NEEDS[OFF],
            "",
            false,
            &Reading {
                by_thread: false,
                focus: &focus,
            },
        );
        // cpython-hot recorded no block: its off-cpu tree is empty, not
        // missing, and the comparison holds zero on its side.
        let Body::Tree(tree) = built.body else {
            panic!("a tree");
        };
        assert_eq!(tree.nodes[0].value[1], 0.0);
        let machine = fixture("machine");
        let built = build(
            &data,
            Some(&machine),
            &NEEDS[OFF],
            "",
            false,
            &Reading {
                by_thread: false,
                focus: &focus,
            },
        );
        assert!(built.aside.unwrap().contains("the other run has none"));
    }

    #[test]
    fn off_cpu_splits_by_thread_and_narrows_to_a_focused_one() {
        let data = fixture("http-server");
        let focus = Focus::default();
        let read = |by_thread, focus: &Focus| match build(
            &data,
            None,
            &NEEDS[OFF],
            "",
            false,
            &Reading { by_thread, focus },
        )
        .body
        {
            Body::Tree(tree) => tree,
            _ => panic!("a tree"),
        };
        let split = read(true, &focus);
        let threads: Vec<&str> = split.nodes[0]
            .children
            .iter()
            .map(|&c| split.nodes[c].name.as_str())
            .collect();
        assert_eq!(threads.len(), 3, "{threads:?}");
        assert!(threads.iter().all(|name| name.starts_with('[')));
        assert_eq!(split.nodes[0].value[0], stack_total(&data, "sched.offcpu"));
        let tid = 2_758_974;
        let narrowed = read(
            false,
            &Focus {
                tid: Some(tid),
                ..Focus::default()
            },
        );
        let expected: f64 = data
            .first("sched.offcpu")
            .unwrap()
            .events()
            .iter()
            .filter(|event| event.tid() == Some(tid))
            .filter_map(|event| event.number("total_ns"))
            .sum();
        assert!(expected > 0.0);
        assert_eq!(narrowed.nodes[0].value[0], expected);
        // On cpu, the stacks carry no thread: not narrowed, and said.
        let on = build(
            &data,
            None,
            &NEEDS[ON],
            "",
            false,
            &Reading {
                by_thread: false,
                focus: &Focus {
                    tid: Some(tid),
                    ..Focus::default()
                },
            },
        );
        assert!(on.aside.unwrap().contains("not narrowed"));
    }

    #[test]
    fn a_live_run_says_what_arrived_and_draws_the_icicle_at_the_end() {
        let data = fixture("sqlite-commits");
        let half = data.opened_ns() + (data.closed_ns() - data.opened_ns()) / 2;
        let live = crate::feed::as_live_until(&data, half);
        let focus = Focus::default();
        let built = build(
            &live,
            None,
            &NEEDS[OFF],
            "",
            false,
            &Reading {
                by_thread: false,
                focus: &focus,
            },
        );
        let Body::Live { events, stacks, .. } = built.body else {
            panic!("a live pane");
        };
        let before: usize = data
            .first("sched.offcpu")
            .unwrap()
            .events()
            .iter()
            .filter(|event| event.ts_ns <= half)
            .count();
        assert_eq!(events as usize, before);
        assert!(stacks > 0);
        assert!(built.trust.marks.contains(&trust::Mark::Provisional));
    }

    #[test]
    fn a_chain_that_never_branches_is_one_box() {
        let data = profile(&[
            (&["a", "b", "c", "d", "e"], 900),
            (&["a", "b", "c", "x"], 100),
        ]);
        let tree = tree(&data, None, ON, false);
        let icicle = lay_out(&tree, 0, 40);
        // all › a › b › c in one box, then d (folding e) and x.
        assert_eq!(icicle.rows.len(), 2);
        let top = icicle.rows[0][0];
        assert_eq!(tree.nodes[top.node].name, "c");
        assert_eq!(label(&tree, &top, None, 40), "all › ⋯2⋯ › c");
        let names: Vec<&str> = icicle.rows[1]
            .iter()
            .map(|cell| tree.nodes[cell.node].name.as_str())
            .collect();
        assert_eq!(names, ["e", "x"]);
        assert_eq!((icicle.rows[1][0].x0, icicle.rows[1][0].x1), (0, 36));
    }

    #[test]
    fn keys_move_zoom_and_search() {
        let mut app = app("http-server");
        draw(&mut app);
        // Down from the root, into the heaviest callee, and zoom.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        let zoomed = text(&draw(&mut app));
        assert!(zoomed.contains("zoom: "), "{zoomed}");
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Backspace);
        let out = text(&draw(&mut app));
        assert!(out.contains("width ∝ samples"), "{out}");
        // A search says the share of each pane it covers.
        press(&mut app, KeyCode::Char('/'));
        typed(&mut app, "fstat");
        press(&mut app, KeyCode::Enter);
        let searched = text(&draw(&mut app));
        assert!(searched.contains("/fstat/ on cpu:"), "{searched}");
        press(&mut app, KeyCode::Esc);
        assert!(!text(&draw(&mut app)).contains("/fstat/"));
    }

    // The gallery's other screens: what a key or a second run shows, for
    // the person judging the view.

    #[test]
    fn snapshot_a_comparison_of_two_runs() {
        let mut app = app("cpython-hot");
        app.against = Some(fixture("cpython-hot-heavier"));
        assert_snapshot(
            "flame-against--cpython-hot--cpython-hot-heavier",
            &draw(&mut app),
        );
        press(&mut app, KeyCode::Char('d'));
        assert_snapshot(
            "flame-against-shape--cpython-hot--cpython-hot-heavier",
            &draw(&mut app),
        );
    }

    #[test]
    fn snapshot_zoomed_into_a_frame() {
        let mut app = app("http-server");
        draw(&mut app);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_snapshot("flame-zoomed--http-server", &draw(&mut app));
    }

    #[test]
    fn snapshot_a_search() {
        let mut app = app("http-server");
        draw(&mut app);
        press(&mut app, KeyCode::Char('/'));
        typed(&mut app, "lookup");
        press(&mut app, KeyCode::Enter);
        assert_snapshot("flame-search--http-server", &draw(&mut app));
    }

    #[test]
    fn snapshot_off_cpu_by_thread_alone() {
        let mut app = app("http-server");
        draw(&mut app);
        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Char('t'));
        press(&mut app, KeyCode::Char('m'));
        assert_snapshot("flame-threads--http-server", &draw(&mut app));
    }

    #[test]
    fn snapshot_a_live_run_half_way() {
        let data = fixture("sqlite-commits");
        let half = data.opened_ns() + (data.closed_ns() - data.opened_ns()) / 2;
        let mut app = App::new(crate::feed::as_live_until(&data, half));
        app.cost = Cost::Snapshot;
        app.open("flame").unwrap();
        assert_snapshot("flame-live--sqlite-commits", &draw(&mut app));
    }

    #[test]
    fn snapshot_a_narrow_terminal() {
        let mut app = app("http-server");
        assert_snapshot("flame-narrow--http-server", &draw_app(&mut app, 80, 24));
    }
}
