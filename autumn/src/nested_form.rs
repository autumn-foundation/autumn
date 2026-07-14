//! Nested (`has_many`) form binding — a parent struct plus one child
//! collection, decoded and validated in a single extractor.
//!
//! # Overview
//!
//! [`NestedChangesetForm<P, C>`] is the nested-form counterpart of
//! [`ChangesetForm<T>`](crate::form::ChangesetForm): it decodes a form body
//! carrying a parent struct `P` **and** a repeated child collection `C`
//! (rendered with input names like `items[0][sku]`, `items[1][sku]`, …),
//! runs [`validator::Validate`] on the parent and on every non-destroyed
//! child row, and captures per-field errors so the whole form can be
//! re-rendered inline after a failed submission.
//!
//! The child collection is identified by the [`NestedChild::COLLECTION`]
//! constant, which names the field group in input names
//! (`items[i][field]`) and in combined error keys (`items[1].quantity`).
//!
//! # Wire format
//!
//! ```text
//! name=Order+1                 // parent field
//! items[0][sku]=A-1            // child row 0, subfield `sku`
//! items[0][quantity]=2         // child row 0, subfield `quantity`
//! items[1][sku]=B-2            // child row 1
//! items[1][quantity]=3
//! items[1][_destroy]=1         // optional: mark row 1 for removal
//! ```
//!
//! Row indices need not be contiguous — client-side removal can leave gaps
//! (`items[0]`, `items[2]`) which are compacted in ascending order to
//! preserve row order. The compacted 0-based position is what appears in
//! combined error keys and is what a renderer iterates.
//!
//! # Binding and validating
//!
//! ```rust
//! use autumn_web::nested_form::{decode_nested_urlencoded, NestedChild};
//!
//! #[derive(serde::Deserialize, validator::Validate)]
//! struct NewOrder {
//!     #[validate(length(min = 1))]
//!     name: String,
//! }
//!
//! #[derive(serde::Deserialize, validator::Validate)]
//! struct NewLineItem {
//!     #[validate(length(min = 1))]
//!     sku: String,
//!     #[validate(range(min = 1))]
//!     quantity: i32,
//! }
//!
//! impl NestedChild for NewLineItem {
//!     const COLLECTION: &'static str = "items";
//! }
//!
//! let pairs = vec![
//!     ("name".to_string(), "Order 1".to_string()),
//!     ("items[0][sku]".to_string(), "A-1".to_string()),
//!     ("items[0][quantity]".to_string(), "2".to_string()),
//!     // second row is invalid: quantity is below the range minimum
//!     ("items[1][sku]".to_string(), "B-2".to_string()),
//!     ("items[1][quantity]".to_string(), "0".to_string()),
//! ];
//!
//! let changeset = decode_nested_urlencoded::<NewOrder, NewLineItem>(&pairs)
//!     .expect("parent decodes");
//!
//! // The invalid child surfaces under its per-row combined key, and the whole
//! // changeset refuses to yield a valid `(parent, children)` pair.
//! assert!(!changeset.errors_for("items[1].quantity").is_empty());
//! assert!(changeset.errors_for("items[0].quantity").is_empty());
//! assert!(!changeset.is_valid());
//! assert!(changeset.into_valid().is_err());
//! ```
//!
//! # Per-row error keys
//!
//! Errors are addressable with `#[validate(nested)]`-style combined keys of
//! the shape `"{COLLECTION}[{i}].{sub}"`. A child whose `quantity` fails
//! validation on the (compacted) second row surfaces under
//! [`errors_for("items[1].quantity")`](NestedChangeset::errors_for); a bare
//! `"items[1]"` returns that row's row-level (parse) error. Parent field
//! errors keep their plain field-name keys and are delegated to
//! [`Changeset::errors_for`]. This is exactly the key shape a Maud renderer
//! reads back per row via [`RowScope::errors_for`], so a failed submission
//! re-renders each field's message inline next to the offending input.
//!
//! # `_destroy` marker
//!
//! A child subfield named `_destroy` with a truthy value (`"1"`, `"true"`,
//! `"on"`) marks its row for removal. Destroyed rows are **retained** for
//! re-rendering (so the checkbox state survives a round-trip) but never
//! contribute to `valid_children`, so [`into_valid`](NestedChangeset::into_valid)
//! drops them from the returned child vector. [`RowScope::destroy_checkbox`]
//! renders the durable no-JS control that drives this.
//!
//! # Rendering: `inputs_for` + htmx / no-JS
//!
//! With the `maud` feature, [`inputs_for`] renders the repeating child block:
//! it re-emits every submitted row (values + inline errors) and then appends
//! at least one blank template row so a user can add a child **without any
//! JavaScript** — the pre-rendered blank row's `items[n][…]` inputs post like
//! any other. Supplying [`InputsForOptions::add_row_url`] additionally emits an
//! htmx "Add row" button (`hx-get` + `hx-swap="beforeend"`, with
//! `hx-params="not _submit_token"` so the one-time submit token is not spent
//! fetching the fragment); [`nested_row_fragment`] renders the server response
//! for that endpoint. htmx is a progressive enhancement layered over the no-JS
//! path — it is never required.
//!
//! # Handler + atomic save
//!
//! ```rust,ignore
//! #[post("/orders")]
//! async fn create(
//!     mut db: Db,
//!     form: NestedChangesetForm<NewOrder, NewLineItem>,
//! ) -> impl IntoResponse {
//!     match form.into_valid() {
//!         Ok((order, items)) => save_order_with_items(&mut db, order, items).await,
//!         Err(form) => (StatusCode::UNPROCESSABLE_ENTITY, render(&form)).into_response(),
//!     }
//! }
//! ```
//!
//! A parent and its children must be persisted **atomically** — a half-saved
//! order with only some of its line items is never acceptable. Do it inside a
//! **single** [`Db::tx`](crate::db::Db::tx): insert the parent, read back its
//! generated `id`, stamp each child's foreign key with that `id`, and insert
//! the children — all on the one `conn` the closure is handed. Returning `Err`
//! from anywhere in the closure (a failing child, a DB constraint violation)
//! rolls the **whole** transaction back, so neither the parent nor any child
//! row is left behind.
//!
//! ```rust,ignore
//! use scoped_futures::ScopedFutureExt;
//!
//! db.tx(|conn| async move {
//!     let order = diesel::insert_into(orders::table)
//!         .values(&new_order)
//!         .returning(Order::as_returning())
//!         .get_result(conn)
//!         .await?;
//!     for mut item in new_items {
//!         item.order_id = order.id; // stamp the FK from the freshly-read parent id
//!         diesel::insert_into(line_items::table)
//!             .values(&item)
//!             .execute(conn)
//!             .await?; // any Err here rolls back the parent insert too
//!     }
//!     Ok::<_, diesel::result::Error>(order.id)
//! }.scope_boxed())
//! .await
//! ```
//!
//! Use **raw diesel inserts on `conn`**, not a generated
//! [`Repository`](crate::repository) `create`: that `create` opens its *own*
//! `Db::tx`, and `Db::tx` cannot be re-entered on the same connection — the
//! nested call trips the nested-transaction guard and returns a `400`. Keep the
//! whole parent-plus-children unit of work in the one outer `tx`.
//!
//! See `tests/integration/nested_form_atomic_save.rs` for the rollback
//! correctness gate (a failing child leaves **zero** rows in either table) and
//! `tests/integration/nested_form_order_example.rs` for the full create flow.

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
    )
)]

use std::collections::{BTreeMap, HashMap};

use axum::extract::{FromRequest, Request};
use axum::response::IntoResponse;

use crate::form::{
    Changeset, IntoChangeset, decode_urlencoded_dropping_blank_optional_fields,
    validation_errors_to_map,
};

// ── NestedChild ────────────────────────────────────────────────────

/// A child row type bound as part of a nested (`has_many`) form.
///
/// The [`COLLECTION`](NestedChild::COLLECTION) constant names the field
/// group used in input names (`items[i][field]`) and in combined error
/// keys (`items[1].quantity`).
pub trait NestedChild: serde::de::DeserializeOwned + validator::Validate + Send {
    /// Field-group name used in input names (`items[i][field]`) and error
    /// keys (`items[1].quantity`). e.g. `"items"`.
    const COLLECTION: &'static str;
}

// ── NestedRow ──────────────────────────────────────────────────────

/// One submitted child row, retained for re-rendering regardless of whether
/// it parsed or validated.
#[derive(Debug, Clone)]
pub struct NestedRow {
    /// Raw submitted subfield values (`sku` → `"A-1"`), used to pre-fill
    /// inputs when re-rendering after a failed submission.
    values: HashMap<String, String>,
    /// Per-subfield validation (or parse) errors. A row that failed to parse
    /// records its error under the empty-string key.
    errors: HashMap<String, Vec<String>>,
    /// `true` when the row carried a truthy `_destroy` marker.
    destroyed: bool,
}

impl NestedRow {
    /// The raw submitted value for subfield `sub`, if present.
    #[must_use]
    pub fn value(&self, sub: &str) -> Option<&str> {
        self.values.get(sub).map(String::as_str)
    }

    /// Validation messages for subfield `sub`, or an empty slice.
    ///
    /// A row-level parse failure is stored under the empty-string key, so
    /// `row.errors_for("")` returns any whole-row decode error.
    #[must_use]
    pub fn errors_for(&self, sub: &str) -> &[String] {
        self.errors.get(sub).map_or(&[], Vec::as_slice)
    }

    /// `true` when the row was marked for removal via a truthy `_destroy`.
    #[must_use]
    pub const fn is_destroyed(&self) -> bool {
        self.destroyed
    }

    /// Every error keyed by subfield name (empty key = row-level parse error).
    #[must_use]
    pub const fn all_errors(&self) -> &HashMap<String, Vec<String>> {
        &self.errors
    }
}

// ── NestedChangeset ────────────────────────────────────────────────

/// A parent [`Changeset<P>`] plus its bound child collection.
///
/// Obtain one from [`decode_nested_urlencoded`] or (preferred) the
/// [`NestedChangesetForm`] axum extractor.
#[derive(Debug)]
pub struct NestedChangeset<P, C> {
    /// The parent changeset (values + per-field errors), reusing the existing
    /// [`Changeset`] pipeline.
    pub parent: Changeset<P>,
    /// The submitted child rows in compacted order, retained for re-render.
    rows: Vec<NestedRow>,
    /// `Some(children)` iff the parent is valid **and** every non-destroyed
    /// row both parsed and validated; `None` otherwise.
    valid_children: Option<Vec<C>>,
}

impl<P, C: NestedChild> NestedChangeset<P, C> {
    /// `true` when the parent is valid, every non-destroyed row has no
    /// errors, and the children all parsed and validated.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.parent.is_valid()
            && self.valid_children.is_some()
            && self.rows.iter().all(|r| r.destroyed || r.errors.is_empty())
    }

    /// Consume the changeset, returning `Ok((parent, children))` when valid or
    /// `Err(self)` (with all rows retained for re-render) when not.
    ///
    /// The returned child vector contains only non-destroyed rows, in order.
    ///
    /// # Errors
    ///
    /// Returns `Err(self)` when the parent or any non-destroyed child row has
    /// validation errors, or any child failed to parse.
    #[allow(
        clippy::result_large_err,
        reason = "the Err variant intentionally returns the whole changeset (rows + raw \
                  values) so the handler can re-render the form inline with errors"
    )]
    pub fn into_valid(self) -> Result<(P, Vec<C>), Self> {
        if !self.is_valid() {
            return Err(self);
        }
        let Self {
            parent,
            rows,
            valid_children,
        } = self;
        // `is_valid()` guarantees both arms below take the happy path; the
        // reconstruction branches keep the code panic-free without relying on
        // that invariant.
        match valid_children {
            Some(children) => match parent.into_valid() {
                Ok(p) => Ok((p, children)),
                Err(parent) => Err(Self {
                    parent,
                    rows,
                    valid_children: None,
                }),
            },
            None => Err(Self {
                parent,
                rows,
                valid_children: None,
            }),
        }
    }

    /// Validation messages for `key`, or an empty slice.
    ///
    /// Supports both parent field keys (delegated to
    /// [`Changeset::errors_for`]) and combined child keys of the form
    /// `"{COLLECTION}[{i}].{sub}"` (e.g. `"items[1].quantity"`). A bare
    /// `"{COLLECTION}[{i}]"` returns that row's row-level (parse) errors.
    #[must_use]
    pub fn errors_for(&self, key: &str) -> &[String] {
        if let Some((idx, sub)) = parse_combined_child_key(key, C::COLLECTION) {
            return self.rows.get(idx).map_or(&[], |r| r.errors_for(sub));
        }
        self.parent.errors_for(key)
    }

    /// The submitted child rows in compacted (ascending-index) order.
    #[must_use]
    pub fn rows(&self) -> &[NestedRow] {
        &self.rows
    }

    /// The child collection name, i.e. [`NestedChild::COLLECTION`].
    #[must_use]
    pub const fn collection_name(&self) -> &'static str {
        C::COLLECTION
    }
}

// ── Decoder ────────────────────────────────────────────────────────

/// Decode URL-encoded `pairs` into a [`NestedChangeset<P, C>`].
///
/// Pairs whose key matches `^{COLLECTION}\[(\d+)\]\[([^\]]+)\]$` are child
/// subfields (captured index + subfield name); everything else is a parent
/// pair. Child pairs are grouped by numeric index into ascending order and
/// **compacted** so non-contiguous indices (gaps from client-side removal)
/// still yield sequential rows preserving submission order. Subfield order
/// within a row is preserved.
///
/// A `_destroy` subfield with a truthy value (`"1"`, `"true"`, `"on"`) marks
/// its row destroyed; destroyed rows are retained (for re-render) but never
/// contribute to `valid_children`.
///
/// Both the parent and each non-destroyed child are decoded through the same
/// blank-optional-dropping `serde_urlencoded` path
/// [`ChangesetForm`](crate::form::ChangesetForm) uses, so string→typed
/// coercion (`quantity=5` → `i32`) comes for free.
///
/// # Errors
///
/// Returns `Err(message)` when the **parent** fails to decode (a malformed,
/// non-blank typed value) — mirroring the hard-400 contract of
/// [`ChangesetForm`](crate::form::ChangesetForm). A **child** row that fails
/// to parse is not a hard error: it is retained with a row-level error and
/// marks the changeset invalid.
pub fn decode_nested_urlencoded<P, C>(
    pairs: &[(String, String)],
) -> Result<NestedChangeset<P, C>, String>
where
    P: serde::de::DeserializeOwned + validator::Validate,
    C: NestedChild,
{
    let collection = C::COLLECTION;

    // Split parent pairs from grouped child subfields. `BTreeMap` keeps the
    // rows in ascending index order; enumerating it later compacts gaps.
    let mut parent_pairs: Vec<(String, String)> = Vec::new();
    let mut child_groups: BTreeMap<usize, Vec<(String, String)>> = BTreeMap::new();
    for (key, value) in pairs {
        if let Some((idx, sub)) = parse_child_key(key, collection) {
            child_groups
                .entry(idx)
                .or_default()
                .push((sub.to_string(), value.clone()));
        } else {
            parent_pairs.push((key.clone(), value.clone()));
        }
    }

    // Decode the parent through the shared blank-optional-dropping path.
    let parent_encoded = encode_pairs(&parent_pairs);
    let parent_data: P =
        decode_urlencoded_dropping_blank_optional_fields::<P>(parent_encoded.as_bytes())
            .map_err(|e| e.to_string())?;
    let parent = parent_data.into_changeset();

    let mut rows: Vec<NestedRow> = Vec::new();
    let mut children: Vec<C> = Vec::new();
    let mut all_children_ok = true;

    for subfields in child_groups.into_values() {
        let mut values: HashMap<String, String> = HashMap::new();
        let mut destroyed = false;
        // Subfields decoded into `C`, excluding the `_destroy` marker (not a
        // field of `C`).
        let mut decode_pairs: Vec<(String, String)> = Vec::new();
        for (sub, val) in &subfields {
            values.insert(sub.clone(), val.clone());
            if sub == "_destroy" {
                if is_truthy(val) {
                    destroyed = true;
                }
            } else {
                decode_pairs.push((sub.clone(), val.clone()));
            }
        }

        let mut errors: HashMap<String, Vec<String>> = HashMap::new();

        if destroyed {
            rows.push(NestedRow {
                values,
                errors,
                destroyed,
            });
            continue;
        }

        let encoded = encode_pairs(&decode_pairs);
        match decode_urlencoded_dropping_blank_optional_fields::<C>(encoded.as_bytes()) {
            Ok(child) => match validator::Validate::validate(&child) {
                Ok(()) => children.push(child),
                Err(ve) => {
                    errors = validation_errors_to_map(&ve);
                    all_children_ok = false;
                }
            },
            Err(e) => {
                // Row-level parse failure: keep raw values, record under the
                // empty-string key so `errors_for("items[i]")` surfaces it.
                errors.entry(String::new()).or_default().push(e.to_string());
                all_children_ok = false;
            }
        }

        rows.push(NestedRow {
            values,
            errors,
            destroyed,
        });
    }

    let valid_children = if parent.is_valid() && all_children_ok {
        Some(children)
    } else {
        None
    };

    Ok(NestedChangeset {
        parent,
        rows,
        valid_children,
    })
}

/// Parse `key` as a child subfield reference `COLLECTION[<idx>][<sub>]`,
/// returning `(idx, sub)`. Hand-parses the bracket pattern (no `regex`
/// dependency), matching `^{COLLECTION}\[(\d+)\]\[([^\]]+)\]$` with
/// `COLLECTION` treated literally.
fn parse_child_key<'a>(key: &'a str, collection: &str) -> Option<(usize, &'a str)> {
    let rest = key.strip_prefix(collection)?;
    let rest = rest.strip_prefix('[')?;
    let close = rest.find(']')?;
    let (idx_str, after) = rest.split_at(close);
    let idx: usize = idx_str.parse().ok()?;
    // `after` begins with the `]` that `close` pointed at.
    let after = after.strip_prefix(']')?;
    let after = after.strip_prefix('[')?;
    let close2 = after.find(']')?;
    let (sub, tail) = after.split_at(close2);
    // The subfield must be the final segment: exactly a trailing `]`.
    if tail != "]" || sub.is_empty() {
        return None;
    }
    Some((idx, sub))
}

/// Parse a combined error key `COLLECTION[<idx>]` optionally followed by
/// `.<sub>`, returning `(idx, sub)` where `sub` is `""` for the bare form.
/// Returns `None` when the key is not a child key (so it falls through to the
/// parent).
fn parse_combined_child_key<'a>(key: &'a str, collection: &str) -> Option<(usize, &'a str)> {
    let rest = key.strip_prefix(collection)?;
    let rest = rest.strip_prefix('[')?;
    let close = rest.find(']')?;
    let (idx_str, after) = rest.split_at(close);
    let idx: usize = idx_str.parse().ok()?;
    let after = after.strip_prefix(']')?;
    if after.is_empty() {
        return Some((idx, ""));
    }
    let sub = after.strip_prefix('.')?;
    if sub.is_empty() {
        return None;
    }
    Some((idx, sub))
}

/// Re-encode `pairs` as an `application/x-www-form-urlencoded` string so the
/// shared `serde_urlencoded` decode path can re-parse them.
fn encode_pairs(pairs: &[(String, String)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .finish()
}

/// Whether a `_destroy` marker value counts as "destroy this row".
fn is_truthy(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed == "1" || trimmed.eq_ignore_ascii_case("true") || trimmed.eq_ignore_ascii_case("on")
}

// ── NestedChangesetForm extractor ──────────────────────────────────

/// Axum extractor that decodes a nested (`has_many`) form body, validates the
/// parent and every non-destroyed child row, and captures the CSRF and
/// submit-token context for re-rendering.
///
/// Mirrors [`ChangesetForm`](crate::form::ChangesetForm): errors live in the
/// [`NestedChangeset`] rather than rejecting with 422; the handler decides how
/// to respond. Only `application/x-www-form-urlencoded` bodies are accepted
/// (multipart is a follow-up).
pub struct NestedChangesetForm<P, C> {
    /// The validated (or invalid) nested changeset.
    pub changeset: NestedChangeset<P, C>,
    csrf_token: Option<String>,
    csrf_field: String,
    submit_token: Option<String>,
    submit_field: String,
}

impl<P, C: NestedChild> NestedChangesetForm<P, C> {
    /// The CSRF token captured from the request, if the CSRF middleware is active.
    #[must_use]
    pub fn csrf_token(&self) -> Option<&str> {
        self.csrf_token.as_deref()
    }

    /// The CSRF form-field name (honours `security.csrf.form_field`).
    #[must_use]
    pub fn csrf_field(&self) -> &str {
        &self.csrf_field
    }

    /// The one-time submit token captured from the request, if the
    /// submit-token middleware is active.
    #[must_use]
    pub fn submit_token(&self) -> Option<&str> {
        self.submit_token.as_deref()
    }

    /// The submit-token form-field name (honours
    /// `security.submit_token.field_name`).
    #[must_use]
    pub fn submit_field(&self) -> &str {
        &self.submit_field
    }

    /// Consume and return only the inner [`NestedChangeset`].
    pub fn into_changeset(self) -> NestedChangeset<P, C> {
        self.changeset
    }

    /// Return `Ok((parent, children))` when valid, `Err(self)` when not.
    ///
    /// The `Err` branch retains the CSRF/submit context so the handler can
    /// immediately re-render the form with inline errors.
    ///
    /// # Errors
    ///
    /// Returns `Err(self)` when the inner changeset has validation errors.
    #[allow(
        clippy::result_large_err,
        reason = "the Err variant intentionally returns the whole form (changeset + CSRF/submit \
                  context) so the handler can re-render inline with errors"
    )]
    pub fn into_valid(self) -> Result<(P, Vec<C>), Self> {
        let Self {
            changeset,
            csrf_token,
            csrf_field,
            submit_token,
            submit_field,
        } = self;
        match changeset.into_valid() {
            Ok(pair) => Ok(pair),
            Err(changeset) => Err(Self {
                changeset,
                csrf_token,
                csrf_field,
                submit_token,
                submit_field,
            }),
        }
    }
}

/// Dereferences to [`NestedChangeset<P, C>`] so all changeset methods are
/// available directly on the form (`form.is_valid()`, `form.errors_for(…)`,
/// `form.rows()`, …).
impl<P, C> std::ops::Deref for NestedChangesetForm<P, C> {
    type Target = NestedChangeset<P, C>;
    fn deref(&self) -> &Self::Target {
        &self.changeset
    }
}

impl<S, P, C> FromRequest<S> for NestedChangesetForm<P, C>
where
    S: Send + Sync,
    P: serde::de::DeserializeOwned + validator::Validate + Send,
    C: NestedChild,
{
    type Rejection = axum::response::Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        // Capture CSRF + submit-token context from request extensions exactly
        // as `ChangesetForm` does, before the body is consumed.
        let csrf_token = req
            .extensions()
            .get::<crate::security::CsrfToken>()
            .map(|t| t.token().to_string());
        let csrf_field = req
            .extensions()
            .get::<crate::security::csrf::CsrfFormField>()
            .map_or_else(|| "_csrf".to_owned(), |f| f.0.clone());
        let submit_token = req
            .extensions()
            .get::<crate::security::SubmitToken>()
            .map(|t| t.token().to_string());
        let submit_field = req
            .extensions()
            .get::<crate::security::SubmitFormField>()
            .map_or_else(|| "_submit_token".to_owned(), |f| f.0.clone());

        // Same content-type gate axum's own form extractors apply. Multipart
        // is out of scope for now (follow-up).
        let content_type = req
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if !content_type.starts_with("application/x-www-form-urlencoded") {
            return Err((
                axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Nested form requests must have `Content-Type: application/x-www-form-urlencoded`",
            )
                .into_response());
        }

        // Buffer through axum's `Bytes` extractor so `DefaultBodyLimit` is
        // enforced (a bare `to_bytes(.., usize::MAX)` would defeat it).
        let (parts, body) = req.into_parts();
        let bytes_req = Request::from_parts(parts, body);
        let bytes = axum::body::Bytes::from_request(bytes_req, state)
            .await
            .map_err(IntoResponse::into_response)?;

        let pairs: Vec<(String, String)> = url::form_urlencoded::parse(&bytes)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        let changeset = decode_nested_urlencoded::<P, C>(&pairs)
            .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e).into_response())?;

        Ok(Self {
            changeset,
            csrf_token,
            csrf_field,
            submit_token,
            submit_field,
        })
    }
}

// ── Maud view helpers ──────────────────────────────────────────────

/// A single row's rendering scope inside [`inputs_for`].
///
/// Each scope binds the child collection name, the row's 0-based `index`, and
/// (for existing/submitted rows) the underlying [`NestedRow`] so its raw values
/// pre-fill inputs and its per-subfield errors render inline on re-render after
/// a failed submission. A blank template row carries `row: None` — its inputs
/// render empty with no error blocks.
///
/// The row-scoped input builders mirror the standalone field helpers in
/// [`crate::form`] (`text_input`, `number_input`, …) but emit **nested** input
/// names (`items[{index}][{sub}]`) and per-row-unique element ids
/// (`items-{index}-{sub}`) so ids and `aria-describedby` links stay unique
/// across repeated rows.
#[cfg(feature = "maud")]
pub struct RowScope<'a> {
    collection: &'a str,
    index: usize,
    row: Option<&'a NestedRow>,
}

#[cfg(feature = "maud")]
impl RowScope<'_> {
    /// This row's 0-based position in the collection.
    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    /// The nested input `name` for subfield `sub`, i.e.
    /// `"{collection}[{index}][{sub}]"`.
    #[must_use]
    pub fn field_name(&self, sub: &str) -> String {
        format!("{}[{}][{}]", self.collection, self.index, sub)
    }

    /// The raw submitted value for subfield `sub`, or `None` for a blank row.
    #[must_use]
    pub fn value(&self, sub: &str) -> Option<&str> {
        self.row.and_then(|r| r.value(sub))
    }

    /// Validation messages for subfield `sub`, or an empty slice (always empty
    /// for a blank row).
    #[must_use]
    pub fn errors_for(&self, sub: &str) -> &[String] {
        self.row.map_or(&[], |r| r.errors_for(sub))
    }

    /// `true` when this (existing) row carried a truthy `_destroy` marker.
    #[must_use]
    pub fn is_destroyed(&self) -> bool {
        self.row.is_some_and(NestedRow::is_destroyed)
    }

    /// Per-row-unique element id base for subfield `sub`
    /// (`"{collection}-{index}-{sub}"`), used for `id` / `aria-describedby`
    /// linkage so repeated rows never collide.
    fn element_id(&self, sub: &str) -> String {
        format!("{}-{}-{}", self.collection, self.index, sub)
    }

    /// Render a labeled row-scoped `<input type="text">` for subfield `sub`.
    ///
    /// Mirrors [`crate::form::text_input`]: `autumn-field` wrapper, per-row
    /// pre-fill, `aria-invalid` / `aria-describedby`, and a `role="alert"`
    /// error block — but with the nested `name` and a per-row-unique `id`.
    #[must_use]
    pub fn text_input(&self, sub: &str, label: &str) -> maud::Markup {
        self.text_like_input(sub, label, false)
    }

    /// Like [`RowScope::text_input`] but adds `required` + `aria-required="true"`.
    #[must_use]
    pub fn required_text_input(&self, sub: &str, label: &str) -> maud::Markup {
        self.text_like_input(sub, label, true)
    }

    /// Shared body for the text-like inputs.
    fn text_like_input(&self, sub: &str, label: &str, required: bool) -> maud::Markup {
        let errors = self.errors_for(sub);
        let has_errors = !errors.is_empty();
        let value = self.value(sub).unwrap_or_default();
        let name = self.field_name(sub);
        let id = self.element_id(sub);
        let error_id = format!("{id}-error");
        let wrapper_id = format!("{id}-field");

        maud::html! {
            div id=(wrapper_id) class="autumn-field" {
                label for=(id) class="autumn-field__label" { (label) }
                input
                    type="text"
                    id=(id)
                    name=(name)
                    value=(value)
                    required[required]
                    aria-required=[required.then_some("true")]
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

    /// Render a labeled row-scoped `<input type="number">` for subfield `sub`.
    ///
    /// Mirrors [`crate::form::number_input`] (leaving the browser-default
    /// `step`), with the nested `name` and a per-row-unique `id`.
    #[must_use]
    pub fn number_input(&self, sub: &str, label: &str) -> maud::Markup {
        let errors = self.errors_for(sub);
        let has_errors = !errors.is_empty();
        let value = self.value(sub).unwrap_or_default();
        let name = self.field_name(sub);
        let id = self.element_id(sub);
        let error_id = format!("{id}-error");
        let wrapper_id = format!("{id}-field");

        maud::html! {
            div id=(wrapper_id) class="autumn-field" {
                label for=(id) class="autumn-field__label" { (label) }
                input
                    type="number"
                    id=(id)
                    name=(name)
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

    /// Render a labeled row-scoped `<textarea>` for subfield `sub`.
    ///
    /// Mirrors [`crate::form::textarea_input`]: the value is emitted as the
    /// element body, with the nested `name` and a per-row-unique `id`.
    #[must_use]
    pub fn textarea_input(&self, sub: &str, label: &str) -> maud::Markup {
        let errors = self.errors_for(sub);
        let has_errors = !errors.is_empty();
        let value = self.value(sub).unwrap_or_default();
        let name = self.field_name(sub);
        let id = self.element_id(sub);
        let error_id = format!("{id}-error");
        let wrapper_id = format!("{id}-field");

        maud::html! {
            div id=(wrapper_id) class="autumn-field" {
                label for=(id) class="autumn-field__label" { (label) }
                textarea
                    id=(id)
                    name=(name)
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

    /// Render a row-scoped `<input type="hidden">` for subfield `sub`.
    ///
    /// Use this to carry an existing child's primary key (e.g. `id`) on an edit
    /// form so the decoder can match the submitted row back to a persisted record.
    #[must_use]
    pub fn hidden_input(&self, sub: &str, value: &str) -> maud::Markup {
        let name = self.field_name(sub);
        maud::html! {
            input type="hidden" name=(name) value=(value);
        }
    }

    /// Render the durable no-JS removal control: a `_destroy` checkbox whose
    /// checked state is preserved on re-render (`checked` iff
    /// [`RowScope::is_destroyed`]).
    ///
    /// The decoder honours a truthy `_destroy` marker
    /// ([`decode_nested_urlencoded`]), so ticking this box and submitting the
    /// surrounding form removes the row with no JavaScript. htmx/JS row removal
    /// (swapping the `.nested-fields__row` node's `outerHTML`) is an optional
    /// progressive enhancement layered on top; this checkbox is the required
    /// mechanism.
    #[must_use]
    pub fn destroy_checkbox(&self, label: &str) -> maud::Markup {
        let checked = self.is_destroyed();
        let name = self.field_name("_destroy");
        let id = self.element_id("_destroy");
        maud::html! {
            div class="autumn-field autumn-field--destroy" {
                input
                    type="checkbox"
                    id=(id)
                    name=(name)
                    value="1"
                    checked[checked]
                    class="autumn-field__checkbox";
                label for=(id) class="autumn-field__label" { (label) }
            }
        }
    }
}

/// Options for [`inputs_for`].
#[cfg(feature = "maud")]
pub struct InputsForOptions {
    /// Number of extra blank rows to pre-render after the existing rows. The
    /// no-JS fallback lets users fill and submit these without any JavaScript.
    /// Defaults to `1`; [`inputs_for`] always emits **at least one** blank
    /// template row even when this is `0`.
    pub blank_rows: usize,
    /// Optional htmx URL for the server "Add row" fragment endpoint (see
    /// [`nested_row_fragment`]). When `None`, no Add button is rendered and the
    /// no-JS path still works via the pre-rendered blank rows.
    pub add_row_url: Option<String>,
    /// Container element id. Defaults to `"{collection}-rows"`.
    pub container_id: Option<String>,
}

#[cfg(feature = "maud")]
impl Default for InputsForOptions {
    fn default() -> Self {
        Self {
            blank_rows: 1,
            add_row_url: None,
            container_id: None,
        }
    }
}

/// Render the repeating child field-group block for a nested (`has_many`) form.
///
/// Wraps the rows in `<div id="{container_id}" class="nested-fields">`. For each
/// existing/submitted row (from [`NestedChangeset::rows`], in order — **including**
/// rows re-submitted after a validation failure) it invokes `render_row` with a
/// [`RowScope`] carrying that row, so values and per-field errors pre-fill on
/// re-render. It then appends [`InputsForOptions::blank_rows`] blank rows (always
/// at least one) whose scopes carry `row: None`. Every row is wrapped in
/// `<div class="nested-fields__row" data-index="{i}">` so htmx/JS removal can
/// target the node's `outerHTML`.
///
/// When [`InputsForOptions::add_row_url`] is `Some`, an "Add row"
/// `<button type="button">` is emitted with `hx-get`, `hx-target="#{container_id}"`,
/// `hx-swap="beforeend"`, and — critically — `hx-params="not _submit_token"` so the
/// one-time submit token is **not** spent fetching the fragment (mirroring the
/// inline-validation helpers in [`crate::form`]).
///
/// # CSRF
///
/// This renders only the child block. The surrounding `<form>` — via
/// [`crate::form::form_tag`] / `ChangesetForm` — carries the CSRF and submit-token
/// fields exactly as today; do **not** duplicate them here.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::form::{form_tag, required_text_input, submit_button};
/// use autumn_web::nested_form::{inputs_for, InputsForOptions};
///
/// // `form` is a `NestedChangesetForm<NewOrder, NewLineItem>` re-rendered after
/// // a failed submit; `changeset` is its inner `NestedChangeset`.
/// let opts = InputsForOptions {
///     add_row_url: Some("/orders/line-item-row".into()),
///     ..InputsForOptions::default()
/// };
/// form_tag("/orders", "POST", form.csrf_token(), maud::html! {
///     // Parent fields (CSRF + submit token are emitted by `form_tag`).
///     (required_text_input(&changeset.parent, "name", "Order name"))
///     // Repeating child rows.
///     (inputs_for(&changeset, &opts, |row| maud::html! {
///         (row.required_text_input("sku", "SKU"))
///         (row.number_input("quantity", "Quantity"))
///         (row.destroy_checkbox("Remove"))
///     }))
///     (submit_button("Create order"))
/// });
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn inputs_for<P, C: NestedChild>(
    nested: &NestedChangeset<P, C>,
    opts: &InputsForOptions,
    render_row: impl Fn(&RowScope) -> maud::Markup,
) -> maud::Markup {
    let collection = C::COLLECTION;
    let container_id = opts
        .container_id
        .clone()
        .unwrap_or_else(|| format!("{collection}-rows"));
    let rows = nested.rows();
    // Always emit at least one blank template row so the no-JS path can add a
    // child even when the caller asked for zero.
    let blank_count = opts.blank_rows.max(1);

    maud::html! {
        div id=(container_id) class="nested-fields" {
            @for (i, row) in rows.iter().enumerate() {
                @let scope = RowScope { collection, index: i, row: Some(row) };
                div class="nested-fields__row" data-index=(i) {
                    (render_row(&scope))
                }
            }
            @for k in 0..blank_count {
                @let index = rows.len() + k;
                @let scope = RowScope { collection, index, row: None };
                div class="nested-fields__row" data-index=(index) {
                    (render_row(&scope))
                }
            }
            @if let Some(url) = &opts.add_row_url {
                button
                    type="button"
                    class="nested-fields__add"
                    hx-get=(url)
                    hx-target=(format!("#{container_id}"))
                    hx-swap="beforeend"
                    hx-params="not _submit_token"
                { "Add row" }
            }
        }
    }
}

/// Render a single blank child row for the htmx "Add row" fragment endpoint.
///
/// Returns one `<div class="nested-fields__row" data-index="{index}">` produced
/// by `render_row` with a blank [`RowScope`]. Because the decoder tolerates
/// non-contiguous indices, `index` only needs to be **unique** within the form
/// (e.g. a monotonically increasing counter the client tracks), not contiguous.
#[cfg(feature = "maud")]
#[must_use]
pub fn nested_row_fragment<C: NestedChild>(
    index: usize,
    render_row: impl Fn(&RowScope) -> maud::Markup,
) -> maud::Markup {
    let scope = RowScope {
        collection: C::COLLECTION,
        index,
        row: None,
    };
    maud::html! {
        div class="nested-fields__row" data-index=(index) {
            (render_row(&scope))
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize, validator::Validate)]
    struct Order {
        #[validate(length(min = 1, message = "name required"))]
        name: String,
    }

    #[derive(serde::Deserialize, validator::Validate)]
    struct LineItem {
        #[validate(length(min = 1, message = "sku required"))]
        sku: String,
        #[validate(range(min = 1, message = "quantity must be >= 1"))]
        quantity: i32,
    }

    impl NestedChild for LineItem {
        const COLLECTION: &'static str = "items";
    }

    fn p(k: &str, v: &str) -> (String, String) {
        (k.to_owned(), v.to_owned())
    }

    #[test]
    fn binds_parent_and_two_children_in_order() {
        let pairs = vec![
            p("name", "Order 1"),
            p("items[0][sku]", "A-1"),
            p("items[0][quantity]", "2"),
            p("items[1][sku]", "B-2"),
            p("items[1][quantity]", "3"),
        ];
        let cs = decode_nested_urlencoded::<Order, LineItem>(&pairs).expect("parent decodes");
        assert!(cs.is_valid());
        assert_eq!(cs.rows().len(), 2);
        assert_eq!(cs.rows()[0].value("sku"), Some("A-1"));
        assert_eq!(cs.rows()[1].value("sku"), Some("B-2"));
        assert_eq!(cs.collection_name(), "items");

        let (order, items) = cs.into_valid().unwrap_or_else(|_| panic!("valid"));
        assert_eq!(order.name, "Order 1");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].sku, "A-1");
        assert_eq!(items[0].quantity, 2);
        assert_eq!(items[1].quantity, 3);
    }

    #[test]
    fn non_contiguous_indices_compact_preserving_order() {
        let pairs = vec![
            p("name", "Order"),
            p("items[0][sku]", "A"),
            p("items[0][quantity]", "1"),
            p("items[2][sku]", "C"),
            p("items[2][quantity]", "5"),
        ];
        let cs = decode_nested_urlencoded::<Order, LineItem>(&pairs).expect("parent decodes");
        // Gap at index 1 compacts: two rows in ascending order.
        assert_eq!(cs.rows().len(), 2);
        assert_eq!(cs.rows()[0].value("sku"), Some("A"));
        assert_eq!(cs.rows()[1].value("sku"), Some("C"));
        assert!(cs.is_valid());

        let (_order, items) = cs.into_valid().unwrap_or_else(|_| panic!("valid"));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].sku, "A");
        assert_eq!(items[1].sku, "C");
    }

    #[test]
    fn destroy_marker_drops_row_from_children_but_retains_it() {
        let pairs = vec![
            p("name", "Order"),
            p("items[0][sku]", "A"),
            p("items[0][quantity]", "1"),
            p("items[1][sku]", "X"),
            p("items[1][quantity]", "9"),
            p("items[1][_destroy]", "1"),
        ];
        let cs = decode_nested_urlencoded::<Order, LineItem>(&pairs).expect("parent decodes");
        assert_eq!(cs.rows().len(), 2);
        assert!(!cs.rows()[0].is_destroyed());
        assert!(cs.rows()[1].is_destroyed());
        // Destroyed row is still retained with its raw values for re-render.
        assert_eq!(cs.rows()[1].value("sku"), Some("X"));
        assert!(cs.is_valid());

        let (_order, items) = cs.into_valid().unwrap_or_else(|_| panic!("valid"));
        // Only the non-destroyed row contributes.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].sku, "A");
    }

    #[test]
    fn child_validation_failure_surfaces_combined_key_and_blocks_valid() {
        let pairs = vec![
            p("name", "Order"),
            p("items[0][sku]", "A"),
            p("items[0][quantity]", "2"),
            // quantity = 0 violates range(min = 1)
            p("items[1][sku]", "B"),
            p("items[1][quantity]", "0"),
        ];
        let cs = decode_nested_urlencoded::<Order, LineItem>(&pairs).expect("parent decodes");
        assert!(!cs.is_valid());
        assert!(!cs.errors_for("items[1].quantity").is_empty());
        // The valid sibling row has no error for that key.
        assert!(cs.errors_for("items[0].quantity").is_empty());
        assert!(cs.into_valid().is_err());
    }

    #[test]
    fn all_valid_yields_ok_with_coerced_numeric_fields() {
        let pairs = vec![
            p("name", "Order"),
            p("items[0][sku]", "A"),
            p("items[0][quantity]", "5"),
        ];
        let cs = decode_nested_urlencoded::<Order, LineItem>(&pairs).expect("parent decodes");
        assert!(cs.is_valid());
        let (order, items) = cs.into_valid().unwrap_or_else(|_| panic!("valid"));
        assert_eq!(order.name, "Order");
        assert_eq!(items.len(), 1);
        // "5" coerced to i32.
        let q: i32 = items[0].quantity;
        assert_eq!(q, 5);
    }

    #[test]
    fn parent_invalid_blocks_valid_children() {
        let pairs = vec![
            // Empty parent name violates length(min = 1).
            p("name", ""),
            p("items[0][sku]", "A"),
            p("items[0][quantity]", "1"),
        ];
        let cs = decode_nested_urlencoded::<Order, LineItem>(&pairs).expect("parent decodes");
        assert!(!cs.is_valid());
        assert!(!cs.errors_for("name").is_empty());
        assert!(cs.into_valid().is_err());
    }

    #[test]
    fn child_parse_failure_records_row_level_error() {
        let pairs = vec![
            p("name", "Order"),
            p("items[0][sku]", "A"),
            // Non-numeric quantity: hard parse failure for i32.
            p("items[0][quantity]", "not-a-number"),
        ];
        let cs = decode_nested_urlencoded::<Order, LineItem>(&pairs).expect("parent decodes");
        assert!(!cs.is_valid());
        assert_eq!(cs.rows().len(), 1);
        // Raw values retained for re-render.
        assert_eq!(cs.rows()[0].value("quantity"), Some("not-a-number"));
        // Row-level error surfaced under the bare row key (empty subfield).
        assert!(!cs.errors_for("items[0]").is_empty());
        assert!(!cs.rows()[0].errors_for("").is_empty());
    }

    #[test]
    fn parent_hard_parse_failure_is_err() {
        #[derive(serde::Deserialize, validator::Validate)]
        struct NumericParent {
            #[validate(range(min = 0))]
            count: i32,
        }
        #[derive(serde::Deserialize, validator::Validate)]
        struct Child {
            #[validate(length(min = 1))]
            name: String,
        }
        impl NestedChild for Child {
            const COLLECTION: &'static str = "kids";
        }

        let pairs = vec![p("count", "not-a-number")];
        let result = decode_nested_urlencoded::<NumericParent, Child>(&pairs);
        assert!(result.is_err());
    }

    #[test]
    fn parse_child_key_matches_and_rejects() {
        assert_eq!(parse_child_key("items[0][sku]", "items"), Some((0, "sku")));
        assert_eq!(
            parse_child_key("items[12][quantity]", "items"),
            Some((12, "quantity"))
        );
        // Wrong collection.
        assert_eq!(parse_child_key("other[0][sku]", "items"), None);
        // Not a child subfield (parent key).
        assert_eq!(parse_child_key("name", "items"), None);
        // Trailing junk after the closing bracket.
        assert_eq!(parse_child_key("items[0][sku]x", "items"), None);
        // Non-numeric index.
        assert_eq!(parse_child_key("items[a][sku]", "items"), None);
    }
}

#[cfg(all(test, feature = "maud"))]
mod maud_tests {
    use super::*;

    #[derive(serde::Deserialize, validator::Validate)]
    struct Order {
        #[validate(length(min = 1, message = "name required"))]
        name: String,
    }

    #[derive(serde::Deserialize, validator::Validate)]
    struct LineItem {
        #[validate(length(min = 1, message = "sku required"))]
        sku: String,
        #[validate(range(min = 1, message = "quantity must be >= 1"))]
        quantity: i32,
    }

    impl NestedChild for LineItem {
        const COLLECTION: &'static str = "items";
    }

    fn p(k: &str, v: &str) -> (String, String) {
        (k.to_owned(), v.to_owned())
    }

    /// A changeset with two submitted rows, the second failing child validation
    /// (`quantity = 0`), so both rows are retained for re-render.
    fn two_row_changeset() -> NestedChangeset<Order, LineItem> {
        let pairs = vec![
            p("name", "Order 1"),
            p("items[0][sku]", "A-1"),
            p("items[0][quantity]", "2"),
            p("items[1][sku]", "B-2"),
            // quantity = 0 violates range(min = 1): row 1 is retained with an error.
            p("items[1][quantity]", "0"),
        ];
        decode_nested_urlencoded::<Order, LineItem>(&pairs).expect("parent decodes")
    }

    fn render_row(row: &RowScope) -> maud::Markup {
        maud::html! {
            (row.required_text_input("sku", "SKU"))
            (row.number_input("quantity", "Quantity"))
            (row.destroy_checkbox("Remove"))
        }
    }

    #[test]
    fn existing_rows_render_indexed_names_and_prefilled_values() {
        let cs = two_row_changeset();
        let opts = InputsForOptions::default();
        let html = inputs_for(&cs, &opts, render_row).into_string();

        assert!(html.contains(r#"name="items[0][sku]""#), "{html}");
        assert!(html.contains(r#"name="items[1][sku]""#), "{html}");
        // Pre-filled values from the re-rendered changeset.
        assert!(html.contains(r#"value="A-1""#), "{html}");
        assert!(html.contains(r#"value="B-2""#), "{html}");
        // Container defaults to "{collection}-rows".
        assert!(html.contains(r#"id="items-rows""#), "{html}");
    }

    #[test]
    fn per_row_error_renders_scoped_alert_block() {
        let cs = two_row_changeset();
        let opts = InputsForOptions::default();
        let html = inputs_for(&cs, &opts, render_row).into_string();

        // The failing row's quantity error is scoped to that row's unique id.
        assert!(html.contains(r#"id="items-1-quantity-error""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("quantity must be &gt;= 1"), "{html}");
        // The valid sibling row's quantity has no error block.
        assert!(!html.contains(r#"id="items-0-quantity-error""#), "{html}");
    }

    #[test]
    fn appends_blank_template_row_with_next_index() {
        let cs = two_row_changeset();
        let opts = InputsForOptions::default();
        let html = inputs_for(&cs, &opts, render_row).into_string();

        // Two existing rows (indices 0,1) plus one blank row at index 2.
        assert!(html.contains(r#"data-index="0""#), "{html}");
        assert!(html.contains(r#"data-index="1""#), "{html}");
        assert!(html.contains(r#"data-index="2""#), "{html}");
        assert!(html.contains(r#"name="items[2][sku]""#), "{html}");
    }

    #[test]
    fn always_emits_a_blank_row_even_when_blank_rows_zero() {
        let cs = two_row_changeset();
        let opts = InputsForOptions {
            blank_rows: 0,
            ..InputsForOptions::default()
        };
        let html = inputs_for(&cs, &opts, render_row).into_string();
        // Still emits the blank template row at index 2.
        assert!(html.contains(r#"data-index="2""#), "{html}");
    }

    #[test]
    fn destroy_checkbox_emits_indexed_marker() {
        let cs = two_row_changeset();
        let opts = InputsForOptions::default();
        let html = inputs_for(&cs, &opts, render_row).into_string();

        assert!(html.contains(r#"name="items[0][_destroy]""#), "{html}");
        assert!(html.contains(r#"name="items[1][_destroy]""#), "{html}");
        assert!(html.contains(r#"type="checkbox""#), "{html}");
    }

    #[test]
    fn add_button_renders_only_with_url_and_carries_htmx_attrs() {
        let cs = two_row_changeset();

        // No URL: no Add button.
        let none = inputs_for(&cs, &InputsForOptions::default(), render_row).into_string();
        assert!(!none.contains("Add row"), "{none}");

        // With URL: button carries the submit-token filter and beforeend swap.
        let opts = InputsForOptions {
            add_row_url: Some("/orders/line-item-row".into()),
            ..InputsForOptions::default()
        };
        let html = inputs_for(&cs, &opts, render_row).into_string();
        assert!(html.contains("Add row"), "{html}");
        assert!(html.contains(r#"hx-params="not _submit_token""#), "{html}");
        assert!(html.contains(r#"hx-swap="beforeend""#), "{html}");
        assert!(html.contains(r#"hx-get="/orders/line-item-row""#), "{html}");
        assert!(html.contains(r##"hx-target="#items-rows""##), "{html}");
    }

    #[test]
    fn destroy_checkbox_checked_reflects_destroyed_row() {
        let pairs = vec![
            p("name", "Order"),
            p("items[0][sku]", "A"),
            p("items[0][quantity]", "1"),
            p("items[0][_destroy]", "1"),
        ];
        let cs = decode_nested_urlencoded::<Order, LineItem>(&pairs).expect("parent decodes");
        let html = inputs_for(&cs, &InputsForOptions::default(), render_row).into_string();
        // The destroyed row's checkbox is checked.
        assert!(
            html.contains(r#"name="items[0][_destroy]" value="1" checked"#),
            "{html}"
        );
    }

    #[test]
    fn nested_row_fragment_renders_single_row_at_index() {
        let html = nested_row_fragment::<LineItem>(7, render_row).into_string();
        assert!(html.contains(r#"data-index="7""#), "{html}");
        assert!(html.contains(r#"name="items[7][sku]""#), "{html}");
        // Exactly one row wrapper.
        assert_eq!(html.matches("nested-fields__row").count(), 1, "{html}");
    }
}
