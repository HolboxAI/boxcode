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

/// Which way round the terminal is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Light text on a dark background.
    Dark,
    /// Dark text on a light background.
    Light,
    /// Nobody said and the terminal would not tell us. Accents are picked to
    /// clear the contrast bar on *both* backgrounds, and body text defers to
    /// the terminal's own foreground, so a wrong guess is impossible rather
    /// than merely unlikely.
    Unknown,
}

/// Every colour the UI draws, resolved for one background.
///
/// A struct rather than the `const`s this used to be, because the right answer
/// depends on something only known at runtime. The old constants were tuned
/// for a dark terminal and the app never painted a background, so on a light
/// one the prose colour sat at 1.23:1 against white -- invisible.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub accent: Color,
    pub accent_soft: Color,
    pub user: Color,
    /// Body text, and the prompt you type into.
    ///
    /// `Color::Reset` in every palette, deliberately. This is the one colour a
    /// wrong guess about the background makes *invisible* rather than merely
    /// dull, and it is the bulk of what is on screen. Deferring to the
    /// terminal's own foreground is correct on any background by definition,
    /// so no amount of failed detection can hide what you are typing.
    pub text: Color,
    pub muted: Color,
    pub faint: Color,
    pub border: Color,
    /// Behind the user's own turns.
    pub surface: Color,
    /// On top of `surface`. Set explicitly and never left to the terminal:
    /// the block paints its own background, so its text must contrast with
    /// *that*, not with whatever is behind the app.
    pub on_surface: Color,
    pub tool: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
}

/// For a dark terminal.
const DARK: Palette = Palette {
    // `accent` never carries body text -- it is the logo, the spinner and the
    // focused border -- so it is free to be as vivid as the background allows.
    accent: Color::Rgb(167, 139, 250),
    accent_soft: Color::Rgb(148, 108, 240),
    user: Color::Rgb(43, 146, 199),
    text: Color::Reset,
    muted: Color::Rgb(138, 148, 170),
    faint: Color::Rgb(110, 120, 140),
    border: Color::Rgb(88, 102, 123),
    surface: Color::Rgb(48, 50, 68),
    on_surface: Color::Rgb(232, 236, 244),
    tool: Color::Rgb(128, 142, 164),
    success: Color::Rgb(20, 150, 110),
    warning: Color::Rgb(191, 124, 10),
    danger: Color::Rgb(224, 72, 72),
};

/// The same hues taken darker and more saturated, for a light terminal. Kept
/// recognisably the same violet-and-sky identity rather than swapped for a
/// different scheme -- this is the same app, on a different background.
const LIGHT: Palette = Palette {
    accent: Color::Rgb(91, 33, 182),
    accent_soft: Color::Rgb(136, 92, 225),
    user: Color::Rgb(3, 105, 161),
    text: Color::Reset,
    muted: Color::Rgb(92, 106, 128),
    faint: Color::Rgb(100, 116, 139),
    border: Color::Rgb(126, 142, 166),
    surface: Color::Rgb(237, 233, 254),
    on_surface: Color::Rgb(30, 27, 75),
    tool: Color::Rgb(100, 120, 140),
    success: Color::Rgb(30, 125, 100),
    warning: Color::Rgb(170, 100, 25),
    danger: Color::Rgb(185, 28, 28),
};

/// Used when the background is genuinely unknown.
///
/// Mid-tone accents, chosen to clear the contrast bar against black *and*
/// white -- so they are never invisible, at the cost of being less vivid than
/// either tuned palette. `text`, `muted` and `faint` defer to the terminal's
/// own foreground via `Color::Reset`, which is correct on any background by
/// definition. `surface` still paints, so it carries its own `on_surface`.
const NEUTRAL: Palette = Palette {
    accent: Color::Rgb(124, 92, 230),
    accent_soft: Color::Rgb(136, 92, 225),
    user: Color::Rgb(43, 146, 199),
    text: Color::Reset,
    muted: Color::Rgb(124, 134, 156),
    faint: Color::Rgb(128, 128, 128),
    border: Color::Rgb(128, 128, 128),
    surface: Color::Rgb(88, 76, 140),
    on_surface: Color::Rgb(255, 255, 255),
    tool: Color::Rgb(120, 134, 156),
    success: Color::Rgb(13, 140, 100),
    warning: Color::Rgb(176, 112, 8),
    danger: Color::Rgb(211, 60, 60),
};

static PALETTE: OnceLock<Palette> = OnceLock::new();

/// Fix the palette for the process. Called once from `main` with whatever the
/// config and the terminal between them worked out; later calls do nothing,
/// which keeps `palette()` free of locking on the hot path.
pub fn init(mode: Mode) {
    let _ = PALETTE.set(match mode {
        Mode::Dark => DARK,
        Mode::Light => LIGHT,
        Mode::Unknown => NEUTRAL,
    });
}

/// The palette in force. Defaults to `NEUTRAL` rather than `DARK` when `init`
/// was never called: a test or an embedder that forgets should get the variant
/// that is readable everywhere, not the one that is invisible on half of
/// terminals.
pub fn palette() -> &'static Palette {
    PALETTE.get_or_init(|| NEUTRAL)
}

/// Shorthand -- this is read on essentially every span the UI builds.
pub fn p() -> &'static Palette {
    palette()
}

/// What the environment says about the background, if anything.
///
/// `COLORFGBG` is the only widely-implemented environment signal: rxvt,
/// Konsole and a few others set it to something like `15;0` -- foreground
/// then background, as palette indices. Index 0-6 and 8 are the dark half of
/// the 16-colour palette, so a background there means a dark terminal.
///
/// Most terminals do not set it at all (VS Code, iTerm2 and Apple Terminal all
/// do not), which is why this returns `None` rather than guessing, and why the
/// config option is the reliable route rather than a nicety.
pub fn mode_from_env() -> Option<Mode> {
    mode_from_colorfgbg(std::env::var("COLORFGBG").ok().as_deref())
}

fn mode_from_colorfgbg(value: Option<&str>) -> Option<Mode> {
    let raw = value?;
    // The last field is the background; some terminals emit `fg;bg` and some
    // `fg;<something>;bg`.
    let background = raw.rsplit(';').next()?.trim();
    let index: u8 = background.parse().ok()?;
    Some(match index {
        0..=6 | 8 => Mode::Dark,
        _ => Mode::Light,
    })
}


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
    Style::default().fg(p().accent)
}

pub fn accent_bold() -> Style {
    Style::default().fg(p().accent).add_modifier(Modifier::BOLD)
}

pub fn text() -> Style {
    Style::default().fg(p().text)
}

pub fn muted() -> Style {
    Style::default().fg(p().muted)
}

pub fn faint() -> Style {
    Style::default().fg(p().faint)
}

pub fn danger_bold() -> Style {
    Style::default().fg(p().danger).add_modifier(Modifier::BOLD)
}

/// The user's own words, on their raised block.
pub fn user_turn() -> Style {
    Style::default().fg(p().on_surface).bg(p().surface)
}

/// A key the user can press, in a hint bar.
pub fn key() -> Style {
    Style::default()
        .fg(p().accent_soft)
        .add_modifier(Modifier::BOLD)
}

/// Work out which palette to use: what the user configured, then what the
/// environment states, then `Unknown` -- which is legible either way.
///
/// This deliberately does **not** ask the terminal. An earlier version sent an
/// OSC 11 query and waited for the reply, which hung the app at startup until
/// a key was pressed: `crossterm::event::poll` reports that bytes are
/// available, but crossterm has already drained them into its own buffer, so
/// the blocking `read` that followed waited for bytes that were never coming.
/// The deadline could not save it, because the wait was inside `read` rather
/// than between iterations.
///
/// Doing it correctly means a non-blocking read on the raw file descriptor,
/// never mixed with crossterm's own reader -- and it would still race with
/// that reader once the app is running. It is not worth it: since every
/// palette's body text defers to the terminal's foreground and every other
/// colour is checked against both backgrounds, the only thing detection buys
/// is more vivid accents. `[ui] theme` buys the same thing, reliably, with no
/// startup cost and no possibility of hanging.
pub fn resolve_mode(configured: &str) -> Mode {
    match configured {
        "dark" => Mode::Dark,
        "light" => Mode::Light,
        _ => mode_from_env().unwrap_or(Mode::Unknown),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    // ---- contrast -----------------------------------------------------------

    /// WCAG relative luminance.
    fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
        let channel = |v: u8| {
            let v = v as f64 / 255.0;
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    fn rgb(color: Color) -> Option<(u8, u8, u8)> {
        match color {
            Color::Rgb(r, g, b) => Some((r, g, b)),
            // `Reset` is the terminal's own foreground, which contrasts with
            // its own background by construction -- there is nothing to check.
            _ => None,
        }
    }

    /// Foregrounds that carry actual words, and the bar they must clear.
    fn readable(p: &Palette) -> Vec<(&'static str, Color)> {
        vec![
            ("text", p.text),
            ("muted", p.muted),
            ("faint", p.faint),
            ("user", p.user),
            ("accent_soft", p.accent_soft),
            ("tool", p.tool),
            ("success", p.success),
            ("warning", p.warning),
            ("danger", p.danger),
        ]
    }

    /// The bug this whole module was rewritten for: every colour was tuned for
    /// a dark terminal, the app never painted a background, and on a light one
    /// the prose sat at 1.23:1 against white. Each palette must clear the WCAG
    /// bar against the background it is actually for.
    #[test]
    fn each_palette_is_readable_on_the_background_it_is_for() {
        const WHITE: (u8, u8, u8) = (255, 255, 255);
        const BLACK: (u8, u8, u8) = (0, 0, 0);

        for (name, palette, background) in [
            ("DARK", &DARK, BLACK),
            ("LIGHT", &LIGHT, WHITE),
        ] {
            for (label, color) in readable(palette) {
                let Some(fg) = rgb(color) else { continue };
                let ratio = contrast(fg, background);
                assert!(
                    ratio >= 4.5,
                    "{name}.{label} is {ratio:.2}:1 against its own background, needs 4.5:1"
                );
            }
            // Borders and rules are decoration, not text: 3:1 is the bar.
            for (label, color) in [("border", palette.border), ("accent", palette.accent)] {
                let Some(fg) = rgb(color) else { continue };
                let ratio = contrast(fg, background);
                assert!(ratio >= 3.0, "{name}.{label} is {ratio:.2}:1, needs 3:1");
            }
        }
    }

    /// The one that matters most in practice: detection can be wrong.
    ///
    /// `COLORFGBG` is unset on most terminals and some ignore OSC 11, so
    /// `auto` can land on the wrong palette. When it does, the result has to be
    /// *less vivid*, never unreadable. Every colour that carries words is
    /// therefore checked against **both** backgrounds, in every palette --
    /// which is the property that was missing when a user reported their typed
    /// prompt was still white on a light terminal.
    ///
    /// `accent` is exempt: it draws the logo, the spinner and the focused
    /// border, never body text, so it is free to be vivid for its own
    /// background. It is covered by the 3:1 rule in the test above.
    #[test]
    fn no_palette_can_be_unreadable_on_either_background_even_if_guessed_wrong() {
        const WHITE: (u8, u8, u8) = (255, 255, 255);
        const BLACK: (u8, u8, u8) = (0, 0, 0);

        for (name, palette) in [("DARK", &DARK), ("LIGHT", &LIGHT), ("NEUTRAL", &NEUTRAL)] {
            for (label, color) in readable(palette) {
                let Some(fg) = rgb(color) else { continue };
                for (side, background) in [("white", WHITE), ("black", BLACK)] {
                    let ratio = contrast(fg, background);
                    assert!(
                        ratio >= 3.0,
                        "{name}.{label} is {ratio:.2}:1 on {side}; a wrong guess must cost \
                         vividness, not legibility"
                    );
                }
            }
        }
    }

    /// Body text is the bulk of the screen and the thing a wrong guess hides
    /// completely, so it never picks a colour at all -- it takes the
    /// terminal's own foreground, which cannot clash with the terminal's own
    /// background.
    #[test]
    fn body_text_always_defers_to_the_terminal_foreground() {
        for (name, palette) in [("DARK", &DARK), ("LIGHT", &LIGHT), ("NEUTRAL", &NEUTRAL)] {
            assert_eq!(
                palette.text,
                Color::Reset,
                "{name}.text must be Reset, or a wrong guess makes typing invisible"
            );
        }
    }

    /// The user-turn block paints its own background, so its text contrasts
    /// with *that* -- never with the terminal. Leaving it on `Reset` was how
    /// the block ended up as dark-on-dark on a light terminal.
    #[test]
    fn text_on_the_user_turn_block_contrasts_with_the_block() {
        for (name, palette) in [("DARK", &DARK), ("LIGHT", &LIGHT), ("NEUTRAL", &NEUTRAL)] {
            let (Some(fg), Some(bg)) = (rgb(palette.on_surface), rgb(palette.surface)) else {
                panic!("{name}: the block and its text must both be explicit colours");
            };
            let ratio = contrast(fg, bg);
            assert!(ratio >= 4.5, "{name}: on_surface is {ratio:.2}:1 on surface");
        }
    }

    // ---- background detection -----------------------------------------------

    #[test]
    fn colorfgbg_is_read_as_foreground_then_background() {
        assert_eq!(mode_from_colorfgbg(Some("15;0")), Some(Mode::Dark));
        assert_eq!(mode_from_colorfgbg(Some("0;15")), Some(Mode::Light));
        // Some terminals emit a third field; the background is still last.
        assert_eq!(mode_from_colorfgbg(Some("15;default;0")), Some(Mode::Dark));
        // Absent or unparseable is "no opinion", never a guess.
        assert_eq!(mode_from_colorfgbg(None), None);
        assert_eq!(mode_from_colorfgbg(Some("")), None);
        assert_eq!(mode_from_colorfgbg(Some("nonsense")), None);
    }




    /// Regression: the app hung at startup, indefinitely, until a key was
    /// pressed. `resolve_mode` asked the terminal for its background colour
    /// and then blocked reading the reply, so every launch stalled on a
    /// terminal that had nothing to say. Startup must not wait on anything.
    #[test]
    fn resolving_the_palette_never_waits_on_the_terminal() {
        for configured in ["auto", "dark", "light", "nonsense"] {
            let started = std::time::Instant::now();
            let _ = resolve_mode(configured);
            let took = started.elapsed();
            assert!(
                took < std::time::Duration::from_millis(20),
                "resolve_mode({configured:?}) took {took:?}; it must not block on I/O"
            );
        }
    }

    /// An explicit setting is the one thing that must never be second-guessed:
    /// it is the only reliable route on the terminals that answer nothing.
    #[test]
    fn an_explicit_theme_setting_wins_without_asking_anything() {
        assert_eq!(resolve_mode("dark"), Mode::Dark);
        assert_eq!(resolve_mode("light"), Mode::Light);
    }

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
