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
//! Colours are defined as 24-bit RGB below, and the near-greys here (`MUTED`
//! against `FAINT`) are the whole reason the transcript reads as layered
//! rather than flat -- the 16-colour palette has nothing to say between "grey"
//! and "dark grey". Not every terminal can be trusted with RGB, though: see
//! `supports_truecolor` and `adapt` below, which is where that gets handled --
//! nothing that reads a colour from this module needs to think about it.

use ratatui::style::{Color, Modifier, Style};
use std::sync::OnceLock;
use std::time::Duration;

// ---- truecolor fallback --------------------------------------------------------

/// Whether this terminal can be trusted to render 24-bit RGB (`Color::Rgb`)
/// correctly.
///
/// Narrow on purpose, and biased toward saying yes: this only flags the one
/// terminal ratatui's own docs confirm is broken here -- Apple's Terminal.app,
/// which macOS sets `TERM_PROGRAM=Apple_Terminal` for. Quoting ratatui's
/// `Color::Rgb` docs directly: "macOS Terminal.app do[es] not support this...
/// Crossterm and Termion do not have this capability and the display will be
/// unpredictable." Everything else defaults to truecolor rather than trying
/// to enumerate every terminal that does support it -- wrongly downgrading a
/// terminal that actually has truecolor is a worse failure than leaving an
/// untested one on the (correct, common) default path.
///
/// Cached: this is checked on every cell of every frame, and the answer can't
/// change mid-process.
pub fn supports_truecolor() -> bool {
    static SUPPORTS: OnceLock<bool> = OnceLock::new();
    *SUPPORTS.get_or_init(|| {
        supports_truecolor_given(
            std::env::var("COLORTERM").ok().as_deref(),
            std::env::var("TERM_PROGRAM").ok().as_deref(),
        )
    })
}

/// The actual decision, taking its inputs as plain `Option<&str>` rather than
/// reading the environment directly so it can be tested without mutating real
/// process state.
fn supports_truecolor_given(colorterm: Option<&str>, term_program: Option<&str>) -> bool {
    if matches!(colorterm, Some("truecolor") | Some("24bit")) {
        return true;
    }
    term_program != Some("Apple_Terminal")
}

/// The nearest xterm 256-colour palette index to an RGB triple: the 6×6×6
/// colour cube (indices 16-231, steps of 0/95/135/175/215/255 per channel) or
/// the 24-step greyscale ramp (232-255, 8 to 238 in steps of 10) -- whichever
/// lands closer in RGB space. Both are supported by every terminal that draws
/// 256 colours at all, including the ones that get `Color::Rgb` wrong.
fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    const STEPS: [i32; 6] = [0, 95, 135, 175, 215, 255];

    let cube_index = |c: u8| -> usize {
        STEPS
            .iter()
            .enumerate()
            .min_by_key(|(_, &step)| (step - c as i32).abs())
            .map(|(i, _)| i)
            .expect("STEPS is non-empty")
    };
    let (ri, gi, bi) = (cube_index(r), cube_index(g), cube_index(b));
    let cube_code = 16 + 36 * ri + 6 * gi + bi;
    let cube_rgb = (STEPS[ri], STEPS[gi], STEPS[bi]);

    let gray_level = (r as i32 + g as i32 + b as i32) / 3;
    let gray_step = ((gray_level - 8).max(0) / 10).min(23);
    let gray_value = 8 + gray_step * 10;
    let gray_code = 232 + gray_step;

    let target = (r as i32, g as i32, b as i32);
    let dist = |c: (i32, i32, i32)| {
        (target.0 - c.0).pow(2) + (target.1 - c.1).pow(2) + (target.2 - c.2).pow(2)
    };

    if dist((gray_value, gray_value, gray_value)) < dist(cube_rgb) {
        gray_code as u8
    } else {
        cube_code as u8
    }
}

/// `color` unchanged, unless it's 24-bit RGB and this terminal can't be
/// trusted with that -- in which case its nearest 256-colour equivalent.
/// Anything already `Indexed` or one of the 16 named colours passes through:
/// those render correctly everywhere already.
pub fn adapt(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) if !supports_truecolor() => Color::Indexed(nearest_256(r, g, b)),
        other => other,
    }
}

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

    // ---- truecolor fallback -------------------------------------------------

    /// The one confirmed-bad case: Apple's Terminal.app, with no COLORTERM
    /// override. This is the exact bug report that started this module.
    #[test]
    fn apple_terminal_without_a_colorterm_override_is_not_trusted_with_truecolor() {
        assert!(!supports_truecolor_given(None, Some("Apple_Terminal")));
    }

    /// COLORTERM is a terminal saying so itself -- that outranks the
    /// TERM_PROGRAM guess even for the one program on the deny list, since a
    /// future Terminal.app that gains real truecolor support and starts
    /// setting COLORTERM should not be second-guessed.
    #[test]
    fn an_explicit_colorterm_truecolor_claim_is_trusted_even_for_apple_terminal() {
        assert!(supports_truecolor_given(Some("truecolor"), Some("Apple_Terminal")));
        assert!(supports_truecolor_given(Some("24bit"), Some("Apple_Terminal")));
    }

    /// Everything not on the deny list defaults to truecolor -- the whole
    /// point is not trying to enumerate every terminal that works.
    #[test]
    fn unknown_or_absent_term_program_defaults_to_truecolor() {
        assert!(supports_truecolor_given(None, None));
        assert!(supports_truecolor_given(None, Some("iTerm.app")));
        assert!(supports_truecolor_given(None, Some("vscode")));
        assert!(supports_truecolor_given(None, Some("WarpTerminal")));
    }

    #[test]
    fn pure_primary_colours_map_to_the_expected_cube_corners() {
        // The 6x6x6 cube's corners are exact: 0 and 255 both land on a step
        // with zero error, so these must be exact, not just "close".
        assert_eq!(nearest_256(0, 0, 0), 16); // cube origin
        assert_eq!(nearest_256(255, 0, 0), 16 + 36 * 5); // pure red
        assert_eq!(nearest_256(0, 255, 0), 16 + 6 * 5); // pure green
        assert_eq!(nearest_256(0, 0, 255), 16 + 5); // pure blue
        assert_eq!(nearest_256(255, 255, 255), 16 + 36 * 5 + 6 * 5 + 5); // cube's white corner
    }

    /// A true grey (R == G == B) must resolve through the finer 24-step
    /// greyscale ramp, not the coarse 6-step cube -- that is the entire
    /// reason the ramp exists, and it is what lets MUTED and FAINT stay
    /// visibly different from each other after downgrading.
    #[test]
    fn a_grey_prefers_the_finer_greyscale_ramp_over_the_coarse_cube() {
        let code = nearest_256(128, 128, 128);
        assert!((232..=255).contains(&code), "128,128,128 -> {code}, expected a ramp index");
    }

    /// The actual regression: MUTED (129,140,162) and FAINT (88,98,118) are
    /// deliberately close so the transcript reads as layered, not flat. If
    /// downgrading collapsed them onto the same 256-colour index, that
    /// layering -- the whole reason the module doc calls it out -- would be
    /// lost specifically on the terminals this fallback exists for.
    #[test]
    fn muted_and_faint_remain_visibly_distinct_after_downgrading() {
        let (mr, mg, mb) = (129, 140, 162);
        let (fr, fg, fb) = (88, 98, 118);
        assert_ne!(
            nearest_256(mr, mg, mb),
            nearest_256(fr, fg, fb),
            "MUTED and FAINT collapsed onto the same 256-colour index"
        );
    }

    #[test]
    fn adapt_leaves_non_rgb_colours_alone() {
        // These are unaffected either way, since only Color::Rgb is ever
        // downgraded -- but this locks in that passthrough regardless of
        // which path `adapt` takes on this machine.
        assert_eq!(adapt(Color::Indexed(42)), Color::Indexed(42));
        assert_eq!(adapt(Color::Green), Color::Green);
        assert_eq!(adapt(Color::Reset), Color::Reset);
    }

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
