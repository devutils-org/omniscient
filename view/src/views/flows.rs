//! The `flows` view: where the bytes went: process to socket to interface, process to file to device.
//!
//! A stub until the `flows` stream fills it: it lists what it needs and
//! what the run holds of it. `docs/view/README.md` says what the frame hands a
//! view and how a view is tested on its snapshot.

use ratatui::layout::Rect;
use ratatui::Frame;

use super::{Ctx, Need, Scope, View};

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
        emit: &["summary", "hist"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "bytes per file",
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

pub struct Flows;

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
        super::draw_stub(frame, area, ctx, self);
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(Flows)
}
