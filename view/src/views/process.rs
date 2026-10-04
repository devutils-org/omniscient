//! The `process` view: one process at a glance.
//!
//! The first screen of `omn top -- <program>` and of `--pid`: every view's
//! essence for that one process on one screen, a number and a colour per
//! tile, each tile opening the view that has the detail. Registered here as a
//! stub -- it lists what it reads and whether the run holds it -- until its
//! stream draws it (`docs/view/README.md`, "The links between views").

use ratatui::layout::Rect;
use ratatui::Frame;

use super::{draw_stub, Ctx, Need, View};

/// The tiles read what the other views already ask for: the process view
/// asks for nothing of its own, so it never changes what a live run
/// collects.
const NEEDS: &[Need] = &[];

#[derive(Default)]
pub struct Process;

impl View for Process {
    fn name(&self) -> &'static str {
        "process"
    }

    fn title(&self) -> &'static str {
        "one process at a glance: every view's essence, each tile opening its view"
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        draw_stub(frame, area, ctx, self);
    }
}

pub fn view() -> Box<dyn View> {
    Box::<Process>::default()
}
