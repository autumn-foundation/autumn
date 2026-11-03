//! Layout and container widgets: card, stat card, hero, tabs,
//! accordion, collapsible, modal, drawer, separator and button.

use super::*;

// ── card ──────────────────────────────────────────────────────────────────

/// Heading level for the title element inside a [`card`].
///
/// Used by [`CardConfig::level`] to pick the `<h1>`–`<h6>` element that wraps
/// the card title. Defaults to `<h2>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeadingLevel {
    /// `<h1>`
    H1,
    /// `<h2>` (default)
    #[default]
    H2,
    /// `<h3>`
    H3,
    /// `<h4>`
    H4,
    /// `<h5>`
    H5,
    /// `<h6>`
    H6,
}

/// Configuration for a [`card`] widget.
///
/// Build with [`CardConfig::new`] and chain builder methods for optional slots.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::widgets::{CardConfig, HeadingLevel, card};
/// use maud::html;
///
/// let action = html! { a class="btn" href="/posts/new" { "+ New" } };
/// let config = CardConfig::new()
///     .title("Posts")
///     .level(HeadingLevel::H2)
///     .header_action(action);
/// let markup = card(&html! { p { "body" } }, &config);
/// ```
#[derive(Debug, Clone, Default)]
pub struct CardConfig<'a> {
    /// Optional title text rendered in a `<hN class="autumn-card__title">` element.
    /// Set via [`CardConfig::title`] (HTML-escaped) or [`CardConfig::title_html`]
    /// (pre-built [`maud::Markup`] for rich content).
    title: Option<maud::Markup>,
    /// Heading level for the title element (default [`HeadingLevel::H2`]).
    level: HeadingLevel,
    /// Optional right-side header slot — e.g. action buttons.
    /// Rendered inside `autumn-card__header` alongside the title.
    header_action: Option<maud::Markup>,
    /// Optional footer content rendered in `<div class="autumn-card__footer">`.
    footer: Option<maud::Markup>,
    /// Extra CSS class(es) appended to the root `autumn-card` element.
    class: Option<&'a str>,
}

impl<'a> CardConfig<'a> {
    /// Create a new card configuration with all slots empty and heading level `H2`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            title: None,
            level: HeadingLevel::H2,
            header_action: None,
            footer: None,
            class: None,
        }
    }

    /// Set the card title from a plain `&str`. The text is HTML-escaped by Maud.
    ///
    /// Use [`CardConfig::title_html`] when you need rich markup inside the heading.
    #[must_use]
    pub fn title(mut self, title: &str) -> Self {
        self.title = Some(maud::html! { (title) });
        self
    }

    /// Set the card title from pre-built [`maud::Markup`] for rich heading content
    /// (e.g. a title with an inline badge or count).
    ///
    /// Callers are responsible for escaping any user-supplied content inside `title`.
    #[must_use]
    pub fn title_html(mut self, title: maud::Markup) -> Self {
        self.title = Some(title);
        self
    }

    /// Set the heading level for the title element (default [`HeadingLevel::H2`]).
    #[must_use]
    pub const fn level(mut self, level: HeadingLevel) -> Self {
        self.level = level;
        self
    }

    /// Set the right-side header action slot (e.g. buttons/links).
    #[must_use]
    pub fn header_action(mut self, action: maud::Markup) -> Self {
        self.header_action = Some(action);
        self
    }

    /// Set the footer content rendered in `<div class="autumn-card__footer">`.
    #[must_use]
    pub fn footer(mut self, footer: maud::Markup) -> Self {
        self.footer = Some(footer);
        self
    }

    /// Add extra CSS class(es) to the root `autumn-card` element.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Render a composable card container.
///
/// Emits a `<div class="autumn-card">` with an optional header (title + action slot),
/// a `<div class="autumn-card__body">` wrapping `body`, and an optional
/// `<div class="autumn-card__footer">`.
///
/// The header is rendered only when a title or `header_action` is set.
/// The title is wrapped in a heading element (`<h2>` by default, configurable
/// via [`CardConfig::level`]) so screen readers can navigate card titles.
///
/// All class names (`autumn-card`, `autumn-card__header`, `autumn-card__title`,
/// `autumn-card__body`, `autumn-card__footer`) live in the framework's
/// `autumn-*` namespace and are backed by rules in the widget stylesheet
/// ([`crate::ui::WIDGETS_CSS`]), so the card renders styled out of the box.
/// These names replaced the unprefixed `card`, `card-header`, … classes in
/// 0.8.0 — see `changelog.d/2354-widget-class-namespace.md`.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-card` | Root wrapper |
/// | `.autumn-card__header` | Header row (title + action) |
/// | `.autumn-card__title` | Title heading element |
/// | `.autumn-card__body` | Body wrapper |
/// | `.autumn-card__footer` | Footer wrapper |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{
///     card, CardConfig, property_list, data_table, Column, DataTableConfig,
/// };
/// use maud::html;
///
/// struct Post { id: i64, title: String }
/// let posts = vec![Post { id: 1, title: "Hello".into() }];
///
/// let summary = property_list(&[
///     ("Total", html! { (posts.len()) }),
/// ]);
///
/// let cols: Vec<Column<Post>> = vec![
///     Column::new("ID",    |p: &Post| html! { (p.id) }),
///     Column::new("Title", |p: &Post| html! { (p.title.as_str()) }),
/// ];
/// let table = data_table(&posts, &cols, &DataTableConfig::new("No posts."));
///
/// let new_btn = html! { a class="btn" href="/posts/new" { "New post" } };
/// let config = CardConfig::new().title("Posts").header_action(new_btn);
///
/// let out = card(&html! { (summary) (table) }, &config).into_string();
/// assert!(out.contains(r#"class="autumn-card__header""#));
/// assert!(out.contains(r#"<h2 class="autumn-card__title">Posts</h2>"#));
/// assert!(out.contains(r#"class="autumn-card__body""#));
/// assert!(out.contains(r#"class="autumn-property-list""#));
/// assert!(out.contains("<table"));
/// ```
#[must_use]
pub fn card(body: &maud::Markup, config: &CardConfig<'_>) -> maud::Markup {
    let root_class = merge_class("autumn-card", config.class);
    let has_header = config.title.is_some() || config.header_action.is_some();
    maud::html! {
        div class=(root_class) {
            @if has_header {
                div class="autumn-card__header" {
                    @if let Some(title) = &config.title {
                        (heading(config.level, None, "autumn-card__title", title))
                    }
                    @if let Some(action) = &config.header_action {
                        (action)
                    }
                }
            }
            div class="autumn-card__body" { (body) }
            @if let Some(footer) = &config.footer {
                div class="autumn-card__footer" { (footer) }
            }
        }
    }
}

/// Render a metric stat-card tile: label, value, and an optional "view all" link.
///
/// Emits a `<div class="autumn-stat-card">` matching the pattern used by the admin
/// dashboard for model-count tiles, backed by the widget stylesheet
/// ([`crate::ui::WIDGETS_CSS`]). Both `label` and `value` are HTML-escaped.
///
/// `link` is `(href, link_text)`; omit with `None` to render without the link row.
///
/// All class names live in the framework's `autumn-*` namespace; the
/// unprefixed `stat-card`, `stat-label`, … names were renamed in 0.8.0 —
/// see `changelog.d/2354-widget-class-namespace.md`.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-stat-card` | Root tile |
/// | `.autumn-stat-card__label` | Metric label |
/// | `.autumn-stat-card__value` | Metric number/value |
/// | `.autumn-stat-card__link` | Link row (only present when `link` is `Some`) |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::stat_card;
///
/// let html = stat_card("Users", "1 024", Some(("/users", "View all →"))).into_string();
/// assert!(html.contains(r#"class="autumn-stat-card""#));
/// assert!(html.contains("1 024"));
/// assert!(html.contains(r#"href="/users""#));
/// ```
#[must_use]
pub fn stat_card(label: &str, value: &str, link: Option<(&str, &str)>) -> maud::Markup {
    maud::html! {
        div class="autumn-stat-card" {
            div class="autumn-stat-card__label" { (label) }
            div class="autumn-stat-card__value" { (value) }
            @if let Some((href, text)) = link {
                div class="autumn-stat-card__link" {
                    a href=(href) { (text) }
                }
            }
        }
    }
}

// ── hero ──────────────────────────────────────────────────────────────────

/// Primary/secondary style hint for a [`Cta`] link rendered by [`hero`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CtaStyle {
    /// The main call-to-action (default) — rendered with `.autumn-hero__cta--primary`.
    #[default]
    Primary,
    /// A secondary, lower-emphasis call-to-action — rendered with
    /// `.autumn-hero__cta--secondary`.
    Secondary,
}

/// A single call-to-action link rendered by [`hero`].
///
/// Build with [`Cta::primary`] or [`Cta::secondary`]. `label` and `href`
/// accept anything convertible to `String` (a `&str` literal or an owned
/// `String` built from request data, e.g. `format!("/posts/{id}")`) so
/// building a CTA from computed values never fights the borrow checker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cta {
    /// Visible link text. HTML-escaped by Maud.
    pub label: String,
    /// Link target URL.
    pub href: String,
    /// Visual style hint (default [`CtaStyle::Primary`]).
    pub style: CtaStyle,
}

impl Cta {
    /// Create a primary call-to-action link.
    #[must_use]
    pub fn primary(label: impl Into<String>, href: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            href: href.into(),
            style: CtaStyle::Primary,
        }
    }

    /// Create a secondary call-to-action link.
    #[must_use]
    pub fn secondary(label: impl Into<String>, href: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            href: href.into(),
            style: CtaStyle::Secondary,
        }
    }
}

/// Configuration for a [`hero`] widget.
///
/// Build with [`HeroConfig::new`] (title is required) and chain builder
/// methods for the optional subtitle and CTAs.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::widgets::{Cta, HeroConfig, hero};
///
/// let config = HeroConfig::new("Welcome to the Blog")
///     .subtitle("Thoughts, tutorials, and stories.")
///     .cta(Cta::primary("New post", "/admin/new"))
///     .cta(Cta::secondary("About", "/about"));
///
/// let markup = hero(&config);
/// ```
#[derive(Debug, Clone)]
pub struct HeroConfig {
    /// Headline text/markup, rendered in a single top-level heading element.
    title: maud::Markup,
    /// Heading level for the title element (default [`HeadingLevel::H1`]).
    level: HeadingLevel,
    /// Optional subtitle/lede rendered below the title.
    subtitle: Option<maud::Markup>,
    /// Zero or more call-to-action links rendered below the subtitle.
    ctas: Vec<Cta>,
    /// Extra CSS class(es) appended to the root `autumn-hero` element.
    class: Option<String>,
}

impl HeroConfig {
    /// Create a new hero configuration with the required `title`, heading
    /// level `H1`, and no subtitle or CTAs.
    #[must_use]
    pub fn new(title: &str) -> Self {
        Self {
            title: maud::html! { (title) },
            level: HeadingLevel::H1,
            subtitle: None,
            ctas: Vec::new(),
            class: None,
        }
    }

    /// Set the headline from pre-built [`maud::Markup`] for rich heading
    /// content (e.g. a title with an inline highlight span).
    ///
    /// Callers are responsible for escaping any user-supplied content inside `title`.
    #[must_use]
    pub fn title_html(mut self, title: maud::Markup) -> Self {
        self.title = title;
        self
    }

    /// Set the heading level for the title element (default [`HeadingLevel::H1`]).
    #[must_use]
    pub const fn level(mut self, level: HeadingLevel) -> Self {
        self.level = level;
        self
    }

    /// Set the subtitle/lede from a plain `&str`. The text is HTML-escaped by Maud.
    ///
    /// Use [`HeroConfig::subtitle_html`] when you need rich markup in the subtitle.
    #[must_use]
    pub fn subtitle(mut self, subtitle: &str) -> Self {
        self.subtitle = Some(maud::html! { (subtitle) });
        self
    }

    /// Set the subtitle/lede from pre-built [`maud::Markup`].
    ///
    /// Callers are responsible for escaping any user-supplied content inside `subtitle`.
    #[must_use]
    pub fn subtitle_html(mut self, subtitle: maud::Markup) -> Self {
        self.subtitle = Some(subtitle);
        self
    }

    /// Append a call-to-action link. May be called multiple times; an empty
    /// CTA list is valid and renders no button container.
    #[must_use]
    pub fn cta(mut self, cta: Cta) -> Self {
        self.ctas.push(cta);
        self
    }

    /// Add extra CSS class(es) to the root `autumn-hero` element.
    #[must_use]
    pub fn class(mut self, class: impl Into<String>) -> Self {
        self.class = Some(class.into());
        self
    }
}

/// Render a landing-page hero banner: headline, optional subtitle, and zero
/// or more call-to-action links.
///
/// Emits a `<section class="autumn-hero">` containing a single top-level
/// heading (`<h1>` by default, configurable via [`HeroConfig::level`]), an
/// optional `<p class="autumn-hero__subtitle">`, and — only when at least one
/// CTA is configured — a `<div class="autumn-hero__actions">` of `<a>` links.
/// A hero with zero CTAs renders no actions container at all.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-hero` | Root `<section>` wrapper |
/// | `.autumn-hero__title` | Headline heading element |
/// | `.autumn-hero__subtitle` | Subtitle/lede paragraph (only when set) |
/// | `.autumn-hero__actions` | CTA container (only when non-empty) |
/// | `.autumn-hero__cta` | Every CTA link |
/// | `.autumn-hero__cta--primary` / `.autumn-hero__cta--secondary` | Style hint per [`CtaStyle`] |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{Cta, HeroConfig, hero};
///
/// let config = HeroConfig::new("Welcome to the Blog")
///     .subtitle("Thoughts, tutorials, and stories.")
///     .cta(Cta::primary("New post", "/admin/new"));
///
/// let html = hero(&config).into_string();
/// assert!(html.contains(r#"class="autumn-hero""#));
/// assert!(html.contains("<h1"));
/// assert!(html.contains(r#"class="autumn-hero__subtitle""#));
/// assert!(html.contains(r#"class="autumn-hero__actions""#));
/// ```
#[must_use]
pub fn hero(config: &HeroConfig) -> maud::Markup {
    let root_class = merge_class("autumn-hero", config.class.as_deref());
    maud::html! {
        section class=(root_class) {
            (heading(config.level, None, "autumn-hero__title", &config.title))
            @if let Some(subtitle) = &config.subtitle {
                p class="autumn-hero__subtitle" { (subtitle) }
            }
            @if !config.ctas.is_empty() {
                div class="autumn-hero__actions" {
                    @for cta in &config.ctas {
                        @match cta.style {
                            CtaStyle::Primary => {
                                a href=(cta.href) class="autumn-hero__cta autumn-hero__cta--primary" { (cta.label) }
                            }
                            CtaStyle::Secondary => {
                                a href=(cta.href) class="autumn-hero__cta autumn-hero__cta--secondary" { (cta.label) }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ── modal / drawer ───────────────────────────────────────────────────────

/// Configuration for a [`modal`] dialog.
///
/// Build with [`ModalConfig::new`] and chain builder methods for optional slots.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{ModalConfig, modal};
/// use maud::html;
///
/// let body = html! { p { "Are you sure you want to continue?" } };
/// let footer = html! { button { "OK" } };
/// let config = ModalConfig::new().footer(footer);
/// let dialog = modal("confirm-dialog", "Confirm", &body, &config);
/// ```
#[derive(Debug, Clone, Default)]
pub struct ModalConfig {
    /// Optional footer content (e.g. action buttons) rendered in
    /// `<div class="autumn-modal__footer">`.
    footer: Option<maud::Markup>,
    /// Heading level for the title element (default [`HeadingLevel::H2`]).
    level: HeadingLevel,
    /// Extra CSS class(es) appended to the root `autumn-modal` element.
    class: Option<String>,
    /// When `true`, emits `closedby="any"` so clicking the backdrop or
    /// pressing Escape dismisses the dialog. Off by default so a
    /// destructive confirm isn't accidentally dismissed by a stray click.
    light_dismiss: bool,
    /// Size of the dialog (default [`DialogSize::Md`]).
    size: DialogSize,
    /// Accessible name of the built-in close button. `None` omits it.
    close_label: Option<String>,
}

impl ModalConfig {
    /// Create a new modal configuration with no footer, `<h2>` title, and
    /// light-dismiss disabled.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            footer: None,
            level: HeadingLevel::H2,
            class: None,
            light_dismiss: false,
            size: DialogSize::Md,
            close_label: None,
        }
    }

    /// Set the dialog size (default [`DialogSize::Md`]).
    #[must_use]
    pub const fn size(mut self, size: DialogSize) -> Self {
        self.size = size;
        self
    }

    /// Add a built-in × button. `label` is its accessible name, for example
    /// `"Close dialog"`. The button is off by default.
    #[must_use]
    pub fn close_button(mut self, label: impl Into<String>) -> Self {
        self.close_label = Some(label.into());
        self
    }

    /// Set the footer content (e.g. action buttons) rendered in
    /// `<div class="autumn-modal__footer">`.
    #[must_use]
    pub fn footer(mut self, footer: maud::Markup) -> Self {
        self.footer = Some(footer);
        self
    }

    /// Set the heading level for the title element (default [`HeadingLevel::H2`]).
    #[must_use]
    pub const fn level(mut self, level: HeadingLevel) -> Self {
        self.level = level;
        self
    }

    /// Add extra CSS class(es) to the root `autumn-modal` element.
    #[must_use]
    pub fn class(mut self, class: impl Into<String>) -> Self {
        self.class = Some(class.into());
        self
    }

    /// When `true`, emits `closedby="any"` on the `<dialog>` so the backdrop
    /// and Escape dismiss it (default `false`). Escape-to-close is universal
    /// `<dialog>` behavior; `closedby="any"` itself has limited browser
    /// support (unsupported in Firefox and Safari as of this writing), so
    /// `autumn-widgets.js` ships a backdrop-click polyfill scoped to
    /// `dialog[closedby="any"]` for browsers that don't honor the attribute
    /// natively yet.
    #[must_use]
    pub const fn light_dismiss(mut self, light_dismiss: bool) -> Self {
        self.light_dismiss = light_dismiss;
        self
    }
}

/// Render a native `<dialog>` modal: an `id`, a labelled title, `aria-modal`,
/// a body slot, and an optional footer/actions slot.
///
/// Opened via `showModal()` — either the native HTML Invoker Commands API
/// (`command="show-modal"` / `commandfor`, see [`modal_trigger`]) or the
/// framework-shipped fallback in `autumn-widgets.js` — the dialog gets ESC-close,
/// a focus trap, focus-into-dialog on open, and focus-return-to-trigger on
/// close for free from the browser: no app-authored JavaScript is required.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-modal` | Root `<dialog>` |
/// | `.autumn-modal__content` | Inner content wrapper |
/// | `.autumn-modal__title` | Title heading |
/// | `.autumn-modal__close` | × button (only with [`ModalConfig::close_button`]) |
/// | `.autumn-modal--sm`, `--lg`, `--full` | Size modifiers on the root |
/// | `.autumn-modal__body` | Body wrapper |
/// | `.autumn-modal__footer` | Footer wrapper (only present when configured) |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{ModalConfig, modal};
/// use maud::html;
///
/// let body = html! { p { "This action cannot be undone." } };
/// let html = modal("delete-confirm", "Delete this post?", &body, &ModalConfig::new())
///     .into_string();
/// assert!(html.contains("<dialog"));
/// assert!(html.contains(r#"aria-labelledby="delete-confirm-title""#));
/// assert!(html.contains(r#"aria-modal="true""#));
/// ```
#[must_use]
pub fn modal(id: &str, title: &str, body: &maud::Markup, config: &ModalConfig) -> maud::Markup {
    let size_class = config.size.modifier("autumn-modal");
    let root_class = merge_class(
        &merge_class("autumn-modal", size_class.as_deref()),
        config.class.as_deref(),
    );
    let title_id = format!("{id}-title");
    let closedby = config.light_dismiss.then_some("any");
    maud::html! {
        dialog id=(id) class=(root_class) role="dialog" aria-modal="true"
            aria-labelledby=(title_id) closedby=[closedby] {
            div class="autumn-modal__content" {
                @if let Some(label) = &config.close_label {
                    (dialog_close_x("autumn-modal__close", id, label))
                }
                (heading(config.level, Some(title_id.as_str()), "autumn-modal__title", &maud::html! { (title) }))
                div class="autumn-modal__body" { (body) }
                @if let Some(footer) = &config.footer {
                    div class="autumn-modal__footer" { (footer) }
                }
            }
        }
    }
}

/// Render a `<button>` that opens the `<dialog>` identified by `dialog_id`.
///
/// Uses the native HTML Invoker Commands API (`command="show-modal"` +
/// `commandfor`) with a `data-modal-open` fallback attribute wired by
/// `autumn-widgets.js` for browsers that don't yet support it — no
/// app-authored JavaScript is required either way.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::modal_trigger;
///
/// let html = modal_trigger("Delete", "delete-confirm", Some("btn btn-danger")).into_string();
/// assert!(html.contains(r#"commandfor="delete-confirm""#));
/// ```
#[must_use]
pub fn modal_trigger(label: &str, dialog_id: &str, class: Option<&str>) -> maud::Markup {
    maud::html! {
        button type="button" class=[class]
            command="show-modal" commandfor=(dialog_id) data-modal-open=(dialog_id) {
            (label)
        }
    }
}

/// Render a `<button>` that closes the `<dialog>` identified by `dialog_id`.
///
/// Uses the native HTML Invoker Commands API (`command="close"` +
/// `commandfor`) with a `data-modal-close` fallback attribute wired by
/// `autumn-widgets.js` for browsers that don't yet support it.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::modal_close_button;
///
/// let html = modal_close_button("Cancel", "delete-confirm", None).into_string();
/// assert!(html.contains(r#"command="close""#));
/// ```
#[must_use]
pub fn modal_close_button(label: &str, dialog_id: &str, class: Option<&str>) -> maud::Markup {
    maud::html! {
        button type="button" class=[class]
            command="close" commandfor=(dialog_id) data-modal-close=(dialog_id) {
            (label)
        }
    }
}

/// Size of a [`modal`] or [`drawer`].
///
/// `Md` is the default and adds no modifier class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DialogSize {
    /// Small.
    Sm,
    /// Medium (default).
    #[default]
    Md,
    /// Large.
    Lg,
    /// The whole viewport.
    Full,
}

impl DialogSize {
    /// The modifier class for `base`, or `None` for `Md`.
    fn modifier(self, base: &str) -> Option<String> {
        let suffix = match self {
            Self::Sm => "sm",
            Self::Md => return None,
            Self::Lg => "lg",
            Self::Full => "full",
        };
        Some(format!("{base}--{suffix}"))
    }
}

/// A × button that closes the dialog `dialog_id`. `label` is its accessible name.
fn dialog_close_x(class: &str, dialog_id: &str, label: &str) -> maud::Markup {
    maud::html! {
        button type="button" class=(class) aria-label=(label)
            command="close" commandfor=(dialog_id) data-modal-close=(dialog_id) {
            span aria-hidden="true" { "×" }
        }
    }
}

/// The edge a [`drawer`] slides from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DrawerSide {
    /// Left edge.
    Left,
    /// Right edge (default).
    #[default]
    Right,
    /// Top edge.
    Top,
    /// Bottom edge.
    Bottom,
}

impl DrawerSide {
    const fn class(self) -> &'static str {
        match self {
            Self::Left => "autumn-drawer--left",
            Self::Right => "autumn-drawer--right",
            Self::Top => "autumn-drawer--top",
            Self::Bottom => "autumn-drawer--bottom",
        }
    }
}

/// Configuration for a [`drawer`].
///
/// Build with [`DrawerConfig::new`] and chain builder methods.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{DialogSize, DrawerConfig, DrawerSide, drawer};
/// use maud::html;
///
/// let config = DrawerConfig::new()
///     .side(DrawerSide::Left)
///     .size(DialogSize::Lg)
///     .close_button("Close filters");
/// let html = drawer("filters", "Filters", &html! { p { "…" } }, &config);
/// ```
#[derive(Debug, Clone)]
pub struct DrawerConfig {
    footer: Option<maud::Markup>,
    level: HeadingLevel,
    class: Option<String>,
    side: DrawerSide,
    size: DialogSize,
    modal: bool,
    light_dismiss: bool,
    open: bool,
    close_label: Option<String>,
}

impl Default for DrawerConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl DrawerConfig {
    /// A modal drawer on the right edge, medium size, with light dismiss on
    /// and no close button.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            footer: None,
            level: HeadingLevel::H2,
            class: None,
            side: DrawerSide::Right,
            size: DialogSize::Md,
            modal: true,
            light_dismiss: true,
            open: false,
            close_label: None,
        }
    }

    /// Set the footer content, rendered in `<div class="autumn-drawer__footer">`.
    #[must_use]
    pub fn footer(mut self, footer: maud::Markup) -> Self {
        self.footer = Some(footer);
        self
    }

    /// Set the title heading level (default [`HeadingLevel::H2`]).
    #[must_use]
    pub const fn level(mut self, level: HeadingLevel) -> Self {
        self.level = level;
        self
    }

    /// Add extra CSS class(es) to the root element.
    #[must_use]
    pub fn class(mut self, class: impl Into<String>) -> Self {
        self.class = Some(class.into());
        self
    }

    /// Set the edge (default [`DrawerSide::Right`]).
    #[must_use]
    pub const fn side(mut self, side: DrawerSide) -> Self {
        self.side = side;
        self
    }

    /// Set the size (default [`DialogSize::Md`]).
    #[must_use]
    pub const fn size(mut self, size: DialogSize) -> Self {
        self.size = size;
        self
    }

    /// `true` (default): opens with `showModal()`. The browser traps focus
    /// and makes the page inert.
    ///
    /// `false`: opens with `show()`. The page stays usable. Open it with
    /// [`drawer_trigger`] (needs `autumn-widgets.js`) or [`DrawerConfig::open`].
    /// Pass the same config to [`drawer`] and [`drawer_trigger`].
    #[must_use]
    pub const fn modal(mut self, modal: bool) -> Self {
        self.modal = modal;
        self
    }

    /// `true` (default): Escape closes the drawer. A modal drawer also closes
    /// on a click on the backdrop (`closedby="any"`; see
    /// [`ModalConfig::light_dismiss`]). A non-modal drawer ignores page clicks
    /// (`closedby="closerequest"`).
    #[must_use]
    pub const fn light_dismiss(mut self, light_dismiss: bool) -> Self {
        self.light_dismiss = light_dismiss;
        self
    }

    /// Render a non-modal drawer already open. A modal drawer ignores this:
    /// a modal dialog opens only through `showModal()`.
    #[must_use]
    pub const fn open(mut self, open: bool) -> Self {
        self.open = open;
        self
    }

    /// Add a built-in × button. `label` is its accessible name.
    #[must_use]
    pub fn close_button(mut self, label: impl Into<String>) -> Self {
        self.close_label = Some(label.into());
        self
    }
}

/// Render a native `<dialog>` drawer that slides in from an edge.
///
/// It reuses the [`modal`] machinery: invoker commands, `closedby` light
/// dismiss and the `autumn-widgets.js` fallbacks. Open a modal drawer with
/// [`modal_trigger`] or [`drawer_trigger`]. It needs no app JavaScript.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-drawer` | Root `<dialog>` |
/// | `.autumn-drawer--left`, `--right`, `--top`, `--bottom` | Edge |
/// | `.autumn-drawer--sm`, `--lg`, `--full` | Size |
/// | `.autumn-drawer__content` | Content wrapper |
/// | `.autumn-drawer__title` | Title heading |
/// | `.autumn-drawer__close` | × button (only with [`DrawerConfig::close_button`]) |
/// | `.autumn-drawer__body` | Body wrapper |
/// | `.autumn-drawer__footer` | Footer (only when set) |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{DrawerConfig, drawer, drawer_trigger};
/// use maud::html;
///
/// let config = DrawerConfig::new();
/// let trigger = drawer_trigger("Filters", "filters", &config, None);
/// let html = drawer("filters", "Filters", &html! { p { "…" } }, &config).into_string();
/// assert!(html.contains("autumn-drawer--right"));
/// assert!(trigger.into_string().contains(r#"commandfor="filters""#));
/// ```
#[must_use]
pub fn drawer(id: &str, title: &str, body: &maud::Markup, config: &DrawerConfig) -> maud::Markup {
    let size_class = config.size.modifier("autumn-drawer");
    let classes = [
        Some("autumn-drawer"),
        Some(config.side.class()),
        size_class.as_deref(),
        config.class.as_deref().filter(|c| !c.is_empty()),
    ];
    let root_class = classes.into_iter().flatten().collect::<Vec<_>>().join(" ");
    let title_id = format!("{id}-title");
    let closedby = match (config.light_dismiss, config.modal) {
        (false, _) => None,
        (true, true) => Some("any"),
        (true, false) => Some("closerequest"),
    };
    let aria_modal = config.modal.then_some("true");
    let open = config.open && !config.modal;
    maud::html! {
        dialog id=(id) class=(root_class) role="dialog" aria-modal=[aria_modal]
            aria-labelledby=(title_id) closedby=[closedby] open[open] {
            div class="autumn-drawer__content" {
                @if let Some(label) = &config.close_label {
                    (dialog_close_x("autumn-drawer__close", id, label))
                }
                (heading(config.level, Some(title_id.as_str()), "autumn-drawer__title", &maud::html! { (title) }))
                div class="autumn-drawer__body" { (body) }
                @if let Some(footer) = &config.footer {
                    div class="autumn-drawer__footer" { (footer) }
                }
            }
        }
    }
}

/// Render a `<button>` that opens the [`drawer`] `dialog_id`.
///
/// A modal drawer gets the same invoker command as [`modal_trigger`]. A
/// non-modal drawer gets `data-drawer-open`, which `autumn-widgets.js` wires
/// to `show()`: the invoker commands have no non-modal `show`.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{DrawerConfig, drawer_trigger};
///
/// let html = drawer_trigger("Menu", "menu", &DrawerConfig::new().modal(false), None)
///     .into_string();
/// assert!(html.contains(r#"data-drawer-open="menu""#));
/// ```
#[must_use]
pub fn drawer_trigger(
    label: &str,
    dialog_id: &str,
    config: &DrawerConfig,
    class: Option<&str>,
) -> maud::Markup {
    if config.modal {
        return modal_trigger(label, dialog_id, class);
    }
    maud::html! {
        button type="button" class=[class] aria-controls=(dialog_id) data-drawer-open=(dialog_id) {
            (label)
        }
    }
}

// ── tabs ─────────────────────────────────────────────────────────────────

/// Render a no-JavaScript tabs widget: a `tablist` strip plus its panels,
/// from an ordered list of `(id, label, panel_body)` tuples plus the id of
/// the tab that should be active by default.
///
/// `active_id` selects the default-active tab/panel: `Some(panel_id)` that
/// matches one of the panels makes that panel default-active; `None` (or an
/// id that matches no panel) falls back to the first panel.
///
/// Without JS, switching is pure CSS: each tab is an `<a href="#panel-id">` and each
/// panel is targeted by `:target` in the widget stylesheet, so URL fragments
/// (`/settings#security`) select a tab with zero JavaScript. The
/// default-active panel additionally carries `autumn-tabs__panel--active` so
/// a panel is always visible even when no fragment is present, and the
/// active-tab highlight tracks the actually-`:target`ed panel by position for
/// up to the first 6 tabs (see the widget stylesheet — ids are arbitrary caller
/// strings, so this can only be done positionally in a shared stylesheet).
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-tabs` | Root wrapper |
/// | `.autumn-tabs__list` | Tab strip (`role="tablist"`) |
/// | `.autumn-tabs__tab` | Individual tab link |
/// | `.autumn-tabs__tab--active` | The initially-selected tab |
/// | `.autumn-tabs__panel` | Individual panel |
/// | `.autumn-tabs__panel--active` | The initially-visible panel |
///
/// # Accessibility
///
/// The tab strip carries `role="tablist"`; each tab carries `role="tab"`,
/// `aria-controls` pointing at its panel's `id`, and `aria-selected`; each
/// panel carries `role="tabpanel"`, `aria-labelledby` pointing at its tab's
/// `id`, and `tabindex="0"` so it can receive keyboard focus directly.
///
/// # Keyboard (with `autumn-widgets.js`)
///
/// The tab strip binds the keyboard core (`data-autumn-roving`,
/// `docs/guide/keyboard.md`). The runtime gives the APG tabs pattern with
/// automatic activation: `ArrowLeft`/`ArrowRight` (wrapping), `Home`/`End`,
/// and `Tab` to the panel. `aria-selected`, the roving `tabindex` and the shown panel follow
/// the selected tab, also for a deep link. The URL fragment follows too.
///
/// With JS off, `aria-selected` stays on the server's default tab, because
/// the server never sees the URL fragment.
///
/// # Panel `id`s
///
/// A panel's `id` is the raw id from its tuple (not namespaced by `id`) so
/// it can be targeted directly by a URL fragment. Each tab's own element
/// `id` is `"{id}-tab-{panel_id}"`. Duplicate panel ids within one call are
/// rendered as given — the caller is responsible for uniqueness if
/// fragment targeting of a specific panel matters.
///
/// **Rendering more than one `tabs()` widget on the same page:** panel ids
/// must be unique across the *entire page*, not just within one call —
/// two different `tabs()` calls that both use a panel id like `"overview"`
/// will emit two elements with `id="overview"`, which is invalid duplicate-id
/// HTML and makes `:target`/`aria-controls`/`aria-labelledby` lookups
/// ambiguous. Prefix panel ids per widget instance (e.g. `"post-overview"`,
/// `"related-overview"`) if more than one `tabs()` appears on a page.
///
/// # Nesting a `tabs()` widget inside another `tabs()` panel
///
/// This is supported for panel *visibility*: the widget stylesheet reveals the whole
/// ancestor chain down to a deep-linked inner panel, so content nested
/// inside any outer panel (not just the outer widget's default one) is
/// reachable via its own fragment. The outer widget's *active-tab visual
/// highlight*, however, can only be synced when the shown outer panel is
/// itself the direct `:target` — CSS forbids nesting `:has()` inside
/// another `:has()`, so a panel that's shown only because it *contains* a
/// nested target (rather than being the target itself) can't be
/// attributed to a specific outer tab position. In that case no outer tab
/// is highlighted as active (a graceful degrade — no tab looks active,
/// rather than the wrong one looking active).
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::tabs;
/// use maud::html;
///
/// let panels = [
///     ("profile", "Profile", html! { p { "Profile settings" } }),
///     ("security", "Security", html! { p { "Security settings" } }),
///     ("billing", "Billing", html! { p { "Billing settings" } }),
/// ];
/// // Open on the "security" tab by default; pass `None` to default to the first.
/// let widget = tabs("settings-tabs", Some("security"), &panels).into_string();
/// assert!(widget.contains(r#"role="tablist""#));
/// assert!(widget.contains(r#"aria-controls="security""#));
/// assert!(widget.contains(r##"href="#billing""##));
/// ```
#[must_use]
pub fn tabs(
    id: &str,
    active_id: Option<&str>,
    panels: &[(&str, &str, maud::Markup)],
) -> maud::Markup {
    let tab_ids: Vec<String> = panels
        .iter()
        .map(|(panel_id, _, _)| format!("{id}-tab-{panel_id}"))
        .collect();
    // Which panel is default-active. `Some(id)` that matches a panel selects
    // it; `None` or an unknown id falls back to the first panel (the historic
    // behavior). The chosen index only sets the server's *default* selection —
    // `:target` deep-linking still overrides it in pure CSS.
    let active_index = active_id
        .and_then(|target| {
            panels
                .iter()
                .position(|(panel_id, _, _)| *panel_id == target)
        })
        .unwrap_or(0);
    maud::html! {
        div id=(id) class="autumn-tabs" data-autumn-tabs {
            div class="autumn-tabs__list" role="tablist"
                data-autumn-roving data-autumn-orientation="horizontal" {
                @for (i, (panel_id, label, _)) in panels.iter().enumerate() {
                    @let tab_class = merge_class(
                        "autumn-tabs__tab",
                        (i == active_index).then_some("autumn-tabs__tab--active"),
                    );
                    a id=(tab_ids[i]) class=(tab_class) role="tab" href=(format!("#{panel_id}"))
                        aria-controls=(panel_id) aria-selected=(if i == active_index { "true" } else { "false" }) {
                        (label)
                    }
                }
            }
            @for (i, (panel_id, _, body)) in panels.iter().enumerate() {
                @let panel_class = merge_class(
                    "autumn-tabs__panel",
                    (i == active_index).then_some("autumn-tabs__panel--active"),
                );
                section id=(panel_id) class=(panel_class) role="tabpanel"
                    aria-labelledby=(tab_ids[i]) tabindex="0" {
                    (body)
                }
            }
        }
    }
}

// ── accordion / collapsible (#3401) ─────────────────────────────────────────

/// Settings for an [`accordion`].
///
/// `name` is the root `id` and, in exclusive mode, the shared `name` of the
/// items. Use a different name for each accordion on a page. A
/// [`CollapsibleConfig::attrs`] `name` must differ from it too: items with one
/// `name` close each other.
///
/// ```rust
/// use autumn_web::widgets::{AccordionConfig, HeadingLevel};
///
/// let config = AccordionConfig::new("faq").multiple().open(0).level(HeadingLevel::H2);
/// ```
#[derive(Debug, Clone)]
pub struct AccordionConfig<'a> {
    name: &'a str,
    multiple: bool,
    open: Vec<usize>,
    level: HeadingLevel,
    class: Option<&'a str>,
    attrs: &'a [(&'a str, &'a str)],
}

impl<'a> AccordionConfig<'a> {
    /// An exclusive accordion called `name`, with every item closed and
    /// `<h3>` titles. `name` must not be empty or hold spaces. Set
    /// [`AccordionConfig::level`] one level below the heading that holds
    /// the accordion.
    #[must_use]
    pub const fn new(name: &'a str) -> Self {
        Self {
            name,
            multiple: false,
            open: Vec::new(),
            level: HeadingLevel::H3,
            class: None,
            attrs: &[],
        }
    }

    /// Let many items be open at once. The default is exclusive: one open item.
    #[must_use]
    pub const fn multiple(mut self) -> Self {
        self.multiple = true;
        self
    }

    /// Open the item at `index` at render. Call it again for more items. An
    /// exclusive accordion opens only the first open item in page order. An
    /// index out of range does nothing.
    #[must_use]
    pub fn open(mut self, index: usize) -> Self {
        self.open.push(index);
        self
    }

    /// Set the heading level of each title (default [`HeadingLevel::H3`]).
    #[must_use]
    pub const fn level(mut self, level: HeadingLevel) -> Self {
        self.level = level;
        self
    }

    /// Add class(es) to the root element.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }

    /// Add attributes to the root element. `on*` and `hx-on*` handlers and
    /// `style` are dropped. The widget sets `id` and `class` first, so a
    /// second `id` or `class` is dropped.
    #[must_use]
    pub const fn attrs(mut self, attrs: &'a [(&'a str, &'a str)]) -> Self {
        self.attrs = attrs;
        self
    }
}

/// Render a no-JS accordion of native `<details>` items.
///
/// Each `(title, body)` is one item. The title is escaped. It sits in a
/// heading inside `<summary>`. Enter and Space toggle it natively. Give each
/// item a non-empty title: an empty title leaves an unnamed toggle.
///
/// An exclusive accordion (the default) gives each `<details>` the same
/// `name`, so the browser keeps one open. [`AccordionConfig::multiple`]
/// omits the `name`.
///
/// ```rust
/// use autumn_web::widgets::{AccordionConfig, accordion};
/// use maud::html;
///
/// let items = [("Shipping", html! { p { "2 days." } })];
/// let html = accordion(&items, &AccordionConfig::new("faq").open(0)).into_string();
/// assert!(html.contains(r#"name="faq" open"#));
/// ```
#[must_use]
pub fn accordion(items: &[(&str, maud::Markup)], config: &AccordionConfig<'_>) -> maud::Markup {
    // An exclusive accordion opens one item: the first in page order.
    let exclusive_open = config
        .open
        .iter()
        .copied()
        .filter(|&i| i < items.len())
        .min();
    let is_open = |i: usize| {
        if config.multiple {
            config.open.contains(&i)
        } else {
            exclusive_open == Some(i)
        }
    };
    let group = (!config.multiple).then_some(config.name);
    let root_class = merge_class("autumn-accordion", config.class);
    let mut attrs: Vec<(&str, &str)> = vec![("id", config.name), ("class", &root_class)];
    attrs.extend_from_slice(config.attrs);
    let body = maud::html! {
        @for (i, (title, body)) in items.iter().enumerate() {
            details class="autumn-accordion__item" name=[group] open[is_open(i)] {
                summary class="autumn-accordion__summary" {
                    (heading(
                        config.level,
                        None,
                        "autumn-accordion__heading",
                        &maud::html! { (title) },
                    ))
                }
                div class="autumn-accordion__body" { (body) }
            }
        }
    };
    crate::form::widget_element("div", &attrs, &crate::form::FieldOptions::new(), body)
}

/// Settings for a [`collapsible_with`].
///
/// ```rust
/// use autumn_web::widgets::CollapsibleConfig;
///
/// let config = CollapsibleConfig::new().open(true).class("card");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct CollapsibleConfig<'a> {
    open: bool,
    class: Option<&'a str>,
    attrs: &'a [(&'a str, &'a str)],
}

impl Default for CollapsibleConfig<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> CollapsibleConfig<'a> {
    /// A closed collapsible with no extra class or attributes.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            open: false,
            class: None,
            attrs: &[],
        }
    }

    /// Render it open (default `false`).
    #[must_use]
    pub const fn open(mut self, open: bool) -> Self {
        self.open = open;
        self
    }

    /// Add class(es) to the `<details>` element.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }

    /// Add attributes to the `<details>` element, for example a `name` to
    /// group collapsibles. `on*` and `hx-on*` handlers, `style`, `class` and
    /// `open` are dropped: use [`CollapsibleConfig::open`] for the open state.
    #[must_use]
    pub const fn attrs(mut self, attrs: &'a [(&'a str, &'a str)]) -> Self {
        self.attrs = attrs;
        self
    }
}

/// Render one closed disclosure: a native `<details>` with `summary` as its
/// toggle. The summary is escaped.
///
/// ```rust
/// use autumn_web::widgets::collapsible;
/// use maud::html;
///
/// let html = collapsible("More", &html! { p { "Body" } }).into_string();
/// assert!(html.starts_with(r#"<details class="autumn-collapsible">"#));
/// ```
#[must_use]
pub fn collapsible(summary: &str, body: &maud::Markup) -> maud::Markup {
    collapsible_with(summary, body, &CollapsibleConfig::new())
}

/// Like [`collapsible`], with [`CollapsibleConfig`] settings.
#[must_use]
pub fn collapsible_with(
    summary: &str,
    body: &maud::Markup,
    config: &CollapsibleConfig<'_>,
) -> maud::Markup {
    let root_class = merge_class("autumn-collapsible", config.class);
    let mut attrs: Vec<(&str, &str)> = vec![("class", &root_class)];
    if config.open {
        attrs.push(("open", ""));
    }
    // `open` belongs to the config, so `open(false)` always means closed.
    attrs.extend(config.attrs.iter().filter(|(name, _)| *name != "open"));
    let inner = maud::html! {
        summary class="autumn-collapsible__summary" { (summary) }
        div class="autumn-collapsible__body" { (body) }
    };
    crate::form::widget_element("details", &attrs, &crate::form::FieldOptions::new(), inner)
}

// ── button / button_group (#3406) ───────────────────────────────────────────

/// The look of a [`button`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonVariant {
    /// The main action. A filled button. The default.
    #[default]
    Primary,
    /// A second action. An outlined button.
    Secondary,
    /// A quiet action. It has no fill and no border.
    Ghost,
    /// An action that deletes data or that you cannot undo. A red button.
    Destructive,
    /// A button that looks like a link.
    Link,
}

impl ButtonVariant {
    /// The variant class, for example `autumn-button--primary`.
    const fn class(self) -> &'static str {
        match self {
            Self::Primary => "autumn-button--primary",
            Self::Secondary => "autumn-button--secondary",
            Self::Ghost => "autumn-button--ghost",
            Self::Destructive => "autumn-button--destructive",
            Self::Link => "autumn-button--link",
        }
    }
}

/// The size of a [`button`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonSize {
    /// A small button, for dense rows and tables.
    Small,
    /// The default size.
    #[default]
    Medium,
    /// A large button, for a main call to action.
    Large,
}

impl ButtonSize {
    /// The size class, for example `autumn-button--md`.
    const fn class(self) -> &'static str {
        match self {
            Self::Small => "autumn-button--sm",
            Self::Medium => "autumn-button--md",
            Self::Large => "autumn-button--lg",
        }
    }
}

/// When a [`button`] is busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ButtonBusy {
    /// Never.
    #[default]
    No,
    /// Now: the server renders `aria-busy="true"`.
    Now,
    /// During an htmx request: `data-autumn-busy`.
    OnRequest,
}

/// Options for [`button`]. The default is a medium primary
/// `type="button"`.
#[derive(Debug, Clone, Default)]
pub struct ButtonConfig<'a> {
    variant: ButtonVariant,
    size: ButtonSize,
    kind: crate::a11y::ButtonType,
    icon: Option<maud::Markup>,
    icon_only: bool,
    busy: ButtonBusy,
    disabled: bool,
    id: Option<&'a str>,
    class: Option<&'a str>,
    attrs: &'a [(&'a str, &'a str)],
}

impl<'a> ButtonConfig<'a> {
    /// A medium [`ButtonVariant::Primary`] button with `type="button"`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            variant: ButtonVariant::Primary,
            size: ButtonSize::Medium,
            kind: crate::a11y::ButtonType::Button,
            icon: None,
            icon_only: false,
            busy: ButtonBusy::No,
            disabled: false,
            id: None,
            class: None,
            attrs: &[],
        }
    }

    /// Set the look.
    #[must_use]
    pub const fn variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }

    /// Set the size.
    #[must_use]
    pub const fn size(mut self, size: ButtonSize) -> Self {
        self.size = size;
        self
    }

    /// Set the `type`. The default is `type="button"`.
    #[must_use]
    pub const fn kind(mut self, kind: crate::a11y::ButtonType) -> Self {
        self.kind = kind;
        self
    }

    /// Set `type="submit"`.
    #[must_use]
    pub const fn submit(mut self) -> Self {
        self.kind = crate::a11y::ButtonType::Submit;
        self
    }

    /// Show `icon` before the label. The icon is `aria-hidden`.
    #[must_use]
    pub fn icon(mut self, icon: maud::Markup) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Show only the icon. The label stays in the button as visually
    /// hidden text, so it is still the accessible name.
    #[must_use]
    pub const fn icon_only(mut self) -> Self {
        self.icon_only = true;
        self
    }

    /// Render the button busy: `aria-busy="true"`, `disabled` and a
    /// spinner. Use it when the server knows that work is in progress.
    /// It replaces [`loading_on_request`](Self::loading_on_request).
    #[must_use]
    pub const fn loading(mut self) -> Self {
        self.busy = ButtonBusy::Now;
        self
    }

    /// Show the busy state during an htmx request. The button gets
    /// `data-autumn-busy` and a hidden spinner.
    ///
    /// The button is busy when it sends the request, when it submits the
    /// form that sends it, or when `hx-indicator` names it. CSS shows the
    /// spinner while htmx gives the button the `htmx-request` class.
    /// `autumn-widgets.js` sets `aria-busy="true"` on the button and on
    /// the swap target. On a form submitter, this `aria-busy` shows the
    /// spinner.
    /// It replaces [`loading`](Self::loading).
    #[must_use]
    pub const fn loading_on_request(mut self) -> Self {
        self.busy = ButtonBusy::OnRequest;
        self
    }

    /// Add `disabled`.
    #[must_use]
    pub const fn disabled(mut self) -> Self {
        self.disabled = true;
        self
    }

    /// Set the `id`, for example for `hx-indicator="#save"`.
    #[must_use]
    pub const fn id(mut self, id: &'a str) -> Self {
        self.id = Some(id);
        self
    }

    /// Add a class to the `<button>`.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }

    /// Add attributes to the `<button>`, for example `hx-post`. Values are
    /// escaped. They pass through as given, `hx-on*` and `style` too: the
    /// app owns its CSP. A malformed name is dropped. The widget sets
    /// `type`, `class`, `id`, `disabled`, `aria-busy` and
    /// `data-autumn-busy` first, so a second copy of one of them, or of an
    /// earlier attribute, is dropped.
    #[must_use]
    pub const fn attrs(mut self, attrs: &'a [(&'a str, &'a str)]) -> Self {
        self.attrs = attrs;
        self
    }
}

/// Render a `<button>` with a variant, a size, an optional icon and a
/// loading state.
///
/// `label` is the visible text and the accessible name. Do not give an
/// empty label: a debug build panics.
///
/// The `autumn-button` classes also style an `<a href>`. Use them with
/// [`crate::links::link_to_with`] or [`crate::links::button_to_with`].
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-button` | The `<button>` |
/// | `.autumn-button--<variant>` | `primary`, `secondary`, `ghost`, `destructive` or `link` |
/// | `.autumn-button--<size>` | `sm`, `md` or `lg` |
/// | `.autumn-button--icon` | An icon-only button |
/// | `.autumn-button__icon` | The icon |
/// | `.autumn-button__label` | The label |
/// | `.autumn-button__spinner` | The busy spinner |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{ButtonConfig, ButtonVariant, button};
///
/// let config = ButtonConfig::new()
///     .variant(ButtonVariant::Destructive)
///     .loading_on_request()
///     .attrs(&[("hx-delete", "/posts/7")]);
/// let html = button("Delete", &config).into_string();
/// assert!(html.contains("autumn-button--destructive"));
/// assert!(html.contains("data-autumn-busy"));
/// ```
#[must_use]
#[track_caller]
pub fn button(label: &str, config: &ButtonConfig<'_>) -> maud::Markup {
    assert_named("button", label);
    debug_assert!(
        !config.icon_only || config.icon.is_some(),
        "button: icon_only() needs an icon(), or the button shows nothing"
    );
    let mut root = format!(
        "autumn-button {} {}",
        config.variant.class(),
        config.size.class()
    );
    if config.icon_only {
        root.push_str(" autumn-button--icon");
    }
    let root = merge_class(&root, config.class);
    let mut attrs: Vec<(&str, &str)> = vec![("type", config.kind.as_str()), ("class", &root)];
    if let Some(id) = config.id {
        attrs.push(("id", id));
    }
    match config.busy {
        ButtonBusy::Now => attrs.extend([("disabled", ""), ("aria-busy", "true")]),
        ButtonBusy::OnRequest => attrs.push(("data-autumn-busy", "")),
        ButtonBusy::No => {}
    }
    if config.disabled && config.busy != ButtonBusy::Now {
        attrs.push(("disabled", ""));
    }
    let label_class = if config.icon_only {
        "autumn-button__label autumn-visually-hidden"
    } else {
        "autumn-button__label"
    };
    // The spinner is first. CSS hides the elements after it while busy.
    let body = maud::html! {
        @if config.busy != ButtonBusy::No {
            span class="autumn-button__spinner autumn-spinner__icon" aria-hidden="true" {}
        }
        @if let Some(icon) = &config.icon {
            span class="autumn-button__icon" aria-hidden="true" { (icon) }
        }
        span class=(label_class) { (label) }
    };
    // The app owns its CSP: its attributes pass through, `on*` and `style`
    // too. Only a malformed name, or a name the widget sets, is dropped.
    let mut out = String::from("<button");
    for (name, value) in &attrs {
        crate::form::push_attr(&mut out, name, value);
    }
    for &(name, value) in config.attrs {
        let well_formed = name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'));
        if well_formed && !attrs.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
            crate::form::push_attr(&mut out, name, value);
            attrs.push((name, value));
        }
    }
    out.push('>');
    out.push_str(&body.into_string());
    out.push_str("</button>");
    maud::PreEscaped(out)
}

/// Options for [`button_group_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ButtonGroupConfig<'a> {
    vertical: bool,
    class: Option<&'a str>,
}

impl<'a> ButtonGroupConfig<'a> {
    /// A horizontal group with no extra class.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            vertical: false,
            class: None,
        }
    }

    /// Put the buttons in a column.
    #[must_use]
    pub const fn vertical(mut self) -> Self {
        self.vertical = true;
        self
    }

    /// Add a class to the group.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Render related buttons as one group: `role="group"` with
/// `aria-label`.
///
/// `label` names the group, for example "Text alignment". The buttons
/// touch, and the outer corners are round.
///
/// ```rust
/// use autumn_web::widgets::{ButtonConfig, ButtonVariant, button, button_group};
///
/// let secondary = ButtonConfig::new().variant(ButtonVariant::Secondary);
/// let html = button_group(
///     "Text alignment",
///     &[button("Left", &secondary), button("Right", &secondary)],
/// )
/// .into_string();
/// assert!(html.contains(r#"role="group""#));
/// assert!(html.contains(r#"aria-label="Text alignment""#));
/// ```
#[must_use]
#[track_caller]
pub fn button_group(label: &str, buttons: &[maud::Markup]) -> maud::Markup {
    button_group_with(label, buttons, &ButtonGroupConfig::new())
}

/// Like [`button_group`], with a [`ButtonGroupConfig`].
#[must_use]
#[track_caller]
pub fn button_group_with(
    label: &str,
    buttons: &[maud::Markup],
    config: &ButtonGroupConfig<'_>,
) -> maud::Markup {
    assert_named("button_group", label);
    let base = if config.vertical {
        "autumn-button-group autumn-button-group--vertical"
    } else {
        "autumn-button-group"
    };
    let root = merge_class(base, config.class);
    maud::html! {
        div class=(root) role="group" aria-label=(label) {
            @for button in buttons { (button) }
        }
    }
}

// ── separator (#3406) ───────────────────────────────────────────────────────

/// Options for [`separator_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SeparatorConfig<'a> {
    vertical: bool,
    decorative: bool,
    class: Option<&'a str>,
}

impl<'a> SeparatorConfig<'a> {
    /// A horizontal `<hr>`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            vertical: false,
            decorative: false,
            class: None,
        }
    }

    /// A vertical line, for a row of items: a `<span>` with
    /// `role="separator"` and `aria-orientation="vertical"`. A `<span>`
    /// can go in a paragraph.
    #[must_use]
    pub const fn vertical(mut self) -> Self {
        self.vertical = true;
        self
    }

    /// A line for looks only: `role="none"`. Screen readers do not read it.
    #[must_use]
    pub const fn decorative(mut self) -> Self {
        self.decorative = true;
        self
    }

    /// Add a class.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Render a horizontal separator: `<hr class="autumn-separator">`.
///
/// ```rust
/// use autumn_web::widgets::separator;
///
/// assert_eq!(separator().into_string(), r#"<hr class="autumn-separator">"#);
/// ```
#[must_use]
pub fn separator() -> maud::Markup {
    separator_with(&SeparatorConfig::new())
}

/// Like [`separator`], with a [`SeparatorConfig`]: vertical, decorative
/// and class.
///
/// ```rust
/// use autumn_web::widgets::{SeparatorConfig, separator_with};
///
/// let html = separator_with(&SeparatorConfig::new().vertical()).into_string();
/// assert!(html.contains(r#"role="separator""#));
/// assert!(html.contains(r#"aria-orientation="vertical""#));
/// ```
#[must_use]
pub fn separator_with(config: &SeparatorConfig<'_>) -> maud::Markup {
    let decorative = config.decorative.then_some("none");
    if config.vertical {
        let root = merge_class("autumn-separator autumn-separator--vertical", config.class);
        maud::html! {
            span class=(root) role=(decorative.unwrap_or("separator"))
                aria-orientation=[(!config.decorative).then_some("vertical")] {}
        }
    } else {
        let root = merge_class("autumn-separator", config.class);
        maud::html! { hr class=(root) role=[decorative]; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maud::html;

    // ── selector_to_id ─────────────────────────────────────────────────

    #[test]
    fn selector_to_id_strips_hash() {
        assert_eq!(selector_to_id("#my-results"), "my-results");
    }

    #[test]
    fn selector_to_id_passthrough_without_hash() {
        assert_eq!(selector_to_id("results"), "results");
    }

    // ── CardConfig builder ─────────────────────────────────────────────

    #[test]
    fn card_config_defaults() {
        // no title/action → no autumn-card__header rendered
        let html = card(&maud::html! {}, &CardConfig::new()).into_string();
        assert!(!html.contains("autumn-card__header"), "{html}");
        assert!(!html.contains("autumn-card__footer"), "{html}");
        // default heading level is H2: setting a title renders <h2
        let html2 = card(&maud::html! {}, &CardConfig::new().title("X")).into_string();
        assert!(html2.contains("<h2"), "{html2}");
    }

    #[test]
    fn card_config_builders_chain() {
        let html = card(
            &maud::html! {},
            &CardConfig::new()
                .title("T")
                .level(HeadingLevel::H3)
                .class("wide"),
        )
        .into_string();
        assert!(html.contains(r#"class="autumn-card wide""#), "{html}");
        assert!(html.contains("<h3"), "{html}");
        assert!(html.contains('T'), "{html}");
    }

    // ── card structure ─────────────────────────────────────────────────

    #[test]
    fn card_has_root_and_body_classes() {
        let body = maud::html! { p { "hello" } };
        let html = card(&body, &CardConfig::new()).into_string();
        assert!(html.contains(r#"class="autumn-card""#), "{html}");
        assert!(html.contains(r#"class="autumn-card__body""#), "{html}");
        assert!(html.contains("hello"), "{html}");
        assert!(!html.contains("autumn-card__header"), "{html}");
        assert!(!html.contains("autumn-card__footer"), "{html}");
    }

    #[test]
    fn card_renders_title_as_h2_by_default() {
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new().title("Posts")).into_string();
        assert!(
            html.contains(r#"<h2 class="autumn-card__title">Posts</h2>"#),
            "{html}"
        );
        assert!(html.contains(r#"class="autumn-card__header""#), "{html}");
    }

    #[test]
    fn card_title_respects_heading_level() {
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new().title("X").level(HeadingLevel::H3)).into_string();
        assert!(
            html.contains(r#"<h3 class="autumn-card__title">"#),
            "{html}"
        );
        assert!(!html.contains("<h2"), "{html}");
    }

    #[test]
    fn card_omits_header_when_empty() {
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new()).into_string();
        assert!(!html.contains("autumn-card__header"), "{html}");
    }

    #[test]
    fn card_renders_header_action() {
        let action = maud::html! { a class="btn" href="/new" { "New" } };
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new().header_action(action)).into_string();
        assert!(html.contains(r#"class="autumn-card__header""#), "{html}");
        assert!(html.contains(r#"class="btn""#), "{html}");
        assert!(html.contains(r#"href="/new""#), "{html}");
    }

    #[test]
    fn card_header_action_only_no_title() {
        let action = maud::html! { button { "Click" } };
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new().header_action(action)).into_string();
        assert!(html.contains("autumn-card__header"), "{html}");
        assert!(!html.contains("autumn-card__title"), "{html}");
        assert!(!html.contains("<h2"), "{html}");
    }

    #[test]
    fn card_renders_footer() {
        let footer = maud::html! { span { "Save" } };
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new().footer(footer)).into_string();
        assert!(html.contains(r#"class="autumn-card__footer""#), "{html}");
        assert!(html.contains("Save"), "{html}");
    }

    #[test]
    fn card_omits_footer_when_none() {
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new()).into_string();
        assert!(!html.contains("autumn-card__footer"), "{html}");
    }

    #[test]
    fn card_extra_class_escape_hatch() {
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new().class("dashboard")).into_string();
        assert!(html.contains(r#"class="autumn-card dashboard""#), "{html}");
    }

    #[test]
    fn card_escapes_title() {
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new().title("<script>alert(1)</script>")).into_string();
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>alert"), "{html}");
    }

    #[test]
    fn card_title_html_passes_markup() {
        let rich = maud::html! {
            span { "Posts" }
            span class="muted" { " (42)" }
        };
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new().title_html(rich)).into_string();
        assert!(html.contains(r#"class="muted""#), "{html}");
        assert!(html.contains("(42)"), "{html}");
    }

    #[test]
    fn card_body_composes_property_list() {
        let rows = vec![("Name", maud::html! { "Alice" })];
        let body = property_list(&rows);
        let html = card(&body, &CardConfig::new().title("Detail")).into_string();
        assert!(html.contains(r#"class="autumn-card__body""#), "{html}");
        assert!(html.contains(r#"class="autumn-property-list""#), "{html}");
    }

    // ── stat_card ──────────────────────────────────────────────────────

    #[test]
    fn stat_card_renders_label_value() {
        let html = stat_card("Users", "42", None).into_string();
        assert!(html.contains(r#"class="autumn-stat-card""#), "{html}");
        assert!(
            html.contains(r#"class="autumn-stat-card__label""#),
            "{html}"
        );
        assert!(
            html.contains(r#"class="autumn-stat-card__value""#),
            "{html}"
        );
        assert!(html.contains("Users"), "{html}");
        assert!(html.contains("42"), "{html}");
        assert!(!html.contains("autumn-stat-card__link"), "{html}");
    }

    #[test]
    fn stat_card_renders_optional_link() {
        let html = stat_card("Users", "42", Some(("/users", "View all →"))).into_string();
        assert!(html.contains(r#"class="autumn-stat-card__link""#), "{html}");
        assert!(html.contains(r#"href="/users""#), "{html}");
        assert!(html.contains("View all"), "{html}");
    }

    #[test]
    fn stat_card_escapes_value() {
        let html = stat_card("L", "<b>x</b>", None).into_string();
        assert!(html.contains("&lt;b&gt;"), "{html}");
        assert!(!html.contains("<b>x</b>"), "{html}");
    }

    // ── hero ─────────────────────────────────────────────────────────────

    #[test]
    fn hero_title_only() {
        let html = hero(&HeroConfig::new("Welcome")).into_string();
        assert!(html.contains("Welcome"), "{html}");
        assert!(html.contains(r#"class="autumn-hero""#), "{html}");
        // Single top-level heading, no subtitle, no CTA container.
        assert!(html.contains("<h1"), "{html}");
        assert_eq!(html.matches("<h1").count(), 1, "{html}");
        assert!(!html.contains("autumn-hero__subtitle"), "{html}");
        assert!(!html.contains("autumn-hero__actions"), "{html}");
        assert!(!html.contains("<a"), "{html}");
    }

    #[test]
    fn hero_title_and_subtitle() {
        let html = hero(&HeroConfig::new("Welcome").subtitle("A tagline")).into_string();
        assert!(html.contains("Welcome"), "{html}");
        assert!(html.contains(r#"class="autumn-hero__subtitle""#), "{html}");
        assert!(html.contains("A tagline"), "{html}");
    }

    #[test]
    fn hero_title_subtitle_two_ctas() {
        let html = hero(
            &HeroConfig::new("Welcome")
                .subtitle("A tagline")
                .cta(Cta::primary("Get started", "/signup"))
                .cta(Cta::secondary("Learn more", "/about")),
        )
        .into_string();
        assert!(html.contains(r#"class="autumn-hero__actions""#), "{html}");
        assert!(html.contains(r#"href="/signup""#), "{html}");
        assert!(html.contains("Get started"), "{html}");
        assert!(html.contains(r#"href="/about""#), "{html}");
        assert!(html.contains("Learn more"), "{html}");
        assert!(html.contains("autumn-hero__cta--primary"), "{html}");
        assert!(html.contains("autumn-hero__cta--secondary"), "{html}");
    }

    #[test]
    fn hero_heading_level_configurable() {
        let html = hero(&HeroConfig::new("Welcome").level(HeadingLevel::H2)).into_string();
        assert!(html.contains("<h2"), "{html}");
        assert!(!html.contains("<h1"), "{html}");
    }

    #[test]
    fn hero_default_heading_level_is_h1() {
        let config = HeroConfig::new("Welcome");
        let html = hero(&config).into_string();
        assert!(html.contains("<h1"), "{html}");
    }

    #[test]
    fn hero_escapes_title_and_subtitle() {
        let html = hero(&HeroConfig::new("<b>hi</b>").subtitle("<i>tag</i>")).into_string();
        assert!(html.contains("&lt;b&gt;"), "{html}");
        assert!(!html.contains("<b>hi</b>"), "{html}");
        assert!(html.contains("&lt;i&gt;"), "{html}");
        assert!(!html.contains("<i>tag</i>"), "{html}");
    }

    #[test]
    fn hero_extra_class_escape_hatch() {
        let html = hero(&HeroConfig::new("Welcome").class("centered")).into_string();
        assert!(html.contains(r#"class="autumn-hero centered""#), "{html}");
    }

    #[test]
    fn hero_cta_and_class_accept_owned_strings_built_inline() {
        // Cta::primary/secondary and .class() take `impl Into<String>` so a
        // caller can build them from owned data (e.g. a DB row) in the same
        // expression, without a `let` binding to extend a temporary's lifetime.
        let post_id = 42;
        let html = hero(
            &HeroConfig::new("Welcome")
                .class(format!("theme-{post_id}"))
                .cta(Cta::primary(
                    format!("Read post {post_id}"),
                    format!("/posts/{post_id}"),
                )),
        )
        .into_string();
        assert!(html.contains(r#"class="autumn-hero theme-42""#), "{html}");
        assert!(html.contains(r#"href="/posts/42""#), "{html}");
        assert!(html.contains("Read post 42"), "{html}");
    }

    #[test]
    fn cta_style_default_is_primary() {
        assert_eq!(CtaStyle::default(), CtaStyle::Primary);
    }

    fn btn(config: &ButtonConfig<'_>) -> String {
        button("Save", config).into_string()
    }

    // ── button ──────────────────────────────────────────────────────────

    #[test]
    fn button_default_is_a_medium_primary_type_button() {
        let html = btn(&ButtonConfig::new());
        assert!(html.starts_with("<button"), "{html}");
        assert!(html.contains(r#"type="button""#), "{html}");
        assert!(
            html.contains(r#"class="autumn-button autumn-button--primary autumn-button--md""#),
            "{html}"
        );
        assert!(
            html.contains(r#"<span class="autumn-button__label">Save</span>"#),
            "{html}"
        );
        assert!(!html.contains("disabled"), "{html}");
        assert!(!html.contains("aria-busy"), "{html}");
        assert!(!html.contains("autumn-button__spinner"), "{html}");
    }

    #[test]
    fn button_variant_and_size_set_their_classes() {
        for (variant, class) in [
            (ButtonVariant::Primary, "autumn-button--primary"),
            (ButtonVariant::Secondary, "autumn-button--secondary"),
            (ButtonVariant::Ghost, "autumn-button--ghost"),
            (ButtonVariant::Destructive, "autumn-button--destructive"),
            (ButtonVariant::Link, "autumn-button--link"),
        ] {
            let html = btn(&ButtonConfig::new().variant(variant));
            assert!(html.contains(class), "{variant:?}: {html}");
        }
        for (size, class) in [
            (ButtonSize::Small, "autumn-button--sm"),
            (ButtonSize::Medium, "autumn-button--md"),
            (ButtonSize::Large, "autumn-button--lg"),
        ] {
            let html = btn(&ButtonConfig::new().size(size));
            assert!(html.contains(class), "{size:?}: {html}");
        }
    }

    #[test]
    fn button_kind_sets_the_type() {
        assert!(btn(&ButtonConfig::new().submit()).contains(r#"type="submit""#));
        let reset = ButtonConfig::new().kind(crate::a11y::ButtonType::Reset);
        assert!(btn(&reset).contains(r#"type="reset""#));
    }

    #[test]
    fn button_icon_is_hidden_and_before_the_label() {
        let icon = html! { svg viewBox="0 0 1 1" {} };
        let html = btn(&ButtonConfig::new().icon(icon));
        let at_icon = html
            .find(r#"<span class="autumn-button__icon" aria-hidden="true"><svg"#)
            .unwrap_or_else(|| panic!("no icon: {html}"));
        let at_label = html.find("autumn-button__label").unwrap();
        assert!(at_icon < at_label, "{html}");
    }

    #[test]
    fn icon_only_button_keeps_the_label_as_hidden_text() {
        let icon = html! { "×" };
        let html = btn(&ButtonConfig::new().icon(icon).icon_only());
        assert!(html.contains("autumn-button--icon"), "{html}");
        assert!(
            html.contains(
                r#"<span class="autumn-button__label autumn-visually-hidden">Save</span>"#
            ),
            "{html}"
        );
        assert!(!html.contains("aria-label"), "{html}");
    }

    #[test]
    fn loading_button_is_busy_disabled_and_has_a_spinner() {
        let html = btn(&ButtonConfig::new().loading());
        assert!(html.contains(r#"aria-busy="true""#), "{html}");
        assert!(html.contains("disabled"), "{html}");
        assert!(
            html.contains(
                r#"<span class="autumn-button__spinner autumn-spinner__icon" aria-hidden="true"></span>"#
            ),
            "{html}"
        );
        // The spinner is first, so CSS can hide the icon and the label after it.
        let at_spinner = html.find("autumn-button__spinner").unwrap();
        assert!(at_spinner < html.find("autumn-button__label").unwrap());
        assert!(!html.contains("data-autumn-busy"), "{html}");
    }

    #[test]
    fn loading_on_request_marks_the_button_for_the_runtime() {
        let html = btn(&ButtonConfig::new().loading_on_request());
        assert!(html.contains("data-autumn-busy"), "{html}");
        assert!(html.contains("autumn-button__spinner"), "{html}");
        // The runtime sets aria-busy. The server does not.
        assert!(!html.contains("aria-busy"), "{html}");
        assert!(!html.contains("disabled"), "{html}");
    }

    #[test]
    fn the_last_loading_call_wins() {
        let html = btn(&ButtonConfig::new().loading_on_request().loading());
        assert!(html.contains(r#"aria-busy="true""#), "{html}");
        assert!(!html.contains("data-autumn-busy"), "{html}");
        let html = btn(&ButtonConfig::new().loading().loading_on_request());
        assert!(html.contains("data-autumn-busy"), "{html}");
        assert!(!html.contains("aria-busy"), "{html}");
        // `disabled` with a static loading state is one attribute.
        let html = btn(&ButtonConfig::new().disabled().loading());
        assert_eq!(html.matches("disabled").count(), 1, "{html}");
    }

    #[test]
    fn button_id_class_and_disabled() {
        let html = btn(&ButtonConfig::new().id("save").class("wide").disabled());
        assert!(html.contains(r#"id="save""#), "{html}");
        assert!(html.contains("autumn-button--md wide"), "{html}");
        assert!(html.contains("disabled"), "{html}");
    }

    #[test]
    fn button_attrs_pass_through_but_cannot_replace_owned_ones() {
        // The app owns its CSP (CLAUDE.md): handlers and `style` pass
        // through. The widget's own attributes win.
        let attrs = [
            ("hx-post", "/save?a=1&b=2"),
            ("hx-indicator", "this"),
            ("type", "submit"),
            ("CLASS", "evil"),
            ("hx-on::after-request", "this.form.reset()"),
            ("data-user_id", "7"),
            ("style", "color:red"),
            ("title", "a\"b"),
            ("title", "second"),
            ("Bad Name", "x"),
            ("\"><script>", "x"),
        ];
        let html = btn(&ButtonConfig::new().attrs(&attrs));
        assert!(html.contains(r#"hx-post="/save?a=1&amp;b=2""#), "{html}");
        assert!(html.contains(r#"hx-indicator="this""#), "{html}");
        assert_eq!(html.matches("type=").count(), 1, "{html}");
        assert!(html.contains(r#"type="button""#), "{html}");
        assert!(!html.contains("evil"), "{html}");
        assert!(
            html.contains(r#"hx-on::after-request="this.form.reset()""#),
            "{html}"
        );
        assert!(html.contains(r#"data-user_id="7""#), "{html}");
        assert!(html.contains(r#"style="color:red""#), "{html}");
        assert!(html.contains(r#"title="a&quot;b""#), "{html}");
        assert!(!html.contains("second"), "{html}");
        assert!(!html.contains("Bad Name"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "icon_only() needs an icon()")]
    fn icon_only_without_an_icon_panics_in_debug() {
        let _ = btn(&ButtonConfig::new().icon_only());
    }

    #[test]
    fn button_label_is_escaped() {
        let html = button("<script>x</script>", &ButtonConfig::new()).into_string();
        assert!(html.contains("&lt;script&gt;x&lt;/script&gt;"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "accessible name")]
    fn button_with_a_blank_label_panics_in_debug() {
        let _ = button("  ", &ButtonConfig::new());
    }

    // ── button_group ────────────────────────────────────────────────────

    #[test]
    fn button_group_is_a_named_group() {
        let a = button("Left", &ButtonConfig::new());
        let b = button("Right", &ButtonConfig::new());
        let html = button_group("Text alignment", &[a, b]).into_string();
        assert!(
            html.starts_with(
                r#"<div class="autumn-button-group" role="group" aria-label="Text alignment">"#
            ),
            "{html}"
        );
        assert!(
            html.contains(">Left<") && html.contains(">Right<"),
            "{html}"
        );
        assert!(html.ends_with("</div>"), "{html}");
    }

    #[test]
    fn button_group_vertical_and_class() {
        let config = ButtonGroupConfig::new().vertical().class("tools");
        let html = button_group_with("Tools", &[], &config).into_string();
        assert!(
            html.contains(r#"class="autumn-button-group autumn-button-group--vertical tools""#),
            "{html}"
        );
    }

    // ── separator ───────────────────────────────────────────────────────

    #[test]
    fn separator_is_an_hr() {
        assert_eq!(
            separator().into_string(),
            r#"<hr class="autumn-separator">"#
        );
        let html = separator_with(&SeparatorConfig::new().decorative().class("x")).into_string();
        assert_eq!(html, r#"<hr class="autumn-separator x" role="none">"#);
    }

    #[test]
    fn vertical_separator_has_role_and_orientation() {
        let html = separator_with(&SeparatorConfig::new().vertical()).into_string();
        assert_eq!(
            html,
            concat!(
                r#"<span class="autumn-separator autumn-separator--vertical" "#,
                r#"role="separator" aria-orientation="vertical"></span>"#,
            )
        );
        let html = separator_with(&SeparatorConfig::new().vertical().decorative()).into_string();
        assert!(html.contains(r#"role="none""#), "{html}");
        assert!(!html.contains("aria-orientation"), "{html}");
    }
}
