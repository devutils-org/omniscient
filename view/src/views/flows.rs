//! The `flows` view: where the bytes went, as flows across the layers they
//! crossed.
//!
//! Two bands, each a Sankey diagram drawn in half-block cells, widths in
//! proportion to bytes:
//!
//! - **network**: thread -> socket -> the network stack -> interface;
//! - **storage**: thread -> file -> the page cache -> device.
//!
//! The first two columns are the target's, measured per call row: a ribbon
//! from a thread to a socket or a file is the bytes that thread sent or
//! received, wrote or read, through it (`net.socket`, and `io.fs` or, failing
//! it, `io.syscall` -- each asked as `raw`). The last column is the
//! **machine's**: an interface's or a device's bytes over the run, everybody's
//! (`net.bytes`, `io.bytes`). Between them stands the kernel layer that
//! decouples the two, drawn as a junction rather than as edges, because no
//! source ties one socket to one interface or one file's bytes to one device:
//! what reaches the junction from the left and what leaves it to the right are
//! two different measurements, and the gap between their heights is the
//! finding -- reads the page cache served never reach a device, a write
//! reaches it later and larger, loopback crosses its interface twice.
//!
//! Everything is drawn in the trust style: a ribbon and its figure are dimmed
//! when their result must not be read at face value, the band's title carries
//! each source's badges, and a source that was refused, or that this run did
//! not record in a form that carries bytes, says so where its column would be.

use std::collections::{BTreeMap, BTreeSet};

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::{Action, Ctx, Focus, Need, Region, Scope, View};
use crate::model::{Data, Event, Key, Output, Results};
use crate::trust::{self, palette, Trust};
use crate::units;

/// What this view needs measured.
pub const NEEDS: &[Need] = &[
    Need {
        source: "net.socket",
        mode: "trace",
        emit: &["raw", "hist"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "bytes per socket",
    },
    Need {
        source: "net.bytes",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "bytes per interface",
    },
    Need {
        source: "io.syscall",
        mode: "trace",
        emit: &["raw", "summary", "hist"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "bytes per descriptor, from its rows: a file, a pipe or a socket",
    },
    Need {
        source: "io.bytes",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "bytes per device",
    },
];

/// Bytes into the target: received, read.
const INWARD: Color = Color::Cyan;
/// Bytes out of the target: sent, written.
const OUTWARD: Color = Color::Green;
/// The machine's bytes, whoever moved them.
const MACHINE: Color = Color::DarkGray;
/// The bars a column's nodes are drawn as.
const BAR: Color = Color::Gray;

/// Cells of a sparkline, lowest first.
const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Which way a byte went, seen from the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Dir {
    Out,
    In,
}

impl Dir {
    fn colour(self) -> Color {
        match self {
            Dir::Out => OUTWARD,
            Dir::In => INWARD,
        }
    }
}

/// Which of the two bands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Network,
    Storage,
}

impl Kind {
    fn junction(self) -> &'static str {
        match self {
            Kind::Network => "stack",
            Kind::Storage => "cache",
        }
    }
}

/// One thread, as the left column draws it.
#[derive(Debug, Clone, Default)]
struct Task {
    tid: i32,
    pid: i32,
    comm: String,
    bytes: u64,
}

/// One socket, file or descriptor, as the middle column draws it.
#[derive(Debug, Clone, Default)]
struct Object {
    /// What it is called on screen, in a dozen cells.
    name: String,
    /// What it is, in full, for the detail line.
    detail: String,
    out_bytes: u64,
    in_bytes: u64,
    out_calls: u64,
    in_calls: u64,
    /// Time inside the calls, in nanoseconds, summed.
    total_ns: u64,
    errors: u64,
    /// Bytes per bin of the run, for its sparkline.
    spark: Vec<f64>,
    /// How many objects it stands for: more than one for the "others" row.
    many: usize,
    pid: Option<i32>,
    tid: Option<i32>,
    /// Which of the band's results its rows came from: its trust is all of
    /// theirs, not the first's.
    from: BTreeSet<usize>,
    trust: Trust,
}

impl Object {
    fn bytes(&self) -> u64 {
        self.out_bytes + self.in_bytes
    }
}

/// Bytes from one thread through one object, one way.
#[derive(Debug, Clone)]
struct Edge {
    task: usize,
    object: usize,
    dir: Dir,
    bytes: u64,
}

/// One interface or device, as the right column draws it.
#[derive(Debug, Clone, Default)]
struct Sink {
    name: String,
    /// Its key in the machine's source (`dev:259+0`, `iface:eth0`), which a
    /// file's `blk_devs` names.
    key: String,
    bytes: f64,
    spark: Vec<f64>,
    /// The target's files on it, from `io.fs`'s `blk_devs`, and the bytes
    /// they moved through the page cache: the file -> device edge; for an
    /// interface, the target's sockets whose route left by it, from
    /// `net.socket`'s `iface`, and their bytes: the socket -> interface edge.
    fed: Option<(usize, u64)>,
}

/// One band, ready to draw.
#[derive(Debug, Clone)]
struct Band {
    kind: Kind,
    tasks: Vec<Task>,
    objects: Vec<Object>,
    edges: Vec<Edge>,
    /// Why the target's columns are empty, when they are.
    target_note: Option<Vec<Line<'static>>>,
    sinks: Vec<Sink>,
    /// Interfaces or devices that moved nothing in the run.
    idle: usize,
    /// Why the machine's column is empty, when it is.
    machine_note: Option<String>,
    /// The target's source: its name, backend and badges, for the title.
    title: Vec<Span<'static>>,
    /// The machine's source, the same.
    machine_title: Vec<Span<'static>>,
    target_trust: Trust,
    machine_trust: Trust,
    /// The window the target's results cover, in seconds, for the rates.
    seconds: f64,
    /// Whether the target's rows were in the run at all: an empty middle
    /// column means "nothing moved" only when they were.
    measured: bool,
    /// What the middle column holds, one and many: a socket, a file, or a
    /// descriptor when only `io.syscall`'s rows are there.
    object_word: &'static str,
    object_words: &'static str,
    /// Whether the middle column and the machine's measure the same kind of
    /// bytes, so that their ratio means something: not for descriptors, which
    /// include sockets and pipes that never reach a device.
    comparable: bool,
}

impl Band {
    fn out_bytes(&self) -> u64 {
        self.objects.iter().map(|object| object.out_bytes).sum()
    }

    fn in_bytes(&self) -> u64 {
        self.objects.iter().map(|object| object.in_bytes).sum()
    }

    fn machine_bytes(&self) -> f64 {
        self.sinks
            .iter()
            .map(|sink| sink.bytes)
            .fold(0.0, |sum, bytes| sum + bytes)
    }
}

/// The view's state: which object is selected, counted over both bands.
#[derive(Debug, Default)]
pub struct Flows {
    selected: usize,
    /// The objects as last drawn, over both bands.
    regions: Vec<Region>,
}

impl View for Flows {
    fn name(&self) -> &'static str {
        "flows"
    }

    fn title(&self) -> &'static str {
        "where the bytes went: process to socket to interface, process to file to device"
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        self.regions.clear();
        let bands = bands(ctx);
        let count = selectable(&bands);
        if count > 0 && self.selected >= count {
            self.selected = count - 1;
        }
        let block = Block::new().borders(Borders::ALL).title(format!(
            " {} -- {} ",
            self.name(),
            self.title()
        ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < MIN_INNER.1 || inner.width < MIN_INNER.0 {
            frame.render_widget(
                Paragraph::new(format!(
                    "too small for the flows: the view needs {}x{} cells, border included, \
                     and has {}x{}",
                    MIN_INNER.0 + 2,
                    MIN_INNER.1 + 2,
                    area.width,
                    area.height
                ))
                .wrap(Wrap { trim: true }),
                inner,
            );
            return;
        }
        let band_rows = inner.height.saturating_sub(3) / 2;
        let [summary, network, storage, detail] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(band_rows),
            Constraint::Length(band_rows),
            Constraint::Min(2),
        ])
        .areas(inner);
        frame.render_widget(Paragraph::new(summary_line(ctx, &bands)), summary);
        let selected = (count > 0).then_some(self.selected);
        let first = bands[0].objects.len();
        self.regions = draw_band(
            frame,
            network,
            &bands[0],
            selected.filter(|&at| at < first),
            0,
        );
        self.regions.extend(draw_band(
            frame,
            storage,
            &bands[1],
            selected.and_then(|at| at.checked_sub(first)),
            first,
        ));
        frame.render_widget(
            Paragraph::new(detail_lines(&bands, selected)).wrap(Wrap { trim: true }),
            detail,
        );
    }

    fn key(&mut self, key: KeyEvent, ctx: &Ctx<'_>) -> Action {
        let bands = bands(ctx);
        let count = selectable(&bands);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') if count > 0 => {
                self.selected = (self.selected + 1).min(count - 1);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                Action::None
            }
            KeyCode::Enter => match selected_object(&bands, self.selected) {
                Some(object) if object.pid.is_some() || object.tid.is_some() => {
                    Action::Focus(Focus {
                        pid: object.pid,
                        tid: object.tid,
                        request: None,
                    })
                }
                _ => Action::None,
            },
            _ => Action::None,
        }
    }

    fn keys(&self) -> &'static [(&'static str, &'static str)] {
        &[("↑/↓", "socket or file"), ("enter", "focus its thread")]
    }

    fn regions(&self) -> &[Region] {
        &self.regions
    }

    fn select(&mut self, id: usize, _ctx: &Ctx<'_>) -> Action {
        self.selected = id;
        Action::None
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(Flows::default())
}

/// How many objects can be selected, over both bands.
fn selectable(bands: &[Band; 2]) -> usize {
    bands[0].objects.len() + bands[1].objects.len()
}

fn selected_object(bands: &[Band; 2], at: usize) -> Option<&Object> {
    let first = bands[0].objects.len();
    if at < first {
        bands[0].objects.get(at)
    } else {
        bands[1].objects.get(at - first)
    }
}

// ---------------------------------------------------------------------------
// The model: what each band holds, read from the run.
// ---------------------------------------------------------------------------

/// The least room inside the view's border the flows are drawn in: a summary
/// line, two bands of a title, a heading, two rows of diagram and a footer
/// each, and two detail lines. Below it the view says so rather than draw
/// bands that hold only their titles.
const MIN_INNER: (u16, u16) = (60, 1 + 2 * BAND_LEAST + 2);
/// The least rows of a band: its title, heading, footer and two of diagram.
const BAND_LEAST: u16 = 5;

/// The most objects or threads a column draws; the rest are one row.
const MOST: usize = 6;
/// The most interfaces or devices the machine's column draws.
const MOST_SINKS: usize = 5;
/// Bins of a sparkline.
const BINS: usize = 8;

fn bands(ctx: &Ctx<'_>) -> [Band; 2] {
    [network(ctx), storage(ctx)]
}

/// The results of a source that carry `raw` rows, and those that do not.
fn with_rows<'a>(data: &'a Data, source: &'a str) -> (Vec<&'a Results>, Vec<&'a Results>) {
    data.of_source(source)
        .partition(|results| results.output("raw").is_some())
}

/// What an object is, read off one row: its key, its name on screen, its
/// description, and which way the row's bytes went.
type Identity = (String, String, String, Dir);

fn network(ctx: &Ctx<'_>) -> Band {
    let data = ctx.data;
    let mut band = empty_band(Kind::Network);
    let (raw, other) = with_rows(data, "net.socket");
    if !raw.is_empty() {
        fill_target(&mut band, ctx, &raw, |event| {
            let cookie = event.keys.iter().find_map(|key| match key {
                Key::Sock { cookie } => Some(*cookie),
                _ => None,
            })?;
            let family = event.number("family").map(|family| family as u32);
            let port = event
                .number("dport")
                .map(|port| port as u32)
                .filter(|port| *port > 0 && family != Some(1));
            let family_name = match family {
                Some(1) => "unix",
                Some(2) => "inet",
                Some(10) => "inet6",
                _ => "sock",
            };
            let name = match port {
                Some(port) => format!("{family_name} :{port}"),
                None => format!("{family_name} #{cookie}"),
            };
            let detail = format!(
                "socket {cookie} ({family_name}{})",
                port.map(|port| format!(", peer port {port}"))
                    .unwrap_or_default()
            );
            let dir = match event.attr("op").and_then(|op| op.as_str()) {
                Some("send") => Dir::Out,
                _ => Dir::In,
            };
            Some((format!("sock:{cookie}"), name, detail, dir))
        });
        band.title = title(data, &raw, "net.socket");
    } else {
        band.target_note = Some(missing_note(
            data,
            "net.socket",
            &other,
            "bytes per socket",
            "net.socket is in this run without its `raw` rows, which are what carry each \
             socket's bytes",
        ));
        band.title = title(data, &other, "net.socket");
    }
    fill_machine(&mut band, data, "net.bytes", "iface:");
    route(&mut band, ctx, &raw);
    band
}

/// The socket -> interface edge: each socket row's interface key (the
/// interface its cached route left by at the call, `net.socket`'s `iface`)
/// names the interface the machine's column measured, as a file's
/// `blk_devs` names its device. A row with no route carries none, and ties
/// nothing.
/// The series key an interface was written under, as the recorder spelled
/// it: with `@netns:` when the interface was not in the recorder's own
/// namespace, bare when it was. Which of the two the recording holds says
/// which, wherever the recording is drawn -- the viewer's own namespace,
/// which `Key`'s spelling is relative to, is no part of it.
fn sink_key(sinks: &[Sink], netns: u64, name: &str) -> String {
    let qualified = format!("iface:{name}@netns:{netns}");
    if netns == 0 || !sinks.iter().any(|sink| sink.key == qualified) {
        format!("iface:{name}")
    } else {
        qualified
    }
}

fn route(band: &mut Band, ctx: &Ctx<'_>, sockets: &[&Results]) {
    let mut fed: BTreeMap<String, (BTreeSet<u64>, u64)> = BTreeMap::new();
    for results in sockets {
        for event in results.events() {
            let tid = event
                .tid()
                .or_else(|| event.number("tid").map(|tid| tid as i32))
                .unwrap_or(0);
            let pid = event
                .pid()
                .or_else(|| event.number("pid").map(|pid| pid as i32))
                .unwrap_or(0);
            if ctx.focus.pid.is_some_and(|focus| focus != pid)
                || ctx.focus.tid.is_some_and(|focus| focus != tid)
            {
                continue;
            }
            let Some(cookie) = event.keys.iter().find_map(|key| match key {
                Key::Sock { cookie } => Some(*cookie),
                _ => None,
            }) else {
                continue;
            };
            // The key names the interface as the machine's source keys it,
            // its namespace included; the attribute is the bare name.
            let Some(iface) = event.keys.iter().find_map(|key| match key {
                Key::Iface { netns, name, .. } => Some(sink_key(&band.sinks, *netns, name)),
                _ => None,
            }) else {
                continue;
            };
            let bytes = event.number("bytes").unwrap_or(0.0).max(0.0) as u64;
            let entry = fed.entry(iface).or_default();
            entry.0.insert(cookie);
            entry.1 += bytes;
        }
    }
    for sink in &mut band.sinks {
        if let Some((sockets, bytes)) = fed.get(&sink.key) {
            sink.fed = Some((sockets.len(), *bytes));
        }
    }
}

fn storage(ctx: &Ctx<'_>) -> Band {
    let data = ctx.data;
    let mut band = empty_band(Kind::Storage);
    let (files, _) = with_rows(data, "io.fs");
    let (descriptors, syscalls) = with_rows(data, "io.syscall");
    let dir_of = |event: &Event| match event.attr("op").and_then(|op| op.as_str()) {
        Some("write") => Dir::Out,
        _ => Dir::In,
    };
    if !files.is_empty() {
        fill_target(&mut band, ctx, &files, |event| {
            let (dev, ino) = event.keys.iter().find_map(|key| match key {
                Key::File { dev, ino } => Some((*dev, *ino)),
                _ => None,
            })?;
            let fs = event
                .attr("fs")
                .and_then(|fs| fs.as_str())
                .unwrap_or("file")
                .to_owned();
            let device = event
                .attr("dev")
                .and_then(|dev| dev.as_str())
                .unwrap_or("?")
                .to_owned();
            let under = match blk_devs(event) {
                devices if devices.is_empty() => String::new(),
                devices => format!(
                    ", above block device {}",
                    devices
                        .iter()
                        .map(|key| key.trim_start_matches("dev:").replace('+', ":"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            Some((
                format!("file:{dev}+{ino}"),
                format!("{fs} {ino}"),
                format!("{fs} inode {ino} on {device}{under}"),
                dir_of(event),
            ))
        });
        band.title = title(data, &files, "io.fs");
    } else if !descriptors.is_empty() {
        // A descriptor may be a file, a pipe or a socket: its bytes are not
        // files' bytes, and the band says so rather than compare them with
        // the devices'.
        band.object_word = "descriptor";
        band.object_words = "descriptors";
        band.comparable = false;
        fill_target(&mut band, ctx, &descriptors, |event| {
            let fd = event.number("fd")? as u64;
            let pid = event
                .pid()
                .or_else(|| event.number("pid").map(|pid| pid as i32))?;
            Some((
                format!("fd:{pid}:{fd}"),
                format!("fd {fd}"),
                format!(
                    "descriptor {fd} of process {pid}, which may be a file, a pipe or a socket"
                ),
                dir_of(event),
            ))
        });
        band.title = title(data, &descriptors, "io.syscall");
    } else {
        let held: Vec<&Results> = data
            .of_source("io.fs")
            .chain(syscalls.iter().copied())
            .collect();
        band.target_note = Some(missing_note(
            data,
            "io.syscall",
            &held,
            "bytes per file",
            "io.syscall is in this run as a summary and a histogram: latency per thread, which \
             carries no bytes. Its `raw` rows (per descriptor) or io.fs's (per file) do",
        ));
        band.title = title(data, &held, "io.syscall");
    }
    fill_machine(&mut band, data, "io.bytes", "dev:");
    feed(&mut band, ctx, &files);
    band
}

/// The block devices under a file, as `io.fs` names them (`blk_devs`: the
/// keys `io.bytes` gives the same devices, comma-separated).
fn blk_devs(event: &Event) -> Vec<String> {
    event
        .attr("blk_devs")
        .and_then(|under| under.as_str())
        .map(|under| {
            under
                .split(',')
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The file -> device edge: each device of the machine's column that one of
/// the target's files sits above, with how many files and their bytes. A
/// file over several devices (a mirror, a stripe) is counted on each, its
/// bytes said whole on each: how they split is not measured.
fn feed(band: &mut Band, ctx: &Ctx<'_>, files: &[&Results]) {
    let mut fed: BTreeMap<String, (BTreeSet<String>, u64)> = BTreeMap::new();
    for results in files {
        for event in results.events() {
            let tid = event
                .tid()
                .or_else(|| event.number("tid").map(|tid| tid as i32))
                .unwrap_or(0);
            let pid = event
                .pid()
                .or_else(|| event.number("pid").map(|pid| pid as i32))
                .unwrap_or(0);
            if ctx.focus.pid.is_some_and(|focus| focus != pid)
                || ctx.focus.tid.is_some_and(|focus| focus != tid)
            {
                continue;
            }
            let Some((dev, ino)) = event.keys.iter().find_map(|key| match key {
                Key::File { dev, ino } => Some((*dev, *ino)),
                _ => None,
            }) else {
                continue;
            };
            let bytes = event.number("bytes").unwrap_or(0.0).max(0.0) as u64;
            for device in blk_devs(event) {
                let entry = fed.entry(device).or_default();
                entry.0.insert(format!("{dev}+{ino}"));
                entry.1 += bytes;
            }
        }
    }
    for sink in &mut band.sinks {
        if let Some((files, bytes)) = fed.get(&sink.key) {
            sink.fed = Some((files.len(), *bytes));
        }
    }
}

fn empty_band(kind: Kind) -> Band {
    Band {
        kind,
        tasks: Vec::new(),
        objects: Vec::new(),
        edges: Vec::new(),
        target_note: None,
        sinks: Vec::new(),
        idle: 0,
        machine_note: None,
        title: Vec::new(),
        machine_title: Vec::new(),
        target_trust: Trust::clean(),
        machine_trust: Trust::clean(),
        seconds: 0.0,
        measured: false,
        object_word: match kind {
            Kind::Network => "socket",
            Kind::Storage => "file",
        },
        object_words: match kind {
            Kind::Network => "sockets",
            Kind::Storage => "files",
        },
        comparable: true,
    }
}

/// Why the target's columns are empty: refused, not asked, or asked in a form
/// that carries no bytes.
fn missing_note(
    data: &Data,
    source: &str,
    held: &[&Results],
    what: &str,
    without_rows: &str,
) -> Vec<Line<'static>> {
    if let Some(refusal) = data.refusal(source) {
        return trust::refusal_lines(refusal);
    }
    let why = if held.is_empty() {
        format!("{source} was not asked for in this run.")
    } else {
        format!("{without_rows}.")
    };
    vec![
        Line::from(Span::styled(
            format!("{what}: not in this run"),
            Style::new().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(why, Style::new().add_modifier(Modifier::DIM))),
        Line::from(Span::styled(
            "docs/view/flows.md has the session that draws it.",
            Style::new().add_modifier(Modifier::DIM),
        )),
    ]
}

/// The title of a band: its source with its backend and badges.
fn title(data: &Data, held: &[&Results], source: &str) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        source.to_owned(),
        Style::new().add_modifier(Modifier::BOLD),
    )];
    if let Some(first) = held.first() {
        spans.push(Span::raw(format!(" {} ", first.backend)));
        spans.extend(Trust::joined(data, held.iter().copied()).badges());
    } else if data.refusal(source).is_some() {
        spans.push(Span::styled(" refused", Style::new().fg(palette::REFUSED)));
    } else {
        spans.push(Span::styled(
            " not in this run",
            Style::new().add_modifier(Modifier::DIM),
        ));
    }
    spans
}

/// The target's two columns, from rows of `raw` events: each a call row with
/// `bytes`, `count`, `total_ns` and `errors` summed over the calls it stands
/// for, keyed by its thread.
fn fill_target(
    band: &mut Band,
    ctx: &Ctx<'_>,
    held: &[&Results],
    identify: impl Fn(&Event) -> Option<Identity>,
) {
    let data = ctx.data;
    let opened = data.opened_ns();
    let closed = data.closed_ns().max(opened + 1);
    band.seconds = held
        .iter()
        .map(|results| {
            let window = &results.window;
            let close = if window.closed_ns == u64::MAX || window.closed_ns == 0 {
                closed
            } else {
                window.closed_ns
            };
            close.saturating_sub(window.opened_ns.max(opened)) as f64 / 1e9
        })
        .fold(0.0, f64::max);
    if band.seconds <= 0.0 {
        band.seconds = (closed - opened) as f64 / 1e9;
    }
    band.target_trust = Trust::joined(data, held.iter().copied());
    band.measured = true;

    let mut tasks: BTreeMap<i32, Task> = BTreeMap::new();
    let mut comm_bytes: BTreeMap<(i32, String), u64> = BTreeMap::new();
    let mut objects: BTreeMap<String, Object> = BTreeMap::new();
    let mut edges: BTreeMap<(i32, String, Dir), u64> = BTreeMap::new();
    for (source_index, results) in held.iter().enumerate() {
        for event in results.events() {
            let tid = event
                .tid()
                .or_else(|| event.number("tid").map(|tid| tid as i32))
                .unwrap_or(0);
            let pid = event
                .pid()
                .or_else(|| event.number("pid").map(|pid| pid as i32))
                .unwrap_or(0);
            if ctx.focus.pid.is_some_and(|focus| focus != pid)
                || ctx.focus.tid.is_some_and(|focus| focus != tid)
            {
                continue;
            }
            let Some((key, name, detail, dir)) = identify(event) else {
                continue;
            };
            let bytes = event.number("bytes").unwrap_or(0.0).max(0.0) as u64;
            let calls = event.number("count").unwrap_or(1.0).max(0.0) as u64;
            let comm = event
                .attr("comm")
                .and_then(|comm| comm.as_str())
                .unwrap_or("?")
                .to_owned();
            let task = tasks.entry(tid).or_insert_with(|| Task {
                tid,
                pid,
                ..Task::default()
            });
            task.bytes += bytes;
            *comm_bytes.entry((tid, comm)).or_default() += bytes.max(1);
            let object = objects.entry(key.clone()).or_insert_with(|| Object {
                name,
                detail,
                spark: vec![0.0; BINS],
                many: 1,
                pid: Some(pid),
                tid: Some(tid),
                ..Object::default()
            });
            object.from.insert(source_index);
            match dir {
                Dir::Out => {
                    object.out_bytes += bytes;
                    object.out_calls += calls;
                }
                Dir::In => {
                    object.in_bytes += bytes;
                    object.in_calls += calls;
                }
            }
            object.total_ns += event.number("total_ns").unwrap_or(0.0).max(0.0) as u64;
            object.errors += event.number("errors").unwrap_or(0.0).max(0.0) as u64;
            if object.tid != Some(tid) {
                object.tid = None;
            }
            if object.pid != Some(pid) {
                object.pid = None;
            }
            let at = event.ts_ns.saturating_sub(opened) as f64 / (closed - opened) as f64;
            let bin = (at * BINS as f64) as usize;
            object.spark[bin.min(BINS - 1)] += bytes as f64;
            *edges.entry((tid, key, dir)).or_default() += bytes;
        }
    }
    // Each thread is called by the name it moved the most bytes under: a
    // thread renamed by an exec is the program it became.
    let mut named: Vec<((i32, String), u64)> = comm_bytes.into_iter().collect();
    named.sort_by_key(|(_, bytes)| *bytes);
    for ((tid, comm), _) in named {
        if let Some(task) = tasks.get_mut(&tid) {
            task.comm = comm;
        }
    }

    // The largest first; past MOST, the rest are one row.
    let mut objects: Vec<(String, Object)> = objects.into_iter().collect();
    objects.sort_by(|a, b| b.1.bytes().cmp(&a.1.bytes()).then(a.0.cmp(&b.0)));
    let crowded = objects.len() > MOST;
    let mut keys: BTreeMap<String, usize> = BTreeMap::new();
    let mut kept: Vec<Object> = Vec::new();
    for (index, (key, object)) in objects.iter().enumerate() {
        if !crowded || index + 1 < MOST {
            keys.insert(key.clone(), kept.len());
            kept.push(object.clone());
            continue;
        }
        keys.insert(key.clone(), MOST - 1);
        if kept.len() < MOST {
            kept.push(Object {
                spark: vec![0.0; BINS],
                many: 0,
                pid: object.pid,
                tid: object.tid,
                ..Object::default()
            });
        }
        let others = &mut kept[MOST - 1];
        others.many += 1;
        others.out_bytes += object.out_bytes;
        others.in_bytes += object.in_bytes;
        others.out_calls += object.out_calls;
        others.in_calls += object.in_calls;
        others.total_ns += object.total_ns;
        others.errors += object.errors;
        others.from.extend(object.from.iter().copied());
        if others.pid != object.pid {
            others.pid = None;
        }
        if others.tid != object.tid {
            others.tid = None;
        }
        for (bin, bytes) in others.spark.iter_mut().zip(&object.spark) {
            *bin += bytes;
        }
    }
    for object in &mut kept {
        object.trust = Trust::joined(data, object.from.iter().map(|&index| held[index]));
    }
    if crowded {
        let what = band.object_words;
        let others = &mut kept[MOST - 1];
        others.name = format!("{} more {what}", others.many);
        others.detail = format!(
            "the {} {what} that moved the fewest bytes, drawn as one",
            others.many
        );
    }

    let mut tasks: Vec<Task> = tasks.into_values().collect();
    tasks.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.tid.cmp(&b.tid)));
    let crowded = tasks.len() > MOST;
    let count = tasks.len();
    let mut task_index: BTreeMap<i32, usize> = BTreeMap::new();
    let mut kept_tasks: Vec<Task> = Vec::new();
    for (index, task) in tasks.into_iter().enumerate() {
        if !crowded || index + 1 < MOST {
            task_index.insert(task.tid, kept_tasks.len());
            kept_tasks.push(task);
            continue;
        }
        task_index.insert(task.tid, MOST - 1);
        if kept_tasks.len() < MOST {
            kept_tasks.push(Task {
                comm: format!("{} more threads", count - (MOST - 1)),
                ..Task::default()
            });
        }
        kept_tasks[MOST - 1].bytes += task.bytes;
    }

    let mut merged: BTreeMap<(usize, usize, Dir), u64> = BTreeMap::new();
    for ((tid, key, dir), bytes) in edges {
        let (Some(&task), Some(&object)) = (task_index.get(&tid), keys.get(&key)) else {
            continue;
        };
        *merged.entry((task, object, dir)).or_default() += bytes;
    }
    band.edges = merged
        .into_iter()
        .map(|((task, object, dir), bytes)| Edge {
            task,
            object,
            dir,
            bytes,
        })
        .collect();
    band.tasks = kept_tasks;
    band.objects = kept;
    if band.objects.is_empty() {
        let rows: usize = held.iter().map(|results| results.events().len()).sum();
        band.target_note = Some(vec![
            Line::from(Span::styled(
                format!("no {} moved a byte", band.object_word),
                Style::new().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                if ctx.focus.is_empty() {
                    format!("{rows} rows in this run, none with bytes")
                } else {
                    format!("none for {}", ctx.focus.describe())
                },
                Style::new().add_modifier(Modifier::DIM),
            )),
        ]);
    }
}

/// The machine's column: each interface or device's bytes over the run, from
/// its timeline.
fn fill_machine(band: &mut Band, data: &Data, source: &str, prefix: &str) {
    let held: Vec<&Results> = data.of_source(source).collect();
    band.machine_title = title(data, &held, source);
    if let Some(refusal) = data.refusal(source) {
        band.machine_note = Some(format!("{source} refused: {}", refusal.reason));
        return;
    }
    if held.is_empty() {
        band.machine_note = Some(format!("{source} was not asked for in this run"));
        return;
    }
    band.machine_trust = Trust::joined(data, held.iter().copied());
    let opened = data.opened_ns();
    let span = data.closed_ns().saturating_sub(opened).max(1) as f64;
    let mut sinks: BTreeMap<String, Sink> = BTreeMap::new();
    let mut timelines = 0;
    for results in &held {
        for output in &results.outputs {
            let Output::Timeline { series, .. } = output else {
                continue;
            };
            timelines += 1;
            for one in series {
                let Some(name) = one.key.strip_prefix(prefix) else {
                    continue;
                };
                let sink = sinks.entry(name.to_owned()).or_insert_with(|| Sink {
                    name: name.replace('+', ":"),
                    key: one.key.clone(),
                    spark: vec![0.0; BINS],
                    ..Sink::default()
                });
                for point in &one.points {
                    let value = point.value.max(0.0);
                    sink.bytes += value;
                    let at = (point.offset_ns as f64 + results.window.opened_ns as f64
                        - opened as f64)
                        / span;
                    let bin = (at.clamp(0.0, 0.999) * BINS as f64) as usize;
                    sink.spark[bin] += value;
                }
            }
        }
    }
    if timelines == 0 {
        band.machine_note = Some(if data.live.is_some() {
            format!("{source} is a timeline, which a live run holds only at its end")
        } else {
            format!("{source} is in this run without its timeline")
        });
        return;
    }
    let mut sinks: Vec<Sink> = sinks.into_values().collect();
    band.idle = sinks.iter().filter(|sink| sink.bytes <= 0.0).count();
    sinks.retain(|sink| sink.bytes > 0.0);
    if sinks.is_empty() {
        band.machine_note = Some(format!(
            "no {} moved a byte in this run ({} idle)",
            match band.kind {
                Kind::Network => "interface",
                Kind::Storage => "device",
            },
            band.idle
        ));
    }
    sinks.sort_by(|a, b| b.bytes.total_cmp(&a.bytes).then(a.name.cmp(&b.name)));
    if sinks.len() > MOST_SINKS {
        let rest: Vec<Sink> = sinks.split_off(MOST_SINKS - 1);
        let mut others = Sink {
            name: format!("{} more", rest.len()),
            spark: vec![0.0; BINS],
            ..Sink::default()
        };
        for sink in rest {
            others.bytes += sink.bytes;
            for (bin, bytes) in others.spark.iter_mut().zip(&sink.spark) {
                *bin += bytes;
            }
        }
        sinks.push(others);
    }
    band.sinks = sinks;
}

// ---------------------------------------------------------------------------
// Drawing.
// ---------------------------------------------------------------------------

fn summary_line(ctx: &Ctx<'_>, bands: &[Band; 2]) -> Line<'static> {
    let mut spans = Vec::new();
    let mut pids: Vec<i32> = bands
        .iter()
        .flat_map(|band| band.tasks.iter().map(|task| task.pid))
        .filter(|pid| *pid > 0)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    if !ctx.focus.is_empty() {
        spans.push(Span::styled(
            format!("{} ", ctx.focus.describe()),
            Style::new().fg(palette::ACCENT),
        ));
    } else if !pids.is_empty() {
        spans.push(Span::raw(match pids.as_slice() {
            [one] => format!("pid {one} "),
            many => format!("{} processes ", many.len()),
        }));
    }
    let seconds = bands
        .iter()
        .map(|band| band.seconds)
        .fold(0.0, f64::max)
        .max(ctx.data.closed_ns().saturating_sub(ctx.data.opened_ns()) as f64 / 1e9);
    spans.push(Span::raw(format!(
        "over {}   ",
        units::duration(seconds * 1e9)
    )));
    for band in bands {
        if band.objects.is_empty() {
            continue;
        }
        let (out_word, in_word) = match band.kind {
            Kind::Network => ("sent", "received"),
            Kind::Storage => ("written", "read"),
        };
        let style = band.target_trust.style();
        spans.push(Span::styled(
            units::bytes(band.out_bytes() as f64),
            style.fg(OUTWARD),
        ));
        spans.push(Span::styled(format!(" {out_word}, "), style));
        spans.push(Span::styled(
            units::bytes(band.in_bytes() as f64),
            style.fg(INWARD),
        ));
        spans.push(Span::styled(format!(" {in_word}   "), style));
    }
    spans.push(Span::styled("█", Style::new().fg(OUTWARD)));
    spans.push(Span::raw(" out "));
    spans.push(Span::styled("█", Style::new().fg(INWARD)));
    spans.push(Span::raw(" in "));
    spans.push(Span::styled("█", Style::new().fg(MACHINE)));
    spans.push(Span::raw(" the machine's"));
    Line::from(spans)
}

/// The columns of a band, in cells from its left edge.
#[derive(Debug, Clone, Copy)]
struct Columns {
    task_width: u16,
    task_bar: u16,
    object_bar: u16,
    object_label: u16,
    object_width: u16,
    object_bar_right: u16,
    junction: u16,
    sink_bar: u16,
    sink_label: u16,
    sink_width: u16,
}

impl Columns {
    fn for_width(width: u16) -> Columns {
        let task_width = 15;
        let sink_width = 24.min(width / 5);
        // Past the 118 cells of a 120-column terminal the ribbons and the
        // objects' column take the extra width, so the diagram spans the
        // terminal rather than leaving its right half to the sinks' labels.
        let extra = width.saturating_sub(118);
        let ribbon = (width / 12).clamp(4, 10) + extra / 7;
        let fixed = task_width + 1 + 1 + ribbon + 1 + 1 + ribbon + 1 + ribbon + 2 + sink_width;
        let object_width = width.saturating_sub(fixed).clamp(12, 44 + extra / 6);
        let task_bar = task_width + 1;
        let object_bar = task_bar + 1 + ribbon;
        let object_label = object_bar + 1;
        let object_bar_right = object_label + object_width;
        let junction = object_bar_right + 1 + ribbon;
        let sink_bar = junction + 1 + ribbon;
        Columns {
            task_width,
            task_bar,
            object_bar,
            object_label,
            object_width,
            object_bar_right,
            junction,
            sink_bar,
            sink_label: sink_bar + 2,
            sink_width: width.saturating_sub(sink_bar + 2),
        }
    }
}

/// Splits `rows` among `values` in proportion, each at least one row, never
/// more than `rows` in all (the largest give way first when the minimum of
/// one row overruns).
fn allot(values: &[f64], rows: usize) -> Vec<usize> {
    if values.is_empty() || rows == 0 {
        return vec![0; values.len()];
    }
    let total: f64 = values.iter().sum();
    let share = |value: f64| {
        if total > 0.0 {
            value / total * rows as f64
        } else {
            rows as f64 / values.len() as f64
        }
    };
    let mut alloted: Vec<usize> = values
        .iter()
        .map(|value| share(*value).floor().max(1.0) as usize)
        .collect();
    let mut sum: usize = alloted.iter().sum();
    while sum < rows {
        let (index, _) = values
            .iter()
            .enumerate()
            .map(|(index, value)| (index, share(*value) - alloted[index] as f64))
            .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))
            .expect("not empty");
        alloted[index] += 1;
        sum += 1;
    }
    while sum > rows {
        let Some((index, _)) = alloted
            .iter()
            .enumerate()
            .filter(|(_, rows)| **rows > 1)
            .max_by_key(|(_, rows)| **rows)
        else {
            break;
        };
        alloted[index] -= 1;
        sum -= 1;
    }
    // More nodes than rows: the last, the smallest, are not drawn.
    for height in alloted.iter_mut().rev() {
        if sum <= rows {
            break;
        }
        sum -= *height;
        *height = 0;
    }
    alloted
}

/// Where each node of a column is, in rows from the diagram's top.
fn stack(heights: &[usize]) -> Vec<(usize, usize)> {
    let mut top = 0;
    heights
        .iter()
        .map(|height| {
            let span = (top, top + height);
            top += height;
            span
        })
        .collect()
}

/// A canvas of half rows over the band's diagram: each half row of each cell
/// holds the style of the ribbon drawn there last.
struct Canvas {
    width: usize,
    halves: usize,
    cells: Vec<Option<Style>>,
}

impl Canvas {
    fn new(width: u16, rows: u16) -> Canvas {
        let width = usize::from(width);
        let halves = usize::from(rows) * 2;
        Canvas {
            width,
            halves,
            cells: vec![None; width * halves],
        }
    }

    fn set(&mut self, x: usize, half: usize, style: Style) {
        if x < self.width && half < self.halves {
            self.cells[half * self.width + x] = Some(style);
        }
    }

    /// A ribbon from the half rows `[from.0, from.1)` at column `x0` to
    /// `[to.0, to.1)` at column `x1` (exclusive), its edges eased so it bends
    /// rather than steps. At least one half row thick everywhere.
    fn ribbon(&mut self, x0: usize, x1: usize, from: (f64, f64), to: (f64, f64), style: Style) {
        let length = x1.saturating_sub(x0);
        for step in 0..length {
            let t = (step as f64 + 0.5) / length as f64;
            let eased = t * t * (3.0 - 2.0 * t);
            let top = from.0 + (to.0 - from.0) * eased;
            let bottom = from.1 + (to.1 - from.1) * eased;
            let first = top.round() as usize;
            let last = (bottom.round() as usize).max(first + 1);
            for half in first..last {
                self.set(x0 + step, half, style);
            }
        }
    }

    /// Writes the canvas into the buffer at `area`: a cell whose two halves
    /// hold the same style is a full block; two styles, an upper half block
    /// over the lower one's colour.
    fn render(&self, buffer: &mut Buffer, area: Rect) {
        for row in 0..usize::from(area.height).min(self.halves / 2) {
            for x in 0..self.width.min(usize::from(area.width)) {
                let top = self.cells[(row * 2) * self.width + x];
                let bottom = self.cells[(row * 2 + 1) * self.width + x];
                let (symbol, style) = match (top, bottom) {
                    (None, None) => continue,
                    (Some(top), None) => ("▀", top),
                    (None, Some(bottom)) => ("▄", bottom),
                    (Some(top), Some(bottom)) if top == bottom => ("█", top),
                    (Some(top), Some(bottom)) => ("▀", top.bg(bottom.fg.unwrap_or(Color::Reset))),
                };
                let position = (area.x + x as u16, area.y + row as u16);
                buffer[position].set_symbol(symbol).set_style(style);
            }
        }
    }
}

/// The style a ribbon of a result is drawn in: its colour, dimmed when the
/// result must not be read at face value.
fn ribbon_style(colour: Color, trust: &Trust) -> Style {
    trust.style().fg(colour)
}

/// Draws one band, and returns where each of its objects was drawn, each
/// region's id its index among the band's objects plus `offset`.
fn draw_band(
    frame: &mut Frame<'_>,
    area: Rect,
    band: &Band,
    selected: Option<usize>,
    offset: usize,
) -> Vec<Region> {
    let mut regions = Vec::new();
    let name = match band.kind {
        Kind::Network => " network ",
        Kind::Storage => " storage ",
    };
    let mut title = vec![Span::styled(
        name,
        Style::new().add_modifier(Modifier::BOLD),
    )];
    title.push(Span::raw("· "));
    title.extend(band.title.iter().cloned());
    title.push(Span::raw(" · "));
    title.extend(band.machine_title.iter().cloned());
    title.push(Span::raw(" "));
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(Style::new().fg(BAR))
        .title(Line::from(title));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height + 1 < BAND_LEAST {
        // The view's own guard keeps this from happening; should it, the band
        // says so rather than stand empty.
        frame.render_widget(
            Paragraph::new(Span::styled(
                "too few rows to draw this band",
                Style::new().add_modifier(Modifier::DIM),
            )),
            inner,
        );
        return regions;
    }
    let [heading, diagram, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let columns = Columns::for_width(inner.width);
    draw_heading(frame.buffer_mut(), heading, band, &columns);
    let rows = usize::from(diagram.height);

    // One scale for the whole band, so the target's bytes and the machine's
    // are compared by height.
    let target_total = (band.out_bytes() + band.in_bytes()) as f64;
    let machine_total = band.machine_bytes();
    let largest = target_total.max(machine_total);
    let column_rows = |total: f64, nodes: usize| -> usize {
        if nodes == 0 || largest <= 0.0 {
            return nodes.min(rows);
        }
        (((total / largest) * rows as f64).round() as usize).clamp(nodes.min(rows), rows)
    };

    let object_values: Vec<f64> = band.objects.iter().map(|o| o.bytes() as f64).collect();
    let object_rows = allot(
        &object_values,
        column_rows(target_total, band.objects.len()),
    );
    let object_spans = stack(&object_rows);
    // An object is its rows, from its bar to the junction: its bar, its
    // name, its figures.
    let object_x = diagram.x + columns.object_bar;
    let object_width = columns.junction.saturating_sub(columns.object_bar).max(1);
    for (index, (top, bottom)) in object_spans.iter().enumerate() {
        let bottom = (*bottom).min(rows);
        if bottom > *top {
            regions.push(Region::new(
                Rect::new(
                    object_x,
                    diagram.y + *top as u16,
                    object_width,
                    (bottom - top) as u16,
                ),
                offset + index,
            ));
        }
    }
    let task_values: Vec<f64> = band.tasks.iter().map(|t| t.bytes as f64).collect();
    let task_rows = allot(&task_values, column_rows(target_total, band.tasks.len()));
    let task_spans = stack(&task_rows);
    let sink_values: Vec<f64> = band.sinks.iter().map(|s| s.bytes).collect();
    let sink_rows = allot(&sink_values, column_rows(machine_total, band.sinks.len()));
    let sink_spans = stack(&sink_rows);
    // A node the rows could not hold is not drawn: say which, and their
    // bytes, so the drawn rows and the footer's total can be reconciled.
    let mut dropped = Vec::new();
    for (word, words, count, bytes) in [
        (
            "thread",
            "threads",
            dropped_nodes(&task_rows, band.tasks.iter().map(|_| 1)),
            dropped_bytes(&task_rows, &task_values),
        ),
        (
            band.object_word,
            band.object_words,
            dropped_nodes(&object_rows, band.objects.iter().map(|o| o.many.max(1))),
            dropped_bytes(&object_rows, &object_values),
        ),
        (
            match band.kind {
                Kind::Network => "interface",
                Kind::Storage => "device",
            },
            match band.kind {
                Kind::Network => "interfaces",
                Kind::Storage => "devices",
            },
            dropped_nodes(&sink_rows, band.sinks.iter().map(|_| 1)),
            dropped_bytes(&sink_rows, &sink_values),
        ),
    ] {
        if count > 0 {
            dropped.push(format!(
                "{count} {} ({}) not drawn",
                if count == 1 { word } else { words },
                units::bytes(bytes)
            ));
        }
    }
    let object_height: usize = object_rows.iter().sum();
    let sink_height: usize = sink_rows.iter().sum();
    let junction_height = if band.objects.is_empty() && band.sinks.is_empty() {
        0
    } else {
        object_height.max(sink_height).max(1)
    };

    let mut canvas = Canvas::new(diagram.width, diagram.height);
    // Thread to object: each edge a slot of its thread's span and of its
    // object's, in proportion to its bytes.
    let task_slots = slots(
        &band.edges,
        |edge| edge.task,
        |edge| (edge.object, edge.dir as usize),
        &task_spans,
    );
    let object_slots = slots(
        &band.edges,
        |edge| edge.object,
        |edge| (edge.dir as usize, edge.task),
        &object_spans,
    );
    for (index, edge) in band.edges.iter().enumerate() {
        let object = &band.objects[edge.object];
        let (task_span, object_span) = (task_spans[edge.task], object_spans[edge.object]);
        if task_span.0 == task_span.1 || object_span.0 == object_span.1 {
            continue;
        }
        canvas.ribbon(
            usize::from(columns.task_bar) + 1,
            usize::from(columns.object_bar),
            task_slots[index],
            object_slots[index],
            ribbon_style(edge.dir.colour(), &object.trust),
        );
    }
    // Object to the junction: each object whole, stacked from the top, out
    // above in.
    let mut junction_top = 0.0;
    for (index, object) in band.objects.iter().enumerate() {
        let (top, bottom) = object_spans[index];
        let (top, bottom) = (top as f64 * 2.0, bottom as f64 * 2.0);
        let height = bottom - top;
        if height <= 0.0 {
            continue;
        }
        let out_share = if object.bytes() > 0 {
            object.out_bytes as f64 / object.bytes() as f64
        } else {
            0.0
        };
        let mut split = top + (height * out_share).round();
        if object.out_bytes > 0 && object.in_bytes > 0 {
            split = split.clamp(top + 1.0, bottom - 1.0);
        }
        for (dir, from, to) in [(Dir::Out, top, split), (Dir::In, split, bottom)] {
            if to <= from {
                continue;
            }
            let at = junction_top + (from - top);
            canvas.ribbon(
                usize::from(columns.object_bar_right) + 1,
                usize::from(columns.junction),
                (from, to),
                (at, at + (to - from)),
                ribbon_style(dir.colour(), &object.trust),
            );
        }
        junction_top += height;
    }
    // The junction to the machine's interfaces or devices.
    let mut junction_top = 0.0;
    for span in &sink_spans {
        let (top, bottom) = (span.0 as f64 * 2.0, span.1 as f64 * 2.0);
        canvas.ribbon(
            usize::from(columns.junction) + 1,
            usize::from(columns.sink_bar),
            (junction_top, junction_top + bottom - top),
            (top, bottom),
            ribbon_style(MACHINE, &band.machine_trust),
        );
        junction_top += bottom - top;
    }
    canvas.render(frame.buffer_mut(), diagram);

    let buffer = frame.buffer_mut();
    let bar = |buffer: &mut Buffer, x: u16, (top, bottom): (usize, usize), style: Style| {
        for row in top..bottom.min(rows) {
            let position = (diagram.x + x, diagram.y + row as u16);
            buffer[position].set_symbol("┃").set_style(style);
        }
    };
    let task_width = usize::from(columns.task_width);
    for (index, task) in band.tasks.iter().enumerate() {
        let (top, bottom) = task_spans[index];
        if top == bottom {
            continue;
        }
        bar(
            buffer,
            columns.task_bar,
            (top, bottom),
            band.target_trust.style().fg(BAR),
        );
        let label = truncate(&task.comm, task_width);
        buffer.set_stringn(
            diagram.x,
            diagram.y + top as u16,
            format!("{label:>task_width$}"),
            task_width,
            Style::new(),
        );
        if bottom - top >= 2 && task.tid > 0 {
            let tid = format!("tid {}", task.tid);
            buffer.set_stringn(
                diagram.x,
                diagram.y + top as u16 + 1,
                format!("{tid:>task_width$}"),
                task_width,
                Style::new().add_modifier(Modifier::DIM),
            );
        }
    }
    for (index, object) in band.objects.iter().enumerate() {
        let (top, bottom) = object_spans[index];
        if top == bottom {
            continue;
        }
        let chosen = selected == Some(index);
        let bar_style = if chosen {
            Style::new().fg(palette::ACCENT)
        } else {
            object.trust.style().fg(BAR)
        };
        bar(buffer, columns.object_bar, (top, bottom), bar_style);
        bar(buffer, columns.object_bar_right, (top, bottom), bar_style);
        let line = object_line(object, band.kind, chosen, usize::from(columns.object_width));
        buffer.set_line(
            diagram.x + columns.object_label,
            diagram.y + top as u16,
            &line,
            columns.object_width,
        );
        if bottom - top >= 2 {
            buffer.set_stringn(
                diagram.x + columns.object_label + 2,
                diagram.y + top as u16 + 1,
                object_figures(object, band.seconds),
                usize::from(columns.object_width.saturating_sub(2)),
                object.trust.style().add_modifier(Modifier::DIM),
            );
        }
    }
    if junction_height > 0 {
        bar(
            buffer,
            columns.junction,
            (0, junction_height),
            Style::new().fg(BAR),
        );
    }
    for (index, sink) in band.sinks.iter().enumerate() {
        let span = sink_spans[index];
        if span.0 == span.1 {
            continue;
        }
        bar(
            buffer,
            columns.sink_bar,
            span,
            band.machine_trust.style().fg(MACHINE),
        );
        let line = sink_line(
            sink,
            band.kind,
            &band.machine_trust,
            usize::from(columns.sink_width),
        );
        buffer.set_line(
            diagram.x + columns.sink_label,
            diagram.y + span.0 as u16,
            &line,
            columns.sink_width,
        );
    }

    // What stands where a column is empty.
    if let Some(note) = &band.target_note {
        let note_area = Rect {
            x: diagram.x + 1,
            y: diagram.y,
            width: columns.object_bar_right.saturating_sub(1),
            height: diagram.height,
        };
        frame.render_widget(
            Paragraph::new(note.clone()).wrap(Wrap { trim: true }),
            note_area,
        );
    }
    if let Some(note) = &band.machine_note {
        let x = columns.junction + 2;
        let note_area = Rect {
            x: diagram.x + x,
            y: diagram.y,
            width: diagram.width.saturating_sub(x),
            height: diagram.height,
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                note.clone(),
                Style::new().add_modifier(Modifier::DIM),
            ))
            .wrap(Wrap { trim: true }),
            note_area,
        );
    }
    frame.render_widget(Paragraph::new(footer_line(band, &dropped)), footer);
    regions
}

/// How many nodes a column did not draw, each counted for what it stands for
/// (the "more files" row is many).
fn dropped_nodes(rows: &[usize], many: impl Iterator<Item = usize>) -> usize {
    rows.iter()
        .zip(many)
        .filter(|(rows, _)| **rows == 0)
        .map(|(_, many)| many)
        .sum()
}

/// The bytes of the nodes a column did not draw.
fn dropped_bytes(rows: &[usize], values: &[f64]) -> f64 {
    rows.iter()
        .zip(values)
        .filter(|(rows, _)| **rows == 0)
        .map(|(_, bytes)| *bytes)
        .sum()
}

/// Each edge's slot in its node's span, in half rows: the node's edges in
/// `order`, each in proportion to its bytes and one half row at least.
fn slots(
    edges: &[Edge],
    node: impl Fn(&Edge) -> usize,
    order: impl Fn(&Edge) -> (usize, usize),
    spans: &[(usize, usize)],
) -> Vec<(f64, f64)> {
    let mut out = vec![(0.0, 0.0); edges.len()];
    for (index, span) in spans.iter().enumerate() {
        let mut mine: Vec<usize> = (0..edges.len())
            .filter(|&edge| node(&edges[edge]) == index)
            .collect();
        mine.sort_by_key(|&edge| order(&edges[edge]));
        let total: f64 = mine.iter().map(|&edge| edges[edge].bytes as f64).sum();
        let (top, bottom) = (span.0 as f64 * 2.0, span.1 as f64 * 2.0);
        let mut at = top;
        for edge in mine {
            let share = if total > 0.0 {
                edges[edge].bytes as f64 / total
            } else {
                0.0
            };
            let next = (at + (bottom - top) * share).min(bottom);
            let first = at.round().min(bottom - 1.0);
            out[edge] = (first, next.round().max(first + 1.0));
            at = next;
        }
    }
    out
}

/// The first line of an object: its name, a sparkline of when its bytes
/// moved, and its bytes out and in.
fn object_line(object: &Object, kind: Kind, chosen: bool, width: usize) -> Line<'static> {
    let style = object.trust.style();
    let name_width = 14.min(width.saturating_sub(2));
    let mut spans = vec![Span::styled(
        format!(
            "{}{:<name_width$}",
            if chosen { "▶" } else { " " },
            truncate(&object.name, name_width.saturating_sub(1))
        ),
        if chosen {
            Style::new()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::BOLD)
        },
    )];
    spans.push(Span::styled(sparkline(&object.spark), style.fg(BAR)));
    let (out_mark, in_mark) = match kind {
        Kind::Network => ("↑", "↓"),
        Kind::Storage => ("→", "←"),
    };
    if object.out_bytes > 0 {
        spans.push(Span::styled(
            format!(" {out_mark}{}", units::bytes(object.out_bytes as f64)),
            style.fg(OUTWARD),
        ));
    }
    if object.in_bytes > 0 || object.out_bytes == 0 {
        spans.push(Span::styled(
            format!(" {in_mark}{}", units::bytes(object.in_bytes as f64)),
            style.fg(INWARD),
        ));
    }
    // A loss is said on the object itself; the rest of a source's marks are
    // in the band's title, once.
    for mark in &object.trust.marks {
        if matches!(mark, trust::Mark::Lost { .. }) {
            spans.push(Span::raw(" "));
            spans.push(mark.badge());
        }
    }
    Line::from(spans)
}

/// An object's second line: its rate over the run, its calls, and the time
/// a call took on average.
fn object_figures(object: &Object, seconds: f64) -> String {
    let calls = object.out_calls + object.in_calls;
    let mut text = format!(
        "{}/s, {} calls",
        units::bytes(object.bytes() as f64 / seconds.max(1e-9)),
        units::count(calls as f64)
    );
    if calls > 0 && object.total_ns > 0 {
        text.push_str(&format!(
            ", {} each",
            units::duration(object.total_ns as f64 / calls as f64)
        ));
    }
    text
}

fn sink_line(sink: &Sink, kind: Kind, trust: &Trust, width: usize) -> Line<'static> {
    let style = trust.style();
    let name_width = width.saturating_sub(17).clamp(4, 10);
    let mut spans = vec![
        Span::styled(
            format!("{:<name_width$} ", truncate(&sink.name, name_width)),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{:>7}", units::bytes(sink.bytes)), style),
        Span::raw(" "),
        Span::styled(sparkline(&sink.spark), style.fg(BAR)),
    ];
    if let Some((count, _)) = sink.fed {
        // The edge from the target's files or sockets, in words beside the
        // device or the interface.
        let word = match kind {
            Kind::Network => "socket",
            Kind::Storage => "file",
        };
        spans.push(Span::styled(
            format!(" ◂ {count} {word}{}", if count == 1 { "" } else { "s" }),
            Style::new().add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

fn draw_heading(buffer: &mut Buffer, area: Rect, band: &Band, columns: &Columns) {
    let dim = Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC);
    let object = band.object_word;
    let sink = match band.kind {
        Kind::Network => "interface, the machine's",
        Kind::Storage => "device, the machine's",
    };
    let width = usize::from(columns.task_width);
    buffer.set_stringn(area.x, area.y, format!("{:>width$}", "thread"), width, dim);
    buffer.set_stringn(
        area.x + columns.object_label + 1,
        area.y,
        object,
        usize::from(columns.object_width),
        dim,
    );
    let junction = band.kind.junction();
    let x = columns.junction.saturating_sub(junction.len() as u16 / 2);
    buffer.set_stringn(area.x + x, area.y, junction, junction.len(), dim);
    buffer.set_stringn(
        area.x + columns.sink_label,
        area.y,
        sink,
        usize::from(columns.sink_width),
        dim,
    );
}

/// What the junction says: the target's bytes on its left, the machine's on
/// its right, and what stands between them.
fn footer_line(band: &Band, dropped: &[String]) -> Line<'static> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let mut spans = Vec::new();
    let target = band.out_bytes() + band.in_bytes();
    let machine = band.machine_bytes();
    if !band.objects.is_empty() {
        let what = band.object_words;
        spans.push(Span::styled(
            format!("{what} {}", units::bytes(target as f64)),
            band.target_trust.style(),
        ));
    }
    if !band.sinks.is_empty() {
        if !spans.is_empty() {
            spans.push(Span::raw(" -> "));
        }
        spans.push(Span::styled(
            format!(
                "{} {}",
                match band.kind {
                    Kind::Network => "interfaces",
                    Kind::Storage => "devices",
                },
                units::bytes(machine)
            ),
            band.machine_trust.style(),
        ));
        if band.comparable && target > 0 && machine > 0.0 {
            spans.push(Span::styled(
                format!(" = {:.2}x", machine / target as f64),
                band.machine_trust.style().add_modifier(Modifier::BOLD),
            ));
        }
    }
    // What was not drawn comes right after the totals, so a narrow screen
    // cuts the idle count and the caveat rather than it.
    for one in dropped {
        spans.push(Span::styled(
            format!(" · {one}"),
            Style::new().fg(palette::LOSS),
        ));
    }
    if !band.sinks.is_empty() && band.idle > 0 {
        let what = match band.kind {
            Kind::Network => "interfaces",
            Kind::Storage => "devices",
        };
        spans.push(Span::styled(format!(" · {} {what} idle", band.idle), dim));
    }
    if spans.is_empty() {
        return Line::default();
    }
    let routed = band.sinks.iter().any(|sink| sink.fed.is_some());
    spans.push(Span::styled(
        match band.kind {
            Kind::Network if routed => {
                " · lo counts a byte twice; a socket is tied to the interface its route left by"
            }
            Kind::Network => " · lo counts a byte twice; sockets are not tied to interfaces",
            Kind::Storage if !band.comparable => {
                " · no ratio: a descriptor may be a socket or a pipe, whose bytes never \
                 reach a device"
            }
            Kind::Storage => " · page-cache hits never reach a device; writes reach it later",
        },
        dim,
    ));
    Line::from(spans)
}

fn detail_lines(bands: &[Band; 2], selected: Option<usize>) -> Vec<Line<'static>> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let Some(at) = selected else {
        return vec![Line::from(Span::styled(nothing_to_select(bands), dim))];
    };
    let first = bands[0].objects.len();
    let band = if at < first { &bands[0] } else { &bands[1] };
    let Some(object) = selected_object(bands, at) else {
        return Vec::new();
    };
    let (out_word, in_word) = match band.kind {
        Kind::Network => ("sent", "received"),
        Kind::Storage => ("written", "read"),
    };
    let seconds = band.seconds.max(1e-9);
    let mut spans = vec![Span::styled(
        format!("▶ {}: ", object.detail),
        Style::new().fg(palette::ACCENT),
    )];
    let mut parts = Vec::new();
    for (word, bytes, calls, colour) in [
        (out_word, object.out_bytes, object.out_calls, OUTWARD),
        (in_word, object.in_bytes, object.in_calls, INWARD),
    ] {
        if calls == 0 {
            continue;
        }
        parts.push(Span::styled(
            format!(
                "{} {word} in {} calls, {}/s",
                units::bytes(bytes as f64),
                units::count(calls as f64),
                units::bytes(bytes as f64 / seconds)
            ),
            object.trust.style().fg(colour),
        ));
    }
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("; "));
        }
        spans.push(part);
    }
    if object.errors > 0 {
        spans.push(Span::styled(
            format!("; {} calls failed", units::count(object.errors as f64)),
            Style::new().fg(palette::REFUSED),
        ));
    }
    let mut second = Vec::new();
    match (object.pid, object.tid) {
        (Some(pid), Some(tid)) => second.push(Span::styled(
            format!("  pid {pid}, thread {tid} -- enter narrows every view to it"),
            dim,
        )),
        (Some(pid), None) => second.push(Span::styled(
            format!("  pid {pid}, several threads -- enter narrows every view to it"),
            dim,
        )),
        _ => {}
    }
    let sentences: Vec<String> = object
        .trust
        .marks
        .iter()
        .filter(|mark| matches!(mark, trust::Mark::Lost { .. }))
        .map(trust::Mark::sentence)
        .collect();
    if !sentences.is_empty() {
        second.push(Span::styled(
            format!(" · {}", sentences.join("; ")),
            Style::new().fg(palette::LOSS),
        ));
    }
    vec![Line::from(spans), Line::from(second)]
}

/// Why nothing can be selected: per band, its rows were there and carried no
/// bytes, or they were not measured at all. Never the first said of the
/// second.
fn nothing_to_select(bands: &[Band; 2]) -> String {
    let parts: Vec<String> = bands
        .iter()
        .map(|band| {
            if band.measured {
                format!("no {} of the target's moved a byte", band.object_word)
            } else {
                format!("bytes per {} were not measured", band.object_word)
            }
        })
        .collect();
    format!("nothing to select: {}", parts.join("; "))
}

fn sparkline(values: &[f64]) -> String {
    let max = values.iter().copied().fold(0.0, f64::max);
    values
        .iter()
        .map(|value| {
            if max <= 0.0 || *value <= 0.0 {
                ' '
            } else {
                let level = ((value / max) * (SPARK.len() - 1) as f64).round() as usize;
                SPARK[level.min(SPARK.len() - 1)]
            }
        })
        .collect()
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_owned()
    } else {
        let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::snapshot::{assert_snapshot, draw, text, HEIGHT, WIDTH};

    /// The run the view is judged on: a small file server under a client that
    /// downloads three files of known sizes twelve times each and uploads
    /// twelve of 256 KiB (`docs/view/gallery/flows-run/record.sh`).
    fn file_server() -> Data {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/view/gallery/flows-run/data");
        let mut data = Data::load(&path).expect("the flows recording");
        data.origin = "recording flows-run".to_owned();
        data
    }

    fn ctx<'a>(data: &'a Data, focus: &'a Focus) -> Ctx<'a> {
        Ctx {
            data,
            against: None,
            focus,
        }
    }

    fn total(band: &Band) -> f64 {
        (band.out_bytes() + band.in_bytes()) as f64
    }

    #[test]
    fn the_file_server_is_drawn_as_its_snapshot() {
        let buffer = draw(file_server(), "flows", WIDTH, HEIGHT);
        assert_snapshot("flows--file-server", &buffer);
    }

    /// The files the server read are the files the client asked for, byte
    /// for byte: twelve times 8 MiB, 1 MiB and 64 KiB, the three largest.
    /// The uploads are twelve files of exactly 256 KiB written.
    #[test]
    fn each_file_holds_the_bytes_the_workload_moved_through_it() {
        let data = file_server();
        let focus = Focus::default();
        let band = storage(&ctx(&data, &focus));
        let read: Vec<u64> = band.objects.iter().map(|o| o.in_bytes).take(3).collect();
        assert_eq!(read, [12 * (8 << 20), 12 * (1 << 20), 12 * (64 << 10)]);
        let rows: Vec<&Event> = data
            .of_source("io.fs")
            .flat_map(|results| results.events())
            .collect();
        let mut written: BTreeMap<String, f64> = BTreeMap::new();
        for event in &rows {
            if event.attr("op").and_then(|op| op.as_str()) == Some("write") {
                let key = event.keys[0].to_string();
                *written.entry(key).or_default() += event.number("bytes").unwrap_or(0.0);
            }
        }
        let uploads = written
            .values()
            .filter(|bytes| **bytes == f64::from(256 << 10))
            .count();
        assert_eq!(uploads, 12, "{written:?}");
        // Nothing is lost between the rows and the drawing: the band's
        // totals are the rows' sums, the "more files" row included.
        let sum: f64 = rows
            .iter()
            .map(|event| event.number("bytes").unwrap_or(0.0))
            .sum();
        assert_eq!(total(&band), sum);
    }

    /// One socket: the uploads received and the downloads (and their headers)
    /// sent. Loopback carried every byte of it twice, once sent and once
    /// received: the junction's gap, measured.
    #[test]
    fn the_socket_and_the_interface_disagree_by_the_loopback_double_count() {
        let data = file_server();
        let focus = Focus::default();
        let band = network(&ctx(&data, &focus));
        assert_eq!(band.objects.len(), 1);
        let socket = &band.objects[0];
        let downloads: u64 = 12 * ((8 << 20) + (1 << 20) + (64 << 10));
        assert!(socket.out_bytes > downloads && socket.out_bytes < downloads + downloads / 100);
        assert!(socket.in_bytes > 12 * (256 << 10));
        let lo = band
            .sinks
            .iter()
            .find(|sink| sink.name == "lo")
            .expect("loopback");
        let ratio = lo.bytes / total(&band);
        assert!((1.9..2.1).contains(&ratio), "lo / socket = {ratio}");
    }

    /// A recording with `io.syscall` and `net.socket` asked without `raw`,
    /// as the fixtures were recorded before every view asked for the rows.
    fn without_rows(name: &str) -> Data {
        let mut data = crate::snapshot::fixture(name);
        for results in &mut data.run.results {
            if ["io.syscall", "net.socket"].contains(&results.source.as_str()) {
                results
                    .outputs
                    .retain(|output| !matches!(output, crate::model::Output::Raw { .. }));
            }
        }
        data
    }

    /// A run with io.syscall without `raw`: the storage band says so where
    /// its files would be, and still draws the machine's devices.
    #[test]
    fn a_run_without_rows_says_why_its_files_are_missing() {
        let buffer = draw(without_rows("sqlite-commits"), "flows", WIDTH, HEIGHT);
        let screen = text(&buffer);
        assert!(
            screen.contains("bytes per file: not in this run"),
            "{screen}"
        );
        assert!(screen.contains("259:0"), "{screen}");
    }

    /// Down selects the next object; enter narrows every view to its thread.
    #[test]
    fn enter_focuses_the_selected_objects_thread() {
        let data = file_server();
        let focus = Focus::default();
        let ctx = ctx(&data, &focus);
        let mut view = Flows::default();
        assert_eq!(view.key(KeyEvent::from(KeyCode::Down), &ctx), Action::None);
        assert_eq!(view.selected, 1);
        let bands = bands(&ctx);
        let file = selected_object(&bands, 1).expect("the largest file");
        assert_eq!(file.in_bytes, 12 * (8 << 20));
        match view.key(KeyEvent::from(KeyCode::Enter), &ctx) {
            Action::Focus(focus) => {
                assert_eq!(focus.tid, file.tid);
                assert!(focus.pid.is_some());
            }
            other => panic!("{other:?}"),
        }
        for _ in 0..50 {
            view.key(KeyEvent::from(KeyCode::Down), &ctx);
        }
        assert_eq!(view.selected, selectable(&bands) - 1);
    }

    /// Narrowed to the main thread, the serving thread's socket and files are
    /// gone, and what is left is the main thread's own reads.
    #[test]
    fn a_focus_narrows_the_flows_to_its_thread() {
        let data = file_server();
        let all = Focus::default();
        let whole = storage(&ctx(&data, &all));
        let main = whole
            .tasks
            .iter()
            .find(|task| task.tid == task.pid)
            .expect("the main thread")
            .tid;
        let focus = Focus {
            tid: Some(main),
            ..Focus::default()
        };
        let narrowed = storage(&ctx(&data, &focus));
        assert!(narrowed.tasks.iter().all(|task| task.tid == main));
        assert!(total(&narrowed) < total(&whole) / 10.0);
        assert!(network(&ctx(&data, &focus)).objects.is_empty());
    }

    /// Half way through, live: the target's rows are there and the machine's
    /// timelines are said to come at the end.
    #[test]
    fn half_way_live_the_machine_comes_at_the_end() {
        let data = file_server();
        let middle = (data.opened_ns() + data.closed_ns()) / 2;
        let live = crate::feed::as_live_until(&data, middle);
        let focus = Focus::default();
        let band = storage(&ctx(&live, &focus));
        assert!(!band.objects.is_empty());
        assert!(band
            .machine_note
            .as_deref()
            .is_some_and(|note| note.contains("at its end")));
        let screen = text(&draw(live, "flows", WIDTH, HEIGHT));
        assert!(screen.contains("live"), "{screen}");
    }

    /// A run that did not ask for the target's sources says they were not
    /// measured; it never says that nothing moved.
    #[test]
    fn unmeasured_bytes_are_never_said_to_be_zero() {
        for name in ["sqlite-latency", "sqlite-commits", "machine"] {
            let screen = text(&draw(without_rows(name), "flows", WIDTH, HEIGHT));
            assert!(!screen.contains("no file"), "{name}: {screen}");
            assert!(
                screen.contains("bytes per file were not measured"),
                "{name}: {screen}"
            );
        }
        // Measured and empty, under a focus that matches no row: that one is
        // said to have moved nothing.
        let data = file_server();
        let focus = Focus {
            pid: Some(1),
            ..Focus::default()
        };
        let ctx = ctx(&data, &focus);
        let bands = bands(&ctx);
        assert_eq!(selectable(&bands), 0);
        assert_eq!(
            nothing_to_select(&bands),
            "nothing to select: no socket of the target's moved a byte; \
             no file of the target's moved a byte"
        );
    }

    /// Too short for two bands, the view says so in a sentence rather than
    /// draw bands that hold only their titles; at the least it says, both
    /// bands draw their diagrams and footers.
    #[test]
    fn below_its_least_size_the_view_says_so() {
        let mut least = None;
        for height in 10..30 {
            let screen = text(&draw(file_server(), "flows", WIDTH, height));
            if screen.contains("too small for the flows") {
                assert!(least.is_none(), "too small at {height} after drawn");
                continue;
            }
            least.get_or_insert(height);
            for footer in ["sockets ", "files "] {
                assert!(
                    screen
                        .lines()
                        .any(|line| line.trim_start_matches('│').starts_with(footer)),
                    "{height} rows, no `{footer}` footer:\n{screen}"
                );
            }
        }
        assert!(least.is_some(), "never drawn");
    }

    /// At the standard 80x24 the storage band cannot hold every file: what
    /// it leaves out is counted on screen with its bytes.
    #[test]
    fn a_node_the_rows_cannot_hold_is_counted_on_screen() {
        let data = file_server();
        let focus = Focus::default();
        let band = storage(&ctx(&data, &focus));
        let others = band.objects.last().expect("files");
        assert!(others.many > 1);
        let screen = text(&draw(data, "flows", 80, 24));
        let note = format!(
            "{} files ({}) not drawn",
            others.many,
            units::bytes(others.bytes() as f64)
        );
        assert!(screen.contains(&note), "{note}:\n{screen}");
        // Tall enough, nothing is left out and nothing is said.
        let screen = text(&draw(file_server(), "flows", WIDTH, 60));
        assert!(!screen.contains("not drawn"), "{screen}");
    }

    /// A file's `blk_devs` is the edge to the device `io.bytes` measured:
    /// the device says how many of the target's files sit above it, and the
    /// file's detail names the device.
    #[test]
    fn a_file_is_joined_to_the_device_under_it() {
        let data = crate::snapshot::fixture("file-serving");
        let focus = Focus::default();
        let band = storage(&ctx(&data, &focus));
        let fed: Vec<&Sink> = band
            .sinks
            .iter()
            .filter(|sink| sink.fed.is_some())
            .collect();
        assert_eq!(fed.len(), 1, "{:?}", band.sinks);
        assert_eq!(fed[0].key, "dev:259+0");
        let (files, bytes) = fed[0].fed.unwrap();
        assert!(files >= 1 && bytes > 0);
        assert!(band
            .objects
            .iter()
            .any(|object| object.detail.contains("above block device 259:0")));
        let line = sink_line(fed[0], Kind::Storage, &Trust::clean(), 40);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("◂"), "{text}");
    }

    /// Without io.fs, the storage band falls back on io.syscall's rows per
    /// descriptor: called descriptors, not files, and with no ratio to the
    /// devices, since the socket's bytes are among them.
    #[test]
    fn descriptors_are_not_compared_with_devices() {
        let mut data = file_server();
        data.run.results.retain(|results| results.source != "io.fs");
        let focus = Focus::default();
        let band = storage(&ctx(&data, &focus));
        assert!(band.measured && !band.comparable);
        assert_eq!(band.object_word, "descriptor");
        // The socket's bytes, sent and received, are read and written
        // through a descriptor too: the band holds them.
        let socket = network(&ctx(&data, &focus)).objects[0].bytes() as f64;
        assert!(total(&band) > socket, "{} <= {socket}", total(&band));
        let screen = text(&draw(data, "flows", 160, 50));
        assert!(screen.contains("descriptor"), "{screen}");
        assert!(screen.contains("no ratio"), "{screen}");
        let footer = screen
            .lines()
            .find(|line| line.trim_start_matches('│').starts_with("descriptors "))
            .expect("the storage footer");
        assert!(
            !footer.contains("x ·") && !footer.contains(" = "),
            "{footer}"
        );
    }

    /// An object's trust is every one of its results', not the first's.
    #[test]
    fn an_objects_trust_is_all_of_its_results() {
        let mut data = file_server();
        for results in &mut data.run.results {
            results.losses = Default::default();
        }
        let mut second = data.of_source("io.fs").next().expect("io.fs").clone();
        second.label = "io.fs again".to_owned();
        second.losses.dropped = 7;
        data.run.results.push(second);
        let focus = Focus::default();
        let band = storage(&ctx(&data, &focus));
        for object in &band.objects {
            assert!(
                object
                    .trust
                    .marks
                    .iter()
                    .any(|mark| matches!(mark, trust::Mark::Lost { .. })),
                "{}: {:?}",
                object.name,
                object.trust
            );
        }
    }

    #[test]
    fn rows_are_allotted_in_proportion_with_one_at_least() {
        assert_eq!(allot(&[90.0, 9.0, 1.0], 10), vec![8, 1, 1]);
        assert_eq!(allot(&[1.0, 1.0], 6), vec![3, 3]);
        assert_eq!(allot(&[5.0, 1.0, 1.0, 1.0], 3).iter().sum::<usize>(), 3);
        assert_eq!(allot(&[], 4), Vec::<usize>::new());
    }
}
