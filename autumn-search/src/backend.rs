//! The pluggable [`SearchBackend`] seam and the query/result vocabulary.
//!
//! The trait is deliberately **plain data in, plain data out**: an index
//! definition, documents, a filter, a page request. Nothing in the signature
//! assumes Postgres, so an external engine (Meilisearch, Tantivy, a vector
//! store) is a new `impl`, not a fork — and, crucially, an out-of-scope engine
//! still receives the [`SearchFilter`] verbatim, which is what keeps
//! authorization enforceable everywhere.
//!
//! # Adding operations without breaking implementors
//!
//! Every query/result struct is `#[non_exhaustive]` with builder methods, and
//! every trait method beyond the required core has a default implementation —
//! [`SearchBackend::capabilities`] reports a keyword-only backend, and the
//! vector methods return [`SearchError::Unsupported`]. New capabilities are
//! therefore additive.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use autumn_web::pagination::{Page, PageRequest};
use autumn_web::search::{IndexDefinition, SearchDocument};
use serde::{Deserialize, Serialize};

use crate::error::{SearchError, SearchResult};

/// Boxed future returned by the object-safe search traits.
///
/// `SearchBackend`, [`crate::Embedder`], [`crate::DocumentSource`] and
/// [`crate::SearchVisibility`] are all stored as `Arc<dyn …>`, so they cannot
/// use `impl Future`. This mirrors `autumn_web::authorization::Policy`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ── Documents ───────────────────────────────────────────────────────────────

/// A [`SearchDocument`] paired with its (optional) embedding.
///
/// The document comes from the model via `#[searchable]`; the embedding comes
/// from the app's [`crate::Embedder`]. Keeping them in one struct means a
/// backend writes both halves of a record in a single call, so a keyword index
/// and a vector index can never drift apart.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct IndexedDocument {
    /// The extracted record.
    pub document: SearchDocument,
    /// The embedding of [`SearchDocument::embed_text`], when one was produced.
    pub embedding: Option<Vec<f32>>,
}

impl IndexedDocument {
    /// Wrap a document with no embedding.
    #[must_use]
    pub const fn new(document: SearchDocument) -> Self {
        Self {
            document,
            embedding: None,
        }
    }

    /// Attach an embedding.
    #[must_use]
    pub fn with_embedding(mut self, embedding: Vec<f32>) -> Self {
        self.embedding = Some(embedding);
        self
    }

    /// Primary key of the underlying record.
    #[must_use]
    pub const fn id(&self) -> i64 {
        self.document.id
    }
}

// ── Filters (the authorization seam) ────────────────────────────────────────

/// Restrictions applied *inside* the backend query.
///
/// This is the "tenant/visibility filter hook" every engine must honour. It is
/// a required argument of the query methods rather than an optional
/// post-processing step, so a backend cannot accidentally return rows the
/// caller is not allowed to see: forgetting the filter is a visibly missing
/// field, not a silently skipped `if`.
///
/// Filters **intersect** ([`SearchFilter::intersect`]) rather than overwrite,
/// so a caller-supplied filter can only ever narrow what authorization allowed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SearchFilter {
    /// Only match documents owned by this tenant.
    pub tenant_id: Option<String>,
    /// When `Some`, only these record ids may match. `Some(vec![])` means
    /// "nothing is visible" — an empty allowlist is a real restriction, never
    /// "no restriction".
    pub allowed_ids: Option<Vec<i64>>,
    /// Record ids that must never match.
    pub excluded_ids: Vec<i64>,
    /// Exact-match predicates on indexed fields (plus the reserved
    /// `tenant_id` key). This is what carries an app's own scoping to an
    /// engine that has no SQL to join against.
    pub equals: BTreeMap<String, String>,
}

/// The reserved [`SearchFilter::equals`] key that maps onto
/// [`SearchDocument::tenant_id`] rather than an indexed field.
pub const TENANT_FILTER_KEY: &str = "tenant_id";

impl SearchFilter {
    /// Restrict to one tenant.
    #[must_use]
    pub fn tenant(mut self, tenant_id: impl Into<String>) -> Self {
        self.tenant_id = Some(tenant_id.into());
        self
    }

    /// Restrict to an explicit set of record ids.
    #[must_use]
    pub fn allow_ids(mut self, ids: impl IntoIterator<Item = i64>) -> Self {
        self.allowed_ids = Some(ids.into_iter().collect());
        self
    }

    /// Exclude specific record ids.
    #[must_use]
    pub fn exclude_ids(mut self, ids: impl IntoIterator<Item = i64>) -> Self {
        self.excluded_ids.extend(ids);
        self
    }

    /// Add an exact-match predicate on an indexed field.
    ///
    /// `field` must name a field the index declares (or the reserved
    /// [`TENANT_FILTER_KEY`]). A key that does not is **not** an error here —
    /// it makes the filter match nothing, in both the in-Rust
    /// ([`SearchFilter::permits`]) and SQL paths. Backends must never
    /// interpolate an undeclared key into a query: the value is bound, but a
    /// key is an identifier position, so an unchecked one would let request
    /// text terminate the predicate and `OR` away the restrictions before it.
    #[must_use]
    pub fn equals(mut self, field: impl Into<String>, value: impl Into<String>) -> Self {
        self.equals.insert(field.into(), value.into());
        self
    }

    /// Whether this filter can never match anything.
    ///
    /// Backends short-circuit on this so an impossible filter costs no query.
    #[must_use]
    pub fn matches_nothing(&self) -> bool {
        self.allowed_ids.as_ref().is_some_and(Vec::is_empty)
    }

    /// Combine two filters so that **both** must hold.
    ///
    /// Two incompatible constraints (different tenants, disjoint allowlists,
    /// conflicting `equals` on the same key) collapse to an impossible filter
    /// rather than arbitrarily picking a winner — the safe direction.
    #[must_use]
    pub fn intersect(mut self, other: Self) -> Self {
        let mut impossible = false;

        match (&self.tenant_id, &other.tenant_id) {
            (Some(a), Some(b)) if a != b => impossible = true,
            (None, Some(b)) => self.tenant_id = Some(b.clone()),
            _ => {}
        }

        self.allowed_ids = match (self.allowed_ids.take(), other.allowed_ids) {
            (Some(a), Some(b)) => Some(a.into_iter().filter(|id| b.contains(id)).collect()),
            (Some(a), None) => Some(a),
            (None, b) => b,
        };

        self.excluded_ids.extend(other.excluded_ids);
        self.excluded_ids.sort_unstable();
        self.excluded_ids.dedup();

        for (key, value) in other.equals {
            match self.equals.get(&key) {
                Some(existing) if *existing != value => impossible = true,
                _ => {
                    self.equals.insert(key, value);
                }
            }
        }

        if impossible {
            self.allowed_ids = Some(Vec::new());
        }
        self
    }

    /// Whether `document` satisfies this filter.
    ///
    /// Shared by every backend that filters in Rust (the in-memory backend and
    /// any engine whose driver cannot express a predicate), so the semantics
    /// are defined once.
    #[must_use]
    pub fn permits(&self, document: &SearchDocument) -> bool {
        if let Some(tenant) = &self.tenant_id
            && document.tenant_id.as_deref() != Some(tenant.as_str())
        {
            return false;
        }
        if let Some(allowed) = &self.allowed_ids
            && !allowed.contains(&document.id)
        {
            return false;
        }
        if self.excluded_ids.contains(&document.id) {
            return false;
        }
        for (field, expected) in &self.equals {
            let actual = if field == TENANT_FILTER_KEY {
                document.tenant_id.as_deref()
            } else {
                document.field(field)
            };
            if actual != Some(expected.as_str()) {
                return false;
            }
        }
        true
    }
}

// ── Queries ─────────────────────────────────────────────────────────────────

/// A ranked keyword query.
///
/// **Query semantics (the cross-backend contract).** `text` is a bag of words,
/// not a query language: every token must be present for a document to match,
/// and operator-looking input (`OR`, `field:`, `*`, quotes) is never
/// interpreted as syntax. That keeps results consistent across engines and
/// makes a hostile query string structurally incapable of widening the result
/// set. The Postgres backend implements this with `plainto_tsquery`, which is
/// parameterized and cannot be injected into.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct KeywordQuery {
    /// Raw user query text.
    pub text: String,
    /// Restrictions applied inside the backend query.
    pub filter: SearchFilter,
    /// Page number and size.
    pub page: PageRequest,
}

impl KeywordQuery {
    /// Build a keyword query.
    #[must_use]
    pub fn new(text: impl Into<String>, page: PageRequest) -> Self {
        Self {
            text: text.into(),
            filter: SearchFilter::default(),
            page,
        }
    }

    /// Apply a filter (replacing any previously set filter).
    #[must_use]
    pub fn filter(mut self, filter: SearchFilter) -> Self {
        self.filter = filter;
        self
    }
}

/// A vector / k-NN similarity query.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct VectorQuery {
    /// The query embedding.
    pub vector: Vec<f32>,
    /// Maximum neighbours to return.
    pub limit: usize,
    /// Restrictions applied inside the backend query.
    pub filter: SearchFilter,
    /// Drop neighbours whose cosine similarity is below this.
    pub min_score: Option<f32>,
}

impl VectorQuery {
    /// Build a vector query for the `limit` nearest neighbours.
    #[must_use]
    pub fn new(vector: Vec<f32>, limit: usize) -> Self {
        Self {
            vector,
            limit,
            filter: SearchFilter::default(),
            min_score: None,
        }
    }

    /// Apply a filter (replacing any previously set filter).
    #[must_use]
    pub fn filter(mut self, filter: SearchFilter) -> Self {
        self.filter = filter;
        self
    }

    /// Require at least this cosine similarity.
    #[must_use]
    pub const fn min_score(mut self, min_score: f32) -> Self {
        self.min_score = Some(min_score);
        self
    }
}

// ── Results ─────────────────────────────────────────────────────────────────

/// One ranked result.
///
/// A hit carries the *identity* of a record, never its contents: hydration is
/// the app's job (see `SearchClient::search_hydrated`), which keeps the index
/// from becoming a second, staler copy of the database that can leak columns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SearchHit {
    /// Index the hit came from.
    pub index: String,
    /// Primary key of the matching record.
    pub id: i64,
    /// Backend-defined relevance score; higher is better, and comparable only
    /// within a single result set.
    pub score: f32,
}

impl SearchHit {
    /// Construct a hit.
    #[must_use]
    pub fn new(index: impl Into<String>, id: i64, score: f32) -> Self {
        Self {
            index: index.into(),
            id,
            score,
        }
    }
}

/// What a backend can do, so callers can degrade deliberately.
///
/// Four independent yes/no capabilities. `clippy::struct_excessive_bools`
/// suggests enums, but each of these is genuinely orthogonal — an engine can
/// support any subset — so a flag set is the honest shape.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackendCapabilities {
    /// Ranked keyword search.
    pub keyword: bool,
    /// Vector / k-NN search.
    pub vector: bool,
    /// Per-field weighting (`#[searchable(weight = "A")]`).
    pub weighted_fields: bool,
    /// Reading a stored embedding back (`similar_to`).
    pub embedding_readback: bool,
}

impl Default for BackendCapabilities {
    fn default() -> Self {
        Self {
            keyword: true,
            vector: false,
            weighted_fields: false,
            embedding_readback: false,
        }
    }
}

impl BackendCapabilities {
    /// Declare vector / k-NN support.
    #[must_use]
    pub const fn with_vector(mut self, supported: bool) -> Self {
        self.vector = supported;
        self
    }

    /// Declare per-field weighting support.
    #[must_use]
    pub const fn with_weighted_fields(mut self, supported: bool) -> Self {
        self.weighted_fields = supported;
        self
    }

    /// Declare stored-embedding read-back support (`similar_to`).
    #[must_use]
    pub const fn with_embedding_readback(mut self, supported: bool) -> Self {
        self.embedding_readback = supported;
        self
    }

    /// Declare keyword-search support. Backends that cannot answer a keyword
    /// query at all set this `false`.
    #[must_use]
    pub const fn with_keyword(mut self, supported: bool) -> Self {
        self.keyword = supported;
        self
    }
}

// ── The trait ───────────────────────────────────────────────────────────────

/// A search engine `autumn-search` can drive.
///
/// Implement this to add an engine. The three write methods plus
/// [`keyword_search`](SearchBackend::keyword_search) are required; vector
/// operations default to [`SearchError::Unsupported`] so a keyword-only engine
/// is a complete, honest implementation.
pub trait SearchBackend: Send + Sync + 'static {
    /// Stable backend name, used in errors and logs.
    fn name(&self) -> &'static str;

    /// What this backend supports.
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::default()
    }

    /// Create or migrate the storage for `definition`. Must be idempotent —
    /// it runs on every boot — and must reject a definition that fails
    /// [`IndexDefinition::validate`].
    fn ensure_index<'a>(
        &'a self,
        definition: &'a IndexDefinition,
    ) -> BoxFuture<'a, SearchResult<()>>;

    /// Upsert `documents`, keyed by `(index, id)`. Must be idempotent: the
    /// reindex job delivers at least once.
    fn index<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        documents: &'a [IndexedDocument],
    ) -> BoxFuture<'a, SearchResult<()>>;

    /// Remove documents by record id. Removing an absent id is a no-op, not an
    /// error — deletes are replayed.
    fn delete<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        ids: &'a [i64],
    ) -> BoxFuture<'a, SearchResult<()>>;

    /// Remove every document in the index.
    fn clear<'a>(&'a self, definition: &'a IndexDefinition) -> BoxFuture<'a, SearchResult<()>>;

    /// Ranked, paginated keyword search. See [`KeywordQuery`] for the query
    /// semantics every backend must implement.
    fn keyword_search<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        query: &'a KeywordQuery,
    ) -> BoxFuture<'a, SearchResult<Page<SearchHit>>>;

    /// k-NN search over the embedded field.
    fn vector_search<'a>(
        &'a self,
        _definition: &'a IndexDefinition,
        _query: &'a VectorQuery,
    ) -> BoxFuture<'a, SearchResult<Vec<SearchHit>>> {
        let name = self.name();
        Box::pin(async move {
            Err(SearchError::Unsupported {
                backend: name,
                operation: "vector search",
            })
        })
    }

    /// Read back the stored embedding for one record, so "find records like
    /// *this* one" needs no re-embedding.
    ///
    /// `filter` is **not** optional: this read is a query like any other, and
    /// the seed record is caller-supplied. Returning an embedding the caller
    /// may not see would be a cross-tenant inference channel — the caller
    /// could rank their *own* probe documents against a record they cannot
    /// read. Implementations MUST return `None` for a record the filter
    /// excludes, exactly as if it were not indexed.
    fn embedding<'a>(
        &'a self,
        _definition: &'a IndexDefinition,
        _id: i64,
        _filter: &'a SearchFilter,
    ) -> BoxFuture<'a, SearchResult<Option<Vec<f32>>>> {
        let name = self.name();
        Box::pin(async move {
            Err(SearchError::Unsupported {
                backend: name,
                operation: "embedding read-back",
            })
        })
    }
}

/// Build an empty page for `request` — the fail-closed result every backend
/// returns for a blank query or an impossible filter.
#[must_use]
pub fn empty_page(request: &PageRequest) -> Page<SearchHit> {
    Page::new(Vec::new(), 0, request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: i64) -> SearchDocument {
        SearchDocument::new("articles", id).with_field("title", 'A', "Hello")
    }

    #[test]
    fn an_empty_filter_permits_everything() {
        assert!(SearchFilter::default().permits(&doc(1)));
        assert!(!SearchFilter::default().matches_nothing());
    }

    #[test]
    fn an_empty_allowlist_is_a_restriction_not_an_absence() {
        let filter = SearchFilter::default().allow_ids(Vec::<i64>::new());
        assert!(filter.matches_nothing());
        assert!(!filter.permits(&doc(1)));
    }

    #[test]
    fn tenant_filter_requires_an_exact_owner() {
        let filter = SearchFilter::default().tenant("acme");
        assert!(!filter.permits(&doc(1)), "an untenanted doc is not acme's");
        assert!(filter.permits(&doc(1).with_tenant("acme")));
        assert!(!filter.permits(&doc(1).with_tenant("globex")));
    }

    #[test]
    fn excluded_ids_win_over_the_allowlist() {
        let filter = SearchFilter::default()
            .allow_ids([1_i64])
            .exclude_ids([1_i64]);
        assert!(!filter.permits(&doc(1)));
    }

    #[test]
    fn equals_matches_indexed_fields_and_the_reserved_tenant_key() {
        let filter = SearchFilter::default().equals("title", "Hello");
        assert!(filter.permits(&doc(1)));
        assert!(
            !SearchFilter::default()
                .equals("title", "Nope")
                .permits(&doc(1))
        );
        assert!(
            SearchFilter::default()
                .equals(TENANT_FILTER_KEY, "acme")
                .permits(&doc(1).with_tenant("acme"))
        );
    }

    #[test]
    fn equals_on_a_field_the_document_lacks_never_matches() {
        assert!(
            !SearchFilter::default()
                .equals("missing", "x")
                .permits(&doc(1))
        );
    }

    #[test]
    fn intersect_narrows_and_never_widens() {
        let merged = SearchFilter::default()
            .allow_ids([1_i64, 2, 3])
            .intersect(SearchFilter::default().allow_ids([2_i64, 4]));
        assert_eq!(merged.allowed_ids, Some(vec![2]));

        // An unconstrained side must not erase the constrained one.
        let merged = SearchFilter::default()
            .allow_ids([1_i64])
            .intersect(SearchFilter::default());
        assert_eq!(merged.allowed_ids, Some(vec![1]));

        let merged = SearchFilter::default().intersect(SearchFilter::default().allow_ids([1_i64]));
        assert_eq!(merged.allowed_ids, Some(vec![1]));
    }

    #[test]
    fn intersect_collapses_conflicting_constraints_to_nothing() {
        let merged = SearchFilter::default()
            .tenant("acme")
            .intersect(SearchFilter::default().tenant("globex"));
        assert!(merged.matches_nothing());

        let merged = SearchFilter::default()
            .equals("state", "published")
            .intersect(SearchFilter::default().equals("state", "draft"));
        assert!(merged.matches_nothing());
    }

    #[test]
    fn intersect_unions_and_dedups_exclusions() {
        let merged = SearchFilter::default()
            .exclude_ids([1_i64, 2])
            .intersect(SearchFilter::default().exclude_ids([2_i64, 3]));
        assert_eq!(merged.excluded_ids, vec![1, 2, 3]);
    }

    #[test]
    fn default_capabilities_claim_only_keyword_search() {
        let caps = BackendCapabilities::default();
        assert!(caps.keyword);
        assert!(!caps.vector);
        assert!(!caps.weighted_fields);
        assert!(!caps.embedding_readback);
    }

    #[test]
    fn capabilities_are_declarable_from_outside_this_crate() {
        // `#[non_exhaustive]` forbids a struct literal downstream, so without
        // these builders a third-party backend could only ever report the
        // keyword-only default — it could never advertise vector support.
        let caps = BackendCapabilities::default()
            .with_vector(true)
            .with_weighted_fields(true)
            .with_embedding_readback(true);
        assert!(caps.keyword);
        assert!(caps.vector);
        assert!(caps.weighted_fields);
        assert!(caps.embedding_readback);

        assert!(!BackendCapabilities::default().with_keyword(false).keyword);
    }

    #[test]
    fn empty_page_reports_a_zero_total() {
        let page = empty_page(&PageRequest::default());
        assert!(page.content.is_empty());
        assert_eq!(page.total_elements, 0);
    }
}
