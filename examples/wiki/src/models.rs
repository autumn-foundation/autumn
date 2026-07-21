use crate::schema::{api_credentials, collection_links, collections, pages, revisions};

/// A stored third-party API token, encrypted at rest (issue #805).
///
/// The `token` field is declared `#[encrypted]`, so it persists as an opaque
/// AES-256-GCM envelope on disk while staying a plain `String` in Rust:
/// `repo.find(id)?.token` is plaintext, and inserts take plaintext. Configure
/// the key once with `autumn credentials edit`:
///
/// ```toml
/// [active_record_encryption]
/// primary_key = "<64 hex chars from `openssl rand -hex 32`>"
/// ```
///
/// See `docs/guide/attribute-encryption.md` for the full workflow.
#[autumn_web::model]
pub struct ApiCredential {
    #[id]
    pub id: i64,
    pub label: String,
    #[encrypted]
    pub token: String,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

#[autumn_web::model]
#[searchable(language = "english")]
pub struct Page {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
    pub slug: String,
    #[searchable(weight = "B")]
    pub body: String,
    #[state_machine(transitions(
        draft -> published: "can_publish",
        published -> archived,
    ))]
    pub status: String,
    #[lock_version]
    pub lock_version: i32,
    #[default]
    pub created_at: chrono::NaiveDateTime,
    #[default]
    pub updated_at: chrono::NaiveDateTime,
}

impl Page {
    pub fn can_publish(&self) -> bool {
        !self.title.trim().is_empty() && !self.body.trim().is_empty()
    }
}

// ── Collections: a nested (`has_many`) master-detail form ──────────────────
//
// A `Collection` is the parent record; each `CollectionLink` is a child row.
// Both are bound, validated, and saved together through one
// `NestedChangesetForm<CollectionForm, LinkForm>` — see
// `docs/guide/nested-forms.md` and `routes/collections.rs`. The read models
// below are plain Diesel structs (like `Revision`); the `*Form` structs are the
// deserialize/validate shapes the nested form decodes a submission into.

/// The parent record, read from the database.
#[derive(Debug, Clone, diesel::Queryable, diesel::Selectable, serde::Serialize)]
#[diesel(table_name = collections)]
pub struct Collection {
    pub id: i64,
    pub title: String,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, diesel::Insertable)]
#[diesel(table_name = collections)]
pub struct NewCollection {
    pub title: String,
}

/// One child link row, read from the database.
#[derive(Debug, Clone, diesel::Queryable, diesel::Selectable, serde::Serialize)]
#[diesel(table_name = collection_links)]
pub struct CollectionLink {
    pub id: i64,
    pub collection_id: i64,
    pub label: String,
    pub url: String,
    pub position: i32,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, diesel::Insertable)]
#[diesel(table_name = collection_links)]
pub struct NewCollectionLink {
    pub collection_id: i64,
    pub label: String,
    pub url: String,
    pub position: i32,
}

/// Parent side of the nested form — the fields the `<form>` submits for the
/// collection itself. Deriving `Validate` runs the same rules on the initial
/// create and on every edit; a rejected submission re-renders the whole form
/// (parent + children) at `422` with each field preserved and an inline error.
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize, validator::Validate)]
pub struct CollectionForm {
    #[validate(length(min = 1, message = "Title is required"))]
    pub title: String,
}

/// One repeated child row: a labeled external link. `NestedChild::COLLECTION`
/// names the field group, so its inputs post as `links[0][label]`,
/// `links[0][url]`, `links[1][label]`, … and its per-row errors surface under
/// keys like `links[1].url`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, validator::Validate)]
pub struct LinkForm {
    #[validate(length(min = 1, message = "Label is required"))]
    pub label: String,
    #[validate(url(message = "Must be a valid URL (e.g. https://example.com)"))]
    pub url: String,
}

impl autumn_web::nested_form::NestedChild for LinkForm {
    const COLLECTION: &'static str = "links";
}

// Revision is manual — write-only from hooks, read-only from routes
#[derive(Debug, Clone, diesel::Queryable, diesel::Selectable, serde::Serialize)]
#[diesel(table_name = revisions)]
pub struct Revision {
    pub id: i64,
    pub page_id: i64,
    pub op: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub changed_by: Option<String>,
    pub summary: Option<String>,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, diesel::Insertable)]
#[diesel(table_name = revisions)]
pub struct NewRevision {
    pub page_id: i64,
    pub op: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub changed_by: Option<String>,
    pub summary: Option<String>,
}
