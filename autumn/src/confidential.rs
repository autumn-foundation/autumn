//! Type-level capabilities for confidential model fields.
//!
//! `#[model]` gives every `#[confidential]` field a zero-sized
//! [`ConfidentialField`] marker.  The marker deliberately is not a Diesel
//! expression: it implements no ordering, range, pattern, aggregate, or join
//! traits.  Its sole query capability is [`ConfidentialField::blind_index_eq`].
//!
//! # Raw Diesel boundary
//!
//! This guarantee applies to Autumn's generated model/repository API. Diesel's
//! `table!` module is application-owned and its columns are therefore not made
//! private by a procedural macro on the model. Code which imports that module
//! and constructs raw Diesel expressions has explicitly crossed the boundary
//! and must be reviewed like handwritten SQL. Keep schema modules private when
//! an application wants the Rust module system to enforce the same boundary.

use core::marker::PhantomData;

/// An opaque, already-keyed blind-index token.
///
/// Construct tokens at the application's key-management boundary. This type
/// intentionally has no plaintext constructor and does not implement `Display`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct BlindIndexToken(String);

impl BlindIndexToken {
    /// Import an encoded token produced by the configured blind-index service.
    #[must_use]
    pub fn from_encoded(encoded: impl Into<String>) -> Self {
        Self(encoded.into())
    }

    /// Borrow the encoded value for binding to the generated predicate.
    #[must_use]
    pub fn as_encoded(&self) -> &str {
        &self.0
    }
}

/// Equality predicate produced by the only supported confidential query.
#[derive(Clone, PartialEq, Eq)]
pub struct BlindIndexPredicate<F> {
    token: BlindIndexToken,
    _field: PhantomData<fn() -> F>,
}

impl<F> BlindIndexPredicate<F> {
    /// The opaque bind value. This does not reveal plaintext or ciphertext.
    #[must_use]
    pub fn token(&self) -> &BlindIndexToken {
        &self.token
    }
}

/// Zero-sized capability naming one confidential model field.
///
/// It intentionally implements no Diesel expression traits. In particular it
/// cannot be ordered, ranged, pattern-matched, aggregated, or used as a join
/// key.
pub struct ConfidentialField<F>(PhantomData<fn() -> F>);

impl<F> ConfidentialField<F> {
    /// Build the named blind-index equality predicate for this exact field.
    #[must_use]
    pub fn blind_index_eq(token: BlindIndexToken) -> BlindIndexPredicate<F> {
        BlindIndexPredicate {
            token,
            _field: PhantomData,
        }
    }
}
