//! Shared fixtures for the `autumn-search` suites.

#![allow(dead_code)]

use autumn_search::{IndexDefinition, IndexedDocument, MemorySearchBackend, SearchBackend as _};
use autumn_web::search::{SearchDocument, SearchIndexed as _};

diesel::table! {
    search_articles (id) {
        id -> Int8,
        title -> Text,
        body -> Text,
    }
}

/// The canonical searchable model: two weighted keyword fields and a
/// `#[searchable(embed)]` body for vector search.
///
/// Deliberately **not** tenant-scoped — it has no `tenant_id` column — so the
/// suites that are not about tenancy can query it without establishing a
/// tenant scope. Tenancy has its own model below.
#[autumn_web::model(table = "search_articles")]
#[searchable(language = "english")]
pub struct Article {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
    #[searchable(weight = "B", embed)]
    pub body: String,
}

diesel::table! {
    search_tenant_articles (id) {
        id -> Int8,
        title -> Text,
        body -> Text,
        tenant_id -> Nullable<Text>,
    }
}

/// A searchable model that **is** tenant-scoped: `#[model]` sees the
/// `tenant_id` column, carries it into every document, and marks the index
/// tenant-scoped so a query with no tenant context is refused rather than run
/// across every tenant.
#[autumn_web::model(table = "search_tenant_articles")]
#[searchable(language = "english")]
pub struct TenantArticle {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
    #[searchable(weight = "B", embed)]
    pub body: String,
    pub tenant_id: Option<String>,
}

diesel::table! {
    search_notes (note_id) {
        note_id -> Int8,
        // A legacy column that is NOT the key and is not part of the model.
        // Its values deliberately disagree with `note_id` below, so a source
        // reader that assumes `id` produces wrong answers rather than an
        // error — see `Note`.
        id -> Int8,
        title -> Text,
        body -> Text,
    }
}

/// A searchable model whose primary key is **not** called `id`, on a table
/// that happens to have an `id` column anyway.
///
/// The combination is what makes this dangerous rather than merely broken.
/// `#[model]`'s bulk-upsert path names `<table>::id`, so a table with no `id`
/// column at all fails to compile — loud, and nobody ships it. A table that
/// *does* have a leftover `id` compiles fine, and then a source reader that
/// assumes `id` silently keys every backfilled document off the wrong column:
/// the sync hooks write documents keyed on `note_id`, the backfill writes
/// documents keyed on `id`, and the index quietly holds two of everything with
/// no error anywhere. The key column has to come from the model.
#[autumn_web::model(table = "search_notes")]
#[searchable(language = "english")]
// `note_id` is the point of the fixture — the column name has to stay.
#[allow(clippy::struct_field_names)]
pub struct Note {
    #[id]
    pub note_id: i64,
    /// The leftover column. Ordinary data, not a key, and not searchable.
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
    #[searchable(weight = "B", embed)]
    pub body: String,
}

/// Build a note keyed on `note_id`, with the decoy `id` offset by 100 so
/// reading the wrong column is unmistakable.
pub fn note(note_id: i64, title: &str, body: &str) -> Note {
    Note {
        note_id,
        id: note_id + 100,
        title: title.to_owned(),
        body: body.to_owned(),
    }
}

/// Build an article without touching a database.
pub fn article(id: i64, title: &str, body: &str) -> Article {
    Article {
        id,
        title: title.to_owned(),
        body: body.to_owned(),
    }
}

/// Build a tenant-owned article.
pub fn tenant_article(id: i64, title: &str, body: &str, tenant: &str) -> TenantArticle {
    TenantArticle {
        id,
        title: title.to_owned(),
        body: body.to_owned(),
        tenant_id: Some(tenant.to_owned()),
    }
}

/// A tenant-scoped model's record with no owning tenant.
pub fn untenanted_article(id: i64, title: &str, body: &str) -> TenantArticle {
    TenantArticle {
        id,
        title: title.to_owned(),
        body: body.to_owned(),
        tenant_id: None,
    }
}

/// Run `future` inside `tenant`'s scope, exactly as the tenancy layer would.
pub async fn with_tenant<T>(tenant: &str, future: impl Future<Output = T>) -> T {
    autumn_web::tenancy::CURRENT_TENANT
        .scope(Some(tenant.to_owned()), future)
        .await
}

/// A `MemorySearchBackend` with `Article`'s index created and `articles`
/// indexed (no embeddings).
pub async fn seeded_backend(articles: &[Article]) -> MemorySearchBackend {
    seeded_backend_with(
        Article::index_definition(),
        articles.iter().map(Article::search_document).collect(),
    )
    .await
}

/// A `MemorySearchBackend` seeded with `TenantArticle` documents.
pub async fn seeded_tenant_backend(articles: &[TenantArticle]) -> MemorySearchBackend {
    seeded_backend_with(
        TenantArticle::index_definition(),
        articles
            .iter()
            .map(TenantArticle::search_document)
            .collect(),
    )
    .await
}

async fn seeded_backend_with(
    definition: IndexDefinition,
    documents: Vec<SearchDocument>,
) -> MemorySearchBackend {
    let backend = MemorySearchBackend::new();
    backend
        .ensure_index(&definition)
        .await
        .expect("ensure_index");
    let documents: Vec<IndexedDocument> = documents.into_iter().map(IndexedDocument::new).collect();
    backend.index(&definition, &documents).await.expect("index");
    backend
}

/// A `PolicyContext` for `user`, built the same way a hand-rolled policy unit
/// test would (`PolicyContext::from_session`), so the visibility hook sees the
/// exact shape a request produces.
pub async fn policy_context(user: Option<&str>) -> autumn_web::authorization::PolicyContext {
    use std::collections::HashMap;

    let mut data = HashMap::new();
    if let Some(user) = user {
        data.insert("user_id".to_owned(), user.to_owned());
    }
    let session = autumn_web::session::Session::new_for_test("test-session".to_owned(), data);
    autumn_web::authorization::PolicyContext::from_session(&session, "user_id").await
}
