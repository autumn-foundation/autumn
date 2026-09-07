//! Field normalization primitives (issue #1379).
//!
//! See the [forms, validation and normalization guide](https://github.com/autumn-foundation/autumn/blob/trunk/docs/guide/forms.md)
//! for where normalization sits relative to validation and hooks.
//!
//! Canonicalizes `#[model]` `String` columns declared with the
//! `#[normalize(...)]` attribute. Normalizers are the small, composable
//! building blocks the macro chains left-to-right; the traits are the
//! runtime hooks the generated code and the derived `#[repository]` finders
//! call.
//!
//! # Built-in normalizers
//!
//! - [`trim`] — strip leading/trailing whitespace.
//! - [`downcase`] — lowercase (str casing; simple ASCII/Unicode `to_lowercase`).
//! - [`upcase`] — uppercase (str casing).
//! - [`squish`] — trim **and** collapse internal runs of whitespace to a single
//!   space.
//! - [`strip_nul`] — remove every NUL (`U+0000`), which a Postgres
//!   `TEXT`/`VARCHAR` column cannot store (#2423).
//!
//! All built-ins are idempotent: normalizing an already-normalized value is a
//! no-op, so applying them on both the write path and on lookups converges on
//! the same canonical value.
//!
//! # Ordering (write path)
//!
//! `#[model]` runs normalization at the head of the repository insert flow,
//! before the model's `#[validate(...)]` rules, before the hooks, and before the
//! row is written. Normalizers apply left-to-right in the order written in the
//! attribute.
//!
//! **Insert** (`save`, `save_many`, `save_many_skip_invalid`, and the create
//! half of `find_or_create_by_*`) normalizes the `New*` input, so the model's
//! rules *and the database* observe the canonical value (#2586).
//!
//! **Update is not symmetric with insert, and there are three cases, not two.**
//!
//! | Repository | Merged draft | Persisted |
//! |---|---|---|
//! | no hooks, no knob (**blind**) | none built — `from_patch` is not emitted | raw patch |
//! | `validate_on_update = fetch` | built and normalized, **validation only** | raw patch |
//! | has hooks | built and normalized | the normalized draft |
//!
//! The middle row is the one that surprises: `from_patch` normalizes the merged
//! model so validators see the canonical value, but the generated code keeps
//! only its 422 and persists `changes.__to_changeset()` unchanged. So on either
//! of the first two, an `update` that sets a `#[normalize(trim, downcase)]`
//! column to `"  FOO@X.com "` stores it verbatim, and
//! [`normalize_lookup_value`]-based finders will not match that row. Only the
//! hooked path writes the canonical value.
//!
//! If a normalized column is ever written through `update`, give the repository
//! hooks or normalize before building the patch — and consider a `CITEXT`
//! column or a functional unique index as the durable backstop. See
//! `docs/guide/forms.md`.

/// Strip leading and trailing whitespace.
#[must_use]
pub fn trim(s: &str) -> String {
    s.trim().to_owned()
}

/// Lowercase the value (str casing).
#[must_use]
pub fn downcase(s: &str) -> String {
    s.to_lowercase()
}

/// Uppercase the value (str casing).
#[must_use]
pub fn upcase(s: &str) -> String {
    s.to_uppercase()
}

/// Trim and collapse every internal run of whitespace to a single ASCII space.
#[must_use]
pub fn squish(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Remove every NUL (`U+0000`) character (issue #2423).
///
/// A Postgres `TEXT`/`VARCHAR` column cannot hold `0x00`, so a value carrying
/// one is rejected by the server at `INSERT`/`UPDATE` time. Reach for this on
/// a column fed by input that may legitimately arrive dirty (a paste from a
/// binary source, a misbehaving input method) and where silently dropping the
/// byte is preferable to rejecting the write.
///
/// Only NUL is removed: every other control character is storable and is left
/// exactly as written.
///
/// Prefer rejecting over cleaning where the author can see and fix the value —
/// [`crate::form::ChangesetForm`] already surfaces a NUL as an inline field
/// error without any attribute.
#[must_use]
pub fn strip_nul(s: &str) -> String {
    s.replace('\u{0}', "")
}

/// A type whose `#[normalize]` columns can be canonicalized in place.
///
/// Implemented by `#[model]` for every generated `New*` insert struct and for
/// the model itself. `normalize` applies each field's normalizer chain; it is a
/// no-op for models with no `#[normalize]` columns.
pub trait Normalize {
    /// Canonicalize every `#[normalize]` field in place.
    fn normalize(&mut self);
}

/// A model that knows how to canonicalize a lookup argument for one of its
/// columns, so derived `#[repository]` `find_by_`/`count_by_` finders match the
/// stored (already-normalized) value.
///
/// Implemented by `#[model]` for every model — `normalize_lookup` returns
/// `None` for a column with no `#[normalize]` attribute (the finder then uses
/// the raw argument), and `Some(canonical)` for a normalized column.
pub trait NormalizedModel {
    /// The canonical form of `value` for `column`, or `None` if `column` is not
    /// a `#[normalize]` column on this model.
    fn normalize_lookup(column: &str, value: &str) -> Option<String>;
}

/// Normalize a derived-finder argument for `column` on model `M`.
///
/// Falls back to the raw value when the column is not normalized. Called by the
/// generated `#[repository]` finder chain so `find_by_email("  FOO@X.com ")`
/// matches the stored `foo@x.com` row.
#[must_use]
pub fn normalize_lookup_value<M: NormalizedModel>(column: &str, value: &str) -> String {
    M::normalize_lookup(column, value).unwrap_or_else(|| value.to_owned())
}

// ── Autoref specialization for the `#[repository]` call sites ─────────────
//
// A `#[repository]` may target a model whose `New*`/model types are *hand
// written* (not generated by `#[model]`), so they need not implement
// `Normalize` / `NormalizedModel`. The generated `save`/finder code therefore
// dispatches through these zero-cost probe wrappers: the specialized impl (by
// value, gated on the trait bound) is selected when the concrete type
// implements the trait, otherwise method resolution autorefs to the no-op
// fallback. This resolves at the concrete call site the macro emits — it does
// **not** work through a generic boundary, which is why the wrappers are
// constructed in the generated code rather than behind a generic helper.

/// Probe wrapper for write-path normalization.
///
/// Holds an immutable borrow of the caller's input. The specialized `Yes` impl
/// (selected only when the concrete type is `Normalize + Clone`) clones and
/// canonicalizes, returning the owned value; the `No` fallback returns the
/// borrow untouched — so a model with no `#[normalize]` columns (or a
/// hand-written `New*` that doesn't implement `Normalize`) pays no clone on the
/// save path. The generated code unifies the two arms with `Borrow` (see
/// `#[repository]` `save`, `save_many`, `save_many_skip_invalid` and
/// `find_or_create_by_*`).
#[doc(hidden)]
pub struct SpezNormalize<'a, T: ?Sized>(pub &'a T);

#[doc(hidden)]
pub trait SpezNormalizeYes<'a, T> {
    fn spez_normalize(self) -> T;
}
impl<'a, T: Normalize + Clone> SpezNormalizeYes<'a, T> for SpezNormalize<'a, T> {
    #[inline]
    fn spez_normalize(self) -> T {
        let mut owned = self.0.clone();
        owned.normalize();
        owned
    }
}

#[doc(hidden)]
pub trait SpezNormalizeNo<'a, T: ?Sized> {
    fn spez_normalize(self) -> &'a T;
}
impl<'a, T: ?Sized> SpezNormalizeNo<'a, T> for &SpezNormalize<'a, T> {
    #[inline]
    fn spez_normalize(self) -> &'a T {
        self.0
    }
}

#[doc(hidden)]
pub trait SpezNormalizeManyYes<'a, T> {
    fn spez_normalize_many(self) -> Vec<T>;
}
impl<'a, T: Normalize + Clone> SpezNormalizeManyYes<'a, T> for SpezNormalize<'a, [T]> {
    #[inline]
    fn spez_normalize_many(self) -> Vec<T> {
        self.0
            .iter()
            .map(|item| {
                let mut owned = item.clone();
                owned.normalize();
                owned
            })
            .collect()
    }
}

#[doc(hidden)]
pub trait SpezNormalizeManyNo<'a, T> {
    fn spez_normalize_many(self) -> &'a [T];
}
impl<'a, T> SpezNormalizeManyNo<'a, T> for &SpezNormalize<'a, [T]> {
    #[inline]
    fn spez_normalize_many(self) -> &'a [T] {
        self.0
    }
}

/// Probe wrapper for finder-argument normalization on lookups.
#[doc(hidden)]
pub struct SpezLookup<'a, M>(pub core::marker::PhantomData<M>, pub &'a str, pub &'a str);

#[doc(hidden)]
pub trait SpezLookupYes {
    fn spez_lookup(self) -> String;
}
impl<M: NormalizedModel> SpezLookupYes for SpezLookup<'_, M> {
    #[inline]
    fn spez_lookup(self) -> String {
        normalize_lookup_value::<M>(self.1, self.2)
    }
}

#[doc(hidden)]
pub trait SpezLookupNo {
    fn spez_lookup(self) -> String;
}
impl<M> SpezLookupNo for &SpezLookup<'_, M> {
    #[inline]
    fn spez_lookup(self) -> String {
        self.2.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_strips_edges() {
        assert_eq!(trim("  hi  "), "hi");
        assert_eq!(trim("hi"), "hi");
    }

    #[test]
    fn strip_nul_removes_embedded_nuls() {
        assert_eq!(strip_nul("before\u{0}after"), "beforeafter");
        assert_eq!(strip_nul("\u{0}\u{0}a\u{0}"), "a");
    }

    #[test]
    fn strip_nul_leaves_clean_text_untouched() {
        assert_eq!(strip_nul("plain text"), "plain text");
        // Other control characters are storable in a Postgres TEXT column and
        // are deliberately left alone.
        assert_eq!(strip_nul("tab\there"), "tab\there");
    }

    #[test]
    fn strip_nul_is_idempotent() {
        let once = strip_nul("a\u{0}b");
        assert_eq!(strip_nul(&once), once);
    }

    #[test]
    fn downcase_and_upcase() {
        assert_eq!(downcase("Foo@X.COM"), "foo@x.com");
        assert_eq!(upcase("abc"), "ABC");
    }

    #[test]
    fn squish_collapses_internal_whitespace() {
        assert_eq!(squish("  a   b\tc\n d "), "a b c d");
    }

    #[test]
    fn builtins_are_idempotent() {
        // Round-trip: normalizing an already-normalized value is a no-op
        // (issue #1379 AC: idempotency).
        for input in ["  Foo@X.COM ", "a  b   c", "MixedCase", ""] {
            let once = squish(&downcase(&trim(input)));
            let twice = squish(&downcase(&trim(&once)));
            assert_eq!(once, twice, "not idempotent for {input:?}");
        }
    }

    struct Account;
    impl NormalizedModel for Account {
        fn normalize_lookup(column: &str, value: &str) -> Option<String> {
            match column {
                "email" => Some(downcase(&trim(value))),
                _ => None,
            }
        }
    }

    #[test]
    fn normalize_lookup_value_canonicalizes_known_column() {
        assert_eq!(
            normalize_lookup_value::<Account>("email", "  FOO@X.com "),
            "foo@x.com"
        );
        // Unknown column passes through unchanged.
        assert_eq!(
            normalize_lookup_value::<Account>("nickname", "  Kept "),
            "  Kept "
        );
    }

    // ── Autoref specialization dispatch (repository call sites) ────────────

    #[derive(Clone)]
    struct Norms(String);
    impl Normalize for Norms {
        fn normalize(&mut self) {
            self.0 = downcase(&trim(&self.0));
        }
    }
    struct Plain(#[allow(dead_code)] String); // no Normalize impl

    #[test]
    fn spez_normalize_runs_for_normalize_types_and_noops_otherwise() {
        use super::{SpezNormalizeNo as _, SpezNormalizeYes as _};
        use std::borrow::Borrow as _;

        // A `Normalize + Clone` type is cloned and canonicalized; the borrow
        // unifies both arms to `&T`.
        let n = Norms("  Foo ".into());
        let normalized = SpezNormalize(&n).spez_normalize();
        let n_ref: &Norms = normalized.borrow();
        assert_eq!(n_ref.0, "foo", "Normalize type must be canonicalized");
        // The original input is untouched (the probe borrows immutably).
        assert_eq!(n.0, "  Foo ", "input must not be mutated in place");

        // A type that does NOT implement Normalize compiles and is returned
        // untouched by reference (mirrors a hand-written `New*` used with
        // `#[repository]`) — no clone is performed. The `No` arm yields `&Plain`
        // directly (the generated code's `.borrow()` is a no-op in this case).
        let p = Plain("  Foo ".into());
        let p_ref: &Plain = SpezNormalize(&p).spez_normalize();
        assert_eq!(p_ref.0, "  Foo ", "non-Normalize type must be untouched");
    }

    #[test]
    fn spez_lookup_uses_normalized_model_or_falls_back() {
        use super::{SpezLookupNo as _, SpezLookupYes as _};

        // NormalizedModel type: argument is canonicalized.
        let got =
            SpezLookup::<Account>(std::marker::PhantomData, "email", "  FOO@X.com ").spez_lookup();
        assert_eq!(got, "foo@x.com");

        // A type without NormalizedModel falls back to the raw value.
        let got = SpezLookup::<Plain>(std::marker::PhantomData, "email", "  Kept ").spez_lookup();
        assert_eq!(got, "  Kept ");
    }
}
