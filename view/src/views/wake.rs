//! The `wake` view: who woke whom, from a blocked thread back to what
//! unblocked it.
//!
//! Drawn from `sched.wakeup` alone, one event per wakeup: who was made
//! runnable (the wakee), by whom (a task, or an interrupt), and when. A chain
//! is read backwards in time: the chosen thread was woken by a waker; that
//! waker had itself been woken a moment before, by another; and so on until
//! the chain reaches an interrupt (a device), a task whose own wakeup the run
//! did not see, or a thread already on the chain (a cycle: a request and its
//! reply).
//!
//! Two times are drawn at each link. The **hold** is the waker's: from its
//! own wakeup to the wakeup it issued -- how long the chain sat with it before
//! it passed it on. The **quiet** is the woken thread's: from its last sign of
//! life in the run (a wakeup it received or issued) to this wakeup -- at most
//! how long it was blocked. The wakeups of one thread are folded into one
//! tree, as a profiler folds stacks: each path from the thread to the end of
//! its chain is a branch, with how many wakeups came that way, their share,
//! the medians of both times, the worst hold, and when in the run they came.
//! Below the tree, the longest single chains, link by link.
//!
//! What this view cannot say, it says in place: a wakeup from a soft
//! interrupt may be a device or a loopback send served in its sender's own
//! context (the source's caveat: the sender is not named), and a waker outside
//! the target has no wakeups of its own in the run, so its chain stops there.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::{Action, Ctx, Focus, Need, Scope, View};
use crate::model::{Data, Output};
use crate::trust::{self, palette, Trust};
use crate::units;

/// What this view needs measured.
pub const NEEDS: &[Need] = &[Need {
    source: "sched.wakeup",
    mode: "trace",
    emit: &["raw"],
    keys: &[],
    stack: None,
    scope: Scope::Target,
    why: "the wake chain",
}];

/// How far back a chain is followed when nothing else ends it.
const DEPTH: usize = 6;
const DEPTH_MAX: usize = 12;
/// One thread in the grid of woken threads: its name, its wakeups, and
/// what mostly woke it.
const CELL: usize = 38;
const CELL_GAP: usize = 2;
/// The grid's rows at most: the tree below it is the view.
const GRID_ROWS: usize = 8;
/// The cells of the "when" strip, one per slice of the run.
const STRIP: usize = 24;
const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// The figure columns of the tree: wakes, share, quiet, hold, max hold.
const FIGURES: usize = 6 + 6 + 9 + 9 + 9;

pub struct Wake {
    /// The thread whose chains are drawn, by tid: kept by identity rather
    /// than by row, so a live run adding threads does not move it.
    selected: Option<i32>,
    depth: usize,
    cache: Option<(Stamp, Rc<Index>)>,
    /// The tree last drawn, by the index it was folded from, its thread and
    /// its depth: a live run's 200 000 wakeups are not refolded every frame.
    folded: Option<((usize, i32, usize), Rc<Tree>)>,
}

impl View for Wake {
    fn name(&self) -> &'static str {
        "wake"
    }

    fn title(&self) -> &'static str {
        "who woke whom: from a blocked thread to what unblocked it"
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
        let held = ctx.data.for_need(&NEEDS[0].label(), NEEDS[0].source);
        if held.is_empty() {
            if let Some(refusal) = ctx.data.refusal(NEEDS[0].source) {
                trust::render_refusal(frame, area, refusal);
            } else {
                let inner = block.inner(area);
                frame.render_widget(block, area);
                frame.render_widget(Paragraph::new(absent()).wrap(Wrap { trim: true }), inner);
            }
            return;
        }
        let trust = Trust::joined(ctx.data, held.iter().copied());
        let index = self.index(ctx.data);
        let rows = index.rows(ctx.focus);
        let root = self.root(&rows, ctx.focus);

        let inner = block.inner(area);
        frame.render_widget(block, area);
        // A short screen keeps the tree: the grid gives up rows first, and
        // the legend says itself in one line.
        let short = inner.height < 30;
        let legend = legend_lines(short);
        let legend_height = legend_height(&legend, inner.width);
        let grid_rows = usize::from(inner.height / 5).clamp(2, GRID_ROWS);
        let grid = index.grid_lines(&rows, root, usize::from(inner.width), grid_rows, &trust);
        let [head, grid_area, tree_area, legend_area] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(grid.len() as u16 + 1),
            Constraint::Min(0),
            Constraint::Length(legend_height),
        ])
        .areas(inner);
        frame.render_widget(Paragraph::new(index.headline(&trust)), head);
        frame.render_widget(Paragraph::new(grid), grid_area);
        frame.render_widget(
            Paragraph::new(legend).wrap(Wrap { trim: true }),
            legend_area,
        );
        let lines = match root {
            Some(tid) => {
                let folded = self.folded(&index, tid);
                index.tree_lines(&folded, tid, tree_area.width, tree_area.height, &trust)
            }
            None => vec![Line::from(Span::styled(
                match ctx.focus.pid {
                    Some(pid) => format!("No thread of pid {pid} was woken in this run."),
                    None => "No thread was woken in this run.".to_owned(),
                },
                Style::new().add_modifier(Modifier::ITALIC),
            ))],
        };
        frame.render_widget(Paragraph::new(lines), tree_area);
    }

    fn key(&mut self, key: KeyEvent, ctx: &Ctx<'_>) -> Action {
        let index = self.index(ctx.data);
        let rows = index.rows(ctx.focus);
        let root = self.root(&rows, ctx.focus);
        let at = root.and_then(|tid| rows.iter().position(|row| row.tid == tid));
        let mut select = |to: Option<usize>| {
            if let Some(to) = to.filter(|to| *to < rows.len()) {
                self.selected = Some(rows[to].tid);
            }
            Action::None
        };
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => select(at.map(|at| at + 1)),
            KeyCode::Up | KeyCode::Char('k') => select(at.and_then(|at| at.checked_sub(1))),
            KeyCode::PageDown => select(at.map(|at| (at + GRID_ROWS).min(rows.len() - 1))),
            KeyCode::PageUp => select(at.map(|at| at.saturating_sub(GRID_ROWS))),
            KeyCode::Home => select(Some(0)),
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.depth = (self.depth + 1).min(DEPTH_MAX);
                Action::None
            }
            KeyCode::Char('-') => {
                self.depth = self.depth.saturating_sub(1).max(1);
                Action::None
            }
            KeyCode::Enter => match root {
                Some(tid) => Action::Focus(index.focus_of(tid)),
                None => Action::None,
            },
            KeyCode::Char('l') => match root {
                Some(tid) => Action::Goto {
                    view: "lanes",
                    focus: index.focus_of(tid),
                },
                None => Action::None,
            },
            _ => Action::None,
        }
    }

    fn keys(&self) -> &'static [(&'static str, &'static str)] {
        &[
            ("↑/↓", "thread"),
            ("+/-", "chain depth"),
            ("enter", "focus it"),
            ("l", "its lanes"),
        ]
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(Wake {
        selected: None,
        depth: DEPTH,
        cache: None,
        folded: None,
    })
}

impl Wake {
    /// The run's wakeups, indexed once per state of the run: a replay builds
    /// it once, a live run again when events have arrived.
    fn index(&mut self, data: &Data) -> Rc<Index> {
        let stamp = Stamp::of(data);
        match &self.cache {
            Some((held, index)) if *held == stamp => Rc::clone(index),
            _ => {
                let index = Rc::new(Index::build(data));
                self.cache = Some((stamp, Rc::clone(&index)));
                // A new index may be allocated where the old one was: the
                // tree folded from the old one goes with it.
                self.folded = None;
                index
            }
        }
    }

    /// The tree of `root`'s chains, folded once per index, thread and depth.
    fn folded(&mut self, index: &Rc<Index>, root: i32) -> Rc<Tree> {
        let key = (Rc::as_ptr(index) as usize, root, self.depth);
        match &self.folded {
            Some((held, tree)) if *held == key => Rc::clone(tree),
            _ => {
                let tree = Rc::new(index.tree(root, self.depth));
                self.folded = Some((key, Rc::clone(&tree)));
                tree
            }
        }
    }

    /// The thread drawn: the one the user moved to, else the focused thread,
    /// else the most woken thread of the target.
    fn root(&self, rows: &[Row], focus: &Focus) -> Option<i32> {
        let present = |tid: &i32| rows.iter().any(|row| row.tid == *tid);
        self.selected
            .filter(present)
            .or_else(|| focus.tid.filter(present))
            .or_else(|| rows.first().map(|row| row.tid))
    }
}

/// What the run held when the index was built.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    origin: String,
    events: usize,
    last_ns: u64,
    closed_ns: u64,
}

impl Stamp {
    fn of(data: &Data) -> Stamp {
        let wakeups: Vec<_> = data
            .of_source(NEEDS[0].source)
            .map(|results| results.events())
            .collect();
        Stamp {
            origin: data.origin.clone(),
            events: wakeups.iter().map(|events| events.len()).sum(),
            last_ns: wakeups
                .iter()
                .filter_map(|events| events.last().map(|event| event.ts_ns))
                .max()
                .unwrap_or(0),
            closed_ns: data.closed_ns(),
        }
    }
}

/// Who issued a wakeup.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Waker {
    /// A task, by tid.
    Task(i32),
    /// The idle task: a waker of 0 in a task's context.
    Idle,
    /// An interrupt, as the source names it: `hardirq`, `softirq`, `nmi`.
    Interrupt(String),
}

/// One wakeup.
#[derive(Debug, Clone)]
struct Wakeup {
    ts: u64,
    wakee: i32,
    waker: Waker,
}

/// The run's wakeups, by wakee, with what the run says of each thread.
#[derive(Debug, Default)]
struct Index {
    /// In time order.
    wakeups: Vec<Wakeup>,
    /// For each wakee, its wakeups' positions in `wakeups`, in time order.
    by_wakee: HashMap<i32, Vec<usize>>,
    /// For each waker task, the wakeups it issued, in time order.
    by_waker: HashMap<i32, Vec<usize>>,
    /// Each thread's name, the one the run's events most often give it.
    comm: HashMap<i32, String>,
    pid: HashMap<i32, i32>,
    /// The threads the run measured on the target, where it can tell; empty
    /// when the run holds nothing to tell it by, and then nothing is marked.
    target: HashSet<i32>,
    opened_ns: u64,
    closed_ns: u64,
}

/// A thread in the grid.
#[derive(Debug, Clone)]
struct Row {
    tid: i32,
    woken: usize,
    in_target: bool,
    /// Its most frequent waker and that waker's share.
    top: Option<(Waker, f64)>,
}

/// One node of a thread's folded chains.
#[derive(Debug, Clone)]
struct Node {
    waker: Waker,
    children: Vec<usize>,
    /// The wakeups of the root that came this way.
    count: usize,
    /// The holds at this link, where the waker's own wakeup was seen.
    holds: Vec<u64>,
    /// The quiet of the thread this link woke, where its last sign of life
    /// was seen.
    quiets: Vec<u64>,
    /// When the root's wakeups that came this way happened.
    when: Vec<u64>,
    /// How the chains that stop here stopped.
    ends: BTreeMap<End, usize>,
}

impl Node {
    fn new(waker: Waker) -> Node {
        Node {
            waker,
            children: Vec::new(),
            count: 0,
            holds: Vec::new(),
            quiets: Vec::new(),
            when: Vec::new(),
            ends: BTreeMap::new(),
        }
    }
}

/// Why a chain stops at a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum End {
    /// An interrupt or the idle task: there is nothing before it.
    Origin,
    /// A thread already on this chain: it came back.
    Cycle,
    /// The waker's own wakeup is not in the run: outside the target, or
    /// running since before the run opened.
    Unseen,
    /// The depth asked for.
    Depth,
}

/// One thread's wakeups, their chains folded. `nodes[0]` is the thread.
struct Tree {
    nodes: Vec<Node>,
    /// Each wakeup's chain, in time order.
    chains: Vec<Chain>,
}

/// One link of a chain: who woke the thread before it, and the two times.
#[derive(Debug, Clone, PartialEq)]
struct Link {
    waker: Waker,
    /// How long the waker had been awake when it issued the wakeup.
    hold: Option<u64>,
    /// How long the thread it woke had shown no sign of life.
    quiet: Option<u64>,
}

/// One wakeup followed back.
#[derive(Debug, Clone)]
struct Chain {
    /// When the thread was woken.
    ts: u64,
    /// From the thread's waker back.
    links: Vec<Link>,
    end: End,
    /// When the deepest wakeup the chain reached happened: two wakeups of
    /// the thread that one earlier wakeup led to share it.
    origin_ts: u64,
}

impl Chain {
    /// The time the chain held the wakeup before it reached the thread.
    fn held(&self) -> u64 {
        self.links.iter().filter_map(|link| link.hold).sum()
    }
}

impl Index {
    fn build(data: &Data) -> Index {
        let mut index = Index {
            opened_ns: data.opened_ns(),
            closed_ns: data.closed_ns(),
            ..Index::default()
        };
        let mut names: HashMap<i32, HashMap<String, usize>> = HashMap::new();
        let mut measured: HashSet<i32> = HashSet::new();
        for results in &data.run.results {
            let is_wakeup = results.source == NEEDS[0].source;
            let on_target = !results.label.contains("@system");
            for event in results.events() {
                let Some(tid) = event.tid() else { continue };
                if let Some(comm) = event.attr("comm").and_then(|value| value.as_str()) {
                    *names
                        .entry(tid)
                        .or_default()
                        .entry(comm.to_owned())
                        .or_default() += 1;
                }
                if !is_wakeup {
                    if on_target {
                        measured.insert(tid);
                    }
                    continue;
                }
                let number = |name: &str| event.number(name).map(|value| value as i64);
                let wakee = number("wakee_tid").map_or(tid, |value| value as i32);
                let wakee_pid = number("wakee_pid")
                    .or_else(|| number("pid"))
                    .map_or(wakee, |value| value as i32);
                let context = event
                    .attr("waker_context")
                    .and_then(|value| value.as_str())
                    .unwrap_or("task");
                let waker_tid = number("waker_tid").unwrap_or(0) as i32;
                let waker = match (context, waker_tid) {
                    ("task", 0) => Waker::Idle,
                    ("task", tid) => Waker::Task(tid),
                    (other, _) => Waker::Interrupt(other.to_owned()),
                };
                if let (Some(pid), Waker::Task(tid)) = (number("waker_pid"), &waker) {
                    index.pid.entry(*tid).or_insert(pid as i32);
                }
                index.pid.insert(wakee, wakee_pid);
                if matches!(waker, Waker::Interrupt(_)) {
                    // A target keeps an interrupt's wakeup only as its wakee:
                    // the wakee is the target's.
                    measured.insert(wakee);
                }
                index.wakeups.push(Wakeup {
                    ts: event.ts_ns,
                    wakee,
                    waker,
                });
            }
        }
        // Without a source other than this one to tell the target's threads
        // by, every wakee an interrupt did not wake would read as outside:
        // say nothing then.
        let others = data.run.results.iter().any(|results| {
            results.source != NEEDS[0].source
                && !results.label.contains("@system")
                && results
                    .outputs
                    .iter()
                    .any(|output| matches!(output, Output::Raw { events } if !events.is_empty()))
        });
        if others {
            index.target = measured;
        }
        index.comm = names
            .into_iter()
            .filter_map(|(tid, counts)| {
                counts
                    .into_iter()
                    .max_by(|one, two| one.1.cmp(&two.1).then_with(|| two.0.cmp(&one.0)))
                    .map(|(comm, _)| (tid, comm))
            })
            .collect();
        index.wakeups.sort_by_key(|wakeup| wakeup.ts);
        for (at, wakeup) in index.wakeups.iter().enumerate() {
            index.by_wakee.entry(wakeup.wakee).or_default().push(at);
            if let Waker::Task(tid) = wakeup.waker {
                index.by_waker.entry(tid).or_default().push(at);
            }
        }
        index
    }

    fn in_target(&self, tid: i32) -> bool {
        self.target.is_empty() || self.target.contains(&tid)
    }

    /// The woken threads: the target's first, by wakeups, then the threads
    /// outside it the target woke.
    fn rows(&self, focus: &Focus) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .by_wakee
            .iter()
            .filter(|(tid, _)| {
                focus
                    .pid
                    .is_none_or(|pid| self.pid.get(tid).is_some_and(|held| *held == pid))
            })
            .map(|(tid, woken)| {
                let mut wakers: HashMap<&Waker, usize> = HashMap::new();
                for at in woken {
                    *wakers.entry(&self.wakeups[*at].waker).or_default() += 1;
                }
                let top = wakers
                    .into_iter()
                    .max_by(|one, two| one.1.cmp(&two.1).then_with(|| two.0.cmp(one.0)))
                    .map(|(waker, count)| (waker.clone(), count as f64 / woken.len() as f64));
                Row {
                    tid: *tid,
                    woken: woken.len(),
                    in_target: self.in_target(*tid),
                    top,
                }
            })
            .collect();
        rows.sort_by(|one, two| {
            two.in_target
                .cmp(&one.in_target)
                .then(two.woken.cmp(&one.woken))
                .then(one.tid.cmp(&two.tid))
        });
        rows
    }

    fn focus_of(&self, tid: i32) -> Focus {
        Focus {
            pid: self.pid.get(&tid).copied(),
            tid: Some(tid),
            request: None,
        }
    }

    /// The latest wakeup of `tid` strictly before `ts`.
    fn previous(&self, tid: i32, ts: u64) -> Option<&Wakeup> {
        let woken = self.by_wakee.get(&tid)?;
        let before = woken.partition_point(|at| self.wakeups[*at].ts < ts);
        before.checked_sub(1).map(|at| &self.wakeups[woken[at]])
    }

    /// The last moment before `ts` the run saw `tid` alive: made runnable,
    /// or running and waking another.
    fn last_sign(&self, tid: i32, ts: u64) -> Option<u64> {
        let issued = self.by_waker.get(&tid).and_then(|issued| {
            let before = issued.partition_point(|at| self.wakeups[*at].ts < ts);
            before.checked_sub(1).map(|at| self.wakeups[issued[at]].ts)
        });
        let woken = self.previous(tid, ts).map(|wakeup| wakeup.ts);
        issued.max(woken)
    }

    /// One wakeup's chain, followed back from the thread it woke: each
    /// link's waker and its two times, and why the chain stops where it
    /// does.
    fn chain(&self, at: usize, depth: usize) -> Chain {
        let first = &self.wakeups[at];
        let mut on_chain: HashSet<i32> = HashSet::from([first.wakee]);
        let mut links = Vec::new();
        let mut wakeup = first;
        let mut origin_ts;
        let end = loop {
            origin_ts = wakeup.ts;
            let quiet = self
                .last_sign(wakeup.wakee, wakeup.ts)
                .map(|seen| wakeup.ts - seen);
            let link = |hold| Link {
                waker: wakeup.waker.clone(),
                hold,
                quiet,
            };
            let (tid, before) = match &wakeup.waker {
                Waker::Interrupt(_) | Waker::Idle => {
                    links.push(link(None));
                    break End::Origin;
                }
                Waker::Task(tid) => match self.previous(*tid, wakeup.ts) {
                    None => {
                        links.push(link(None));
                        break End::Unseen;
                    }
                    Some(before) => (*tid, before),
                },
            };
            links.push(link(Some(wakeup.ts - before.ts)));
            if !on_chain.insert(tid) {
                break End::Cycle;
            }
            if links.len() == depth {
                break End::Depth;
            }
            wakeup = before;
        };
        Chain {
            ts: first.ts,
            links,
            end,
            origin_ts,
        }
    }

    /// Every wakeup of `root`, its chain followed back and folded into a
    /// tree.
    fn tree(&self, root: i32, depth: usize) -> Tree {
        let mut nodes = vec![Node::new(Waker::Task(root))];
        let mut chains = Vec::new();
        for at in self.by_wakee.get(&root).map(Vec::as_slice).unwrap_or(&[]) {
            let chain = self.chain(*at, depth);
            nodes[0].count += 1;
            nodes[0].when.push(chain.ts);
            let mut parent = 0;
            for (level, link) in chain.links.iter().enumerate() {
                let found = nodes[parent]
                    .children
                    .iter()
                    .copied()
                    .find(|child| nodes[*child].waker == link.waker);
                let child = found.unwrap_or_else(|| {
                    nodes.push(Node::new(link.waker.clone()));
                    let child = nodes.len() - 1;
                    nodes[parent].children.push(child);
                    child
                });
                let node = &mut nodes[child];
                node.count += 1;
                node.when.push(chain.ts);
                node.holds.extend(link.hold);
                node.quiets.extend(link.quiet);
                if level + 1 == chain.links.len() {
                    *node.ends.entry(chain.end).or_default() += 1;
                }
                parent = child;
            }
            chains.push(chain);
        }
        for node in 0..nodes.len() {
            let mut children = std::mem::take(&mut nodes[node].children);
            children.sort_by(|one, two| {
                nodes[*two]
                    .count
                    .cmp(&nodes[*one].count)
                    .then_with(|| nodes[*one].waker.cmp(&nodes[*two].waker))
            });
            nodes[node].children = children;
        }
        Tree { nodes, chains }
    }

    /// A thread as the screen names it.
    fn thread(&self, tid: i32) -> String {
        match self.comm.get(&tid) {
            Some(comm) => format!("{comm} {tid}"),
            None => format!("tid {tid}"),
        }
    }

    /// A thread in `width` cells: the name gives way before the tid does.
    fn thread_in(&self, tid: i32, width: usize) -> String {
        let tid_text = tid.to_string();
        match self.comm.get(&tid) {
            Some(comm) if comm.chars().count() + 1 + tid_text.len() > width => format!(
                "{} {tid_text}",
                clip(comm, width.saturating_sub(tid_text.len() + 1))
            ),
            _ => clip(&self.thread(tid), width),
        }
    }

    fn waker_name(&self, waker: &Waker) -> String {
        match waker {
            Waker::Task(tid) => self.thread(*tid),
            Waker::Idle => "idle task".to_owned(),
            Waker::Interrupt(context) => context.clone(),
        }
    }

    fn headline(&self, trust: &Trust) -> Vec<Line<'static>> {
        let total = self.wakeups.len();
        let tasks = self
            .wakeups
            .iter()
            .filter(|wakeup| matches!(wakeup.waker, Waker::Task(_) | Waker::Idle))
            .count();
        let mut contexts: BTreeMap<&str, usize> = BTreeMap::new();
        for wakeup in &self.wakeups {
            if let Waker::Interrupt(context) = &wakeup.waker {
                *contexts.entry(context.as_str()).or_default() += 1;
            }
        }
        let share = |count: usize| units::percent(100.0 * count as f64 / total.max(1) as f64);
        let mut text = format!(
            "{} wakeups of {} threads -- from a task {}",
            units::count(total as f64),
            self.by_wakee.len(),
            share(tasks)
        );
        for (context, count) in &contexts {
            text.push_str(&format!(", {context} {}", share(*count)));
        }
        let span_ns = self.closed_ns.saturating_sub(self.opened_ns);
        if span_ns > 0 {
            text.push_str(&format!(", in {}", units::duration(span_ns as f64)));
        }
        vec![trust::figure(text, trust)]
    }

    /// The woken threads as a grid, column after column, the chosen one
    /// reversed: as many columns as the width holds, a page of them around
    /// the chosen thread, and a line saying what is off the page.
    fn grid_lines(
        &self,
        rows: &[Row],
        root: Option<i32>,
        width: usize,
        max_rows: usize,
        trust: &Trust,
    ) -> Vec<Line<'static>> {
        let dim = Style::new().add_modifier(Modifier::DIM);
        let columns = ((width + CELL_GAP) / (CELL + CELL_GAP)).max(1);
        let height = rows.len().div_ceil(columns).clamp(1, max_rows);
        let page = columns * height;
        let at = root
            .and_then(|tid| rows.iter().position(|row| row.tid == tid))
            .unwrap_or(0);
        let start = at / page * page;
        let shown = &rows[start..rows.len().min(start + page)];
        let mut lines = vec![Line::from(
            (0..columns.min(shown.len().div_ceil(height).max(1)))
                .flat_map(|column| {
                    let mut spans = Vec::new();
                    if column > 0 {
                        spans.push(Span::raw(" ".repeat(CELL_GAP)));
                    }
                    spans.push(Span::styled(
                        format!("{:<17} {:>5}  {:<13}", "woken thread", "wakes", "mostly by"),
                        dim.add_modifier(Modifier::UNDERLINED),
                    ));
                    spans
                })
                .collect::<Vec<_>>(),
        )];
        for line in 0..height {
            let mut spans = Vec::new();
            for column in 0..columns {
                let Some(row) = shown.get(column * height + line) else {
                    continue;
                };
                if column > 0 {
                    spans.push(Span::raw(" ".repeat(CELL_GAP)));
                }
                let mut name_style = if row.in_target { Style::new() } else { dim };
                if Some(row.tid) == root {
                    name_style = Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD);
                }
                let mostly = row
                    .top
                    .as_ref()
                    .map(|(waker, share)| {
                        let name = match waker {
                            Waker::Task(tid) => self
                                .comm
                                .get(tid)
                                .cloned()
                                .unwrap_or_else(|| format!("{tid}")),
                            other => self.waker_name(other),
                        };
                        format!("{:<8}{:>4.0}%", clip(&name, 8), share * 100.0)
                    })
                    .unwrap_or_default();
                spans.push(Span::styled(
                    format!("{:<17}", self.thread_in(row.tid, 17)),
                    name_style,
                ));
                spans.push(Span::styled(
                    format!(" {:>5}", units::count(row.woken as f64)),
                    trust.style(),
                ));
                spans.push(Span::styled(format!("  {mostly:<13}"), dim));
            }
            lines.push(Line::from(spans));
        }
        let outside = rows.iter().filter(|row| !row.in_target).count();
        let mut caption = Vec::new();
        if rows.len() > page {
            caption.push(format!(
                "threads {}-{} of {}, ↑/↓ to move",
                start + 1,
                start + shown.len(),
                rows.len()
            ));
        }
        if outside > 0 {
            caption.push(format!("{outside} dimmed: outside the target, woken by it"));
        }
        if !caption.is_empty() {
            lines.push(Line::from(Span::styled(
                caption.join("; "),
                dim.add_modifier(Modifier::ITALIC),
            )));
        }
        lines
    }

    fn tree_lines(
        &self,
        tree: &Tree,
        root: i32,
        width: u16,
        height: u16,
        trust: &Trust,
    ) -> Vec<Line<'static>> {
        let width = usize::from(width);
        let height = usize::from(height);
        let figures = trust.style();
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let dim = Style::new().add_modifier(Modifier::DIM);
        let strip = width >= FIGURES + 2 + STRIP + 30;
        let label_width = width
            .saturating_sub(FIGURES + if strip { 2 + STRIP } else { 0 })
            .max(12);
        let head = &tree.nodes[0];
        let mut lines = Vec::new();

        // The thread itself.
        let span = self.closed_ns.saturating_sub(self.opened_ns);
        let rate = if span > 0 {
            format!(
                ", {}/s",
                units::count(head.count as f64 / (span as f64 / 1e9))
            )
        } else {
            String::new()
        };
        let mut title = vec![
            Span::styled(self.thread(root), bold.fg(palette::ACCENT)),
            Span::raw(match self.pid.get(&root) {
                Some(pid) if *pid != root => format!(" (pid {pid})"),
                _ => String::new(),
            }),
            Span::styled(
                format!(
                    " was woken {} {}{rate}",
                    units::count(head.count as f64),
                    if head.count == 1 { "time" } else { "times" }
                ),
                figures,
            ),
        ];
        if !self.in_target(root) {
            title.push(Span::styled(
                " -- outside the target: only what the target woke",
                dim,
            ));
        }
        lines.push(Line::from(title));
        lines.push(Line::from(Span::styled(
            format!(
                "{:<label_width$}{:>6}{:>6}{:>9}{:>9}{:>9}{}",
                "<- woken by   <- and what woke it before",
                "wakes",
                "share",
                "quiet",
                "hold",
                "max hold",
                if strip {
                    format!("  {:<STRIP$}", "when in the run")
                } else {
                    String::new()
                },
            ),
            dim.add_modifier(Modifier::UNDERLINED),
        )));

        // Wakeups one earlier wakeup led to, along the same links, are one
        // chain said once, with how many times it woke the thread.
        let mut longest: Vec<(&Chain, usize)> = Vec::new();
        for chain in tree.chains.iter().filter(|chain| chain.held() > 0) {
            let same = longest.iter_mut().find(|(held, _)| {
                held.origin_ts == chain.origin_ts
                    && held.links.len() == chain.links.len()
                    && held
                        .links
                        .iter()
                        .zip(&chain.links)
                        .all(|(one, two)| one.waker == two.waker)
            });
            match same {
                Some((_, times)) => *times += 1,
                None => longest.push((chain, 1)),
            }
        }
        longest.sort_by(|one, two| {
            two.0
                .held()
                .cmp(&one.0.held())
                .then(one.0.ts.cmp(&two.0.ts))
        });

        // Rows: the tree, one line for its left-out branches, then -- when
        // any chain held time -- at least three chains under a heading, and
        // two lines for what the thread woke in turn.
        let mut order: Vec<(usize, String)> = Vec::new();
        flatten(tree, 0, "", &mut order);
        let below = height.saturating_sub(lines.len() + 3);
        let room = if longest.is_empty() {
            below
        } else {
            order.len().min(below.saturating_sub(6)).max(below.min(3))
        };
        // When the tree does not fit, the smallest branches give way: the
        // rows kept are the largest, drawn in tree order, and a parent is
        // never left out under a child it holds, since a parent counts at
        // least as many wakeups and comes first among equals.
        // One row left out would take the row that says so: draw it.
        let room = if order.len() == room + 1 {
            room + 1
        } else {
            room
        };
        let kept: HashSet<usize> = if order.len() > room {
            let mut by_size: Vec<usize> = (0..order.len()).collect();
            by_size.sort_by(|one, two| {
                tree.nodes[order[*two].0]
                    .count
                    .cmp(&tree.nodes[order[*one].0].count)
                    .then(one.cmp(two))
            });
            by_size.into_iter().take(room).collect()
        } else {
            (0..order.len()).collect()
        };
        let busiest = buckets(&head.when, self.opened_ns, self.closed_ns)
            .into_iter()
            .max()
            .unwrap_or(0);
        let mut left_out = (0usize, 0usize);
        for (at, (node, prefix)) in order.iter().enumerate() {
            let node = &tree.nodes[*node];
            if !kept.contains(&at) {
                left_out.0 += 1;
                // Only the top of a left-out branch counts its wakeups.
                let parent = order[..at]
                    .iter()
                    .rposition(|(_, before)| before.chars().count() < prefix.chars().count());
                if parent.is_none_or(|parent| kept.contains(&parent)) {
                    left_out.1 += node.count;
                }
                continue;
            }
            let (name_style, tag) = self.tag(node);
            let prefix_width = prefix.chars().count();
            let name = clip(
                &self.waker_name(&node.waker),
                label_width.saturating_sub(prefix_width + 1),
            );
            let tag = clip(
                &tag,
                label_width.saturating_sub(prefix_width + name.chars().count() + 1),
            );
            let used = prefix_width + name.chars().count() + tag.chars().count();
            let share = 100.0 * node.count as f64 / head.count.max(1) as f64;
            let mut line = vec![
                Span::styled(prefix.clone(), dim),
                Span::styled(name, name_style),
                Span::styled(tag, dim),
                Span::raw(" ".repeat(label_width.saturating_sub(used))),
                Span::styled(
                    format!(
                        "{:>6}{:>6}{:>9}{:>9}{:>9}",
                        units::count(node.count as f64),
                        format!("{share:.0}%"),
                        median(&node.quiets),
                        median(&node.holds),
                        worst(&node.holds),
                    ),
                    figures,
                ),
            ];
            if strip {
                line.push(Span::raw("  "));
                line.push(Span::styled(
                    sparkline(&node.when, self.opened_ns, self.closed_ns, busiest),
                    figures.fg(palette::ACCENT),
                ));
            }
            lines.push(Line::from(line));
        }
        if left_out.0 > 0 {
            lines.push(Line::from(Span::styled(
                format!(
                    "   ... {} smaller links not drawn ({} wakeups)",
                    left_out.0,
                    units::count(left_out.1 as f64)
                ),
                dim,
            )));
        }
        if head.children.is_empty() {
            lines.push(Line::from(Span::styled("   (no wakeup)", dim)));
        }

        // The longest chains: one wakeup each, the time it spent reaching
        // the thread, link by link.
        let chain_rows = height.saturating_sub(lines.len() + 4);
        if !longest.is_empty() && chain_rows > 0 {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                format!(
                    "{:<9}{:>9}   {:<rest$}",
                    "woken at",
                    "held",
                    "the longest chains, link by link: each waker and its hold",
                    rest = width.saturating_sub(21),
                ),
                dim.add_modifier(Modifier::UNDERLINED),
            )));
            for (chain, times) in longest.iter().take(chain_rows) {
                let offset = chain.ts.saturating_sub(self.opened_ns) as f64;
                let mut line = vec![
                    Span::styled(
                        format!("{:<9}", format!("+{}", units::duration(offset))),
                        dim,
                    ),
                    Span::styled(
                        format!("{:>9}", units::duration(chain.held() as f64)),
                        figures.add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                ];
                for link in &chain.links {
                    line.push(Span::styled("  <- ", dim));
                    let style = match link.waker {
                        Waker::Task(_) => Style::new(),
                        _ => Style::new().fg(palette::TIME),
                    };
                    line.push(Span::styled(self.waker_name(&link.waker), style));
                    if let Some(hold) = link.hold {
                        line.push(Span::styled(
                            format!(" {}", units::duration(hold as f64)),
                            figures.fg(palette::ACCENT),
                        ));
                    }
                }
                line.push(Span::styled(
                    match chain.end {
                        End::Cycle => " ↺",
                        End::Depth => " ...",
                        End::Unseen => " (its wakeup unseen)",
                        End::Origin => "",
                    },
                    dim,
                ));
                if *times > 1 {
                    line.push(Span::styled(format!("  x{times}"), dim));
                }
                lines.push(Line::from(line));
            }
        }

        // And what it woke in turn.
        let mut woke: HashMap<i32, usize> = HashMap::new();
        for at in self.by_waker.get(&root).map(Vec::as_slice).unwrap_or(&[]) {
            *woke.entry(self.wakeups[*at].wakee).or_default() += 1;
        }
        let mut woke: Vec<(i32, usize)> = woke.into_iter().collect();
        woke.sort_by(|one, two| two.1.cmp(&one.1).then(one.0.cmp(&two.0)));
        let mut line = vec![Span::styled("-> it woke in turn: ", bold)];
        if woke.is_empty() {
            line.push(Span::styled("no thread", dim));
        }
        for (n, (tid, count)) in woke.iter().take(4).enumerate() {
            if n > 0 {
                line.push(Span::raw(", "));
            }
            line.push(Span::raw(self.thread(*tid)));
            line.push(Span::styled(
                format!(" x{}", units::count(*count as f64)),
                figures,
            ));
        }
        if woke.len() > 4 {
            line.push(Span::styled(format!(", and {} more", woke.len() - 4), dim));
        }
        lines.push(Line::default());
        lines.push(Line::from(line));
        lines
    }

    /// How a node is drawn: its name's style, and what its chains' ends
    /// say, in a few words.
    fn tag(&self, node: &Node) -> (Style, String) {
        let ends = |end: End| node.ends.get(&end).copied().unwrap_or(0);
        match &node.waker {
            Waker::Interrupt(context) => (
                Style::new().fg(palette::TIME),
                match context.as_str() {
                    "hardirq" => "  a device",
                    "softirq" => "  a device, or a loopback send",
                    _ => "  an interrupt",
                }
                .to_owned(),
            ),
            Waker::Idle => (Style::new().fg(palette::TIME), String::new()),
            Waker::Task(tid) => {
                // What ends here, said with its count when only some of the
                // chains through this link end at it.
                let said = |end: End, words: &str| match ends(end) {
                    0 => None,
                    count if count == node.count => Some(format!("  {words}")),
                    count => Some(format!("  {words} x{count}")),
                };
                let tag = said(End::Cycle, "↺ back on the chain")
                    .or_else(|| {
                        said(
                            End::Unseen,
                            if self.in_target(*tid) {
                                "its wakeup unseen"
                            } else {
                                "outside the target: stops"
                            },
                        )
                    })
                    .or_else(|| said(End::Depth, "+ goes deeper"))
                    .unwrap_or_default();
                let style = if self.in_target(*tid) {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                };
                (style, tag)
            }
        }
    }
}

/// The tree in drawing order, each node with its tree-drawing prefix.
fn flatten(tree: &Tree, node: usize, indent: &str, out: &mut Vec<(usize, String)>) {
    let children = &tree.nodes[node].children;
    for (n, child) in children.iter().enumerate() {
        let last = n + 1 == children.len();
        out.push((
            *child,
            format!("{indent}{}", if last { "└─ " } else { "├─ " }),
        ));
        let deeper = format!("{indent}{}", if last { "   " } else { "│  " });
        flatten(tree, *child, &deeper, out);
    }
}

/// The median of some durations, or a dash where none was seen.
fn median(times: &[u64]) -> String {
    if times.is_empty() {
        return "--".to_owned();
    }
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    units::duration(sorted[(sorted.len() - 1) / 2] as f64)
}

/// The worst of some durations, or a dash where none was seen.
fn worst(times: &[u64]) -> String {
    match times.iter().max() {
        Some(worst) => units::duration(*worst as f64),
        None => "--".to_owned(),
    }
}

fn buckets(when: &[u64], opened: u64, closed: u64) -> Vec<usize> {
    let mut counts = vec![0usize; STRIP];
    let span = closed.saturating_sub(opened).max(1);
    for ts in when {
        let offset = ts.saturating_sub(opened).min(span - 1);
        let at = (u128::from(offset) * STRIP as u128 / u128::from(span)) as usize;
        counts[at.min(STRIP - 1)] += 1;
    }
    counts
}

/// When a branch's wakeups came, one cell per slice of the run, scaled to
/// the thread's busiest slice, so a branch's shape reads against the
/// thread's.
fn sparkline(when: &[u64], opened: u64, closed: u64, busiest: usize) -> String {
    buckets(when, opened, closed)
        .into_iter()
        .map(|count| {
            if count == 0 || busiest == 0 {
                ' '
            } else {
                let level = (count * LEVELS.len())
                    .div_ceil(busiest)
                    .clamp(1, LEVELS.len());
                LEVELS[level - 1]
            }
        })
        .collect()
}

fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_owned()
    } else if width == 0 {
        String::new()
    } else {
        let mut clipped: String = text.chars().take(width - 1).collect();
        clipped.push('…');
        clipped
    }
}

/// The rows a wrapped legend takes at a width, with one to spare for the
/// words a wrap pushes over.
fn legend_height(legend: &[Line<'_>], width: u16) -> u16 {
    let width = usize::from(width.max(1));
    let rows: usize = legend
        .iter()
        .map(|line| line.width().div_ceil(width).max(1))
        .sum();
    (rows + 1) as u16
}

fn legend_lines(short: bool) -> Vec<Line<'static>> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let term = dim.add_modifier(Modifier::BOLD);
    if short {
        return vec![Line::from(vec![
            Span::styled("quiet", term),
            Span::styled(": at most how long the woken thread was blocked; ", dim),
            Span::styled("hold", term),
            Span::styled(": how long the waker held the chain", dim),
        ])];
    }
    vec![Line::from(vec![
        Span::styled("quiet", term),
        Span::styled(
            ": the woken thread's time with no sign of life in the run (no wakeup received or \
             issued) before this one -- at most how long it was blocked. ",
            dim,
        ),
        Span::styled("hold", term),
        Span::styled(
            ": the waker's time from its own wakeup to the one it issued -- how long the chain \
             sat with it. A chain stops at an interrupt, at a thread already on it, or at a \
             waker whose own wakeup the run did not see. A softirq may be a loopback send served \
             in its sender's context: the sender is not named.",
            dim,
        ),
    ])]
}

fn absent() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            "This run did not trace sched.wakeup: there is no wake chain to draw.",
            Style::new().add_modifier(Modifier::ITALIC),
        )),
        Line::default(),
        Line::from(
            "`omn top -- prog` asks for it, and so does `omn top --headless -o DIR -- prog`; \
             a machine-wide recording made for the overview alone does not.",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::snapshot::{assert_snapshot, draw_app, fixture, text, HEIGHT, WIDTH};
    use crate::views::Focus;

    fn index(name: &str) -> Index {
        Index::build(&fixture(name))
    }

    #[test]
    fn every_wakeup_of_a_thread_is_in_its_tree_once_at_the_first_link() {
        let index = index("sqlite-commits");
        for tid in index.by_wakee.keys() {
            let tree = index.tree(*tid, DEPTH);
            let first: usize = tree.nodes[0]
                .children
                .iter()
                .map(|child| tree.nodes[*child].count)
                .sum();
            assert_eq!(first, index.by_wakee[tid].len(), "tid {tid}");
            // A branch never counts more than the link it hangs from.
            for node in &tree.nodes {
                let below: usize = node.children.iter().map(|c| tree.nodes[*c].count).sum();
                assert!(below <= node.count);
            }
        }
    }

    #[test]
    fn the_server_handoff_is_a_two_link_cycle_with_its_holds() {
        // http-server: the server's main thread accepts and hands the
        // connection to its handler thread, which answers and wakes the
        // main thread again. Thread-1's chain is main <- Thread-1: a cycle.
        let index = index("http-server");
        let handler = *index
            .comm
            .iter()
            .find(|(_, comm)| comm.starts_with("Thread-1"))
            .expect("the handler thread is named")
            .0;
        let main = index.pid[&handler];
        assert_ne!(main, handler);
        let tree = index.tree(handler, DEPTH);
        let by_main = tree.nodes[0]
            .children
            .iter()
            .map(|child| &tree.nodes[*child])
            .find(|node| node.waker == Waker::Task(main))
            .expect("the main thread wakes its handler");
        assert!(by_main.count >= 5, "{}", by_main.count);
        assert!(!by_main.holds.is_empty());
        let back = by_main
            .children
            .iter()
            .map(|child| &tree.nodes[*child])
            .find(|node| node.waker == Waker::Task(handler));
        assert!(
            back.is_some_and(|node| node.ends.contains_key(&End::Cycle)),
            "the handler woke main before main woke it: the chain comes back"
        );
        // The hold is a duration inside the run, never longer than it.
        let span = index.closed_ns - index.opened_ns;
        assert!(by_main.holds.iter().all(|hold| *hold <= span));
    }

    #[test]
    fn interrupts_end_a_chain_and_outside_wakers_are_not_followed() {
        let index = index("sqlite-commits");
        let rows = index.rows(&Focus::default());
        let sqlite = rows
            .iter()
            .find(|row| {
                index
                    .comm
                    .get(&row.tid)
                    .is_some_and(|comm| comm == "sqlite3")
            })
            .expect("sqlite3 was woken");
        assert!(sqlite.in_target);
        assert_eq!(
            rows[0].tid, sqlite.tid,
            "the target's most woken thread comes first"
        );
        let tree = index.tree(sqlite.tid, DEPTH);
        for child in &tree.nodes[0].children {
            let node = &tree.nodes[*child];
            match &node.waker {
                Waker::Interrupt(_) => {
                    assert!(node.children.is_empty());
                    assert_eq!(node.ends.get(&End::Origin), Some(&node.count));
                }
                Waker::Task(tid) if !index.in_target(*tid) => {
                    assert!(node.children.is_empty(), "tid {tid} was followed");
                }
                _ => {}
            }
        }
        // The outside threads come after the target's in the list.
        let first_outside = rows.iter().position(|row| !row.in_target);
        assert!(first_outside.is_some_and(|at| rows[at..].iter().all(|row| !row.in_target)));
    }

    #[test]
    fn a_pid_focus_keeps_its_threads_and_a_tid_focus_draws_it() {
        let data = fixture("http-server");
        let index = Index::build(&data);
        let handler = *index
            .comm
            .iter()
            .find(|(_, comm)| comm.starts_with("Thread-1"))
            .unwrap()
            .0;
        let focus = index.focus_of(handler);
        let rows = index.rows(&focus);
        assert!(rows
            .iter()
            .all(|row| index.pid[&row.tid] == focus.pid.unwrap()));
        let view = Wake {
            selected: None,
            depth: DEPTH,
            cache: None,
            folded: None,
        };
        assert_eq!(view.root(&rows, &focus), Some(handler));
    }

    #[test]
    fn keys_move_the_thread_and_open_its_lanes() {
        let data = fixture("http-server");
        let focus = Focus::default();
        let ctx = Ctx {
            data: &data,
            against: None,
            focus: &focus,
        };
        let mut view = Wake {
            selected: None,
            depth: DEPTH,
            cache: None,
            folded: None,
        };
        let key = |code| KeyEvent::from(code);
        let rows = view.index(&data).rows(&focus);
        assert_eq!(view.key(key(KeyCode::Down), &ctx), Action::None);
        assert_eq!(view.root(&rows, &focus), Some(rows[1].tid));
        view.key(key(KeyCode::Up), &ctx);
        view.key(key(KeyCode::Up), &ctx);
        assert_eq!(view.root(&rows, &focus), Some(rows[0].tid));
        match view.key(key(KeyCode::Char('l')), &ctx) {
            Action::Goto { view, focus } => {
                assert_eq!(view, "lanes");
                assert_eq!(focus.tid, Some(rows[0].tid));
            }
            other => panic!("{other:?}"),
        }
        view.key(key(KeyCode::Char('-')), &ctx);
        assert_eq!(view.depth, DEPTH - 1);
    }

    #[test]
    fn a_run_without_the_source_says_so_rather_than_drawing_nothing() {
        let mut app = App::new(fixture("machine"));
        app.cost = crate::app::Cost::Snapshot;
        app.open("wake").unwrap();
        let drawn = text(&draw_app(&mut app, WIDTH, HEIGHT));
        assert!(drawn.contains("did not trace sched.wakeup"), "{drawn}");
    }

    #[test]
    fn a_narrow_terminal_keeps_the_tree() {
        let mut app = App::new(fixture("sqlite-commits"));
        app.cost = crate::app::Cost::Snapshot;
        app.open("wake").unwrap();
        let drawn = text(&draw_app(&mut app, 80, 24));
        if std::env::var_os("OMN_WAKE_PRINT").is_some() {
            println!("{drawn}");
        }
        assert!(drawn.contains("hardirq"), "{drawn}");
        assert!(drawn.contains("was woken"), "{drawn}");
    }

    #[test]
    fn a_live_run_half_way_draws_what_it_has_and_says_it_is_live() {
        let data = fixture("http-server");
        let half = data.opened_ns() + (data.closed_ns() - data.opened_ns()) / 2;
        let live = crate::feed::as_live_until(&data, half);
        let whole = Index::build(&data).wakeups.len();
        let so_far = Index::build(&live).wakeups.len();
        assert!(so_far > 0 && so_far < whole, "{so_far} of {whole}");
        let mut app = App::new(live);
        app.cost = crate::app::Cost::Snapshot;
        app.open("wake").unwrap();
        let drawn = text(&draw_app(&mut app, WIDTH, HEIGHT));
        assert!(drawn.contains(&format!("{so_far} wakeups")), "{drawn}");
        assert!(drawn.contains("live"), "{drawn}");
    }

    /// The handler thread of the HTTP server, focused: the chain the
    /// default screen (the most woken thread, the client) does not show.
    #[test]
    fn snapshot_of_the_server_handler() {
        let data = fixture("http-server");
        let handler = *Index::build(&data)
            .comm
            .iter()
            .find(|(_, comm)| comm.starts_with("Thread-1"))
            .unwrap()
            .0;
        let mut app = App::new(data);
        app.cost = crate::app::Cost::Snapshot;
        app.open("wake").unwrap();
        app.focus = Focus {
            pid: None,
            tid: Some(handler),
            request: None,
        };
        assert_snapshot(
            "wake--http-server--handler",
            &draw_app(&mut app, WIDTH, HEIGHT),
        );
    }

    /// The server's main thread, two links deep.
    #[test]
    fn snapshot_of_the_server_main_thread() {
        let data = fixture("http-server");
        let index = Index::build(&data);
        let handler = *index
            .comm
            .iter()
            .find(|(_, comm)| comm.starts_with("Thread-1"))
            .unwrap()
            .0;
        let main = index.pid[&handler];
        let mut app = App::new(data);
        app.cost = crate::app::Cost::Snapshot;
        app.open("wake").unwrap();
        app.focus = Focus {
            pid: None,
            tid: Some(main),
            request: None,
        };
        assert_snapshot(
            "wake--http-server--main",
            &draw_app(&mut app, WIDTH, HEIGHT),
        );
    }
}
