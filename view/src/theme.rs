//! Terminal colours. Tokyo Night follows the btop theme; no background is set.
//! Set OMN_TUI_THEME=accessible before launch for the high-contrast palette
//! designed to work with both light and dark terminal backgrounds.
use ratatui::style::{Color, Modifier, Style};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    TokyoNight,
    Accessible,
}

pub fn current() -> Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    *THEME.get_or_init(|| {
        if std::env::var("OMN_TUI_THEME").ok().as_deref() == Some("accessible") {
            Theme::Accessible
        } else {
            Theme::TokyoNight
        }
    })
}

// The accessible colours are also retained as public constants for contrast tests.
pub const ACCENT: Color = Color::Indexed(30);
pub const BORDER: Color = Color::Indexed(66);
pub const TITLE: Color = Color::Indexed(97);
pub const TOKYO_ACCENT: Color = Color::Rgb(125, 207, 255); // #7dcfff
pub const TOKYO_BORDER: Color = Color::Rgb(86, 95, 137); // #565f89
pub const TOKYO_TITLE: Color = Color::Rgb(207, 201, 194); // #cfc9c2
pub fn choose(tokyo: Color, accessible: Color) -> Color {
    match current() {
        Theme::TokyoNight => tokyo,
        Theme::Accessible => accessible,
    }
}
pub fn accent() -> Color {
    choose(TOKYO_ACCENT, ACCENT)
}
pub fn border() -> Style {
    Style::new().fg(choose(TOKYO_BORDER, BORDER))
}
pub fn title() -> Style {
    Style::new()
        .fg(choose(TOKYO_TITLE, TITLE))
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::{palette, usage};

    #[test]
    fn selected_theme_drives_chrome_badges_and_graphs() {
        if std::env::var("OMN_TUI_THEME").ok().as_deref() == Some("accessible") {
            assert_eq!(current(), Theme::Accessible);
            assert_eq!(accent(), ACCENT);
            assert_eq!(palette::loss(), palette::LOSS);
            assert_eq!(usage::colour(0.0), usage::RAMP[0]);
        } else {
            assert_eq!(current(), Theme::TokyoNight);
            assert_eq!(accent(), TOKYO_ACCENT);
            assert_eq!(palette::loss(), Color::Rgb(224, 175, 104));
            assert_eq!(usage::colour(0.0), usage::TOKYO_RAMP[0]);
        }
    }
}
