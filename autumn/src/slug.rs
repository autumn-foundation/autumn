//! URL-safe slug generation (issue #1260).
//!
//! `slugify` is the single shared implementation behind the scaffold
//! generator's `slug:slug{from:...}` DSL token (autumn-cli) and any
//! hand-written app that wants the same "human-readable, shareable" URL
//! segment behavior — replacing the two near-identical hand-rolled copies
//! that used to live in `examples/wiki` and `examples/reddit-clone`.

use unicode_normalization::UnicodeNormalization;

/// The Unicode "Combining Diacritical Marks" block. NFD-decomposing an
/// accented Latin letter (e.g. `é`) splits it into a base letter (`e`) plus
/// one of these combining marks (`´`); dropping characters in this range
/// after decomposition is what gives `slugify` its best-effort ASCII folding
/// ("café" -> "cafe") without pulling in a full transliteration table.
const COMBINING_MARKS: std::ops::RangeInclusive<u32> = 0x0300..=0x036F;

/// Convert `input` into a URL-safe slug.
///
/// - Lowercases ASCII letters.
/// - Best-effort ASCII-folds accented Latin characters via NFD decomposition
///   (`"café"` -> `"cafe"`, `"Zürich"` -> `"zurich"`). Non-Latin scripts (CJK,
///   Cyrillic, emoji, ...) have no ASCII fold and are treated as separators —
///   full transliteration is out of scope.
/// - Every other character (punctuation, whitespace, symbols, un-folded
///   non-ASCII) is treated as a separator.
/// - Runs of separators collapse to a single `-`; leading and trailing `-`
///   are trimmed.
///
/// If the result would be empty (input is empty, or entirely punctuation /
/// un-folded non-Latin text), returns a stable, non-empty fallback token
/// deterministically derived from `input` — the same input always slugifies
/// to the same fallback, so callers can rely on `slugify` never returning an
/// empty string.
#[must_use]
pub fn slugify(input: &str) -> String {
    let mut slug = String::with_capacity(input.len());
    let mut pending_separator = false;
    for c in input.nfd() {
        if COMBINING_MARKS.contains(&u32::from(c)) {
            continue;
        }
        if c.is_ascii_alphanumeric() {
            if pending_separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(c.to_ascii_lowercase());
            pending_separator = false;
        } else {
            pending_separator = true;
        }
    }
    if slug.is_empty() {
        fallback_token(input)
    } else {
        slug
    }
}

/// A stable, non-empty fallback for input that slugifies to nothing. Derived
/// from a plain FNV-1a hash of the raw input bytes (not the language's
/// `DefaultHasher`, whose algorithm is explicitly unstable across Rust
/// versions) so the same input always produces the same fallback token.
fn fallback_token(input: &str) -> String {
    format!("n{:x}", fnv1a64(input.as_bytes()))
}

/// 64-bit FNV-1a — small, dependency-free, and deterministic.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_title() {
        assert_eq!(slugify("Hello World"), "hello-world");
    }

    #[test]
    fn punctuation_becomes_separator() {
        assert_eq!(slugify("Rust & WebAssembly!"), "rust-webassembly");
    }

    #[test]
    fn already_a_slug_is_unchanged() {
        assert_eq!(slugify("already-a-slug"), "already-a-slug");
    }

    #[test]
    fn leading_trailing_and_duplicate_separators_collapse() {
        assert_eq!(slugify("  --Hello---World--  "), "hello-world");
    }

    #[test]
    fn apostrophe_is_a_separator() {
        assert_eq!(slugify("What's new?"), "what-s-new");
    }

    #[test]
    fn numbers_are_kept() {
        assert_eq!(slugify("Post 123"), "post-123");
    }

    #[test]
    fn uppercase_is_lowered() {
        assert_eq!(slugify("HERO"), "hero");
    }

    #[test]
    fn unicode_accented_latin_is_ascii_folded() {
        assert_eq!(slugify("Café Münchën"), "cafe-munchen");
        assert_eq!(slugify("Zürich"), "zurich");
        assert_eq!(slugify("naïve"), "naive");
    }

    #[test]
    fn unicode_non_latin_acts_as_separator() {
        // No ASCII fold exists for CJK, so it's dropped like punctuation —
        // the surrounding ASCII words still join with a single separator.
        assert_eq!(slugify("Data 表 Table"), "data-table");
    }

    #[test]
    fn empty_input_falls_back_to_a_stable_non_empty_token() {
        let a = slugify("");
        assert!(!a.is_empty());
        assert_eq!(a, slugify(""), "fallback must be deterministic");
    }

    #[test]
    fn empty_after_strip_falls_back_to_a_stable_non_empty_token() {
        let a = slugify("!!!");
        assert!(!a.is_empty());
        assert_eq!(a, slugify("!!!"), "fallback must be deterministic");
    }

    #[test]
    fn distinct_empty_after_strip_inputs_get_distinct_fallbacks() {
        // Not required for correctness (uniqueness is enforced at the DB
        // layer), but a hash-derived fallback avoids needless collisions.
        assert_ne!(slugify("!!!"), slugify("???"));
        assert_ne!(slugify(""), slugify("!!!"));
    }
}
