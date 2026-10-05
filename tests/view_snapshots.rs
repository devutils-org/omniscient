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

/// The sizes every view is drawn at to be seen filling the terminal: a
/// small one, a large one and a wide one.
const SIZES: [(u16, u16); 3] = [(80, 24), (160, 48), (240, 60)];

/// The views this file holds to the sizes, each on its own workload's
/// recording. `system` is the model the others follow; it and `process`
/// are held to the sizes by their own tests (`tests/view_top_*.rs`).
const SIZED: [&str; 8] = [
    "flame", "ledger", "request", "timebar", "heatmap", "lanes", "wake", "flows",
];

/// How far down and how far right a drawing reaches inside the frame, as
/// a share of the rows between the tabs and the help line and of the
/// columns: what is not a frame's border or a blank.
fn reach(buffer: &omn_view::ratatui::buffer::Buffer) -> (f64, f64) {
    let text = omn_view::snapshot::text(buffer);
    let rows: Vec<&str> = text.lines().collect();
    // The header and the tabs above, the help line below.
    let body = &rows[2..rows.len() - 1];
    let border = |c: char| matches!(c, '│' | '┃' | '─' | '┌' | '┐' | '└' | '┘' | '├' | '┤' | ' ');
    let mut lowest = 0;
    let mut rightmost = 0;
    for (index, row) in body.iter().enumerate() {
        let cells: Vec<char> = row.chars().collect();
        if let Some(last) = cells.iter().rposition(|c| !border(*c)) {
            lowest = index + 1;
            rightmost = rightmost.max(last + 1);
        }
    }
    (
        lowest as f64 / body.len() as f64,
        rightmost as f64 / f64::from(buffer.area.width),
    )
}

/// The user's remark (2026-10-05): every view fills the terminal the way
/// `system` does, not drawn in a corner with the rest empty. Each view is
/// drawn at the three sizes, held to a snapshot at each, and its drawing
/// must reach the last quarter of the rows and of the columns.
#[test]
fn every_view_fills_the_terminal() {
    for view in SIZED {
        let recording = format!("workload-{view}");
        for (width, height) in SIZES {
            let buffer = draw(fixture(&recording), view, width, height);
            let (down, right) = reach(&buffer);
            assert!(
                down >= 0.75 && right >= 0.75,
                "{view} at {width}x{height} reaches {:.0}% of the rows and {:.0}% of the \
                 columns: drawn in a corner\n{}",
                down * 100.0,
                right * 100.0,
                omn_view::snapshot::text(&buffer)
            );
            assert_snapshot(&format!("{view}-{width}x{height}--{recording}"), &buffer);
        }
    }
}

/// The two runs the views that draw a difference are compared on: the same
/// program, the second made heavier.
const AGAINST: (&str, &str) = ("cpython-hot", "cpython-hot-heavier");

/// `--against` in the standard set: each view that draws a difference,
/// drawn on one run against the other, at the snapshot size and the wide
/// one.
#[test]
fn every_view_against_another_run() {
    use omn_view::snapshot::draw_with;
    use omn_view::views::Focus;
    for view in ["flame", "ledger"] {
        for (width, height) in [(WIDTH, HEIGHT), (240, 60)] {
            let buffer = draw_with(
                fixture(AGAINST.0),
                view,
                Some(fixture(AGAINST.1)),
                Focus::default(),
                width,
                height,
            );
            assert_snapshot(
                &format!(
                    "{view}-against-{width}x{height}--{}--{}",
                    AGAINST.0, AGAINST.1
                ),
                &buffer,
            );
        }
    }
}

/// A focus in the standard set: each view narrowed to one thread (or, for
/// `request`, one request) of its own workload that is not the only one
/// there, so what the focus leaves out is seen gone.
#[test]
fn every_view_focused() {
    use omn_view::snapshot::draw_with;
    use omn_view::views::Focus;
    let thread = |tid| Focus {
        tid: Some(tid),
        ..Focus::default()
    };
    let focused: [(&str, Focus); 8] = [
        ("flame", thread(2620698)),
        ("ledger", thread(2621704)),
        (
            "request",
            Focus {
                request: Some("a003".to_owned()),
                ..Focus::default()
            },
        ),
        ("timebar", thread(2620839)),
        ("heatmap", thread(2621526)),
        ("lanes", thread(2621957)),
        ("wake", thread(2621138)),
        ("flows", thread(2621408)),
    ];
    for (view, focus) in focused {
        let recording = format!("workload-{view}");
        let name = focus
            .tid
            .map(|tid| format!("tid{tid}"))
            .or(focus.request.clone())
            .unwrap_or_default();
        let buffer = draw_with(fixture(&recording), view, None, focus, WIDTH, HEIGHT);
        assert_snapshot(&format!("{view}-focus-{name}--{recording}"), &buffer);
    }
}
