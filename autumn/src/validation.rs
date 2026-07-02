//! Validation support via the `validator` crate.
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
fn validation_errors_to_map(errors: &validator::ValidationErrors) -> HashMap<String, Vec<String>> {
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
