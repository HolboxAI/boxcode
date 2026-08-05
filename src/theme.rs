//! One place for every colour and glyph the UI draws.
//!
//! Centralised so the look can be retuned without touching layout code, and so
//! "what colour is a tool line?" has exactly one answer instead of a `Color::`
//! literal repeated across a dozen call sites.
//!
//! The palette is violet and sky rather than the warm orange terminal coding
//! assistants tend to use -- the layout borrows the shape of that convention
//! (continuous transcript, rounded prompt, inline spinner) but should not be
//! mistaken for any particular one at a glance.
//!
//! Colours are 24-bit. Every terminal this runs in realistically supports
//! truecolor, and the near-greys here (`MUTED` against `FAINT`) are the whole
//! reason the transcript reads as layered rather than flat -- the 16-colour
//! palette has nothing to say between "grey" and "dark grey".

use ratatui::style::{Color, Modifier, Style};
use std::time::Duration;

// ---- palette ------------------------------------------------------------------

/// Headline colour: the logo, focused borders, the spinner.
pub const ACCENT: Color = Color::Rgb(167, 139, 250);
/// A lighter violet for text that should read as accent but not shout.
pub const ACCENT_SOFT: Color = Color::Rgb(196, 181, 253);
/// The human's own words. Deliberately a different hue from ACCENT so a glance
/// down the transcript separates "what I said" from "what the app said".
pub const USER: Color = Color::Rgb(125, 211, 252);
/// Ordinary prose.
pub const TEXT: Color = Color::Rgb(226, 232, 240);
/// Secondary text: hints, labels, the key bar.
pub const MUTED: Color = Color::Rgb(129, 140, 162);
/// Barely-there text: placeholder, box rules.
pub const FAINT: Color = Color::Rgb(88, 98, 118);
/// Unfocused borders.
pub const BORDER: Color = Color::Rgb(71, 85, 105);
/// A raised block behind the user's own turns, so scrolling back finds "where
/// did I ask that" by shape rather than by reading. Light enough to separate
/// from the terminal background, dark enough that `TEXT` on top of it is still
/// comfortable to read.
pub const SURFACE: Color = Color::Rgb(48, 50, 68);
/// Tool activity -- scaffolding, not conversation, so it sits back.
pub const TOOL: Color = Color::Rgb(148, 163, 184);

pub const SUCCESS: Color = Color::Rgb(110, 231, 183);
pub const WARNING: Color = Color::Rgb(251, 191, 36);
pub const DANGER: Color = Color::Rgb(248, 113, 113);

// ---- glyphs -------------------------------------------------------------------

/// The mark that stands in for the app itself.
pub const LOGO: &str = "◈";
/// Prefix on the line the user typed.
pub const USER_MARK: &str = "❯";
/// Prefix inside the prompt box.
pub const PROMPT_MARK: &str = "❯";
/// Leads a tool line in the transcript.
pub const TOOL_MARK: &str = "·";

/// The mascot on the welcome panel.
///
/// Block-drawing characters rather than an emoji or a Nerd Font glyph: those
/// render as a replacement box, or at double width and shear the art in half,
/// on any terminal without the right font. `█▀▄` are plain Unicode blocks that
/// every monospace font has had for decades.
///
/// Every row must be the same display width or the silhouette leans. There is
/// a test.
pub const MASCOT: [&str; 5] = [
    "▟█▙       ▟█▙",
    "▜███████████▛",
    "██  █████  ██",
    "▜███████████▛",
    "  ▜█▛   ▜█▛  ",
];

/// Braille spinner. Ten frames at ~12/second reads as a smooth rotation rather
/// than a stutter, and unlike a pulsing asterisk it stays one cell wide in
/// every font, so the text after it never shifts left and right.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How long each spinner frame is held.
const FRAME: u128 = 80;

/// The spinner frame for a turn that has been running `elapsed`.
///
/// Derived from elapsed time rather than a counter incremented per draw: the
/// event loop redraws on every input event as well as on its 16ms tick, so a
/// per-frame counter would spin visibly faster while someone was typing.
pub fn spinner(elapsed: Duration) -> &'static str {
    let index = (elapsed.as_millis() / FRAME) as usize % SPINNER.len();
    SPINNER[index]
}

// ---- styles -------------------------------------------------------------------

pub fn accent() -> Style {
    Style::default().fg(ACCENT)
}

pub fn accent_bold() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn text() -> Style {
    Style::default().fg(TEXT)
}

pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

pub fn faint() -> Style {
    Style::default().fg(FAINT)
}

pub fn danger_bold() -> Style {
    Style::default().fg(DANGER).add_modifier(Modifier::BOLD)
}

/// The user's own words, on their raised block.
pub fn user_turn() -> Style {
    Style::default().fg(TEXT).bg(SURFACE)
}

/// A key the user can press, in a hint bar.
pub fn key() -> Style {
    Style::default()
        .fg(ACCENT_SOFT)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spinner_advances_over_time_and_wraps() {
        let first = spinner(Duration::from_millis(0));
        let second = spinner(Duration::from_millis(FRAME as u64));

        assert_ne!(first, second, "the spinner has to actually move");
        // A full cycle returns to the start rather than running off the end.
        assert_eq!(
            spinner(Duration::from_millis(FRAME as u64 * SPINNER.len() as u64)),
            first
        );
    }

    /// A spinner that changes width makes the text after it jitter left and
    /// right, which is worse than no spinner at all.
    #[test]
    fn every_spinner_frame_is_one_column_wide() {
        for frame in SPINNER {
            assert_eq!(frame.chars().count(), 1, "{frame:?} is not a single char");
        }
    }

    /// Rows of different widths make the mascot lean, and centring it computes
    /// a different offset per row so the lean gets worse.
    #[test]
    fn every_mascot_row_is_the_same_width() {
        let first = MASCOT[0].chars().count();
        for row in MASCOT {
            assert_eq!(row.chars().count(), first, "{row:?} is a different width");
        }
    }

    /// Block-drawing characters only. An emoji here is double-width in some
    /// terminals and a replacement box in others, either of which wrecks the
    /// art -- and neither shows up on the developer machine that has the font.
    #[test]
    fn the_mascot_uses_only_block_characters_and_spaces() {
        for row in MASCOT {
            for ch in row.chars() {
                assert!(
                    ch == ' ' || ('\u{2580}'..='\u{259F}').contains(&ch),
                    "{ch:?} in {row:?} is not a block-drawing character"
                );
            }
        }
    }
}
