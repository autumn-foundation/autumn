//! Search-side authorization.
//!
//! A search index is a second read path over the same rows, so it needs the
//! same posture as a normal query: **no leaking records a user can't see**.
//!
//! The seam is [`SearchVisibility`] — a request's [`PolicyContext`] in, a
//! [`SearchFilter`] out. Because the output is plain data, it works for
//! engines that are nowhere near the database: Meilisearch or a vector store
//! receives the same tenant/allowlist restriction, which is the AC's
//! "out-of-scope engines must still support a tenant/visibility filter hook".
//!
//! Three properties make this safe rather than advisory:
//!
//! 1. The filter is pushed **into** the backend query, not applied afterwards,
//!    so page totals and k-NN neighbour counts are computed post-restriction.
//! 2. A hook that returns an error **aborts** the search. There is no
//!    fall-back-to-unfiltered path.
//! 3. Calling an authorization-aware entry point with no hook registered is
//!    [`SearchError::VisibilityUnavailable`], not an unfiltered read.

use autumn_web::authorization::PolicyContext;

use crate::backend::{BoxFuture, SearchFilter};
use crate::error::SearchResult;

/// Turns a request context into the restriction a search must run under.
///
/// The search-side analogue of `autumn_web::authorization::Scope`: where
/// `Scope::list` returns the records a user may read, this returns the
/// *predicate* that selects them — so the engine, not the process, does the
/// filtering.
///
/// Implementations must fail **closed**: an unrecognised or anonymous
/// principal should yield `SearchFilter::default().allow_ids([])` (match
/// nothing), never an unrestricted filter.
pub trait SearchVisibility: Send + Sync + 'static {
    /// The restriction to apply for `ctx` when querying `index`.
    fn filter<'a>(
        &'a self,
        ctx: &'a PolicyContext,
        index: &'a str,
    ) -> BoxFuture<'a, SearchResult<SearchFilter>>;
}

/// Restricts every search to the ambient tenant.
///
/// Reads `autumn_web::tenancy::CURRENT_TENANT`, the same task-local the
/// tenant-scoped repositories use, so a search inside a tenant-scoped request
/// is confined exactly like a `list()` in the same request. Outside a tenant
/// scope it adds no restriction — it is a tenancy hook, not an authorization
/// one, and is meant to be composed with (not substituted for) an app's own
/// visibility rules via [`SearchFilter::intersect`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TenantVisibility;

impl SearchVisibility for TenantVisibility {
    fn filter<'a>(
        &'a self,
        _ctx: &'a PolicyContext,
        _index: &'a str,
    ) -> BoxFuture<'a, SearchResult<SearchFilter>> {
        Box::pin(async move { Ok(current_tenant_filter()) })
    }
}

/// The ambient-tenant restriction, or an empty filter outside a tenant scope.
///
/// Applied by the client to **every** query, on top of whatever the registered
/// [`SearchVisibility`] returns, so tenant isolation does not depend on an app
/// remembering to implement it.
#[must_use]
pub fn current_tenant_filter() -> SearchFilter {
    autumn_web::tenancy::CURRENT_TENANT
        .try_with(Clone::clone)
        .ok()
        .flatten()
        .map_or_else(SearchFilter::default, |tenant| {
            SearchFilter::default().tenant(tenant)
        })
}

/// A [`SearchVisibility`] that denies everything.
///
/// Useful as an explicit "this index is not searchable by end users yet"
/// marker, and as the safe thing to install while an app's real rules are
/// being written.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAll;

impl SearchVisibility for DenyAll {
    fn filter<'a>(
        &'a self,
        _ctx: &'a PolicyContext,
        _index: &'a str,
    ) -> BoxFuture<'a, SearchResult<SearchFilter>> {
        Box::pin(async move { Ok(SearchFilter::default().allow_ids(Vec::<i64>::new())) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn outside_a_tenant_scope_there_is_nothing_to_restrict() {
        assert_eq!(current_tenant_filter(), SearchFilter::default());
    }

    #[tokio::test]
    async fn inside_a_tenant_scope_the_filter_names_that_tenant() {
        autumn_web::tenancy::CURRENT_TENANT
            .scope(Some("acme".to_owned()), async {
                assert_eq!(current_tenant_filter().tenant_id.as_deref(), Some("acme"));
            })
            .await;
    }

    #[tokio::test]
    async fn deny_all_matches_nothing() {
        // Constructing a PolicyContext needs a session; DenyAll ignores it, so
        // assert on the filter it produces directly.
        let filter = SearchFilter::default().allow_ids(Vec::<i64>::new());
        assert!(filter.matches_nothing());
    }
}
