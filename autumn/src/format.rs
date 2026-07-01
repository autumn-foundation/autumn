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
//! ```rust,ignore
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
    pub fn format(&self, _value: Decimal) -> maud::Markup {
        // TODO(red): not implemented yet.
        maud::html! { "" }
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

// ── Delimited numbers ────────────────────────────────────────────────────────

/// Render an integer or decimal with grouped thousands (`1,234,567`).
#[cfg(feature = "maud")]
#[must_use]
pub fn number_with_delimiter<T: Into<Decimal>>(_value: T) -> maud::Markup {
    // TODO(red): not implemented yet.
    maud::html! { "" }
}

// ── Pluralize ────────────────────────────────────────────────────────────────

/// Render `"{count} {word}"`, pluralizing `singular` with a simple
/// irregular-aware English rule when `count != 1`.
#[cfg(feature = "maud")]
#[must_use]
pub fn pluralize(_count: i64, _singular: &str) -> maud::Markup {
    // TODO(red): not implemented yet.
    maud::html! { "" }
}

/// Render `"{count} {word}"`, choosing between `singular` and an explicit `plural`.
#[cfg(feature = "maud")]
#[must_use]
pub fn pluralize_with(_count: i64, _singular: &str, _plural: &str) -> maud::Markup {
    // TODO(red): not implemented yet.
    maud::html! { "" }
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
pub fn truncate_with(_text: &str, _len: usize, _omission: &str) -> maud::Markup {
    // TODO(red): not implemented yet.
    maud::html! { "" }
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
pub fn truncate_words_with(_text: &str, _n: usize, _omission: &str) -> maud::Markup {
    // TODO(red): not implemented yet.
    maud::html! { "" }
}

// ── Dates & times ────────────────────────────────────────────────────────────

/// Render a human-readable relative time (`"3 minutes ago"`, `"in 2 days"`)
/// between `dt` and the current instant of `clock`.
#[cfg(feature = "maud")]
#[must_use]
pub fn time_ago_in_words(_dt: DateTime<Utc>, _clock: &dyn ClockSource) -> maud::Markup {
    // TODO(red): not implemented yet.
    maud::html! { "" }
}

/// Render `dt` (UTC) using a `chrono` strftime-style absolute format string.
#[cfg(feature = "maud")]
#[must_use]
pub fn format_datetime(_dt: DateTime<Utc>, _fmt: &str) -> maud::Markup {
    // TODO(red): not implemented yet.
    maud::html! { "" }
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
        assert_eq!(
            number_to_currency(dec("1234.5")).into_string(),
            "$1,234.50"
        );
    }

    #[test]
    fn currency_formats_zero() {
        assert_eq!(number_to_currency(dec("0")).into_string(), "$0.00");
    }

    #[test]
    fn currency_formats_negative() {
        assert_eq!(
            number_to_currency(dec("-42.5")).into_string(),
            "-$42.50"
        );
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
        assert_eq!(
            truncate_with("The quick brown fox", 12, " [more]").into_string(),
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
        assert_eq!(
            time_ago_in_words(dt, &clock).into_string(),
            "3 minutes ago"
        );
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
