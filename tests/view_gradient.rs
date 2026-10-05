//! The usage gradient (`omn_view::trust::usage`) checked where no person
//! looks: on a dark and on a light terminal, in the 256 colours it is sent
//! as.
//!
//! Each step of the ramp is one of the 256-colour cube's indices; a terminal
//! that has only those 256 draws it as the cube's own RGB, which is what is
//! measured here. A step reads when its contrast ratio with the background
//! (WCAG 2's, the measure for a figure's colour against what is behind it)
//! is at least 3:1, the floor WCAG sets for graphical objects -- a bar, a
//! sparkline -- and the steps go from green to red in order.
//!
//! The strips are also drawn into `docs/view/gallery/gradient--*.ansi` on
//! each background, for `cat`.

use omn_view::ratatui::buffer::Buffer;
use omn_view::ratatui::layout::Rect;
use omn_view::ratatui::style::{Color, Style};
use omn_view::snapshot::assert_snapshot;
use omn_view::trust::usage;

/// Backgrounds a terminal is commonly given: pure black and a dark grey,
/// pure white and a warm light one.
const DARK: [(&str, (u8, u8, u8)); 2] = [("black", (0, 0, 0)), ("dark", (0x1e, 0x1e, 0x1e))];
const LIGHT: [(&str, (u8, u8, u8)); 2] =
    [("white", (255, 255, 255)), ("light", (0xfd, 0xf6, 0xe3))];

/// WCAG 2's floor for a graphical object against its background.
const FLOOR: f64 = 3.0;

/// A ramp of the same six even steps, green to red, every one of them at
/// least 3:1 on the four backgrounds above: what the light check asks of
/// `usage::RAMP` (filed to the stream that owns `view/src/trust.rs`).
const READS_ON_BOTH: [u8; 6] = [28, 64, 100, 136, 166, 160];

/// An index of the 256-colour palette as the RGB xterm gives it: the 16
/// system colours are the terminal's own and not measured.
fn rgb(index: u8) -> (u8, u8, u8) {
    match index {
        16..=231 => {
            let i = index - 16;
            let level = |c: u8| if c == 0 { 0 } else { 55 + 40 * c };
            (level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        232..=255 => {
            let grey = 8 + 10 * (index - 232);
            (grey, grey, grey)
        }
        _ => panic!("index {index} is a system colour: the terminal chooses it"),
    }
}

fn indexed(colour: Color) -> u8 {
    match colour {
        Color::Indexed(index) => index,
        other => panic!("{other:?} is not one of the 256 colours"),
    }
}

/// WCAG 2's relative luminance.
fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let channel = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

fn contrast(one: (u8, u8, u8), two: (u8, u8, u8)) -> f64 {
    let (a, b) = (luminance(one), luminance(two));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// The hue of a colour, in degrees.
fn hue((r, g, b): (u8, u8, u8)) -> f64 {
    let (r, g, b) = (f64::from(r), f64::from(g), f64::from(b));
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    if max == min {
        return 0.0;
    }
    let h = if max == r {
        ((g - b) / (max - min)).rem_euclid(6.0)
    } else if max == g {
        (b - r) / (max - min) + 2.0
    } else {
        (r - g) / (max - min) + 4.0
    };
    h * 60.0
}

/// Every step below the floor on a background, with its ratio.
fn below(ramp: &[u8], background: (u8, u8, u8)) -> Vec<(u8, f64)> {
    ramp.iter()
        .map(|index| (*index, contrast(rgb(*index), background)))
        .filter(|(_, ratio)| *ratio < FLOOR)
        .collect()
}

fn ramp() -> Vec<u8> {
    usage::RAMP.iter().map(|colour| indexed(*colour)).collect()
}

/// Green to red, each step its own colour: the hue falls step by step.
fn in_order(ramp: &[u8]) {
    let hues: Vec<f64> = ramp.iter().map(|index| hue(rgb(*index))).collect();
    assert!(
        hues.windows(2).all(|pair| pair[0] > pair[1]),
        "the hues do not go from green to red: {hues:?}"
    );
    let mut seen = ramp.to_vec();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), ramp.len(), "two steps are the same colour");
}

#[test]
fn the_ramp_goes_from_green_to_red_in_the_256_colours() {
    in_order(&ramp());
    // The neutral hue of an unknown capacity is none of the steps.
    assert!(!ramp().contains(&indexed(usage::NEUTRAL)));
}

#[test]
fn the_ramp_reads_on_a_dark_terminal() {
    for (name, background) in DARK {
        let low = below(&ramp(), background);
        assert!(low.is_empty(), "on {name}: under {FLOOR}:1 -- {low:?}");
        assert!(contrast(rgb(indexed(usage::NEUTRAL)), background) >= FLOOR);
    }
}

/// Measured 2026-10-05: on white, 34 is 2.94:1, 106 2.57:1, 178 2.10:1, 208
/// 2.41:1; on #fdf6e3, 202 too (2.83:1). The yellow middle of the ramp is
/// the faintest. Kept as a test that fails until the ramp changes, which is
/// another stream's file.
#[test]
#[ignore = "fails today: usage::RAMP's yellow steps are under 3:1 on a light terminal (178 is 2.10:1 on white); the fix is in view/src/trust.rs, filed"]
fn the_ramp_reads_on_a_light_terminal() {
    for (name, background) in LIGHT {
        let low = below(&ramp(), background);
        assert!(low.is_empty(), "on {name}: under {FLOOR}:1 -- {low:?}");
    }
}

/// The ramp proposed for a light terminal holds on both, in order.
#[test]
fn a_ramp_that_reads_on_both_exists_in_the_256_colours() {
    in_order(&READS_ON_BOTH);
    for (name, background) in DARK.into_iter().chain(LIGHT) {
        let low = below(&READS_ON_BOTH, background);
        assert!(low.is_empty(), "on {name}: under {FLOOR}:1 -- {low:?}");
    }
}

/// The ramp drawn as bars on each background, with each step's ratio, for
/// a person to `cat` in their own terminal: `docs/view/gallery/gradient--*`.
#[test]
fn the_ramp_drawn_on_each_background() {
    for (name, background) in DARK.into_iter().chain(LIGHT) {
        for (which, steps) in [("usage", ramp()), ("proposed", READS_ON_BOTH.to_vec())] {
            let width = 6 * 12;
            let mut buffer = Buffer::empty(Rect::new(0, 0, width, 2));
            let (r, g, b) = background;
            let behind = Color::Rgb(r, g, b);
            for (step, index) in steps.iter().enumerate() {
                let x = step as u16 * 12;
                let style = Style::new().fg(Color::Indexed(*index)).bg(behind);
                buffer.set_string(x, 0, "██████████  ", style);
                buffer.set_string(
                    x,
                    1,
                    format!(
                        "{:<12}",
                        format!("{index} {:.2}", contrast(rgb(*index), background))
                    ),
                    style,
                );
            }
            assert_snapshot(&format!("gradient--{which}-on-{name}"), &buffer);
        }
    }
}
