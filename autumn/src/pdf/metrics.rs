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
        ' ' => 278,
        '.' | ',' | '\'' | '!' | ':' | ';' | '|' | 'i' | 'j' | 'l' | 'I' => 222,
        '(' | ')' | '[' | ']' | '"' | '-' | 'f' | 'r' | 't' | '/' | '\\' => 333,
        '0'..='9' => 556,
        'm' | 'M' | 'w' | 'W' | '@' | '%' => 833,
        'A'..='Z' => 667,
        _ if ch.is_ascii() => 556,
        // Non-ASCII glyphs aren't in the base-14 WinAnsi encoding; printpdf
        // renders them as `?` (222/1000 em under the classification above),
        // so estimate accordingly rather than assuming a full-width glyph.
        _ => 222,
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
}
