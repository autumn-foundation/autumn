//! Data display widgets: property list, data table, badge, avatar,
//! avatar group and kbd.

use super::{assert_named, fnv1a_hash, merge_class};

// ── property_list ─────────────────────────────────────────────────────────

/// Render a property list (definition list) for a record's fields.
///
/// Emits a `<dl class="autumn-property-list">` with one `<dt>`/`<dd>` pair per
/// entry. Labels (`&str`) are HTML-escaped; values are pre-escaped [`Markup`](maud::Markup)
/// so callers can pass plain text, a formatted date, or a nested link/badge
/// without escaping foot-guns.
///
/// An empty `rows` slice renders an empty `<dl>`, never panics.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-property-list` | The `<dl>` wrapper |
/// | `.autumn-property-list dt` | Label term |
/// | `.autumn-property-list dd` | Value description |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::property_list;
/// use maud::html;
///
/// struct Post { id: i64, title: String, published: bool }
/// let post = Post { id: 1, title: "Hello".into(), published: true };
///
/// let rows = vec![
///     ("Id", html! { (post.id) }),
///     ("Title", html! { (&post.title) }),
///     ("Published", html! { (post.published.to_string()) }),
/// ];
/// let markup = property_list(&rows);
/// let html = markup.into_string();
/// assert!(html.contains(r#"class="autumn-property-list""#));
/// assert!(html.contains("<dt>Id</dt>"));
/// assert!(html.contains("<dd>1</dd>"));
/// ```
#[must_use]
pub fn property_list(rows: &[(&str, maud::Markup)]) -> maud::Markup {
    maud::html! {
        dl class="autumn-property-list" {
            @for (label, value) in rows {
                dt { (label) }
                dd { (value) }
            }
        }
    }
}

// ── data_table ────────────────────────────────────────────────────────────

/// Sort direction for a [`data_table`] sortable column header.
///
/// Re-exported from [`crate::pagination`] so the widget layer and the
/// `ListQuery`/`list()` query layer share a single canonical type — a
/// `data_table` renders exactly the direction the repository applies.
pub use crate::pagination::SortDir;

/// A single column definition for [`data_table`].
///
/// Each column carries a header label, an optional sort key (opt-in via
/// [`Column::sortable`]), and a cell closure that maps a row reference to
/// `Markup`.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::widgets::{Column, data_table, DataTableConfig};
///
/// struct Post { id: i64, title: String }
///
/// let cols: Vec<Column<Post>> = vec![
///     Column::new("ID", |row| html! { (row.id) }),
///     Column::new("Title", |row| html! { (row.title.as_str()) })
///         .sortable("title"),
/// ];
/// ```
pub struct Column<'a, T> {
    /// Column header label shown in `<th>`.
    pub header: &'a str,
    /// If `Some`, this column is sortable and the value is the `sort=` query param.
    pub sort_key: Option<&'a str>,
    /// Cell renderer: maps a row reference to rendered `Markup`.
    pub cell: Box<dyn Fn(&T) -> maud::Markup + Send + 'a>,
}

impl<'a, T> Column<'a, T> {
    /// Create a new non-sortable column with a header label and cell closure.
    #[must_use]
    pub fn new(header: &'a str, cell: impl Fn(&T) -> maud::Markup + Send + 'a) -> Self {
        Self {
            header,
            sort_key: None,
            cell: Box::new(cell),
        }
    }

    /// Make this column sortable, linking the header with `sort={sort_key}` query params.
    ///
    /// The server owns the actual ordering; the widget only renders the link.
    #[must_use]
    pub const fn sortable(mut self, sort_key: &'a str) -> Self {
        self.sort_key = Some(sort_key);
        self
    }
}

/// Configuration for a [`data_table`] widget.
///
/// Build with [`DataTableConfig::new`] and chain builder methods for optional overrides.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::widgets::DataTableConfig;
///
/// let config = DataTableConfig::new("No posts found.")
///     .base_path("/posts")
///     .query("q=foo")
///     .active_sort("title")
///     .caption("Posts");
/// ```
#[derive(Debug, Clone)]
pub struct DataTableConfig<'a> {
    /// Optional `<caption>` for the table (table name/description, not the empty state).
    pub caption: Option<&'a str>,
    /// Message shown when `rows` is empty (required).
    pub empty_message: &'a str,
    /// Base path for sort links, e.g. `"/posts"`.
    pub base_path: &'a str,
    /// Raw query string of the current request (preserved on sort links).
    pub query: &'a str,
    /// Query parameter name for the sort column (default `"sort"`).
    pub sort_param: &'a str,
    /// Query parameter name for the sort direction (default `"dir"`).
    pub dir_param: &'a str,
    /// Query parameter name for the page (stripped on sort, default `"page"`).
    pub page_param: &'a str,
    /// The currently active sort column key, if any.
    pub active_sort: Option<&'a str>,
    /// Direction of the active sort (default [`SortDir::Asc`]).
    pub active_dir: SortDir,
    /// Optional CSS class to add to the `<table>` element.
    pub class: Option<&'a str>,
}

impl<'a> DataTableConfig<'a> {
    /// Create a new config with the given empty-state message and sensible defaults.
    #[must_use]
    pub const fn new(empty_message: &'a str) -> Self {
        Self {
            caption: None,
            empty_message,
            base_path: "",
            query: "",
            sort_param: "sort",
            dir_param: "dir",
            page_param: "page",
            active_sort: None,
            active_dir: SortDir::Asc,
            class: None,
        }
    }

    /// Set the table `<caption>` (table name; use for accessibility, not for empty state).
    #[must_use]
    pub const fn caption(mut self, caption: &'a str) -> Self {
        self.caption = Some(caption);
        self
    }

    /// Set the base path for sort links (default `""`).
    #[must_use]
    pub const fn base_path(mut self, base_path: &'a str) -> Self {
        self.base_path = base_path;
        self
    }

    /// Preserve the current request query string on sort links.
    #[must_use]
    pub const fn query(mut self, query: &'a str) -> Self {
        self.query = query;
        self
    }

    /// Set the active sort column key.
    #[must_use]
    pub const fn active_sort(mut self, key: &'a str) -> Self {
        self.active_sort = Some(key);
        self
    }

    /// Set the active sort direction.
    #[must_use]
    pub const fn active_dir(mut self, dir: SortDir) -> Self {
        self.active_dir = dir;
        self
    }

    /// Add a CSS class to the `<table>` element.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Strip named params from a raw query string, returning the preserved remainder.
///
/// Intentional duplication of the same helper in `ui::pagination` — that one is
/// private to its module. Both are small enough that sharing outweighs coupling.
fn dt_filter_query(query: &str, drop_keys: &[&str]) -> String {
    let query = query.strip_prefix('?').unwrap_or(query);
    query
        .split('&')
        .filter(|pair| {
            if pair.is_empty() {
                return false;
            }
            let key = pair.split('=').next().unwrap_or(pair);
            !drop_keys.contains(&key)
        })
        .collect::<Vec<&str>>()
        .join("&")
}

/// Render a column-driven data table from a slice of records.
///
/// Emits a semantic, accessible `<table>` with a `<thead>` row of `<th scope="col">`
/// headers and a `<tbody>` of `<tr>`/`<td>` cells. Empty rows render a single
/// placeholder row spanning all columns. Sortable columns (opt-in via
/// [`Column::sortable`]) carry `aria-sort` on the `<th>` and a link that toggles
/// direction while preserving the current query string (sort resets pagination).
///
/// # Composes with `active_search` and `pagination_nav`
///
/// ```rust
/// # use autumn_web::pagination::{Page, PageRequest};
/// # use autumn_web::ui::pagination::{pagination_nav, PagerOptions};
/// # use autumn_web::widgets::{Column, DataTableConfig, data_table};
/// # struct Post { id: i64, title: String }
/// # let req = PageRequest::new(1, 10);
/// # let rows: Vec<Post> = vec![];
/// # let page: Page<Post> = Page::new(rows, 0, &req);
/// let cols: Vec<Column<Post>> = vec![
///     Column::new("ID", |row: &Post| maud::html! { (row.id) }),
///     Column::new("Title", |row: &Post| maud::html! { (row.title.as_str()) }),
/// ];
/// let html = maud::html! {
///     (data_table(&page.content, &cols, &DataTableConfig::new("No posts yet.").base_path("/posts")))
///     (pagination_nav(&page, &PagerOptions::new("/posts")))
/// };
/// assert!(html.into_string().contains("<table"));
/// ```
///
/// # Example with sortable columns
///
/// ```rust,ignore
/// use autumn_web::prelude::*;
/// use autumn_web::widgets::{Column, DataTableConfig, SortDir, data_table};
/// use serde::Deserialize;
///
/// #[derive(Deserialize)]
/// struct SortQuery { sort: Option<String>, dir: Option<String> }
///
/// #[get("/posts")]
/// async fn index(
///     Query(q): Query<SortQuery>,
///     page_req: PageRequest,
///     repo: PgPostRepository,
/// ) -> AutumnResult<Markup> {
///     let active_dir = if q.dir.as_deref() == Some("desc") { SortDir::Desc } else { SortDir::Asc };
///     let page_data = repo.page(&page_req).await?;
///     let cols: Vec<Column<Post>> = vec![
///         Column::new("Title", |row| html! { (row.title.as_str()) }).sortable("title"),
///         Column::new("", |row| html! { a href=(format!("/posts/{}", row.id)) { "Show" } }),
///     ];
///     let config = DataTableConfig::new("No posts yet.")
///         .base_path("/posts")
///         .active_sort(q.sort.as_deref().unwrap_or(""))
///         .active_dir(active_dir);
///     Ok(layout("Posts", html! {
///         (data_table(&page_data.content, &cols, &config))
///         (pagination_nav(&page_data, &PagerOptions::new("/posts")))
///     }))
/// }
/// ```
#[must_use]
pub fn data_table<T>(
    rows: &[T],
    columns: &[Column<'_, T>],
    config: &DataTableConfig<'_>,
) -> maud::Markup {
    let col_count = columns.len();
    // Hoist loop-invariant: query/drop-keys are the same for every sortable column.
    let filtered = dt_filter_query(
        config.query,
        &[config.sort_param, config.dir_param, config.page_param],
    );

    maud::html! {
        table class=[config.class] {
            @if let Some(caption) = config.caption {
                caption { (caption) }
            }
            thead {
                tr {
                    @for col in columns {
                        @if let Some(sort_key) = col.sort_key {
                            // Determine sort direction for this column's link.
                            @let is_active = config.active_sort == Some(sort_key);
                            @let link_dir = if is_active {
                                config.active_dir.toggled()
                            } else {
                                SortDir::Asc
                            };
                            @let aria_sort = if is_active {
                                config.active_dir.aria_value()
                            } else {
                                "none"
                            };
                            @let href = if filtered.is_empty() {
                                format!(
                                    "{}?{}={}&{}={}",
                                    config.base_path,
                                    config.sort_param,
                                    sort_key,
                                    config.dir_param,
                                    link_dir.param_value(),
                                )
                            } else {
                                format!(
                                    "{}?{}&{}={}&{}={}",
                                    config.base_path,
                                    filtered,
                                    config.sort_param,
                                    sort_key,
                                    config.dir_param,
                                    link_dir.param_value(),
                                )
                            };
                            th scope="col" aria-sort=(aria_sort) {
                                a href=(href) { (col.header) }
                            }
                        } @else {
                            th scope="col" { (col.header) }
                        }
                    }
                }
            }
            tbody {
                @if rows.is_empty() {
                    tr {
                        td colspan=(col_count) {
                            span role="status" { (config.empty_message) }
                        }
                    }
                } @else {
                    @for row in rows {
                        tr {
                            @for col in columns {
                                td { ((col.cell)(row)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ── badge / status_tag (#1259) ──────────────────────────────────────────────

/// Semantic color for a [`badge`] / [`status_tag`].
///
/// Each variant emits a stable, documented class (`autumn-badge--<variant>`) so the
/// framework stylesheet themes it; there are no inline styles. Color is never
/// the *only* signal — the badge's text label always communicates the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BadgeVariant {
    /// Grey, no strong connotation — the default (e.g. "archived", "draft").
    #[default]
    Neutral,
    /// Blue, informational (e.g. "new", "open").
    Info,
    /// Green, positive (e.g. "published", "active", "paid").
    Success,
    /// Amber, needs attention (e.g. "pending review", "trial").
    Warning,
    /// Red, negative (e.g. "failed", "rejected", "overdue").
    Danger,
}

impl BadgeVariant {
    /// The variant name as a lowercase string (matches the CSS class suffix).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Danger => "danger",
        }
    }

    /// Map an arbitrary status string to a deterministic variant.
    ///
    /// Common status vocabularies (`published`, `active`, `failed`, …) map to
    /// their natural color; any other string is hashed to one of the colored
    /// variants so an enum/status column gets a **stable** color per value
    /// without the author enumerating every case. The mapping is
    /// case-insensitive and whitespace-trimmed, and is guaranteed
    /// deterministic: the same input always yields the same variant.
    #[must_use]
    pub fn for_label(label: &str) -> Self {
        match label.trim().to_ascii_lowercase().as_str() {
            "success" | "active" | "published" | "done" | "complete" | "completed" | "approved"
            | "enabled" | "online" | "paid" | "live" | "ok" | "passed" | "ready" => Self::Success,
            "info" | "new" | "open" | "in_progress" | "in progress" | "processing" | "queued"
            | "running" | "scheduled" => Self::Info,
            "warning" | "warn" | "draft" | "pending" | "pending_review" | "review" | "paused"
            | "trial" | "expiring" | "on_hold" => Self::Warning,
            "danger" | "error" | "failed" | "failure" | "rejected" | "cancelled" | "canceled"
            | "disabled" | "offline" | "overdue" | "banned" | "deleted" | "blocked" => Self::Danger,
            "" | "neutral" | "inactive" | "unknown" | "archived" | "closed" | "none" => {
                Self::Neutral
            }
            other => {
                // Deterministic fallback over the colored (non-neutral) set.
                const SET: [BadgeVariant; 4] = [
                    BadgeVariant::Info,
                    BadgeVariant::Success,
                    BadgeVariant::Warning,
                    BadgeVariant::Danger,
                ];
                SET[(fnv1a_hash(other) % 4) as usize]
            }
        }
    }
}

/// Rendering options for [`badge_with`].
#[derive(Debug, Clone, Default)]
pub struct BadgeConfig<'a> {
    title: Option<&'a str>,
    aria_label: Option<&'a str>,
    class: Option<&'a str>,
}

impl<'a> BadgeConfig<'a> {
    /// A default config: no `title`, no `aria-label`, no extra class.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a `title` — useful when the visible label is abbreviated.
    ///
    /// Keyboard and touch users cannot see a `title`. For an accessible
    /// tooltip on a focusable element, use [`tooltip`].
    #[must_use]
    pub const fn title(mut self, title: &'a str) -> Self {
        self.title = Some(title);
        self
    }

    /// Set an `aria-label` — the accessible name announced for an abbreviated
    /// or icon-only label.
    #[must_use]
    pub const fn aria_label(mut self, label: &'a str) -> Self {
        self.aria_label = Some(label);
        self
    }

    /// Append an extra class to the root `<span>`.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Render a small semantic status pill.
///
/// Emits `<span class="autumn-badge autumn-badge--<variant>">label</span>`. The label is
/// text content (screen-reader legible) and HTML-escaped by Maud; color is
/// never the sole signal. Style the `.autumn-badge` / `.autumn-badge--*` classes via the
/// framework stylesheet — no inline styles are emitted.
///
/// For a tooltip / accessible name on an abbreviated label, use
/// [`badge_with`] with [`BadgeConfig::title`] / [`BadgeConfig::aria_label`].
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{badge, BadgeVariant, Column, DataTableConfig, data_table};
/// use maud::html;
///
/// // Inline in a template — color chosen deterministically from the value:
/// let pill = badge("published", BadgeVariant::for_label("published"));
/// assert!(pill.into_string().contains(r#"class="autumn-badge autumn-badge--success""#));
///
/// // Inside a `data_table` cell:
/// struct Post { title: String, status: String }
/// let posts = vec![Post { title: "Hi".into(), status: "draft".into() }];
/// let cols: Vec<Column<Post>> = vec![
///     Column::new("Title",  |p: &Post| html! { (p.title.as_str()) }),
///     Column::new("Status", |p: &Post| badge(&p.status, BadgeVariant::for_label(&p.status))),
/// ];
/// let table = data_table(&posts, &cols, &DataTableConfig::new("No posts.")).into_string();
/// assert!(table.contains("autumn-badge--warning")); // "draft" → Warning
/// ```
#[must_use]
pub fn badge(label: &str, variant: BadgeVariant) -> maud::Markup {
    badge_with(label, variant, &BadgeConfig::new())
}

/// Render a status pill with a [`BadgeConfig`] (tooltip / `aria-label` / extra
/// class). See [`badge`] for the common case.
#[must_use]
pub fn badge_with(label: &str, variant: BadgeVariant, config: &BadgeConfig<'_>) -> maud::Markup {
    let root = merge_class(
        &format!("autumn-badge autumn-badge--{}", variant.as_str()),
        config.class,
    );
    maud::html! {
        span class=(root) title=[config.title] aria-label=[config.aria_label] { (label) }
    }
}

/// Render a neutral status pill in one call — shorthand for the common
/// "just show this value as a badge" case.
///
/// Equivalent to `badge(label, BadgeVariant::Neutral)`. For a color chosen from
/// the value, use `badge(label, BadgeVariant::for_label(label))`.
///
/// The admin plugin's hand-rolled `badge badge-{op}` audit-log spans are the
/// migration target for this helper; the `.autumn-badge` class contract here is a
/// superset that can replace them.
#[must_use]
pub fn status_tag(label: &str) -> maud::Markup {
    badge(label, BadgeVariant::Neutral)
}

// ── avatar (#1263) ──────────────────────────────────────────────────────────

/// Named avatar size. Each maps to a fixed, square pixel dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AvatarSize {
    /// 24×24 — inline with text, dense lists.
    Small,
    /// 40×40 — the default, member rows and nav bars.
    #[default]
    Medium,
    /// 64×64 — profile headers.
    Large,
}

impl AvatarSize {
    /// The square edge length in CSS pixels.
    #[must_use]
    pub const fn px(self) -> u32 {
        match self {
            Self::Small => 24,
            Self::Medium => 40,
            Self::Large => 64,
        }
    }

    /// The size modifier class.
    const fn class(self) -> &'static str {
        match self {
            Self::Small => "autumn-avatar--sm",
            Self::Medium => "autumn-avatar--md",
            Self::Large => "autumn-avatar--lg",
        }
    }
}

/// Rendering options for [`avatar`].
///
/// Build with [`AvatarConfig::new`] and chain the setters. With no image URL,
/// [`avatar`] renders a deterministic colored-initials badge instead.
#[derive(Debug, Clone, Default)]
pub struct AvatarConfig<'a> {
    image_url: Option<&'a str>,
    size: AvatarSize,
    class: Option<&'a str>,
}

impl<'a> AvatarConfig<'a> {
    /// A default config: no image (initials fallback), [`AvatarSize::Medium`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the image URL. An empty/whitespace-only URL is treated as absent
    /// (falls back to initials), so a `Some("")` from the database never
    /// renders a broken image.
    #[must_use]
    pub const fn image(mut self, url: &'a str) -> Self {
        self.image_url = Some(url);
        self
    }

    /// Set the avatar size (default [`AvatarSize::Medium`]).
    #[must_use]
    pub const fn size(mut self, size: AvatarSize) -> Self {
        self.size = size;
        self
    }

    /// Append an extra class to the root element.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Derive 1–2 uppercase initials from a display name.
///
/// `"Ada Lovelace"` → `"AL"`, `"cher"` → `"C"`, `"  José  "` → `"J"`. Unicode
/// safe (uppercases the first *character*, not byte) and never panics: an
/// empty or whitespace-only name yields a neutral placeholder glyph.
fn avatar_initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let first_char = |w: &str| w.chars().next();
    match words.as_slice() {
        [] => "?".to_string(),
        [only] => {
            first_char(only).map_or_else(|| "?".to_string(), |c| c.to_uppercase().to_string())
        }
        [first, .., last] => match (first_char(first), first_char(last)) {
            (Some(a), Some(b)) => format!("{}{}", a.to_uppercase(), b.to_uppercase()),
            _ => "?".to_string(),
        },
    }
}

/// The deterministic avatar background-color palette. Each entry is a CSS class
/// (backed by a rule in the widget stylesheet) rather than an inline color, so
/// the per-name color survives a nonce-based Content-Security-Policy — which
/// forbids inline `style` attributes (`style-src` gains a nonce and drops
/// `'unsafe-inline'`; nonces cover `<style>` elements, not attribute styles).
const AVATAR_PALETTE: [&str; 12] = [
    "autumn-avatar--c0",
    "autumn-avatar--c1",
    "autumn-avatar--c2",
    "autumn-avatar--c3",
    "autumn-avatar--c4",
    "autumn-avatar--c5",
    "autumn-avatar--c6",
    "autumn-avatar--c7",
    "autumn-avatar--c8",
    "autumn-avatar--c9",
    "autumn-avatar--c10",
    "autumn-avatar--c11",
];

/// Deterministic background-color class for a name's initials badge. The same
/// name always yields the same class across renders/machines, and the color is
/// applied via the stylesheet (no inline `style` attribute) so it is not
/// stripped by a nonce-based CSP.
fn avatar_palette_class(name: &str) -> &'static str {
    // `% 12` (a literal) keeps the index in range without a truncating `usize`
    // cast; the assertion keeps the literal in sync with the palette length.
    const _: () = assert!(AVATAR_PALETTE.len() == 12);
    AVATAR_PALETTE[(fnv1a_hash(name.trim()) % 12) as usize]
}

/// Render a person's avatar: an image when one is supplied, or a deterministic
/// colored-initials badge when it isn't.
///
/// * **Image branch** — `<img>` with the given `src`, a non-empty `alt` derived
///   from `name`, `loading="lazy"`, and square `width`/`height` for the chosen
///   size (no layout shift).
/// * **Initials branch** — a square badge with 1–2 uppercase initials and a
///   background color chosen deterministically from `name` (via a stable
///   `autumn-avatar--cN` palette class), so the same person always gets the
///   same color and there is never a broken-image request.
///
/// Both `name` and the image URL are HTML-escaped by Maud (no XSS via a crafted
/// display name). All output carries `autumn-avatar*` classes backed by the
/// framework stylesheet and emits **no inline `style` attribute** — the per-name
/// color is a palette class, so it survives a nonce-based Content-Security-Policy
/// that forbids inline styles.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{avatar, AvatarConfig, AvatarSize};
/// use maud::html;
///
/// // A member row: avatar + name.
/// let row = html! {
///     div class="member" {
///         (avatar("Ada Lovelace", &AvatarConfig::new().size(AvatarSize::Small)))
///         span { "Ada Lovelace" }
///     }
/// };
/// let out = row.into_string();
/// assert!(out.contains("autumn-avatar--initials")); // no image → initials
/// assert!(out.contains(">AL<"));                      // deterministic initials
///
/// // With an image:
/// let img = avatar("Ada", &AvatarConfig::new().image("/u/ada.png")).into_string();
/// assert!(img.contains(r#"loading="lazy""#));
/// assert!(img.contains(r#"alt="Ada""#));
/// ```
#[must_use]
pub fn avatar(name: &str, config: &AvatarConfig<'_>) -> maud::Markup {
    let px = config.size.px();
    let size_class = config.size.class();
    let alt = if name.trim().is_empty() {
        "avatar".to_string()
    } else {
        name.to_string()
    };
    match config.image_url {
        Some(url) if !url.trim().is_empty() => {
            let root = merge_class(
                &format!("autumn-avatar autumn-avatar--image {size_class}"),
                config.class,
            );
            maud::html! {
                img class=(root) src=(url) alt=(alt)
                    loading="lazy" width=(px) height=(px);
            }
        }
        _ => {
            let palette = avatar_palette_class(name);
            let root = merge_class(
                &format!("autumn-avatar autumn-avatar--initials {size_class} {palette}"),
                config.class,
            );
            maud::html! {
                span class=(root) role="img" aria-label=(alt) title=(alt) {
                    span class="autumn-avatar__initials" aria-hidden="true" { (avatar_initials(name)) }
                }
            }
        }
    }
}

// ── kbd (#3406) ─────────────────────────────────────────────────────────────

/// Render a keyboard key: `<kbd class="autumn-kbd">`.
///
/// ```rust
/// use autumn_web::widgets::kbd;
///
/// assert_eq!(kbd("Esc").into_string(), r#"<kbd class="autumn-kbd">Esc</kbd>"#);
/// ```
#[must_use]
pub fn kbd(key: &str) -> maud::Markup {
    maud::html! { kbd class="autumn-kbd" { (key) } }
}

/// Render a key combination: one `<kbd>` for each key, with `+` between,
/// in an outer `<kbd>`. This is the HTML pattern for keys that you press
/// together. An empty `keys` gives no markup.
///
/// ```rust
/// use autumn_web::widgets::kbd_combo;
///
/// let html = kbd_combo(&["Ctrl", "K"]).into_string();
/// assert!(html.starts_with(r#"<kbd class="autumn-kbd-combo">"#));
/// assert_eq!(html.matches(r#"<kbd class="autumn-kbd">"#).count(), 2);
/// ```
#[must_use]
pub fn kbd_combo(keys: &[&str]) -> maud::Markup {
    if keys.is_empty() {
        return maud::html! {};
    }
    maud::html! {
        kbd class="autumn-kbd-combo" {
            @for (i, key) in keys.iter().enumerate() {
                @if i > 0 { "+" }
                (kbd(key))
            }
        }
    }
}

// ── avatar_group (#3406) ────────────────────────────────────────────────────

/// The default [`AvatarGroupConfig::max`]: five items, the `+N` item
/// included.
pub const DEFAULT_AVATAR_GROUP_MAX: usize = 5;

/// Options for [`avatar_group_with`].
#[derive(Debug, Clone, Copy)]
pub struct AvatarGroupConfig<'a> {
    max: usize,
    total: Option<usize>,
    size: AvatarSize,
    more_label: &'a str,
    class: Option<&'a str>,
}

impl Default for AvatarGroupConfig<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> AvatarGroupConfig<'a> {
    /// At most [`DEFAULT_AVATAR_GROUP_MAX`] items, medium size, the label
    /// `"{count} more"`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max: DEFAULT_AVATAR_GROUP_MAX,
            total: None,
            size: AvatarSize::Medium,
            more_label: "{count} more",
            class: None,
        }
    }

    /// Show at most `max` items, the `+N` item included. With more
    /// avatars, the group shows `max - 1` avatars and `+N`. A `max` of 0
    /// counts as 1.
    #[must_use]
    pub const fn max(mut self, max: usize) -> Self {
        self.max = max;
        self
    }

    /// Set the number of people when it is more than the avatars you
    /// give, for example 120 likes with 3 avatars. A `total` below the
    /// number of avatars does nothing.
    #[must_use]
    pub const fn total(mut self, total: usize) -> Self {
        self.total = Some(total);
        self
    }

    /// Set the size of the `+N` item. Give the avatars the same size.
    #[must_use]
    pub const fn size(mut self, size: AvatarSize) -> Self {
        self.size = size;
        self
    }

    /// Set the accessible name of the `+N` item. `{count}` becomes the
    /// number. The default is `"{count} more"`. Translate it here. Do not
    /// give an empty label: a debug build panics.
    #[must_use]
    pub const fn more_label(mut self, label: &'a str) -> Self {
        self.more_label = label;
        self
    }

    /// Add a class to the root.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Render avatars as one group that overlaps, with a `+N` item for the
/// rest.
///
/// `label` names the group (`role="group"`), for example "Project
/// members". Make each avatar with [`avatar`]. With more than
/// [`DEFAULT_AVATAR_GROUP_MAX`] avatars, the group shows four and `+N`.
///
/// ```rust
/// use autumn_web::widgets::{AvatarConfig, avatar, avatar_group};
///
/// let names = ["Ada", "Grace", "Linus", "Margaret", "Barbara", "Edsger"];
/// let avatars: Vec<_> = names.iter().map(|n| avatar(n, &AvatarConfig::new())).collect();
/// let html = avatar_group("Project members", &avatars).into_string();
/// assert!(html.contains(r#"aria-label="2 more""#));
/// assert!(html.contains("+2"));
/// ```
#[must_use]
#[track_caller]
pub fn avatar_group(label: &str, avatars: &[maud::Markup]) -> maud::Markup {
    avatar_group_with(label, avatars, &AvatarGroupConfig::new())
}

/// Like [`avatar_group`], with an [`AvatarGroupConfig`]: max, total, size,
/// `+N` label and class.
#[must_use]
#[track_caller]
pub fn avatar_group_with(
    label: &str,
    avatars: &[maud::Markup],
    config: &AvatarGroupConfig<'_>,
) -> maud::Markup {
    assert_named("avatar_group", label);
    assert_named("avatar_group more_label", config.more_label);
    let (shown, extra) = avatar_group_split(avatars.len(), config.total, config.max);
    let root = merge_class("autumn-avatar-group", config.class);
    let more = format!(
        "autumn-avatar {} autumn-avatar-group__more",
        config.size.class()
    );
    let more_label = config.more_label.replace("{count}", &extra.to_string());
    maud::html! {
        div class=(root) role="group" aria-label=(label) {
            @for avatar in &avatars[..shown] { (avatar) }
            @if extra > 0 {
                span class=(more) role="img" aria-label=(more_label) {
                    span aria-hidden="true" { "+" (extra) }
                }
            }
        }
    }
}

/// How many avatars show, and the `+N` count: `(shown, extra)`.
///
/// `max` counts the `+N` item. Thus `shown + extra` is the people count,
/// and `shown`, plus 1 for a `+N` item, is never above `max`.
const fn avatar_group_split(len: usize, total: Option<usize>, max: usize) -> (usize, usize) {
    let count = match total {
        Some(total) if total > len => total,
        _ => len,
    };
    let max = if max == 0 { 1 } else { max };
    let shown = if count <= max || len < max - 1 {
        len
    } else {
        max - 1
    };
    (shown, count - shown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::{MeterConfig, meter_with, progress};
    use proptest::prelude::*;

    // ── property_list ──────────────────────────────────────────────────

    #[test]
    fn property_list_empty_slice_renders_empty_dl_never_panics() {
        let html = property_list(&[]).into_string();
        assert!(html.contains("<dl"), "{html}");
        assert!(!html.contains("<dt"), "{html}");
    }

    #[test]
    fn property_list_has_autumn_property_list_class() {
        let html = property_list(&[]).into_string();
        assert!(html.contains(r#"class="autumn-property-list""#), "{html}");
    }

    #[test]
    fn property_list_renders_dt_and_dd_per_row() {
        let rows = vec![
            ("Title", maud::html! { "My Post" }),
            ("Published", maud::html! { "true" }),
        ];
        let html = property_list(&rows).into_string();
        assert!(html.contains("<dt>Title</dt>"), "{html}");
        assert!(html.contains("<dt>Published</dt>"), "{html}");
        assert!(html.contains("<dd>My Post</dd>"), "{html}");
        assert!(html.contains("<dd>true</dd>"), "{html}");
    }

    #[test]
    fn property_list_escapes_label() {
        let rows = vec![("<script>alert(1)</script>", maud::html! { "safe" })];
        let html = property_list(&rows).into_string();
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    #[test]
    fn property_list_renders_markup_value_unescaped() {
        let rows = vec![("Link", maud::html! { a href="/foo" { "click" } })];
        let html = property_list(&rows).into_string();
        assert!(html.contains(r#"<a href="/foo">click</a>"#), "{html}");
    }

    #[test]
    fn property_list_multiple_rows_all_rendered() {
        let rows = vec![
            ("A", maud::html! { "1" }),
            ("B", maud::html! { "2" }),
            ("C", maud::html! { "3" }),
        ];
        let html = property_list(&rows).into_string();
        assert_eq!(html.matches("<dt>").count(), 3, "{html}");
        assert_eq!(html.matches("<dd>").count(), 3, "{html}");
    }

    // ── data_table ─────────────────────────────────────────────────────

    #[test]
    fn sortdir_param_value() {
        assert_eq!(SortDir::Asc.param_value(), "asc");
        assert_eq!(SortDir::Desc.param_value(), "desc");
    }

    #[test]
    fn sortdir_aria_value() {
        assert_eq!(SortDir::Asc.aria_value(), "ascending");
        assert_eq!(SortDir::Desc.aria_value(), "descending");
    }

    #[test]
    fn sortdir_toggled() {
        assert_eq!(SortDir::Asc.toggled(), SortDir::Desc);
        assert_eq!(SortDir::Desc.toggled(), SortDir::Asc);
    }

    #[test]
    fn data_table_config_defaults() {
        let cfg = DataTableConfig::new("No items");
        assert_eq!(cfg.empty_message, "No items");
        assert_eq!(cfg.sort_param, "sort");
        assert_eq!(cfg.dir_param, "dir");
        assert_eq!(cfg.page_param, "page");
        assert!(cfg.active_sort.is_none());
        assert_eq!(cfg.active_dir, SortDir::Asc);
        assert!(cfg.caption.is_none());
        assert!(cfg.class.is_none());
        assert_eq!(cfg.base_path, "");
        assert_eq!(cfg.query, "");
    }

    #[test]
    fn data_table_config_builders() {
        let cfg = DataTableConfig::new("No items")
            .base_path("/posts")
            .query("q=foo")
            .caption("Posts table")
            .active_sort("title")
            .active_dir(SortDir::Desc)
            .class("my-table");
        assert_eq!(cfg.base_path, "/posts");
        assert_eq!(cfg.query, "q=foo");
        assert_eq!(cfg.caption, Some("Posts table"));
        assert_eq!(cfg.active_sort, Some("title"));
        assert_eq!(cfg.active_dir, SortDir::Desc);
        assert_eq!(cfg.class, Some("my-table"));
    }

    #[test]
    fn data_table_has_thead_th_scope_col() {
        let cols: Vec<Column<i32>> = vec![
            Column::new("Name", |_| maud::html! { "n" }),
            Column::new("Age", |_| maud::html! { "a" }),
        ];
        let html = data_table(&[1i32], &cols, &DataTableConfig::new("empty")).into_string();
        assert!(html.contains("<thead"), "{html}");
        assert!(html.contains(r#"scope="col""#), "{html}");
        assert!(html.contains("Name"), "{html}");
        assert!(html.contains("Age"), "{html}");
    }

    #[test]
    fn data_table_renders_tbody_rows_and_cells() {
        let cols: Vec<Column<&str>> = vec![Column::new("Word", |row| maud::html! { (*row) })];
        let html =
            data_table(&["hello", "world"], &cols, &DataTableConfig::new("empty")).into_string();
        assert!(html.contains("<tbody"), "{html}");
        assert!(html.contains("hello"), "{html}");
        assert!(html.contains("world"), "{html}");
        // Two data rows
        assert_eq!(
            html.matches("<tr").count(),
            3,
            "1 header row + 2 data rows: {html}"
        );
    }

    #[test]
    fn data_table_caption_when_set() {
        let cols: Vec<Column<i32>> = vec![Column::new("X", |_| maud::html! { "x" })];
        let html = data_table(
            &[1i32],
            &cols,
            &DataTableConfig::new("e").caption("My Table"),
        )
        .into_string();
        assert!(html.contains("<caption"), "{html}");
        assert!(html.contains("My Table"), "{html}");
    }

    #[test]
    fn data_table_no_caption_by_default() {
        let cols: Vec<Column<i32>> = vec![Column::new("X", |_| maud::html! { "x" })];
        let html = data_table(&[1i32], &cols, &DataTableConfig::new("e")).into_string();
        assert!(!html.contains("<caption"), "{html}");
    }

    #[test]
    fn data_table_applies_class_when_set() {
        let cols: Vec<Column<i32>> = vec![Column::new("X", |_| maud::html! { "x" })];
        let html =
            data_table(&[1i32], &cols, &DataTableConfig::new("e").class("styled")).into_string();
        assert!(html.contains(r#"class="styled""#), "{html}");
    }

    #[test]
    fn data_table_empty_renders_single_colspan_row() {
        let cols: Vec<Column<i32>> = vec![
            Column::new("A", |_| maud::html! { "a" }),
            Column::new("B", |_| maud::html! { "b" }),
            Column::new("C", |_| maud::html! { "c" }),
        ];
        let html = data_table(&[], &cols, &DataTableConfig::new("Nothing here")).into_string();
        assert!(html.contains("Nothing here"), "{html}");
        assert!(html.contains(r#"colspan="3""#), "{html}");
        // Only the header tr, no data rows
        assert_eq!(html.matches("<tr").count(), 2, "header + empty row: {html}");
    }

    #[test]
    fn data_table_empty_message_announced() {
        let cols: Vec<Column<i32>> = vec![Column::new("X", |_| maud::html! { "x" })];
        let html = data_table(&[], &cols, &DataTableConfig::new("No results")).into_string();
        assert!(html.contains(r#"role="status""#), "{html}");
    }

    #[test]
    fn data_table_nonsortable_header_is_plain() {
        let cols: Vec<Column<i32>> = vec![Column::new("Title", |_| maud::html! { "t" })];
        let html = data_table(&[1i32], &cols, &DataTableConfig::new("e")).into_string();
        // No sort link in header
        let thead_end = html.find("</thead>").unwrap_or(html.len());
        let thead = &html[..thead_end];
        assert!(!thead.contains("<a "), "{html}");
        assert!(!thead.contains("aria-sort"), "{html}");
    }

    #[test]
    fn data_table_sortable_inactive_has_link_and_aria_sort_none() {
        let cols: Vec<Column<i32>> =
            vec![Column::new("Title", |_| maud::html! { "t" }).sortable("title")];
        let cfg = DataTableConfig::new("e").base_path("/posts");
        let html = data_table(&[1i32], &cols, &cfg).into_string();
        assert!(html.contains(r#"aria-sort="none""#), "{html}");
        assert!(html.contains("sort=title"), "{html}");
        assert!(html.contains("dir=asc"), "{html}");
    }

    #[test]
    fn data_table_sortable_active_reflects_dir_and_toggles() {
        let cols: Vec<Column<i32>> =
            vec![Column::new("Title", |_| maud::html! { "t" }).sortable("title")];
        let cfg = DataTableConfig::new("e")
            .base_path("/posts")
            .active_sort("title")
            .active_dir(SortDir::Asc);
        let html = data_table(&[1i32], &cols, &cfg).into_string();
        assert!(html.contains(r#"aria-sort="ascending""#), "{html}");
        // toggled dir link
        assert!(html.contains("dir=desc"), "{html}");
    }

    #[test]
    fn data_table_sortable_link_preserves_other_query_params() {
        let cols: Vec<Column<i32>> =
            vec![Column::new("Name", |_| maud::html! { "n" }).sortable("name")];
        let cfg = DataTableConfig::new("e")
            .base_path("/posts")
            .query("q=hello");
        let html = data_table(&[1i32], &cols, &cfg).into_string();
        assert!(html.contains("q=hello"), "{html}");
    }

    #[test]
    fn data_table_sortable_link_drops_existing_sort_dir_page() {
        let cols: Vec<Column<i32>> =
            vec![Column::new("Name", |_| maud::html! { "n" }).sortable("name")];
        let cfg = DataTableConfig::new("e")
            .base_path("/posts")
            .query("q=foo&sort=old&dir=asc&page=3");
        let html = data_table(&[1i32], &cols, &cfg).into_string();
        // old sort params stripped, new sort=name present
        assert!(html.contains("sort=name"), "{html}");
        // page stripped (reset to page 1)
        assert!(!html.contains("page=3"), "{html}");
        // q preserved
        assert!(html.contains("q=foo"), "{html}");
        // no duplicate old sort
        assert!(!html.contains("sort=old"), "{html}");
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "accessible name")]
    fn avatar_group_with_a_blank_more_label_panics_in_debug() {
        let config = AvatarGroupConfig::new().more_label("");
        let _ = avatar_group_with("Members", &[], &config);
    }

    // ── kbd ─────────────────────────────────────────────────────────────

    #[test]
    fn kbd_renders_and_escapes_a_key() {
        assert_eq!(
            kbd("Esc").into_string(),
            r#"<kbd class="autumn-kbd">Esc</kbd>"#
        );
        assert_eq!(
            kbd("<").into_string(),
            r#"<kbd class="autumn-kbd">&lt;</kbd>"#
        );
    }

    #[test]
    fn kbd_combo_nests_keys_with_plus() {
        assert_eq!(
            kbd_combo(&["Ctrl", "Shift", "K"]).into_string(),
            concat!(
                r#"<kbd class="autumn-kbd-combo">"#,
                r#"<kbd class="autumn-kbd">Ctrl</kbd>+"#,
                r#"<kbd class="autumn-kbd">Shift</kbd>+"#,
                r#"<kbd class="autumn-kbd">K</kbd></kbd>"#,
            )
        );
        assert_eq!(kbd_combo(&[]).into_string(), "");
    }

    // ── avatar_group ────────────────────────────────────────────────────

    fn avatars(n: usize) -> Vec<maud::Markup> {
        (0..n)
            .map(|i| avatar(&format!("Person {i}"), &AvatarConfig::new()))
            .collect()
    }

    #[test]
    fn avatar_group_is_a_named_group_with_every_avatar_under_max() {
        let html = avatar_group("Members", &avatars(3)).into_string();
        assert!(
            html.starts_with(
                r#"<div class="autumn-avatar-group" role="group" aria-label="Members">"#
            ),
            "{html}"
        );
        assert_eq!(html.matches("autumn-avatar--initials").count(), 3, "{html}");
        assert!(!html.contains("autumn-avatar-group__more"), "{html}");
    }

    #[test]
    fn avatar_group_over_max_shows_max_minus_one_and_more() {
        let html = avatar_group("Members", &avatars(8)).into_string();
        assert_eq!(html.matches("autumn-avatar--initials").count(), 4, "{html}");
        assert!(
            html.contains(concat!(
                r#"<span class="autumn-avatar autumn-avatar--md autumn-avatar-group__more" "#,
                r#"role="img" aria-label="4 more"><span aria-hidden="true">+4</span></span>"#,
            )),
            "{html}"
        );
    }

    #[test]
    fn avatar_group_total_size_label_and_class() {
        let config = AvatarGroupConfig::new()
            .max(3)
            .total(120)
            .size(AvatarSize::Small)
            .more_label("{count} weitere")
            .class("likes");
        let html = avatar_group_with("Likes", &avatars(3), &config).into_string();
        assert!(
            html.contains(r#"class="autumn-avatar-group likes""#),
            "{html}"
        );
        assert_eq!(html.matches("autumn-avatar--initials").count(), 2, "{html}");
        assert!(
            html.contains("autumn-avatar--sm autumn-avatar-group__more"),
            "{html}"
        );
        assert!(html.contains(r#"aria-label="118 weitere""#), "{html}");
        assert!(html.contains(">+118<"), "{html}");
    }

    #[test]
    fn avatar_group_split_examples() {
        assert_eq!(avatar_group_split(3, None, 5), (3, 0));
        assert_eq!(avatar_group_split(5, None, 5), (5, 0));
        assert_eq!(avatar_group_split(6, None, 5), (4, 2));
        assert_eq!(avatar_group_split(2, Some(3), 5), (2, 1));
        assert_eq!(avatar_group_split(3, Some(1), 5), (3, 0));
        assert_eq!(avatar_group_split(3, None, 1), (0, 3));
        assert_eq!(avatar_group_split(3, None, 0), (0, 3));
        assert_eq!(avatar_group_split(0, None, 5), (0, 0));
    }

    proptest! {
        /// The split never loses a person and never shows more than `max`
        /// items.
        #[test]
        fn avatar_group_split_keeps_count_and_max(
            len in 0usize..50,
            total in proptest::option::of(0usize..500),
            max in 0usize..20,
        ) {
            let (shown, extra) = avatar_group_split(len, total, max);
            let count = total.map_or(len, |t| t.max(len));
            prop_assert_eq!(shown + extra, count);
            prop_assert!(shown <= len);
            prop_assert!(shown + usize::from(extra > 0) <= max.max(1));
        }

        /// A progress value is always in `0..=max`, and `max` is above 0.
        #[test]
        fn progress_value_is_always_in_range(
            value in proptest::option::of(proptest::num::f64::ANY),
            max in proptest::num::f64::ANY,
        ) {
            let html = progress("P", value, max).into_string();
            let attr = |name: &str| -> Option<f64> {
                let key = format!(r#" {name}=""#);
                let at = html.find(&key)? + key.len();
                html[at..at + html[at..].find('"')?].parse().ok()
            };
            let max = attr("max").expect("max");
            prop_assert!(max.is_finite() && max > 0.0);
            if let Some(v) = attr("value") {
                prop_assert!((0.0..=max).contains(&v), "{} not in 0..={}", v, max);
            }
        }

        /// A meter value and its thresholds are always in `min..=max`.
        #[test]
        fn meter_values_are_always_in_range(
            value in proptest::num::f64::ANY,
            a in proptest::num::f64::ANY,
            b in proptest::num::f64::ANY,
            low in proptest::num::f64::ANY,
        ) {
            let html = meter_with("M", value, a..=b, &MeterConfig::new().low(low)).into_string();
            let attr = |name: &str| -> Option<f64> {
                let key = format!(r#" {name}=""#);
                let at = html.find(&key)? + key.len();
                html[at..at + html[at..].find('"')?].parse().ok()
            };
            let (min, max) = (attr("min").expect("min"), attr("max").expect("max"));
            prop_assert!(min.is_finite() && max.is_finite() && min <= max);
            let v = attr("value").expect("value");
            prop_assert!((min..=max).contains(&v));
            if let Some(l) = attr("low") {
                prop_assert!((min..=max).contains(&l));
            }
        }
    }
}
