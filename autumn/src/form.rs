//! Changeset-style form helpers with validation and Maud rendering.
//!
//! See the [forms, validation and normalization guide](https://github.com/autumn-foundation/autumn/blob/trunk/docs/guide/forms.md)
//! for the narrative version of everything below.
//!
//! # Overview
//!
//! [`Changeset<T>`] captures submitted form values together with per-field
//! validation errors, enabling the create/edit/validate-failure round-trip
//! in a single route handler — no manual flash-carrying, no conditional
//! error-threading.
//!
//! [`ChangesetForm<T>`] is the **default server-rendered HTML form validation
//! path** for any `T: DeserializeOwned + Serialize + Validate`.  It is the
//! axum extractor that decodes the request body (URL-encoded **or**
//! multipart), runs [`validator::Validate`], captures the CSRF token, and
//! hands the handler a ready-to-use changeset — CSRF is emitted
//! automatically when you call [`ChangesetForm::form_tag`].
//!
//! # htmx inline field validation
//!
//! Use [`text_input_htmx`] to wire up per-field inline validation with htmx.
//! The rendered input POSTs to a validation endpoint when its value changes,
//! and htmx swaps the returned field wrapper in place with `outerHTML`.
//!
//! A minimal inline-validation endpoint:
//!
//! ```rust,ignore
//! #[post("/users/validate/email")]
//! async fn validate_email(form: ChangesetForm<UserForm>) -> Markup {
//!     text_input_htmx(&form.changeset, "email", "Email", "/users/validate/email")
//! }
//! ```
//!
//! No-JavaScript fallback is automatic: when htmx is absent the browser
//! falls through to the standard full-form `POST` handler, which returns
//! 422 with inline errors via the same `text_input_htmx` partial.
//!
//! # Model vs custom form structs
//!
//! ## Pattern A — `NewModel` direct
//!
//! When the model struct already has `#[derive(Validate)]` and the form
//! shape matches, use `ChangesetForm<NewModel>` directly:
//!
//! ```rust,ignore
//! #[post("/todos")]
//! async fn create(db: Db, form: ChangesetForm<NewTodo>) -> impl IntoResponse {
//!     match form.into_valid() {
//!         Ok(new_todo) => { /* insert new_todo directly */ }
//!         Err(form) => (StatusCode::UNPROCESSABLE_ENTITY, render(&form)).into_response(),
//!     }
//! }
//! ```
//!
//! ## Pattern B — Custom workflow struct
//!
//! Define a separate form struct when the form needs extra fields (e.g.
//! `confirm_password`), different validation rules, or UI-specific derives.
//! Convert to the model type on successful validation:
//!
//! ```rust,ignore
//! #[post("/users")]
//! async fn create_user(form: ChangesetForm<RegistrationForm>) -> impl IntoResponse {
//!     match form.into_valid() {
//!         Ok(f) => { let user = NewUser::from(f); /* persist */ }
//!         Err(form) => (StatusCode::UNPROCESSABLE_ENTITY, render(&form)).into_response(),
//!     }
//! }
//! ```
//!
//! # Framework comparison
//!
//! | Framework | Changeset type | Rendering helper |
//! |-----------|---------------|-----------------|
//! | Phoenix (Elixir) | `Ecto.Changeset` | `<.input field={@form[:name]} />` |
//! | Rails (Ruby) | `errors[:field]` | `f.text_field :name` |
//! | Django (Python) | `forms.Form` | `{{ form.name.errors }}` |
//! | Autumn (Rust) | `Changeset<T>` | `form.text_input("name", "Name")` |
//!
//! # Happy-path + validation-failure in ≤ 40 `LoC`
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::form::{ChangesetForm, Changeset, submit_button};
//! use serde::{Deserialize, Serialize};
//! use validator::Validate;
//! use axum::{http::StatusCode, response::IntoResponse};
//!
//! #[derive(Deserialize, Serialize, Validate)]
//! struct GreetForm {
//!     #[validate(length(min = 3, message = "Name must be at least 3 characters"))]
//!     name: String,
//!     #[validate(email(message = "Must be a valid email address"))]
//!     email: String,
//! }
//!
//! fn greet_form_partial(form: &ChangesetForm<GreetForm>, action: &str) -> Markup {
//!     form.form_tag(action, "post", html! {
//!         (form.text_input("name", "Full name"))
//!         (form.text_input("email", "Email"))
//!         (form.submit_button("Submit"))
//!     })
//! }
//!
//! #[get("/greet/new")]
//! async fn new_greet(csrf: CsrfToken) -> Markup {
//!     let blank = ChangesetForm::blank(GreetForm { name: String::new(), email: String::new() },
//!                                     csrf.token());
//!     greet_form_partial(&blank, "/greet")
//! }
//!
//! #[post("/greet")]
//! async fn create_greet(form: ChangesetForm<GreetForm>) -> impl IntoResponse {
//!     match form.into_valid() {
//!         Ok(f) => html! { p { "Hello, " (f.name) "!" } }.into_response(),
//!         Err(form) => (StatusCode::UNPROCESSABLE_ENTITY,
//!                       greet_form_partial(&form, "/greet")).into_response(),
//!     }
//! }
//! ```
//!
//! # CSRF
//!
//! The CSRF token is captured automatically by the [`ChangesetForm`] extractor
//! from the request extensions set by [`crate::security::CsrfLayer`].
//! Calling [`ChangesetForm::form_tag`] emits the hidden `_csrf` input with no
//! additional developer action in POST handlers.
//!
//! For GET handlers (new/edit forms), construct the form context via
//! [`ChangesetForm::blank`], passing `csrf.token()` from a [`crate::security::CsrfToken`]
//! extractor — the only extra line needed is the parameter itself.
//!
//! # Multipart
//!
//! When the `multipart` feature is enabled, [`ChangesetForm`] also decodes
//! `multipart/form-data` bodies.  File fields are skipped; only text fields
//! are decoded.  File upload storage is out of scope here (see issue #494).
//!
//! # Non-htmx fallback
//!
//! When JavaScript is disabled htmx falls back to a standard form POST.
//! The handler pattern above still works: browsers display the 422 page
//! inline.  For a redirect-after-post pattern, serialise `cs.errors()` into
//! the flash store and redirect; restore on the next GET.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::collections::HashMap;
use std::sync::OnceLock;

use axum::extract::{FromRequest, Request};
use axum::response::IntoResponse;
use serde::Serialize;

// ── Changeset<T> ───────────────────────────────────────────────────

/// Carries submitted form values and per-field validation errors.
///
/// Analogous to `Ecto.Changeset` in Phoenix or `errors[:field]` in Rails.
///
/// Obtain a `Changeset` from:
/// - [`Changeset::new`] for a blank/valid changeset
/// - [`IntoChangeset::into_changeset`] after manual construction
/// - The [`ChangesetForm`] axum extractor (preferred)
pub struct Changeset<T> {
    data: T,
    errors: HashMap<String, Vec<String>>,
    /// Lazily-computed `serde_json::to_value(&data)`, shared across every
    /// [`Self::field_value`] call on this changeset instead of re-serializing
    /// the whole record per field. A form with N fields previously paid a
    /// full-struct serialization N times over — once per rendered input — for
    /// each render.
    ///
    /// Boxed rather than a bare `OnceLock<serde_json::Value>`: an inline
    /// `Value` pushes `Changeset` (and therefore `Result<T, Changeset<T>>` in
    /// [`Self::into_valid`]) over clippy's `result_large_err` threshold.
    ///
    /// A snapshot, not a live view: if `T` holds interior-mutable fields
    /// (`RefCell`, `Mutex`, `Atomic*`, …) mutated through the shared
    /// reference [`Self::data`] hands out, a `field_value` call made before
    /// that mutation poisons every later call on the same `Changeset` with
    /// the pre-mutation value — `serde_json::to_value(&self.data)` used to
    /// re-run, and would have observed it, on every call. `Changeset` is
    /// documented and universally used as build-once-render-once submitted
    /// form data (see every call site of `field_value`/`data`), which has no
    /// interior mutability and no reason to mutate between renders, so this
    /// is a real but currently theoretical caveat rather than an active bug.
    ///
    /// Deliberately excluded from [`Debug`](std::fmt::Debug) (see the manual
    /// impl below): it holds a raw `serde_json` snapshot of every
    /// serializable field, which would bypass any redaction a caller's own
    /// `Debug` impl on `T` applies to a field `Serialize` still includes
    /// (e.g. a password field masked in `Debug` output but still submitted
    /// and therefore serialized) — deriving `Debug` here would print that
    /// unredacted snapshot alongside `T`'s redacted one, and would only start
    /// doing so once some `field_value` call happened to fill the cache,
    /// making the leak initialization-order-dependent as well.
    field_value_cache: OnceLock<Box<serde_json::Value>>,
}

#[allow(
    clippy::missing_fields_in_debug,
    reason = "field_value_cache is deliberately omitted — see its doc comment"
)]
impl<T: std::fmt::Debug> std::fmt::Debug for Changeset<T> {
    /// Mirrors the pre-cache derived output exactly (`data` and `errors`
    /// only): the cache field's own doc comment explains why it must never
    /// appear here.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Changeset")
            .field("data", &self.data)
            .field("errors", &self.errors)
            .finish()
    }
}

impl<T> Changeset<T> {
    /// Create a changeset with no errors (valid state).
    pub fn new(data: T) -> Self {
        Self {
            data,
            errors: HashMap::new(),
            field_value_cache: OnceLock::new(),
        }
    }

    /// Create a changeset pre-loaded with field-level errors.
    pub const fn from_errors(data: T, errors: HashMap<String, Vec<String>>) -> Self {
        Self {
            data,
            errors,
            field_value_cache: OnceLock::new(),
        }
    }

    /// Returns `true` when there are no field-level errors.
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    /// Returns the validation messages for `field`, or an empty slice.
    pub fn errors_for(&self, field: &str) -> &[String] {
        self.errors.get(field).map_or(&[], Vec::as_slice)
    }

    /// Returns every field-level error keyed by field name.
    ///
    /// Iteration order is unspecified (it is a [`HashMap`]); callers that render
    /// a stable list (e.g. [`crate::widgets::error_summary`]) should sort. Used
    /// by the error-summary widget to enumerate all messages without the caller
    /// needing to know the field set in advance.
    #[must_use]
    pub const fn all_errors(&self) -> &HashMap<String, Vec<String>> {
        &self.errors
    }

    /// Unwrap the inner data regardless of validity.
    pub fn into_inner(self) -> T {
        self.data
    }

    /// Consume the changeset, returning `Ok(T)` if valid or `Err(self)` if not.
    ///
    /// # Errors
    ///
    /// Returns `Err(self)` when there are field-level validation errors.
    pub fn into_valid(self) -> Result<T, Self> {
        if self.is_valid() {
            Ok(self.data)
        } else {
            Err(self)
        }
    }

    /// Shared reference to the inner data.
    pub const fn data(&self) -> &T {
        &self.data
    }

    /// All field errors as a map (field name → list of messages).
    pub const fn errors(&self) -> &HashMap<String, Vec<String>> {
        &self.errors
    }
}

impl<T: Serialize> Changeset<T> {
    /// Serialize the value of `field` from the inner data to a `String`.
    ///
    /// Used by rendering helpers to re-populate `<input value="…">` after a
    /// failed submission.  Returns `None` for missing or non-scalar fields.
    ///
    /// The serialization is cached on first call and reused by later calls on
    /// the same `Changeset`, rather than re-run per call. This is a snapshot:
    /// if `T` holds interior-mutable fields mutated through the shared
    /// reference [`Self::data`] hands out, a call made before that mutation
    /// makes every later call on this `Changeset` observe the pre-mutation
    /// value instead of the current one.
    pub fn field_value(&self, field: &str) -> Option<String> {
        let json = self.field_value_cache.get_or_init(|| {
            Box::new(serde_json::to_value(&self.data).unwrap_or(serde_json::Value::Null))
        });
        match json.get(field)? {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }
}

// ── IntoChangeset ──────────────────────────────────────────────────

/// Validate `self` and wrap in a [`Changeset`].
///
/// Blanket-implemented for every type that implements [`validator::Validate`].
pub trait IntoChangeset: Sized {
    /// Run validation and produce a `Changeset<Self>`.
    fn into_changeset(self) -> Changeset<Self>;
}

impl<T: validator::Validate> IntoChangeset for T {
    fn into_changeset(self) -> Changeset<Self> {
        match validator::Validate::validate(&self) {
            Ok(()) => Changeset::new(self),
            Err(errors) => Changeset::from_errors(self, validation_errors_to_map(&errors)),
        }
    }
}

// ── ChangesetForm<T> ───────────────────────────────────────────────

/// Axum extractor that decodes a form body, runs validation, and captures the
/// CSRF token — all in one step.
///
/// Supports both `application/x-www-form-urlencoded` (always) and
/// `multipart/form-data` (when the `multipart` feature is enabled).
///
/// Unlike [`crate::validation::Valid`], this extractor **never** rejects with
/// 422 — errors live in the [`Changeset`] and the handler decides how to
/// respond.  Fails with 400 only when the body cannot be decoded into `T` at
/// all.
///
/// # CSRF — no extra developer action in POST handlers
///
/// The extractor reads the `CsrfToken` from request extensions (placed there
/// by [`crate::security::CsrfLayer`]).  Calling
/// [`ChangesetForm::form_tag`] then emits the hidden `_csrf` input
/// automatically — no separate `CsrfToken` parameter needed.
///
/// For GET handlers (new/edit), use [`ChangesetForm::blank`] and pass
/// `csrf.token()` from a `CsrfToken` extractor.
///
/// # Example
///
/// ```rust,ignore
/// #[post("/users")]
/// async fn create(form: ChangesetForm<NewUser>) -> impl IntoResponse {
///     match form.into_valid() {
///         Ok(user) => { /* persist & redirect */ }
///         Err(form) => (StatusCode::UNPROCESSABLE_ENTITY,
///                       form.form_tag("/users", "post", html! {
///                           (form.text_input("name", "Name"))
///                           (form.submit_button("Save"))
///                       })).into_response()
///     }
/// }
/// ```
pub struct ChangesetForm<T> {
    /// The validated (or invalid) changeset.
    pub changeset: Changeset<T>,
    pub(crate) csrf_token: Option<String>,
    pub(crate) csrf_field: String,
}

impl<T> ChangesetForm<T> {
    /// Build a blank form context for GET handlers (new / edit).
    ///
    /// Wraps `data` in a valid [`Changeset`] and stores `csrf_token` so that
    /// [`ChangesetForm::form_tag`] can emit the hidden input automatically.
    ///
    /// ```rust,ignore
    /// #[get("/users/new")]
    /// async fn new_user(csrf: CsrfToken) -> Markup {
    ///     let ctx = ChangesetForm::blank(UserForm::default(), csrf.token());
    ///     ctx.form_tag("/users", "post", html! { (ctx.text_input("name", "Name")) })
    /// }
    /// ```
    pub fn blank(data: T, csrf_token: &str) -> Self {
        Self {
            changeset: Changeset::new(data),
            csrf_token: Some(csrf_token.to_owned()),
            csrf_field: "_csrf".to_owned(),
        }
    }

    /// Construct a display-only `ChangesetForm` with no CSRF token.
    ///
    /// Use this on GET handlers where CSRF middleware is not active, or when
    /// the form will be re-rendered purely for display (e.g. an initial blank
    /// form on a page that does not enforce CSRF).  [`form_tag`](Self::form_tag)
    /// will omit the hidden CSRF input when no token is stored.
    #[must_use]
    pub fn without_csrf(data: T) -> Self {
        Self {
            changeset: Changeset::new(data),
            csrf_token: None,
            csrf_field: "_csrf".to_owned(),
        }
    }

    /// Wrap a pre-built [`Changeset`] (which may already carry validation errors)
    /// in a `ChangesetForm` without a CSRF token.
    ///
    /// Useful in tests and cases where a `Changeset` was produced externally
    /// (e.g. via [`IntoChangeset`]) before constructing a form for rendering.
    #[must_use]
    pub fn from_changeset(changeset: Changeset<T>) -> Self {
        Self {
            changeset,
            csrf_token: None,
            csrf_field: "_csrf".to_owned(),
        }
    }

    /// Override the CSRF form-field name used by [`ChangesetForm::form_tag`].
    ///
    /// Call this when `security.csrf.form_field` is set to something other than
    /// `"_csrf"` (e.g. `"authenticity_token"`).  The `CsrfFormField` extension
    /// populated by [`from_request`](Self::from_request) sets this automatically
    /// for POST handlers; use this builder on GET handlers that construct a blank
    /// form with [`blank`](Self::blank).
    #[must_use]
    pub fn with_csrf_field(mut self, field: impl Into<String>) -> Self {
        self.csrf_field = field.into();
        self
    }

    /// The CSRF token captured from the request, if the CSRF middleware is active.
    pub fn csrf_token(&self) -> Option<&str> {
        self.csrf_token.as_deref()
    }

    /// Consume and return only the inner [`Changeset`].
    pub fn into_changeset(self) -> Changeset<T> {
        self.changeset
    }

    /// Return `Ok(T)` if the changeset is valid, `Err(self)` if not.
    ///
    /// The `Err` branch returns the whole `ChangesetForm` (with its CSRF
    /// token) so the handler can immediately call `form.form_tag()` to
    /// re-render with inline errors.
    ///
    /// # Errors
    ///
    /// Returns `Err(self)` when the inner changeset has field-level validation errors.
    pub fn into_valid(self) -> Result<T, Self> {
        if self.changeset.is_valid() {
            Ok(self.changeset.into_inner())
        } else {
            Err(self)
        }
    }
}

/// Dereferences to [`Changeset<T>`] so all changeset methods are available
/// directly on `ChangesetForm<T>` — `form.is_valid()`, `form.errors_for(…)`,
/// etc.
impl<T> std::ops::Deref for ChangesetForm<T> {
    type Target = Changeset<T>;
    fn deref(&self) -> &Self::Target {
        &self.changeset
    }
}

/// Maud rendering methods — emit form HTML with automatic CSRF injection.
#[cfg(feature = "maud")]
impl<T: Serialize> ChangesetForm<T> {
    /// Render a `<form>` element with the stored CSRF token injected as a
    /// hidden input — the field name honours `security.csrf.form_field` from
    /// config, so no developer action is required even for non-default names.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn form_tag(&self, action: &str, method: &str, content: maud::Markup) -> maud::Markup {
        form_tag_inner(
            action,
            method,
            &self.csrf_field,
            self.csrf_token.as_deref(),
            None,
            content,
        )
    }

    /// Render a labeled `<input type="text">` for `field` using the stored
    /// changeset (value + errors).
    pub fn text_input(&self, field: &str, label: &str) -> maud::Markup {
        text_input(&self.changeset, field, label)
    }

    /// Render a labeled `<input type="text">` with htmx inline-validation
    /// attributes for `field`.
    ///
    /// Delegates to [`text_input_htmx`]; see that function for full docs.
    pub fn text_input_htmx(&self, field: &str, label: &str, validate_url: &str) -> maud::Markup {
        text_input_htmx(&self.changeset, field, label, validate_url)
    }

    /// Render a labeled `<input type="text">` with htmx inline-validation
    /// attributes for `field`, excluding the configured submit-token field
    /// `token_field` from the validation POST.
    ///
    /// Delegates to [`text_input_htmx_with_token_field`]; use this when the app
    /// customizes `[security.submit_token].field_name` (issue #1843). Source the
    /// name from the [`SubmitFormField`](crate::security::SubmitFormField)
    /// extractor.
    pub fn text_input_htmx_with_token_field(
        &self,
        field: &str,
        label: &str,
        validate_url: &str,
        token_field: &str,
    ) -> maud::Markup {
        text_input_htmx_with_token_field(&self.changeset, field, label, validate_url, token_field)
    }

    /// Render a labeled Markdown editor for a rich-text `field` (issue #1255).
    ///
    /// Delegates to [`rich_text_area`]; see that function for the full contract,
    /// including how to render the stored value back out safely.
    pub fn rich_text_area(&self, field: &str, label: &str) -> maud::Markup {
        rich_text_area(&self.changeset, field, label)
    }

    /// Render a Markdown editor with an htmx-driven live preview pane.
    ///
    /// Delegates to [`rich_text_area_htmx`]; see that function for full docs.
    pub fn rich_text_area_htmx(&self, field: &str, label: &str, preview_url: &str) -> maud::Markup {
        rich_text_area_htmx(&self.changeset, field, label, preview_url)
    }

    /// Render a Markdown editor with an htmx live preview, excluding the
    /// configured submit-token field `token_field` from the preview POST.
    ///
    /// Delegates to [`rich_text_area_htmx_with_token_field`]; use this when the
    /// app customizes `[security.submit_token].field_name` (issue #1843).
    pub fn rich_text_area_htmx_with_token_field(
        &self,
        field: &str,
        label: &str,
        preview_url: &str,
        token_field: &str,
    ) -> maud::Markup {
        rich_text_area_htmx_with_token_field(
            &self.changeset,
            field,
            label,
            preview_url,
            token_field,
        )
    }

    /// Render a `<button type="submit">` with `label`.
    pub fn submit_button(&self, label: &str) -> maud::Markup {
        submit_button(label)
    }
}

impl<S, T> FromRequest<S> for ChangesetForm<T>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned + validator::Validate + Send,
{
    type Rejection = axum::response::Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        // Capture CSRF token and field name before the body is consumed.
        let csrf_token = req
            .extensions()
            .get::<crate::security::CsrfToken>()
            .map(|t| t.token().to_string());
        let csrf_field = req
            .extensions()
            .get::<crate::security::csrf::CsrfFormField>()
            .map_or_else(|| "_csrf".to_owned(), |f| f.0.clone());

        let data: T = decode_form_body(req, state).await?;

        Ok(Self {
            changeset: data.into_changeset(),
            csrf_token,
            csrf_field,
        })
    }
}

/// Decode a form body — URL-encoded always, multipart when that feature is on.
// `clippy::result_large_err` (armed by rustc 1.98) measures the `Err` variant at
// 128 bytes — that is `axum::response::Response`'s own size, not something this
// crate chose. Returning a ready-made rejection response IS the idiom here, and
// boxing it would add an allocation to every rejection path to satisfy a size
// heuristic. Allowed at the site rather than workspace-wide so the lint stays
// armed for error types we do control.
#[allow(clippy::result_large_err)]
async fn decode_form_body<T, S>(req: Request, state: &S) -> Result<T, axum::response::Response>
where
    T: serde::de::DeserializeOwned + validator::Validate + Send,
    S: Send + Sync,
{
    let content_type = req
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();

    #[cfg(feature = "multipart")]
    if content_type.starts_with("multipart/form-data") {
        return decode_multipart(req, state).await;
    }

    // Same content-type gate axum's own `Form`/`RawForm` extractors apply: a
    // POST whose Content-Type isn't (or doesn't start with) the form-urlencoded
    // mime type is rejected outright, rather than being decoded anyway. `Bytes`
    // alone has no opinion on Content-Type, so this must be checked explicitly
    // now that we no longer route through `axum::extract::Form`.
    if !content_type.starts_with("application/x-www-form-urlencoded") {
        return Err((
            axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Form requests must have `Content-Type: application/x-www-form-urlencoded`",
        )
            .into_response());
    }

    // Buffer the body through axum's own `Bytes` extractor so the configured
    // `DefaultBodyLimit` is enforced exactly as it would be for a single-shot
    // `Form` extraction (a bare `to_bytes(.., usize::MAX)` would accept an
    // unbounded body and defeat that protection).
    let (parts, body) = req.into_parts();
    let bytes_req = Request::from_parts(parts, body);
    let bytes = axum::body::Bytes::from_request(bytes_req, state)
        .await
        .map_err(IntoResponse::into_response)?;

    decode_urlencoded_dropping_blank_optional_fields::<T>(&bytes)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()).into_response())
}

/// Deserialize `T` from `application/x-www-form-urlencoded` bytes, tolerating
/// blank values for non-string `Option<T>` fields (a number/date/uuid input
/// the user left empty submits `field=`, which `serde_urlencoded` otherwise
/// rejects outright — an empty string is not a valid `i32`/`Uuid`/etc., so it
/// never gets the chance to become "missing" and deserialize as `None`).
///
/// [`serde_path_to_error`] pinpoints exactly which field a decode failure
/// came from; if that field's submitted value is blank, drop just that one
/// pair and retry. This converges within (number of blank fields) iterations
/// and never touches a field that decoded successfully — critically, it
/// leaves a blank *required* `String` field's pair alone (an empty string
/// already deserializes fine for `String`, so that field never appears as
/// the error path), letting it flow into the `Changeset` as a real
/// validation error instead of being dropped into a spurious "missing
/// field" decode failure. A genuinely malformed (non-blank) value still
/// fails immediately, matching the "undecodable body is a hard 400" contract.
///
/// # Scope: any field that already tolerates a missing key
///
/// This helper can only observe "decoding this field's blank value failed,
/// and removing the key fixes it" — it cannot see the target type, so it
/// cannot distinguish `Option<T>` from a required `#[serde(default)]` field
/// (e.g. [`checkbox_input`]'s documented `#[serde(default)] published: bool`
/// convention for an unchecked box). Dropping the key makes both resolve to
/// whatever that field already treats a *missing* key as — `None` or the
/// `#[serde(default)]` value respectively. This is intentionally consistent
/// rather than a leak: a field with no such tolerance (no `Option`, no
/// `#[serde(default)]`) still hard-fails, because removing its key surfaces
/// a *missing field* error next iteration instead of resolving — see
/// `blank_required_field_without_default_still_fails_to_decode` for the
/// guarantee. And nothing new is reachable through this leniency: a client
/// that wants a `#[serde(default)]` field to take its default value could
/// already get that by omitting the key entirely.
pub(crate) fn decode_urlencoded_dropping_blank_optional_fields<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, serde_path_to_error::Error<serde_urlencoded::de::Error>> {
    let mut pairs: Vec<(String, String)> = url::form_urlencoded::parse(bytes)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    loop {
        let encoded = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .finish();
        let deserializer =
            serde_urlencoded::Deserializer::new(url::form_urlencoded::parse(encoded.as_bytes()));
        match serde_path_to_error::deserialize(deserializer) {
            Ok(data) => return Ok(data),
            Err(err) => {
                let field = err.path().to_string();
                let Some(pos) = pairs.iter().position(|(k, v)| *k == field && v.is_empty()) else {
                    return Err(err);
                };
                pairs.remove(pos);
            }
        }
    }
}

/// Fuzzing seam: run the `application/x-www-form-urlencoded` body decoder
/// (including the blank-optional-field retry loop) over arbitrary bytes.
/// Deserializes into a permissive `HashMap` so any well-formed pair shape is
/// accepted and only panics (not decode errors) are of interest.
///
/// Compiled only under `--cfg fuzzing`; the published crate is unaffected.
/// See `fuzz/fuzz_targets/body.rs`.
#[cfg(fuzzing)]
pub fn __fuzz_decode_urlencoded(bytes: &[u8]) {
    let _ = decode_urlencoded_dropping_blank_optional_fields::<
        std::collections::HashMap<String, String>,
    >(bytes);
}

/// Decode `multipart/form-data` text fields and deserialize into `T`.
///
/// File-upload fields are skipped (file storage is out of scope here).
/// The collected text pairs are re-encoded as URL-encoded so that
/// `serde_urlencoded` handles the same type coercions axum's `Form` does.
#[cfg(feature = "multipart")]
// Same as `decode_form_body` above: the `Err` variant is an
// `axum::response::Response`, which IS the rejection, and boxing it would cost
// an allocation on every rejection to satisfy a size heuristic.
#[allow(clippy::result_large_err)]
async fn decode_multipart<T, S>(req: Request, state: &S) -> Result<T, axum::response::Response>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    let mut multipart = axum::extract::Multipart::from_request(req, state)
        .await
        .map_err(IntoResponse::into_response)?;

    let mut pairs: Vec<(String, String)> = Vec::new();

    loop {
        let field = multipart
            .next_field()
            .await
            .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()).into_response())?;

        let Some(field) = field else { break };

        let name = match field.name() {
            Some(n) => n.to_string(),
            None => continue,
        };

        // Skip file-upload fields; text-only decoding is in scope.
        if field.file_name().is_some() {
            continue;
        }

        let value = field
            .text()
            .await
            .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()).into_response())?;

        pairs.push((name, value));
    }

    // Re-encode as URL-encoded so serde_urlencoded handles type coercions
    // ("30" → u32, "true" → bool, etc.) consistently with the Form extractor.
    let encoded = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .finish();

    match serde_urlencoded::from_str::<T>(&encoded) {
        Ok(data) => Ok(data),
        Err(first_err) => {
            // Same blank-optional-field accommodation as `decode_form_body`:
            // a number/date/uuid field left empty submits an empty text
            // value, which `serde_urlencoded` rejects for non-string
            // `Option<T>` fields — retry with those fields dropped entirely
            // so they deserialize as `None`.
            let (blank, non_blank): (Vec<_>, Vec<_>) =
                pairs.iter().partition(|(_, v)| v.is_empty());
            if blank.is_empty() {
                return Err(
                    (axum::http::StatusCode::BAD_REQUEST, first_err.to_string()).into_response()
                );
            }
            let filtered = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(non_blank.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .finish();
            serde_urlencoded::from_str::<T>(&filtered).map_err(|_| {
                (axum::http::StatusCode::BAD_REQUEST, first_err.to_string()).into_response()
            })
        }
    }
}

// ── Internal helpers ───────────────────────────────────────────────

pub(crate) fn validation_errors_to_map(
    errors: &validator::ValidationErrors,
) -> HashMap<String, Vec<String>> {
    let mut map = HashMap::new();
    collect_errors(errors, "", &mut map);
    map
}

fn collect_errors(
    errors: &validator::ValidationErrors,
    prefix: &str,
    map: &mut HashMap<String, Vec<String>>,
) {
    for (field, kind) in errors.errors() {
        let key = if prefix.is_empty() {
            (*field).to_string()
        } else {
            format!("{prefix}.{field}")
        };
        match kind {
            validator::ValidationErrorsKind::Field(errs) => {
                let messages: Vec<String> = errs
                    .iter()
                    .map(|e| {
                        e.message.as_ref().map_or_else(
                            || format!("validation failed: {}", e.code),
                            ToString::to_string,
                        )
                    })
                    .collect();
                map.entry(key).or_default().extend(messages);
            }
            validator::ValidationErrorsKind::Struct(nested) => {
                collect_errors(nested, &key, map);
            }
            validator::ValidationErrorsKind::List(list) => {
                for (idx, nested) in list {
                    let indexed_key = format!("{key}[{idx}]");
                    collect_errors(nested, &indexed_key, map);
                }
            }
        }
    }
}

// ── Standalone Maud helpers ─────────────────────────────────────────
//
// These are the building blocks used by `ChangesetForm` methods.
// They are also public so GET handlers can use them with a bare `Changeset`.

/// Render a `<form>` element wrapping `content`.
///
/// When `csrf_token` is `Some(token)`, a hidden `<input name="_csrf">` is
/// emitted automatically — compatible with [`crate::security::CsrfLayer`]
/// using the default field name `_csrf`.
///
/// In **POST** handlers, prefer [`ChangesetForm::form_tag`] which injects
/// the token **and** honours any custom `security.csrf.form_field` from config.
#[cfg(feature = "maud")]
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn form_tag(
    action: &str,
    method: &str,
    csrf_token: Option<&str>,
    content: maud::Markup,
) -> maud::Markup {
    form_tag_inner(action, method, "_csrf", csrf_token, None, content)
}

/// Internal: render a `<form>` element using an explicit CSRF field name.
///
/// When `method` is `PUT`, `PATCH`, or `DELETE` (case-insensitive), the
/// browser-facing form method is rewritten to `POST` and a hidden
/// `<input name="_method" value="...">` is emitted so the autumn
/// [`MethodOverrideLayer`](crate::middleware::MethodOverrideLayer) can
/// rewrite the request back to the declared method before route matching.
/// This lets server-rendered HTML target `#[put]` / `#[patch]` /
/// `#[delete]` routes without any client JavaScript.
///
/// `enctype` is emitted verbatim when `Some` (e.g.
/// `Some("multipart/form-data")` for [`FormFor`] forms containing a file
/// input) so every `<form>` — whatever its encoding — flows through this one
/// audited CSRF/method-override path.
#[cfg(feature = "maud")]
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn form_tag_inner(
    action: &str,
    method: &str,
    csrf_field: &str,
    csrf_token: Option<&str>,
    enctype: Option<&str>,
    content: maud::Markup,
) -> maud::Markup {
    let (browser_method, override_value) = browser_method_and_override(method);
    maud::html! {
        form action=(action) method=(browser_method) enctype=[enctype] {
            @if let Some(override_method) = override_value {
                input
                    type="hidden"
                    name=(crate::middleware::DEFAULT_METHOD_OVERRIDE_FIELD)
                    value=(override_method);
            }
            @if let Some(token) = csrf_token {
                input type="hidden" name=(csrf_field) value=(token);
            }
            (content)
        }
    }
}

/// Translate a declared form method into the browser transport method and
/// any required `_method` override value.
///
/// Returns `(transport, override)` where `override` is `Some(value)` only
/// when the declared method needs a hidden `_method` field.
#[cfg(feature = "maud")]
fn browser_method_and_override(method: &str) -> (&'static str, Option<&'static str>) {
    let trimmed = method.trim();
    if trimmed.eq_ignore_ascii_case("PUT") {
        ("post", Some("PUT"))
    } else if trimmed.eq_ignore_ascii_case("PATCH") {
        ("post", Some("PATCH"))
    } else if trimmed.eq_ignore_ascii_case("DELETE") {
        ("post", Some("DELETE"))
    } else if trimmed.eq_ignore_ascii_case("GET") {
        ("get", None)
    } else {
        ("post", None)
    }
}

/// Render a hidden `<input name="_method" value="...">` field for the
/// declared HTTP method.
///
/// Use this directly when constructing a form by hand (without
/// [`ChangesetForm`] or [`form_tag`]) targeting a `#[put]`, `#[patch]`,
/// or `#[delete]` route from a plain HTML browser submission.
///
/// ```rust,ignore
/// use autumn_web::form::method_input;
///
/// maud::html! {
///     form method="post" action="/posts/42" {
///         (method_input("DELETE"))
///         button { "Delete post" }
///     }
/// }
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn method_input(method: &str) -> maud::Markup {
    let normalized = method.trim();
    let value = if normalized.eq_ignore_ascii_case("PUT") {
        "PUT"
    } else if normalized.eq_ignore_ascii_case("PATCH") {
        "PATCH"
    } else if normalized.eq_ignore_ascii_case("DELETE") {
        "DELETE"
    } else {
        // `GET`/`POST` (and anything else) don't need an override — emit
        // nothing rather than producing an invalid override field.
        return maud::html! {};
    };
    maud::html! {
        input
            type="hidden"
            name=(crate::middleware::DEFAULT_METHOD_OVERRIDE_FIELD)
            value=(value);
    }
}

/// HTML-escapes `input` (the same 4 characters `maud::escape::escape_to_string`
/// does: `&`, `<`, `>`, `"`) by appending to `out`.
///
/// `maud::escape::escape_to_string` scans one byte at a time and calls
/// `String::push`/`push_str` per byte even when nothing needs escaping —
/// on a scaffolded form's field values (mostly clean prose/numbers, no
/// `&<>"`) that per-byte dispatch dominated `benches/form_render.rs`'s
/// profile. This does the identical escape (output verified byte-for-byte
/// against a naive reference by the `fast_escape_matches_naive_reference`
/// proptest) but copies each clean run in one `push_str` instead of one
/// call per byte. Slicing at the positions found below is always on a
/// UTF-8 char boundary: `&`, `<`, `>`, and `"` are single-byte ASCII code
/// points, so they can never be a continuation byte of a multi-byte
/// sequence, and no multi-byte sequence can contain one of those bytes
/// either.
#[cfg(feature = "maud")]
#[allow(
    clippy::string_slice,
    reason = "every slice index below comes from a position() match on a single-byte ASCII \
              value (&, <, >, \"), which can never land inside a multi-byte UTF-8 sequence — \
              always a char boundary. Proven for arbitrary input by the \
              fast_escape_matches_naive_reference proptest."
)]
#[allow(
    clippy::arithmetic_side_effects,
    reason = "`i + 1` where `i` is a byte index already `< input.len()` (from `bytes.iter().\
              enumerate()`), and `input.len() <= isize::MAX` is a `str` invariant — cannot \
              overflow `usize`."
)]
fn write_escaped(input: &str, out: &mut String) {
    let bytes = input.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let replacement = match b {
            b'&' => "&amp;",
            b'<' => "&lt;",
            b'>' => "&gt;",
            b'"' => "&quot;",
            _ => continue,
        };
        out.push_str(&input[start..i]);
        out.push_str(replacement);
        start = i + 1;
    }
    out.push_str(&input[start..]);
}

/// A field value, label, or error message, escaped for direct
/// interpolation into a `maud::html!` buffer.
///
/// Implements [`maud::Render`] itself — rather than pre-building an owned,
/// escaped `String` and handing it to `maud::PreEscaped` — so the escaped
/// bytes are written straight into `html!`'s own growing output buffer.
/// Building a separate `String` first (an earlier version of this code
/// did that, via a `Cow`-returning `fast_escape`) means a *second* copy
/// for any value that actually needs escaping: once into the temporary
/// `String`, then again when `PreEscaped` hands it off. This way there is
/// only ever the one copy `maud::escape::escape_to_string` was already
/// doing — a `Render` impl is documented as receiving "no further
/// escaping" from `html!`, so this fully replaces it rather than skipping
/// it.
///
/// Named `esc`, not `Escaped`/`escape`: `maud_macros` sizes a `html!`
/// block's output buffer from the *source token length* of the macro
/// invocation, not runtime content, so a longer call spelled out at every
/// interpolation site measurably over-reserves that buffer's initial
/// capacity even though it's never actually grown again. A short call
/// keeps the estimate close to what the plain, unescaped `(value)` it
/// replaces already cost.
#[cfg(feature = "maud")]
struct Esc<'a>(&'a str);

#[cfg(feature = "maud")]
impl maud::Render for Esc<'_> {
    fn render_to(&self, w: &mut String) {
        write_escaped(self.0, w);
    }
}

#[cfg(feature = "maud")]
const fn esc(s: &str) -> Esc<'_> {
    Esc(s)
}

/// Pre-escape a field name the same way `maud::html!` would, for building
/// an `id`/`name`-derived string (`format!("{field_html}-error")`) ahead
/// of a `maud::html!` block rather than inside one.
///
/// Unlike [`esc`]/[`Esc`], this has to produce an owned-or-borrowed `str`
/// rather than write into a buffer, since the result gets concatenated via
/// `format!` before it's ever interpolated — so it keeps the `Cow`
/// interface and, when there's nothing to escape (the overwhelmingly
/// common case for a field name), returns the input completely
/// unallocated. Escaping first and concatenating after is equivalent to
/// concatenating first and escaping after here, since the literal
/// `"-error"`/`"-field"` suffixes contain no characters that need
/// escaping — see `fast_escape_matches_naive_reference` for the
/// byte-for-byte equivalence check against a naive reference.
#[cfg(feature = "maud")]
fn fast_escape(input: &str) -> std::borrow::Cow<'_, str> {
    if !input
        .as_bytes()
        .iter()
        .any(|b| matches!(b, b'&' | b'<' | b'>' | b'"'))
    {
        return std::borrow::Cow::Borrowed(input);
    }
    let mut out = String::with_capacity(input.len());
    write_escaped(input, &mut out);
    std::borrow::Cow::Owned(out)
}

/// Build `"{base}{suffix}"` by direct, exactly-sized concatenation instead of
/// `format!`. `format!("{base}{suffix}")` drives an unsized `String::new()`
/// through the `Arguments`/`fmt::Write` machinery — arguments are boxed into
/// a `[ArgumentV1]`, each piece is dispatched through `Display::fmt`, and the
/// output `String` grows via reallocation as pieces land rather than being
/// sized up front. For the fixed two-part `-field`/`-error` id suffixes every
/// scaffolded input helper builds once per render, that dispatch is pure
/// overhead: both parts are already `&str`, so one `String::with_capacity`
/// plus two `push_str` calls produces the identical bytes.
#[cfg(feature = "maud")]
fn concat_suffix(base: &str, suffix: &str) -> String {
    let mut s = String::with_capacity(base.len().saturating_add(suffix.len()));
    s.push_str(base);
    s.push_str(suffix);
    s
}

/// Render a labeled `<input type="text">` tied to a changeset field.
///
/// - Sets `name` and `id` to `field`
/// - Wraps in a `<div id="{field}-field">` for stable htmx targeting
/// - Populates `value` from the changeset's serialized data
/// - Adds `aria-invalid="true"` + `aria-describedby` when errors exist
/// - Emits a `<div role="alert">` with per-message `<p>` error elements
///
/// Use [`text_input_htmx`] to add htmx inline-validation attributes.
#[cfg(feature = "maud")]
#[must_use]
pub fn text_input<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
) -> maud::Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let value = changeset.field_value(field).unwrap_or_default();
    let field_html = fast_escape(field);
    let error_id = has_errors.then(|| concat_suffix(&field_html, "-error"));
    let wrapper_id = concat_suffix(&field_html, "-field");
    let field_pe = maud::PreEscaped(field_html.as_ref());
    let wrapper_pe = maud::PreEscaped(&wrapper_id);
    let aria_pe = maud::PreEscaped(error_id.as_deref().unwrap_or(""));

    maud::html! {
        div id=(wrapper_pe) class="autumn-field" {
            label for=(field_pe) class="autumn-field__label" { (esc(label)) }
            input
                type="text"
                id=(field_pe)
                name=(field_pe)
                value=(esc(&value))
                class=(if has_errors { maud::PreEscaped("autumn-field__input autumn-field__input--invalid") } else { maud::PreEscaped("autumn-field__input") })
                aria-invalid=(if has_errors { maud::PreEscaped("true") } else { maud::PreEscaped("false") })
                aria-describedby=(aria_pe);
            @if has_errors {
                div id=(maud::PreEscaped(error_id.as_deref().unwrap_or_default())) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (esc(error)) }
                    }
                }
            }
        }
    }
}

/// Render a labeled `<input type="text">` with htmx inline-validation attributes.
///
/// Like [`text_input`] but adds `hx-post`, `hx-trigger="change"`,
/// `hx-target="closest [data-autumn-field-wrapper]"`, `hx-swap="outerHTML"`,
/// `hx-include="closest form"`, and `hx-params="not _submit_token"` to the input
/// element so htmx POSTs the whole form to `validate_url` after a changed value
/// is committed and swaps the returned field wrapper in place — no JavaScript
/// required.
///
/// The `hx-params="not _submit_token"` filter drops the hidden one-time
/// submit-token field (issue #1360) from the inline-validation POST: `SubmitTokenLayer`
/// consumes any `_submit_token` a mutating POST carries, so without this filter
/// the first field validation would spend the token and the real create/update
/// submit would replay the validation fragment instead of running the mutation.
///
/// # Custom `field_name` limitation
///
/// This helper hardcodes the **default** submit-token field name
/// `_submit_token` in its `hx-params` filter, so it excludes only that default
/// from inline htmx validation posts. If you customize
/// `[security.submit_token].field_name` **and** hand-write a live-validation
/// form using these helpers, the filter will not exclude your custom field, so
/// the token leaks into the validation POST and `SubmitTokenLayer` consumes it.
///
/// The proper resolution is [`text_input_htmx_with_token_field`], which takes
/// the configured field name and filters `not <field_name>` instead. Source
/// the name at request time from the [`SubmitFormField`](crate::security::SubmitFormField)
/// extractor. Alternatively, add your validation route(s) to
/// `[security.submit_token].exempt_paths` so the one-time token is not consumed
/// by validation requests. Generated scaffolds keep the default field name and
/// wire their validation routes into `exempt_paths` automatically, so this only
/// affects hand-written forms with a customized `field_name`.
///
/// The inline-validation handler should extract [`ChangesetForm<T>`],
/// validate, and return `text_input_htmx(...)` for just the single field.
///
/// # Example
///
/// ```rust,ignore
/// // Render:
/// form.text_input_htmx("email", "Email", "/users/validate/email")
///
/// // Inline-validation handler:
/// #[post("/users/validate/email")]
/// async fn validate_email(form: ChangesetForm<UserForm>) -> Markup {
///     text_input_htmx(&form.changeset, "email", "Email", "/users/validate/email")
/// }
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn text_input_htmx<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    validate_url: &str,
) -> maud::Markup {
    text_input_htmx_with_token_field(
        changeset,
        field,
        label,
        validate_url,
        DEFAULT_SUBMIT_TOKEN_FIELD,
    )
}

/// The default `[security.submit_token].field_name`, matching
/// `default_submit_token_field()` in [`crate::security`]. Used by
/// [`text_input_htmx`] / [`required_text_input_htmx`] to preserve their original
/// hardcoded `hx-params="not _submit_token"` behaviour when delegating to the
/// `*_with_token_field` variants.
#[cfg(feature = "maud")]
const DEFAULT_SUBMIT_TOKEN_FIELD: &str = "_submit_token";

/// Like [`text_input_htmx`] but excludes the caller-supplied submit-token field
/// name (`token_field`) from the inline-validation POST instead of the hardcoded
/// default `_submit_token`.
///
/// Use this when the app customizes `[security.submit_token].field_name` and
/// hand-writes a live-validation form: pass the configured field name so the
/// emitted `hx-params="not <token_field>"` filter drops the actual one-time
/// submit-token field. Without this, the token leaks into the validation POST,
/// `SubmitTokenLayer` consumes it, and the real create/update submit replays the
/// validation fragment instead of running the mutation (issue #1843).
///
/// Callers obtain the configured field name at request time from the
/// [`SubmitFormField`](crate::security::SubmitFormField) extractor. Passing
/// `"_submit_token"` here is equivalent to calling [`text_input_htmx`].
///
/// # Example
///
/// ```rust,ignore
/// async fn validate_email(
///     form: ChangesetForm<UserForm>,
///     SubmitFormField(token_field): SubmitFormField,
/// ) -> Markup {
///     text_input_htmx_with_token_field(
///         &form.changeset, "email", "Email", "/users/validate/email", &token_field,
///     )
/// }
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn text_input_htmx_with_token_field<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    validate_url: &str,
    token_field: &str,
) -> maud::Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let value = changeset.field_value(field).unwrap_or_default();
    let error_id = has_errors.then(|| format!("{field}-error"));
    let wrapper_id = format!("{field}-field");
    let target = "closest [data-autumn-field-wrapper]";
    let hx_params = format!("not {token_field}");

    maud::html! {
        div id=(wrapper_id) class="autumn-field" data-autumn-field-wrapper=(field) {
            label for=(field) class="autumn-field__label" { (label) }
            input
                type="text"
                id=(field)
                name=(field)
                value=(value)
                class=(if has_errors { maud::PreEscaped("autumn-field__input autumn-field__input--invalid") } else { maud::PreEscaped("autumn-field__input") })
                aria-invalid=(if has_errors { maud::PreEscaped("true") } else { maud::PreEscaped("false") })
                aria-describedby=(error_id.as_deref().unwrap_or(""))
                hx-post=(validate_url)
                hx-trigger="change"
                hx-target=(target)
                hx-swap="outerHTML"
                hx-include="closest form"
                hx-params=(hx_params);
            @if has_errors {
                div id=(error_id.as_deref().unwrap_or_default()) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (error) }
                    }
                }
            }
        }
    }
}

/// Like [`text_input_htmx`] but for a required field: adds `required` and
/// `aria-required="true"` on the `<input>`, exactly like
/// [`required_text_input`] does for the non-htmx variant.
///
/// Without this, a required field wired up with `text_input_htmx` has no
/// client-side "must fill this in" signal, and a server-side rule that
/// happens to permit an empty string (e.g. a max-only `length` rule) would
/// let a blank required field through silently.
///
/// Like [`text_input_htmx`], this helper hardcodes the **default** submit-token
/// field name in its `hx-params="not _submit_token"` filter. See
/// [`text_input_htmx`](text_input_htmx#custom-field_name-limitation) for the
/// custom `[security.submit_token].field_name` caveat. When you customize the
/// field name, use [`required_text_input_htmx_with_token_field`] (the proper
/// resolution) or add the validation route(s) to
/// `[security.submit_token].exempt_paths` (the alternative) for hand-written
/// live-validation forms.
#[cfg(feature = "maud")]
#[must_use]
pub fn required_text_input_htmx<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    validate_url: &str,
) -> maud::Markup {
    required_text_input_htmx_with_token_field(
        changeset,
        field,
        label,
        validate_url,
        DEFAULT_SUBMIT_TOKEN_FIELD,
    )
}

/// Like [`required_text_input_htmx`] but excludes the caller-supplied
/// submit-token field name (`token_field`) from the inline-validation POST
/// instead of the hardcoded default `_submit_token`.
///
/// This is the required-field counterpart of
/// [`text_input_htmx_with_token_field`]; see that function for the full
/// rationale (issue #1843) and how to source the configured field name from the
/// [`SubmitFormField`](crate::security::SubmitFormField) extractor. Passing
/// `"_submit_token"` here is equivalent to calling [`required_text_input_htmx`].
#[cfg(feature = "maud")]
#[must_use]
pub fn required_text_input_htmx_with_token_field<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    validate_url: &str,
    token_field: &str,
) -> maud::Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let value = changeset.field_value(field).unwrap_or_default();
    let error_id = has_errors.then(|| format!("{field}-error"));
    let wrapper_id = format!("{field}-field");
    let target = "closest [data-autumn-field-wrapper]";
    let hx_params = format!("not {token_field}");

    maud::html! {
        div id=(wrapper_id) class="autumn-field" data-autumn-field-wrapper=(field) {
            label for=(field) class="autumn-field__label" { (label) }
            input
                type="text"
                id=(field)
                name=(field)
                value=(value)
                required
                aria-required="true"
                class=(if has_errors { maud::PreEscaped("autumn-field__input autumn-field__input--invalid") } else { maud::PreEscaped("autumn-field__input") })
                aria-invalid=(if has_errors { maud::PreEscaped("true") } else { maud::PreEscaped("false") })
                aria-describedby=(error_id.as_deref().unwrap_or(""))
                hx-post=(validate_url)
                hx-trigger="change"
                hx-target=(target)
                hx-swap="outerHTML"
                hx-include="closest form"
                hx-params=(hx_params);
            @if has_errors {
                div id=(error_id.as_deref().unwrap_or_default()) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (error) }
                    }
                }
            }
        }
    }
}

/// Render a `<button type="submit">` with `label`.
#[cfg(feature = "maud")]
#[must_use]
pub fn submit_button(label: &str) -> maud::Markup {
    maud::html! {
        button type="submit" class="autumn-submit" { (label) }
    }
}

/// Render a labeled `<input type="password">` tied to a changeset field.
///
/// Like [`text_input`] but uses `type="password"` and never populates the
/// `value` attribute — browsers must not auto-fill passwords into the markup
/// and screen readers must not announce the value.
///
/// Wraps in `<div id="{field}-field">` for stable htmx targeting.
/// ARIA annotations (`aria-invalid`, `aria-describedby`, error block) behave
/// identically to [`text_input`].
#[cfg(feature = "maud")]
#[must_use]
pub fn password_input<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
) -> maud::Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let field_html = fast_escape(field);
    let error_id = has_errors.then(|| concat_suffix(&field_html, "-error"));
    let wrapper_id = concat_suffix(&field_html, "-field");
    let field_pe = maud::PreEscaped(field_html.as_ref());
    let wrapper_pe = maud::PreEscaped(&wrapper_id);
    let aria_pe = maud::PreEscaped(error_id.as_deref().unwrap_or(""));

    maud::html! {
        div id=(wrapper_pe) class="autumn-field" {
            label for=(field_pe) class="autumn-field__label" { (esc(label)) }
            input
                type="password"
                id=(field_pe)
                name=(field_pe)
                class=(if has_errors { maud::PreEscaped("autumn-field__input autumn-field__input--invalid") } else { maud::PreEscaped("autumn-field__input") })
                aria-invalid=(if has_errors { maud::PreEscaped("true") } else { maud::PreEscaped("false") })
                aria-describedby=(aria_pe);
            @if has_errors {
                div id=(maud::PreEscaped(error_id.as_deref().unwrap_or_default())) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (esc(error)) }
                    }
                }
            }
        }
    }
}

/// Render a labeled `<textarea>` tied to a changeset field.
///
/// The current field value is emitted as the textarea body (not a `value`
/// attribute). Wraps in `<div id="{field}-field">` for stable htmx targeting.
/// ARIA annotations behave identically to [`text_input`].
#[cfg(feature = "maud")]
#[must_use]
pub fn textarea_input<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
) -> maud::Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let value = changeset.field_value(field).unwrap_or_default();
    let field_html = fast_escape(field);
    let error_id = has_errors.then(|| concat_suffix(&field_html, "-error"));
    let wrapper_id = concat_suffix(&field_html, "-field");
    let field_pe = maud::PreEscaped(field_html.as_ref());
    let wrapper_pe = maud::PreEscaped(&wrapper_id);
    let aria_pe = maud::PreEscaped(error_id.as_deref().unwrap_or(""));

    maud::html! {
        div id=(wrapper_pe) class="autumn-field" {
            label for=(field_pe) class="autumn-field__label" { (esc(label)) }
            textarea
                id=(field_pe)
                name=(field_pe)
                class=(if has_errors { maud::PreEscaped("autumn-field__input autumn-field__input--invalid") } else { maud::PreEscaped("autumn-field__input") })
                aria-invalid=(if has_errors { maud::PreEscaped("true") } else { maud::PreEscaped("false") })
                aria-describedby=(aria_pe)
                { (esc(&value)) }
            @if has_errors {
                div id=(maud::PreEscaped(error_id.as_deref().unwrap_or_default())) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (esc(error)) }
                    }
                }
            }
        }
    }
}

/// Extract one field's value from a URL-encoded form body, without
/// deserializing the whole form.
///
/// # Why this exists
///
/// A fragment endpoint that only needs *one* field — a rich-text live preview,
/// say — is usually reached from an htmx `hx-include="closest form"`, which
/// posts **every** field on the page. Deserializing that into the form struct
/// to read one value couples the fragment to the validity of every *other*
/// field: on a freshly-opened "new" form, a required `i64` column arrives as
/// `count=`, `serde_urlencoded` fails on the empty string, and the fragment
/// renders nothing. The author would have to fill in every unrelated column
/// before the preview started working (PR #2157 review).
///
/// Reading the one field leniently avoids that entirely. It is the right tool
/// *only* when the endpoint genuinely needs a single raw value and performs no
/// validation — inline **validation** fragments still decode the whole form,
/// because checking a field against its `#[validate]` rules is exactly what
/// they are for.
///
/// Percent-escapes and `+`-as-space are decoded. The first occurrence of a
/// repeated key wins, matching `serde_urlencoded`. A present-but-empty field
/// returns `Some("")`, not `None`, so a cleared editor previews as empty
/// rather than falling back to a stale value.
///
/// # Example
///
/// ```
/// use autumn_web::form::field_from_urlencoded;
///
/// let body = b"title=Hi&body=a+%2B+b&count=";
/// assert_eq!(field_from_urlencoded(body, "body").as_deref(), Some("a + b"));
/// assert_eq!(field_from_urlencoded(body, "missing"), None);
/// ```
#[must_use]
pub fn field_from_urlencoded(body: &[u8], field: &str) -> Option<String> {
    url::form_urlencoded::parse(body)
        .find(|(key, _)| key.as_ref() == field)
        .map(|(_, value)| value.into_owned())
}

/// The hint shown under a [`rich_text_area`] editor. Deliberately short: the
/// syntax itself lives in the toolbar ([`RICH_TEXT_TOOLBAR`]), so this sentence
/// only carries the part the toolbar can't — that HTML is *not* accepted.
#[cfg(feature = "maud")]
const RICH_TEXT_HINT: &str = "Markdown supported. HTML is not allowed and is shown as plain text.";

/// The minimal Markdown toolbar rendered above a [`rich_text_area`] editor, as
/// `(label, syntax)` pairs.
///
/// This is a **no-JavaScript** toolbar: it shows the syntax for each supported
/// construct at the point of use rather than inserting it on click. That is a
/// deliberate trade-off. Inserting text into a `<textarea>` is impossible in
/// HTML alone, so a click-to-insert toolbar would mean shipping a script — and a
/// scripted control that silently does nothing when scripting is off is worse
/// than no control at all. The editor's contract is that it works with no
/// JavaScript (issue #1255), so the toolbar holds to the same bar.
///
/// Every entry corresponds to something the renderer actually emits (see
/// [`RICH_TEXT_ALLOWED_TAGS`](crate::markdown::RICH_TEXT_ALLOWED_TAGS)) — there
/// is no affordance here for syntax the sanitizer would strip.
#[cfg(feature = "maud")]
const RICH_TEXT_TOOLBAR: &[(&str, &str)] = &[
    ("Bold", "**bold**"),
    ("Italic", "_italic_"),
    ("Link", "[text](url)"),
    ("Code", "`code`"),
    ("List", "- item"),
    ("Heading", "# Heading"),
    ("Quote", "> quote"),
];

/// Render a labeled Markdown editor for a rich-text field (issue #1255).
///
/// The control is a plain `<textarea>` carrying the Markdown **source** — the
/// column stores the source, never rendered HTML — plus a hint naming the
/// supported syntax. It needs no JavaScript and degrades to an ordinary
/// textarea everywhere.
///
/// Render the stored value back out with
/// [`markdown::render_user_content`](crate::markdown::render_user_content),
/// which disables raw-HTML passthrough and applies an allowlist sanitizer.
/// Rendering it any other way — `PreEscaped(&post.body)`, or the trusted-content
/// [`markdown::render`](crate::markdown::render) — reintroduces stored XSS.
///
/// Use [`rich_text_area_htmx`] to add a live preview pane.
///
/// - Sets `name` and `id` to `field`
/// - Wraps in a `<div id="{field}-field">` for stable htmx targeting
/// - Populates the textarea body from the changeset's serialized data
/// - Points `aria-describedby` at the hint, plus the error region when errors exist
#[cfg(feature = "maud")]
#[must_use]
pub fn rich_text_area<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
) -> maud::Markup {
    rich_text_area_inner(changeset, field, label, None, false)
}

/// Render a [`rich_text_area`] with an htmx-driven live preview pane.
///
/// The textarea POSTs the form to `preview_url` a short moment after typing
/// stops and swaps the response into `#{field}-preview` — **no JavaScript of
/// your own**, just htmx attributes. Without htmx (or with scripting off) the
/// control still works: it is the same textarea, and the pane simply shows the
/// server-rendered preview of the value the page was rendered with.
///
/// As with [`text_input_htmx`], `hx-params` excludes the default one-time
/// submit-token field so a preview request cannot spend the token the real
/// submit needs; use [`rich_text_area_htmx_with_token_field`] when the app
/// customizes `[security.submit_token].field_name`.
///
/// The preview handler should decode the body **leniently** and return
/// [`markdown::render_user_content`](crate::markdown::render_user_content) of
/// the submitted field — this is exactly what `autumn generate scaffold
/// … body:richtext` emits:
///
/// ```rust,ignore
/// #[post("/posts/preview/body")]
/// pub async fn preview_body(body: Bytes) -> Markup {
///     let source = autumn_web::form::field_from_urlencoded(&body, "body")
///         .unwrap_or_default();
///     autumn_web::markdown::render_user_content(&source)
/// }
/// ```
///
/// Note it reads the one field it needs with [`field_from_urlencoded`] rather
/// than deserializing the whole form: `hx-include` posts **every** field, so on
/// a freshly-opened "new" page an unrelated required `i64` column arrives as
/// `count=`, and a whole-form decode would fail on that empty string and blank
/// the preview until the author had filled in every other column.
///
/// For the same reason, do **not** use the [`ChangesetForm<T>`] extractor here:
/// it rejects a body it cannot deserialize with a 4xx, htmx does not swap a
/// 4xx, and the preview would silently stop updating mid-edit. Reading the one
/// field has no failure mode — an absent or empty field previews as empty,
/// which is exactly right for a cleared editor.
#[cfg(feature = "maud")]
#[must_use]
pub fn rich_text_area_htmx<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    preview_url: &str,
) -> maud::Markup {
    rich_text_area_htmx_with_token_field(
        changeset,
        field,
        label,
        preview_url,
        DEFAULT_SUBMIT_TOKEN_FIELD,
    )
}

/// Like [`rich_text_area_htmx`] but excludes the caller-supplied submit-token
/// field name from the preview POST instead of the hardcoded default
/// `_submit_token`.
///
/// Use this when the app customizes `[security.submit_token].field_name`
/// (issue #1843) — source the configured name at request time from the
/// [`SubmitFormField`](crate::security::SubmitFormField) extractor. Passing
/// `"_submit_token"` here is equivalent to calling [`rich_text_area_htmx`].
#[cfg(feature = "maud")]
#[must_use]
pub fn rich_text_area_htmx_with_token_field<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    preview_url: &str,
    token_field: &str,
) -> maud::Markup {
    rich_text_area_inner(
        changeset,
        field,
        label,
        Some((preview_url, token_field)),
        false,
    )
}

/// Render a labeled Markdown editor for a **required** rich-text field.
///
/// Identical to [`rich_text_area`] but adds the HTML `required` attribute and
/// `aria-required="true"`, giving both AT users and browser-native validation
/// the required-field signal — matching the `required_*` siblings for every
/// other control.
///
/// This matters more here than it looks: both `String` deserialization and a
/// `TEXT NOT NULL` column accept the empty string, so without a client-side
/// signal a non-nullable rich-text column would silently persist a blank body
/// unless the field also declared a `{min=N}` length constraint (PR #2157
/// review). The generator picks this variant for a non-nullable `richtext`
/// column and [`rich_text_area`] for an `Option<richtext>` one.
#[cfg(feature = "maud")]
#[must_use]
pub fn required_rich_text_area<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
) -> maud::Markup {
    rich_text_area_inner(changeset, field, label, None, true)
}

/// Render a **required** Markdown editor with an htmx-driven live preview.
///
/// The required-field counterpart of [`rich_text_area_htmx`]; see
/// [`required_rich_text_area`] for why the signal matters and
/// [`rich_text_area_htmx`] for the preview wiring.
#[cfg(feature = "maud")]
#[must_use]
pub fn required_rich_text_area_htmx<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    preview_url: &str,
) -> maud::Markup {
    required_rich_text_area_htmx_with_token_field(
        changeset,
        field,
        label,
        preview_url,
        DEFAULT_SUBMIT_TOKEN_FIELD,
    )
}

/// Like [`required_rich_text_area_htmx`] but excludes the caller-supplied
/// submit-token field name from the preview POST instead of the hardcoded
/// default `_submit_token`.
///
/// The required-field counterpart of [`rich_text_area_htmx_with_token_field`].
#[cfg(feature = "maud")]
#[must_use]
pub fn required_rich_text_area_htmx_with_token_field<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    preview_url: &str,
    token_field: &str,
) -> maud::Markup {
    rich_text_area_inner(
        changeset,
        field,
        label,
        Some((preview_url, token_field)),
        true,
    )
}

/// Shared body of [`rich_text_area`] and its htmx variant. `preview` is
/// `Some((preview_url, token_field))` for the live-preview flavour.
#[cfg(feature = "maud")]
fn rich_text_area_inner<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    preview: Option<(&str, &str)>,
    required: bool,
) -> maud::Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let value = changeset.field_value(field).unwrap_or_default();
    let error_id = has_errors.then(|| format!("{field}-error"));
    let hint_id = format!("{field}-hint");
    let wrapper_id = format!("{field}-field");
    let preview_id = format!("{field}-preview");
    // The hint is always present, so `aria-describedby` is never empty and
    // never names an element that isn't in the DOM.
    let described_by = if has_errors {
        format!("{hint_id} {}", error_id.as_deref().unwrap_or_default())
    } else {
        hint_id.clone()
    };
    let preview_target = format!("#{preview_id}");
    let hx_params = preview.map(|(_, token_field)| format!("not {token_field}"));

    maud::html! {
        div id=(wrapper_id) class="autumn-field autumn-rich-text" data-autumn-field-wrapper=(field) {
            label for=(field) class="autumn-field__label" { (label) }
            div class="autumn-rich-text__toolbar" role="group" aria-label="Markdown formatting" {
                @for (control, syntax) in RICH_TEXT_TOOLBAR {
                    span class="autumn-rich-text__toolbar-item" {
                        span class="autumn-rich-text__toolbar-label" { (control) }
                        code class="autumn-rich-text__toolbar-syntax" { (syntax) }
                    }
                }
            }
            textarea
                id=(field)
                name=(field)
                rows="12"
                class=(if has_errors { maud::PreEscaped("autumn-field__input autumn-rich-text__editor autumn-field__input--invalid") } else { maud::PreEscaped("autumn-field__input autumn-rich-text__editor") })
                aria-invalid=(if has_errors { maud::PreEscaped("true") } else { maud::PreEscaped("false") })
                aria-describedby=(described_by)
                required[required]
                aria-required=[required.then_some("true")]
                hx-post=[preview.map(|(url, _)| url)]
                hx-trigger=[preview.map(|_| "keyup changed delay:400ms, change")]
                hx-target=[preview.map(|_| preview_target.as_str())]
                hx-swap=[preview.map(|_| "innerHTML")]
                hx-include=[preview.map(|_| "closest form")]
                hx-params=[hx_params.as_deref()]
                { (value) }
            p id=(hint_id) class="autumn-rich-text__hint" { (RICH_TEXT_HINT) }
            @if has_errors {
                div id=(error_id.as_deref().unwrap_or_default()) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (error) }
                    }
                }
            }
            @if preview.is_some() {
                div class="autumn-rich-text__preview-wrapper" {
                    span class="autumn-rich-text__preview-label" id=(format!("{field}-preview-label")) { "Preview" }
                    // Deliberately NOT an `aria-live` region: this element is
                    // the htmx swap target and re-renders on every pause in
                    // typing, so announcing it would read the entire post back
                    // to the author mid-sentence. `docs/guide/accessibility.md`
                    // reserves live regions for short, deliberate status
                    // messages delivered out-of-band. It stays a labeled
                    // landmark the author can navigate to on demand.
                    div
                        id=(preview_id)
                        class="autumn-rich-text__preview"
                        role="region"
                        aria-labelledby=(format!("{field}-preview-label"))
                        { (rich_text_preview(&value)) }
                }
            }
        }
    }
}

/// Server-side pre-render of the preview pane's initial contents.
///
/// With the `markdown` feature on, the current value is rendered through the
/// same sanitizer the live preview and the show page use, so a 422 re-render
/// displays the preview immediately instead of waiting for the first keystroke.
/// Without it, the pane starts empty and fills in on the first htmx response.
#[cfg(all(feature = "maud", feature = "markdown"))]
fn rich_text_preview(value: &str) -> maud::Markup {
    crate::markdown::render_user_content(value)
}

/// Fallback for builds without the `markdown` feature — see the documented
/// sibling above.
#[cfg(all(feature = "maud", not(feature = "markdown")))]
fn rich_text_preview(_value: &str) -> maud::Markup {
    maud::html! {}
}

/// Render a labeled `<input type="text">` for a required field.
///
/// Identical to [`text_input`] but adds `aria-required="true"` and the HTML
/// `required` attribute, giving both AT users and browser-native validation
/// the required-field signal without relying solely on color.
/// Wraps in `<div id="{field}-field">` for stable htmx targeting.
#[cfg(feature = "maud")]
#[must_use]
pub fn required_text_input<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
) -> maud::Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let value = changeset.field_value(field).unwrap_or_default();
    let error_id = has_errors.then(|| format!("{field}-error"));
    let wrapper_id = format!("{field}-field");

    maud::html! {
        div id=(wrapper_id) {
            label for=(field) { (label) }
            input
                type="text"
                id=(field)
                name=(field)
                value=(value)
                required
                aria-required="true"
                aria-invalid=(if has_errors { maud::PreEscaped("true") } else { maud::PreEscaped("false") })
                aria-describedby=(error_id.as_deref().unwrap_or(""));
            @if has_errors {
                div id=(error_id.as_deref().unwrap_or_default()) role="alert" {
                    @for error in errors {
                        p { (error) }
                    }
                }
            }
        }
    }
}

/// Render a labeled `<input type="checkbox">` tied to a `bool` changeset field.
///
/// # Required: `#[serde(default)]` on the target field
///
/// HTML checkboxes are omitted from submitted form data entirely when
/// unchecked — there is no way to distinguish "unchecked" from "field not
/// present" on the wire. **Do not** pair this with a hidden `<input
/// type="hidden" value="false">` sibling sharing the same `name`: a checked
/// box then submits the key *twice* (`field=false` from the hidden input,
/// `field=true` from the checkbox), and `serde_urlencoded` (used by both
/// axum's `Form` extractor and [`ChangesetForm`]) rejects duplicate keys
/// with a "duplicate field" deserialize error instead of taking the last
/// value — every checked submission would 400.
///
/// Instead, mark the target `bool` field `#[serde(default)]` so a missing
/// key decodes as `false`:
///
/// ```rust,ignore
/// #[derive(serde::Deserialize)]
/// struct PostForm {
///     #[serde(default)]
///     published: bool,
/// }
/// ```
///
/// For a nullable `Option<bool>` field where `None` is a meaningful third
/// state (distinct from `Some(false)`), a checkbox cannot represent it
/// losslessly — use [`select_input`] with three options instead.
///
/// The `checked` attribute reflects the changeset's current value via
/// [`Changeset::field_value`], which serializes `bool` as `"true"`/`"false"`.
/// Wraps in `<div id="{field}-field">` for stable htmx targeting. ARIA
/// annotations behave identically to [`text_input`].
#[cfg(feature = "maud")]
#[must_use]
pub fn checkbox_input<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
) -> maud::Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let checked = changeset.field_value(field).as_deref() == Some("true");
    let field_html = fast_escape(field);
    let error_id = has_errors.then(|| concat_suffix(&field_html, "-error"));
    let wrapper_id = concat_suffix(&field_html, "-field");
    let field_pe = maud::PreEscaped(field_html.as_ref());
    let wrapper_pe = maud::PreEscaped(&wrapper_id);
    let aria_pe = maud::PreEscaped(error_id.as_deref().unwrap_or(""));

    maud::html! {
        div id=(wrapper_pe) class="autumn-field" {
            label for=(field_pe) class="autumn-field__label" { (esc(label)) }
            input
                type="checkbox"
                id=(field_pe)
                name=(field_pe)
                value="true"
                checked[checked]
                class=(if has_errors { maud::PreEscaped("autumn-field__input autumn-field__input--invalid") } else { maud::PreEscaped("autumn-field__input") })
                aria-invalid=(if has_errors { maud::PreEscaped("true") } else { maud::PreEscaped("false") })
                aria-describedby=(aria_pe);
            @if has_errors {
                div id=(maud::PreEscaped(error_id.as_deref().unwrap_or_default())) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (esc(error)) }
                    }
                }
            }
        }
    }
}

/// Render a labeled `<input type="number">` tied to a numeric changeset field
/// (`i32`, `i64`, `f32`, `f64`).
///
/// `step` sets the HTML `step` attribute — pass `Some("1")` for integer
/// fields, `Some("0.01")` or `Some("any")` for floating-point fields, or
/// `None` to leave the browser default (`step="1"`, whole numbers only).
/// Wraps in `<div id="{field}-field">` for stable htmx targeting. ARIA
/// annotations behave identically to [`text_input`].
#[cfg(feature = "maud")]
#[must_use]
pub fn number_input<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    step: Option<&str>,
) -> maud::Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let value = changeset.field_value(field).unwrap_or_default();
    let field_html = fast_escape(field);
    let error_id = has_errors.then(|| concat_suffix(&field_html, "-error"));
    let wrapper_id = concat_suffix(&field_html, "-field");
    let field_pe = maud::PreEscaped(field_html.as_ref());
    let wrapper_pe = maud::PreEscaped(&wrapper_id);
    let aria_pe = maud::PreEscaped(error_id.as_deref().unwrap_or(""));

    maud::html! {
        div id=(wrapper_pe) class="autumn-field" {
            label for=(field_pe) class="autumn-field__label" { (esc(label)) }
            input
                type="number"
                id=(field_pe)
                name=(field_pe)
                value=(esc(&value))
                step=[step]
                class=(if has_errors { maud::PreEscaped("autumn-field__input autumn-field__input--invalid") } else { maud::PreEscaped("autumn-field__input") })
                aria-invalid=(if has_errors { maud::PreEscaped("true") } else { maud::PreEscaped("false") })
                aria-describedby=(aria_pe);
            @if has_errors {
                div id=(maud::PreEscaped(error_id.as_deref().unwrap_or_default())) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (esc(error)) }
                    }
                }
            }
        }
    }
}

/// Render a labeled `<input type="number">` for a required numeric field.
///
/// Identical to [`number_input`] but adds `aria-required="true"` and the HTML
/// `required` attribute, giving both AT users and browser-native validation
/// the required-field signal without relying solely on color.
#[cfg(feature = "maud")]
#[must_use]
pub fn required_number_input<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    step: Option<&str>,
) -> maud::Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let value = changeset.field_value(field).unwrap_or_default();
    let error_id = has_errors.then(|| format!("{field}-error"));
    let wrapper_id = format!("{field}-field");

    maud::html! {
        div id=(wrapper_id) class="autumn-field" {
            label for=(field) class="autumn-field__label" { (label) }
            input
                type="number"
                id=(field)
                name=(field)
                value=(value)
                step=[step]
                required
                aria-required="true"
                class=(if has_errors { maud::PreEscaped("autumn-field__input autumn-field__input--invalid") } else { maud::PreEscaped("autumn-field__input") })
                aria-invalid=(if has_errors { maud::PreEscaped("true") } else { maud::PreEscaped("false") })
                aria-describedby=(error_id.as_deref().unwrap_or(""));
            @if has_errors {
                div id=(error_id.as_deref().unwrap_or_default()) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (error) }
                    }
                }
            }
        }
    }
}

/// Normalize a stored date/datetime string into `YYYY-MM-DD`, the shape the
/// HTML `<input type="date">` control requires.
///
/// Accepts a bare date, a full RFC 3339 timestamp (with offset/`Z`), or a
/// naive datetime, and keeps just the date component. Falls back to the
/// input unchanged when none of those shapes match (e.g. an empty string).
#[cfg(feature = "maud")]
fn normalize_date_value(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    // `NaiveDateTime`'s `Display` (as opposed to its serde serialization,
    // which uses `T`) separates date and time with a space, e.g. from a raw
    // `.to_string()` or some database drivers. Normalize defensively so
    // those still parse instead of falling through to the raw string.
    let normalized = raw.replace(' ', "T");
    if let Ok(date) = chrono::NaiveDate::parse_from_str(&normalized, "%Y-%m-%d") {
        return date.to_string();
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&normalized) {
        return dt.format("%Y-%m-%d").to_string();
    }
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S%.f") {
        return ndt.format("%Y-%m-%d").to_string();
    }
    raw.to_owned()
}

/// Normalize a stored datetime string into a shape the HTML `<input
/// type="datetime-local">` control accepts (`YYYY-MM-DDTHH:MM[:SS[.fff]]`).
/// Browsers silently reject RFC 3339 timestamps carrying a `Z`/offset
/// suffix.
///
/// **Seconds/fractional-seconds preserved when present.** `chrono`'s default
/// `serde::Deserialize` for `NaiveDateTime`/`DateTime<Utc>` requires the
/// seconds component — truncating to `YYYY-MM-DDTHH:MM` here would make
/// [`ChangesetForm`] fail to decode the *pre-filled* value on any
/// submission where the user doesn't manually retype it (chrono's parser
/// returns "premature end of input"). `%.f` omits the fractional part
/// cleanly when it's zero, so whole-second values still render without a
/// trailing dot.
///
/// **Wall-clock preserved.** For RFC 3339 input with an explicit offset, the
/// offset is dropped but the local clock components are kept as-is (no
/// conversion to UTC) — the datetime-local input has no timezone concept, so
/// shifting the clock would mutate the value on a no-op save.
#[cfg(feature = "maud")]
fn normalize_datetime_local_value(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    // See `normalize_date_value`'s comment on space-separated input.
    let normalized = raw.replace(' ', "T");
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S%.f") {
        return ndt.format("%Y-%m-%dT%H:%M:%S%.f").to_string();
    }
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M") {
        return ndt.format("%Y-%m-%dT%H:%M:%S%.f").to_string();
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&normalized) {
        return dt.naive_local().format("%Y-%m-%dT%H:%M:%S%.f").to_string();
    }
    raw.to_owned()
}

/// Pad an HTML `datetime-local` submission (`YYYY-MM-DDTHH:MM`, seconds
/// omitted by the browser at minute granularity) to `YYYY-MM-DDTHH:MM:00` so
/// chrono's parser — which requires the seconds component — accepts it.
fn pad_datetime_local_seconds(raw: &str) -> String {
    if raw.chars().count() == 16 {
        format!("{raw}:00")
    } else {
        raw.to_owned()
    }
}

/// Parse a submitted datetime string into `chrono::DateTime<Utc>`, accepting
/// both wire shapes the same struct has to serve:
///
/// - RFC 3339 with an explicit offset (JSON API bodies) — the offset is
///   honored and the instant converted to UTC;
/// - the offsetless HTML `datetime-local` shape `YYYY-MM-DDTHH:MM[:SS[.f]]`
///   (browser form posts) — interpreted as UTC wall-clock time.
fn parse_datetime_local_or_rfc3339_utc(
    raw: &str,
) -> Result<chrono::DateTime<chrono::Utc>, chrono::ParseError> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(dt.with_timezone(&chrono::Utc));
    }
    chrono::NaiveDateTime::parse_from_str(&pad_datetime_local_seconds(raw), "%Y-%m-%dT%H:%M:%S%.f")
        .map(|ndt| ndt.and_utc())
}

/// Parse a submitted datetime string into `chrono::DateTime<Local>`,
/// accepting both wire shapes the same struct has to serve:
///
/// - RFC 3339 with an explicit offset (JSON API bodies) — the offset is
///   honored and the instant converted to the server's local zone;
/// - the offsetless HTML `datetime-local` shape `YYYY-MM-DDTHH:MM[:SS[.f]]`
///   (browser form posts) — interpreted as wall-clock time in the server's
///   local zone. A wall clock repeated by a DST fall-back transition maps to
///   the **earlier** of the two instants (deterministic rather than
///   rejected); a wall clock skipped by a spring-forward transition has no
///   corresponding instant and errors.
fn parse_datetime_local_or_rfc3339_local(
    raw: &str,
) -> Result<chrono::DateTime<chrono::Local>, String> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(dt.with_timezone(&chrono::Local));
    }
    let ndt = chrono::NaiveDateTime::parse_from_str(
        &pad_datetime_local_seconds(raw),
        "%Y-%m-%dT%H:%M:%S%.f",
    )
    .map_err(|e| e.to_string())?;
    match ndt.and_local_timezone(chrono::Local) {
        chrono::LocalResult::Single(dt) => Ok(dt),
        chrono::LocalResult::Ambiguous(earliest, _) => Ok(earliest),
        chrono::LocalResult::None => Err(format!(
            "local time {ndt} does not exist in the server's timezone (skipped by a DST transition)"
        )),
    }
}

/// Deserialize an HTML `datetime-local` submission into `chrono::DateTime<Utc>`,
/// treating an offsetless submitted wall-clock value as UTC.
///
/// RFC 3339 input with an explicit offset is also accepted (converted to
/// UTC), so a struct that doubles as a JSON API body keeps decoding.
///
/// `<input type="datetime-local">` has no timezone concept, so [`datetime_input`]
/// necessarily strips any offset when rendering a `DateTime<Utc>` field's
/// current value — the browser then posts back an offsetless string (e.g.
/// `2024-03-15T10:30:56`). Chrono's *default* `Deserialize` for
/// `DateTime<Utc>` requires an RFC 3339 offset and rejects that string with
/// "premature end of input" on every submission. The `#[model]`-generated
/// `NewX` insert struct attaches this deserializer to non-nullable
/// `DateTime<Utc>` columns automatically (and
/// [`deserialize_datetime_local_utc_option`] to nullable ones); attach it
/// yourself on any hand-written form struct with a `DateTime<Utc>` field
/// rendered via `datetime_input`:
///
/// ```rust,ignore
/// #[derive(serde::Deserialize)]
/// struct EventForm {
///     #[serde(deserialize_with = "autumn_web::form::deserialize_datetime_local_utc")]
///     starts_at: chrono::DateTime<chrono::Utc>,
/// }
/// ```
///
/// `chrono::NaiveDateTime` fields don't need an offset, but still benefit
/// from [`deserialize_naive_datetime_local`] as a defensive measure — see
/// its doc comment.
///
/// # Errors
///
/// Returns a deserializer error when the submitted value is neither a valid
/// `YYYY-MM-DDTHH:MM[:SS[.f]]` local datetime string nor a valid RFC 3339
/// timestamp.
pub fn deserialize_datetime_local_utc<'de, D>(
    deserializer: D,
) -> Result<chrono::DateTime<chrono::Utc>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <String as serde::Deserialize>::deserialize(deserializer)?;
    parse_datetime_local_or_rfc3339_utc(&raw).map_err(serde::de::Error::custom)
}

/// `Option<chrono::DateTime<Utc>>` counterpart to
/// [`deserialize_datetime_local_utc`], for a nullable field — an absent,
/// `null`, or empty submitted value decodes as `None`.
///
/// Like the required variant, both the offsetless `datetime-local` shape
/// (interpreted as UTC) and RFC 3339 (offset converted to UTC) are accepted.
///
/// Pair it with `#[serde(default)]`: `deserialize_with` disables serde's
/// implicit missing-`Option`-field-is-`None` handling, and `default` restores
/// it (the `#[model]`-generated `NewX` struct emits both).
///
/// # Errors
///
/// Returns a deserializer error when a non-empty submitted value is neither
/// a valid `YYYY-MM-DDTHH:MM[:SS[.f]]` local datetime string nor a valid
/// RFC 3339 timestamp.
pub fn deserialize_datetime_local_utc_option<'de, D>(
    deserializer: D,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <Option<String> as serde::Deserialize>::deserialize(deserializer)?;
    match raw {
        Some(s) if !s.is_empty() => parse_datetime_local_or_rfc3339_utc(&s)
            .map(Some)
            .map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}

/// Deserialize an HTML `datetime-local` submission into
/// `chrono::DateTime<Local>`, treating an offsetless submitted wall-clock
/// value as the server's local time.
///
/// RFC 3339 input with an explicit offset is also accepted (the instant is
/// converted to the local zone), so a struct that doubles as a JSON API body
/// keeps decoding.
///
/// This is the `DateTime<chrono::Local>` counterpart to
/// [`deserialize_datetime_local_utc`] — see its doc comment for why derived
/// forms need a datetime-local-tolerant deserializer at all. The
/// `#[model]`-generated `NewX` insert struct attaches this deserializer to
/// non-nullable `DateTime<Local>` columns automatically (and
/// [`deserialize_datetime_local_local_option`] to nullable ones); attach it
/// yourself on any hand-written form struct with a `DateTime<Local>` field
/// rendered via [`datetime_input`].
///
/// **DST edge cases** (the submitted wall clock has no offset, so the local
/// zone decides which instant it names): a wall clock that occurs twice
/// because of a fall-back transition maps to the **earlier** of the two
/// instants — deterministic, rather than rejecting the submission; a wall
/// clock skipped by a spring-forward transition names no instant at all and
/// is rejected as a decode error.
///
/// # Errors
///
/// Returns a deserializer error when the submitted value is neither a valid
/// `YYYY-MM-DDTHH:MM[:SS[.f]]` local datetime string nor a valid RFC 3339
/// timestamp, or when the offsetless wall clock falls in a DST gap of the
/// server's local zone.
pub fn deserialize_datetime_local_local<'de, D>(
    deserializer: D,
) -> Result<chrono::DateTime<chrono::Local>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <String as serde::Deserialize>::deserialize(deserializer)?;
    parse_datetime_local_or_rfc3339_local(&raw).map_err(serde::de::Error::custom)
}

/// `Option<chrono::DateTime<Local>>` counterpart to
/// [`deserialize_datetime_local_local`], for a nullable field — an absent,
/// `null`, or empty submitted value decodes as `None`.
///
/// Like the required variant, both the offsetless `datetime-local` shape
/// (interpreted as the server's local wall clock; DST-ambiguous values map
/// to the earlier instant, DST-skipped values error) and RFC 3339 (offset
/// converted to the local zone) are accepted.
///
/// Pair it with `#[serde(default)]`: `deserialize_with` disables serde's
/// implicit missing-`Option`-field-is-`None` handling, and `default` restores
/// it (the `#[model]`-generated `NewX` struct emits both).
///
/// # Errors
///
/// Returns a deserializer error when a non-empty submitted value is neither
/// a valid `YYYY-MM-DDTHH:MM[:SS[.f]]` local datetime string nor a valid
/// RFC 3339 timestamp, or when the offsetless wall clock falls in a DST gap
/// of the server's local zone.
pub fn deserialize_datetime_local_local_option<'de, D>(
    deserializer: D,
) -> Result<Option<chrono::DateTime<chrono::Local>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <Option<String> as serde::Deserialize>::deserialize(deserializer)?;
    match raw {
        Some(s) if !s.is_empty() => parse_datetime_local_or_rfc3339_local(&s)
            .map(Some)
            .map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}

/// Deserialize an HTML `datetime-local` submission into `chrono::NaiveDateTime`.
///
/// The *pre-filled* value [`datetime_input`] renders always includes seconds
/// (see `normalize_datetime_local_value`), so chrono's default `Deserialize`
/// — which requires the seconds component — decodes an untouched submission
/// fine on its own. But a value the user actively edits through the
/// browser's native picker isn't guaranteed to include seconds (`step="any"`
/// only requests that the control *allow* seconds; it doesn't guarantee
/// every browser's UI captures them), which would otherwise 400 with
/// "premature end of input". The `#[model]`-generated `NewX` insert struct
/// attaches this deserializer to non-nullable `NaiveDateTime` columns
/// automatically (and [`deserialize_naive_datetime_local_option`] to nullable
/// ones); attach it yourself on hand-written form structs to defend against
/// that:
///
/// ```rust,ignore
/// #[derive(serde::Deserialize)]
/// struct EventForm {
///     #[serde(deserialize_with = "autumn_web::form::deserialize_naive_datetime_local")]
///     starts_at: chrono::NaiveDateTime,
/// }
/// ```
///
/// # Errors
///
/// Returns a deserializer error when the submitted value isn't a valid
/// `YYYY-MM-DDTHH:MM[:SS[.f]]` local datetime string.
pub fn deserialize_naive_datetime_local<'de, D>(
    deserializer: D,
) -> Result<chrono::NaiveDateTime, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <String as serde::Deserialize>::deserialize(deserializer)?;
    chrono::NaiveDateTime::parse_from_str(&pad_datetime_local_seconds(&raw), "%Y-%m-%dT%H:%M:%S%.f")
        .map_err(serde::de::Error::custom)
}

/// `Option<chrono::NaiveDateTime>` counterpart to
/// [`deserialize_naive_datetime_local`], for a nullable field — an absent,
/// `null`, or empty submitted value decodes as `None`.
///
/// Pair it with `#[serde(default)]`: `deserialize_with` disables serde's
/// implicit missing-`Option`-field-is-`None` handling, and `default` restores
/// it (the `#[model]`-generated `NewX` struct emits both).
///
/// # Errors
///
/// Returns a deserializer error when a non-empty submitted value isn't a
/// valid `YYYY-MM-DDTHH:MM[:SS[.f]]` local datetime string.
pub fn deserialize_naive_datetime_local_option<'de, D>(
    deserializer: D,
) -> Result<Option<chrono::NaiveDateTime>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <Option<String> as serde::Deserialize>::deserialize(deserializer)?;
    match raw {
        Some(s) if !s.is_empty() => chrono::NaiveDateTime::parse_from_str(
            &pad_datetime_local_seconds(&s),
            "%Y-%m-%dT%H:%M:%S%.f",
        )
        .map(Some)
        .map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}

/// Render a labeled `<input type="date">` tied to a changeset field.
///
/// The current value is normalized via `normalize_date_value` to the
/// `YYYY-MM-DD` shape HTML5 date pickers require, regardless of whether the
/// underlying field serializes as a bare date or a full timestamp. Wraps in
/// `<div id="{field}-field">` for stable htmx targeting. ARIA annotations
/// behave identically to [`text_input`].
#[cfg(feature = "maud")]
#[must_use]
pub fn date_input<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
) -> maud::Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let value = normalize_date_value(&changeset.field_value(field).unwrap_or_default());
    let field_html = fast_escape(field);
    let error_id = has_errors.then(|| concat_suffix(&field_html, "-error"));
    let wrapper_id = concat_suffix(&field_html, "-field");
    let field_pe = maud::PreEscaped(field_html.as_ref());
    let wrapper_pe = maud::PreEscaped(&wrapper_id);
    let aria_pe = maud::PreEscaped(error_id.as_deref().unwrap_or(""));

    maud::html! {
        div id=(wrapper_pe) class="autumn-field" {
            label for=(field_pe) class="autumn-field__label" { (esc(label)) }
            input
                type="date"
                id=(field_pe)
                name=(field_pe)
                value=(esc(&value))
                class=(if has_errors { maud::PreEscaped("autumn-field__input autumn-field__input--invalid") } else { maud::PreEscaped("autumn-field__input") })
                aria-invalid=(if has_errors { maud::PreEscaped("true") } else { maud::PreEscaped("false") })
                aria-describedby=(aria_pe);
            @if has_errors {
                div id=(maud::PreEscaped(error_id.as_deref().unwrap_or_default())) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (esc(error)) }
                    }
                }
            }
        }
    }
}

/// Render a labeled `<input type="date">` for a required field.
///
/// Identical to [`date_input`] but adds `aria-required="true"` and the HTML
/// `required` attribute, giving both AT users and browser-native validation
/// the required-field signal without relying solely on color.
#[cfg(feature = "maud")]
#[must_use]
pub fn required_date_input<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
) -> maud::Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let value = normalize_date_value(&changeset.field_value(field).unwrap_or_default());
    let error_id = has_errors.then(|| format!("{field}-error"));
    let wrapper_id = format!("{field}-field");

    maud::html! {
        div id=(wrapper_id) class="autumn-field" {
            label for=(field) class="autumn-field__label" { (label) }
            input
                type="date"
                id=(field)
                name=(field)
                value=(value)
                required
                aria-required="true"
                class=(if has_errors { maud::PreEscaped("autumn-field__input autumn-field__input--invalid") } else { maud::PreEscaped("autumn-field__input") })
                aria-invalid=(if has_errors { maud::PreEscaped("true") } else { maud::PreEscaped("false") })
                aria-describedby=(error_id.as_deref().unwrap_or(""));
            @if has_errors {
                div id=(error_id.as_deref().unwrap_or_default()) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (error) }
                    }
                }
            }
        }
    }
}

/// Render a labeled `<input type="datetime-local">` tied to a changeset
/// field (`NaiveDateTime` or `DateTime`).
///
/// The current value is normalized via `normalize_datetime_local_value` to
/// the shape HTML5 datetime pickers require, preserving seconds/fractional
/// seconds when present — `chrono`'s default `Deserialize` requires the
/// seconds component, so truncating to minutes would break decoding the
/// pre-filled value on any untouched submission. Renders `step="any"` so a
/// value carrying seconds doesn't fail the browser's step-mismatch
/// constraint validation (the default step is minute-granularity). Wraps in
/// `<div id="{field}-field">` for stable htmx targeting. ARIA annotations
/// behave identically to [`text_input`].
///
/// # Attach a `deserialize_with` matching the field's chrono type
///
/// `<input type="datetime-local">` has no timezone concept, so the value
/// this renders for a `DateTime<Utc>` field never carries an offset —
/// chrono's default `Deserialize` for `DateTime<Utc>` requires one and
/// rejects the submission. Attach [`deserialize_datetime_local_utc`] (or
/// [`deserialize_datetime_local_utc_option`] for `Option<DateTime<Utc>>`)
/// via `#[serde(deserialize_with = "...")]` on that field. `DateTime<Local>`
/// fields have the same problem and take
/// [`deserialize_datetime_local_local`] (or its `_option` variant), which
/// interprets the offsetless wall clock in the server's local zone. Other
/// zone parameters (e.g. `DateTime<FixedOffset>`) have **no** sound
/// interpretation of an offsetless value — don't render them through this
/// control; use a text input carrying the RFC 3339 string instead (which is
/// what a derived `form_for` does).
///
/// `NaiveDateTime` fields don't hit that offset problem, but a value the
/// user actively edits through the browser's native picker isn't guaranteed
/// to include seconds (unlike the always-seconds-inclusive pre-filled
/// value), which chrono's default `Deserialize` also rejects. Attach
/// [`deserialize_naive_datetime_local`] (or
/// [`deserialize_naive_datetime_local_option`] for `Option<NaiveDateTime>`)
/// as a defensive measure.
#[cfg(feature = "maud")]
#[must_use]
pub fn datetime_input<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
) -> maud::Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let value = normalize_datetime_local_value(&changeset.field_value(field).unwrap_or_default());
    let error_id = has_errors.then(|| format!("{field}-error"));
    let wrapper_id = format!("{field}-field");

    maud::html! {
        div id=(wrapper_id) class="autumn-field" {
            label for=(field) class="autumn-field__label" { (label) }
            input
                type="datetime-local"
                id=(field)
                name=(field)
                value=(value)
                step="any"
                class=(if has_errors { maud::PreEscaped("autumn-field__input autumn-field__input--invalid") } else { maud::PreEscaped("autumn-field__input") })
                aria-invalid=(if has_errors { maud::PreEscaped("true") } else { maud::PreEscaped("false") })
                aria-describedby=(error_id.as_deref().unwrap_or(""));
            @if has_errors {
                div id=(error_id.as_deref().unwrap_or_default()) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (error) }
                    }
                }
            }
        }
    }
}

/// Render a labeled `<input type="datetime-local">` for a required field.
///
/// Identical to [`datetime_input`] but adds `aria-required="true"` and the
/// HTML `required` attribute, giving both AT users and browser-native
/// validation the required-field signal without relying solely on color.
#[cfg(feature = "maud")]
#[must_use]
pub fn required_datetime_input<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
) -> maud::Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let value = normalize_datetime_local_value(&changeset.field_value(field).unwrap_or_default());
    let error_id = has_errors.then(|| format!("{field}-error"));
    let wrapper_id = format!("{field}-field");

    maud::html! {
        div id=(wrapper_id) class="autumn-field" {
            label for=(field) class="autumn-field__label" { (label) }
            input
                type="datetime-local"
                id=(field)
                name=(field)
                value=(value)
                step="any"
                required
                aria-required="true"
                class=(if has_errors { maud::PreEscaped("autumn-field__input autumn-field__input--invalid") } else { maud::PreEscaped("autumn-field__input") })
                aria-invalid=(if has_errors { maud::PreEscaped("true") } else { maud::PreEscaped("false") })
                aria-describedby=(error_id.as_deref().unwrap_or(""));
            @if has_errors {
                div id=(error_id.as_deref().unwrap_or_default()) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (error) }
                    }
                }
            }
        }
    }
}

/// Render a labeled `<select>` tied to a closed-set changeset field, with
/// `options` given as `(value, label)` pairs.
///
/// The option whose `value` matches the changeset's current field value
/// (via [`Changeset::field_value`]) is marked `selected`. This is the
/// control the enum ([#1030]) and references ([#1026]) field types render
/// once those field kinds ship — this slice ships the widget, not the DSL
/// tokens that will target it.
/// Wraps in `<div id="{field}-field">` for stable htmx targeting. ARIA
/// annotations behave identically to [`text_input`].
///
/// [#1030]: https://github.com/autumn-foundation/autumn/issues/1030
/// [#1026]: https://github.com/autumn-foundation/autumn/issues/1026
#[cfg(feature = "maud")]
#[must_use]
pub fn select_input<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    options: &[(&str, &str)],
) -> maud::Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let current = changeset.field_value(field).unwrap_or_default();
    let error_id = has_errors.then(|| format!("{field}-error"));
    let wrapper_id = format!("{field}-field");

    maud::html! {
        div id=(wrapper_id) class="autumn-field" {
            label for=(field) class="autumn-field__label" { (label) }
            select
                id=(field)
                name=(field)
                class=(if has_errors { maud::PreEscaped("autumn-field__input autumn-field__input--invalid") } else { maud::PreEscaped("autumn-field__input") })
                aria-invalid=(if has_errors { maud::PreEscaped("true") } else { maud::PreEscaped("false") })
                aria-describedby=(error_id.as_deref().unwrap_or("")) {
                @for (option_value, option_label) in options {
                    option value=(option_value) selected[*option_value == current] { (option_label) }
                }
            }
            @if has_errors {
                div id=(error_id.as_deref().unwrap_or_default()) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (error) }
                    }
                }
            }
        }
    }
}

/// Render a labeled `<select>` for a required closed-set field.
///
/// Identical to [`select_input`] but adds `aria-required="true"` and the HTML
/// `required` attribute — combined with a blank placeholder option (an empty
/// `value=""`), this blocks submission until the user picks a real option,
/// giving both AT users and browser-native validation the required-field
/// signal without relying solely on color.
#[cfg(feature = "maud")]
#[must_use]
pub fn required_select_input<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    options: &[(&str, &str)],
) -> maud::Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let current = changeset.field_value(field).unwrap_or_default();
    let error_id = has_errors.then(|| format!("{field}-error"));
    let wrapper_id = format!("{field}-field");

    maud::html! {
        div id=(wrapper_id) class="autumn-field" {
            label for=(field) class="autumn-field__label" { (label) }
            select
                id=(field)
                name=(field)
                required
                aria-required="true"
                class=(if has_errors { maud::PreEscaped("autumn-field__input autumn-field__input--invalid") } else { maud::PreEscaped("autumn-field__input") })
                aria-invalid=(if has_errors { maud::PreEscaped("true") } else { maud::PreEscaped("false") })
                aria-describedby=(error_id.as_deref().unwrap_or("")) {
                @for (option_value, option_label) in options {
                    option value=(option_value) selected[*option_value == current] { (option_label) }
                }
            }
            @if has_errors {
                div id=(error_id.as_deref().unwrap_or_default()) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (error) }
                    }
                }
            }
        }
    }
}

/// Render an ARIA live region for htmx swap announcements.
///
/// Emits `<div id="…" role="status" aria-live="polite" aria-atomic="true">`.
/// Place this element in your page layout and update its content via
/// `hx-swap-oob` to announce htmx-driven changes to screen readers without
/// moving keyboard focus.
///
/// # Example
///
/// ```rust,ignore
/// // In your page layout:
/// (aria_live_region("htmx-status", ""))
///
/// // In an htmx response fragment (announces to screen readers):
/// div id="htmx-status" role="status" aria-live="polite" aria-atomic="true"
///     hx-swap-oob="true" {
///     "Post submitted successfully"
/// }
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn aria_live_region(id: &str, message: &str) -> maud::Markup {
    maud::html! {
        div id=(id) role="status" aria-live="polite" aria-atomic="true" {
            (message)
        }
    }
}

/// Render a visually-hidden skip-to-content link that becomes visible on focus.
///
/// Place this as the **first element inside `<body>`** so keyboard users can
/// bypass repeated navigation and jump directly to main content.
///
/// The link carries the `skip-link` CSS class; pair it with the bundled
/// Tailwind config's `skip-link` utility or add your own:
///
/// ```css
/// .skip-link { position: absolute; top: -9999px; }
/// .skip-link:focus { position: static; }
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn skip_link(target: &str, label: &str) -> maud::Markup {
    maud::html! {
        a href=(target) class="skip-link" { (label) }
    }
}

// ── form_for (issue #1135 phase 2) ──────────────────────────────────

/// The HTML control a model field renders as inside [`form_for`].
///
/// This is plain data (no `maud` dependency) so the `#[model]`-derived
/// [`FormModel`] implementation is always available; only the rendering side
/// ([`form_for`]) is gated behind the `maud` feature.
/// The enum is `#[non_exhaustive]`: new control kinds will be added without
/// a semver-major bump, so external `match`es need a wildcard arm. All
/// existing variants remain freely constructible.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FieldControl {
    /// `<input type="text">`.
    Text,
    /// `<textarea>`.
    Textarea,
    /// `<input type="password">`.
    Password,
    /// `<input type="number">`, with an optional HTML `step` attribute.
    Number {
        /// The HTML `step` attribute, e.g. `Some("0.01")`, or `None` to use
        /// the browser default.
        step: Option<String>,
    },
    /// `<input type="checkbox">`.
    Checkbox,
    /// `<input type="date">`.
    Date,
    /// `<input type="datetime-local">`.
    DateTime,
    /// `<select>` with `(value, label)` option pairs, e.g. enum variants or
    /// a nullable-bool tri-state.
    Select {
        /// The `(value, label)` option pairs rendered as `<option>` elements.
        options: Vec<(String, String)>,
    },
    /// `<input type="file">`. The presence of any `File` field makes
    /// [`FormFor::render`] emit `enctype="multipart/form-data"`.
    File,
}

/// One field descriptor consumed by [`form_for`].
///
/// Typically produced by a `#[model]`-derived [`FormModel::form_fields`]
/// implementation, one entry per persisted column that should render as a
/// form control.
#[derive(Debug, Clone)]
pub struct FormField {
    /// The field's `name`/`id` attribute — i.e. the key the browser POSTs
    /// this control under, which must match what the form's decode target
    /// (e.g. the `#[model]`-generated `NewX` insert struct) expects. Also
    /// the key [`Changeset::errors_for`] messages are looked up by, and the
    /// key [`FormFor::exclude`]/[`FormFor::override_field`]/
    /// [`FormFor::override_label`] match on.
    ///
    /// When the changeset's data type serializes this field under a
    /// *different* key (a serde rename), set [`FormField::value_name`] so the
    /// pre-filled value is still found — `name` itself deliberately stays the
    /// POST key, not the serialized key.
    pub name: String,
    /// The human-readable `<label>` text.
    pub label: String,
    /// Which HTML control renders this field.
    pub control: FieldControl,
    /// Whether the rendered control should be marked required (`required`
    /// attribute + `aria-required="true"`), where a required variant of the
    /// control exists.
    pub required: bool,
    /// The serde-effective *serialized* key of this field on the changeset's
    /// data type, when it differs from [`FormField::name`] (e.g.
    /// `#[serde(rename = "headline")]` or a struct-level
    /// `#[serde(rename_all = "camelCase")]`). Used **only** to look up the
    /// pre-filled value via [`Changeset::field_value`]; the rendered
    /// `name`/`id` attributes, error lookup, and builder matching all keep
    /// using [`FormField::name`]. `None` means the serialized key equals
    /// `name` (the common case).
    ///
    /// The `#[model]`-derived [`FormModel`] sets this automatically for
    /// serde-renamed columns; hand-written descriptors use
    /// [`FormField::with_value_name`].
    pub value_name: Option<String>,
}

impl FormField {
    /// Construct a new field descriptor.
    ///
    /// The pre-fill lookup key ([`FormField::value_name`]) defaults to
    /// `name`; use [`FormField::with_value_name`] when the changeset's data
    /// type serializes the field under a different (serde-renamed) key.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        label: impl Into<String>,
        control: FieldControl,
        required: bool,
    ) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            control,
            required,
            value_name: None,
        }
    }

    /// Set the serialized key used to pre-fill this field's value from the
    /// changeset (see [`FormField::value_name`]), returning `self` for
    /// chaining after [`FormField::new`].
    #[must_use]
    pub fn with_value_name(mut self, value_name: impl Into<String>) -> Self {
        self.value_name = Some(value_name.into());
        self
    }
}

/// Implemented by model types (typically via `#[model]`) to drive
/// [`form_for`].
///
/// Returns the ordered list of fields to render — one [`FormField`] per
/// column that should appear as a control in the generated form.
pub trait FormModel {
    /// The ordered list of fields [`form_for`] should render.
    fn form_fields() -> Vec<FormField>;
}

/// Render a full `<form>` for `T` from `changeset`, deriving one control per
/// [`FormModel::form_fields`] entry.
///
/// This is the "one call renders the whole form" counterpart to composing
/// the individual typed-input helpers (e.g. [`text_input`], [`select_input`])
/// by hand: `form_for` walks `T::form_fields()`, dispatches each field to the
/// matching helper based on its [`FieldControl`], and wraps the result in a
/// `<form>` via the same audited CSRF/method-override path as [`form_tag`].
///
/// Use the [`FormFor`] builder methods to exclude fields, override a field's
/// control or label, inject a CSRF token, append extra markup before the
/// submit button, or force a `multipart/form-data` encoding.
///
/// # Checkboxes and `bool` fields
///
/// A [`FieldControl::Checkbox`] field renders via [`checkbox_input`], which
/// deliberately emits **no** hidden `false` fallback (`serde_urlencoded` — used
/// by axum's `Form` and [`ChangesetForm`] — rejects duplicate keys, so a
/// hidden sibling would 400 every *checked* submission). An unchecked box
/// therefore submits no key at all, and the struct the form posts into must
/// decode a missing key as `false` via `#[serde(default)]` on the `bool`
/// field. The `#[model]`-generated `NewX` insert struct already does this for
/// non-nullable `bool` columns (matching the scaffold's `{Model}Form`
/// convention); hand-written form targets must add the attribute themselves —
/// see [`checkbox_input`]'s documentation.
///
/// # Datetime fields and `datetime-local` values
///
/// A [`FieldControl::DateTime`] field renders via [`datetime_input`], whose
/// `<input type="datetime-local">` has no timezone concept: the pre-filled
/// value is offsetless (`YYYY-MM-DDTHH:MM:SS`) and the browser posts back an
/// offsetless string — not always with seconds. Chrono's default
/// `Deserialize` for `DateTime<Utc>` demands an RFC 3339 offset, so a plain
/// field would reject even an *untouched* submission as a 400 before
/// validation. The `#[model]`-generated `NewX` insert struct therefore
/// attaches [`deserialize_datetime_local_utc`] (or the `_option` variant) to
/// `DateTime<Utc>` columns, [`deserialize_datetime_local_local`] (or its
/// `_option` variant) to `DateTime<Local>` columns, and
/// [`deserialize_naive_datetime_local`] (or its `_option` variant) to
/// `NaiveDateTime` columns: the offsetless value is interpreted as UTC or
/// the server's local zone respectively (see
/// [`deserialize_datetime_local_local`] for the DST edge cases), and
/// RFC 3339 JSON API bodies posted to the same struct keep decoding (an
/// explicit offset is converted to the field's zone).
///
/// A `DateTime` column with any **other** zone parameter (e.g.
/// `DateTime<FixedOffset>`, or a bare `DateTime` alias whose zone the derive
/// can't see) does *not* get the `datetime-local` picker: an offsetless
/// wall clock is genuinely ambiguous for such a zone, so inventing an
/// interpretation would silently shift instants. The derived [`FormModel`]
/// falls back to [`FieldControl::Text`] for those columns — the pre-filled
/// value is the field's serialized RFC 3339 string, which chrono's default
/// `Deserialize` round-trips as-is.
///
/// A required [`FieldControl::Date`] needs no such treatment — the browser's
/// `YYYY-MM-DD` wire shape is exactly what chrono's `NaiveDate`
/// `Deserialize` accepts. Hand-written form targets must attach the
/// deserializers themselves — see each helper's documentation.
///
/// # Serde-renamed fields
///
/// A model column carrying `#[serde(rename = "...")]` (or covered by a
/// struct-level `#[serde(rename_all = "...")]`) serializes under a key that
/// differs from its Rust identifier, and [`Changeset::field_value`] — which
/// pre-fills every control by serializing the changeset's data — indexes by
/// that *serialized* key. The rendered input `name`, however, must stay the
/// Rust identifier: the `#[model]`-generated `NewX`/`UpdateX` structs the
/// form posts into do **not** propagate serde renames, so they decode by
/// identifier. [`FormField`] therefore carries the two keys separately:
/// [`FormField::name`] is the input `name`/`id`/error-lookup key, and
/// [`FormField::value_name`] is the serde-effective serialized key used only
/// for the pre-fill lookup. The `#[model]`-derived [`FormModel`] resolves
/// field-level `rename`/`rename(serialize = ...)` and struct-level
/// `rename_all` automatically; hand-written [`FormModel`] impls whose data
/// type renames fields must call [`FormField::with_value_name`] themselves.
/// Validation errors stay keyed by the Rust identifier (the `validator`
/// crate reports the field ident, and the generated `NewX` has no renames).
///
/// # Example
///
/// ```rust
/// use autumn_web::form::{Changeset, FieldControl, FormField, FormModel, form_for};
/// use serde::Serialize;
///
/// #[derive(Serialize)]
/// struct Post {
///     title: String,
///     views: i32,
///     published: bool,
/// }
///
/// impl FormModel for Post {
///     fn form_fields() -> Vec<FormField> {
///         vec![
///             FormField::new("title", "Title", FieldControl::Text, true),
///             FormField::new("views", "Views", FieldControl::Number { step: None }, false),
///             FormField::new("published", "Published", FieldControl::Checkbox, false),
///         ]
///     }
/// }
///
/// let changeset = Changeset::new(Post { title: "Hello".into(), views: 0, published: false });
/// let html = form_for(&changeset, "/posts", "post")
///     .csrf("tok")
///     .render()
///     .into_string();
///
/// assert!(html.contains("<form"));
/// assert!(html.contains(r#"name="_csrf""#));
/// assert!(html.contains(r#"name="title""#));
/// assert!(html.contains(r#"name="views""#));
/// assert!(html.contains(r#"name="published""#));
/// assert!(html.contains(r#"type="submit""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn form_for<T>(
    changeset: &Changeset<T>,
    action: impl Into<String>,
    method: impl Into<String>,
) -> FormFor<'_, T>
where
    T: Serialize + FormModel,
{
    FormFor {
        changeset,
        action: action.into(),
        method: method.into(),
        csrf_token: None,
        csrf_field_name: "_csrf".to_string(),
        excluded: Vec::new(),
        field_overrides: Vec::new(),
        label_overrides: Vec::new(),
        prepended: Vec::new(),
        appended: Vec::new(),
        submit_label: "Save".to_string(),
        force_multipart: false,
    }
}

/// Builder returned by [`form_for`]; call [`FormFor::render`] to produce the
/// final `<form>` markup.
#[cfg(feature = "maud")]
pub struct FormFor<'a, T: Serialize + FormModel> {
    changeset: &'a Changeset<T>,
    action: String,
    method: String,
    csrf_token: Option<String>,
    csrf_field_name: String,
    excluded: Vec<String>,
    field_overrides: Vec<(String, FieldControl)>,
    label_overrides: Vec<(String, String)>,
    prepended: Vec<maud::Markup>,
    appended: Vec<maud::Markup>,
    submit_label: String,
    force_multipart: bool,
}

#[cfg(feature = "maud")]
impl<T: Serialize + FormModel> FormFor<'_, T> {
    /// Inject a hidden CSRF input carrying `token`.
    #[must_use]
    pub fn csrf(mut self, token: impl Into<String>) -> Self {
        self.csrf_token = Some(token.into());
        self
    }

    /// Override the hidden CSRF field name (default `"_csrf"`).
    #[must_use]
    pub fn csrf_field_name(mut self, name: impl Into<String>) -> Self {
        self.csrf_field_name = name.into();
        self
    }

    /// Drop `field` from the rendered form.
    #[must_use]
    pub fn exclude(mut self, field: impl Into<String>) -> Self {
        self.excluded.push(field.into());
        self
    }

    /// Replace the derived control for `field`. Calling this again for the
    /// same field replaces the earlier override (last call wins).
    #[must_use]
    pub fn override_field(mut self, field: impl Into<String>, control: FieldControl) -> Self {
        self.field_overrides.push((field.into(), control));
        self
    }

    /// Replace the label for `field`. Calling this again for the same field
    /// replaces the earlier override (last call wins).
    #[must_use]
    pub fn override_label(mut self, field: impl Into<String>, label: impl Into<String>) -> Self {
        self.label_overrides.push((field.into(), label.into()));
        self
    }

    /// Insert extra markup before the submit button.
    #[must_use]
    pub fn append(mut self, markup: maud::Markup) -> Self {
        self.appended.push(markup);
        self
    }

    /// Insert extra markup at the front of the form, immediately after the
    /// hidden CSRF/method-override inputs and before every derived field.
    ///
    /// Use this for hidden control fields — such as a one-time
    /// `_submit_token` — that a body-scanning middleware must find near the
    /// start of the URL-encoded body. A large earlier field (e.g. a long
    /// `<textarea>`) would otherwise push an appended token past the scan cap,
    /// leaving the request effectively token-less.
    #[must_use]
    pub fn prepend(mut self, markup: maud::Markup) -> Self {
        self.prepended.push(markup);
        self
    }

    /// Override the submit button label (default `"Save"`).
    #[must_use]
    pub fn submit_label(mut self, label: impl Into<String>) -> Self {
        self.submit_label = label.into();
        self
    }

    /// Force `enctype="multipart/form-data"` even when no field is a
    /// [`FieldControl::File`].
    #[must_use]
    pub const fn multipart(mut self) -> Self {
        self.force_multipart = true;
        self
    }

    /// Render the full `<form>` markup.
    #[must_use]
    pub fn render(self) -> maud::Markup {
        let Self {
            changeset,
            action,
            method,
            csrf_token,
            csrf_field_name,
            excluded,
            field_overrides,
            label_overrides,
            prepended,
            appended,
            submit_label,
            force_multipart,
        } = self;

        let fields = effective_form_fields::<T>(&excluded, &field_overrides, &label_overrides);
        let is_multipart = force_multipart
            || fields
                .iter()
                .any(|f| matches!(f.control, FieldControl::File));

        let inner = maud::html! {
            @for markup in prepended {
                (markup)
            }
            @for field in &fields {
                (render_form_field(changeset, field))
            }
            @for markup in appended {
                (markup)
            }
            button type="submit" { (submit_label) }
        };

        form_tag_inner(
            &action,
            &method,
            &csrf_field_name,
            csrf_token.as_deref(),
            is_multipart.then_some("multipart/form-data"),
            inner,
        )
    }
}

/// Compute the effective field list for a [`FormFor::render`] call: start
/// from `T::form_fields()`, drop excluded fields, then apply control/label
/// overrides in place. Overrides apply in registration order, so when the
/// same field is overridden twice the **last** call wins — conventional
/// builder semantics.
#[cfg(feature = "maud")]
fn effective_form_fields<T: FormModel>(
    excluded: &[String],
    field_overrides: &[(String, FieldControl)],
    label_overrides: &[(String, String)],
) -> Vec<FormField> {
    T::form_fields()
        .into_iter()
        .filter(|field| !excluded.contains(&field.name))
        .map(|mut field| {
            if let Some((_, control)) = field_overrides
                .iter()
                .rev()
                .find(|(name, _)| *name == field.name)
            {
                field.control = control.clone();
            }
            if let Some((_, label)) = label_overrides
                .iter()
                .rev()
                .find(|(name, _)| *name == field.name)
            {
                field.label = label.clone();
            }
            field
        })
        .collect()
}

/// Serialize adapter that re-exposes one serde-renamed field of `data`
/// under the descriptor's [`FormField::name`], so the typed-input helpers'
/// [`Changeset::field_value`] lookup (keyed by the rendered input name)
/// finds the value that `data` actually serializes under
/// [`FormField::value_name`].
///
/// Serializes as a single-entry map `{ exposed_name: data.serialized_name }`
/// (`null` when the serialized key is absent, which [`Changeset::field_value`]
/// renders as an empty value — same as any missing field today).
#[cfg(feature = "maud")]
struct PrefillAlias<'a, T> {
    /// The changeset's inner data (serialized in full, then re-keyed).
    data: &'a T,
    /// The serde-effective key `data` serializes the field under.
    serialized_name: &'a str,
    /// The key the rendering helpers look the value up by (`FormField::name`).
    exposed_name: &'a str,
}

#[cfg(feature = "maud")]
impl<T: Serialize> Serialize for PrefillAlias<'_, T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::{Error as _, SerializeMap as _};
        let value = serde_json::to_value(self.data).map_err(S::Error::custom)?;
        let field_value = value
            .get(self.serialized_name)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(self.exposed_name, &field_value)?;
        map.end()
    }
}

/// Wrap a primitive-rendered control (its own `<label>` + input) in the
/// changeset field skeleton shared by every routed control: the stable
/// `<div id="{field}-field" class="autumn-field">` htmx target and, when the
/// field has errors, the `role="alert"` inline-error block keyed to
/// `{field}-error`. This is the same wrapper/error markup the hand-written
/// `*_input` helpers emit; only the inner control now comes from an
/// `a11y` primitive.
#[cfg(feature = "maud")]
fn wrap_field_control(
    field_name: &str,
    control: impl maud::Render,
    errors: &[String],
) -> maud::Markup {
    let error_id = (!errors.is_empty()).then(|| format!("{field_name}-error"));
    let wrapper_id = format!("{field_name}-field");
    maud::html! {
        div id=(wrapper_id) class="autumn-field" {
            (control)
            @if !errors.is_empty() {
                div id=(error_id.as_deref().unwrap_or_default()) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (error) }
                    }
                }
            }
        }
    }
}

/// The `class` a routed control carries, mirroring the `*_input` helpers:
/// `autumn-field__input`, plus the `--invalid` modifier when the field errored.
#[cfg(feature = "maud")]
const fn field_control_class(has_errors: bool) -> &'static str {
    if has_errors {
        "autumn-field__input autumn-field__input--invalid"
    } else {
        "autumn-field__input"
    }
}

/// Render a single [`FormField`], routing the pre-fill lookup through the
/// field's serialized key ([`FormField::value_name`]) when it differs from
/// the rendered input name — see [`PrefillAlias`]. Everything else (input
/// `name`/`id`, error lookup) stays keyed by [`FormField::name`].
#[cfg(feature = "maud")]
fn render_form_field<T: Serialize>(changeset: &Changeset<T>, field: &FormField) -> maud::Markup {
    if let Some(value_name) = field
        .value_name
        .as_deref()
        .filter(|value_name| *value_name != field.name)
    {
        let aliased = Changeset::from_errors(
            PrefillAlias {
                data: changeset.data(),
                serialized_name: value_name,
                exposed_name: &field.name,
            },
            changeset.errors().clone(),
        );
        return render_form_control(&aliased, field);
    }
    render_form_control(changeset, field)
}

/// Dispatch a single [`FormField`] to the matching typed-input helper.
#[cfg(feature = "maud")]
fn render_form_control<T: Serialize>(changeset: &Changeset<T>, field: &FormField) -> maud::Markup {
    match &field.control {
        FieldControl::Text => {
            if field.required {
                required_text_input(changeset, &field.name, &field.label)
            } else {
                text_input(changeset, &field.name, &field.label)
            }
        }
        FieldControl::Textarea => {
            let errors = changeset.errors_for(&field.name);
            let has_errors = !errors.is_empty();
            let value = changeset.field_value(&field.name).unwrap_or_default();
            let mut control = crate::a11y::TextArea::new(field.name.as_str())
                .label(field.label.as_str())
                .label_class("autumn-field__label")
                .value(value)
                .class(field_control_class(has_errors))
                .aria_invalid(has_errors);
            if has_errors {
                control = control.described_by(format!("{}-error", field.name));
            }
            wrap_field_control(&field.name, control, errors)
        }
        FieldControl::Password => password_input(changeset, &field.name, &field.label),
        FieldControl::Number { step } => {
            if field.required {
                required_number_input(changeset, &field.name, &field.label, step.as_deref())
            } else {
                number_input(changeset, &field.name, &field.label, step.as_deref())
            }
        }
        FieldControl::Checkbox => {
            let errors = changeset.errors_for(&field.name);
            let has_errors = !errors.is_empty();
            let checked = changeset.field_value(&field.name).as_deref() == Some("true");
            let mut control = crate::a11y::Checkbox::new(field.name.as_str())
                .label(field.label.as_str())
                .label_class("autumn-field__label")
                .value("true")
                .checked(checked)
                .class(field_control_class(has_errors))
                .aria_invalid(has_errors);
            if has_errors {
                control = control.described_by(format!("{}-error", field.name));
            }
            wrap_field_control(&field.name, control, errors)
        }
        FieldControl::Date => {
            if field.required {
                required_date_input(changeset, &field.name, &field.label)
            } else {
                date_input(changeset, &field.name, &field.label)
            }
        }
        FieldControl::DateTime => {
            if field.required {
                required_datetime_input(changeset, &field.name, &field.label)
            } else {
                datetime_input(changeset, &field.name, &field.label)
            }
        }
        FieldControl::Select { options } => {
            let errors = changeset.errors_for(&field.name);
            let has_errors = !errors.is_empty();
            let current = changeset.field_value(&field.name).unwrap_or_default();
            let mut control = crate::a11y::Select::new(field.name.as_str())
                .label(field.label.as_str())
                .label_class("autumn-field__label")
                .options(options.iter().map(|(value, label)| {
                    crate::a11y::SelectOption::new(value.as_str(), label.as_str())
                }))
                .selected_value(current)
                .class(field_control_class(has_errors))
                .aria_invalid(has_errors);
            if field.required {
                control = control.required().aria_required();
            }
            if has_errors {
                control = control.described_by(format!("{}-error", field.name));
            }
            wrap_field_control(&field.name, control, errors)
        }
        FieldControl::File => {
            // A file input can't be value-prefilled (browser security), so the
            // typed [`crate::a11y::FileField`] primitive — which deliberately
            // carries no `value` — is a natural fit; it also drops the
            // competing derived `aria-label` in favour of the visible
            // `<label for=…>`. Multipart encoding is unchanged: the
            // `FieldControl::File` variant (not its rendering) still drives the
            // `is_multipart` gate on the parent form.
            let errors = changeset.errors_for(&field.name);
            let has_errors = !errors.is_empty();
            let mut control = crate::a11y::FileField::new(field.name.as_str())
                .label(field.label.as_str())
                .label_class("autumn-field__label")
                .class(field_control_class(has_errors))
                .aria_invalid(has_errors);
            if field.required {
                control = control.required().aria_required();
            }
            if has_errors {
                control = control.described_by(format!("{}-error", field.name));
            }
            wrap_field_control(&field.name, control, errors)
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── fast_escape ─────────────────────────────────────────────────

    #[test]
    fn fast_escape_no_special_chars_borrows() {
        let input = "ART-1042";
        match fast_escape(input) {
            std::borrow::Cow::Borrowed(s) => assert_eq!(s, input),
            std::borrow::Cow::Owned(_) => panic!("expected a borrow, no allocation needed"),
        }
    }

    #[test]
    fn fast_escape_empty_string_borrows() {
        assert!(matches!(fast_escape(""), std::borrow::Cow::Borrowed("")));
    }

    #[test]
    fn fast_escape_escapes_all_four_chars() {
        assert_eq!(fast_escape(r#"&<>""#), "&amp;&lt;&gt;&quot;");
    }

    #[test]
    fn fast_escape_leading_trailing_and_adjacent_specials() {
        assert_eq!(fast_escape("&start"), "&amp;start");
        assert_eq!(fast_escape("end&"), "end&amp;");
        assert_eq!(fast_escape("a&&b"), "a&amp;&amp;b");
        assert_eq!(
            fast_escape("<script>alert('x')</script>"),
            "&lt;script&gt;alert('x')&lt;/script&gt;"
        );
    }

    #[test]
    fn fast_escape_preserves_multibyte_utf8() {
        assert_eq!(fast_escape("café <3"), "café &lt;3");
        assert_eq!(fast_escape("日本語"), "日本語");
    }

    /// Naive per-byte reference matching `maud::escape::escape_to_string`'s
    /// documented behavior verbatim (that module is private to the `maud`
    /// crate, so it can't be called directly from here). `fast_escape` is
    /// checked against this obviously-correct-by-inspection reference
    /// rather than against its own bulk-copy logic.
    fn naive_escape_reference(input: &str) -> String {
        // Builds on raw bytes (like `maud`'s own implementation does) rather
        // than `char`, since casting a UTF-8 continuation/lead byte to `char`
        // and pushing it would re-encode it as a *different* multi-byte
        // sequence instead of preserving the original bytes.
        let mut out = Vec::with_capacity(input.len());
        for b in input.bytes() {
            match b {
                b'&' => out.extend_from_slice(b"&amp;"),
                b'<' => out.extend_from_slice(b"&lt;"),
                b'>' => out.extend_from_slice(b"&gt;"),
                b'"' => out.extend_from_slice(b"&quot;"),
                _ => out.push(b),
            }
        }
        String::from_utf8(out).expect("escaping ASCII-only replacements preserves valid UTF-8")
    }

    proptest::proptest! {
        /// `fast_escape` must be byte-for-byte identical to the naive
        /// reference for arbitrary input, including strings that mix the
        /// four special ASCII bytes with arbitrary (possibly multi-byte)
        /// Unicode text.
        #[test]
        fn fast_escape_matches_naive_reference(s in ".*") {
            proptest::prop_assert_eq!(fast_escape(&s).into_owned(), naive_escape_reference(&s));
        }
    }

    // ── Changeset::new ─────────────────────────────────────────────

    #[test]
    fn new_changeset_is_valid() {
        let cs = Changeset::new(42_i32);
        assert!(cs.is_valid());
    }

    #[test]
    fn new_changeset_has_no_errors() {
        let cs = Changeset::new("hello");
        assert!(cs.errors().is_empty());
    }

    #[test]
    fn new_changeset_into_inner() {
        let cs = Changeset::new(99_u8);
        assert_eq!(cs.into_inner(), 99);
    }

    #[test]
    fn new_changeset_data_ref() {
        let cs = Changeset::new(vec![1, 2, 3]);
        assert_eq!(cs.data(), &vec![1, 2, 3]);
    }

    // ── Changeset::from_errors ─────────────────────────────────────

    #[test]
    fn from_errors_changeset_is_invalid() {
        let mut errors = HashMap::new();
        errors.insert("name".to_string(), vec!["too short".to_string()]);
        let cs = Changeset::from_errors("data", errors);
        assert!(!cs.is_valid());
    }

    #[test]
    fn from_errors_returns_correct_field_errors() {
        let mut errors = HashMap::new();
        errors.insert("email".to_string(), vec!["invalid email".to_string()]);
        let cs = Changeset::from_errors("data", errors);
        assert_eq!(cs.errors_for("email"), &["invalid email"]);
    }

    #[test]
    fn errors_for_unknown_field_returns_empty_slice() {
        let cs = Changeset::new("data");
        assert!(cs.errors_for("nonexistent").is_empty());
    }

    #[test]
    fn from_errors_multiple_messages_per_field() {
        let mut errors = HashMap::new();
        errors.insert(
            "password".to_string(),
            vec!["too short".to_string(), "must contain a digit".to_string()],
        );
        let cs = Changeset::from_errors("data", errors);
        let msgs = cs.errors_for("password");
        assert_eq!(msgs.len(), 2);
        assert!(msgs.contains(&"too short".to_string()));
        assert!(msgs.contains(&"must contain a digit".to_string()));
    }

    // ── Changeset::into_valid ──────────────────────────────────────

    #[test]
    fn into_valid_returns_ok_when_valid() {
        let cs = Changeset::new(42_i32);
        assert_eq!(cs.into_valid().unwrap(), 42);
    }

    #[test]
    fn into_valid_returns_err_when_invalid() {
        let mut errors = HashMap::new();
        errors.insert("x".to_string(), vec!["err".to_string()]);
        let cs = Changeset::from_errors(42_i32, errors);
        assert!(cs.into_valid().is_err());
    }

    #[test]
    fn into_valid_err_preserves_changeset() {
        let mut errors = HashMap::new();
        errors.insert("name".to_string(), vec!["required".to_string()]);
        let cs = Changeset::from_errors(7_i32, errors);
        let err_cs = cs.into_valid().unwrap_err();
        assert_eq!(err_cs.into_inner(), 7);
    }

    // ── Changeset::field_value ─────────────────────────────────────

    #[test]
    fn field_value_returns_string_field() {
        #[derive(serde::Serialize)]
        struct Form {
            name: String,
        }
        let cs = Changeset::new(Form {
            name: "Alice".into(),
        });
        assert_eq!(cs.field_value("name"), Some("Alice".to_string()));
    }

    #[test]
    fn field_value_returns_number_as_string() {
        #[derive(serde::Serialize)]
        struct Form {
            age: u32,
        }
        let cs = Changeset::new(Form { age: 30 });
        assert_eq!(cs.field_value("age"), Some("30".to_string()));
    }

    #[test]
    fn field_value_returns_bool_as_string() {
        #[derive(serde::Serialize)]
        struct Form {
            active: bool,
        }
        let cs = Changeset::new(Form { active: true });
        assert_eq!(cs.field_value("active"), Some("true".to_string()));
    }

    #[test]
    fn field_value_returns_none_for_missing_field() {
        #[derive(serde::Serialize)]
        struct Form {
            name: String,
        }
        let cs = Changeset::new(Form {
            name: "Alice".into(),
        });
        assert_eq!(cs.field_value("email"), None);
    }

    #[test]
    fn field_value_after_errors_uses_submitted_data() {
        #[derive(serde::Serialize)]
        struct Form {
            name: String,
        }
        let mut errors = HashMap::new();
        errors.insert("name".to_string(), vec!["too short".to_string()]);
        let cs = Changeset::from_errors(Form { name: "ab".into() }, errors);
        assert_eq!(cs.field_value("name"), Some("ab".to_string()));
    }

    // ── IntoChangeset ──────────────────────────────────────────────

    #[test]
    fn into_changeset_valid_input_produces_no_errors() {
        #[derive(validator::Validate)]
        struct F {
            #[validate(length(min = 3))]
            name: String,
        }
        let cs = F {
            name: "Alice".into(),
        }
        .into_changeset();
        assert!(cs.is_valid());
        assert!(cs.errors_for("name").is_empty());
    }

    #[test]
    fn into_changeset_invalid_input_populates_errors() {
        #[derive(validator::Validate)]
        struct F {
            #[validate(length(min = 5))]
            name: String,
        }
        let cs = F { name: "ab".into() }.into_changeset();
        assert!(!cs.is_valid());
        assert!(!cs.errors_for("name").is_empty());
    }

    #[test]
    fn into_changeset_preserves_data_on_failure() {
        #[derive(validator::Validate)]
        struct F {
            #[validate(length(min = 5))]
            name: String,
        }
        let cs = F { name: "ab".into() }.into_changeset();
        assert_eq!(cs.data().name, "ab");
    }

    #[test]
    fn into_changeset_multiple_fields_errors() {
        #[derive(validator::Validate)]
        struct F {
            #[validate(length(min = 3))]
            name: String,
            #[validate(email)]
            email: String,
        }
        let cs = F {
            name: "a".into(),
            email: "not-email".into(),
        }
        .into_changeset();
        assert!(!cs.is_valid());
        assert!(!cs.errors_for("name").is_empty());
        assert!(!cs.errors_for("email").is_empty());
    }

    mod nested_validation {
        use super::*;
        use validator::Validate as _;

        #[derive(validator::Validate)]
        struct NestedAddress {
            #[validate(length(min = 3, message = "street too short"))]
            street: String,
        }

        #[derive(validator::Validate)]
        struct PersonWithAddress {
            #[validate(nested)]
            address: NestedAddress,
        }

        #[test]
        fn nested_struct_errors_are_flattened_with_dot_notation() {
            let cs = PersonWithAddress {
                address: NestedAddress { street: "x".into() },
            }
            .into_changeset();
            assert!(!cs.is_valid());
            assert!(!cs.errors_for("address.street").is_empty());
        }
    }

    // ── ChangesetForm helpers ──────────────────────────────────────

    #[test]
    fn changeset_form_blank_is_valid() {
        #[derive(validator::Validate, serde::Serialize)]
        struct F {
            #[validate(length(min = 1))]
            name: String,
        }
        let form = ChangesetForm::blank(F { name: "ok".into() }, "tok");
        assert!(form.is_valid()); // via Deref
        assert_eq!(form.csrf_token(), Some("tok"));
    }

    #[test]
    fn changeset_form_deref_exposes_changeset_methods() {
        #[derive(validator::Validate)]
        struct F {
            #[validate(length(min = 3))]
            name: String,
        }
        let changeset = F { name: "ab".into() }.into_changeset();
        let form = ChangesetForm {
            changeset,
            csrf_token: None,
            csrf_field: "_csrf".into(),
        };
        // Deref gives access to Changeset methods
        assert!(!form.is_valid());
        assert!(!form.errors_for("name").is_empty());
    }

    #[test]
    fn changeset_form_into_valid_ok() {
        #[derive(validator::Validate)]
        struct F {
            #[validate(length(min = 1))]
            name: String,
        }
        let form = ChangesetForm {
            changeset: F { name: "ok".into() }.into_changeset(),
            csrf_token: None,
            csrf_field: "_csrf".into(),
        };
        assert!(form.into_valid().is_ok());
    }

    #[test]
    fn changeset_form_into_valid_err_preserves_csrf() {
        #[derive(Debug, validator::Validate)]
        struct F {
            #[validate(length(min = 5))]
            name: String,
        }
        let form = ChangesetForm {
            changeset: F { name: "ab".into() }.into_changeset(),
            csrf_token: Some("tok123".into()),
            csrf_field: "_csrf".into(),
        };
        let err_form = form.into_valid().unwrap_err();
        assert_eq!(err_form.csrf_token(), Some("tok123"));
    }

    // ── Maud helpers ───────────────────────────────────────────────

    #[cfg(feature = "maud")]
    #[test]
    fn form_tag_renders_action_and_method() {
        let html = form_tag("/users", "post", None, maud::html! { "" }).into_string();
        assert!(html.contains(r#"action="/users""#), "{html}");
        assert!(html.contains(r#"method="post""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_tag_emits_csrf_hidden_input_when_token_provided() {
        let html = form_tag("/users", "post", Some("tok123"), maud::html! { "" }).into_string();
        assert!(html.contains(r#"name="_csrf""#), "{html}");
        assert!(html.contains(r#"value="tok123""#), "{html}");
        assert!(html.contains(r#"type="hidden""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_tag_omits_csrf_input_when_none() {
        let html = form_tag("/users", "post", None, maud::html! { "" }).into_string();
        assert!(!html.contains("_csrf"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_tag_includes_content() {
        let html = form_tag("/x", "post", None, maud::html! { span { "inner" } }).into_string();
        assert!(html.contains("inner"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_tag_emits_method_override_for_delete() {
        let html = form_tag("/posts/42", "delete", None, maud::html! { "" }).into_string();
        // Browser-facing method must be POST so native form submission works.
        assert!(html.contains(r#"method="post""#), "{html}");
        assert!(!html.contains(r#"method="delete""#), "{html}");
        // Hidden override field tells the autumn middleware to rewrite to DELETE.
        assert!(html.contains(r#"name="_method""#), "{html}");
        assert!(html.contains(r#"value="DELETE""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_tag_emits_method_override_for_put_and_patch() {
        let put_html = form_tag("/p/1", "put", None, maud::html! { "" }).into_string();
        assert!(put_html.contains(r#"method="post""#));
        assert!(put_html.contains(r#"value="PUT""#));

        let patch_html = form_tag("/p/1", "PATCH", None, maud::html! { "" }).into_string();
        assert!(patch_html.contains(r#"method="post""#));
        assert!(patch_html.contains(r#"value="PATCH""#));
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_tag_no_override_for_get_or_post() {
        let get_html = form_tag("/p", "get", None, maud::html! { "" }).into_string();
        assert!(!get_html.contains("_method"), "{get_html}");
        let post_html = form_tag("/p", "post", None, maud::html! { "" }).into_string();
        assert!(!post_html.contains("_method"), "{post_html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn method_input_emits_hidden_field_for_mutating_methods() {
        for method in ["PUT", "PATCH", "DELETE", "delete"] {
            let html = method_input(method).into_string();
            assert!(html.contains(r#"name="_method""#), "{html}");
            assert!(html.contains(r#"type="hidden""#), "{html}");
        }
    }

    #[cfg(feature = "maud")]
    #[test]
    fn method_input_is_empty_for_safe_or_unknown_methods() {
        assert_eq!(method_input("GET").into_string(), "");
        assert_eq!(method_input("POST").into_string(), "");
        assert_eq!(method_input("BREW").into_string(), "");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn changeset_form_form_tag_injects_stored_csrf() {
        #[derive(validator::Validate, serde::Serialize)]
        struct F {
            name: String,
        }
        let form = ChangesetForm::blank(
            F {
                name: String::new(),
            },
            "secret-token",
        );
        let html = form
            .form_tag("/x", "post", maud::html! { "" })
            .into_string();
        assert!(html.contains(r#"value="secret-token""#), "{html}");
        assert!(html.contains(r#"name="_csrf""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn changeset_form_form_tag_honours_custom_csrf_field_name() {
        #[derive(validator::Validate, serde::Serialize)]
        struct F {
            name: String,
        }
        let form = ChangesetForm {
            changeset: Changeset::new(F {
                name: String::new(),
            }),
            csrf_token: Some("tok".into()),
            csrf_field: "authenticity_token".into(),
        };
        let html = form
            .form_tag("/x", "post", maud::html! { "" })
            .into_string();
        assert!(html.contains(r#"name="authenticity_token""#), "{html}");
        assert!(!html.contains(r#"name="_csrf""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_renders_label_name_and_value() {
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let cs = Changeset::new(F {
            name: "Alice".into(),
        });
        let html = text_input(&cs, "name", "Full Name").into_string();
        assert!(html.contains(r#"name="name""#), "{html}");
        assert!(html.contains(r#"value="Alice""#), "{html}");
        assert!(html.contains("Full Name"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_aria_invalid_false_when_no_errors() {
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let cs = Changeset::new(F {
            name: "Alice".into(),
        });
        let html = text_input(&cs, "name", "Name").into_string();
        assert!(html.contains(r#"aria-invalid="false""#), "{html}");
        assert!(!html.contains(r#"role="alert""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_aria_invalid_true_and_error_block_on_failure() {
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let mut errors = HashMap::new();
        errors.insert("name".to_string(), vec!["too short".to_string()]);
        let cs = Changeset::from_errors(F { name: "ab".into() }, errors);
        let html = text_input(&cs, "name", "Name").into_string();
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("too short"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_error_block_has_describedby_link() {
        #[derive(serde::Serialize)]
        struct F {
            email: String,
        }
        let mut errors = HashMap::new();
        errors.insert("email".to_string(), vec!["invalid".to_string()]);
        let cs = Changeset::from_errors(F { email: "x".into() }, errors);
        let html = text_input(&cs, "email", "Email").into_string();
        assert!(html.contains("email-error"), "{html}");
        assert!(html.contains(r#"aria-describedby="email-error""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_multiple_errors_all_rendered() {
        #[derive(serde::Serialize)]
        struct F {
            password: String,
        }
        let mut errors = HashMap::new();
        errors.insert(
            "password".to_string(),
            vec!["too short".to_string(), "needs digit".to_string()],
        );
        let cs = Changeset::from_errors(
            F {
                password: "x".into(),
            },
            errors,
        );
        let html = text_input(&cs, "password", "Password").into_string();
        assert!(html.contains("too short"), "{html}");
        assert!(html.contains("needs digit"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn submit_button_renders_button_with_label() {
        let html = submit_button("Save").into_string();
        assert!(html.contains(r#"type="submit""#), "{html}");
        assert!(html.contains("Save"), "{html}");
    }

    // ── RED: accessible form helpers ───────────────────────────────

    #[cfg(feature = "maud")]
    #[test]
    fn password_input_renders_type_password() {
        #[derive(serde::Serialize)]
        struct F {
            password: String,
        }
        let cs = Changeset::new(F {
            password: String::new(),
        });
        let html = password_input(&cs, "password", "Password").into_string();
        assert!(html.contains(r#"type="password""#), "{html}");
        assert!(html.contains(r#"name="password""#), "{html}");
        assert!(html.contains("Password"), "{html}");
        // Must NOT expose the value in the rendered HTML
        assert!(!html.contains(r#"value=""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn password_input_emits_aria_invalid_on_error() {
        #[derive(serde::Serialize)]
        struct F {
            password: String,
        }
        let mut errors = HashMap::new();
        errors.insert("password".to_string(), vec!["too short".to_string()]);
        let cs = Changeset::from_errors(
            F {
                password: "x".into(),
            },
            errors,
        );
        let html = password_input(&cs, "password", "Password").into_string();
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("too short"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn textarea_input_renders_textarea_element() {
        #[derive(serde::Serialize)]
        struct F {
            bio: String,
        }
        let cs = Changeset::new(F {
            bio: "Hello world".into(),
        });
        let html = textarea_input(&cs, "bio", "Bio").into_string();
        assert!(html.contains("<textarea"), "{html}");
        assert!(html.contains(r#"name="bio""#), "{html}");
        assert!(html.contains(r#"id="bio""#), "{html}");
        assert!(html.contains("Bio"), "{html}");
        assert!(html.contains("Hello world"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn textarea_input_aria_invalid_on_error() {
        #[derive(serde::Serialize)]
        struct F {
            bio: String,
        }
        let mut errors = HashMap::new();
        errors.insert("bio".to_string(), vec!["required".to_string()]);
        let cs = Changeset::from_errors(F { bio: String::new() }, errors);
        let html = textarea_input(&cs, "bio", "Bio").into_string();
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("required"), "{html}");
    }

    // ── Rich text (issue #1255) ────────────────────────────────────────────

    #[test]
    fn strict_form_decode_fails_on_a_partially_filled_form() {
        // The premise of `field_from_urlencoded` (PR #2157 review): a live
        // preview `hx-include`s the WHOLE form, so on a fresh "new" page every
        // other field is still empty. Deserializing the whole form struct to
        // reach one field therefore fails on the first strictly-typed empty
        // field — and the preview would go blank until the author filled in
        // every unrelated required column.
        #[derive(serde::Deserialize)]
        struct PostForm {
            #[allow(dead_code)]
            body: String,
            #[allow(dead_code)]
            count: i64,
        }
        let partial = b"body=hello+%2A%2Aworld%2A%2A&count=";
        assert!(
            serde_urlencoded::from_bytes::<PostForm>(partial).is_err(),
            "whole-form decode must fail here — that is the bug being fixed"
        );
        // The lenient single-field extractor reaches the field regardless.
        assert_eq!(
            field_from_urlencoded(partial, "body").as_deref(),
            Some("hello **world**")
        );
    }

    #[test]
    fn field_from_urlencoded_decodes_percent_and_plus_escapes() {
        let body = b"title=hi&body=a+%2B+b+%26+%3Cc%3E&other=x";
        assert_eq!(
            field_from_urlencoded(body, "body").as_deref(),
            Some("a + b & <c>")
        );
        assert_eq!(field_from_urlencoded(body, "title").as_deref(), Some("hi"));
        assert_eq!(field_from_urlencoded(body, "absent"), None);
    }

    #[test]
    fn field_from_urlencoded_handles_empty_and_repeated_keys() {
        // An empty value is present-but-blank, not absent — a cleared editor
        // must preview as empty, not fall back to a stale value.
        assert_eq!(
            field_from_urlencoded(b"body=&x=1", "body").as_deref(),
            Some("")
        );
        // First occurrence wins, matching serde_urlencoded's own behaviour.
        assert_eq!(
            field_from_urlencoded(b"body=first&body=second", "body").as_deref(),
            Some("first")
        );
        assert_eq!(field_from_urlencoded(b"", "body"), None);
        // A key that merely *contains* the name must not match.
        assert_eq!(field_from_urlencoded(b"body_extra=no", "body"), None);
    }

    #[cfg(feature = "maud")]
    #[test]
    fn required_rich_text_area_signals_a_required_field() {
        // PR #2157 review: a non-nullable `body:richtext` column had no
        // required signal at all, unlike every other non-nullable generated
        // control. Both `String` deserialization and a `TEXT NOT NULL` column
        // accept `""`, so an empty editor persisted a blank body silently.
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: String::new(),
        });
        let html = required_rich_text_area(&cs, "body", "Body").into_string();
        assert!(html.contains("required"), "{html}");
        assert!(html.contains(r#"aria-required="true""#), "{html}");
        // Still the same editor otherwise.
        assert!(html.contains("<textarea"), "{html}");
        assert!(html.contains("autumn-rich-text"), "{html}");

        // The optional variant keeps its previous behaviour: leaving a nullable
        // column blank is valid, so no browser-native "please fill this out".
        let optional = rich_text_area(&cs, "body", "Body").into_string();
        assert!(!optional.contains(r#"aria-required="true""#), "{optional}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn required_rich_text_area_htmx_keeps_both_the_preview_and_the_signal() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: String::new(),
        });
        let html =
            required_rich_text_area_htmx(&cs, "body", "Body", "/posts/preview/body").into_string();
        assert!(html.contains(r#"aria-required="true""#), "{html}");
        assert!(html.contains(r#"hx-post="/posts/preview/body""#), "{html}");
        assert!(html.contains(r##"hx-target="#body-preview""##), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn rich_text_area_renders_a_labeled_markdown_editor() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: "Hello **world**".into(),
        });
        let html = rich_text_area(&cs, "body", "Body").into_string();
        assert!(html.contains("<textarea"), "{html}");
        assert!(html.contains(r#"name="body""#), "{html}");
        assert!(html.contains(r#"id="body""#), "{html}");
        assert!(html.contains(r#"<label for="body""#), "{html}");
        assert!(html.contains("Body"), "{html}");
        // The submitted Markdown source round-trips into the editor.
        assert!(html.contains("Hello **world**"), "{html}");
        // A hint tells the author the field takes Markdown — the whole point of
        // the control over a bare textarea.
        assert!(html.to_lowercase().contains("markdown"), "{html}");
        assert!(html.contains("autumn-rich-text"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn rich_text_area_renders_a_minimal_markdown_toolbar() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: String::new(),
        });
        let html = rich_text_area(&cs, "body", "Body").into_string();

        // A grouped, labeled toolbar — announced as one unit rather than as
        // loose text between the label and the editor.
        assert!(html.contains(r#"role="group""#), "{html}");
        assert!(html.contains("autumn-rich-text__toolbar"), "{html}");
        assert!(
            html.to_lowercase()
                .contains(r#"aria-label="markdown formatting"#),
            "{html}"
        );

        // Every syntax the allowlist actually renders is represented.
        for token in [
            "**bold**",
            "_italic_",
            "[text](url)",
            "- item",
            "# Heading",
            "> quote",
        ] {
            assert!(
                html.contains(&maud::html! { (token) }.into_string()),
                "toolbar is missing the {token:?} affordance:\n{html}"
            );
        }

        // The graceful base case: the toolbar must not depend on JavaScript,
        // and must not render a control that silently does nothing without it.
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("<button"), "{html}");
        assert!(!html.contains("onclick"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn rich_text_area_escapes_the_editor_value() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: "</textarea><script>alert(1)</script>".into(),
        });
        let html = rich_text_area(&cs, "body", "Body").into_string();
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("</textarea><script"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn rich_text_area_describes_itself_with_hint_and_errors() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: String::new(),
        });
        let ok = rich_text_area(&cs, "body", "Body").into_string();
        // Error-free: described by the hint alone, never a dangling error id.
        assert!(ok.contains(r#"aria-describedby="body-hint""#), "{ok}");
        assert!(ok.contains(r#"aria-invalid="false""#), "{ok}");

        let mut errors = HashMap::new();
        errors.insert("body".to_string(), vec!["must not be blank".to_string()]);
        let cs = Changeset::from_errors(
            F {
                body: String::new(),
            },
            errors,
        );
        let bad = rich_text_area(&cs, "body", "Body").into_string();
        assert!(
            bad.contains(r#"aria-describedby="body-hint body-error""#),
            "{bad}"
        );
        assert!(bad.contains(r#"aria-invalid="true""#), "{bad}");
        assert!(bad.contains(r#"role="alert""#), "{bad}");
        assert!(bad.contains("must not be blank"), "{bad}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn rich_text_area_htmx_wires_a_live_preview_with_no_javascript() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F { body: "hi".into() });
        let html = rich_text_area_htmx(&cs, "body", "Body", "/posts/preview/body").into_string();
        assert!(html.contains(r#"hx-post="/posts/preview/body""#), "{html}");
        assert!(html.contains(r##"hx-target="#body-preview""##), "{html}");
        assert!(html.contains(r#"hx-swap="innerHTML""#), "{html}");
        assert!(html.contains("hx-trigger="), "{html}");
        // The one-time submit token must not be spent by preview requests.
        assert!(html.contains(r#"hx-params="not _submit_token""#), "{html}");
        // The pane htmx swaps into. Deliberately not an `aria-live` region —
        // it re-renders on every pause in typing, and announcing the whole
        // rendered body back to its author is noise, not help.
        assert!(html.contains(r#"id="body-preview""#), "{html}");
        assert!(!html.contains("aria-live"), "{html}");
        assert!(html.contains(r#"role="region""#), "{html}");
        assert!(html.contains("aria-labelledby="), "{html}");
        // No inline JavaScript anywhere — htmx attributes only.
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("onkeyup"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn rich_text_area_htmx_with_token_field_filters_the_configured_field() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: String::new(),
        });
        let html = rich_text_area_htmx_with_token_field(
            &cs,
            "body",
            "Body",
            "/posts/preview/body",
            "_one_time",
        )
        .into_string();
        assert!(html.contains(r#"hx-params="not _one_time""#), "{html}");
        assert!(!html.contains("_submit_token"), "{html}");
    }

    #[cfg(all(feature = "maud", feature = "markdown"))]
    #[test]
    fn rich_text_area_htmx_preview_pane_is_prerendered_and_sanitized() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: "**bold** [x](javascript:alert(1))".into(),
        });
        let html = rich_text_area_htmx(&cs, "body", "Body", "/posts/preview/body").into_string();
        // A 422 re-render shows the preview immediately, without waiting for
        // the first keystroke to fire the htmx request…
        assert!(html.contains("<strong>bold</strong>"), "{html}");
        // …and that server-side pre-render goes through the same sanitizer.
        // Scoped to the preview pane: the textarea legitimately still holds the
        // raw Markdown source, escaped as text content, because the column
        // stores the source and the author must be able to edit it back.
        let pane = html
            .split_once(r#"id="body-preview""#)
            .expect("preview pane present")
            .1;
        assert!(!pane.to_ascii_lowercase().contains("javascript:"), "{pane}");
        assert!(!pane.contains("href="), "{pane}");
        // The editor keeps the source verbatim (escaped), not the render.
        assert!(
            html.contains("[x](javascript:alert(1))</textarea>"),
            "{html}"
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn changeset_form_exposes_the_rich_text_helpers() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let form = ChangesetForm {
            changeset: Changeset::new(F { body: "hi".into() }),
            csrf_token: None,
            csrf_field: "_csrf".to_owned(),
        };
        assert_eq!(
            form.rich_text_area("body", "Body").into_string(),
            rich_text_area(&form.changeset, "body", "Body").into_string()
        );
        assert_eq!(
            form.rich_text_area_htmx("body", "Body", "/p").into_string(),
            rich_text_area_htmx(&form.changeset, "body", "Body", "/p").into_string()
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn required_text_input_emits_aria_required() {
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let cs = Changeset::new(F {
            name: "Alice".into(),
        });
        let html = required_text_input(&cs, "name", "Name").into_string();
        assert!(html.contains(r#"aria-required="true""#), "{html}");
        assert!(html.contains("required"), "{html}");
        assert!(html.contains(r#"name="name""#), "{html}");
        assert!(html.contains("Name"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn required_text_input_preserves_error_handling() {
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let mut errors = HashMap::new();
        errors.insert("name".to_string(), vec!["required".to_string()]);
        let cs = Changeset::from_errors(
            F {
                name: String::new(),
            },
            errors,
        );
        let html = required_text_input(&cs, "name", "Name").into_string();
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains(r#"aria-required="true""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn aria_live_region_renders_role_status() {
        let html = aria_live_region("status-msg", "").into_string();
        assert!(html.contains(r#"role="status""#), "{html}");
        assert!(html.contains(r#"aria-live="polite""#), "{html}");
        assert!(html.contains(r#"id="status-msg""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn aria_live_region_renders_message_content() {
        let html = aria_live_region("status-msg", "Form submitted").into_string();
        assert!(html.contains("Form submitted"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn skip_link_renders_anchor_with_href() {
        let html = skip_link("#main-content", "Skip to main content").into_string();
        assert!(html.contains(r##"href="#main-content""##), "{html}");
        assert!(html.contains("Skip to main content"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn skip_link_has_visually_hidden_class_for_focus_reveal() {
        let html = skip_link("#main", "Skip").into_string();
        assert!(html.contains("skip-link"), "{html}");
    }

    // ── AC2: Stable wrapper IDs ────────────────────────────────────

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_wrapper_div_has_stable_id() {
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let cs = Changeset::new(F {
            name: "Alice".into(),
        });
        let html = text_input(&cs, "name", "Name").into_string();
        assert!(html.contains(r#"id="name-field""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn password_input_wrapper_div_has_stable_id() {
        #[derive(serde::Serialize)]
        struct F {
            password: String,
        }
        let cs = Changeset::new(F {
            password: String::new(),
        });
        let html = password_input(&cs, "password", "Password").into_string();
        assert!(html.contains(r#"id="password-field""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn textarea_input_wrapper_div_has_stable_id() {
        #[derive(serde::Serialize)]
        struct F {
            bio: String,
        }
        let cs = Changeset::new(F {
            bio: "Hello".into(),
        });
        let html = textarea_input(&cs, "bio", "Bio").into_string();
        assert!(html.contains(r#"id="bio-field""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn required_text_input_wrapper_div_has_stable_id() {
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let cs = Changeset::new(F {
            name: "Alice".into(),
        });
        let html = required_text_input(&cs, "name", "Name").into_string();
        assert!(html.contains(r#"id="name-field""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn required_number_input_emits_aria_required() {
        #[derive(serde::Serialize)]
        struct F {
            age: i32,
        }
        let cs = Changeset::new(F { age: 30 });
        let html = required_number_input(&cs, "age", "Age", Some("1")).into_string();
        assert!(html.contains(r#"aria-required="true""#), "{html}");
        assert!(html.contains("required"), "{html}");
        assert!(html.contains(r#"type="number""#), "{html}");
        assert!(html.contains(r#"name="age""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn required_number_input_preserves_error_handling() {
        #[derive(serde::Serialize)]
        struct F {
            age: i32,
        }
        let mut errors = HashMap::new();
        errors.insert("age".to_string(), vec!["must be positive".to_string()]);
        let cs = Changeset::from_errors(F { age: -1 }, errors);
        let html = required_number_input(&cs, "age", "Age", Some("1")).into_string();
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("must be positive"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn required_datetime_input_emits_aria_required() {
        #[derive(serde::Serialize)]
        struct F {
            scheduled_at: String,
        }
        let cs = Changeset::new(F {
            scheduled_at: "2026-01-01T12:00:00".into(),
        });
        let html = required_datetime_input(&cs, "scheduled_at", "Scheduled at").into_string();
        assert!(html.contains(r#"aria-required="true""#), "{html}");
        assert!(html.contains("required"), "{html}");
        assert!(html.contains(r#"type="datetime-local""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn required_select_input_emits_aria_required() {
        #[derive(serde::Serialize)]
        struct F {
            status: String,
        }
        let cs = Changeset::new(F {
            status: "draft".into(),
        });
        let html = required_select_input(
            &cs,
            "status",
            "Status",
            &[("draft", "Draft"), ("published", "Published")],
        )
        .into_string();
        assert!(html.contains(r#"aria-required="true""#), "{html}");
        assert!(html.contains("required"), "{html}");
        assert!(html.contains("<select"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn required_select_input_preserves_error_handling() {
        #[derive(serde::Serialize)]
        struct F {
            status: String,
        }
        let mut errors = HashMap::new();
        errors.insert("status".to_string(), vec!["is required".to_string()]);
        let cs = Changeset::from_errors(
            F {
                status: String::new(),
            },
            errors,
        );
        let html =
            required_select_input(&cs, "status", "Status", &[("draft", "Draft")]).into_string();
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("is required"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn required_text_input_htmx_emits_aria_required() {
        #[derive(serde::Serialize)]
        struct F {
            title: String,
        }
        let cs = Changeset::new(F {
            title: "Hello".into(),
        });
        let html =
            required_text_input_htmx(&cs, "title", "Title", "/posts/validate/title").into_string();
        assert!(html.contains(r#"aria-required="true""#), "{html}");
        assert!(html.contains("required"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn required_text_input_htmx_preserves_htmx_attributes() {
        #[derive(serde::Serialize)]
        struct F {
            title: String,
        }
        let cs = Changeset::new(F {
            title: String::new(),
        });
        let html =
            required_text_input_htmx(&cs, "title", "Title", "/posts/validate/title").into_string();
        assert!(
            html.contains(r#"hx-post="/posts/validate/title""#),
            "{html}"
        );
        assert!(html.contains(r#"hx-trigger="change""#), "{html}");
        assert!(
            html.contains(r#"hx-target="closest [data-autumn-field-wrapper]""#),
            "{html}"
        );
        assert!(html.contains(r#"hx-swap="outerHTML""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn required_text_input_htmx_preserves_error_handling() {
        #[derive(serde::Serialize)]
        struct F {
            title: String,
        }
        let mut errors = HashMap::new();
        errors.insert("title".to_string(), vec!["is required".to_string()]);
        let cs = Changeset::from_errors(
            F {
                title: String::new(),
            },
            errors,
        );
        let html =
            required_text_input_htmx(&cs, "title", "Title", "/posts/validate/title").into_string();
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("is required"), "{html}");
    }

    // ── AC2 + AC3: text_input_htmx ────────────────────────────────

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_htmx_wrapper_has_stable_id() {
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let cs = Changeset::new(F {
            name: "Alice".into(),
        });
        let html = text_input_htmx(&cs, "name", "Name", "/validate/name").into_string();
        assert!(html.contains(r#"id="name-field""#), "{html}");
        assert!(
            html.contains(r#"data-autumn-field-wrapper="name""#),
            "{html}"
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_htmx_renders_hx_post() {
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let cs = Changeset::new(F {
            name: "Alice".into(),
        });
        let html = text_input_htmx(&cs, "name", "Name", "/validate/name").into_string();
        assert!(html.contains(r#"hx-post="/validate/name""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_htmx_renders_hx_trigger_change() {
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let cs = Changeset::new(F {
            name: String::new(),
        });
        let html = text_input_htmx(&cs, "name", "Name", "/validate/name").into_string();
        assert!(html.contains(r#"hx-trigger="change""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_htmx_renders_hx_target_and_swap() {
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let cs = Changeset::new(F {
            name: String::new(),
        });
        let html = text_input_htmx(&cs, "name", "Name", "/validate/name").into_string();
        assert!(
            html.contains(r#"hx-target="closest [data-autumn-field-wrapper]""#),
            "{html}"
        );
        assert!(html.contains(r#"hx-swap="outerHTML""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_htmx_target_is_safe_for_nested_field_names() {
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let cs = Changeset::new(F {
            name: String::new(),
        });
        let html =
            text_input_htmx(&cs, "address.street", "Street", "/validate/street").into_string();
        assert!(html.contains(r#"id="address.street-field""#), "{html}");
        assert!(
            html.contains(r#"hx-target="closest [data-autumn-field-wrapper]""#),
            "{html}"
        );
        assert!(
            !html.contains("hx-target=\"#address.street-field\""),
            "{html}"
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_htmx_includes_all_form_fields() {
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let cs = Changeset::new(F {
            name: String::new(),
        });
        let html = text_input_htmx(&cs, "name", "Name", "/validate/name").into_string();
        assert!(html.contains(r#"hx-include="closest form""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_htmx_drops_submit_token_param() {
        // The inline-validation POST `hx-include`s the whole form, which carries
        // the hidden one-time `_submit_token` (issue #1360). `SubmitTokenLayer`
        // consumes any `_submit_token` a mutating POST carries, so validation
        // must filter it out via `hx-params` — otherwise the first field
        // validation spends the token and the real submit replays the fragment.
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let cs = Changeset::new(F {
            name: String::new(),
        });
        let plain = text_input_htmx(&cs, "name", "Name", "/validate/name").into_string();
        assert!(
            plain.contains(r#"hx-params="not _submit_token""#),
            "{plain}"
        );
        let required =
            required_text_input_htmx(&cs, "name", "Name", "/validate/name").into_string();
        assert!(
            required.contains(r#"hx-params="not _submit_token""#),
            "{required}"
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_htmx_with_token_field_filters_custom_field() {
        // When the app customizes `[security.submit_token].field_name`, the
        // `*_with_token_field` variants must exclude the configured field name
        // from the inline-validation POST (issue #1843), not the default.
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let cs = Changeset::new(F {
            name: String::new(),
        });
        let plain =
            text_input_htmx_with_token_field(&cs, "name", "Name", "/validate/name", "csrf_tok")
                .into_string();
        assert!(plain.contains(r#"hx-params="not csrf_tok""#), "{plain}");
        assert!(!plain.contains("_submit_token"), "{plain}");

        let required = required_text_input_htmx_with_token_field(
            &cs,
            "name",
            "Name",
            "/validate/name",
            "csrf_tok",
        )
        .into_string();
        assert!(
            required.contains(r#"hx-params="not csrf_tok""#),
            "{required}"
        );
        assert!(!required.contains("_submit_token"), "{required}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn with_token_field_default_matches_legacy_helpers() {
        // Passing the default field name reproduces the original hardcoded
        // `hx-params="not _submit_token"` behaviour, guarding backward compat.
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let cs = Changeset::new(F {
            name: String::new(),
        });
        assert_eq!(
            text_input_htmx_with_token_field(
                &cs,
                "name",
                "Name",
                "/validate/name",
                "_submit_token"
            )
            .into_string(),
            text_input_htmx(&cs, "name", "Name", "/validate/name").into_string(),
        );
        assert_eq!(
            required_text_input_htmx_with_token_field(
                &cs,
                "name",
                "Name",
                "/validate/name",
                "_submit_token",
            )
            .into_string(),
            required_text_input_htmx(&cs, "name", "Name", "/validate/name").into_string(),
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_htmx_valid_state_no_error_markup() {
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let cs = Changeset::new(F {
            name: "Alice".into(),
        });
        let html = text_input_htmx(&cs, "name", "Name", "/v").into_string();
        assert!(!html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains(r#"aria-invalid="false""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_htmx_invalid_preserves_value_and_shows_errors() {
        #[derive(serde::Serialize)]
        struct F {
            name: String,
        }
        let mut errors = HashMap::new();
        errors.insert("name".to_string(), vec!["too short".to_string()]);
        let cs = Changeset::from_errors(F { name: "ab".into() }, errors);
        let html = text_input_htmx(&cs, "name", "Name", "/v").into_string();
        assert!(html.contains(r#"value="ab""#), "{html}");
        assert!(html.contains("too short"), "{html}");
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn text_input_htmx_invalid_has_describedby_link() {
        #[derive(serde::Serialize)]
        struct F {
            email: String,
        }
        let mut errors = HashMap::new();
        errors.insert("email".to_string(), vec!["invalid".to_string()]);
        let cs = Changeset::from_errors(F { email: "x".into() }, errors);
        let html = text_input_htmx(&cs, "email", "Email", "/v").into_string();
        assert!(html.contains("email-error"), "{html}");
        assert!(html.contains(r#"aria-describedby="email-error""#), "{html}");
    }

    // ── Typed inputs: checkbox_input / number_input / date_input /
    //    datetime_input / select_input (issue #1131) ────────────────

    #[cfg(feature = "maud")]
    #[test]
    fn checkbox_input_renders_type_checkbox() {
        #[derive(serde::Serialize)]
        struct F {
            active: bool,
        }
        let cs = Changeset::new(F { active: false });
        let html = checkbox_input(&cs, "active", "Active").into_string();
        assert!(html.contains(r#"type="checkbox""#), "{html}");
        assert!(html.contains(r#"name="active""#), "{html}");
        assert!(html.contains("Active"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn checkbox_input_unchecked_when_value_false() {
        #[derive(serde::Serialize)]
        struct F {
            active: bool,
        }
        let cs = Changeset::new(F { active: false });
        let html = checkbox_input(&cs, "active", "Active").into_string();
        assert!(!html.contains("checked"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn checkbox_input_checked_when_value_true() {
        #[derive(serde::Serialize)]
        struct F {
            active: bool,
        }
        let cs = Changeset::new(F { active: true });
        let html = checkbox_input(&cs, "active", "Active").into_string();
        assert!(html.contains("checked"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn checkbox_input_never_emits_a_hidden_fallback() {
        // A hidden "false" sibling sharing the checkbox's `name` would make a
        // *checked* submission send the key twice (`field=false&field=true`).
        // serde_urlencoded rejects duplicate keys outright rather than taking
        // the last value, so every checked submission would 400. Unchecked
        // state must be recovered via `#[serde(default)]` on the target
        // field instead (see the function's doc comment) — never via a
        // hidden fallback input.
        #[derive(serde::Serialize)]
        struct F {
            active: bool,
        }
        let cs = Changeset::new(F { active: false });
        let html = checkbox_input(&cs, "active", "Active").into_string();
        assert!(!html.contains(r#"type="hidden""#), "{html}");
        assert_eq!(
            html.matches(r#"name="active""#).count(),
            1,
            "checkbox_input must emit exactly one input named `active` \
             (a second `name=\"active\"` sibling would duplicate the key \
             on submission): {html}"
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn checkbox_input_round_trips_through_real_url_decode_when_checked() {
        // Regression test for the duplicate-key 400: decode the *exact*
        // query string a browser sends for a CHECKED box rendered by
        // checkbox_input (i.e. only the fields checkbox_input itself
        // renders — no hidden sibling), through the same serde_urlencoded
        // machinery axum's `Form` extractor and ChangesetForm use.
        #[derive(serde::Serialize)]
        struct F {
            active: bool,
        }
        #[derive(serde::Deserialize)]
        struct Decoded {
            #[serde(default)]
            active: bool,
        }
        let cs = Changeset::new(F { active: true });
        let html = checkbox_input(&cs, "active", "Active").into_string();
        assert!(html.contains("checked"), "{html}");

        // A checked box submits `active=true` and nothing else.
        let decoded: Decoded = serde_urlencoded::from_str("active=true").unwrap();
        assert!(decoded.active);
    }

    #[cfg(feature = "maud")]
    #[test]
    fn checkbox_input_round_trips_through_real_url_decode_when_unchecked() {
        // An unchecked box submits no `active` key at all; `#[serde(default)]`
        // must recover `false` rather than erroring "missing field".
        #[derive(serde::Deserialize)]
        struct Decoded {
            #[serde(default)]
            active: bool,
        }
        let decoded: Decoded = serde_urlencoded::from_str("").unwrap();
        assert!(!decoded.active);
    }

    #[cfg(feature = "maud")]
    #[test]
    fn checkbox_input_wrapper_div_has_stable_id() {
        #[derive(serde::Serialize)]
        struct F {
            active: bool,
        }
        let cs = Changeset::new(F { active: false });
        let html = checkbox_input(&cs, "active", "Active").into_string();
        assert!(html.contains(r#"id="active-field""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn checkbox_input_emits_aria_invalid_and_errors() {
        #[derive(serde::Serialize)]
        struct F {
            active: bool,
        }
        let mut errors = HashMap::new();
        errors.insert("active".to_string(), vec!["must be true".to_string()]);
        let cs = Changeset::from_errors(F { active: false }, errors);
        let html = checkbox_input(&cs, "active", "Active").into_string();
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("must be true"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn number_input_renders_type_number() {
        #[derive(serde::Serialize)]
        struct F {
            age: i32,
        }
        let cs = Changeset::new(F { age: 30 });
        let html = number_input(&cs, "age", "Age", Some("1")).into_string();
        assert!(html.contains(r#"type="number""#), "{html}");
        assert!(html.contains(r#"name="age""#), "{html}");
        assert!(html.contains(r#"value="30""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn number_input_renders_step_when_provided() {
        #[derive(serde::Serialize)]
        struct F {
            price: f64,
        }
        let cs = Changeset::new(F { price: 9.99 });
        let html = number_input(&cs, "price", "Price", Some("0.01")).into_string();
        assert!(html.contains(r#"step="0.01""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn number_input_omits_step_when_none() {
        #[derive(serde::Serialize)]
        struct F {
            age: i32,
        }
        let cs = Changeset::new(F { age: 30 });
        let html = number_input(&cs, "age", "Age", None).into_string();
        assert!(!html.contains("step="), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn number_input_emits_aria_invalid_and_errors() {
        #[derive(serde::Serialize)]
        struct F {
            age: i32,
        }
        let mut errors = HashMap::new();
        errors.insert("age".to_string(), vec!["must be positive".to_string()]);
        let cs = Changeset::from_errors(F { age: -1 }, errors);
        let html = number_input(&cs, "age", "Age", None).into_string();
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("must be positive"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn number_input_wrapper_div_has_stable_id() {
        #[derive(serde::Serialize)]
        struct F {
            age: i32,
        }
        let cs = Changeset::new(F { age: 30 });
        let html = number_input(&cs, "age", "Age", None).into_string();
        assert!(html.contains(r#"id="age-field""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn date_input_renders_type_date() {
        #[derive(serde::Serialize)]
        struct F {
            born_on: String,
        }
        let cs = Changeset::new(F {
            born_on: "2024-03-15".into(),
        });
        let html = date_input(&cs, "born_on", "Born on").into_string();
        assert!(html.contains(r#"type="date""#), "{html}");
        assert!(html.contains(r#"value="2024-03-15""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn date_input_normalizes_full_timestamp_to_date_only() {
        #[derive(serde::Serialize)]
        struct F {
            born_on: String,
        }
        let cs = Changeset::new(F {
            born_on: "2024-03-15T10:30:00Z".into(),
        });
        let html = date_input(&cs, "born_on", "Born on").into_string();
        assert!(html.contains(r#"value="2024-03-15""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn date_input_normalizes_space_separated_datetime_to_date_only() {
        // `NaiveDateTime`'s `Display` (as opposed to its serde
        // serialization) uses a space separator, e.g. from a raw
        // `.to_string()` or some database drivers — accept it defensively
        // rather than falling through to the raw (browser-rejected) string.
        #[derive(serde::Serialize)]
        struct F {
            born_on: String,
        }
        let cs = Changeset::new(F {
            born_on: "2024-03-15 10:30:00".into(),
        });
        let html = date_input(&cs, "born_on", "Born on").into_string();
        assert!(html.contains(r#"value="2024-03-15""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn date_input_emits_aria_invalid_and_errors() {
        #[derive(serde::Serialize)]
        struct F {
            born_on: String,
        }
        let mut errors = HashMap::new();
        errors.insert("born_on".to_string(), vec!["required".to_string()]);
        let cs = Changeset::from_errors(
            F {
                born_on: String::new(),
            },
            errors,
        );
        let html = date_input(&cs, "born_on", "Born on").into_string();
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("required"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn datetime_input_renders_type_datetime_local() {
        #[derive(serde::Serialize)]
        struct F {
            starts_at: String,
        }
        let cs = Changeset::new(F {
            starts_at: "2024-03-15T10:30:00".into(),
        });
        let html = datetime_input(&cs, "starts_at", "Starts at").into_string();
        assert!(html.contains(r#"type="datetime-local""#), "{html}");
        // Seconds must be preserved, not truncated to minutes: chrono's
        // default Deserialize requires the seconds component, so a
        // minute-only value would fail to decode on an untouched submission.
        assert!(html.contains(r#"value="2024-03-15T10:30:00""#), "{html}");
        // `step="any"` so that value doesn't fail step-mismatch validation
        // (default step is minute-granularity) and block submission.
        assert!(html.contains(r#"step="any""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn datetime_input_normalizes_rfc3339_with_offset_to_local_shape() {
        #[derive(serde::Serialize)]
        struct F {
            starts_at: String,
        }
        let cs = Changeset::new(F {
            starts_at: "2024-03-15T10:30:00Z".into(),
        });
        let html = datetime_input(&cs, "starts_at", "Starts at").into_string();
        // Browsers reject a trailing `Z`/offset in a `datetime-local` value;
        // must be reduced to the bare local-shaped `YYYY-MM-DDTHH:MM:SS`.
        assert!(html.contains(r#"value="2024-03-15T10:30:00""#), "{html}");
        assert!(!html.contains(r#"value="2024-03-15T10:30:00Z""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn datetime_input_normalizes_space_separated_datetime() {
        // `NaiveDateTime`'s `Display` uses a space separator, e.g. from a
        // raw `.to_string()` or some database drivers — accept it
        // defensively rather than falling through to the raw string, which
        // `<input type="datetime-local">` (strictly requiring `T`) rejects.
        #[derive(serde::Serialize)]
        struct F {
            starts_at: String,
        }
        let cs = Changeset::new(F {
            starts_at: "2024-03-15 10:30:00".into(),
        });
        let html = datetime_input(&cs, "starts_at", "Starts at").into_string();
        assert!(html.contains(r#"value="2024-03-15T10:30:00""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn datetime_input_emits_aria_invalid_and_errors() {
        #[derive(serde::Serialize)]
        struct F {
            starts_at: String,
        }
        let mut errors = HashMap::new();
        errors.insert("starts_at".to_string(), vec!["required".to_string()]);
        let cs = Changeset::from_errors(
            F {
                starts_at: String::new(),
            },
            errors,
        );
        let html = datetime_input(&cs, "starts_at", "Starts at").into_string();
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("required"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn datetime_input_value_round_trips_through_chronos_default_deserialize() {
        // Regression test: chrono's default `serde::Deserialize` for
        // `NaiveDateTime` requires the seconds component ("premature end of
        // input" otherwise). A hand-written form using `datetime_input` with
        // a plain `chrono::NaiveDateTime` changeset field must be able to
        // submit the *pre-filled, untouched* value and have it decode
        // successfully — not just render without visible truncation.
        #[derive(serde::Serialize)]
        struct F {
            starts_at: chrono::NaiveDateTime,
        }
        #[derive(serde::Deserialize)]
        struct Decoded {
            starts_at: chrono::NaiveDateTime,
        }
        let stored = chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(10, 30, 56)
            .unwrap();
        let cs = Changeset::new(F { starts_at: stored });
        let html = datetime_input(&cs, "starts_at", "Starts at").into_string();

        // Extract the rendered value attribute and submit it exactly as a
        // browser would on a no-op edit (field untouched).
        let value = html
            .split("value=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .expect("value attribute present");
        let body = format!("starts_at={value}");
        let decoded: Decoded =
            serde_urlencoded::from_str(&body).expect("pre-filled value must decode");
        assert_eq!(decoded.starts_at, stored);
    }

    #[cfg(feature = "maud")]
    #[test]
    fn datetime_input_value_round_trips_for_utc_datetime_with_deserialize_helper() {
        // Regression test: a DateTime<Utc> field's datetime_input value has
        // no offset (datetime-local has no timezone concept), which chrono's
        // *default* Deserialize for DateTime<Utc> rejects. Confirm the
        // pre-filled value decodes successfully when the field is annotated
        // with deserialize_datetime_local_utc, as documented on
        // datetime_input.
        #[derive(serde::Serialize)]
        struct F {
            starts_at: chrono::DateTime<chrono::Utc>,
        }
        #[derive(serde::Deserialize)]
        struct Decoded {
            #[serde(deserialize_with = "deserialize_datetime_local_utc")]
            starts_at: chrono::DateTime<chrono::Utc>,
        }
        let stored = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
            chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
                .unwrap()
                .and_hms_opt(10, 30, 56)
                .unwrap(),
            chrono::Utc,
        );
        let cs = Changeset::new(F { starts_at: stored });
        let html = datetime_input(&cs, "starts_at", "Starts at").into_string();
        // The rendered value must not carry an offset/Z (would break the
        // datetime-local input); confirm that first.
        let value = html
            .split("value=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .expect("value attribute present");
        assert!(!value.contains('Z') && !value.contains('+'), "{value}");

        let body = format!("starts_at={value}");
        let decoded: Decoded =
            serde_urlencoded::from_str(&body).expect("pre-filled value must decode");
        assert_eq!(decoded.starts_at, stored);
    }

    #[cfg(feature = "maud")]
    #[test]
    fn deserialize_datetime_local_utc_pads_missing_seconds() {
        #[derive(serde::Deserialize)]
        struct Decoded {
            #[serde(deserialize_with = "deserialize_datetime_local_utc")]
            starts_at: chrono::DateTime<chrono::Utc>,
        }
        let decoded: Decoded = serde_urlencoded::from_str("starts_at=2024-03-15T10:30").unwrap();
        assert_eq!(
            decoded.starts_at,
            chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
                chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
                    .unwrap()
                    .and_hms_opt(10, 30, 0)
                    .unwrap(),
                chrono::Utc,
            )
        );
    }

    #[test]
    fn deserialize_datetime_local_utc_accepts_rfc3339_with_offset() {
        // The same struct often serves JSON API create bodies — the helper
        // must keep accepting RFC 3339, honoring an explicit offset by
        // converting to UTC.
        #[derive(serde::Deserialize)]
        struct Decoded {
            #[serde(deserialize_with = "deserialize_datetime_local_utc")]
            starts_at: chrono::DateTime<chrono::Utc>,
        }
        let expected = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
            chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
                .unwrap()
                .and_hms_opt(10, 30, 56)
                .unwrap(),
            chrono::Utc,
        );
        let decoded: Decoded =
            serde_json::from_str(r#"{"starts_at":"2024-03-15T10:30:56Z"}"#).unwrap();
        assert_eq!(decoded.starts_at, expected);
        // +09:00 wall clock 19:30:56 is the same instant.
        let decoded: Decoded =
            serde_json::from_str(r#"{"starts_at":"2024-03-15T19:30:56+09:00"}"#).unwrap();
        assert_eq!(decoded.starts_at, expected);
    }

    #[cfg(feature = "maud")]
    #[test]
    fn deserialize_datetime_local_utc_option_absent_key_is_none() {
        #[derive(serde::Deserialize)]
        struct Decoded {
            #[serde(default, deserialize_with = "deserialize_datetime_local_utc_option")]
            starts_at: Option<chrono::DateTime<chrono::Utc>>,
        }
        let decoded: Decoded = serde_urlencoded::from_str("").unwrap();
        assert_eq!(decoded.starts_at, None);
    }

    #[cfg(feature = "maud")]
    #[test]
    fn deserialize_datetime_local_utc_option_present_key_is_some() {
        #[derive(serde::Deserialize)]
        struct Decoded {
            #[serde(default, deserialize_with = "deserialize_datetime_local_utc_option")]
            starts_at: Option<chrono::DateTime<chrono::Utc>>,
        }
        let decoded: Decoded = serde_urlencoded::from_str("starts_at=2024-03-15T10:30:56").unwrap();
        assert_eq!(
            decoded.starts_at,
            Some(chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
                chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
                    .unwrap()
                    .and_hms_opt(10, 30, 56)
                    .unwrap(),
                chrono::Utc,
            ))
        );
    }

    #[test]
    fn deserialize_datetime_local_local_interprets_offsetless_as_local_wall_clock() {
        // The offsetless datetime-local shape names a wall clock in the
        // server's local zone; the decoded instant's local wall clock must
        // match it regardless of what TZ the test host runs in. Midday is
        // safely outside every real zone's DST transition window, so this is
        // deterministic without pinning `TZ`.
        #[derive(serde::Deserialize)]
        struct Decoded {
            #[serde(deserialize_with = "deserialize_datetime_local_local")]
            starts_at: chrono::DateTime<chrono::Local>,
        }
        let expected_wall_clock = chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(12, 30, 56)
            .unwrap();
        let decoded: Decoded =
            serde_json::from_str(r#"{"starts_at":"2024-03-15T12:30:56"}"#).unwrap();
        assert_eq!(decoded.starts_at.naive_local(), expected_wall_clock);

        // Seconds dropped by a minute-granularity picker are padded.
        let decoded: Decoded = serde_json::from_str(r#"{"starts_at":"2024-03-15T12:30"}"#).unwrap();
        assert_eq!(
            decoded.starts_at.naive_local(),
            chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
                .unwrap()
                .and_hms_opt(12, 30, 0)
                .unwrap()
        );
    }

    #[test]
    fn deserialize_datetime_local_local_accepts_rfc3339_with_offset() {
        // The same struct often serves JSON API create bodies — the helper
        // must keep accepting RFC 3339, honoring an explicit offset by
        // converting the instant to the local zone.
        #[derive(serde::Deserialize)]
        struct Decoded {
            #[serde(deserialize_with = "deserialize_datetime_local_local")]
            starts_at: chrono::DateTime<chrono::Local>,
        }
        let expected = chrono::DateTime::parse_from_rfc3339("2024-03-15T10:30:56Z")
            .unwrap()
            .with_timezone(&chrono::Local);
        let decoded: Decoded =
            serde_json::from_str(r#"{"starts_at":"2024-03-15T10:30:56Z"}"#).unwrap();
        assert_eq!(decoded.starts_at, expected);
        // +09:00 wall clock 19:30:56 is the same instant.
        let decoded: Decoded =
            serde_json::from_str(r#"{"starts_at":"2024-03-15T19:30:56+09:00"}"#).unwrap();
        assert_eq!(decoded.starts_at, expected);
    }

    #[cfg(feature = "maud")]
    #[test]
    fn deserialize_datetime_local_local_option_absent_and_empty_are_none() {
        #[derive(serde::Deserialize)]
        struct Decoded {
            #[serde(default, deserialize_with = "deserialize_datetime_local_local_option")]
            starts_at: Option<chrono::DateTime<chrono::Local>>,
        }
        let decoded: Decoded = serde_urlencoded::from_str("").unwrap();
        assert_eq!(decoded.starts_at, None);
        let decoded: Decoded = serde_urlencoded::from_str("starts_at=").unwrap();
        assert_eq!(decoded.starts_at, None);
        let decoded: Decoded = serde_urlencoded::from_str("starts_at=2024-03-15T12:30:56").unwrap();
        assert_eq!(
            decoded.starts_at.map(|dt| dt.naive_local()),
            Some(
                chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
                    .unwrap()
                    .and_hms_opt(12, 30, 56)
                    .unwrap()
            )
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn deserialize_naive_datetime_local_pads_missing_seconds() {
        // Regression test: a value the user actively edits through the
        // native picker isn't guaranteed to include seconds, unlike the
        // always-seconds-inclusive pre-filled value — this must not 400.
        #[derive(serde::Deserialize)]
        struct Decoded {
            #[serde(deserialize_with = "deserialize_naive_datetime_local")]
            starts_at: chrono::NaiveDateTime,
        }
        let decoded: Decoded = serde_urlencoded::from_str("starts_at=2024-03-15T10:30").unwrap();
        assert_eq!(
            decoded.starts_at,
            chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
                .unwrap()
                .and_hms_opt(10, 30, 0)
                .unwrap()
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn deserialize_naive_datetime_local_preserves_seconds_when_present() {
        #[derive(serde::Deserialize)]
        struct Decoded {
            #[serde(deserialize_with = "deserialize_naive_datetime_local")]
            starts_at: chrono::NaiveDateTime,
        }
        let decoded: Decoded = serde_urlencoded::from_str("starts_at=2024-03-15T10:30:56").unwrap();
        assert_eq!(
            decoded.starts_at,
            chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
                .unwrap()
                .and_hms_opt(10, 30, 56)
                .unwrap()
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn deserialize_naive_datetime_local_option_absent_key_is_none() {
        #[derive(serde::Deserialize)]
        struct Decoded {
            #[serde(default, deserialize_with = "deserialize_naive_datetime_local_option")]
            starts_at: Option<chrono::NaiveDateTime>,
        }
        let decoded: Decoded = serde_urlencoded::from_str("").unwrap();
        assert_eq!(decoded.starts_at, None);
    }

    #[cfg(feature = "maud")]
    #[test]
    fn deserialize_naive_datetime_local_option_present_key_is_some() {
        #[derive(serde::Deserialize)]
        struct Decoded {
            #[serde(default, deserialize_with = "deserialize_naive_datetime_local_option")]
            starts_at: Option<chrono::NaiveDateTime>,
        }
        let decoded: Decoded = serde_urlencoded::from_str("starts_at=2024-03-15T10:30").unwrap();
        assert_eq!(
            decoded.starts_at,
            Some(
                chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
                    .unwrap()
                    .and_hms_opt(10, 30, 0)
                    .unwrap()
            )
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn datetime_input_wrapper_div_has_stable_id() {
        #[derive(serde::Serialize)]
        struct F {
            starts_at: String,
        }
        let cs = Changeset::new(F {
            starts_at: "2024-03-15T10:30:00".into(),
        });
        let html = datetime_input(&cs, "starts_at", "Starts at").into_string();
        assert!(html.contains(r#"id="starts_at-field""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn select_input_renders_select_element_with_options() {
        #[derive(serde::Serialize)]
        struct F {
            status: String,
        }
        let cs = Changeset::new(F {
            status: "draft".into(),
        });
        let options = [("draft", "Draft"), ("published", "Published")];
        let html = select_input(&cs, "status", "Status", &options).into_string();
        assert!(html.contains("<select"), "{html}");
        assert!(html.contains(r#"name="status""#), "{html}");
        assert!(html.contains(r#"value="draft""#), "{html}");
        assert!(html.contains("Draft"), "{html}");
        assert!(html.contains(r#"value="published""#), "{html}");
        assert!(html.contains("Published"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn select_input_marks_current_value_selected() {
        #[derive(serde::Serialize)]
        struct F {
            status: String,
        }
        let cs = Changeset::new(F {
            status: "published".into(),
        });
        let options = [("draft", "Draft"), ("published", "Published")];
        let html = select_input(&cs, "status", "Status", &options).into_string();
        assert!(html.contains(r#"value="published" selected"#), "{html}");
        assert!(!html.contains(r#"value="draft" selected"#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn select_input_emits_aria_invalid_and_errors() {
        #[derive(serde::Serialize)]
        struct F {
            status: String,
        }
        let mut errors = HashMap::new();
        errors.insert("status".to_string(), vec!["required".to_string()]);
        let cs = Changeset::from_errors(
            F {
                status: String::new(),
            },
            errors,
        );
        let options = [("draft", "Draft"), ("published", "Published")];
        let html = select_input(&cs, "status", "Status", &options).into_string();
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("required"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn select_input_wrapper_div_has_stable_id() {
        #[derive(serde::Serialize)]
        struct F {
            status: String,
        }
        let cs = Changeset::new(F {
            status: "draft".into(),
        });
        let options = [("draft", "Draft"), ("published", "Published")];
        let html = select_input(&cs, "status", "Status", &options).into_string();
        assert!(html.contains(r#"id="status-field""#), "{html}");
    }

    // ── form_for (issue #1135 phase 2) ──────────────────────────────

    #[cfg(feature = "maud")]
    #[derive(serde::Serialize)]
    struct FormForTestModel {
        title: String,
        views: i32,
        published: bool,
    }

    #[cfg(feature = "maud")]
    impl FormModel for FormForTestModel {
        fn form_fields() -> Vec<FormField> {
            vec![
                FormField::new("title", "Title", FieldControl::Text, true),
                FormField::new("views", "Views", FieldControl::Number { step: None }, false),
                FormField::new("published", "Published", FieldControl::Checkbox, false),
            ]
        }
    }

    #[cfg(feature = "maud")]
    fn blank_form_for_model() -> FormForTestModel {
        FormForTestModel {
            title: String::new(),
            views: 0,
            published: false,
        }
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_for_prefills_serde_renamed_field_via_value_name() {
        // The data type serializes `title` under "headline" — field_value
        // indexes serialized output, so without value_name routing the
        // pre-fill would come back blank even though the data is present.
        #[derive(serde::Serialize)]
        struct RenamedModel {
            #[serde(rename = "headline")]
            title: String,
        }
        impl FormModel for RenamedModel {
            fn form_fields() -> Vec<FormField> {
                vec![
                    FormField::new("title", "Title", FieldControl::Text, true)
                        .with_value_name("headline"),
                ]
            }
        }

        let mut errors = HashMap::new();
        errors.insert("title".to_string(), vec!["too short".to_string()]);
        let cs = Changeset::from_errors(
            RenamedModel {
                title: "Hello".into(),
            },
            errors,
        );
        let html = form_for(&cs, "/posts", "post").render().into_string();

        // The POST key / id stays the Rust identifier…
        assert!(html.contains(r#"name="title""#), "{html}");
        assert!(!html.contains(r#"name="headline""#), "{html}");
        // …the value is pre-filled through the serialized key…
        assert!(html.contains(r#"value="Hello""#), "{html}");
        // …and errors stay keyed by the identifier.
        assert!(html.contains("too short"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_for_renders_form_tag_csrf_and_submit() {
        let cs = Changeset::new(blank_form_for_model());
        let html = form_for(&cs, "/posts", "post")
            .csrf("tok123")
            .render()
            .into_string();
        assert!(html.contains("<form"), "{html}");
        assert!(html.contains(r#"name="_csrf""#), "{html}");
        assert!(html.contains(r#"value="tok123""#), "{html}");
        assert!(html.contains(r#"type="submit""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_for_method_override_emits_hidden_input() {
        let cs = Changeset::new(blank_form_for_model());
        let html = form_for(&cs, "/posts/1", "PUT").render().into_string();
        assert!(html.contains(r#"name="_method""#), "{html}");
        assert!(html.contains(r#"value="PUT""#), "{html}");
        assert!(html.contains(r#"method="post""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_for_renders_one_control_per_field() {
        let cs = Changeset::new(blank_form_for_model());
        let html = form_for(&cs, "/posts", "post").render().into_string();
        assert!(html.contains(r#"name="title""#), "{html}");
        assert!(html.contains(r#"name="views""#), "{html}");
        assert!(html.contains(r#"name="published""#), "{html}");
        assert!(html.contains(r#"type="checkbox""#), "{html}");
        assert!(html.contains(r#"type="number""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_for_prefills_values() {
        let cs = Changeset::new(FormForTestModel {
            title: "Hello".into(),
            ..blank_form_for_model()
        });
        let html = form_for(&cs, "/posts", "post").render().into_string();
        assert!(html.contains(r#"value="Hello""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_for_renders_inline_error_adjacent() {
        let mut errors = HashMap::new();
        errors.insert("title".to_string(), vec!["can't be blank".to_string()]);
        let cs = Changeset::from_errors(blank_form_for_model(), errors);
        let html = form_for(&cs, "/posts", "post").render().into_string();
        assert!(html.contains("can't be blank"), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_for_exclude_drops_field() {
        let cs = Changeset::new(blank_form_for_model());
        let html = form_for(&cs, "/posts", "post")
            .exclude("published")
            .render()
            .into_string();
        assert!(!html.contains(r#"name="published""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_for_override_field_changes_control() {
        let cs = Changeset::new(blank_form_for_model());
        let html = form_for(&cs, "/posts", "post")
            .override_field(
                "title",
                FieldControl::Select {
                    options: vec![("a".into(), "A".into())],
                },
            )
            .render()
            .into_string();
        assert!(html.contains("<select"), "{html}");
        assert!(html.contains(r#"name="title""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_for_override_label() {
        let cs = Changeset::new(blank_form_for_model());
        let html = form_for(&cs, "/posts", "post")
            .override_label("title", "Headline")
            .render()
            .into_string();
        assert!(html.contains("Headline"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_for_append_inserts_before_submit() {
        let cs = Changeset::new(blank_form_for_model());
        let html = form_for(&cs, "/posts", "post")
            .append(
                maud::html! { input type="password" name="confirm" aria-label="Confirm password"; },
            )
            .render()
            .into_string();
        let append_idx = html
            .find(r#"name="confirm""#)
            .expect("append markup missing");
        let submit_idx = html
            .find(r#"type="submit""#)
            .expect("submit button missing");
        assert!(append_idx < submit_idx, "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_for_prepend_inserts_before_fields() {
        // A prepended hidden field (e.g. a one-time submit token, issue #1360)
        // must render at the FRONT of the form — after the CSRF input but before
        // every derived field — so a body-scanning middleware finds it even when
        // an earlier field carries a large value.
        let cs = Changeset::new(blank_form_for_model());
        let html = form_for(&cs, "/posts", "post")
            .csrf("tok")
            .prepend(maud::html! { input type="hidden" name="_submit_token" value="st"; })
            .render()
            .into_string();
        let csrf_idx = html.find(r#"name="_csrf""#).expect("csrf input missing");
        let prepend_idx = html
            .find(r#"name="_submit_token""#)
            .expect("prepend markup missing");
        let first_field_idx = html.find(r#"name="title""#).expect("derived field missing");
        assert!(
            csrf_idx < prepend_idx && prepend_idx < first_field_idx,
            "prepended token must sit after csrf and before the first derived field: {html}"
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_for_required_date_field_renders_required_attribute() {
        let cs = Changeset::new(blank_form_for_model());
        // `title` is a required field; promoted to a Date control it must
        // keep the required signal (issue #1135 audit: `required` used to be
        // silently dropped for `FieldControl::Date`).
        let html = form_for(&cs, "/posts", "post")
            .override_field("title", FieldControl::Date)
            .render()
            .into_string();
        assert!(html.contains(r#"type="date""#), "{html}");
        assert!(html.contains(r#"aria-required="true""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_for_file_field_sets_multipart() {
        let cs = Changeset::new(blank_form_for_model());
        let html = form_for(&cs, "/posts", "post")
            .override_field("title", FieldControl::File)
            .render()
            .into_string();
        assert!(html.contains(r#"enctype="multipart/form-data""#), "{html}");
    }

    /// A multipart `PUT` form must carry the method-override hidden input,
    /// the CSRF hidden input, AND the `enctype` in the *same* rendered output
    /// — the multipart path flows through the same audited `form_tag_inner`
    /// as every other form, so none of the three can drift apart.
    #[cfg(feature = "maud")]
    #[test]
    fn form_for_multipart_put_emits_method_override_and_csrf_together() {
        let cs = Changeset::new(blank_form_for_model());
        let html = form_for(&cs, "/posts/1", "PUT")
            .csrf("tok456")
            .multipart()
            .render()
            .into_string();
        assert!(html.contains(r#"enctype="multipart/form-data""#), "{html}");
        assert!(html.contains(r#"method="post""#), "{html}");
        assert!(html.contains(r#"name="_method""#), "{html}");
        assert!(html.contains(r#"value="PUT""#), "{html}");
        assert!(html.contains(r#"name="_csrf""#), "{html}");
        assert!(html.contains(r#"value="tok456""#), "{html}");

        // The non-multipart sibling keeps the same combined contract.
        let cs = Changeset::new(blank_form_for_model());
        let html = form_for(&cs, "/posts/1", "PUT")
            .csrf("tok456")
            .render()
            .into_string();
        assert!(!html.contains("enctype"), "{html}");
        assert!(html.contains(r#"name="_method""#), "{html}");
        assert!(html.contains(r#"name="_csrf""#), "{html}");
    }

    /// `FieldControl::File` renders the same inline-error/ARIA/wrapper
    /// skeleton as the changeset-aware helpers: `{field}-field` wrapper id,
    /// `aria-invalid`/`aria-describedby`, a `role="alert"` error block, and
    /// the required signal.
    #[cfg(feature = "maud")]
    #[test]
    fn form_for_file_field_renders_errors_aria_and_required() {
        let mut errors = HashMap::new();
        errors.insert("title".to_string(), vec!["must be a PDF".to_string()]);
        let cs = Changeset::from_errors(blank_form_for_model(), errors);
        // `title` is required in the test model.
        let html = form_for(&cs, "/posts", "post")
            .override_field("title", FieldControl::File)
            .render()
            .into_string();
        assert!(html.contains(r#"type="file""#), "{html}");
        assert!(html.contains(r#"id="title-field""#), "{html}");
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains(r#"aria-describedby="title-error""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("must be a PDF"), "{html}");
        assert!(html.contains(r#"aria-required="true""#), "{html}");
        assert!(html.contains("required"), "{html}");

        // Error-free, non-required file field: no required signal, no alert.
        // (`title` — the only required field — is excluded so any
        // `aria-required` in the output could only come from the file arm.)
        let cs = Changeset::new(blank_form_for_model());
        let html = form_for(&cs, "/posts", "post")
            .exclude("title")
            .override_field("published", FieldControl::File)
            .render()
            .into_string();
        assert!(html.contains(r#"id="published-field""#), "{html}");
        assert!(html.contains(r#"aria-invalid="false""#), "{html}");
        assert!(!html.contains(r#"aria-required="true""#), "{html}");
    }

    /// Slice 3b (issue #1933): `render_form_control` routes the Textarea,
    /// Checkbox, Select, and File arms through the typed `a11y` primitives
    /// while preserving the `{field}-field` wrapper + `role="alert"` error
    /// skeleton. This pins the two intentional accessibility improvements the
    /// primitives introduce over the hand-written `*_input` helpers: (1) an
    /// error-free control omits the old empty `aria-describedby=""` (an IDREF
    /// referencing nothing), and (2) a visible-label checkbox emits the
    /// `<input>` before its `<label for=…>` (the conventional checkbox layout;
    /// the `for`/`id` association is unchanged).
    #[cfg(feature = "maud")]
    #[test]
    fn form_for_routes_controls_through_a11y_primitives() {
        // Error-free controls drop the empty `aria-describedby=""` no-op.
        let cs = Changeset::new(blank_form_for_model());
        let html = form_for(&cs, "/posts", "post")
            .exclude("views")
            .exclude("published")
            .override_field("title", FieldControl::Textarea)
            .render()
            .into_string();
        assert!(html.contains("<textarea"), "{html}");
        assert!(html.contains(r#"aria-invalid="false""#), "{html}");
        assert!(
            !html.contains(r#"aria-describedby="""#),
            "error-free textarea must not emit an empty aria-describedby: {html}"
        );

        // Checkbox: the visible label's `for`/`id` association is preserved and
        // the `<input>` precedes the `<label>`.
        let cs = Changeset::new(blank_form_for_model());
        let html = form_for(&cs, "/posts", "post")
            .exclude("views")
            .exclude("title")
            .render()
            .into_string();
        assert!(html.contains(r#"type="checkbox""#), "{html}");
        assert!(html.contains(r#"<label for="published""#), "{html}");
        let input_idx = html
            .find(r#"type="checkbox""#)
            .expect("checkbox input missing");
        let label_idx = html
            .find(r#"<label for="published""#)
            .expect("checkbox label missing");
        assert!(
            input_idx < label_idx,
            "checkbox input must precede its label (conventional layout): {html}"
        );
        assert!(
            !html.contains(r#"aria-describedby="""#),
            "error-free checkbox must not emit an empty aria-describedby: {html}"
        );
    }

    /// Duplicate `.override_field`/`.override_label` calls on the same field
    /// resolve last-wins (conventional builder semantics).
    #[cfg(feature = "maud")]
    #[test]
    fn form_for_duplicate_overrides_last_wins() {
        let cs = Changeset::new(blank_form_for_model());
        let html = form_for(&cs, "/posts", "post")
            .override_field("title", FieldControl::Date)
            .override_field("title", FieldControl::Textarea)
            .override_label("title", "First")
            .override_label("title", "Second")
            .render()
            .into_string();
        assert!(html.contains("<textarea"), "{html}");
        assert!(!html.contains(r#"type="date""#), "{html}");
        assert!(html.contains("Second"), "{html}");
        assert!(!html.contains("First"), "{html}");
    }

    // ── ChangesetForm extractor (axum integration) ─────────────────

    mod extractor_tests {
        use super::*;
        use axum::{Router, body::Body, routing::post};
        use tower::ServiceExt;

        #[derive(serde::Deserialize, validator::Validate)]
        struct TestForm {
            #[validate(length(min = 3))]
            name: String,
        }

        #[tokio::test]
        async fn valid_form_body_produces_valid_changeset() {
            async fn handler(form: ChangesetForm<TestForm>) -> String {
                format!("valid={}", form.is_valid())
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "name=Alice"))
                .await
                .unwrap();
            assert_body(resp, "valid=true").await;
        }

        #[tokio::test]
        async fn invalid_form_body_produces_invalid_changeset() {
            async fn handler(form: ChangesetForm<TestForm>) -> String {
                format!("valid={}", form.is_valid())
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "name=ab"))
                .await
                .unwrap();
            assert_body(resp, "valid=false").await;
        }

        #[tokio::test]
        async fn invalid_form_exposes_field_errors() {
            async fn handler(form: ChangesetForm<TestForm>) -> String {
                form.errors_for("name").join("|")
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "name=ab"))
                .await
                .unwrap();
            let body = body_text(resp).await;
            assert!(!body.is_empty(), "expected errors, got empty string");
        }

        #[tokio::test]
        async fn missing_required_field_returns_non_200() {
            async fn handler(form: ChangesetForm<TestForm>) -> String {
                format!("valid={}", form.is_valid())
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "other=value"))
                .await
                .unwrap();
            assert_ne!(resp.status(), axum::http::StatusCode::OK);
        }

        #[derive(serde::Deserialize, validator::Validate)]
        struct OptionalNumericForm {
            #[validate(length(min = 3))]
            name: String,
            age: Option<i32>,
        }

        #[tokio::test]
        async fn blank_optional_numeric_field_decodes_as_none_not_400() {
            // A number input the user left empty submits `age=` — an empty
            // string is not a valid `i32`, so `serde_urlencoded` rejects it
            // outright unless the extractor retries with the blank pair
            // dropped (falling back to `None`) instead of surfacing a
            // spurious decode failure that bypasses the Changeset entirely.
            async fn handler(form: ChangesetForm<OptionalNumericForm>) -> String {
                format!("valid={} age={:?}", form.is_valid(), form.data().age)
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "name=Alice&age="))
                .await
                .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            assert_body(resp, "valid=true age=None").await;
        }

        #[tokio::test]
        async fn filled_optional_numeric_field_still_decodes() {
            async fn handler(form: ChangesetForm<OptionalNumericForm>) -> String {
                format!("valid={} age={:?}", form.is_valid(), form.data().age)
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "name=Alice&age=30"))
                .await
                .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            assert_body(resp, "valid=true age=Some(30)").await;
        }

        #[tokio::test]
        async fn garbage_numeric_field_still_fails_decode() {
            // A genuinely undecodable value (not blank) must still 400/422 —
            // the blank-retry accommodation must not paper over real garbage.
            async fn handler(_form: ChangesetForm<OptionalNumericForm>) -> String {
                "unreachable".to_string()
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "name=Alice&age=not-a-number"))
                .await
                .unwrap();
            assert_ne!(resp.status(), axum::http::StatusCode::OK);
        }

        #[tokio::test]
        async fn blank_required_string_survives_alongside_blank_optional_numeric() {
            // A submission with *both* a blank optional numeric field and a
            // blank required/validated `String` field (`name=&age=`) must not
            // have the blank-optional-field retry drop the string pair too —
            // that would turn a real validation error (name too short) into a
            // spurious decode failure. `name` must reach the Changeset as an
            // empty string so `is_valid()` reports the actual length-rule
            // violation, not a 400/422 that hides it.
            async fn handler(form: ChangesetForm<OptionalNumericForm>) -> String {
                format!(
                    "valid={} name={:?} age={:?}",
                    form.is_valid(),
                    form.data().name,
                    form.data().age
                )
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "name=&age="))
                .await
                .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            assert_body(resp, "valid=false name=\"\" age=None").await;
        }

        #[derive(serde::Deserialize, validator::Validate)]
        struct CheckboxForm {
            #[serde(default)]
            published: bool,
        }

        #[tokio::test]
        async fn blank_defaulted_checkbox_field_decodes_to_its_default() {
            // Documented, intended behavior (see
            // `decode_urlencoded_dropping_blank_optional_fields`'s "Scope"
            // doc section): a `#[serde(default)] bool` field explicitly
            // submitted blank (as opposed to omitted, which is what a real
            // unchecked `<input type="checkbox">` actually sends) resolves
            // to the same default a client could already get by omitting
            // the key entirely — no new capability, just consistent with
            // the field's own declared tolerance for a missing key.
            async fn handler(form: ChangesetForm<CheckboxForm>) -> String {
                format!("published={}", form.data().published)
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "published="))
                .await
                .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            assert_body(resp, "published=false").await;
        }

        #[derive(serde::Deserialize, validator::Validate)]
        struct RequiredBoolForm {
            #[allow(
                dead_code,
                reason = "decode is expected to fail before this is ever read"
            )]
            accepted_terms: bool,
        }

        #[tokio::test]
        async fn blank_required_field_without_default_still_fails_to_decode() {
            // The boundary the "Scope" doc section on
            // `decode_urlencoded_dropping_blank_optional_fields` promises:
            // a field with *no* tolerance for a missing key at all (no
            // `Option`, no `#[serde(default)]`) must still hard-fail on a
            // blank submission — dropping its key surfaces a "missing
            // field" error on the next attempt instead of resolving, so
            // the function gives up and returns that error rather than
            // silently accepting a bogus value.
            async fn handler(_form: ChangesetForm<RequiredBoolForm>) -> String {
                "unreachable".to_string()
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "accepted_terms="))
                .await
                .unwrap();
            assert_ne!(resp.status(), axum::http::StatusCode::OK);
        }

        #[tokio::test]
        async fn oversized_body_is_rejected_not_fully_buffered() {
            // The blank-optional-field retry must not come at the cost of
            // bypassing axum's `DefaultBodyLimit` (2MB by default) — a body
            // larger than the configured limit must still be rejected rather
            // than fully buffered into memory.
            async fn handler(_form: ChangesetForm<TestForm>) -> String {
                "unreachable".to_string()
            }
            let oversized = format!("name={}", "a".repeat(3 * 1024 * 1024));
            let req = axum::http::Request::builder()
                .method("POST")
                .uri("/test")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(oversized))
                .unwrap();
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(req)
                .await
                .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        }

        #[tokio::test]
        async fn wrong_content_type_is_rejected_not_decoded_anyway() {
            // `Bytes` alone has no opinion on Content-Type, so a `text/plain`
            // (or missing-Content-Type) POST whose body happens to look like
            // `name=Alice` must still be rejected outright — matching axum's
            // own `Form`/`RawForm` extractors — rather than silently decoded.
            async fn handler(_form: ChangesetForm<TestForm>) -> String {
                "unreachable".to_string()
            }
            let req = axum::http::Request::builder()
                .method("POST")
                .uri("/test")
                .header("Content-Type", "text/plain")
                .body(Body::from("name=Alice"))
                .unwrap();
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(req)
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE
            );
        }

        #[tokio::test]
        async fn missing_content_type_is_rejected_not_decoded_anyway() {
            async fn handler(_form: ChangesetForm<TestForm>) -> String {
                "unreachable".to_string()
            }
            let req = axum::http::Request::builder()
                .method("POST")
                .uri("/test")
                .body(Body::from("name=Alice"))
                .unwrap();
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(req)
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE
            );
        }

        #[tokio::test]
        async fn csrf_token_is_none_without_csrf_middleware() {
            async fn handler(form: ChangesetForm<TestForm>) -> String {
                form.csrf_token().unwrap_or("none").to_string()
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "name=Alice"))
                .await
                .unwrap();
            assert_body(resp, "none").await;
        }

        #[tokio::test]
        async fn csrf_token_captured_from_request_extensions() {
            // Build a request with CsrfToken pre-inserted in extensions,
            // simulating what CsrfLayer does, then call from_request directly.
            use crate::security::CsrfToken;

            let mut req = axum::http::Request::builder()
                .method("POST")
                .uri("/test")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from("name=Alice"))
                .unwrap();
            req.extensions_mut()
                .insert(CsrfToken::new("secret-tok".to_string()));

            let form = ChangesetForm::<TestForm>::from_request(req, &())
                .await
                .expect("extraction should succeed");

            assert_eq!(form.csrf_token(), Some("secret-tok"));
        }

        #[cfg(feature = "multipart")]
        #[tokio::test]
        async fn multipart_form_decodes_text_fields() {
            async fn handler(form: ChangesetForm<TestForm>) -> String {
                format!("valid={} name={}", form.is_valid(), form.data().name)
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(multipart_req("/test", "name", "Alice"))
                .await
                .unwrap();
            assert_body(resp, "valid=true name=Alice").await;
        }

        #[cfg(feature = "multipart")]
        #[tokio::test]
        async fn multipart_blank_optional_numeric_field_decodes_as_none_not_400() {
            async fn handler(form: ChangesetForm<OptionalNumericForm>) -> String {
                format!("valid={} age={:?}", form.is_valid(), form.data().age)
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(multipart_req_multi(
                    "/test",
                    &[("name", "Alice"), ("age", "")],
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            assert_body(resp, "valid=true age=None").await;
        }

        #[cfg(feature = "multipart")]
        #[tokio::test]
        async fn multipart_form_validates_fields() {
            async fn handler(form: ChangesetForm<TestForm>) -> String {
                format!("valid={}", form.is_valid())
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(multipart_req("/test", "name", "ab"))
                .await
                .unwrap();
            assert_body(resp, "valid=false").await;
        }

        // ── AC3: Inline field validation (htmx partial response) ──

        #[derive(serde::Deserialize, validator::Validate, serde::Serialize)]
        struct InlineTestForm {
            #[validate(length(min = 3, message = "Name must be at least 3 characters"))]
            name: String,
        }

        #[cfg(feature = "maud")]
        #[tokio::test]
        async fn inline_valid_field_returns_field_partial_without_errors() {
            async fn handler(form: ChangesetForm<InlineTestForm>) -> maud::Markup {
                text_input_htmx(&form.changeset, "name", "Name", "/validate/name")
            }
            let resp = Router::new()
                .route("/validate/name", post(handler))
                .oneshot(urlencoded_req("/validate/name", "name=Alice"))
                .await
                .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            let body = body_text(resp).await;
            assert!(body.contains(r#"aria-invalid="false""#), "{body}");
            assert!(!body.contains(r#"role="alert""#), "{body}");
            assert!(body.contains(r#"value="Alice""#), "{body}");
        }

        #[cfg(feature = "maud")]
        #[tokio::test]
        async fn inline_invalid_field_returns_field_partial_with_errors() {
            async fn handler(form: ChangesetForm<InlineTestForm>) -> maud::Markup {
                text_input_htmx(&form.changeset, "name", "Name", "/validate/name")
            }
            let resp = Router::new()
                .route("/validate/name", post(handler))
                .oneshot(urlencoded_req("/validate/name", "name=ab"))
                .await
                .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            let body = body_text(resp).await;
            assert!(body.contains(r#"aria-invalid="true""#), "{body}");
            assert!(body.contains(r#"role="alert""#), "{body}");
            assert!(
                body.contains("Name must be at least 3 characters"),
                "{body}"
            );
            // Value preserved after failed validation
            assert!(body.contains(r#"value="ab""#), "{body}");
        }

        #[cfg(feature = "maud")]
        #[tokio::test]
        async fn inline_invalid_field_partial_is_htmx_swappable() {
            async fn handler(form: ChangesetForm<InlineTestForm>) -> maud::Markup {
                text_input_htmx(&form.changeset, "name", "Name", "/validate/name")
            }
            let resp = Router::new()
                .route("/validate/name", post(handler))
                .oneshot(urlencoded_req("/validate/name", "name=ab"))
                .await
                .unwrap();
            let body = body_text(resp).await;
            // Wrapper must have stable id for hx-swap="outerHTML" targeting
            assert!(body.contains(r#"id="name-field""#), "{body}");
        }

        #[cfg(feature = "maud")]
        #[tokio::test]
        async fn full_form_submit_invalid_returns_422() {
            async fn handler(form: ChangesetForm<InlineTestForm>) -> impl IntoResponse {
                match form.into_valid() {
                    Ok(_) => axum::http::StatusCode::OK.into_response(),
                    Err(form) => (
                        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                        text_input_htmx(&form.changeset, "name", "Name", "/validate/name"),
                    )
                        .into_response(),
                }
            }
            let resp = Router::new()
                .route("/submit", post(handler))
                .oneshot(urlencoded_req("/submit", "name=ab"))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                "full-form invalid submit must return 422"
            );
            let body = body_text(resp).await;
            assert!(
                body.contains("Name must be at least 3 characters"),
                "{body}"
            );
        }

        #[cfg(feature = "maud")]
        #[tokio::test]
        async fn full_form_submit_valid_returns_200() {
            async fn handler(form: ChangesetForm<InlineTestForm>) -> impl IntoResponse {
                match form.into_valid() {
                    Ok(_) => axum::http::StatusCode::OK.into_response(),
                    Err(form) => (
                        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                        text_input_htmx(&form.changeset, "name", "Name", "/validate/name"),
                    )
                        .into_response(),
                }
            }
            let resp = Router::new()
                .route("/submit", post(handler))
                .oneshot(urlencoded_req("/submit", "name=Alice"))
                .await
                .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
        }

        // ── Helpers ────────────────────────────────────────────────

        fn urlencoded_req(uri: &str, body: &'static str) -> axum::http::Request<Body> {
            axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap()
        }

        #[cfg(feature = "multipart")]
        fn multipart_req(uri: &str, field: &str, value: &str) -> axum::http::Request<Body> {
            multipart_req_multi(uri, &[(field, value)])
        }

        #[cfg(feature = "multipart")]
        fn multipart_req_multi(uri: &str, fields: &[(&str, &str)]) -> axum::http::Request<Body> {
            use std::fmt::Write as _;

            let boundary = "----FormBoundary7MA4YWxkTrZu0gW";
            let mut body = String::new();
            for (field, value) in fields {
                let _ = write!(
                    body,
                    "--{boundary}\r\n\
                     Content-Disposition: form-data; name=\"{field}\"\r\n\r\n\
                     {value}\r\n"
                );
            }
            let _ = write!(body, "--{boundary}--\r\n");
            axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header(
                    "Content-Type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap()
        }

        async fn body_text(resp: axum::response::Response) -> String {
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        }

        async fn assert_body(resp: axum::response::Response, expected: &str) {
            assert_eq!(body_text(resp).await, expected);
        }
    }
}
