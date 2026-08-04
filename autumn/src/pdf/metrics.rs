//! Approximate glyph-width table for word-wrapping base-14 text.
//!
//! `printpdf`'s core (non-`html`-feature) API does not expose the Adobe AFM
//! width tables for the built-in fonts, so exact per-glyph widths aren't
//! available without embedding a font. Pixel-perfect wrapping is explicitly
//! out of scope for this renderer (see [`crate::pdf`] module docs) — instead
//! this uses a small, hand-classified width table (in 1/1000 em units, the
//! same unit PDF glyph widths are expressed in) that is close enough for
//! reasonable word-wrap decisions without pretending to be exact font
//! metrics.

/// Approximate width of `ch` in 1/1000 em units for a Helvetica-family font.
///
/// `bold` widens a handful of categories slightly, matching the general shape
/// of `Helvetica-Bold` vs `Helvetica` AFM metrics (bold glyphs run a little
/// wider) without claiming per-glyph accuracy.
const fn base_width_1000em(ch: char) -> u16 {
    match ch {
        ' ' | '\u{00A0}' => 278, // space, non-breaking space
        // narrow ASCII punctuation, plus curly single quotes and low-9 quote
        '.' | ',' | '\'' | '!' | ':' | ';' | '|' | 'i' | 'j' | 'l' | 'I' | '\u{2018}'
        | '\u{2019}' | '\u{201A}' => 222,
        // wider ASCII punctuation, plus curly double quotes and double low-9 quote
        '(' | ')' | '[' | ']' | '"' | '-' | 'f' | 'r' | 't' | '/' | '\\' | '\u{201C}'
        | '\u{201D}' | '\u{201E}' => 333,
        '0'..='9' | '\u{2013}' /* – en dash */ | '\u{20AC}' /* € euro */ => 556,
        'm' | 'M' | 'w' | 'W' | '@' | '%' => 833,
        'A'..='Z' => 667,
        '\u{00C0}'..='\u{00DE}' if ch != '\u{00D7}' => 667, // Latin-1 uppercase accented (approx like A-Z)
        '\u{2014}' | '\u{2026}' | '\u{2030}' | '\u{2122}' => 1000, // — em dash, … ellipsis, ‰, ™
        '\u{00A9}' | '\u{00AE}' => 737,                     // © copyright, ® registered
        '\u{2022}' => 350,                                  // • bullet
        '\u{2020}' | '\u{2021}' => 500,                     // † dagger, ‡ double dagger
        '\u{00B0}' => 400,                                  // ° degree
        _ if ch.is_ascii() => 556,
        '\u{00DF}'..='\u{00FF}' if ch != '\u{00F7}' => 556, // Latin-1 lowercase accented (approx default)
        // Everything else that WinAnsi (`printpdf`'s built-in-font encoding,
        // effectively CP1252) *can* represent renders as its actual glyph,
        // not a placeholder — approximate it at the same "ordinary letter"
        // width used for plain ASCII above rather than assuming it's
        // narrow. Characters WinAnsi truly can't represent (CJK, emoji,
        // ...) are replaced with a literal `?` glyph by `printpdf`, whose
        // own Helvetica width is also 556 (it isn't in the narrow-glyph
        // list above), so the same estimate is correct for both cases —
        // unlike the previous 222 estimate, which matched neither.
        _ => 556,
    }
}

/// Estimated width of `ch` in 1/1000 em units, `bold` widening it slightly.
pub(super) const fn char_width_1000em(ch: char, bold: bool) -> u16 {
    let w = base_width_1000em(ch);
    if bold {
        // +40/1000em (~7% at typical text sizes) mirrors the modest widening
        // seen between Helvetica and Helvetica-Bold AFM widths; deliberately
        // approximate, see module docs.
        w.saturating_add(40)
    } else {
        w
    }
}

/// Estimated width of `text` in points at `font_size_pt`.
// `units` sums per-character 1/1000em widths (u16 each) — even a
// pathologically long line stays many orders of magnitude below f32's
// 24-bit mantissa, so the cast below can't meaningfully lose precision.
#[allow(clippy::cast_precision_loss)]
pub(super) fn text_width_pt(text: &str, font_size_pt: f32, bold: bool) -> f32 {
    let units: u32 = text
        .chars()
        .map(|c| u32::from(char_width_1000em(c, bold)))
        .sum();
    (units as f32 / 1000.0) * font_size_pt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_glyphs_are_narrower_than_wide_glyphs() {
        assert!(char_width_1000em('i', false) < char_width_1000em('M', false));
    }

    #[test]
    fn bold_is_never_narrower_than_regular() {
        for ch in ['a', 'M', ' ', '.', '维'] {
            assert!(char_width_1000em(ch, true) >= char_width_1000em(ch, false));
        }
    }

    #[test]
    fn text_width_scales_with_font_size() {
        let small = text_width_pt("Total", 10.0, false);
        let large = text_width_pt("Total", 20.0, false);
        assert!((large - small * 2.0).abs() < 0.01);
    }

    #[test]
    fn empty_text_has_zero_width() {
        // `0u32 as f32 * anything` is exact under IEEE 754 — no rounding
        // error possible, so a direct equality check is safe here.
        #[allow(clippy::float_cmp)]
        let is_zero = text_width_pt("", 12.0, false) == 0.0;
        assert!(is_zero);
    }

    #[test]
    fn em_dash_is_not_estimated_as_narrow_punctuation() {
        // Regression: an em dash (what `&mdash;` decodes to — see
        // `html::decode_one_entity`) is representable in WinAnsi and
        // printpdf renders it as a real, wide glyph, not `?`. The generic
        // non-ASCII fallback used to estimate it at the same width as a
        // narrow character like `.` or `,`, underestimating a real em
        // dash's width by roughly 4-5x and letting em-dash-heavy text run
        // past a line/column boundary that a correct estimate would have
        // wrapped before.
        let narrow = char_width_1000em('.', false);
        let em_dash = char_width_1000em('\u{2014}', false);
        assert!(
            em_dash > narrow * 3,
            "em dash ({em_dash}) must be estimated much wider than narrow punctuation ({narrow})"
        );
    }

    #[test]
    fn non_win_ansi_character_gets_the_same_width_as_a_literal_question_mark() {
        // `printpdf` substitutes `?` for any character WinAnsi can't
        // represent (CJK, emoji, ...) — the estimate for such a character
        // should match a real `?`'s own width, not an arbitrary narrower
        // guess (the previous 222/1000em estimate matched neither).
        assert_eq!(
            char_width_1000em('维', false),
            char_width_1000em('?', false)
        );
    }
}
