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
//! `multipart/form-data` bodies. Text parts decode into `T`. File parts go to
//! [`ChangesetForm::file`], [`ChangesetForm::files`] and
//! [`ChangesetForm::take_files`], under the `security.upload` policy.
//!
//! # Repeated keys
//!
//! A repeated key (`tags=a&tags=b`) fills a `Vec<T>`, `HashSet<T>` or
//! `Option<Vec<T>>` field. On a single-value field it is a 422. See
//! [`from_urlencoded`] for the full rules.
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

#[cfg(feature = "maud")]
use std::borrow::Cow;
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

    /// Record one more error message against `field`, keeping any already
    /// there.
    ///
    /// Appends rather than replaces, so a field that failed a
    /// `#[validate(...)]` rule *and* carried unstorable input shows both
    /// messages. Adding any error makes the changeset invalid.
    ///
    /// Used by [`ChangesetForm`] to attach the
    /// [`NUL_CHARACTER_FIELD_ERROR`] a validator cannot see, and available to
    /// handlers that need to fold a post-decode failure (a uniqueness check
    /// that only the database can answer, say) back into the form round-trip.
    pub fn add_error(&mut self, field: impl Into<String>, message: impl Into<String>) {
        self.errors
            .entry(field.into())
            .or_default()
            .push(message.into());
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
        json_scalar_to_string(json.get(field)?)
    }

    /// Every value of `field` as strings, for multi-value controls (issue
    /// #3394).
    ///
    /// A sequence field (`Vec<T>`, `HashSet<T>`) gives one string per scalar
    /// element. A scalar field gives one string. A missing or `None` field
    /// gives an empty list.
    pub fn field_values(&self, field: &str) -> Vec<String> {
        let json = self.field_value_cache.get_or_init(|| {
            Box::new(serde_json::to_value(&self.data).unwrap_or(serde_json::Value::Null))
        });
        match json.get(field) {
            Some(serde_json::Value::Array(items)) => {
                items.iter().filter_map(json_scalar_to_string).collect()
            }
            Some(value) => json_scalar_to_string(value).into_iter().collect(),
            None => Vec::new(),
        }
    }
}

/// A string, number or bool as text. Any other JSON value is `None`.
fn json_scalar_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

// ── #2423: NUL bytes in submitted text ─────────────────────────────

/// The message recorded against a field whose submitted value carried an
/// embedded NUL (`U+0000`) character (issue #2423).
///
/// A Postgres `TEXT`/`VARCHAR` column cannot hold `0x00`, so such a value used
/// to sail past every `#[validate(...)]` rule and fail only at the
/// Diesel→Postgres boundary, surfacing as an unhandled `500`. [`ChangesetForm`]
/// now records this message against the offending field instead, so the
/// submission is rejected the same way any other invalid input is — inline, on
/// the field, with the author's remaining text intact.
///
/// A real user can produce a NUL without meaning to (a paste from a binary
/// source, an input-method glitch), so the wording names the character rather
/// than accusing the author of anything.
pub const NUL_CHARACTER_FIELD_ERROR: &str = "Cannot contain the NUL character (0x00)";

/// Catalog key for [`NUL_CHARACTER_FIELD_ERROR`] in a localized app.
///
/// When the request carries an `i18n` bundle, [`ChangesetForm`] and
/// [`crate::nested_form::NestedChangesetForm`] look this key up in the request
/// locale. If the key is missing, the message stays [`NUL_CHARACTER_FIELD_ERROR`].
/// `autumn generate --i18n` scaffolds add the key to their catalog.
pub const NUL_CHARACTER_MESSAGE_KEY: &str = "common.error.nul_character";

/// Returns the request and the NUL field message for it.
///
/// Resolves the locale before the body is read. Without an `i18n` bundle in
/// the request extensions, this is [`NUL_CHARACTER_FIELD_ERROR`].
#[cfg(feature = "i18n")]
pub(crate) async fn nul_field_message<S: Send + Sync>(
    req: Request,
    state: &S,
) -> (Request, String) {
    use axum::extract::FromRequestParts as _;

    let Some(bundle) = req
        .extensions()
        .get::<std::sync::Arc<crate::i18n::Bundle>>()
        .cloned()
    else {
        return (req, NUL_CHARACTER_FIELD_ERROR.to_owned());
    };
    let (mut parts, body) = req.into_parts();
    let message = match crate::i18n::Locale::from_request_parts(&mut parts, state).await {
        Ok(locale) => bundle.lookup(locale.tag(), NUL_CHARACTER_MESSAGE_KEY),
        Err(never) => match never {},
    };
    (
        Request::from_parts(parts, body),
        message.unwrap_or_else(|| NUL_CHARACTER_FIELD_ERROR.to_owned()),
    )
}

/// Returns the request and the NUL field message for it. Without the `i18n`
/// feature, the message is always English.
#[cfg(not(feature = "i18n"))]
#[allow(clippy::unused_async)]
pub(crate) async fn nul_field_message<S: Send + Sync>(
    req: Request,
    _state: &S,
) -> (Request, String) {
    (req, NUL_CHARACTER_FIELD_ERROR.to_owned())
}

/// Submitted names that carry framework plumbing rather than form data.
///
/// No template renders an error against one of these, so a
/// [`NUL_CHARACTER_FIELD_ERROR`] keyed under one would leave the form invalid
/// with nothing on screen to explain why — the failure the exemption exists to
/// prevent. None is ever written to a column, so nothing is lost by cleaning
/// one silently.
///
/// These are the *default* names. Both token fields are configurable, so the
/// extractors additionally exempt whatever name request extensions carry; the
/// defaults are listed here as well so a direct caller of the public
/// [`crate::nested_form::decode_nested_urlencoded`], which never sees those
/// extensions, is covered for the common case.
const PLUMBING_FIELDS: &[&str] = &["_method", "_csrf", "_submit_token"];

#[cfg_attr(not(feature = "nested-form"), allow(dead_code))]
/// Whether `name` is one of the default [`PLUMBING_FIELDS`].
pub(crate) fn is_plumbing_field(name: &str) -> bool {
    PLUMBING_FIELDS.contains(&name)
}

/// Remove every NUL from each submitted value, returning the names of the
/// fields that carried one (issue #2423).
///
/// Runs on the decoded key/value pairs, *before* deserialization, which is the
/// only point where the offending field is still identifiable by name — after
/// `serde` has built `T` the bytes are just some `String` field among others.
///
/// Values are cleaned rather than left as submitted for two reasons: the
/// rejected form is re-rendered from this data, and neither the author's
/// browser nor any downstream consumer should be handed back a raw `0x00`; and
/// the cleaned text is what the author most likely meant, so the resubmission
/// they make after reading the error succeeds.
///
/// Keys are deliberately left alone. Stripping a NUL out of a *key* would
/// rename it — potentially onto a real field of `T` — inventing a submission
/// the client never made. A mangled key simply fails to match any field and is
/// ignored, exactly as any other unknown key is.
///
/// The returned names are deduplicated, so a repeated key (a multi-value
/// field) contributes at most one message.
///
/// Deduplication goes through a `BTreeSet` rather than a linear scan of the
/// names collected so far. That scan is O(n²) in the number of *distinct*
/// offending keys, and the key count is bounded only by `DefaultBodyLimit` — a
/// 32 MiB body of `k1=%00&k2=%00&…` is millions of distinct keys, which is
/// minutes of CPU burned synchronously on a runtime worker thread. The set also
/// gives the names a deterministic order.
pub(crate) fn strip_nul_from_pairs(pairs: &mut [(String, String)]) -> Vec<String> {
    let mut offenders: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (key, value) in pairs {
        if value.as_bytes().contains(&0) {
            *value = crate::normalize::strip_nul(value);
            if !offenders.contains(key.as_str()) {
                offenders.insert(key.clone());
            }
        }
    }
    offenders.into_iter().collect()
}

// ── IntoChangeset ──────────────────────────────────────────────────

/// Validate `self` and wrap in a [`Changeset`].
///
/// Blanket-implemented for every type that implements [`validator::Validate`].
pub trait IntoChangeset: Sized {
    /// Run validation and produce a `Changeset<Self>`.
    fn into_changeset(self) -> Changeset<Self>;

    /// Build a changeset. Use `resolve` to look up a message for a field and
    /// error code.
    /// Return `None` from `resolve` to keep autumn-web's default English
    /// message.
    ///
    /// The default implementation ignores `resolve` and defers to
    /// [`into_changeset`](Self::into_changeset). This keeps a hand-rolled
    /// `IntoChangeset` impl written before this method compiling unchanged.
    fn into_changeset_with(
        self,
        _resolve: impl Fn(&str, &str) -> Option<String>,
    ) -> Changeset<Self> {
        self.into_changeset()
    }
}

impl<T: validator::Validate> IntoChangeset for T {
    fn into_changeset(self) -> Changeset<Self> {
        match validator::Validate::validate(&self) {
            Ok(()) => Changeset::new(self),
            Err(errors) => Changeset::from_errors(self, validation_errors_to_map(&errors)),
        }
    }

    fn into_changeset_with(
        self,
        resolve: impl Fn(&str, &str) -> Option<String>,
    ) -> Changeset<Self> {
        match validator::Validate::validate(&self) {
            Ok(()) => Changeset::new(self),
            Err(errors) => {
                Changeset::from_errors(self, validation_errors_to_map_with(&errors, &resolve))
            }
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
/// Unlike [`crate::validation::Valid`], this extractor does not reject a
/// validation error — errors live in the [`Changeset`] and the handler decides
/// how to respond. It rejects only a body it cannot decode into `T`: 415 for
/// the wrong `Content-Type`, 400 for a value that does not parse, 422 for a
/// single-value field sent more than once (see [`from_urlencoded`]), and on
/// multipart the `security.upload` rejections (400, 413).
///
/// # Unstorable bytes
///
/// Submitted text values are swept for NUL (`U+0000`) before deserialization
/// (issue #2423). A Postgres `TEXT`/`VARCHAR` column cannot hold `0x00`, and no
/// `#[validate(...)]` rule can express that — the value is a perfectly good
/// Rust `String` — so such a field used to fail only at the database, as an
/// unhandled 500. A field that carried one now gets
/// [`NUL_CHARACTER_FIELD_ERROR`] like any other field error, and the value kept
/// for re-render is the author's text with the byte removed. The CSRF token is
/// exempt (no template renders it); file parts of a multipart body are
/// untouched.
///
/// # File parts
///
/// A multipart body keeps its file parts: see [`ChangesetForm::file`].
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
    /// File parts of a multipart body, in body order. `None` when there are
    /// none. A thin pointer, so `Result<T, Self>` in [`Self::into_valid`]
    /// stays under clippy's `result_large_err` threshold.
    #[allow(
        clippy::box_collection,
        reason = "a `Box<Vec>` is one pointer wide; a `Vec` or `Box<[T]>` pushes `into_valid`'s `Err` over `result_large_err`"
    )]
    pub(crate) files: Option<Box<Vec<FormFile>>>,
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
            files: None,
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
            files: None,
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
            files: None,
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

    /// The first file part named `field`, from a `multipart/form-data` body.
    ///
    /// Returns `None` for a URL-encoded body, for an absent part, and for an
    /// empty optional file input (no file chosen).
    pub fn file(&self, field: &str) -> Option<&FormFile> {
        self.all_files().iter().find(|file| file.field == field)
    }

    /// Every file part named `field`, in body order. Use this for
    /// `<input type="file" multiple>`.
    pub fn files<'a>(&'a self, field: &'a str) -> impl Iterator<Item = &'a FormFile> {
        self.all_files()
            .iter()
            .filter(move |file| file.field == field)
    }

    /// Move every file part out of the form. Call it before
    /// [`into_valid`](Self::into_valid), which consumes the form.
    pub fn take_files(&mut self) -> Vec<FormFile> {
        self.files.take().map(|files| *files).unwrap_or_default()
    }

    /// Attach file parts, as the extractor does for a multipart body. Use it
    /// to build a form with files in a test.
    #[must_use]
    pub fn with_files(mut self, files: Vec<FormFile>) -> Self {
        self.files = (!files.is_empty()).then(|| Box::new(files));
        self
    }

    fn all_files(&self) -> &[FormFile] {
        self.files.as_deref().map_or(&[], Vec::as_slice)
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

    /// Render a labeled Markdown editor for a rich-text `field`, with
    /// caller-supplied chrome labels.
    ///
    /// Delegates to [`rich_text_area_with_labels`]; see [`rich_text_area`] for
    /// the full contract.
    pub fn rich_text_area_with_labels(
        &self,
        field: &str,
        label: &str,
        labels: &RichTextLabels<'_>,
    ) -> maud::Markup {
        rich_text_area_with_labels(&self.changeset, field, label, labels)
    }

    /// Render a Markdown editor with an htmx-driven live preview pane.
    ///
    /// Delegates to [`rich_text_area_htmx`]; see that function for full docs.
    pub fn rich_text_area_htmx(&self, field: &str, label: &str, preview_url: &str) -> maud::Markup {
        rich_text_area_htmx(&self.changeset, field, label, preview_url)
    }

    /// Render a Markdown editor with an htmx-driven live preview pane, with
    /// caller-supplied chrome labels.
    ///
    /// Delegates to [`rich_text_area_htmx_with_labels`]; see
    /// [`rich_text_area_htmx`] for full docs.
    pub fn rich_text_area_htmx_with_labels(
        &self,
        field: &str,
        label: &str,
        preview_url: &str,
        labels: &RichTextLabels<'_>,
    ) -> maud::Markup {
        rich_text_area_htmx_with_labels(&self.changeset, field, label, preview_url, labels)
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

    /// Render a Markdown editor with an htmx live preview, excluding the
    /// configured submit-token field `token_field` from the preview POST,
    /// with caller-supplied chrome labels.
    ///
    /// Delegates to [`rich_text_area_htmx_with_token_field_with_labels`]; use
    /// this when the app both customizes `[security.submit_token].field_name`
    /// (issue #1843) and needs translated chrome labels.
    pub fn rich_text_area_htmx_with_token_field_with_labels(
        &self,
        field: &str,
        label: &str,
        preview_url: &str,
        token_field: &str,
        labels: &RichTextLabels<'_>,
    ) -> maud::Markup {
        rich_text_area_htmx_with_token_field_with_labels(
            &self.changeset,
            field,
            label,
            preview_url,
            token_field,
            labels,
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
        // Read for the #2423 exemption only. `ChangesetForm` does not otherwise
        // use the submit token — `SubmitTokenLayer` consumes it from the raw
        // body upstream — but a scaffolded flat form does post the hidden
        // field, so its name has to be exempt here exactly as the nested
        // extractor exempts it.
        let submit_field = req
            .extensions()
            .get::<crate::security::SubmitFormField>()
            .map_or_else(|| "_submit_token".to_owned(), |f| f.0.clone());

        let (req, nul_message) = nul_field_message(req, state).await;
        let (data, nul_fields, files) = decode_form_body::<T, S>(req, state).await?;

        let mut changeset = data.into_changeset();
        for field in nul_fields {
            // The two tokens and the method override are transport plumbing,
            // not form fields: no template renders any of them, so an error
            // keyed under one would make the form permanently invalid with
            // nothing on screen to explain why, and none is ever written to a
            // column. The configurable names come from request extensions; the
            // defaults are in `PLUMBING_FIELDS` too, because `CsrfService`
            // accepts the literal `_csrf` even when another name is configured.
            if field == csrf_field
                || field == submit_field
                || PLUMBING_FIELDS.contains(&field.as_str())
            {
                continue;
            }
            changeset.add_error(field, nul_message.as_str());
        }

        Ok(Self {
            changeset,
            csrf_token,
            csrf_field,
            files: (!files.is_empty()).then(|| Box::new(files)),
        })
    }
}

/// Decode a form body — URL-encoded always, multipart when that feature is on.
///
/// Returns the decoded `T`, the names of the fields whose submitted value
/// carried a NUL byte (#2423), and the file parts of a multipart body. NUL
/// values are cleaned before `T` is built, so the caller only has to decide
/// what to say about them.
// `clippy::result_large_err` (armed by rustc 1.98) measures the `Err` variant at
// 128 bytes — that is `axum::response::Response`'s own size, not something this
// crate chose. Returning a ready-made rejection response IS the idiom here, and
// boxing it would add an allocation to every rejection path to satisfy a size
// heuristic. Allowed at the site rather than workspace-wide so the lint stays
// armed for error types we do control.
#[allow(clippy::result_large_err)]
async fn decode_form_body<T, S>(
    req: Request,
    state: &S,
) -> Result<(T, Vec<String>, Vec<FormFile>), axum::response::Response>
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
    if crate::media_type::is_media_type(&content_type, "multipart/form-data") {
        return decode_multipart(req, state).await;
    }

    // Same content-type gate axum's own `Form`/`RawForm` extractors apply: a
    // POST whose Content-Type isn't (or doesn't start with) the form-urlencoded
    // mime type is rejected outright, rather than being decoded anyway. `Bytes`
    // alone has no opinion on Content-Type, so this must be checked explicitly
    // now that we no longer route through `axum::extract::Form`.
    if !crate::media_type::is_media_type(&content_type, "application/x-www-form-urlencoded") {
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

    let mut pairs = parse_urlencoded_pairs(&bytes);
    let nul_fields = strip_nul_from_pairs(&mut pairs);
    let data = decode_pairs_dropping_blank_optional_fields::<T>(pairs)
        .map_err(|e| (e.status(), e.to_string()).into_response())?;
    Ok((data, nul_fields, Vec::new()))
}

/// Deserialize `T` from `application/x-www-form-urlencoded` bytes, tolerating
/// blank values for non-string `Option<T>` fields (a number/date/uuid input
/// the user left empty submits `field=`, which the part parser otherwise
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
///
/// # Repeated keys (issue #3394)
///
/// A repeated key fills a sequence field (`Vec<T>`, `HashSet<T>`,
/// `Option<Vec<T>>`). On a single-value field it is a
/// [`FormDecodeError::is_repeated_field`] error. That check runs before the
/// blank drop, so `count=&count=5` never decodes as `5`. A blank element of a
/// typed sequence (`ids=`) is dropped like a blank `Option`.
/// `verification/form_decode.rs` models one step of this loop.
pub(crate) fn decode_urlencoded_dropping_blank_optional_fields<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, FormDecodeError> {
    decode_pairs_dropping_blank_optional_fields(parse_urlencoded_pairs(bytes))
}

/// Parse an `application/x-www-form-urlencoded` body into owned key/value
/// pairs — the form in which the #2423 NUL sweep can still name the offending
/// field.
pub(crate) fn parse_urlencoded_pairs(bytes: &[u8]) -> Vec<(String, String)> {
    url::form_urlencoded::parse(bytes)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// The blank-optional-dropping retry loop itself, over already-parsed pairs.
///
/// See [`decode_urlencoded_dropping_blank_optional_fields`] for the contract;
/// this is the same function with the parse step hoisted out so callers can
/// inspect and clean the pairs first.
pub(crate) fn decode_pairs_dropping_blank_optional_fields<T: serde::de::DeserializeOwned>(
    mut pairs: Vec<(String, String)>,
) -> Result<T, FormDecodeError> {
    loop {
        let encoded = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .finish();
        let deserializer = serde_html_form::Deserializer::from_bytes(encoded.as_bytes());
        match serde_path_to_error::deserialize(deserializer) {
            Ok(data) => return Ok(data),
            Err(err) => match next_retry(&pairs, err.path()) {
                Retry::DropPair(pos) => {
                    pairs.remove(pos);
                }
                Retry::DropBlanksOf(field) => {
                    pairs.retain(|(k, v)| !(*k == field && v.is_empty()));
                }
                Retry::Repeated(field) => return Err(FormDecodeError::repeated(field)),
                Retry::Stop => return Err(FormDecodeError::invalid(err)),
            },
        }
    }
}

/// What the retry loop does after a decode error at `path`.
///
/// Each retry removes all the blank pairs of one key, so the loop runs at most
/// once for each field of `T`. It never runs once for each pair: a body of
/// many blank `ids=` would then cost one full decode per pair.
enum Retry {
    /// Remove the pair at this index and decode again.
    DropPair(usize),
    /// Remove every blank pair with this key and decode again.
    DropBlanksOf(String),
    /// Stop: a single-value field got more than one value.
    Repeated(String),
    /// Stop with the decode error.
    Stop,
}

/// Find the blank pairs that caused a decode error, if there are any.
///
/// - `field`: a single-value field. More than one pair with that key is a
///   repeat. Else the blank pair with that key.
/// - `field[n]`: element `n` of a sequence field. If the `n`-th pair with that
///   key is blank, every blank pair of that key fails the same way, so all of
///   them go.
/// - Any other path: nothing to drop.
fn next_retry(pairs: &[(String, String)], path: &serde_path_to_error::Path) -> Retry {
    let mut segments = path.iter();
    let Some(serde_path_to_error::Segment::Map { key: field }) = segments.next() else {
        return Retry::Stop;
    };
    let mut same_key = pairs.iter().enumerate().filter(|(_, (k, _))| k == field);
    match (segments.next(), segments.next()) {
        (None, _) => {
            if pairs.iter().filter(|(k, _)| k == field).nth(1).is_some() {
                return Retry::Repeated(field.clone());
            }
            same_key
                .find(|(_, (_, v))| v.is_empty())
                .map_or(Retry::Stop, |(i, _)| Retry::DropPair(i))
        }
        (Some(serde_path_to_error::Segment::Seq { index }), None) => {
            if same_key.nth(*index).is_some_and(|(_, (_, v))| v.is_empty()) {
                Retry::DropBlanksOf(field.clone())
            } else {
                Retry::Stop
            }
        }
        _ => Retry::Stop,
    }
}

/// Decode an `application/x-www-form-urlencoded` body into `T` (issue #3394).
///
/// The same decoder [`ChangesetForm`] uses. Use it in a handler that reads
/// the raw body as `Bytes`:
///
/// - A repeated key fills a `Vec<T>`, `HashSet<T>` or `Option<Vec<T>>` field.
/// - A repeated key on a single-value field is an error with status 422.
/// - A blank value for a typed `Option<T>` (`age=`) decodes as `None`. A blank
///   element of a typed sequence (`ids=`) is dropped.
/// - A value that does not parse is an error with status 400.
///
/// Brackets are part of the key: `ids[]=1` does not fill `ids`. Use
/// `#[serde(alias = "ids[]")]` to accept both spellings.
///
/// Only `Vec`-like fields take repeats. A fixed-size array (`[i64; 2]`) and a
/// newtype around a `Vec` take one value, so a repeat on them is a 422.
///
/// # Example
///
/// ```
/// #[derive(serde::Deserialize)]
/// struct Selection {
///     #[serde(default)]
///     ids: Vec<i64>,
/// }
///
/// let selection: Selection = autumn_web::form::from_urlencoded(b"ids=1&ids=2").unwrap();
/// assert_eq!(selection.ids, [1, 2]);
/// ```
///
/// # Errors
///
/// Returns a [`FormDecodeError`] when the body does not decode into `T`.
/// [`FormDecodeError::status`] tells a 400 from a 422.
pub fn from_urlencoded<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, FormDecodeError> {
    decode_urlencoded_dropping_blank_optional_fields(body)
}

/// The longest field name a [`FormDecodeError`] message repeats.
const MAX_REPORTED_FIELD_CHARS: usize = 64;

/// Why a form body did not decode into its target type (issue #3394).
///
/// - A **repeated field**: a single-value field got its key more than once
///   (`name=a&name=b`). [`status`](Self::status) is 422. The decoder does not
///   select one of the values.
/// - An **invalid body**: a value does not parse, or a required field is
///   missing. [`status`](Self::status) is 400.
#[derive(Debug, Clone)]
pub struct FormDecodeError {
    field: Option<String>,
    kind: FormDecodeErrorKind,
}

#[derive(Debug, Clone)]
enum FormDecodeErrorKind {
    Repeated,
    Invalid(serde_path_to_error::Error<serde_html_form::de::Error>),
}

impl FormDecodeError {
    const fn repeated(field: String) -> Self {
        Self {
            field: Some(field),
            kind: FormDecodeErrorKind::Repeated,
        }
    }

    fn invalid(err: serde_path_to_error::Error<serde_html_form::de::Error>) -> Self {
        let field = err.path().iter().find_map(|segment| match segment {
            serde_path_to_error::Segment::Map { key } => Some(key.clone()),
            _ => None,
        });
        Self {
            field,
            kind: FormDecodeErrorKind::Invalid(err),
        }
    }

    /// `true` when a single-value field got its key more than once.
    #[must_use]
    pub const fn is_repeated_field(&self) -> bool {
        matches!(self.kind, FormDecodeErrorKind::Repeated)
    }

    /// The top-level form field that failed, when serde names one. A
    /// missing-field error names no path, so it returns `None`.
    #[must_use]
    pub fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }

    /// 422 for a repeated field, 400 for every other failure.
    #[must_use]
    pub const fn status(&self) -> http::StatusCode {
        match self.kind {
            FormDecodeErrorKind::Repeated => http::StatusCode::UNPROCESSABLE_ENTITY,
            FormDecodeErrorKind::Invalid(_) => http::StatusCode::BAD_REQUEST,
        }
    }

    #[cfg_attr(not(feature = "nested-form"), allow(dead_code))]
    /// The `serde_path_to_error` error of an invalid body. `None` for a
    /// repeated field.
    pub(crate) const fn invalid_error(
        &self,
    ) -> Option<&serde_path_to_error::Error<serde_html_form::de::Error>> {
        match &self.kind {
            FormDecodeErrorKind::Invalid(err) => Some(err),
            FormDecodeErrorKind::Repeated => None,
        }
    }
}

impl std::fmt::Display for FormDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            FormDecodeErrorKind::Repeated => {
                // The key comes from the request when `T` is a map, so bound it
                // and remove control and bidi characters before it reaches a
                // response or a log.
                let field = self.field.as_deref().unwrap_or_default();
                let mut shown: String = field
                    .chars()
                    .filter(|c| is_printable_key_char(*c))
                    .take(MAX_REPORTED_FIELD_CHARS)
                    .collect();
                if field
                    .chars()
                    .filter(|c| is_printable_key_char(*c))
                    .nth(MAX_REPORTED_FIELD_CHARS)
                    .is_some()
                {
                    shown.push('…');
                }
                write!(
                    f,
                    "form field `{shown}` takes one value, but the form sent it more than once"
                )
            }
            FormDecodeErrorKind::Invalid(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for FormDecodeError {}

/// `false` for control, bidi and zero-width characters. These can hide or
/// reorder text in a log line.
fn is_printable_key_char(c: char) -> bool {
    !c.is_control()
        && !matches!(
            c,
            '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
        )
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

/// Decode a `multipart/form-data` body: text parts into `T`, file parts into
/// [`FormFile`]s.
///
/// The parts come through [`crate::extract::Multipart`], so the
/// `security.upload` policy applies: the MIME allowlist, the per-file cap and
/// the request cap. Text parts decode with the same rules as a URL-encoded
/// body. An empty optional file input (no file name, no bytes) is skipped.
#[cfg(feature = "multipart")]
// Same as `decode_form_body` above: the `Err` variant is an
// `axum::response::Response`, which IS the rejection, and boxing it would cost
// an allocation on every rejection to satisfy a size heuristic.
#[allow(clippy::result_large_err)]
async fn decode_multipart<T, S>(
    req: Request,
    state: &S,
) -> Result<(T, Vec<String>, Vec<FormFile>), axum::response::Response>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    let mut multipart = crate::extract::Multipart::from_request_with_route_limit(req, state)
        .await
        .map_err(IntoResponse::into_response)?;

    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut files: Vec<FormFile> = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(IntoResponse::into_response)?
    {
        let Some(name) = field.name().map(str::to_owned) else {
            continue;
        };
        // An empty optional file input (no file chosen) is not a file. On the
        // MIME-policy path `file_name()` reports it as `None`, so test it first:
        // it must not become a blank text value of `T`.
        if field.is_empty_optional_input() {
            continue;
        }
        let file_name = field.file_name().map(str::to_owned);
        let content_type = field.content_type().map(str::to_owned);
        let sniffed_content_type = field.sniffed_content_type().map(str::to_owned);
        let mut bytes = field
            .bytes_limited()
            .await
            .map_err(IntoResponse::into_response)?;
        if let Some(file_name) = file_name {
            if file_name.is_empty() && bytes.is_empty() {
                continue;
            }
            // The sniff buffer can hold more capacity than a small file needs.
            // Many small parts would then keep far more memory than the body.
            bytes.shrink_to_fit();
            files.push(FormFile {
                field: name,
                file_name,
                content_type,
                sniffed_content_type,
                bytes,
            });
        } else {
            let value = String::from_utf8(bytes).map_err(|_| {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    "a multipart text field is not valid UTF-8",
                )
                    .into_response()
            })?;
            pairs.push((name, value));
        }
    }

    // #2423: the same NUL sweep the URL-encoded path runs, applied to the
    // text fields only. File bytes are not text.
    let nul_fields = strip_nul_from_pairs(&mut pairs);
    let data = decode_pairs_dropping_blank_optional_fields::<T>(pairs)
        .map_err(|e| (e.status(), e.to_string()).into_response())?;
    Ok((data, nul_fields, files))
}

/// One file part of a `multipart/form-data` body (issue #3394).
///
/// [`ChangesetForm::file`], [`ChangesetForm::files`] and
/// [`ChangesetForm::take_files`] return these. The bytes are in memory. The
/// `security.upload` per-file cap and request cap limit their size.
#[derive(Clone)]
pub struct FormFile {
    field: String,
    file_name: String,
    content_type: Option<String>,
    sniffed_content_type: Option<String>,
    bytes: Vec<u8>,
}

impl FormFile {
    /// Make a file part, as the extractor does for a multipart body. Use it
    /// with [`ChangesetForm::with_files`] to build a form in a test.
    #[must_use]
    pub fn new(
        field: impl Into<String>,
        file_name: impl Into<String>,
        content_type: Option<String>,
        bytes: Vec<u8>,
    ) -> Self {
        Self {
            field: field.into(),
            file_name: file_name.into(),
            content_type,
            sniffed_content_type: None,
            bytes,
        }
    }

    /// The form field name (`name="avatar"`).
    #[must_use]
    pub fn field(&self) -> &str {
        &self.field
    }

    /// The file name the client sent. The client can send any text: `../`, a
    /// NUL, control characters. Do not use it as a storage path, and escape it
    /// when you show it.
    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// The `Content-Type` the client sent. The client can set any value.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// The content type found from the bytes. Set only when a
    /// `security.upload` MIME policy is on and the format is known.
    #[must_use]
    pub fn sniffed_content_type(&self) -> Option<&str> {
        self.sniffed_content_type.as_deref()
    }

    /// The file contents.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The file contents, moved out.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// The size in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// `true` for a zero-byte file.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// Shows the size, not the bytes: a file can be large, and its contents do not
/// belong in a log.
impl std::fmt::Debug for FormFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FormFile")
            .field("field", &self.field)
            .field("file_name", &self.file_name)
            .field("content_type", &self.content_type)
            .field("sniffed_content_type", &self.sniffed_content_type)
            .field("len", &self.bytes.len())
            .finish()
    }
}

// ── Internal helpers ───────────────────────────────────────────────

pub(crate) fn validation_errors_to_map(
    errors: &validator::ValidationErrors,
) -> HashMap<String, Vec<String>> {
    let mut map = HashMap::new();
    collect_errors(errors, "", &mut map, &|_, _| None);
    map
}

/// Like [`validation_errors_to_map`], but `resolve` gets a chance to supply a
/// message for a field and code before the hardcoded English fallback runs.
///
/// `resolve` is consulted only when a `validator::ValidationError` has no
/// explicit `.message` — an explicit message always wins.
pub(crate) fn validation_errors_to_map_with(
    errors: &validator::ValidationErrors,
    resolve: &dyn Fn(&str, &str) -> Option<String>,
) -> HashMap<String, Vec<String>> {
    let mut map = HashMap::new();
    collect_errors(errors, "", &mut map, resolve);
    map
}

fn collect_errors(
    errors: &validator::ValidationErrors,
    prefix: &str,
    map: &mut HashMap<String, Vec<String>>,
    resolve: &dyn Fn(&str, &str) -> Option<String>,
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
                            || {
                                resolve(&key, &e.code)
                                    .unwrap_or_else(|| format!("validation failed: {}", e.code))
                            },
                            ToString::to_string,
                        )
                    })
                    .collect();
                map.entry(key).or_default().extend(messages);
            }
            validator::ValidationErrorsKind::Struct(nested) => {
                collect_errors(nested, &key, map, resolve);
            }
            validator::ValidationErrorsKind::List(list) => {
                for (idx, nested) in list {
                    let indexed_key = format!("{key}[{idx}]");
                    collect_errors(nested, &indexed_key, map, resolve);
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

/// Pre-escape a field name the same way `maud::html!` would, for building
/// an `id`/`name`-derived string (`{field_html}-error`).
///
/// It produces an owned-or-borrowed `str`, because the caller adds a suffix
/// before it writes the result — so it keeps the `Cow`
/// interface and, when there's nothing to escape (the overwhelmingly
/// common case for a field name), returns the input completely
/// unallocated. Escaping first and concatenating after is equivalent to
/// concatenating first and escaping after here, since the literal
/// `"-error"`/`"-field"` suffixes contain no characters that need
/// escaping — see `fast_escape_matches_naive_reference` for the
/// byte-for-byte equivalence check against a naive reference.
#[cfg(feature = "maud")]
fn fast_escape(input: &str) -> Cow<'_, str> {
    if !input
        .as_bytes()
        .iter()
        .any(|b| matches!(b, b'&' | b'<' | b'>' | b'"'))
    {
        return Cow::Borrowed(input);
    }
    let mut out = String::with_capacity(input.len());
    write_escaped(input, &mut out);
    Cow::Owned(out)
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

// ── Field wrapper v2 (issue #3395) ──────────────────────────────────

/// Options for the `*_with` field helpers and [`FormFor::field_options`].
///
/// Each option is typed. There is no setter for `id`, `name`, `class`,
/// `type`, `value` or any `aria-*` attribute. The helper sets those, so the
/// links between the label, hint, error and control stay correct. There is no
/// label setter. The label is a positional argument of every helper.
///
/// - [`hint`](Self::hint) renders `<p id="{field}-hint">` between the label
///   and the control (after the label for a checkbox). It adds `{field}-hint`
///   to `aria-describedby`, before the error id.
/// - [`required`](Self::required) adds `required`, `aria-required="true"`
///   and a visual `*` marker. The marker has `aria-hidden="true"`, so a
///   screen reader announces "required" one time only.
/// - The typed attributes render only on controls that use them:
///
/// | Option | Controls |
/// |---|---|
/// | `placeholder`, `inputmode` | text, password, textarea, number |
/// | `pattern` | text, password |
/// | `min`, `max` | number, date, datetime |
/// | `readonly` | all except checkbox, select, file |
/// | `autocomplete` | all except checkbox, file |
/// | `disabled` | all |
///
/// - [`data`](Self::data) and [`hx`](Self::hx) add `data-*` and `hx-*`
///   attributes. The helper adds the prefix, so the name cannot be a reserved
///   attribute. A name must be `[a-z0-9-]` (`hx` also accepts `:`). The setter
///   drops a name that does not match, and a `data-autumn-*` name (Autumn uses
///   these). An htmx helper drops an `hx-*` name that it sets. If you set one
///   name two times, the last value applies.
///
/// The browser does not submit the value of a `disabled` control.
///
/// # Example
///
/// ```rust
/// use autumn_web::form::{Changeset, FieldOptions, text_input_with};
///
/// #[derive(serde::Serialize)]
/// struct Signup { email: String }
///
/// let changeset = Changeset::new(Signup { email: String::new() });
/// let html = text_input_with(
///     &changeset,
///     "email",
///     "Email",
///     &FieldOptions::new()
///         .hint("We never share it.")
///         .required()
///         .autocomplete("email")
///         .data("test-id", "signup-email"),
/// )
/// .into_string();
///
/// assert!(html.contains(r#"aria-describedby="email-hint""#));
/// assert!(html.contains(r#"<p id="email-hint" class="autumn-field__hint">"#));
/// assert!(html.contains(r#"autocomplete="email""#));
/// assert!(html.contains(r#"data-test-id="signup-email""#));
/// ```
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldOptions<'a> {
    hint: Option<Cow<'a, str>>,
    required: bool,
    disabled: bool,
    readonly: bool,
    placeholder: Option<Cow<'a, str>>,
    autocomplete: Option<Cow<'a, str>>,
    inputmode: Option<Cow<'a, str>>,
    pattern: Option<Cow<'a, str>>,
    min: Option<Cow<'a, str>>,
    max: Option<Cow<'a, str>>,
    auto_grow: bool,
    /// `(prefix, name, value)`. The setters check `name`.
    extra: Vec<(&'static str, Cow<'a, str>, Cow<'a, str>)>,
}

#[cfg(feature = "maud")]
impl<'a> FieldOptions<'a> {
    /// No hint, not required, no extra attributes.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            hint: None,
            required: false,
            disabled: false,
            readonly: false,
            placeholder: None,
            autocomplete: None,
            inputmode: None,
            pattern: None,
            min: None,
            max: None,
            auto_grow: false,
            extra: Vec::new(),
        }
    }

    /// Show `text` as a hint. Add the hint `id` to `aria-describedby`.
    #[must_use]
    pub fn hint(mut self, text: impl Into<Cow<'a, str>>) -> Self {
        self.hint = Some(text.into());
        self
    }

    /// Mark the field required: `required`, `aria-required="true"` and the
    /// visual marker.
    #[must_use]
    pub const fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Add `disabled`. The browser does not submit the value.
    #[must_use]
    pub const fn disabled(mut self) -> Self {
        self.disabled = true;
        self
    }

    /// Add `readonly`.
    #[must_use]
    pub const fn readonly(mut self) -> Self {
        self.readonly = true;
        self
    }

    /// Set `placeholder`. Use [`hint`](Self::hint) for instructions. The
    /// browser removes the placeholder when the user types.
    #[must_use]
    pub fn placeholder(mut self, text: impl Into<Cow<'a, str>>) -> Self {
        self.placeholder = Some(text.into());
        self
    }

    /// Set `autocomplete` (for example `"email"` or `"current-password"`).
    #[must_use]
    pub fn autocomplete(mut self, token: impl Into<Cow<'a, str>>) -> Self {
        self.autocomplete = Some(token.into());
        self
    }

    /// Set `inputmode` (for example `"numeric"`).
    #[must_use]
    pub fn inputmode(mut self, mode: impl Into<Cow<'a, str>>) -> Self {
        self.inputmode = Some(mode.into());
        self
    }

    /// Set `pattern`.
    #[must_use]
    pub fn pattern(mut self, regex: impl Into<Cow<'a, str>>) -> Self {
        self.pattern = Some(regex.into());
        self
    }

    /// Set `min`.
    #[must_use]
    pub fn min(mut self, min: impl Into<Cow<'a, str>>) -> Self {
        self.min = Some(min.into());
        self
    }

    /// Set `max`.
    #[must_use]
    pub fn max(mut self, max: impl Into<Cow<'a, str>>) -> Self {
        self.max = Some(max.into());
        self
    }

    /// Let a `<textarea>` grow with its text (`field-sizing: content`). The
    /// textarea gets the `autumn-auto-grow` class. Other controls ignore
    /// it. A browser with no `field-sizing` keeps the fixed height.
    #[must_use]
    pub const fn auto_grow(mut self) -> Self {
        self.auto_grow = true;
        self
    }

    /// Add `data-{name}="{value}"`. The setter drops `name` if it is not
    /// `[a-z0-9-]+` or if it starts with `autumn-`.
    #[must_use]
    pub fn data(self, name: impl Into<Cow<'a, str>>, value: impl Into<Cow<'a, str>>) -> Self {
        let name = name.into();
        if name.starts_with("autumn-") || !is_valid_extra_name(&name, false) {
            return self;
        }
        self.with_extra("data-", name, value.into())
    }

    /// Add `hx-{name}="{value}"`, for example `.hx("indicator", "#spin")`.
    /// The setter drops `name` if it is not `[a-z0-9-:]+`.
    #[must_use]
    pub fn hx(self, name: impl Into<Cow<'a, str>>, value: impl Into<Cow<'a, str>>) -> Self {
        let name = name.into();
        if !is_valid_extra_name(&name, true) {
            return self;
        }
        self.with_extra("hx-", name, value.into())
    }

    /// Whether [`required`](Self::required) is set.
    #[must_use]
    pub const fn is_required(&self) -> bool {
        self.required
    }

    /// Whether [`disabled`](Self::disabled) is set.
    #[must_use]
    pub const fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// Whether [`readonly`](Self::readonly) is set.
    #[must_use]
    pub const fn is_readonly(&self) -> bool {
        self.readonly
    }

    /// Whether [`auto_grow`](Self::auto_grow) is set.
    #[must_use]
    pub const fn is_auto_grow(&self) -> bool {
        self.auto_grow
    }

    /// The hint text, if set.
    #[must_use]
    pub fn hint_text(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    #[cfg_attr(not(feature = "nested-form"), allow(dead_code))]
    /// Clear [`required`](Self::required).
    pub(crate) const fn optional(mut self) -> Self {
        self.required = false;
        self
    }

    /// Store an extra attribute. A second value for one name replaces the
    /// first, so the output has no duplicate attribute.
    fn with_extra(mut self, prefix: &'static str, name: Cow<'a, str>, value: Cow<'a, str>) -> Self {
        if let Some(slot) = self
            .extra
            .iter_mut()
            .find(|(p, n, _)| *p == prefix && *n == name)
        {
            slot.2 = value;
        } else {
            self.extra.push((prefix, name, value));
        }
        self
    }
}

/// `true` when `name` is a safe `data-*` / `hx-*` suffix. The alphabet has
/// no space, `=`, `"` or `>`, so the name cannot end the attribute.
#[cfg(feature = "maud")]
fn is_valid_extra_name(name: &str, allow_colon: bool) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || (allow_colon && b == b':')
        })
}

/// The `hx-*` names the htmx helpers set. A passthrough cannot replace them:
/// a caller `hx-params` would send `_submit_token` with the validation POST
/// (issue #1360).
#[cfg(feature = "maud")]
const HTMX_OWNED: &[&str] = &["post", "trigger", "target", "swap", "include", "params"];

/// The element a field helper renders.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldKind {
    Text,
    Password,
    Textarea,
    Checkbox,
    Number,
    Date,
    DateTime,
    Select,
    File,
}

#[cfg(feature = "maud")]
impl FieldKind {
    const fn input_type(self) -> Option<&'static str> {
        match self {
            Self::Text => Some("text"),
            Self::Password => Some("password"),
            Self::Checkbox => Some("checkbox"),
            Self::Number => Some("number"),
            Self::Date => Some("date"),
            Self::DateTime => Some("datetime-local"),
            Self::File => Some("file"),
            Self::Textarea | Self::Select => None,
        }
    }

    /// `placeholder` and `inputmode`.
    const fn takes_text_entry(self) -> bool {
        matches!(
            self,
            Self::Text | Self::Password | Self::Textarea | Self::Number
        )
    }

    const fn takes_pattern(self) -> bool {
        matches!(self, Self::Text | Self::Password)
    }

    const fn takes_range(self) -> bool {
        matches!(self, Self::Number | Self::Date | Self::DateTime)
    }

    const fn takes_readonly(self) -> bool {
        !matches!(self, Self::Checkbox | Self::Select | Self::File)
    }

    const fn takes_autocomplete(self) -> bool {
        !matches!(self, Self::Checkbox | Self::File)
    }
}

/// The `<option>` pairs of a select, borrowed or owned.
#[cfg(feature = "maud")]
#[derive(Clone, Copy)]
pub(crate) enum SelectChoices<'a> {
    None,
    Borrowed(&'a [(&'a str, &'a str)]),
    Owned(&'a [(String, String)]),
}

/// What [`render_field`] renders. The helpers fill it from the changeset.
#[cfg(feature = "maud")]
pub(crate) struct FieldSpec<'a> {
    pub(crate) kind: FieldKind,
    /// The `id`, and the base of the `-field`, `-hint` and `-error` ids.
    pub(crate) field: &'a str,
    /// The `name`, when it is not `field` (a nested row).
    pub(crate) name: Option<&'a str>,
    pub(crate) label: &'a str,
    /// The `value` attribute, or the textarea body. `None` for password and
    /// file (never pre-filled) and for select.
    pub(crate) value: Option<&'a str>,
    /// The current value a select marks `selected`.
    pub(crate) current: &'a str,
    pub(crate) checked: bool,
    pub(crate) step: Option<&'a str>,
    pub(crate) choices: SelectChoices<'a>,
    /// `(validate_url, token_field)` for the htmx inline-validation input.
    pub(crate) htmx: Option<(&'a str, &'a str)>,
}

#[cfg(feature = "maud")]
impl<'a> FieldSpec<'a> {
    pub(crate) const fn new(kind: FieldKind, field: &'a str, label: &'a str) -> Self {
        Self {
            kind,
            field,
            name: None,
            label,
            value: None,
            current: "",
            checked: false,
            step: None,
            choices: SelectChoices::None,
            htmx: None,
        }
    }
}

/// Append ` {name}="{value}"` with the value escaped.
#[cfg(feature = "maud")]
pub(crate) fn push_attr(out: &mut String, name: &str, value: &str) {
    out.push(' ');
    out.push_str(name);
    out.push_str("=\"");
    write_escaped(value, out);
    out.push('"');
}

/// The required marker. `aria-hidden` stops a second "required" from a
/// screen reader: `aria-required` already gives the state.
#[cfg(feature = "maud")]
const REQUIRED_MARKER: &str = r#"<span class="autumn-field__required" aria-hidden="true">*</span>"#;

/// Append the `<label>` (with the required marker when `required`).
#[cfg(feature = "maud")]
fn push_label(out: &mut String, field_html: &str, label: &str, required: bool) {
    out.push_str(r#"<label for=""#);
    out.push_str(field_html);
    out.push_str(r#"" class="autumn-field__label">"#);
    write_escaped(label, out);
    if required {
        out.push_str(REQUIRED_MARKER);
    }
    out.push_str("</label>");
}

/// Append the typed options and the `data-*`/`hx-*` passthrough. `reserved`
/// lists the `hx-*` names the helper already set.
#[cfg(feature = "maud")]
fn push_option_attrs(
    out: &mut String,
    kind: Option<FieldKind>,
    opts: &FieldOptions<'_>,
    reserved: &[&str],
) {
    // `None` is a custom control: it gets each option the app set.
    let takes = |accepts: fn(FieldKind) -> bool| kind.is_none_or(accepts);
    let typed = [
        (
            "placeholder",
            opts.placeholder.as_deref(),
            takes(FieldKind::takes_text_entry),
        ),
        (
            "autocomplete",
            opts.autocomplete.as_deref(),
            takes(FieldKind::takes_autocomplete),
        ),
        (
            "inputmode",
            opts.inputmode.as_deref(),
            takes(FieldKind::takes_text_entry),
        ),
        (
            "pattern",
            opts.pattern.as_deref(),
            takes(FieldKind::takes_pattern),
        ),
        ("min", opts.min.as_deref(), takes(FieldKind::takes_range)),
        ("max", opts.max.as_deref(), takes(FieldKind::takes_range)),
    ];
    for (name, value, accepted) in typed {
        if accepted && let Some(value) = value {
            push_attr(out, name, value);
        }
    }
    if opts.disabled {
        out.push_str(" disabled");
    }
    if opts.readonly && takes(FieldKind::takes_readonly) {
        out.push_str(" readonly");
    }
    for (prefix, name, value) in &opts.extra {
        if *prefix == "hx-" && reserved.contains(&name.as_ref()) {
            continue;
        }
        out.push(' ');
        out.push_str(prefix);
        out.push_str(name);
        out.push_str("=\"");
        write_escaped(value, out);
        out.push('"');
    }
}

/// Append `<p id="{field}-hint">`.
#[cfg(feature = "maud")]
fn push_hint(out: &mut String, field_html: &str, hint: &str) {
    out.push_str(r#"<p id=""#);
    out.push_str(field_html);
    out.push_str(r#"-hint" class="autumn-field__hint">"#);
    write_escaped(hint, out);
    out.push_str("</p>");
}

/// Append the `role="alert"` error block.
#[cfg(feature = "maud")]
fn push_errors(out: &mut String, field_html: &str, errors: &[String]) {
    out.push_str(r#"<div id=""#);
    out.push_str(field_html);
    out.push_str(r#"-error" role="alert" class="autumn-field__errors">"#);
    for error in errors {
        out.push_str(r#"<p class="autumn-field__error">"#);
        write_escaped(error, out);
        out.push_str("</p>");
    }
    out.push_str("</div>");
}

/// The value of `aria-describedby`: the hint id, then the error id.
/// `None` when the field has neither, so the attribute is not emitted.
#[cfg(feature = "maud")]
fn described_by(field_html: &str, has_hint: bool, has_errors: bool) -> Option<String> {
    match (has_hint, has_errors) {
        (false, false) => None,
        (true, false) => Some(concat_suffix(field_html, "-hint")),
        (false, true) => Some(concat_suffix(field_html, "-error")),
        (true, true) => {
            let mut ids =
                String::with_capacity(field_html.len().saturating_mul(2).saturating_add(12));
            ids.push_str(field_html);
            ids.push_str("-hint ");
            ids.push_str(field_html);
            ids.push_str("-error");
            Some(ids)
        }
    }
}

/// Render one field: wrapper, label, hint, control and errors.
///
/// Every changeset helper, every [`FieldControl`] arm of [`form_for`] and the
/// nested-row helpers use this function, so all wrappers are the same. This
/// function writes the markup directly, because maud needs static attribute
/// names and the passthrough names are dynamic. Each value goes through
/// [`write_escaped`], the same escape maud uses.
#[cfg(feature = "maud")]
#[allow(
    clippy::too_many_lines,
    reason = "one linear writer for the whole field; splitting it hides the attribute order"
)]
pub(crate) fn render_field(
    spec: &FieldSpec<'_>,
    errors: &[String],
    opts: &FieldOptions<'_>,
) -> maud::Markup {
    let kind = spec.kind;
    let has_errors = !errors.is_empty();
    let hint = opts.hint.as_deref();
    let field_html = fast_escape(spec.field);
    let field_html = field_html.as_ref();
    let describedby = described_by(field_html, hint.is_some(), has_errors);

    let errors_len: usize = errors.iter().map(|e| e.len().saturating_add(40)).sum();
    let mut out = String::with_capacity(
        360usize
            .saturating_add(field_html.len().saturating_mul(6))
            .saturating_add(spec.label.len())
            .saturating_add(spec.value.map_or(0, str::len))
            .saturating_add(hint.map_or(0, str::len))
            .saturating_add(errors_len),
    );

    out.push_str(r#"<div id=""#);
    out.push_str(field_html);
    out.push_str(r#"-field" class=""#);
    out.push_str(if kind == FieldKind::Checkbox {
        "autumn-field autumn-field--checkbox"
    } else {
        "autumn-field"
    });
    out.push('"');
    if spec.htmx.is_some() {
        out.push_str(r#" data-autumn-field-wrapper=""#);
        out.push_str(field_html);
        out.push('"');
    }
    out.push('>');

    // A checkbox shows its box first, then the label. Other controls show
    // the label and the hint first, so the user reads the hint before typing.
    if kind != FieldKind::Checkbox {
        push_label(&mut out, field_html, spec.label, opts.required);
        if let Some(hint) = hint {
            push_hint(&mut out, field_html, hint);
        }
    }

    let tag = match kind {
        FieldKind::Textarea => "textarea",
        FieldKind::Select => "select",
        _ => "input",
    };
    out.push('<');
    out.push_str(tag);
    if let Some(input_type) = kind.input_type() {
        push_attr(&mut out, "type", input_type);
    }
    out.push_str(r#" id=""#);
    out.push_str(field_html);
    out.push('"');
    if let Some(name) = spec.name {
        push_attr(&mut out, "name", name);
    } else {
        out.push_str(r#" name=""#);
        out.push_str(field_html);
        out.push('"');
    }
    if kind == FieldKind::Checkbox {
        out.push_str(r#" value="true""#);
        if spec.checked {
            out.push_str(" checked");
        }
    } else if kind.input_type().is_some()
        && let Some(value) = spec.value
    {
        push_attr(&mut out, "value", value);
    }
    if let Some(step) = spec.step {
        push_attr(&mut out, "step", step);
    }
    if opts.required {
        out.push_str(r#" required aria-required="true""#);
    }
    out.push_str(r#" class="autumn-field__input"#);
    if has_errors {
        out.push_str(" autumn-field__input--invalid");
    }
    if kind == FieldKind::Textarea && opts.auto_grow {
        out.push_str(" autumn-auto-grow");
    }
    out.push_str(if has_errors {
        r#"" aria-invalid="true""#
    } else {
        r#"" aria-invalid="false""#
    });
    if let Some(ids) = &describedby {
        out.push_str(r#" aria-describedby=""#);
        out.push_str(ids);
        out.push('"');
    }
    let reserved: &[&str] = if let Some((validate_url, token_field)) = spec.htmx {
        push_attr(&mut out, "hx-post", validate_url);
        out.push_str(
            r#" hx-trigger="change" hx-target="closest [data-autumn-field-wrapper]" hx-swap="outerHTML" hx-include="closest form" hx-params="not "#,
        );
        write_escaped(token_field, &mut out);
        out.push('"');
        HTMX_OWNED
    } else {
        &[]
    };
    push_option_attrs(&mut out, Some(kind), opts, reserved);
    out.push('>');

    match kind {
        FieldKind::Textarea => {
            write_escaped(spec.value.unwrap_or_default(), &mut out);
            out.push_str("</textarea>");
        }
        FieldKind::Select => {
            let mut push_option = |value: &str, label: &str| {
                out.push_str("<option");
                push_attr(&mut out, "value", value);
                if value == spec.current {
                    out.push_str(" selected");
                }
                out.push('>');
                write_escaped(label, &mut out);
                out.push_str("</option>");
            };
            match spec.choices {
                SelectChoices::None => {}
                SelectChoices::Borrowed(choices) => {
                    for (value, label) in choices {
                        push_option(value, label);
                    }
                }
                SelectChoices::Owned(choices) => {
                    for (value, label) in choices {
                        push_option(value, label);
                    }
                }
            }
            out.push_str("</select>");
        }
        _ => {}
    }

    if kind == FieldKind::Checkbox {
        push_label(&mut out, field_html, spec.label, opts.required);
        if let Some(hint) = hint {
            push_hint(&mut out, field_html, hint);
        }
    }
    if has_errors {
        push_errors(&mut out, field_html, errors);
    }
    out.push_str("</div>");
    maud::PreEscaped(out)
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
    text_input_with(changeset, field, label, &FieldOptions::new())
}

/// [`text_input`] with a hint and typed attributes. See [`FieldOptions`].
#[cfg(feature = "maud")]
#[must_use]
pub fn text_input_with<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    options: &FieldOptions<'_>,
) -> maud::Markup {
    let value = changeset.field_value(field).unwrap_or_default();
    let spec = FieldSpec {
        value: Some(&value),
        ..FieldSpec::new(FieldKind::Text, field, label)
    };
    render_field(&spec, changeset.errors_for(field), options)
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
    text_input_htmx_with(
        changeset,
        field,
        label,
        validate_url,
        token_field,
        &FieldOptions::new(),
    )
}

/// [`text_input_htmx_with_token_field`] with a hint and typed attributes.
///
/// See [`FieldOptions`]. The helper keeps its own `hx-post`, `hx-trigger`,
/// `hx-target`, `hx-swap`, `hx-include` and `hx-params`, and drops a
/// passthrough with one of these names.
#[cfg(feature = "maud")]
#[must_use]
pub fn text_input_htmx_with<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    validate_url: &str,
    token_field: &str,
    options: &FieldOptions<'_>,
) -> maud::Markup {
    let value = changeset.field_value(field).unwrap_or_default();
    let spec = FieldSpec {
        value: Some(&value),
        htmx: Some((validate_url, token_field)),
        ..FieldSpec::new(FieldKind::Text, field, label)
    };
    render_field(&spec, changeset.errors_for(field), options)
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
    text_input_htmx_with(
        changeset,
        field,
        label,
        validate_url,
        token_field,
        &FieldOptions::new().required(),
    )
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
    password_input_with(changeset, field, label, &FieldOptions::new())
}

/// [`password_input`] for a required field. See [`required_text_input`].
#[cfg(feature = "maud")]
#[must_use]
pub fn required_password_input<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
) -> maud::Markup {
    password_input_with(changeset, field, label, &FieldOptions::new().required())
}

/// [`password_input`] with a hint and typed attributes. See [`FieldOptions`].
/// The value is never rendered.
#[cfg(feature = "maud")]
#[must_use]
pub fn password_input_with<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    options: &FieldOptions<'_>,
) -> maud::Markup {
    let spec = FieldSpec::new(FieldKind::Password, field, label);
    render_field(&spec, changeset.errors_for(field), options)
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
    textarea_input_with(changeset, field, label, &FieldOptions::new())
}

/// [`textarea_input`] for a required field. See [`required_text_input`].
#[cfg(feature = "maud")]
#[must_use]
pub fn required_textarea_input<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
) -> maud::Markup {
    textarea_input_with(changeset, field, label, &FieldOptions::new().required())
}

/// [`textarea_input`] with a hint and typed attributes. See [`FieldOptions`].
#[cfg(feature = "maud")]
#[must_use]
pub fn textarea_input_with<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    options: &FieldOptions<'_>,
) -> maud::Markup {
    let value = changeset.field_value(field).unwrap_or_default();
    let spec = FieldSpec {
        value: Some(&value),
        ..FieldSpec::new(FieldKind::Textarea, field, label)
    };
    render_field(&spec, changeset.errors_for(field), options)
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
/// `count=`, the decoder fails on the empty string, and the fragment
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
/// repeated key wins. This helper is lenient on purpose: [`ChangesetForm`]
/// rejects a repeated single-value key (422). A present-but-empty field
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

/// Labels for the rich text editor chrome. Set a label to change its text.
///
/// The default labels are English. Pass a customized `RichTextLabels` to a
/// `_with_labels` function (e.g. [`rich_text_area_with_labels`]) to translate
/// the toolbar, hint, and preview heading for a non-English locale.
#[cfg(feature = "maud")]
#[derive(Clone, Copy)]
pub struct RichTextLabels<'a> {
    toolbar_group: &'a str,
    controls: &'a [(&'a str, &'a str)],
    hint: &'a str,
    preview_heading: &'a str,
}

#[cfg(feature = "maud")]
impl<'a> RichTextLabels<'a> {
    /// Make the default English labels.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            toolbar_group: "Markdown formatting",
            controls: RICH_TEXT_TOOLBAR,
            hint: RICH_TEXT_HINT,
            preview_heading: "Preview",
        }
    }

    /// Set the toolbar group label.
    #[must_use]
    pub const fn toolbar_group(mut self, label: &'a str) -> Self {
        self.toolbar_group = label;
        self
    }

    /// Set the toolbar control names and syntax hints.
    #[must_use]
    pub const fn controls(mut self, controls: &'a [(&'a str, &'a str)]) -> Self {
        self.controls = controls;
        self
    }

    /// Set the hint text under the editor.
    #[must_use]
    pub const fn hint(mut self, hint: &'a str) -> Self {
        self.hint = hint;
        self
    }

    /// Set the preview pane heading.
    #[must_use]
    pub const fn preview_heading(mut self, heading: &'a str) -> Self {
        self.preview_heading = heading;
        self
    }
}

#[cfg(feature = "maud")]
impl Default for RichTextLabels<'static> {
    fn default() -> Self {
        Self::new()
    }
}

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
    rich_text_area_inner(
        changeset,
        field,
        label,
        None,
        &RichTextLabels::new(),
        &FieldOptions::new(),
    )
}

/// Like [`rich_text_area`] but with caller-supplied chrome labels, for
/// translating the toolbar, hint, and preview heading (issue #2227).
#[cfg(feature = "maud")]
#[must_use]
pub fn rich_text_area_with_labels<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    labels: &RichTextLabels<'_>,
) -> maud::Markup {
    rich_text_area_inner(changeset, field, label, None, labels, &FieldOptions::new())
}

/// [`rich_text_area_with_labels`] with typed attributes. See
/// [`FieldOptions`]. A [`FieldOptions::hint`] replaces the chrome hint text
/// ([`RichTextLabels::hint`]) and keeps its `{field}-hint` id.
#[cfg(feature = "maud")]
#[must_use]
pub fn rich_text_area_with<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    labels: &RichTextLabels<'_>,
    options: &FieldOptions<'_>,
) -> maud::Markup {
    rich_text_area_inner(changeset, field, label, None, labels, options)
}

/// [`rich_text_area_htmx_with_token_field_with_labels`] with typed
/// attributes.
///
/// See [`rich_text_area_with`]. The helper keeps its own `hx-*`
/// preview attributes and drops a passthrough with one of these names.
#[cfg(feature = "maud")]
#[must_use]
pub fn rich_text_area_htmx_with<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    preview_url: &str,
    token_field: &str,
    labels: &RichTextLabels<'_>,
    options: &FieldOptions<'_>,
) -> maud::Markup {
    rich_text_area_inner(
        changeset,
        field,
        label,
        Some((preview_url, token_field)),
        labels,
        options,
    )
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

/// Like [`rich_text_area_htmx`] but with caller-supplied chrome labels; see
/// [`rich_text_area_with_labels`] for why the labels seam exists.
#[cfg(feature = "maud")]
#[must_use]
pub fn rich_text_area_htmx_with_labels<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    preview_url: &str,
    labels: &RichTextLabels<'_>,
) -> maud::Markup {
    rich_text_area_htmx_with_token_field_with_labels(
        changeset,
        field,
        label,
        preview_url,
        DEFAULT_SUBMIT_TOKEN_FIELD,
        labels,
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
        &RichTextLabels::new(),
        &FieldOptions::new(),
    )
}

/// Like [`rich_text_area_htmx_with_token_field`] but with caller-supplied
/// chrome labels; see [`rich_text_area_with_labels`] for why the labels seam
/// exists.
#[cfg(feature = "maud")]
#[must_use]
pub fn rich_text_area_htmx_with_token_field_with_labels<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    preview_url: &str,
    token_field: &str,
    labels: &RichTextLabels<'_>,
) -> maud::Markup {
    rich_text_area_inner(
        changeset,
        field,
        label,
        Some((preview_url, token_field)),
        labels,
        &FieldOptions::new(),
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
    rich_text_area_inner(
        changeset,
        field,
        label,
        None,
        &RichTextLabels::new(),
        &FieldOptions::new().required(),
    )
}

/// Like [`required_rich_text_area`] but with caller-supplied chrome labels;
/// see [`rich_text_area_with_labels`] for why the labels seam exists.
#[cfg(feature = "maud")]
#[must_use]
pub fn required_rich_text_area_with_labels<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    labels: &RichTextLabels<'_>,
) -> maud::Markup {
    rich_text_area_inner(
        changeset,
        field,
        label,
        None,
        labels,
        &FieldOptions::new().required(),
    )
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

/// Like [`required_rich_text_area_htmx`] but with caller-supplied chrome
/// labels; see [`rich_text_area_with_labels`] for why the labels seam exists.
#[cfg(feature = "maud")]
#[must_use]
pub fn required_rich_text_area_htmx_with_labels<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    preview_url: &str,
    labels: &RichTextLabels<'_>,
) -> maud::Markup {
    required_rich_text_area_htmx_with_token_field_with_labels(
        changeset,
        field,
        label,
        preview_url,
        DEFAULT_SUBMIT_TOKEN_FIELD,
        labels,
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
        &RichTextLabels::new(),
        &FieldOptions::new().required(),
    )
}

/// Like [`required_rich_text_area_htmx_with_token_field`] but with
/// caller-supplied chrome labels; see [`rich_text_area_with_labels`] for why
/// the labels seam exists.
#[cfg(feature = "maud")]
#[must_use]
pub fn required_rich_text_area_htmx_with_token_field_with_labels<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    preview_url: &str,
    token_field: &str,
    labels: &RichTextLabels<'_>,
) -> maud::Markup {
    rich_text_area_inner(
        changeset,
        field,
        label,
        Some((preview_url, token_field)),
        labels,
        &FieldOptions::new().required(),
    )
}

/// Shared body of [`rich_text_area`] and its htmx variant. `preview` is
/// `Some((preview_url, token_field))` for the live-preview flavour. `labels`
/// carries the chrome text (toolbar, hint, preview heading) — pass
/// `&RichTextLabels::new()` for the default English labels.
#[cfg(feature = "maud")]
fn rich_text_area_inner<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    preview: Option<(&str, &str)>,
    labels: &RichTextLabels<'_>,
    options: &FieldOptions<'_>,
) -> maud::Markup {
    let required = options.required;
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
    let hint = options.hint.as_deref().unwrap_or(labels.hint);

    // The editor is written directly, so the `data-*`/`hx-*` passthrough
    // (dynamic attribute names) can go on it.
    let mut editor = String::with_capacity(320usize.saturating_add(value.len()));
    editor.push_str("<textarea");
    push_attr(&mut editor, "id", field);
    push_attr(&mut editor, "name", field);
    editor.push_str(r#" rows="12""#);
    editor.push_str(r#" class="autumn-field__input autumn-rich-text__editor"#);
    if has_errors {
        editor.push_str(" autumn-field__input--invalid");
    }
    if options.auto_grow {
        editor.push_str(" autumn-auto-grow");
    }
    editor.push_str(if has_errors {
        r#"" aria-invalid="true""#
    } else {
        r#"" aria-invalid="false""#
    });
    push_attr(&mut editor, "aria-describedby", &described_by);
    if required {
        editor.push_str(r#" required aria-required="true""#);
    }
    let reserved: &[&str] = if let Some((url, token_field)) = preview {
        push_attr(&mut editor, "hx-post", url);
        editor.push_str(r##" hx-trigger="keyup changed delay:400ms, change" hx-target="#"##);
        write_escaped(&preview_id, &mut editor);
        editor.push_str(r#"" hx-swap="innerHTML" hx-include="closest form" hx-params="not "#);
        write_escaped(token_field, &mut editor);
        editor.push('"');
        HTMX_OWNED
    } else {
        &[]
    };
    push_option_attrs(&mut editor, Some(FieldKind::Textarea), options, reserved);
    editor.push('>');
    write_escaped(&value, &mut editor);
    editor.push_str("</textarea>");

    maud::html! {
        div id=(wrapper_id) class="autumn-field autumn-rich-text" data-autumn-field-wrapper=(field) {
            label for=(field) class="autumn-field__label" {
                (label)
                @if required { (maud::PreEscaped(REQUIRED_MARKER)) }
            }
            div class="autumn-rich-text__toolbar" role="group" aria-label=(labels.toolbar_group) {
                @for (control, syntax) in labels.controls {
                    span class="autumn-rich-text__toolbar-item" {
                        span class="autumn-rich-text__toolbar-label" { (control) }
                        code class="autumn-rich-text__toolbar-syntax" { (syntax) }
                    }
                }
            }
            (maud::PreEscaped(editor))
            p id=(hint_id) class="autumn-rich-text__hint" { (hint) }
            @if has_errors {
                div id=(error_id.as_deref().unwrap_or_default()) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (error) }
                    }
                }
            }
            @if preview.is_some() {
                div class="autumn-rich-text__preview-wrapper" {
                    span class="autumn-rich-text__preview-label" id=(format!("{field}-preview-label")) { (labels.preview_heading) }
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
    text_input_with(changeset, field, label, &FieldOptions::new().required())
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
/// `field=true` from the checkbox). A `bool` field takes one value, so axum's
/// `Form` rejects that body (400) and [`ChangesetForm`] rejects it (422)
/// instead of taking the last value — every checked submission would fail.
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
    checkbox_input_with(changeset, field, label, &FieldOptions::new())
}

/// [`checkbox_input`] for a box the user must check (for example "I accept
/// the terms"). See [`required_text_input`].
#[cfg(feature = "maud")]
#[must_use]
pub fn required_checkbox_input<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
) -> maud::Markup {
    checkbox_input_with(changeset, field, label, &FieldOptions::new().required())
}

/// [`checkbox_input`] with a hint and typed attributes. See [`FieldOptions`].
#[cfg(feature = "maud")]
#[must_use]
pub fn checkbox_input_with<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    options: &FieldOptions<'_>,
) -> maud::Markup {
    let spec = FieldSpec {
        checked: changeset.field_value(field).as_deref() == Some("true"),
        ..FieldSpec::new(FieldKind::Checkbox, field, label)
    };
    render_field(&spec, changeset.errors_for(field), options)
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
    number_input_with(changeset, field, label, step, &FieldOptions::new())
}

/// [`number_input`] with a hint and typed attributes. See [`FieldOptions`].
#[cfg(feature = "maud")]
#[must_use]
pub fn number_input_with<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    step: Option<&str>,
    options: &FieldOptions<'_>,
) -> maud::Markup {
    let value = changeset.field_value(field).unwrap_or_default();
    let spec = FieldSpec {
        value: Some(&value),
        step,
        ..FieldSpec::new(FieldKind::Number, field, label)
    };
    render_field(&spec, changeset.errors_for(field), options)
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
    number_input_with(
        changeset,
        field,
        label,
        step,
        &FieldOptions::new().required(),
    )
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
    date_input_with(changeset, field, label, &FieldOptions::new())
}

/// [`date_input`] with a hint and typed attributes. See [`FieldOptions`].
#[cfg(feature = "maud")]
#[must_use]
pub fn date_input_with<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    options: &FieldOptions<'_>,
) -> maud::Markup {
    let value = normalize_date_value(&changeset.field_value(field).unwrap_or_default());
    let spec = FieldSpec {
        value: Some(&value),
        ..FieldSpec::new(FieldKind::Date, field, label)
    };
    render_field(&spec, changeset.errors_for(field), options)
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
    date_input_with(changeset, field, label, &FieldOptions::new().required())
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
    datetime_input_with(changeset, field, label, &FieldOptions::new())
}

/// [`datetime_input`] with a hint and typed attributes. See [`FieldOptions`].
#[cfg(feature = "maud")]
#[must_use]
pub fn datetime_input_with<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    options: &FieldOptions<'_>,
) -> maud::Markup {
    let value = normalize_datetime_local_value(&changeset.field_value(field).unwrap_or_default());
    let spec = FieldSpec {
        value: Some(&value),
        step: Some("any"),
        ..FieldSpec::new(FieldKind::DateTime, field, label)
    };
    render_field(&spec, changeset.errors_for(field), options)
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
    datetime_input_with(changeset, field, label, &FieldOptions::new().required())
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
    select_input_with(changeset, field, label, options, &FieldOptions::new())
}

/// [`select_input`] with a hint and typed attributes. See [`FieldOptions`].
#[cfg(feature = "maud")]
#[must_use]
pub fn select_input_with<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    choices: &[(&str, &str)],
    options: &FieldOptions<'_>,
) -> maud::Markup {
    let current = changeset.field_value(field).unwrap_or_default();
    let spec = FieldSpec {
        current: &current,
        choices: SelectChoices::Borrowed(choices),
        ..FieldSpec::new(FieldKind::Select, field, label)
    };
    render_field(&spec, changeset.errors_for(field), options)
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
    select_input_with(
        changeset,
        field,
        label,
        options,
        &FieldOptions::new().required(),
    )
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
///
/// A plugin control uses [`FieldControl::Custom`] (issue #3400). It needs
/// the `maud` feature.
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
    /// A control from a widget pack (issue #3400).
    ///
    /// The [`FieldRenderer`]
    /// writes the control. The framework writes the wrapper, the label, the
    /// hint and the errors. Make one with [`FieldControl::custom`].
    #[cfg(feature = "maud")]
    Custom(CustomControl),
}

#[cfg(feature = "maud")]
impl FieldControl {
    /// A [`FieldControl::Custom`] that `renderer` writes.
    #[must_use]
    pub fn custom(renderer: impl FieldRenderer) -> Self {
        Self::Custom(CustomControl::new(renderer))
    }

    /// `true` when the control posts a file, so the form must be
    /// `multipart/form-data`.
    fn is_multipart(&self) -> bool {
        match self {
            Self::File => true,
            Self::Custom(control) => control.renderer().multipart(),
            _ => false,
        }
    }
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
    /// Mark the control `required` and `aria-required="true"`, with the
    /// visual marker. A checkbox and a password ignore this flag. Use
    /// [`FormFor::field_options`] with [`FieldOptions::required`] for them.
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
/// deliberately emits **no** hidden `false` fallback (a `bool` field takes one
/// value, so axum's `Form` would 400 and [`ChangesetForm`] would 422 every
/// *checked* submission). An unchecked box
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
        field_options: Vec::new(),
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
    field_options: Vec<(String, FieldOptions<'a>)>,
    prepended: Vec<maud::Markup>,
    appended: Vec<maud::Markup>,
    submit_label: String,
    force_multipart: bool,
}

#[cfg(feature = "maud")]
impl<'a, T: Serialize + FormModel> FormFor<'a, T> {
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

    /// Set the hint, required state and typed attributes for `field`. See
    /// [`FieldOptions`]. If you call this two times for one field, the last
    /// call applies.
    ///
    /// The field is required when [`FormField::required`] or
    /// [`FieldOptions::required`] is set. A checkbox and a password use only
    /// [`FieldOptions::required`]: a required checkbox means "must be
    /// checked", and a password is never pre-filled.
    #[must_use]
    pub fn field_options(mut self, field: impl Into<String>, options: FieldOptions<'a>) -> Self {
        self.field_options.push((field.into(), options));
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
            field_options,
            prepended,
            appended,
            submit_label,
            force_multipart,
        } = self;
        let no_options = FieldOptions::new();

        let fields = effective_form_fields::<T>(&excluded, &field_overrides, &label_overrides);
        let is_multipart = force_multipart || fields.iter().any(|f| f.control.is_multipart());

        let inner = maud::html! {
            @for markup in prepended {
                (markup)
            }
            @for field in &fields {
                (render_form_field(
                    changeset,
                    field,
                    field_options
                        .iter()
                        .rev()
                        .find(|(name, _)| *name == field.name)
                        .map_or(&no_options, |(_, options)| options),
                ))
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

/// Render a single [`FormField`], routing the pre-fill lookup through the
/// field's serialized key ([`FormField::value_name`]) when it differs from
/// the rendered input name — see [`PrefillAlias`]. Everything else (input
/// `name`/`id`, error lookup) stays keyed by [`FormField::name`].
#[cfg(feature = "maud")]
fn render_form_field<T: Serialize>(
    changeset: &Changeset<T>,
    field: &FormField,
    options: &FieldOptions<'_>,
) -> maud::Markup {
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
        return render_form_control(&aliased, field, options);
    }
    render_form_control(changeset, field, options)
}

/// Dispatch a single [`FormField`] to the matching `*_with` helper. Every
/// arm uses the same renderer, so the hint, marker and passthrough are the
/// same on each [`FieldControl`].
#[cfg(feature = "maud")]
fn render_form_control<T: Serialize>(
    changeset: &Changeset<T>,
    field: &FormField,
    options: &FieldOptions<'_>,
) -> maud::Markup {
    // A non-null `bool` is not a "must be checked" box. A password is never
    // pre-filled, so a required one blocks every edit form. These two controls
    // use only `FieldOptions::required`.
    let required_by_model = field.required
        && !matches!(
            field.control,
            FieldControl::Checkbox | FieldControl::Password
        );
    let merged;
    let options = if required_by_model && !options.required {
        merged = options.clone().required();
        &merged
    } else {
        options
    };
    let (name, label) = (field.name.as_str(), field.label.as_str());
    match &field.control {
        FieldControl::Text => text_input_with(changeset, name, label, options),
        FieldControl::Textarea => textarea_input_with(changeset, name, label, options),
        FieldControl::Password => password_input_with(changeset, name, label, options),
        FieldControl::Number { step } => {
            number_input_with(changeset, name, label, step.as_deref(), options)
        }
        FieldControl::Checkbox => checkbox_input_with(changeset, name, label, options),
        FieldControl::Date => date_input_with(changeset, name, label, options),
        FieldControl::DateTime => datetime_input_with(changeset, name, label, options),
        FieldControl::Select { options: choices } => {
            let current = changeset.field_value(name).unwrap_or_default();
            let spec = FieldSpec {
                current: &current,
                choices: SelectChoices::Owned(choices),
                ..FieldSpec::new(FieldKind::Select, name, label)
            };
            render_field(&spec, changeset.errors_for(name), options)
        }
        // The browser does not let markup fill a file input. The
        // `FieldControl::File` variant, not this arm, makes the form
        // multipart.
        FieldControl::File => {
            let spec = FieldSpec::new(FieldKind::File, name, label);
            render_field(&spec, changeset.errors_for(name), options)
        }
        FieldControl::Custom(control) => {
            custom_input_with(changeset, name, label, control.renderer(), options)
        }
    }
}

// ── Custom controls (issue #3400) ───────────────────────────────────

/// Writes the control of a plugin form field.
///
/// A widget pack implements this trait to bind its input to a
/// [`Changeset<T>`] ([`custom_input_with`]) and to [`form_for`]
/// ([`FieldControl::custom`]). The framework writes the wrapper `<div>`, the
/// `<label>`, the hint and the errors. The renderer writes only the control.
/// Use [`FieldContext::control`] for the control tag: it sets `id`, `name`,
/// `value`, `required`, the `aria-*` links and the passthrough.
///
/// # Example
///
/// ```rust
/// use autumn_web::form::{Changeset, FieldContext, FieldOptions, FieldRenderer, custom_input_with};
///
/// struct ColorInput;
///
/// impl FieldRenderer for ColorInput {
///     fn name(&self) -> &'static str {
///         "color"
///     }
///
///     fn render(&self, field: &FieldContext<'_>) -> maud::Markup {
///         field.control("input", &[("type", "color")], maud::html! {})
///     }
/// }
///
/// #[derive(serde::Serialize)]
/// struct Theme { accent: String }
///
/// let changeset = Changeset::new(Theme { accent: "#7c3aed".into() });
/// let html = custom_input_with(&changeset, "accent", "Accent", &ColorInput, &FieldOptions::new())
///     .into_string();
/// assert!(html.contains(r#"<label id="accent-label" for="accent" class="autumn-field__label">Accent</label>"#));
/// assert!(html.contains(r##"<input type="color" id="accent" name="accent" value="#7c3aed""##));
/// ```
#[cfg(feature = "maud")]
pub trait FieldRenderer: Send + Sync + 'static {
    /// A short name for `Debug` output, for example `"datepicker"`.
    fn name(&self) -> &'static str;

    /// Write the control for `field`.
    fn render(&self, field: &FieldContext<'_>) -> maud::Markup;

    /// `true` when the control posts a file. Then [`form_for`] sets
    /// `enctype="multipart/form-data"`. The default is `false`.
    fn multipart(&self) -> bool {
        false
    }

    /// `true` when the control is a group of inputs (for example radios).
    ///
    /// Then the wrapper is a `<fieldset>` with a `<legend>`, and the
    /// `aria-*` links go on the `<fieldset>`. The default is `false`.
    fn grouped(&self) -> bool {
        false
    }
}

/// A shared [`FieldRenderer`], held by [`FieldControl::Custom`].
///
/// Two values are equal only when they share one renderer (a clone).
#[cfg(feature = "maud")]
#[derive(Clone)]
pub struct CustomControl(std::sync::Arc<dyn FieldRenderer>);

#[cfg(feature = "maud")]
impl CustomControl {
    /// Wrap `renderer`.
    #[must_use]
    pub fn new(renderer: impl FieldRenderer) -> Self {
        Self(std::sync::Arc::new(renderer))
    }

    /// The renderer.
    #[must_use]
    pub fn renderer(&self) -> &dyn FieldRenderer {
        &*self.0
    }
}

#[cfg(feature = "maud")]
impl std::fmt::Debug for CustomControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CustomControl")
            .field(&self.0.name())
            .finish()
    }
}

#[cfg(feature = "maud")]
impl PartialEq for CustomControl {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

#[cfg(feature = "maud")]
impl Eq for CustomControl {}

/// What a [`FieldRenderer`] gets: the field ids, the current value, the
/// errors and the [`FieldOptions`].
#[cfg(feature = "maud")]
#[derive(Debug)]
pub struct FieldContext<'a> {
    field: &'a str,
    label: &'a str,
    value: Option<&'a str>,
    values: &'a [String],
    errors: &'a [String],
    options: &'a FieldOptions<'a>,
    /// The wrapper is a `<fieldset>` that holds the `aria-*` links.
    grouped: bool,
}

#[cfg(feature = "maud")]
impl<'a> FieldContext<'a> {
    /// The control `id`. The label, hint and error ids start with it.
    #[must_use]
    pub const fn id(&self) -> &'a str {
        self.field
    }

    /// The control `name`, the key the browser posts.
    #[must_use]
    pub const fn name(&self) -> &'a str {
        self.field
    }

    /// The label text. The framework writes the `<label>`.
    #[must_use]
    pub const fn label(&self) -> &'a str {
        self.label
    }

    /// The `id` of the `<label>`: `{field}-label`. A control that is not a
    /// form element points `aria-labelledby` at it.
    #[must_use]
    pub fn label_id(&self) -> String {
        concat_suffix(self.field, "-label")
    }

    /// The current value from the changeset, or `None`.
    #[must_use]
    pub const fn value(&self) -> Option<&'a str> {
        self.value
    }

    /// All values of a repeated field (for example a multi-select).
    #[must_use]
    pub const fn values(&self) -> &'a [String] {
        self.values
    }

    /// The validation errors for the field.
    #[must_use]
    pub const fn errors(&self) -> &'a [String] {
        self.errors
    }

    /// `true` when the field has errors.
    #[must_use]
    pub const fn is_invalid(&self) -> bool {
        !self.errors.is_empty()
    }

    /// `true` when the field is required.
    #[must_use]
    pub const fn is_required(&self) -> bool {
        self.options.required
    }

    /// The options the app set: hint, typed attributes and passthrough.
    #[must_use]
    pub const fn options(&self) -> &'a FieldOptions<'a> {
        self.options
    }

    /// The `aria-describedby` value (hint id, then error id), or `None`.
    /// The value is not HTML-escaped.
    #[must_use]
    pub fn described_by(&self) -> Option<String> {
        described_by(self.field, self.options.hint.is_some(), self.is_invalid())
    }

    /// Write the control element: `<{tag} …>{body}</{tag}>`.
    ///
    /// The method writes these attributes:
    ///
    /// - `id` and, on a form element, `name`.
    /// - `value` on an `input`, from the changeset. A `password`, `file`,
    ///   `checkbox` or `radio` input gets no changeset value.
    /// - `required` on `input`, `select` and `textarea`. `aria-required` on
    ///   those tags, or when `attrs` sets a `role`.
    /// - `class="autumn-field__input"`, with `--invalid` when the field has
    ///   errors.
    /// - `aria-invalid` and `aria-describedby`. A tag that is not a form
    ///   element also gets `aria-labelledby` to the label. A grouped control
    ///   has these links on the `<fieldset>`.
    /// - The [`FieldOptions`] attributes.
    ///
    /// `attrs` adds attributes, and their values are escaped. A `class`
    /// value goes after the framework class. A `value` replaces the
    /// changeset value. A `button` gets `type="button"` when `attrs` has no
    /// `type`. The method removes these names from `attrs`: a name that is
    /// not `[a-z][a-z0-9:-]*`, a name that the framework writes, an `on*` or
    /// `hx-on*` handler, and `style`. When a name occurs two times, the
    /// first one applies. A void tag (`input`) ignores `body`. The app
    /// passthrough in [`FieldOptions`] is not filtered: it works as on a core
    /// helper, and the app owns its CSP.
    ///
    /// # Panics
    ///
    /// Panics when `tag` is not `[a-z][a-z0-9]*`, or is a tag that loads
    /// code (`script`, `style`, `iframe`, `object`, …). A tag is fixed
    /// code, so this is a programmer error.
    #[must_use]
    pub fn control(
        &self,
        tag: &'static str,
        attrs: &[(&str, &str)],
        body: maud::Markup,
    ) -> maud::Markup {
        assert!(
            is_valid_control_tag(tag),
            "control tag {tag:?} is not [a-z][a-z0-9]* or loads code"
        );
        let attrs = clean_attrs(attrs, CONTROL_OWNED);
        let find = |wanted: &str| attrs.iter().find(|(name, _)| *name == wanted).map(|a| a.1);
        let form_element = is_form_element(tag);
        let input_type = find("type").or_else(|| (tag == "button").then_some("button"));
        let mut out = String::with_capacity(240usize.saturating_add(body.0.len()));

        out.push('<');
        out.push_str(tag);
        if let Some(input_type) = input_type {
            push_attr(&mut out, "type", input_type);
        }
        push_attr(&mut out, "id", self.field);
        if form_element {
            push_attr(&mut out, "name", self.field);
        }
        let prefill = tag == "input"
            && !input_type.is_some_and(|t| {
                ["password", "file", "checkbox", "radio"]
                    .iter()
                    .any(|no| t.eq_ignore_ascii_case(no))
            });
        match (find("value"), self.value) {
            (Some(value), _) => push_attr(&mut out, "value", value),
            (None, Some(value)) if prefill => push_attr(&mut out, "value", value),
            _ => {}
        }
        if self.is_required() {
            let native = matches!(tag, "input" | "select" | "textarea");
            if native {
                out.push_str(" required");
            }
            if native || find("role").is_some() {
                out.push_str(r#" aria-required="true""#);
            }
        }
        out.push_str(if self.is_invalid() {
            r#" class="autumn-field__input autumn-field__input--invalid"#
        } else {
            r#" class="autumn-field__input"#
        });
        if let Some(class) = find("class") {
            out.push(' ');
            write_escaped(class, &mut out);
        }
        out.push('"');
        if !self.grouped {
            out.push_str(if self.is_invalid() {
                r#" aria-invalid="true""#
            } else {
                r#" aria-invalid="false""#
            });
            if let Some(ids) = self.described_by() {
                push_attr(&mut out, "aria-describedby", &ids);
            }
            if !form_element && find("aria-label").is_none() && find("aria-labelledby").is_none() {
                push_attr(&mut out, "aria-labelledby", &self.label_id());
            }
        }
        push_option_attrs(&mut out, None, self.options, &[]);
        let skip = option_attr_names(self.options);
        for (name, value) in &attrs {
            if !matches!(*name, "type" | "value" | "class") && !skip.iter().any(|s| s == name) {
                push_attr(&mut out, name, value);
            }
        }
        close_element(&mut out, tag, body);
        maud::PreEscaped(out)
    }

    /// Write one input of a grouped control, for example one radio.
    ///
    /// The element gets `id="{field}-{suffix}"`, `name` (on a form
    /// element), `required` (on `input`, `select` and `textarea`),
    /// `aria-invalid="true"` when the field has errors, and the
    /// [`FieldOptions`] attributes. Set `type`, `value`, `class` and
    /// `checked` in `attrs`. The method removes the same names from `attrs`
    /// as [`control`](Self::control).
    ///
    /// # Panics
    ///
    /// Panics when `tag` is not `[a-z][a-z0-9]*`, or loads code.
    #[must_use]
    pub fn group_item(
        &self,
        tag: &'static str,
        suffix: &str,
        attrs: &[(&str, &str)],
        body: maud::Markup,
    ) -> maud::Markup {
        assert!(
            is_valid_control_tag(tag),
            "control tag {tag:?} is not [a-z][a-z0-9]* or loads code"
        );
        let attrs = clean_attrs(attrs, CONTROL_OWNED);
        let mut out = String::with_capacity(160usize.saturating_add(body.0.len()));
        out.push('<');
        out.push_str(tag);
        if let Some((_, input_type)) = attrs.iter().find(|(name, _)| *name == "type") {
            push_attr(&mut out, "type", input_type);
        }
        let mut id = String::with_capacity(
            self.field
                .len()
                .saturating_add(suffix.len())
                .saturating_add(1),
        );
        id.push_str(self.field);
        id.push('-');
        id.push_str(suffix);
        push_attr(&mut out, "id", &id);
        if is_form_element(tag) {
            push_attr(&mut out, "name", self.field);
        }
        if self.is_required() && matches!(tag, "input" | "select" | "textarea") {
            out.push_str(" required");
        }
        if self.is_invalid() {
            out.push_str(r#" aria-invalid="true""#);
        }
        push_option_attrs(&mut out, None, self.options, &[]);
        let skip = option_attr_names(self.options);
        for (name, value) in &attrs {
            if *name != "type" && !skip.iter().any(|s| s == name) {
                push_attr(&mut out, name, value);
            }
        }
        close_element(&mut out, tag, body);
        maud::PreEscaped(out)
    }
}

/// Write the root element of a widget that is not a form input.
///
/// The element gets `attrs`, then the app `data-*` and `hx-*` passthrough from `options`. See
/// [`crate::widget_pack::element`].
#[cfg(feature = "maud")]
pub(crate) fn widget_element(
    tag: &'static str,
    attrs: &[(&str, &str)],
    options: &FieldOptions<'_>,
    body: maud::Markup,
) -> maud::Markup {
    assert!(
        is_valid_control_tag(tag),
        "widget tag {tag:?} is not [a-z][a-z0-9]*"
    );
    let attrs = clean_attrs(attrs, &[]);
    let mut out = String::with_capacity(120usize.saturating_add(body.0.len()));
    out.push('<');
    out.push_str(tag);
    for (name, value) in &attrs {
        push_attr(&mut out, name, value);
    }
    for (prefix, name, value) in &options.extra {
        let full = format!("{prefix}{name}");
        if !attrs.iter().any(|(n, _)| *n == full) {
            push_attr(&mut out, &full, value);
        }
    }
    close_element(&mut out, tag, body);
    maud::PreEscaped(out)
}

/// The names [`FieldContext::control`] and [`FieldContext::group_item`]
/// write. A caller cannot set them.
#[cfg(feature = "maud")]
const CONTROL_OWNED: &[&str] = &[
    "id",
    "name",
    "required",
    "aria-required",
    "aria-invalid",
    "aria-describedby",
];

/// `true` for an element the browser submits and a `<label for>` names.
#[cfg(feature = "maud")]
fn is_form_element(tag: &str) -> bool {
    matches!(tag, "input" | "select" | "textarea" | "button")
}

/// Close a tag: the body and the end tag, or nothing for a void tag.
#[cfg(feature = "maud")]
fn close_element(out: &mut String, tag: &str, body: maud::Markup) {
    out.push('>');
    if !is_void_tag(tag) {
        out.push_str(&body.into_string());
        out.push_str("</");
        out.push_str(tag);
        out.push('>');
    }
}

/// The attribute names [`push_option_attrs`] writes for `opts` on a custom
/// control.
#[cfg(feature = "maud")]
fn option_attr_names(opts: &FieldOptions<'_>) -> Vec<String> {
    let typed = [
        ("placeholder", opts.placeholder.is_some()),
        ("autocomplete", opts.autocomplete.is_some()),
        ("inputmode", opts.inputmode.is_some()),
        ("pattern", opts.pattern.is_some()),
        ("min", opts.min.is_some()),
        ("max", opts.max.is_some()),
        ("disabled", opts.disabled),
        ("readonly", opts.readonly),
    ];
    typed
        .into_iter()
        .filter(|(_, set)| *set)
        .map(|(name, _)| name.to_owned())
        .chain(
            opts.extra
                .iter()
                .map(|(prefix, name, _)| format!("{prefix}{name}")),
        )
        .collect()
}

/// `true` when `tag` is `[a-z][a-z0-9]*` and is not a tag that loads or
/// runs code or changes the page (`script`, `style`, `iframe`, …).
#[cfg(feature = "maud")]
fn is_valid_control_tag(tag: &str) -> bool {
    tag.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && !matches!(
            tag,
            "script"
                | "style"
                | "iframe"
                | "frame"
                | "object"
                | "embed"
                | "base"
                | "link"
                | "meta"
                | "template"
        )
}

/// `true` for an HTML void element: it has no body and no end tag.
#[cfg(feature = "maud")]
fn is_void_tag(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Keep the attributes a widget may set.
///
/// Drop a malformed name, a name in
/// `owned`, an `on*` or `hx-on*` handler and `style`, and a second copy of
/// a name. An inline handler and `style` break a strict CSP.
#[cfg(feature = "maud")]
pub(crate) fn clean_attrs<'v>(
    attrs: &[(&'v str, &'v str)],
    owned: &[&str],
) -> Vec<(&'v str, &'v str)> {
    let mut kept: Vec<(&str, &str)> = Vec::with_capacity(attrs.len());
    for &(name, value) in attrs {
        let well_formed = name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b':');
        let handler =
            name.starts_with("on") || name.starts_with("hx-on") || name.starts_with("data-hx-on");
        if well_formed
            && !handler
            && name != "style"
            && !owned.contains(&name)
            && !kept.iter().any(|(k, _)| *k == name)
        {
            kept.push((name, value));
        }
    }
    kept
}

/// Render a [`FieldRenderer`] control bound to `changeset[field]`, with the
/// same wrapper, label, hint and errors as the core helpers.
///
/// A widget pack wraps this in its own helper, for example
/// `date_picker(changeset, "due", "Due", &options)`. In [`form_for`], use
/// [`FieldControl::custom`] instead.
#[cfg(feature = "maud")]
#[must_use]
pub fn custom_input_with<T: Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    renderer: &dyn FieldRenderer,
    options: &FieldOptions<'_>,
) -> maud::Markup {
    let value = changeset.field_value(field);
    let values = changeset.field_values(field);
    let context = FieldContext {
        field,
        label,
        value: value.as_deref(),
        values: &values,
        errors: changeset.errors_for(field),
        options,
        grouped: renderer.grouped(),
    };
    let control = renderer.render(&context);
    render_custom_field(&context, &control)
}

/// Write the wrapper, label, hint and errors around a custom `control`.
///
/// A grouped control gets a `<fieldset>` and a `<legend>`, and the
/// `<fieldset>` holds the `aria-*` links.
#[cfg(feature = "maud")]
fn render_custom_field(context: &FieldContext<'_>, control: &maud::Markup) -> maud::Markup {
    let field_html = fast_escape(context.field);
    let field_html = field_html.as_ref();
    let hint = context.options.hint.as_deref();
    let mut out = String::with_capacity(
        240usize
            .saturating_add(field_html.len().saturating_mul(4))
            .saturating_add(context.label.len())
            .saturating_add(control.0.len()),
    );
    let wrapper = if context.grouped { "fieldset" } else { "div" };
    out.push('<');
    out.push_str(wrapper);
    out.push_str(r#" id=""#);
    out.push_str(field_html);
    out.push_str(r#"-field" class="autumn-field"#);
    if context.grouped {
        out.push_str(r#" autumn-field--group""#);
        if context.is_invalid() {
            out.push_str(r#" aria-invalid="true""#);
        }
        if let Some(ids) = context.described_by() {
            push_attr(&mut out, "aria-describedby", &ids);
        }
        out.push_str(r#"><legend class="autumn-field__label">"#);
        write_escaped(context.label, &mut out);
        if context.is_required() {
            out.push_str(REQUIRED_MARKER);
        }
        out.push_str("</legend>");
    } else {
        // A label with an id: a control that is not a form element points
        // `aria-labelledby` at it.
        out.push_str(r#""><label id=""#);
        out.push_str(field_html);
        out.push_str(r#"-label" for=""#);
        out.push_str(field_html);
        out.push_str(r#"" class="autumn-field__label">"#);
        write_escaped(context.label, &mut out);
        if context.is_required() {
            out.push_str(REQUIRED_MARKER);
        }
        out.push_str("</label>");
    }
    if let Some(hint) = hint {
        push_hint(&mut out, field_html, hint);
    }
    out.push_str(&control.0);
    if context.is_invalid() {
        push_errors(&mut out, field_html, context.errors);
    }
    out.push_str("</");
    out.push_str(wrapper);
    out.push('>');
    maud::PreEscaped(out)
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

    // ── Repeated keys (issue #3394) ────────────────────────────────

    mod repeated_keys {
        use super::super::*;
        use std::collections::HashSet;

        #[derive(Debug, serde::Deserialize)]
        struct Numbers {
            #[serde(default)]
            a: Vec<u32>,
        }

        #[test]
        fn repeated_key_decodes_into_vec() {
            let form: Numbers = from_urlencoded(b"a=1&a=2").unwrap();
            assert_eq!(form.a, vec![1, 2]);
        }

        #[test]
        fn no_key_decodes_into_empty_vec() {
            let form: Numbers = from_urlencoded(b"").unwrap();
            assert!(form.a.is_empty());
        }

        #[test]
        fn single_key_decodes_into_one_element_vec() {
            let form: Numbers = from_urlencoded(b"a=7").unwrap();
            assert_eq!(form.a, vec![7]);
        }

        #[test]
        fn repeated_key_decodes_into_hash_set() {
            #[derive(serde::Deserialize)]
            struct Tags {
                #[serde(default)]
                tag: HashSet<String>,
            }
            let form: Tags = from_urlencoded(b"tag=a&tag=b&tag=a").unwrap();
            assert_eq!(form.tag, HashSet::from(["a".to_owned(), "b".to_owned()]));
        }

        #[test]
        fn option_vec_is_none_when_absent_and_some_when_present() {
            #[derive(serde::Deserialize)]
            struct F {
                ids: Option<Vec<i64>>,
            }
            let absent: F = from_urlencoded(b"").unwrap();
            assert_eq!(absent.ids, None);
            let one: F = from_urlencoded(b"ids=3").unwrap();
            assert_eq!(one.ids, Some(vec![3]));
            let two: F = from_urlencoded(b"ids=3&ids=4").unwrap();
            assert_eq!(two.ids, Some(vec![3, 4]));
        }

        #[test]
        fn percent_encoded_values_decode_into_vec() {
            #[derive(serde::Deserialize)]
            struct F {
                #[serde(default)]
                q: Vec<String>,
            }
            let form: F = from_urlencoded(b"q=a%20b&q=c+d&q=%26%3D&q=%C3%A9").unwrap();
            assert_eq!(form.q, vec!["a b", "c d", "&=", "é"]);
        }

        #[test]
        fn repeated_key_into_scalar_is_a_repeated_field_error() {
            #[derive(Debug, serde::Deserialize)]
            struct F {
                #[allow(dead_code)]
                name: String,
            }
            let err = from_urlencoded::<F>(b"name=a&name=b").unwrap_err();
            assert!(err.is_repeated_field(), "{err}");
            assert_eq!(err.field(), Some("name"));
            assert_eq!(err.status(), http::StatusCode::UNPROCESSABLE_ENTITY);
            assert!(err.to_string().contains("`name`"), "{err}");
        }

        #[test]
        fn repeated_blank_and_filled_scalar_is_still_repeated() {
            // The blank-drop retry must not remove `count=` and accept `5`.
            #[derive(Debug, serde::Deserialize)]
            struct F {
                #[allow(dead_code)]
                count: Option<i32>,
            }
            let err = from_urlencoded::<F>(b"count=&count=5").unwrap_err();
            assert!(err.is_repeated_field(), "{err}");
        }

        #[test]
        fn malformed_element_is_a_400_not_a_repeat() {
            let err = from_urlencoded::<Numbers>(b"a=1&a=x").unwrap_err();
            assert!(!err.is_repeated_field(), "{err}");
            assert_eq!(err.status(), http::StatusCode::BAD_REQUEST);
            assert_eq!(err.field(), Some("a"));
        }

        #[test]
        fn blank_element_of_a_typed_vec_is_dropped() {
            let form: Numbers = from_urlencoded(b"a=&a=2&a=").unwrap();
            assert_eq!(form.a, vec![2]);
        }

        #[test]
        fn blank_element_of_a_string_vec_is_kept() {
            #[derive(serde::Deserialize)]
            struct F {
                #[serde(default)]
                q: Vec<String>,
            }
            let form: F = from_urlencoded(b"q=&q=x").unwrap();
            assert_eq!(form.q, vec!["", "x"]);
        }

        #[test]
        fn repeated_field_error_bounds_a_hostile_key() {
            let key = format!("k{}\u{7}", "x".repeat(500));
            let body = format!("{key}=1&{key}=2");
            let err = from_urlencoded::<std::collections::HashMap<String, String>>(body.as_bytes())
                .unwrap_err();
            assert!(err.is_repeated_field(), "{err}");
            let text = err.to_string();
            assert!(text.len() < 300, "{text}");
            assert!(!text.contains('\u{7}'), "{text}");
        }

        #[test]
        fn single_value_fields_decode_as_before() {
            #[derive(Debug, PartialEq, serde::Deserialize)]
            struct F {
                name: String,
                age: Option<i32>,
                note: Option<String>,
                #[serde(default)]
                active: bool,
            }
            let form: F = from_urlencoded(b"name=Ann&age=&note=&active=true").unwrap();
            assert_eq!(
                form,
                F {
                    name: "Ann".to_owned(),
                    age: None,
                    note: Some(String::new()),
                    active: true,
                }
            );
        }

        #[test]
        fn repeated_field_error_converts_to_a_422_autumn_error() {
            #[derive(Debug, serde::Deserialize)]
            struct F {
                #[allow(dead_code)]
                name: String,
            }
            let err: crate::AutumnError =
                from_urlencoded::<F>(b"name=a&name=b").unwrap_err().into();
            assert_eq!(err.status(), http::StatusCode::UNPROCESSABLE_ENTITY);
        }

        #[test]
        fn many_blank_elements_decode_in_one_retry() {
            // Each retry drops every blank pair of one key. One retry per pair
            // made this body cost one full decode per blank (quadratic).
            let mut body = "ids=&".repeat(50_000);
            body.push_str("ids=1");
            let started = std::time::Instant::now();
            let form: Numbers = from_urlencoded(body.replace("ids", "a").as_bytes()).unwrap();
            assert_eq!(form.a, vec![1]);
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "{:?}",
                started.elapsed()
            );
        }

        #[test]
        fn invalid_body_converts_to_a_400_autumn_error() {
            let err: crate::AutumnError = from_urlencoded::<Numbers>(b"a=x").unwrap_err().into();
            assert_eq!(err.status(), http::StatusCode::BAD_REQUEST);
        }

        #[test]
        fn brackets_are_part_of_the_key_unless_aliased() {
            #[derive(serde::Deserialize)]
            struct Plain {
                #[serde(default)]
                ids: Vec<i64>,
            }
            #[derive(Debug, serde::Deserialize)]
            struct Aliased {
                #[serde(default, alias = "ids[]")]
                ids: Vec<i64>,
            }
            let plain: Plain = from_urlencoded(b"ids[]=1").unwrap();
            assert!(plain.ids.is_empty());
            let aliased: Aliased = from_urlencoded(b"ids[]=1&ids[]=2").unwrap();
            assert_eq!(aliased.ids, vec![1, 2]);
            let mixed = from_urlencoded::<Aliased>(b"ids=1&ids[]=2").unwrap_err();
            assert_eq!(mixed.status(), http::StatusCode::BAD_REQUEST);
        }

        #[test]
        fn bool_accepts_on() {
            #[derive(serde::Deserialize)]
            struct F {
                #[serde(default)]
                active: bool,
            }
            let form: F = from_urlencoded(b"active=on").unwrap();
            assert!(form.active);
        }

        #[test]
        fn form_file_accessors_and_debug_hide_the_bytes() {
            let file = FormFile::new(
                "avatar",
                "me.png",
                Some("image/png".to_owned()),
                b"secret".to_vec(),
            );
            assert_eq!(file.field(), "avatar");
            assert_eq!(file.file_name(), "me.png");
            assert_eq!(file.content_type(), Some("image/png"));
            assert_eq!(file.sniffed_content_type(), None);
            assert_eq!(file.len(), 6);
            assert!(!file.is_empty());
            let debug = format!("{file:?}");
            assert!(debug.contains("len: 6"), "{debug}");
            assert!(!debug.contains("secret"), "{debug}");
            assert_eq!(file.into_bytes(), b"secret");
        }

        #[test]
        fn with_files_attaches_files_to_a_form() {
            #[derive(serde::Serialize)]
            struct F {
                name: String,
            }
            let mut form = ChangesetForm::without_csrf(F {
                name: "x".to_owned(),
            })
            .with_files(vec![FormFile::new("a", "a.txt", None, Vec::new())]);
            assert!(form.file("a").is_some_and(FormFile::is_empty));
            assert_eq!(form.take_files().len(), 1);
            assert!(form.file("a").is_none());
        }

        #[test]
        fn field_values_lists_every_element_of_a_sequence_field() {
            #[derive(serde::Serialize)]
            struct F {
                tags: Vec<String>,
                ids: Vec<i64>,
                name: String,
            }
            let cs = Changeset::new(F {
                tags: vec!["a".to_owned(), "b".to_owned()],
                ids: vec![1, 2],
                name: "x".to_owned(),
            });
            assert_eq!(cs.field_values("tags"), vec!["a", "b"]);
            assert_eq!(cs.field_values("ids"), vec!["1", "2"]);
            assert_eq!(cs.field_values("name"), vec!["x"]);
            assert!(cs.field_values("missing").is_empty());
        }

        #[test]
        fn field_values_of_a_none_field_is_empty() {
            #[derive(serde::Serialize)]
            struct G {
                tags: Option<Vec<String>>,
            }
            assert!(
                Changeset::new(G { tags: None })
                    .field_values("tags")
                    .is_empty()
            );
        }

        proptest::proptest! {
            /// Encode a `Vec<String>` as repeated keys, then decode it.
            #[test]
            fn vec_of_strings_round_trips(
                values in proptest::collection::vec(proptest::prelude::any::<String>(), 0..8)
            ) {
                #[derive(serde::Deserialize)]
                struct F {
                    #[serde(default)]
                    v: Vec<String>,
                }
                let body = url::form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(values.iter().map(|value| ("v", value.as_str())))
                    .finish();
                let form: F = from_urlencoded(body.as_bytes()).unwrap();
                proptest::prop_assert_eq!(form.v, values);
            }
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
    fn into_changeset_with_resolves_a_translated_message_for_an_unmessaged_code() {
        #[derive(validator::Validate)]
        struct F {
            #[validate(email)]
            email: String,
        }
        let cs = F {
            email: "not-an-email".into(),
        }
        .into_changeset_with(|field, code| {
            if field == "email" && code == "email" {
                Some("not a valid address".to_string())
            } else {
                None
            }
        });
        assert!(!cs.is_valid());
        assert_eq!(cs.errors_for("email"), ["not a valid address".to_string()]);
    }

    #[test]
    fn into_changeset_with_falls_back_to_the_default_message_when_resolve_returns_none() {
        #[derive(validator::Validate)]
        struct F {
            #[validate(email)]
            email: String,
        }
        let without_resolver = F {
            email: "not-an-email".into(),
        }
        .into_changeset();
        let with_noop_resolver = F {
            email: "not-an-email".into(),
        }
        .into_changeset_with(|_, _| None);
        assert_eq!(
            with_noop_resolver.errors_for("email"),
            without_resolver.errors_for("email")
        );
    }

    #[test]
    fn into_changeset_with_never_overrides_an_explicit_message() {
        #[derive(validator::Validate)]
        struct F {
            #[validate(length(min = 5, message = "too short"))]
            name: String,
        }
        let cs = F { name: "ab".into() }.into_changeset_with(|_, _| {
            Some("the resolver's message, which must never win".to_string())
        });
        assert_eq!(cs.errors_for("name"), ["too short".to_string()]);
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

        #[derive(validator::Validate)]
        struct NestedContact {
            #[validate(email)]
            email: String,
        }

        #[derive(validator::Validate)]
        struct PersonWithContact {
            #[validate(nested)]
            contact: NestedContact,
        }

        #[test]
        fn into_changeset_with_resolver_sees_the_dotted_nested_key() {
            use std::cell::RefCell;

            let seen_keys: RefCell<Vec<String>> = RefCell::new(Vec::new());
            let cs = PersonWithContact {
                contact: NestedContact {
                    email: "not-an-email".into(),
                },
            }
            .into_changeset_with(|field, code| {
                seen_keys.borrow_mut().push(field.to_string());
                (field == "contact.email" && code == "email")
                    .then(|| "not a valid address".to_string())
            });
            assert!(!cs.is_valid());
            assert_eq!(
                cs.errors_for("contact.email"),
                ["not a valid address".to_string()]
            );
            // The resolver received the fully dotted key, not the bare field name.
            assert_eq!(seen_keys.into_inner(), vec!["contact.email".to_string()]);
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
            files: None,
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
            files: None,
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
            files: None,
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
            files: None,
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

    /// Issue #3406: `auto_grow` adds the class to a textarea only.
    #[cfg(feature = "maud")]
    #[test]
    fn auto_grow_marks_a_textarea_and_no_other_control() {
        #[derive(serde::Serialize)]
        struct F {
            bio: String,
            name: String,
        }
        let cs = Changeset::new(F {
            bio: "Hi".into(),
            name: "Ada".into(),
        });
        let grow = FieldOptions::new().auto_grow();
        assert!(grow.is_auto_grow());
        assert!(!FieldOptions::new().is_auto_grow());

        let html = textarea_input_with(&cs, "bio", "Bio", &grow).into_string();
        assert!(
            html.contains(r#"class="autumn-field__input autumn-auto-grow" aria-invalid="false""#),
            "{html}"
        );
        let plain = textarea_input(&cs, "bio", "Bio").into_string();
        assert!(!plain.contains("autumn-auto-grow"), "{plain}");
        let text = text_input_with(&cs, "name", "Name", &grow).into_string();
        assert!(!text.contains("autumn-auto-grow"), "{text}");
    }

    /// Issue #3406: an invalid auto-grow textarea keeps both classes.
    #[cfg(feature = "maud")]
    #[test]
    fn auto_grow_keeps_the_invalid_class() {
        #[derive(serde::Serialize)]
        struct F {
            bio: String,
        }
        let mut cs = Changeset::new(F { bio: String::new() });
        cs.add_error("bio", "Too short");
        let html =
            textarea_input_with(&cs, "bio", "Bio", &FieldOptions::new().auto_grow()).into_string();
        assert!(
            html.contains(
                r#"class="autumn-field__input autumn-field__input--invalid autumn-auto-grow" aria-invalid="true""#
            ),
            "{html}"
        );
    }

    /// Issue #3406: the rich-text editor is a textarea too.
    #[cfg(feature = "maud")]
    #[test]
    fn auto_grow_reaches_the_rich_text_editor() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: "# Hi".into(),
        });
        let html = rich_text_area_with(
            &cs,
            "body",
            "Body",
            &RichTextLabels::new(),
            &FieldOptions::new().auto_grow(),
        )
        .into_string();
        assert!(
            html.contains(
                r#"class="autumn-field__input autumn-rich-text__editor autumn-auto-grow""#
            ),
            "{html}"
        );
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
        // First occurrence wins. This helper is lenient on purpose.
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
    fn rich_text_area_with_labels_overrides_the_hint() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: String::new(),
        });
        let labels = RichTextLabels::new().hint("Se admite Markdown.");
        let html = rich_text_area_with_labels(&cs, "body", "Body", &labels).into_string();
        assert!(html.contains("Se admite Markdown."), "{html}");
        assert!(!html.contains("Markdown supported"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn rich_text_area_with_labels_overrides_the_toolbar_group_label() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: String::new(),
        });
        let labels = RichTextLabels::new().toolbar_group("Formato Markdown");
        let html = rich_text_area_with_labels(&cs, "body", "Body", &labels).into_string();
        assert!(html.contains(r#"aria-label="Formato Markdown""#), "{html}");
        assert!(!html.contains("Markdown formatting"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn rich_text_area_with_labels_overrides_the_controls() {
        const CUSTOM_CONTROLS: &[(&str, &str)] = &[("Negrita", "**negrita**")];

        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: String::new(),
        });
        let labels = RichTextLabels::new().controls(CUSTOM_CONTROLS);
        let html = rich_text_area_with_labels(&cs, "body", "Body", &labels).into_string();
        assert!(html.contains("Negrita"), "{html}");
        assert!(html.contains("**negrita**"), "{html}");
        // The default English control names are gone entirely, not just augmented.
        assert!(!html.contains(">Bold<"), "{html}");
        assert!(!html.contains("**bold**"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn rich_text_area_htmx_with_labels_overrides_the_preview_heading() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: String::new(),
        });
        let labels = RichTextLabels::new().preview_heading("Vista previa");
        let html =
            rich_text_area_htmx_with_labels(&cs, "body", "Body", "/p", &labels).into_string();
        assert!(html.contains("Vista previa"), "{html}");
        assert!(!html.contains(">Preview<"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn rich_text_area_htmx_with_token_field_with_labels_overrides_the_preview_heading() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: String::new(),
        });
        let labels = RichTextLabels::new().preview_heading("Vista previa");
        let html = rich_text_area_htmx_with_token_field_with_labels(
            &cs,
            "body",
            "Body",
            "/p",
            "_one_time",
            &labels,
        )
        .into_string();
        assert!(html.contains("Vista previa"), "{html}");
        assert!(html.contains(r#"hx-params="not _one_time""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn required_rich_text_area_with_labels_keeps_the_required_signal_and_labels() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: String::new(),
        });
        let labels = RichTextLabels::new().hint("Se admite Markdown.");
        let html = required_rich_text_area_with_labels(&cs, "body", "Body", &labels).into_string();
        assert!(html.contains(r#"aria-required="true""#), "{html}");
        assert!(html.contains("Se admite Markdown."), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn required_rich_text_area_htmx_with_labels_keeps_the_required_signal_and_labels() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: String::new(),
        });
        let labels = RichTextLabels::new().preview_heading("Vista previa");
        let html = required_rich_text_area_htmx_with_labels(&cs, "body", "Body", "/p", &labels)
            .into_string();
        assert!(html.contains(r#"aria-required="true""#), "{html}");
        assert!(html.contains("Vista previa"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn required_rich_text_area_htmx_with_token_field_with_labels_keeps_the_required_signal_and_labels()
     {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: String::new(),
        });
        let labels = RichTextLabels::new().preview_heading("Vista previa");
        let html = required_rich_text_area_htmx_with_token_field_with_labels(
            &cs,
            "body",
            "Body",
            "/p",
            "_one_time",
            &labels,
        )
        .into_string();
        assert!(html.contains(r#"aria-required="true""#), "{html}");
        assert!(html.contains("Vista previa"), "{html}");
        assert!(html.contains(r#"hx-params="not _one_time""#), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn rich_text_area_with_labels_default_labels_match_rich_text_area() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F {
            body: "Hello **world**".into(),
        });
        assert_eq!(
            rich_text_area_with_labels(&cs, "body", "Body", &RichTextLabels::new()).into_string(),
            rich_text_area(&cs, "body", "Body").into_string()
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn rich_text_area_htmx_with_labels_default_labels_match_rich_text_area_htmx() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let cs = Changeset::new(F { body: "hi".into() });
        assert_eq!(
            rich_text_area_htmx_with_labels(&cs, "body", "Body", "/p", &RichTextLabels::new())
                .into_string(),
            rich_text_area_htmx(&cs, "body", "Body", "/p").into_string()
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn changeset_form_exposes_the_rich_text_labels_helpers() {
        #[derive(serde::Serialize)]
        struct F {
            body: String,
        }
        let form = ChangesetForm {
            changeset: Changeset::new(F { body: "hi".into() }),
            csrf_token: None,
            csrf_field: "_csrf".to_owned(),
            files: None,
        };
        let labels = RichTextLabels::new().hint("Se admite Markdown.");
        assert_eq!(
            form.rich_text_area_with_labels("body", "Body", &labels)
                .into_string(),
            rich_text_area_with_labels(&form.changeset, "body", "Body", &labels).into_string()
        );
        assert_eq!(
            form.rich_text_area_htmx_with_labels("body", "Body", "/p", &labels)
                .into_string(),
            rich_text_area_htmx_with_labels(&form.changeset, "body", "Body", "/p", &labels)
                .into_string()
        );
        assert_eq!(
            form.rich_text_area_htmx_with_token_field_with_labels(
                "body",
                "Body",
                "/p",
                "_one_time",
                &labels
            )
            .into_string(),
            rich_text_area_htmx_with_token_field_with_labels(
                &form.changeset,
                "body",
                "Body",
                "/p",
                "_one_time",
                &labels
            )
            .into_string()
        );
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
            files: None,
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
        // A `bool` field takes one value, so the form decoders reject the
        // repeat rather than taking the last value. Unchecked
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
        // renders — no hidden sibling), through both decoders: axum's `Form`
        // (`serde_urlencoded`) and ChangesetForm's (`from_urlencoded`).
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
        let decoded: Decoded = from_urlencoded(b"active=true").unwrap();
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
        async fn mixed_case_urlencoded_content_type_is_accepted() {
            // RFC 9110 8.3.1: media types are case-insensitive (issue #3267).
            async fn handler(_form: ChangesetForm<TestForm>) -> &'static str {
                "ok"
            }
            for ct in [
                "Application/X-WWW-Form-Urlencoded",
                "APPLICATION/X-WWW-FORM-URLENCODED; charset=UTF-8",
            ] {
                let req = axum::http::Request::builder()
                    .method("POST")
                    .uri("/test")
                    .header("Content-Type", ct)
                    .body(Body::from("name=Alice"))
                    .unwrap();
                let resp = Router::new()
                    .route("/test", post(handler))
                    .oneshot(req)
                    .await
                    .unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK, "{ct}");
            }
        }

        #[cfg(feature = "multipart")]
        #[tokio::test]
        async fn mixed_case_multipart_content_type_is_accepted() {
            async fn handler(_form: ChangesetForm<TestForm>) -> &'static str {
                "ok"
            }
            let mut req = multipart_req("/test", "name", "Alice");
            let ct = req.headers()["content-type"]
                .to_str()
                .unwrap()
                .replace("multipart/form-data", "Multipart/Form-Data");
            req.headers_mut()
                .insert("content-type", ct.parse().unwrap());
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(req)
                .await
                .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
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

        // ── #2423: an embedded NUL byte is a field error, not a 500 ───

        #[derive(serde::Deserialize, serde::Serialize, validator::Validate)]
        struct NulTestForm {
            #[validate(length(min = 3))]
            name: String,
            #[serde(default)]
            body: String,
        }

        /// The shape from issue #2423: a free-text field carrying `%00`
        /// decodes cleanly and satisfies every `#[validate(...)]` rule, so it
        /// used to reach Postgres raw and come back as an unhandled 500. It is
        /// now one ordinary field error on the changeset, which the handler
        /// re-renders inline like any other rejected submission.
        #[tokio::test]
        async fn nul_byte_in_text_field_is_a_field_error() {
            async fn handler(form: ChangesetForm<NulTestForm>) -> String {
                format!(
                    "valid={} body_errors={}",
                    form.is_valid(),
                    form.errors_for("body").join("|")
                )
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "name=Alice&body=before%00after"))
                .await
                .unwrap();
            assert_body(
                resp,
                &format!("valid=false body_errors={NUL_CHARACTER_FIELD_ERROR}"),
            )
            .await;
        }

        /// The retained value is the author's text minus the NUL: the
        /// re-rendered form keeps their work (the `ChangesetForm` round-trip
        /// contract) without echoing a raw `0x00` back into the HTML.
        #[tokio::test]
        async fn nul_byte_is_stripped_from_the_redisplayed_value() {
            async fn handler(form: ChangesetForm<NulTestForm>) -> String {
                form.into_changeset()
                    .field_value("body")
                    .unwrap_or_default()
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "name=Alice&body=before%00after"))
                .await
                .unwrap();
            assert_body(resp, "beforeafter").await;
        }

        /// Only the field that actually carried the byte is flagged.
        #[tokio::test]
        async fn nul_byte_flags_only_the_offending_field() {
            async fn handler(form: ChangesetForm<NulTestForm>) -> String {
                format!(
                    "name={} body={}",
                    form.errors_for("name").len(),
                    form.errors_for("body").len()
                )
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "name=Alice&body=a%00b"))
                .await
                .unwrap();
            assert_body(resp, "name=0 body=1").await;
        }

        /// No false positives: an ordinary submission is untouched.
        #[tokio::test]
        async fn form_without_nul_is_unaffected() {
            async fn handler(form: ChangesetForm<NulTestForm>) -> String {
                format!("valid={} body={}", form.is_valid(), form.data().body)
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "name=Alice&body=plain"))
                .await
                .unwrap();
            assert_body(resp, "valid=true body=plain").await;
        }

        /// Framework plumbing fields carry no renderable error key, so a NUL
        /// in one is cleaned but not reported — same reasoning as `_csrf`.
        #[tokio::test]
        async fn nul_byte_in_the_method_override_is_not_a_form_error() {
            async fn handler(form: ChangesetForm<NulTestForm>) -> String {
                format!("valid={} errors={}", form.is_valid(), form.errors().len())
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req(
                    "/test",
                    "_method=PA%00TCH&name=Alice&body=ok",
                ))
                .await
                .unwrap();
            assert_body(resp, "valid=true errors=0").await;
        }

        /// A scaffolded flat form posts the submit token as a hidden field, so
        /// its name has to be exempt here too — the nested extractor already
        /// exempted it, and the flat one must not diverge.
        #[tokio::test]
        async fn nul_byte_in_the_submit_token_is_not_a_form_error() {
            async fn handler(form: ChangesetForm<NulTestForm>) -> String {
                format!("valid={} errors={}", form.is_valid(), form.errors().len())
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req(
                    "/test",
                    "_submit_token=to%00ken&name=Alice&body=ok",
                ))
                .await
                .unwrap();
            assert_body(resp, "valid=true errors=0").await;
        }

        /// The exemption follows the *configured* field name, not just the
        /// default, so renaming `security.csrf.form_field` does not reintroduce
        /// an unrenderable error.
        #[tokio::test]
        async fn nul_byte_in_a_renamed_csrf_field_is_not_a_form_error() {
            let mut req = urlencoded_req("/test", "authenticity=to%00ken&name=Alice&body=ok");
            req.extensions_mut()
                .insert(crate::security::csrf::CsrfFormField(
                    "authenticity".to_owned(),
                ));

            let form = ChangesetForm::<NulTestForm>::from_request(req, &())
                .await
                .expect("extraction should succeed");
            assert!(form.is_valid(), "errors: {:?}", form.errors());
        }

        /// A submitted name that is not a field of `T` is normally ignored, but
        /// a NUL in one is still reported — under that name. Documented in
        /// `docs/guide/forms.md`; pinned here so the choice is deliberate
        /// rather than incidental.
        #[tokio::test]
        async fn nul_byte_in_an_unknown_key_is_reported_under_that_key() {
            async fn handler(form: ChangesetForm<NulTestForm>) -> String {
                format!(
                    "valid={} unknown={}",
                    form.is_valid(),
                    form.errors_for("return_to").len()
                )
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req(
                    "/test",
                    "return_to=a%00b&name=Alice&body=ok",
                ))
                .await
                .unwrap();
            assert_body(resp, "valid=false unknown=1").await;
        }

        /// A key submitted more than once contributes one message, not one per
        /// occurrence. Repeating a key that *is* a single-value field of `T` is a
        /// hard repeated-field decode failure (a 422 before
        /// any of this runs), so the case that actually reaches the sweep is a
        /// repeated key `T` does not declare — which is also the shape that
        /// makes the dedupe load-bearing, since an unbounded body can repeat a
        /// key arbitrarily many times.
        #[tokio::test]
        async fn a_repeated_key_reports_once() {
            async fn handler(form: ChangesetForm<NulTestForm>) -> String {
                format!("errors={}", form.errors_for("return_to").len())
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req(
                    "/test",
                    "name=Alice&body=ok&return_to=a%00a&return_to=b%00b&return_to=c%00c",
                ))
                .await
                .unwrap();
            assert_body(resp, "errors=1").await;
        }

        /// Keys are deliberately never cleaned. Stripping a NUL out of a key
        /// would rename it — potentially onto a real field of `T` — inventing a
        /// submission the client never made.
        #[tokio::test]
        async fn a_nul_in_a_key_never_renames_it_onto_a_real_field() {
            async fn handler(form: ChangesetForm<NulTestForm>) -> String {
                format!("valid={} body={:?}", form.is_valid(), form.data().body)
            }
            // `bod y` must NOT become `body`.
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "name=Alice&bod%00y=injected"))
                .await
                .unwrap();
            assert_body(resp, r#"valid=true body="""#).await;
        }

        /// A NUL in a *rule-violating* field adds to that field's messages
        /// rather than replacing the validator's own.
        #[tokio::test]
        async fn nul_byte_error_is_appended_to_existing_field_errors() {
            async fn handler(form: ChangesetForm<NulTestForm>) -> String {
                let errs = form.errors_for("name");
                format!(
                    "count={} has_nul={}",
                    errs.len(),
                    errs.iter().any(|e| e == NUL_CHARACTER_FIELD_ERROR)
                )
            }
            // "a\0b" is 2 characters after stripping, so `length(min = 3)`
            // fails as well.
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "name=a%00b&body="))
                .await
                .unwrap();
            assert_body(resp, "count=2 has_nul=true").await;
        }

        /// The CSRF token is transport plumbing, not a form field: a NUL there
        /// must not surface as a validation error on a name no template
        /// renders (which would make the form permanently, invisibly invalid).
        /// CSRF verification itself still rejects the mangled token upstream.
        #[tokio::test]
        async fn nul_byte_in_the_csrf_field_is_not_a_form_error() {
            async fn handler(form: ChangesetForm<NulTestForm>) -> String {
                format!("valid={} errors={}", form.is_valid(), form.errors().len())
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "_csrf=to%00ken&name=Alice&body=ok"))
                .await
                .unwrap();
            assert_body(resp, "valid=true errors=0").await;
        }

        /// The multipart blank-optional retry re-deserializes from a filtered
        /// pair set; the NUL report must survive that second attempt rather
        /// than being dropped with the blank field.
        #[cfg(feature = "multipart")]
        #[tokio::test]
        async fn multipart_nul_report_survives_the_blank_optional_retry() {
            async fn handler(form: ChangesetForm<OptionalNumericForm>) -> String {
                format!(
                    "age={:?} name_errors={}",
                    form.data().age,
                    form.errors_for("name").len()
                )
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(multipart_req_multi(
                    "/test",
                    // The blank `age` forces the retry path; the NUL is on
                    // `name`, which the retry keeps.
                    &[("name", "Ali\u{0}ce"), ("age", "")],
                ))
                .await
                .unwrap();
            assert_body(resp, "age=None name_errors=1").await;
        }

        /// A file part is binary by definition, so it is never swept and never
        /// blocks the submission.
        #[cfg(feature = "multipart")]
        #[tokio::test]
        async fn multipart_file_part_carrying_a_nul_is_untouched() {
            async fn handler(form: ChangesetForm<NulTestForm>) -> String {
                format!("valid={} errors={}", form.is_valid(), form.errors().len())
            }
            let boundary = "----FormBoundary7MA4YWxkTrZu0gW";
            let body = format!(
                "--{boundary}\r\n\
                 Content-Disposition: form-data; name=\"name\"\r\n\r\n\
                 Alice\r\n\
                 --{boundary}\r\n\
                 Content-Disposition: form-data; name=\"avatar\"; filename=\"a.bin\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n\
                 bin\u{0}ary\r\n\
                 --{boundary}--\r\n"
            );
            let req = axum::http::Request::builder()
                .method("POST")
                .uri("/test")
                .header(
                    "Content-Type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap();

            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(req)
                .await
                .unwrap();
            assert_body(resp, "valid=true errors=0").await;
        }

        #[cfg(feature = "multipart")]
        #[tokio::test]
        async fn multipart_nul_byte_in_text_field_is_a_field_error() {
            async fn handler(form: ChangesetForm<NulTestForm>) -> String {
                format!(
                    "valid={} body_errors={} body={}",
                    form.is_valid(),
                    form.errors_for("body").len(),
                    form.data().body
                )
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(multipart_req_multi(
                    "/test",
                    &[("name", "Alice"), ("body", "before\u{0}after")],
                ))
                .await
                .unwrap();
            assert_body(resp, "valid=false body_errors=1 body=beforeafter").await;
        }

        // ── Repeated keys and file parts (issue #3394) ─────────────

        #[derive(serde::Deserialize, validator::Validate)]
        struct TagsForm {
            #[validate(length(min = 1))]
            name: String,
            #[serde(default)]
            tags: Vec<String>,
            #[serde(default)]
            ids: Vec<i64>,
        }

        async fn tags_handler(form: ChangesetForm<TagsForm>) -> String {
            format!(
                "name={} tags={:?} ids={:?}",
                form.data().name,
                form.data().tags,
                form.data().ids
            )
        }

        #[tokio::test]
        async fn urlencoded_repeated_keys_reach_vec_fields() {
            let resp = Router::new()
                .route("/test", post(tags_handler))
                .oneshot(urlencoded_req(
                    "/test",
                    "name=Ann&tags=a&tags=b%20c&ids=1&ids=2",
                ))
                .await
                .unwrap();
            assert_body(resp, r#"name=Ann tags=["a", "b c"] ids=[1, 2]"#).await;
        }

        #[tokio::test]
        async fn urlencoded_repeated_scalar_is_422_naming_the_field() {
            let resp = Router::new()
                .route("/test", post(tags_handler))
                .oneshot(urlencoded_req("/test", "name=a&name=b"))
                .await
                .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::UNPROCESSABLE_ENTITY);
            assert!(body_text(resp).await.contains("`name`"));
        }

        #[cfg(feature = "multipart")]
        #[tokio::test]
        async fn multipart_repeated_keys_reach_vec_fields() {
            let resp = Router::new()
                .route("/test", post(tags_handler))
                .oneshot(multipart_req_multi(
                    "/test",
                    &[("name", "Ann"), ("tags", "a"), ("tags", "b"), ("ids", "9")],
                ))
                .await
                .unwrap();
            assert_body(resp, r#"name=Ann tags=["a", "b"] ids=[9]"#).await;
        }

        #[cfg(feature = "multipart")]
        #[tokio::test]
        async fn multipart_file_field_reaches_the_form() {
            async fn handler(form: ChangesetForm<TestForm>) -> String {
                let file = form.file("avatar").expect("file part must reach the form");
                format!(
                    "valid={} field={} name={} type={:?} bytes={:?}",
                    form.is_valid(),
                    file.field(),
                    file.file_name(),
                    file.content_type(),
                    std::str::from_utf8(file.bytes()).unwrap()
                )
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(multipart_parts_req(
                    "/test",
                    &[
                        Part::text("name", "Alice"),
                        Part::file("avatar", "me.txt", "text/plain", b"hello"),
                    ],
                ))
                .await
                .unwrap();
            assert_body(
                resp,
                r#"valid=true field=avatar name=me.txt type=Some("text/plain") bytes="hello""#,
            )
            .await;
        }

        #[cfg(feature = "multipart")]
        #[tokio::test]
        async fn multipart_files_under_one_name_all_reach_the_form() {
            async fn handler(mut form: ChangesetForm<TestForm>) -> String {
                let names: Vec<String> = form
                    .files("photos")
                    .map(|f| f.file_name().to_owned())
                    .collect();
                let taken = form.take_files();
                format!(
                    "{names:?} taken={} left={}",
                    taken.len(),
                    form.files("photos").count()
                )
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(multipart_parts_req(
                    "/test",
                    &[
                        Part::text("name", "Alice"),
                        Part::file("photos", "a.txt", "text/plain", b"a"),
                        Part::file("photos", "b.txt", "text/plain", b"b"),
                    ],
                ))
                .await
                .unwrap();
            assert_body(resp, r#"["a.txt", "b.txt"] taken=2 left=0"#).await;
        }

        #[cfg(feature = "multipart")]
        #[tokio::test]
        async fn multipart_empty_optional_file_input_is_skipped() {
            async fn handler(form: ChangesetForm<TestForm>) -> String {
                format!(
                    "valid={} file={}",
                    form.is_valid(),
                    form.file("avatar").is_some()
                )
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(multipart_parts_req(
                    "/test",
                    &[
                        Part::text("name", "Alice"),
                        Part::file("avatar", "", "application/octet-stream", b""),
                    ],
                ))
                .await
                .unwrap();
            assert_body(resp, "valid=true file=false").await;
        }

        #[cfg(feature = "multipart")]
        #[tokio::test]
        async fn multipart_file_obeys_the_upload_mime_allow_list() {
            async fn handler(_form: ChangesetForm<TestForm>) -> &'static str {
                "accepted"
            }
            let mut req = multipart_parts_req(
                "/test",
                &[
                    Part::text("name", "Alice"),
                    Part::file("avatar", "x.html", "image/png", b"<script>1</script>"),
                ],
            );
            req.extensions_mut()
                .insert(crate::security::config::UploadConfig {
                    allowed_mime_types: vec!["image/png".to_owned()],
                    ..Default::default()
                });
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(req)
                .await
                .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
        }

        #[cfg(feature = "multipart")]
        #[tokio::test]
        async fn multipart_file_obeys_the_upload_size_cap() {
            async fn handler(_form: ChangesetForm<TestForm>) -> &'static str {
                "accepted"
            }
            let mut req = multipart_parts_req(
                "/test",
                &[
                    Part::text("name", "Alice"),
                    Part::file("avatar", "big.txt", "text/plain", &[b'x'; 64]),
                ],
            );
            req.extensions_mut()
                .insert(crate::security::config::UploadConfig {
                    max_file_size_bytes: 16,
                    ..Default::default()
                });
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(req)
                .await
                .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        }

        #[cfg(feature = "multipart")]
        #[tokio::test]
        async fn multipart_empty_optional_file_input_is_skipped_under_a_mime_policy() {
            #[derive(serde::Deserialize, validator::Validate)]
            #[serde(deny_unknown_fields)]
            struct Strict {
                name: String,
            }
            async fn handler(form: ChangesetForm<Strict>) -> String {
                format!(
                    "{} file={}",
                    form.data().name,
                    form.file("avatar").is_some()
                )
            }
            let mut req = multipart_parts_req(
                "/test",
                &[
                    Part::text("name", "Alice"),
                    Part::file("avatar", "", "application/octet-stream", b""),
                ],
            );
            req.extensions_mut()
                .insert(crate::security::config::UploadConfig {
                    allowed_mime_types: vec!["image/png".to_owned()],
                    ..Default::default()
                });
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(req)
                .await
                .unwrap();
            assert_body(resp, "Alice file=false").await;
        }

        #[cfg(feature = "multipart")]
        #[tokio::test]
        async fn multipart_keeps_a_tighter_route_body_limit() {
            async fn handler(_form: ChangesetForm<TestForm>) -> &'static str {
                "accepted"
            }
            let req = multipart_parts_req(
                "/test",
                &[
                    Part::text("name", "Alice"),
                    Part::file("avatar", "big.txt", "text/plain", &[b'x'; 4096]),
                ],
            );
            let resp = Router::new()
                .route("/test", post(handler))
                .layer(axum::extract::DefaultBodyLimit::max(1024))
                .oneshot(req)
                .await
                .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        }

        #[tokio::test]
        async fn urlencoded_form_has_no_files() {
            async fn handler(form: ChangesetForm<TestForm>) -> String {
                format!("{}", form.files("avatar").count())
            }
            let resp = Router::new()
                .route("/test", post(handler))
                .oneshot(urlencoded_req("/test", "name=Alice&avatar=x"))
                .await
                .unwrap();
            assert_body(resp, "0").await;
        }

        // ── Helpers ────────────────────────────────────────────────

        /// One part of a hand-built multipart body.
        #[cfg(feature = "multipart")]
        struct Part<'a> {
            name: &'a str,
            file: Option<(&'a str, &'a str)>,
            value: &'a [u8],
        }

        #[cfg(feature = "multipart")]
        impl<'a> Part<'a> {
            const fn text(name: &'a str, value: &'a str) -> Self {
                Self {
                    name,
                    file: None,
                    value: value.as_bytes(),
                }
            }

            const fn file(
                name: &'a str,
                file_name: &'a str,
                content_type: &'a str,
                value: &'a [u8],
            ) -> Self {
                Self {
                    name,
                    file: Some((file_name, content_type)),
                    value,
                }
            }
        }

        #[cfg(feature = "multipart")]
        fn multipart_parts_req(uri: &str, parts: &[Part<'_>]) -> axum::http::Request<Body> {
            let boundary = "----FormBoundary7MA4YWxkTrZu0gW";
            let mut body: Vec<u8> = Vec::new();
            for part in parts {
                body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
                match part.file {
                    Some((file_name, content_type)) => body.extend_from_slice(
                        format!(
                            "Content-Disposition: form-data; name=\"{}\"; filename=\"{file_name}\"\r\n\
                             Content-Type: {content_type}\r\n\r\n",
                            part.name
                        )
                        .as_bytes(),
                    ),
                    None => body.extend_from_slice(
                        format!(
                            "Content-Disposition: form-data; name=\"{}\"\r\n\r\n",
                            part.name
                        )
                        .as_bytes(),
                    ),
                }
                body.extend_from_slice(part.value);
                body.extend_from_slice(b"\r\n");
            }
            body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
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

// ── Custom control tests (issue #3400) ─────────────────────────────

#[cfg(all(test, feature = "maud"))]
mod custom_control_tests {
    use super::*;

    /// A one-element control, as a date picker would write it.
    struct DateLike;

    impl FieldRenderer for DateLike {
        fn name(&self) -> &'static str {
            "datelike"
        }

        fn render(&self, field: &FieldContext<'_>) -> maud::Markup {
            field.control(
                "input",
                &[("type", "text"), ("class", "autumn-datelike-input")],
                maud::html! {},
            )
        }
    }

    /// A renderer that tries to set owned and unsafe attributes.
    struct Hostile;

    impl FieldRenderer for Hostile {
        fn name(&self) -> &'static str {
            "hostile"
        }

        fn render(&self, field: &FieldContext<'_>) -> maud::Markup {
            field.control(
                "div",
                &[
                    ("id", "evil"),
                    ("name", "evil"),
                    ("required", ""),
                    ("aria-invalid", "false"),
                    ("aria-describedby", "evil"),
                    ("onclick", "alert(1)"),
                    ("ONCLICK", "alert(1)"),
                    ("hx-on:click", "alert(1)"),
                    ("hx-on--click", "alert(1)"),
                    ("data-hx-on:click", "alert(1)"),
                    ("role", "first"),
                    ("style", "color:red"),
                    ("bad name", "x"),
                    ("x\"y", "x"),
                    ("role", "listbox"),
                    ("aria-haspopup", "listbox"),
                    ("data-autumn-roving", ""),
                    ("value", "<b>\"quoted\"</b>"),
                ],
                maud::html! { span { "body" } },
            )
        }
    }

    /// A radio group: the wrapper must be a fieldset.
    struct Radios;

    impl FieldRenderer for Radios {
        fn name(&self) -> &'static str {
            "radios"
        }

        fn grouped(&self) -> bool {
            true
        }

        fn render(&self, field: &FieldContext<'_>) -> maud::Markup {
            maud::html! {
                @for choice in ["a", "b"] {
                    label {
                        @let checked: &[(&str, &str)] = if field.value() == Some(choice) {
                            &[("checked", "")]
                        } else {
                            &[]
                        };
                        @let attrs: Vec<(&str, &str)> = [("type", "radio"), ("value", choice)]
                            .into_iter()
                            .chain(checked.iter().copied())
                            .collect();
                        (field.group_item("input", choice, &attrs, maud::html! {}))
                        (choice)
                    }
                }
            }
        }
    }

    /// A file-like control.
    struct Upload;

    impl FieldRenderer for Upload {
        fn name(&self) -> &'static str {
            "upload"
        }

        fn multipart(&self) -> bool {
            true
        }

        fn render(&self, field: &FieldContext<'_>) -> maud::Markup {
            field.control("input", &[("type", "file")], maud::html! {})
        }
    }

    #[derive(Serialize)]
    struct Task {
        due: String,
        title: String,
    }

    impl FormModel for Task {
        fn form_fields() -> Vec<FormField> {
            vec![
                FormField::new("title", "Title", FieldControl::Text, false),
                FormField::new("due", "Due", FieldControl::Text, true),
            ]
        }
    }

    fn task() -> Task {
        Task {
            due: "2026-10-29".into(),
            title: "Ship".into(),
        }
    }

    fn invalid_task() -> Changeset<Task> {
        let mut changeset = Changeset::new(task());
        changeset.add_error("due", "is in the past");
        changeset
    }

    #[test]
    fn custom_input_writes_wrapper_label_hint_and_errors() {
        let html = custom_input_with(
            &invalid_task(),
            "due",
            "Due",
            &DateLike,
            &FieldOptions::new().hint("Pick a day."),
        )
        .into_string();
        assert!(
            html.starts_with(r#"<div id="due-field" class="autumn-field">"#),
            "{html}"
        );
        assert!(
            html.contains(
                r#"<label id="due-label" for="due" class="autumn-field__label">Due</label>"#
            ),
            "{html}"
        );
        assert!(
            html.contains(r#"<p id="due-hint" class="autumn-field__hint">Pick a day.</p>"#),
            "{html}"
        );
        assert!(
            html.contains(
                r#"<input type="text" id="due" name="due" value="2026-10-29" class="autumn-field__input autumn-field__input--invalid autumn-datelike-input" aria-invalid="true" aria-describedby="due-hint due-error">"#
            ),
            "{html}"
        );
        assert!(
            html.contains(r#"<div id="due-error" role="alert" class="autumn-field__errors"><p class="autumn-field__error">is in the past</p></div>"#),
            "{html}"
        );
        assert!(html.ends_with("</div>"), "{html}");
    }

    #[test]
    fn custom_input_renders_the_passthrough_on_the_control() {
        let html = custom_input_with(
            &Changeset::new(task()),
            "due",
            "Due",
            &DateLike,
            &FieldOptions::new()
                .required()
                .placeholder("YYYY-MM-DD")
                .autocomplete("off")
                .disabled()
                .data("test-id", "due")
                .hx("get", "/check"),
        )
        .into_string();
        for want in [
            r#" required aria-required="true""#,
            r#" placeholder="YYYY-MM-DD""#,
            r#" autocomplete="off""#,
            " disabled",
            r#" data-test-id="due""#,
            r#" hx-get="/check""#,
            r#"aria-invalid="false""#,
            r#"<span class="autumn-field__required" aria-hidden="true">*</span>"#,
        ] {
            assert!(html.contains(want), "missing `{want}`: {html}");
        }
    }

    #[test]
    fn control_drops_owned_and_unsafe_attributes() {
        let html = custom_input_with(
            &invalid_task(),
            "due",
            "Due",
            &Hostile,
            &FieldOptions::new(),
        )
        .into_string();
        for banned in [
            "evil",
            "onclick",
            "ONCLICK",
            "alert(1)",
            "hx-on",
            r#"role="listbox""#,
            "style=",
            "bad name",
            "x\"y",
            r#"aria-invalid="false""#,
        ] {
            assert!(!html.contains(banned), "`{banned}` leaked: {html}");
        }
        assert!(!html.contains(" required"), "required leaked: {html}");
        assert!(!html.contains(" name="), "a div has no name: {html}");
        for kept in [
            r#"<div id="due" value="&lt;b&gt;&quot;quoted&quot;&lt;/b&gt;" class="autumn-field__input autumn-field__input--invalid" aria-invalid="true" aria-describedby="due-error" aria-labelledby="due-label" role="first""#,
            r#" aria-haspopup="listbox""#,
            r#" data-autumn-roving="""#,
            r#" value="&lt;b&gt;&quot;quoted&quot;&lt;/b&gt;""#,
            r#"aria-describedby="due-error""#,
            "><span>body</span></div>",
        ] {
            assert!(html.contains(kept), "missing `{kept}`: {html}");
        }
    }

    #[test]
    #[should_panic(expected = "tag")]
    fn control_panics_on_a_bad_tag() {
        struct BadTag;
        impl FieldRenderer for BadTag {
            fn name(&self) -> &'static str {
                "bad"
            }
            fn render(&self, field: &FieldContext<'_>) -> maud::Markup {
                field.control("div onclick=x", &[], maud::html! {})
            }
        }
        let _ = custom_input_with(
            &Changeset::new(task()),
            "due",
            "Due",
            &BadTag,
            &FieldOptions::new(),
        );
    }

    #[test]
    fn grouped_control_uses_fieldset_and_legend() {
        let mut changeset = Changeset::new(Task {
            due: "b".into(),
            title: String::new(),
        });
        changeset.add_error("due", "pick one");
        let html = custom_input_with(
            &changeset,
            "due",
            "Due",
            &Radios,
            &FieldOptions::new().hint("One only.").required(),
        )
        .into_string();
        assert!(
            html.starts_with(
                r#"<fieldset id="due-field" class="autumn-field autumn-field--group" aria-invalid="true" aria-describedby="due-hint due-error"><legend class="autumn-field__label">Due<span class="autumn-field__required" aria-hidden="true">*</span></legend>"#
            ),
            "{html}"
        );
        assert!(!html.contains("<label id="), "{html}");
        assert!(
            html.contains(
                r#"<input type="radio" id="due-b" name="due" required aria-invalid="true" value="b" checked="">"#
            ),
            "{html}"
        );
        assert!(
            html.contains(r#"<input type="radio" id="due-a" name="due" required aria-invalid="true" value="a">"#),
            "{html}"
        );
        assert!(html.ends_with("</div></fieldset>"), "{html}");
    }

    #[test]
    fn form_for_renders_a_custom_override_with_model_required() {
        let changeset = Changeset::new(task());
        let html = form_for(&changeset, "/tasks", "post")
            .override_field("due", FieldControl::custom(DateLike))
            .render()
            .into_string();
        assert!(
            html.contains(
                r#"<input type="text" id="due" name="due" value="2026-10-29" required aria-required="true" class="autumn-field__input autumn-datelike-input""#
            ),
            "{html}"
        );
        assert!(!html.contains("multipart"), "{html}");
    }

    #[test]
    fn form_for_is_multipart_for_a_multipart_custom_control() {
        let changeset = Changeset::new(task());
        let html = form_for(&changeset, "/tasks", "post")
            .override_field("due", FieldControl::custom(Upload))
            .render()
            .into_string();
        assert!(html.contains(r#"enctype="multipart/form-data""#), "{html}");
    }

    #[test]
    fn form_for_prefills_a_custom_control_through_a_serde_rename() {
        #[derive(Serialize)]
        struct Renamed {
            #[serde(rename = "dueOn")]
            due: String,
        }
        impl FormModel for Renamed {
            fn form_fields() -> Vec<FormField> {
                vec![
                    FormField::new("due", "Due", FieldControl::custom(DateLike), false)
                        .with_value_name("dueOn"),
                ]
            }
        }
        let changeset = Changeset::new(Renamed {
            due: "2026-11-01".into(),
        });
        let html = form_for(&changeset, "/t", "post").render().into_string();
        assert!(html.contains(r#"name="due" value="2026-11-01""#), "{html}");
    }

    #[test]
    fn custom_control_equality_is_by_shared_renderer() {
        let a = FieldControl::custom(DateLike);
        assert_eq!(a, a.clone());
        assert_ne!(a, FieldControl::custom(DateLike));
        assert_ne!(a, FieldControl::Text);
        assert_eq!(format!("{a:?}"), r#"Custom(CustomControl("datelike"))"#);
    }

    #[test]
    fn field_context_reports_ids_and_state() {
        let changeset = invalid_task();
        let options = FieldOptions::new().hint("h").required();
        let value = changeset.field_value("due");
        let context = FieldContext {
            field: "due",
            label: "Due",
            value: value.as_deref(),
            values: &[],
            errors: changeset.errors_for("due"),
            options: &options,
            grouped: false,
        };
        assert_eq!(context.id(), "due");
        assert_eq!(context.name(), "due");
        assert_eq!(context.label(), "Due");
        assert_eq!(context.value(), Some("2026-10-29"));
        assert!(context.is_invalid());
        assert!(context.is_required());
        assert_eq!(context.errors(), ["is in the past"]);
        assert_eq!(
            context.described_by().as_deref(),
            Some("due-hint due-error")
        );
        assert!(context.options().is_required());
    }

    /// A control that must not echo a value: the browser does not fill it.
    struct Secret;

    impl FieldRenderer for Secret {
        fn name(&self) -> &'static str {
            "secret"
        }

        fn render(&self, field: &FieldContext<'_>) -> maud::Markup {
            maud::html! {
                (field.control("input", &[("type", "PASSWORD")], maud::html! {}))
                (field.control("button", &[("aria-label", "Show")], maud::html! { "Show" }))
            }
        }
    }

    #[test]
    fn control_does_not_prefill_a_password_and_gives_a_button_a_type() {
        let html = custom_input_with(&invalid_task(), "due", "Due", &Secret, &FieldOptions::new())
            .into_string();
        assert!(!html.contains("2026-10-29"), "password echoed: {html}");
        assert!(
            html.contains(r#"<button type="button" id="due" name="due""#),
            "{html}"
        );
        assert!(!html.contains("aria-labelledby"), "{html}");
    }

    /// Sets attributes that the options also set, two times.
    struct Duplicates;

    impl FieldRenderer for Duplicates {
        fn name(&self) -> &'static str {
            "duplicates"
        }

        fn render(&self, field: &FieldContext<'_>) -> maud::Markup {
            field.control(
                "div",
                &[
                    ("role", "listbox"),
                    ("data-x", "renderer"),
                    ("data-x", "second"),
                    ("placeholder", "renderer"),
                ],
                maud::html! {},
            )
        }
    }

    #[test]
    fn control_writes_each_name_once_and_aria_required_with_a_role() {
        let html = custom_input_with(
            &Changeset::new(task()),
            "due",
            "Due",
            &Duplicates,
            &FieldOptions::new()
                .required()
                .placeholder("app")
                .data("x", "app"),
        )
        .into_string();
        assert_eq!(html.matches("data-x=").count(), 1, "{html}");
        assert_eq!(html.matches("placeholder=").count(), 1, "{html}");
        assert!(html.contains(r#"data-x="app""#), "{html}");
        assert!(html.contains(r#"placeholder="app""#), "{html}");
        assert!(html.contains(r#" aria-required="true""#), "{html}");
        assert!(!html.contains(" required "), "{html}");
    }

    #[test]
    fn described_by_is_escaped_once() {
        #[derive(Serialize)]
        struct Odd {
            #[serde(rename = "a&b")]
            value: String,
        }
        let mut changeset = Changeset::new(Odd {
            value: String::new(),
        });
        changeset.add_error("a&b", "bad");
        let html = custom_input_with(&changeset, "a&b", "A", &DateLike, &FieldOptions::new())
            .into_string();
        assert!(
            html.contains(r#"aria-describedby="a&amp;b-error""#),
            "{html}"
        );
        assert!(html.contains(r#"<div id="a&amp;b-error""#), "{html}");
    }

    #[test]
    fn field_context_exposes_all_values_of_a_repeated_field() {
        struct Tags;
        impl FieldRenderer for Tags {
            fn name(&self) -> &'static str {
                "tags"
            }
            fn render(&self, field: &FieldContext<'_>) -> maud::Markup {
                maud::html! { span { (field.values().join(",")) } }
            }
        }
        #[derive(Serialize)]
        struct Post {
            tags: Vec<String>,
        }
        let changeset = Changeset::new(Post {
            tags: vec!["rust".into(), "web".into()],
        });
        let html = custom_input_with(&changeset, "tags", "Tags", &Tags, &FieldOptions::new())
            .into_string();
        assert!(html.contains("<span>rust,web</span>"), "{html}");
    }

    #[test]
    fn widget_element_writes_attrs_then_the_passthrough() {
        let html = widget_element(
            "div",
            &[
                ("class", "autumn-x-bar"),
                ("role", "toolbar"),
                ("data-x", "widget"),
                ("onclick", "alert(1)"),
                ("hx-on:click", "alert(1)"),
                ("style", "color:red"),
            ],
            &FieldOptions::new()
                .data("x", "app")
                .data("y", "app")
                .hx("get", "/y"),
            maud::html! { "body" },
        )
        .into_string();
        assert_eq!(
            html,
            r#"<div class="autumn-x-bar" role="toolbar" data-x="widget" data-y="app" hx-get="/y">body</div>"#
        );
    }

    #[test]
    fn code_loading_tags_are_refused() {
        for tag in ["script", "style", "iframe", "object", "base"] {
            let result = std::panic::catch_unwind(|| {
                widget_element(tag, &[], &FieldOptions::new(), maud::html! {})
            });
            assert!(result.is_err(), "`{tag}` must be refused");
        }
    }

    /// The app passthrough works as on a core helper: the app owns its CSP
    /// and may allow `hx-on:*`. Only the attributes of the widget pack are
    /// filtered, because the widget contract promises CSP-safe pack markup.
    #[test]
    fn app_passthrough_matches_core_helpers_and_pack_attrs_are_filtered() {
        let options = FieldOptions::new().hx("on:click", "app()").hx("get", "/ok");
        let core = text_input_with(&Changeset::new(task()), "due", "Due", &options).into_string();
        let control = custom_input_with(&Changeset::new(task()), "due", "Due", &DateLike, &options)
            .into_string();
        let element = widget_element(
            "div",
            &[("hx-on:click", "pack()"), ("onclick", "pack()")],
            &options,
            maud::html! {},
        )
        .into_string();
        for html in [&core, &control, &element] {
            assert!(html.contains(r#"hx-on:click="app()""#), "{html}");
            assert!(html.contains(r#"hx-get="/ok""#), "{html}");
            assert!(!html.contains("pack()"), "pack handler leaked: {html}");
        }
    }

    #[test]
    fn field_options_getters_report_the_state() {
        let options = FieldOptions::new()
            .hint("h")
            .required()
            .disabled()
            .readonly();
        assert!(options.is_required() && options.is_disabled() && options.is_readonly());
        assert_eq!(options.hint_text(), Some("h"));
        assert_eq!(FieldOptions::new().hint_text(), None);
    }
}
