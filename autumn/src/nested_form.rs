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
//! any other. Because the browser submits that blank row's empty inputs, the
//! decoder applies Rails-style `reject_if: :all_blank`: a child row whose every
//! non-`_destroy` subfield is blank (empty or whitespace-only) is **ignored**
//! entirely — never decoded, validated, or persisted — so the auto-rendered
//! blank template row is safe with no JS and never becomes a phantom child that
//! blocks submission (on a required-field child) or saves an empty row (on an
//! all-optional child). Supplying [`InputsForOptions::add_row_url`] additionally emits an
//! htmx "Add row" button (`hx-get` + `hx-swap="beforeend"`, with
//! `hx-params="none"` so no form fields — and no CSRF/submit token — are
//! serialized into the fragment request; only the `hx-vals` `index` is sent);
//! [`nested_row_fragment`] renders the server response
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
//! [`Repository`](macro@crate::repository) `create`: that `create` opens its *own*
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
    /// records its error under the offending subfield's name (via
    /// `serde_path_to_error`), falling back to the empty-string (row-level) key
    /// only when no field can be identified.
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
    /// Build a blank, **non-validating** nested changeset for the initial
    /// `new` (or `edit`) render, before the user has submitted anything.
    ///
    /// Mirrors [`ChangesetForm::blank`](crate::form::ChangesetForm::blank): it
    /// wraps `parent` in a valid [`Changeset`] (via [`Changeset::new`], which
    /// records **no** errors — it does **not** run [`validator::Validate`])
    /// with **zero** child rows. So `is_valid()` returns `true`,
    /// `errors_for(..)` is empty, `rows()` is empty, and `into_valid()` returns
    /// `Ok((parent, vec![]))` even when a required parent field is still empty.
    ///
    /// Use this for the initial GET render so the blank page does not show a
    /// premature "field is required" error or `aria-invalid="true"` before the
    /// user has typed. Use [`decode_nested_urlencoded`] (or the
    /// [`NestedChangesetForm`] extractor) for the POST decode that validates a
    /// real submission and re-renders with inline errors.
    #[must_use]
    pub fn blank(parent: P) -> Self {
        Self {
            parent: Changeset::new(parent),
            rows: Vec::new(),
            valid_children: Some(Vec::new()),
        }
    }

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
/// Following Rails' `reject_if: :all_blank`, a child row whose every
/// non-`_destroy` subfield value is blank (empty or whitespace-only after
/// trimming) is **ignored**: it is not decoded, not validated, not retained in
/// `rows`, and never counts toward `valid_children` — exactly as if that index
/// had never been submitted. This is what makes the blank template row
/// [`inputs_for`] auto-renders safe with no JavaScript. A row with at least one
/// non-blank non-`_destroy` value is kept and validated as usual, so a
/// partially filled row that omits a required field still surfaces per-field
/// errors.
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

        // Rails `reject_if: :all_blank`: if every non-`_destroy` subfield value
        // is blank (empty or whitespace-only after trimming), drop the row
        // entirely. This is the auto-rendered blank template row `inputs_for`
        // emits for the no-JS "add a child" path, not a real submitted child —
        // treat it as if the index was never submitted: do not decode, do not
        // validate, do not retain it, and do not count it toward the children.
        // A row with at least one non-blank non-`_destroy` value is kept and
        // validated as usual, so a partially filled row still surfaces its
        // per-field errors. (`decode_pairs` already excludes `_destroy`, so an
        // all-blank row that also carries `_destroy` is dropped here too — the
        // same outcome as the destroy path.)
        let all_blank = decode_pairs.iter().all(|(_, val)| val.trim().is_empty());
        if all_blank {
            continue;
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
                // Row parse failure (deserialization failed before validation,
                // e.g. `sku` filled but the required numeric `quantity` is
                // malformed or present-but-blank). Key the message under the
                // offending subfield so it surfaces as `items[i].{field}`,
                // rendered by `errors_for("{field}")` next to the offending
                // input rather than only under the row-level "" key.
                //
                // The blank-optional retry inside
                // `decode_urlencoded_dropping_blank_optional_fields` can DROP a
                // present-but-blank typed field (`quantity=`) before serde sees
                // it, so the primary error is then `missing field \`quantity\``
                // reported at the ROW ROOT with an empty path — losing the field
                // name the row helpers need. `recover_child_error_field` layers a
                // raw (non-dropping) re-decode and a message parse on top of the
                // primary path to recover it; see that helper. Only if all layers
                // fail does it fall back to the row-level "" key, so the error is
                // never silently dropped.
                let field = recover_child_error_field::<C>(&e, &decode_pairs);
                errors.entry(field).or_default().push(e.to_string());
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

/// Last `Map`/`Enum` key in a `serde_path_to_error` path, if any is non-empty.
///
/// This is the subfield the decode failure is attributed to (e.g. `quantity`
/// for `items[i][quantity]`). `Seq`/`Unknown` segments carry no field name.
fn last_path_field(path: &serde_path_to_error::Path) -> Option<String> {
    path.iter()
        .filter_map(|seg| match seg {
            serde_path_to_error::Segment::Map { key }
            | serde_path_to_error::Segment::Enum { variant: key } => Some(key.clone()),
            serde_path_to_error::Segment::Seq { .. } | serde_path_to_error::Segment::Unknown => {
                None
            }
        })
        .next_back()
        .filter(|f| !f.is_empty())
}

/// Extract the field name from serde's stable "missing field" message — the
/// backtick-quoted identifier in [`serde::de::Error::missing_field`]'s
/// `Display` format — if present.
fn missing_field_name(msg: &str) -> Option<String> {
    let after = msg.split_once("missing field `")?.1;
    let name = after.split_once('`')?.0;
    (!name.is_empty()).then(|| name.to_owned())
}

/// Recover the offending subfield name for a child-row parse failure so the
/// error can be keyed to `items[i].{field}` (rendered next to the input) rather
/// than only under the row-level "" key.
///
/// Layered because the blank-optional retry inside
/// [`decode_urlencoded_dropping_blank_optional_fields`] can DROP a
/// present-but-blank typed field (`quantity=`) before serde ever sees it, so
/// the primary decode reports a root-level missing-field error for `quantity`
/// with an empty path — losing the field name the row helpers need:
///
/// 1. The primary decode's `serde_path_to_error` path (last `Map`/`Enum` key).
///    For a malformed non-blank value (`quantity=abc`) this already yields the
///    field, since that pair is never dropped.
/// 2. If empty, a raw re-decode over the ORIGINAL row pairs with blanks
///    RETAINED (no dropping), mirroring the shared helper's deserializer minus
///    the drop loop. For a present-but-blank typed field serde now fails
///    parsing `""` AT `.quantity`, recovering the field.
/// 3. If still empty (a field truly never submitted), the backtick-quoted
///    identifier in serde's missing-field message.
/// 4. Otherwise `""` (row-level key) so the error is never silently dropped.
///
/// This runs ONLY on the already-failed path to LOCATE the field; it does not
/// affect the success path. Legitimately-blank optional fields still decode to
/// `None` via the primary blank-dropping decode, which succeeds for them and
/// never reaches here.
fn recover_child_error_field<C: serde::de::DeserializeOwned>(
    primary: &serde_path_to_error::Error<serde_urlencoded::de::Error>,
    decode_pairs: &[(String, String)],
) -> String {
    // 1. Primary decode path.
    if let Some(field) = last_path_field(primary.path()) {
        return field;
    }

    // 2. Raw (non-dropping) re-decode over the original row pairs.
    let encoded = encode_pairs(decode_pairs);
    let deserializer =
        serde_urlencoded::Deserializer::new(url::form_urlencoded::parse(encoded.as_bytes()));
    if let Err(raw_err) = serde_path_to_error::deserialize::<_, C>(deserializer) {
        if let Some(field) = last_path_field(raw_err.path()) {
            return field;
        }
        // 3. Parse serde's stable `missing field \`X\`` message.
        if let Some(field) = missing_field_name(&raw_err.to_string()) {
            return field;
        }
    }

    // 4. Row-level key.
    String::new()
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
    /// Build a blank nested-form context for the initial `new`/`edit` GET
    /// render, before any submission.
    ///
    /// Mirrors [`ChangesetForm::blank`](crate::form::ChangesetForm::blank): it
    /// wraps `parent` in a **non-validating** [`NestedChangeset::blank`] (no
    /// child rows, no errors) so the initial page renders clean — no premature
    /// "field is required" message or `aria-invalid="true"` before the user has
    /// typed. Contrast the POST path (the [`NestedChangesetForm`] extractor /
    /// [`decode_nested_urlencoded`]), which validates the submission and
    /// re-renders inline errors.
    ///
    /// `csrf_token` is the token from a `CsrfToken` extractor, or `None` when
    /// CSRF middleware is not active. The CSRF and submit-token field names
    /// default to `_csrf` / `_submit_token` (exactly what the extractor falls
    /// back to when the corresponding config extensions are absent); when the
    /// app customizes `security.csrf.form_field`, set it with
    /// [`with_csrf_field`](Self::with_csrf_field) so
    /// [`form_tag`](Self::form_tag) emits the right hidden field name.
    ///
    /// The submit token starts `None`, so a bare `blank(..).form_tag(..)` emits
    /// **no** submit-token hidden input and the first submission is not protected
    /// against double-submit ([`SubmitTokenLayer`](crate::security::submit_token)
    /// passes tokenless mutating requests through). Supply the minted token on the
    /// initial GET with [`with_submit_token`](Self::with_submit_token) (and
    /// [`with_submit_field`](Self::with_submit_field) if the field name is
    /// customized) so the **first** submit carries a token too:
    ///
    /// ```rust,ignore
    /// #[get("/orders/new")]
    /// async fn new_order(csrf: CsrfToken, submit: SubmitToken) -> Markup {
    ///     let form = NestedChangesetForm::<NewOrder, NewLineItem>::blank(
    ///         NewOrder::default(),
    ///         Some(csrf.token().to_owned()),
    ///     )
    ///     .with_submit_token(Some(submit.token().to_owned()));
    ///     form.form_tag("/orders", "post", /* … */)
    /// }
    /// ```
    #[must_use]
    pub fn blank(parent: P, csrf_token: Option<String>) -> Self {
        Self {
            changeset: NestedChangeset::blank(parent),
            csrf_token,
            csrf_field: "_csrf".to_owned(),
            submit_token: None,
            submit_field: "_submit_token".to_owned(),
        }
    }

    /// Wrap a pre-built [`NestedChangeset`] (which may already carry validation
    /// errors) in a form for rendering, with no CSRF/submit token.
    ///
    /// Mirrors [`ChangesetForm::from_changeset`](crate::form::ChangesetForm::from_changeset):
    /// useful in tests and cases where a `NestedChangeset` was produced
    /// externally (e.g. via [`decode_nested_urlencoded`]) before constructing a
    /// form for re-render. The CSRF/submit-token field names default to
    /// `_csrf` / `_submit_token`; add a token with
    /// [`with_csrf_field`](Self::with_csrf_field) as needed.
    #[must_use]
    pub fn from_changeset(changeset: NestedChangeset<P, C>) -> Self {
        Self {
            changeset,
            csrf_token: None,
            csrf_field: "_csrf".to_owned(),
            submit_token: None,
            submit_field: "_submit_token".to_owned(),
        }
    }

    /// Override the CSRF form-field name used by [`form_tag`](Self::form_tag).
    ///
    /// Mirrors
    /// [`ChangesetForm::with_csrf_field`](crate::form::ChangesetForm::with_csrf_field):
    /// call this on a [`blank`](Self::blank) GET-handler form when
    /// `security.csrf.form_field` is customized (e.g. `"authenticity_token"`).
    /// The extractor captures the configured name automatically on the POST
    /// path.
    #[must_use]
    pub fn with_csrf_field(mut self, field: impl Into<String>) -> Self {
        self.csrf_field = field.into();
        self
    }

    /// Supply the one-time submit token to a [`blank`](Self::blank) (or
    /// [`from_changeset`](Self::from_changeset)) GET-handler form so
    /// [`form_tag`](Self::form_tag) emits the hidden submit-token input on the
    /// **initial** render — protecting the very first submission against
    /// double-submit, not just later 422 re-renders.
    ///
    /// [`blank`](Self::blank) leaves this `None` (the initial page renders no
    /// submit-token field otherwise), and [`SubmitTokenLayer`](crate::security::submit_token)
    /// passes tokenless mutating requests through unchanged — so without calling
    /// this the first create-form submit is unprotected. Source the token from a
    /// [`SubmitToken`](crate::security::SubmitToken) extractor on the GET handler.
    /// The extractor captures it automatically on the POST re-render path.
    ///
    /// When the app customizes `security.submit_token.field_name`, pair this with
    /// [`with_submit_field`](Self::with_submit_field) so the hidden input carries
    /// the right name.
    #[must_use]
    pub fn with_submit_token(mut self, token: Option<String>) -> Self {
        self.submit_token = token;
        self
    }

    /// Override the submit-token form-field name used by
    /// [`form_tag`](Self::form_tag).
    ///
    /// Mirrors [`with_csrf_field`](Self::with_csrf_field): call this on a
    /// [`blank`](Self::blank) GET-handler form when
    /// `security.submit_token.field_name` is customized (the default is
    /// `_submit_token`). The extractor captures the configured name automatically
    /// on the POST path.
    #[must_use]
    pub fn with_submit_field(mut self, field: impl Into<String>) -> Self {
        self.submit_field = field.into();
        self
    }

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

/// Maud rendering — emit the `<form>` open tag with the **captured** CSRF (and
/// submit-token) hidden fields injected.
#[cfg(feature = "maud")]
impl<P, C: NestedChild> NestedChangesetForm<P, C> {
    /// Render a `<form>` element wrapping `content`, injecting the CSRF hidden
    /// input under the **captured** field name (honouring
    /// `security.csrf.form_field`) and — when present — the one-time
    /// submit-token hidden input under its captured field name.
    ///
    /// Mirrors [`ChangesetForm::form_tag`](crate::form::ChangesetForm::form_tag),
    /// including the `PUT`/`PATCH`/`DELETE` → hidden `_method` override. **Prefer
    /// this over the standalone [`crate::form::form_tag`]** for a nested-form
    /// re-render: the standalone helper hardcodes the default `_csrf` field name
    /// and would emit the wrong hidden field for an app that customized
    /// `security.csrf.form_field`, so the next submit's CSRF check would reject
    /// the form. This method uses the field name the extractor captured (or the
    /// one set via [`with_csrf_field`](Self::with_csrf_field) on a
    /// [`blank`](Self::blank) form), keeping CSRF parity across the re-render.
    ///
    /// The submit-token hidden input is emitted only when a submit token is
    /// present. The POST re-render path captures it automatically; on the initial
    /// GET render from [`blank`](Self::blank), supply it with
    /// [`with_submit_token`](Self::with_submit_token) so the **first** submit is
    /// protected against double-submit too — otherwise a bare
    /// `blank(..).form_tag(..)` create form carries no submit token and its first
    /// submission passes through [`SubmitTokenLayer`](crate::security::submit_token)
    /// unprotected.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn form_tag(&self, action: &str, method: &str, content: maud::Markup) -> maud::Markup {
        crate::form::form_tag_inner(
            action,
            method,
            &self.csrf_field,
            self.csrf_token.as_deref(),
            None,
            maud::html! {
                @if let Some(token) = self.submit_token.as_deref() {
                    input type="hidden" name=(self.submit_field) value=(token);
                }
                (content)
            },
        )
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

    /// Like [`RowScope::text_input`] but adds `required` + `aria-required="true"`
    /// — **only for real/submitted rows that are not marked for destruction**
    /// (`self.row.is_some() && !self.is_destroyed()`).
    ///
    /// A blank template row (`row: None`, the one [`inputs_for`] auto-appends
    /// for the no-JS "add a child" path) deliberately renders the *same* input
    /// **without** the client-side `required`/`aria-required` attributes so it
    /// stays leaveable-empty. Otherwise the browser's native constraint
    /// validation would block form submit on the empty trailing template row
    /// *before* the server's all-blank rejection can run — a user who filled one
    /// child row could not submit while a blank row sat beneath it. A submitted
    /// row the user ticked `_destroy` on is skipped for the same reason: the
    /// decoder drops destroyed rows before validation (and from `into_valid()`),
    /// so re-emitting client-side `required` on an empty destroyed field would
    /// let the browser block the next submit before the server can honor the
    /// removal. Required-ness is still enforced server-side for any row the user
    /// actually engages: a partially-filled, non-destroyed row is retained as a
    /// `Some` row on re-render and re-acquires `required`, and the decoder's
    /// all-blank rejection drops the untouched template row. This composes with
    /// that all-blank rejection so
    /// neither the client nor the server treats the template row as a real child.
    #[must_use]
    pub fn required_text_input(&self, sub: &str, label: &str) -> maud::Markup {
        self.text_like_input(sub, label, true)
    }

    /// Shared body for the text-like inputs.
    fn text_like_input(&self, sub: &str, label: &str, required: bool) -> maud::Markup {
        // Emit the client-side `required`/`aria-required` constraint only for a
        // real/submitted row that is *not* marked for destruction. A blank
        // template row (`row: None`) must stay leaveable-empty so the browser
        // does not block submit on the trailing auto-appended blank row before
        // the server's all-blank rejection runs; server-side validation still
        // enforces required-ness for any row the user actually fills (it is
        // retained as a `Some` row on re-render). A submitted row the user
        // ticked `_destroy` on is likewise skipped: the decoder drops destroyed
        // rows before validation, so re-emitting client-side `required` on an
        // empty destroyed field would let the browser's constraint validation
        // block the next submit before the server can honor `_destroy`.
        let required = required && self.row.is_some() && !self.is_destroyed();
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
/// The appended blank template row makes adding a child work with **no
/// JavaScript**, and it is safe even though the browser submits its empty
/// inputs: [`decode_nested_urlencoded`] applies Rails-style
/// `reject_if: :all_blank`, so a child row whose every non-`_destroy` subfield
/// is blank is ignored rather than decoded as a phantom child. An untouched
/// blank row therefore never blocks submission or persists an empty child.
///
/// So the no-JS path stays submittable, the appended blank template row's inputs
/// omit the client-side `required`/`aria-required` constraint by design (see
/// [`RowScope::required_text_input`]): a required-variant input renders empty and
/// *leaveable* on a `row: None` template row, so the browser's native validation
/// does not block submitting a form that still carries a trailing blank row.
/// This composes with the decoder's all-blank rejection — the untouched template
/// row is dropped server-side — while server-side validation still enforces
/// required-ness for any row the user actually fills.
///
/// When [`InputsForOptions::add_row_url`] is `Some`, an "Add row"
/// `<button type="button">` is emitted with `hx-get`, `hx-target="#{container_id}"`,
/// `hx-swap="beforeend"`, and — critically — `hx-params="none"` so **no**
/// form-field inputs (and no `_csrf`/submit token) are serialized into the
/// Add-row GET; per htmx semantics `not X` would include every parameter
/// except `X`, leaking the current parent + line-item fields into the request
/// query string / proxy logs. The fragment endpoint needs only the `index`,
/// which the `hx-vals` below supplies independently of `hx-params` filtering.
///
/// The button also carries an `hx-vals` (`js:` form) that computes a fresh
/// `index` for each request — `max(existing data-index) + 1` scanning only
/// `#{container_id} .nested-fields__row[data-index]` (scoped to this form's
/// container so multiple nested forms on a page never clash). The endpoint
/// should read that `index` param and pass it to [`nested_row_fragment`] so
/// every appended row gets a name unique within the form; a static request
/// would reuse the same index and emit duplicate control names. This is the
/// JS-present enhancement path only — the no-JS fallback (pre-rendered blank
/// rows) needs no such counter and is unaffected.
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
/// use autumn_web::form::{required_text_input, submit_button};
/// use autumn_web::nested_form::{inputs_for, InputsForOptions};
///
/// // `form` is a `NestedChangesetForm<NewOrder, NewLineItem>` re-rendered after
/// // a failed submit; it derefs to its inner `NestedChangeset`.
/// let opts = InputsForOptions {
///     add_row_url: Some("/orders/line-item-row".into()),
///     ..InputsForOptions::default()
/// };
/// // Prefer `form.form_tag` over the standalone `form_tag`: it emits the CSRF
/// // hidden field under the app-configured field name (and the submit token),
/// // so a customized `security.csrf.form_field` survives the re-render.
/// form.form_tag("/orders", "POST", maud::html! {
///     // Parent fields (CSRF + submit token are emitted by `form.form_tag`).
///     (required_text_input(&form.parent, "name", "Order name"))
///     // Repeating child rows.
///     (inputs_for(&form, &opts, |row| maud::html! {
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
                // Send a *fresh* unique row index with every Add-row request.
                // `nested_row_fragment` requires the returned row's index to be
                // unique within this form; a static request would keep reusing
                // the same index and produce duplicate control names. Compute
                // `max(existing data-index) + 1` from THIS container's rows only
                // (scoped by `#{container_id}` so multiple nested forms on one
                // page never clash). The decoder tolerates gaps, so this stays
                // collision-free even after client-side row removals.
                //
                // `hx-params="none"` serializes NO form-field inputs with the
                // Add-row GET: per htmx semantics `not X` would include every
                // parameter except `X`, leaking the current parent + line-item
                // fields (and `_csrf`) into the request query string / proxy
                // logs and risking URL-length limits. The fragment endpoint
                // needs only the `index`, which `hx-vals` supplies independently
                // of `hx-params` filtering, so `"none"` sends the `index` and
                // nothing else (no form fields, no CSRF/submit token).
                @let hx_vals = format!(
                    "js:{{\"index\": Math.max(-1, ...Array.from(document.querySelectorAll(\"#{container_id} .nested-fields__row[data-index]\")).map(function(e){{return parseInt(e.dataset.index,10);}})) + 1}}"
                );
                button
                    type="button"
                    class="nested-fields__add"
                    hx-get=(url)
                    hx-target=(format!("#{container_id}"))
                    hx-swap="beforeend"
                    hx-params="none"
                    hx-vals=(hx_vals)
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
///
/// The [`inputs_for`] Add-row button sends this unique value as an `index`
/// param (via `hx-vals`, computed as `max(existing data-index) + 1`). Because
/// the button sets `hx-params="none"`, the Add-row request carries ONLY the
/// `hx-vals` `index` — no parent/line-item form fields and no CSRF/submit
/// token are serialized — and `index` is the only value this endpoint needs.
/// The endpoint should read that param from the query/form and pass it straight
/// to `nested_row_fragment(index, ..)`. Uniqueness — not contiguity — is the
/// contract: the decoder tolerates gaps, so indices left behind by client-side
/// row removals never cause collisions.
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

    /// A child with one **required** field (`name`) and one **optional** field
    /// (`nickname`), used to exercise the reject-all-blank rules.
    #[derive(serde::Deserialize, validator::Validate)]
    struct Contact {
        #[validate(length(min = 1, message = "name required"))]
        name: String,
        #[validate(length(min = 1, message = "nickname too short"))]
        nickname: Option<String>,
    }

    impl NestedChild for Contact {
        const COLLECTION: &'static str = "contacts";
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
    fn child_parse_failure_keys_error_to_offending_field() {
        let pairs = vec![
            p("name", "Order"),
            // `sku` is filled, so the row is NOT all-blank (it is kept, not
            // dropped) — contrast `all_blank_template_row_is_ignored`, where an
            // empty row is dropped entirely with no error.
            p("items[0][sku]", "A"),
            // Non-numeric quantity: hard parse failure for i32, before
            // validation. `serde_path_to_error` attributes it to `quantity`.
            p("items[0][quantity]", "not-a-number"),
        ];
        let cs = decode_nested_urlencoded::<Order, LineItem>(&pairs).expect("parent decodes");
        assert!(!cs.is_valid());
        assert_eq!(cs.rows().len(), 1);
        // Raw values retained for re-render.
        assert_eq!(cs.rows()[0].value("sku"), Some("A"));
        assert_eq!(cs.rows()[0].value("quantity"), Some("not-a-number"));
        // The parse error is keyed to the offending subfield, so it renders next
        // to the bad input via `errors_for("quantity")` on that row.
        assert!(!cs.errors_for("items[0].quantity").is_empty());
        assert!(!cs.rows()[0].errors_for("quantity").is_empty());
        // It is NOT stored under the row-level "" key anymore.
        assert!(cs.rows()[0].errors_for("").is_empty());
        assert!(cs.errors_for("items[0]").is_empty());
        assert!(cs.into_valid().is_err());
    }

    #[test]
    fn child_parse_failure_on_blank_typed_field_keys_error_to_field() {
        // The Codex case: a row with `sku` filled but the required TYPED
        // `quantity` present-but-blank. The blank-optional retry inside
        // `decode_urlencoded_dropping_blank_optional_fields` DROPS the blank
        // `quantity=` pair, so serde reports `missing field \`quantity\`` at the
        // ROW ROOT (empty path). The field-recovery re-decode over the original
        // (blank-retained) pairs must still key the error to `quantity` so it
        // renders next to the blank input rather than the row-level "" key.
        let pairs = vec![
            p("name", "Order"),
            p("items[0][sku]", "A"),
            p("items[0][quantity]", ""),
        ];
        let cs = decode_nested_urlencoded::<Order, LineItem>(&pairs).expect("parent decodes");
        assert!(!cs.is_valid());
        assert_eq!(cs.rows().len(), 1);
        // Keyed to the offending subfield, so it renders next to the blank input.
        assert!(!cs.errors_for("items[0].quantity").is_empty());
        assert!(!cs.rows()[0].errors_for("quantity").is_empty());
        // NOT stored under the row-level "" / `items[0]` key.
        assert!(cs.rows()[0].errors_for("").is_empty());
        assert!(cs.errors_for("items[0]").is_empty());
        assert!(cs.into_valid().is_err());
    }

    #[test]
    fn child_row_with_blank_optional_typed_field_still_decodes_valid() {
        // Regression guard for the fix: a row with the required field filled and
        // an OPTIONAL TYPED field present-but-blank (`weight=`) must still decode
        // to a VALID row. The primary blank-dropping decode drops the blank
        // `weight=` pair so it resolves to `None` and the decode SUCCEEDS — the
        // field-recovery re-decode (which runs only on the failed path) is never
        // reached, and the row must stay valid.
        #[derive(serde::Deserialize, validator::Validate)]
        struct Widget {
            #[validate(length(min = 1, message = "label required"))]
            label: String,
            // Typed optional: a blank submission is dropped to `None` by the
            // blank-optional decode (an empty string is not a valid `i32`).
            weight: Option<i32>,
        }
        impl NestedChild for Widget {
            const COLLECTION: &'static str = "widgets";
        }

        let pairs = vec![
            p("name", "Order"),
            p("widgets[0][label]", "Bolt"),
            p("widgets[0][weight]", ""),
        ];
        let cs = decode_nested_urlencoded::<Order, Widget>(&pairs).expect("parent decodes");
        assert!(cs.is_valid());
        assert_eq!(cs.rows().len(), 1);
        assert!(cs.errors_for("widgets[0].weight").is_empty());
        assert!(cs.rows()[0].errors_for("weight").is_empty());
        let (_order, widgets) = cs.into_valid().unwrap_or_else(|_| panic!("valid"));
        assert_eq!(widgets.len(), 1);
        assert_eq!(widgets[0].label, "Bolt");
        assert_eq!(widgets[0].weight, None);
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

    // ── reject_if: :all_blank ──────────────────────────────────────

    /// The core regression test for the phantom-blank-row P1: `inputs_for`
    /// always emits a trailing all-blank template row, and with no JS the
    /// browser submits its empty inputs. That row must be ignored, not decoded
    /// as a real (empty) child.
    #[test]
    fn all_blank_template_row_is_ignored() {
        let pairs = vec![
            p("name", "Order"),
            // One filled, valid child row.
            p("contacts[0][name]", "Alice"),
            p("contacts[0][nickname]", "Al"),
            // The auto-rendered blank template row: every subfield empty.
            p("contacts[1][name]", ""),
            p("contacts[1][nickname]", ""),
        ];
        let cs = decode_nested_urlencoded::<Order, Contact>(&pairs).expect("parent decodes");
        assert!(cs.is_valid());
        // The blank template row was dropped entirely — only the real row remains.
        assert_eq!(cs.rows().len(), 1);
        assert_eq!(cs.rows()[0].value("name"), Some("Alice"));

        let (_order, contacts) = cs.into_valid().unwrap_or_else(|_| panic!("valid"));
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].name, "Alice");
    }

    /// Blank detection trims: a row whose fields are whitespace-only (or empty)
    /// is treated as blank and dropped.
    #[test]
    fn whitespace_only_row_is_ignored() {
        let pairs = vec![
            p("name", "Order"),
            p("contacts[0][name]", "Bob"),
            // Whitespace-only + empty: still all-blank after trimming.
            p("contacts[1][name]", "   "),
            p("contacts[1][nickname]", ""),
        ];
        let cs = decode_nested_urlencoded::<Order, Contact>(&pairs).expect("parent decodes");
        assert!(cs.is_valid());
        assert_eq!(cs.rows().len(), 1);
        assert_eq!(cs.rows()[0].value("name"), Some("Bob"));

        let (_order, contacts) = cs.into_valid().unwrap_or_else(|_| panic!("valid"));
        assert_eq!(contacts.len(), 1);
    }

    /// A row with at least one non-blank value is a real child the user is
    /// adding: it is kept and validated, so a missing **required** field still
    /// surfaces a per-row error. We must not over-aggressively drop it.
    #[test]
    fn partially_filled_row_with_missing_required_still_errors() {
        let pairs = vec![
            p("name", "Order"),
            // Required `name` left blank, but the optional `nickname` is filled,
            // so the row is NOT all-blank: it is kept and must error.
            p("contacts[0][name]", ""),
            p("contacts[0][nickname]", "Ally"),
        ];
        let cs = decode_nested_urlencoded::<Order, Contact>(&pairs).expect("parent decodes");
        assert!(!cs.is_valid());
        assert_eq!(cs.rows().len(), 1);
        // The kept row surfaces the required-field error under its combined key.
        assert!(!cs.errors_for("contacts[0].name").is_empty());
        // Raw values retained for re-render.
        assert_eq!(cs.rows()[0].value("nickname"), Some("Ally"));
        assert!(cs.into_valid().is_err());
    }

    /// A fully filled row is still bound — reject-all-blank does not regress the
    /// happy path when every submitted row carries real values.
    #[test]
    fn fully_filled_row_is_still_bound() {
        let pairs = vec![
            p("name", "Order"),
            p("contacts[0][name]", "Carol"),
            p("contacts[0][nickname]", "Caz"),
        ];
        let cs = decode_nested_urlencoded::<Order, Contact>(&pairs).expect("parent decodes");
        assert!(cs.is_valid());
        assert_eq!(cs.rows().len(), 1);

        let (_order, contacts) = cs.into_valid().unwrap_or_else(|_| panic!("valid"));
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].name, "Carol");
        assert_eq!(contacts[0].nickname.as_deref(), Some("Caz"));
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

    // ── blank (non-validating) constructor ─────────────────────────

    #[test]
    fn blank_changeset_is_valid_with_no_rows_or_errors() {
        // The initial `new`-page path: the required parent `name` is still
        // empty, but `blank` does NOT validate, so nothing errors before the
        // user types (unlike `decode_nested_urlencoded`).
        let cs = NestedChangeset::<Order, LineItem>::blank(Order {
            name: String::new(),
        });
        assert!(cs.is_valid());
        assert!(cs.errors_for("name").is_empty());
        assert!(cs.rows().is_empty());

        let (order, items) = cs
            .into_valid()
            .unwrap_or_else(|_| panic!("blank changeset is valid"));
        assert_eq!(order.name, "");
        assert!(items.is_empty());
    }

    #[test]
    fn decoded_empty_parent_errors_unlike_blank() {
        // Contrast the POST decode path: it DOES validate, so the same empty
        // required parent surfaces an error — exactly what `blank` avoids on the
        // initial GET render.
        let cs =
            decode_nested_urlencoded::<Order, LineItem>(&[p("name", "")]).expect("parent decodes");
        assert!(!cs.is_valid());
        assert!(!cs.errors_for("name").is_empty());
    }
}

#[cfg(all(test, feature = "maud"))]
mod maud_tests {
    use super::*;

    #[derive(serde::Serialize, serde::Deserialize, validator::Validate)]
    struct Order {
        #[validate(length(min = 1, message = "name required"))]
        name: String,
    }

    #[derive(serde::Serialize, serde::Deserialize, validator::Validate)]
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

    /// Extract the full `<...>` tag containing the given `name="…"` attribute,
    /// so a per-input attribute assertion isn't fooled by a sibling row's tag.
    fn tag_with_name(html: &str, name: &str) -> String {
        let needle = format!(r#"name="{name}""#);
        let at = html
            .find(&needle)
            .unwrap_or_else(|| panic!("missing {name} in {html}"));
        let open = html[..at].rfind('<').expect("tag has an opening '<'");
        let close = html[at..].find('>').expect("tag has a closing '>'") + at;
        html[open..=close].to_string()
    }

    /// Fix 1: a required-variant input on a submitted row carries the client-side
    /// `required`/`aria-required` constraint, but the auto-appended blank template
    /// row (`row: None`) omits it so the form stays submittable with a trailing
    /// blank row present (server-side validation still enforces required-ness).
    #[test]
    fn blank_template_row_omits_client_required_but_submitted_row_keeps_it() {
        let cs = two_row_changeset();
        let opts = InputsForOptions::default();
        let html = inputs_for(&cs, &opts, render_row).into_string();

        // Submitted rows (indices 0, 1) keep the client-side constraint.
        let submitted = tag_with_name(&html, "items[0][sku]");
        assert!(submitted.contains(" required"), "{submitted}");
        assert!(submitted.contains(r#"aria-required="true""#), "{submitted}");

        // The appended blank template row (index 2) omits both, so it is
        // leaveable-empty and does not block submitting the form.
        let blank = tag_with_name(&html, "items[2][sku]");
        assert!(!blank.contains("required"), "{blank}");
        assert!(!blank.contains("aria-required"), "{blank}");
    }

    /// Fix (Codex P2, near the `required` gate): a submitted row the user ticked
    /// `_destroy` on must ALSO omit the client-side `required`/`aria-required`,
    /// exactly like a blank template row. Otherwise, in a 422 re-render where the
    /// user checked Remove on a row with an empty required field, the browser's
    /// constraint validation would block the next submit before the server can
    /// honor `_destroy`. A non-destroyed submitted row in the same render keeps
    /// the constraint.
    #[test]
    fn destroyed_row_omits_client_required_while_submitted_row_keeps_it() {
        // Row 0: an ordinary submitted row. Row 1: the user ticked Remove on a
        // row whose required `sku` is empty (it is retained because `quantity`
        // is non-blank, so the all-blank rejection does not drop it).
        let pairs = vec![
            p("name", "Order 1"),
            p("items[0][sku]", "A-1"),
            p("items[0][quantity]", "2"),
            p("items[1][sku]", ""),
            p("items[1][quantity]", "9"),
            p("items[1][_destroy]", "1"),
        ];
        let cs = decode_nested_urlencoded::<Order, LineItem>(&pairs).expect("parent decodes");
        assert!(cs.rows()[1].is_destroyed());
        let html = inputs_for(&cs, &InputsForOptions::default(), render_row).into_string();

        // The non-destroyed submitted row keeps the client-side constraint.
        let kept = tag_with_name(&html, "items[0][sku]");
        assert!(kept.contains(" required"), "{kept}");
        assert!(kept.contains(r#"aria-required="true""#), "{kept}");

        // The destroyed row omits both, exactly like a blank template row, so a
        // form carrying an empty required field on a to-be-removed row still
        // submits and the server can honor `_destroy`.
        let destroyed = tag_with_name(&html, "items[1][sku]");
        assert!(!destroyed.contains("required"), "{destroyed}");
        assert!(!destroyed.contains("aria-required"), "{destroyed}");
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

        // With URL: button serializes no form fields (`hx-params="none"`) and
        // uses a beforeend swap.
        let opts = InputsForOptions {
            add_row_url: Some("/orders/line-item-row".into()),
            ..InputsForOptions::default()
        };
        let html = inputs_for(&cs, &opts, render_row).into_string();
        assert!(html.contains("Add row"), "{html}");
        assert!(html.contains(r#"hx-params="none""#), "{html}");
        assert!(html.contains(r#"hx-swap="beforeend""#), "{html}");
        assert!(html.contains(r#"hx-get="/orders/line-item-row""#), "{html}");
        assert!(html.contains(r##"hx-target="#items-rows""##), "{html}");
    }

    /// Fix (Codex P2, near the Add-row button): the Add-row button sends a fresh
    /// unique `index` with each htmx request via `hx-vals` (`js:` form) computed
    /// from THIS container's rows (`max(existing data-index) + 1`), so repeated
    /// clicks never reuse an index and emit duplicate control names. The selector
    /// is scoped to the form's own container id so multiple nested forms on one
    /// page do not clash. The pre-existing htmx attrs stay intact.
    #[test]
    fn add_button_hx_vals_sends_fresh_container_scoped_index() {
        let cs = two_row_changeset();
        let opts = InputsForOptions {
            add_row_url: Some("/orders/line-item-row".into()),
            ..InputsForOptions::default()
        };
        let html = inputs_for(&cs, &opts, render_row).into_string();

        // Pre-existing htmx wiring is preserved.
        assert!(html.contains(r#"hx-get="/orders/line-item-row""#), "{html}");
        assert!(html.contains(r#"hx-swap="beforeend""#), "{html}");
        assert!(html.contains(r#"hx-params="none""#), "{html}");

        // A JS-computed `hx-vals` that sends a fresh `index`…
        assert!(html.contains("hx-vals="), "{html}");
        assert!(html.contains("Math.max(-1"), "{html}");
        // …referencing the `index` param and reading each row's `data-index`…
        assert!(html.contains("parseInt(e.dataset.index,10)"), "{html}");
        // …via a selector scoped to THIS form's container id.
        assert!(
            html.contains("#items-rows .nested-fields__row[data-index]"),
            "{html}"
        );
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

    // ── Fix 1: form_tag preserves the captured CSRF/submit-token fields ─

    /// `NestedChangesetForm::form_tag` must emit the CSRF hidden input under the
    /// **captured/configured** field name — NOT the standalone `form_tag`'s
    /// hardcoded `_csrf` — so an app with a custom `security.csrf.form_field`
    /// keeps CSRF parity across a nested re-render.
    #[test]
    fn form_tag_emits_custom_csrf_field_name_not_default() {
        let form = NestedChangesetForm::<Order, LineItem>::blank(
            Order {
                name: String::new(),
            },
            Some("tok-123".to_owned()),
        )
        .with_csrf_field("authenticity_token");

        let html = form
            .form_tag("/orders", "post", maud::html! {})
            .into_string();

        // The captured custom field name carries the token…
        assert!(
            html.contains(r#"name="authenticity_token" value="tok-123""#),
            "{html}"
        );
        // …and the hardcoded default the standalone `form_tag` would emit does not.
        assert!(!html.contains(r#"name="_csrf""#), "{html}");
        assert!(html.contains(r#"action="/orders""#), "{html}");
        assert!(html.contains(r#"method="post""#), "{html}");
    }

    /// When a submit token is captured, `form_tag` also emits it under its
    /// captured field name (alongside the custom CSRF field).
    #[test]
    fn form_tag_emits_captured_submit_token_field() {
        let form = NestedChangesetForm::<Order, LineItem> {
            changeset: NestedChangeset::blank(Order {
                name: String::new(),
            }),
            csrf_token: Some("tok".to_owned()),
            csrf_field: "authenticity_token".to_owned(),
            submit_token: Some("stok-9".to_owned()),
            submit_field: "_submit_token".to_owned(),
        };

        let html = form
            .form_tag("/orders", "post", maud::html! {})
            .into_string();

        assert!(
            html.contains(r#"name="authenticity_token" value="tok""#),
            "{html}"
        );
        assert!(
            html.contains(r#"name="_submit_token" value="stok-9""#),
            "{html}"
        );
        assert!(!html.contains(r#"name="_csrf""#), "{html}");
    }

    // ── Fix 2: blank render shows no premature parent errors ────────────

    /// The initial `new`-page render from `blank`: the required parent field is
    /// empty but un-validated, so no error message or `aria-invalid="true"`
    /// appears before the user types.
    #[test]
    fn blank_form_render_shows_no_parent_validation_error() {
        let form = NestedChangesetForm::<Order, LineItem>::blank(
            Order {
                name: String::new(),
            },
            Some("tok".to_owned()),
        );

        let html = form
            .form_tag(
                "/orders",
                "post",
                maud::html! {
                    (crate::form::required_text_input(&form.parent, "name", "Order name"))
                    (inputs_for(&*form, &InputsForOptions::default(), render_row))
                },
            )
            .into_string();

        assert!(!html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(!html.contains("name required"), "{html}");
    }

    /// Contrast: a re-render from a failed decode DOES surface the parent error
    /// and marks the field invalid — the exact behaviour `blank` suppresses on
    /// the initial GET render.
    #[test]
    fn decoded_invalid_parent_render_shows_error_unlike_blank() {
        let cs =
            decode_nested_urlencoded::<Order, LineItem>(&[p("name", "")]).expect("parent decodes");
        let form = NestedChangesetForm::from_changeset(cs);

        let html = form
            .form_tag(
                "/orders",
                "post",
                maud::html! {
                    (crate::form::required_text_input(&form.parent, "name", "Order name"))
                },
            )
            .into_string();

        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains("name required"), "{html}");
    }

    // ── Fix 3: blank forms can carry the current submit token ───────────

    /// A `blank` form given a submit token (and a custom submit field name) emits
    /// the hidden submit-token input on the initial GET render, so the FIRST
    /// submission is protected against double-submit — not just later 422
    /// re-renders.
    #[test]
    fn blank_form_with_submit_token_emits_hidden_submit_field() {
        let form = NestedChangesetForm::<Order, LineItem>::blank(
            Order {
                name: String::new(),
            },
            Some("csrf".to_owned()),
        )
        .with_submit_field("authenticity_submit")
        .with_submit_token(Some("tok".to_owned()));

        let html = form
            .form_tag("/orders", "post", maud::html! {})
            .into_string();

        assert!(
            html.contains(r#"name="authenticity_submit" value="tok""#),
            "{html}"
        );
    }

    /// Without supplying a submit token, a `blank` form emits NO submit-token
    /// hidden input — documenting the default and that the caller must supply the
    /// minted token on the initial GET to protect the first submit.
    #[test]
    fn blank_form_without_submit_token_omits_submit_field() {
        let form = NestedChangesetForm::<Order, LineItem>::blank(
            Order {
                name: String::new(),
            },
            Some("csrf".to_owned()),
        );

        let html = form
            .form_tag("/orders", "post", maud::html! {})
            .into_string();

        assert!(!html.contains("_submit_token"), "{html}");
        assert!(!html.contains(r#"name="_submit_token""#), "{html}");
    }
}
