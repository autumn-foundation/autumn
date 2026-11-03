//! Navigation widgets: breadcrumb, nav link, nav bar, locale switcher,
//! theme toggle, popover and dropdown menu.

#[cfg(feature = "maud")]
use super::*;

// ── breadcrumb ────────────────────────────────────────────────────────────

/// A single crumb in a [`breadcrumb`] navigation trail.
///
/// Build with [`Crumb::link`] for a linked crumb or [`Crumb::current`] for the
/// current (non-linked) page. The last item in the slice passed to [`breadcrumb`]
/// is always treated as the current page regardless of whether `href` is set.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crumb<'a> {
    /// Visible label for this crumb. HTML-escaped by Maud.
    pub label: &'a str,
    /// Optional URL for this crumb. `None` on the last crumb (current page).
    pub href: Option<&'a str>,
}

#[cfg(feature = "maud")]
impl<'a> Crumb<'a> {
    /// Create a linked crumb.
    #[must_use]
    pub const fn link(label: &'a str, href: &'a str) -> Self {
        Self {
            label,
            href: Some(href),
        }
    }

    /// Create the current-page crumb (no link).
    #[must_use]
    pub const fn current(label: &'a str) -> Self {
        Self { label, href: None }
    }
}

/// Render an accessible breadcrumb navigation trail.
///
/// Emits a `<nav aria-label="Breadcrumb">` containing an `<ol>`. Every crumb
/// except the last renders as an `<a>` link (using `href` when provided); the
/// last crumb renders as the current page marked with `aria-current="page"`.
/// Separators are wrapped in `<span aria-hidden="true">` so screen readers
/// announce only the crumb labels.
///
/// An empty slice renders empty [`maud::Markup`] (no output, no panic).
/// A single crumb renders without a leading separator.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{Crumb, breadcrumb};
///
/// let crumbs = [
///     Crumb::link("Home", "/"),
///     Crumb::link("Posts", "/posts"),
///     Crumb::current("My Post"),
/// ];
/// let html = breadcrumb(&crumbs).into_string();
/// assert!(html.contains(r#"aria-current="page""#));
/// assert!(html.contains(r#"href="/posts""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn breadcrumb(crumbs: &[Crumb<'_>]) -> maud::Markup {
    if crumbs.is_empty() {
        return maud::html! {};
    }
    let last = crumbs.len() - 1;
    maud::html! {
        nav aria-label="Breadcrumb" class="autumn-breadcrumb" {
            ol class="autumn-breadcrumb__list" {
                @for (i, crumb) in crumbs.iter().enumerate() {
                    li class="autumn-breadcrumb__item" {
                        @if i > 0 {
                            span aria-hidden="true" class="autumn-breadcrumb__separator" { "›" }
                        }
                        @if i == last {
                            span aria-current="page" class="autumn-breadcrumb__current" { (crumb.label) }
                        } @else if let Some(href) = crumb.href {
                            a href=(href) class="autumn-breadcrumb__link" { (crumb.label) }
                        } @else {
                            span class="autumn-breadcrumb__text" { (crumb.label) }
                        }
                    }
                }
            }
        }
    }
}

// ── nav_link ────────────────────────────────────────────────────────────────

/// Match mode for [`nav_link_matched`] — how the current request path is
/// compared against a link's `href` to decide the active state.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NavLinkMatch {
    /// Active only when the current path equals `href` exactly (default).
    #[default]
    Exact,
    /// Active when the current path equals `href`, or starts with `href`
    /// followed by a `/` segment boundary — so `/admin/posts/3/edit`
    /// activates an `/admin/posts` link, but `/admin/postsX` does not. A
    /// trailing slash on `href` is normalized away first, so `/admin/posts/`
    /// behaves identically to `/admin/posts`. An empty `href` — or `href`
    /// that normalizes to one — never matches, since that would otherwise
    /// activate on every absolute path.
    Prefix,
}

/// Render a navigation anchor, marked active when `href` equals `current_path`.
///
/// Uses [`NavLinkMatch::Exact`]; see [`nav_link_matched`] for prefix matching.
/// Pair with the [`CurrentPath`](crate::extract::CurrentPath) extractor to
/// get `current_path` from the incoming request.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::nav_link;
///
/// let html = nav_link("/posts", "/posts/new", "New Post").into_string();
/// assert!(!html.contains("autumn-active"), "different path stays inactive: {html}");
///
/// let html = nav_link("/posts", "/posts", "Posts").into_string();
/// assert!(html.contains(r#"class="autumn-active""#));
/// assert!(html.contains(r#"aria-current="page""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn nav_link(current_path: &str, href: &str, label: &str) -> maud::Markup {
    nav_link_matched(current_path, href, label, NavLinkMatch::Exact)
}

/// Render a navigation anchor with an explicit [`NavLinkMatch`] mode.
///
/// When active, the anchor carries `class="autumn-active"` and
/// `aria-current="page"`; when inactive, neither attribute is emitted.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{NavLinkMatch, nav_link, nav_link_matched};
///
/// let html = nav_link("/posts", "/posts", "Posts").into_string();
/// assert!(html.contains(r#"class="autumn-active""#));
/// assert!(html.contains(r#"aria-current="page""#));
///
/// // `/posts/3/edit` activates the `/posts` link only in Prefix mode.
/// let html = nav_link_matched("/posts/3/edit", "/posts", "Posts", NavLinkMatch::Prefix)
///     .into_string();
/// assert!(html.contains(r#"class="autumn-active""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn nav_link_matched(
    current_path: &str,
    href: &str,
    label: &str,
    mode: NavLinkMatch,
) -> maud::Markup {
    let active = nav_link_is_active(current_path, href, mode);
    maud::html! {
        a href=(href) class=[active.then_some("autumn-active")] aria-current=[active.then_some("page")] {
            (label)
        }
    }
}

/// Decide whether `href` is the active link for `current_path` under `mode`.
#[cfg(feature = "maud")]
fn nav_link_is_active(current_path: &str, href: &str, mode: NavLinkMatch) -> bool {
    match mode {
        NavLinkMatch::Exact => current_path == href,
        NavLinkMatch::Prefix => {
            if current_path == href {
                return true;
            }
            // Normalize a trailing slash so "/posts/" behaves the same as
            // "/posts", but preserve a bare "/" rather than trimming it to an
            // empty prefix — same convention as tenancy::is_public_path, since
            // an empty prefix would otherwise match every absolute path.
            let href = if href.len() > 1 {
                href.trim_end_matches('/')
            } else {
                href
            };
            !href.is_empty() && crate::router::path_matches_route_prefix(current_path, href)
        }
    }
}

// ── locale switcher (issue #1251) ─────────────────────────────────────────────

/// Rewrite a path — optionally with a query string — to point at a
/// different locale's prefixed URL (issue #1251).
///
/// `path` is the current page's locale-stripped path, exactly as returned by
/// axum's `Uri` extractor inside a `/{locale}/...` nest (nesting strips the
/// matched locale segment before extraction reaches the handler) — e.g.
/// `"/posts"` or `"/posts?sort=asc"`. Any query string is preserved
/// verbatim; only the locale segment changes.
///
/// The root path is a special case: axum's `nest("/{locale}", router)` makes
/// the *bare* `/{locale}` (no trailing slash) match the inner router's own
/// `"/"` route — `/{locale}/` 404s — so `localized_path("/", "es")` returns
/// `"/es"`, not `"/es/"`.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::localized_path;
///
/// assert_eq!(localized_path("/posts", "es"), "/es/posts");
/// assert_eq!(localized_path("/posts?sort=asc", "es"), "/es/posts?sort=asc");
/// assert_eq!(localized_path("/", "es"), "/es");
/// assert_eq!(localized_path("/?ref=newsletter", "es"), "/es?ref=newsletter");
/// ```
#[must_use]
pub fn localized_path(path: &str, locale: &str) -> String {
    let (before_query, query) = path.find('?').map_or((path, ""), |i| path.split_at(i));
    let joined = if before_query == "/" || before_query.is_empty() {
        format!("/{locale}")
    } else {
        let rest = before_query.strip_prefix('/').unwrap_or(before_query);
        format!("/{locale}/{rest}")
    };
    format!("{joined}{query}")
}

/// Render links to the current page in each supported locale (issue #1251),
/// preserving the current path and query — only the locale segment changes.
///
/// `path` is the current page's locale-stripped path (see [`localized_path`]).
/// `current_locale`'s entry renders as inert text with `aria-current="true"`
/// rather than a self-link.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::locale_switcher;
///
/// let html =
///     locale_switcher("/posts", "en", &["en".to_owned(), "es".to_owned()]).into_string();
/// assert!(html.contains(r#"href="/es/posts""#));
/// assert!(!html.contains(r#"href="/en/posts""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn locale_switcher(
    path: &str,
    current_locale: &str,
    supported_locales: &[String],
) -> maud::Markup {
    maud::html! {
        nav class="autumn-locale-switcher" aria-label="Language" {
            @for locale in supported_locales {
                @if locale == current_locale {
                    span class="autumn-locale-switcher__current" aria-current="true" { (locale) }
                } @else {
                    a href=(localized_path(path, locale)) { (locale) }
                }
            }
        }
    }
}

// ── theme_toggle (#3396) ────────────────────────────────────────────────────

/// Settings for [`theme_toggle`].
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy)]
pub struct ThemeToggleConfig<'a> {
    current: crate::ui::theme::Theme,
    action: &'a str,
    return_to: Option<&'a str>,
    csrf_token: Option<&'a str>,
    csrf_field: &'a str,
    label: &'a str,
    option_labels: [&'a str; 3],
    class: Option<&'a str>,
}

#[cfg(feature = "maud")]
impl<'a> ThemeToggleConfig<'a> {
    /// A toggle that marks `current` as pressed and posts to
    /// [`crate::ui::theme::THEME_PATH`].
    #[must_use]
    pub const fn new(current: crate::ui::theme::Theme) -> Self {
        Self {
            current,
            action: crate::ui::theme::THEME_PATH,
            return_to: None,
            csrf_token: None,
            csrf_field: "_csrf",
            label: "Theme",
            option_labels: ["System", "Light", "Dark"],
            class: None,
        }
    }

    /// Post to a different path (an app's own theme route). With JS, the
    /// theme applies at once and the form still posts, so the app stores the
    /// choice. The script writes the `autumn.theme` cookie only for a form
    /// that posts to [`crate::ui::theme::THEME_PATH`].
    #[must_use]
    pub const fn action(mut self, action: &'a str) -> Self {
        self.action = action;
        self
    }

    /// The local path to go back to after the change. When omitted, the
    /// handler uses the `Referer` path.
    #[must_use]
    pub const fn return_to(mut self, path: &'a str) -> Self {
        self.return_to = Some(path);
        self
    }

    /// Set the CSRF token. Without a token no hidden field is rendered,
    /// and the no-JS post gets `403` from the CSRF layer.
    #[must_use]
    pub const fn csrf_token(mut self, token: &'a str) -> Self {
        self.csrf_token = Some(token);
        self
    }

    /// Set the CSRF field name. Default: `"_csrf"`.
    #[must_use]
    pub const fn csrf_field(mut self, field: &'a str) -> Self {
        self.csrf_field = field;
        self
    }

    /// Set both from the handler's extractors. A `None` token renders no
    /// field; a `None` field keeps `"_csrf"`. Same as `ReactionControls::csrf`.
    #[must_use]
    pub fn csrf(
        mut self,
        token: Option<&'a crate::security::CsrfToken>,
        field: Option<&'a crate::security::CsrfFormField>,
    ) -> Self {
        if let Some(token) = token {
            self.csrf_token = Some(token.token());
        }
        if let Some(field) = field {
            self.csrf_field = field.0.as_str();
        }
        self
    }

    /// The accessible name of the button group (default `"Theme"`).
    #[must_use]
    pub const fn label(mut self, label: &'a str) -> Self {
        self.label = label;
        self
    }

    /// The button texts, in the order System, Light, Dark.
    #[must_use]
    pub const fn option_labels(mut self, system: &'a str, light: &'a str, dark: &'a str) -> Self {
        self.option_labels = [system, light, dark];
        self
    }

    /// Append an extra class to the root `<form>`.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Render a System / Light / Dark theme switch (issue #3396).
///
/// Emits a `POST` form with one submit button per theme. The current theme
/// has `aria-pressed="true"`. Without JS, the form posts to
/// [`crate::ui::theme::THEME_PATH`] (mount [`crate::ui::theme::router`]).
/// The route stores the `autumn.theme` cookie and redirects back.
/// `autumn-widgets.js` finds the `data-autumn-theme-toggle` hook. It applies
/// `data-theme` on `<html>` and writes the cookie. It sends no request.
///
/// Read the cookie in the layout with [`crate::ui::theme::Theme`]. Render
/// `data-theme` on `<html>`. Thus the first paint shows the correct theme.
///
/// # Example
///
/// ```rust
/// use autumn_web::ui::theme::Theme;
/// use autumn_web::widgets::{ThemeToggleConfig, theme_toggle};
///
/// let html = theme_toggle(&ThemeToggleConfig::new(Theme::Dark).csrf_token("tok"))
///     .into_string();
/// assert!(html.contains(r#"value="dark" class="autumn-theme-toggle__option" aria-pressed="true""#));
/// assert!(html.contains(r#"name="_csrf" value="tok""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn theme_toggle(config: &ThemeToggleConfig<'_>) -> maud::Markup {
    use crate::ui::theme::Theme;

    let root = merge_class("autumn-theme-toggle", config.class);
    maud::html! {
        form method="post" action=(config.action) class=(root) data-autumn-theme-toggle {
            @if let Some(token) = config.csrf_token {
                input type="hidden" name=(config.csrf_field) value=(token);
            }
            @if let Some(path) = config.return_to {
                input type="hidden" name="return_to" value=(path);
            }
            div class="autumn-theme-toggle__group" role="group" aria-label=(config.label) {
                @for (theme, text) in Theme::ALL.into_iter().zip(config.option_labels) {
                    button type="submit" name="theme" value=(theme.as_str())
                        class="autumn-theme-toggle__option"
                        aria-pressed=(if theme == config.current { "true" } else { "false" })
                    {
                        (text)
                    }
                }
            }
        }
    }
}

// ── nav_bar ──────────────────────────────────────────────────────────────────

/// Layout variant for [`nav_bar`] — selects a modifier class on the root
/// `<nav>` element.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NavBarLayout {
    /// Horizontal top navigation bar (default).
    #[default]
    Horizontal,
    /// Vertical sidebar / dashboard nav — adds `.autumn-nav--sidebar`.
    Sidebar,
}

/// A single navigation link, as rendered inside a [`nav_bar`] or [`NavMenu`].
#[cfg(feature = "maud")]
#[derive(Debug, Clone)]
pub struct NavLinkItem {
    href: String,
    label: String,
    match_mode: Option<NavLinkMatch>,
}

#[cfg(feature = "maud")]
impl NavLinkItem {
    /// Build from anything convertible to `String` — an owned `String`
    /// (e.g. a `format!()` result) moves in for free via `.into()`, while a
    /// `&str` literal still works ergonomically.
    fn new(
        href: impl Into<String>,
        label: impl Into<String>,
        match_mode: Option<NavLinkMatch>,
    ) -> Self {
        Self {
            href: href.into(),
            label: label.into(),
            match_mode,
        }
    }
}

/// A dropdown navigation item: a `<button>` trigger plus a `<ul>` of
/// sub-links. Build with [`NavMenu::new`] and chain `.link()` / `.link_matched()`
/// / `.plain_link()` for each sub-item.
#[cfg(feature = "maud")]
#[derive(Debug, Clone)]
pub struct NavMenu {
    label: String,
    links: Vec<NavLinkItem>,
}

#[cfg(feature = "maud")]
impl NavMenu {
    /// Create a new dropdown menu with the given trigger label and no sub-items.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            links: Vec::new(),
        }
    }

    /// Append a sub-link using [`NavLinkMatch::Exact`] matching.
    #[must_use]
    pub fn link(self, href: impl Into<String>, label: impl Into<String>) -> Self {
        self.link_matched(href, label, NavLinkMatch::Exact)
    }

    /// Append a sub-link with an explicit [`NavLinkMatch`] mode.
    #[must_use]
    pub fn link_matched(
        mut self,
        href: impl Into<String>,
        label: impl Into<String>,
        mode: NavLinkMatch,
    ) -> Self {
        self.links.push(NavLinkItem::new(href, label, Some(mode)));
        self
    }

    /// Append a sub-link that never becomes active (e.g. an external link).
    #[must_use]
    pub fn plain_link(mut self, href: impl Into<String>, label: impl Into<String>) -> Self {
        self.links.push(NavLinkItem::new(href, label, None));
        self
    }
}

/// One item in a [`nav_bar`]'s primary or trailing item list.
#[cfg(feature = "maud")]
#[derive(Debug, Clone)]
pub enum NavItem {
    /// A single navigation link.
    Link(NavLinkItem),
    /// A dropdown menu of sub-links.
    Menu(NavMenu),
    /// A non-interactive group label (e.g. `"Models"` in a sidebar nav).
    Section(String),
}

#[cfg(feature = "maud")]
impl NavItem {
    /// A link using [`NavLinkMatch::Exact`] matching against the current path.
    #[must_use]
    pub fn link(href: impl Into<String>, label: impl Into<String>) -> Self {
        Self::link_matched(href, label, NavLinkMatch::Exact)
    }

    /// A link with an explicit [`NavLinkMatch`] mode.
    #[must_use]
    pub fn link_matched(
        href: impl Into<String>,
        label: impl Into<String>,
        mode: NavLinkMatch,
    ) -> Self {
        Self::Link(NavLinkItem::new(href, label, Some(mode)))
    }

    /// A link that never becomes active regardless of the current path —
    /// e.g. a link to an unrelated tool (an admin actuator page) that should
    /// never claim `aria-current`.
    #[must_use]
    pub fn plain_link(href: impl Into<String>, label: impl Into<String>) -> Self {
        Self::Link(NavLinkItem::new(href, label, None))
    }

    /// A dropdown menu item.
    #[must_use]
    pub const fn menu(menu: NavMenu) -> Self {
        Self::Menu(menu)
    }

    /// A non-interactive group label.
    #[must_use]
    pub fn section(label: impl Into<String>) -> Self {
        Self::Section(label.into())
    }
}

/// Configuration for [`nav_bar`]. Build with [`NavBarConfig::new`].
#[cfg(feature = "maud")]
#[derive(Debug, Clone)]
pub struct NavBarConfig {
    id: String,
    aria_label: String,
    brand: Option<(maud::Markup, Option<String>)>,
    items: Vec<NavItem>,
    trailing: Vec<NavItem>,
    layout: NavBarLayout,
    toggle_label: String,
    class: Option<String>,
}

#[cfg(feature = "maud")]
impl Default for NavBarConfig {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "maud")]
impl NavBarConfig {
    /// Create a new nav bar configuration: no brand or items, `aria-label`
    /// `"Main"`, id `"autumn-nav"`, toggle label `"Menu"`, horizontal layout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: "autumn-nav".to_string(),
            aria_label: "Main".to_string(),
            brand: None,
            items: Vec::new(),
            trailing: Vec::new(),
            layout: NavBarLayout::Horizontal,
            toggle_label: "Menu".to_string(),
            class: None,
        }
    }

    /// Set the brand/logo slot from plain text linking to `href`. The text is
    /// HTML-escaped by Maud.
    #[must_use]
    pub fn brand(self, label: &str, href: &str) -> Self {
        self.brand_html(maud::html! { (label) }, Some(href))
    }

    /// Set the brand/logo slot from pre-built [`maud::Markup`]. When `href`
    /// is `Some`, the brand renders as an anchor; when `None`, it renders as
    /// a `<span>` (e.g. for a plain wordmark with no link).
    ///
    /// Callers are responsible for escaping any user-supplied content inside `brand`.
    #[must_use]
    pub fn brand_html(mut self, brand: maud::Markup, href: Option<&str>) -> Self {
        self.brand = Some((brand, href.map(str::to_string)));
        self
    }

    /// Append a primary navigation item.
    #[must_use]
    pub fn item(mut self, item: NavItem) -> Self {
        self.items.push(item);
        self
    }

    /// Append multiple primary navigation items at once — convenient when
    /// building the list from a dynamic source (e.g. a loop over registered models).
    #[must_use]
    pub fn items(mut self, items: impl IntoIterator<Item = NavItem>) -> Self {
        self.items.extend(items);
        self
    }

    /// Append an item to the right-aligned trailing slot (account menu, CTA, etc.).
    #[must_use]
    pub fn trailing(mut self, item: NavItem) -> Self {
        self.trailing.push(item);
        self
    }

    /// Set the `aria-label` on the `<nav>` landmark (default `"Main"`). Give
    /// each `nav_bar` on a page a distinct label so screen-reader users can
    /// tell them apart.
    #[must_use]
    pub fn aria_label(mut self, label: &str) -> Self {
        self.aria_label = label.to_string();
        self
    }

    /// Set the layout variant (default [`NavBarLayout::Horizontal`]).
    #[must_use]
    pub const fn layout(mut self, layout: NavBarLayout) -> Self {
        self.layout = layout;
        self
    }

    /// Set the element `id` used to derive `aria-controls` target ids
    /// (default `"autumn-nav"`). Give a second `nav_bar` on the same page a
    /// distinct id so their collapse/menu ids don't collide.
    #[must_use]
    pub fn id(mut self, id: &str) -> Self {
        self.id = id.to_string();
        self
    }

    /// Set the visible/accessible text of the hamburger toggle button
    /// (default `"Menu"`).
    #[must_use]
    pub fn toggle_label(mut self, label: &str) -> Self {
        self.toggle_label = label.to_string();
        self
    }

    /// Add extra CSS class(es) to the root `autumn-nav` element.
    #[must_use]
    pub fn class(mut self, class: impl Into<String>) -> Self {
        self.class = Some(class.into());
        self
    }
}

/// Render an accessible top navigation bar (or sidebar, via [`NavBarLayout::Sidebar`]).
///
/// Emits a labelled `<nav>` landmark containing an optional brand, primary
/// items, a hidden-until-enhanced hamburger toggle, and an optional
/// right-aligned trailing item list.
///
/// Every [`NavItem::Link`] built with [`NavItem::link`] / `link_matched`
/// renders through [`nav_link_matched`], so it carries `class="autumn-active"` and
/// `aria-current="page"` when its `href` matches `current_path` — pair with
/// the [`CurrentPath`](crate::extract::CurrentPath) extractor to get
/// `current_path` from the incoming request. [`NavItem::plain_link`] never
/// becomes active, for links to unrelated tools that shouldn't claim
/// "you are here" (e.g. an external or admin-actuator link).
///
/// [`NavItem::menu`] renders a menu button plus a `role="menu"` popover of
/// sub-links. It is the same primitive as [`dropdown_menu`]. The trigger uses
/// `command`/`commandfor`, so the menu opens and closes with JavaScript off.
/// The hamburger toggle stays hidden until the runtime shows it. The
/// framework's `/static/js/autumn-widgets.js` runtime (same-origin, CSP-safe
/// under the default `script-src 'self'`) wires the hamburger and the arrow
/// keys. No app-authored JavaScript, and no inline `<script>` or `style=`
/// attributes, are ever required.
///
/// # Keyboard (with `autumn-widgets.js`)
///
/// Each dropdown is an APG menu button. `Enter`, `Space` or `ArrowDown` opens
/// the menu on the first item; `ArrowUp` opens it on the last. In the menu:
/// `ArrowDown`/`ArrowUp` (wrapping), `Home`/`End`, a letter (typeahead),
/// `Escape` (close, focus returns to the button) and `Tab` (close). The
/// browser handles light dismiss. The toggle id is `{menu-id}-toggle`.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-nav` | Root `<nav>` landmark |
/// | `.autumn-nav--sidebar` | Modifier added by [`NavBarLayout::Sidebar`] |
/// | `.autumn-nav--enhanced` | Added by the JS runtime once initialized |
/// | `.autumn-nav__brand` | Brand link (`<a>`) or wordmark (`<span>`) |
/// | `.autumn-nav__toggle` | Hamburger `<button>` |
/// | `.autumn-nav__collapse` | Collapsible container wrapping the item lists |
/// | `.autumn-nav__collapse--open` | Added by the JS runtime while expanded |
/// | `.autumn-nav__items` | Primary or trailing `<ul>` |
/// | `.autumn-nav__items--trailing` | Added to the trailing `<ul>` only |
/// | `.autumn-nav__item` | Every `<li>` wrapping a link |
/// | `.autumn-nav__item--menu` | `<li>` wrapping a dropdown menu |
/// | `.autumn-nav__section` | Non-interactive group-label `<li>` |
/// | `.autumn-nav__menu-toggle` | Dropdown trigger `<button>` |
/// | `.autumn-nav__menu` | Dropdown sub-item `<ul>` (a popover) |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{NavBarConfig, NavItem, NavLinkMatch, NavMenu, nav_bar};
///
/// let config = NavBarConfig::new()
///     .brand("Acme", "/")
///     .item(NavItem::link("/", "Home"))
///     .item(NavItem::link_matched("/posts", "Posts", NavLinkMatch::Prefix))
///     .item(NavItem::link("/about", "About"))
///     .item(NavItem::menu(
///         NavMenu::new("Products")
///             .link("/widgets", "Widgets")
///             .link("/gadgets", "Gadgets"),
///     ))
///     .trailing(NavItem::plain_link("/login", "Log in"));
///
/// let html = nav_bar("/posts/3", &config).into_string();
/// assert!(html.contains(r#"aria-current="page""#));
/// assert!(!html.contains("style="));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn nav_bar(current_path: &str, config: &NavBarConfig) -> maud::Markup {
    let root_class = merge_class(
        match config.layout {
            NavBarLayout::Horizontal => "autumn-nav",
            NavBarLayout::Sidebar => "autumn-nav autumn-nav--sidebar",
        },
        config.class.as_deref(),
    );
    let collapse_id = format!("{}-collapse", config.id);
    let mut menu_counter = 0usize;

    maud::html! {
        nav id=(config.id) class=(root_class) aria-label=(config.aria_label) data-autumn-nav {
            @if let Some((brand, href)) = &config.brand {
                @if let Some(href) = href {
                    a class="autumn-nav__brand" href=(href) { (brand) }
                } @else {
                    span class="autumn-nav__brand" { (brand) }
                }
            }
            button type="button" class="autumn-nav__toggle" aria-expanded="false"
                aria-controls=(collapse_id) hidden data-nav-toggle {
                (config.toggle_label)
            }
            div id=(collapse_id) class="autumn-nav__collapse" {
                @if !config.items.is_empty() {
                    ul class="autumn-nav__items" {
                        (render_nav_items(current_path, &config.items, &config.id, &mut menu_counter))
                    }
                }
                @if !config.trailing.is_empty() {
                    ul class="autumn-nav__items autumn-nav__items--trailing" {
                        (render_nav_items(current_path, &config.trailing, &config.id, &mut menu_counter))
                    }
                }
            }
        }
    }
}

/// Render the `<li>` items for one [`nav_bar`] item list (primary or trailing).
#[cfg(feature = "maud")]
fn render_nav_items(
    current_path: &str,
    items: &[NavItem],
    id: &str,
    menu_counter: &mut usize,
) -> maud::Markup {
    maud::html! {
        @for item in items {
            @match item {
                NavItem::Link(link) => {
                    li class="autumn-nav__item" { (render_nav_link_item(current_path, link)) }
                }
                NavItem::Section(label) => {
                    li class="autumn-nav__section" { (label) }
                }
                NavItem::Menu(menu) => {
                    (render_nav_menu(current_path, menu, id, menu_counter))
                }
            }
        }
    }
}

/// Render a single nav link: through [`nav_link_matched`] when it has a
/// [`NavLinkMatch`] mode, or as a plain never-active anchor otherwise.
#[cfg(feature = "maud")]
fn render_nav_link_item(current_path: &str, link: &NavLinkItem) -> maud::Markup {
    if let Some(mode) = link.match_mode {
        return nav_link_matched(current_path, &link.href, &link.label, mode);
    }
    maud::html! { a href=(link.href) { (link.label) } }
}

/// Render a dropdown menu `<li>`: a trigger `<button>` plus its `<ul>` of
/// sub-links, using and incrementing `menu_counter` to derive a unique id.
///
/// The menu is the same popover menu as [`dropdown_menu`].
#[cfg(feature = "maud")]
fn render_nav_menu(
    current_path: &str,
    menu: &NavMenu,
    id: &str,
    menu_counter: &mut usize,
) -> maud::Markup {
    *menu_counter += 1;
    let menu_id = format!("{id}-menu-{menu_counter}");
    let toggle_id = format!("{menu_id}-toggle");
    maud::html! {
        li class="autumn-nav__item autumn-nav__item--menu" {
            button type="button" id=(toggle_id) class="autumn-nav__menu-toggle"
                command="toggle-popover" commandfor=(menu_id) aria-controls=(menu_id)
                aria-haspopup="menu" data-popover-toggle=(menu_id) {
                (menu.label)
            }
            ul id=(menu_id) class="autumn-popover__panel autumn-menu autumn-nav__menu" popover
                role="menu" aria-labelledby=(toggle_id) data-side="bottom" data-align="start"
                data-autumn-menu data-autumn-roving data-autumn-orientation="vertical"
                data-autumn-typeahead {
                @for link in &menu.links {
                    li role="none" class="autumn-nav__item" {
                        (render_nav_menu_item(current_path, link))
                    }
                }
            }
        }
    }
}

/// Render a nav link as a `menuitem`. It marks the active page like [`nav_link_matched`].
#[cfg(feature = "maud")]
fn render_nav_menu_item(current_path: &str, link: &NavLinkItem) -> maud::Markup {
    let active = link
        .match_mode
        .is_some_and(|mode| nav_link_is_active(current_path, &link.href, mode));
    maud::html! {
        a href=(link.href) role="menuitem"
            class=(if active { "autumn-menu__item autumn-active" } else { "autumn-menu__item" })
            aria-current=[active.then_some("page")] { (link.label) }
    }
}

// ── popover + dropdown menu (issue #3402) ────────────────────────────────────

/// Which side of the trigger a [`popover`] opens on.
///
/// `Start` and `End` are logical: they follow the writing direction, so
/// `Start` is the left side in a left-to-right page.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PopoverSide {
    /// Above the trigger.
    Top,
    /// Below the trigger (default).
    #[default]
    Bottom,
    /// Before the trigger, in the writing direction.
    Start,
    /// After the trigger, in the writing direction.
    End,
}

#[cfg(feature = "maud")]
impl PopoverSide {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::Start => "start",
            Self::End => "end",
        }
    }
}

/// How a [`popover`] lines up with the trigger along the other axis.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PopoverAlign {
    /// Edges meet at the start (default).
    #[default]
    Start,
    /// Centered on the trigger.
    Center,
    /// Edges meet at the end.
    End,
}

#[cfg(feature = "maud")]
impl PopoverAlign {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Center => "center",
            Self::End => "end",
        }
    }
}

/// Settings for [`popover`] and [`dropdown_menu`].
///
/// `id` is the id of the popover panel. The trigger gets `{id}-trigger`.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy)]
pub struct PopoverConfig<'a> {
    id: &'a str,
    side: PopoverSide,
    align: PopoverAlign,
    class: Option<&'a str>,
    trigger_class: Option<&'a str>,
}

#[cfg(feature = "maud")]
impl<'a> PopoverConfig<'a> {
    /// Start a config. `id` must be unique on the page. An empty `id`
    /// panics in a debug build.
    #[must_use]
    pub const fn new(id: &'a str) -> Self {
        debug_assert!(!id.is_empty(), "PopoverConfig::new needs a non-empty id");
        Self {
            id,
            side: PopoverSide::Bottom,
            align: PopoverAlign::Start,
            class: None,
            trigger_class: None,
        }
    }

    /// Set the side. The browser flips it when the panel does not fit.
    #[must_use]
    pub const fn side(mut self, side: PopoverSide) -> Self {
        self.side = side;
        self
    }

    /// Set the alignment.
    #[must_use]
    pub const fn align(mut self, align: PopoverAlign) -> Self {
        self.align = align;
        self
    }

    /// Add a class to the root element.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }

    /// Add a class to the trigger button.
    #[must_use]
    pub const fn trigger_class(mut self, class: &'a str) -> Self {
        self.trigger_class = Some(class);
        self
    }
}

/// The trigger button of a popover or a menu.
///
/// `command`/`commandfor` open the panel with no JavaScript.
/// `data-popover-toggle` is the hook for browsers without invoker commands.
/// The server sends no `aria-expanded`: it cannot know the state. The
/// runtime adds it.
#[cfg(feature = "maud")]
fn popover_trigger(label: &str, config: &PopoverConfig<'_>, menu: bool) -> maud::Markup {
    maud::html! {
        button type="button" id=(format!("{}-trigger", config.id))
            class=(merge_class("autumn-popover__trigger", config.trigger_class))
            command="toggle-popover" commandfor=(config.id) aria-controls=(config.id)
            aria-haspopup=[menu.then_some("menu")] data-popover-toggle=(config.id) {
            (label)
        }
    }
}

/// Render a trigger button and a native `popover` panel.
///
/// The trigger uses `command="toggle-popover"` and `commandfor`. The
/// browser opens and closes the panel, and handles light dismiss and
/// Escape. CSS anchor positioning places the panel by
/// [`PopoverConfig::side`] and [`PopoverConfig::align`], and flips it at
/// the viewport edge. No JavaScript is needed.
///
/// The panel has `role="group"` and is named by the trigger.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{PopoverConfig, popover};
///
/// let html = popover(
///     "Help",
///     &maud::html! { p { "Press ? for shortcuts." } },
///     &PopoverConfig::new("help"),
/// )
/// .into_string();
/// assert!(html.contains(r#"commandfor="help""#));
/// assert!(html.contains("popover"));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn popover(trigger: &str, content: &maud::Markup, config: &PopoverConfig<'_>) -> maud::Markup {
    maud::html! {
        div class=(merge_class("autumn-popover", config.class)) {
            (popover_trigger(trigger, config, false))
            div id=(config.id) class="autumn-popover__panel" popover role="group"
                aria-labelledby=(format!("{}-trigger", config.id))
                data-side=(config.side.as_str()) data-align=(config.align.as_str())
                data-autumn-popover {
                (content)
            }
        }
    }
}

#[cfg(feature = "maud")]
#[derive(Clone)]
enum MenuKind {
    Link {
        label: String,
        href: String,
    },
    Action {
        label: String,
        action: String,
        method: http::Method,
        csrf: String,
        csrf_field: Option<String>,
    },
    Separator,
    Group {
        label: String,
        entries: Vec<MenuEntry>,
    },
}

// `Debug` hides the CSRF token, so a log line never carries it.
#[cfg(feature = "maud")]
impl std::fmt::Debug for MenuKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Link { label, href } => f
                .debug_struct("Link")
                .field("label", label)
                .field("href", href)
                .finish(),
            Self::Action {
                label,
                action,
                method,
                csrf_field,
                ..
            } => f
                .debug_struct("Action")
                .field("label", label)
                .field("action", action)
                .field("method", method)
                .field("csrf", &"<redacted>")
                .field("csrf_field", csrf_field)
                .finish(),
            Self::Separator => f.write_str("Separator"),
            Self::Group { label, entries } => f
                .debug_struct("Group")
                .field("label", label)
                .field("entries", entries)
                .finish(),
        }
    }
}

/// One entry of a [`dropdown_menu`].
///
/// A link, a `button_to` form, a separator or a labelled group.
#[cfg(feature = "maud")]
#[derive(Debug, Clone)]
pub struct MenuEntry {
    kind: MenuKind,
    destructive: bool,
}

#[cfg(feature = "maud")]
impl MenuEntry {
    const fn of(kind: MenuKind) -> Self {
        Self {
            kind,
            destructive: false,
        }
    }

    /// A link item (`<a role="menuitem">`).
    #[must_use]
    pub fn link(label: impl Into<String>, href: impl Into<String>) -> Self {
        Self::of(MenuKind::Link {
            label: label.into(),
            href: href.into(),
        })
    }

    /// A form item. It works without JavaScript and carries the CSRF token.
    ///
    /// Same rules as [`button_to`](crate::links::button_to).
    ///
    /// # Panics
    ///
    /// Panics when `method` is not `GET`, `POST`, `PUT`, `PATCH` or
    /// `DELETE`. A `<form>` cannot send any other method.
    #[must_use]
    pub fn action(
        label: impl Into<String>,
        action: impl Into<String>,
        method: http::Method,
        csrf_token: impl Into<String>,
    ) -> Self {
        assert!(
            [
                http::Method::GET,
                http::Method::POST,
                http::Method::PUT,
                http::Method::PATCH,
                http::Method::DELETE,
            ]
            .contains(&method),
            "MenuEntry::action only supports GET, POST, PUT, PATCH, or DELETE (got {method})"
        );
        Self::of(MenuKind::Action {
            label: label.into(),
            action: action.into(),
            method,
            csrf: csrf_token.into(),
            csrf_field: None,
        })
    }

    /// Set the name of the hidden CSRF input of an [`action`](Self::action)
    /// item. Use it when `security.csrf.form_field` is not `_csrf`. Other
    /// entries stay as they are.
    #[must_use]
    pub fn csrf_field(mut self, name: impl Into<String>) -> Self {
        if let MenuKind::Action { csrf_field, .. } = &mut self.kind {
            *csrf_field = Some(name.into());
        }
        self
    }

    /// A line between groups of items.
    #[must_use]
    pub const fn separator() -> Self {
        Self::of(MenuKind::Separator)
    }

    /// A labelled group of items.
    #[must_use]
    pub fn group(label: impl Into<String>, entries: Vec<Self>) -> Self {
        Self::of(MenuKind::Group {
            label: label.into(),
            entries,
        })
    }

    /// Mark a link or form item as destructive. Other entries stay as they are.
    #[must_use]
    pub const fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }
}

#[cfg(feature = "maud")]
fn render_menu_entry(entry: &MenuEntry) -> maud::Markup {
    let item_class = if entry.destructive {
        "autumn-menu__item autumn-menu__item--destructive"
    } else {
        "autumn-menu__item"
    };
    match &entry.kind {
        MenuKind::Link { label, href } => maud::html! {
            a href=(href) role="menuitem" class=(item_class) { (label) }
        },
        MenuKind::Action {
            label,
            action,
            method,
            csrf,
            csrf_field,
        } => maud::html! {
            // The form is generic. `role="none"` keeps the menu's children valid.
            div role="none" class="autumn-menu__form" {
                (crate::links::button_to_with(
                    label,
                    action,
                    method.clone(),
                    csrf,
                    &crate::links::ButtonToOptions::new()
                        .class(item_class)
                        .csrf_field(csrf_field.as_deref().unwrap_or("_csrf"))
                        .attrs(&[("role", "menuitem")]),
                ))
            }
        },
        MenuKind::Separator => maud::html! {
            hr role="separator" class="autumn-menu__separator";
        },
        MenuKind::Group { label, entries } => maud::html! {
            div role="group" aria-label=(label) class="autumn-menu__group" {
                div class="autumn-menu__label" aria-hidden="true" { (label) }
                @for e in entries { (render_menu_entry(e)) }
            }
        },
    }
}

/// Render an APG menu button: a trigger and a `role="menu"` popover.
///
/// Items are links or `button_to` forms, so they work without JavaScript.
/// With `autumn-widgets.js`, the menu has the APG keys: Enter, Space,
/// `ArrowDown` or `ArrowUp` opens it on the first or last item; the arrow keys
/// wrap; Home/End and typeahead move; Escape and Tab close it, and focus
/// goes back to the trigger. Without JavaScript the popover still opens and
/// closes; only the arrow keys need JavaScript.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{MenuEntry, PopoverConfig, dropdown_menu};
/// use http::Method;
///
/// let html = dropdown_menu(
///     "Actions",
///     &[
///         MenuEntry::link("Edit", "/posts/1/edit"),
///         MenuEntry::separator(),
///         MenuEntry::action("Delete", "/posts/1", Method::DELETE, "csrf-token")
///             .destructive(),
///     ],
///     &PopoverConfig::new("post-actions"),
/// )
/// .into_string();
/// assert!(html.contains(r#"role="menu""#));
/// assert!(html.contains(r#"role="menuitem""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn dropdown_menu(
    trigger: &str,
    items: &[MenuEntry],
    config: &PopoverConfig<'_>,
) -> maud::Markup {
    maud::html! {
        div class=(merge_class("autumn-popover", config.class)) {
            (popover_trigger(trigger, config, true))
            div id=(config.id) class="autumn-popover__panel autumn-menu" popover role="menu"
                aria-labelledby=(format!("{}-trigger", config.id))
                data-side=(config.side.as_str()) data-align=(config.align.as_str())
                data-autumn-menu data-autumn-roving data-autumn-orientation="vertical"
                data-autumn-typeahead {
                @for e in items { (render_menu_entry(e)) }
            }
        }
    }
}

#[cfg(all(test, feature = "maud"))]
mod tests {
    use super::*;
    use http::Method;

    // ── breadcrumb ─────────────────────────────────────────────────────

    #[test]
    fn breadcrumb_empty_slice_renders_nothing_no_panic() {
        let html = breadcrumb(&[]).into_string();
        assert!(html.is_empty(), "expected empty output, got: {html}");
    }

    #[test]
    fn breadcrumb_wrapped_in_nav_with_aria_label() {
        let crumbs = [Crumb::link("Home", "/"), Crumb::current("Posts")];
        let html = breadcrumb(&crumbs).into_string();
        assert!(html.contains("<nav"), "{html}");
        assert!(html.contains("aria-label"), "{html}");
    }

    #[test]
    fn breadcrumb_contains_ordered_list() {
        let crumbs = [Crumb::link("Home", "/"), Crumb::current("Posts")];
        let html = breadcrumb(&crumbs).into_string();
        assert!(html.contains("<ol"), "{html}");
        assert!(html.contains("</ol>"), "{html}");
    }

    #[test]
    fn breadcrumb_last_crumb_has_aria_current_page() {
        let crumbs = [
            Crumb::link("Home", "/"),
            Crumb::link("Posts", "/posts"),
            Crumb::current("My Post"),
        ];
        let html = breadcrumb(&crumbs).into_string();
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    #[test]
    fn breadcrumb_preceding_crumbs_render_as_links() {
        let crumbs = [
            Crumb::link("Home", "/"),
            Crumb::link("Posts", "/posts"),
            Crumb::current("My Post"),
        ];
        let html = breadcrumb(&crumbs).into_string();
        assert!(html.contains(r#"href="/""#), "{html}");
        assert!(html.contains(r#"href="/posts""#), "{html}");
    }

    #[test]
    fn breadcrumb_last_crumb_is_not_a_link() {
        let crumbs = [Crumb::link("Home", "/"), Crumb::current("Current Page")];
        let html = breadcrumb(&crumbs).into_string();
        // "Current Page" must not be wrapped in an <a>; it carries aria-current="page" instead.
        assert!(html.contains("Current Page"), "{html}");
        // The current page span must not itself be a link.
        assert!(!html.contains("<a href=\"\">Current Page"), "{html}");
        assert!(html.contains("aria-current=\"page\""), "{html}");
        assert!(html.contains("autumn-breadcrumb__current"), "{html}");
    }

    #[test]
    fn breadcrumb_single_crumb_no_leading_separator() {
        let crumbs = [Crumb::current("Only Page")];
        let html = breadcrumb(&crumbs).into_string();
        // No separator before the first (and only) item
        assert!(!html.contains("aria-hidden"), "{html}");
        assert!(html.contains("Only Page"), "{html}");
    }

    #[test]
    fn breadcrumb_single_crumb_has_aria_current_page() {
        let crumbs = [Crumb::current("Only Page")];
        let html = breadcrumb(&crumbs).into_string();
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    #[test]
    fn breadcrumb_separators_are_aria_hidden() {
        let crumbs = [
            Crumb::link("Home", "/"),
            Crumb::link("Posts", "/posts"),
            Crumb::current("Current"),
        ];
        let html = breadcrumb(&crumbs).into_string();
        // Every separator must carry aria-hidden="true"
        assert!(html.contains(r#"aria-hidden="true""#), "{html}");
    }

    #[test]
    fn breadcrumb_label_containing_html_is_escaped() {
        let crumbs = [
            Crumb::link("<script>alert(1)</script>", "/"),
            Crumb::current("Safe"),
        ];
        let html = breadcrumb(&crumbs).into_string();
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    #[test]
    fn breadcrumb_href_containing_html_is_escaped() {
        let crumbs = [
            Crumb::link("Home", r#"/" onerror="bad"#),
            Crumb::current("Page"),
        ];
        let html = breadcrumb(&crumbs).into_string();
        assert!(!html.contains("onerror=\"bad"), "{html}");
    }

    #[test]
    fn breadcrumb_crumb_link_constructor() {
        let c = Crumb::link("Posts", "/posts");
        assert_eq!(c.label, "Posts");
        assert_eq!(c.href, Some("/posts"));
    }

    #[test]
    fn breadcrumb_crumb_current_constructor() {
        let c = Crumb::current("My Page");
        assert_eq!(c.label, "My Page");
        assert!(c.href.is_none());
    }

    #[test]
    fn breadcrumb_non_last_crumb_without_href_renders_span_not_bare_text() {
        // A Crumb with href:None in a non-last position must still be wrapped
        // in a <span> so it has an accessible element, not invisible bare text.
        let crumbs = [Crumb::current("Unlinked Middle"), Crumb::current("Current")];
        let html = breadcrumb(&crumbs).into_string();
        assert!(
            html.contains("<span class=\"autumn-breadcrumb__text\">Unlinked Middle</span>"),
            "{html}"
        );
        // Only the last item carries aria-current
        assert_eq!(html.matches(r#"aria-current="page""#).count(), 1, "{html}");
    }

    // ── nav_link ──────────────────────────────────────────────────────────

    #[test]
    fn nav_link_exact_match_is_active() {
        let html = nav_link("/admin/posts", "/admin/posts", "Posts").into_string();
        assert!(html.contains(r#"class="autumn-active""#), "{html}");
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    #[test]
    fn nav_link_no_match_is_inactive() {
        let html = nav_link("/admin/jobs", "/admin/posts", "Posts").into_string();
        assert!(!html.contains("active"), "{html}");
        assert!(!html.contains("aria-current"), "{html}");
    }

    #[test]
    fn nav_link_exact_mode_rejects_sub_paths() {
        let html = nav_link("/admin/posts/3", "/admin/posts", "Posts").into_string();
        assert!(!html.contains("active"), "{html}");
        assert!(!html.contains("aria-current"), "{html}");
    }

    #[test]
    fn nav_link_prefix_match_is_active() {
        let html = nav_link_matched(
            "/admin/posts/3/edit",
            "/admin/posts",
            "Posts",
            NavLinkMatch::Prefix,
        )
        .into_string();
        assert!(html.contains(r#"class="autumn-active""#), "{html}");
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    #[test]
    fn nav_link_prefix_mode_rejects_false_positive() {
        // "/admin/postsX" must NOT match "/admin/posts" as a prefix.
        let html = nav_link_matched(
            "/admin/postsX",
            "/admin/posts",
            "Posts",
            NavLinkMatch::Prefix,
        )
        .into_string();
        assert!(!html.contains("active"), "{html}");
        assert!(!html.contains("aria-current"), "{html}");
    }

    #[test]
    fn nav_link_prefix_mode_exact_path_is_active() {
        let html = nav_link_matched(
            "/admin/posts",
            "/admin/posts",
            "Posts",
            NavLinkMatch::Prefix,
        )
        .into_string();
        assert!(html.contains(r#"class="autumn-active""#), "{html}");
    }

    #[test]
    fn nav_link_prefix_mode_trailing_slash_href_still_matches() {
        // href with a trailing slash must behave the same as without one.
        let html = nav_link_matched(
            "/admin/posts/3/edit",
            "/admin/posts/",
            "Posts",
            NavLinkMatch::Prefix,
        )
        .into_string();
        assert!(html.contains(r#"class="autumn-active""#), "{html}");
    }

    #[test]
    fn nav_link_prefix_mode_empty_href_never_matches() {
        // An empty href must never act as a wildcard that matches every path.
        let html = nav_link_matched("/admin/posts/3/edit", "", "Posts", NavLinkMatch::Prefix)
            .into_string();
        assert!(!html.contains("active"), "{html}");
        assert!(!html.contains("aria-current"), "{html}");
    }

    #[test]
    fn nav_link_prefix_mode_root_href_does_not_match_every_path() {
        // href="/" must not wildcard-match every path either; only an exact "/"
        // current_path is active.
        let html =
            nav_link_matched("/admin/posts", "/", "Home", NavLinkMatch::Prefix).into_string();
        assert!(!html.contains("active"), "{html}");

        let html = nav_link_matched("/", "/", "Home", NavLinkMatch::Prefix).into_string();
        assert!(html.contains(r#"class="autumn-active""#), "{html}");
    }

    #[test]
    fn nav_link_renders_label_and_href() {
        let html = nav_link("/somewhere/else", "/admin/posts", "Posts").into_string();
        assert!(html.contains(r#"href="/admin/posts""#), "{html}");
        assert!(html.contains(">Posts<"), "{html}");
    }

    // ── locale switcher (issue #1251) ─────────────────────────────────────

    #[test]
    fn localized_path_prepends_locale_segment() {
        assert_eq!(localized_path("/posts", "es"), "/es/posts");
    }

    #[test]
    fn localized_path_preserves_query_string() {
        assert_eq!(
            localized_path("/posts?sort=asc&page=2", "es"),
            "/es/posts?sort=asc&page=2"
        );
    }

    #[test]
    fn localized_path_handles_root() {
        // No trailing slash: axum's `nest("/es", router)` makes bare "/es"
        // match the inner router's "/" route, while "/es/" 404s.
        assert_eq!(localized_path("/", "es"), "/es");
    }

    #[test]
    fn localized_path_handles_root_with_query() {
        assert_eq!(
            localized_path("/?ref=newsletter", "es"),
            "/es?ref=newsletter"
        );
    }

    #[test]
    fn locale_switcher_links_every_locale_except_current() {
        let supported = vec!["en".to_owned(), "es".to_owned(), "fr".to_owned()];
        let html = locale_switcher("/posts", "es", &supported).into_string();
        assert!(html.contains(r#"href="/en/posts""#), "{html}");
        assert!(html.contains(r#"href="/fr/posts""#), "{html}");
        assert!(
            !html.contains(r#"href="/es/posts""#),
            "current locale must not self-link: {html}"
        );
        assert!(html.contains(r#"aria-current="true""#), "{html}");
    }

    #[test]
    fn locale_switcher_preserves_query_string_in_every_link() {
        let supported = vec!["en".to_owned(), "es".to_owned()];
        let html = locale_switcher("/posts?sort=asc", "en", &supported).into_string();
        assert!(html.contains(r#"href="/es/posts?sort=asc""#), "{html}");
    }

    // ── nav_bar ──────────────────────────────────────────────────────────

    #[test]
    fn nav_bar_renders_single_nav_landmark_with_default_label() {
        let config = NavBarConfig::new();
        let html = nav_bar("/", &config).into_string();
        assert_eq!(html.matches("<nav").count(), 1, "{html}");
        assert!(html.contains(r#"aria-label="Main""#), "{html}");
        assert!(html.contains(r#"class="autumn-nav""#), "{html}");
        assert!(html.contains("data-autumn-nav"), "{html}");
    }

    #[test]
    fn nav_bar_custom_aria_label() {
        let config = NavBarConfig::new().aria_label("Admin navigation");
        let html = nav_bar("/", &config).into_string();
        assert!(html.contains(r#"aria-label="Admin navigation""#), "{html}");
    }

    #[test]
    fn nav_bar_brand_text_renders_brand_link() {
        let config = NavBarConfig::new().brand("Acme", "/");
        let html = nav_bar("/", &config).into_string();
        assert!(
            html.contains(r#"<a class="autumn-nav__brand" href="/">Acme</a>"#),
            "{html}"
        );
    }

    #[test]
    fn nav_bar_brand_html_without_href_renders_span() {
        let config = NavBarConfig::new().brand_html(maud::html! { "Acme" }, None);
        let html = nav_bar("/", &config).into_string();
        assert!(
            html.contains(r#"<span class="autumn-nav__brand">Acme</span>"#),
            "{html}"
        );
        assert!(!html.contains("<a class=\"autumn-nav__brand\""), "{html}");
    }

    #[test]
    fn nav_bar_without_brand_omits_brand_element() {
        let config = NavBarConfig::new();
        let html = nav_bar("/", &config).into_string();
        assert!(!html.contains("autumn-nav__brand"), "{html}");
    }

    #[test]
    fn nav_bar_active_link_gets_aria_current() {
        let config = NavBarConfig::new().item(NavItem::link("/posts", "Posts"));
        let html = nav_bar("/posts", &config).into_string();
        assert_eq!(html.matches(r#"aria-current="page""#).count(), 1, "{html}");
        assert!(html.contains(r#"class="autumn-active""#), "{html}");
    }

    #[test]
    fn nav_bar_prefix_match_mode_activates_on_subpath() {
        let config = NavBarConfig::new().item(NavItem::link_matched(
            "/posts",
            "Posts",
            NavLinkMatch::Prefix,
        ));
        let html = nav_bar("/posts/3/edit", &config).into_string();
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    #[test]
    fn nav_bar_plain_link_never_active() {
        let config = NavBarConfig::new().item(NavItem::plain_link("/actuator/ui", "Actuator"));
        let html = nav_bar("/actuator/ui", &config).into_string();
        assert!(!html.contains("aria-current"), "{html}");
        assert!(!html.contains(r#"class="autumn-active""#), "{html}");
        assert!(html.contains(r#"href="/actuator/ui""#), "{html}");
    }

    #[test]
    fn nav_bar_section_renders_label_li() {
        let config = NavBarConfig::new().item(NavItem::section("System"));
        let html = nav_bar("/", &config).into_string();
        assert!(
            html.contains(r#"<li class="autumn-nav__section">System</li>"#),
            "{html}"
        );
    }

    #[test]
    fn nav_bar_menu_renders_button_controlling_ul() {
        let config = NavBarConfig::new().item(NavItem::menu(
            NavMenu::new("Products").link("/widgets", "Widgets"),
        ));
        let html = nav_bar("/", &config).into_string();
        assert!(html.contains(r#"type="button""#), "{html}");
        assert!(
            html.contains(r#"aria-controls="autumn-nav-menu-1""#),
            "{html}"
        );
        assert!(
            html.contains(r#"<ul id="autumn-nav-menu-1" class="autumn-popover__panel autumn-menu autumn-nav__menu" popover"#),
            "{html}"
        );
    }

    #[test]
    fn nav_bar_menu_binds_keyboard_core() {
        let config = NavBarConfig::new().item(NavItem::menu(
            NavMenu::new("Products").link("/widgets", "Widgets"),
        ));
        let html = nav_bar("/", &config).into_string();
        let menu = html
            .split(r#"<ul id="autumn-nav-menu-1""#)
            .nth(1)
            .and_then(|rest| rest.split('>').next())
            .expect("menu tag");
        assert!(menu.contains("data-autumn-roving"), "{html}");
        assert!(
            menu.contains(r#"data-autumn-orientation="vertical""#),
            "{html}"
        );
        assert!(menu.contains("data-autumn-typeahead"), "{html}");
        // `aria-labelledby` on the menu needs a toggle id.
        assert!(html.contains(r#"id="autumn-nav-menu-1-toggle""#), "{html}");
        assert!(
            html.contains(r#"aria-labelledby="autumn-nav-menu-1-toggle""#),
            "{html}"
        );
    }

    #[test]
    fn nav_bar_menu_sub_link_active_gets_aria_current() {
        let config = NavBarConfig::new().item(NavItem::menu(
            NavMenu::new("Products").link("/widgets", "Widgets"),
        ));
        let html = nav_bar("/widgets", &config).into_string();
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    #[test]
    fn nav_bar_menu_ids_unique_and_respect_custom_id() {
        let config = NavBarConfig::new()
            .item(NavItem::menu(NavMenu::new("A").link("/a", "A")))
            .item(NavItem::menu(NavMenu::new("B").link("/b", "B")))
            .id("top");
        let html = nav_bar("/", &config).into_string();
        assert!(html.contains(r#"aria-controls="top-menu-1""#), "{html}");
        assert!(html.contains(r#"id="top-menu-1""#), "{html}");
        assert!(html.contains(r#"aria-controls="top-menu-2""#), "{html}");
        assert!(html.contains(r#"id="top-menu-2""#), "{html}");
        assert!(html.contains(r#"id="top-collapse""#), "{html}");
    }

    #[test]
    fn nav_bar_toggle_button_is_hidden_with_accessible_name() {
        let config = NavBarConfig::new();
        let html = nav_bar("/", &config).into_string();
        assert!(html.contains("data-nav-toggle"), "{html}");
        assert!(html.contains("hidden"), "{html}");
        assert!(html.contains(r#"aria-expanded="false""#), "{html}");
        assert!(
            html.contains(r#"aria-controls="autumn-nav-collapse""#),
            "{html}"
        );
        assert!(html.contains(">Menu<"), "{html}");

        let config = NavBarConfig::new().toggle_label("Navigation");
        let html = nav_bar("/", &config).into_string();
        assert!(html.contains(">Navigation<"), "{html}");
    }

    #[test]
    fn nav_bar_trailing_slot_renders_separate_list() {
        let config = NavBarConfig::new().trailing(NavItem::plain_link("/login", "Log in"));
        let html = nav_bar("/", &config).into_string();
        assert!(html.contains("autumn-nav__items--trailing"), "{html}");
        assert!(html.contains(r#"href="/login""#), "{html}");

        let config = NavBarConfig::new();
        let html = nav_bar("/", &config).into_string();
        assert!(!html.contains("autumn-nav__items--trailing"), "{html}");
    }

    #[test]
    fn nav_bar_sidebar_layout_adds_modifier() {
        let config = NavBarConfig::new().layout(NavBarLayout::Sidebar);
        let html = nav_bar("/", &config).into_string();
        assert!(html.contains("autumn-nav--sidebar"), "{html}");

        let config = NavBarConfig::new();
        let html = nav_bar("/", &config).into_string();
        assert!(!html.contains("autumn-nav--sidebar"), "{html}");
    }

    #[test]
    fn nav_bar_class_merges_extra() {
        let config = NavBarConfig::new().class("admin-sidebar");
        let html = nav_bar("/", &config).into_string();
        assert!(
            html.contains(r#"class="autumn-nav admin-sidebar""#),
            "{html}"
        );
    }

    #[test]
    fn nav_item_constructors_accept_owned_strings_without_extra_copy() {
        // href/label should accept `impl Into<String>` (like Cta::primary),
        // not force callers with an already-owned String (e.g. from
        // format!()) through an extra borrow-then-reallocate round trip.
        let post_id = 3;
        let href: String = format!("/posts/{post_id}");
        let label: String = "Posts".to_string();
        let config = NavBarConfig::new()
            .item(NavItem::link(href, label))
            .item(NavItem::plain_link("/x".to_string(), "X".to_string()))
            .item(NavItem::section("System".to_string()))
            .item(NavItem::menu(
                NavMenu::new("Products".to_string())
                    .link(format!("/widgets/{post_id}"), "Widgets".to_string())
                    .plain_link("/external".to_string(), "External".to_string()),
            ));
        let html = nav_bar("/posts/3", &config).into_string();
        assert!(html.contains(r#"href="/posts/3""#), "{html}");
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    #[test]
    fn nav_bar_escapes_labels() {
        let config = NavBarConfig::new()
            .brand("<script>alert(1)</script>", "/")
            .item(NavItem::link("/posts", "<script>alert(2)</script>"))
            .item(NavItem::menu(
                NavMenu::new("<script>alert(3)</script>").link("/x", "<script>alert(4)</script>"),
            ));
        let html = nav_bar("/", &config).into_string();
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    #[test]
    fn nav_bar_emits_no_inline_style_or_script() {
        let config = NavBarConfig::new()
            .brand("Acme", "/")
            .item(NavItem::link("/", "Home"))
            .item(NavItem::menu(
                NavMenu::new("Products").link("/widgets", "Widgets"),
            ))
            .trailing(NavItem::plain_link("/login", "Log in"));
        let html = nav_bar("/", &config).into_string();
        assert!(!html.contains("style="), "{html}");
        assert!(!html.contains("<script"), "{html}");
    }

    const fn cfg() -> PopoverConfig<'static> {
        PopoverConfig::new("more")
    }

    fn pop() -> String {
        popover("Details", &maud::html! { p { "Body" } }, &cfg()).into_string()
    }

    fn menu() -> String {
        dropdown_menu(
            "Actions",
            &[
                MenuEntry::link("Edit", "/edit"),
                MenuEntry::separator(),
                MenuEntry::action("Delete", "/del", Method::DELETE, "tok").destructive(),
            ],
            &cfg(),
        )
        .into_string()
    }

    #[test]
    fn popover_trigger_uses_invoker_commands() {
        let html = pop();
        assert!(html.contains(r#"command="toggle-popover""#), "{html}");
        assert!(html.contains(r#"commandfor="more""#), "{html}");
        assert!(html.contains(r#"type="button""#), "{html}");
        assert!(html.contains(r#"id="more-trigger""#), "{html}");
        assert!(html.contains(">Details</button>"), "{html}");
    }

    #[test]
    fn popover_panel_is_a_native_popover() {
        let html = pop();
        assert!(html.contains(r#"id="more""#), "{html}");
        assert!(html.contains(" popover"), "{html}");
        assert!(!html.contains(r#"popover="manual""#), "{html}");
        assert!(html.contains(r#"role="group""#), "{html}");
        assert!(html.contains(r#"aria-labelledby="more-trigger""#), "{html}");
        assert!(html.contains("<p>Body</p>"), "{html}");
    }

    #[test]
    fn popover_has_no_inline_style_or_handler() {
        let html = pop();
        assert!(!html.contains("style="), "{html}");
        assert!(!html.contains("onclick"), "{html}");
        // The server cannot know the state. The runtime sets aria-expanded.
        assert!(!html.contains("aria-expanded"), "{html}");
    }

    #[test]
    fn popover_side_and_align_default_to_bottom_start() {
        let html = pop();
        assert!(html.contains(r#"data-side="bottom""#), "{html}");
        assert!(html.contains(r#"data-align="start""#), "{html}");
    }

    #[test]
    fn popover_side_and_align_are_configurable() {
        let c = PopoverConfig::new("p")
            .side(PopoverSide::Top)
            .align(PopoverAlign::End);
        let html = popover("T", &maud::html! {}, &c).into_string();
        assert!(html.contains(r#"data-side="top""#), "{html}");
        assert!(html.contains(r#"data-align="end""#), "{html}");
        for (side, want) in [
            (PopoverSide::Bottom, "bottom"),
            (PopoverSide::Start, "start"),
            (PopoverSide::End, "end"),
        ] {
            let c = PopoverConfig::new("p").side(side);
            let html = popover("T", &maud::html! {}, &c).into_string();
            assert!(html.contains(&format!(r#"data-side="{want}""#)), "{html}");
        }
        let c = PopoverConfig::new("p").align(PopoverAlign::Center);
        let html = popover("T", &maud::html! {}, &c).into_string();
        assert!(html.contains(r#"data-align="center""#), "{html}");
    }

    #[test]
    fn popover_classes_are_merged() {
        let c = PopoverConfig::new("p").class("mine").trigger_class("btn");
        let html = popover("T", &maud::html! {}, &c).into_string();
        assert!(html.contains(r#"class="autumn-popover mine""#), "{html}");
        assert!(
            html.contains(r#"class="autumn-popover__trigger btn""#),
            "{html}"
        );
    }

    #[test]
    fn popover_escapes_label_and_id() {
        let c = PopoverConfig::new(r#"a"b"#);
        let html = popover("<b>x</b>", &maud::html! {}, &c).into_string();
        assert!(html.contains("&lt;b&gt;x&lt;/b&gt;"), "{html}");
        assert!(html.contains("a&quot;b"), "{html}");
        assert!(!html.contains(r#"id="a"b""#), "{html}");
    }

    #[test]
    fn menu_trigger_is_an_apg_menu_button() {
        let html = menu();
        assert!(html.contains(r#"aria-haspopup="menu""#), "{html}");
        assert!(html.contains(r#"aria-controls="more""#), "{html}");
        assert!(html.contains(r#"command="toggle-popover""#), "{html}");
        assert!(html.contains(r#"commandfor="more""#), "{html}");
        assert!(!html.contains("aria-expanded"), "{html}");
    }

    #[test]
    fn menu_container_has_role_menu_and_keyboard_core() {
        let html = menu();
        assert!(html.contains(r#"role="menu""#), "{html}");
        assert!(html.contains(r#"aria-labelledby="more-trigger""#), "{html}");
        assert!(html.contains(" popover"), "{html}");
        assert!(html.contains("data-autumn-menu"), "{html}");
        assert!(html.contains("data-autumn-roving"), "{html}");
        assert!(
            html.contains(r#"data-autumn-orientation="vertical""#),
            "{html}"
        );
        assert!(html.contains("data-autumn-typeahead"), "{html}");
        // The browser owns Escape and light dismiss for a popover.
        assert!(!html.contains("data-autumn-dismiss"), "{html}");
    }

    #[test]
    fn menu_link_item_is_a_menuitem_anchor() {
        let html = menu();
        assert!(
            html.contains(r#"<a href="/edit" role="menuitem" class="autumn-menu__item">Edit</a>"#),
            "{html}"
        );
    }

    #[test]
    fn menu_destructive_item_has_a_variant_class() {
        let html = menu();
        assert!(
            html.contains("autumn-menu__item autumn-menu__item--destructive"),
            "{html}"
        );
        let link = dropdown_menu(
            "A",
            &[MenuEntry::link("Remove", "/rm").destructive()],
            &cfg(),
        )
        .into_string();
        assert!(link.contains("autumn-menu__item--destructive"), "{link}");
        assert_eq!(
            html.matches("autumn-menu__item--destructive").count(),
            1,
            "only the marked item: {html}"
        );
    }

    #[test]
    fn menu_group_is_labelled() {
        let html = dropdown_menu(
            "A",
            &[MenuEntry::group(
                "Account",
                vec![MenuEntry::link("Profile", "/me")],
            )],
            &cfg(),
        )
        .into_string();
        assert!(html.contains(r#"role="group""#), "{html}");
        assert!(html.contains(r#"aria-label="Account""#), "{html}");
        assert!(html.contains(r#"aria-hidden="true""#), "{html}");
        assert!(html.contains(">Account<"), "{html}");
        assert!(html.contains(">Profile</a>"), "{html}");
    }

    #[test]
    fn menu_escapes_labels_and_hrefs() {
        let html = dropdown_menu(
            "<i>t</i>",
            &[MenuEntry::link("<b>x</b>", r#"/a"b"#)],
            &cfg(),
        )
        .into_string();
        assert!(html.contains("&lt;i&gt;t&lt;/i&gt;"), "{html}");
        assert!(html.contains("&lt;b&gt;x&lt;/b&gt;"), "{html}");
        assert!(html.contains("/a&quot;b"), "{html}");
    }

    #[test]
    fn menu_without_items_still_renders_a_menu() {
        let html = dropdown_menu("A", &[], &cfg()).into_string();
        assert!(html.contains(r#"role="menu""#), "{html}");
    }

    #[test]
    fn popover_panel_is_marked_for_the_runtime() {
        assert!(pop().contains("data-autumn-popover"));
    }

    #[test]
    fn menu_entry_debug_hides_the_csrf_token() {
        let entry = MenuEntry::action("Delete", "/del", Method::DELETE, "secret-token");
        let shown = format!("{entry:?}");
        assert!(!shown.contains("secret-token"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
        let group = MenuEntry::group("G", vec![entry]);
        assert!(!format!("{group:?}").contains("secret-token"));
    }

    #[test]
    fn menu_action_uses_the_configured_csrf_field_name() {
        let html = dropdown_menu(
            "A",
            &[MenuEntry::action("Delete", "/del", Method::DELETE, "tok")
                .csrf_field("authenticity")],
            &cfg(),
        )
        .into_string();
        assert!(
            html.contains(r#"name="authenticity" value="tok""#),
            "{html}"
        );
        assert!(!html.contains(r#"name="_csrf""#), "{html}");
    }

    #[test]
    #[should_panic(expected = "MenuEntry::action only supports")]
    fn menu_action_rejects_an_unsupported_method_at_build_time() {
        let _ = MenuEntry::action("Peek", "/x", Method::HEAD, "t");
    }

    #[test]
    fn nav_bars_with_distinct_ids_get_distinct_menu_ids() {
        let menu = || NavItem::menu(NavMenu::new("A").link("/a", "A"));
        let one = nav_bar("/", &NavBarConfig::new().item(menu()).id("one")).into_string();
        let two = nav_bar("/", &NavBarConfig::new().item(menu()).id("two")).into_string();
        assert!(one.contains(r#"commandfor="one-menu-1""#), "{one}");
        assert!(two.contains(r#"commandfor="two-menu-1""#), "{two}");
    }

    #[test]
    fn nav_menu_is_a_popover_menu() {
        let config = NavBarConfig::new().item(NavItem::menu(
            NavMenu::new("Products").link("/widgets", "Widgets"),
        ));
        let html = nav_bar("/widgets", &config).into_string();
        assert!(html.contains(r#"command="toggle-popover""#), "{html}");
        assert!(html.contains(r#"commandfor="autumn-nav-menu-1""#), "{html}");
        assert!(html.contains(r#"aria-haspopup="menu""#), "{html}");
        assert!(html.contains(r#"role="menu""#), "{html}");
        assert!(html.contains(r#"<li role="none""#), "{html}");
        assert!(html.contains(r#"role="menuitem""#), "{html}");
        assert!(html.contains(r#"aria-current="page""#), "{html}");
        assert!(html.contains("data-autumn-menu"), "{html}");
        assert!(!html.contains("data-nav-menu-toggle"), "{html}");
        assert!(!html.contains("data-autumn-dismiss"), "{html}");
    }

    #[test]
    fn menu_action_item_is_a_csrf_form_button() {
        let html = menu();
        assert!(html.contains(r#"action="/del""#), "{html}");
        assert!(html.contains(r#"method="post""#), "{html}");
        assert!(html.contains(r#"name="_method" value="DELETE""#), "{html}");
        assert!(html.contains(r#"name="_csrf" value="tok""#), "{html}");
        assert!(html.contains(r#"<div role="none""#), "{html}");
        assert!(
            html.contains(r#"role="menuitem""#) && html.contains(">Delete</button>"),
            "{html}"
        );
    }

    #[test]
    fn menu_separator_has_role_separator() {
        let html = menu();
        assert!(html.contains(r#"role="separator""#), "{html}");
    }
}
