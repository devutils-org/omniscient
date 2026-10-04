//! The `lanes` view: threads side by side: one lane per thread and per GPU stream, along time.
//!
//! A stub until the `lanes` stream fills it: it lists what it needs and
//! what the run holds of it. `docs/view/README.md` says what the frame hands a
//! view and how a view is tested on its snapshot.

use ratatui::layout::Rect;
use ratatui::Frame;

use super::{Ctx, Need, Scope, View};

/// What this view needs measured.
pub const NEEDS: &[Need] = &[
    Need {
        source: "sched.switches",
        mode: "trace",
        emit: &["raw"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "each thread's on and off cpu",
    },
    Need {
        source: "sched.offcpu",
        mode: "trace",
        emit: &["raw", "stack"],
        keys: &[],
        stack: Some("fp"),
        scope: Scope::Target,
        why: "why a thread was off cpu, each block on its own",
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
];

pub struct Lanes;

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
        super::draw_stub(frame, area, ctx, self);
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(Lanes)
}
