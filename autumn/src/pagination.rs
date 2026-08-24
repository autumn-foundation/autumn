//! Standardized pagination primitives.
//!
//! Autumn ships two complementary flavours of pagination:
//!
//! 1. **Offset pagination** ([`PageRequest`] / [`Page<T>`]) — classic
//!    `?page=N&size=M` with metadata (total elements, total pages).
//!    Best for stable, browse-style UIs.
//! 2. **Cursor pagination** ([`CursorRequest`] / [`CursorPage<T>`]) —
//!    keyset/seek pagination with an opaque `next_cursor` token. Best
//!    for real-time feeds and infinite scroll: O(1) page depth and
//!    zero duplicates under concurrent inserts.
//!
//! # Quick start (offset)
//!
//! Paginating a handler takes three lines: run the count query, run the page
//! query, and wrap the result in a [`Page`].
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::pagination::{Page, PageRequest};
//! use diesel::prelude::*;
//! use diesel_async::RunQueryDsl;
//!
//! #[get("/api/posts")]
//! async fn list(page: PageRequest, mut db: Db) -> AutumnResult<Json<Page<Post>>> {
//!     let total: i64 = posts::table.count().get_result(&mut db).await?;
//!     let items: Vec<Post> = posts::table
//!         .limit(page.limit()).offset(page.offset())
//!         .select(Post::as_select())
//!         .load(&mut db).await?;
//!     Ok(Json(Page::new(items, total, &page)))
//! }
//! ```
//!
//! # Quick start (cursor)
//!
//! Cursor pagination is keyset pagination: filter by a stable, deterministic
//! sort key (with a unique tie-breaker like `id`), fetch `limit + 1` rows,
//! and let [`CursorPage::from_overfetched`] derive the `next_cursor` from
//! the boundary row.
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::pagination::{CursorPage, CursorRequest};
//! use chrono::{DateTime, Utc};
//! use diesel::prelude::*;
//! use diesel_async::RunQueryDsl;
//! use serde::{Deserialize, Serialize};
//!
//! // Sort key — created_at + id is the stable, deterministic tie-breaker.
//! #[derive(Serialize, Deserialize)]
//! struct PostCursor { created_at: DateTime<Utc>, id: i64 }
//!
//! #[get("/api/feed")]
//! async fn feed(cur: CursorRequest, mut db: Db) -> AutumnResult<Json<CursorPage<Post>>> {
//!     let mut q = posts::table.into_boxed();
//!     if let Some(c) = cur.decode::<PostCursor>() {
//!         q = q.filter(
//!             posts::created_at.lt(c.created_at)
//!                 .or(posts::created_at.eq(c.created_at).and(posts::id.lt(c.id))),
//!         );
//!     }
//!     let items: Vec<Post> = q
//!         .order((posts::created_at.desc(), posts::id.desc()))
//!         .limit(cur.fetch_limit())
//!         .select(Post::as_select())
//!         .load(&mut db).await?;
//!     Ok(Json(CursorPage::from_overfetched(items, &cur, |p| {
//!         PostCursor { created_at: p.created_at, id: p.id }
//!     })))
//! }
//! ```
//!
//! # Query contract
//!
//! Offset pagination uses two query parameters:
//!
//! | Parameter | Meaning | Default | Clamped to |
//! |-----------|---------|---------|------------|
//! | `page` | 1-based page index | `1` | `>= 1` |
//! | `size` | Items per page | [`DEFAULT_PAGE_SIZE`] | <code>1..=[`MAX_PAGE_SIZE`]</code> |
//!
//! Cursor pagination uses:
//!
//! | Parameter | Meaning | Default | Clamped to |
//! |-----------|---------|---------|------------|
//! | `cursor` | Opaque token from a prior `next_cursor` (omit for first page) | `None` | — |
//! | `size` | Items per page | [`DEFAULT_PAGE_SIZE`] | <code>1..=[`MAX_PAGE_SIZE`]</code> |
//!
//! Requests like `?size=0`, `?size=9999`, `?page=0`, or even `?page=abc`
//! are silently coerced to the valid range rather than rejected — bad
//! pagination parameters should not 400. Unparseable or tampered cursors
//! decode to `None` (i.e. fall back to the first page) for the same reason.
//!
//! # Signed cursors (optional)
//!
//! Plain cursors are *opaque but unsigned* — the same model used by
//! Stripe, GitHub, and Relay. Forging one is equivalent to seeking to
//! an arbitrary offset, which clients can already do with `?page=N`,
//! so for sort-key-only cursors (timestamp + id) signing adds no real
//! protection.
//!
//! However, if a handler ever encodes anything *sensitive to tampering*
//! into the cursor payload — a tenant id, a user scope, anything the
//! handler relies on to filter results — switch to the signed API:
//!
//! - [`Cursor::encode_signed`] / [`Cursor::decode_signed`]
//! - [`CursorRequest::decode_signed`]
//! - [`CursorPage::from_overfetched_signed`]
//!
//! All three take a key as `&[u8]`. Tokens are signed with HMAC-SHA256
//! and verified in constant time; tampered or unsigned tokens decode
//! to `None`.
//!
//! # Response shape
//!
//! [`Page<T>`] serializes as:
//!
//! ```json
//! {
//!   "content": [ ... ],
//!   "page": 1,
//!   "size": 20,
//!   "total_elements": 137,
//!   "total_pages": 7,
//!   "has_next": true,
//!   "has_previous": false
//! }
//! ```
//!
//! [`CursorPage<T>`] serializes as:
//!
//! ```json
//! {
//!   "content": [ ... ],
//!   "size": 20,
//!   "next_cursor": "eyJpZCI6MTIzfQ",
//!   "has_next": true
//! }
//! ```

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Default number of items per page when no `size` is provided.
pub const DEFAULT_PAGE_SIZE: u32 = 20;

/// Hard upper bound on `size` — prevents clients from requesting huge
/// pages that could OOM the server or overwhelm the database.
pub const MAX_PAGE_SIZE: u32 = 100;

// ── PageRequest ─────────────────────────────────────────────────────

/// Pagination parameters parsed from the query string.
///
/// Use as a handler extractor to receive `?page=N&size=M`. Both fields
/// are optional; missing values fall back to [`DEFAULT_PAGE_SIZE`] and
/// page `1`. Out-of-range *and unparseable* values are clamped rather
/// than rejected: `page < 1` becomes `1`, `size` is clamped to
/// <code>1..=[`MAX_PAGE_SIZE`]</code>, and inputs like `?page=abc` are
/// silently ignored. A list endpoint should never 400 because of a
/// malformed pager.
///
/// # Repository `page()` method
///
/// Every `#[repository]`-derived struct generates a `page` method that
/// accepts a `&PageRequest` and returns a [`Page<Model>`]:
///
/// ```rust
/// use autumn_web::pagination::{Page, PageRequest};
///
/// // Simulate what `repo.page(&req)` returns: a Page built from items +
/// // a total row count.  This doctest exercises the public constructors
/// // and field visibility (catches pub(crate) regressions).
/// let req = PageRequest::new(2, 10);
/// let items: Vec<u32> = (11..=20).collect();
/// let page: Page<u32> = Page::new(items, 37, &req);
///
/// assert_eq!(page.page, 2);
/// assert_eq!(page.size, 10);
/// assert_eq!(page.total_elements, 37);
/// assert_eq!(page.total_pages, 4);
/// assert!(page.has_next);
/// assert!(page.has_previous);
/// assert_eq!(page.content.len(), 10);
/// ```
///
/// # Handler example
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
/// use autumn_web::pagination::PageRequest;
///
/// #[get("/api/items")]
/// async fn list(page: PageRequest) -> String {
///     format!("page {} (limit {}, offset {})", page.page(), page.limit(), page.offset())
/// }
/// ```
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct PageRequest {
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    size: Option<u32>,
}

impl PageRequest {
    /// Construct a [`PageRequest`] explicitly. Values are clamped to the
    /// valid ranges defined by [`DEFAULT_PAGE_SIZE`] / [`MAX_PAGE_SIZE`].
    #[must_use]
    pub const fn new(page: u32, size: u32) -> Self {
        Self {
            page: Some(page),
            size: Some(size),
        }
    }

    /// Resolved 1-based page number. `0` or missing is coerced to `1`.
    #[must_use]
    pub const fn page(&self) -> u32 {
        match self.page {
            Some(p) if p >= 1 => p,
            _ => 1,
        }
    }

    /// Resolved page size, clamped to <code>1..=[`MAX_PAGE_SIZE`]</code>.
    #[must_use]
    pub const fn size(&self) -> u32 {
        match self.size {
            Some(0) | None => DEFAULT_PAGE_SIZE,
            Some(s) if s > MAX_PAGE_SIZE => MAX_PAGE_SIZE,
            Some(s) => s,
        }
    }

    /// `LIMIT` value for a Diesel or raw SQL query (`== size()`).
    #[must_use]
    pub const fn limit(&self) -> i64 {
        self.size() as i64
    }

    /// `OFFSET` value for a Diesel or raw SQL query.
    #[must_use]
    pub const fn offset(&self) -> i64 {
        ((self.page() - 1) as i64) * (self.size() as i64)
    }
}

impl<S> FromRequestParts<S> for PageRequest
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Manual parse rather than `Query::<Self>::from_request_parts` so
        // that unparseable values (`?page=abc`, `?size=`, duplicate keys,
        // percent-encoding errors) fall back to defaults instead of
        // rejecting the whole request with a 400.
        Ok(parts.uri.query().map_or_else(Self::default, parse_query))
    }
}

/// Best-effort parse of a URL-encoded query string into a [`PageRequest`].
/// Unknown keys, malformed values, and percent-decoding failures are
/// silently ignored. Later occurrences of `page`/`size` win, matching the
/// behaviour of `serde_urlencoded`.
fn parse_query(query: &str) -> PageRequest {
    let mut req = PageRequest::default();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "page" => {
                if let Ok(n) = value.parse::<u32>() {
                    req.page = Some(n);
                }
            }
            "size" => {
                if let Ok(n) = value.parse::<u32>() {
                    req.size = Some(n);
                }
            }
            _ => {}
        }
    }
    req
}

// ── SortDir ─────────────────────────────────────────────────────────

/// Sort direction for an allowlisted list query column.
///
/// This is the single canonical sort-direction type for the framework:
/// [`ListQuery::direction`] returns it, the `data_table` widget renders
/// header links with it, and the repository `list()` method applies it.
/// The `data_table` widget re-exports this same type as
/// `autumn_web::widgets::SortDir`.
///
/// `Asc` is the default — an absent or unrecognized `dir=` query parameter
/// resolves to ascending order (see [`SortDir::from_param`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDir {
    /// Ascending order (the default).
    #[default]
    Asc,
    /// Descending order.
    Desc,
}

impl SortDir {
    /// Query-parameter value: `"asc"` or `"desc"`.
    #[must_use]
    pub const fn param_value(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }

    /// `aria-sort` attribute value: `"ascending"` or `"descending"`.
    #[must_use]
    pub const fn aria_value(self) -> &'static str {
        match self {
            Self::Asc => "ascending",
            Self::Desc => "descending",
        }
    }

    /// Returns the opposite direction.
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }

    /// Parse a `dir=` query value. Only `"desc"` (case-insensitive) selects
    /// [`SortDir::Desc`]; **every other value — including an empty string, a
    /// typo, or a missing parameter — falls back to [`SortDir::Asc`]**. A list
    /// endpoint must never 400 on a malformed direction.
    #[must_use]
    pub fn from_param(value: &str) -> Self {
        if value.eq_ignore_ascii_case("desc") {
            Self::Desc
        } else {
            Self::Asc
        }
    }
}

// ── ListQuery ───────────────────────────────────────────────────────

/// Sort/filter parameters parsed from the query string, composing with
/// [`PageRequest`] to drive an allowlisted list query.
///
/// `ListQuery` is the request half of the safe sort/filter feature. It parses
/// three families of query parameters, all optional and all best-effort:
///
/// | Parameter | Meaning | Default |
/// |-----------|---------|---------|
/// | `sort` | Column key to order by | model's default order (primary key) |
/// | `dir` | `asc` or `desc` | `asc` (anything unrecognized → `asc`) |
/// | `filter[<col>]` | Equality filter on column `<col>` | none |
///
/// # The allowlist is the security boundary
///
/// `ListQuery` itself performs **no** validation of column names — it only
/// carries the raw request intent. The safety guarantee lives in the
/// repository `list()` method generated by `#[repository]`: that method
/// matches the requested `sort`/`filter[..]` keys against the model's own
/// columns using Diesel's typed DSL. A key that is not a real column hits the
/// default match arm and is **silently ignored** — it can never be
/// interpolated into SQL. This is why an attacker-supplied
/// `?sort=id;DROP TABLE users` is inert: `id;DROP TABLE users` is not a
/// column, so it falls through to the model's default ordering.
///
/// Because the extractor is [`Infallible`](std::convert::Infallible) it never
/// rejects a request: an empty `sort` falls back to the default order, an
/// invalid `dir` falls back to `asc`, and unknown parameters are dropped —
/// mirroring [`PageRequest`]'s forgiving posture.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::prelude::*;
/// use autumn_web::pagination::{ListQuery, PageRequest};
///
/// // GET /posts?sort=title&dir=desc&filter[published]=true&page=2
/// #[get("/posts")]
/// async fn index(
///     list_query: ListQuery,
///     page_req: PageRequest,
///     repo: PgPostRepository,
/// ) -> AutumnResult<Json<Page<Post>>> {
///     // `list()` applies only allowlisted columns; unknown keys are ignored.
///     let page = repo.list(&list_query, &page_req).await?;
///     Ok(Json(page))
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    sort: Option<String>,
    dir: SortDir,
    filters: Vec<(String, String)>,
}

impl ListQuery {
    /// Construct a [`ListQuery`] explicitly. Useful in tests and when driving
    /// `list()` from code rather than a request.
    #[must_use]
    pub fn new(sort: Option<&str>, dir: SortDir, filters: &[(&str, &str)]) -> Self {
        Self {
            sort: sort.map(str::to_owned),
            dir,
            filters: filters
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    /// The requested sort column key, if any.
    ///
    /// This is the *raw* requested key — it may name a column that is not in
    /// the model's allowlist, in which case `list()` ignores it and falls back
    /// to the default order. Returns `None` when no (or an empty) `sort=` was
    /// provided.
    #[must_use]
    pub fn sort(&self) -> Option<&str> {
        self.sort.as_deref()
    }

    /// The resolved sort direction, defaulting to [`SortDir::Asc`].
    #[must_use]
    pub const fn direction(&self) -> SortDir {
        self.dir
    }

    /// Iterate over the requested equality filters as `(column, value)` pairs.
    ///
    /// As with [`sort`](Self::sort), these are raw requested keys: `list()`
    /// applies only the ones that name a real, filterable column and silently
    /// drops the rest.
    pub fn filters(&self) -> impl Iterator<Item = (&str, &str)> {
        self.filters.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// `true` when no sort, direction override, or filters were requested.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sort.is_none() && self.dir == SortDir::Asc && self.filters.is_empty()
    }
}

/// No-op fallback for the repository `list()` sort/filter allowlist.
///
/// The `#[model]` macro generates **typed, per-column** inherent
/// `__autumn_list_apply_order` / `__autumn_list_apply_filters` associated
/// functions on the model struct; those take precedence over the no-op defaults
/// here. A `#[repository]` declared on a hand-written model (one not produced by
/// `#[model]`, so with no column metadata) falls back to these no-ops: `list()`
/// still paginates, but applies no sort/filter — there is no column allowlist to
/// enforce, so nothing from the request touches the query. This keeps `list()`
/// available on every repository while guaranteeing the allowlist is the only
/// path a column name can reach SQL.
#[doc(hidden)]
pub trait AutumnListable {
    #[doc(hidden)]
    fn __autumn_list_apply_order<Q>(query_builder: Q, _list: &ListQuery) -> Q {
        query_builder
    }
    #[doc(hidden)]
    fn __autumn_list_apply_filters<Q>(query_builder: Q, _list: &ListQuery) -> Q {
        query_builder
    }
}

impl<T> AutumnListable for T {}

impl<S> FromRequestParts<S> for ListQuery
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Best-effort parse, mirroring `PageRequest`: a list endpoint must
        // never 400 on a malformed sort/filter parameter.
        Ok(parts
            .uri
            .query()
            .map_or_else(Self::default, parse_list_query))
    }
}

/// Best-effort parse of a URL-encoded query string into a [`ListQuery`].
///
/// - `sort=<key>` — the last non-empty occurrence wins.
/// - `dir=<asc|desc>` — anything other than `desc` (case-insensitive) → `asc`.
/// - `filter[<col>]=<val>` — every occurrence is collected; an empty `<col>`
///   is dropped. Duplicate columns are all kept (the allowlist layer applies
///   each in turn; the last equality filter for a column effectively wins at
///   the SQL level).
///
/// Unknown keys (including `page`/`size`, which belong to [`PageRequest`]) are
/// ignored so the two extractors compose without interference.
fn parse_list_query(query: &str) -> ListQuery {
    let mut req = ListQuery::default();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let key = key.as_ref();
        if key == "sort" {
            if value.is_empty() {
                req.sort = None;
            } else {
                req.sort = Some(value.into_owned());
            }
        } else if key == "dir" {
            req.dir = SortDir::from_param(&value);
        } else if let Some(col) = key
            .strip_prefix("filter[")
            .and_then(|rest| rest.strip_suffix(']'))
            && !col.is_empty()
        {
            req.filters.push((col.to_owned(), value.into_owned()));
        }
    }
    req
}

// ── Page<T> ─────────────────────────────────────────────────────────

/// Paginated response wrapper with navigation metadata.
///
/// `Page` serializes to JSON for API responses and exposes the fields a
/// Maud template needs to render pager links (previous/next, page index,
/// total pages).
///
/// Construct one with [`Page::new`] after running your count + page
/// queries, or with [`Page::empty`] when you have no data to return.
///
/// # JSON shape
///
/// ```json
/// {
///   "content": [ /* T items */ ],
///   "page": 1,
///   "size": 20,
///   "total_elements": 137,
///   "total_pages": 7,
///   "has_next": true,
///   "has_previous": false
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    /// The items on this page.
    pub content: Vec<T>,
    /// Current 1-based page index.
    pub page: u32,
    /// Page size used to produce `content`.
    pub size: u32,
    /// Total number of items across every page.
    pub total_elements: u64,
    /// Total number of pages (`ceil(total_elements / size)`), minimum `1`.
    pub total_pages: u32,
    /// Whether there is a page after this one.
    pub has_next: bool,
    /// Whether there is a page before this one.
    pub has_previous: bool,
}

impl<T> Page<T> {
    /// Build a page from the materialized `items` and the total row
    /// count returned by the database.
    ///
    /// `total` is accepted as `i64` to match Diesel's
    /// `COUNT(*)` result type; values below zero are treated as zero.
    #[must_use]
    pub fn new(items: Vec<T>, total: i64, request: &PageRequest) -> Self {
        let size = request.size();
        let page = request.page();
        let total_elements = u64::try_from(total).unwrap_or(0);

        // ceil(total / size), minimum 1 so an empty result still
        // reports `total_pages = 1` — callers don't have to branch on
        // "no rows" when rendering a pager.
        let total_pages = if total_elements == 0 {
            1
        } else {
            // size() is always >= 1, so this division is safe.
            u32::try_from(total_elements.div_ceil(u64::from(size))).unwrap_or(u32::MAX)
        };

        Self {
            content: items,
            page,
            size,
            total_elements,
            total_pages,
            has_next: page < total_pages,
            has_previous: page > 1,
        }
    }

    /// Build an empty page using the caller's request parameters.
    ///
    /// Useful when a filter short-circuits before hitting the database.
    #[must_use]
    pub fn empty(request: &PageRequest) -> Self {
        Self::new(Vec::new(), 0, request)
    }

    /// Build a metadata-only page from raw pagination counters, without a
    /// `PageRequest`. Useful for bridging external count types (e.g. `u64`)
    /// into the standard `Page` shape for rendering or serialisation.
    ///
    /// `page` is clamped to `[1, u32::MAX]`; `total_pages` is clamped to
    /// at least `1` so callers don't have to special-case empty result sets.
    /// `content` is empty — use [`Page::new`] when you have items.
    #[must_use]
    pub fn from_raw(page: u32, size: u32, total_elements: u64, total_pages: u32) -> Self {
        let page = page.max(1);
        let total_pages = total_pages.max(1);
        Self {
            content: Vec::new(),
            page,
            size,
            total_elements,
            total_pages,
            has_next: page < total_pages,
            has_previous: page > 1,
        }
    }

    /// Paginate an already fully-materialized collection in memory.
    ///
    /// Used when every row must be loaded before the page window can be
    /// applied — for example a list endpoint that filters each row through a
    /// per-row authorization policy (the filter has to run in Rust, so a
    /// SQL `LIMIT/OFFSET` can't bound the result on its own). The returned
    /// [`Page`] carries the full `total_elements` (so `total_pages` and the
    /// nav links are correct) but only the `request`-selected window in
    /// `content`.
    ///
    /// Prefer [`Page::new`] with a SQL `LIMIT/OFFSET` + `COUNT(*)` whenever
    /// the window can be pushed into the database.
    #[must_use]
    pub fn paginate_in_memory(all: Vec<T>, request: &PageRequest) -> Self {
        let total = i64::try_from(all.len()).unwrap_or(i64::MAX);
        let offset = usize::try_from(request.offset()).unwrap_or(usize::MAX);
        let size = request.size() as usize;
        let content: Vec<T> = all.into_iter().skip(offset).take(size).collect();
        Self::new(content, total, request)
    }

    /// Transform the content while preserving pagination metadata.
    ///
    /// Typical use: converting database rows into DTOs for JSON output
    /// without re-running the count query.
    pub fn map<U, F: FnMut(T) -> U>(self, f: F) -> Page<U> {
        Page {
            content: self.content.into_iter().map(f).collect(),
            page: self.page,
            size: self.size,
            total_elements: self.total_elements,
            total_pages: self.total_pages,
            has_next: self.has_next,
            has_previous: self.has_previous,
        }
    }
}

/// Build an RFC 8288 `Link` header value for an offset [`Page`].
///
/// Uses *relative* query-only references (`<?page=2&size=20>`) which a client
/// resolves against the request URL — so the response doesn't need to know its
/// own mount path. `first` and `last` are always emitted (offset pagination
/// always knows both bounds); `prev`/`next` only when they exist.
fn page_link_header_value(page: u32, size: u32, total_pages: u32, has_next: bool) -> String {
    let mut links: Vec<String> = Vec::with_capacity(4);
    let rel = |target: u32, name: &str| format!("<?page={target}&size={size}>; rel=\"{name}\"");
    links.push(rel(1, "first"));
    if page > 1 {
        links.push(rel(page - 1, "prev"));
    }
    if has_next {
        links.push(rel(page + 1, "next"));
    }
    // For an empty collection `total_pages` is 0; clamp to 1 so the `last`
    // link stays a valid 1-indexed page rather than `page=0`.
    let last_page = total_pages.max(1);
    links.push(rel(last_page, "last"));
    links.join(", ")
}

impl<T> IntoResponse for Page<T>
where
    T: Serialize,
{
    fn into_response(self) -> Response {
        // Snapshot the nav counters before `self` is moved into `Json`.
        let (page, size, total_pages, has_next) =
            (self.page, self.size, self.total_pages, self.has_next);
        let mut response = axum::Json(self).into_response();
        if let Ok(value) =
            HeaderValue::from_str(&page_link_header_value(page, size, total_pages, has_next))
        {
            response.headers_mut().insert(header::LINK, value);
        }
        response
    }
}

impl<T> IntoResponse for CursorPage<T>
where
    T: Serialize,
{
    fn into_response(self) -> Response {
        // Only a `next` link is meaningful for keyset pagination — there is no
        // cheap way back to `first`/`prev`/`last` without a full scan.
        let next_link = self
            .next_cursor
            .as_ref()
            .filter(|_| self.has_next)
            .map(|token| {
                let encoded: String =
                    url::form_urlencoded::byte_serialize(token.as_bytes()).collect();
                format!("<?cursor={encoded}&size={}>; rel=\"next\"", self.size)
            });
        let mut response = axum::Json(self).into_response();
        if let Some(link) = next_link
            && let Ok(value) = HeaderValue::from_str(&link)
        {
            response.headers_mut().insert(header::LINK, value);
        }
        response
    }
}

// ── Cursor encoding ─────────────────────────────────────────────────
//
// Cursor tokens are base64url-encoded JSON. base64url is used (rather
// than the standard alphabet) so that tokens are safe to embed in a
// URL without percent-encoding, and padding (`=`) is omitted to keep
// them tidy. Tokens are *opaque* to clients — encoding the structure
// keeps callers from forging cursors but, more importantly, lets the
// server change the schema without breaking the wire contract.

const BASE64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

// `clippy::chunks_exact_to_as_chunks` (armed by rustc 1.98) suggests
// `as_chunks::<3>()`. That API stabilized in 1.88, which is this workspace's
// exact MSRV — CI builds a `1.88.0` lane, so adopting it leaves zero margin for
// an MSRV the project may want to hold. Not a trade worth making for an
// incidental cleanup; revisit when the MSRV moves past 1.88.
#[allow(clippy::chunks_exact_to_as_chunks)]
fn base64url_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for chunk in &mut chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.push(BASE64URL_ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(BASE64URL_ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(BASE64URL_ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        out.push(BASE64URL_ALPHABET[(n & 0x3F) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        0 => {}
        1 => {
            let n = u32::from(rem[0]) << 16;
            out.push(BASE64URL_ALPHABET[((n >> 18) & 0x3F) as usize] as char);
            out.push(BASE64URL_ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        }
        2 => {
            let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            out.push(BASE64URL_ALPHABET[((n >> 18) & 0x3F) as usize] as char);
            out.push(BASE64URL_ALPHABET[((n >> 12) & 0x3F) as usize] as char);
            out.push(BASE64URL_ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        }
        _ => unreachable!("chunks_exact remainder is < 3 by construction"),
    }
    out
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let value = |b: u8| -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some(u32::from(b - b'A')),
            b'a'..=b'z' => Some(u32::from(b - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(b - b'0') + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut i = 0;
    while i + 4 <= bytes.len() {
        let n = (value(bytes[i])? << 18)
            | (value(bytes[i + 1])? << 12)
            | (value(bytes[i + 2])? << 6)
            | value(bytes[i + 3])?;
        out.push(u8::try_from((n >> 16) & 0xFF).ok()?);
        out.push(u8::try_from((n >> 8) & 0xFF).ok()?);
        out.push(u8::try_from(n & 0xFF).ok()?);
        i += 4;
    }
    match bytes.len() - i {
        0 => {}
        1 => return None, // a single trailing char is not a valid base64 group
        2 => {
            let n = (value(bytes[i])? << 18) | (value(bytes[i + 1])? << 12);
            out.push(u8::try_from((n >> 16) & 0xFF).ok()?);
        }
        3 => {
            let n = (value(bytes[i])? << 18)
                | (value(bytes[i + 1])? << 12)
                | (value(bytes[i + 2])? << 6);
            out.push(u8::try_from((n >> 16) & 0xFF).ok()?);
            out.push(u8::try_from((n >> 8) & 0xFF).ok()?);
        }
        _ => unreachable!("bytes.len() - i is < 4 by the loop condition"),
    }
    Some(out)
}

/// Opaque cursor tokens for cursor-based pagination.
///
/// A cursor wraps a serializable sort-key payload (typically a struct
/// containing a timestamp and a unique tie-breaker like `id`) into a
/// URL-safe string. The on-the-wire format is base64url-encoded JSON;
/// callers should treat tokens as opaque.
///
/// # Examples
///
/// ```rust
/// use autumn_web::pagination::Cursor;
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Serialize, Deserialize, PartialEq, Debug)]
/// struct Key { id: i64 }
///
/// let token = Cursor::encode(&Key { id: 42 }).unwrap();
/// let decoded: Key = Cursor::decode(&token).unwrap();
/// assert_eq!(decoded, Key { id: 42 });
/// ```
pub struct Cursor;

impl Cursor {
    /// Encode a serializable value as an opaque URL-safe cursor token.
    ///
    /// # Errors
    ///
    /// Returns `serde_json::Error` if the value cannot be serialized.
    pub fn encode<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
        Ok(base64url_encode(&serde_json::to_vec(value)?))
    }

    /// Decode an opaque cursor token back into a typed value.
    ///
    /// Returns `None` for any malformed token — invalid base64, invalid
    /// UTF-8, or JSON that doesn't match `T`. A list endpoint should
    /// silently fall back to the first page rather than 400 on a
    /// tampered or stale cursor, matching the forgiving behaviour of
    /// the `?page=` / `?size=` parser.
    #[must_use]
    pub fn decode<T: DeserializeOwned>(token: &str) -> Option<T> {
        let bytes = base64url_decode(token)?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Encode a value as a *signed* cursor token using HMAC-SHA256.
    ///
    /// Use this when the cursor payload encodes anything sensitive to
    /// tampering — for example a tenant boundary, a user id, or any
    /// scope a handler relies on to filter results. Without signing,
    /// a client could edit the JSON and re-encode it. Plain
    /// [`Cursor::encode`] is fine for cursors that only carry sort-key
    /// values (timestamps, primary keys), since forging one is
    /// equivalent to seeking to an arbitrary offset.
    ///
    /// The token format is `<base64url(json)>.<base64url(hmac)>`.
    /// The signature covers exactly the payload bytes; the encoded
    /// payload itself is not encrypted (cursors are not secrets).
    ///
    /// # Errors
    ///
    /// Returns `serde_json::Error` if the value cannot be serialized.
    pub fn encode_signed<T: Serialize>(value: &T, key: &[u8]) -> Result<String, serde_json::Error> {
        let payload = serde_json::to_vec(value)?;
        let payload_b64 = base64url_encode(&payload);
        let mac = hmac_sha256(key, payload_b64.as_bytes());
        let sig_b64 = base64url_encode(&mac);
        Ok(format!("{payload_b64}.{sig_b64}"))
    }

    /// Decode a signed cursor token, verifying its HMAC-SHA256 signature.
    ///
    /// Returns `None` for any of: malformed structure, malformed
    /// base64, signature mismatch, JSON that doesn't match `T`. The
    /// signature is verified in constant time. A handler that uses
    /// this should treat `None` the same way it treats no cursor
    /// (fall back to first page) rather than returning an error —
    /// the goal is to ignore tampered cursors, not to surface them
    /// as user-facing failures.
    #[must_use]
    pub fn decode_signed<T: DeserializeOwned>(token: &str, key: &[u8]) -> Option<T> {
        let (payload_b64, sig_b64) = token.split_once('.')?;
        let expected_sig = base64url_decode(sig_b64)?;
        let actual_sig = hmac_sha256(key, payload_b64.as_bytes());
        if !constant_time_eq(&expected_sig, &actual_sig) {
            return None;
        }
        let payload = base64url_decode(payload_b64)?;
        serde_json::from_slice(&payload).ok()
    }
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    // `Hmac::new_from_slice` accepts any key length.
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.ct_eq(b).into()
}

// ── CursorRequest ───────────────────────────────────────────────────

/// Cursor pagination parameters parsed from the query string.
///
/// Use as a handler extractor to receive `?cursor=<token>&size=<n>`.
/// Both fields are optional: missing `cursor` means "give me the
/// first page", missing `size` means [`DEFAULT_PAGE_SIZE`]. The same
/// forgiving coercion as [`PageRequest`] applies — `?size=0` falls
/// back to the default, `?size=9999` is clamped to [`MAX_PAGE_SIZE`],
/// and unparseable values are silently ignored. A malformed cursor is
/// treated as no cursor.
///
/// # Examples
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
/// use autumn_web::pagination::CursorRequest;
///
/// #[get("/api/feed")]
/// async fn feed(cur: CursorRequest) -> String {
///     format!("size={}, cursor={:?}", cur.size(), cur.cursor())
/// }
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CursorRequest {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    size: Option<u32>,
}

impl CursorRequest {
    /// Construct a [`CursorRequest`] explicitly. Useful in tests.
    #[must_use]
    pub const fn new(cursor: Option<String>, size: u32) -> Self {
        Self {
            cursor,
            size: Some(size),
        }
    }

    /// The raw, opaque cursor token, if any.
    #[must_use]
    pub fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }

    /// Decode the cursor into a typed sort-key value.
    ///
    /// Returns `None` if the cursor is missing or unparseable. Use
    /// this in handlers to add the keyset filter to the query.
    #[must_use]
    pub fn decode<T: DeserializeOwned>(&self) -> Option<T> {
        Cursor::decode(self.cursor.as_deref()?)
    }

    /// Decode a *signed* cursor, verifying its HMAC-SHA256 signature
    /// against `key`. See [`Cursor::decode_signed`] for the threat
    /// model and when to use signed vs. unsigned cursors.
    ///
    /// Returns `None` if the cursor is missing, malformed, or has an
    /// invalid signature.
    #[must_use]
    pub fn decode_signed<T: DeserializeOwned>(&self, key: &[u8]) -> Option<T> {
        Cursor::decode_signed(self.cursor.as_deref()?, key)
    }

    /// Resolved page size, clamped to <code>1..=[`MAX_PAGE_SIZE`]</code>.
    #[must_use]
    pub const fn size(&self) -> u32 {
        match self.size {
            Some(0) | None => DEFAULT_PAGE_SIZE,
            Some(s) if s > MAX_PAGE_SIZE => MAX_PAGE_SIZE,
            Some(s) => s,
        }
    }

    /// `LIMIT` value matching the requested page size.
    #[must_use]
    pub const fn limit(&self) -> i64 {
        self.size() as i64
    }

    /// `LIMIT + 1` — fetch one extra row so the handler can detect
    /// whether a next page exists without an extra query.
    ///
    /// This is the value to pass to Diesel's `.limit(...)` when using
    /// [`CursorPage::from_overfetched`].
    #[must_use]
    pub const fn fetch_limit(&self) -> i64 {
        self.size() as i64 + 1
    }
}

impl<S> FromRequestParts<S> for CursorRequest
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .uri
            .query()
            .map_or_else(Self::default, parse_cursor_query))
    }
}

/// Best-effort parse of a URL-encoded query string into a [`CursorRequest`].
/// Same coercion rules as [`parse_query`]: unknown keys, malformed values,
/// and percent-decoding failures are silently ignored. Later occurrences of
/// `cursor`/`size` win.
fn parse_cursor_query(query: &str) -> CursorRequest {
    let mut req = CursorRequest::default();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "cursor" if !value.is_empty() => {
                req.cursor = Some(value.into_owned());
            }
            "size" => {
                if let Ok(n) = value.parse::<u32>() {
                    req.size = Some(n);
                }
            }
            _ => {}
        }
    }
    req
}

// ── CursorPage<T> ───────────────────────────────────────────────────

/// Paginated response wrapper for cursor-based pagination.
///
/// `CursorPage` serializes to JSON for API responses and is the
/// counterpart to [`Page<T>`] for keyset/seek pagination. Construct
/// one with [`CursorPage::from_overfetched`] after running a single
/// `LIMIT n+1` query.
///
/// # JSON shape
///
/// ```json
/// {
///   "content": [ /* T items */ ],
///   "size": 20,
///   "next_cursor": "eyJpZCI6MTIzfQ",
///   "has_next": true
/// }
/// ```
///
/// `next_cursor` is `null` on the last page (`has_next == false`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorPage<T> {
    /// The items on this page.
    pub content: Vec<T>,
    /// Page size used to produce `content`.
    pub size: u32,
    /// Opaque cursor for the next page, or `None` if this is the last page.
    pub next_cursor: Option<String>,
    /// Whether there is a page after this one.
    pub has_next: bool,
}

impl<T> CursorPage<T> {
    /// Build a page from an over-fetched result set.
    ///
    /// The caller fetches `request.fetch_limit()` rows (one more than
    /// the page size). If that returned the extra row, this method
    /// truncates `content` back to the page size, marks `has_next`
    /// true, and derives `next_cursor` from the *last kept* item via
    /// `cursor_fn`. If fewer rows came back, this is the last page
    /// and `next_cursor` is `None`.
    ///
    /// `cursor_fn` is called at most once.
    ///
    /// # Errors / panics
    ///
    /// If `cursor_fn`'s value fails to serialize (extremely unlikely
    /// for the simple sort-key structs typically used here),
    /// `next_cursor` is set to `None` and `has_next` to `false`. The
    /// page is still returned — the alternative would be a 500 on a
    /// successful query, which is the wrong tradeoff for a list
    /// endpoint.
    #[must_use]
    pub fn from_overfetched<K, F>(items: Vec<T>, request: &CursorRequest, cursor_fn: F) -> Self
    where
        K: Serialize,
        F: FnOnce(&T) -> K,
    {
        Self::from_overfetched_inner(items, request, cursor_fn, |k| Cursor::encode(&k).ok())
    }

    /// Variant of [`Self::from_overfetched`] that signs `next_cursor`
    /// with HMAC-SHA256 using `key`.
    ///
    /// Use this when the cursor payload encodes anything sensitive to
    /// tampering (tenant ids, user scopes). For sort-key-only cursors
    /// (timestamp + id), plain [`Self::from_overfetched`] is fine —
    /// forging an unsigned cursor is equivalent to seeking to an
    /// arbitrary offset, which clients can already do with `?page=N`.
    ///
    /// The corresponding extractor side calls
    /// [`CursorRequest::decode_signed`] with the same key.
    #[must_use]
    pub fn from_overfetched_signed<K, F>(
        items: Vec<T>,
        request: &CursorRequest,
        key: &[u8],
        cursor_fn: F,
    ) -> Self
    where
        K: Serialize,
        F: FnOnce(&T) -> K,
    {
        Self::from_overfetched_inner(items, request, cursor_fn, |k| {
            Cursor::encode_signed(&k, key).ok()
        })
    }

    fn from_overfetched_inner<K, F, E>(
        mut items: Vec<T>,
        request: &CursorRequest,
        cursor_fn: F,
        encode: E,
    ) -> Self
    where
        K: Serialize,
        F: FnOnce(&T) -> K,
        E: FnOnce(K) -> Option<String>,
    {
        let size = request.size();
        let limit = size as usize;
        let has_next = items.len() > limit;
        if has_next {
            items.truncate(limit);
        }
        let next_cursor = if has_next {
            // The boundary row is the last one we kept. Encoding from
            // it (rather than the popped row) means the next query
            // can use a strict inequality and still see every row,
            // even under concurrent inserts that land between pages.
            items.last().map(cursor_fn).and_then(encode)
        } else {
            None
        };
        // If encoding failed for some reason, don't claim a next page
        // we can't actually serve.
        let has_next = has_next && next_cursor.is_some();
        Self {
            content: items,
            size,
            next_cursor,
            has_next,
        }
    }

    /// Build an empty page using the caller's request parameters.
    ///
    /// Useful when a filter short-circuits before hitting the database.
    #[must_use]
    pub const fn empty(request: &CursorRequest) -> Self {
        Self {
            content: Vec::new(),
            size: request.size(),
            next_cursor: None,
            has_next: false,
        }
    }

    /// Transform the content while preserving pagination metadata.
    pub fn map<U, F: FnMut(T) -> U>(self, f: F) -> CursorPage<U> {
        CursorPage {
            content: self.content.into_iter().map(f).collect(),
            size: self.size,
            next_cursor: self.next_cursor,
            has_next: self.has_next,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    // ── PageRequest coercion ────────────────────────────────────

    #[test]
    fn defaults_when_nothing_provided() {
        let r = PageRequest::default();
        assert_eq!(r.page(), 1);
        assert_eq!(r.size(), DEFAULT_PAGE_SIZE);
        assert_eq!(r.limit(), i64::from(DEFAULT_PAGE_SIZE));
        assert_eq!(r.offset(), 0);
    }

    #[test]
    fn page_zero_is_coerced_to_one() {
        let r = PageRequest::new(0, 10);
        assert_eq!(r.page(), 1);
        assert_eq!(r.offset(), 0);
    }

    #[test]
    fn size_is_clamped_to_max() {
        let r = PageRequest::new(1, 9_999);
        assert_eq!(r.size(), MAX_PAGE_SIZE);
        assert_eq!(r.limit(), i64::from(MAX_PAGE_SIZE));

        let exact = PageRequest::new(1, MAX_PAGE_SIZE);
        assert_eq!(exact.size(), MAX_PAGE_SIZE);

        let over = PageRequest::new(1, MAX_PAGE_SIZE + 1);
        assert_eq!(over.size(), MAX_PAGE_SIZE);

        let under = PageRequest::new(1, MAX_PAGE_SIZE - 1);
        assert_eq!(under.size(), MAX_PAGE_SIZE - 1);
    }

    #[test]
    fn size_zero_falls_back_to_default() {
        let r = PageRequest::new(3, 0);
        assert_eq!(r.size(), DEFAULT_PAGE_SIZE);
    }

    #[test]
    fn offset_matches_page_and_size() {
        let r = PageRequest::new(3, 25);
        assert_eq!(r.offset(), 50);
        assert_eq!(r.limit(), 25);
    }

    // ── Page metadata ──────────────────────────────────────────

    #[test]
    fn empty_page_has_one_total_page() {
        let req = PageRequest::new(5, 50);
        let page: Page<i32> = Page::empty(&req);
        assert_eq!(page.page, 5);
        assert_eq!(page.size, 50);
        assert_eq!(page.total_elements, 0);
        assert_eq!(page.total_pages, 1);
        assert!(!page.has_next);
        assert!(page.has_previous);
        assert!(page.content.is_empty());
    }

    #[test]
    fn metadata_reflects_middle_page() {
        let req = PageRequest::new(3, 20);
        let page = Page::new(vec![1_i32; 20], 137, &req);
        assert_eq!(page.page, 3);
        assert_eq!(page.size, 20);
        assert_eq!(page.total_elements, 137);
        assert_eq!(page.total_pages, 7); // ceil(137/20) == 7
        assert!(page.has_next);
        assert!(page.has_previous);
    }

    #[test]
    fn metadata_reflects_last_page() {
        let req = PageRequest::new(7, 20);
        let page = Page::new(vec![1_i32; 17], 137, &req);
        assert_eq!(page.total_pages, 7);
        assert!(!page.has_next);
        assert!(page.has_previous);
    }

    #[test]
    fn negative_total_is_treated_as_zero() {
        let page: Page<i32> = Page::new(vec![], -1, &PageRequest::default());
        assert_eq!(page.total_elements, 0);
        assert_eq!(page.total_pages, 1);
    }

    #[test]
    fn map_preserves_metadata() {
        let req = PageRequest::new(2, 10);
        let page = Page::new(vec![1_i32, 2, 3], 25, &req);
        let mapped = page.map(|n| n.to_string());
        assert_eq!(mapped.page, 2);
        assert_eq!(mapped.size, 10);
        assert_eq!(mapped.total_elements, 25);
        assert_eq!(mapped.total_pages, 3);
        assert!(mapped.has_next);
        assert!(mapped.has_previous);
        assert_eq!(mapped.content, vec!["1", "2", "3"]);
    }

    // ── JSON serialization ─────────────────────────────────────

    #[test]
    fn page_serializes_to_expected_shape() {
        let req = PageRequest::new(2, 10);
        let page = Page::new(vec!["a", "b"], 25, &req);
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(json["page"], 2);
        assert_eq!(json["size"], 10);
        assert_eq!(json["total_elements"], 25);
        assert_eq!(json["total_pages"], 3);
        assert_eq!(json["has_next"], true);
        assert_eq!(json["has_previous"], true);
        assert_eq!(json["content"], serde_json::json!(["a", "b"]));
    }

    // ── Extractor tests ────────────────────────────────────────

    async fn echo(page: PageRequest) -> String {
        format!("{}:{}:{}", page.page(), page.size(), page.offset())
    }

    async fn fetch(uri: &str) -> (StatusCode, String) {
        let app = Router::new().route("/items", get(echo));
        let res = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn extractor_uses_defaults_when_query_missing() {
        let (status, body) = fetch("/items").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, format!("1:{DEFAULT_PAGE_SIZE}:0"));
    }

    #[tokio::test]
    async fn extractor_parses_page_and_size() {
        let (status, body) = fetch("/items?page=4&size=25").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "4:25:75");
    }

    #[tokio::test]
    async fn extractor_clamps_size_over_max() {
        let (status, body) = fetch("/items?page=1&size=5000").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, format!("1:{MAX_PAGE_SIZE}:0"));
    }

    #[tokio::test]
    async fn extractor_coerces_page_zero_to_one() {
        let (status, body) = fetch("/items?page=0&size=10").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "1:10:0");
    }

    // ── Malformed input handling ───────────────────────────────
    //
    // A list endpoint should never 400 because of a malformed pager.
    // These cases used to reject through `Query::from_request_parts` —
    // they now fall back to defaults.

    #[tokio::test]
    async fn extractor_ignores_non_numeric_page() {
        let (status, body) = fetch("/items?page=abc&size=10").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "1:10:0");
    }

    #[tokio::test]
    async fn extractor_ignores_empty_size() {
        let (status, body) = fetch("/items?page=2&size=").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, format!("2:{DEFAULT_PAGE_SIZE}:{DEFAULT_PAGE_SIZE}"));
    }

    #[tokio::test]
    async fn extractor_uses_last_value_on_duplicate_keys() {
        let (status, body) = fetch("/items?page=1&page=4&size=5").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "4:5:15");
    }

    #[tokio::test]
    async fn extractor_ignores_unknown_keys() {
        let (status, body) = fetch("/items?sort=name&page=2&size=10").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "2:10:10");
    }

    #[tokio::test]
    async fn extractor_handles_percent_encoded_values() {
        // `%32` decodes to `2`
        let (status, body) = fetch("/items?page=%32&size=10").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "2:10:10");
    }

    #[tokio::test]
    async fn extractor_handles_negative_page_value() {
        // `-1` is not a valid u32 — fall back to the default page.
        let (status, body) = fetch("/items?page=-1&size=10").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "1:10:0");
    }

    // ── ListQuery parsing / coercion ───────────────────────────

    #[test]
    fn sort_dir_from_param_defaults_to_asc() {
        assert_eq!(SortDir::from_param("desc"), SortDir::Desc);
        assert_eq!(SortDir::from_param("DESC"), SortDir::Desc);
        assert_eq!(SortDir::from_param("asc"), SortDir::Asc);
        // Anything unrecognized falls back to Asc — never an error.
        assert_eq!(SortDir::from_param(""), SortDir::Asc);
        assert_eq!(SortDir::from_param("descending"), SortDir::Asc);
        assert_eq!(SortDir::from_param("id;DROP TABLE"), SortDir::Asc);
    }

    #[test]
    fn list_query_default_is_empty() {
        let q = ListQuery::default();
        assert!(q.sort().is_none());
        assert_eq!(q.direction(), SortDir::Asc);
        assert_eq!(q.filters().count(), 0);
        assert!(q.is_empty());
    }

    #[test]
    fn list_query_parses_sort_dir_and_filters() {
        let q = parse_list_query("sort=title&dir=desc&filter[published]=true&filter[author]=ada");
        assert_eq!(q.sort(), Some("title"));
        assert_eq!(q.direction(), SortDir::Desc);
        let filters: Vec<(&str, &str)> = q.filters().collect();
        assert_eq!(filters, vec![("published", "true"), ("author", "ada")]);
        assert!(!q.is_empty());
    }

    #[test]
    fn list_query_ignores_pagination_and_unknown_keys() {
        // page/size belong to PageRequest; unknown keys are dropped.
        let q = parse_list_query("page=3&size=10&whatever=x&sort=name");
        assert_eq!(q.sort(), Some("name"));
        assert_eq!(q.filters().count(), 0);
    }

    #[test]
    fn list_query_empty_sort_and_bad_dir_fall_back() {
        // Empty sort → None (default order); unrecognized dir → Asc.
        let q = parse_list_query("sort=&dir=sideways");
        assert!(q.sort().is_none());
        assert_eq!(q.direction(), SortDir::Asc);
    }

    #[test]
    fn list_query_drops_empty_filter_column() {
        let q = parse_list_query("filter[]=orphan&filter[ok]=1");
        let filters: Vec<(&str, &str)> = q.filters().collect();
        assert_eq!(filters, vec![("ok", "1")]);
    }

    #[test]
    fn list_query_last_sort_wins() {
        let q = parse_list_query("sort=a&sort=b");
        assert_eq!(q.sort(), Some("b"));
    }

    #[test]
    fn list_query_carries_injection_payload_verbatim() {
        // The extractor does NOT sanitize — it faithfully carries the raw
        // request. The *allowlist* in the generated `list()` is what makes
        // this inert (the key is not a column, so it never reaches SQL).
        let q = parse_list_query("sort=id;DROP%20TABLE%20users");
        assert_eq!(q.sort(), Some("id;DROP TABLE users"));
    }

    async fn list_echo(list: ListQuery) -> String {
        let filters: Vec<String> = list.filters().map(|(k, v)| format!("{k}={v}")).collect();
        format!(
            "{}:{}:{}",
            list.sort().unwrap_or("<none>"),
            list.direction().param_value(),
            filters.join(",")
        )
    }

    async fn fetch_list(uri: &str) -> (StatusCode, String) {
        let app = Router::new().route("/items", get(list_echo));
        let res = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn list_query_extractor_never_400s_on_garbage() {
        // Malicious/garbage input is carried verbatim, request still 200s.
        let (status, body) = fetch_list("/items?sort=id;DROP+TABLE&dir=%00&filter[x]=1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "id;DROP TABLE:asc:x=1");
    }

    #[tokio::test]
    async fn list_query_extractor_defaults_when_query_missing() {
        let (status, body) = fetch_list("/items").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "<none>:asc:");
    }

    // ── Cursor: base64url round-trip ───────────────────────────

    #[test]
    fn base64url_encode_known_vectors() {
        // RFC 4648 §10 vectors, with `=` padding stripped (we use base64url-no-pad).
        assert_eq!(base64url_encode(b""), "");
        assert_eq!(base64url_encode(b"f"), "Zg");
        assert_eq!(base64url_encode(b"fo"), "Zm8");
        assert_eq!(base64url_encode(b"foo"), "Zm9v");
        assert_eq!(base64url_encode(b"foob"), "Zm9vYg");
        assert_eq!(base64url_encode(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64url_uses_url_safe_alphabet() {
        // Bytes 0xFB, 0xEF would produce `+` and `/` in standard base64.
        // The url-safe alphabet uses `-` and `_` instead.
        let encoded = base64url_encode(&[0xFB, 0xEF, 0xFF]);
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));
        assert!(encoded.contains('-') || encoded.contains('_'));
    }

    #[test]
    fn base64url_round_trip_arbitrary_bytes() {
        for len in 0_u8..=32 {
            let input: Vec<u8> = (0..len).map(|i| i.wrapping_mul(37)).collect();
            let encoded = base64url_encode(&input);
            let decoded = base64url_decode(&encoded).unwrap();
            assert_eq!(decoded, input, "round-trip failed at len {len}");
        }
    }

    #[test]
    fn base64url_decode_rejects_invalid_chars() {
        assert_eq!(base64url_decode("!!!!"), None);
        assert_eq!(base64url_decode("AAAA="), None); // padding not accepted
        assert_eq!(base64url_decode("AAA+"), None); // standard-alphabet char
    }

    #[test]
    fn base64url_decode_rejects_one_trailing_char() {
        // A single base64 char carries only 6 bits — not enough for a byte.
        assert_eq!(base64url_decode("A"), None);
        assert_eq!(base64url_decode("ZmA"), Some(vec![0x66, 0x60]));
    }

    // ── Cursor: encode/decode ──────────────────────────────────

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct PostKey {
        created_at: String,
        id: i64,
    }

    #[test]
    fn cursor_round_trip_preserves_payload() {
        let key = PostKey {
            created_at: "2026-04-27T12:00:00Z".to_string(),
            id: 12_345,
        };
        let token = Cursor::encode(&key).unwrap();
        let decoded: PostKey = Cursor::decode(&token).unwrap();
        assert_eq!(decoded, key);
    }

    #[test]
    fn cursor_token_is_url_safe() {
        // Pick a payload that contains JSON characters which would
        // otherwise need percent-encoding (`{`, `}`, `:`, `"`).
        let key = PostKey {
            created_at: "2026-04-27T12:00:00Z".to_string(),
            id: 1,
        };
        let token = Cursor::encode(&key).unwrap();
        // Only chars from the base64url alphabet — no `+`, `/`, `=`, `{`, `:`.
        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
    }

    #[test]
    fn cursor_decode_returns_none_for_garbage() {
        // Not base64url at all.
        let decoded: Option<PostKey> = Cursor::decode("!!!not a token!!!");
        assert!(decoded.is_none());
    }

    #[test]
    fn cursor_decode_returns_none_for_wrong_schema() {
        // Valid base64url, valid JSON, but doesn't match the target type.
        let other = serde_json::json!({"unrelated": "value"});
        let token = Cursor::encode(&other).unwrap();
        let decoded: Option<PostKey> = Cursor::decode(&token);
        assert!(decoded.is_none());
    }

    // ── CursorRequest coercion ─────────────────────────────────

    #[test]
    fn cursor_request_defaults_when_empty() {
        let r = CursorRequest::default();
        assert!(r.cursor().is_none());
        assert_eq!(r.size(), DEFAULT_PAGE_SIZE);
        assert_eq!(r.limit(), i64::from(DEFAULT_PAGE_SIZE));
        assert_eq!(r.fetch_limit(), i64::from(DEFAULT_PAGE_SIZE) + 1);
    }

    #[test]
    fn cursor_request_clamps_size_to_max() {
        let r = CursorRequest::new(None, 9_999);
        assert_eq!(r.size(), MAX_PAGE_SIZE);
        assert_eq!(r.fetch_limit(), i64::from(MAX_PAGE_SIZE) + 1);

        let exact = CursorRequest::new(None, MAX_PAGE_SIZE);
        assert_eq!(exact.size(), MAX_PAGE_SIZE);

        let over = CursorRequest::new(None, MAX_PAGE_SIZE + 1);
        assert_eq!(over.size(), MAX_PAGE_SIZE);

        let under = CursorRequest::new(None, MAX_PAGE_SIZE - 1);
        assert_eq!(under.size(), MAX_PAGE_SIZE - 1);
    }

    #[test]
    fn cursor_request_zero_size_falls_back_to_default() {
        let r = CursorRequest::new(None, 0);
        assert_eq!(r.size(), DEFAULT_PAGE_SIZE);
    }

    #[test]
    fn cursor_request_decode_helper_returns_none_when_missing() {
        let r = CursorRequest::default();
        let decoded: Option<PostKey> = r.decode();
        assert!(decoded.is_none());
    }

    #[test]
    fn cursor_request_decode_helper_round_trips() {
        let key = PostKey {
            created_at: "2026-04-27T00:00:00Z".to_string(),
            id: 7,
        };
        let token = Cursor::encode(&key).unwrap();
        let r = CursorRequest::new(Some(token), 10);
        let decoded: PostKey = r.decode().unwrap();
        assert_eq!(decoded, key);
    }

    // ── CursorPage from_overfetched ────────────────────────────

    #[test]
    fn cursor_page_signals_no_next_when_under_limit() {
        let req = CursorRequest::new(None, 5);
        let items = vec![1_i32, 2, 3]; // fewer than size
        let page = CursorPage::from_overfetched(items, &req, |&n| serde_json::json!({"id": n}));
        assert_eq!(page.content, vec![1, 2, 3]);
        assert!(!page.has_next);
        assert!(page.next_cursor.is_none());
        assert_eq!(page.size, 5);
    }

    #[test]
    fn cursor_page_signals_no_next_at_exact_limit() {
        let req = CursorRequest::new(None, 3);
        // Caller fetched limit+1 = 4, but only got 3 — last page.
        let items = vec![1_i32, 2, 3];
        let page = CursorPage::from_overfetched(items, &req, |&n| serde_json::json!({"id": n}));
        assert_eq!(page.content.len(), 3);
        assert!(!page.has_next);
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn cursor_page_truncates_overflow_and_emits_next_cursor() {
        let req = CursorRequest::new(None, 3);
        // Caller fetched limit+1 = 4 rows.
        let items = vec![1_i32, 2, 3, 4];
        let page = CursorPage::from_overfetched(items, &req, |&n| serde_json::json!({"id": n}));
        // Only `size` items kept.
        assert_eq!(page.content, vec![1, 2, 3]);
        assert!(page.has_next);
        let token = page.next_cursor.as_ref().expect("next cursor present");
        // Cursor encodes the *last kept* row (id=3), not the popped row (id=4).
        // This matters: the next page filters with a strict inequality
        // against this id, which keeps zero-duplicate behaviour even
        // if a new row gets inserted between page boundaries.
        let decoded: serde_json::Value = Cursor::decode(token).unwrap();
        assert_eq!(decoded, serde_json::json!({"id": 3}));
    }

    #[test]
    fn cursor_page_from_overfetched_handles_encoding_failure() {
        let req = CursorRequest::new(None, 2);
        let items = vec![1_i32, 2, 3];
        let page = CursorPage::from_overfetched_inner(
            items,
            &req,
            |&n| n,
            |_| None::<String>, // Force encoding to fail
        );

        assert_eq!(page.content, vec![1, 2]);
        assert_eq!(page.size, 2);
        assert!(page.next_cursor.is_none());
        assert!(!page.has_next);
    }

    #[test]
    fn cursor_page_empty_helper() {
        let req = CursorRequest::new(None, 10);
        let page: CursorPage<i32> = CursorPage::empty(&req);
        assert!(page.content.is_empty());
        assert_eq!(page.size, 10);
        assert!(!page.has_next);
        assert!(page.next_cursor.is_none());

        let req_diff_size = CursorRequest::new(None, 5);
        let page_diff_size: CursorPage<i32> = CursorPage::empty(&req_diff_size);
        assert_eq!(page_diff_size.size, 5);
    }

    #[test]
    fn cursor_page_map_preserves_metadata() {
        let req = CursorRequest::new(None, 2);
        let items = vec![1_i32, 2, 3]; // overfetch by 1
        let page = CursorPage::from_overfetched(items, &req, |&n| serde_json::json!({"id": n}));

        let original_cursor = page.next_cursor.clone();

        let mapped = page.map(|n| n.to_string());
        assert_eq!(mapped.content, vec!["1", "2"]);
        assert!(mapped.has_next);
        assert_eq!(mapped.next_cursor, original_cursor);
        assert_eq!(mapped.size, 2);
    }

    #[test]
    fn cursor_page_serializes_to_expected_shape() {
        let req = CursorRequest::new(None, 2);
        let items = vec!["a", "b", "c"];
        let page = CursorPage::from_overfetched(items, &req, |s| serde_json::json!({"key": s}));
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(json["size"], 2);
        assert_eq!(json["has_next"], true);
        assert!(json["next_cursor"].is_string());
        assert_eq!(json["content"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn cursor_page_last_page_serializes_null_cursor() {
        let req = CursorRequest::new(None, 5);
        let items = vec!["only"];
        let page = CursorPage::from_overfetched(items, &req, |s| serde_json::json!({"key": s}));
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(json["has_next"], false);
        assert!(json["next_cursor"].is_null());
    }

    // ── CursorRequest extractor ────────────────────────────────

    async fn cursor_echo(req: CursorRequest) -> String {
        format!(
            "{}|{}|{}",
            req.cursor().unwrap_or("-"),
            req.size(),
            req.fetch_limit(),
        )
    }

    async fn fetch_cursor(uri: &str) -> (StatusCode, String) {
        let app = Router::new().route("/feed", get(cursor_echo));
        let res = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn cursor_extractor_uses_defaults_when_query_missing() {
        let (status, body) = fetch_cursor("/feed").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            format!("-|{DEFAULT_PAGE_SIZE}|{}", DEFAULT_PAGE_SIZE + 1)
        );
    }

    #[tokio::test]
    async fn cursor_extractor_parses_cursor_and_size() {
        let (status, body) = fetch_cursor("/feed?cursor=abc123&size=5").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "abc123|5|6");
    }

    #[tokio::test]
    async fn cursor_extractor_clamps_size_over_max() {
        let (status, body) = fetch_cursor("/feed?cursor=t&size=9999").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, format!("t|{MAX_PAGE_SIZE}|{}", MAX_PAGE_SIZE + 1));

        // Exact MAX_PAGE_SIZE
        let (status, body) = fetch_cursor(&format!("/feed?cursor=t&size={MAX_PAGE_SIZE}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, format!("t|{MAX_PAGE_SIZE}|{}", MAX_PAGE_SIZE + 1));

        // MAX_PAGE_SIZE + 1
        let size = MAX_PAGE_SIZE + 1;
        let (status, body) = fetch_cursor(&format!("/feed?cursor=t&size={size}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, format!("t|{MAX_PAGE_SIZE}|{}", MAX_PAGE_SIZE + 1));
    }

    #[tokio::test]
    async fn cursor_extractor_ignores_empty_cursor() {
        // Empty `cursor=` should be treated as "no cursor", not as `Some("")`.
        let (status, body) = fetch_cursor("/feed?cursor=&size=10").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "-|10|11");
    }

    #[tokio::test]
    async fn cursor_extractor_does_not_400_on_malformed_size() {
        let (status, body) = fetch_cursor("/feed?cursor=t&size=abc").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            format!("t|{DEFAULT_PAGE_SIZE}|{}", DEFAULT_PAGE_SIZE + 1)
        );
    }

    #[tokio::test]
    async fn cursor_extractor_handles_percent_encoded_token() {
        // base64url tokens never need percent-encoding, but a paranoid
        // client might do it anyway. `%2D` decodes to `-`.
        let (status, body) = fetch_cursor("/feed?cursor=ab%2Dcd&size=2").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ab-cd|2|3");
    }

    // ── Concurrent-insert simulation ───────────────────────────
    //
    // The story's success metric is "zero duplicate items during
    // concurrent inserts". This test simulates a feed where a new row
    // arrives between the first and second page request, and verifies
    // the keyset filter still returns every original row exactly once.

    #[test]
    fn concurrent_inserts_do_not_cause_duplicates() {
        // Sort by (created_at desc, id desc) — this is the recommended
        // tie-breaker pattern from the docs.
        #[derive(Clone, Debug, PartialEq, Eq)]
        struct Row {
            id: i64,
            created_at: i64,
        }
        #[derive(Serialize, Deserialize)]
        struct Key {
            created_at: i64,
            id: i64,
        }

        let mut table: Vec<Row> = (1..=5)
            .map(|id| Row {
                id,
                created_at: 1_000 - id, // older as id grows
            })
            .collect();
        // Sort newest-first so id=1 is first.
        table.sort_by_key(|r| std::cmp::Reverse((r.created_at, r.id)));

        // First request: no cursor, size=2.
        let req1 = CursorRequest::new(None, 2);
        let fetch1 = usize::try_from(req1.fetch_limit()).unwrap();
        let fetched1: Vec<Row> = table.iter().take(fetch1).cloned().collect();
        let page1 = CursorPage::from_overfetched(fetched1, &req1, |r| Key {
            created_at: r.created_at,
            id: r.id,
        });
        let cursor1 = page1.next_cursor.clone().expect("page 1 has next");
        assert_eq!(page1.content.len(), 2);

        // ── Concurrent insert lands BEFORE the next request ──────
        // A new row is inserted with the highest created_at, so it
        // would appear on a fresh page 1 — but our cursor pagination
        // is keyset-based, so the second request must skip it.
        table.insert(
            0,
            Row {
                id: 99,
                created_at: 9_999,
            },
        );

        // Second request: cursor from page 1, size=2.
        let req2 = CursorRequest::new(Some(cursor1), 2);
        let key: Key = req2.decode().unwrap();
        let fetch2 = usize::try_from(req2.fetch_limit()).unwrap();

        // Apply the keyset filter: rows that come *after* the cursor
        // in the (created_at desc, id desc) ordering, then take the
        // overfetch window.
        let fetched2: Vec<Row> = table
            .iter()
            .filter(|r| {
                r.created_at < key.created_at || (r.created_at == key.created_at && r.id < key.id)
            })
            .take(fetch2)
            .cloned()
            .collect();
        let page2 = CursorPage::from_overfetched(fetched2, &req2, |r| Key {
            created_at: r.created_at,
            id: r.id,
        });

        // Combine the two pages. No row should appear twice, and the
        // newly-inserted id=99 must NOT show up (the user already
        // scrolled past where it would have appeared).
        let mut all: Vec<Row> = page1.content;
        all.extend(page2.content);
        let mut ids: Vec<i64> = all.iter().map(|r| r.id).collect();
        let original_len = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), original_len, "no duplicates across pages");
        assert!(
            !all.iter().any(|r| r.id == 99),
            "concurrently-inserted row not duplicated"
        );
    }

    // ── Signed cursors ─────────────────────────────────────────

    const TEST_KEY: &[u8] = b"test-signing-key-do-not-use-in-prod";

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct ScopedCursor {
        tenant_id: i64,
        cursor_id: i64,
    }

    #[test]
    fn signed_cursor_round_trip() {
        let payload = ScopedCursor {
            tenant_id: 42,
            cursor_id: 7,
        };
        let token = Cursor::encode_signed(&payload, TEST_KEY).unwrap();
        // Token shape: <payload>.<sig>
        assert!(token.contains('.'));
        let decoded: ScopedCursor = Cursor::decode_signed(&token, TEST_KEY).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn signed_cursor_rejects_tampered_payload() {
        let payload = ScopedCursor {
            tenant_id: 42,
            cursor_id: 7,
        };
        let token = Cursor::encode_signed(&payload, TEST_KEY).unwrap();
        // Forge: re-encode a payload with tenant_id=99 but reuse the
        // original signature.
        let forged_payload = ScopedCursor {
            tenant_id: 99,
            cursor_id: 7,
        };
        let forged_b64 = base64url_encode(&serde_json::to_vec(&forged_payload).unwrap());
        let (_, sig_b64) = token.split_once('.').unwrap();
        let forged_token = format!("{forged_b64}.{sig_b64}");
        let decoded: Option<ScopedCursor> = Cursor::decode_signed(&forged_token, TEST_KEY);
        assert!(decoded.is_none(), "tampered cursor must not verify");
    }

    #[test]
    fn signed_cursor_rejects_wrong_key() {
        let payload = ScopedCursor {
            tenant_id: 42,
            cursor_id: 7,
        };
        let token = Cursor::encode_signed(&payload, TEST_KEY).unwrap();
        let decoded: Option<ScopedCursor> = Cursor::decode_signed(&token, b"different-key");
        assert!(decoded.is_none());
    }

    #[test]
    fn signed_cursor_rejects_unsigned_token() {
        // A plain (unsigned) token should not verify against the
        // signed decoder — even though it's structurally valid JSON.
        let payload = ScopedCursor {
            tenant_id: 42,
            cursor_id: 7,
        };
        let unsigned = Cursor::encode(&payload).unwrap();
        let decoded: Option<ScopedCursor> = Cursor::decode_signed(&unsigned, TEST_KEY);
        assert!(
            decoded.is_none(),
            "unsigned token must not pass signed verification"
        );
    }

    #[test]
    fn signed_cursor_rejects_missing_signature_segment() {
        // Token without a `.` separator.
        let decoded: Option<ScopedCursor> = Cursor::decode_signed("just-some-bytes", TEST_KEY);
        assert!(decoded.is_none());
    }

    #[test]
    fn signed_cursor_rejects_garbage() {
        let decoded: Option<ScopedCursor> = Cursor::decode_signed("!!!.!!!", TEST_KEY);
        assert!(decoded.is_none());
    }

    #[test]
    fn cursor_request_decode_signed_returns_none_when_missing() {
        let r = CursorRequest::default();
        let decoded: Option<ScopedCursor> = r.decode_signed(TEST_KEY);
        assert!(decoded.is_none());
    }

    #[test]
    fn cursor_request_decode_signed_round_trips() {
        let payload = ScopedCursor {
            tenant_id: 42,
            cursor_id: 7,
        };
        let token = Cursor::encode_signed(&payload, TEST_KEY).unwrap();
        let r = CursorRequest::new(Some(token), 10);
        let decoded: ScopedCursor = r.decode_signed(TEST_KEY).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn cursor_page_from_overfetched_signed_emits_signed_token() {
        let req = CursorRequest::new(None, 2);
        let items = vec![1_i32, 2, 3]; // overfetch by 1
        let page = CursorPage::from_overfetched_signed(items, &req, TEST_KEY, |&n| ScopedCursor {
            tenant_id: 42,
            cursor_id: i64::from(n),
        });
        assert!(page.has_next);
        let token = page.next_cursor.as_ref().unwrap();
        assert!(token.contains('.'), "signed token format is payload.sig");
        // Round-trip through the signed decoder.
        let key: ScopedCursor = Cursor::decode_signed(token, TEST_KEY).unwrap();
        assert_eq!(key.cursor_id, 2); // boundary = last kept item
        // Plain decoder must NOT happen to extract a structurally
        // valid value, because the token contains the signature suffix.
        let mishandled: Option<ScopedCursor> = Cursor::decode(token);
        assert!(mishandled.is_none());
    }

    #[test]
    fn signed_cursor_signature_is_constant_time_compared() {
        // Smoke test: same key, same input → identical sig. Different
        // key → different sig. (Constant-time-ness itself is not
        // observable from this test; we're just exercising the path.)
        let p = ScopedCursor {
            tenant_id: 1,
            cursor_id: 1,
        };
        let a = Cursor::encode_signed(&p, b"k1").unwrap();
        let b = Cursor::encode_signed(&p, b"k1").unwrap();
        let c = Cursor::encode_signed(&p, b"k2").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[tokio::test]
    async fn page_request_extractor_defaults_on_missing_uri_query() {
        use axum::extract::FromRequestParts;
        use axum::http::Request;
        let req = Request::builder().uri("/").body(()).unwrap();
        let (mut parts, ()) = req.into_parts();

        let extracted = PageRequest::from_request_parts(&mut parts, &())
            .await
            .unwrap();

        assert_eq!(extracted.page(), 1);
        assert_eq!(extracted.size(), 20); // Default is 20
    }

    #[tokio::test]
    async fn cursor_extractor_defaults_on_missing_uri_query() {
        use axum::extract::FromRequestParts;
        use axum::http::Request;
        let req = Request::builder().uri("/").body(()).unwrap();
        let (mut parts, ()) = req.into_parts();

        let extracted = CursorRequest::from_request_parts(&mut parts, &())
            .await
            .unwrap();

        assert_eq!(extracted.size(), 20); // Default is 20
        assert!(extracted.cursor.is_none());
    }

    #[test]
    fn base64url_encode_pad_branch() {
        // 2 byte string leads to rem.len() == 2, which exercises the second padding path.
        let encoded = base64url_encode(b"ab");
        assert_eq!(encoded, "YWI");
    }

    #[test]
    fn base64url_decode_pad_branch() {
        // Try to decode exactly a length that produces rem 3.
        // YWI decodes to 'ab' (2 bytes)
        let decoded = base64url_decode("YWI").unwrap();
        assert_eq!(decoded, b"ab");
    }

    #[test]
    fn cursor_encode_fails_gracefully_on_serialization_error() {
        use serde::Serialize;

        struct FailToSerialize;

        impl Serialize for FailToSerialize {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                Err(serde::ser::Error::custom("forced failure"))
            }
        }

        let res = Cursor::encode(&FailToSerialize);
        assert!(res.is_err());

        let res_signed = Cursor::encode_signed(&FailToSerialize, b"key");
        assert!(res_signed.is_err());
    }

    // ── IntoResponse + RFC 8288 Link headers ────────────────────

    #[tokio::test]
    async fn page_into_response_emits_envelope_and_link_headers() {
        async fn handler() -> Page<u32> {
            // Page 2 of 7 (137 items, size 20): prev + next both exist.
            Page::new((21..=40).collect(), 137, &PageRequest::new(2, 20))
        }
        let app = Router::new().route("/items", get(handler));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/items")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        let link = res
            .headers()
            .get(header::LINK)
            .expect("Link header must be present")
            .to_str()
            .unwrap()
            .to_owned();
        assert!(link.contains("<?page=1&size=20>; rel=\"first\""), "{link}");
        assert!(
            link.contains("<?page=1&size=20>; rel=\"prev\""),
            "prev must point to page 1: {link}"
        );
        assert!(link.contains("<?page=3&size=20>; rel=\"next\""), "{link}");
        assert!(link.contains("<?page=7&size=20>; rel=\"last\""), "{link}");

        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["page"], 2);
        assert_eq!(json["size"], 20);
        assert_eq!(json["total_elements"], 137);
        assert_eq!(json["total_pages"], 7);
        assert_eq!(json["content"].as_array().unwrap().len(), 20);
    }

    #[tokio::test]
    async fn first_page_has_no_prev_link() {
        async fn handler() -> Page<u32> {
            Page::new((1..=20).collect(), 40, &PageRequest::new(1, 20))
        }
        let app = Router::new().route("/items", get(handler));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/items")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let link = res
            .headers()
            .get(header::LINK)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(
            !link.contains("rel=\"prev\""),
            "page 1 must not advertise prev: {link}"
        );
        assert!(link.contains("rel=\"next\""), "{link}");
        assert!(link.contains("rel=\"first\""), "{link}");
        assert!(link.contains("rel=\"last\""), "{link}");
    }

    #[test]
    fn empty_page_last_link_clamps_to_one() {
        // An empty collection has `total_pages == 0`. The `last` link must
        // still target a valid 1-indexed page (`page=1`), never `page=0`.
        let link = page_link_header_value(1, 20, 0, false);
        assert!(
            link.contains("<?page=1&size=20>; rel=\"last\""),
            "last link must clamp to page 1 for an empty collection: {link}"
        );
        assert!(
            !link.contains("page=0"),
            "no link may reference the invalid page 0: {link}"
        );
    }

    #[tokio::test]
    async fn cursor_page_emits_next_link_only() {
        async fn handler() -> CursorPage<u32> {
            // Over-fetch: request size 2, 3 rows returned → has_next, next_cursor set.
            CursorPage::from_overfetched(vec![1, 2, 3], &CursorRequest::new(None, 2), |&n| n)
        }
        let app = Router::new().route("/feed", get(handler));
        let res = app
            .oneshot(Request::builder().uri("/feed").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let link = res
            .headers()
            .get(header::LINK)
            .expect("cursor page with a next page must emit a Link")
            .to_str()
            .unwrap()
            .to_owned();
        assert!(link.contains("rel=\"next\""), "{link}");
        assert!(
            link.contains("cursor="),
            "next link must carry the cursor token: {link}"
        );
        assert!(
            !link.contains("rel=\"prev\""),
            "keyset pagination has no prev: {link}"
        );
    }

    #[tokio::test]
    async fn cursor_page_last_page_has_no_link() {
        async fn handler() -> CursorPage<u32> {
            // Fewer rows than the page size → last page, no next cursor.
            CursorPage::from_overfetched(vec![1, 2], &CursorRequest::new(None, 5), |&n| n)
        }
        let app = Router::new().route("/feed", get(handler));
        let res = app
            .oneshot(Request::builder().uri("/feed").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(
            res.headers().get(header::LINK).is_none(),
            "last cursor page must not advertise a next link"
        );
    }

    #[test]
    fn paginate_in_memory_windows_and_counts() {
        let all: Vec<u32> = (1..=25).collect();
        let page = Page::paginate_in_memory(all, &PageRequest::new(2, 10));
        assert_eq!(page.page, 2);
        assert_eq!(page.size, 10);
        assert_eq!(page.total_elements, 25);
        assert_eq!(page.total_pages, 3);
        assert_eq!(page.content, (11..=20).collect::<Vec<_>>());
        assert!(page.has_next);
        assert!(page.has_previous);
    }
}
