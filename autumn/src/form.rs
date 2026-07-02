//! Changeset-style form helpers with validation and Maud rendering.
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

use std::collections::HashMap;

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
#[derive(Debug)]
pub struct Changeset<T> {
    data: T,
    errors: HashMap<String, Vec<String>>,
}

impl<T> Changeset<T> {
    /// Create a changeset with no errors (valid state).
    pub fn new(data: T) -> Self {
        Self {
            data,
            errors: HashMap::new(),
        }
    }

    /// Create a changeset pre-loaded with field-level errors.
    pub const fn from_errors(data: T, errors: HashMap<String, Vec<String>>) -> Self {
        Self { data, errors }
    }

    /// Returns `true` when there are no field-level errors.
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    /// Returns the validation messages for `field`, or an empty slice.
    pub fn errors_for(&self, field: &str) -> &[String] {
        self.errors.get(field).map_or(&[], Vec::as_slice)
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
    pub fn field_value(&self, field: &str) -> Option<String> {
        let json = serde_json::to_value(&self.data).ok()?;
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

    /// Render a `<button type="submit">` with `label`.
    pub fn submit_button(&self, label: &str) -> maud::Markup {
        submit_button(label)
    }
}

impl<S, T> FromRequest<S> for ChangesetForm<T>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned + validator::Validate,
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
async fn decode_form_body<T, S>(req: Request, state: &S) -> Result<T, axum::response::Response>
where
    T: serde::de::DeserializeOwned + validator::Validate,
    S: Send + Sync,
{
    #[cfg(feature = "multipart")]
    {
        let content_type = req
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if content_type.starts_with("multipart/form-data") {
            return decode_multipart(req, state).await;
        }
    }

    let axum::extract::Form(data) = axum::extract::Form::<T>::from_request(req, state)
        .await
        .map_err(IntoResponse::into_response)?;
    Ok(data)
}

/// Decode `multipart/form-data` text fields and deserialize into `T`.
///
/// File-upload fields are skipped (file storage is out of scope here).
/// The collected text pairs are re-encoded as URL-encoded so that
/// `serde_urlencoded` handles the same type coercions axum's `Form` does.
#[cfg(feature = "multipart")]
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

    serde_urlencoded::from_str::<T>(&encoded)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()).into_response())
}

// ── Internal helpers ───────────────────────────────────────────────

fn validation_errors_to_map(errors: &validator::ValidationErrors) -> HashMap<String, Vec<String>> {
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
    form_tag_inner(action, method, "_csrf", csrf_token, content)
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
#[cfg(feature = "maud")]
#[allow(clippy::needless_pass_by_value)]
fn form_tag_inner(
    action: &str,
    method: &str,
    csrf_field: &str,
    csrf_token: Option<&str>,
    content: maud::Markup,
) -> maud::Markup {
    let (browser_method, override_value) = browser_method_and_override(method);
    maud::html! {
        form action=(action) method=(browser_method) {
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
    let error_id = format!("{field}-error");
    let wrapper_id = format!("{field}-field");

    maud::html! {
        div id=(wrapper_id) class="autumn-field" {
            label for=(field) class="autumn-field__label" { (label) }
            input
                type="text"
                id=(field)
                name=(field)
                value=(value)
                class=(if has_errors { "autumn-field__input autumn-field__input--invalid" } else { "autumn-field__input" })
                aria-invalid=(if has_errors { "true" } else { "false" })
                aria-describedby=(if has_errors { error_id.as_str() } else { "" });
            @if has_errors {
                div id=(error_id) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (error) }
                    }
                }
            }
        }
    }
}

/// Render a labeled `<input type="text">` with htmx inline-validation attributes.
///
/// Like [`text_input`] but adds `hx-post`, `hx-trigger="change"`,
/// `hx-target="closest [data-autumn-field-wrapper]"`, `hx-swap="outerHTML"`, and
/// `hx-include="closest form"` to the input element so htmx
/// POSTs the whole form to `validate_url` after a changed value is committed
/// and swaps the returned field wrapper in place — no JavaScript required.
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
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let value = changeset.field_value(field).unwrap_or_default();
    let error_id = format!("{field}-error");
    let wrapper_id = format!("{field}-field");
    let target = "closest [data-autumn-field-wrapper]";

    maud::html! {
        div id=(wrapper_id) class="autumn-field" data-autumn-field-wrapper=(field) {
            label for=(field) class="autumn-field__label" { (label) }
            input
                type="text"
                id=(field)
                name=(field)
                value=(value)
                class=(if has_errors { "autumn-field__input autumn-field__input--invalid" } else { "autumn-field__input" })
                aria-invalid=(if has_errors { "true" } else { "false" })
                aria-describedby=(if has_errors { error_id.as_str() } else { "" })
                hx-post=(validate_url)
                hx-trigger="change"
                hx-target=(target)
                hx-swap="outerHTML"
                hx-include="closest form";
            @if has_errors {
                div id=(error_id) role="alert" class="autumn-field__errors" {
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
    let error_id = format!("{field}-error");
    let wrapper_id = format!("{field}-field");

    maud::html! {
        div id=(wrapper_id) class="autumn-field" {
            label for=(field) class="autumn-field__label" { (label) }
            input
                type="password"
                id=(field)
                name=(field)
                class=(if has_errors { "autumn-field__input autumn-field__input--invalid" } else { "autumn-field__input" })
                aria-invalid=(if has_errors { "true" } else { "false" })
                aria-describedby=(if has_errors { error_id.as_str() } else { "" });
            @if has_errors {
                div id=(error_id) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (error) }
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
    let error_id = format!("{field}-error");
    let wrapper_id = format!("{field}-field");

    maud::html! {
        div id=(wrapper_id) class="autumn-field" {
            label for=(field) class="autumn-field__label" { (label) }
            textarea
                id=(field)
                name=(field)
                class=(if has_errors { "autumn-field__input autumn-field__input--invalid" } else { "autumn-field__input" })
                aria-invalid=(if has_errors { "true" } else { "false" })
                aria-describedby=(if has_errors { error_id.as_str() } else { "" })
                { (value) }
            @if has_errors {
                div id=(error_id) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (error) }
                    }
                }
            }
        }
    }
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
    let error_id = format!("{field}-error");
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
                aria-invalid=(if has_errors { "true" } else { "false" })
                aria-describedby=(if has_errors { error_id.as_str() } else { "" });
            @if has_errors {
                div id=(error_id) role="alert" {
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
    let error_id = format!("{field}-error");
    let wrapper_id = format!("{field}-field");

    maud::html! {
        div id=(wrapper_id) class="autumn-field" {
            label for=(field) class="autumn-field__label" { (label) }
            input
                type="checkbox"
                id=(field)
                name=(field)
                value="true"
                checked[checked]
                class=(if has_errors { "autumn-field__input autumn-field__input--invalid" } else { "autumn-field__input" })
                aria-invalid=(if has_errors { "true" } else { "false" })
                aria-describedby=(if has_errors { error_id.as_str() } else { "" });
            @if has_errors {
                div id=(error_id) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (error) }
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
    let error_id = format!("{field}-error");
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
                class=(if has_errors { "autumn-field__input autumn-field__input--invalid" } else { "autumn-field__input" })
                aria-invalid=(if has_errors { "true" } else { "false" })
                aria-describedby=(if has_errors { error_id.as_str() } else { "" });
            @if has_errors {
                div id=(error_id) role="alert" class="autumn-field__errors" {
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
    if let Ok(date) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return date.to_string();
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return dt.format("%Y-%m-%d").to_string();
    }
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f") {
        return ndt.format("%Y-%m-%d").to_string();
    }
    raw.to_owned()
}

/// Normalize a stored datetime string into `YYYY-MM-DDTHH:MM`, the only
/// shape the HTML `<input type="datetime-local">` control accepts. Browsers
/// silently reject RFC 3339 timestamps carrying a `Z`/offset suffix.
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
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M") {
        return ndt.format("%Y-%m-%dT%H:%M").to_string();
    }
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f") {
        return ndt.format("%Y-%m-%dT%H:%M").to_string();
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return dt.naive_local().format("%Y-%m-%dT%H:%M").to_string();
    }
    raw.to_owned()
}

/// Render a labeled `<input type="date">` tied to a changeset field.
///
/// The current value is normalized via [`normalize_date_value`] to the
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
    let error_id = format!("{field}-error");
    let wrapper_id = format!("{field}-field");

    maud::html! {
        div id=(wrapper_id) class="autumn-field" {
            label for=(field) class="autumn-field__label" { (label) }
            input
                type="date"
                id=(field)
                name=(field)
                value=(value)
                class=(if has_errors { "autumn-field__input autumn-field__input--invalid" } else { "autumn-field__input" })
                aria-invalid=(if has_errors { "true" } else { "false" })
                aria-describedby=(if has_errors { error_id.as_str() } else { "" });
            @if has_errors {
                div id=(error_id) role="alert" class="autumn-field__errors" {
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
/// The current value is normalized via [`normalize_datetime_local_value`] to
/// the `YYYY-MM-DDTHH:MM` shape HTML5 datetime pickers require. Wraps in
/// `<div id="{field}-field">` for stable htmx targeting. ARIA annotations
/// behave identically to [`text_input`].
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
    let error_id = format!("{field}-error");
    let wrapper_id = format!("{field}-field");

    maud::html! {
        div id=(wrapper_id) class="autumn-field" {
            label for=(field) class="autumn-field__label" { (label) }
            input
                type="datetime-local"
                id=(field)
                name=(field)
                value=(value)
                class=(if has_errors { "autumn-field__input autumn-field__input--invalid" } else { "autumn-field__input" })
                aria-invalid=(if has_errors { "true" } else { "false" })
                aria-describedby=(if has_errors { error_id.as_str() } else { "" });
            @if has_errors {
                div id=(error_id) role="alert" class="autumn-field__errors" {
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
/// [#1030]: https://github.com/madmax983/autumn/issues/1030
/// [#1026]: https://github.com/madmax983/autumn/issues/1026
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
    let error_id = format!("{field}-error");
    let wrapper_id = format!("{field}-field");

    maud::html! {
        div id=(wrapper_id) class="autumn-field" {
            label for=(field) class="autumn-field__label" { (label) }
            select
                id=(field)
                name=(field)
                class=(if has_errors { "autumn-field__input autumn-field__input--invalid" } else { "autumn-field__input" })
                aria-invalid=(if has_errors { "true" } else { "false" })
                aria-describedby=(if has_errors { error_id.as_str() } else { "" }) {
                @for (option_value, option_label) in options {
                    option value=(option_value) selected[*option_value == current] { (option_label) }
                }
            }
            @if has_errors {
                div id=(error_id) role="alert" class="autumn-field__errors" {
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

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(html.contains(r#"value="2024-03-15T10:30""#), "{html}");
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
        // must be reduced to the bare local-shaped `YYYY-MM-DDTHH:MM`.
        assert!(html.contains(r#"value="2024-03-15T10:30""#), "{html}");
        assert!(!html.contains(r#"value="2024-03-15T10:30:00Z""#), "{html}");
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
            let boundary = "----FormBoundary7MA4YWxkTrZu0gW";
            let body = format!(
                "--{boundary}\r\n\
                 Content-Disposition: form-data; name=\"{field}\"\r\n\r\n\
                 {value}\r\n\
                 --{boundary}--\r\n"
            );
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
