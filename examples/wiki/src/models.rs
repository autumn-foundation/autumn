use autumn_web::AutumnResult;
use diesel_async::{AsyncPgConnection, RunQueryDsl};

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
    // A runtime `#[state_machine]` combining a data-dependent guard with
    // per-edge *transition effects* (docs/guide/transition-effects.md). Each
    // `on = "method"` names a synchronous, in-transaction effect that fires when
    // its edge is taken: here it appends the audit `Revision`, so the audit row
    // and the status change commit — or roll back — together. The `guard` still
    // blocks publishing an empty page. See `transition_status` in
    // `src/routes/pages.rs` for the transactional call site.
    #[state_machine(transitions(
        draft -> published: guard = "can_publish", on = "record_publish_revision",
        published -> archived: on = "record_archive_revision",
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

    /// `draft -> published` transition effect. The `on = "record_publish_revision"`
    /// edge in the state machine calls this inside the transition's transaction
    /// (through the generated `transition_status_to_on_conn`), so the audit row
    /// written here is atomic with the status change.
    async fn record_publish_revision(&self, conn: &mut AsyncPgConnection) -> AutumnResult<()> {
        self.record_status_revision(conn, "published").await
    }

    /// `published -> archived` transition effect (see `record_publish_revision`).
    async fn record_archive_revision(&self, conn: &mut AsyncPgConnection) -> AutumnResult<()> {
        self.record_status_revision(conn, "archived").await
    }

    /// Shared effect body: append a `Revision` recording the status change.
    /// `self` is the pre-transition snapshot, so `self.status` is the *old*
    /// state and `to` is the state being entered. Returning `Err` here rolls the
    /// whole transition back.
    async fn record_status_revision(
        &self,
        conn: &mut AsyncPgConnection,
        to: &str,
    ) -> AutumnResult<()> {
        diesel::insert_into(revisions::table)
            .values(&NewRevision {
                page_id: self.id,
                op: "update".into(),
                title: self.title.clone(),
                body: self.body.clone(),
                status: to.to_string(),
                changed_by: None,
                summary: Some(format!("Status changed: {} → {to}", self.status)),
            })
            .execute(conn)
            .await?;
        Ok(())
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

#[cfg(test)]
mod state_machine_tests {
    use super::*;

    fn page_with(status: &str, body: &str) -> Page {
        Page {
            id: 1,
            title: "My Page".into(),
            slug: "my-page".into(),
            body: body.into(),
            status: status.into(),
            lock_version: 0,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        }
    }

    // Adding per-edge transition effects (`on = "..."`) leaves the pure
    // `#[state_machine]` validators byte-for-byte unchanged: the guarded
    // `draft -> published` edge and the unguarded `published -> archived` edge
    // still govern which transitions are legal, and the `can_publish` guard
    // still blocks publishing an empty page.
    #[test]
    fn pure_validators_still_govern_allowed_edges() {
        assert!(page_with("draft", "Some content").can_transition_status_to("published"));
        assert!(page_with("published", "Some content").can_transition_status_to("archived"));
        // Undeclared edge and rejected guard remain denied.
        assert!(!page_with("published", "Some content").can_transition_status_to("draft"));
        assert!(!page_with("draft", "").can_transition_status_to("published"));
    }

    // Compilation contract: declaring an effect makes the model gain the
    // connection-taking `transition_status_to_on_conn`, which validates the edge,
    // runs the synchronous `on` effect, and returns the new state string to
    // persist. Never called (no DB here) — its mere existence type-checks the
    // effect codegen end to end.
    #[allow(dead_code)]
    async fn _assert_on_conn_signature(
        page: &Page,
        conn: &mut AsyncPgConnection,
    ) -> AutumnResult<String> {
        page.transition_status_to_on_conn(conn, "published").await
    }
}
