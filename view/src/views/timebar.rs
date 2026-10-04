//! The `timebar` view: where the wall-clock went: one bar per thread, by state.
//!
//! A stub until the `timebar` stream fills it: it lists what it needs and
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
        why: "on cpu",
    },
    Need {
        source: "sched.runqlat",
        mode: "trace",
        emit: &["raw", "summary"],
        keys: &["task"],
        stack: None,
        scope: Scope::Target,
        why: "waiting to run",
    },
    Need {
        source: "sched.offcpu",
        mode: "trace",
        emit: &["stack"],
        keys: &[],
        stack: Some("fp"),
        scope: Scope::Target,
        why: "blocked, with the stack it blocked in",
    },
    Need {
        source: "sched.futex",
        mode: "trace",
        emit: &["summary"],
        keys: &["task"],
        stack: None,
        scope: Scope::Target,
        why: "waiting on a lock",
    },
];

pub struct Timebar;

impl View for Timebar {
    fn name(&self) -> &'static str {
        "timebar"
    }

    fn title(&self) -> &'static str {
        "where the wall-clock went: one bar per thread, by state"
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        super::draw_stub(frame, area, ctx, self);
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(Timebar)
}
