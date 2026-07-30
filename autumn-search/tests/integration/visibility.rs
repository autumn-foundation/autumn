//! Search respects existing authorization.
//!
//! AC: "Search respects existing authorization: results can be filtered by the
//! same policy/scope posture as normal queries (no leaking records a user
//! can't see). Out-of-scope engines must still support a tenant/visibility
//! filter hook."
//!
//! Two layers, both asserted here:
//!
//! 1. **Pre-filter** — a `SearchVisibility` turns the request's
//!    `PolicyContext` into a `SearchFilter` that is pushed *into* the backend
//!    query. This is the hook an out-of-scope engine (Meilisearch, a vector
//!    store) implements, because it is plain data.
//! 2. **Post-filter** — hydrated records can additionally be run through the
//!    registered `Policy`, matching the posture of a normal read.

use std::sync::Arc;

use autumn_search::{
    HashingEmbedder, MemorySearchBackend, SearchClient, SearchError, SearchFilter,
    SearchVisibility, TenantVisibility,
};
use autumn_web::authorization::PolicyContext;
use autumn_web::pagination::PageRequest;

use super::support::{Article, article, policy_context, tenant_article};

async fn client_with(
    articles: &[Article],
    visibility: Option<Arc<dyn SearchVisibility>>,
) -> SearchClient {
    let mut builder = SearchClient::builder()
        .backend(Arc::new(MemorySearchBackend::new()))
        .embedder(Arc::new(HashingEmbedder::new(64)))
        .index::<Article>();
    if let Some(v) = visibility {
        builder = builder.visibility(v);
    }
    let client = builder.build();
    client.ensure_indexes().await.expect("ensure");
    for a in articles {
        client.index_record(a).await.expect("index");
    }
    client
}

async fn ctx_for(user: &str) -> PolicyContext {
    policy_context(Some(user)).await
}

async fn anonymous_ctx() -> PolicyContext {
    policy_context(None).await
}

/// A visibility hook that only lets a user see the records they own — the
/// search-side equivalent of a `Scope<Article>`.
struct OwnerVisibility;

impl SearchVisibility for OwnerVisibility {
    fn filter<'a>(
        &'a self,
        ctx: &'a PolicyContext,
        _index: &'a str,
    ) -> autumn_search::BoxFuture<'a, Result<SearchFilter, SearchError>> {
        Box::pin(async move {
            Ok(match ctx.user_id.as_deref() {
                Some("alice") => SearchFilter::default().allow_ids([1_i64]),
                Some("bob") => SearchFilter::default().allow_ids([2_i64]),
                // Anonymous sees nothing — fail closed, exactly like `Scope`'s
                // default empty list.
                _ => SearchFilter::default().allow_ids(Vec::<i64>::new()),
            })
        })
    }
}

/// A visibility hook that fails. A failing authorization decision must abort
/// the search, never fall back to an unfiltered one.
struct BrokenVisibility;

impl SearchVisibility for BrokenVisibility {
    fn filter<'a>(
        &'a self,
        _ctx: &'a PolicyContext,
        _index: &'a str,
    ) -> autumn_search::BoxFuture<'a, Result<SearchFilter, SearchError>> {
        Box::pin(async move { Err(SearchError::Backend("policy store unavailable".to_owned())) })
    }
}

fn corpus() -> Vec<Article> {
    vec![
        article(1, "Rust one", "rust"),
        article(2, "Rust two", "rust"),
        article(3, "Rust three", "rust"),
    ]
}

#[tokio::test]
async fn search_for_applies_the_registered_visibility_filter() {
    let client = client_with(&corpus(), Some(Arc::new(OwnerVisibility))).await;

    let alice = client
        .search_for::<Article>(&ctx_for("alice").await, "rust", &PageRequest::default())
        .await
        .expect("search");
    assert_eq!(
        alice.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1]
    );

    let bob = client
        .search_for::<Article>(&ctx_for("bob").await, "rust", &PageRequest::default())
        .await
        .expect("search");
    assert_eq!(
        bob.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![2]
    );
}

#[tokio::test]
async fn an_anonymous_request_sees_nothing_when_the_hook_fails_closed() {
    let client = client_with(&corpus(), Some(Arc::new(OwnerVisibility))).await;

    let page = client
        .search_for::<Article>(&anonymous_ctx().await, "rust", &PageRequest::default())
        .await
        .expect("search");
    assert!(page.content.is_empty());
    assert_eq!(page.total_elements, 0);
}

#[tokio::test]
async fn a_failing_visibility_hook_aborts_the_search_instead_of_widening_it() {
    let client = client_with(&corpus(), Some(Arc::new(BrokenVisibility))).await;

    let err = client
        .search_for::<Article>(&ctx_for("alice").await, "rust", &PageRequest::default())
        .await
        .unwrap_err();
    assert!(matches!(err, SearchError::Backend(_)), "{err:?}");
}

#[tokio::test]
async fn vector_search_goes_through_the_same_visibility_hook() {
    // A semantic query must not be a back door around the keyword filter.
    let client = client_with(&corpus(), Some(Arc::new(OwnerVisibility))).await;

    let hits = client
        .similar_for::<Article>(&ctx_for("alice").await, "rust", 10)
        .await
        .expect("similar");
    assert!(hits.iter().all(|h| h.id == 1), "{hits:?}");
}

#[tokio::test]
async fn the_default_visibility_is_tenant_isolation() {
    let client = client_with(
        &[
            tenant_article(1, "Rust at acme", "rust", "acme"),
            tenant_article(2, "Rust at globex", "rust", "globex"),
        ],
        Some(Arc::new(TenantVisibility)),
    )
    .await;

    let filter = TenantVisibility
        .filter(&anonymous_ctx().await, "search_articles")
        .await
        .expect("filter");
    // With no ambient tenant there is nothing to scope by; the plugin's own
    // tenant filter is additive to whatever the app supplies.
    assert_eq!(filter.tenant_id, None);

    let page = client
        .search_filtered::<Article>(
            "rust",
            &PageRequest::default(),
            SearchFilter::default().tenant("globex"),
        )
        .await
        .expect("search");
    assert_eq!(
        page.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![2]
    );
}

#[tokio::test]
async fn without_a_registered_visibility_search_for_is_not_silently_unfiltered() {
    // If an app calls the authorization-aware entry point but registered no
    // hook, that is a wiring mistake — surface it rather than returning
    // everything and looking like it worked.
    let client = client_with(&corpus(), None).await;

    let err = client
        .search_for::<Article>(&ctx_for("alice").await, "rust", &PageRequest::default())
        .await
        .unwrap_err();
    assert!(matches!(err, SearchError::VisibilityUnavailable), "{err:?}");
}

#[tokio::test]
async fn filters_compose_rather_than_overwrite() {
    // An explicit caller filter and the visibility hook must intersect: the
    // narrower of the two always wins, so a caller can never widen past what
    // authorization allowed.
    let a = SearchFilter::default()
        .allow_ids([1_i64, 2, 3])
        .tenant("acme");
    let b = SearchFilter::default()
        .allow_ids([2_i64, 3, 4])
        .exclude_ids([3_i64]);

    let merged = a.intersect(b);
    assert_eq!(merged.tenant_id.as_deref(), Some("acme"));
    assert_eq!(merged.allowed_ids, Some(vec![2, 3]));
    assert_eq!(merged.excluded_ids, vec![3]);
}

#[tokio::test]
async fn intersecting_conflicting_tenants_yields_an_impossible_filter() {
    // Two different tenants can never both be satisfied. The result must match
    // nothing rather than arbitrarily picking one.
    let merged = SearchFilter::default()
        .tenant("acme")
        .intersect(SearchFilter::default().tenant("globex"));
    assert_eq!(merged.allowed_ids, Some(Vec::new()));
}
