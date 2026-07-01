//! View-layer value formatting helpers for Maud templates.
//!
//! Autumn's widget lane (`crate::widgets`) renders *containers* — tables,
//! cards, property lists — but stops short of formatting the scalar values
//! that go inside them. This module fills that gap with small, pure,
//! allocation-light helpers that return [`maud::Markup`] so their output is
//! HTML-escaped by construction and safe to interpolate directly into
//! `html! { ... }` blocks.
//!
//! # Quick example
//!
//! ```rust
//! use autumn_web::format::{number_to_currency, pluralize, time_ago_in_words};
//! use autumn_web::time::FixedClock;
//! use chrono::{TimeZone, Utc};
//! use maud::html;
//! use rust_decimal::Decimal;
//!
//! let price: Decimal = "1234.5".parse().unwrap();
//! let posted_at = Utc.with_ymd_and_hms(2026, 1, 1, 11, 58, 0).unwrap();
//! let clock = FixedClock::at(Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap());
//!
//! let markup = html! {
//!     (number_to_currency(price))
//!     " — "
//!     (pluralize(3, "comment"))
//!     " — "
//!     (time_ago_in_words(posted_at, &clock))
//! };
//!
//! assert_eq!(markup.into_string(), "$1,234.50 — 3 comments — 2 minutes ago");
//! ```

use crate::time::ClockSource;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

// ── Currency ───────────────────────────────────────────────────────────────

/// Configuration for [`CurrencyOptions::format`] — symbol, precision, and
/// thousands/decimal separators.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurrencyOptions<'a> {
    symbol: &'a str,
    precision: u32,
    thousands_separator: char,
    decimal_separator: char,
}

#[cfg(feature = "maud")]
impl<'a> CurrencyOptions<'a> {
    /// Create a new currency configuration with sane defaults: `$` symbol,
    /// 2 decimal places, `,` thousands separator, `.` decimal separator.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            symbol: "$",
            precision: 2,
            thousands_separator: ',',
            decimal_separator: '.',
        }
    }

    /// Set the currency symbol prefix (default `"$"`).
    #[must_use]
    pub const fn symbol(mut self, symbol: &'a str) -> Self {
        self.symbol = symbol;
        self
    }

    /// Set the number of decimal places to round and display (default `2`).
    #[must_use]
    pub const fn precision(mut self, precision: u32) -> Self {
        self.precision = precision;
        self
    }

    /// Set the character grouping integer digits in threes (default `,`).
    #[must_use]
    pub const fn thousands_separator(mut self, separator: char) -> Self {
        self.thousands_separator = separator;
        self
    }

    /// Set the character separating the integer and fractional parts (default `.`).
    #[must_use]
    pub const fn decimal_separator(mut self, separator: char) -> Self {
        self.decimal_separator = separator;
        self
    }

    /// Format `value` per this configuration, returning HTML-escaped [`maud::Markup`].
    #[must_use]
    pub fn format(&self, value: Decimal) -> maud::Markup {
        let text = format_currency(value, self);
        maud::html! { (text) }
    }
}

#[cfg(feature = "maud")]
impl Default for CurrencyOptions<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Format `value` as currency using [`CurrencyOptions`] defaults (`$1,234.50`).
#[cfg(feature = "maud")]
#[must_use]
pub fn number_to_currency(value: Decimal) -> maud::Markup {
    CurrencyOptions::new().format(value)
}

#[cfg(feature = "maud")]
fn format_currency(value: Decimal, options: &CurrencyOptions<'_>) -> String {
    // `round_dp` rounds but does not pad trailing zeros onto a shorter scale
    // (e.g. 1234.5 stays scale=1), so the digit string is built by rounding
    // first, then formatting with an explicit precision to force zero-padding.
    let rounded = value.round_dp_with_strategy(
        options.precision,
        rust_decimal::RoundingStrategy::MidpointAwayFromZero,
    );
    let negative = rounded.is_sign_negative() && !rounded.is_zero();
    let abs = rounded.abs();
    let digits = format!("{:.prec$}", abs, prec = options.precision as usize);
    let (int_part, frac_part) = split_decimal_string(&digits);
    let grouped_int = group_thousands(int_part, options.thousands_separator);

    let mut out =
        String::with_capacity(options.symbol.len() + grouped_int.len() + frac_part.len() + 2);
    if negative {
        out.push('-');
    }
    out.push_str(options.symbol);
    out.push_str(&grouped_int);
    if options.precision > 0 {
        out.push(options.decimal_separator);
        out.push_str(frac_part);
    }
    out
}

// ── Delimited numbers ────────────────────────────────────────────────────────

/// Render an integer or decimal with grouped thousands (`1,234,567`).
///
/// Accepts any type that converts into [`rust_decimal::Decimal`] (the common
/// Rust integer types, plus `Decimal` itself).
#[cfg(feature = "maud")]
#[must_use]
pub fn number_with_delimiter<T: Into<Decimal>>(value: T) -> maud::Markup {
    let value: Decimal = value.into();
    let negative = value.is_sign_negative() && !value.is_zero();
    let abs = value.abs();
    let digits = abs.to_string();
    let (int_part, frac_part) = split_decimal_string(&digits);
    let grouped_int = group_thousands(int_part, ',');

    let mut out = String::with_capacity(grouped_int.len() + frac_part.len() + 2);
    if negative {
        out.push('-');
    }
    out.push_str(&grouped_int);
    if !frac_part.is_empty() {
        out.push('.');
        out.push_str(frac_part);
    }
    maud::html! { (out) }
}

/// Split a plain decimal string (`"1234.50"` or `"1234"`) into its integer
/// and fractional parts (without the separating dot).
fn split_decimal_string(s: &str) -> (&str, &str) {
    s.split_once('.').unwrap_or((s, ""))
}

/// Group the ASCII digits of `int_digits` into threes, joined by `separator`.
fn group_thousands(int_digits: &str, separator: char) -> String {
    let len = int_digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in int_digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(separator);
        }
        out.push(ch);
    }
    out
}

// ── Pluralize ────────────────────────────────────────────────────────────────

/// Render `"{count} {word}"`, pluralizing `singular` with a simple
/// irregular-aware English rule when `count != 1`.
///
/// Use [`pluralize_with`] to supply an explicit plural for irregulars this
/// rule misses.
#[cfg(feature = "maud")]
#[must_use]
pub fn pluralize(count: i64, singular: &str) -> maud::Markup {
    let word = if count == 1 {
        singular.to_owned()
    } else {
        pluralize_word(singular)
    };
    maud::html! { (count) " " (word) }
}

/// Render `"{count} {word}"`, choosing between `singular` and an explicit
/// `plural` — the escape hatch for irregulars [`pluralize`]'s built-in rule
/// misses.
#[cfg(feature = "maud")]
#[must_use]
pub fn pluralize_with(count: i64, singular: &str, plural: &str) -> maud::Markup {
    let word = if count == 1 { singular } else { plural };
    maud::html! { (count) " " (word) }
}

/// Naive English pluraliser for display words. Mirrors the identifier
/// pluraliser in `autumn-cli`'s naming helpers, minus the `snake_case`
/// segment-splitting (display words are whole words, not identifiers).
fn pluralize_word(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    match word {
        "person" => return "people".to_owned(),
        "child" => return "children".to_owned(),
        "man" => return "men".to_owned(),
        "woman" => return "women".to_owned(),
        "mouse" => return "mice".to_owned(),
        "goose" => return "geese".to_owned(),
        _ => {}
    }
    let lower = word.to_ascii_lowercase();
    if lower.ends_with("ss")
        || lower.ends_with('x')
        || lower.ends_with('z')
        || lower.ends_with("ch")
        || lower.ends_with("sh")
    {
        return format!("{word}es");
    }
    if lower.ends_with('y') {
        let prev = word.chars().rev().nth(1);
        if prev.is_some_and(|c| !"aeiouAEIOU".contains(c)) {
            let mut out: String = word.chars().take(word.chars().count() - 1).collect();
            out.push_str("ies");
            return out;
        }
    }
    format!("{word}s")
}

// ── Truncation ────────────────────────────────────────────────────────────────

/// Shorten `text` to at most `len` characters (including the ellipsis),
/// never splitting a UTF-8 character mid-byte.
#[cfg(feature = "maud")]
#[must_use]
pub fn truncate(text: &str, len: usize) -> maud::Markup {
    truncate_with(text, len, "…")
}

/// Like [`truncate`], with a caller-supplied omission marker instead of `"…"`.
#[cfg(feature = "maud")]
#[must_use]
pub fn truncate_with(text: &str, len: usize, omission: &str) -> maud::Markup {
    let char_count = text.chars().count();
    if char_count <= len {
        return maud::html! { (text) };
    }
    let omission_len = omission.chars().count();
    let keep = len.saturating_sub(omission_len);
    let truncated: String = text.chars().take(keep).collect();
    maud::html! { (truncated) (omission) }
}

/// Shorten `text` to at most `n` whitespace-delimited words.
#[cfg(feature = "maud")]
#[must_use]
pub fn truncate_words(text: &str, n: usize) -> maud::Markup {
    truncate_words_with(text, n, "…")
}

/// Like [`truncate_words`], with a caller-supplied omission marker instead of `"…"`.
#[cfg(feature = "maud")]
#[must_use]
pub fn truncate_words_with(text: &str, n: usize, omission: &str) -> maud::Markup {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() <= n {
        return maud::html! { (text) };
    }
    let truncated = words[..n].join(" ");
    maud::html! { (truncated) (omission) }
}

// ── Dates & times ────────────────────────────────────────────────────────────

/// Render a human-readable relative time (`"3 minutes ago"`, `"in 2 days"`)
/// between `dt` and the current instant of `clock`.
///
/// Pass a [`crate::time::FixedClock`] or [`crate::time::TickingClock`] in
/// tests for deterministic output.
#[cfg(feature = "maud")]
#[must_use]
pub fn time_ago_in_words(dt: DateTime<Utc>, clock: &dyn ClockSource) -> maud::Markup {
    let text = relative_time_words(dt, clock.now());
    maud::html! { (text) }
}

fn relative_time_words(dt: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = now.signed_duration_since(dt).num_seconds();
    if secs < 0 {
        format!("in {}", duration_words(secs.unsigned_abs()))
    } else {
        format!("{} ago", duration_words(secs.unsigned_abs()))
    }
}

/// Render a whole-unit duration as `"{n} {second,minute,hour,day}(s)"`.
fn duration_words(secs: u64) -> String {
    let (n, singular, plural) = if secs < 60 {
        (secs, "second", "seconds")
    } else if secs < 3600 {
        (secs / 60, "minute", "minutes")
    } else if secs < 86_400 {
        (secs / 3600, "hour", "hours")
    } else {
        (secs / 86_400, "day", "days")
    };
    let word = if n == 1 { singular } else { plural };
    format!("{n} {word}")
}

/// Render `dt` (UTC) using a `chrono` strftime-style absolute format string.
///
/// ```rust
/// use autumn_web::format::format_datetime;
/// use chrono::{TimeZone, Utc};
///
/// let dt = Utc.with_ymd_and_hms(2026, 6, 7, 14, 32, 1).unwrap();
/// assert_eq!(format_datetime(dt, "%Y-%m-%d").into_string(), "2026-06-07");
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn format_datetime(dt: DateTime<Utc>, fmt: &str) -> maud::Markup {
    let text = dt.format(fmt).to_string();
    maud::html! { (text) }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "maud"))]
mod tests {
    use super::*;
    use crate::time::FixedClock;
    use chrono::TimeZone;

    fn dec(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    // ── number_to_currency / CurrencyOptions ────────────────────────────

    #[test]
    fn currency_formats_with_default_options() {
        assert_eq!(number_to_currency(dec("1234.5")).into_string(), "$1,234.50");
    }

    #[test]
    fn currency_formats_zero() {
        assert_eq!(number_to_currency(dec("0")).into_string(), "$0.00");
    }

    #[test]
    fn currency_formats_negative() {
        assert_eq!(number_to_currency(dec("-42.5")).into_string(), "-$42.50");
    }

    #[test]
    fn currency_groups_large_values() {
        assert_eq!(
            number_to_currency(dec("1234567.89")).into_string(),
            "$1,234,567.89"
        );
    }

    #[test]
    fn currency_rounds_to_precision() {
        assert_eq!(number_to_currency(dec("1.005")).into_string(), "$1.01");
    }

    #[test]
    fn currency_custom_options() {
        let opts = CurrencyOptions::new()
            .symbol("€")
            .precision(2)
            .thousands_separator('.')
            .decimal_separator(',');
        assert_eq!(opts.format(dec("1234.5")).into_string(), "€1.234,50");
    }

    #[test]
    fn currency_zero_precision_omits_decimal_separator() {
        let opts = CurrencyOptions::new().precision(0);
        assert_eq!(opts.format(dec("42.6")).into_string(), "$43");
    }

    #[test]
    fn currency_does_not_render_negative_zero() {
        assert_eq!(number_to_currency(dec("-0.001")).into_string(), "$0.00");
    }

    // ── number_with_delimiter ────────────────────────────────────────────

    #[test]
    fn delimiter_groups_large_integer() {
        assert_eq!(
            number_with_delimiter(1_234_567_i64).into_string(),
            "1,234,567"
        );
    }

    #[test]
    fn delimiter_no_grouping_needed() {
        assert_eq!(number_with_delimiter(42_i64).into_string(), "42");
    }

    #[test]
    fn delimiter_handles_negative() {
        assert_eq!(number_with_delimiter(-1_234_i64).into_string(), "-1,234");
    }

    #[test]
    fn delimiter_handles_decimal() {
        assert_eq!(
            number_with_delimiter(dec("1234567.89")).into_string(),
            "1,234,567.89"
        );
    }

    #[test]
    fn delimiter_handles_zero() {
        assert_eq!(number_with_delimiter(0_i64).into_string(), "0");
    }

    // ── pluralize / pluralize_with ───────────────────────────────────────

    #[test]
    fn pluralize_singular_boundary() {
        assert_eq!(pluralize(1, "comment").into_string(), "1 comment");
    }

    #[test]
    fn pluralize_many_boundary() {
        assert_eq!(pluralize(2, "comment").into_string(), "2 comments");
    }

    #[test]
    fn pluralize_zero_is_plural() {
        assert_eq!(pluralize(0, "comment").into_string(), "0 comments");
    }

    #[test]
    fn pluralize_sibilant_ending() {
        assert_eq!(pluralize(2, "box").into_string(), "2 boxes");
    }

    #[test]
    fn pluralize_consonant_y() {
        assert_eq!(pluralize(2, "category").into_string(), "2 categories");
    }

    #[test]
    fn pluralize_vowel_y_keeps_y() {
        assert_eq!(pluralize(2, "day").into_string(), "2 days");
    }

    #[test]
    fn pluralize_built_in_irregular() {
        assert_eq!(pluralize(2, "person").into_string(), "2 people");
    }

    #[test]
    fn pluralize_with_custom_irregular() {
        assert_eq!(
            pluralize_with(2, "octopus", "octopi").into_string(),
            "2 octopi"
        );
        assert_eq!(
            pluralize_with(1, "octopus", "octopi").into_string(),
            "1 octopus"
        );
    }

    // ── truncate / truncate_with ──────────────────────────────────────────

    #[test]
    fn truncate_shortens_long_text() {
        assert_eq!(
            truncate("The quick brown fox", 10).into_string(),
            "The quick…"
        );
    }

    #[test]
    fn truncate_leaves_short_text_untouched() {
        assert_eq!(truncate("short", 10).into_string(), "short");
    }

    #[test]
    fn truncate_exact_length_untouched() {
        assert_eq!(truncate("exactly10!", 10).into_string(), "exactly10!");
    }

    #[test]
    fn truncate_never_splits_multibyte_grapheme() {
        // Each "🎉" is a 4-byte UTF-8 scalar; a byte-oriented truncation would panic.
        let text = "🎉🎉🎉🎉🎉";
        let result = truncate(text, 3).into_string();
        assert!(result.is_char_boundary(0));
        assert!(result.chars().all(|c| c == '🎉' || c == '…'));
    }

    #[test]
    fn truncate_with_custom_omission() {
        // len (16) caps the *total* output, including the omission marker:
        // "The quick" (9 chars) + " [more]" (7 chars) = 16.
        assert_eq!(
            truncate_with("The quick brown fox", 16, " [more]").into_string(),
            "The quick [more]"
        );
    }

    // ── truncate_words / truncate_words_with ─────────────────────────────

    #[test]
    fn truncate_words_shortens_long_text() {
        assert_eq!(
            truncate_words("The quick brown fox jumps", 3).into_string(),
            "The quick brown…"
        );
    }

    #[test]
    fn truncate_words_leaves_short_text_untouched() {
        assert_eq!(truncate_words("short text", 5).into_string(), "short text");
    }

    #[test]
    fn truncate_words_custom_omission() {
        assert_eq!(
            truncate_words_with("The quick brown fox", 2, " [read more]").into_string(),
            "The quick [read more]"
        );
    }

    // ── time_ago_in_words ──────────────────────────────────────────────────

    #[test]
    fn time_ago_seconds() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 30).unwrap();
        let dt = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let clock = FixedClock::at(now);
        assert_eq!(
            time_ago_in_words(dt, &clock).into_string(),
            "30 seconds ago"
        );
    }

    #[test]
    fn time_ago_one_minute_is_singular() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 1, 0).unwrap();
        let dt = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let clock = FixedClock::at(now);
        assert_eq!(time_ago_in_words(dt, &clock).into_string(), "1 minute ago");
    }

    #[test]
    fn time_ago_minutes() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 3, 0).unwrap();
        let dt = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let clock = FixedClock::at(now);
        assert_eq!(time_ago_in_words(dt, &clock).into_string(), "3 minutes ago");
    }

    #[test]
    fn time_ago_hours() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 14, 0, 0).unwrap();
        let dt = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let clock = FixedClock::at(now);
        assert_eq!(time_ago_in_words(dt, &clock).into_string(), "2 hours ago");
    }

    #[test]
    fn time_ago_days() {
        let now = Utc.with_ymd_and_hms(2026, 1, 3, 12, 0, 0).unwrap();
        let dt = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let clock = FixedClock::at(now);
        assert_eq!(time_ago_in_words(dt, &clock).into_string(), "2 days ago");
    }

    #[test]
    fn time_ago_future() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let dt = Utc.with_ymd_and_hms(2026, 1, 3, 12, 0, 0).unwrap();
        let clock = FixedClock::at(now);
        assert_eq!(time_ago_in_words(dt, &clock).into_string(), "in 2 days");
    }

    // ── format_datetime ─────────────────────────────────────────────────────

    #[test]
    fn format_datetime_custom_format() {
        let dt = Utc.with_ymd_and_hms(2026, 6, 7, 14, 32, 1).unwrap();
        assert_eq!(
            format_datetime(dt, "%Y-%m-%d %H:%M:%S").into_string(),
            "2026-06-07 14:32:01"
        );
    }

    #[test]
    fn format_datetime_date_only() {
        let dt = Utc.with_ymd_and_hms(2026, 6, 7, 14, 32, 1).unwrap();
        assert_eq!(format_datetime(dt, "%Y-%m-%d").into_string(), "2026-06-07");
    }
}
