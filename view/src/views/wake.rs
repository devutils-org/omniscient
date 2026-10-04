//! The `wake` view: who woke whom: from a blocked thread to what unblocked it.
//!
//! A stub until the `wake` stream fills it: it lists what it needs and
//! what the run holds of it. `docs/view/README.md` says what the frame hands a
//! view and how a view is tested on its snapshot.

use ratatui::layout::Rect;
use ratatui::Frame;

use super::{Ctx, Need, Scope, View};

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

pub struct Wake;

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
        super::draw_stub(frame, area, ctx, self);
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(Wake)
}
