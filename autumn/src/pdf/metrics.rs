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
        // Helvetica AFM widths, grouped into buckets. Keep each bucket at or above its members'
        // real widths: an underestimate lets an unbroken run be judged to fit a line it
        // overflows, past the character-wrap safety net in `layout::wrap`.
        ' ' | '\u{00A0}' | '.' | ',' | '!' | ':' | ';' | 'I' => 278,
        '|' => 260, // narrower than the 278 punctuation above, wider than the 222 below
        // narrow ASCII punctuation, plus curly single quotes and low-9 quote
        '\'' | 'i' | 'j' | 'l' | '\u{2018}' | '\u{2019}' | '\u{201A}' => 222,
        // wider ASCII punctuation, plus curly double quotes and double low-9 quote
        '(' | ')' | '[' | ']' | '-' | 'f' | 'r' | 't' | '/' | '\\' | '\u{201C}' | '\u{201D}'
        | '\u{201E}' => 333,
        // ASCII quotedbl is wider than the curly double quotes.
        '"' => 355,
        '0'..='9' | '\u{2013}' /* – en dash */ | '\u{20AC}' /* € euro */
        | '\u{2020}' /* † dagger */ | '\u{2021}' /* ‡ double dagger */ => 556,
        'm' | 'M' => 833,
        // œ shares this arm and æ shares `%`'s: separate arms with equal widths would trip
        // clippy's `match_same_arms`.
        'W' | '\u{0153}' => 944,
        '@' => 1015,
        '%' | '\u{00E6}' => 889,
        // Letters wider than the 667 A-Z default, with their accented Latin-1 forms (Ç, Ð, Ñ,
        // Ù-Ü), which the Latin-1 range below would put at 667.
        'w' | 'C' | 'D' | 'H' | 'N' | 'R' | 'U' | '\u{00C7}' | '\u{00D0}' | '\u{00D1}'
        | '\u{00D9}' | '\u{00DA}' | '\u{00DB}' | '\u{00DC}' => 722,
        // O-shaped letters and their accented Latin-1 forms.
        'G' | 'O' | 'Q' | '\u{00D2}' | '\u{00D3}' | '\u{00D4}' | '\u{00D5}' | '\u{00D6}'
        | '\u{00D8}' => 778,
        // Š and Ÿ are CP1252 letters outside the Latin-1 block; no range below covers them.
        '&' | 'A'..='Z' | '\u{0160}' | '\u{0178}' => 667,
        // Ž, ß, ø, ¿ and þ. ß, ø and þ also fall in the Latin-1 lowercase range below (556);
        // this arm wins because match arms are tried in order.
        '\u{017D}' | '\u{00DF}' | '\u{00F8}' | '\u{00BF}' | '\u{00FE}' => 611,
        // The Æ/Œ ligatures (æ/œ are merged into the arms above) are much wider than the 667 or
        // 556 the arms below would give them.
        '\u{00C6}' | '\u{0152}' | '\u{2014}' | '\u{2026}' | '\u{2030}' | '\u{2122}' => 1000, // Æ, Œ, — em dash, … ellipsis, ‰, ™
        '\u{00C0}'..='\u{00DE}' if ch != '\u{00D7}' => 667, // Latin-1 uppercase accented (approx like A-Z)
        '\u{00A9}' | '\u{00AE}' => 737,                     // © copyright, ® registered
        '\u{2022}' => 350,                                  // • bullet
        '\u{00B0}' => 400,                                  // ° degree
        // Math operators, plus the Latin-1 signs ×, ÷, ± and ¬.
        '+' | '<' | '=' | '>' | '~' | '\u{00D7}' | '\u{00F7}' | '\u{00B1}' | '\u{00AC}' => 584,
        // ¼, ½, ¾
        '\u{00BC}' | '\u{00BD}' | '\u{00BE}' => 834,
        _ if ch.is_ascii() => 556,
        '\u{00DF}'..='\u{00FF}' if ch != '\u{00F7}' => 556, // Latin-1 lowercase accented (approx default)
        // Anything else WinAnsi (`printpdf`'s built-in-font encoding, effectively CP1252) can
        // represent renders as its real glyph, so estimate it at the ordinary letter width.
        // `printpdf` replaces a character WinAnsi cannot represent (CJK, emoji) with `?`, which
        // is also 556 wide.
        _ => 556,
    }
}

/// Estimated width of `ch` in 1/1000 em units, `bold` widening it slightly.
pub(super) const fn char_width_1000em(ch: char, bold: bool) -> u16 {
    let w = base_width_1000em(ch);
    if bold {
        // +56 approximates Helvetica-Bold's widening; it is the smallest offset that covers
        // bold `i` (222 regular, 278 bold). The double quotes widen more than that: bold
        // quotedbl is 474 and the bold curly double quotes are 500.
        if ch == '"' {
            474
        } else if ch == '\u{201C}' || ch == '\u{201D}' || ch == '\u{201E}' {
            500
        } else {
            w.saturating_add(56)
        }
    } else {
        w
    }
}

/// Estimated width of `text` in points at `font_size_pt`.
// `units` is a u64: a multi-megabyte unbroken token (a base64 blob) could overflow a u32,
// wrapping in release and looking narrow enough to slip past the character-wrap safety net.
// The f32 cast keeps enough relative precision for a width estimate.
#[allow(clippy::cast_precision_loss)]
pub(super) fn text_width_pt(text: &str, font_size_pt: f32, bold: bool) -> f32 {
    let units: u64 = text
        .chars()
        .map(|c| u64::from(char_width_1000em(c, bold)))
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
    fn bold_lowercase_i_is_not_underestimated_by_the_flat_bold_offset() {
        assert_eq!(char_width_1000em('i', true), 278);
        let hundred_seventy_one_is = text_width_pt(&"i".repeat(171), 11.0, true);
        let content_area_pt = 495.0;
        assert!(
            hundred_seventy_one_is > content_area_pt,
            "171 is at 11pt bold ({hundred_seventy_one_is}pt) must be estimated wider than a \
             495pt content area, matching Helvetica-Bold's real ~523pt rendering, not the old \
             ~493pt underestimate"
        );
    }

    #[test]
    fn regular_double_quotes_are_not_underestimated() {
        assert_eq!(char_width_1000em('"', false), 355);
        let hundred_thirty_five_quotes = text_width_pt(&"\"".repeat(135), 11.0, false);
        let content_area_pt = 495.28;
        assert!(
            hundred_thirty_five_quotes > content_area_pt,
            "135 \"s at 11pt ({hundred_thirty_five_quotes}pt) must be estimated wider than a \
             495.28pt content area, matching Helvetica's real ~527pt rendering, not the old \
             ~494.5pt underestimate"
        );
    }

    #[test]
    fn bold_double_quotes_are_not_underestimated_by_the_flat_bold_offset() {
        // Bold widths are normally regular + 56, which would give quotedbl only 389.
        assert_eq!(char_width_1000em('"', true), 474);
        let hundred_twenty_quotes = text_width_pt(&"\"".repeat(120), 11.0, true);
        let content_area_pt = 495.0;
        assert!(
            hundred_twenty_quotes > content_area_pt,
            "120 \"s at 11pt bold ({hundred_twenty_quotes}pt) must be estimated wider than a \
             495pt content area, matching Helvetica-Bold's real ~626pt rendering, not the old \
             ~492pt underestimate"
        );
    }

    #[test]
    fn bold_curly_double_quotes_are_not_underestimated_by_the_flat_bold_offset() {
        assert_eq!(char_width_1000em('\u{201C}', true), 500);
        assert_eq!(char_width_1000em('\u{201D}', true), 500);
        assert_eq!(char_width_1000em('\u{201E}', true), 500);
        for ch in ['\u{201C}', '\u{201E}'] {
            let hundred_fifteen_quotes = text_width_pt(&ch.to_string().repeat(115), 11.0, true);
            let content_area_pt = 495.0;
            assert!(
                hundred_fifteen_quotes > content_area_pt,
                "115 {ch:?}s at 11pt bold ({hundred_fifteen_quotes}pt) must be estimated wider \
                 than a 495pt content area, matching Helvetica-Bold's real ~633pt rendering, \
                 not the old ~492pt underestimate"
            );
        }
    }

    #[test]
    fn text_width_scales_with_font_size() {
        let small = text_width_pt("Total", 10.0, false);
        let large = text_width_pt("Total", 20.0, false);
        assert!(small.mul_add(-2.0, large).abs() < 0.01);
    }

    #[test]
    fn empty_text_has_zero_width() {
        // Zero times anything is exactly zero, so exact comparison is safe.
        #[allow(clippy::float_cmp)]
        let is_zero = text_width_pt("", 12.0, false) == 0.0;
        assert!(is_zero);
    }

    #[test]
    fn a_multi_million_character_token_does_not_overflow_the_accumulator() {
        // 6 million 'M's (833 each) sum to ~5.0 billion units, past u32::MAX (~4.3 billion).
        let text = "M".repeat(6_000_000);
        let width = text_width_pt(&text, 12.0, false);
        let expected = 6_000_000.0 * 833.0 / 1000.0 * 12.0;
        assert!(
            width.is_finite() && (width - expected).abs() < 1.0,
            "expected ~{expected}, got {width}"
        );
    }

    #[test]
    fn em_dash_is_not_estimated_as_narrow_punctuation() {
        // An em dash (what `&mdash;` decodes to) is a real WinAnsi glyph, not `?`.
        let narrow = char_width_1000em('.', false);
        let em_dash = char_width_1000em('\u{2014}', false);
        assert!(
            em_dash > narrow * 3,
            "em dash ({em_dash}) must be estimated much wider than narrow punctuation ({narrow})"
        );
    }

    #[test]
    fn wide_glyphs_that_used_to_share_ms_width_are_not_underestimated() {
        assert_eq!(char_width_1000em('W', false), 944);
        assert_eq!(char_width_1000em('@', false), 1015);
        assert_eq!(char_width_1000em('%', false), 889);
        let fifty_ws = text_width_pt(&"W".repeat(50), 11.0, false);
        let content_area_pt = 495.0;
        assert!(
            fifty_ws > content_area_pt,
            "50 Ws at 11pt ({fifty_ws}pt) must be estimated wider than a 495pt content area, \
             matching Helvetica's real ~519pt rendering, not the old ~458pt underestimate"
        );
    }

    #[test]
    fn remaining_wide_uppercase_letters_are_not_underestimated() {
        assert_eq!(char_width_1000em('G', false), 778);
        assert_eq!(char_width_1000em('O', false), 778);
        assert_eq!(char_width_1000em('Q', false), 778);
        assert_eq!(char_width_1000em('C', false), 722);
        assert_eq!(char_width_1000em('D', false), 722);
        assert_eq!(char_width_1000em('H', false), 722);
        assert_eq!(char_width_1000em('N', false), 722);
        assert_eq!(char_width_1000em('R', false), 722);
        assert_eq!(char_width_1000em('U', false), 722);
        let sixty_gs = text_width_pt(&"G".repeat(60), 11.0, false);
        let content_area_pt = 495.0;
        assert!(
            sixty_gs > content_area_pt,
            "60 Gs at 11pt ({sixty_gs}pt) must be estimated wider than a 495pt content area, \
             matching Helvetica's real ~513pt rendering, not the old ~440pt underestimate"
        );
    }

    #[test]
    fn remaining_narrow_glyphs_are_not_underestimated() {
        assert_eq!(char_width_1000em('I', false), 278);
        assert_eq!(char_width_1000em('!', false), 278);
        assert_eq!(char_width_1000em(':', false), 278);
        assert_eq!(char_width_1000em(';', false), 278);
        assert_eq!(char_width_1000em('.', false), 278);
        assert_eq!(char_width_1000em(',', false), 278);
        // `i`/`j`/`l` are genuinely narrow and must stay that way.
        assert_eq!(char_width_1000em('i', false), 222);
        let hundred_eighty_is = text_width_pt(&"I".repeat(180), 11.0, false);
        let content_area_pt = 495.0;
        assert!(
            hundred_eighty_is > content_area_pt,
            "180 Is at 11pt ({hundred_eighty_is}pt) must be estimated wider than a 495pt \
             content area, matching Helvetica's real ~550pt rendering, not the old ~440pt \
             underestimate"
        );
    }

    #[test]
    fn ampersand_is_not_underestimated_as_a_generic_ascii_character() {
        assert_eq!(char_width_1000em('&', false), 667);
        let seventy_ampersands = text_width_pt(&"&".repeat(70), 11.0, false);
        let content_area_pt = 495.0;
        assert!(
            seventy_ampersands > content_area_pt,
            "70 &s at 11pt ({seventy_ampersands}pt) must be estimated wider than a 495pt \
             content area, matching Helvetica's real ~514pt rendering, not the old ~428pt \
             underestimate"
        );
    }

    #[test]
    fn ae_ligatures_are_not_underestimated_as_ordinary_accented_letters() {
        assert_eq!(char_width_1000em('\u{00C6}', false), 1000);
        assert_eq!(char_width_1000em('\u{00E6}', false), 889);
        let fifty_aes = text_width_pt(&"\u{00C6}".repeat(50), 11.0, false);
        let content_area_pt = 495.0;
        assert!(
            fifty_aes > content_area_pt,
            "50 Æs at 11pt ({fifty_aes}pt) must be estimated wider than a 495pt content area, \
             matching Helvetica's real ~550pt rendering, not the old ~367pt underestimate"
        );
    }

    #[test]
    fn oe_ligatures_are_not_underestimated_as_the_generic_fallback() {
        assert_eq!(char_width_1000em('\u{0152}', false), 1000);
        assert_eq!(char_width_1000em('\u{0153}', false), 944);
        let fifty_oes = text_width_pt(&"\u{0152}".repeat(50), 11.0, false);
        let content_area_pt = 495.0;
        assert!(
            fifty_oes > content_area_pt,
            "50 Œs at 11pt ({fifty_oes}pt) must be estimated wider than a 495pt content area, \
             matching Helvetica's real ~550pt rendering, not the old ~306pt underestimate"
        );
    }

    #[test]
    fn math_operators_are_not_underestimated_as_generic_ascii_characters() {
        for op in ['+', '<', '=', '>', '~'] {
            assert_eq!(char_width_1000em(op, false), 584, "operator {op:?}");
        }
        let eighty_pluses = text_width_pt(&"+".repeat(80), 11.0, false);
        let content_area_pt = 495.0;
        assert!(
            eighty_pluses > content_area_pt,
            "80 +s at 11pt ({eighty_pluses}pt) must be estimated wider than a 495pt content \
             area, matching Helvetica's real ~514pt rendering, not the old ~489pt underestimate"
        );
    }

    #[test]
    fn multiplication_and_division_signs_are_not_underestimated() {
        assert_eq!(char_width_1000em('\u{00D7}', false), 584); // ×
        assert_eq!(char_width_1000em('\u{00F7}', false), 584); // ÷
        let eighty_times = text_width_pt(&"\u{00D7}".repeat(80), 11.0, false);
        let content_area_pt = 495.0;
        assert!(
            eighty_times > content_area_pt,
            "80 ×s at 11pt ({eighty_times}pt) must be estimated wider than a 495pt content \
             area, matching Helvetica's real ~514pt rendering, not the old ~489pt underestimate"
        );
    }

    #[test]
    fn remaining_wide_latin1_symbols_are_not_underestimated() {
        assert_eq!(char_width_1000em('\u{00B1}', false), 584); // ±
        assert_eq!(char_width_1000em('\u{00AC}', false), 584); // ¬
        assert_eq!(char_width_1000em('\u{00BF}', false), 611); // ¿
        let eighty_plusminus = text_width_pt(&"\u{00B1}".repeat(80), 11.0, false);
        let content_area_pt = 495.0;
        assert!(
            eighty_plusminus > content_area_pt,
            "80 ±s at 11pt ({eighty_plusminus}pt) must be estimated wider than a 495pt content \
             area, matching Helvetica's real ~514pt rendering, not the old ~489pt underestimate"
        );
    }

    #[test]
    fn dagger_glyphs_are_not_underestimated() {
        assert_eq!(char_width_1000em('\u{2020}', false), 556); // †
        assert_eq!(char_width_1000em('\u{2021}', false), 556); // ‡
        let ninety_daggers = text_width_pt(&"\u{2020}".repeat(90), 11.0, false);
        let content_area_pt = 495.28;
        assert!(
            ninety_daggers > content_area_pt,
            "90 †s at 11pt ({ninety_daggers}pt) must be estimated wider than a 495.28pt \
             content area, matching Helvetica's real ~550pt rendering, not the old ~495pt \
             underestimate"
        );
    }

    #[test]
    fn accented_letters_inherit_their_base_letters_real_width() {
        // The Latin-1 uppercase range puts accented letters at 667; these take their wider
        // base letter's width instead.
        assert_eq!(char_width_1000em('\u{00C7}', false), 722); // Ç, like C
        assert_eq!(char_width_1000em('\u{00D1}', false), 722); // Ñ, like N
        assert_eq!(char_width_1000em('\u{00D9}', false), 722); // Ù, like U
        assert_eq!(char_width_1000em('\u{00D6}', false), 778); // Ö, like O
        assert_eq!(char_width_1000em('\u{00D8}', false), 778); // Ø, like O
        // Accented letters whose base letter is 667 stay in the range.
        assert_eq!(char_width_1000em('\u{00C0}', false), 667); // À, like A
        let sixty_odiaereses = text_width_pt(&"\u{00D6}".repeat(60), 11.0, false);
        let content_area_pt = 495.0;
        assert!(
            sixty_odiaereses > content_area_pt,
            "60 Ös at 11pt ({sixty_odiaereses}pt) must be estimated wider than a 495pt content \
             area, matching Helvetica's real ~513pt rendering, not the old ~440pt underestimate"
        );
    }

    #[test]
    fn remaining_cp1252_uppercase_letters_are_not_underestimated() {
        // Š, Ž and Ÿ are CP1252 letters outside the Latin-1 block.
        assert_eq!(char_width_1000em('\u{0160}', false), 667); // Š
        assert_eq!(char_width_1000em('\u{0178}', false), 667); // Ÿ
        assert_eq!(char_width_1000em('\u{017D}', false), 611); // Ž
        let seventy_scarons = text_width_pt(&"\u{0160}".repeat(70), 11.0, false);
        let content_area_pt = 495.0;
        assert!(
            seventy_scarons > content_area_pt,
            "70 Šs at 11pt ({seventy_scarons}pt) must be estimated wider than a 495pt content \
             area, matching Helvetica's real ~514pt rendering, not the old ~428pt underestimate"
        );
    }

    #[test]
    fn sharp_s_and_o_slash_are_not_underestimated() {
        assert_eq!(char_width_1000em('\u{00DF}', false), 611); // ß
        assert_eq!(char_width_1000em('\u{00F8}', false), 611); // ø
        let eighty_sharp_s = text_width_pt(&"\u{00DF}".repeat(80), 11.0, false);
        let content_area_pt = 495.0;
        assert!(
            eighty_sharp_s > content_area_pt,
            "80 ßs at 11pt ({eighty_sharp_s}pt) must be estimated wider than a 495pt content \
             area, matching Helvetica's real ~538pt rendering, not the old ~489pt underestimate"
        );
    }

    #[test]
    fn lowercase_thorn_is_not_underestimated() {
        assert_eq!(char_width_1000em('\u{00FE}', false), 611); // þ
        let eighty_thorns = text_width_pt(&"\u{00FE}".repeat(80), 11.0, false);
        let content_area_pt = 495.0;
        assert!(
            eighty_thorns > content_area_pt,
            "80 þs at 11pt ({eighty_thorns}pt) must be estimated wider than a 495pt content \
             area, matching Helvetica's real ~538pt rendering, not the old ~489pt underestimate"
        );
    }

    #[test]
    fn fraction_glyphs_are_not_underestimated_as_generic_non_ascii_characters() {
        assert_eq!(char_width_1000em('\u{00BC}', false), 834); // ¼
        assert_eq!(char_width_1000em('\u{00BD}', false), 834); // ½
        assert_eq!(char_width_1000em('\u{00BE}', false), 834); // ¾
        let sixty_quarters = text_width_pt(&"\u{00BC}".repeat(60), 11.0, false);
        let content_area_pt = 495.0;
        assert!(
            sixty_quarters > content_area_pt,
            "60 ¼s at 11pt ({sixty_quarters}pt) must be estimated wider than a 495pt content \
             area, matching Helvetica's real ~550pt rendering, not the old ~367pt underestimate"
        );
    }

    #[test]
    fn non_win_ansi_character_gets_the_same_width_as_a_literal_question_mark() {
        // `printpdf` substitutes `?` for any character WinAnsi cannot represent.
        assert_eq!(
            char_width_1000em('维', false),
            char_width_1000em('?', false)
        );
    }
}
