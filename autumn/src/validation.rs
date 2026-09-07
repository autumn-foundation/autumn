//! Validation support via the `validator` crate.
//!
//! See the [forms, validation and normalization guide](https://github.com/autumn-foundation/autumn/blob/trunk/docs/guide/forms.md)
//! for when to reach for [`Valid`] versus [`crate::form::ChangesetForm`].
//!
//! Provides [`Validated<T>`] — a newtype that proves validation has run —
//! and [`Valid<T>`] — an extractor that auto-validates request bodies.
//!
//! # Usage
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use validator::Validate;
//!
//! #[derive(Deserialize, Validate)]
//! struct NewPost {
//!     #[validate(length(min = 1, max = 200))]
//!     title: String,
//! }
//!
//! #[post("/posts")]
//! async fn create(Valid(Json(post)): Valid<Json<NewPost>>) -> &'static str {
//!     // `post` is guaranteed valid
//!     "created"
//! }
//! ```

use std::collections::HashMap;

use axum::extract::{FromRequest, Request};
use axum::response::{IntoResponse, Response};

// ── Validated<T> newtype ────────────────────────────────────────

/// Proof that `T` has passed validation.
///
/// Cannot be constructed outside this crate — the only way to obtain one
/// is via [`ValidateExt::validate`] or the [`Valid`] extractor.
///
/// Dereferences transparently to `T` for reading, but intentionally does
/// **not** implement `DerefMut` to prevent mutation into an invalid state.
pub struct Validated<T>(T);

impl<T> Validated<T> {
    /// Create a new `Validated<T>`. Restricted to this crate.
    pub(crate) const fn new(value: T) -> Self {
        Self(value)
    }

    /// Unwrap the validated value.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> std::ops::Deref for Validated<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> AsRef<T> for Validated<T> {
    fn as_ref(&self) -> &T {
        &self.0
    }
}

// ── ValidateExt trait ───────────────────────────────────────────

/// Extension trait that adds `.validate()` to any type implementing
/// [`validator::Validate`].
///
/// Returns `AutumnResult<Validated<Self>>` so the `?` operator works
/// in handlers.
///
/// # Hazard: don't combine with `#[validate(nested)]`
///
/// This blanket `impl<T: validator::Validate> ValidateExt for T` applies to
/// *every* `Validate` type, including one used as a `#[validate(nested)]`
/// field inside another `#[derive(validator::Validate)]` struct. `nested`'s
/// generated code calls that field with bare method syntax —
/// `(&self.field).validate()` — and if this trait is ALSO in scope in the
/// **struct's own defining module** at that point (it is, the moment that
/// module does `use autumn_web::prelude::*;`), rustc reports `E0034: multiple
/// applicable items in scope`, pointing at the derive expansion rather than
/// at anything you wrote. This applies equally to `#[autumn_web::model]`
/// structs (issue #1751) — the macro forwards `#[validate(...)]` verbatim and
/// cannot detect the collision at expansion time, since a derive/attribute
/// macro only sees the item it's attached to, not the rest of its enclosing
/// module's `use` statements, so it does not (and cannot reliably) refuse
/// `nested` on your behalf. The collision is scoped to that one module,
/// though: importing this trait in some *other* module that merely uses the
/// nested-validating type does not trigger it. So keep a nested-validating
/// struct's own module free of this trait (and of `autumn_web::prelude`), or
/// express the rule with `#[validate(custom(function = "..."))]` instead.
pub trait ValidateExt: validator::Validate + Sized {
    /// Validate this value and wrap it in [`Validated`].
    ///
    /// # Errors
    ///
    /// Returns [`crate::AutumnError`] with status 422 and field-level
    /// error details if validation fails.
    fn validate(self) -> crate::AutumnResult<Validated<Self>> {
        if let Err(errors) = validator::Validate::validate(&self) {
            return Err(validation_errors_to_autumn_error(&errors));
        }
        Ok(Validated::new(self))
    }
}

impl<T: validator::Validate> ValidateExt for T {}

// ── Valid<T> extractor ──────────────────────────────────────────

/// Extractor that deserializes and validates in one step.
///
/// Wraps any inner extractor (`Json`, `Form`, `Query`). If
/// deserialization succeeds but validation fails, returns 422 with
/// structured error details.
///
/// # Examples
///
/// ```rust,ignore
/// use autumn_web::prelude::*;
/// use autumn_web::Valid;
///
/// #[post("/posts")]
/// async fn create(Valid(Json(new)): Valid<Json<NewPost>>) -> &'static str {
///     // `new` is guaranteed valid
///     "created"
/// }
/// ```
pub struct Valid<T>(pub T);

impl<S, T, Inner> FromRequest<S> for Valid<Inner>
where
    S: Send + Sync,
    Inner: FromRequest<S> + AsValidatable<Inner = T>,
    Inner::Rejection: IntoResponse,
    T: validator::Validate,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let inner = Inner::from_request(req, state)
            .await
            .map_err(IntoResponse::into_response)?;

        let value = inner.as_validatable();
        if let Err(errors) = validator::Validate::validate(value) {
            return Err(
                crate::AutumnError::validation(validation_errors_to_map(&errors)).into_response(),
            );
        }

        Ok(Self(inner))
    }
}

/// Helper trait for extracting the validatable inner type from extractors
/// like `Json<T>`, `Form<T>`, `Query<T>`.
pub trait AsValidatable {
    /// The inner type to validate.
    type Inner;
    /// Returns a reference to the inner type to validate.
    fn as_validatable(&self) -> &Self::Inner;
}

impl<T> AsValidatable for axum::Json<T> {
    type Inner = T;
    fn as_validatable(&self) -> &T {
        &self.0
    }
}

impl<T> AsValidatable for crate::extract::Json<T> {
    type Inner = T;
    fn as_validatable(&self) -> &T {
        &self.0
    }
}

impl<T> AsValidatable for axum::extract::Form<T> {
    type Inner = T;
    fn as_validatable(&self) -> &T {
        &self.0
    }
}

impl<T> AsValidatable for crate::extract::Form<T> {
    type Inner = T;
    fn as_validatable(&self) -> &T {
        &self.0
    }
}

impl<T> AsValidatable for axum::extract::Query<T> {
    type Inner = T;
    fn as_validatable(&self) -> &T {
        &self.0
    }
}

impl<T> AsValidatable for crate::extract::Query<T> {
    type Inner = T;
    fn as_validatable(&self) -> &T {
        &self.0
    }
}

/// Convert `validator::ValidationErrors` into a field → messages map.
pub(crate) fn validation_errors_to_map(
    errors: &validator::ValidationErrors,
) -> HashMap<String, Vec<String>> {
    errors
        .field_errors()
        .into_iter()
        .map(|(field, errs)| {
            let messages = errs
                .iter()
                .map(|e| {
                    e.message.as_ref().map_or_else(
                        || format!("validation failed: {}", e.code),
                        ToString::to_string,
                    )
                })
                .collect();
            (field.to_string(), messages)
        })
        .collect()
}

/// Convert validation errors into an `AutumnError` with 422 status
/// and structured field-level details.
///
/// Not implemented via `From` because `AutumnError` already has a blanket
/// `From<E: Error>` impl that would conflict.
fn validation_errors_to_autumn_error(errors: &validator::ValidationErrors) -> crate::AutumnError {
    crate::AutumnError::validation(validation_errors_to_map(errors))
}

// ── Conditional validation (autoref specialization) ─────────────
//
// The `#[repository(api = ...)]` macro needs to validate a decoded write
// payload *only when its type implements `validator::Validate`* — the
// generated `NewModel` derives `Validate` solely when the model declares
// `#[validate(...)]` rules, and a hand-written `NewModel` may not implement
// it at all. Rust has no stable negative/​specialization reasoning, so we use
// autoref-based specialization (Kalbertodt/dtolnay): a value that implements
// `Validate` resolves to the real validating impl (fewer autorefs), while
// everything else falls through to a no-op. This keeps existing repositories
// compiling with zero migration burden.

/// Wrapper carrying a reference to a candidate write payload for the autoref
/// specialization used by the generated API write handlers.
///
/// Not part of the stable API — used only by macro-generated code.
#[doc(hidden)]
pub struct MaybeValidate<'a, T>(pub &'a T);

/// Specialized branch: runs `validator::Validate` and maps failures to a
/// `422` [`crate::AutumnError`] with the per-field `errors` map.
#[doc(hidden)]
pub trait MaybeValidateViaValidator {
    /// Validate the wrapped value; `Ok(())` when it passes.
    ///
    /// # Errors
    /// Returns a `422` validation error when the wrapped value fails a rule.
    fn autumn_maybe_validate(&self) -> crate::AutumnResult<()>;
}

impl<T: validator::Validate> MaybeValidateViaValidator for MaybeValidate<'_, T> {
    fn autumn_maybe_validate(&self) -> crate::AutumnResult<()> {
        match validator::Validate::validate(self.0) {
            Ok(()) => Ok(()),
            Err(errors) => Err(validation_errors_to_autumn_error(&errors)),
        }
    }
}

/// Fallback branch (behind one autoref): any type that does *not* implement
/// `Validate` is accepted without validation.
#[doc(hidden)]
pub trait MaybeValidateFallback {
    /// No-op validation for types without `#[validate]` rules.
    ///
    /// # Errors
    /// Never returns an error.
    fn autumn_maybe_validate(&self) -> crate::AutumnResult<()>;
}

impl<T> MaybeValidateFallback for &MaybeValidate<'_, T> {
    fn autumn_maybe_validate(&self) -> crate::AutumnResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validated_deref() {
        let v = Validated::new(42);
        assert_eq!(*v, 42);
    }

    #[test]
    fn validated_into_inner() {
        let v = Validated::new("hello".to_string());
        let s = v.into_inner();
        assert_eq!(s, "hello");
    }

    #[test]
    fn validated_as_ref() {
        let v = Validated::new(vec![1, 2, 3]);
        let r: &Vec<i32> = v.as_ref();
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn validation_errors_to_map_basic() {
        #[derive(validator::Validate)]
        struct TestForm {
            #[validate(length(min = 5))]
            name: String,
        }

        let form = TestForm {
            name: "ab".to_string(),
        };
        let errors = validator::Validate::validate(&form).unwrap_err();
        let map = validation_errors_to_map(&errors);

        assert!(map.contains_key("name"));
        assert_eq!(map["name"].len(), 1);
        assert_eq!(map["name"][0], "validation failed: length");
    }

    #[test]
    fn update_patch_field_validates_via_derive() {
        // #1719: a generated `UpdateModel` derives `validator::Validate` and
        // carries `#[validate(...)]` on `Patch<T>` fields. A `Set` value runs
        // the rule (and surfaces a per-field error keyed by the field name),
        // while an absent (`Unchanged`/`Clear`) field is skipped.
        use crate::hooks::Patch;

        #[derive(validator::Validate)]
        struct UpdatePost {
            #[validate(length(min = 1))]
            title: Patch<String>,
        }

        // `Set("")` violates `length(min = 1)` → 422-shaped field error map.
        let bad = UpdatePost {
            title: Patch::Set(String::new()),
        };
        let errors = validator::Validate::validate(&bad).unwrap_err();
        let map = validation_errors_to_map(&errors);
        assert!(map.contains_key("title"));
        assert_eq!(map["title"][0], "validation failed: length");

        // Absent field → rule skipped → passes.
        let unchanged = UpdatePost {
            title: Patch::Unchanged,
        };
        assert!(validator::Validate::validate(&unchanged).is_ok());

        // `Set` with a satisfying value → passes.
        let good = UpdatePost {
            title: Patch::Set("hello".into()),
        };
        assert!(validator::Validate::validate(&good).is_ok());
    }

    #[test]
    fn validate_ext_ok() {
        #[derive(validator::Validate)]
        struct GoodInput {
            #[validate(length(min = 1))]
            value: String,
        }

        let input = GoodInput {
            value: "hello".into(),
        };
        let validated = input.validate();
        assert!(validated.is_ok());
        assert_eq!(validated.unwrap().value, "hello");
    }

    #[test]
    fn validate_ext_err() {
        #[derive(validator::Validate)]
        struct BadInput {
            #[validate(length(min = 5))]
            value: String,
        }

        let input = BadInput { value: "hi".into() };
        let result = input.validate();
        assert!(result.is_err());
    }

    #[test]
    fn validation_errors_convert_to_autumn_error() {
        #[derive(validator::Validate)]
        struct Form {
            #[validate(email)]
            email: String,
        }

        let form = Form {
            email: "not-an-email".into(),
        };
        let errors = validator::Validate::validate(&form).unwrap_err();
        let autumn_err = validation_errors_to_autumn_error(&errors);
        assert_eq!(
            autumn_err.status(),
            axum::http::StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn validation_errors_to_map_fallback_message() {
        let mut errors = validator::ValidationErrors::new();
        // Create an error with no custom message
        let error = validator::ValidationError::new("custom_code");
        errors.add("my_field", error);

        let map = validation_errors_to_map(&errors);

        assert!(map.contains_key("my_field"));
        assert_eq!(map["my_field"][0], "validation failed: custom_code");
    }

    #[tokio::test]
    async fn valid_extractor_ok() {
        use axum::body::Body;

        #[derive(serde::Deserialize, validator::Validate)]
        struct TestInput {
            #[validate(length(min = 1))]
            name: String,
        }

        let req = Request::builder()
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name": "Alice"}"#))
            .unwrap();

        let state = ();
        let result = Valid::<axum::Json<TestInput>>::from_request(req, &state).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap().0.0.name, "Alice");
    }

    #[test]
    // Mirrors the `(&MaybeValidate(&x)).autumn_maybe_validate()` form the
    // repository macro emits: both traits in scope, explicit leading borrow.
    #[allow(clippy::needless_borrow, unused_imports)]
    fn maybe_validate_runs_for_validate_types() {
        // Autoref specialization: a type implementing `Validate` resolves to
        // the real validating branch and surfaces a 422 on failure.
        use super::{MaybeValidate, MaybeValidateFallback as _, MaybeValidateViaValidator as _};

        #[derive(validator::Validate)]
        struct HasRules {
            #[validate(length(min = 5))]
            name: String,
        }

        let bad = HasRules {
            name: "ab".to_string(),
        };
        let err = (&MaybeValidate(&bad))
            .autumn_maybe_validate()
            .expect_err("short name must fail validation");
        assert_eq!(err.status(), axum::http::StatusCode::UNPROCESSABLE_ENTITY);

        let good = HasRules {
            name: "alice".to_string(),
        };
        assert!(
            (&MaybeValidate(&good)).autumn_maybe_validate().is_ok(),
            "valid input must pass"
        );
    }

    #[test]
    #[allow(clippy::needless_borrow, unused_imports)]
    fn maybe_validate_fires_through_a_reference_binding() {
        // #2586: the repository insert path holds its payload as `&New*` and
        // passes that binding straight in. Wrapping `&payload` there would make
        // `T` a reference type, which takes the no-op arm and validates
        // nothing — a silent bypass. Pin the shape the macro emits.
        use super::{MaybeValidate, MaybeValidateFallback as _, MaybeValidateViaValidator as _};

        #[derive(validator::Validate)]
        struct HasRules {
            #[validate(length(min = 5))]
            name: String,
        }

        let owned = HasRules {
            name: "ab".to_string(),
        };
        let payload: &HasRules = &owned;
        assert!(
            (&MaybeValidate(payload)).autumn_maybe_validate().is_err(),
            "a `&New*` binding must reach the validating branch"
        );
    }

    #[test]
    #[allow(clippy::needless_borrow, unused_imports)]
    fn maybe_validate_is_noop_for_non_validate_types() {
        // Autoref specialization: a type that does NOT implement `Validate`
        // falls through to the no-op branch and compiles + always succeeds.
        use super::{MaybeValidate, MaybeValidateFallback as _, MaybeValidateViaValidator as _};

        struct NoRules {
            _name: String,
        }

        let value = NoRules {
            _name: "anything".to_string(),
        };
        assert!(
            (&MaybeValidate(&value)).autumn_maybe_validate().is_ok(),
            "types without validation rules must be accepted unchanged"
        );
    }

    #[tokio::test]
    async fn valid_extractor_err() {
        use axum::body::Body;

        #[derive(serde::Deserialize, validator::Validate)]
        struct TestInput {
            #[validate(length(min = 5))]
            name: String,
        }

        let req = Request::builder()
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name": "Bob"}"#)) // Too short
            .unwrap();

        let state = ();
        let result = Valid::<axum::Json<TestInput>>::from_request(req, &state).await;

        match result {
            Ok(_) => panic!("Expected validation error"),
            Err(response) => {
                assert_eq!(
                    response.status(),
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY
                );
            }
        }
    }
}
