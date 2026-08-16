//! WCAG 2.x contrast-ratio math.
//!
//! boxcode has no way to look at a page it just wrote -- no image input
//! reaches the model at all (see `db.rs`'s and `tools.rs`'s own doc comments
//! on that gap). "Does this look right" is therefore permanently out of
//! reach for anything that actually requires eyes. Contrast is not one of
//! those things: WCAG defines it as pure arithmetic on two colors, so it is
//! exactly the kind of "looks premium" claim that can be checked mechanically
//! instead of taken on the model's word -- the same principle as verifying a
//! `publish_artifact` result with a real GET instead of trusting the upload
//! response (`artifacts.rs`'s `verify_live`).

/// Parses `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa` into 0-255 sRGB channels.
/// Alpha, if present, is accepted and ignored -- contrast is a function of
/// the two colors as painted, and a caller checking a token pair already
/// knows what is behind it.
pub fn parse_hex(s: &str) -> Result<(u8, u8, u8), String> {
    let s = s.trim().strip_prefix('#').unwrap_or(s.trim());
    let expand = |c: char| -> String { [c, c].iter().collect() };
    let (r, g, b) = match s.len() {
        3 | 4 => {
            let mut chars = s.chars();
            let r = expand(chars.next().ok_or_else(|| format!("'{s}' is not a valid hex color"))?);
            let g = expand(chars.next().ok_or_else(|| format!("'{s}' is not a valid hex color"))?);
            let b = expand(chars.next().ok_or_else(|| format!("'{s}' is not a valid hex color"))?);
            (r, g, b)
        }
        6 | 8 => (s[0..2].to_string(), s[2..4].to_string(), s[4..6].to_string()),
        _ => return Err(format!("'{s}' is not a valid hex color (expected #rgb or #rrggbb)")),
    };
    let parse = |h: &str| u8::from_str_radix(h, 16).map_err(|_| format!("'{s}' is not a valid hex color"));
    Ok((parse(&r)?, parse(&g)?, parse(&b)?))
}

/// WCAG relative luminance (the `L` in the contrast formula), 0.0 (black) to
/// 1.0 (white). Each sRGB channel is linearized before the weighted sum --
/// skipping that step is the single most common hand-rolled-contrast bug,
/// since it looks plausible and is wrong for every color that is not pure
/// black, white, or grey.
fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
    let channel = |c: u8| -> f64 {
        let c = c as f64 / 255.0;
        if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// WCAG contrast ratio between two colors, always >= 1.0 (identical colors)
/// and <= 21.0 (pure black against pure white) -- the lighter color's
/// luminance is always the numerator by construction, so argument order
/// never changes the result.
pub fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let (la, lb) = (relative_luminance(a.0, a.1, a.2), relative_luminance(b.0, b.1, b.2));
    let (lighter, darker) = if la >= lb { (la, lb) } else { (lb, la) };
    (lighter + 0.05) / (darker + 0.05)
}

/// WCAG 2.x AA thresholds. AAA exists too but is deliberately not checked
/// here -- it demands 7:1/4.5:1, a bar most real, tasteful palettes (Stripe's
/// own included) do not clear, so reporting it as a failure would train
/// exactly the wrong lesson.
const AA_NORMAL_TEXT: f64 = 4.5;
const AA_LARGE_TEXT: f64 = 3.0;

#[derive(Debug, Clone, PartialEq)]
pub struct ContrastCheck {
    pub label: String,
    pub ratio: f64,
    pub passes_normal_text: bool,
    pub passes_large_text: bool,
}

/// Checks one foreground/background pair. Only ever fails on a malformed hex
/// string -- there is no "unusable" pair, only a passing or failing one.
pub fn check(label: &str, foreground_hex: &str, background_hex: &str) -> Result<ContrastCheck, String> {
    let fg = parse_hex(foreground_hex).map_err(|e| format!("foreground: {e}"))?;
    let bg = parse_hex(background_hex).map_err(|e| format!("background: {e}"))?;
    let ratio = contrast_ratio(fg, bg);
    Ok(ContrastCheck {
        label: label.to_string(),
        ratio,
        passes_normal_text: ratio >= AA_NORMAL_TEXT,
        passes_large_text: ratio >= AA_LARGE_TEXT,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_three_and_six_digit_hex_the_same() {
        assert_eq!(parse_hex("#fff").unwrap(), (255, 255, 255));
        assert_eq!(parse_hex("#ffffff").unwrap(), (255, 255, 255));
        assert_eq!(parse_hex("fff").unwrap(), (255, 255, 255), "leading # is optional");
        assert_eq!(parse_hex("#a7b").unwrap(), (0xaa, 0x77, 0xbb));
    }

    #[test]
    fn eight_digit_hex_ignores_alpha() {
        assert_eq!(parse_hex("#ffffff80").unwrap(), (255, 255, 255));
    }

    #[test]
    fn malformed_hex_is_refused_with_a_readable_message() {
        let error = parse_hex("not-a-color").expect_err("should refuse");
        assert!(error.contains("not-a-color"), "{error}");
    }

    /// The one contrast value the WCAG spec itself states exactly: pure
    /// black against pure white is 21:1, the maximum the formula can ever
    /// produce. If the linearization step is wrong, this is usually the
    /// first value to drift.
    #[test]
    fn black_on_white_is_exactly_21_to_1() {
        let ratio = contrast_ratio((0, 0, 0), (255, 255, 255));
        assert!((ratio - 21.0).abs() < 0.01, "{ratio}");
    }

    #[test]
    fn identical_colors_are_1_to_1() {
        assert!((contrast_ratio((100, 100, 100), (100, 100, 100)) - 1.0).abs() < 0.001);
    }

    #[test]
    fn argument_order_does_not_change_the_result() {
        let a = contrast_ratio((20, 20, 20), (240, 240, 240));
        let b = contrast_ratio((240, 240, 240), (20, 20, 20));
        assert!((a - b).abs() < 0.001, "{a} vs {b}");
    }

    /// A real, deliberately borderline pair -- muted grey text on white --
    /// confirmed against a reference WCAG contrast calculator rather than
    /// hand-derived, so this catches a formula bug the round-number cases
    /// above could both pass despite being subtly wrong.
    #[test]
    fn a_known_borderline_pair_matches_a_reference_calculator() {
        let ratio = contrast_ratio(parse_hex("#767676").unwrap(), (255, 255, 255));
        assert!((ratio - 4.54).abs() < 0.02, "{ratio}");
    }

    #[test]
    fn check_reports_both_thresholds_independently() {
        // 3.5:1 -- clears large-text (3:1) but not normal-text (4.5:1).
        let result = check("body text", "#949494", "#ffffff").unwrap();
        assert!(!result.passes_normal_text, "{result:?}");
        assert!(result.passes_large_text, "{result:?}");

        let result = check("body text", "#000000", "#ffffff").unwrap();
        assert!(result.passes_normal_text, "{result:?}");
        assert!(result.passes_large_text, "{result:?}");
    }

    #[test]
    fn a_malformed_pair_names_which_side_was_wrong() {
        let error = check("test", "#fff", "not-a-color").expect_err("should refuse");
        assert!(error.starts_with("background:"), "{error}");
    }
}
