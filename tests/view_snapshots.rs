//! Every view drawn on every fixture recording, against its snapshot in
//! `docs/view/gallery/`.
//!
//! One test per view, so a view's stream runs its own
//! (`cargo test --test view_snapshots flame`) and a failure names the view.
//! A snapshot is `<view>--<fixture>.txt` -- what is compared -- with
//! `<view>--<fixture>.ansi` beside it, the same screen with its dimming and
//! colours, for `cat`. `OMN_VIEW_BLESS=1` rewrites them once a change is the
//! one meant; a snapshot that does not exist yet is written and fails once,
//! so it is looked at before it counts.
//!
//! The frame's own snapshots -- the help, a live run, a refusal -- are here
//! too, as `frame--*`.

use omn_view::app::{App, Cost};
use omn_view::model::Refusal;
use omn_view::snapshot::{assert_snapshot, draw, draw_app, fixture, fixture_names, HEIGHT, WIDTH};

/// Draws one view on every fixture and holds each to its snapshot.
fn every_fixture(view: &str) {
    let names = fixture_names();
    assert!(
        !names.is_empty(),
        "no fixture recording: tests/view_fixtures/record.sh"
    );
    for name in names {
        let buffer = draw(fixture(&name), view, WIDTH, HEIGHT);
        assert_snapshot(&format!("{view}--{name}"), &buffer);
    }
}

macro_rules! views {
    ($($view:ident),* $(,)?) => {
        $(
            #[test]
            fn $view() {
                every_fixture(stringify!($view));
            }
        )*

        /// The tests above are the registry, view for view: a view
        /// registered without a test here would have no snapshot.
        #[test]
        fn every_registered_view_has_its_test() {
            let tested = [$(stringify!($view)),*];
            for view in omn_view::views::registry() {
                assert!(
                    tested.contains(&view.name()),
                    "the view `{}` has no snapshot test in tests/view_snapshots.rs",
                    view.name()
                );
            }
        }
    };
}

views!(system, flame, ledger, request, timebar, heatmap, lanes, wake, flows, process);

/// The fixture every frame snapshot is drawn on.
const FRAME_FIXTURE: &str = "http-server";

fn frame_app() -> App {
    let mut app = App::new(fixture(FRAME_FIXTURE));
    app.cost = Cost::Snapshot;
    app
}

#[test]
fn frame_help() {
    let mut app = frame_app();
    app.help = true;
    assert_snapshot("frame--help", &draw_app(&mut app, WIDTH, HEIGHT));
}

#[test]
fn frame_gallery() {
    let mut app = frame_app();
    app.gallery = true;
    // The frame is drawn on a view its stream changes rarely, so a change to
    // a view is not a change to the frame's snapshots.
    app.open("ledger").unwrap();
    assert_snapshot("frame--gallery", &draw_app(&mut app, WIDTH, HEIGHT));
}

/// A live run, half way: every figure provisional, no aggregate yet.
#[test]
fn frame_live() {
    let data = fixture(FRAME_FIXTURE);
    let half = data.opened_ns() + (data.closed_ns() - data.opened_ns()) / 2;
    let mut app = App::new(omn_view::feed::as_live_until(&data, half));
    app.cost = Cost::Snapshot;
    app.open("ledger").unwrap();
    assert_snapshot("frame--live", &draw_app(&mut app, WIDTH, HEIGHT));
}

/// A refusal drawn where its panel would have been.
#[test]
fn frame_refusal() {
    use omn_view::ratatui::backend::TestBackend;
    use omn_view::ratatui::Terminal;
    let refusal = Refusal {
        label: "gpu.kernel".to_owned(),
        source: "gpu.kernel".to_owned(),
        reason: "no collector is written for `gpu.kernel` on backend `vendor:cupti` yet".to_owned(),
    };
    let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
    terminal
        .draw(|frame| omn_view::trust::render_refusal(frame, frame.area(), &refusal))
        .unwrap();
    assert_snapshot("frame--refusal", terminal.backend().buffer());
}

/// A narrow terminal still draws the frame, cut rather than broken.
#[test]
fn frame_narrow() {
    let mut app = frame_app();
    assert_snapshot("frame--narrow", &draw_app(&mut app, 60, 20));
}

/// Each view after a click, on its own workload's recording where it has
/// something to select there: the selection a
/// click makes is drawn as the keys' would be. A view's regions are computed
/// from the layout it draws with; this is where each is seen to land on what
/// was clicked. `needle` is text the view draws, `down` the rows below it
/// the click lands on (a row of a list under its header).
#[test]
fn every_view_after_a_click() {
    use omn_view::snapshot::{click, find};
    let clicks: [(&str, &str, &str, u16); 9] = [
        ("system", "workload-system", "ID ", 2),
        ("flame", "workload-flame", "py::burn", 0),
        ("ledger", "workload-ledger", "memory held", 0),
        ("request", "workload-request", "a003", 0),
        ("timebar", "workload-timebar", "worker", 0),
        ("heatmap", "workload-heatmap", "▓███", 0),
        ("lanes", "workload-lanes", "lane-b", 0),
        ("wake", "workload-wake", "relay-b", 0),
        // The flows workload's one socket is selected from the start; the
        // file server has files beside it.
        ("flows", "file-serving", "btrfs", 0),
    ];
    for (view, recording, needle, down) in clicks {
        let mut app = App::new(fixture(recording));
        app.cost = Cost::Snapshot;
        app.open(view).unwrap();
        let before = draw_app(&mut app, WIDTH, HEIGHT);
        let (column, row) = find(&before, needle)
            .unwrap_or_else(|| panic!("`{needle}` is not drawn by {view} on its workload"));
        // Two cells in, so a click lands inside a frame's label rather than
        // on its left edge.
        click(&mut app, column + 2, row + down);
        assert_snapshot(
            &format!("{view}-click--{recording}"),
            &draw_app(&mut app, WIDTH, HEIGHT),
        );
    }
}
