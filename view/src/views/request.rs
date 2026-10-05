//! The `request` view: what one request costs -- its life by state, and its
//! share of each resource.
//!
//! It reads `net.request`, which follows a unit of work between two probes the
//! program fires itself (`request__start(id)`, `request__done(id)`) and adds
//! its time up by state: on a cpu, waiting for one, asleep (a lock), asleep
//! uninterruptibly (the disk), and the network on either side of it, read by
//! the request's socket. Three things are drawn:
//!
//! - **every request**, one row each, its span as a bar split by state and
//!   scaled to the slowest, so what made one request slow reads at a glance;
//! - **the selected request's waterfall** on its own time axis, from its first
//!   byte in to its last byte out: the network before and after it where they
//!   happened, its service between its two probes, and each block request it
//!   issued at the moment it was on the device (`io.latency` events carrying
//!   its `req` key, when the run holds them);
//! - **where its time went and its share of each resource**: each state with
//!   the evidence the run's decomposition grades it with, the lock's releaser,
//!   the cpu it took of its process's, the device time it took of the run's.
//!
//! What is not known is said rather than drawn: `net.request` sums the time
//! inside the service by state, it does not record their order, so the
//! service bar is the states' proportions, labelled as such, never a
//! sequence.

use std::collections::BTreeMap;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;
use serde_json::Value;

use super::{Action, Ctx, Focus, Need, Region, Scope, View};
use crate::model::{Data, Event, Key, Results};
use crate::trust::{self, palette, Trust};
use crate::units;

/// What this view needs measured.
pub const NEEDS: &[Need] = &[Need {
    source: "net.request",
    mode: "trace",
    emit: &["raw"],
    keys: &[],
    stack: None,
    scope: Scope::Target,
    why: "each request's life by state, and the network on either side of it",
}];

/// The states a request's span is tiled by, in the order of its life and of
/// the decomposition `examples/vertical-one-request.yml` asks: the six add
/// up to the span exactly.
const STATES: [State; 6] = [
    State {
        name: "net in",
        decomposed: "net_rx",
        glyph: '░',
        colour: Color::Cyan,
    },
    State {
        name: "on cpu",
        decomposed: "cpu_on",
        glyph: '█',
        colour: Color::Green,
    },
    State {
        name: "run queue",
        decomposed: "sched_wait",
        glyph: '▓',
        colour: Color::Yellow,
    },
    State {
        name: "lock/sleep",
        decomposed: "lock_wait",
        glyph: '▀',
        colour: Color::Magenta,
    },
    State {
        name: "disk",
        decomposed: "block_io",
        glyph: '▄',
        colour: Color::Blue,
    },
    State {
        name: "net out",
        decomposed: "net_tx",
        glyph: '░',
        colour: Color::LightCyan,
    },
];

/// The width of the table of where a request's time went.
const WHERE_WIDTH: u16 = 66;

const NET_IN: usize = 0;
const ON_CPU: usize = 1;
const RUN_QUEUE: usize = 2;
const LOCK: usize = 3;
const DISK: usize = 4;
const NET_OUT: usize = 5;

struct State {
    name: &'static str,
    /// The state's name in the decomposition, for its evidence.
    decomposed: &'static str,
    glyph: char,
    colour: Color,
}

impl State {
    fn style(&self) -> Style {
        Style::new().fg(self.colour)
    }
}

/// One request, read off its `net.request` event.
#[derive(Debug, Clone, PartialEq)]
struct Request {
    id: String,
    pid: i32,
    tid: i32,
    comm: String,
    sock: Option<u64>,
    /// Its first byte reaching the server's socket; `None` when `net.request`
    /// could not place it and so published no `net_rx`.
    first_byte: Option<u64>,
    /// `request__start`.
    born: u64,
    /// `request__done`.
    done: u64,
    /// Its last byte leaving the server's socket; `None` when the response
    /// had not left the stack, and `net.request` published no `net_tx` rather
    /// than a short one.
    drained: Option<u64>,
    /// The six states, in the order of [`STATES`]. Where a network side is
    /// unknown its state holds only the part inside the service (the wait in
    /// `recv` or `send`), a lower bound: see [`Request::known`].
    states: [f64; 6],
    /// First byte in to last byte out, as `net.request` added it up.
    span_ns: Option<f64>,
    rx_queued: f64,
    rx_wait: f64,
    tx_wait: f64,
    tx_drain: f64,
    blocks: u64,
    preempted: u64,
    holder_tid: Option<i64>,
    holder_ns: f64,
}

/// The request id an event carries, as its `req` key spells it.
fn req_id(event: &Event) -> Option<String> {
    event.keys.iter().find_map(|key| match key {
        Key::Req { id } => Some(id.clone()),
        _ => None,
    })
}

impl Request {
    fn of(event: &Event) -> Option<Request> {
        let id = req_id(event)?;
        let number = |name: &str| event.number(name).unwrap_or(0.0).max(0.0);
        let instant = |name: &str| event.number(name).filter(|t| *t > 0.0).map(|t| t as u64);
        let total = number("total_ns");
        let born = instant("born_ns").unwrap_or(event.ts_ns.saturating_sub(total as u64));
        let done = event.ts_ns.max(born);
        let rx_wait = number("rx_wait_ns");
        let tx_wait = number("tx_wait_ns");
        // A side is known only when `net.request` closed it: it leaves out
        // `net_rx` (`net_tx`) rather than publish a short one, and the view
        // does not fill it in.
        let net_rx = event.number("net_rx_ns").map(|ns| ns.max(0.0));
        let net_tx = event.number("net_tx_ns").map(|ns| ns.max(0.0));
        let first_byte = net_rx
            .and(instant("first_byte_ns"))
            .map(|first| first.min(born));
        let drained = net_tx.and(instant("drained_ns")).map(|last| last.max(done));
        Some(Request {
            id,
            pid: event
                .pid()
                .or_else(|| event.number("pid").map(|pid| pid as i32))
                .unwrap_or(0),
            tid: event
                .tid()
                .or_else(|| event.number("tid").map(|tid| tid as i32))
                .unwrap_or(0),
            comm: event
                .attr("comm")
                .and_then(|comm| comm.as_str())
                .unwrap_or("")
                .to_owned(),
            sock: event.keys.iter().find_map(|key| match key {
                Key::Sock { cookie } => Some(*cookie),
                _ => None,
            }),
            first_byte,
            born,
            done,
            drained,
            states: [
                net_rx.filter(|_| first_byte.is_some()).unwrap_or(rx_wait),
                number("oncpu_ns"),
                number("runq_ns"),
                number("sleep_ns"),
                number("disk_ns"),
                net_tx.filter(|_| drained.is_some()).unwrap_or(tx_wait),
            ],
            span_ns: event.number("span_ns").filter(|ns| *ns > 0.0),
            rx_queued: number("rx_queued_ns"),
            rx_wait,
            tx_wait,
            tx_drain: number("tx_drain_ns"),
            blocks: number("blocks") as u64,
            preempted: number("preempted") as u64,
            holder_tid: event
                .number("holder_tid")
                .filter(|tid| *tid > 0.0)
                .map(|tid| tid as i64),
            holder_ns: number("holder_ns"),
        })
    }

    /// Whether a state's time is known whole. Only the network's two sides
    /// can be unknown: their part outside the service is, when `net.request`
    /// could not close it.
    fn known(&self, state: usize) -> bool {
        match state {
            NET_IN => self.first_byte.is_some(),
            NET_OUT => self.drained.is_some(),
            _ => true,
        }
    }

    /// Whether its span runs from its first byte in to its last byte out;
    /// otherwise it is a lower bound, from wherever the known ends are.
    fn whole(&self) -> bool {
        self.known(NET_IN) && self.known(NET_OUT)
    }

    /// Where the known part of its life starts and ends.
    fn start(&self) -> u64 {
        self.first_byte.unwrap_or(self.born)
    }

    fn end(&self) -> u64 {
        self.drained.unwrap_or(self.done)
    }

    /// From its first byte in to its last byte out: what the six states tile.
    /// A lower bound when a network side is unknown.
    fn span(&self) -> f64 {
        let parts: f64 = self.states.iter().sum();
        parts.max((self.end() - self.start()) as f64)
    }

    /// The span in words, `≥` before it when it is a lower bound.
    fn span_text(&self) -> String {
        at_least(self.span(), self.whole())
    }

    /// The states that made most of it, largest first: each one above a
    /// twentieth of the span, two at most.
    fn mostly(&self) -> Vec<(usize, f64)> {
        let span = self.span();
        let mut shares: Vec<(usize, f64)> = self
            .states
            .iter()
            .enumerate()
            .map(|(index, ns)| (index, if span > 0.0 { ns / span } else { 0.0 }))
            .filter(|(_, share)| *share >= 0.05)
            .collect();
        shares.sort_by(|a, b| b.1.total_cmp(&a.1));
        shares.truncate(2);
        shares
    }
}

/// One block request a request issued, from `io.latency`.
#[derive(Debug, Clone, PartialEq)]
struct BlockIo {
    start: u64,
    end: u64,
    latency: f64,
    bytes: f64,
    dev: String,
}

/// Everything the view reads from a run, gathered once per draw.
struct Gathered<'a> {
    data: &'a Data,
    results: Vec<&'a Results>,
    requests: Vec<Request>,
    /// Block requests by the request id they carry.
    blocks: BTreeMap<String, Vec<BlockIo>>,
    /// The device time of every block request the run saw, carried or not.
    device_total: f64,
    block_results: Vec<&'a Results>,
    /// What `net.request` counted and could put in no event.
    undrawn: Undrawn,
}

/// The requests `net.request` counted and could put in no event, as its notes
/// say: a request with no end has no total, so it is in no event, and the
/// view says how many rather than let the list read as all of them.
#[derive(Debug, Clone, Default, PartialEq)]
struct Undrawn {
    /// Born and never ended, and the first few named (`req:<id> in task
    /// <tid>`).
    never_ended: u64,
    named: String,
    /// Of those, the ones a later `request__start` on their thread replaced.
    replaced: u64,
    /// `request__done` on a thread carrying no request.
    unborn: u64,
    /// Ended, and removed by the request's filter.
    filtered: u64,
}

impl Undrawn {
    /// Read off the fields the collector published (`never_ended`,
    /// `unborn`, `replaced`, `filtered_out`) and, for a recording made
    /// before it published them, off its notes, which the run carries as its
    /// result's notes or degradations. The first few names are in the notes
    /// only.
    fn of<'a>(results: impl IntoIterator<Item = &'a Results>) -> Undrawn {
        let results: Vec<&Results> = results.into_iter().collect();
        let mut from_notes = Undrawn::from_notes(results.iter().copied());
        let published = |name: &str| -> Option<u64> {
            let mut any = false;
            let mut total = 0;
            for results in &results {
                if let Some(count) = results.field_u64(name) {
                    any = true;
                    total += count;
                }
            }
            any.then_some(total)
        };
        if let Some(count) = published("never_ended") {
            from_notes.never_ended = count;
        }
        if let Some(count) = published("unborn") {
            from_notes.unborn = count;
        }
        if let Some(count) = published("replaced") {
            from_notes.replaced = count;
        }
        if let Some(count) = published("filtered_out") {
            from_notes.filtered = count;
        }
        from_notes
    }

    /// The counts in the collector's sentences.
    fn from_notes<'a>(results: impl IntoIterator<Item = &'a Results>) -> Undrawn {
        let mut undrawn = Undrawn::default();
        let leading = |text: &str| -> Option<u64> { text.split_whitespace().next()?.parse().ok() };
        for text in results
            .into_iter()
            .flat_map(|results| results.notes.iter().chain(&results.degradations))
        {
            if text.contains(" never ended in the run (") {
                undrawn.never_ended += leading(text).unwrap_or(0);
                if let Some(named) = text
                    .split_once(" never ended in the run (")
                    .and_then(|(_, rest)| rest.split_once("); "))
                {
                    if !undrawn.named.is_empty() {
                        undrawn.named.push_str(", ");
                    }
                    undrawn.named.push_str(named.0);
                }
            } else if text.contains("` hit") && text.contains("a task carrying no request") {
                undrawn.unborn += leading(text).unwrap_or(0);
            } else if text.contains("` hit") && text.contains("still carrying a request") {
                undrawn.replaced += leading(text).unwrap_or(0);
            } else if let Some(rest) = text.strip_prefix("the filter removed ") {
                undrawn.filtered += leading(rest).unwrap_or(0);
            }
        }
        undrawn
    }

    fn is_empty(&self) -> bool {
        self.never_ended + self.unborn + self.filtered == 0
    }

    /// In words, for the line under the summary.
    fn text(&self) -> String {
        let mut parts = Vec::new();
        if self.never_ended > 0 {
            let mut part = format!("{} never ended", self.never_ended);
            let mut named = vec![];
            if !self.named.is_empty() {
                named.push(self.named.clone());
            }
            if self.replaced > 0 {
                named.push(format!("{} replaced by a later start", self.replaced));
            }
            if !named.is_empty() {
                part.push_str(&format!(" ({})", named.join(", ")));
            }
            parts.push(part);
        }
        if self.unborn > 0 {
            parts.push(format!("{} ended with no birth", self.unborn));
        }
        if self.filtered > 0 {
            parts.push(format!("{} removed by the filter", self.filtered));
        }
        format!("  in no event, not drawn: {}", parts.join(" · "))
    }
}

impl<'a> Gathered<'a> {
    fn of(data: &'a Data) -> Gathered<'a> {
        let results = data.for_need(&NEEDS[0].label(), NEEDS[0].source);
        let mut requests: Vec<Request> = results
            .iter()
            .flat_map(|results| results.events())
            .filter_map(Request::of)
            .collect();
        requests.sort_by(|a, b| a.born.cmp(&b.born).then(a.id.cmp(&b.id)));
        // A request is one event; a second of the same id and birth (a live
        // round handed over twice) is the same request.
        requests.dedup_by(|a, b| a.id == b.id && a.born == b.born);
        // `io.latency` is read when the run holds it -- the session that
        // asked for requests asked for it, or another view did: a block
        // request submitted while a request was served carries its id. A run
        // may hold it under two labels (a target's and the system's): an
        // event both saw is one block request.
        let block_results: Vec<&Results> = data.of_source("io.latency").collect();
        let mut blocks: BTreeMap<String, Vec<BlockIo>> = BTreeMap::new();
        let mut device_total = 0.0;
        let mut seen = std::collections::BTreeSet::new();
        for event in block_results.iter().flat_map(|results| results.events()) {
            let latency = event.number("latency_ns").unwrap_or(0.0);
            let open = event
                .number("open_ts")
                .map(|t| t as u64)
                .or_else(|| {
                    event.keys.iter().find_map(|key| match key {
                        Key::Blk { open_ts, .. } => Some(*open_ts),
                        _ => None,
                    })
                })
                .unwrap_or(event.ts_ns.saturating_sub(latency as u64));
            let dev = event
                .attr("dev")
                .map(|dev| dev.to_string())
                .unwrap_or_default();
            if !seen.insert((event.ts_ns, open, dev.clone(), latency.to_bits())) {
                continue;
            }
            device_total += latency;
            let Some(id) = req_id(event) else {
                continue;
            };
            blocks.entry(id).or_default().push(BlockIo {
                start: open.min(event.ts_ns),
                end: event.ts_ns,
                latency,
                bytes: event.number("bytes").unwrap_or(0.0),
                dev,
            });
        }
        let undrawn = Undrawn::of(results.iter().copied());
        Gathered {
            data,
            results,
            requests,
            blocks,
            device_total,
            block_results,
            undrawn,
        }
    }

    /// Why a request with no block request carrying its id may still have
    /// issued some: `io.latency` lost events, or was not watching for the
    /// whole of it. `None` when its absence is a zero -- from a source that
    /// watched the whole request and lost nothing, the join engine's rule.
    fn blocks_unsure(&self, request: &Request) -> Option<Unsure> {
        let lost: u64 = self
            .block_results
            .iter()
            .map(|results| results.losses.missing())
            .sum();
        if lost > 0 {
            return Some(Unsure::Lost(lost));
        }
        let watched = self.block_results.iter().any(|results| {
            let window = results.window;
            let from = window.attach_ns.unwrap_or(window.opened_ns);
            from <= request.born && (window.closed_ns == 0 || window.closed_ns >= request.done)
        });
        (!watched).then_some(Unsure::Unwatched)
    }

    /// The cpu the requests' process took over the whole run, as the runner
    /// read it at its end.
    fn process_cpu_ns(&self) -> Option<f64> {
        self.results.iter().find_map(|results| {
            results
                .lifetime
                .as_ref()?
                .get("cpu_ns")?
                .as_f64()
                .filter(|cpu| *cpu > 0.0)
        })
    }
}

/// Why no block request carrying a request's id is not a zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unsure {
    /// `io.latency` lost this many records.
    Lost(u64),
    /// `io.latency` was not watching for the whole of the request.
    Unwatched,
}

impl Unsure {
    /// In a few words, for the waterfall's note.
    fn short(self) -> String {
        match self {
            Unsure::Lost(lost) => format!("io.latency lost {lost}"),
            Unsure::Unwatched => "io.latency not watching".to_owned(),
        }
    }

    /// In a sentence, for the share column.
    fn sentence(self) -> String {
        match self {
            Unsure::Lost(lost) => format!("io.latency lost {lost}, so some may be missing"),
            Unsure::Unwatched => {
                "io.latency was not watching for all of it, so some may be missing".to_owned()
            }
        }
    }
}

/// How the list is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Order {
    #[default]
    Arrival,
    Slowest,
}

#[derive(Default)]
pub struct RequestView {
    /// The selected request, by id, so a re-sort keeps it.
    selected: Option<String>,
    order: Order,
    /// The focus last applied, so a focus selects once and the arrows still
    /// move.
    applied: Option<String>,
    /// The ids as last drawn, in order, for the keys.
    shown: Vec<String>,
    /// The list's rows as last drawn, each its index in `shown`.
    regions: Vec<Region>,
}

impl RequestView {
    fn visible(&self, gathered: &Gathered<'_>, focus: &Focus) -> Vec<Request> {
        let mut requests: Vec<Request> = gathered
            .requests
            .iter()
            .filter(|request| focus.pid.is_none_or(|pid| pid == request.pid))
            .filter(|request| focus.tid.is_none_or(|tid| tid == request.tid))
            .cloned()
            .collect();
        if self.order == Order::Slowest {
            requests.sort_by(|a, b| b.span().total_cmp(&a.span()));
        }
        requests
    }

    fn settle(&mut self, focus: &Focus, requests: &[Request]) -> usize {
        if focus.request != self.applied {
            self.applied = focus.request.clone();
            if let Some(id) = &focus.request {
                self.selected = Some(id.clone());
            }
        }
        let index = self
            .selected
            .as_ref()
            .and_then(|id| requests.iter().position(|request| &request.id == id))
            .unwrap_or(0);
        self.selected = requests.get(index).map(|request| request.id.clone());
        self.shown = requests.iter().map(|request| request.id.clone()).collect();
        index
    }

    /// The selected request's threads, in `view`, narrowed to it.
    fn open(&self, view: &'static str, ctx: &Ctx<'_>) -> Action {
        let gathered = Gathered::of(ctx.data);
        match gathered
            .requests
            .iter()
            .find(|request| Some(&request.id) == self.selected.as_ref())
        {
            Some(request) => Action::Goto {
                view,
                focus: Focus {
                    pid: Some(request.pid),
                    tid: Some(request.tid),
                    request: Some(request.id.clone()),
                },
            },
            None => Action::None,
        }
    }

    fn step(&mut self, by: isize) {
        if self.shown.is_empty() {
            return;
        }
        let at = self
            .selected
            .as_ref()
            .and_then(|id| self.shown.iter().position(|shown| shown == id))
            .unwrap_or(0) as isize;
        let to = at
            .saturating_add(by)
            .clamp(0, self.shown.len() as isize - 1) as usize;
        self.selected = Some(self.shown[to].clone());
    }
}

impl View for RequestView {
    fn name(&self) -> &'static str {
        "request"
    }

    fn title(&self) -> &'static str {
        "what one request costs: its life by state, and its share of each resource"
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        self.regions.clear();
        let block = Block::new()
            .borders(Borders::ALL)
            .title(" request -- what one request costs ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width < 20 || inner.height < 4 {
            return;
        }
        let gathered = Gathered::of(ctx.data);
        let requests = self.visible(&gathered, ctx.focus);
        if requests.is_empty() {
            draw_none(frame, inner, ctx, &gathered);
            return;
        }
        let selected = self.settle(ctx.focus, &requests);
        let notice = notice_line(ctx.focus, &requests, &requests[selected], &gathered);
        let trust = Trust::joined(ctx.data, gathered.results.iter().copied());
        let block_trust = Trust::joined(ctx.data, gathered.block_results.iter().copied());

        let list_rows = (requests.len() as u16).min(inner.height.saturating_sub(24).max(3));
        let [summary, under, list, legend, _, detail] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(list_rows + 1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(inner);

        frame.render_widget(
            Paragraph::new(summary_line(&requests, &trust, self.order)),
            summary,
        );
        if let Some(notice) = notice {
            frame.render_widget(Paragraph::new(notice), under);
        }
        frame.render_widget(
            Paragraph::new(list_lines(&requests, selected, list, &trust)),
            list,
        );
        // The rows as `list_lines` draws them: a heading, then a window
        // that scrolls with the selection.
        let rows = usize::from(list.height.saturating_sub(1));
        let first = selected.saturating_sub(rows.saturating_sub(1));
        self.regions = (first..requests.len().min(first + rows))
            .map(|index| {
                let y = list.y + 1 + (index - first) as u16;
                Region::new(Rect::new(list.x, y, list.width, 1), index)
            })
            .collect();
        frame.render_widget(Paragraph::new(legend_line()), legend);
        draw_detail(
            frame,
            detail,
            &requests[selected],
            &requests,
            &gathered,
            &trust,
            &block_trust,
        );
    }

    fn key(&mut self, key: KeyEvent, ctx: &Ctx<'_>) -> Action {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::Home | KeyCode::Char('g') => self.step(isize::MIN),
            KeyCode::End | KeyCode::Char('G') => self.step(isize::MAX),
            KeyCode::Char('s') => {
                self.order = match self.order {
                    Order::Arrival => Order::Slowest,
                    Order::Slowest => Order::Arrival,
                }
            }
            KeyCode::Enter => {
                if let Some(id) = &self.selected {
                    // Once focused, `Enter` goes in: its threads in lanes,
                    // on its moment.
                    if ctx.focus.request.as_ref() == Some(id) {
                        return self.open("lanes", ctx);
                    }
                    let mut focus = ctx.focus.clone();
                    focus.request = Some(id.clone());
                    self.applied = Some(id.clone());
                    return Action::Focus(focus);
                }
            }
            KeyCode::Char('l') => return self.open("lanes", ctx),
            KeyCode::Char('w') => return self.open("wake", ctx),
            _ => {}
        }
        Action::None
    }

    fn keys(&self) -> &'static [(&'static str, &'static str)] {
        &[
            ("↑/↓", "request"),
            ("s", "sort"),
            ("enter", "focus it, then its lanes"),
            ("l", "its thread in lanes"),
            ("w", "its wake-ups"),
        ]
    }

    fn regions(&self) -> &[Region] {
        &self.regions
    }

    fn select(&mut self, id: usize, _ctx: &Ctx<'_>) -> Action {
        if let Some(request) = self.shown.get(id) {
            self.selected = Some(request.clone());
        }
        Action::None
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(RequestView::default())
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn dimmed(style: Style, dims: bool) -> Style {
    if dims {
        style.add_modifier(Modifier::DIM)
    } else {
        style
    }
}

/// One line on every request: how many, whose, the slowest and the median.
fn summary_line(requests: &[Request], trust: &Trust, order: Order) -> Line<'static> {
    let mut spans: Vec<f64> = requests.iter().map(Request::span).collect();
    spans.sort_by(f64::total_cmp);
    let median = spans[spans.len() / 2];
    let slowest = requests
        .iter()
        .max_by(|a, b| a.span().total_cmp(&b.span()))
        .expect("at least one request");
    let mut processes: Vec<(String, i32)> = requests
        .iter()
        .map(|request| (request.comm.clone(), request.pid))
        .collect();
    processes.sort();
    processes.dedup();
    let whose = match processes.as_slice() {
        [(comm, pid)] if !comm.is_empty() => format!("{comm} (pid {pid})"),
        [(_, pid)] => format!("pid {pid}"),
        many => format!("{} processes", many.len()),
    };
    let mut line = vec![
        Span::styled(
            format!(
                "{} request{} served by {whose}",
                requests.len(),
                if requests.len() == 1 { "" } else { "s" }
            ),
            trust.style().patch(bold()),
        ),
        Span::styled(
            format!(
                " · slowest {} {} · median {}",
                slowest.id,
                slowest.span_text(),
                units::duration(median)
            ),
            trust.style(),
        ),
        Span::styled(
            format!(
                " · {}  ",
                match order {
                    Order::Arrival => "in order of arrival",
                    Order::Slowest => "slowest first",
                }
            ),
            dim(),
        ),
    ];
    line.extend(trust.badges());
    Line::from(line)
}

/// The line under the summary: a focus on a request the list does not hold,
/// the requests whose span is a lower bound, and the requests `net.request`
/// counted and could put in no event.
fn notice_line(
    focus: &Focus,
    requests: &[Request],
    drawn: &Request,
    gathered: &Gathered<'_>,
) -> Option<Line<'static>> {
    let mut spans = Vec::new();
    if let Some(id) = &focus.request {
        if !requests.iter().any(|request| &request.id == id) {
            let why = if gathered.requests.iter().any(|request| &request.id == id) {
                "hidden by the pid/tid focus"
            } else {
                "in no event of this run"
            };
            spans.push(Span::styled(
                format!(
                    "  the focus names {id}, {why}: {} is drawn instead",
                    drawn.id
                ),
                Style::new().fg(palette::LOSS),
            ));
        }
    }
    let partial = requests.iter().filter(|request| !request.whole()).count();
    if partial > 0 {
        spans.push(Span::styled(
            format!("  {partial} ≥: a network side unknown"),
            Style::new().fg(palette::LOSS),
        ));
    }
    if !gathered.undrawn.is_empty() {
        spans.push(Span::styled(
            gathered.undrawn.text(),
            Style::new().fg(palette::LOSS),
        ));
    }
    (!spans.is_empty()).then(|| Line::from(spans))
}

/// A duration, `≥` before it when it is a lower bound.
fn at_least(ns: f64, known: bool) -> String {
    if known {
        units::duration(ns)
    } else {
        format!("≥{}", units::duration(ns))
    }
}

/// Splits `width` cells between `values`, in proportion to `of`, so that the
/// cells drawn add up to the whole's share of `width` -- the largest
/// remainders get the leftover cells, and no state is drawn wider than it is.
fn allocate(values: &[f64], of: f64, width: usize) -> Vec<usize> {
    if of <= 0.0 || width == 0 {
        return vec![0; values.len()];
    }
    let exact: Vec<f64> = values
        .iter()
        .map(|value| value.max(0.0) / of * width as f64)
        .collect();
    let whole = (exact.iter().sum::<f64>().round() as usize).min(width);
    let mut cells: Vec<usize> = exact.iter().map(|x| x.floor() as usize).collect();
    let mut left = whole.saturating_sub(cells.iter().sum());
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|a, b| {
        (exact[*b] - exact[*b].floor()).total_cmp(&(exact[*a] - exact[*a].floor()))
    });
    for index in order {
        if left == 0 {
            break;
        }
        if exact[index] > 0.0 {
            cells[index] += 1;
            left -= 1;
        }
    }
    cells
}

/// A bar of the six states, `width` cells for `of` nanoseconds.
fn state_bar(states: &[f64; 6], of: f64, width: usize, dims: bool) -> Vec<Span<'static>> {
    let cells = allocate(states, of, width);
    let mut spans = Vec::new();
    let mut drawn = 0;
    for (state, cells) in STATES.iter().zip(cells) {
        if cells > 0 {
            spans.push(Span::styled(
                state.glyph.to_string().repeat(cells),
                dimmed(state.style(), dims),
            ));
            drawn += cells;
        }
    }
    if drawn == 0 && states.iter().sum::<f64>() > 0.0 && width > 0 {
        // Something, smaller than a cell: never drawn as nothing.
        spans.push(Span::raw("▏"));
        drawn = 1;
    }
    spans.push(Span::raw(" ".repeat(width.saturating_sub(drawn))));
    spans
}

/// The states that made most of a request, in words: `lock/sleep 99.8%`.
/// When its span is a lower bound a share of it would be invented, so the
/// states are said by their time instead.
fn mostly_text(request: &Request) -> String {
    request
        .mostly()
        .iter()
        .map(|(index, share)| {
            let amount = if request.whole() {
                units::percent(share * 100.0)
            } else {
                at_least(request.states[*index], request.known(*index))
            };
            format!("{} {amount}", STATES[*index].name)
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn list_lines(
    requests: &[Request],
    selected: usize,
    area: Rect,
    trust: &Trust,
) -> Vec<Line<'static>> {
    let rows = usize::from(area.height.saturating_sub(1));
    let id_w = requests
        .iter()
        .map(|request| request.id.chars().count())
        .max()
        .unwrap_or(3)
        .max(3);
    let slowest = requests.iter().map(Request::span).fold(0.0, f64::max);
    let mostly_w = 34usize;
    let fixed = 2 + id_w + 1 + 8 + 2 + 2 + mostly_w;
    let bar_w = usize::from(area.width).saturating_sub(fixed).max(4);
    let heading = truncate(
        &format!(
            "its life by state, to one scale: the slowest is {}",
            units::duration(slowest)
        ),
        bar_w,
    );
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("  {:<id_w$} {:>8}  ", "req", "span"), bold()),
        Span::styled(format!("{heading:<bar_w$}  "), bold()),
        Span::styled("mostly", bold()),
    ])];
    // Keep the selection in sight: the window scrolls with it.
    let first = selected.saturating_sub(rows.saturating_sub(1));
    for (index, request) in requests.iter().enumerate().skip(first).take(rows) {
        let chosen = index == selected;
        let marker = if chosen { "▶ " } else { "  " };
        let mut row_style = trust.style();
        if chosen {
            row_style = row_style.patch(bold()).fg(palette::ACCENT);
        }
        let mut line = vec![
            Span::styled(marker, Style::new().fg(palette::ACCENT)),
            Span::styled(format!("{:<id_w$} ", request.id), row_style),
            Span::styled(
                format!("{:>8}  ", request.span_text()),
                trust
                    .style()
                    .patch(if chosen { bold() } else { Style::new() }),
            ),
        ];
        line.extend(state_bar(&request.states, slowest, bar_w, trust.dims()));
        line.push(Span::raw("  "));
        line.push(Span::styled(
            truncate(&mostly_text(request), mostly_w),
            trust.style(),
        ));
        lines.push(Line::from(line));
    }
    let more = requests.len().saturating_sub(first + rows);
    if more > 0 && lines.len() > 1 {
        let last = lines.len() - 1;
        lines[last]
            .spans
            .push(Span::styled(format!("  +{more} more ↓"), dim()));
    }
    lines
}

fn legend_line() -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    for state in &STATES {
        spans.push(Span::styled(
            format!("{}{} ", state.glyph, state.glyph),
            state.style(),
        ));
        spans.push(Span::styled(format!("{}   ", state.name), dim()));
    }
    Line::from(spans)
}

fn truncate(text: &str, room: usize) -> String {
    if text.chars().count() <= room {
        text.to_owned()
    } else {
        let mut out: String = text.chars().take(room.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// The tables of the run's decompositions that are about one request.
fn tables_of<'a>(data: &'a Data, id: &str) -> impl Iterator<Item = &'a Value> + 'a {
    let key = format!("req:{id}");
    data.run
        .decompositions
        .iter()
        .filter_map(|decomposition| decomposition.get("tables")?.as_array())
        .flatten()
        .filter(move |one| one.get("key").and_then(Value::as_str) == Some(key.as_str()))
}

/// A request's row of a state in the run's decompositions, if one carries
/// it: its evidence grade and what it names.
fn decomposed<'a>(data: &'a Data, id: &str, state: &str) -> Option<&'a Value> {
    tables_of(data, id)
        .filter_map(|one| one.get("rows")?.as_array())
        .flatten()
        .find(|row| row.get("state").and_then(Value::as_str) == Some(state))
}

/// The table that splits a request's disk wait by the device's time: its
/// whole, and the residue the device did not do.
fn block_table<'a>(data: &'a Data, id: &str) -> Option<&'a Value> {
    tables_of(data, id).find(|one| {
        one.get("rows")
            .and_then(Value::as_array)
            .is_some_and(|rows| {
                rows.iter()
                    .any(|row| row.get("source").and_then(Value::as_str) == Some("io.latency"))
            })
    })
}

fn draw_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    request: &Request,
    visible: &[Request],
    gathered: &Gathered<'_>,
    trust: &Trust,
    block_trust: &Trust,
) {
    let data = gathered.data;
    let blocks = gathered
        .blocks
        .get(&request.id)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    // The two columns are as tall as the taller needs, the share column's
    // wrapped lines counted at its width, so a narrow terminal pushes the
    // line under them down rather than cutting the share column's last lines.
    let share_width = area.width.saturating_sub(WHERE_WIDTH + 2).max(1);
    let share = share_lines(request, blocks, gathered, data, trust, block_trust);
    let share_rows: usize = share
        .iter()
        .map(|line| line.width().max(1).div_ceil(usize::from(share_width)))
        .sum();
    // Never fewer than nine: the where table and a row of air under it.
    let where_rows = STATES.len() + 3;
    let rows = share_rows.max(where_rows) as u16;
    // What a tall terminal leaves under the tables thickens the waterfall's
    // four rows, up to three rows each, rather than standing empty.
    let spare = area.height.saturating_sub(1 + WATERFALL + 1 + rows + 2);
    let thickness = 1 + (spare / WATERFALL_ROWS).min(THICKEST - 1);
    let [title, waterfall, _, tables] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(WATERFALL + WATERFALL_ROWS * (thickness - 1)),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);
    let heading = format!(
        "── {} · {} from {} to {} · thread {} ",
        request.id,
        request.span_text(),
        if request.known(NET_IN) {
            "its first byte in"
        } else {
            "request__start"
        },
        if request.known(NET_OUT) {
            "its last byte out"
        } else {
            "request__done"
        },
        request.tid
    );
    let rule = "─".repeat(usize::from(area.width).saturating_sub(heading.chars().count()));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(heading, bold().fg(palette::ACCENT)),
            Span::styled(rule, dim()),
        ])),
        title,
    );
    let lines = waterfall_lines(
        request,
        blocks,
        gathered,
        waterfall.width,
        trust,
        block_trust,
    );
    frame.render_widget(
        Paragraph::new(thickened(lines, usize::from(thickness))),
        waterfall,
    );
    let [columns, _, apart] = Layout::vertical([
        Constraint::Length(rows),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(tables);
    if let Some(line) = against_the_others(request, visible, trust) {
        frame.render_widget(Paragraph::new(line), apart);
    }
    let [left, _, right] = Layout::horizontal([
        Constraint::Length(WHERE_WIDTH),
        Constraint::Length(2),
        Constraint::Min(0),
    ])
    .areas(columns);
    frame.render_widget(
        Paragraph::new(where_lines(request, data, left.width, trust)),
        left,
    );
    frame.render_widget(Paragraph::new(share).wrap(Wrap { trim: false }), right);
}

/// The waterfall's rows: its axis, its four rows and the line under them.
const WATERFALL: u16 = 6;
/// The rows of the waterfall that are bars.
const WATERFALL_ROWS: u16 = 4;
/// The most rows a waterfall bar is drawn on.
const THICKEST: u16 = 3;

/// The waterfall with each of its four bars drawn on `thickness` rows: the
/// rows above a bar repeat its cells, its label and its note left to the
/// row that names it.
fn thickened(lines: Vec<Line<'static>>, thickness: usize) -> Vec<Line<'static>> {
    if thickness <= 1 {
        return lines;
    }
    let mut out = Vec::new();
    for (index, line) in lines.into_iter().enumerate() {
        if (1..=usize::from(WATERFALL_ROWS)).contains(&index) && line.spans.len() > 2 {
            let mut bare = line.spans.clone();
            bare.pop();
            bare[0] = Span::raw(" ".repeat(bare[0].width()));
            for _ in 1..thickness {
                out.push(Line::from(bare.clone()));
            }
        }
        out.push(line);
    }
    out
}

/// The step of a time axis of `span` nanoseconds: 1, 2 or 5 times a power of
/// ten, for at most `most` ticks.
fn tick_step(span: f64, most: usize) -> f64 {
    if span <= 0.0 || most == 0 {
        return 0.0;
    }
    let rough = span / most as f64;
    let power = 10f64.powf(rough.log10().floor());
    [1.0, 2.0, 5.0, 10.0]
        .into_iter()
        .map(|factor| power * factor)
        .find(|step| *step >= rough)
        .unwrap_or(power * 10.0)
}

/// A row of cells on a time axis.
struct Row {
    cells: Vec<Option<(char, Style)>>,
}

impl Row {
    fn new(width: usize) -> Row {
        Row {
            cells: vec![None; width],
        }
    }

    fn fill(&mut self, from: usize, to: usize, glyph: char, style: Style) {
        let to = to.min(self.cells.len().saturating_sub(1));
        for cell in self.cells.iter_mut().take(to + 1).skip(from) {
            *cell = Some((glyph, style));
        }
    }

    fn spans(&self) -> Vec<Span<'static>> {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for cell in &self.cells {
            let (glyph, style) = cell.unwrap_or((' ', Style::new()));
            match spans.last_mut() {
                Some(last) if last.style == style => last.content.to_mut().push(glyph),
                _ => spans.push(Span::styled(glyph.to_string(), style)),
            }
        }
        spans
    }
}

/// The waterfall: a time axis from the request's first byte, and a row each
/// for the network before it, its service, its block requests and the
/// network after it -- each drawn where it happened.
fn waterfall_lines(
    request: &Request,
    blocks: &[BlockIo],
    gathered: &Gathered<'_>,
    width: u16,
    trust: &Trust,
    block_trust: &Trust,
) -> Vec<Line<'static>> {
    let label_w = 12usize;
    let note_w = 38usize;
    let bar_w = usize::from(width)
        .saturating_sub(label_w + note_w + 2)
        .max(8);
    let origin = blocks
        .iter()
        .map(|one| one.start)
        .fold(request.start(), u64::min);
    let end = blocks
        .iter()
        .map(|one| one.end)
        .fold(request.end(), u64::max);
    let span = end.saturating_sub(origin).max(1) as f64;
    let col = |t: u64| -> usize {
        ((t.saturating_sub(origin) as f64 / span) * bar_w as f64)
            .floor()
            .min((bar_w - 1) as f64) as usize
    };
    // The cells [from, to) of time covers, at least one.
    let cols = |from: u64, to: u64| -> (usize, usize) {
        let a = col(from);
        (a, col(to.saturating_sub(1).max(from)).max(a))
    };
    let label = |text: &str, style: Style| Span::styled(format!("{text:<label_w$}"), style);
    let note =
        |text: String, style: Style| Span::styled(format!("  {}", truncate(&text, note_w)), style);

    let mut lines = Vec::new();
    // The axis.
    let step = tick_step(span, (bar_w / 10).max(1));
    let mut axis = vec![' '; bar_w];
    let mut t = 0.0;
    while step > 0.0 && t <= span {
        let at = ((t / span) * bar_w as f64).floor() as usize;
        let text = if t == 0.0 {
            "0".to_owned()
        } else {
            units::duration(t)
        };
        let tick: Vec<char> = format!("┊{text}").chars().collect();
        if at + tick.len() <= bar_w && axis[at..at + tick.len()].iter().all(|c| *c == ' ') {
            axis[at..at + tick.len()].copy_from_slice(&tick);
        }
        t += step;
    }
    lines.push(Line::from(vec![
        label("", dim()),
        Span::styled(axis.into_iter().collect::<String>(), dim()),
    ]));

    // The network in: its first byte to its birth, where it happened. When
    // `net.request` could not place the first byte, that part is unknown:
    // said, and marked `?` at the edge of what is known, never drawn as none.
    let unknown = Style::new().fg(palette::LOSS);
    let net_in = &STATES[NET_IN];
    let mut row = Row::new(bar_w);
    let mut line = vec![label("  net in", net_in.style())];
    match request.first_byte {
        Some(first) => {
            if request.born > first {
                let (a, b) = cols(first, request.born);
                row.fill(a, b, net_in.glyph, dimmed(net_in.style(), trust.dims()));
            }
            line.extend(row.spans());
            line.push(note(
                format!(
                    "{} before start{}",
                    units::duration(request.rx_queued),
                    if request.rx_wait > 0.0 {
                        format!(", {} in recv", units::duration(request.rx_wait))
                    } else {
                        String::new()
                    }
                ),
                trust.style(),
            ));
        }
        None => {
            let at = col(request.born);
            row.fill(at, at, '?', unknown);
            line.extend(row.spans());
            line.push(note(
                format!(
                    "unknown before start{}",
                    if request.rx_wait > 0.0 {
                        format!(", {} in recv", units::duration(request.rx_wait))
                    } else {
                        String::new()
                    }
                ),
                unknown,
            ));
        }
    }
    lines.push(Line::from(line));

    // The service: between its two probes, filled by its states' shares.
    let mut row = Row::new(bar_w);
    let (a, b) = cols(request.born, request.done);
    let inside = [
        request.rx_wait,
        request.states[ON_CPU],
        request.states[RUN_QUEUE],
        request.states[LOCK],
        request.states[DISK],
        request.tx_wait,
    ];
    let mut at = a;
    for (state, cells) in STATES
        .iter()
        .zip(allocate(&inside, inside.iter().sum(), b - a + 1))
    {
        if cells > 0 {
            row.fill(
                at,
                at + cells - 1,
                state.glyph,
                dimmed(state.style(), trust.dims()),
            );
        }
        at += cells;
    }
    let mut line = vec![label("  serving", bold())];
    line.extend(row.spans());
    line.push(note(
        format!(
            "{} from start to done",
            units::duration((request.done - request.born) as f64)
        ),
        trust.style(),
    ));
    lines.push(Line::from(line));

    // Its block requests, each on the device when it was.
    let disk = &STATES[DISK];
    let mut row = Row::new(bar_w);
    for one in blocks {
        let (a, b) = cols(one.start, one.end);
        row.fill(a, b, disk.glyph, dimmed(disk.style(), block_trust.dims()));
    }
    let mut line = vec![label("  on device", disk.style())];
    line.extend(row.spans());
    line.push(if blocks.is_empty() {
        let none = if request.states[DISK] > 0.0 {
            "no block request carries its id"
        } else {
            "no block request"
        };
        if gathered.block_results.is_empty() {
            note("io.latency not in this run".to_owned(), dim())
        } else if let Some(why) = gathered.blocks_unsure(request) {
            note(format!("none seen: {}", why.short()), unknown)
        } else {
            note(none.to_owned(), dim())
        }
    } else {
        note(
            format!(
                "{} block requests, {}",
                blocks.len(),
                units::bytes(blocks.iter().map(|one| one.bytes).sum())
            ),
            block_trust.style(),
        )
    });
    lines.push(Line::from(line));

    // The network out: its end to its last byte leaving; unknown when the
    // response had not left the stack, which is not "gone when done".
    let net_out = &STATES[NET_OUT];
    let mut row = Row::new(bar_w);
    let mut line = vec![label("  net out", net_out.style())];
    let in_send = if request.tx_wait > 0.0 {
        format!(", {} in send", units::duration(request.tx_wait))
    } else {
        String::new()
    };
    match request.drained {
        Some(drained) => {
            if drained > request.done {
                let (a, b) = cols(request.done, drained);
                row.fill(a, b, net_out.glyph, dimmed(net_out.style(), trust.dims()));
            }
            line.extend(row.spans());
            line.push(if request.states[NET_OUT] > 0.0 {
                note(
                    format!("{} after done{in_send}", units::duration(request.tx_drain)),
                    trust.style(),
                )
            } else {
                note("gone when done".to_owned(), dim())
            });
        }
        None => {
            let at = col(request.done);
            row.fill(at, at, '?', unknown);
            line.extend(row.spans());
            line.push(note(format!("unknown after done{in_send}"), unknown));
        }
    }
    lines.push(Line::from(line));
    lines.push(Line::from(Span::styled(
        "  inside `serving` the states are drawn by share: net.request sums them, it does not keep their order",
        dim().add_modifier(Modifier::ITALIC),
    )));
    lines
}

/// Where its time went: each state, its share, the evidence the run's
/// decomposition gives it, and what it names.
fn where_lines(request: &Request, data: &Data, width: u16, trust: &Trust) -> Vec<Line<'static>> {
    let span = request.span();
    let bar_w = usize::from(width)
        .saturating_sub(4 + 11 + 8 + 8 + 1 + 1 + 9 + 20)
        .clamp(3, 20);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!(
                "{:<w$}",
                format!("  where its {} went", request.span_text()),
                w = 4 + 11 + 8 + 8 + 1 + bar_w + 1
            ),
            bold(),
        ),
        Span::styled("evidence", dim()),
    ])];
    for (index, state) in STATES.iter().enumerate() {
        let ns = request.states[index];
        let share = if span > 0.0 { ns / span } else { 0.0 };
        let row_style = if ns > 0.0 { trust.style() } else { dim() };
        let cells = (share * bar_w as f64).round() as usize;
        let known = request.known(index);
        // A share of a span that is only a lower bound would be invented:
        // when a network side is unknown, no state gets a share or a bar.
        let shared = request.whole();
        let bar = if !shared {
            String::new()
        } else if cells == 0 && ns > 0.0 {
            "▏".to_owned()
        } else {
            state.glyph.to_string().repeat(cells)
        };
        let evidence = decomposed(data, &request.id, state.decomposed)
            .and_then(|row| row.get("evidence")?.as_str())
            .unwrap_or("");
        let (duration, percent) = if !known {
            // A lower bound: its share of a lower bound is no share.
            (
                if ns > 0.0 {
                    at_least(ns, false)
                } else {
                    "?".to_owned()
                },
                String::new(),
            )
        } else if ns > 0.0 && shared {
            (units::duration(ns), units::percent(share * 100.0))
        } else if ns > 0.0 {
            (units::duration(ns), String::new())
        } else {
            ("·".to_owned(), String::new())
        };
        let row_style = if known { row_style } else { trust.style() };
        let mut line = vec![
            Span::styled(format!("  {} ", state.glyph), state.style()),
            Span::styled(format!("{:<11}", state.name), row_style),
            Span::styled(format!("{duration:>8}"), row_style),
            Span::styled(format!("{percent:>8} "), row_style),
            Span::styled(
                format!("{bar:<bar_w$} "),
                dimmed(state.style(), trust.dims()),
            ),
            Span::styled(
                format!("{evidence:<9}"),
                if evidence == "IDENTITY" {
                    Style::new()
                } else {
                    dim()
                },
            ),
        ];
        let named = match index {
            NET_IN if !known => Some("unknown before start".to_owned()),
            NET_OUT if !known => Some("unknown after done".to_owned()),
            LOCK if ns > 0.0 => request.holder_tid.map(|tid| format!("woken by tid {tid}")),
            NET_IN | NET_OUT if ns > 0.0 => {
                request.sock.map(|cookie| format!("by socket {cookie}"))
            }
            _ => None,
        };
        if let Some(named) = named {
            line.push(Span::styled(
                named,
                if known {
                    dim()
                } else {
                    Style::new().fg(palette::LOSS)
                },
            ));
        }
        lines.push(Line::from(line));
    }
    lines.push(Line::from(Span::styled(
        if data.run.decompositions.is_empty() {
            "  no decomposition in this run: the evidence is not graded".to_owned()
        } else {
            "  IDENTITY: read off the serving thread  KEY: joined by the socket".to_owned()
        },
        dim(),
    )));
    lines
}

/// What sets a request apart: the states it spent more in than the other
/// requests typically did -- each against the median of the others' same
/// state -- the two furthest above it.
fn against_the_others(request: &Request, all: &[Request], trust: &Trust) -> Option<Line<'static>> {
    let others: Vec<&Request> = all.iter().filter(|other| other.id != request.id).collect();
    if others.is_empty() {
        return None;
    }
    // The median of the others' state, over those whose state is known: a
    // lower bound would pull it down. A request's own unknown side is a lower
    // bound, so what it shows above the median is "at least".
    let median = |index: usize| -> Option<f64> {
        let mut theirs: Vec<f64> = others
            .iter()
            .filter(|other| other.known(index))
            .map(|other| other.states[index])
            .collect();
        if theirs.is_empty() {
            return None;
        }
        theirs.sort_by(f64::total_cmp);
        let half = theirs.len() / 2;
        Some(if theirs.len() % 2 == 0 {
            (theirs[half - 1] + theirs[half]) / 2.0
        } else {
            theirs[half]
        })
    };
    let mut above: Vec<(usize, f64, f64)> = (0..STATES.len())
        .filter_map(|index| {
            let typical = median(index)?;
            Some((index, request.states[index] - typical, typical))
        })
        .filter(|(_, difference, _)| *difference >= request.span() * 0.05)
        .collect();
    above.sort_by(|a, b| b.1.total_cmp(&a.1));
    above.truncate(2);
    let mut line = vec![Span::styled(
        format!(
            "  what sets it apart from the other {} request{}: ",
            others.len(),
            if others.len() == 1 { "" } else { "s" }
        ),
        bold(),
    )];
    if above.is_empty() {
        line.push(Span::styled(
            "nothing, every state is within a twentieth of their median",
            dim(),
        ));
    }
    for (n, (index, difference, typical)) in above.into_iter().enumerate() {
        if n > 0 {
            line.push(Span::styled(" · ", dim()));
        }
        let state = &STATES[index];
        line.push(Span::styled(format!("{} ", state.glyph), state.style()));
        line.push(Span::styled(
            format!(
                "{} +{}",
                state.name,
                at_least(difference, request.known(index))
            ),
            trust.style().fg(palette::WORSE),
        ));
        line.push(Span::styled(
            format!(" (theirs {})", units::duration(typical)),
            dim(),
        ));
    }
    Some(Line::from(line))
}

/// Its share of each resource: the cpu of its process, the device time the
/// run saw, the network, the lock.
fn share_lines(
    request: &Request,
    blocks: &[BlockIo],
    gathered: &Gathered<'_>,
    data: &Data,
    trust: &Trust,
    block_trust: &Trust,
) -> Vec<Line<'static>> {
    let head = |name: &str| Span::styled(format!("{name:<11}"), bold());
    let indent = || Span::raw(" ".repeat(11));
    let mut lines = vec![Line::from(Span::styled(
        "its share of each resource",
        bold(),
    ))];

    // Cpu.
    let on_cpu = request.states[ON_CPU];
    let mut cpu = vec![
        head("cpu"),
        Span::styled(units::duration(on_cpu), trust.style()),
    ];
    if let Some(process) = gathered.process_cpu_ns() {
        let whose = if request.comm.is_empty() {
            format!("pid {}", request.pid)
        } else {
            request.comm.clone()
        };
        cpu.push(Span::styled(
            format!(
                ": {} of the {} {whose} took",
                units::percent(on_cpu / process * 100.0),
                units::duration(process)
            ),
            trust.style(),
        ));
    }
    lines.push(Line::from(cpu));
    lines.push(Line::from(vec![
        indent(),
        Span::styled(
            format!(
                "off cpu {} time{}, preempted {}",
                request.blocks,
                if request.blocks == 1 { "" } else { "s" },
                request.preempted
            ),
            trust.style(),
        ),
    ]));

    // Disk.
    if blocks.is_empty() {
        let unsure = (!gathered.block_results.is_empty())
            .then(|| gathered.blocks_unsure(request))
            .flatten();
        let said = if gathered.block_results.is_empty() {
            "io.latency not in this run: its block requests are unknown".to_owned()
        } else if request.states[DISK] > 0.0 {
            format!(
                "{} waiting, no block request {} its id",
                units::duration(request.states[DISK]),
                if unsure.is_some() {
                    "seen carries"
                } else {
                    "carries"
                }
            )
        } else if unsure.is_some() {
            "no block request seen".to_owned()
        } else {
            "no block request issued".to_owned()
        };
        lines.push(Line::from(vec![
            head("disk"),
            Span::styled(
                said,
                if unsure.is_some() {
                    block_trust.style()
                } else {
                    dim()
                },
            ),
        ]));
        if let Some(why) = unsure {
            let mut line = vec![
                indent(),
                Span::styled(
                    format!("{} ", why.sentence()),
                    Style::new().fg(palette::LOSS),
                ),
            ];
            line.extend(block_trust.badges());
            lines.push(Line::from(line));
        }
    } else {
        let device: f64 = blocks.iter().map(|one| one.latency).sum();
        let mut devs: Vec<&str> = blocks.iter().map(|one| one.dev.as_str()).collect();
        devs.sort_unstable();
        devs.dedup();
        lines.push(Line::from(vec![
            head("disk"),
            Span::styled(
                format!(
                    "{} requests, {} on {}",
                    blocks.len(),
                    units::duration(device),
                    devs.join(", ")
                ),
                block_trust.style(),
            ),
        ]));
        if gathered.device_total > 0.0 {
            lines.push(Line::from(vec![
                indent(),
                Span::styled(
                    format!(
                        "{} of the run's device time",
                        units::percent(device / gathered.device_total * 100.0)
                    ),
                    block_trust.style(),
                ),
            ]));
        }
        if let Some(table) = block_table(data, &request.id) {
            let whole = table
                .get("whole")
                .and_then(|whole| whole.get("amount"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let residue = table.get("residue").and_then(Value::as_f64).unwrap_or(0.0);
            lines.push(Line::from(vec![
                indent(),
                Span::styled(
                    format!(
                        "waited {}, {} off the device",
                        units::duration(whole),
                        units::duration(residue)
                    ),
                    block_trust.style(),
                ),
            ]));
        }
        let badges = block_trust.badges();
        if !badges.is_empty() {
            let mut line = vec![indent()];
            line.extend(badges);
            lines.push(Line::from(line));
        }
    }

    // Network.
    lines.push(Line::from(vec![
        head("network"),
        Span::styled(
            format!(
                "in {} · out {}",
                net_text(request, NET_IN),
                net_text(request, NET_OUT),
            ),
            trust.style(),
        ),
    ]));

    // The lock.
    if request.states[LOCK] > 0.0 {
        let mut line = vec![
            head("lock/sleep"),
            Span::styled(
                format!("longest {}", units::duration(request.holder_ns)),
                trust.style(),
            ),
        ];
        if let Some(tid) = request.holder_tid {
            line.push(Span::styled(format!(", woken by tid {tid}"), trust.style()));
        }
        lines.push(Line::from(line));
    }
    lines
}

/// A network side's time, a lower bound or `?` when `net.request` could not
/// close it.
fn net_text(request: &Request, state: usize) -> String {
    let ns = request.states[state];
    match (request.known(state), ns > 0.0) {
        (true, _) => units::duration(ns),
        (false, true) => at_least(ns, false),
        (false, false) => "?".to_owned(),
    }
}

/// No request to draw: the source's refusal in its own words, or what the
/// run lacks, and how to get requests.
fn draw_none(frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>, gathered: &Gathered<'_>) {
    let refusal = ctx
        .data
        .refused
        .iter()
        .find(|refusal| refusal.label == NEEDS[0].label())
        .or_else(|| ctx.data.refusal(NEEDS[0].source));
    let [top, _, bottom] = Layout::vertical([
        Constraint::Length(if refusal.is_some() { 7 } else { 1 }),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);
    match refusal {
        Some(refusal) => trust::render_refusal(frame, top, refusal),
        None => {
            let said = if gathered.results.is_empty() {
                "This run did not ask for net.request: there is no request to draw.".to_owned()
            } else if ctx.focus.is_empty() {
                "net.request was served, and no request ended in this run.".to_owned()
            } else {
                format!("No request of {} in this run.", ctx.focus.describe())
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(format!(" {said}"), bold())))
                    .wrap(Wrap { trim: true }),
                top,
            );
        }
    }
    let mut lines = vec![
        Line::from(Span::styled(" How to see what one request costs", bold())),
        Line::from(""),
    ];
    for text in [
        " A request is what the program says it is: net.request follows it from a",
        " request__start(id) probe to a request__done(id) probe the program fires itself,",
        " and nothing is inferred from a connection instead. A server that fires them:",
        "",
        "     omn top -f examples/vertical-one-request.yml",
        "",
        " Each request is then drawn as its life by state -- the network on either side,",
        " on cpu, run queue, a lock, the disk -- with the block requests it issued, its",
        " share of its process's cpu and of the run's device time.",
    ] {
        lines.push(Line::from(Span::styled(text, dim())));
    }
    frame.render_widget(Paragraph::new(lines), bottom);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot;
    use ratatui::crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};

    /// The vertical case injects one cause per request; the state that made
    /// most of each must be that cause, and by a wide margin.
    #[test]
    fn each_request_is_mostly_its_injected_cause() {
        let data = snapshot::fixture("vertical-requests");
        let requests = Gathered::of(&data).requests;
        let ids: Vec<&str> = requests.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["a001", "a002", "a003", "a004", "a005"]);
        let injected = [
            LOCK,    // 40 ms on a lock another thread sleeps under
            DISK,    // 64 pages read with O_DIRECT
            LOCK,    // 20 ms on the lock, then 32 pages
            NET_IN,  // the client pauses 30 ms half way through its request
            NET_OUT, // the client reads its answer through a 4 KiB buffer
        ];
        for (request, cause) in requests.iter().zip(injected) {
            let mostly = request.mostly();
            assert_eq!(
                mostly[0].0, cause,
                "{}: mostly {mostly:?}, injected {}",
                request.id, STATES[cause].name
            );
            assert!(mostly[0].1 > 0.5, "{}: {mostly:?}", request.id);
        }
        // a003 is both: the disk is its second state.
        assert_eq!(requests[2].mostly()[1].0, DISK);
        // The six states tile each span exactly: against `net.request`'s
        // own whole and against the wall clock from first byte to last.
        for request in &requests {
            let parts: f64 = request.states.iter().sum();
            let span_ns = request.span_ns.expect("a whole request carries span_ns");
            let wall =
                (request.drained.expect("drained") - request.first_byte.expect("first")) as f64;
            assert!(
                (parts - span_ns).abs() < 1.0,
                "{}: {parts} against {span_ns}",
                request.id
            );
            assert!(
                (parts - wall).abs() < 1.0,
                "{}: {parts} against {wall}",
                request.id
            );
            assert!(request.whole() && request.span_text() == units::duration(span_ns));
        }
    }

    #[test]
    fn block_requests_go_to_the_request_whose_id_they_carry() {
        let data = snapshot::fixture("vertical-requests");
        let gathered = Gathered::of(&data);
        let count = |id: &str| gathered.blocks.get(id).map(Vec::len).unwrap_or(0);
        // The session injected 64 and 32 pages; the collector said 96 of
        // its block requests carried an id.
        assert_eq!(count("a002"), 64);
        assert_eq!(count("a003"), 32);
        assert_eq!(count("a001") + count("a004") + count("a005"), 0);
        // Each was on the device while its request was served.
        for request in &gathered.requests {
            for one in gathered.blocks.get(&request.id).into_iter().flatten() {
                assert!(
                    one.start >= request.born && one.end <= request.done,
                    "{}: block request [{}, {}] outside [{}, {}]",
                    request.id,
                    one.start,
                    one.end,
                    request.born,
                    request.done
                );
            }
        }
        // Their device time is the decomposition's: the view and the join
        // engine read the same events, and agree.
        let device: f64 = gathered.blocks["a002"].iter().map(|one| one.latency).sum();
        let table = block_table(&data, "a002").expect("a block-io table for a002");
        assert_eq!(Some(device), table["parts"].as_f64());
    }

    #[test]
    fn cells_add_up_and_never_overdraw() {
        assert_eq!(
            allocate(&[1.0, 1.0, 1.0], 3.0, 10).iter().sum::<usize>(),
            10
        );
        assert_eq!(allocate(&[5.0, 0.0], 10.0, 10), [5, 0]);
        assert_eq!(allocate(&[0.0, 0.0], 0.0, 10), [0, 0]);
        // A state far below a cell gets none; the whole still rounds right.
        let cells = allocate(&[99.0, 0.1, 0.9], 100.0, 50);
        assert_eq!(cells.iter().sum::<usize>(), 50);
        assert_eq!(cells[1], 0);
        // Scaled to a longer whole, a request takes its share of the width.
        assert_eq!(allocate(&[10.0], 40.0, 80), [20]);
    }

    #[test]
    fn ticks_are_round() {
        assert_eq!(tick_step(40e6, 6), 1e7);
        assert_eq!(tick_step(33e6, 6), 1e7);
        assert_eq!(tick_step(10e6, 6), 2e6);
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn the_arrows_select_and_enter_focuses_every_view_on_it() {
        let data = snapshot::fixture("vertical-requests");
        let focus = Focus::default();
        let ctx = Ctx {
            data: &data,
            against: None,
            focus: &focus,
        };
        let mut view = RequestView::default();
        let gathered = Gathered::of(&data);
        let shown = view.visible(&gathered, &focus);
        assert_eq!(view.settle(&focus, &shown), 0);
        view.key(press(KeyCode::Down), &ctx);
        view.key(press(KeyCode::Down), &ctx);
        assert_eq!(view.selected.as_deref(), Some("a003"));
        assert_eq!(
            view.key(press(KeyCode::Enter), &ctx),
            Action::Focus(Focus {
                request: Some("a003".into()),
                ..Focus::default()
            })
        );
        view.key(press(KeyCode::End), &ctx);
        assert_eq!(view.selected.as_deref(), Some("a005"));
        view.key(press(KeyCode::Home), &ctx);
        assert_eq!(view.selected.as_deref(), Some("a001"));
        // Slowest first keeps the selection, and puts a001 on top.
        view.key(press(KeyCode::Down), &ctx);
        view.key(press(KeyCode::Char('s')), &ctx);
        let shown = view.visible(&gathered, &focus);
        assert_eq!(shown[0].id, "a001");
        assert_eq!(shown[view.settle(&focus, &shown)].id, "a002");
        assert!(matches!(
            view.key(press(KeyCode::Char('l')), &ctx),
            Action::Goto { view: "lanes", .. }
        ));
    }

    #[test]
    fn a_focused_request_is_the_one_drawn() {
        let mut app = crate::app::App::new(snapshot::fixture("vertical-requests"));
        app.cost = crate::app::Cost::Snapshot;
        app.open("request").expect("registered");
        app.focus = Focus {
            request: Some("a005".into()),
            ..Focus::default()
        };
        let text = snapshot::text(&snapshot::draw_app(&mut app, 120, 40));
        assert!(text.contains("▶ a005"), "{text}");
        assert!(text.contains("── a005 ·"), "{text}");
    }

    /// Each request of the vertical case, selected in turn with the arrows,
    /// held to its own snapshot: the five injected causes are the five
    /// screens the view is judged on.
    #[test]
    fn each_vertical_request_has_its_snapshot() {
        let mut app = crate::app::App::new(snapshot::fixture("vertical-requests"));
        app.cost = crate::app::Cost::Snapshot;
        app.open("request").expect("registered");
        for id in ["a001", "a002", "a003", "a004", "a005"] {
            let buffer = snapshot::draw_app(&mut app, snapshot::WIDTH, snapshot::HEIGHT);
            assert!(
                snapshot::text(&buffer).contains(&format!("── {id} ·")),
                "{id} is not the one drawn"
            );
            snapshot::assert_snapshot(&format!("request--vertical-requests--{id}"), &buffer);
            app.key(press(KeyCode::Down));
        }
    }

    /// What sets each request apart is its injected cause, said in words.
    #[test]
    fn what_sets_each_request_apart_is_its_cause() {
        let data = snapshot::fixture("vertical-requests");
        let all = Gathered::of(&data).requests;
        let trust = Trust::clean();
        let said = |id: &str| -> String {
            let request = all.iter().find(|r| r.id == id).expect("in the run");
            against_the_others(request, &all, &trust)
                .expect("four others")
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        // Each cause said with its size, within what the plan injected
        // (`examples/vertical-one-request.yml`): the bounds are the plan's,
        // not one recording's numbers.
        let check = |id: &str, mark: &str, cause: &str, low_ms: f64, high_ms: f64| {
            let text = said(id);
            let ms = added_ms(&text, &format!("{mark} {cause}"))
                .unwrap_or_else(|| panic!("{id}: no `{mark} {cause} +…` in {text}"));
            assert!(
                (low_ms..=high_ms).contains(&ms),
                "{id}: {cause} +{ms}ms, the plan says {low_ms} to {high_ms}: {text}"
            );
        };
        check("a001", "▀", "lock/sleep", 40.0, 48.0);
        check("a002", "▄", "disk", 0.5, 50.0);
        check("a003", "▀", "lock/sleep", 20.0, 28.0);
        check("a003", "▄", "disk", 0.2, 50.0);
        check("a004", "░", "net in", 30.0, 38.0);
        check("a005", "░", "net out", 25.0, 60.0);
        // The one cause is the first said, ahead of what differs by less.
        assert!(said("a002").contains("from the other 4 requests: ▄ disk +"));
    }

    /// What follows `<cause> +` in a sentence, in milliseconds: `40.1ms`,
    /// `547µs`, a lower bound `≥28.6ms`.
    fn added_ms(text: &str, cause: &str) -> Option<f64> {
        let rest = &text[text.find(&format!("{cause} +"))? + cause.len() + 2..];
        let figure: String = rest
            .trim_start_matches('≥')
            .chars()
            .take_while(|c| !c.is_whitespace())
            .collect();
        let (number, scale) = if let Some(n) = figure.strip_suffix("ms") {
            (n, 1.0)
        } else if let Some(n) = figure.strip_suffix("µs") {
            (n, 1e-3)
        } else if let Some(n) = figure.strip_suffix("ns") {
            (n, 1e-6)
        } else {
            (figure.strip_suffix('s')?, 1e3)
        };
        number.parse::<f64>().ok().map(|n| n * scale)
    }

    /// Half way through a live run, the requests that ended are drawn, as
    /// provisional; the rest come as they end.
    #[test]
    fn a_live_run_draws_the_requests_ended_so_far() {
        let data = snapshot::fixture("vertical-requests");
        let a003 = Gathered::of(&data).requests[2].done;
        let live = crate::feed::as_live_until(&data, a003 + 1);
        let gathered = Gathered::of(&live);
        let ids: Vec<&str> = gathered.requests.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["a001", "a002", "a003"]);
        let text = snapshot::text(&snapshot::draw(live.clone(), "request", 120, 40));
        assert!(text.contains("3 requests served by server"), "{text}");
        // The request figures themselves carry `live`, not only the header.
        let trust = Trust::joined(&live, gathered.results.iter().copied());
        assert!(trust.marks.contains(&crate::trust::Mark::Provisional));
        let summary: String = summary_line(&gathered.requests, &trust, Order::Arrival)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(summary.contains("  live"), "{summary}");
    }

    /// The vertical run, rewritten into the shape of
    /// `examples/vertical-abandoned.yml`'s loss path: a005's response never
    /// left the stack, so its event has no `net_tx`, and the collector's
    /// notes count a request that never ended, replaced on its thread, and a
    /// `request__done` with no birth.
    fn abandoned() -> Data {
        let mut data = snapshot::fixture("vertical-requests");
        let results = data
            .run
            .results
            .iter_mut()
            .find(|results| results.source == "net.request")
            .expect("net.request");
        // What the sentences below say, and nothing published as a number:
        // the recording's own counts, all zero, would win over them.
        results.fields.clear();
        for output in &mut results.outputs {
            if let crate::model::Output::Raw { events } = output {
                for event in events
                    .iter_mut()
                    .filter(|e| req_id(e).as_deref() == Some("a005"))
                {
                    event.attrs.retain(|attr| {
                        !["drained_ns", "tx_drain_ns", "net_tx_ns", "span_ns"]
                            .contains(&attr.name.as_str())
                    });
                }
            }
        }
        results.degradations.extend([
            "1 request never ended in the run (req:1 in task 7); a request with no end has no \
             total and is in no event"
                .to_owned(),
            "1 `request__start` hit a task still carrying a request that never ended; the old \
             one was replaced and is among those that never ended"
                .to_owned(),
            "2 `request__done` hits a task carrying no request: begun before the probes \
             attached, or never declared at `request__start`"
                .to_owned(),
            "1 response had not left the stack when the run ended, so its request has no \
             `net_tx` rather than a short one"
                .to_owned(),
        ]);
        data
    }

    /// A response that never left the stack is drawn as unknown after done,
    /// never as drained at `request__done`.
    #[test]
    fn an_undrained_response_is_unknown_not_gone_when_done() {
        let data = abandoned();
        let gathered = Gathered::of(&data);
        let a005 = &gathered.requests[4];
        assert_eq!(a005.drained, None);
        assert!(!a005.known(NET_OUT) && !a005.whole());
        // Only the part inside the service is known: the wait in `send`.
        assert_eq!(a005.states[NET_OUT], a005.tx_wait);
        assert!(a005.span_text().starts_with('≥'), "{}", a005.span_text());

        let mut app = crate::app::App::new(data);
        app.cost = crate::app::Cost::Snapshot;
        app.open("request").expect("registered");
        app.focus = Focus {
            request: Some("a005".into()),
            ..Focus::default()
        };
        let text = snapshot::text(&snapshot::draw_app(&mut app, 120, 40));
        assert!(
            text.contains("from its first byte in to request__done"),
            "{text}"
        );
        assert!(text.contains("unknown after done"), "{text}");
        assert!(!text.contains("gone when done"), "{text}");
        // No drain is timed after `done`: no `<duration> after done`.
        assert!(!text.contains("s after done"), "{text}");
        // The send wait, the one part known, as a lower bound.
        let out = at_least(a005.tx_wait, false);
        assert!(out.starts_with('≥'), "{out}");
        assert!(text.contains(&format!("out {out}")), "{text}");
        assert!(text.contains("1 ≥: a network side unknown"), "{text}");
        // The summary keeps its badges in 120 columns.
        assert!(text.contains("arrival  degraded x6"), "{text}");
        assert!(text.contains(&format!("net out {out}")), "{text}");
        // The shares, of a span that is a lower bound, are not given.
        assert!(!text.contains("net out 98"), "{text}");
    }

    /// What `net.request` counted and could put in no event is said under the
    /// summary, by count, with the ids it named.
    #[test]
    fn requests_in_no_event_are_counted_on_screen() {
        let undrawn = Gathered::of(&abandoned()).undrawn;
        assert_eq!(
            undrawn,
            Undrawn {
                never_ended: 1,
                named: "req:1 in task 7".into(),
                replaced: 1,
                unborn: 2,
                filtered: 0,
            }
        );
        let text = snapshot::text(&snapshot::draw(abandoned(), "request", 160, 40));
        assert!(
            text.contains(
                "in no event, not drawn: 1 never ended (req:1 in task 7, 1 replaced by a later \
                 start) · 2 ended with no birth"
            ),
            "{text}"
        );
        // The recorded run lost nothing of the kind, and says nothing.
        assert!(Gathered::of(&snapshot::fixture("vertical-requests"))
            .undrawn
            .is_empty());
    }

    /// The published fields are read before the sentences: a count the
    /// collector gave as a number wins, and the names stay the notes'.
    #[test]
    fn requests_in_no_event_are_read_off_the_published_fields() {
        let mut data = abandoned();
        for results in &mut data.run.results {
            if results.source == "net.request" {
                results.fields.insert("never_ended".into(), 4.into());
                results.fields.insert("unborn".into(), 0.into());
                results.fields.insert("filtered_out".into(), 3.into());
            }
        }
        let undrawn = Gathered::of(&data).undrawn;
        assert_eq!(undrawn.never_ended, 4);
        assert_eq!(undrawn.unborn, 0);
        assert_eq!(undrawn.filtered, 3);
        assert_eq!(undrawn.replaced, 1, "not published: the note's count");
        assert_eq!(undrawn.named, "req:1 in task 7");
    }

    /// With `io.latency` losses, a request with no block request is "none
    /// seen", with the loss beside it, never "none issued".
    #[test]
    fn no_block_request_under_losses_is_none_seen_with_the_loss() {
        let mut data = snapshot::fixture("vertical-requests");
        for results in data
            .run
            .results
            .iter_mut()
            .filter(|r| r.source == "io.latency")
        {
            results.losses.dropped = 3;
        }
        let gathered = Gathered::of(&data);
        assert_eq!(
            gathered.blocks_unsure(&gathered.requests[0]),
            Some(Unsure::Lost(3))
        );
        let text = snapshot::text(&snapshot::draw(data, "request", 120, 40));
        assert!(text.contains("no block request seen"), "{text}");
        assert!(text.contains("none seen: io.latency lost 3"), "{text}");
        assert!(text.contains("io.latency lost 3, so some may"), "{text}");
        assert!(text.contains("-3 lost"), "{text}");
        assert!(!text.contains("no block request issued"), "{text}");
        // Lost nothing, watched the whole run: a zero, said as one.
        let clean = snapshot::fixture("vertical-requests");
        let gathered = Gathered::of(&clean);
        assert_eq!(gathered.blocks_unsure(&gathered.requests[0]), None);
    }

    /// `io.latency` held under two labels is one set of block requests.
    #[test]
    fn io_latency_under_two_labels_is_counted_once() {
        let mut data = snapshot::fixture("vertical-requests");
        let once = Gathered::of(&data).device_total;
        let mut twin = data.first("io.latency").expect("io.latency").clone();
        twin.label = "system".into();
        data.run.results.push(twin);
        let gathered = Gathered::of(&data);
        assert_eq!(gathered.blocks["a002"].len(), 64);
        assert_eq!(gathered.device_total, once);
    }

    /// A focus on a request the list does not hold says so, and which one is
    /// drawn instead.
    #[test]
    fn a_focus_the_list_does_not_hold_is_said() {
        let mut app = crate::app::App::new(snapshot::fixture("vertical-requests"));
        app.cost = crate::app::Cost::Snapshot;
        app.open("request").expect("registered");
        app.focus = Focus {
            request: Some("zz9".into()),
            ..Focus::default()
        };
        let text = snapshot::text(&snapshot::draw_app(&mut app, 120, 40));
        assert!(
            text.contains("the focus names zz9, in no event of this run: a001 is drawn instead"),
            "{text}"
        );
        let all = Gathered::of(&snapshot::fixture("vertical-requests")).requests;
        let hidden = Focus {
            pid: Some(1),
            request: Some("a003".into()),
            ..Focus::default()
        };
        let notice: String = notice_line(
            &hidden,
            &all[..1],
            &all[0],
            &Gathered::of(&snapshot::fixture("vertical-requests")),
        )
        .expect("a notice")
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
        assert!(
            notice.contains("a003, hidden by the pid/tid focus"),
            "{notice}"
        );
    }

    /// On a narrow terminal the share column grows rather than cut its last
    /// line, the lock's waker.
    #[test]
    fn a_narrow_share_column_keeps_its_last_line() {
        let mut app = crate::app::App::new(snapshot::fixture("vertical-requests"));
        app.cost = crate::app::Cost::Snapshot;
        app.open("request").expect("registered");
        app.focus = Focus {
            request: Some("a003".into()),
            ..Focus::default()
        };
        let text = snapshot::text(&snapshot::draw_app(&mut app, 100, 50));
        assert!(text.contains("longest"), "{text}");
        assert!(text.contains("what sets it apart"), "{text}");
    }

    #[test]
    fn a_refused_source_draws_its_reason_and_the_way_to_requests() {
        let text = snapshot::text(&snapshot::draw(
            snapshot::fixture("http-server"),
            "request",
            120,
            40,
        ));
        assert!(text.contains("net.request refused"), "{text}");
        assert!(text.contains("declares no request"), "{text}");
        assert!(text.contains("examples/vertical-one-request.yml"), "{text}");
    }

    #[test]
    fn small_terminals_draw_without_panicking() {
        for (width, height) in [(40, 12), (60, 20), (80, 24), (200, 60), (22, 6), (120, 10)] {
            snapshot::draw(
                snapshot::fixture("vertical-requests"),
                "request",
                width,
                height,
            );
        }
    }
}
