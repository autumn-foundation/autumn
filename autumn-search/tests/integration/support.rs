//! Shared fixtures for the `autumn-search` suites.

#![allow(dead_code)]

use autumn_search::{IndexedDocument, MemorySearchBackend, SearchBackend as _};
use autumn_web::search::SearchIndexed as _;

diesel::table! {
    search_articles (id) {
        id -> Int8,
        title -> Text,
        body -> Text,
        tenant_id -> Nullable<Text>,
    }
}

/// The canonical searchable model: two weighted keyword fields and a
/// `#[searchable(embed)]` body for vector search.
#[autumn_web::model(table = "search_articles")]
#[searchable(language = "english")]
pub struct Article {
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
        tenant_id: None,
    }
}

/// Build an article owned by `tenant`.
pub fn tenant_article(id: i64, title: &str, body: &str, tenant: &str) -> Article {
    Article {
        id,
        title: title.to_owned(),
        body: body.to_owned(),
        tenant_id: Some(tenant.to_owned()),
    }
}

/// A `MemorySearchBackend` with `Article`'s index created and `articles`
/// indexed (no embeddings).
pub async fn seeded_backend(articles: &[Article]) -> MemorySearchBackend {
    let backend = MemorySearchBackend::new();
    let def = Article::index_definition();
    backend.ensure_index(&def).await.expect("ensure_index");
    let docs: Vec<IndexedDocument> = articles
        .iter()
        .map(|a| IndexedDocument::new(a.search_document()))
        .collect();
    backend.index(&def, &docs).await.expect("index");
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
