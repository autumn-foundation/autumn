//! Shared display-width helper for the CLI's aligned-table renderers
//! (`autumn routes`, `autumn flags/config/experiments list/status`).
//!
//! `str::len()` returns UTF-8 byte length, but the padding built into
//! `format!("{:width$}", ...)` for `&str` counts Unicode scalar values
//! (`chars().count()`), not bytes. Using byte length to compute a column's
//! target width over-pads any cell containing multi-byte characters
//! relative to same-length ASCII cells in the same column.

/// Display width used for column-alignment purposes: Unicode scalar count,
/// matching what `format!("{:width$}")` actually pads against.
pub fn display_width(s: &str) -> usize {
    s.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_width_matches_byte_length() {
        assert_eq!(display_width("hello"), 5);
    }

    #[test]
    fn multi_byte_chars_count_once_each() {
        // "café" is 4 chars but 5 bytes (é is 2 bytes in UTF-8).
        assert_eq!(display_width("café"), 4);
        assert_ne!(display_width("café"), "café".len());
    }

    #[test]
    fn empty_string_has_zero_width() {
        assert_eq!(display_width(""), 0);
    }
}
