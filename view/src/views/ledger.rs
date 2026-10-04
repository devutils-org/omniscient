//! The `ledger` view: what a process costs, across every resource omn measures.
//!
//! A stub until the `ledger` stream fills it: it lists what it needs and
//! what the run holds of it. `docs/view/README.md` says what the frame hands a
//! view and how a view is tested on its snapshot.

use ratatui::layout::Rect;
use ratatui::Frame;

use super::{Ctx, Need, Scope, View};

/// What this view needs measured.
pub const NEEDS: &[Need] = &[
    Need {
        source: "cpu.oncpu_time",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &["task"],
        stack: None,
        scope: Scope::Target,
        why: "cpu time, per thread",
    },
    Need {
        source: "mem.rss",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "memory held",
    },
    Need {
        source: "mem.faults",
        mode: "trace",
        emit: &["summary"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "memory faulted in",
    },
    Need {
        source: "io.syscall",
        mode: "trace",
        emit: &["summary", "hist"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "bytes read and written, and their latency as the program saw it",
    },
    Need {
        source: "net.tcp.retransmit",
        mode: "trace",
        emit: &["summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "retransmissions, the machine's: a retransmission is sent from a timer, so the collector cannot give it to a process",
    },
    Need {
        source: "gpu.mem",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "GPU memory held",
    },
    Need {
        source: "gpu.kernel",
        mode: "trace",
        emit: &["raw", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::Target,
        why: "GPU time per kernel",
    },
    Need {
        source: "power.energy",
        mode: "poll@250ms",
        emit: &["timeline", "summary"],
        keys: &[],
        stack: None,
        scope: Scope::System,
        why: "energy, where it can be attributed",
    },
];

pub struct Ledger;

impl View for Ledger {
    fn name(&self) -> &'static str {
        "ledger"
    }

    fn title(&self) -> &'static str {
        "what a process costs, across every resource omn measures"
    }

    fn needs(&self) -> &'static [Need] {
        NEEDS
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &Ctx<'_>) {
        super::draw_stub(frame, area, ctx, self);
    }
}

pub fn view() -> Box<dyn View> {
    Box::new(Ledger)
}
