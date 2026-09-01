//! Reusable Maud renderers for pagination controls.
//!
//! Autumn ships pagination *data* ([`Page`](crate::pagination::Page), [`CursorPage`](crate::pagination::CursorPage)) and this module
//! ships the matching *view*: a one-line, accessible, filter-preserving,
//! htmx-ready pager you drop below any list, feed, table, or search-results
//! view. No page-window math or query-string juggling in your handlers.
//!
//! # Offset pager
//!
//! ```rust
//! use autumn_web::pagination::{Page, PageRequest};
//! use autumn_web::ui::pagination::{pagination_nav, PagerOptions};
//!
//! // A page built the way `repo.page(&req)` would return it.
//! let req = PageRequest::new(5, 10);
//! let page: Page<u32> = Page::new((41..=50).collect(), 200, &req);
//!
//! // Preserve the active filters/sort from the current request's query string.
//! let opts = PagerOptions::new("/posts").query("q=foo&sort=name");
//! let html = pagination_nav(&page, &opts).into_string();
//!
//! assert!(html.contains("<nav"));
//! assert!(html.contains(r#"aria-current="page""#));
//! // Every link keeps the existing filters:
//! assert!(html.contains("q=foo"));
//! assert!(html.contains("sort=name"));
//! ```
//!
//! # Cursor pager
//!
//! Cursor pagination has no total, so the cursor variant renders prev/next
//! affordances only — no page numbers.
//!
//! ```rust
//! use autumn_web::pagination::{CursorPage, CursorRequest};
//! use autumn_web::ui::pagination::{cursor_pagination_nav, PagerOptions};
//!
//! let req = CursorRequest::new(None, 20);
//! let page: CursorPage<u32> = CursorPage::from_overfetched((1..=21).collect(), &req, |n| *n);
//!
//! let opts = PagerOptions::new("/feed");
//! let html = cursor_pagination_nav(&page, &opts).into_string();
//! assert!(html.contains("<nav"));
//! ```
//!
//! # htmx
//!
//! htmx is opt-in via [`PagerOptions::hx_target`](crate::ui::pagination::PagerOptions::hx_target). When unset the links are
//! plain `<a href>` so pagination works with zero JavaScript
//! (progressive-enhancement default).

use crate::pagination::{CursorPage, Page};

/// One entry in a rendered page-number window: a real page or an ellipsis gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageItem {
    /// A clickable (or current) page number.
    Page(u32),
    /// A `…` gap standing in for a run of skipped pages.
    Ellipsis,
}

/// Options controlling how a pager is rendered.
///
/// Build with [`PagerOptions::new`] and chain the `const` builder methods for
/// optional overrides. Sensible defaults: `page`/`size` query params, a window
/// radius of `2`, "Previous"/"Next" labels, and no htmx (plain links).
#[derive(Debug, Clone)]
pub struct PagerOptions<'a> {
    /// Path that page links point at, without query string, e.g. `"/posts"`.
    pub base_path: &'a str,
    /// The current request's raw (already percent-encoded) query string, e.g.
    /// `"q=foo&sort=name&page=3"`. The `page`/`size` params are stripped and
    /// re-added per link; everything else (filters, sort, search) is preserved
    /// verbatim so active state survives a page click.
    pub query: &'a str,
    /// Query parameter name for the page index (default `"page"`).
    pub page_param: &'a str,
    /// Query parameter name for the page size (default `"size"`).
    pub size_param: &'a str,
    /// Query parameter name for the cursor token (default `"cursor"`).
    pub cursor_param: &'a str,
    /// How many page numbers to show on either side of the current page
    /// (default `2`). The first and last pages are always shown, with `…`
    /// gaps bridging to the window.
    pub window: u32,
    /// Emit the size param on every link (default `false` — size is usually a
    /// separate control and inheriting it from the query is enough).
    pub include_size: bool,
    /// `aria-label` for the wrapping `<nav>` (default `"Pagination"`).
    pub aria_label: &'a str,
    /// Visible text for the previous-page affordance (default `"Previous"`).
    pub prev_label: &'a str,
    /// Visible text for the next-page affordance (default `"Next"`).
    pub next_label: &'a str,
    /// CSS selector for the htmx swap target. When `Some`, links carry
    /// `hx-get`/`hx-target`; when `None` (default) they are plain `<a href>`.
    pub hx_target: Option<&'a str>,
    /// Emit `hx-push-url="true"` so htmx navigation updates the address bar
    /// (default `false`). Only meaningful when `hx_target` is set.
    pub hx_push_url: bool,
    /// Cursor token for the previous page in the cursor variant. The data model
    /// ([`CursorPage`]) is forward-only (no `has_previous`), so a back-link is
    /// rendered only when the caller supplies the prior cursor here.
    pub prev_cursor: Option<&'a str>,
}

impl<'a> PagerOptions<'a> {
    /// Create pager options for `base_path` with sensible defaults.
    #[must_use]
    pub const fn new(base_path: &'a str) -> Self {
        Self {
            base_path,
            query: "",
            page_param: "page",
            size_param: "size",
            cursor_param: "cursor",
            window: 2,
            include_size: false,
            aria_label: "Pagination",
            prev_label: "Previous",
            next_label: "Next",
            hx_target: None,
            hx_push_url: false,
            prev_cursor: None,
        }
    }

    /// Preserve `query` (the current request's raw query string) on every link.
    #[must_use]
    pub const fn query(mut self, query: &'a str) -> Self {
        self.query = query;
        self
    }

    /// Override the page-index query parameter name (default `"page"`).
    #[must_use]
    pub const fn page_param(mut self, name: &'a str) -> Self {
        self.page_param = name;
        self
    }

    /// Override the page-size query parameter name (default `"size"`).
    #[must_use]
    pub const fn size_param(mut self, name: &'a str) -> Self {
        self.size_param = name;
        self
    }

    /// Override the cursor query parameter name (default `"cursor"`).
    #[must_use]
    pub const fn cursor_param(mut self, name: &'a str) -> Self {
        self.cursor_param = name;
        self
    }

    /// Set the page-window radius — pages shown on either side of the current
    /// page (default `2`).
    #[must_use]
    pub const fn window(mut self, radius: u32) -> Self {
        self.window = radius;
        self
    }

    /// Emit the size param on every link.
    #[must_use]
    pub const fn include_size(mut self) -> Self {
        self.include_size = true;
        self
    }

    /// Set the `aria-label` for the wrapping `<nav>` (default `"Pagination"`).
    #[must_use]
    pub const fn aria_label(mut self, label: &'a str) -> Self {
        self.aria_label = label;
        self
    }

    /// Set the previous-page label (default `"Previous"`).
    #[must_use]
    pub const fn prev_label(mut self, label: &'a str) -> Self {
        self.prev_label = label;
        self
    }

    /// Set the next-page label (default `"Next"`).
    #[must_use]
    pub const fn next_label(mut self, label: &'a str) -> Self {
        self.next_label = label;
        self
    }

    /// Enable htmx: every link gets `hx-get` plus `hx-target=(target)`.
    #[must_use]
    pub const fn hx_target(mut self, target: &'a str) -> Self {
        self.hx_target = Some(target);
        self
    }

    /// Emit `hx-push-url="true"` for htmx navigation (requires [`Self::hx_target`]).
    #[must_use]
    pub const fn hx_push_url(mut self) -> Self {
        self.hx_push_url = true;
        self
    }

    /// Supply the previous-page cursor token for the cursor variant's back-link.
    #[must_use]
    pub const fn prev_cursor(mut self, cursor: &'a str) -> Self {
        self.prev_cursor = Some(cursor);
        self
    }
}

/// Compute the windowed page-number sequence for the offset pager.
///
/// Always includes page `1` and `total`, the `radius` pages on either side of
/// `current`, and inserts a [`PageItem::Ellipsis`] wherever a run of pages is
/// skipped. Returns a compact sequence like `1 … 4 5 6 … 20`.
fn page_window(current: u32, total: u32, radius: u32) -> Vec<PageItem> {
    if total <= 1 {
        return vec![PageItem::Page(1)];
    }
    let current = current.clamp(1, total);
    let lo = current.saturating_sub(radius).max(1);
    let hi = current.saturating_add(radius).min(total);

    let mut items: Vec<PageItem> =
        Vec::with_capacity((hi as usize).saturating_sub(lo as usize).saturating_add(5));

    // Prefix: page 1 when not already in the window, plus a gap indicator.
    if lo > 1 {
        items.push(PageItem::Page(1));
        if lo == 3 {
            // A single skipped page (page 2) is cheaper to show than `…`.
            items.push(PageItem::Page(2));
        } else if lo > 2 {
            items.push(PageItem::Ellipsis);
        }
        // lo == 2: pages 1 and 2 are adjacent, no gap.
    }

    for p in lo..=hi {
        items.push(PageItem::Page(p));
    }

    // Suffix: gap indicator plus `total` when not already in the window.
    if hi < total {
        if hi == total - 2 {
            items.push(PageItem::Page(total - 1));
        } else if hi < total - 1 {
            items.push(PageItem::Ellipsis);
        }
        // hi == total - 1: hi and total are adjacent, no gap.
        items.push(PageItem::Page(total));
    }

    items
}

/// Strip named params from a query string, returning the preserved remainder.
///
/// Splits `query` on `&`, drops pairs whose key matches any entry in
/// `drop_keys`, and joins the rest verbatim (already percent-encoded). The
/// leading `?` is *not* included in the input or output.
fn filter_query<'a>(query: &'a str, drop_keys: &[&str]) -> String {
    let query = query.strip_prefix('?').unwrap_or(query);
    let parts: Vec<&'a str> = query
        .split('&')
        .filter(|pair| {
            if pair.is_empty() {
                return false;
            }
            let key = pair.split('=').next().unwrap_or(pair);
            !drop_keys.contains(&key)
        })
        .collect();
    parts.join("&")
}

/// Render a single clickable page/affordance anchor, wiring htmx attributes
/// only when [`PagerOptions::hx_target`] is set (plain `<a href>` otherwise).
#[cfg(feature = "maud")]
fn anchor(href: &str, class: &str, content: &str, opts: &PagerOptions<'_>) -> maud::Markup {
    // hx-get mirrors the href, but only when htmx is opted in.
    let hx_get = opts.hx_target.map(|_| href);
    let hx_push = if opts.hx_push_url && opts.hx_target.is_some() {
        Some("true")
    } else {
        None
    };
    maud::html! {
        a
            class=(class)
            href=(href)
            hx-get=[hx_get]
            hx-target=[opts.hx_target]
            hx-push-url=[hx_push] {
            (content)
        }
    }
}

/// Render an accessible, filter-preserving pager from an offset [`Page`].
///
/// Emits a `<nav>` containing previous/next affordances and a windowed
/// page-number sequence (`1 … 4 5 6 … 20`). The active page carries
/// `aria-current="page"`; disabled prev/next render as non-focusable
/// `aria-disabled` spans. Existing query-string state (filters, sort, search)
/// is preserved on every link; only the page/size params are swapped. htmx is
/// opt-in via [`PagerOptions::hx_target`].
///
/// See the [module docs](self) for a full example.
#[cfg(feature = "maud")]
#[must_use]
pub fn pagination_nav<T>(page: &Page<T>, opts: &PagerOptions<'_>) -> maud::Markup {
    // Pre-compute the invariant filter prefix once; each page link only appends
    // the new page number, avoiding O(window) repeated query-string parses.
    let filtered = filter_query(opts.query, &[opts.page_param, opts.size_param]);
    let prefix = if filtered.is_empty() {
        String::new()
    } else {
        format!("{filtered}&")
    };

    let href = |p: u32| -> String {
        if opts.include_size {
            format!(
                "{}?{}{}={p}&{}={}",
                opts.base_path, prefix, opts.page_param, opts.size_param, page.size
            )
        } else {
            format!("{}?{}{}={p}", opts.base_path, prefix, opts.page_param)
        }
    };

    maud::html! {
        nav aria-label=(opts.aria_label) class="autumn-pager" {
            @if page.has_previous {
                (anchor(&href(page.page.saturating_sub(1)), "autumn-pager__link autumn-pager__prev", opts.prev_label, opts))
            } @else {
                span class="autumn-pager__prev autumn-pager__disabled" aria-disabled="true" {
                    (opts.prev_label)
                }
            }
            @for item in page_window(page.page, page.total_pages, opts.window) {
                @match item {
                    PageItem::Page(p) => {
                        @if p == page.page {
                            span class="autumn-pager__current" aria-current="page" { (p) }
                        } @else {
                            (anchor(&href(p), "autumn-pager__link", &p.to_string(), opts))
                        }
                    }
                    PageItem::Ellipsis => {
                        span class="autumn-pager__ellipsis" aria-hidden="true" { "…" }
                    }
                }
            }
            @if page.has_next {
                (anchor(&href(page.page + 1), "autumn-pager__link autumn-pager__next", opts.next_label, opts))
            } @else {
                span class="autumn-pager__next autumn-pager__disabled" aria-disabled="true" {
                    (opts.next_label)
                }
            }
        }
    }
}

/// Render a prev/next pager from a cursor [`CursorPage`].
///
/// Cursor pagination has no total, so no page numbers are emitted — only
/// previous/next affordances. The next link is built from
/// [`CursorPage::next_cursor`]; it is disabled (non-focusable) when there is no
/// next page. A previous link is rendered only when
/// [`PagerOptions::prev_cursor`] is supplied (the data model is forward-only).
///
/// See the [module docs](self) for a full example.
#[cfg(feature = "maud")]
#[must_use]
pub fn cursor_pagination_nav<T>(page: &CursorPage<T>, opts: &PagerOptions<'_>) -> maud::Markup {
    let cursor_filtered = filter_query(
        opts.query,
        &[opts.cursor_param, opts.page_param, opts.size_param],
    );
    let cursor_prefix = if cursor_filtered.is_empty() {
        String::new()
    } else {
        format!("{cursor_filtered}&")
    };
    let cursor_href = |token: &str| -> String {
        format!(
            "{}?{}{}={token}",
            opts.base_path, cursor_prefix, opts.cursor_param
        )
    };
    // Render the Next link whenever a cursor token is available; no token → disabled.
    let next_token = page.next_cursor.as_deref();

    maud::html! {
        nav aria-label=(opts.aria_label) class="autumn-pager autumn-pager--cursor" {
            // ── Previous (caller-supplied; data model is forward-only) ─
            @if let Some(prev) = opts.prev_cursor {
                (anchor(&cursor_href(prev), "autumn-pager__link autumn-pager__prev", opts.prev_label, opts))
            } @else {
                span class="autumn-pager__prev autumn-pager__disabled" aria-disabled="true" {
                    (opts.prev_label)
                }
            }
            // ── Next ──────────────────────────────────────────────────
            @if let Some(token) = next_token {
                (anchor(&cursor_href(token), "autumn-pager__link autumn-pager__next", opts.next_label, opts))
            } @else {
                span class="autumn-pager__next autumn-pager__disabled" aria-disabled="true" {
                    (opts.next_label)
                }
            }
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "maud"))]
mod tests {
    use super::*;
    use crate::pagination::{CursorPage, CursorRequest, Page, PageRequest};

    fn offset_page(page: u32, size: u32, total: i64) -> Page<u32> {
        let req = PageRequest::new(page, size);
        Page::new(Vec::new(), total, &req)
    }

    // ── page_window ────────────────────────────────────────────────────

    #[test]
    fn window_single_page() {
        assert_eq!(page_window(1, 1, 2), vec![PageItem::Page(1)]);
    }

    #[test]
    fn filter_query_preserves_empty_keys_that_are_unlisted() {
        let q = filter_query("=value&page=2", &["page"]);
        assert_eq!(q, "=value");
    }

    #[test]
    fn window_small_total_has_no_ellipsis() {
        // 1..=5 all fit; no gaps.
        let items = page_window(3, 5, 2);
        assert_eq!(
            items,
            vec![
                PageItem::Page(1),
                PageItem::Page(2),
                PageItem::Page(3),
                PageItem::Page(4),
                PageItem::Page(5),
            ]
        );
    }

    #[test]
    fn window_lo_is_two_includes_page_one_without_ellipsis_or_two() {
        let items = page_window(2, 5, 0); // current=2, radius=0 => lo=2, hi=2
        // It should render: 1 (prefix), 2 (for loop), ..., 5 (suffix)
        let expected = vec![
            PageItem::Page(1),
            PageItem::Page(2),
            PageItem::Ellipsis,
            PageItem::Page(5),
        ];
        assert_eq!(items, expected);
    }

    #[test]
    fn window_hi_is_total_minus_one_includes_total_without_ellipsis_or_total_minus_two() {
        let items = page_window(4, 5, 0); // current=4, radius=0 => lo=4, hi=4
        // It should render: 1, ..., 4, 5
        let expected = vec![
            PageItem::Page(1),
            PageItem::Ellipsis,
            PageItem::Page(4),
            PageItem::Page(5),
        ];
        assert_eq!(items, expected);
    }

    #[test]
    fn window_first_and_last_always_present() {
        let items = page_window(10, 20, 2);
        assert_eq!(items.first(), Some(&PageItem::Page(1)));
        assert_eq!(items.last(), Some(&PageItem::Page(20)));
    }

    #[test]
    fn window_middle_has_ellipses_both_sides() {
        // current 10 of 20, radius 2 => 1 … 8 9 10 11 12 … 20
        let items = page_window(10, 20, 2);
        assert_eq!(
            items,
            vec![
                PageItem::Page(1),
                PageItem::Ellipsis,
                PageItem::Page(8),
                PageItem::Page(9),
                PageItem::Page(10),
                PageItem::Page(11),
                PageItem::Page(12),
                PageItem::Ellipsis,
                PageItem::Page(20),
            ]
        );
    }

    #[test]
    fn window_no_ellipsis_for_single_gap() {
        // A gap of exactly one page is filled with that page, not an ellipsis.
        // current 4 of 6, radius 1 => 1 [gap=2] 3 4 5 6 ; gap between 1 and 3
        // is a single page (2) so it is shown instead of `…`.
        let items = page_window(4, 6, 1);
        assert!(!items.contains(&PageItem::Ellipsis), "{items:?}");
        assert!(items.contains(&PageItem::Page(2)), "{items:?}");
    }

    #[test]
    fn window_no_adjacent_duplicate_pages() {
        let items = page_window(2, 20, 2);
        // page 1 should appear exactly once even though the window starts low.
        let ones = items.iter().filter(|i| **i == PageItem::Page(1)).count();
        assert_eq!(ones, 1, "{items:?}");
    }

    #[test]
    fn window_returns_empty_or_one_when_radius_is_zero_and_total_is_zero_or_one() {
        assert_eq!(page_window(1, 0, 0), vec![PageItem::Page(1)]);
        assert_eq!(page_window(1, 1, 0), vec![PageItem::Page(1)]);
    }

    // ── filter_query ───────────────────────────────────────────────────

    #[test]
    fn filter_query_drops_named_params_and_preserves_rest() {
        let q = filter_query("q=foo&page=3&sort=name", &["page", "size"]);
        assert_eq!(q, "q=foo&sort=name");
    }

    #[test]
    fn filter_query_drops_all_listed_keys() {
        let q = filter_query("q=foo&page=1&size=25", &["page", "size"]);
        assert_eq!(q, "q=foo");
    }

    #[test]
    fn filter_query_handles_empty_query() {
        assert_eq!(filter_query("", &["page"]), "");
    }

    #[test]
    fn filter_query_handles_query_with_only_stripped_params() {
        assert_eq!(filter_query("page=5&size=10", &["page", "size"]), "");
    }

    #[test]
    fn filter_query_strips_leading_question_mark() {
        let q = filter_query("?q=foo&page=3", &["page"]);
        assert_eq!(q, "q=foo");
    }
    #[test]
    fn filter_query_preserves_unlisted_keys() {
        let q = filter_query("a=1&b=2&c=3", &["b"]);
        assert_eq!(q, "a=1&c=3");
    }

    #[test]
    fn window_total_u32_max_does_not_panic() {
        // Must not overflow in debug mode.
        let _ = page_window(1, u32::MAX, 2);
    }
    #[test]
    fn window_total_zero_returns_single_page() {
        let items = page_window(1, 0, 2);
        assert_eq!(items, vec![PageItem::Page(1)]);
    }

    // include_size is opt-in: verify via pagination_nav output.
    #[test]
    fn offset_include_size_appends_size_param_to_links() {
        let page = offset_page(2, 25, 100);
        let opts = PagerOptions::new("/posts").include_size();
        let html = pagination_nav(&page, &opts).into_string();
        assert!(html.contains("size=25"), "size param missing: {html}");
        // size= should not appear more than once per link (no duplication).
        let hrefs: Vec<&str> = html
            .match_indices("href=\"")
            .map(|(i, _)| &html[i..])
            .collect();
        for h in &hrefs {
            let end = h[6..].find('"').map_or(h.len(), |e| 6 + e);
            let href = &h[6..end];
            assert!(
                href.matches("size=").count() <= 1,
                "duplicate size= in {href}"
            );
        }
    }

    // ── pagination_nav (offset) ────────────────────────────────────────

    #[test]
    fn offset_renders_nav_with_aria_label() {
        let page = offset_page(2, 10, 100);
        let opts = PagerOptions::new("/posts").aria_label("Posts pagination");
        let html = pagination_nav(&page, &opts).into_string();
        assert!(html.contains("<nav"), "{html}");
        assert!(html.contains(r#"aria-label="Posts pagination""#), "{html}");
    }

    #[test]
    fn offset_active_page_has_aria_current() {
        let page = offset_page(3, 10, 100);
        let opts = PagerOptions::new("/posts");
        let html = pagination_nav(&page, &opts).into_string();
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    #[test]
    fn offset_disabled_prev_is_non_focusable_on_first_page() {
        let page = offset_page(1, 10, 100);
        let opts = PagerOptions::new("/posts");
        let html = pagination_nav(&page, &opts).into_string();
        // The disabled "Previous" must be a span with aria-disabled and no href,
        // so it cannot receive keyboard focus.
        assert!(html.contains(r#"aria-disabled="true""#), "{html}");
        // There must be no anchor whose text is the prev label on page 1.
        assert!(
            !html.contains(r#"href="/posts?page=0"#),
            "prev link must not point at page 0: {html}"
        );
    }

    #[test]
    fn offset_disabled_next_on_last_page() {
        let page = offset_page(10, 10, 100); // 10 pages, on the last
        let opts = PagerOptions::new("/posts");
        let html = pagination_nav(&page, &opts).into_string();
        assert!(html.contains(r#"aria-disabled="true""#), "{html}");
        assert!(!html.contains("page=11"), "{html}");
    }

    #[test]
    fn offset_preserves_query_on_every_link() {
        let page = offset_page(5, 10, 200); // 20 pages
        let opts = PagerOptions::new("/posts").query("q=foo&sort=name");
        let html = pagination_nav(&page, &opts).into_string();
        // Maud escapes & to &amp; in attributes; assert both filters survive
        // on each emitted href. Every href must carry q=foo and sort=name.
        let hrefs: Vec<&str> = html
            .match_indices("href=\"")
            .map(|(i, _)| &html[i..])
            .collect();
        assert!(!hrefs.is_empty(), "expected page links: {html}");
        for h in &hrefs {
            let end = h[6..].find('"').map_or(h.len(), |e| 6 + e);
            let href = &h[6..end];
            assert!(href.contains("q=foo"), "missing q in {href}");
            assert!(href.contains("sort=name"), "missing sort in {href}");
        }
    }

    #[test]
    fn offset_emits_windowed_numbers_with_ellipsis() {
        let page = offset_page(10, 10, 200); // 20 pages, middle
        let opts = PagerOptions::new("/posts");
        let html = pagination_nav(&page, &opts).into_string();
        assert!(html.contains('…'), "{html}");
        assert!(html.contains(">1<"), "first page anchor: {html}");
        assert!(html.contains(">20<"), "last page anchor: {html}");
    }

    #[test]
    fn offset_no_htmx_by_default() {
        let page = offset_page(2, 10, 100);
        let opts = PagerOptions::new("/posts");
        let html = pagination_nav(&page, &opts).into_string();
        assert!(!html.contains("hx-get"), "{html}");
        assert!(html.contains("href="), "plain links expected: {html}");
    }

    #[test]
    fn offset_htmx_opt_in_emits_hx_get_and_target() {
        let page = offset_page(2, 10, 100);
        let opts = PagerOptions::new("/posts").hx_target("#list").hx_push_url();
        let html = pagination_nav(&page, &opts).into_string();
        assert!(html.contains("hx-get="), "{html}");
        assert!(html.contains(r##"hx-target="#list""##), "{html}");
        assert!(html.contains(r#"hx-push-url="true""#), "{html}");
    }

    #[test]
    fn offset_custom_page_param() {
        let page = offset_page(2, 10, 100);
        let opts = PagerOptions::new("/admin/users").page_param("p");
        let html = pagination_nav(&page, &opts).into_string();
        assert!(html.contains("p=1"), "{html}");
        assert!(!html.contains("page="), "{html}");
    }

    #[test]
    fn offset_single_page_renders_nav_without_links() {
        let page = offset_page(1, 10, 5); // only 1 page
        let opts = PagerOptions::new("/posts");
        let html = pagination_nav(&page, &opts).into_string();
        assert!(html.contains("<nav"), "{html}");
        // current page 1 is marked current; no other page anchors.
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    // ── cursor_pagination_nav ──────────────────────────────────────────

    fn cursor_page(size: u32, overfetch: u32) -> CursorPage<u32> {
        let req = CursorRequest::new(None, size);
        let items: Vec<u32> = (1..=overfetch).collect();
        CursorPage::from_overfetched(items, &req, |n| *n)
    }

    #[test]
    fn cursor_renders_nav() {
        let page = cursor_page(20, 21);
        let opts = PagerOptions::new("/feed");
        let html = cursor_pagination_nav(&page, &opts).into_string();
        assert!(html.contains("<nav"), "{html}");
    }

    #[test]
    fn cursor_has_no_page_numbers() {
        let page = cursor_page(20, 21);
        let opts = PagerOptions::new("/feed");
        let html = cursor_pagination_nav(&page, &opts).into_string();
        assert!(!html.contains("aria-current"), "{html}");
        assert!(!html.contains('…'), "{html}");
    }

    #[test]
    fn cursor_next_uses_next_cursor_when_has_next() {
        let page = cursor_page(20, 21); // overfetched => has_next
        assert!(page.has_next);
        let token = page.next_cursor.clone().unwrap();
        let opts = PagerOptions::new("/feed");
        let html = cursor_pagination_nav(&page, &opts).into_string();
        assert!(html.contains("cursor="), "{html}");
        assert!(html.contains(&token), "next token in link: {html}");
    }

    #[test]
    fn cursor_next_disabled_on_last_page() {
        let page = cursor_page(20, 10); // fewer than size => no next
        assert!(!page.has_next);
        let opts = PagerOptions::new("/feed");
        let html = cursor_pagination_nav(&page, &opts).into_string();
        assert!(html.contains(r#"aria-disabled="true""#), "{html}");
    }

    #[test]
    fn cursor_prev_link_only_when_prev_cursor_supplied() {
        let page = cursor_page(20, 21);
        let opts = PagerOptions::new("/feed").prev_cursor("PREVTOK");
        let html = cursor_pagination_nav(&page, &opts).into_string();
        assert!(html.contains("PREVTOK"), "{html}");
    }
}
