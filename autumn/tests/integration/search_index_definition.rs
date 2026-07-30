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
fn index_definition_validates_its_own_identifiers() {
    // Every identifier the backend interpolates into SQL comes from here, so
    // the definition validates itself rather than trusting the caller.
    assert!(Article::index_definition().validate().is_ok());

    let mut bad = Article::index_definition();
    bad.name = "articles\"; DROP TABLE users; --";
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
