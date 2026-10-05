//! The `system` view: the overview: cpu, memory, disk, network, GPU and power, and the processes.
//!
//! A btop-like front page, drawn from what the run measured and nothing else.
//! Three things set it apart from a system monitor:
//!
//! - **One time axis.** Every chart spans the run, from its first window to
//!   its last (or to now, live), so in a column of panels a disk burst and
//!   the cpu that caused it sit one above the other.
//! - **Colour is how full.** A chart and its figures take the usage gradient
//!   ([`usage`]): green where the resource is free, through yellow, to red
//!   where it is full, by its share of a capacity the run knows (the
//!   machine's cores, an engine's time, the VRAM). A resource with no known
//!   capacity (energy, bytes on a device of unknown speed) keeps one neutral
//!   hue and its section says so. Trust does not wash the colour out: a lossy
//!   figure keeps its hue, is underlined, and carries its badge.
//!   `gpu.engine_busy`'s one-second sampling window is drawn as a ruler on
//!   the run's own time axis, not as a footnote.
//! - **What it costs.** A corner lists what each panel's collector cost, as a
//!   share of one cpu.
//!
//! It reads a finished run's `timeline`s and, live (when a result holds its
//! `raw` events only), the events themselves -- which say more: the cpu by
//! state, bytes read apart from written, in apart from out, a process's name.
//! What only the events say is drawn when they are there, and said to be
//! missing when they are not.

use std::collections::{BTreeMap, BTreeSet};

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::{Action, Ctx, Focus, Need, Region, Scope, View};
use crate::model::{Data, Event, Key, Output, Refusal, Results};
use crate::trust::{self, palette, usage, Trust};
use crate::units;

/// What this view needs measured.
pub const NEEDS: &[Need] = &[
    Need {
        source: "cpu.util",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "the cpu panel: utilization by state, and its sparkline",
    },
    Need {
        source: "mem.pressure",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "the memory panel: stall time under memory pressure",
    },
    Need {
        source: "io.bytes",
        mode: "poll@250ms",
        emit: &["raw", "timeline", "summary"],
        keys: &["dev", "dir"],
        stack: None,
        scope: Scope::System,
        why: "the disk panel: bytes read and written per device, apart, by the `dir` key",
    },
    Need {
        source: "net.bytes",
        mode: "poll@250ms",
        emit: &["raw", "timeline", "summary"],
        keys: &["iface", "dir"],
        stack: None,
        scope: Scope::System,
        why: "the network panel: bytes in and out per interface, apart, by the `dir` key",
    },
    Need {
        source: "gpu.engine_busy",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "the GPU panel: occupancy per engine",
    },
    Need {
        source: "gpu.mem",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "the GPU panel: memory held",
    },
    Need {
        source: "power.energy",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "the power panel: watts per RAPL domain",
    },
    Need {
        source: "cpu.oncpu_time",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &["task"],
        stack: None,
        scope: Scope::Target,
        why: "the process list: cpu time per thread",
    },
];

/// What the view shows, in one line.
const TITLE: &str = "the overview: cpu, memory, disk, network, GPU and power, and the processes";

/// The view's keys, for the help line.
const KEYS: &[(&str, &str)] = &[
    ("↑/↓", "process"),
    ("enter", "its ledger"),
    ("s", "sort"),
    ("i", "idle devices"),
];

/// What the process list can be sorted by, in the order `s` steps through.
const SORTS: &[Sort] = &[
    Sort::Cpu,
    Sort::Memory,
    Sort::Faults,
    Sort::Io,
    Sort::RunQueue,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sort {
    Cpu,
    Memory,
    Faults,
    Io,
    RunQueue,
}

impl Sort {
    fn column(self) -> Col {
        match self {
            Sort::Cpu => Col::CpuTime,
            Sort::Memory => Col::Rss,
            Sort::Faults => Col::Faults,
            Sort::Io => Col::InIo,
            Sort::RunQueue => Col::RunQueue,
        }
    }

    fn words(self) -> &'static str {
        match self {
            Sort::Cpu => "cpu",
            Sort::Memory => "memory held",
            Sort::Faults => "faults",
            Sort::Io => "time in I/O",
            Sort::RunQueue => "time waiting to run",
        }
    }
}

pub struct System {
    /// The selected row of the process list, by its id and whether it is a
    /// process's row, so a re-sort or a live update keeps the same row.
    selected: Option<(i32, bool)>,
    sort: usize,
    /// The columns the list was last drawn with: `s` steps only through
    /// the sorts whose column is on screen.
    shown: Vec<Col>,
    /// Whether devices and interfaces that moved nothing are listed one by
    /// one, rather than folded into one line.
    idle: bool,
    /// The process rows last drawn, each its index in the sorted list.
    regions: Vec<Region>,
}

impl View for System {
    fn name(&self) -> &'static str {
        "system"
    }

    fn title(&self) -> &'static str {
        TITLE
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        self.regions.clear();
        let axis = Axis::of(ctx.data);
        let procs = processes(ctx.data, SORTS[self.sort]);
        let held_machine = MACHINE
            .iter()
            .any(|(label, source)| served(ctx.data, label, source).is_some());
        let list_height = (procs.rows.len() as u16 + 4).clamp(7, 14);
        let top_height = if held_machine {
            let width = usize::from(area.width);
            let lines = |cpu_rows| Strips::new(ctx.data, width, cpu_rows, self.idle).build();
            let mut machine = lines(CPU_ROWS);
            let spare =
                usize::from(area.height).saturating_sub(machine.len() + usize::from(list_height));
            if spare > 0 {
                machine = lines(CPU_ROWS + spare.min(4));
            }
            let machine = machine.fit(usize::from(area.height.saturating_sub(5)));
            let height = machine.len() as u16;
            frame.render_widget(Paragraph::new(machine), Rect { height, ..area });
            height
        } else {
            let height = 5.min(area.height);
            draw_no_machine(frame, Rect { height, ..area }, ctx.data);
            height
        };
        // Below the machine, the rest; without it, what the list needs, so
        // a short run is not drawn as a tall empty frame.
        let rest = area.height - top_height;
        let bottom = Rect {
            y: area.y + top_height,
            height: if held_machine {
                rest
            } else {
                rest.min(list_height + 2)
            },
            ..area
        };
        let [list, corner] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(34.min(area.width / 3)),
        ])
        .areas(bottom);
        self.draw_processes(frame, list, ctx, &axis, &procs);
        draw_cost(frame, corner, ctx.data);
    }

    fn key(&mut self, key: KeyEvent, ctx: &Ctx<'_>) -> Action {
        let procs = processes(ctx.data, SORTS[self.sort]);
        let index = self
            .selected
            .and_then(|key| procs.rows.iter().position(|row| row.key() == key));
        let last = procs.rows.len().saturating_sub(1);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                let next = index.map_or(0, |index| (index + 1).min(last));
                self.selected = procs.rows.get(next).map(Proc::key);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let next = index.map_or(0, |index| index.saturating_sub(1));
                self.selected = procs.rows.get(next).map(Proc::key);
                Action::None
            }
            KeyCode::Enter => match procs.rows.get(index.unwrap_or(0)) {
                Some(row) => Action::Goto {
                    view: "ledger",
                    focus: row.focus(),
                },
                None => Action::None,
            },
            KeyCode::Char('s') => {
                // The next sort whose column is on screen: a list sorted by
                // a column the width hid would be in an order nothing shows.
                let shown = |sort: Sort| {
                    self.shown.is_empty()
                        || self.shown.contains(&sort.column())
                        || (sort == Sort::Cpu && self.shown.contains(&Col::Cpu))
                };
                self.sort = (1..=SORTS.len())
                    .map(|step| (self.sort + step) % SORTS.len())
                    .find(|&index| shown(SORTS[index]))
                    .unwrap_or(self.sort);
                Action::None
            }
            KeyCode::Char('i') => {
                self.idle = !self.idle;
                Action::None
            }
            _ => Action::None,
        }
    }

    fn keys(&self) -> &'static [(&'static str, &'static str)] {
        KEYS
    }

    fn regions(&self) -> &[Region] {
        &self.regions
    }

    fn select(&mut self, id: usize, ctx: &Ctx<'_>) -> Action {
        let procs = processes(ctx.data, SORTS[self.sort]);
        if let Some(row) = procs.rows.get(id) {
            self.selected = Some(row.key());
        }
        Action::None
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(System {
        selected: None,
        sort: 0,
        shown: Vec::new(),
        idle: false,
        regions: Vec::new(),
    })
}

/// The machine's panels, by the label `omn top` gives each and its source.
const MACHINE: &[(&str, &str)] = &[
    ("cpu.util@system", "cpu.util"),
    ("mem.pressure@system", "mem.pressure"),
    ("io.bytes@system", "io.bytes"),
    ("net.bytes@system", "net.bytes"),
    ("gpu.engine_busy@system", "gpu.engine_busy"),
    ("gpu.mem@system", "gpu.mem"),
    ("power.energy@system", "power.energy"),
];

// ---------------------------------------------------------------------------
// The data: a source's series, on the run's time axis.
// ---------------------------------------------------------------------------

/// How one point's value is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Per {
    /// A share of something, between 0 and 1: drawn as it is.
    Share,
    /// An amount since the last read (bytes, joules, nanoseconds of cpu):
    /// drawn per second, summed for a total.
    Tick,
    /// An amount held (bytes of memory): drawn as it is, the last is now.
    Level,
}

/// How a source's events are read into a series: the attribute `omn`'s
/// collector names as its value, and the parts only its events carry.
struct Reading {
    attr: &'static str,
    per: Per,
    /// Named parts of the value, each the sum of these attributes.
    parts: &'static [(&'static str, &'static [&'static str])],
    /// An attribute that says the ceiling of a level (the device's memory).
    ceiling: Option<&'static str>,
    /// For a source that reads several parts and reports them in one event
    /// (RAPL's domains through sysfs): the attribute listing the parts'
    /// names, and the prefix of each part's own value attribute
    /// (`domains: "package-0,dram"`, `domain.package_0`).
    fanout: Option<(&'static str, &'static str)>,
}

fn reading(source: &str) -> Reading {
    let plain = |attr, per| Reading {
        attr,
        per,
        parts: &[],
        ceiling: None,
        fanout: None,
    };
    match source {
        "cpu.util" => Reading {
            attr: "busy",
            per: Per::Share,
            parts: &[
                ("user", &["user", "nice"]),
                ("sys", &["system", "irq", "softirq"]),
                ("iowait", &["iowait"]),
                ("steal", &["steal"]),
            ],
            ceiling: None,
            fanout: None,
        },
        "mem.pressure" => plain("stall_ratio", Per::Share),
        "io.bytes" => Reading {
            attr: "bytes",
            per: Per::Tick,
            parts: &[("read", &["read_bytes"]), ("written", &["write_bytes"])],
            ceiling: None,
            fanout: None,
        },
        "net.bytes" => Reading {
            attr: "bytes",
            per: Per::Tick,
            parts: &[("in", &["rx"]), ("out", &["tx"])],
            ceiling: None,
            fanout: None,
        },
        "gpu.engine_busy" => plain("ratio", Per::Share),
        "gpu.mem" => Reading {
            attr: "bytes",
            per: Per::Level,
            parts: &[],
            ceiling: Some("device_total_bytes"),
            fanout: None,
        },
        "power.energy" => Reading {
            fanout: Some(("domains", "domain")),
            ..plain("joules", Per::Tick)
        },
        "cpu.oncpu_time" => plain("ns", Per::Tick),
        "mem.rss" => plain("bytes", Per::Level),
        "mem.faults" => plain("count", Per::Tick),
        "io.syscall" => plain("lat_ns", Per::Tick),
        "sched.runqlat" | "sched.futex" => plain("wait_ns", Per::Tick),
        _ => plain("value", Per::Level),
    }
}

/// One point: when, on the run's axis, and what.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Point {
    at_ns: u64,
    /// What was read: an amount since the last read, a share or a level.
    read: f64,
    /// What is drawn: per second for an amount, as read otherwise.
    shown: f64,
}

/// One series of one source.
#[derive(Debug, Clone, Default)]
struct Track {
    key: String,
    points: Vec<Point>,
    /// The value's parts, point for point, when the events carried them.
    parts: Vec<(&'static str, Vec<f64>)>,
    /// A text the events name the series with (a process's `comm`).
    name: Option<String>,
    /// The ceiling a level is held under, when the events say it.
    ceiling: Option<f64>,
    /// One of several parts a timeline carried in one series, told apart by
    /// their order at each instant: `key` is its place (`#1`), not its name.
    lane: bool,
    /// Several parts shared the series and could not be told apart: at
    /// most this many at one instant. Nothing is drawn from it.
    tangled: Option<usize>,
}

impl Track {
    fn total(&self) -> f64 {
        self.points.iter().map(|point| point.read).sum()
    }

    fn peak(&self) -> f64 {
        self.points
            .iter()
            .map(|point| point.shown)
            .filter(|shown| shown.is_finite())
            .fold(0.0, f64::max)
    }

    fn mean(&self) -> f64 {
        let finite: Vec<f64> = self
            .points
            .iter()
            .map(|point| point.shown)
            .filter(|shown| shown.is_finite())
            .collect();
        if finite.is_empty() {
            return 0.0;
        }
        finite.iter().sum::<f64>() / finite.len() as f64
    }

    fn last(&self) -> Option<f64> {
        self.points.last().map(|point| point.shown)
    }

    /// Each part's sum over the whole series, by name.
    fn part_totals(&self) -> Vec<(&'static str, f64)> {
        self.parts
            .iter()
            .map(|(name, values)| (*name, values.iter().sum::<f64>()))
            .collect()
    }
}

/// The series of a result: from its `raw` events when it kept them -- a live
/// run, or one that asked for them, and the events say more -- and from its
/// `timeline` otherwise.
fn tracks(data: &Data, results: &Results) -> Vec<Track> {
    let reading = reading(&results.source);
    let timeline = results.outputs.iter().find_map(|output| match output {
        Output::Timeline { series, .. } => Some(series),
        _ => None,
    });
    let raw = results.events();
    if !raw.is_empty() || timeline.is_none() {
        return fold_dirs(from_events(data, raw, &reading), &reading);
    }
    let timeline = timeline.map(Vec::as_slice).unwrap_or_default();
    let offsets: Vec<u64> = timeline
        .iter()
        .flat_map(|series| series.points.iter().map(|point| point.offset_ns))
        .collect();
    let origin = timeline_origin(data, results, &offsets);
    let mut tracks = Vec::new();
    for series in timeline {
        let points: Vec<Point> = series
            .points
            .iter()
            .map(|point| Point {
                at_ns: origin + point.offset_ns,
                read: point.value,
                shown: point.value,
            })
            .collect();
        match lanes(points) {
            Lanes::One(mut points) => {
                per_second(&mut points, reading.per, &[]);
                tracks.push(Track {
                    key: series.key.clone(),
                    points,
                    ..Track::default()
                });
            }
            Lanes::Several(lanes) => {
                for (index, mut points) in lanes.into_iter().enumerate() {
                    per_second(&mut points, reading.per, &[]);
                    tracks.push(Track {
                        key: format!("#{}", index + 1),
                        points,
                        lane: true,
                        ..Track::default()
                    });
                }
            }
            Lanes::Tangled(most) => tracks.push(Track {
                key: series.key.clone(),
                tangled: Some(most),
                ..Track::default()
            }),
        }
    }
    fold_dirs(tracks, &reading)
}

/// The parts of a series key. `omn` joins a timeline's axes with a space
/// (`dev:259+0 dir:read`, `src/emit.rs`'s series key) and this view joins an
/// event's keys with a comma; a space inside a part that is not a key (a
/// RAPL domain's `package 1`) keeps the part whole.
fn key_parts(key: &str) -> Vec<&str> {
    key.split(',')
        .flat_map(|piece| {
            let words: Vec<&str> = piece.split(' ').filter(|word| !word.is_empty()).collect();
            if words.len() > 1 && words.iter().all(|word| word.contains(':')) {
                words
            } else {
                vec![piece]
            }
        })
        .filter(|part| !part.is_empty())
        .collect()
}

/// Each direction a `dir` key names, as the part of the bytes it is: the
/// two the device's own total adds up (`/proc/diskstats`' read and written
/// sectors, `/proc/net/dev`'s received and sent). A discard or a flush is
/// not in that total, so it is not folded into it.
const DIRS: &[(&str, &str)] = &[
    ("read", "read"),
    ("write", "written"),
    ("rx", "in"),
    ("tx", "out"),
];

/// A device's directions being folded: its name, and each instant's point
/// per part.
type Folded = (Option<String>, BTreeMap<u64, Vec<Point>>);

/// Series split by a `dir` key folded back into one per device, each
/// direction one of its parts: a recording that kept only its timeline says
/// read apart from written as its events would. Series with no `dir` part
/// are kept as they are.
fn fold_dirs(tracks: Vec<Track>, reading: &Reading) -> Vec<Track> {
    if reading.parts.is_empty()
        || !tracks.iter().any(|track| {
            key_parts(&track.key)
                .iter()
                .any(|part| part.starts_with("dir:"))
        })
    {
        return tracks;
    }
    let names: Vec<&'static str> = reading.parts.iter().map(|(name, _)| *name).collect();
    let mut kept = Vec::new();
    // By the key without its direction: each part's points by instant.
    let mut folded: BTreeMap<String, Folded> = BTreeMap::new();
    for track in tracks {
        let parts = key_parts(&track.key);
        let Some(dir) = parts.iter().find_map(|part| part.strip_prefix("dir:")) else {
            kept.push(track);
            continue;
        };
        let rest: Vec<&str> = parts
            .iter()
            .copied()
            .filter(|part| !part.starts_with("dir:"))
            .collect();
        let rest = if rest.is_empty() {
            "system".to_owned()
        } else {
            rest.join(",")
        };
        let entry = folded.entry(rest).or_default();
        if entry.0.is_none() {
            entry.0.clone_from(&track.name);
        }
        let Some(index) = DIRS
            .iter()
            .find(|(named, _)| *named == dir)
            .and_then(|(_, part)| names.iter().position(|name| name == part))
        else {
            // A discard or a flush: the device is listed, its bytes are not
            // in the total.
            continue;
        };
        for point in track.points {
            let slots = entry.1.entry(point.at_ns).or_insert_with(|| {
                vec![
                    Point {
                        at_ns: point.at_ns,
                        read: 0.0,
                        shown: 0.0,
                    };
                    names.len()
                ]
            });
            slots[index] = point;
        }
    }
    for (key, (name, instants)) in folded {
        let mut points = Vec::with_capacity(instants.len());
        let mut parts: Vec<(&'static str, Vec<f64>)> =
            names.iter().map(|name| (*name, Vec::new())).collect();
        for (at_ns, slots) in instants {
            points.push(Point {
                at_ns,
                read: slots.iter().map(|slot| slot.read).sum(),
                shown: slots.iter().map(|slot| slot.shown).sum(),
            });
            for ((_, values), slot) in parts.iter_mut().zip(&slots) {
                values.push(slot.read);
            }
        }
        kept.push(Track {
            key,
            points,
            parts,
            name,
            ..Track::default()
        });
    }
    kept
}

/// Where a finished run's timeline starts, on the run's axis.
///
/// A timeline's offsets are measured from its own result's first event, not
/// from the run's opening (`src/emit.rs`, `first_ts_ns`), and the result does
/// not record when that event was. For a poller, the window's opening and
/// its close bound it: its first round of reads is at the opening, and its
/// last lands at the close (`the_runner_closes_the_window`). A collector that
/// reports its first read emits there; one that keeps it as a baseline -- a
/// ratio or a difference needs two reads -- emits one read later. So the
/// first event is at the opening or one read after it, and only the second
/// puts the last read at the close: that is the one taken when it fits.
///
/// Only a source of the machine is placed this way. A target's last read is
/// its last before it exits, not the run's close, so nothing ties its end
/// down; its timelines start at the opening, which is right for every
/// per-task source this view reads (none keeps its first read back).
fn timeline_origin(data: &Data, results: &Results, offsets: &[u64]) -> u64 {
    // Said by the run, when it says it: the inference below is for a
    // recording made before `omn` wrote the timeline's first event.
    if let Some(first) = results.outputs.iter().find_map(|output| match output {
        Output::Timeline {
            first_ts_ns: Some(first),
            ..
        } => Some(*first),
        _ => None,
    }) {
        return first.saturating_sub(data.opened_ns());
    }
    let opened = results.window.opened_ns.saturating_sub(data.opened_ns());
    if names_a_task(results) || results.window.closed_ns == 0 {
        return opened;
    }
    let mut distinct = offsets.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    let mut gaps: Vec<u64> = distinct.windows(2).map(|pair| pair[1] - pair[0]).collect();
    gaps.sort_unstable();
    let (Some(&read), Some(&last)) = (gaps.get(gaps.len() / 2), distinct.last()) else {
        return opened;
    };
    let closed = results.window.closed_ns.saturating_sub(data.opened_ns());
    // A read lands a few milliseconds either side of its tick; a quarter of
    // one is far from both answers, which are a whole read apart.
    if opened + read + last <= closed + read / 4 {
        opened + read
    } else {
        opened
    }
}

/// How a timeline's points sit: one per instant, or several -- a source
/// keyed on nothing that reports several parts on each read (RAPL's
/// domains through the perf unit, a GPU's engines) shares one series
/// between them, every part of one read at the same instant.
enum Lanes {
    One(Vec<Point>),
    /// The same number at every instant, in the same order each time (a
    /// collector reads its parts in a fixed order): one lane per part.
    Several(Vec<Vec<Point>>),
    /// A different number at some instants (a part that failed a read): the
    /// parts cannot be told apart. The most there were at one instant.
    Tangled(usize),
}

fn lanes(points: Vec<Point>) -> Lanes {
    let mut instants: Vec<Vec<Point>> = Vec::new();
    for point in points {
        match instants.last_mut() {
            Some(instant) if instant[0].at_ns == point.at_ns => instant.push(point),
            _ => instants.push(vec![point]),
        }
    }
    let most = instants.iter().map(Vec::len).max().unwrap_or(0);
    if most <= 1 {
        return Lanes::One(instants.into_iter().flatten().collect());
    }
    if instants.iter().any(|instant| instant.len() != most) {
        return Lanes::Tangled(most);
    }
    let mut lanes = vec![Vec::new(); most];
    for instant in instants {
        for (lane, point) in lanes.iter_mut().zip(instant) {
            lane.push(point);
        }
    }
    Lanes::Several(lanes)
}

/// Whether a result is a task's: its series are keyed by one.
fn names_a_task(results: &Results) -> bool {
    let keyed = |key: &str| key_parts(key).iter().any(|part| part.starts_with("task:"));
    results.outputs.iter().any(|output| match output {
        Output::Timeline { series, .. } => series.iter().any(|series| keyed(&series.key)),
        Output::Summary { series } => series.iter().any(|series| keyed(&series.key)),
        Output::Raw { events } => events.iter().any(|event| event.tid().is_some()),
        _ => false,
    })
}

/// The series of a run's events, grouped as `omn` groups a timeline: by the
/// event's keys, or -- for a source keyed on nothing that names a domain --
/// by that name. An event that carries several parts' values (RAPL's
/// domains through sysfs) is one point of each part's series.
fn from_events(data: &Data, events: &[Event], reading: &Reading) -> Vec<Track> {
    let opened = data.opened_ns();
    let mut grouped: BTreeMap<String, (Vec<Option<f64>>, Track)> = BTreeMap::new();
    for event in events {
        for (key, value) in values_of(event, reading) {
            add_point(&mut grouped, event, key, value, opened, reading);
        }
    }
    grouped
        .into_values()
        .map(|(intervals, mut track)| {
            // A part no event carried is not a part of zero: it is not there.
            track
                .parts
                .retain(|(_, values)| values.iter().any(|value| value.is_finite()));
            for (_, values) in &mut track.parts {
                for value in values.iter_mut().filter(|value| !value.is_finite()) {
                    *value = 0.0;
                }
            }
            per_second(&mut track.points, reading.per, &intervals);
            track
        })
        .collect()
}

/// What one event says, by the series it belongs to: its value under its
/// key, or, for an event carrying several parts, each part's under its name.
fn values_of(event: &Event, reading: &Reading) -> Vec<(String, f64)> {
    let listed = reading.fanout.and_then(|(list, prefix)| {
        let names = event.attr(list).and_then(|value| value.as_str())?;
        let values: Vec<(String, f64)> = names
            .split(',')
            .filter(|name| !name.is_empty())
            .filter_map(|name| {
                event
                    .number(&format!("{prefix}.{}", attribute_part(name)))
                    .map(|value| (name.to_owned(), value))
            })
            .collect();
        (!values.is_empty()).then_some(values)
    });
    match listed {
        Some(values) => values,
        None => event
            .number(reading.attr)
            .map(|value| vec![(series_key(event), value)])
            .unwrap_or_default(),
    }
}

/// A part's name as its attribute spells it: lowercase, everything but
/// letters and digits an underscore (`src/collect/power/sysfs.rs`,
/// `attribute_name`).
fn attribute_part(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// One point of one event, filed under `key`.
fn add_point(
    grouped: &mut BTreeMap<String, (Vec<Option<f64>>, Track)>,
    event: &Event,
    key: String,
    value: f64,
    opened: u64,
    reading: &Reading,
) {
    let (intervals, track) = grouped.entry(key.clone()).or_insert_with(|| {
        (
            Vec::new(),
            Track {
                key,
                parts: reading
                    .parts
                    .iter()
                    .map(|(name, _)| (*name, Vec::new()))
                    .collect(),
                ..Track::default()
            },
        )
    });
    intervals.push(event.number("interval_ns"));
    track.points.push(Point {
        at_ns: event.ts_ns.saturating_sub(opened),
        read: value,
        shown: value,
    });
    for ((_, attrs), (_, values)) in reading.parts.iter().zip(track.parts.iter_mut()) {
        let carried: Vec<f64> = attrs.iter().filter_map(|attr| event.number(attr)).collect();
        if carried.is_empty() {
            // Kept apart from a zero, to be dropped below if no event
            // carries the part at all.
            values.push(f64::NAN);
        } else {
            values.push(carried.iter().sum());
        }
    }
    if let Some(name) = event.attr("comm").and_then(|value| value.as_str()) {
        track.name = Some(name.to_owned());
    }
    if let Some(ceiling) = reading.ceiling.and_then(|attr| event.number(attr)) {
        track.ceiling = Some(ceiling);
    }
}

/// The key a timeline would file this event under -- or, for a source keyed
/// on nothing that names a domain on each event (RAPL through the perf unit:
/// `domain: package`, `package: 1`), that domain, and its package past the
/// first, so two domains read at one instant are two series.
fn series_key(event: &Event) -> String {
    if event.keys.is_empty() {
        let Some(domain) = event.attr("domain").and_then(|value| value.as_str()) else {
            return "system".to_owned();
        };
        return match event.number("package").filter(|package| *package > 0.0) {
            Some(package) => format!("{domain} {package}"),
            None => domain.to_owned(),
        };
    }
    event
        .keys
        .iter()
        .map(key_text)
        .collect::<Vec<_>>()
        .join(",")
}

/// A key as a series names it. A key this side's mirror does not know is
/// kept as its JSON; a direction is spelled as `omn` spells it in a
/// timeline (`dir:read`), so an event and a series of one direction agree.
fn key_text(key: &Key) -> String {
    if let Key::Other(value) = key {
        if let Some(dir) = value
            .get("dir")
            .and_then(|dir| dir.get("dir").or(Some(dir)))
            .and_then(|dir| dir.as_str())
        {
            return format!("dir:{dir}");
        }
    }
    key.to_string()
}

/// Turns amounts since the last read into amounts per second: by the
/// interval the event says it covers, or the spacing of the reads, or --
/// for the first, which has no read before it -- the reads' usual spacing.
fn per_second(points: &mut [Point], per: Per, intervals: &[Option<f64>]) {
    if per != Per::Tick {
        return;
    }
    let mut spacing: Vec<u64> = points
        .windows(2)
        .map(|pair| pair[1].at_ns.saturating_sub(pair[0].at_ns))
        .filter(|gap| *gap > 0)
        .collect();
    spacing.sort_unstable();
    let typical = spacing.get(spacing.len() / 2).copied();
    for index in 0..points.len() {
        let interval = intervals
            .get(index)
            .copied()
            .flatten()
            .filter(|ns| *ns > 0.0)
            .or_else(|| {
                (index > 0)
                    .then(|| points[index].at_ns.saturating_sub(points[index - 1].at_ns))
                    .filter(|gap| *gap > 0)
                    .map(|gap| gap as f64)
            })
            .or(typical.map(|gap| gap as f64));
        points[index].shown = match interval {
            Some(ns) => points[index].read * 1e9 / ns,
            None => f64::NAN,
        };
    }
}

/// What the run holds for one of the view's needs: its results and their
/// trust, or its refusal.
enum Held<'a> {
    /// The results, their trust, and whether their scope is assumed: no
    /// result carries the machine's label (`omn top` gives every machine
    /// source one), so these are the source's results whose series name no
    /// task -- the machine's, most likely, but a cgroup's would look the
    /// same, and the run does not say.
    Served(Vec<&'a Results>, Trust, bool),
    Refused(&'a Refusal),
}

/// What the run holds of a machine section's source: the results labelled
/// as the machine's, or, in a run that labels none, the source's results
/// that are not a task's. A task's result (`gpu.mem` on the target) is
/// never drawn as the machine's.
fn served<'a>(data: &'a Data, label: &str, source: &str) -> Option<Held<'a>> {
    let labelled: Vec<&Results> = data
        .run
        .results
        .iter()
        .filter(|results| results.label == label)
        .collect();
    let assumed = labelled.is_empty();
    let held: Vec<&Results> = if assumed {
        data.run
            .results
            .iter()
            .filter(|results| results.source == source && !names_a_task(results))
            .collect()
    } else {
        labelled
    };
    held_or_refused(data, held, label, source, assumed)
}

/// What the run holds of a source on the target: its results not labelled
/// as the machine's.
fn on_target<'a>(data: &'a Data, source: &str) -> Option<Held<'a>> {
    let held: Vec<&Results> = data
        .run
        .results
        .iter()
        .filter(|results| results.source == source && !results.label.ends_with("@system"))
        .collect();
    held_or_refused(data, held, source, source, false)
}

fn held_or_refused<'a>(
    data: &'a Data,
    held: Vec<&'a Results>,
    label: &str,
    source: &str,
    assumed: bool,
) -> Option<Held<'a>> {
    if held.is_empty() {
        // A source refused in one scope may be served in another (`gpu.mem`
        // on the target and on the machine): only a source with no result
        // is drawn as its refusal.
        return data
            .refused
            .iter()
            .find(|refusal| refusal.label == label || refusal.source == source)
            .map(Held::Refused);
    }
    let trust = Trust::joined(data, held.iter().copied());
    Some(Held::Served(held, trust, assumed))
}

/// A section's note, saying so when its scope is assumed.
fn unlabelled(note: &str, assumed: bool) -> String {
    if assumed {
        format!("{note} (scope not recorded: taken as the machine's)")
    } else {
        note.to_owned()
    }
}

/// The run's time axis: how long it is.
#[derive(Debug, Clone, Copy)]
struct Axis {
    span_ns: u64,
}

impl Axis {
    fn of(data: &Data) -> Axis {
        Axis {
            span_ns: data.closed_ns().saturating_sub(data.opened_ns()).max(1),
        }
    }

    /// One value per cell: the largest drawn value whose point falls in the
    /// cell's slice of the run -- a spike is never averaged away -- or, for
    /// a cell no point falls in, the value of the point before it, held
    /// until the next. A value is held across the reads' usual spacing and
    /// a half, no further: a longer gap is reads that did not happen (lost,
    /// or a source that paused), and it stays blank. Nothing after the last
    /// point: a live run stops where it is.
    fn cells(&self, points: &[Point], width: usize) -> Vec<Option<f64>> {
        let mut cells = vec![None; width];
        let points: Vec<&Point> = points
            .iter()
            .filter(|point| point.shown.is_finite())
            .collect();
        if width == 0 || points.is_empty() {
            return cells;
        }
        let cell_ns = self.span_ns as f64 / width as f64;
        let cell_of = |at_ns: u64| (at_ns as f64 / cell_ns).clamp(0.0, width as f64 - 1.0) as usize;
        // The latest point of each cell, which a value is held from.
        let mut latest: Vec<Option<u64>> = vec![None; width];
        for point in &points {
            let cell = cell_of(point.at_ns);
            cells[cell] = Some(cells[cell].map_or(point.shown, |held: f64| held.max(point.shown)));
            latest[cell] = Some(latest[cell].map_or(point.at_ns, |at: u64| at.max(point.at_ns)));
        }
        let mut gaps: Vec<u64> = points
            .windows(2)
            .map(|pair| pair[1].at_ns.saturating_sub(pair[0].at_ns))
            .filter(|gap| *gap > 0)
            .collect();
        gaps.sort_unstable();
        let reach = gaps
            .get(gaps.len() / 2)
            .map_or(f64::INFINITY, |gap| *gap as f64 * 1.5);
        let first = cell_of(points[0].at_ns);
        let last = cell_of(points[points.len() - 1].at_ns);
        let mut held: Option<(f64, u64)> = None;
        for index in first..=last {
            match (cells[index], latest[index]) {
                (Some(value), Some(at)) => held = Some((value, at)),
                _ => {
                    let start = index as f64 * cell_ns;
                    cells[index] = held
                        .filter(|(_, at)| start - *at as f64 <= reach)
                        .map(|(value, _)| value);
                }
            }
        }
        cells
    }
}

// ---------------------------------------------------------------------------
// Drawing: charts.
// ---------------------------------------------------------------------------

const EIGHTHS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// An area chart of `rows` lines, each cell filled in eighths of a row up to
/// its value against `max`. A cell with something in it is never drawn
/// empty; a cell measured at zero is a dot on the baseline, and a cell with
/// nothing measured is blank.
fn chart(cells: &[Option<f64>], max: f64, rows: usize, style: Style) -> Vec<Line<'static>> {
    let eighths: Vec<Option<usize>> = cells
        .iter()
        .map(|cell| {
            cell.map(|value| {
                if max <= 0.0 || value <= 0.0 {
                    0
                } else {
                    (((value / max).min(1.0) * (rows * 8) as f64).round() as usize).max(1)
                }
            })
        })
        .collect();
    (0..rows)
        .map(|row| {
            // Row 0 is the top line.
            let floor = (rows - 1 - row) * 8;
            let text: String = eighths
                .iter()
                .map(|filled| match filled {
                    None => ' ',
                    Some(0) if row == rows - 1 => '·',
                    Some(filled) => EIGHTHS[filled.saturating_sub(floor).min(8)],
                })
                .collect();
            Line::from(Span::styled(text, style))
        })
        .collect()
}

/// [`chart`] in the usage gradient: each cell takes the colour of its own
/// value's share of `capacity` (green free, red full), or one neutral hue
/// when the capacity is not known; the trust underlines rather than dims, so
/// a lossy chart keeps its colour ([`Trust::hued`]).
fn usage_chart(
    cells: &[Option<f64>],
    max: f64,
    rows: usize,
    trust: &Trust,
    capacity: Option<f64>,
) -> Vec<Line<'static>> {
    let plain = chart(cells, max, rows, Style::new());
    plain
        .into_iter()
        .map(|line| {
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut run = String::new();
            let mut run_style: Option<Style> = None;
            for (glyph, cell) in text
                .chars()
                .zip(cells.iter().chain(std::iter::repeat(&None)))
            {
                let style = trust.hued(usage::of(cell.unwrap_or(0.0), capacity));
                if run_style != Some(style) {
                    if let Some(done) = run_style {
                        spans.push(Span::styled(std::mem::take(&mut run), done));
                    }
                    run_style = Some(style);
                }
                run.push(glyph);
            }
            if let Some(done) = run_style {
                spans.push(Span::styled(run, done));
            }
            Line::from(spans)
        })
        .collect()
}

/// The style of a figure in the usage gradient: see [`usage_chart`].
fn hue(trust: &Trust, value: f64, capacity: Option<f64>) -> Style {
    trust.hued(usage::of(value, capacity))
}

/// The smallest round ceiling at or above `peak`: 1, 2 or 5 times a power of
/// ten, so an axis reads at a glance.
fn ceiling(peak: f64) -> f64 {
    if peak.is_nan() || peak <= 0.0 || peak.is_infinite() {
        return 1.0;
    }
    let power = 10f64.powf(peak.log10().floor());
    for step in [1.0, 2.0, 5.0] {
        if step * power >= peak * 0.999_999 {
            return step * power;
        }
    }
    10.0 * power
}

/// A panel's frame: its name, a note, and its trust's badges on the right.
fn panel<'a>(name: &str, note: &str, trust: Option<&Trust>) -> Block<'a> {
    let mut title = vec![Span::styled(
        format!(" {name} "),
        Style::new().add_modifier(Modifier::BOLD),
    )];
    if !note.is_empty() {
        title.push(Span::styled(format!("{note} "), dim()));
    }
    let mut block = Block::new().borders(Borders::ALL).title(Line::from(title));
    if let Some(trust) = trust {
        let badges = trust.badges();
        if !badges.is_empty() {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(badges);
            spans.push(Span::raw(" "));
            block = block.title_top(Line::from(spans).right_aligned());
        }
    }
    block
}

/// The width of a row's label, before its chart.
const LABEL: usize = 10;
/// The most cells a row's figure is given, after its chart: a third of the
/// box on a narrow terminal.
const FIGURE: usize = 36;
/// The fewest rows the cpu chart is drawn on.
const CPU_ROWS: usize = 5;

/// A label of exactly `LABEL` cells.
fn label(text: &str) -> Span<'static> {
    let text: String = text.chars().take(LABEL - 1).collect();
    Span::raw(format!("{text:<LABEL$}"))
}

/// A label in the dim style, for a row that continues the one above.
fn dim_label(text: &str) -> Span<'static> {
    let text: String = text.chars().take(LABEL - 1).collect();
    Span::styled(format!("{text:<LABEL$}"), dim())
}

/// Spans cut to exactly `width` cells: padded, or cut with `…`.
fn fit(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let span_width = span.width();
        if used + span_width <= width {
            used += span_width;
            out.push(span);
            continue;
        }
        let room = width - used;
        let mut text = String::new();
        let mut taken = 0;
        for ch in span.content.chars() {
            let cell = Span::raw(ch.to_string()).width();
            if taken + cell + 1 > room {
                break;
            }
            text.push(ch);
            taken += cell;
        }
        if room > 0 {
            text.push('…');
            taken += 1;
        }
        out.push(Span::styled(text, span.style));
        used += taken;
        break;
    }
    if used < width {
        out.push(Span::raw(" ".repeat(width - used)));
    }
    out
}

/// The time axis under the charts: the run's start, its middle and its end.
fn time_axis(axis: &Axis, width: usize, live: bool) -> Vec<Span<'static>> {
    let end = if live {
        format!("now {}", units::duration(axis.span_ns as f64))
    } else {
        units::duration(axis.span_ns as f64)
    };
    let middle = units::duration(axis.span_ns as f64 / 2.0);
    let mut text = vec!['─'; width];
    let mut put = |at: usize, word: &str| {
        for (offset, ch) in word.chars().enumerate() {
            if let Some(cell) = text.get_mut(at + offset) {
                *cell = ch;
            }
        }
    };
    put(0, "└0s");
    let end_at = width.saturating_sub(end.chars().count() + 1);
    let middle_at = (width / 2).saturating_sub(middle.chars().count() / 2);
    if middle_at > 5 && middle_at + middle.chars().count() + 2 < end_at {
        put(middle_at, &middle);
    }
    put(end_at, &format!("{end}┘"));
    vec![Span::styled(text.into_iter().collect::<String>(), dim())]
}

/// A share as a person reads it, a tiny one included: `<0.01%`, not `0.00%`.
fn share(value: f64) -> String {
    if value > 0.0 && value < 0.0001 {
        "<0.01%".to_owned()
    } else {
        units::percent(value * 100.0)
    }
}

/// Watts, to three digits.
fn watts(value: f64) -> String {
    if !value.is_finite() {
        "?W".to_owned()
    } else if value >= 100.0 {
        format!("{value:.0}W")
    } else if value >= 10.0 {
        format!("{value:.1}W")
    } else {
        format!("{value:.2}W")
    }
}

// ---------------------------------------------------------------------------
// Drawing: the machine, one section per resource, on one time axis.
// ---------------------------------------------------------------------------

/// The machine's box, line by line: one section per resource, each chart in
/// the same columns, so one moment is one column from top to bottom.
struct Strips<'a> {
    data: &'a Data,
    axis: Axis,
    /// The box's width inside its borders.
    inner: usize,
    /// Every chart's width.
    chart: usize,
    cpu_rows: usize,
    idle: bool,
    lines: Vec<Line<'static>>,
    /// Where each section starts in `lines`, by name.
    sections: Vec<(String, usize)>,
}

impl<'a> Strips<'a> {
    fn new(data: &'a Data, width: usize, cpu_rows: usize, idle: bool) -> Strips<'a> {
        let inner = width.saturating_sub(2);
        Strips {
            data,
            axis: Axis::of(data),
            inner,
            chart: inner
                .saturating_sub(LABEL + 1 + (inner / 3).clamp(16, FIGURE))
                .max(8),
            cpu_rows,
            idle,
            lines: Vec::new(),
            sections: Vec::new(),
        }
    }

    /// Everything, in order.
    fn build(mut self) -> Machine {
        // The view's own name and title on the box, as every view frames
        // itself (`┌ system -- ...`).
        self.section("system --", TITLE, None);
        self.cpu();
        self.gpu();
        self.memory();
        self.power();
        self.devices(&DISK);
        self.devices(&NETWORK);
        let mut axis = vec![Span::raw(" ".repeat(LABEL))];
        axis.extend(time_axis(&self.axis, self.chart, self.data.live.is_some()));
        axis.push(Span::styled(
            format!(
                " a cell is {}, drawn at its peak",
                units::duration(self.axis.span_ns as f64 / self.chart as f64)
            ),
            dim(),
        ));
        self.lines.push(boxed(axis, self.inner));
        self.lines
            .push(Line::from(format!("└{}┘", "─".repeat(self.inner))));
        Machine {
            lines: self.lines,
            sections: self.sections,
            inner: self.inner,
        }
    }

    /// A rule across the box naming a section, its trust on the right:
    /// `├ disk  bytes moved ───── ~low det ┤`.
    fn section(&mut self, name: &str, note: &str, trust: Option<&Trust>) {
        let (open, close) = if self.lines.is_empty() {
            ("┌", "┐")
        } else {
            self.sections.push((name.to_owned(), self.lines.len()));
            ("├", "┤")
        };
        let mut right = Vec::new();
        if let Some(badges) = trust.map(Trust::badges).filter(|badges| !badges.is_empty()) {
            right.push(Span::raw(" "));
            right.extend(badges);
            right.push(Span::raw(" "));
        }
        right.push(Span::raw(close));
        let right_width: usize = right.iter().map(Span::width).sum();
        let mut left = vec![
            Span::raw(format!("{open} ")),
            Span::styled(name.to_owned(), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" "),
        ];
        if !note.is_empty() {
            left.push(Span::styled(format!("{note} "), dim()));
        }
        let left_width: usize = left.iter().map(Span::width).sum();
        let total = self.inner + 2;
        let mut spans = if left_width + right_width + 1 > total {
            fit(left, total.saturating_sub(right_width + 1))
        } else {
            left
        };
        let used: usize = spans.iter().map(Span::width).sum();
        spans.push(Span::raw(
            "─".repeat(total.saturating_sub(used + right_width)),
        ));
        spans.extend(right);
        self.lines.push(Line::from(spans));
    }

    /// A row: its label, its chart in the chart's columns, its figure.
    fn row(&mut self, label: Span<'static>, chart: Vec<Span<'static>>, figure: Vec<Span<'static>>) {
        let mut spans = vec![label];
        spans.extend(fit(chart, self.chart));
        spans.push(Span::raw(" "));
        spans.extend(figure);
        self.lines.push(boxed(spans, self.inner));
    }

    /// A section the run does not hold: its refusal, in the refusal's own
    /// words, or that nothing asked for it.
    fn missing(&mut self, name: &str, held: Option<Held<'_>>) {
        self.section(name, "", None);
        let text = match held {
            Some(Held::Refused(refusal)) => Span::styled(
                format!("{} refused: {}", refusal.source, refusal.reason),
                Style::new().fg(palette::REFUSED),
            ),
            _ => Span::styled("not in this run", dim()),
        };
        self.lines.push(boxed(vec![label(""), text], self.inner));
    }

    fn tracks_of(&self, results: &[&Results]) -> Vec<Track> {
        results
            .iter()
            .flat_map(|results| tracks(self.data, results))
            .collect()
    }

    fn cpu(&mut self) {
        let held = served(self.data, "cpu.util@system", "cpu.util");
        let Some(Held::Served(results, trust, assumed)) = held else {
            return self.missing("cpu", held);
        };
        let all = self.tracks_of(&results);
        let track = all
            .iter()
            .find(|track| track.key == "system")
            .or(all.first())
            .cloned()
            .unwrap_or_default();
        self.section(
            "cpu",
            &unlabelled("busy, as a share of the whole machine", assumed),
            Some(&trust),
        );
        // A share of the whole machine: its capacity is one.
        let whole_machine = Some(1.0);
        let style = trust.hued(usage::NEUTRAL);
        let top = ceiling(track.peak());
        let cells = self.axis.cells(&track.points, self.chart);
        let name = |word: &str| Span::styled(format!("{word:<5}"), dim());
        // The last read is "now" only while the run is still going.
        let last = if self.data.live.is_some() {
            "now"
        } else {
            "last"
        };
        let mut figures = vec![
            vec![
                name(last),
                Span::styled(
                    track.last().map_or("-".to_owned(), share),
                    hue(&trust, track.last().unwrap_or(0.0), whole_machine)
                        .add_modifier(Modifier::BOLD),
                ),
            ],
            vec![
                name("mean"),
                Span::styled(
                    share(track.mean()),
                    hue(&trust, track.mean(), whole_machine),
                ),
            ],
            vec![
                name("peak"),
                Span::styled(
                    share(track.peak()),
                    hue(&trust, track.peak(), whole_machine),
                ),
            ],
        ];
        // The kernel's own account of the same machine over the window, from
        // the run's cost: busy cpu-seconds per second, the figure a person
        // holds against a process's "100 %".
        let busy_cpus = results.iter().find_map(|results| {
            let overhead = &results.overhead;
            overhead
                .observed_cpu_ns
                .filter(|_| overhead.wall_ns > 0 && self.data.live.is_none())
                .map(|observed| observed as f64 / overhead.wall_ns as f64)
        });
        if let Some(cpus) = busy_cpus {
            figures.push(vec![
                Span::styled(format!("{cpus:.2}"), style),
                Span::styled(" cpus busy, on average", dim()),
            ]);
        }
        let parts = track.part_totals();
        let whole: f64 = parts.iter().map(|(_, total)| total).sum();
        if whole <= 0.0 {
            figures.push(vec![Span::styled("by state: in the events only", dim())]);
        }
        let rows = self.cpu_rows.max(figures.len());
        for (index, line) in usage_chart(&cells, top, rows, &trust, whole_machine)
            .into_iter()
            .enumerate()
        {
            let scale = if index == 0 {
                format!("{:>8} ┤", share(top))
            } else if index == rows - 1 {
                format!("{:>8} ┤", "0")
            } else {
                format!("{:>8} │", "")
            };
            let figure = figures.get(index).cloned().unwrap_or_default();
            self.row(Span::styled(scale, dim()), line.spans, figure);
        }
        if whole > 0.0 {
            // The busy time by state, as one bar the chart's width: what of
            // it was the programs', the kernel's, waiting on I/O, stolen.
            const FILLS: [&str; 4] = ["█", "▓", "▒", "░"];
            let mut bar = Vec::new();
            let mut figure = Vec::new();
            let mut drawn = 0;
            for (index, (name, total)) in parts.iter().enumerate() {
                let fraction = total / whole;
                let cells = if index + 1 == parts.len() {
                    self.chart.saturating_sub(drawn)
                } else {
                    (fraction * self.chart as f64).round() as usize
                };
                drawn += cells;
                bar.push(Span::styled(
                    FILLS[index % FILLS.len()].repeat(cells),
                    style,
                ));
                if *total > 0.0 {
                    let gap = if figure.is_empty() { "" } else { " " };
                    figure.push(Span::styled(
                        format!("{gap}{}{name} ", FILLS[index % FILLS.len()]),
                        dim(),
                    ));
                    figure.push(Span::styled(share(fraction), style));
                }
            }
            self.row(dim_label("by state"), bar, figure);
        }
    }

    fn gpu(&mut self) {
        let busy = served(self.data, "gpu.engine_busy@system", "gpu.engine_busy");
        let memory = served(self.data, "gpu.mem@system", "gpu.mem");
        if !matches!(busy, Some(Held::Served(..))) && !matches!(memory, Some(Held::Served(..))) {
            return self.missing("gpu", busy.or(memory));
        }
        let mut joined = Trust::clean();
        let mut assumed = false;
        for held in [&busy, &memory] {
            if let Some(Held::Served(_, trust, scope)) = held {
                joined.merge(trust.clone());
                assumed |= scope;
            }
        }
        self.section(
            "gpu",
            &unlabelled("the device, as its driver says", assumed),
            Some(&joined),
        );
        match busy {
            Some(Held::Served(results, trust, _)) => {
                // A share of the engine's time: its capacity is one.
                let engine = Some(1.0);
                let all = self.tracks_of(&results);
                let several = all.len() > 1;
                for track in all {
                    if let Some(most) = track.tangled {
                        self.lines.push(boxed(
                            vec![
                                label("busy"),
                                Span::styled(
                                    format!(
                                        "up to {most} engines in one series, a read missing at \
                                         some instants: not told apart, so not drawn"
                                    ),
                                    dim(),
                                ),
                            ],
                            self.inner,
                        ));
                        continue;
                    }
                    let cells = self.axis.cells(&track.points, self.chart);
                    let top = ceiling(track.peak());
                    let lines = usage_chart(&cells, top, 2, &trust, engine);
                    let mut lines = lines.into_iter();
                    // Several engines or devices: each row says which.
                    let name = if several {
                        key_name(&track.key)
                    } else {
                        "busy".to_owned()
                    };
                    self.row(
                        label(&name),
                        lines.next().map(|line| line.spans).unwrap_or_default(),
                        vec![
                            Span::styled(
                                share(track.mean()),
                                hue(&trust, track.mean(), engine).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(" mean  ", dim()),
                            Span::styled(share(track.peak()), hue(&trust, track.peak(), engine)),
                            Span::styled(" peak", dim()),
                        ],
                    );
                    self.row(
                        dim_label(&format!(" ≤{}", share(top))),
                        lines.next().map(|line| line.spans).unwrap_or_default(),
                        Vec::new(),
                    );
                }
                // The time error, drawn on the run's own axis: how far each
                // point's moment may be from where it is drawn.
                if let Some(error) = results
                    .iter()
                    .filter_map(|results| results.max_ts_err_ns())
                    .max()
                    .filter(|error| *error > 0)
                {
                    let half = (error as f64 / self.axis.span_ns as f64 * self.chart as f64).round()
                        as usize;
                    let cells = (2 * half + 1).clamp(3, self.chart.max(3));
                    let time = Style::new().fg(palette::TIME);
                    self.row(
                        dim_label("when"),
                        vec![Span::styled(format!("├{}┤", "─".repeat(cells - 2)), time)],
                        vec![Span::styled(
                            format!(
                                "±{}: the driver's sampling window",
                                units::duration(error as f64)
                            ),
                            time,
                        )],
                    );
                }
            }
            held => self.missing_row("busy", held),
        }
        match memory {
            Some(Held::Served(results, trust, _)) => {
                for track in self.tracks_of(&results) {
                    let now = track.last().unwrap_or(0.0);
                    // VRAM against the device's size, when the run read it.
                    let style = hue(&trust, now, track.ceiling);
                    let flat = track
                        .points
                        .iter()
                        .all(|point| point.shown == track.points[0].shown);
                    let (chart_spans, figure) = match track.ceiling {
                        Some(total) => (
                            usage_chart(
                                &self.axis.cells(&track.points, self.chart),
                                total,
                                1,
                                &trust,
                                Some(total),
                            )
                            .remove(0)
                            .spans,
                            vec![
                                Span::styled(units::bytes(now), style.add_modifier(Modifier::BOLD)),
                                Span::styled(format!(" held of {}", units::bytes(total)), dim()),
                            ],
                        ),
                        None if flat => (
                            vec![Span::styled(
                                "held the whole run, unmoved (the device's size: in its events)",
                                dim(),
                            )],
                            vec![
                                Span::styled(units::bytes(now), style.add_modifier(Modifier::BOLD)),
                                Span::styled(" held", dim()),
                            ],
                        ),
                        None => (
                            chart(
                                &self.axis.cells(&track.points, self.chart),
                                track.peak(),
                                1,
                                style,
                            )
                            .remove(0)
                            .spans,
                            vec![
                                Span::styled(units::bytes(now), style.add_modifier(Modifier::BOLD)),
                                Span::styled(" held, peak ", dim()),
                                Span::styled(units::bytes(track.peak()), style),
                            ],
                        ),
                    };
                    self.row(label("memory"), chart_spans, figure);
                }
            }
            held => self.missing_row("memory", held),
        }
    }

    /// One row of a section saying a part of it is not there, and why.
    fn missing_row(&mut self, name: &str, held: Option<Held<'_>>) {
        let text = match held {
            Some(Held::Refused(refusal)) => Span::styled(
                format!("refused: {}", refusal.reason),
                Style::new().fg(palette::REFUSED),
            ),
            _ => Span::styled("not in this run", dim()),
        };
        self.lines.push(boxed(vec![label(name), text], self.inner));
    }

    fn memory(&mut self) {
        let held = served(self.data, "mem.pressure@system", "mem.pressure");
        let Some(Held::Served(results, trust, assumed)) = held else {
            return self.missing("memory", held);
        };
        self.section(
            "memory",
            &unlabelled("time the machine stalled waiting for it", assumed),
            Some(&trust),
        );
        // A share of the wall time stalled: always stalled is full.
        let always = Some(1.0);
        for track in self.tracks_of(&results) {
            let peak = track.peak();
            let figure = if peak <= 0.0 {
                vec![Span::styled("never stalled", hue(&trust, 0.0, always))]
            } else {
                vec![
                    Span::styled(
                        share(track.mean()),
                        hue(&trust, track.mean(), always).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" mean  ", dim()),
                    Span::styled(share(peak), hue(&trust, peak, always)),
                    Span::styled(" peak", dim()),
                ]
            };
            let cells = self.axis.cells(&track.points, self.chart);
            self.row(
                label("stalled"),
                usage_chart(&cells, ceiling(peak), 1, &trust, always)
                    .remove(0)
                    .spans,
                figure,
            );
        }
        // What the target holds, when the run measured it: the one memory
        // figure that is the target's rather than the machine's.
        if let Some(Held::Served(rss, rss_trust, _)) = on_target(self.data, "mem.rss") {
            // The machine's memory is not in the run: one neutral hue.
            let rss_style = rss_trust.hued(usage::NEUTRAL);
            let rss_tracks = self.tracks_of(&rss);
            let several = rss_tracks.len() > 1;
            for track in rss_tracks {
                let peak = track.peak();
                let cells = self.axis.cells(&track.points, self.chart);
                let mut figure = vec![
                    Span::styled(
                        units::bytes(track.last().unwrap_or(0.0)),
                        rss_style.add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" held, peak ", dim()),
                    Span::styled(units::bytes(peak), rss_style),
                ];
                if rss_trust != trust {
                    figure.push(Span::raw(" "));
                    figure.extend(rss_trust.badges());
                }
                let name = match task_of(&track.key) {
                    Some(tid) if several => tid.to_string(),
                    _ => "target".to_owned(),
                };
                self.row(
                    label(&name),
                    usage_chart(&cells, peak, 1, &rss_trust, None)
                        .remove(0)
                        .spans,
                    figure,
                );
            }
        }
    }

    fn power(&mut self) {
        let held = served(self.data, "power.energy@system", "power.energy");
        let Some(Held::Served(results, trust, scope)) = held else {
            return self.missing("power", held);
        };
        let all = self.tracks_of(&results);
        // A finished run keeps one timeline for every domain together: the
        // sum of the domains sysfs read, or the perf unit's domains one
        // after another at each instant, told apart by their order only.
        // The names are in the events.
        let named = all
            .iter()
            .all(|track| track.key != "system" && !track.lane && track.tangled.is_none());
        // Energy has no capacity to be a share of: one neutral hue, said.
        let note = if named {
            "watts per RAPL domain; no capacity, so one hue"
        } else {
            "watts, from the RAPL counters, no capacity so one hue; per domain by name: in the \
             events only"
        };
        self.section("power", &unlabelled(note, scope), Some(&trust));
        let style = trust.hued(usage::NEUTRAL);
        let top = all.iter().map(Track::peak).fold(0.0, f64::max);
        for track in &all {
            if let Some(most) = track.tangled {
                self.lines.push(boxed(
                    vec![
                        label("domains"),
                        Span::styled(
                            format!(
                                "up to {most} domains in one series, a read missing at some \
                                 instants: not told apart, so not drawn"
                            ),
                            dim(),
                        ),
                    ],
                    self.inner,
                ));
                continue;
            }
            let cells = self.axis.cells(&track.points, self.chart);
            let name = if track.key == "system" {
                "all"
            } else {
                track.key.as_str()
            };
            self.row(
                label(name),
                usage_chart(&cells, top, 1, &trust, None).remove(0).spans,
                vec![
                    Span::styled(
                        watts(track.total() * 1e9 / self.axis.span_ns as f64),
                        style.add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" mean  ", dim()),
                    Span::styled(watts(track.peak()), style),
                    Span::styled(" peak  ", dim()),
                    Span::styled(format!("{}J", units::count(track.total().round())), style),
                ],
            );
        }
    }

    fn devices(&mut self, what: &Devices) {
        let held = served(self.data, what.label, what.source);
        let Some(Held::Served(results, trust, assumed)) = held else {
            return self.missing(what.name, held);
        };
        let mut all = self.tracks_of(&results);
        all.sort_by(|one, other| {
            other
                .total()
                .total_cmp(&one.total())
                .then_with(|| one.key.cmp(&other.key))
        });
        let split = all.iter().any(|track| !track.parts.is_empty());
        // Neither a device's speed nor a link's is in the run: bytes have no
        // capacity to be a share of, so one neutral hue, and the note says so.
        let note = if split {
            format!("{}; speed unknown, so one hue", what.note)
        } else {
            format!(
                "{}, speed unknown so one hue; {} apart from {} needs the `dir` key",
                what.note, what.parts.0, what.parts.1
            )
        };
        self.section(what.name, &unlabelled(&note, assumed), Some(&trust));
        let style = trust.hued(usage::NEUTRAL);
        // One scale for every row, so two devices compare by height.
        let top = all.iter().map(Track::peak).fold(0.0, f64::max);
        let idle: Vec<String> = all
            .iter()
            .filter(|track| track.total() <= 0.0)
            .map(|track| device_name(&track.key))
            .collect();
        let listed = self.idle;
        for track in all.iter().filter(|track| listed || track.total() > 0.0) {
            let cells = self.axis.cells(&track.points, self.chart);
            let parts = track.part_totals();
            let figure = if parts.len() == 2 {
                vec![
                    Span::styled(units::bytes(parts[0].1), style.add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" {}  ", what.parts.0), dim()),
                    Span::styled(units::bytes(parts[1].1), style.add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" {}", what.parts.1), dim()),
                ]
            } else {
                vec![
                    Span::styled(
                        units::bytes(track.total()),
                        style.add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  peak ", dim()),
                    Span::styled(format!("{}/s", units::bytes(track.peak())), style),
                ]
            };
            self.row(
                label(&device_name(&track.key)),
                usage_chart(&cells, top, 1, &trust, None).remove(0).spans,
                figure,
            );
        }
        if all.is_empty() {
            self.lines.push(boxed(
                vec![label(""), Span::styled("nothing in it", dim())],
                self.inner,
            ));
        } else if !self.idle && !idle.is_empty() {
            self.lines.push(boxed(
                vec![
                    label(""),
                    Span::styled(
                        format!("+{} moved nothing: {}", idle.len(), idle.join(" ")),
                        dim(),
                    ),
                ],
                self.inner,
            ));
        }
    }
}

/// The machine's box, line by line, and where each section starts.
struct Machine {
    lines: Vec<Line<'static>>,
    sections: Vec<(String, usize)>,
    inner: usize,
}

impl Machine {
    fn len(&self) -> usize {
        self.lines.len()
    }

    /// The box in at most `room` lines. When it does not fit, whole sections
    /// are left out from the last, and one line says which -- the head, the
    /// time axis and the bottom border stay, so what is drawn still reads on
    /// its axis and what is not is counted.
    fn fit(self, room: usize) -> Vec<Line<'static>> {
        let Machine {
            lines,
            sections,
            inner,
        } = self;
        if lines.len() <= room {
            return lines;
        }
        // The axis and the border close the box.
        let tail = lines.len().saturating_sub(2);
        // A section ends where the next starts, the last where the axis is.
        let ends = sections
            .iter()
            .skip(1)
            .map(|(_, at)| *at)
            .chain(std::iter::once(tail));
        // Sections are kept, in order, while they fit with the line saying
        // what was left out, the axis and the border.
        let kept = ends.take_while(|end| end + 3 <= room).count();
        let left: Vec<&str> = sections[kept..]
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        let cut = boxed(
            vec![
                label(""),
                Span::styled(
                    format!(
                        "+{} left out, the terminal is too short: {}",
                        left.len(),
                        left.join(" ")
                    ),
                    Style::new().fg(palette::LOSS),
                ),
            ],
            inner,
        );
        let body_end = sections.get(kept).map_or(tail, |(_, at)| *at);
        let mut out: Vec<Line<'static>> = lines[..body_end].to_vec();
        out.push(cut);
        out.extend(lines[tail..].iter().cloned());
        if out.len() > room {
            // Not even the head and the axis: the line saying so, alone.
            out = vec![out.swap_remove(body_end)];
            out.truncate(room);
        }
        out
    }
}

/// A line of the box: its content between two borders.
fn boxed(spans: Vec<Span<'static>>, inner: usize) -> Line<'static> {
    let mut line = vec![Span::raw("│")];
    line.extend(fit(spans, inner));
    line.push(Span::raw("│"));
    Line::from(line)
}

/// What a section of devices draws: disks or interfaces.
struct Devices {
    name: &'static str,
    note: &'static str,
    label: &'static str,
    source: &'static str,
    /// The two parts the events split the bytes into.
    parts: (&'static str, &'static str),
}

const DISK: Devices = Devices {
    name: "disk",
    note: "bytes moved, per device",
    label: "io.bytes@system",
    source: "io.bytes",
    parts: ("read", "written"),
};

const NETWORK: Devices = Devices {
    name: "network",
    note: "bytes moved, per interface",
    label: "net.bytes@system",
    source: "net.bytes",
    parts: ("in", "out"),
};

/// A series key as a row's label: its values without their kinds
/// (`gpu:0,engine:gr` is `0/gr`), and a timeline's lane as it is (`#2`).
fn key_name(key: &str) -> String {
    key_parts(key)
        .into_iter()
        .map(|part| part.split_once(':').map_or(part, |(_, value)| value))
        .collect::<Vec<_>>()
        .join("/")
}

/// A device's name as a person knows it: `wlan0` for `iface:wlan0`,
/// `259:0` for `dev:259+0`.
fn device_name(key: &str) -> String {
    if let Some(name) = key.strip_prefix("iface:") {
        return name.to_owned();
    }
    if let Some(dev) = key.strip_prefix("dev:") {
        return dev.replace('+', ":");
    }
    key.to_owned()
}

/// When the run asked for none of the machine's panels -- a session made by
/// `omn latency`, or by hand -- one paragraph says so, rather than seven
/// empty frames.
fn draw_no_machine(frame: &mut Frame<'_>, area: Rect, data: &Data) {
    let labels: Vec<String> = data
        .run
        .results
        .iter()
        .map(|results| {
            if results.label == results.source {
                results.label.clone()
            } else {
                format!("{} ({})", results.label, results.source)
            }
        })
        .collect();
    let text = vec![
        Line::from(Span::styled(
            "This run did not measure the machine: no cpu, memory, disk, network, GPU or power \
             source is in it. The session that made it asked for:",
            dim(),
        )),
        Line::from(labels.join(", ")),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .block(panel("system --", TITLE, None))
            .wrap(Wrap { trim: true }),
        area,
    );
}

// ---------------------------------------------------------------------------
// The process list.
// ---------------------------------------------------------------------------

/// A column of the process list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Col {
    Cpu,
    Spark,
    CpuTime,
    Rss,
    Faults,
    RunQueue,
    IoCalls,
    InIo,
    Lock,
    GpuMem,
}

/// Each column: its heading, its width, and the source it is read from, in
/// the order they give way when the panel is narrow -- the last first.
const COLUMNS: &[(Col, &str, usize, &str)] = &[
    (Col::Cpu, "CPU", 6, "cpu.oncpu_time"),
    (Col::Spark, "cpu over the run", 16, "cpu.oncpu_time"),
    (Col::CpuTime, "ON CPU", 7, "cpu.oncpu_time"),
    (Col::Rss, "RSS", 8, "mem.rss"),
    (Col::Faults, "FAULTS", 6, "mem.faults"),
    (Col::RunQueue, "RUN QUEUE", 9, "sched.runqlat"),
    (Col::IoCalls, "I/O CALLS", 9, "io.syscall"),
    (Col::InIo, "IN I/O", 7, "io.syscall"),
    (Col::Lock, "ON LOCKS", 8, "sched.futex"),
    (Col::GpuMem, "GPU MEM", 8, "gpu.mem"),
];

/// One process (or thread) of the list.
#[derive(Debug, Clone, Default)]
struct Proc {
    tid: i32,
    name: Option<String>,
    /// Whether the id came from a per-process reader (`/proc/<pid>`, which
    /// `cpu.oncpu_time` reads), so it is a process's and not only a thread's.
    process: bool,
    cpu: Option<Track>,
    cells: BTreeMap<Col, f64>,
}

impl Proc {
    fn key(&self) -> (i32, bool) {
        (self.tid, self.process)
    }

    fn focus(&self) -> Focus {
        Focus {
            pid: self.process.then_some(self.tid),
            tid: (!self.process).then_some(self.tid),
            request: None,
        }
    }
}

/// The list, and how far each column can be trusted.
struct Procs {
    rows: Vec<Proc>,
    columns: Vec<(Col, Trust)>,
    refused: Option<Refusal>,
}

/// The task a series key names, if it names one.
fn task_of(key: &str) -> Option<i32> {
    key_parts(key)
        .into_iter()
        .find_map(|part| part.strip_prefix("task:"))
        .and_then(|tid| tid.parse().ok())
}

/// Per task, what a source's events added up to and how many there were:
/// from its `summary` (`count` x `mean`), or, live, from its events.
fn per_task(results: &Results) -> BTreeMap<i32, (f64, f64)> {
    let mut totals = BTreeMap::new();
    if let Some(Output::Summary { series }) = results.output("summary") {
        for series in series {
            if let Some(tid) = task_of(&series.key) {
                let count = series.count as f64;
                let entry = totals.entry(tid).or_insert((0.0, 0.0));
                entry.0 += series.mean * count;
                entry.1 += count;
            }
        }
        return totals;
    }
    let attr = reading(&results.source).attr;
    for event in results.events() {
        if let Some(tid) = event.tid() {
            let entry = totals.entry(tid).or_insert((0.0, 0.0));
            entry.0 += event.number(attr).unwrap_or(0.0);
            entry.1 += 1.0;
        }
    }
    totals
}

/// Whether a result's task keys name processes rather than threads: a
/// reader of `/proc/<pid>` (cpu time, RSS) and the GPU driver's account
/// are a whole process's, while a probe or a per-thread counter (faults, run
/// queue, I/O calls, locks) is the thread's that it fired in. A process's id
/// is its main thread's, so the two are kept apart rather than merged into
/// one row that would add a process's cpu to one thread's faults.
fn per_process(results: &Results) -> bool {
    match results.source.as_str() {
        "mem.rss" | "gpu.mem" => true,
        // A live run may not say its backend: `omn top`'s is `fs:proc`.
        "cpu.oncpu_time" => results.backend.is_empty() || results.backend.starts_with("fs:"),
        _ => false,
    }
}

/// The row of a task, as a process or as a thread.
fn row_of(rows: &mut BTreeMap<(i32, bool), Proc>, tid: i32, process: bool) -> &mut Proc {
    rows.entry((tid, process)).or_insert_with(|| Proc {
        tid,
        process,
        ..Proc::default()
    })
}

/// Every task the run measured, with what each source says of it. Sources
/// on the machine (`@system`) are not a process's and are left out. A
/// process and its main thread, which share an id, are two rows.
fn processes(data: &Data, sort: Sort) -> Procs {
    let mut rows: BTreeMap<(i32, bool), Proc> = BTreeMap::new();
    let mut columns: Vec<(Col, Trust)> = Vec::new();
    for &(col, _, _, source) in COLUMNS {
        let held: Vec<&Results> = data
            .of_source(source)
            .filter(|results| !results.label.ends_with("@system"))
            .collect();
        if held.is_empty() {
            continue;
        }
        for results in &held {
            let process = per_process(results);
            match col {
                Col::Spark | Col::CpuTime => {}
                Col::Cpu => {
                    for track in tracks(data, results) {
                        let Some(tid) = task_of(&track.key) else {
                            continue;
                        };
                        let row = row_of(&mut rows, tid, process);
                        if track.name.is_some() {
                            row.name = track.name.clone();
                        }
                        row.cells.insert(Col::CpuTime, track.total());
                        row.cpu = Some(track);
                    }
                }
                Col::Rss | Col::GpuMem => {
                    for track in tracks(data, results) {
                        let Some(tid) = task_of(&track.key) else {
                            continue;
                        };
                        row_of(&mut rows, tid, process)
                            .cells
                            .insert(col, track.last().unwrap_or(0.0));
                    }
                }
                Col::Faults | Col::InIo | Col::RunQueue | Col::Lock | Col::IoCalls => {
                    for (tid, (total, count)) in per_task(results) {
                        let value = if col == Col::IoCalls { count } else { total };
                        row_of(&mut rows, tid, process).cells.insert(col, value);
                    }
                }
            }
        }
        columns.push((col, Trust::joined(data, held.iter().copied())));
    }
    machine_tasks(data, &mut rows, &mut columns);
    let span_ns = Axis::of(data).span_ns as f64;
    for row in rows.values_mut() {
        if let Some(cpu_ns) = row.cells.get(&Col::CpuTime).copied() {
            row.cells.insert(Col::Cpu, cpu_ns / span_ns);
        }
    }
    let by = sort.column();
    let mut rows: Vec<Proc> = rows.into_values().collect();
    rows.sort_by(|one, other| {
        let value = |row: &Proc| row.cells.get(&by).copied().unwrap_or(f64::NEG_INFINITY);
        value(other)
            .total_cmp(&value(one))
            .then_with(|| one.tid.cmp(&other.tid))
            .then_with(|| other.process.cmp(&one.process))
    });
    let refused = data
        .refused
        .iter()
        .find(|refusal| refusal.source == "cpu.oncpu_time")
        .cloned();
    Procs {
        rows,
        columns,
        refused,
    }
}

/// The source that lists the machine's processes, `top`'s and `ps`'s table:
/// one event per process each poll, with its name, its share of one cpu
/// since the poll before, its cpu time and its resident size.
const TASKS: &str = "proc.tasks";

/// The machine's processes, from [`TASKS`], into the list: each its name,
/// its cpu over the run as a sparkline and a total, and its resident size
/// at the last poll. Its columns join the ones the run's other sources fill,
/// on the same rows: a process's id is the same whichever source read it.
fn machine_tasks(
    data: &Data,
    rows: &mut BTreeMap<(i32, bool), Proc>,
    columns: &mut Vec<(Col, Trust)>,
) {
    let held: Vec<&Results> = data.of_source(TASKS).collect();
    if held.is_empty() {
        return;
    }
    let opened = data.opened_ns();
    // Per task, in the order read: each poll's share and when it was.
    let mut polls: BTreeMap<(i32, bool), Vec<&Event>> = BTreeMap::new();
    for results in &held {
        for event in results.events() {
            let Some(tid) = event
                .tid()
                .or_else(|| event.number("tid").map(|tid| tid as i32))
            else {
                continue;
            };
            let process = event.number("pid").is_none_or(|pid| pid as i32 == tid);
            polls.entry((tid, process)).or_default().push(event);
        }
    }
    for ((tid, process), events) in polls {
        let row = row_of(rows, tid, process);
        let last = events.last().expect("a task is listed by its events");
        if let Some(name) = last.attr("comm").and_then(|value| value.as_str()) {
            row.name = Some(name.to_owned());
        }
        if let Some(rss) = last.number("rss_bytes") {
            row.cells.insert(Col::Rss, rss);
        }
        // What it ran within the run: each poll's share over its interval;
        // a first reading is over the task's life, which is the run's only
        // when the task started inside it.
        let mut cpu_ns = 0.0;
        let mut points = Vec::new();
        for event in &events {
            let busy = event.number("busy").unwrap_or(0.0);
            let since_start = event
                .attr("since")
                .and_then(|value| value.as_str())
                .is_some_and(|since| since == "start");
            let at_ns = event.ts_ns.saturating_sub(opened);
            if since_start {
                let started = event
                    .ts_ns
                    .saturating_sub(event.number("age_ns").unwrap_or(f64::MAX) as u64);
                if started >= opened {
                    cpu_ns += event.number("cpu_ns").unwrap_or(0.0);
                }
                // A share over a whole life is not this instant's.
                continue;
            }
            let over = event.number("over_ns").unwrap_or(0.0);
            cpu_ns += busy * over;
            points.push(Point {
                at_ns,
                read: busy * over,
                // Drawn as the cpu's other sources are, in nanoseconds of
                // cpu per second.
                shown: busy * 1e9,
            });
        }
        row.cells.entry(Col::CpuTime).or_insert(cpu_ns);
        if row.cpu.is_none() && !points.is_empty() {
            row.cpu = Some(Track {
                key: format!("task:{tid}"),
                points,
                name: row.name.clone(),
                ..Track::default()
            });
        }
    }
    let trust = Trust::joined(data, held.iter().copied());
    for col in [Col::Cpu, Col::Spark, Col::CpuTime, Col::Rss] {
        match columns.iter_mut().find(|(held, _)| *held == col) {
            Some((_, joined)) => joined.merge(trust.clone()),
            None => columns.push((col, trust.clone())),
        }
    }
    columns.sort_by_key(|(col, _)| *col);
}

/// A column's cell, in its unit.
fn cell_text(col: Col, value: f64) -> String {
    match col {
        Col::Cpu => units::percent(value * 100.0),
        Col::CpuTime | Col::InIo | Col::RunQueue | Col::Lock => units::duration(value),
        Col::Rss | Col::GpuMem => units::bytes(value),
        Col::Faults | Col::IoCalls => units::count(value.round()),
        Col::Spark => String::new(),
    }
}

impl System {
    fn draw_processes(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        ctx: &Ctx<'_>,
        axis: &Axis,
        procs: &Procs,
    ) {
        let sort = SORTS[self.sort];
        // A launched program's run is the program's; any other lists every
        // task it measured, which on the machine is not one target's.
        let whose = if ctx.data.has_target() {
            "the target's"
        } else {
            "every task measured"
        };
        let block = panel("processes", &format!("{whose}, by {}", sort.words()), None);
        let inner = block.inner(area);
        self.shown.clear();
        if procs.rows.is_empty() {
            match &procs.refused {
                Some(refusal) => trust::render_refusal(frame, area, refusal),
                None => frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "no process in this run: nothing in it is measured per task",
                        dim(),
                    )))
                    .block(block),
                    area,
                ),
            }
            return;
        }
        frame.render_widget(block, area);

        let named = procs.rows.iter().any(|row| row.name.is_some());
        let fixed = 10 + if named { 13 } else { 0 };
        let mut room = usize::from(inner.width).saturating_sub(fixed);
        let mut columns: Vec<(Col, &str, usize, Trust)> = Vec::new();
        for &(col, head, width, _) in COLUMNS {
            let Some((_, trust)) = procs.columns.iter().find(|(held, _)| *held == col) else {
                continue;
            };
            if width + 1 > room {
                break;
            }
            room -= width + 1;
            columns.push((col, head, width, trust.clone()));
        }
        self.shown = columns.iter().map(|(col, ..)| *col).collect();

        let bold = Style::new().add_modifier(Modifier::BOLD);
        let mut lines = Vec::new();
        let mut head = vec![Span::styled(format!("{:>9} ", "ID"), bold)];
        if named {
            head.push(Span::styled(format!("{:<12} ", "NAME"), bold));
        }
        for (col, title, width, _) in &columns {
            let style = if *col == sort.column() || (*col == Col::Cpu && sort == Sort::Cpu) {
                bold.add_modifier(Modifier::UNDERLINED)
            } else {
                bold
            };
            let text = if *col == Col::Spark {
                format!("{title:<width$} ")
            } else {
                format!("{title:>width$} ")
            };
            head.push(Span::styled(text, style));
        }
        lines.push(Line::from(head));

        let rows_room = usize::from(inner.height).saturating_sub(2).max(1);
        let selected = self
            .selected
            .and_then(|key| procs.rows.iter().position(|row| row.key() == key));
        let skip = selected.map_or(0, |index| (index + 1).saturating_sub(rows_room));
        for (index, row) in procs.rows.iter().enumerate().skip(skip).take(rows_room) {
            let y = inner.y + 1 + (index - skip) as u16;
            if y < inner.y + inner.height {
                self.regions
                    .push(Region::new(Rect::new(inner.x, y, inner.width, 1), index));
            }
            let chosen = selected == Some(index);
            let focused = if row.process {
                ctx.focus.pid == Some(row.tid)
            } else {
                ctx.focus.tid == Some(row.tid)
            };
            let marker = if chosen {
                Span::styled("▶", Style::new().fg(palette::ACCENT))
            } else if focused {
                Span::styled("●", Style::new().fg(palette::ACCENT))
            } else {
                Span::raw(" ")
            };
            let id_style = if chosen {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            };
            // A process's main thread has the process's id: its own row
            // is marked as the thread's.
            let id = if !row.process
                && procs
                    .rows
                    .iter()
                    .any(|other| other.key() == (row.tid, true))
            {
                format!("t{}", row.tid)
            } else {
                row.tid.to_string()
            };
            let mut spans = vec![
                marker,
                Span::styled(format!("{id:>8}"), id_style),
                Span::raw(" "),
            ];
            if named {
                let name: String = row
                    .name
                    .clone()
                    .unwrap_or_default()
                    .chars()
                    .take(12)
                    .collect();
                spans.push(Span::raw(format!("{name:<12} ")));
            }
            for (col, _, width, trust) in &columns {
                let style = trust.style();
                if *col == Col::Spark {
                    match &row.cpu {
                        Some(track) => {
                            let cells = axis.cells(&track.points, *width);
                            // Against one whole cpu, the scale a person
                            // reads a process's share on.
                            spans.extend(chart(&cells, 1e9, 1, style).remove(0).spans);
                            spans.push(Span::raw(" "));
                        }
                        None => spans.push(Span::raw(format!("{:width$} ", ""))),
                    }
                    continue;
                }
                let text = match row.cells.get(col) {
                    Some(value) => cell_text(*col, *value),
                    // Not measured for this task: not a zero.
                    None => "·".to_owned(),
                };
                spans.push(Span::styled(format!("{text:>width$} "), style));
            }
            lines.push(Line::from(spans));
        }
        frame.render_widget(Paragraph::new(lines), inner);

        // What each column is not to be read at face value for, said once
        // beneath them, and what did not fit.
        let more = procs.rows.len().saturating_sub(skip + rows_room);
        let mut said: Vec<Span<'static>> = Vec::new();
        if more > 0 {
            said.push(Span::styled(format!("+{more} more  "), dim()));
        }
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for (col, title, _, trust) in &columns {
            let badges = trust.badges();
            let text: String = badges.iter().map(|span| span.content.as_ref()).collect();
            if *col == Col::Spark || badges.is_empty() || !seen.insert(text) {
                continue;
            }
            said.push(Span::styled(format!("{title} "), dim()));
            said.extend(badges);
            said.push(Span::raw("  "));
        }
        if !said.is_empty() && inner.height > 1 {
            let foot = Rect {
                y: inner.y + inner.height - 1,
                height: 1,
                ..inner
            };
            frame.render_widget(Paragraph::new(Line::from(said)), foot);
        }
    }
}

// ---------------------------------------------------------------------------
// The batch output.
// ---------------------------------------------------------------------------

/// The batch table's columns, in order: each heading and the list's column
/// it prints, raw (nanoseconds, bytes, counts, a share of one cpu).
const BATCH: &[(&str, Col)] = &[
    ("CPU", Col::Cpu),
    ("ON_CPU_NS", Col::CpuTime),
    ("RSS_BYTES", Col::Rss),
    ("FAULTS", Col::Faults),
    ("RUN_QUEUE_NS", Col::RunQueue),
    ("IO_CALLS", Col::IoCalls),
    ("IN_IO_NS", Col::InIo),
    ("LOCK_NS", Col::Lock),
    ("GPU_MEM_BYTES", Col::GpuMem),
];

/// A number as the batch prints it: whole when it is whole, four
/// significant decimals otherwise, never in a unit's words.
fn batch_number(value: f64) -> String {
    if !value.is_finite() {
        return "-".to_owned();
    }
    if value.fract() == 0.0 && value.abs() < 1e15 {
        return format!("{value:.0}");
    }
    format!("{value:.4}")
}

/// The `system` view as text a script parses, `top -b`'s use: what the
/// machine's panels hold, one `machine` line per series, then every task,
/// sorted as `sort` says (`cpu`, `memory`, `faults`, `io`, `runqueue`), a
/// whitespace-separated table with `-` where a column was not measured for a
/// task. Nothing is in a unit's words: shares are fractions, times
/// nanoseconds, sizes bytes. What a column is not to be read at face value
/// for (a loss, a time error, a refusal) is said on a `#` line, never left
/// out.
pub fn batch(data: &Data, sort: &str) -> Result<String, String> {
    let sort = match sort {
        "cpu" => Sort::Cpu,
        "memory" => Sort::Memory,
        "faults" => Sort::Faults,
        "io" => Sort::Io,
        "runqueue" => Sort::RunQueue,
        other => {
            return Err(format!(
                "there is no sort `{other}`; the sorts are cpu, memory, faults, io, runqueue"
            ))
        }
    };
    let mut out = String::new();
    let axis = Axis::of(data);
    // One line: a launched program's own text may hold newlines.
    let origin: String = data
        .origin
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    out.push_str(&format!("# omn top --batch: {origin}\n"));
    out.push_str(&format!("# span_ns {}\n", axis.span_ns));
    out.push_str("# machine SOURCE KEY mean=PER_SECOND_OR_SHARE peak=... last=... total=SOURCE_UNIT [PART=SOURCE_UNIT ...]\n");
    for (label, source) in MACHINE {
        match served(data, label, source) {
            None => {}
            Some(Held::Refused(refusal)) => {
                out.push_str(&format!("# {source} refused: {}\n", refusal.reason));
            }
            Some(Held::Served(results, trust, _)) => {
                for sentence in trust.sentences() {
                    out.push_str(&format!("# {source}: {sentence}\n"));
                }
                let mut all: Vec<Track> = results
                    .iter()
                    .flat_map(|results| tracks(data, results))
                    .collect();
                all.sort_by(|one, other| one.key.cmp(&other.key));
                for track in all.iter().filter(|track| track.tangled.is_none()) {
                    let key = key_name(&track.key).replace(char::is_whitespace, "_");
                    let mut line = format!(
                        "machine {source} {key} mean={} peak={} last={} total={}",
                        batch_number(track.mean()),
                        batch_number(track.peak()),
                        batch_number(track.last().unwrap_or(f64::NAN)),
                        batch_number(track.total()),
                    );
                    for (part, total) in track.part_totals() {
                        line.push_str(&format!(" {part}={}", batch_number(total)));
                    }
                    out.push_str(&line);
                    out.push('\n');
                }
            }
        }
    }
    let procs = processes(data, sort);
    if let Some(refusal) = &procs.refused {
        out.push_str(&format!(
            "# {} refused: {}\n",
            refusal.source, refusal.reason
        ));
    }
    for (col, trust) in &procs.columns {
        let Some((head, _)) = BATCH.iter().find(|(_, held)| held == col) else {
            continue;
        };
        for sentence in trust.sentences() {
            out.push_str(&format!("# {head}: {sentence}\n"));
        }
    }
    let mut head = vec!["ID", "KIND", "NAME"];
    head.extend(BATCH.iter().map(|(head, _)| *head));
    out.push_str(&head.join(" "));
    out.push('\n');
    for row in &procs.rows {
        let name = row
            .name
            .as_deref()
            .map(|name| name.replace(char::is_whitespace, "_"))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "-".to_owned());
        let mut line = vec![
            row.tid.to_string(),
            if row.process { "process" } else { "thread" }.to_owned(),
            name,
        ];
        for (_, col) in BATCH {
            line.push(
                row.cells
                    .get(col)
                    .map_or_else(|| "-".to_owned(), |value| batch_number(*value)),
            );
        }
        out.push_str(&line.join(" "));
        out.push('\n');
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// The cost corner.
// ---------------------------------------------------------------------------

/// What each of this view's collectors cost, as a share of one cpu over its
/// window: what drawing these panels asked of the machine.
fn draw_cost(frame: &mut Frame<'_>, area: Rect, data: &Data) {
    let style = Style::new().fg(palette::COST);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(style)
        .title(Span::styled(" cost, share of one cpu ", style));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let say = |frame: &mut Frame<'_>, text: &'static str| {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text,
                style.add_modifier(Modifier::DIM),
            )))
            .wrap(Wrap { trim: true }),
            inner,
        );
    };
    if data.live.is_some() {
        return say(
            frame,
            "said at the end of the run: a collector's cost is known when it stops",
        );
    }
    let mut costs: Vec<(String, f64, bool)> = Vec::new();
    for need in NEEDS {
        for results in data.for_need(&need.label(), need.source) {
            let overhead = &results.overhead;
            if overhead.wall_ns == 0 {
                continue;
            }
            let spent = overhead.attributed_cpu_ns + overhead.in_target_cpu_ns.unwrap_or(0);
            costs.push((
                results.source.clone(),
                spent as f64 * 100.0 / overhead.wall_ns as f64,
                overhead.in_target_cpu_ns.is_none(),
            ));
        }
    }
    if costs.is_empty() {
        return say(frame, "none of this view's collectors is in this run");
    }
    costs.sort_by(|one, other| other.1.total_cmp(&one.1).then_with(|| one.0.cmp(&other.0)));
    let top = costs.iter().map(|(_, share, _)| *share).fold(0.0, f64::max);
    let bar = 7usize;
    let name_width = usize::from(inner.width).saturating_sub(bar + 9).max(4);
    // Every collector, or as many as fit and the rest in one line: the
    // total is always all of them.
    let room = usize::from(inner.height).saturating_sub(1).max(1);
    let listed = if costs.len() > room {
        room.saturating_sub(1)
    } else {
        costs.len()
    };
    let at_least = |lower: bool| if lower { "≥" } else { "" };
    let figure_style = |lower: bool| {
        if lower {
            style.add_modifier(Modifier::DIM)
        } else {
            style
        }
    };
    let mut lines = Vec::new();
    for (source, share, lower) in &costs[..listed] {
        let name: String = source.chars().take(name_width).collect();
        let figure = format!("{}{}", at_least(*lower), units::percent(*share));
        let mut spans = vec![Span::raw(format!("{name:<name_width$} "))];
        spans.extend(
            trust::error_bar(*share, *share, *share, top, bar as u16)
                .spans
                .into_iter()
                .map(|span| Span::styled(span.content, style)),
        );
        spans.push(Span::styled(format!(" {figure:>6}"), figure_style(*lower)));
        lines.push(Line::from(spans));
    }
    let rest = &costs[listed..];
    if !rest.is_empty() {
        let share: f64 = rest.iter().map(|(_, share, _)| share).sum();
        let lower = rest.iter().any(|(_, _, lower)| *lower);
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{:<width$}",
                    format!("+{} more", rest.len()),
                    width = name_width + 1 + bar
                ),
                dim(),
            ),
            Span::styled(
                format!(
                    " {:>6}",
                    format!("{}{}", at_least(lower), units::percent(share))
                ),
                figure_style(lower),
            ),
        ]));
    }
    let total: f64 = costs.iter().map(|(_, share, _)| share).sum();
    let any_lower = costs.iter().any(|(_, _, lower)| *lower);
    lines.push(Line::from(vec![
        Span::styled(
            format!("{:<width$}", "all of them", width = name_width + 1 + bar),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " {:>6}",
                format!("{}{}", at_least(any_lower), units::percent(total))
            ),
            style.add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Attr, AttrValue, Key, Live, Run};
    use crate::snapshot::{draw, fixture, text};
    use ratatui::crossterm::event::KeyModifiers;

    fn screen(data: Data) -> String {
        text(&draw(data, "system", 120, 40))
    }

    #[test]
    fn the_machine_is_drawn_panel_by_panel() {
        let screen = screen(fixture("machine"));
        for panel in [
            " cpu ",
            " memory ",
            " disk ",
            " gpu ",
            " power ",
            " network ",
            "cost, share of one cpu",
        ] {
            assert!(screen.contains(panel), "no {panel:?} in\n{screen}");
        }
        // The busiest interface listed, the idle ones folded into one line.
        assert!(screen.contains("wlan0"), "{screen}");
        assert!(screen.contains("moved nothing"), "{screen}");
        // The process list's source is served on this run (per thread, the
        // eBPF backend): the busiest tasks listed by cpu, the rest counted.
        assert!(!screen.contains("cpu.oncpu_time refused"), "{screen}");
        assert!(screen.contains("by cpu"), "{screen}");
        assert!(screen.contains(" more "), "{screen}");
    }

    #[test]
    fn usage_is_coloured_by_how_full_and_bytes_keep_one_hue() {
        let buffer = draw(fixture("machine"), "system", 120, 40);
        let row_of = |needle: &str| {
            (0..buffer.area.height)
                .find(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, *y)].symbol())
                        .collect::<String>()
                        .contains(needle)
                })
                .unwrap_or_else(|| panic!("no {needle:?}"))
        };
        let glyphs = |y: u16| -> Vec<ratatui::style::Color> {
            (0..buffer.area.width)
                .filter(|x| {
                    let symbol = buffer[(*x, y)].symbol();
                    EIGHTHS[1..].iter().any(|glyph| symbol == glyph.to_string())
                })
                .map(|x| buffer[(x, y)].fg)
                .collect()
        };
        // The cpu chart's cells, under its rule: each in the ramp, by its
        // share of the machine.
        let cpu = row_of("├ cpu");
        let cpu_cells: Vec<_> = (cpu + 1..cpu + 4).flat_map(glyphs).collect();
        assert!(!cpu_cells.is_empty());
        assert!(
            cpu_cells.iter().all(|fg| usage::RAMP.contains(fg)),
            "{cpu_cells:?}"
        );
        // A network's bytes have no capacity the run knows: one hue.
        let net = row_of("├ network");
        let net_cells: Vec<_> = (net + 1..net + 3).flat_map(glyphs).collect();
        assert!(!net_cells.is_empty());
        assert!(
            net_cells.iter().all(|fg| *fg == usage::NEUTRAL),
            "{net_cells:?}"
        );
        assert!(text(&buffer).contains("speed unknown"));
    }

    #[test]
    fn the_gpu_sampling_window_is_drawn_on_the_time_axis() {
        let screen = screen(fixture("cpython-hot"));
        let ruler = screen
            .lines()
            .find(|line| line.contains("├") && line.contains("±1.00s"))
            .unwrap_or_else(|| panic!("no ruler in\n{screen}"));
        // One second each side, on a 3.89 s axis: about half the chart.
        let dashes = ruler.matches('─').count();
        assert!(dashes > 5, "{ruler}");
    }

    #[test]
    fn the_target_is_in_the_list_with_its_cpu() {
        let data = fixture("cpython-hot");
        let procs = processes(&data, Sort::Cpu);
        let process: Vec<&Proc> = procs.rows.iter().filter(|row| row.process).collect();
        assert_eq!(process.len(), 1);
        // Its main thread, which the per-thread sources saw under the same
        // id, is a row of its own. This run's `cpu.oncpu_time` is the eBPF
        // backend's, per thread: the cpu is the thread's row, and the
        // process's holds what `/proc/<pid>` says (RSS), never a thread's
        // cpu or faults.
        let thread = procs
            .rows
            .iter()
            .find(|row| !row.process && row.tid == process[0].tid)
            .expect("the main thread's row");
        let cpu = thread.cells[&Col::Cpu];
        // The interpreter spins on one cpu for the whole run.
        assert!(cpu > 0.8 && cpu < 1.1, "{cpu}");
        assert!(process[0].cells.contains_key(&Col::Rss));
        assert!(!process[0].cells.contains_key(&Col::Cpu));
        assert!(!process[0].cells.contains_key(&Col::Faults));
        let pid = process[0].tid;
        let screen = screen(data);
        assert!(screen.contains(&format!(" {pid}")), "{screen}");
        assert!(screen.contains(&format!("t{pid}")), "{screen}");
    }

    /// A finished run's timelines start at their result's first event. A
    /// poller that keeps its first read as a baseline emits one read late,
    /// so it is placed one read later: every source of the machine ends at
    /// the run's close, as its last read did.
    #[test]
    fn every_timeline_of_the_machine_ends_where_the_run_does() {
        let data = fixture("http-server");
        let end_of = |source: &str| {
            let results = data
                .run
                .results
                .iter()
                .find(|results| results.label == format!("{source}@system"))
                .unwrap();
            let tracks = tracks(&data, results);
            let first = tracks[0].points[0].at_ns;
            let last = tracks
                .iter()
                .flat_map(|track| &track.points)
                .map(|point| point.at_ns)
                .max();
            (first, last.unwrap())
        };
        let (_, cpu_end) = end_of("cpu.util");
        for source in [
            "mem.pressure",
            "io.bytes",
            "net.bytes",
            "power.energy",
            "gpu.mem",
        ] {
            let (_, end) = end_of(source);
            assert!(
                end.abs_diff(cpu_end) < 20_000_000,
                "{source}: {end} against {cpu_end}"
            );
        }
        // The pollers that keep their first read back start one read late.
        let (first, _) = end_of("mem.pressure");
        assert!(first > 200_000_000, "{first}");
        let (first, _) = end_of("cpu.util");
        assert!(first < 50_000_000, "{first}");
        // The target's own timelines are not moved: they start where the run
        // says their first event was (`first_ts_ns`), which is the target's
        // first read, a few milliseconds after the window opened -- not one
        // inferred read later, as a poller of the machine may be.
        let oncpu = data.first("cpu.oncpu_time").unwrap();
        let said = oncpu
            .outputs
            .iter()
            .find_map(|output| match output {
                Output::Timeline { first_ts_ns, .. } => *first_ts_ns,
                _ => None,
            })
            .expect("a recording that says its first event");
        let first = tracks(&data, oncpu)[0].points[0].at_ns;
        assert_eq!(first, said - data.opened_ns());
        assert!(first < 50_000_000, "{first}");
    }

    fn power_result(outputs: Vec<Output>, backend: &str) -> Results {
        Results {
            label: "power.energy@system".into(),
            source: "power.energy".into(),
            backend: backend.into(),
            outputs,
            ..Results::default()
        }
    }

    fn data_of(results: Vec<Results>, span_ns: u64) -> Data {
        Data {
            run: Run {
                results,
                ..Run::default()
            },
            live: Some(Live {
                opened_ns: 0,
                now_ns: span_ns,
            }),
            ..Data::default()
        }
    }

    /// Watts per RAPL domain, whichever way the domains came: one event per
    /// domain (the perf unit), or one event naming every domain (sysfs).
    #[test]
    fn power_is_split_by_domain_from_the_events() {
        let tick = 250_000_000u64;
        let pmu: Vec<Event> = (1..=4u64)
            .flat_map(|n| {
                [("package", 5.0), ("dram", 1.0)].map(|(domain, joules)| {
                    event(
                        n * tick,
                        vec![],
                        &[
                            ("joules", AttrValue::Float(joules)),
                            ("domain", AttrValue::Text(domain.into())),
                            ("package", AttrValue::Uint(0)),
                            ("interval_ns", AttrValue::Uint(tick)),
                        ],
                    )
                })
            })
            .collect();
        let drawn = screen(data_of(
            vec![power_result(vec![Output::Raw { events: pmu }], "pmu")],
            5 * tick,
        ));
        assert!(drawn.contains("watts per RAPL domain"), "{drawn}");
        assert!(drawn.contains("package   "), "{drawn}");
        assert!(drawn.contains("dram      "), "{drawn}");
        assert!(drawn.contains("20.0W peak"), "{drawn}");

        let sysfs: Vec<Event> = (1..=4u64)
            .map(|n| {
                event(
                    n * tick,
                    vec![],
                    &[
                        ("joules", AttrValue::Float(6.0)),
                        ("domains", AttrValue::Text("package-0,psys".into())),
                        ("domain.package_0", AttrValue::Float(5.0)),
                        ("domain.psys", AttrValue::Float(1.0)),
                        ("interval_ns", AttrValue::Uint(tick)),
                    ],
                )
            })
            .collect();
        let drawn = screen(data_of(
            vec![power_result(vec![Output::Raw { events: sysfs }], "fs:sys")],
            5 * tick,
        ));
        assert!(drawn.contains("package-0 "), "{drawn}");
        assert!(drawn.contains("psys      "), "{drawn}");
        assert!(drawn.contains("4.00W peak"), "{drawn}");
    }

    /// A timeline cannot name the domains: it is one series. Several points
    /// at each instant are drawn as that many lanes, said to be unnamed; a
    /// series whose instants do not agree is not drawn at all.
    #[test]
    fn a_timeline_of_several_domains_is_split_by_their_order_or_not_drawn() {
        let tick = 250_000_000u64;
        let timeline = |counts: &[usize]| {
            let mut points = Vec::new();
            for (n, count) in counts.iter().enumerate() {
                for lane in 0..*count {
                    points.push(crate::model::Point {
                        offset_ns: n as u64 * tick,
                        value: 1.0 + lane as f64,
                    });
                }
            }
            Output::Timeline {
                first_ts_ns: None,
                series: vec![crate::model::Series {
                    key: "system".into(),
                    points,
                }],
            }
        };
        let drawn = screen(data_of(
            vec![power_result(vec![timeline(&[2, 2, 2, 2])], "pmu")],
            4 * tick,
        ));
        assert!(
            drawn.contains("per domain by name: in the events only"),
            "{drawn}"
        );
        assert!(drawn.contains("#1  "), "{drawn}");
        assert!(drawn.contains("#2  "), "{drawn}");
        let drawn = screen(data_of(
            vec![power_result(vec![timeline(&[2, 1, 2, 2])], "pmu")],
            4 * tick,
        ));
        assert!(drawn.contains("not told apart, so not drawn"), "{drawn}");
        let drawn = screen(data_of(
            vec![power_result(vec![timeline(&[1, 1, 1, 1])], "pmu")],
            4 * tick,
        ));
        assert!(drawn.contains("all       "), "{drawn}");
        assert!(!drawn.contains("package"), "{drawn}");
    }

    /// On a terminal too short for the machine's box, whole sections are
    /// left out from the last and counted; the axis and the border stay.
    #[test]
    fn a_short_terminal_says_which_sections_it_left_out() {
        let screen = text(&draw(fixture("http-server"), "system", 120, 22));
        assert!(
            screen.contains("left out, the terminal is too short"),
            "{screen}"
        );
        assert!(screen.contains("network"), "{screen}");
        assert!(screen.contains("a cell is"), "{screen}");
        assert!(screen.contains("└──"), "{screen}");
        // Tall enough, nothing is left out.
        let screen = screen_of_height(60);
        assert!(!screen.contains("left out"), "{screen}");
    }

    fn screen_of_height(height: u16) -> String {
        text(&draw(fixture("http-server"), "system", 120, height))
    }

    /// A machine section's results are the machine's: a run that labels
    /// none takes the source's results that name no task, and says that it
    /// took them; a task's result is never drawn as the machine's.
    #[test]
    fn a_task_s_result_is_not_drawn_as_the_machine_s() {
        let tick = 250_000_000u64;
        let gpu_mem: Vec<Event> = (1..=4u64)
            .map(|n| {
                event(
                    n * tick,
                    vec![Key::Task { tid: 9 }],
                    &[("bytes", AttrValue::Uint(1 << 20))],
                )
            })
            .collect();
        let cpu: Vec<Event> = (1..=4u64)
            .map(|n| event(n * tick, vec![], &[("busy", AttrValue::Float(0.5))]))
            .collect();
        let result = |source: &str, events| Results {
            label: source.into(),
            source: source.into(),
            outputs: vec![Output::Raw { events }],
            ..Results::default()
        };
        let data = data_of(
            vec![result("gpu.mem", gpu_mem), result("cpu.util", cpu)],
            5 * tick,
        );
        let screen = screen(data);
        assert!(screen.contains("scope not recorded"), "{screen}");
        assert!(!screen.contains("1.00MiB held"), "{screen}");
    }

    /// `s` steps only through sorts whose column is on screen.
    #[test]
    fn sorting_skips_a_column_the_width_hid() {
        let data = fixture("http-server");
        let mut view = System {
            selected: None,
            sort: 0,
            shown: Vec::new(),
            idle: false,
            regions: Vec::new(),
        };
        let focus = Focus::default();
        let ctx = Ctx {
            data: &data,
            against: None,
            focus: &focus,
        };
        // Drawn narrow, the list keeps its first columns only.
        let backend = ratatui::backend::TestBackend::new(70, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view.draw(frame, frame.area(), &ctx))
            .unwrap();
        assert!(!view.shown.contains(&Col::RunQueue), "{:?}", view.shown);
        let press = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
        for _ in 0..SORTS.len() {
            view.key(press, &ctx);
            let column = SORTS[view.sort].column();
            assert!(
                view.shown.contains(&column)
                    || (SORTS[view.sort] == Sort::Cpu && view.shown.contains(&Col::Cpu)),
                "sorted by {column:?}, not shown in {:?}",
                view.shown
            );
        }
    }

    #[test]
    fn a_finished_run_says_last_and_a_live_one_now() {
        let screen = screen(fixture("http-server"));
        assert!(screen.contains("last "), "{screen}");
        assert!(!screen.contains("now "), "{screen}");
    }

    #[test]
    fn every_thread_any_source_saw_is_listed() {
        let data = fixture("http-server");
        let procs = processes(&data, Sort::Cpu);
        let tids: Vec<i32> = procs.rows.iter().map(|row| row.tid).collect();
        // The threads, read from the recording: the ones `cpu.oncpu_time`
        // timed and the ones `sched.offcpu` saw block.
        let mut seen: Vec<i32> = data
            .first("sched.offcpu")
            .unwrap()
            .events()
            .iter()
            .filter_map(Event::tid)
            .collect();
        for output in &data.first("cpu.oncpu_time").unwrap().outputs {
            if let Output::Timeline { series, .. } = output {
                seen.extend(
                    series
                        .iter()
                        .filter_map(|series| series.key.strip_prefix("task:")?.parse::<i32>().ok()),
                );
            }
        }
        seen.sort_unstable();
        seen.dedup();
        assert!(seen.len() >= 3, "{seen:?}");
        for tid in seen {
            assert!(tids.contains(&tid), "{tid} missing from {tids:?}");
        }
        // A row `cpu.oncpu_time` does not read -- here the process, as
        // `/proc/<pid>` sees it, since this run's cpu time is per thread --
        // has no cpu: not a zero.
        let process = procs.rows.iter().find(|row| row.process).unwrap();
        assert!(!process.cells.contains_key(&Col::Cpu));
        assert!(process.cells.contains_key(&Col::Rss));
        assert!(procs
            .rows
            .iter()
            .filter(|row| !row.process)
            .all(|row| row.cells.contains_key(&Col::Faults)));
    }

    #[test]
    fn a_run_without_the_machine_says_so_once() {
        let screen = screen(fixture("vertical-requests"));
        assert!(screen.contains("did not measure the machine"), "{screen}");
        assert_eq!(screen.matches("not in this run").count(), 0, "{screen}");
    }

    #[test]
    fn enter_opens_the_selected_process_in_its_ledger() {
        let data = fixture("http-server");
        let mut view = System {
            selected: None,
            sort: 0,
            shown: Vec::new(),
            idle: false,
            regions: Vec::new(),
        };
        let focus = Focus::default();
        let ctx = Ctx {
            data: &data,
            against: None,
            focus: &focus,
        };
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
        // Each row in turn opens the ledger on itself: a process on its pid,
        // a thread on its tid.
        let rows = processes(&data, SORTS[0]).rows;
        assert!(rows.iter().any(|row| row.process));
        assert!(rows.iter().any(|row| !row.process));
        for row in &rows {
            assert_eq!(view.key(press(KeyCode::Down), &ctx), Action::None);
            match view.key(press(KeyCode::Enter), &ctx) {
                Action::Goto { view, focus } => {
                    assert_eq!(view, "ledger");
                    if row.process {
                        assert_eq!((focus.pid, focus.tid), (Some(row.tid), None));
                    } else {
                        assert_eq!((focus.pid, focus.tid), (None, Some(row.tid)));
                    }
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn sorting_reorders_the_list() {
        let data = fixture("http-server");
        let by_cpu = processes(&data, Sort::Cpu);
        let by_faults = processes(&data, Sort::Faults);
        // Each list is in its column's order, a row without the column last.
        let column = |procs: &Procs, col: Col| -> Vec<f64> {
            procs
                .rows
                .iter()
                .map(|row| row.cells.get(&col).copied().unwrap_or(f64::NEG_INFINITY))
                .collect()
        };
        for (procs, col) in [(&by_cpu, Col::Cpu), (&by_faults, Col::Faults)] {
            let values = column(procs, col);
            assert!(
                values.windows(2).all(|pair| pair[0] >= pair[1]),
                "{col:?}: {values:?}"
            );
        }
        // And the two orders differ on this run: a thread that ran less
        // faulted more.
        let order = |procs: &Procs| procs.rows.iter().map(Proc::key).collect::<Vec<_>>();
        assert_ne!(order(&by_cpu), order(&by_faults));
    }

    /// The machine's processes, as `proc.tasks` lists them each poll: one
    /// row each, by name, sorted, with its cpu over the run and its
    /// resident size; a task first seen at the run's opening counts only
    /// what it ran inside the run.
    #[test]
    fn the_machine_s_processes_are_listed_from_proc_tasks() {
        let tick = 1_000_000_000u64;
        // The run opens at 10 s; poll n is at 10 + n s.
        let opened = 10 * tick;
        let task = |n: u64, pid: i32, comm: &str, busy: f64, since: &str, age: u64, rss: u64| {
            event(
                opened + n * tick,
                vec![Key::Task { tid: pid }],
                &[
                    ("busy", AttrValue::Float(busy)),
                    ("since", AttrValue::Text(since.into())),
                    ("over_ns", AttrValue::Uint(tick)),
                    ("pid", AttrValue::Uint(pid as u64)),
                    ("tid", AttrValue::Uint(pid as u64)),
                    ("comm", AttrValue::Text(comm.into())),
                    ("cpu_ns", AttrValue::Uint(5 * tick)),
                    ("age_ns", AttrValue::Uint(age)),
                    ("rss_bytes", AttrValue::Uint(rss)),
                ],
            )
        };
        let mut events = Vec::new();
        // `old` ran 5 s before the run; it spins one core, then half.
        events.push(task(0, 10, "old", 0.9, "start", 100 * tick, 1 << 20));
        events.push(task(1, 10, "old", 1.0, "poll", 0, 1 << 20));
        events.push(task(2, 10, "old", 0.5, "poll", 0, 2 << 20));
        // `young` is born half a second into the run and has run 0.25 s
        // when first seen.
        let mut first = task(1, 20, "young", 0.5, "start", tick / 2, 4 << 20);
        first.attrs.retain(|attr| attr.name != "cpu_ns");
        first.attrs.push(Attr {
            name: "cpu_ns".into(),
            value: AttrValue::Uint(tick / 4),
        });
        events.push(first);
        events.push(task(2, 20, "young", 0.25, "poll", 0, 4 << 20));
        let results = Results {
            label: "proc.tasks@system".into(),
            source: "proc.tasks".into(),
            backend: "fs:proc".into(),
            outputs: vec![Output::Raw { events }],
            ..Results::default()
        };
        let mut data = data_of(vec![results], 2 * tick);
        data.live = Some(Live {
            opened_ns: opened,
            now_ns: opened + 2 * tick,
        });
        let procs = processes(&data, Sort::Cpu);
        let names: Vec<&str> = procs
            .rows
            .iter()
            .map(|row| row.name.as_deref().unwrap_or("-"))
            .collect();
        assert_eq!(names, ["old", "young"]);
        let old = &procs.rows[0];
        assert!(old.process);
        assert_eq!(old.cells[&Col::CpuTime], 1.5e9, "two polls, not its life");
        assert_eq!(old.cells[&Col::Rss], (2 << 20) as f64, "the last poll's");
        assert_eq!(old.cpu.as_ref().unwrap().points.len(), 2);
        let young = &procs.rows[1];
        assert_eq!(young.cells[&Col::CpuTime], 0.25e9 + 0.25e9, "born inside");
        assert!(procs.columns.iter().any(|(col, _)| *col == Col::Spark));
        let by_memory = processes(&data, Sort::Memory);
        assert_eq!(by_memory.rows[0].name.as_deref(), Some("young"));
        let drawn = screen(data);
        assert!(drawn.contains("old "), "{drawn}");
        assert!(drawn.contains("every task measured"), "{drawn}");
    }

    /// A timeline split by `dir` is one row per device, read apart from
    /// written; a discard is not in the bytes, as `/proc/diskstats`' own
    /// total does not hold it.
    #[test]
    fn a_timeline_split_by_dir_is_one_row_per_device_its_parts_apart() {
        use crate::model::{Point as Reading, Series};
        let series = |dir: &str, value: f64| Series {
            key: format!("dev:259+0 dir:{dir}"),
            points: (0..4)
                .map(|n| Reading {
                    offset_ns: n * 250_000_000,
                    value,
                })
                .collect(),
        };
        let results = Results {
            label: "io.bytes@system".into(),
            source: "io.bytes".into(),
            backend: "fs:proc".into(),
            outputs: vec![Output::Timeline {
                series: vec![
                    series("read", 1000.0),
                    series("write", 3000.0),
                    series("discard", 50_000.0),
                ],
                first_ts_ns: None,
            }],
            ..Results::default()
        };
        let data = data_of(vec![results.clone()], 1_000_000_000);
        let all = tracks(&data, &results);
        assert_eq!(all.len(), 1, "{all:?}");
        assert_eq!(all[0].key, "dev:259+0");
        assert_eq!(all[0].total(), 16_000.0);
        assert_eq!(
            all[0].part_totals(),
            vec![("read", 4000.0), ("written", 12_000.0)]
        );
        assert_eq!(device_name(&all[0].key), "259:0");
        assert_eq!(key_parts("task:7 cpu:2"), ["task:7", "cpu:2"]);
        assert_eq!(task_of("cpu:2 task:7"), Some(7));
        assert_eq!(key_parts("package 1"), ["package 1"]);
    }

    fn event(ts_ns: u64, keys: Vec<Key>, attrs: &[(&str, AttrValue)]) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: String::new(),
            keys,
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

    /// A live run holds events, not timelines: the view draws from them, and
    /// they say what a timeline does not -- bytes in apart from out, the cpu
    /// by state, a process's name.
    #[test]
    fn a_live_run_is_drawn_from_its_events_and_says_more() {
        let start = 1_000_000_000u64;
        let tick = 250_000_000u64;
        let iface = || Key::Iface {
            netns: 0,
            index: 2,
            name: "eth9".into(),
        };
        let net: Vec<Event> = (1..=8)
            .map(|n| {
                event(
                    start + n * tick,
                    vec![iface()],
                    &[
                        ("bytes", AttrValue::Uint(3000)),
                        ("rx", AttrValue::Uint(2000)),
                        ("tx", AttrValue::Uint(1000)),
                    ],
                )
            })
            .collect();
        let cpu: Vec<Event> = (1..=8)
            .map(|n| {
                event(
                    start + n * tick,
                    vec![],
                    &[
                        ("busy", AttrValue::Float(0.5)),
                        ("user", AttrValue::Float(0.3)),
                        ("system", AttrValue::Float(0.1)),
                        ("iowait", AttrValue::Float(0.1)),
                    ],
                )
            })
            .collect();
        let oncpu: Vec<Event> = (1..=8)
            .map(|n| {
                event(
                    start + n * tick,
                    vec![Key::Task { tid: 77 }],
                    &[
                        ("ns", AttrValue::Uint(tick / 2)),
                        ("comm", AttrValue::Text("busybee".into())),
                    ],
                )
            })
            .collect();
        let result = |label: &str, source: &str, events: Vec<Event>| Results {
            label: label.into(),
            source: source.into(),
            outputs: vec![Output::Raw { events }],
            ..Results::default()
        };
        let data = Data {
            run: Run {
                results: vec![
                    result("net.bytes@system", "net.bytes", net),
                    result("cpu.util@system", "cpu.util", cpu),
                    result("cpu.oncpu_time", "cpu.oncpu_time", oncpu),
                ],
                ..Run::default()
            },
            live: Some(Live {
                opened_ns: start,
                now_ns: start + 10 * tick,
            }),
            ..Data::default()
        };
        let screen = screen(data.clone());
        assert!(screen.contains("eth9"), "{screen}");
        assert!(screen.contains("15.6KiB in"), "{screen}");
        assert!(screen.contains("7.81KiB out"), "{screen}");
        assert!(screen.contains("user 60.0%"), "{screen}");
        assert!(screen.contains("iowait 20.0%"), "{screen}");
        assert!(screen.contains("busybee"), "{screen}");
        assert!(screen.contains("said at the end"), "{screen}");
        // Half a cpu for eight ticks of a ten-tick run, from the events' own
        // nanoseconds.
        let procs = processes(&data, Sort::Cpu);
        let cpu = procs.rows[0].cells[&Col::Cpu];
        assert!((cpu - 0.4).abs() < 0.01, "{cpu}");
    }

    #[test]
    fn cells_hold_a_value_until_the_next_and_keep_the_peak() {
        let axis = Axis { span_ns: 1000 };
        let point = |at_ns, shown| Point {
            at_ns,
            read: shown,
            shown,
        };
        let cells = axis.cells(&[point(0, 1.0), point(450, 5.0), point(480, 2.0)], 10);
        let mut expected = vec![Some(1.0); 4];
        expected.push(Some(5.0));
        expected.extend([None; 5]);
        assert_eq!(cells, expected);
    }

    /// A value is held across the reads' usual spacing, not across reads
    /// that did not happen: a gap of lost reads is blank.
    #[test]
    fn cells_do_not_hold_a_value_across_missing_reads() {
        let axis = Axis { span_ns: 1000 };
        let point = |at_ns| Point {
            at_ns,
            read: 1.0,
            shown: 1.0,
        };
        let points: Vec<Point> = [0, 100, 200, 300, 800, 900].map(point).to_vec();
        let cells = axis.cells(&points, 10);
        // Held for a read and a half past the last one, then blank.
        assert_eq!(cells[4], Some(1.0));
        assert_eq!(cells[5], None, "{cells:?}");
        assert_eq!(cells[7], None, "{cells:?}");
        assert_eq!(cells[8], Some(1.0));
    }

    #[test]
    fn amounts_are_drawn_per_second() {
        let point = |at_ns, read| Point {
            at_ns,
            read,
            shown: 0.0,
        };
        let mut points = vec![point(0, 10.0), point(250_000_000, 5.0)];
        per_second(&mut points, Per::Tick, &[]);
        assert_eq!(points[0].shown, 40.0);
        assert_eq!(points[1].shown, 20.0);
    }

    #[test]
    fn ceilings_are_round() {
        assert_eq!(ceiling(0.081), 0.1);
        assert_eq!(ceiling(0.02), 0.02);
        assert_eq!(ceiling(0.031), 0.05);
        assert_eq!(ceiling(0.0), 1.0);
    }
}
