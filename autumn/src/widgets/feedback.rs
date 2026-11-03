//! Feedback widgets: alert, error summary, toast, tooltip, spinner,
//! skeleton, progress, meter, empty state and confirm action.

use super::*;

/// Configuration for a [`confirm_action`] confirmation dialog.
///
/// Build with [`ConfirmActionConfig::new`] and chain builder methods for
/// optional overrides. `title` defaults to `"Are you sure?"`, `cancel_label`
/// to `"Cancel"`, and `danger` (the confirm button's semantic styling) to
/// `true`.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::ConfirmActionConfig;
/// use maud::html;
///
/// let config = ConfirmActionConfig::new()
///     .title("Delete this post?")
///     .message(html! { p { "This action cannot be undone." } })
///     .trigger_class("btn btn-danger")
///     .confirm_class("btn btn-danger");
/// ```
#[derive(Debug, Clone)]
pub struct ConfirmActionConfig<'a> {
    /// Dialog title (default `"Are you sure?"`).
    title: &'a str,
    /// Optional body content rendered above the confirm/cancel actions.
    message: Option<maud::Markup>,
    /// Confirm button label; defaults to the trigger's label when unset.
    confirm_label: Option<&'a str>,
    /// Cancel button label (default `"Cancel"`).
    cancel_label: &'a str,
    /// `class` attribute for the trigger button that opens the dialog.
    trigger_class: Option<&'a str>,
    /// Extra `class`(es) appended to the confirm button, alongside
    /// `autumn-modal__confirm` and (when [`ConfirmActionConfig::danger`] is
    /// `true`) `autumn-modal__confirm--danger`.
    confirm_class: Option<&'a str>,
    /// Whether the confirm button carries the danger semantic class
    /// (default `true` — most uses of `confirm_action` are destructive).
    danger: bool,
    /// CSRF form field name; defaults to `"_csrf"`. Pass the configured
    /// [`CsrfFormField`](crate::security::CsrfFormField) value when
    /// `security.csrf.form_field` is customized.
    csrf_field: Option<&'a str>,
    /// Extra attributes rendered on the confirm `<button>`, e.g. `hx-*`, `data-*`.
    attrs: &'a [(&'a str, &'a str)],
    /// Heading level for the dialog title, forwarded to [`ModalConfig::level`]
    /// (default [`HeadingLevel::H2`]).
    level: HeadingLevel,
    /// Extra CSS class(es) appended to the dialog's root `autumn-modal`
    /// element, forwarded to [`ModalConfig::class`].
    modal_class: Option<&'a str>,
    /// When `true`, the dialog's backdrop and Escape dismiss it, forwarded
    /// to [`ModalConfig::light_dismiss`] (default `false`).
    light_dismiss: bool,
}

impl<'a> ConfirmActionConfig<'a> {
    /// Create a confirm-action configuration with default title ("Are you
    /// sure?"), cancel label ("Cancel"), and `danger` styling enabled.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            title: "Are you sure?",
            message: None,
            confirm_label: None,
            cancel_label: "Cancel",
            trigger_class: None,
            confirm_class: None,
            danger: true,
            csrf_field: None,
            attrs: &[],
            level: HeadingLevel::H2,
            modal_class: None,
            light_dismiss: false,
        }
    }

    /// Set the dialog title (default `"Are you sure?"`).
    #[must_use]
    pub const fn title(mut self, title: &'a str) -> Self {
        self.title = title;
        self
    }

    /// Set optional body content rendered above the confirm/cancel actions.
    #[must_use]
    pub fn message(mut self, message: maud::Markup) -> Self {
        self.message = Some(message);
        self
    }

    /// Override the confirm button's label (defaults to the trigger's label).
    #[must_use]
    pub const fn confirm_label(mut self, label: &'a str) -> Self {
        self.confirm_label = Some(label);
        self
    }

    /// Set the cancel button's label (default `"Cancel"`).
    #[must_use]
    pub const fn cancel_label(mut self, label: &'a str) -> Self {
        self.cancel_label = label;
        self
    }

    /// Set the `class` attribute for the trigger button that opens the dialog.
    #[must_use]
    pub const fn trigger_class(mut self, class: &'a str) -> Self {
        self.trigger_class = Some(class);
        self
    }

    /// Add extra `class`(es) to the confirm button, alongside the built-in
    /// `autumn-modal__confirm`(`--danger`) classes.
    #[must_use]
    pub const fn confirm_class(mut self, class: &'a str) -> Self {
        self.confirm_class = Some(class);
        self
    }

    /// Set whether the confirm button carries the danger semantic class
    /// (default `true`).
    #[must_use]
    pub const fn danger(mut self, danger: bool) -> Self {
        self.danger = danger;
        self
    }

    /// Override the CSRF hidden-input field name (default `"_csrf"`).
    #[must_use]
    pub const fn csrf_field(mut self, csrf_field: &'a str) -> Self {
        self.csrf_field = Some(csrf_field);
        self
    }

    /// Set extra attributes rendered on the confirm `<button>` (e.g.
    /// `hx-*`, `data-*`). Values are HTML-escaped; names must be valid
    /// attribute names (see [`crate::links::button_to_with`] panics).
    #[must_use]
    pub const fn attrs(mut self, attrs: &'a [(&'a str, &'a str)]) -> Self {
        self.attrs = attrs;
        self
    }

    /// Set the heading level for the dialog title (default [`HeadingLevel::H2`]).
    #[must_use]
    pub const fn level(mut self, level: HeadingLevel) -> Self {
        self.level = level;
        self
    }

    /// Add extra CSS class(es) to the dialog's root `autumn-modal` element.
    #[must_use]
    pub const fn modal_class(mut self, class: &'a str) -> Self {
        self.modal_class = Some(class);
        self
    }

    /// When `true`, the dialog's backdrop and Escape dismiss it (default
    /// `false` — a destructive confirm isn't accidentally dismissed by a
    /// stray click). See [`ModalConfig::light_dismiss`] for browser support
    /// notes on the underlying `closedby="any"` attribute.
    #[must_use]
    pub const fn light_dismiss(mut self, light_dismiss: bool) -> Self {
        self.light_dismiss = light_dismiss;
        self
    }
}

impl Default for ConfirmActionConfig<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Render a trigger button plus a confirmation `<dialog>` for a
/// state-changing action.
///
/// Opening the dialog requires no app-authored JavaScript
/// ([`modal_trigger`]/[`modal_close_button`]), and the confirm
/// button is a [`crate::links::button_to_with`] submit — the correct HTTP
/// method (via the `_method` override) and CSRF token are always present. A
/// `<noscript>` fallback renders the same submit button directly (no
/// confirmation, matching plain HTML forms), so the action stays reachable
/// with JavaScript disabled — before JS runs, the trigger's `command`
/// attribute alone can't open the dialog.
///
/// This is the framework's answer to `window.confirm()` / htmx's
/// `hx-confirm`: the whole confirmation UI is server-rendered, so a
/// [`crate::test::TestClient`] test can assert the dialog, its title, and
/// the confirm button's action/method/CSRF token — impossible with the
/// native browser dialog.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-confirm` | Root wrapper (trigger + dialog) |
/// | `.autumn-modal__confirm-form` | The confirm `<form>` |
/// | `.autumn-modal__confirm` | The confirm `<button>` |
/// | `.autumn-modal__confirm--danger` | Added to the confirm button when [`ConfirmActionConfig::danger`] is `true` (the default) |
/// | `.autumn-modal__cancel` | The cancel `<button>` |
///
/// # Example
///
/// Confirm-delete on a scaffolded resource — a `Post` detail page's delete
/// button, with zero lines of app-authored JavaScript:
///
/// ```rust,ignore
/// use autumn_web::prelude::*;
/// use autumn_web::widgets::{ConfirmActionConfig, confirm_action};
/// use http::Method;
///
/// #[get("/posts/{id}")]
/// async fn show(Path(id): Path<i64>, csrf: CsrfToken, repo: PgPostRepository) -> AutumnResult<Markup> {
///     let post = repo.find(id).await?;
///     let action = format!("/posts/{id}");
///     let config = ConfirmActionConfig::new()
///         .title("Delete this post?")
///         .message(html! { p { "This action cannot be undone." } })
///         .trigger_class("btn btn-danger")
///         .confirm_class("btn btn-danger");
///     Ok(html! {
///         h1 { (post.title) }
///         (confirm_action("delete-post", "Delete", &action, Method::DELETE, csrf.token(), &config))
///     })
/// }
/// ```
///
/// ```rust
/// use autumn_web::widgets::{ConfirmActionConfig, confirm_action};
/// use http::Method;
///
/// let html = confirm_action(
///     "delete-post",
///     "Delete",
///     "/posts/42",
///     Method::DELETE,
///     "tok123",
///     &ConfirmActionConfig::new()
///         .title("Delete this post?")
///         .trigger_class("btn btn-danger")
///         .confirm_class("btn btn-danger"),
/// )
/// .into_string();
/// assert!(html.contains(r#"<dialog id="delete-post""#));
/// assert!(html.contains("Delete this post?"));
/// assert!(html.contains(r#"<form action="/posts/42" method="post""#));
/// assert!(html.contains(r#"name="_method" value="DELETE""#));
/// assert!(html.contains(r#"name="_csrf" value="tok123""#));
/// assert!(html.contains("autumn-modal__confirm--danger"));
/// ```
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn confirm_action(
    id: &str,
    trigger_label: &str,
    action: &str,
    method: http::Method,
    csrf_token: &str,
    config: &ConfirmActionConfig<'_>,
) -> maud::Markup {
    let confirm_label = config.confirm_label.unwrap_or(trigger_label);

    let danger_class = config.danger.then_some("autumn-modal__confirm--danger");
    let confirm_class = merge_class(
        &merge_class("autumn-modal__confirm", danger_class),
        config.confirm_class,
    );

    let mut button_options = crate::links::ButtonToOptions::new()
        .class(confirm_class.as_str())
        .form_class("autumn-modal__confirm-form")
        .attrs(config.attrs);
    if let Some(field) = config.csrf_field {
        button_options = button_options.csrf_field(field);
    }

    let confirm_button = crate::links::button_to_with(
        confirm_label,
        action,
        method.clone(),
        csrf_token,
        &button_options,
    );

    let footer = maud::html! {
        (modal_close_button(config.cancel_label, id, Some("autumn-modal__cancel")))
        (confirm_button)
    };

    let body = config.message.clone().unwrap_or_else(|| maud::html! {});
    let mut modal_config = ModalConfig::new()
        .footer(footer)
        .level(config.level)
        .light_dismiss(config.light_dismiss);
    if let Some(class) = config.modal_class {
        modal_config = modal_config.class(class);
    }

    // No-JS / no-Invoker-Commands fallback: the trigger above can't open the
    // dialog without either the native `command`/`commandfor` API or the
    // `autumn-widgets.js` fallback running, so this plain, always-working
    // (unconfirmed) submit button keeps the action reachable — the same
    // fallback pattern `active_search`/`autocomplete_input` use.
    let mut noscript_options = crate::links::ButtonToOptions::new().attrs(config.attrs);
    if let Some(class) = config.trigger_class {
        noscript_options = noscript_options.class(class);
    }
    if let Some(field) = config.csrf_field {
        noscript_options = noscript_options.csrf_field(field);
    }
    let noscript_button =
        crate::links::button_to_with(trigger_label, action, method, csrf_token, &noscript_options);

    maud::html! {
        div class="autumn-confirm" {
            (modal_trigger(trigger_label, id, config.trigger_class))
            noscript { (noscript_button) }
            (modal(id, config.title, &body, &modal_config))
        }
    }
}

// ── tooltip (#3404) ─────────────────────────────────────────────────────────

/// Where a [`tooltip`] shows, next to its trigger.
///
/// With CSS anchor positioning, the text flips to the opposite side when it
/// has no room. `Start` and `End` follow the writing direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TooltipPlacement {
    /// Above the trigger. The default.
    #[default]
    Top,
    /// Below the trigger.
    Bottom,
    /// Before the trigger: left in a left-to-right page.
    Start,
    /// After the trigger: right in a left-to-right page.
    End,
}

impl TooltipPlacement {
    /// The root modifier class, for example `autumn-tooltip--top`.
    const fn class(self) -> &'static str {
        match self {
            Self::Top => "autumn-tooltip--top",
            Self::Bottom => "autumn-tooltip--bottom",
            Self::Start => "autumn-tooltip--start",
            Self::End => "autumn-tooltip--end",
        }
    }
}

/// Options for [`tooltip_with`]. The default is the [`tooltip`] output.
#[derive(Debug, Clone, Default)]
pub struct TooltipConfig<'a> {
    placement: TooltipPlacement,
    class: Option<&'a str>,
}

impl<'a> TooltipConfig<'a> {
    /// A default config: [`TooltipPlacement::Top`], no extra class.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the side where the text shows.
    #[must_use]
    pub const fn placement(mut self, placement: TooltipPlacement) -> Self {
        self.placement = placement;
        self
    }

    /// Add a class to the root `<span>`.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Render an accessible tooltip: a short text that describes a trigger.
///
/// `trigger` gets the id of the text. Put it in `aria-describedby` on the
/// focusable element. For an icon-only trigger, put it in `aria-labelledby`.
/// Then the text is the accessible name. The id goes in the server HTML, so
/// the link works without JS. A leading `#` in `id` is removed.
///
/// If the trigger does not use the id, the text has no link to it. Then
/// screen readers do not read it, and Escape does not hide it.
///
/// A tooltip holds text only (`text: &str`). For links or buttons, use a
/// popover. Do not use `title` for a tooltip: keyboard users cannot see
/// it. A tooltip is not for touch: a touch screen has no hover. Do not put
/// the only label of a control in it on a touch layout.
///
/// **Behaviour (WCAG 1.4.13):**
///
/// - CSS shows the text on hover and on keyboard focus (`:focus-visible`).
///   A mouse click does not keep it open.
/// - A transparent bridge covers the gap. The pointer can move onto the text.
/// - The text has no timer. It stays until hover and focus go. Escape also
///   hides it.
/// - Escape hides it. `autumn-widgets.js` does this with the keyboard core
///   (`data-autumn-dismiss="escape"`). The next hover or focus shows it again.
///   Without JS, Escape does nothing.
/// - CSS anchor positioning places the text and flips it at the viewport
///   edge. Older browsers place it with `position: absolute`.
/// - On Chromium, the runtime adds `popover="hint"` to the text and
///   `interestfor` to a `<button>` or `<a href>` trigger. The browser then shows,
///   hides and dismisses it in the top layer. The server never sends these
///   attributes, so other browsers keep the CSS baseline.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-tooltip` | Root `<span>`, the anchor |
/// | `.autumn-tooltip--<placement>` | `top`, `bottom`, `start` or `end` |
/// | `.autumn-tooltip__bubble` | The `role="tooltip"` text |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::tooltip;
/// use maud::html;
///
/// let out = tooltip("copy-tip", "Copy the link", |tip| html! {
///     button type="button" aria-describedby=(tip) { "Copy" }
/// })
/// .into_string();
/// assert!(out.contains(r#"aria-describedby="copy-tip""#));
/// assert!(out.contains(r#"role="tooltip""#));
/// ```
#[must_use]
pub fn tooltip(id: &str, text: &str, trigger: impl FnOnce(&str) -> maud::Markup) -> maud::Markup {
    tooltip_with(id, text, &TooltipConfig::new(), trigger)
}

/// Like [`tooltip`], with a [`TooltipConfig`]: placement and an extra class.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{TooltipConfig, TooltipPlacement, tooltip_with};
/// use maud::html;
///
/// let config = TooltipConfig::new().placement(TooltipPlacement::Bottom);
/// let out = tooltip_with("del-tip", "Delete", &config, |tip| html! {
///     button type="button" aria-labelledby=(tip) { span aria-hidden="true" { "×" } }
/// })
/// .into_string();
/// assert!(out.contains("autumn-tooltip--bottom"));
/// ```
#[must_use]
pub fn tooltip_with(
    id: &str,
    text: &str,
    config: &TooltipConfig<'_>,
    trigger: impl FnOnce(&str) -> maud::Markup,
) -> maud::Markup {
    let id = selector_to_id(id);
    // A text of spaces only is empty: CSS `:empty` then hides it.
    let text = text.trim();
    let root = merge_class(
        &format!("autumn-tooltip {}", config.placement.class()),
        config.class,
    );
    maud::html! {
        span class=(root) data-autumn-tooltip {
            (trigger(id))
            span id=(id) class="autumn-tooltip__bubble" role="tooltip"
                data-autumn-dismiss="escape" { (text) }
        }
    }
}

// ── alert / callout (#1314) ─────────────────────────────────────────────────

/// Semantic variant for an [`alert`] / [`error_summary`] callout.
///
/// Names match the flash levels (`info`/`success`/`warning`/`error`) so both
/// share one stylesheet. `Error`/`Warning` render `role="alert"`;
/// `Info`/`Success` render `role="status"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlertVariant {
    /// Neutral information (blue). The default.
    #[default]
    Info,
    /// Positive confirmation (green).
    Success,
    /// Something needs attention (amber).
    Warning,
    /// Something went wrong (red).
    Error,
}

impl AlertVariant {
    /// The variant name as a lowercase string (matches the CSS class suffix and
    /// the flash-level names).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    /// The ARIA live-region role: `alert` for error/warning (assertive),
    /// `status` for info/success (polite).
    const fn role(self) -> &'static str {
        match self {
            Self::Error | Self::Warning => "alert",
            Self::Info | Self::Success => "status",
        }
    }

    /// A decorative inline-SVG icon for this variant (no icon-font dependency).
    fn icon(self) -> maud::Markup {
        let glyph = match self {
            Self::Info => "i",
            Self::Success => "✓",
            Self::Warning => "!",
            Self::Error => "×",
        };
        maud::html! {
            svg class="autumn-alert__icon-svg" viewBox="0 0 20 20" width="20" height="20"
                aria-hidden="true" focusable="false" {
                circle cx="10" cy="10" r="10" fill="currentColor" {}
                // `fill` is the no-CSS fallback. The stylesheet sets
                // `--surface`, which stays visible on the dark-scheme circle.
                text class="autumn-alert__icon-glyph" x="10" y="15" text-anchor="middle"
                    font-size="13" font-family="sans-serif" fill="#fff" { (glyph) }
            }
        }
    }
}

/// Builder options for [`alert_with`].
#[derive(Debug, Clone, Default)]
pub struct AlertConfig<'a> {
    title: Option<&'a str>,
    icon: bool,
    dismissible: bool,
    class: Option<&'a str>,
}

impl<'a> AlertConfig<'a> {
    /// A default config: no title, no icon, not dismissible.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a heading rendered above the body (an `<h2 class="autumn-alert__title">`).
    #[must_use]
    pub const fn title(mut self, title: &'a str) -> Self {
        self.title = Some(title);
        self
    }

    /// Render the variant's leading inline-SVG icon.
    #[must_use]
    pub const fn icon(mut self, show: bool) -> Self {
        self.icon = show;
        self
    }

    /// Render a no-JavaScript dismiss control (a `<label>`-wrapped hidden
    /// checkbox; the stylesheet's `:has()` rule hides the alert when checked).
    /// When omitted, no close control and no JS are emitted.
    #[must_use]
    pub const fn dismissible(mut self, yes: bool) -> Self {
        self.dismissible = yes;
        self
    }

    /// Append an extra class to the root element.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Render an inline, block-level contextual message box.
///
/// Emits `<div class="autumn-alert autumn-alert--<variant>" role="...">` wrapping `body`.
/// `role` is `alert` for `Error`/`Warning`, `status` for `Info`/`Success`. All
/// caller-supplied markup is escaped by Maud. No inline `style=` is emitted and
/// no JavaScript is required. For a title, icon, or dismiss control use
/// [`alert_with`].
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{alert, AlertVariant};
/// use maud::html;
///
/// let box_ = alert(AlertVariant::Info, html! { "No posts yet — create your first one." });
/// let out = box_.into_string();
/// assert!(out.contains(r#"class="autumn-alert autumn-alert--info""#));
/// assert!(out.contains(r#"role="status""#));
/// ```
#[must_use]
pub fn alert(variant: AlertVariant, body: maud::Markup) -> maud::Markup {
    alert_with(variant, body, &AlertConfig::new())
}

/// Render an alert with a [`AlertConfig`] (title, icon, dismiss control).
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{alert_with, AlertConfig, AlertVariant};
/// use maud::html;
///
/// let out = alert_with(
///     AlertVariant::Error,
///     html! { "Your card was declined." },
///     &AlertConfig::new().title("Payment failed").icon(true).dismissible(true),
/// )
/// .into_string();
/// assert!(out.contains(r#"role="alert""#));
/// assert!(out.contains(r#"<h2 class="autumn-alert__title">Payment failed</h2>"#));
/// assert!(out.contains("autumn-alert__dismiss"));
/// assert!(!out.contains("<script")); // no JS
/// ```
// `body` is taken by value to match the issue's `alert(variant, body: Markup)`
// signature and keep `alert(v, html! { .. })` ergonomic (no `&` at the call
// site); Maud renders it by reference, hence the allow.
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn alert_with(
    variant: AlertVariant,
    body: maud::Markup,
    config: &AlertConfig<'_>,
) -> maud::Markup {
    alert_root(variant, &body, config, None)
}

/// Shared body of [`alert_with`] and [`error_summary`]. `focus_id` gives the
/// root an `id`, `tabindex="-1"` and `autofocus`, so the browser (and htmx
/// after a swap) moves focus to it.
fn alert_root(
    variant: AlertVariant,
    body: &maud::Markup,
    config: &AlertConfig<'_>,
    focus_id: Option<&str>,
) -> maud::Markup {
    let root = merge_class(
        &format!("autumn-alert autumn-alert--{}", variant.as_str()),
        config.class,
    );
    maud::html! {
        div id=[focus_id] class=(root) role=(variant.role())
            tabindex=[focus_id.map(|_| "-1")] autofocus[focus_id.is_some()] {
            @if config.icon {
                span class="autumn-alert__icon" { (variant.icon()) }
            }
            div class="autumn-alert__content" {
                @if let Some(title) = config.title {
                    (heading(HeadingLevel::H2, None, "autumn-alert__title", &maud::html! { (title) }))
                }
                div class="autumn-alert__body" { (body) }
            }
            @if config.dismissible {
                // The checkbox stays in tab order (visually hidden but focusable
                // via `.autumn-alert__dismiss-toggle`, never `hidden`/`display:none`) so
                // keyboard and screen-reader users can dismiss; the accessible
                // name is on the control itself.
                label class="autumn-alert__dismiss" {
                    input type="checkbox" class="autumn-alert__dismiss-toggle"
                        aria-label="Dismiss this message";
                    span aria-hidden="true" { "×" }
                }
            }
        }
    }
}

/// The `id` of the [`error_summary`] root.
pub const ERROR_SUMMARY_ID: &str = "error-summary";

/// Render an [`AlertVariant::Error`] alert summarizing every field error in a
/// [`Changeset`](crate::form::Changeset), or `None` when the changeset is valid.
///
/// The messages are listed in a `<ul>` in a stable (sorted) order — directly
/// usable at the top of a re-rendered form. Each message is HTML-escaped.
///
/// Each entry links to `#{field}`, the `id` the form helpers give the
/// control, so a click moves to the field. A form-level error has no field
/// and no link: its key, or the part after the last `.`, starts with `__`
/// (for example `__all__` or `address.__all__`).
///
/// The summary renders only when the changeset has errors. Its root has
/// `id="error-summary"`, `tabindex="-1"` and `autofocus`: the browser moves
/// focus to it when the page loads, and htmx does the same after a swap. A
/// screen reader then reads the summary first. Put one summary on a page,
/// and no other `autofocus` element: the browser uses only the first one. Do
/// not return it from an inline-validation response, because it takes focus
/// while the user types.
///
/// # Example
///
/// ```rust
/// use std::collections::HashMap;
/// use autumn_web::form::Changeset;
/// use autumn_web::widgets::error_summary;
///
/// let valid = Changeset::new(());
/// assert!(error_summary(&valid).is_none());
///
/// let mut errs = HashMap::new();
/// errs.insert("email".to_string(), vec!["is invalid".to_string()]);
/// errs.insert("name".to_string(), vec!["must be <set>".to_string()]);
/// let invalid = Changeset::from_errors((), errs);
/// let out = error_summary(&invalid).unwrap().into_string();
/// assert!(out.contains(r#"class="autumn-alert autumn-alert--error""#));
/// assert!(out.contains(r##"<a href="#email">is invalid</a>"##));
/// assert!(out.contains("must be &lt;set&gt;")); // caller text escaped
/// assert!(out.contains(r#"tabindex="-1""#));
/// assert!(out.contains(" autofocus"));
/// ```
#[must_use]
pub fn error_summary<T>(changeset: &crate::form::Changeset<T>) -> Option<maud::Markup> {
    if changeset.is_valid() {
        return None;
    }
    // Stable order: sort by field so the summary is deterministic.
    let mut fields: Vec<&String> = changeset.all_errors().keys().collect();
    fields.sort();
    let body = maud::html! {
        ul class="autumn-alert__errors" {
            @for field in fields {
                @let href = field_fragment(field);
                @for msg in &changeset.all_errors()[field] {
                    @if let Some(href) = &href {
                        li { a href=(href) { (msg) } }
                    } @else {
                        li { (msg) }
                    }
                }
            }
        }
    };
    Some(alert_root(
        AlertVariant::Error,
        &body,
        &AlertConfig::new()
            .title("Please fix the following errors")
            .icon(true),
        Some(ERROR_SUMMARY_ID),
    ))
}

/// The bytes a URL fragment cannot hold (RFC 3986 `fragment`), plus `%`.
const FRAGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'!')
    .remove(b'$')
    .remove(b'&')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')')
    .remove(b'*')
    .remove(b'+')
    .remove(b',')
    .remove(b';')
    .remove(b'=')
    .remove(b':')
    .remove(b'@')
    .remove(b'/')
    .remove(b'?');

/// `#{field}`, percent-encoded for a URL fragment, or `None` for a
/// form-level key.
fn field_fragment(field: &str) -> Option<String> {
    let last = field.rsplit('.').next().unwrap_or(field);
    if field.is_empty() || last.starts_with("__") {
        return None;
    }
    Some(format!(
        "#{}",
        percent_encoding::utf8_percent_encode(field, FRAGMENT)
    ))
}

// ── toast / toast_region (#1320, #3399) ─────────────────────────────────────

/// The default id for the [`toast_region`] container, and the OOB target
/// [`toast`] appends into.
///
/// Drop `toast_region(DEFAULT_TOAST_REGION_ID)` once in your base layout;
/// `toast(msg, variant)` then appends into it out-of-band.
pub const DEFAULT_TOAST_REGION_ID: &str = "toast-region";

/// The time a toast shows before it fades out, when you do not set
/// `ToastConfig::duration`. The stylesheet default (`5s`) must match.
pub const DEFAULT_TOAST_DURATION: std::time::Duration = std::time::Duration::from_secs(5);

/// The default accessible name of the toast close button.
const DEFAULT_TOAST_CLOSE_LABEL: &str = "Dismiss notification";

/// Render the persistent, fixed-position live region that toasts append into.
///
/// Drop this **once** into your base layout (typically just before `</body>`)
/// and leave it in place: it is an **empty, persistent `aria-live="polite"`
/// region**. Appending a toast into an already-in-DOM live region is announced
/// reliably by screen readers, whereas inserting an already-populated
/// `aria-live` element is not — which is why the politeness lives here, on the
/// region, rather than on each non-error toast. `aria-atomic="false"` (and no
/// `role="status"`, which would imply `aria-atomic="true"`) means only the
/// newly-appended toast is announced, not every toast still on screen. Position
/// and stacking come from the shipped `.autumn-toast-region` rule — no inline
/// styles.
///
/// **Top layer (#3399).** The region is `popover="manual"`.
/// `autumn-widgets.js` opens it in the top layer, above a modal `<dialog>`.
/// When a modal or a popover opens later, the runtime moves the region above
/// it again. Without JS, the stylesheet shows the region as a fixed box.
///
/// **Toasts from a modal.** An open modal makes all content outside it inert.
/// A region outside the modal still shows its toasts, but screen readers do
/// not announce them, and the user cannot pause or close them. For feedback
/// from a modal, put a second region inside the `<dialog>`, and send the
/// toast there with [`toast_in`] or [`ToastConfig::region`].
///
/// Non-error toasts inherit this region's politeness; error toasts announce
/// assertively on their own via `role="alert"`. Use [`DEFAULT_TOAST_REGION_ID`]
/// unless you need more than one region; a custom id must be paired with
/// [`toast_in`] or [`ToastConfig::region`] so the OOB target matches.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-toast-region` | Fixed-position, persistent polite live region |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{toast_region, DEFAULT_TOAST_REGION_ID};
///
/// let html = toast_region(DEFAULT_TOAST_REGION_ID).into_string();
/// assert!(html.contains(r#"id="toast-region""#));
/// assert!(html.contains(r#"class="autumn-toast-region""#));
/// assert!(html.contains(r#"popover="manual""#));     // top layer
/// assert!(html.contains(r#"aria-live="polite""#));   // persistent live region
/// assert!(html.contains(r#"aria-atomic="false""#));  // announce only new toasts
/// assert!(!html.contains(r#"role="status""#));       // role=status implies atomic
/// ```
#[must_use]
pub fn toast_region(id: &str) -> maud::Markup {
    // Accept either a bare id or a `#selector` (a natural caller slip); strip a
    // leading `#` so the element id is always bare, matching the id that
    // `toast_in`'s `beforeend:#…` OOB target points at.
    let id = selector_to_id(id);
    // The region is a PERSISTENT polite live region: a `role="status"`/
    // `aria-live="polite"` element inserted already-populated is not reliably
    // announced, so instead this empty region lives in the layout and toasts are
    // appended into it (screen readers announce the mutation). `aria-atomic` is
    // explicitly `false` so only the newly-appended toast is read, not every
    // toast currently in the region — and `role="status"` is deliberately NOT
    // used because it implies `aria-atomic="true"`, which would re-announce all
    // of them. Non-error toasts inherit this politeness (they carry no own
    // `aria-live`); error toasts announce assertively on their own.
    maud::html! {
        div id=(id) class="autumn-toast-region" popover="manual" data-autumn-toast-region
            aria-live="polite" aria-atomic="false" {}
    }
}

/// Builder options for [`toast_with`].
///
/// The defaults give the same toast as [`toast`]: the default region, a
/// 5 s timer and the close label "Dismiss notification".
///
/// # Example
///
/// ```rust
/// use std::time::Duration;
/// use autumn_web::widgets::{toast_with, AlertVariant, ToastConfig};
///
/// let config = ToastConfig::new().duration(Duration::from_secs(10));
/// let out = toast_with("Report ready", AlertVariant::Info, &config).into_string();
/// assert!(out.contains(r#"data-autumn-toast-duration="10000""#));
/// ```
#[derive(Debug, Clone, Default)]
pub struct ToastConfig<'a> {
    region: Option<&'a str>,
    duration: Option<std::time::Duration>,
    persistent: bool,
    close_label: Option<&'a str>,
}

impl<'a> ToastConfig<'a> {
    /// A default config. See [`ToastConfig`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append into the region with this id, not [`DEFAULT_TOAST_REGION_ID`].
    /// A leading `#` is removed.
    #[must_use]
    pub const fn region(mut self, id: &'a str) -> Self {
        self.region = Some(id);
        self
    }

    /// Show the toast for this time before it fades out. The default is
    /// [`DEFAULT_TOAST_DURATION`].
    ///
    /// The value rounds down to whole milliseconds. A zero duration hides the
    /// toast at once. For no timer, use [`persistent`](Self::persistent).
    /// The runtime (`autumn-widgets.js`) applies this value. Without JS, the
    /// stylesheet default applies. Hover or focus pauses the timer.
    #[must_use]
    pub const fn duration(mut self, duration: std::time::Duration) -> Self {
        self.duration = Some(duration);
        self
    }

    /// When `true`, the toast has no timer. The user closes it with the
    /// close button. Use this for a message that the user must act on
    /// (WCAG 2.2.1). The duration does not apply.
    ///
    /// The close button needs `autumn-widgets.js`. Without JS, the toast
    /// stays until the page changes.
    #[must_use]
    pub const fn persistent(mut self, persistent: bool) -> Self {
        self.persistent = persistent;
        self
    }

    /// Set the accessible name of the close button. The default is
    /// "Dismiss notification".
    #[must_use]
    pub const fn close_label(mut self, label: &'a str) -> Self {
        self.close_label = Some(label);
        self
    }
}

/// Render a single transient toast that appends into the default toast region
/// out-of-band, for htmx action feedback.
///
/// This is the common case: it targets [`DEFAULT_TOAST_REGION_ID`]. Drop
/// [`toast_region`] once in your layout, then return this alongside your normal
/// swapped fragment from an htmx handler; htmx appends it into the region via
/// `hx-swap-oob="beforeend:#toast-region"` — no full page reload. The widget
/// emits **no `<script>`**.
///
/// **Timer.** The toast fades out after [`DEFAULT_TOAST_DURATION`] (a CSS
/// `@keyframes` animation). Hover or focus on a toast pauses the timer of all
/// toasts in the region. Hover also works without JS. The runtime removes a
/// toast from the DOM when its fade ends. Use [`toast_with`] to set the
/// duration or to make the toast persistent.
///
/// **Close button.** Each toast has a close button with an accessible name.
/// `autumn-widgets.js` shows it and links it to the message with
/// `aria-describedby`. After a keyboard close, focus goes to the next toast.
/// After the last toast, or after a mouse close, focus goes back to the
/// element that had focus before. Without JS, the button stays hidden.
///
/// `variant` reuses the shared [`AlertVariant`] semantic enum (the alert/badge
/// color lane) — no new color vocabulary. **Accessibility:** `Error` toasts
/// announce assertively on their own (`role="alert"`, `aria-live="assertive"`
/// on the message, so the close button is not read), which is reliably read
/// on insertion; all other variants carry **no own
/// `role`/`aria-live`** and instead inherit the persistent
/// `aria-live="polite"` of the [`toast_region`] they're appended into (an
/// already-populated polite element inserted fresh is otherwise not reliably
/// announced). Every toast carries `aria-atomic="true"` so its message is read
/// as one unit. The message is HTML-escaped by Maud.
///
/// Use [`toast_in`] instead when you rendered [`toast_region`] with a custom id.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-toast` | Root toast (carries the auto-dismiss animation) |
/// | `.autumn-toast--<variant>` | Per-variant accent (`success`/`info`/`warning`/`error`) |
/// | `.autumn-toast--persistent` | A toast with no timer |
/// | `.autumn-toast__message` | The message text |
/// | `.autumn-toast__close` | The close button |
///
/// # Example: end-to-end htmx recipe
///
/// ```rust
/// use autumn_web::widgets::{toast, toast_region, AlertVariant, DEFAULT_TOAST_REGION_ID};
/// use maud::html;
///
/// // 1. Once, in your base layout (before `</body>`) — the persistent live region:
/// //    (toast_region(DEFAULT_TOAST_REGION_ID))
/// let layout = toast_region(DEFAULT_TOAST_REGION_ID).into_string();
/// assert!(layout.contains(r#"id="toast-region""#));
/// assert!(layout.contains(r#"aria-live="polite""#));  // politeness lives here
///
/// // 2. From an htmx create/update/delete handler, return the toast next to
/// //    your swapped fragment — htmx appends it into the region OOB:
/// let response = html! {
///     (toast("Saved", AlertVariant::Success))   // hx-swap-oob append
///     tr id="row-42" { td { "…updated row…" } } // your normal swap
/// };
/// let out = response.into_string();
/// assert!(out.contains(r##"hx-swap-oob="beforeend:#toast-region""##));
/// // A non-error toast inherits the region's politeness: no own role/aria-live,
/// // but it is atomic so its message is announced as one unit.
/// assert!(out.contains(r#"aria-atomic="true""#));
/// assert!(!out.contains(r#"role="status""#));
/// assert!(out.contains(r#"aria-label="Dismiss notification""#));
/// ```
#[must_use]
pub fn toast(message: &str, variant: AlertVariant) -> maud::Markup {
    toast_with(message, variant, &ToastConfig::new())
}

/// Like [`toast`], but appends into a region with a caller-chosen id.
///
/// Use this when your layout renders [`toast_region`] with an id other than
/// [`DEFAULT_TOAST_REGION_ID`] (e.g. separate top/bottom regions). The emitted
/// `hx-swap-oob="beforeend:#<region_id>"` targets that region. `region_id` is
/// HTML-escaped in the attribute like every other caller value.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{toast_in, AlertVariant};
///
/// let out = toast_in("top-toasts", "Uploaded", AlertVariant::Info).into_string();
/// assert!(out.contains(r##"hx-swap-oob="beforeend:#top-toasts""##));
/// ```
#[must_use]
pub fn toast_in(region_id: &str, message: &str, variant: AlertVariant) -> maud::Markup {
    toast_with(message, variant, &ToastConfig::new().region(region_id))
}

/// Like [`toast`], with a [`ToastConfig`]: region, duration, persistent
/// and close label.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{toast_with, AlertVariant, ToastConfig};
///
/// // A message the user must act on: no timer, close it by hand.
/// let config = ToastConfig::new().persistent(true);
/// let out = toast_with("Payment failed", AlertVariant::Error, &config).into_string();
/// assert!(out.contains("autumn-toast--persistent"));
/// assert!(out.contains(r#"role="alert""#));
/// ```
#[must_use]
pub fn toast_with(message: &str, variant: AlertVariant, config: &ToastConfig<'_>) -> maud::Markup {
    // Error toasts announce assertively on their own — `role="alert"` elements
    // ARE reliably announced on insertion, so they don't depend on the
    // persistent region. The alert is on the message only, so the close
    // button label is not part of the announcement. Non-error toasts carry NO `role`/`aria-live`: they
    // inherit the `aria-live="polite"` of the `toast_region` they're appended
    // into (a freshly-inserted, already-populated polite element is otherwise
    // not reliably announced). Every toast keeps `aria-atomic="true"` so its
    // message is read as one unit.
    let (role, live): (Option<&str>, Option<&str>) = match variant {
        AlertVariant::Error => (Some("alert"), Some("assertive")),
        AlertVariant::Info | AlertVariant::Success | AlertVariant::Warning => (None, None),
    };
    let mut class = format!("autumn-toast autumn-toast--{}", variant.as_str());
    if config.persistent {
        class.push_str(" autumn-toast--persistent");
    }
    // A persistent toast has no timer, so it has no duration.
    let duration = config
        .duration
        .filter(|_| !config.persistent)
        .map(|d| d.as_millis().to_string());
    let close_label = config.close_label.unwrap_or(DEFAULT_TOAST_CLOSE_LABEL);
    // Accept a bare id or a `#selector`; strip one leading `#` so a caller
    // passing `"#toast-region"` doesn't produce an invalid `beforeend:##…`.
    let region_id = selector_to_id(config.region.unwrap_or(DEFAULT_TOAST_REGION_ID));
    let oob = format!("beforeend:#{region_id}");
    // For a POSITIONAL OOB swap (`beforeend:#…`) htmx inserts the carrier's
    // *children* into the target and discards the carrier itself, so the carrier
    // must be a plain, discardable `<div>` — NOT a `<template>`.
    //
    // Authority — vendored htmx 2.0.4 (`vendor/htmx.min.js`):
    //   * `oobSwap` (minified `He`): for a non-outerHTML style it swaps
    //     `f(clone)`, i.e. the *carrier element* (`f` returns the element as-is),
    //     then `_e("beforeend", …)` → `Be` → `a`, whose insert loop is
    //     `while(n.childNodes.length>0)` — it iterates the carrier's `childNodes`.
    //   * The response OOB scan `ze` matches a top-level `<template hx-swap-oob=…>`
    //     (`parentElement===null`), so the template is processed as the carrier
    //     itself, not unwrapped. A `<template>` keeps its children in `.content`,
    //     so `template.childNodes` is EMPTY → htmx appends nothing and the toast
    //     never shows. (The template-content rescan only helps when `hx-swap-oob`
    //     sits on a CHILD *inside* `.content`, which is not this positional case.)
    //
    // This mirrors the repo's own tested SSE convention in `channels.rs` (see the
    // passing `broadcast_publish_oob_custom_strategy` test →
    // `<div hx-swap-oob="beforeend:#badge"><span>3</span></div>`). The styled/ARIA
    // `.autumn-toast` element stays the carrier's CHILD; putting `hx-swap-oob`
    // directly on it would throw the wrapper away and append only the inner
    // `<span>`.
    //
    // The close button is `hidden` because it needs the runtime. The runtime
    // shows it.
    maud::html! {
        div hx-swap-oob=(oob) {
            div class=(class) data-autumn-toast data-autumn-toast-duration=[duration]
                aria-atomic="true" {
                span class="autumn-toast__message" role=[role] aria-live=[live]
                    aria-atomic=[role.map(|_| "true")] { (message) }
                button type="button" class="autumn-toast__close" data-autumn-toast-close
                    aria-label=(close_label) hidden {
                    span aria-hidden="true" { "×" }
                }
            }
        }
    }
}

// ── spinner / skeleton (#3406) ──────────────────────────────────────────────

/// The size of a [`spinner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpinnerSize {
    /// 1 rem: inline with text.
    Small,
    /// 1.5 rem. The default.
    #[default]
    Medium,
    /// 2 rem: a page or a panel that loads.
    Large,
}

impl SpinnerSize {
    /// The size class, for example `autumn-spinner--md`.
    const fn class(self) -> &'static str {
        match self {
            Self::Small => "autumn-spinner--sm",
            Self::Medium => "autumn-spinner--md",
            Self::Large => "autumn-spinner--lg",
        }
    }
}

/// Options for [`spinner_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SpinnerConfig<'a> {
    size: SpinnerSize,
    indicator: Option<&'a str>,
    class: Option<&'a str>,
}

impl<'a> SpinnerConfig<'a> {
    /// A medium spinner that always shows.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            size: SpinnerSize::Medium,
            indicator: None,
            class: None,
        }
    }

    /// Set the size.
    #[must_use]
    pub const fn size(mut self, size: SpinnerSize) -> Self {
        self.size = size;
        self
    }

    /// Make the spinner an htmx indicator with this `id`. The widget
    /// removes a leading `#`. The spinner content is hidden until htmx
    /// gives the spinner, or an element around it, the `htmx-request`
    /// class. The `role="status"` root stays. Name it in `hx-indicator`.
    #[must_use]
    pub const fn indicator(mut self, id: &'a str) -> Self {
        self.indicator = Some(id);
        self
    }

    /// Add a class to the root.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Render a loading spinner: `role="status"` with a visually hidden
/// label.
///
/// `label` is the text that a screen reader reads, for example
/// "Loading results". The ring is `aria-hidden`. With
/// `prefers-reduced-motion: reduce`, the ring does not turn. It fades in
/// and out slowly.
///
/// For a busy button, use [`ButtonConfig::loading_on_request`]. Do not
/// put a spinner in a button.
///
/// ```rust
/// use autumn_web::widgets::spinner;
///
/// let html = spinner("Loading results").into_string();
/// assert!(html.contains(r#"role="status""#));
/// assert!(html.contains("Loading results"));
/// ```
#[must_use]
#[track_caller]
pub fn spinner(label: &str) -> maud::Markup {
    spinner_with(label, &SpinnerConfig::new())
}

/// Like [`spinner`], with a [`SpinnerConfig`]: size, htmx indicator and
/// class.
///
/// ```rust
/// use autumn_web::widgets::{SpinnerConfig, spinner_with};
///
/// let config = SpinnerConfig::new().indicator("search-busy");
/// let html = spinner_with("Searching", &config).into_string();
/// assert!(html.contains(r#"id="search-busy""#));
/// assert!(html.contains("autumn-indicator"));
/// ```
#[must_use]
#[track_caller]
pub fn spinner_with(label: &str, config: &SpinnerConfig<'_>) -> maud::Markup {
    assert_named("spinner", label);
    let mut root = format!("autumn-spinner {}", config.size.class());
    if config.indicator.is_some() {
        root.push_str(" autumn-indicator");
    }
    let root = merge_class(&root, config.class);
    maud::html! {
        span id=[config.indicator.map(selector_to_id)] class=(root) role="status" {
            span class="autumn-spinner__icon" aria-hidden="true" {}
            span class="autumn-visually-hidden" { (label) }
        }
    }
}

/// The shape of a [`skeleton`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkeletonShape {
    /// A line of text. The default.
    #[default]
    Line,
    /// A block, for an image or a card.
    Block,
    /// A circle, for an avatar.
    Circle,
}

impl SkeletonShape {
    /// The shape class, for example `autumn-skeleton--line`.
    const fn class(self) -> &'static str {
        match self {
            Self::Line => "autumn-skeleton--line",
            Self::Block => "autumn-skeleton--block",
            Self::Circle => "autumn-skeleton--circle",
        }
    }
}

/// The most shapes that one [`skeleton_with`] renders. A larger
/// [`SkeletonConfig::count`], for example from a query string, renders
/// this many.
pub const MAX_SKELETON_COUNT: usize = 64;

/// Options for [`skeleton_with`].
#[derive(Debug, Clone, Copy)]
pub struct SkeletonConfig<'a> {
    count: usize,
    indicator: Option<&'a str>,
    class: Option<&'a str>,
}

impl Default for SkeletonConfig<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> SkeletonConfig<'a> {
    /// One shape that always shows.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            count: 1,
            indicator: None,
            class: None,
        }
    }

    /// Render `count` shapes. The default is 1, 0 gives 1, and the maximum
    /// is [`MAX_SKELETON_COUNT`]. Lines go in a column, and the last line
    /// is shorter. Circles go in a row.
    #[must_use]
    pub const fn count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }

    /// Make the skeleton an htmx indicator with this `id`. The widget
    /// removes a leading `#`. The skeleton is hidden until htmx gives it,
    /// or an element around it, the `htmx-request` class. Name it in
    /// `hx-indicator`.
    #[must_use]
    pub const fn indicator(mut self, id: &'a str) -> Self {
        self.indicator = Some(id);
        self
    }

    /// Add a class to the root.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Render a skeleton: a grey placeholder shape for content that loads.
///
/// The skeleton is `aria-hidden`. Tell screen readers that the content
/// loads in a different way, for example with `aria-busy` on the region
/// or with a [`spinner`]. With `prefers-reduced-motion: reduce`, the
/// skeleton does not pulse.
///
/// ```rust
/// use autumn_web::widgets::{SkeletonShape, skeleton};
///
/// let html = skeleton(SkeletonShape::Circle).into_string();
/// assert!(html.contains("autumn-skeleton--circle"));
/// assert!(html.contains(r#"aria-hidden="true""#));
/// ```
#[must_use]
pub fn skeleton(shape: SkeletonShape) -> maud::Markup {
    skeleton_with(shape, &SkeletonConfig::new())
}

/// Like [`skeleton`], with a [`SkeletonConfig`]: count, htmx indicator
/// and class.
///
/// ```rust
/// use autumn_web::widgets::{SkeletonConfig, SkeletonShape, skeleton_with};
///
/// let config = SkeletonConfig::new().count(3).indicator("posts-loading");
/// let html = skeleton_with(SkeletonShape::Line, &config).into_string();
/// assert_eq!(html.matches("autumn-skeleton__item").count(), 3);
/// assert!(html.contains(r#"id="posts-loading""#));
/// ```
#[must_use]
pub fn skeleton_with(shape: SkeletonShape, config: &SkeletonConfig<'_>) -> maud::Markup {
    let mut root = format!("autumn-skeleton {}", shape.class());
    if config.indicator.is_some() {
        root.push_str(" autumn-indicator");
    }
    let root = merge_class(&root, config.class);
    maud::html! {
        span id=[config.indicator.map(selector_to_id)] class=(root) aria-hidden="true" {
            @for _ in 0..config.count.clamp(1, MAX_SKELETON_COUNT) {
                span class="autumn-skeleton__item" {}
            }
        }
    }
}

// ── progress / meter (#3406) ────────────────────────────────────────────────

/// Options for [`progress_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ProgressConfig<'a> {
    visible_label: bool,
    value_text: Option<&'a str>,
    id: Option<&'a str>,
    class: Option<&'a str>,
}

impl<'a> ProgressConfig<'a> {
    /// A bar with no visible label.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            visible_label: false,
            value_text: None,
            id: None,
            class: None,
        }
    }

    /// Show the label and the value above the bar.
    #[must_use]
    pub const fn visible_label(mut self) -> Self {
        self.visible_label = true;
        self
    }

    /// Show `text` as the value, for example "3 of 8 files", in place of
    /// the percent. It is also the `aria-valuetext`.
    #[must_use]
    pub const fn value_text(mut self, text: &'a str) -> Self {
        self.value_text = Some(text);
        self
    }

    /// Set the `id` of the root.
    #[must_use]
    pub const fn id(mut self, id: &'a str) -> Self {
        self.id = Some(id);
        self
    }

    /// Add a class to the root.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Render a native `<progress>` bar for a task that completes.
///
/// `label` is the accessible name. `value` goes from 0 to `max`. `None`
/// gives an indeterminate bar, for work with no known end. The widget
/// clamps `value` to `0..=max`. It uses `None` for a `NaN` or infinite
/// `value`, and 1 for a `max` that is not a finite number above 0.
///
/// For a measurement in a range, for example disk use, use [`meter`].
///
/// ```rust
/// use autumn_web::widgets::progress;
///
/// let html = progress("Upload", Some(40.0), 100.0).into_string();
/// assert!(html.contains(r#"aria-label="Upload""#));
/// assert!(html.contains(r#"value="40""#));
/// assert!(html.contains(r#"max="100""#));
/// ```
#[must_use]
#[track_caller]
pub fn progress(label: &str, value: Option<f64>, max: f64) -> maud::Markup {
    progress_with(label, value, max, &ProgressConfig::new())
}

/// Like [`progress`], with a [`ProgressConfig`]: visible label, value
/// text, id and class.
///
/// ```rust
/// use autumn_web::widgets::{ProgressConfig, progress_with};
///
/// let config = ProgressConfig::new().visible_label();
/// let html = progress_with("Upload", Some(0.5), 1.0, &config).into_string();
/// assert!(html.contains("autumn-progress-field__label"));
/// assert!(html.contains("50%"));
/// ```
#[must_use]
#[track_caller]
pub fn progress_with(
    label: &str,
    value: Option<f64>,
    max: f64,
    config: &ProgressConfig<'_>,
) -> maud::Markup {
    assert_named("progress", label);
    let max = if max.is_finite() && max > 0.0 {
        max
    } else {
        1.0
    };
    let value = value.filter(|v| v.is_finite()).map(|v| v.clamp(0.0, max));
    let shown = value.map(|v| {
        config
            .value_text
            .map_or_else(|| percent(v, 0.0, max, true), str::to_owned)
    });
    let value_text = config.value_text.filter(|_| value.is_some());
    let field = config.visible_label;
    let bar_class = if field {
        "autumn-progress".to_owned()
    } else {
        merge_class("autumn-progress", config.class)
    };
    let bar = maud::html! {
        progress class=(bar_class) id=[config.id.filter(|_| !field)] aria-label=(label)
            aria-valuetext=[value_text] value=[value.map(fmt_number)] max=(fmt_number(max)) {
            @if let Some(text) = &shown { (text) }
        }
    };
    if field {
        range_field(label, shown.as_deref(), config.id, config.class, &bar)
    } else {
        bar
    }
}

/// The visible label and value above a progress bar or a meter. The text
/// is `aria-hidden`: the bar has the same name and value.
fn range_field(
    label: &str,
    value: Option<&str>,
    id: Option<&str>,
    class: Option<&str>,
    bar: &maud::Markup,
) -> maud::Markup {
    maud::html! {
        div class=(merge_class("autumn-progress-field", class)) id=[id] {
            div class="autumn-progress-field__header" aria-hidden="true" {
                span class="autumn-progress-field__label" { (label) }
                @if let Some(value) = value {
                    span class="autumn-progress-field__value" { (value) }
                }
            }
            (bar)
        }
    }
}

/// `value` as a whole percent of `min..=max`, for example `"40%"`.
/// `floor` rounds down, so a task at 99.6% does not show "100%".
fn percent(value: f64, min: f64, max: f64, floor: bool) -> String {
    // Halves: `max - min` can overflow to infinity near `f64::MAX`.
    let span = max / 2.0 - min / 2.0;
    let ratio = if span > 0.0 {
        (value / 2.0 - min / 2.0) / span
    } else {
        1.0
    };
    let pct = ratio * 100.0;
    // 1e-9 absorbs float error: 29.0 / 100.0 * 100.0 is 28.999…, not 29.
    let pct = if floor {
        (pct + 1e-9).floor()
    } else {
        pct.round()
    };
    format!("{}%", fmt_number(pct))
}

/// A finite number as an attribute value: `40` not `40.0`, `0` not `-0`.
fn fmt_number(value: f64) -> String {
    // Adding 0.0 turns -0.0 into 0.0.
    format!("{}", value + 0.0)
}

/// Options for [`meter_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct MeterConfig<'a> {
    low: Option<f64>,
    high: Option<f64>,
    optimum: Option<f64>,
    visible_label: bool,
    value_text: Option<&'a str>,
    id: Option<&'a str>,
    class: Option<&'a str>,
}

impl<'a> MeterConfig<'a> {
    /// A meter with no `low`, `high` or `optimum` and no visible label.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            low: None,
            high: None,
            optimum: None,
            visible_label: false,
            value_text: None,
            id: None,
            class: None,
        }
    }

    /// Set `low`: the top of the low part of the range.
    #[must_use]
    pub const fn low(mut self, low: f64) -> Self {
        self.low = Some(low);
        self
    }

    /// Set `high`: the bottom of the high part of the range.
    #[must_use]
    pub const fn high(mut self, high: f64) -> Self {
        self.high = Some(high);
        self
    }

    /// Set `optimum`: the best value. With `low` and `high`, the browser
    /// colors the bar good, average or bad.
    #[must_use]
    pub const fn optimum(mut self, optimum: f64) -> Self {
        self.optimum = Some(optimum);
        self
    }

    /// Show the label and the value above the bar.
    #[must_use]
    pub const fn visible_label(mut self) -> Self {
        self.visible_label = true;
        self
    }

    /// Show `text` as the value, for example "7 GB of 10 GB", in place of
    /// the percent. It is also the `aria-valuetext`.
    #[must_use]
    pub const fn value_text(mut self, text: &'a str) -> Self {
        self.value_text = Some(text);
        self
    }

    /// Set the `id` of the root.
    #[must_use]
    pub const fn id(mut self, id: &'a str) -> Self {
        self.id = Some(id);
        self
    }

    /// Add a class to the root.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Render a native `<meter>`: a measurement in a known range, for example
/// disk use or a score.
///
/// `label` is the accessible name. The widget clamps `value`, `low`,
/// `high` and `optimum` to `range`. If the start is above the end, the
/// widget swaps them. A `NaN` or infinite start becomes 0, and a `NaN` or
/// infinite end becomes 1. A `NaN` or infinite `value` becomes the start.
///
/// For a task that completes, use [`progress`].
///
/// ```rust
/// use autumn_web::widgets::meter;
///
/// let html = meter("Disk use", 70.0, 0.0..=100.0).into_string();
/// assert!(html.contains(r#"aria-label="Disk use""#));
/// assert!(html.contains(r#"value="70""#));
/// ```
#[must_use]
#[track_caller]
pub fn meter(label: &str, value: f64, range: std::ops::RangeInclusive<f64>) -> maud::Markup {
    meter_with(label, value, range, &MeterConfig::new())
}

/// Like [`meter`], with a [`MeterConfig`]: `low`, `high`, `optimum`,
/// visible label, value text, id and class.
///
/// ```rust
/// use autumn_web::widgets::{MeterConfig, meter_with};
///
/// let config = MeterConfig::new().low(25.0).high(75.0).optimum(10.0);
/// let html = meter_with("Disk use", 90.0, 0.0..=100.0, &config).into_string();
/// assert!(html.contains(r#"high="75""#));
/// ```
#[must_use]
#[track_caller]
pub fn meter_with(
    label: &str,
    value: f64,
    range: std::ops::RangeInclusive<f64>,
    config: &MeterConfig<'_>,
) -> maud::Markup {
    assert_named("meter", label);
    let (start, end) = range.into_inner();
    let start = if start.is_finite() { start } else { 0.0 };
    let end = if end.is_finite() { end } else { 1.0 };
    let (min, max) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    let value = if value.is_finite() {
        value.clamp(min, max)
    } else {
        min
    };
    let clamp = |x: Option<f64>| {
        x.filter(|x| x.is_finite())
            .map(|x| fmt_number(x.clamp(min, max)))
    };
    let shown = config
        .value_text
        .map_or_else(|| percent(value, min, max, false), str::to_owned);
    let field = config.visible_label;
    let bar_class = if field {
        "autumn-meter".to_owned()
    } else {
        merge_class("autumn-meter", config.class)
    };
    let bar = maud::html! {
        meter class=(bar_class) id=[config.id.filter(|_| !field)] aria-label=(label)
            aria-valuetext=[config.value_text] value=(fmt_number(value))
            min=(fmt_number(min)) max=(fmt_number(max))
            low=[clamp(config.low)] high=[clamp(config.high)] optimum=[clamp(config.optimum)] {
            (shown)
        }
    };
    if field {
        range_field(label, Some(&shown), config.id, config.class, &bar)
    } else {
        bar
    }
}

// ── empty_state (#3406) ─────────────────────────────────────────────────────

/// Options for [`empty_state_with`].
#[derive(Debug, Clone, Default)]
pub struct EmptyStateConfig<'a> {
    icon: Option<maud::Markup>,
    level: HeadingLevel,
    class: Option<&'a str>,
}

impl<'a> EmptyStateConfig<'a> {
    /// An `<h2>` title, with no icon.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            icon: None,
            level: HeadingLevel::H2,
            class: None,
        }
    }

    /// Show `icon` above the title. The icon is `aria-hidden`.
    #[must_use]
    pub fn icon(mut self, icon: maud::Markup) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Set the heading level of the title (default [`HeadingLevel::H2`]).
    /// Use one level below the heading that holds the empty state.
    #[must_use]
    pub const fn level(mut self, level: HeadingLevel) -> Self {
        self.level = level;
        self
    }

    /// Add a class to the root.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Render an empty state: a title, a short text and an optional action,
/// for a list or a page with no data yet.
///
/// The title is a heading. Do not give an empty title: a debug build
/// panics. An empty `body` gives no paragraph. Give an
/// action that lets the user fix the empty state, for example a link to
/// create the first item.
///
/// ```rust
/// use autumn_web::links::link_to;
/// use autumn_web::widgets::empty_state;
///
/// let action = link_to("New post", "/posts/new");
/// let html = empty_state("No posts yet", "Write your first post.", Some(&action))
///     .into_string();
/// assert!(html.contains(r#"<h2 class="autumn-empty-state__title">No posts yet</h2>"#));
/// assert!(html.contains("autumn-empty-state__action"));
/// ```
#[must_use]
#[track_caller]
pub fn empty_state(title: &str, body: &str, action: Option<&maud::Markup>) -> maud::Markup {
    empty_state_with(title, body, action, &EmptyStateConfig::new())
}

/// Like [`empty_state`], with an [`EmptyStateConfig`]: icon, heading level
/// and class.
#[must_use]
#[track_caller]
pub fn empty_state_with(
    title: &str,
    body: &str,
    action: Option<&maud::Markup>,
    config: &EmptyStateConfig<'_>,
) -> maud::Markup {
    assert_named("empty_state", title);
    let root = merge_class("autumn-empty-state", config.class);
    maud::html! {
        div class=(root) {
            @if let Some(icon) = &config.icon {
                div class="autumn-empty-state__icon" aria-hidden="true" { (icon) }
            }
            (heading(config.level, None, "autumn-empty-state__title", &maud::html! { (title) }))
            @if !body.trim().is_empty() {
                p class="autumn-empty-state__body" { (body) }
            }
            @if let Some(action) = action {
                div class="autumn-empty-state__action" { (action) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maud::html;

    #[test]
    fn skeleton_count_is_capped() {
        let config = SkeletonConfig::new().count(usize::MAX);
        let html = skeleton_with(SkeletonShape::Line, &config).into_string();
        assert_eq!(
            html.matches("autumn-skeleton__item").count(),
            MAX_SKELETON_COUNT
        );
    }

    #[test]
    fn percent_rounds_progress_down_and_survives_huge_ranges() {
        let config = ProgressConfig::new().visible_label();
        let html = progress_with("Upload", Some(99.6), 100.0, &config).into_string();
        assert!(html.contains(">99%</span>"), "{html}");
        // Codex review: float error must not round a whole percent down.
        for value in [29.0, 57.0, 58.0] {
            let html = progress_with("Upload", Some(value), 100.0, &config).into_string();
            assert!(html.contains(&format!(">{value}%</span>")), "{html}");
        }
        let config = MeterConfig::new().visible_label();
        let html = meter_with("Huge", 0.0, -f64::MAX..=f64::MAX, &config).into_string();
        assert!(html.contains(">50%</span>"), "{html}");
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "accessible name")]
    fn empty_state_with_a_blank_title_panics_in_debug() {
        let _ = empty_state(" ", "body", None);
    }

    // ── spinner ─────────────────────────────────────────────────────────

    #[test]
    fn spinner_is_a_status_with_hidden_text() {
        let html = spinner("Loading results").into_string();
        assert_eq!(
            html,
            concat!(
                r#"<span class="autumn-spinner autumn-spinner--md" role="status">"#,
                r#"<span class="autumn-spinner__icon" aria-hidden="true"></span>"#,
                r#"<span class="autumn-visually-hidden">Loading results</span></span>"#,
            )
        );
    }

    #[test]
    fn spinner_size_indicator_and_class() {
        for (size, class) in [
            (SpinnerSize::Small, "autumn-spinner--sm"),
            (SpinnerSize::Large, "autumn-spinner--lg"),
        ] {
            let html = spinner_with("Wait", &SpinnerConfig::new().size(size)).into_string();
            assert!(html.contains(class), "{html}");
        }
        let config = SpinnerConfig::new().indicator("#busy").class("inline");
        let html = spinner_with("Wait", &config).into_string();
        assert!(html.contains(r#"id="busy""#), "{html}");
        assert!(
            html.contains(r#"class="autumn-spinner autumn-spinner--md autumn-indicator inline""#),
            "{html}"
        );
        assert!(!spinner("Wait").into_string().contains("autumn-indicator"));
    }

    // ── skeleton ────────────────────────────────────────────────────────

    #[test]
    fn skeleton_is_hidden_from_screen_readers() {
        let html = skeleton(SkeletonShape::Line).into_string();
        assert_eq!(
            html,
            concat!(
                r#"<span class="autumn-skeleton autumn-skeleton--line" aria-hidden="true">"#,
                r#"<span class="autumn-skeleton__item"></span></span>"#,
            )
        );
    }

    #[test]
    fn skeleton_shapes_count_and_indicator() {
        for (shape, class) in [
            (SkeletonShape::Block, "autumn-skeleton--block"),
            (SkeletonShape::Circle, "autumn-skeleton--circle"),
        ] {
            assert!(skeleton(shape).into_string().contains(class));
        }
        let three = SkeletonConfig::new().count(3);
        let html = skeleton_with(SkeletonShape::Line, &three).into_string();
        assert_eq!(html.matches("autumn-skeleton__item").count(), 3, "{html}");
        let zero = SkeletonConfig::new().count(0);
        let html = skeleton_with(SkeletonShape::Line, &zero).into_string();
        assert_eq!(html.matches("autumn-skeleton__item").count(), 1, "{html}");

        let config = SkeletonConfig::new()
            .indicator("#feed-loading")
            .class("wide");
        let html = skeleton_with(SkeletonShape::Block, &config).into_string();
        assert!(html.contains(r#"id="feed-loading""#), "{html}");
        assert!(
            html.contains(
                r#"class="autumn-skeleton autumn-skeleton--block autumn-indicator wide""#
            ),
            "{html}"
        );
    }

    // ── progress ────────────────────────────────────────────────────────

    #[test]
    fn progress_is_a_named_native_bar() {
        let html = progress("Upload", Some(40.0), 100.0).into_string();
        assert_eq!(
            html,
            r#"<progress class="autumn-progress" aria-label="Upload" value="40" max="100">40%</progress>"#
        );
    }

    #[test]
    fn progress_with_no_value_is_indeterminate() {
        for value in [None, Some(f64::NAN), Some(f64::INFINITY)] {
            let html = progress("Loading", value, 100.0).into_string();
            assert!(!html.contains("value="), "{value:?}: {html}");
            assert!(html.contains(r#"max="100""#), "{html}");
            assert!(html.ends_with("></progress>"), "{html}");
        }
    }

    #[test]
    fn progress_clamps_the_value_and_fixes_a_bad_max() {
        let html = progress("Upload", Some(150.0), 100.0).into_string();
        assert!(html.contains(r#"value="100""#), "{html}");
        let html = progress("Upload", Some(-3.0), 100.0).into_string();
        assert!(html.contains(r#"value="0""#), "{html}");
        assert!(!html.contains("-0"), "{html}");
        for max in [0.0, -5.0, f64::NAN, f64::INFINITY] {
            let html = progress("Upload", Some(0.5), max).into_string();
            assert!(html.contains(r#"max="1""#), "{max}: {html}");
            assert!(html.contains(r#"value="0.5""#), "{max}: {html}");
        }
    }

    #[test]
    fn progress_visible_label_shows_label_and_value() {
        let config = ProgressConfig::new().visible_label().id("up").class("big");
        let html = progress_with("Upload", Some(1.0), 4.0, &config).into_string();
        assert!(
            html.starts_with(r#"<div class="autumn-progress-field big" id="up">"#),
            "{html}"
        );
        assert!(
            html.contains(concat!(
                r#"<div class="autumn-progress-field__header" aria-hidden="true">"#,
                r#"<span class="autumn-progress-field__label">Upload</span>"#,
                r#"<span class="autumn-progress-field__value">25%</span></div>"#,
            )),
            "{html}"
        );
        // The bar keeps its own name: the header is aria-hidden.
        assert!(html.contains(r#"aria-label="Upload""#), "{html}");
    }

    #[test]
    fn progress_value_text_replaces_the_percent() {
        let config = ProgressConfig::new()
            .visible_label()
            .value_text("3 of 8 files");
        let html = progress_with("Upload", Some(3.0), 8.0, &config).into_string();
        assert!(html.contains(r#"aria-valuetext="3 of 8 files""#), "{html}");
        assert!(html.contains(">3 of 8 files</span>"), "{html}");
        assert!(!html.contains("38%"), "{html}");
        // An indeterminate bar has no value, so no value text.
        let html = progress_with("Upload", None, 8.0, &config).into_string();
        assert!(!html.contains("aria-valuetext"), "{html}");
    }

    #[test]
    fn progress_without_visible_label_puts_id_and_class_on_the_bar() {
        let config = ProgressConfig::new().id("up").class("thin");
        let html = progress_with("Upload", Some(0.2), 1.0, &config).into_string();
        assert!(
            html.starts_with(r#"<progress class="autumn-progress thin" id="up""#),
            "{html}"
        );
    }

    // ── meter ───────────────────────────────────────────────────────────

    #[test]
    fn meter_is_a_named_native_meter() {
        let html = meter("Disk use", 70.0, 0.0..=100.0).into_string();
        assert_eq!(
            html,
            r#"<meter class="autumn-meter" aria-label="Disk use" value="70" min="0" max="100">70%</meter>"#
        );
    }

    #[test]
    fn meter_thresholds_are_clamped_to_the_range() {
        let config = MeterConfig::new().low(-10.0).high(80.0).optimum(500.0);
        let html = meter_with("Score", 5.0, 0.0..=10.0, &config).into_string();
        assert!(html.contains(r#"low="0""#), "{html}");
        assert!(html.contains(r#"high="10""#), "{html}");
        assert!(html.contains(r#"optimum="10""#), "{html}");
        let nan = MeterConfig::new().low(f64::NAN);
        let html = meter_with("Score", 5.0, 0.0..=10.0, &nan).into_string();
        assert!(!html.contains("low="), "{html}");
    }

    #[test]
    fn meter_fixes_a_reversed_or_bad_range_and_value() {
        #[allow(clippy::reversed_empty_ranges)]
        let html = meter("Score", 5.0, 10.0..=0.0).into_string();
        assert!(html.contains(r#"min="0" max="10""#), "{html}");
        let html = meter("Score", f64::NAN, 2.0..=4.0).into_string();
        assert!(html.contains(r#"value="2""#), "{html}");
        let html = meter("Score", 9.0, f64::NAN..=f64::INFINITY).into_string();
        assert!(html.contains(r#"value="1" min="0" max="1""#), "{html}");
    }

    #[test]
    fn meter_visible_label_and_value_text() {
        let config = MeterConfig::new()
            .visible_label()
            .value_text("7 GB of 10 GB")
            .id("disk");
        let html = meter_with("Disk use", 7.0, 0.0..=10.0, &config).into_string();
        assert!(
            html.starts_with(r#"<div class="autumn-progress-field" id="disk">"#),
            "{html}"
        );
        assert!(html.contains(">7 GB of 10 GB</span>"), "{html}");
        assert!(html.contains(r#"aria-valuetext="7 GB of 10 GB""#), "{html}");
    }

    // ── empty_state ─────────────────────────────────────────────────────

    #[test]
    fn empty_state_has_title_body_and_action() {
        let action = html! { a href="/posts/new" { "New post" } };
        let html =
            empty_state("No posts yet", "Write your first post.", Some(&action)).into_string();
        assert_eq!(
            html,
            concat!(
                r#"<div class="autumn-empty-state">"#,
                r#"<h2 class="autumn-empty-state__title">No posts yet</h2>"#,
                r#"<p class="autumn-empty-state__body">Write your first post.</p>"#,
                r#"<div class="autumn-empty-state__action"><a href="/posts/new">New post</a></div>"#,
                "</div>",
            )
        );
    }

    #[test]
    fn empty_state_omits_an_empty_body_and_no_action() {
        let html = empty_state("Nothing here", "  ", None).into_string();
        assert!(!html.contains("autumn-empty-state__body"), "{html}");
        assert!(!html.contains("autumn-empty-state__action"), "{html}");
    }

    #[test]
    fn empty_state_icon_level_and_class() {
        let config = EmptyStateConfig::new()
            .icon(html! { svg {} })
            .level(HeadingLevel::H3)
            .class("card");
        let html = empty_state_with("No <b>data</b>", "x", None, &config).into_string();
        assert!(
            html.starts_with(concat!(
                r#"<div class="autumn-empty-state card">"#,
                r#"<div class="autumn-empty-state__icon" aria-hidden="true"><svg></svg></div>"#,
            )),
            "{html}"
        );
        assert!(
            html.contains(
                r#"<h3 class="autumn-empty-state__title">No &lt;b&gt;data&lt;/b&gt;</h3>"#
            ),
            "{html}"
        );
    }
}
