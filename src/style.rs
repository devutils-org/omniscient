//! Colour, and when not to use it.
//!
//! Colour is an accelerator, never the carrier of a fact. Everything it
//! highlights is also written in words, because the output of this tool ends up
//! in pipes, in files and in issue reports, and a fact that only exists as an
//! escape sequence is a fact that is lost there.
//!
//! The default follows what the terminal says and what the environment asks:
//! no colour when stdout is not a terminal, when `NO_COLOR` is set, or when
//! `TERM` is `dumb`. `--color always` forces it back on for the common case of
//! piping into a pager that understands escapes.

use std::io::IsTerminal;

/// Whether the output may carry escape sequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    colour: bool,
}

impl Style {
    /// Colour if stdout is a terminal that has not asked to be left alone.
    pub fn for_stdout() -> Self {
        Self::decide(std::io::stdout().is_terminal(), Self::asked_not_to())
    }

    /// Whether the environment has asked for no colour, by either convention.
    fn asked_not_to() -> bool {
        std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
            || std::env::var("TERM").is_ok_and(|term| term == "dumb")
    }

    /// The decision itself, with nothing read from the process.
    ///
    /// Separated because it is the part worth testing and the only part that
    /// can be: a test cannot ask what `auto` does about a terminal by looking at
    /// its own, since whether the suite was launched from one is not the
    /// subject. `cargo test` captures the print macros, not file descriptor
    /// one, so under a terminal stdout *is* a terminal inside a test -- which is
    /// how the obvious version of this test passed whenever the suite's output
    /// was piped and failed whenever a person ran it.
    fn decide(is_terminal: bool, asked_not_to: bool) -> Self {
        Self {
            colour: is_terminal && !asked_not_to,
        }
    }

    pub fn always() -> Self {
        Self { colour: true }
    }

    pub fn never() -> Self {
        Self { colour: false }
    }

    /// What this build cannot measure yet.
    pub fn absent(&self, text: &str) -> String {
        self.paint("31", text)
    }

    /// What it can.
    pub fn present(&self, text: &str) -> String {
        self.paint("32", text)
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.colour {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four cases of the decision, none of which depends on how the test
    /// binary was launched.
    #[test]
    fn colour_needs_a_terminal_that_has_not_asked_to_be_left_alone() {
        assert!(
            Style::decide(true, false).colour,
            "a plain terminal gets colour"
        );
        assert!(
            !Style::decide(true, true).colour,
            "NO_COLOR is honoured on a terminal"
        );
        assert!(
            !Style::decide(false, false).colour,
            "a pipe never gets colour"
        );
        assert!(!Style::decide(false, true).colour);
    }

    #[test]
    fn a_style_without_colour_returns_the_text_untouched() {
        assert_eq!(Style::never().absent("no"), "no");
        assert_eq!(Style::never().present("yes"), "yes");
    }

    /// `--color` is parsed the way every other option of this tool is parsed:
    /// the spelling that comes back out is the spelling that goes in, and an
    /// unknown value says what the choices are instead of falling back to one.
    #[test]
    fn a_colour_choice_round_trips_through_its_own_spelling() {
        for choice in ColorChoice::ALL {
            assert_eq!(choice.to_string().parse(), Ok(choice));
        }
    }

    #[test]
    fn an_unknown_colour_choice_names_the_ones_that_exist() {
        let error = "sometimes".parse::<ColorChoice>().unwrap_err().to_string();
        assert!(error.contains("auto, always or never"), "{error}");
    }

    /// `never` is not the same as "no terminal": it must hold even where a
    /// terminal is present, because it is the reader's own instruction.
    #[test]
    fn never_and_always_do_not_consult_the_terminal() {
        assert_eq!(ColorChoice::Never.style(), Style::never());
        assert_eq!(ColorChoice::Always.style(), Style::always());
        assert_ne!(Style::always(), Style::never());
    }

    #[test]
    fn a_painted_string_still_contains_its_text() {
        let style = Style::always();
        let painted = style.absent("io.latency");
        assert!(painted.contains("io.latency"));
        assert!(painted.starts_with("\x1b["), "{painted}");
        assert!(painted.ends_with("\x1b[0m"), "{painted}");
    }
}

/// When the caller wants colour, as `--color` names it.
///
/// `auto` is the default and asks the terminal. `always` exists for the common
/// case of piping into a pager that understands escapes, and `never` for a
/// terminal that does but whose reader does not want it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    const ALL: [ColorChoice; 3] = [ColorChoice::Auto, ColorChoice::Always, ColorChoice::Never];

    pub fn as_str(self) -> &'static str {
        match self {
            ColorChoice::Auto => "auto",
            ColorChoice::Always => "always",
            ColorChoice::Never => "never",
        }
    }

    /// The style this choice asks for, resolving `auto` against the terminal.
    pub fn style(self) -> Style {
        match self {
            ColorChoice::Auto => Style::for_stdout(),
            ColorChoice::Always => Style::always(),
            ColorChoice::Never => Style::never(),
        }
    }
}

impl std::fmt::Display for ColorChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ColorChoice {
    type Err = crate::ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let name = s.trim().to_ascii_lowercase();
        ColorChoice::ALL
            .into_iter()
            .find(|choice| choice.as_str() == name)
            .ok_or_else(|| {
                crate::ParseError::new(format!(
                    "unknown colour choice `{s}` (expected auto, always or never)"
                ))
            })
    }
}
