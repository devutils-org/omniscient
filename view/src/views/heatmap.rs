//! The `heatmap` view: latency over time: time across, latency up, density in colour.
//!
//! A stub until the `heatmap` stream fills it: it lists what it needs and
//! what the run holds of it. `docs/view/README.md` says what the frame hands a
//! view and how a view is tested on its snapshot.

use ratatui::layout::Rect;
use ratatui::Frame;

use super::{Ctx, Need, Scope, View};

/// What this view needs measured.
pub const NEEDS: &[Need] = &[
    Need {
        source: "io.latency",
        mode: "trace",
        emit: &["raw", "hist"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "block request latency, of the requests the target issued",
    },
    Need {
        source: "sched.runqlat",
        mode: "trace",
        emit: &["raw", "summary"],
        keys: &["task"],
        stack: None,
        scope: Scope::Target,
        why: "run queue latency",
    },
    Need {
        source: "net.socket",
        mode: "trace",
        emit: &["raw", "hist"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "socket read and write latency",
    },
    Need {
        source: "gpu.kernel",
        mode: "trace",
        emit: &["raw", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "kernel durations",
    },
];

pub struct Heatmap;

impl View for Heatmap {
    fn name(&self) -> &'static str {
        "heatmap"
    }

    fn title(&self) -> &'static str {
        "latency over time: time across, latency up, density in colour"
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        super::draw_stub(frame, area, ctx, self);
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(Heatmap)
}
