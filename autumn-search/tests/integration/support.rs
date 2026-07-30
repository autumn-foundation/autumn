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
