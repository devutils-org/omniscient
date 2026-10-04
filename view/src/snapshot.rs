//! A view drawn into text, for its test and for the person judging it.
//!
//! ratatui's test backend draws the whole frame -- header, tabs, the view,
//! help line -- from a recording into a buffer of cells. Two files are written
//! from it under `docs/view/gallery/`: `<name>.txt`, the characters, which a
//! test compares; and `<name>.ansi`, the same with its colours and dimming
//! as terminal escapes, which `cat` shows as the screen looked -- the trust
//! style is in the dimming, and plain text cannot carry it.
//!
//! `OMN_VIEW_BLESS=1` rewrites both from what is drawn now.

use std::path::{Path, PathBuf};

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use crate::app::{App, Cost};
use crate::model::Data;

/// The size every snapshot is drawn at: a common terminal, wide enough for
/// the tabs.
pub const WIDTH: u16 = 120;
pub const HEIGHT: u16 = 40;

/// Where the gallery is, in this checkout.
pub fn gallery_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/view/gallery")
}

/// Where the fixture recordings are, in this checkout.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/view_fixtures")
}

/// The fixture recording of this name (a directory under `fixtures_dir`).
pub fn fixture(name: &str) -> Data {
    let path = fixtures_dir().join(name);
    let mut data = Data::load(&path).unwrap_or_else(|error| panic!("fixture `{name}`: {error}"));
    // The path a fixture was read from differs between checkouts; the
    // snapshot must not.
    data.origin = format!("fixture {name}");
    data
}

/// The fixtures every view is drawn on, by name.
pub fn fixture_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(fixtures_dir())
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().join("results.json").is_file())
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Draws `view` on `data` at the snapshot size, and returns the buffer.
pub fn draw(data: Data, view: &str, width: u16, height: u16) -> Buffer {
    let mut app = App::new(data);
    app.cost = Cost::Snapshot;
    app.open(view).unwrap_or_else(|error| panic!("{error}"));
    draw_app(&mut app, width, height)
}

/// Draws an app as it stands.
pub fn draw_app(app: &mut App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
    terminal
        .draw(|frame| app.draw(frame))
        .expect("a test terminal draws");
    terminal.backend().buffer().clone()
}

/// The buffer's characters, one line per row, trailing blanks trimmed.
pub fn text(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in 0..area.height {
        let mut line = String::new();
        let mut skip = 0;
        for x in 0..area.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let symbol = buffer[(x, y)].symbol();
            line.push_str(symbol);
            // A wide character covers the next cell, which holds a blank.
            skip = unicode_width(symbol).saturating_sub(1);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn unicode_width(symbol: &str) -> usize {
    ratatui::text::Span::raw(symbol).width()
}

/// The buffer with its colours and modifiers as SGR escapes: `cat` it.
pub fn ansi(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in 0..area.height {
        let mut current = String::new();
        let mut skip = 0;
        for x in 0..area.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let cell = &buffer[(x, y)];
            let sgr = sgr(cell.fg, cell.bg, cell.modifier);
            if sgr != current {
                out.push_str("\x1b[0m");
                out.push_str(&sgr);
                current = sgr;
            }
            out.push_str(cell.symbol());
            skip = unicode_width(cell.symbol()).saturating_sub(1);
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

fn sgr(fg: Color, bg: Color, modifier: Modifier) -> String {
    let mut codes: Vec<String> = Vec::new();
    for (flag, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
    ] {
        if modifier.contains(flag) {
            codes.push(code.to_owned());
        }
    }
    if let Some(code) = colour(fg, false) {
        codes.push(code);
    }
    if let Some(code) = colour(bg, true) {
        codes.push(code);
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

fn colour(colour: Color, background: bool) -> Option<String> {
    let base = if background { 40 } else { 30 };
    let code = match colour {
        Color::Reset => return None,
        Color::Black => base,
        Color::Red => base + 1,
        Color::Green => base + 2,
        Color::Yellow => base + 3,
        Color::Blue => base + 4,
        Color::Magenta => base + 5,
        Color::Cyan => base + 6,
        Color::Gray => base + 7,
        Color::DarkGray => base + 60,
        Color::LightRed => base + 61,
        Color::LightGreen => base + 62,
        Color::LightYellow => base + 63,
        Color::LightBlue => base + 64,
        Color::LightMagenta => base + 65,
        Color::LightCyan => base + 66,
        Color::White => base + 67,
        Color::Indexed(index) => {
            return Some(format!("{};5;{index}", if background { 48 } else { 38 }))
        }
        Color::Rgb(r, g, b) => {
            return Some(format!(
                "{};2;{r};{g};{b}",
                if background { 48 } else { 38 }
            ))
        }
    };
    Some(code.to_string())
}

/// Compares a view's drawing with its snapshot `docs/view/gallery/<name>.txt`,
/// and writes `<name>.ansi` beside it. A snapshot that does not exist yet, or
/// any snapshot under `OMN_VIEW_BLESS=1`, is written from this drawing; the
/// first case still fails, so a new snapshot is looked at before it counts.
pub fn assert_snapshot(name: &str, buffer: &Buffer) {
    let dir = gallery_dir();
    let txt = dir.join(format!("{name}.txt"));
    let drawn = text(buffer);
    let bless = std::env::var_os("OMN_VIEW_BLESS").is_some_and(|value| value == "1");
    let held = std::fs::read_to_string(&txt).ok();
    if bless || held.is_none() {
        std::fs::create_dir_all(&dir).expect("the gallery directory");
        std::fs::write(&txt, &drawn).expect("the snapshot is written");
        std::fs::write(dir.join(format!("{name}.ansi")), ansi(buffer))
            .expect("the snapshot is written");
        if held.is_none() && !bless {
            panic!(
                "{} did not exist and was written from this drawing: look at it (`cat {}`), \
                 then run again",
                txt.display(),
                dir.join(format!("{name}.ansi")).display()
            );
        }
        return;
    }
    let held = held.unwrap_or_default();
    if held != drawn {
        let first = held
            .lines()
            .zip(drawn.lines())
            .position(|(one, two)| one != two)
            .unwrap_or(held.lines().count().min(drawn.lines().count()));
        panic!(
            "{} differs from what is drawn now, first at line {}:\n  held:  {:?}\n  drawn: {:?}\n\
             OMN_VIEW_BLESS=1 rewrites it once the change is the one meant.\n\n{drawn}",
            txt.display(),
            first + 1,
            held.lines().nth(first).unwrap_or(""),
            drawn.lines().nth(first).unwrap_or(""),
        );
    }
}
