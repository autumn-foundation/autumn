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
//! # Example
//!
//! ```rust,ignore
//! #[post("/orders")]
//! async fn create(form: NestedChangesetForm<NewOrder, NewLineItem>) -> impl IntoResponse {
//!     match form.into_valid() {
//!         Ok((order, items)) => { /* persist order + items */ }
//!         Err(form) => (StatusCode::UNPROCESSABLE_ENTITY, render(&form)).into_response(),
//!     }
//! }
//! ```
//!
//! The Maud renderer (`inputs_for`) and the DB integration path are
//! follow-up work; the row/error accessors here
//! ([`NestedChangeset::rows`], [`NestedRow::value`],
//! [`NestedRow::errors_for`], [`NestedRow::is_destroyed`]) expose everything
//! a per-row renderer and a blank template row need.

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
