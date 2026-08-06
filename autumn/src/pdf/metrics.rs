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
        // `.`/`,`/`!`/`:`/`;`/`I` used to share the 222 bucket below with
        // the genuinely narrow glyphs, but their real Helvetica width
        // (278, same as a plain space) is wider — the same underestimation
        // risk already fixed for `W`/`G`/etc: an unbroken run of these
        // could be judged narrow enough to fit a line it actually
        // overflows once rendered.
        ' ' | '\u{00A0}' | '.' | ',' | '!' | ':' | ';' | 'I' => 278,
        '|' => 260, // narrower than the 278 punctuation above, wider than the 222 below
        // narrow ASCII punctuation, plus curly single quotes and low-9 quote
        '\'' | 'i' | 'j' | 'l' | '\u{2018}' | '\u{2019}' | '\u{201A}' => 222,
        // wider ASCII punctuation, plus curly double quotes and double low-9 quote
        '(' | ')' | '[' | ']' | '"' | '-' | 'f' | 'r' | 't' | '/' | '\\' | '\u{201C}'
        | '\u{201D}' | '\u{201E}' => 333,
        '0'..='9' | '\u{2013}' /* – en dash */ | '\u{20AC}' /* € euro */ => 556,
        'm' | 'M' => 833,
        // These four used to share the 833 bucket above with m/M, but their
        // real Helvetica AFM widths vary enough to matter: `W` in particular
        // was underestimated (833 vs its actual 944), which could let a run
        // of wide glyphs be judged narrow enough to fit a line it actually
        // overflows — the exact overflow-past-the-boundary failure mode the
        // character-wrap safety net exists to prevent (see `wrap` in
        // `layout.rs`).
        // `\u{0153}` (œ) shares `W`'s real Helvetica width (944) — merged in
        // here rather than given its own arm to avoid a `match_same_arms`
        // clippy lint; see the Æ/Œ comment further below for why œ/æ need
        // non-fallback widths at all.
        'W' | '\u{0153}' => 944,
        '@' => 1015,
        // `\u{00E6}` (æ) shares `%`'s real Helvetica width (889) — same
        // match_same_arms merge as `W`/œ above.
        '%' | '\u{00E6}' => 889,
        // Same underestimation risk as `W` above, for the rest of the
        // uppercase alphabet (plus lowercase `w`, which happens to share
        // the same real width): these letters are genuinely wider than the
        // 667 fallback the remaining (correctly- or safely-estimated)
        // letters share below — e.g. 60 `G`s at 11pt used to estimate
        // ~440pt (fits a 495pt content area) while Helvetica renders them
        // at ~513pt (doesn't).
        // Accented Latin-1 forms of these same wide base letters (Ç is a
        // cedilla'd C, Ñ an N, Ù/Ú/Û/Ü a U, Ð a D-shaped Eth) inherit the
        // same real Helvetica width as their base letter — the blanket
        // Latin-1-uppercase-accented range further below approximates the
        // *whole* C0-DE block at the plain-A-Z width (667), which
        // underestimates these exactly like the unaccented letters above
        // did before that fix. A repro matching the reported one: 60 `Ö`s
        // at 11pt used to estimate ~440pt (fits a 495pt content area) while
        // Helvetica's real 778-unit `O` width renders them at ~513pt
        // (doesn't) — `Ö` falls in the 778 arm just below, not here, but
        // the same underestimation shape applies to `Ç`/`Ñ`/`Ù`-`Ü`/`Ð`.
        'w' | 'C' | 'D' | 'H' | 'N' | 'R' | 'U' | '\u{00C7}' | '\u{00D0}' | '\u{00D1}'
        | '\u{00D9}' | '\u{00DA}' | '\u{00DB}' | '\u{00DC}' => 722,
        // Accented Latin-1 forms of O — see the Ç/Ñ/Ù-Ü/Ð comment above.
        'G' | 'O' | 'Q' | '\u{00D2}' | '\u{00D3}' | '\u{00D4}' | '\u{00D5}' | '\u{00D6}'
        | '\u{00D8}' => 778,
        // `&` used to fall into the generic ASCII fallback (556) below, but
        // its real Helvetica width (667) is wider — same underestimation
        // risk as the letters above: a 70-character run of `&`s at 11pt
        // used to estimate ~428pt (fits a 495pt content area) while
        // Helvetica renders it at ~514pt (doesn't).
        // `Š`/`Ÿ` (U+0160/U+0178) are uppercase WinAnsi/CP1252 letters
        // outside the Latin-1 block entirely (no C0-DE range arm below
        // catches them), so they fell through to the 556 ASCII fallback
        // like `&` used to — their real Helvetica width (667) matches the
        // rest of this arm.
        '&' | 'A'..='Z' | '\u{0160}' | '\u{0178}' => 667,
        // `Ž` (U+017D) is the same shape of gap as `Š`/`Ÿ` just above. A
        // repro matching the reported one: 70 `Š`s at 11pt used to
        // estimate ~428pt (fits a 495pt content area) while Helvetica
        // actually renders them at ~514pt (doesn't).
        //
        // `ß`/`ø` (U+00DF/U+00F8) share the same real width (611) — they'd
        // otherwise fall into the Latin-1-lowercase-accented range further
        // below, which approximates the whole range at 556, underestimating
        // both the same way. A repro matching the reported one: 80 `ß`s at
        // 11pt used to estimate ~489pt (fits a 495pt content area) while
        // Helvetica actually renders them at ~538pt (doesn't).
        '\u{017D}' | '\u{00DF}' | '\u{00F8}' => 611,
        // `Æ`/`Œ` (and their lowercase forms `æ`/`œ`, merged into the `%`/`W`
        // arms above to avoid duplicate match bodies) are representable
        // WinAnsi ligatures, not accented letters — the Latin-1 range below
        // approximates most of that range at the plain-letter width, but
        // Helvetica renders these ligatures markedly wider (1000/944/889 vs
        // the 667/556 the ranges below would give them), same
        // underestimation risk as the other wide-glyph fixes above. A repro
        // matching the reported one: 50 `Æ`s at 11pt used to estimate
        // ~367pt (fits a 495pt content area) while Helvetica actually
        // renders them at ~550pt (doesn't).
        '\u{00C6}' | '\u{0152}' | '\u{2014}' | '\u{2026}' | '\u{2030}' | '\u{2122}' => 1000, // Æ, Œ, — em dash, … ellipsis, ‰, ™
        '\u{00C0}'..='\u{00DE}' if ch != '\u{00D7}' => 667, // Latin-1 uppercase accented (approx like A-Z)
        '\u{00A9}' | '\u{00AE}' => 737,                     // © copyright, ® registered
        '\u{2022}' => 350,                                  // • bullet
        '\u{2020}' | '\u{2021}' => 500,                     // † dagger, ‡ double dagger
        '\u{00B0}' => 400,                                  // ° degree
        // Math/comparison operators used to fall into the generic ASCII
        // fallback (556) below, but Helvetica renders them wider (584) —
        // same underestimation risk as the other wide-glyph fixes above. A
        // repro matching the reported one: 80 `+`s at 11pt used to estimate
        // ~489pt (fits a 495pt content area) while Helvetica actually
        // renders them at ~514pt (doesn't).
        //
        // `×`/`÷` (U+00D7/U+00F7) share the same real width (584) but are
        // excluded from the Latin-1 ranges above/below (they aren't
        // accented letters) — same underestimation gap as the ASCII
        // operators, just reached via a different fallthrough path.
        '+' | '<' | '=' | '>' | '~' | '\u{00D7}' | '\u{00F7}' => 584,
        // `¼`/`½`/`¾` (U+00BC-00BE) are representable WinAnsi fraction
        // glyphs, not ASCII and outside the Latin-1 accented-letter range
        // below — they fell all the way through to the generic 556
        // fallback, but Helvetica renders all three at 834 units. A repro
        // matching the reported one: 60 `¼`s at 11pt used to estimate
        // ~367pt (fits a 495pt content area) while Helvetica actually
        // renders them at ~550pt (doesn't).
        '\u{00BC}' | '\u{00BD}' | '\u{00BE}' => 834,
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
        // seen between Helvetica and Helvetica-Bold AFM widths for most
        // glyphs; deliberately approximate, see module docs. The straight
        // double quote is a real exception, not just approximation noise:
        // Helvetica-Bold's quotedbl (474) is far wider than a flat +40
        // over the regular width (333 + 40 = 373) would give it. A repro
        // matching the reported one: 120 `"`s at 11pt used to estimate
        // ~492pt (fits a 495pt content area) while Helvetica-Bold actually
        // renders them at ~626pt (doesn't).
        if ch == '"' { 474 } else { w.saturating_add(40) }
    } else {
        w
    }
}

/// Estimated width of `text` in points at `font_size_pt`.
// `units` sums per-character 1/1000em widths (u16 each) in a u64 — even a
// single-token, multi-megabyte pasted value (a base64 blob, a log line
// with no whitespace) stays nowhere near u64::MAX, unlike a u32
// accumulator, which a large enough unbroken token could overflow
// (panicking in debug builds, silently wrapping in release — letting an
// oversized token look narrow enough to slip past the character-wrap
// safety net in `layout::wrap`). For any realistic text, the final f32
// cast stays many orders of magnitude below f32's 24-bit mantissa, so it
// can't meaningfully lose precision either.
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
    fn bold_double_quotes_are_not_underestimated_by_the_flat_bold_offset() {
        // Regression: bold widths are normally estimated as regular + 40,
        // but Helvetica-Bold's quotedbl (474) is far wider than that flat
        // offset gives it (333 regular + 40 = 373) — same underestimation
        // risk as the other wide-glyph fixes. A repro matching the
        // reported one: 120 `"`s at 11pt used to estimate ~492pt (fits a
        // 495pt content area) while Helvetica-Bold actually renders them
        // at ~626pt (doesn't).
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
    fn a_multi_million_character_token_does_not_overflow_the_accumulator() {
        // Regression: `text_width_pt` used to sum per-character widths into
        // a `u32`. A single unbroken token large enough (a pasted base64
        // blob, a log line with no whitespace) could overflow it —
        // panicking in a debug build, silently wrapping in release and
        // making an oversized token look narrow enough to slip past the
        // character-wrap safety net. 6 million 'M's (833/1000em each) sums
        // to ~4.998 billion units, safely past u32::MAX (~4.295 billion).
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
    fn wide_glyphs_that_used_to_share_ms_width_are_not_underestimated() {
        // Regression: `W`/`w`/`@`/`%` used to share the same 833/1000em
        // bucket as `M`/`m`, but Helvetica's real AFM widths for them
        // diverge enough to matter — `W` in particular (944, not 833) could
        // let a `W`-heavy token be judged narrow enough to fit a line it
        // actually overflows once rendered. A real repro: 50 `W`s at 11pt
        // used to estimate ~458pt (fits a 495pt content area) while
        // Helvetica actually renders them at ~519pt (doesn't).
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
        // Regression: after the `W`/`w`/`@`/`%` fix, the rest of the
        // uppercase alphabet still shared one flat 667/1000em fallback —
        // but `C`/`D`/`G`/`H`/`N`/`O`/`Q`/`R`/`U` are genuinely wider than
        // that in real Helvetica. A repro matching the reported one: 60
        // `G`s at 11pt used to estimate ~440pt (fits a 495pt content area)
        // while Helvetica actually renders them at ~513pt (doesn't).
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
        // Regression: `.`/`,`/`!`/`:`/`;`/`I` used to share the 222/1000em
        // bucket with genuinely narrow glyphs (`i`/`j`/`l`), but their real
        // Helvetica width (278, same as a plain space) is wider — same
        // underestimation risk as the earlier wide-glyph fixes. A repro
        // matching the reported one: 180 `I`s at 11pt used to estimate
        // ~440pt (fits a 495pt content area) while Helvetica actually
        // renders them at ~550pt (doesn't).
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
        // Regression: `&` used to fall into the generic ASCII fallback
        // (556), but its real Helvetica width (667) is wider — same
        // underestimation risk as the earlier fixes. A repro matching the
        // reported one: 70 `&`s at 11pt used to estimate ~428pt (fits a
        // 495pt content area) while Helvetica actually renders them at
        // ~514pt (doesn't).
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
        // Regression: `Æ`/`æ` used to fall into the generic Latin-1
        // accented-letter ranges (667/556), but Helvetica renders these
        // ligatures markedly wider (1000/889) — same underestimation risk
        // as the earlier wide-glyph fixes. A repro matching the reported
        // one: 50 `Æ`s at 11pt used to estimate ~367pt (fits a 495pt
        // content area) while Helvetica actually renders them at ~550pt
        // (doesn't).
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
        // Regression: `Œ`/`œ` (U+0152/U+0153) aren't in the Latin-1 range at
        // all, so they fell all the way through to the generic 556 fallback,
        // but Helvetica renders them at 1000/944 — same underestimation risk
        // as the Æ/æ fix above. A repro matching the reported one: 50 `Œ`s
        // at 11pt used to estimate ~306pt (fits a 495pt content area) while
        // Helvetica actually renders them at ~550pt (doesn't).
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
        // Regression: `+`/`<`/`=`/`>`/`~` used to fall into the generic
        // ASCII fallback (556), but Helvetica renders them wider (584) —
        // same underestimation risk as the earlier fixes. A repro matching
        // the reported one: 80 `+`s at 11pt used to estimate ~489pt (fits a
        // 495pt content area) while Helvetica actually renders them at
        // ~514pt (doesn't).
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
        // Regression: `×`/`÷` (U+00D7/U+00F7) share the ASCII operators'
        // real width (584) but are excluded from the Latin-1 ranges (not
        // accented letters), so they fell through to the generic 556
        // fallback instead of the operator bucket. A repro matching the
        // reported one: 80 `×`s at 11pt used to estimate ~489pt (fits a
        // 495pt content area) while Helvetica actually renders them at
        // ~514pt (doesn't).
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
    fn accented_letters_inherit_their_base_letters_real_width() {
        // Regression: the blanket Latin-1-uppercase-accented range
        // approximated the *whole* C0-DE block at the plain-A-Z width
        // (667), which safely overestimates most base letters (e.g. À/È/Ì,
        // whose base A/E/I are genuinely 667) but underestimates accented
        // forms of the letters already known to be wider than 667 (Ç/Ñ/
        // Ò-Ö/Ø/Ù-Ü). A repro matching the reported one: 60 `Ö`s at 11pt
        // used to estimate ~440pt (fits a 495pt content area) while
        // Helvetica's real 778-unit `O` width renders them at ~513pt
        // (doesn't).
        assert_eq!(char_width_1000em('\u{00C7}', false), 722); // Ç, like C
        assert_eq!(char_width_1000em('\u{00D1}', false), 722); // Ñ, like N
        assert_eq!(char_width_1000em('\u{00D9}', false), 722); // Ù, like U
        assert_eq!(char_width_1000em('\u{00D6}', false), 778); // Ö, like O
        assert_eq!(char_width_1000em('\u{00D8}', false), 778); // Ø, like O
        // Unaccented-equivalent letters that were already correctly (or
        // safely-over-) estimated stay exactly as before.
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
        // Regression: `Š`/`Ž`/`Ÿ` (U+0160/U+017D/U+0178) are uppercase
        // WinAnsi/CP1252 letters entirely outside the Latin-1 (C0-DE) block,
        // so they fell all the way through to the generic 556 fallback —
        // same underestimation risk as the earlier wide-glyph fixes. A
        // repro matching the reported one: 70 `Š`s at 11pt used to estimate
        // ~428pt (fits a 495pt content area) while Helvetica actually
        // renders them at ~514pt (doesn't).
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
        // Regression: `ß`/`ø` (U+00DF/U+00F8) used to fall into the blanket
        // Latin-1-lowercase-accented range (556), but Helvetica renders
        // both at 611 units — same underestimation risk as the earlier
        // wide-glyph fixes. A repro matching the reported one: 80 `ß`s at
        // 11pt used to estimate ~489pt (fits a 495pt content area) while
        // Helvetica actually renders them at ~538pt (doesn't).
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
    fn fraction_glyphs_are_not_underestimated_as_generic_non_ascii_characters() {
        // Regression: `¼`/`½`/`¾` (U+00BC-00BE) are representable WinAnsi
        // fraction glyphs, but they're outside both the ASCII and Latin-1
        // accented-letter ranges, so they fell all the way through to the
        // generic 556 fallback — same underestimation risk as the earlier
        // wide-glyph fixes. A repro matching the reported one: 60 `¼`s at
        // 11pt used to estimate ~367pt (fits a 495pt content area) while
        // Helvetica actually renders them at ~550pt (doesn't).
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
