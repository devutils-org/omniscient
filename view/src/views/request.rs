//! The `request` view: what one request costs: its life by state, and its share of each resource.
//!
//! A stub until the `request` stream fills it: it lists what it needs and
//! what the run holds of it. `docs/view/README.md` says what the frame hands a
//! view and how a view is tested on its snapshot.

use ratatui::layout::Rect;
use ratatui::Frame;

use super::{Ctx, Need, Scope, View};

/// What this view needs measured.
pub const NEEDS: &[Need] = &[Need {
    source: "net.request",
    mode: "trace",
    emit: &["raw"],
    keys: &[],
    stack: None,
    scope: Scope::Target,
    why: "each request's life by state, and the network on either side of it",
}];

pub struct Request;

impl View for Request {
    fn name(&self) -> &'static str {
        "request"
    }

    fn title(&self) -> &'static str {
        "what one request costs: its life by state, and its share of each resource"
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        super::draw_stub(frame, area, ctx, self);
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(Request)
}
