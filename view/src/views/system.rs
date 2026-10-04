//! The `system` view: the overview: cpu, memory, disk, network, GPU and power, and the processes.
//!
//! A stub until the `system` stream fills it: it lists what it needs and
//! what the run holds of it. `docs/view/README.md` says what the frame hands a
//! view and how a view is tested on its snapshot.

use ratatui::layout::Rect;
use ratatui::Frame;

use super::{Ctx, Need, Scope, View};

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
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "the disk panel: bytes read and written per device",
    },
    Need {
        source: "net.bytes",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "the network panel: bytes in and out per interface",
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

pub struct System;

impl View for System {
    fn name(&self) -> &'static str {
        "system"
    }

    fn title(&self) -> &'static str {
        "the overview: cpu, memory, disk, network, GPU and power, and the processes"
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        super::draw_stub(frame, area, ctx, self);
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(System)
}
