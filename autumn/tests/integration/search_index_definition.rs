//! `#[searchable]` → engine-agnostic index definition (issue #1191).
//!
//! Issue #842 gave `#[model] #[searchable]` a Postgres `tsvector` column and a
//! `#[repository(searchable)]` `search()` method. #1191 needs the *same*
//! declaration to also produce an engine-agnostic **index definition** and a
//! per-record **document**, so a pluggable `SearchBackend` (Postgres+pgvector
//! today, Meilisearch/Tantivy/a vector store tomorrow) can index a model with
//! no hand-written glue.
//!
//! These tests pin that contract: the attribute is the single source of truth
//! for the index name, the language dictionary, the weighted field list, and
//! the optional embedded field.

#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use autumn_web::search::{SearchIndexed, SearchTextValue};

diesel::table! {
    search_def_articles (id) {
        id -> Int8,
        title -> Text,
        body -> Text,
        summary -> Nullable<Text>,
        views -> Int4,
    }
}

#[autumn_web::model(table = "search_def_articles")]
#[searchable(language = "english")]
pub struct Article {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
    #[searchable(weight = "B", embed)]
    pub body: String,
    #[searchable(weight = "C")]
    pub summary: Option<String>,
    pub views: i32,
}

diesel::table! {
    search_def_notes (id) {
        id -> Int8,
        memo -> Text,
    }
}

diesel::table! {
    search_def_denorm (id) {
        id -> Int8,
        body -> Text,
        tenant_id -> Text,
    }
}

/// A searchable model whose `tenant_id` is **denormalized data**, not a scope:
/// the repository does not opt into `tenant_scoped`, so its finders return
/// rows across tenants — and the index must not be more restrictive than the
/// finders it mirrors.
#[autumn_web::model(table = "search_def_denorm")]
#[searchable]
pub struct DenormDoc {
    #[id]
    pub id: i64,
    #[searchable]
    pub body: String,
    pub tenant_id: String,
}

#[autumn_web::repository(DenormDoc, table = "search_def_denorm")]
pub trait DenormDocRepository {}

diesel::table! {
    search_def_scoped (id) {
        id -> Int8,
        body -> Text,
        tenant_id -> Text,
    }
}

/// The same shape with a genuinely tenant-scoped repository.
#[autumn_web::model(table = "search_def_scoped")]
#[searchable]
pub struct ScopedDoc {
    #[id]
    pub id: i64,
    #[searchable]
    pub body: String,
    pub tenant_id: String,
}

#[autumn_web::repository(ScopedDoc, table = "search_def_scoped", tenant_scoped)]
pub trait ScopedDocRepository {}

/// A `#[searchable]` model with **no** `embed` field: vector search is opt-in,
/// keyword search still works.
#[autumn_web::model(table = "search_def_notes")]
#[searchable]
pub struct Note {
    #[id]
    pub id: i64,
    #[searchable]
    pub memo: String,
}

diesel::table! {
    search_def_audited (id) {
        id -> Int8,
        body -> Text,
        deleted_at -> Nullable<Timestamp>,
    }
}

diesel::table! {
    search_def_archived (id) {
        id -> Int8,
        body -> Text,
        deleted_at -> Nullable<Timestamp>,
    }
}

/// A searchable model whose `deleted_at` is **audit history**, not a
/// tombstone: the repository does not opt into `soft_delete`, so its finders
/// return those rows — and so must the search index.
#[autumn_web::model(table = "search_def_audited")]
#[searchable]
pub struct AuditedDoc {
    #[id]
    pub id: i64,
    #[searchable]
    pub body: String,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(AuditedDoc, table = "search_def_audited")]
pub trait AuditedDocRepository {}

/// The same shape, but genuinely soft-deleted. Its finders hide the rows, so
/// the index must too.
#[autumn_web::model(table = "search_def_archived")]
#[searchable]
pub struct ArchivedDoc {
    #[id]
    pub id: i64,
    #[searchable]
    pub body: String,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(ArchivedDoc, table = "search_def_archived", soft_delete)]
pub trait ArchivedDocRepository {}

diesel::table! {
    search_def_memos (memo_id) {
        memo_id -> Int8,
        // A leftover column that is not the key (see `Memo`).
        id -> Int8,
        memo -> Text,
    }
}

/// A `#[searchable]` model whose primary key is **not** named `id`.
///
/// Nothing about `#[searchable]` requires the conventional name, so the index
/// definition has to carry the real one: a backend that rebuilds documents by
/// reading the source table selects, filters, and paginates on that column,
/// and against a table that still has an unrelated `id` column that reads the
/// wrong values with no error at all.
#[autumn_web::model(table = "search_def_memos")]
#[searchable]
// `memo_id` is the point of the fixture — the column name has to stay.
#[allow(clippy::struct_field_names)]
pub struct Memo {
    #[id]
    pub memo_id: i64,
    /// The leftover column. Ordinary data, not a key.
    pub id: i64,
    #[searchable]
    pub memo: String,
}

fn article() -> Article {
    Article {
        id: 7,
        title: "Rust web frameworks".to_owned(),
        body: "Autumn is a batteries-included Rust web framework.".to_owned(),
        summary: Some("A short summary".to_owned()),
        views: 3,
    }
}

// ── Index definition ────────────────────────────────────────────────────────

#[test]
fn index_definition_is_derived_from_the_searchable_attribute() {
    let def = Article::index_definition();

    assert_eq!(def.name, "search_def_articles");
    assert_eq!(def.language, "english");
    assert_eq!(def.embed_field, Some("body"));
    assert_eq!(
        def.fields
            .iter()
            .map(|f| (f.name, f.weight))
            .collect::<Vec<_>>(),
        vec![("title", 'A'), ("body", 'B'), ("summary", 'C')],
    );
}

#[test]
fn index_definition_without_embed_field_disables_vector_search() {
    let def = Note::index_definition();

    assert_eq!(def.name, "search_def_notes");
    // The model-level `#[searchable]` with no `language` keeps the #842 default.
    assert_eq!(def.language, "simple");
    assert_eq!(def.embed_field, None);
    assert!(!def.supports_vector_search());
    assert_eq!(
        def.fields
            .iter()
            .map(|f| (f.name, f.weight))
            .collect::<Vec<_>>(),
        // A bare `#[searchable]` field keeps #842's lowest weight.
        vec![("memo", 'D')],
    );
}

#[test]
fn index_definition_carries_the_models_real_key_column() {
    // The conventional case still reads `id`, so nothing that assumed it
    // changes behaviour…
    assert_eq!(Article::index_definition().key_column, "id");
    assert_eq!(Note::index_definition().key_column, "id");
    // …and a model keyed elsewhere says so, rather than leaving a backend to
    // guess wrong at runtime.
    assert_eq!(Memo::index_definition().key_column, "memo_id");
    let memo = Memo {
        memo_id: 9,
        id: 109,
        memo: "x".to_owned(),
    };
    assert_eq!(
        memo.search_id(),
        9,
        "the document keys off `#[id]`, not `id`"
    );
    assert_eq!(memo.search_document().id, 9);
}

#[test]
fn index_definition_carries_the_repositorys_soft_delete_semantics() {
    // A `deleted_at` COLUMN is not the question — the repository's opt-in is.
    // The framework explicitly supports `deleted_at` as audit history, and
    // those rows are still returned by finders. A source that inferred a
    // tombstone from the column would hide them from reindex and drop them
    // from a purging backfill, so the index would disagree with the app.
    assert!(
        !AuditedDoc::index_definition().soft_delete,
        "an audit `deleted_at` without `#[repository(soft_delete)]` is not a tombstone"
    );
    assert!(
        ArchivedDoc::index_definition().soft_delete,
        "`#[repository(soft_delete)]` must reach the index definition"
    );
    // A model with no `deleted_at` at all is trivially not soft-deleting.
    assert!(!Article::index_definition().soft_delete);
}

#[test]
fn index_definition_carries_the_repositorys_tenant_scoping() {
    // Same rule as soft-delete, and for the same reason: a `tenant_id` COLUMN
    // is not the question, the repository's opt-in is. A denormalized or audit
    // `tenant_id` on an unscoped repository has unscoped finders, so marking
    // its index tenant-scoped would make every search outside a tenant context
    // fail with `TenantContextMissing` and every search inside one silently
    // filter — neither of which matches what the app's own reads do.
    assert!(
        !DenormDoc::index_definition().tenant_scoped,
        "a `tenant_id` column without `#[repository(tenant_scoped)]` is not a scope"
    );
    assert!(
        ScopedDoc::index_definition().tenant_scoped,
        "`#[repository(tenant_scoped)]` must reach the index definition"
    );
    // A model with no `tenant_id` at all is trivially unscoped.
    assert!(!Note::index_definition().tenant_scoped);

    // The tenant is still carried into the document either way — it is what a
    // caller-supplied filter matches on, and dropping it would make an
    // explicit tenant filter silently match nothing.
    let doc = DenormDoc {
        id: 1,
        body: "x".to_owned(),
        tenant_id: "acme".to_owned(),
    }
    .search_document();
    assert_eq!(doc.tenant_id.as_deref(), Some("acme"));
}

#[test]
fn index_definition_validates_its_own_identifiers() {
    // Every identifier the backend interpolates into SQL comes from here, so
    // the definition validates itself rather than trusting the caller.
    assert!(Article::index_definition().validate().is_ok());

    let mut bad = Article::index_definition();
    bad.name = "articles\"; DROP TABLE users; --";
    assert!(bad.validate().is_err());

    // The key column is interpolated into the source query too, so it is held
    // to the same standard as the index and field names.
    let bad = Article::index_definition().with_key_column("id\"; DROP TABLE users; --");
    assert!(bad.validate().is_err());
}

// ── Document extraction ─────────────────────────────────────────────────────

#[test]
fn search_document_carries_every_declared_field_with_its_weight() {
    let doc = article().search_document();

    assert_eq!(doc.index, "search_def_articles");
    assert_eq!(doc.id, 7);
    assert_eq!(
        doc.fields
            .iter()
            .map(|f| (f.name, f.weight, f.value.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("title", 'A', "Rust web frameworks"),
            (
                "body",
                'B',
                "Autumn is a batteries-included Rust web framework."
            ),
            ("summary", 'C', "A short summary"),
        ],
    );
}

#[test]
fn search_document_exposes_the_embed_text_for_vector_indexing() {
    let doc = article().search_document();

    assert_eq!(
        doc.embed_text.as_deref(),
        Some("Autumn is a batteries-included Rust web framework."),
    );

    let note = Note {
        id: 1,
        memo: "nothing to embed".to_owned(),
    };
    assert_eq!(note.search_document().embed_text, None);
}

#[test]
fn a_none_optional_field_is_indexed_as_empty_not_dropped() {
    // Dropping the field would silently shift weights for backends that index
    // positionally; an absent value must index as an empty string instead.
    let mut record = article();
    record.summary = None;

    let doc = record.search_document();
    let summary = doc
        .fields
        .iter()
        .find(|f| f.name == "summary")
        .expect("summary field must still be present");
    assert_eq!(summary.value, "");
}

#[test]
fn document_text_concatenates_fields_in_weight_order() {
    let text = article().search_document().text();

    assert_eq!(
        text,
        "Rust web frameworks Autumn is a batteries-included Rust web framework. A short summary",
    );
}

// ── The text-extraction seam ────────────────────────────────────────────────

#[test]
fn search_text_value_covers_the_common_column_types() {
    assert_eq!("hello".to_owned().search_text_value(), "hello");
    assert_eq!("hello".search_text_value(), "hello");
    assert_eq!(Some("hi".to_owned()).search_text_value(), "hi");
    assert_eq!(None::<String>.search_text_value(), "");
    assert_eq!(42_i64.search_text_value(), "42");
    assert_eq!(true.search_text_value(), "true");
}
