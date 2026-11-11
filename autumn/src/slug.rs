//! URL-safe slug generation (issue #1260).
//!
//! `slugify` is the single shared implementation behind the scaffold
//! generator's `slug:slug{from:...}` DSL token (autumn-cli) and any
//! hand-written app that wants the same "human-readable, shareable" URL
//! segment behavior — replacing the two near-identical hand-rolled copies
//! that used to live in `examples/wiki` and `examples/reddit-clone`.
//!
//! [`contains_letter_or_number`] lives beside it because `slugify` never
//! returns an empty string, which makes `slugify(x).is_empty()` a tempting
//! but always-false validation check (issue #2424). It is the predicate that
//! check was reaching for.

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
///
/// That guarantee has a sharp edge worth stating outright: **`slugify(input)
/// .is_empty()` is always `false`**, so it is never a usable validation check
/// (issue #2424). A caller that wants to reject input carrying no text at all
/// — `"***"`, `"🎉🔥💯"` — wants [`contains_letter_or_number`] instead.
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

/// Does `input` contain at least one letter or number, in any script?
///
/// Precisely: whether any [`char::is_alphanumeric`] character is present, so
/// letters from every script count, and so do numeric characters outside the
/// ASCII digits (`"½"`, `"Ⅻ"`, `"①"`).
///
/// This is the check to reach for when rejecting user input that carries no
/// text, because [`slugify`] cannot answer that question: it never returns an
/// empty string, so `slugify(input).is_empty()` is always `false` and can only
/// ever be dead code (issue #2424).
///
/// Note it is deliberately *broader* than "[`slugify`] produced a real slug".
/// `"日本語"` has no ASCII fold and slugifies to the stable fallback token,
/// but it is real text the author typed, not junk — and the fallback exists
/// precisely so such a title still gets a reachable URL. Rejecting it would
/// trade one bug for an internationalization bug.
///
/// ```
/// use autumn_web::{contains_letter_or_number, slugify};
///
/// assert!(contains_letter_or_number("Ferris arrives"));
/// assert!(contains_letter_or_number("日本語")); // real text, hashed slug
/// assert!(!contains_letter_or_number("***"));
/// assert!(!contains_letter_or_number("🎉🔥💯"));
///
/// // Why you cannot ask `slugify` instead:
/// assert!(!slugify("***").is_empty());
/// ```
///
/// # What this is not
///
/// It is a content check, not a spoofing defence. A few characters are
/// letters by Unicode yet render as blank — the Hangul fillers (`U+3164`,
/// `U+115F`, `U+1160`, `U+FFA0`) are the usual example — so an input built
/// only from those passes. An application that must reject visually empty
/// input needs a confusable/invisible-character filter on top of this.
#[must_use]
pub fn contains_letter_or_number(input: &str) -> bool {
    input.chars().any(char::is_alphanumeric)
}

/// The characters [`slugify_with_suffix`] draws from: digits and lowercase
/// letters minus `i`, `l`, `o` and `u` (Crockford's base32 set). Dropping the
/// lookalikes keeps a suffix readable aloud and safe to retype. 32 symbols is
/// exactly 5 bits, so every symbol is equally likely.
const SUFFIX_ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// [`slugify`] `input`, then append `-` and `suffix_len` random characters
/// drawn from `entropy`.
///
/// This is the "short random tail" slug (`hello-world-k3x9q`): two posts with
/// the same title get different URLs without a database probe for the next
/// free `-2`, `-3`. Each character carries 5 bits, so a 6-character suffix
/// gives about a billion values per title.
///
/// The randomness comes from the [`Entropy`](crate::entropy::Entropy) you pass,
/// so a handler that takes the [`Rng`](crate::entropy::Rng) extractor and passes
/// `rng.source()` mints the same slugs on every replay of a seeded simulation.
///
/// A `suffix_len` of `0` returns plain [`slugify`]. The suffix makes collisions
/// unlikely, not impossible: keep a unique index on the column.
///
/// ```
/// use autumn_web::entropy::SeededEntropy;
/// use autumn_web::slugify_with_suffix;
///
/// let slug = slugify_with_suffix("Hello World", &SeededEntropy::new(7), 6);
/// assert!(slug.starts_with("hello-world-"));
/// assert_eq!(slug.len(), "hello-world-".len() + 6);
/// ```
#[must_use]
pub fn slugify_with_suffix(
    input: &str,
    entropy: &(impl crate::entropy::Entropy + ?Sized),
    suffix_len: usize,
) -> String {
    let mut slug = slugify(input);
    if suffix_len == 0 {
        return slug;
    }
    slug.reserve(suffix_len + 1);
    slug.push('-');
    // One u64 holds twelve 5-bit symbols; draw a fresh one when it runs out.
    let mut bits = 0u64;
    for i in 0..suffix_len {
        if i % 12 == 0 {
            bits = entropy.next_u64();
        }
        slug.push(char::from(SUFFIX_ALPHABET[(bits & 0x1f) as usize]));
        bits >>= 5;
    }
    slug
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

    // ── contains_letter_or_number (#2424) ───────────────────────────────────

    #[test]
    fn slugify_never_returns_an_empty_string() {
        // The premise `contains_letter_or_number` exists for: every `is_empty()`
        // test on a `slugify` result is dead code, so a caller that wants to
        // *reject* content-free input cannot ask `slugify`.
        for input in ["", "   ", "***", "!!!???...:::", "🎉🔥💯", "---", "日本語"] {
            assert!(
                !slugify(input).is_empty(),
                "slugify({input:?}) must never be empty"
            );
        }
    }

    #[test]
    fn punctuation_emoji_and_blank_input_hold_no_letter_or_number() {
        for input in ["", "   ", "***", "!!!???...:::", "🎉🔥💯", "---", "\t\n"] {
            assert!(
                !contains_letter_or_number(input),
                "{input:?} holds no letter or number"
            );
        }
    }

    #[test]
    fn text_in_any_script_holds_a_letter_or_number() {
        // Deliberately broader than "slugify produced a real slug": a non-Latin
        // title has no ASCII fold and slugifies to the fallback token, but it
        // is a real title, not junk.
        for input in ["Ferris arrives", "42", "Café", "日本語", "Привет", "!a!"] {
            assert!(
                contains_letter_or_number(input),
                "{input:?} holds a letter or number"
            );
        }
        assert_eq!(
            slugify("日本語"),
            fallback_token("日本語"),
            "non-Latin text still slugifies to the fallback -- the two questions \
             are genuinely different"
        );
    }

    #[test]
    fn numeric_characters_outside_the_ascii_digits_count() {
        // `char::is_alphanumeric` is `is_alphabetic() || is_numeric()`, so the
        // `Nl`/`No` categories pass. Documented rather than incidental: these
        // are numbers a reader would call numbers, and they still take
        // `slugify`'s fallback for their URL segment.
        for input in ["½", "Ⅻ", "①"] {
            assert!(
                contains_letter_or_number(input),
                "{input:?} is a number by Unicode"
            );
            assert_eq!(slugify(input), fallback_token(input));
        }
    }

    // ── slugify_with_suffix ──────────────────────────────────────────────

    use crate::entropy::{Entropy, SeededEntropy};

    #[test]
    fn suffixed_slug_is_base_dash_suffix() {
        let rng = SeededEntropy::new(7);
        let slug = slugify_with_suffix("Hello World", &rng, 6);
        let (base, suffix) = slug.rsplit_once('-').expect("has a dash");
        assert_eq!(base, "hello-world");
        assert_eq!(suffix.len(), 6);
        assert!(suffix.bytes().all(|b| SUFFIX_ALPHABET.contains(&b)));
    }

    #[test]
    fn suffixed_slug_replays_under_the_same_seed() {
        let a = slugify_with_suffix("Ferris arrives", &SeededEntropy::new(42), 8);
        let b = slugify_with_suffix("Ferris arrives", &SeededEntropy::new(42), 8);
        assert_eq!(a, b);
        let c = slugify_with_suffix("Ferris arrives", &SeededEntropy::new(43), 8);
        assert_ne!(a, c);
    }

    #[test]
    fn successive_draws_differ() {
        let rng = SeededEntropy::new(1);
        let a = slugify_with_suffix("post", &rng, 6);
        let b = slugify_with_suffix("post", &rng, 6);
        assert_ne!(a, b);
    }

    #[test]
    fn long_suffix_spans_several_draws() {
        let slug = slugify_with_suffix("x", &SeededEntropy::new(3), 40);
        assert_eq!(slug.len(), "x-".len() + 40);
        assert!(slug[2..].bytes().all(|b| SUFFIX_ALPHABET.contains(&b)));
    }

    #[test]
    fn zero_length_suffix_is_plain_slugify() {
        assert_eq!(
            slugify_with_suffix("Hello World", &SeededEntropy::new(0), 0),
            "hello-world"
        );
    }

    #[test]
    fn suffix_alphabet_skips_lookalike_letters() {
        assert_eq!(SUFFIX_ALPHABET.len(), 32);
        for c in *b"ilou" {
            assert!(!SUFFIX_ALPHABET.contains(&c));
        }
    }

    #[test]
    fn fallback_base_still_gets_a_suffix() {
        let slug = slugify_with_suffix("日本語", &SeededEntropy::new(9), 4);
        assert!(slug.starts_with(&format!("{}-", slugify("日本語"))));
    }

    #[test]
    fn works_through_a_shared_handle() {
        let shared: std::sync::Arc<dyn Entropy> = SeededEntropy::shared(5);
        let direct = slugify_with_suffix("a", &SeededEntropy::new(5), 6);
        assert_eq!(slugify_with_suffix("a", &shared, 6), direct);
    }
}
