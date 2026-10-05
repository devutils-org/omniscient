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
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Sparkline, Wrap};
use ratatui::Frame;

use super::{Action, Ctx, Focus, Need, Region, Scope, View};
use crate::model::{Data, Output, Refusal, Results, StackCount, RECOVERED};
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
    ("t", "split"),
    ("d", "shape"),
    ("!", "caveats"),
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
    /// How many of each pane's caveats it said, whole, in its own room on
    /// the last draw; the rest go to the last line.
    caveats_said: [usize; 2],
    /// Every caveat of both panes and both runs, whole, over the panes.
    listing: bool,
    cache: Option<(Fingerprint, [Pane; 2])>,
    /// The boxes as last drawn, each `node * 2 + pane`.
    regions: Vec<Region>,
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
    /// The second run's total over the first's, comparable but before any
    /// scaling of shapes: how much more the second run did.
    growth: f64,
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
            "by over 5 % and 0.25 % of the total (time off cpu has no noise model: the view's \
             rule, not the triage's)"
                .to_owned()
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
    /// Whether a stack's own weight went to the innermost frame a perf map
    /// named rather than to its leaf, as the triage charges it.
    charged_by_map: bool,
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
            charged_by_map: false,
        }
    }

    /// Adds one stack's weight, root first, to run `run` (0 or 1). Its own
    /// weight goes to the innermost frame among `charged` -- the frames a
    /// perf map named: an interpreted function, whose interpreter's native
    /// frames under it are its own cost -- else to its leaf, as `omn
    /// triage`'s difference charges it.
    fn add<'a>(
        &mut self,
        frames: impl IntoIterator<Item = &'a str>,
        weight: f64,
        events: u64,
        run: usize,
        charged: &BTreeSet<String>,
    ) {
        let mut at = 0;
        let mut charge = None;
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
            if !charged.is_empty() && charged.contains(frame.trim_end_matches(RECOVERED)) {
                charge = Some(next);
            }
            at = next;
        }
        if charge.is_some() {
            self.charged_by_map = true;
        }
        self.nodes[charge.unwrap_or(at)].own[run] += weight;
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
    let mut untimed = 0u64;
    let mut unplaced = 0u64;
    for one in results {
        let events = one.events();
        let charged = one.perf_map_named();
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
                // Off cpu a block weighs its time, and one without it is
                // counted and said, never summed as a count in time.
                let weight = if tree.unit == Unit::Nanos {
                    match event.number("total_ns") {
                        Some(ns) => ns,
                        None => {
                            untimed += 1;
                            continue;
                        }
                    }
                } else {
                    event.number("weight").unwrap_or(1.0)
                };
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
                tree.add(path.iter().map(String::as_str), weight, 1, run, &charged);
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
            // Rows keyed by task (`--keys task`) split by thread on their
            // own; without the split asked for, a stack's rows of several
            // series fall into the same boxes.
            let keyed = stack_outputs(one)
                .any(|(stacks, _)| stacks.iter().any(|stack| stack.series.is_some()));
            if reading.by_thread && !keyed {
                asides.push(
                    "not split by thread: the run kept the stacks, not the blocks".to_owned(),
                );
            }
            if narrowed {
                asides.push("not narrowed to the focus: these stacks carry no thread".to_owned());
            }
            for (stacks, unattributed) in stack_outputs(one) {
                for stack in stacks {
                    let series = stack
                        .series
                        .as_ref()
                        .filter(|_| reading.by_thread)
                        .map(|series| format!("[{series}]"));
                    let frames = named(one, stack.stack);
                    tree.add(
                        series.iter().chain(frames.iter()).map(String::as_str),
                        stack.total,
                        stack.events,
                        run,
                        &charged,
                    );
                }
                if unattributed == 0 {
                    continue;
                }
                if tree.unit != Unit::Nanos {
                    tree.add(
                        ["[no stack]"],
                        unattributed as f64,
                        unattributed,
                        run,
                        &BTreeSet::new(),
                    );
                    continue;
                }
                // Off cpu the emit counts the blocks that carried no stack,
                // not their time: the time is in the blocks, when the run
                // kept them, and what cannot be weighed is said.
                let (mut time, mut found) = (0.0, 0u64);
                for event in events
                    .iter()
                    .filter(|event| event.number("stack_id").is_none())
                {
                    match event.number("total_ns") {
                        Some(ns) => {
                            time += ns;
                            found += 1;
                        }
                        None => untimed += 1,
                    }
                }
                if found > 0 {
                    tree.add(["[no stack]"], time, found, run, &BTreeSet::new());
                }
                unplaced += unattributed.saturating_sub(found + untimed);
            }
        }
    }
    if untimed > 0 {
        asides.push(format!(
            "{} blocks carried no time: in no box",
            units::count(untimed as f64)
        ));
    }
    if unplaced > 0 {
        asides.push(format!(
            "{} blocks carried no stack, and the run kept no blocks to weigh them: in no box",
            units::count(unplaced as f64)
        ));
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
    pane.caveats = caveats_of(&held);
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
        for aside in fill(&mut tree, other, 1, reading) {
            if !asides.contains(&aside) {
                asides.push(format!("the other run: {aside}"));
            }
        }
        pane.trust
            .merge(Trust::joined(against, other.iter().copied()));
        // The other run's caveats are said too, after this one's: the badge
        // counts both, so both are said.
        for caveat in caveats_of(other) {
            if !pane.caveats.contains(&caveat) {
                pane.caveats.push(format!("the other run: {caveat}"));
            }
        }
        tree.compare = Some(compare(&tree, &held, other, shape));
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

/// The hottest function of a pane, as another view reads it: the frame with
/// the most weight of its own, the one the flame's header names.
#[derive(Debug, Clone, PartialEq)]
pub struct Hottest {
    /// The function as the flame draws it.
    pub name: String,
    /// Its own share of the pane's whole weight, `0..=1`.
    pub share: f64,
    /// Its own weight: samples on cpu, nanoseconds off cpu.
    pub own: f64,
    /// The pane's whole weight, in the same unit.
    pub total: f64,
    /// `true` off cpu (nanoseconds), `false` on cpu (samples).
    pub off_cpu: bool,
    pub trust: Trust,
}

/// The hottest function on cpu (`off_cpu` false) or off cpu, for `focus`
/// (empty: the whole target); `None` when the run holds no stack of it, or
/// while it is live (a live flame counts events, not weight).
pub fn hottest(data: &Data, focus: &Focus, off_cpu: bool) -> Option<Hottest> {
    let need = &NEEDS[if off_cpu { OFF } else { ON }];
    let pane = build(
        data,
        None,
        need,
        "",
        false,
        &Reading {
            by_thread: false,
            focus,
        },
    );
    let Body::Tree(tree) = &pane.body else {
        return None;
    };
    let total = tree.weight(0);
    let (name, own) = tree
        .own_by_function()
        .into_iter()
        .max_by(|x, y| x.1[0].total_cmp(&y.1[0]).then_with(|| y.0.cmp(x.0)))?;
    Some(Hottest {
        name: display_name(name),
        share: if total > 0.0 { own[0] / total } else { 0.0 },
        own: own[0],
        total,
        off_cpu,
        trust: pane.trust.clone(),
    })
}

/// A source's own caveats: its degradations, then its notes.
fn caveats_of(results: &[&Results]) -> Vec<String> {
    let mut caveats: Vec<String> = Vec::new();
    for one in results {
        caveats.extend(one.degradations.iter().cloned());
    }
    for one in results {
        caveats.extend(
            one.notes
                .iter()
                .filter(|note| !note.starts_with("stacks unwound by"))
                .cloned(),
        );
    }
    caveats
}

/// The comparison of a tree's two runs: the triage's arithmetic. Each side
/// is every result that served the need, and each side's losses are all of
/// theirs.
fn compare(tree: &Tree, first: &[&Results], second: &[&Results], shape: bool) -> Compare {
    let mut notes = Vec::new();
    let poisson = tree.unit != Unit::Nanos;
    let (unit, mut scale) = match (tree.unit, Unit::of(second[0])) {
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
    let growth = raw[1] * scale[1] / (raw[0] * scale[0]).max(f64::MIN_POSITIVE);
    if shape && raw[1] > 0.0 {
        scale[1] *= raw[0] * scale[0] / (raw[1] * scale[1]);
    }
    for (which, results) in [("this run", first), ("the other", second)] {
        let missing: u64 = results.iter().map(|one| one.losses.missing()).sum();
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
        growth,
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

/// The on-cpu comparison of `data` against `against` as the view draws it:
/// the unit it is in and the functions it names as moved beyond the noise,
/// largest first. `None` when either run has no on-cpu profile to compare.
///
/// The view re-states `src/triage/diff.rs`'s arithmetic, since it links
/// nothing of `omn`; this is what a test in the root crate, which links
/// both, holds to the triage's answer on the same two recordings.
pub fn difference(data: &Data, against: &Data) -> Option<(String, Vec<String>)> {
    let focus = Focus::default();
    let pane = build(
        data,
        Some(against),
        &NEEDS[ON],
        "on cpu",
        false,
        &Reading {
            by_thread: false,
            focus: &focus,
        },
    );
    let Body::Tree(tree) = pane.body else {
        return None;
    };
    let compare = tree.compare.as_ref()?;
    Some((
        compare.unit.to_owned(),
        compare
            .difference
            .iter()
            .map(|change| change.function.clone())
            .collect(),
    ))
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

/// A text cut into lines of at most `width` cells, at spaces where it can
/// be and inside a word where it cannot. Nothing is dropped: the lines hold
/// every word, so a caveat drawn from them is said whole or not at all.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut count = 0;
    for word in text.split(' ').filter(|word| !word.is_empty()) {
        let mut word: Vec<char> = word.chars().collect();
        if count > 0 && count + 1 + word.len() <= width {
            line.push(' ');
            line.extend(word.iter());
            count += 1 + word.len();
            continue;
        }
        if count > 0 {
            lines.push(std::mem::take(&mut line));
        }
        while word.len() > width {
            let rest = word.split_off(width);
            lines.push(word.into_iter().collect());
            word = rest;
        }
        count = word.len();
        line = word.into_iter().collect();
    }
    if count > 0 || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// One caveat as the lines it takes at `width`: `‼` and then the text,
/// wrapped under itself.
fn caveat_lines(caveat: &str, width: usize) -> Vec<Line<'static>> {
    wrap_words(caveat, width.saturating_sub(2))
        .into_iter()
        .enumerate()
        .map(|(at, text)| {
            Line::from(vec![
                Span::styled(
                    if at == 0 { "‼ " } else { "  " },
                    Style::new().fg(palette::LOSS),
                ),
                Span::styled(text, Style::new().add_modifier(Modifier::DIM)),
            ])
        })
        .collect()
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
        // The list of caveats closes on `!` or esc, and on any other key,
        // which then does what it does.
        if std::mem::take(&mut self.listing)
            && matches!(key.code, KeyCode::Esc | KeyCode::Char('!'))
        {
            return Action::None;
        }
        let panes = self.panes(ctx);
        let active = self.active_pane(&panes);
        self.active = active;
        match key.code {
            KeyCode::Char('!') => self.listing = true,
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

    fn takes_text(&self) -> bool {
        self.typing.is_some()
    }

    fn regions(&self) -> &[Region] {
        &self.regions
    }

    /// A box clicked is selected in its pane, which becomes the one the keys
    /// move in; clicked again, it is zoomed into, as `Enter` does.
    fn select(&mut self, id: usize, ctx: &Ctx<'_>) -> Action {
        let (node, pane) = (id / 2, id % 2);
        let panes = self.panes(ctx);
        if let Body::Tree(tree) = &panes[pane].body {
            if node < tree.nodes.len() {
                self.active = pane;
                self.panes[pane].selected = tree.path(node);
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
        self.caveats_said = [0; 2];
        self.regions.clear();
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
        frame.render_widget(
            Paragraph::new(self.bottom(&panes[active], active, bottom.width)),
            bottom,
        );
        if self.listing {
            let over = Rect {
                height: middle.height + detail.height,
                ..middle
            };
            self.draw_listing(frame, over, &panes);
        }
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
                // The runs' own totals and how much more the second did;
                // in shape, that the second was scaled to the first.
                spans.push(Span::styled(format!("{}: ", panes[active].title), bold));
                spans.push(Span::raw(format!(
                    "{} → {} (x{:.2}{}) · ",
                    tree.unit.short(tree.nodes[0].value[0]),
                    tree.unit.short(tree.nodes[0].value[1]),
                    compare.growth,
                    if self.shape { ", scaled to one" } else { "" },
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

    /// The last line: the search being typed, or what the pane could not
    /// say in its own room -- the comparison's notes and the caveats left
    /// over -- the first of them as far as the line goes, and how many more
    /// `!` lists whole.
    fn bottom(&self, pane: &Pane, index: usize, width: u16) -> Line<'static> {
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
        if self.listing {
            return Line::from(Span::styled(
                "every caveat of both panes, whole · ! or esc closes",
                Style::new().add_modifier(Modifier::DIM),
            ));
        }
        let mut left: Vec<String> = Vec::new();
        if let Body::Tree(tree) = &pane.body {
            if let Some(compare) = &tree.compare {
                left.extend(compare.notes.iter().cloned());
            }
        }
        let said = self.caveats_said[index].min(pane.caveats.len());
        left.extend(pane.caveats[said..].iter().cloned());
        match left.split_first() {
            None => Line::from(Span::styled(
                "⏎ zooms into the selected box, ⌫ out · / finds a name, * the selected one · o \
                 switches pane, m shows it alone · t splits off cpu by thread · ! lists caveats",
                Style::new().add_modifier(Modifier::DIM),
            )),
            Some((first, rest)) => {
                let head = if rest.is_empty() {
                    format!("{}: ", pane.source)
                } else {
                    format!("{} (+{} more, ! lists them): ", pane.source, rest.len())
                };
                let room = usize::from(width).saturating_sub(head.chars().count());
                Line::from(vec![
                    Span::styled(head, Style::new().fg(palette::LOSS)),
                    Span::styled(fit(first, room), Style::new().add_modifier(Modifier::DIM)),
                ])
            }
        }
    }

    /// `!`: every caveat of both panes -- both runs' in a comparison -- and
    /// the comparison's notes, whole, over the panes.
    fn draw_listing(&self, frame: &mut Frame<'_>, area: Rect, panes: &[Pane; 2]) {
        let block = Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(palette::LOSS))
            .title(Span::styled(
                " caveats, whole ",
                Style::new().add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        let width = usize::from(inner.width);
        let mut lines: Vec<Line<'static>> = Vec::new();
        for pane in panes {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} ", pane.title),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
                Span::styled(pane.source, Style::new().add_modifier(Modifier::DIM)),
            ]));
            let mut said: Vec<String> = Vec::new();
            if let Body::Tree(tree) = &pane.body {
                if let Some(compare) = &tree.compare {
                    said.extend(compare.notes.iter().cloned());
                }
            }
            said.extend(pane.caveats.iter().cloned());
            if let Some(aside) = &pane.aside {
                said.push(aside.clone());
            }
            if said.is_empty() {
                lines.push(Line::from(Span::styled(
                    "no caveat: the source said none",
                    Style::new().add_modifier(Modifier::DIM),
                )));
            }
            for caveat in &said {
                lines.extend(caveat_lines(caveat, width));
            }
        }
        let height = usize::from(inner.height);
        if lines.len() > height && height > 0 {
            let more = lines.len() - (height - 1);
            lines.truncate(height - 1);
            lines.push(Line::from(Span::styled(
                format!("↓ {more} more lines: a taller terminal shows them"),
                Style::new().add_modifier(Modifier::REVERSED),
            )));
        }
        frame.render_widget(Paragraph::new(lines), inner);
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
                // What the source says of itself comes under the count, whole,
                // as long as the chart keeps a few rows.
                let mut caveats: Vec<Line<'static>> = Vec::new();
                let room = usize::from(inner.height).saturating_sub(3 + 3);
                for caveat in &pane.caveats {
                    let lines = caveat_lines(caveat, usize::from(inner.width));
                    if caveats.len() + lines.len() > room {
                        break;
                    }
                    caveats.extend(lines);
                    self.caveats_said[index] += 1;
                }
                let [words, notes, chart] = Layout::vertical([
                    Constraint::Length(3),
                    Constraint::Length(caveats.len() as u16),
                    Constraint::Min(0),
                ])
                .areas(inner);
                frame.render_widget(Paragraph::new(caveats), notes);
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
                if width > 0 {
                    self.regions.push(Region::new(
                        Rect::new(body.x + cell.x0, y, width as u16, 1),
                        cell.node * 2 + index,
                    ));
                }
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
            let (lines, said) = self.ranking(pane, tree, zoom, rest, active);
            self.caveats_said[index] = said;
            // Every line is cut to the width already: nothing wraps, so
            // nothing is clipped at the bottom unsaid.
            frame.render_widget(Paragraph::new(lines), rest);
        }
    }

    /// The functions heaviest on their own under the zoom -- where the
    /// samples end, which an icicle spreads over every path that reaches
    /// them -- with a bar each; in a comparison, the largest movements. Then
    /// the source's caveats, each whole or not at all, and how many did not
    /// fit; returns how many were said.
    fn ranking(
        &self,
        pane: &Pane,
        tree: &Tree,
        zoom: usize,
        area: Rect,
        active: bool,
    ) -> (Vec<Line<'static>>, usize) {
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
        // The ranking keeps its title and its first three; the caveats come
        // next, whole, as many as fit, and say how many did not; the ranking
        // takes what is left, up to twelve.
        let height = usize::from(area.height);
        let floor = 1 + own.len().min(3);
        let blocks: Vec<Vec<Line<'static>>> = pane
            .caveats
            .iter()
            .map(|caveat| caveat_lines(caveat, width))
            .collect();
        let room = height.saturating_sub(floor + 1);
        let mut said = 0;
        let mut said_lines: Vec<Line<'static>> = Vec::new();
        if !blocks.is_empty() {
            let all: usize = blocks.iter().map(Vec::len).sum();
            let budget = if all <= room {
                room
            } else {
                room.saturating_sub(1)
            };
            for block in &blocks {
                if said_lines.len() + block.len() > budget {
                    break;
                }
                said_lines.extend(block.iter().cloned());
                said += 1;
            }
            if said < blocks.len() && room > 0 {
                let more = blocks.len() - said;
                let more = if active {
                    format!("{more} more on the last line; ! lists every caveat whole")
                } else {
                    format!("{more} more; ! lists every caveat whole")
                };
                said_lines.push(Line::from(vec![
                    Span::styled("‼ ", Style::new().fg(palette::LOSS)),
                    Span::styled(
                        fit(&more, width.saturating_sub(2)),
                        Style::new().fg(palette::LOSS),
                    ),
                ]));
            }
        }
        let caveat_rows = if said_lines.is_empty() {
            0
        } else {
            said_lines.len() + 1
        };
        // As many as the pane has rows for, and their bars as wide as a
        // fifth of it: a tall or wide terminal lists more and draws them
        // larger, rather than leaving the pane's foot empty.
        let entries = height.saturating_sub(1 + caveat_rows);
        let bar_cells = (width / 5).clamp(12, 40);
        match &tree.compare {
            None => {
                lines.push(Line::from(vec![
                    Span::styled("heaviest on their own", bold),
                    Span::styled(
                        fit(
                            if tree.charged_by_map {
                                " (the innermost frame a perf map named, else where it ends; \
                                 every path summed)"
                            } else {
                                " (where it ends, every path summed)"
                            },
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
                    let bar = trust::error_bar(value, value, high, max, bar_cells as u16);
                    spans.extend(
                        bar.spans
                            .into_iter()
                            .map(|span| span.patch_style(Style::new().fg(palette_for(pane)))),
                    );
                    spans.push(Span::styled(
                        format!(
                            " {}",
                            fit(&display_name(name), width.saturating_sub(8 + bar_cells))
                        ),
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
        if !said_lines.is_empty() {
            lines.push(Line::default());
            lines.extend(said_lines);
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
                series: None,
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

    /// The public reading names what the header names.
    #[test]
    fn the_hottest_function_is_read_as_the_header_says_it() {
        let data = fixture("sqlite-commits");
        let off = hottest(&data, &Focus::default(), true).expect("blocks");
        assert_eq!(off.name, "fdatasync");
        assert!(off.share > 0.9 && off.share <= 1.0, "{}", off.share);
        assert!(off.off_cpu);
        let on = hottest(&fixture("cpython-hot"), &Focus::default(), false).expect("samples");
        let text = text(&crate::snapshot::draw(
            fixture("cpython-hot"),
            "flame",
            160,
            40,
        ));
        assert!(text.contains(&on.name), "{} not in\n{text}", on.name);
        assert!(hottest(&fixture("cpython-hot"), &Focus::default(), true).is_none());
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
        // Every function with a name spends more. An address with none
        // (`libpython…+0x…`) is one instruction of the interpreter, whose
        // library carries no symbols: a few dozen samples apiece, and a few
        // of those cross the noise downward -- only a few.
        let difference = &absolute.compare.as_ref().unwrap().difference;
        let unnamed = |change: &&Change| change.function.contains("+0x");
        let down: Vec<&Change> = difference
            .iter()
            .filter(|change| change.delta <= 0.0)
            .collect();
        assert!(
            down.iter().all(unnamed),
            "the heavier run spends more in every named function: {down:?}"
        );
        assert!(down.len() * 5 <= moved(&absolute), "{down:?}");
        // In shape, the program is the same: few functions with a name move.
        // The unnamed addresses, small as they are, still cross the noise
        // either way, so they are held more loosely.
        let named = |tree: &Tree| {
            tree.compare
                .as_ref()
                .unwrap()
                .difference
                .iter()
                .filter(|change| !unnamed(change))
                .count()
        };
        // The recordings say which frames their perf map named, so each
        // sample's own weight is the Python function it ran in: the one the
        // heavier run calls more, `score`, moves absolutely, and nothing
        // named moves in shape.
        assert!(
            named(&absolute) > named(&shape),
            "absolutely {} named functions moved, in shape {}: the heavier run is the same \
             program doing more, which only the shape comparison says",
            named(&absolute),
            named(&shape)
        );
        assert!(
            difference
                .iter()
                .any(|change| change.function.starts_with("py::score:")),
            "{difference:?}"
        );
        assert!(
            moved(&absolute) >= 2 * moved(&shape),
            "absolutely {} functions moved, in shape {}",
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
        // A thread that blocked, read from the recording rather than written
        // here: a re-recording gives every thread a new id.
        let tid = data
            .first("sched.offcpu")
            .unwrap()
            .events()
            .iter()
            .filter(|event| event.number("total_ns").is_some_and(|ns| ns > 0.0))
            .find_map(|event| event.tid())
            .expect("a thread that blocked");
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

    /// A profile of named stacks at `period_ns`, from the triage's folded
    /// lines (`a;b 3`).
    fn folded(lines: &str, period_ns: u64) -> Data {
        let stacks: Vec<(Vec<&str>, u64)> = lines
            .lines()
            .map(|line| {
                let (stack, count) = line.rsplit_once(' ').unwrap();
                (stack.split(';').collect(), count.parse().unwrap())
            })
            .collect();
        let borrowed: Vec<(&[&str], u64)> = stacks
            .iter()
            .map(|(frames, count)| (frames.as_slice(), *count))
            .collect();
        let mut data = profile(&borrowed);
        data.run.results[0].period_ns = Some(period_ns);
        data
    }

    /// `src/triage/diff.rs`'s own cases, its inputs verbatim, held to the
    /// view's re-statement: the same functions named, in the same unit.
    #[test]
    fn the_triage_s_own_cases_give_the_triage_s_answers() {
        /// Before, after, their two periods, the unit compared in, and the
        /// functions named.
        type Case<'a> = (&'a str, &'a str, u64, u64, &'a str, &'a [&'a str]);
        let cases: [Case; 4] = [
            // one_function_doubled_is_the_only_difference
            (
                "main;alpha 1000\nmain;beta 1000\nmain;gamma 1000\n",
                "main;alpha 1010\nmain;beta 2000\nmain;gamma 985\n",
                1_000_000,
                1_000_000,
                "samples",
                &["beta"],
            ),
            // shares_would_have_named_everything
            (
                "a 100\nb 100\nc 100\n",
                "a 100\nb 200\nc 100\n",
                1_000_000,
                1_000_000,
                "samples",
                &["b"],
            ),
            // noise_is_not_a_difference
            (
                "a 400\nb 400\n",
                "a 420\nb 385\n",
                1_000_000,
                1_000_000,
                "samples",
                &[],
            ),
            // two_rates_are_compared_in_time
            (
                "a 100\nb 100\n",
                "a 200\nb 400\n",
                1_000_000,
                500_000,
                "ns",
                &["b"],
            ),
        ];
        for (before, after, first, second, unit, named) in cases {
            let on = tree(
                &folded(before, first),
                Some(&folded(after, second)),
                ON,
                false,
            );
            let compare = on.compare.as_ref().unwrap();
            let got: Vec<&str> = compare
                .difference
                .iter()
                .map(|change| change.function.as_str())
                .collect();
            assert_eq!(
                got,
                named,
                "{before} → {after}: {}",
                compare.sentence(false)
            );
            assert_eq!(compare.unit, unit, "{before} → {after}");
        }
    }

    /// Every caveat under an icicle is said whole or not at all, in the room
    /// there is, and the ones that did not fit are counted on screen.
    #[test]
    fn caveats_are_said_whole_or_counted() {
        let flame = Flame::default();
        for name in ["http-server", "cpython-hot"] {
            let data = fixture(name);
            for (index, need) in NEEDS.iter().enumerate() {
                let focus = Focus::default();
                let pane = build(
                    &data,
                    None,
                    need,
                    "",
                    false,
                    &Reading {
                        by_thread: false,
                        focus: &focus,
                    },
                );
                let Body::Tree(tree) = &pane.body else {
                    continue;
                };
                for height in 2..40u16 {
                    let area = Rect::new(0, 0, 56, height);
                    let (lines, said) = flame.ranking(&pane, tree, 0, area, index == ON);
                    assert!(lines.len() <= usize::from(height), "{name}: {height} rows");
                    let words: Vec<String> = lines
                        .iter()
                        .map(|line| line.to_string())
                        .skip_while(|line| !line.is_empty())
                        .skip(1)
                        .collect();
                    let mut expected: Vec<String> = pane.caveats[..said]
                        .iter()
                        .flat_map(|caveat| caveat_lines(caveat, 56))
                        .map(|line| line.to_string())
                        .collect();
                    if said < pane.caveats.len() && !words.is_empty() {
                        let last = words.last().unwrap();
                        assert!(
                            last.contains(&format!("{} more", pane.caveats.len() - said)),
                            "{name} at {height} rows: {last}"
                        );
                        expected.push(last.clone());
                    }
                    assert_eq!(words, expected, "{name} at {height} rows");
                }
                // At full height, every caveat of this fixture fits.
                let (_, said) =
                    flame.ranking(&pane, tree, 0, Rect::new(0, 0, 56, 200), index == ON);
                assert_eq!(said, pane.caveats.len(), "{name}");
            }
        }
    }

    /// What does not fit under the icicle is on the last line, and `!` lists
    /// every caveat whole.
    #[test]
    fn the_last_line_and_the_listing_say_the_rest() {
        let mut app = app("cpython-hot");
        let screen = text(&draw(&mut app));
        assert!(
            screen.contains("more on the last line") || !screen.contains(" more;"),
            "{screen}"
        );
        press(&mut app, KeyCode::Char('!'));
        let listed = text(&draw(&mut app));
        let data = fixture("cpython-hot");
        let held = data.for_need(&NEEDS[ON].label(), NEEDS[ON].source);
        let caveats = caveats_of(&held);
        assert!(caveats
            .iter()
            .any(|caveat| caveat.starts_with("names came from")));
        let width = usize::from(WIDTH) - 2;
        for caveat in &caveats {
            for line in wrap_words(caveat, width - 2) {
                assert!(listed.contains(&line), "{line}\n{listed}");
            }
        }
        press(&mut app, KeyCode::Char('!'));
        assert!(!text(&draw(&mut app)).contains("caveats, whole"));
    }

    #[test]
    fn a_comparison_says_both_runs_caveats() {
        let first = fixture("cpython-hot");
        let second = fixture("cpython-hot-heavier");
        let focus = Focus::default();
        let pane = build(
            &first,
            Some(&second),
            &NEEDS[ON],
            "",
            false,
            &Reading {
                by_thread: false,
                focus: &focus,
            },
        );
        let other = caveats_of(&second.for_need(&NEEDS[ON].label(), NEEDS[ON].source));
        assert!(!other.is_empty());
        for caveat in &other {
            assert!(
                pane.caveats
                    .iter()
                    .any(|said| said == caveat || *said == format!("the other run: {caveat}")),
                "{caveat}"
            );
        }
        assert!(pane
            .caveats
            .iter()
            .any(|said| said.starts_with("the other run: ")));
    }

    /// Off-cpu blocks that arrived with no stack are weighed by their time
    /// under `[no stack]`, the same through the stacks as through the
    /// blocks; without the blocks, how many is said.
    #[test]
    fn off_cpu_blocks_without_a_stack_are_weighed_or_said() {
        let block = |stack: Option<u64>, ns: u64| -> crate::model::Event {
            let mut attrs = vec![serde_json::json!({"name": "total_ns", "value": ns})];
            if let Some(stack) = stack {
                attrs.push(serde_json::json!({"name": "stack_id", "value": stack}));
            }
            serde_json::from_value(serde_json::json!({
                "ts_ns": 1, "kind": "sched.offcpu",
                "keys": [{"task": {"tid": 7}}], "attrs": attrs,
            }))
            .unwrap()
        };
        let mut results = Results {
            label: "sched.offcpu".into(),
            source: "sched.offcpu".into(),
            unit: "ns".into(),
            ..Results::default()
        };
        results.stacks.insert(1, vec!["main".into(), "read".into()]);
        results.outputs = vec![
            Output::Stack {
                stacks: vec![StackCount {
                    stack: 1,
                    events: 1,
                    total: 1000.0,
                    series: None,
                }],
                unattributed: 2,
                short: None,
            },
            Output::Raw {
                events: vec![block(Some(1), 1000), block(None, 300), block(None, 200)],
            },
        ];
        let data = Data {
            run: Run {
                results: vec![results],
                ..Run::default()
            },
            ..Data::default()
        };
        let read = |data: &Data, by_thread: bool| {
            let focus = Focus::default();
            build(
                data,
                None,
                &NEEDS[OFF],
                "",
                false,
                &Reading {
                    by_thread,
                    focus: &focus,
                },
            )
        };
        let total = |pane: &Pane| match &pane.body {
            Body::Tree(tree) => tree.nodes[0].value[0],
            _ => panic!("a tree"),
        };
        let stacks = read(&data, false);
        assert_eq!(total(&stacks), 1500.0);
        assert_eq!(total(&read(&data, true)), 1500.0, "the same by thread");
        // Without the blocks, the time is unknown and the count is said.
        let mut kept = data.clone();
        kept.run.results[0]
            .outputs
            .retain(|output| matches!(output, Output::Stack { .. }));
        let alone = read(&kept, false);
        assert_eq!(total(&alone), 1000.0);
        assert!(alone.aside.unwrap().contains("2 blocks carried no stack"));
    }

    /// A `--keys task` profile has one row per task and stack: one stack id
    /// on two rows is one tower, and split by thread, one tower per task.
    #[test]
    fn keyed_stack_rows_fold_into_one_tower_or_split_by_task() {
        let mut data = profile(&[(&["main", "work"], 0)]);
        let row = |series: &str, events: u64| StackCount {
            stack: 0,
            events,
            total: events as f64,
            series: Some(series.into()),
        };
        data.run.results[0].outputs = vec![Output::Stack {
            stacks: vec![row("task:7", 30), row("task:8", 15)],
            unattributed: 0,
            short: None,
        }];
        let read = |by_thread: bool| {
            let focus = Focus::default();
            let pane = build(
                &data,
                None,
                &NEEDS[ON],
                "",
                false,
                &Reading {
                    by_thread,
                    focus: &focus,
                },
            );
            match pane.body {
                Body::Tree(tree) => (tree, pane.aside),
                _ => panic!("a tree"),
            }
        };
        let path = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let (merged, _) = read(false);
        assert_eq!(merged.nodes[0].children.len(), 1, "one tower");
        let work = merged.find(&path(&["main", "work"]));
        assert_eq!(merged.nodes[work].value[0], 45.0);
        assert_eq!(merged.nodes[work].events[0], 45);
        let (split, aside) = read(true);
        assert_eq!(split.nodes[0].children.len(), 2, "one tower per task");
        let seven = split.find(&path(&["[task:7]", "main", "work"]));
        assert_eq!(split.path(seven), path(&["[task:7]", "main", "work"]));
        assert_eq!(split.nodes[seven].value[0], 30.0);
        let eight = split.find(&path(&["[task:8]", "main", "work"]));
        assert_eq!(split.nodes[eight].value[0], 15.0);
        assert!(
            aside.is_none_or(|aside| !aside.contains("not split by thread")),
            "keyed rows are split"
        );
    }

    /// The view reads a row's series and writes it back as `omn` wrote it.
    #[test]
    fn a_stack_rows_series_survives_a_round_trip() {
        let json = r#"{"emit":"stack","stacks":[{"stack":1,"events":2,"total":2.0,"series":"task:7"},{"stack":1,"events":1,"total":1.0}],"unattributed":0}"#;
        let output: Output = serde_json::from_str(json).unwrap();
        let Output::Stack { stacks, .. } = &output else {
            panic!("a stack emit")
        };
        assert_eq!(stacks[0].series.as_deref(), Some("task:7"));
        assert_eq!(stacks[1].series, None);
        let written = serde_json::to_value(&output).unwrap();
        assert_eq!(written["stacks"][0]["series"], "task:7");
        assert!(written["stacks"][1].get("series").is_none());
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

    /// A recording replayed as live, half way. `as_live_until` keeps a
    /// source's raw events only, so a source the recording kept as `stack`
    /// alone arrives empty -- which a live run, handing over its samples as
    /// they come, would not. That is said on the screen, as a degradation of
    /// the replay, rather than drawn as a run that sampled nothing.
    /// A recording as it looked half way, live: `as_live_until` says itself
    /// that a source kept as `stack` only arrives empty.
    fn live_half_way(name: &str) -> Data {
        let data = fixture(name);
        let half = data.opened_ns() + (data.closed_ns() - data.opened_ns()) / 2;
        crate::feed::as_live_until(&data, half)
    }

    #[test]
    fn snapshot_a_live_run_half_way() {
        let mut app = App::new(live_half_way("sqlite-commits"));
        app.cost = Cost::Snapshot;
        app.open("flame").unwrap();
        let screen = text(&draw(&mut app));
        assert!(
            screen.contains("this recording kept `cpu.profile` as `stack` only"),
            "{screen}"
        );
        assert_snapshot("flame-live--sqlite-commits", &draw(&mut app));
    }

    #[test]
    fn snapshot_a_narrow_terminal() {
        let mut app = app("http-server");
        assert_snapshot("flame-narrow--http-server", &draw_app(&mut app, 80, 24));
    }
}
