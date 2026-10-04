//! The `flame` view: a flamegraph in the terminal: on-cpu and off-cpu stacks, and two runs' difference.
//!
//! A stub until the `flame` stream fills it: it lists what it needs and
//! what the run holds of it. `docs/view/README.md` says what the frame hands a
//! view and how a view is tested on its snapshot.

use ratatui::layout::Rect;
use ratatui::Frame;

use super::{Ctx, Need, Scope, View};

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

pub struct Flame;

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

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        super::draw_stub(frame, area, ctx, self);
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(Flame)
}
