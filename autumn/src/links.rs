//! `link_to` / `button_to` — safe, method-aware link helpers (issue #1138).
//!
//! Autumn ships the pieces of a safe action link — [`crate::form::method_input`]
//! for `_method` overrides and the [`crate::security::CsrfToken`] plumbing —
//! but composing them by hand for every "Delete" button invites a forgotten
//! CSRF field. These helpers close that gap:
//!
//! - [`link_to`] / [`link_to_with`] render a plain `<a>` anchor for GET
//!   navigation, HTML-escaping both label and href, and automatically adding
//!   `rel="noopener"` when `target="_blank"` is requested.
//! - [`button_to`] / [`button_to_with`] render a single-button `<form>` for
//!   state-changing actions: the browser transport is `POST`, a hidden
//!   `_method` input (via [`crate::form::method_input`]) upgrades it to
//!   `PUT`/`PATCH`/`DELETE` for the
//!   [`MethodOverrideLayer`](crate::middleware::MethodOverrideLayer), and a
//!   hidden CSRF input is always emitted. The CSRF token is a **required**
//!   positional argument, so a state-changing `button_to` that silently
//!   omits the CSRF field is unrepresentable.
//!
//! Both helpers accept extra attributes (e.g. htmx `hx-*`) through their
//! options structs, so a `button_to("Delete", path, Method::DELETE, token)`
//! can be upgraded to an `hx-delete` interaction without abandoning the
//! helper.
//!
//! For multi-field forms, use [`crate::form::form_tag`] or
//! [`ChangesetForm::form_tag`](crate::form::ChangesetForm::form_tag) instead —
//! `button_to` is a zero-field action button, not a form builder.

use http::Method;

/// Returns `true` when `name` is a valid HTML attribute name: an ASCII
/// letter followed by ASCII alphanumerics, `-`, `_`, or `:`.
#[cfg(feature = "maud")]
fn is_valid_attr_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':')
}

/// Append `text`, HTML-escaped (`&`, `<`, `>`), to `out`.
#[cfg(feature = "maud")]
fn push_escaped_text(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
}

/// Append `text`, HTML-escaped for use inside a double-quoted attribute
/// value (`&`, `<`, `>`, `"`, `'`), to `out`.
#[cfg(feature = "maud")]
fn push_escaped_attr_value(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
}

/// Append a ` name="escaped-value"` attribute to `out`.
///
/// # Panics
///
/// Panics when `name` is not a valid HTML attribute name — see
/// [`is_valid_attr_name`].
#[cfg(feature = "maud")]
fn push_attr(out: &mut String, name: &str, value: &str) {
    assert!(
        is_valid_attr_name(name),
        "invalid HTML attribute name {name:?} passed to link_to/button_to"
    );
    out.push(' ');
    out.push_str(name);
    out.push_str("=\"");
    push_escaped_attr_value(out, value);
    out.push('"');
}

/// Options for [`link_to_with`]: extra class, `target`, `rel`, and arbitrary
/// additional attributes (e.g. htmx `hx-*`).
///
/// Build with [`LinkToOptions::new`] and chain the setters.
///
/// # Example
///
/// ```rust
/// use autumn_web::links::LinkToOptions;
///
/// let options = LinkToOptions::new()
///     .class("btn")
///     .target("_blank")
///     .attrs(&[("hx-boost", "true")]);
/// ```
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, Default)]
pub struct LinkToOptions<'a> {
    /// `class` attribute for the anchor.
    class: Option<&'a str>,
    /// `target` attribute; `"_blank"` triggers the automatic `rel="noopener"`.
    target: Option<&'a str>,
    /// `rel` attribute. `noopener` is appended when `target="_blank"` is set
    /// and the value does not already contain it.
    rel: Option<&'a str>,
    /// Extra attributes rendered on the anchor, e.g. `hx-*` or `data-*`.
    attrs: &'a [(&'a str, &'a str)],
}

#[cfg(feature = "maud")]
impl<'a> LinkToOptions<'a> {
    /// Create empty link options.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            class: None,
            target: None,
            rel: None,
            attrs: &[],
        }
    }

    /// Set the anchor's `class` attribute.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }

    /// Set the anchor's `target` attribute. `"_blank"` automatically adds
    /// `rel="noopener"` unless the configured `rel` already contains it.
    #[must_use]
    pub const fn target(mut self, target: &'a str) -> Self {
        self.target = Some(target);
        self
    }

    /// Set the anchor's `rel` attribute.
    #[must_use]
    pub const fn rel(mut self, rel: &'a str) -> Self {
        self.rel = Some(rel);
        self
    }

    /// Set extra attributes rendered on the anchor (e.g. `hx-get`,
    /// `hx-target`, `data-*`). Values are HTML-escaped; names must be valid
    /// attribute names (see [`link_to_with`] panics).
    #[must_use]
    pub const fn attrs(mut self, attrs: &'a [(&'a str, &'a str)]) -> Self {
        self.attrs = attrs;
        self
    }
}

/// Options for [`button_to_with`]: button/form classes, a custom CSRF field
/// name, and arbitrary additional attributes on the `<button>`.
///
/// Build with [`ButtonToOptions::new`] and chain the setters.
///
/// # Example
///
/// ```rust
/// use autumn_web::links::ButtonToOptions;
///
/// let options = ButtonToOptions::new()
///     .class("btn btn-danger")
///     .attrs(&[("hx-confirm", "Are you sure?")]);
/// ```
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, Default)]
pub struct ButtonToOptions<'a> {
    /// `class` attribute for the `<button>`.
    class: Option<&'a str>,
    /// `class` attribute for the wrapping `<form>` (layout, e.g. inline).
    form_class: Option<&'a str>,
    /// CSRF form field name; defaults to `"_csrf"`. Pass the configured
    /// [`CsrfFormField`](crate::security::CsrfFormField) value when
    /// `security.csrf.form_field` is customized.
    csrf_field: Option<&'a str>,
    /// Extra attributes rendered on the `<button>`, e.g. `hx-delete`,
    /// `hx-confirm`, `data-*`.
    attrs: &'a [(&'a str, &'a str)],
}

#[cfg(feature = "maud")]
impl<'a> ButtonToOptions<'a> {
    /// Create empty button options (default CSRF field name `"_csrf"`).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            class: None,
            form_class: None,
            csrf_field: None,
            attrs: &[],
        }
    }

    /// Set the `<button>`'s `class` attribute.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }

    /// Set the wrapping `<form>`'s `class` attribute (e.g. an inline-display
    /// class so the button sits in a row of actions).
    #[must_use]
    pub const fn form_class(mut self, form_class: &'a str) -> Self {
        self.form_class = Some(form_class);
        self
    }

    /// Override the CSRF hidden-input field name (default `"_csrf"`).
    #[must_use]
    pub const fn csrf_field(mut self, csrf_field: &'a str) -> Self {
        self.csrf_field = Some(csrf_field);
        self
    }

    /// Set extra attributes rendered on the `<button>` (e.g. `hx-delete`,
    /// `hx-confirm`, `data-*`). Values are HTML-escaped; names must be valid
    /// attribute names (see [`button_to_with`] panics).
    #[must_use]
    pub const fn attrs(mut self, attrs: &'a [(&'a str, &'a str)]) -> Self {
        self.attrs = attrs;
        self
    }
}

/// Render a GET `<a>` anchor with HTML-escaped label and href.
///
/// Use [`link_to_with`] for extra attributes (`class`, `target`, `rel`,
/// `hx-*`), and [`button_to`] for state-changing (non-GET) actions.
///
/// # Example
///
/// ```rust
/// use autumn_web::links::link_to;
///
/// let html = link_to("Posts", "/posts").into_string();
/// assert_eq!(html, r#"<a href="/posts">Posts</a>"#);
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn link_to(label: &str, href: &str) -> maud::Markup {
    link_to_with(label, href, &LinkToOptions::new())
}

/// Render a GET `<a>` anchor with [`LinkToOptions`] — class, `target`,
/// `rel`, and arbitrary extra attributes such as htmx `hx-*`.
///
/// Label, href, and all attribute values are HTML-escaped. When
/// `target="_blank"` is set, `rel="noopener"` is added automatically (merged
/// into any configured `rel` without duplication).
///
/// # Panics
///
/// Panics when an extra attribute name is not a valid HTML attribute name
/// (must start with an ASCII letter, followed by ASCII alphanumerics, `-`,
/// `_`, or `:`). Attribute names are developer-authored literals; a panic
/// surfaces the typo instead of silently emitting broken markup.
///
/// # Example
///
/// ```rust
/// use autumn_web::links::{LinkToOptions, link_to_with};
///
/// let html = link_to_with(
///     "Preview",
///     "/posts/1",
///     &LinkToOptions::new()
///         .class("btn")
///         .attrs(&[("hx-get", "/posts/1/preview"), ("hx-target", "#preview")]),
/// )
/// .into_string();
/// assert!(html.contains(r#"hx-get="/posts/1/preview""#));
/// assert!(html.contains(r#"class="btn""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn link_to_with(label: &str, href: &str, options: &LinkToOptions<'_>) -> maud::Markup {
    let rel = effective_rel(options.target, options.rel);

    let mut out = String::from("<a");
    push_attr(&mut out, "href", href);
    if let Some(class) = options.class {
        push_attr(&mut out, "class", class);
    }
    if let Some(target) = options.target {
        push_attr(&mut out, "target", target);
    }
    if let Some(rel) = rel.as_deref() {
        push_attr(&mut out, "rel", rel);
    }
    for (name, value) in options.attrs {
        push_attr(&mut out, name, value);
    }
    out.push('>');
    push_escaped_text(&mut out, label);
    out.push_str("</a>");

    maud::PreEscaped(out)
}

/// Compute the effective `rel` value for a link: when `target="_blank"`,
/// `noopener` is appended to the caller's `rel` (space-separated, no
/// duplicate) or used on its own.
#[cfg(feature = "maud")]
fn effective_rel(target: Option<&str>, rel: Option<&str>) -> Option<String> {
    if target != Some("_blank") {
        return rel.map(ToString::to_string);
    }
    match rel {
        Some(rel) if rel.split_whitespace().any(|token| token == "noopener") => {
            Some(rel.to_string())
        }
        Some(rel) => Some(format!("{rel} noopener")),
        None => Some("noopener".to_string()),
    }
}

/// Render a state-changing action button: a single-button `<form>` carrying
/// a hidden `_method` override (for `PUT`/`PATCH`/`DELETE`) and a hidden
/// CSRF token input.
///
/// The CSRF token is a required argument — pass
/// [`CsrfToken::token()`](crate::security::CsrfToken::token) from the
/// extractor — so a state-changing button cannot silently omit the CSRF
/// field. Use [`button_to_with`] for classes, a custom CSRF field name, or
/// extra (`hx-*`) attributes.
///
/// `Method::GET` renders a plain `<form method="get">` **without** `_method`
/// or CSRF inputs (GET must not mutate state; the token argument is ignored).
///
/// # Example
///
/// ```rust
/// use autumn_web::links::button_to;
/// use http::Method;
///
/// let html = button_to("Log out", "/logout", Method::POST, "tok123").into_string();
/// assert!(html.contains(r#"method="post""#));
/// assert!(html.contains(r#"name="_csrf" value="tok123""#));
/// assert!(html.contains(r#"<button type="submit">Log out</button>"#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn button_to(label: &str, href: &str, method: Method, csrf_token: &str) -> maud::Markup {
    button_to_with(label, href, method, csrf_token, &ButtonToOptions::new())
}

/// Render a state-changing action button with [`ButtonToOptions`] — button
/// and form classes, custom CSRF field name, and arbitrary extra attributes
/// (e.g. htmx `hx-*`) on the `<button>`.
///
/// For any method other than `GET`, the form's browser transport is `POST`:
/// a hidden `_method` input (via [`crate::form::method_input`]) carries the
/// declared verb for `PUT`/`PATCH`/`DELETE`, and a hidden CSRF input (field
/// name from [`ButtonToOptions::csrf_field`], default `"_csrf"`) is always
/// emitted. `Method::GET` renders a plain GET form without `_method` or CSRF
/// inputs (the token argument is ignored).
///
/// Label, action, and all attribute values are HTML-escaped.
///
/// # Panics
///
/// Panics when an extra attribute name is not a valid HTML attribute name
/// (must start with an ASCII letter, followed by ASCII alphanumerics, `-`,
/// `_`, or `:`). Attribute names are developer-authored literals; a panic
/// surfaces the typo instead of silently emitting broken markup.
///
/// # Example
///
/// The admin-style htmx delete button — `hx-delete` handles the interaction
/// when JavaScript is available, while the rendered form (`POST` +
/// `_method=DELETE` + CSRF) keeps working without it:
///
/// ```rust
/// use autumn_web::links::{ButtonToOptions, button_to_with};
/// use http::Method;
///
/// let html = button_to_with(
///     "Delete",
///     "/admin/widgets/42",
///     Method::DELETE,
///     "tok123",
///     &ButtonToOptions::new()
///         .class("btn btn-danger")
///         .attrs(&[
///             ("hx-delete", "/admin/widgets/42"),
///             ("hx-confirm", "Are you sure?"),
///             ("hx-target", "body"),
///         ]),
/// )
/// .into_string();
/// assert!(html.contains(r#"name="_method" value="DELETE""#));
/// assert!(html.contains(r#"name="_csrf" value="tok123""#));
/// assert!(html.contains(r#"hx-delete="/admin/widgets/42""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn button_to_with(
    label: &str,
    href: &str,
    method: Method,
    csrf_token: &str,
    options: &ButtonToOptions<'_>,
) -> maud::Markup {
    let mut button = String::from("<button type=\"submit\"");
    if let Some(class) = options.class {
        push_attr(&mut button, "class", class);
    }
    for (name, value) in options.attrs {
        push_attr(&mut button, name, value);
    }
    button.push('>');
    push_escaped_text(&mut button, label);
    button.push_str("</button>");
    let button = maud::PreEscaped(button);

    let csrf_field = options.csrf_field.unwrap_or("_csrf");

    if method == Method::GET {
        maud::html! {
            form action=(href) method="get" class=[options.form_class] {
                (button)
            }
        }
    } else {
        maud::html! {
            form action=(href) method="post" class=[options.form_class] {
                (crate::form::method_input(method.as_str()))
                input type="hidden" name=(csrf_field) value=(csrf_token);
                (button)
            }
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "maud"))]
mod tests {
    use super::*;

    // ── link_to ─────────────────────────────────────────────────────────

    #[test]
    fn link_to_renders_basic_anchor() {
        let html = link_to("Posts", "/posts").into_string();
        assert_eq!(html, r#"<a href="/posts">Posts</a>"#);
    }

    #[test]
    fn link_to_escapes_label() {
        let html = link_to("<script>alert(1)</script>", "/posts").into_string();
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn link_to_escapes_href() {
        let html = link_to("x", r#"/x?a=1&b="><script>"#).into_string();
        assert!(html.contains("&amp;"), "{html}");
        assert!(html.contains("&quot;"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains(r#"b=">"#), "{html}");
    }

    #[test]
    fn link_to_with_class_target_rel() {
        let options = LinkToOptions::new()
            .class("btn btn-link")
            .target("_top")
            .rel("nofollow");
        let html = link_to_with("Docs", "/docs", &options).into_string();
        assert!(html.contains(r#"class="btn btn-link""#), "{html}");
        assert!(html.contains(r#"target="_top""#), "{html}");
        assert!(html.contains(r#"rel="nofollow""#), "{html}");
    }

    #[test]
    fn link_to_target_blank_adds_noopener() {
        let options = LinkToOptions::new().target("_blank");
        let html = link_to_with("Ext", "https://example.com", &options).into_string();
        assert!(html.contains(r#"rel="noopener""#), "{html}");
    }

    #[test]
    fn link_to_target_blank_merges_rel() {
        let options = LinkToOptions::new().target("_blank").rel("nofollow");
        let html = link_to_with("Ext", "https://example.com", &options).into_string();
        assert!(html.contains(r#"rel="nofollow noopener""#), "{html}");
    }

    #[test]
    fn link_to_target_blank_no_duplicate_noopener() {
        let options = LinkToOptions::new()
            .target("_blank")
            .rel("noopener external");
        let html = link_to_with("Ext", "https://example.com", &options).into_string();
        assert_eq!(html.matches("noopener").count(), 1, "{html}");
        assert!(html.contains(r#"rel="noopener external""#), "{html}");
    }

    #[test]
    fn link_to_no_target_no_auto_rel() {
        let html = link_to("Posts", "/posts").into_string();
        assert!(!html.contains("rel="), "{html}");
    }

    #[test]
    fn link_to_extra_hx_attrs() {
        let options =
            LinkToOptions::new().attrs(&[("hx-get", "/posts/1"), ("hx-target", "#detail")]);
        let html = link_to_with("Show", "/posts/1", &options).into_string();
        assert!(html.contains(r#"hx-get="/posts/1""#), "{html}");
        assert!(html.contains(r##"hx-target="#detail""##), "{html}");
    }

    #[test]
    fn link_to_extra_attr_value_escaped() {
        let options = LinkToOptions::new().attrs(&[("data-note", r#"x" onmouseover="evil()"#)]);
        let html = link_to_with("x", "/x", &options).into_string();
        assert!(html.contains("&quot;"), "{html}");
        assert!(!html.contains(r#"" onmouseover"#), "{html}");
    }

    #[test]
    #[should_panic(expected = "invalid HTML attribute name")]
    fn link_to_invalid_attr_name_panics() {
        let options = LinkToOptions::new().attrs(&[("on click", "evil()")]);
        let _ = link_to_with("x", "/x", &options);
    }

    // ── button_to ───────────────────────────────────────────────────────

    #[test]
    fn button_to_post_renders_form_csrf_and_button() {
        let html = button_to("Log out", "/logout", Method::POST, "tok123").into_string();
        assert!(
            html.contains(r#"<form action="/logout" method="post""#),
            "{html}"
        );
        assert!(
            html.contains(r#"input type="hidden" name="_csrf" value="tok123""#),
            "{html}"
        );
        assert!(
            html.contains(r#"<button type="submit">Log out</button>"#),
            "{html}"
        );
        assert!(!html.contains("_method"), "{html}");
    }

    #[test]
    fn button_to_delete_adds_method_override() {
        let html = button_to("Delete", "/posts/42", Method::DELETE, "tok123").into_string();
        assert!(html.contains(r#"method="post""#), "{html}");
        assert!(html.contains(r#"name="_method" value="DELETE""#), "{html}");
        assert!(html.contains(r#"name="_csrf" value="tok123""#), "{html}");
    }

    #[test]
    fn button_to_put_and_patch_overrides() {
        for (method, value) in [(Method::PUT, "PUT"), (Method::PATCH, "PATCH")] {
            let html = button_to("Save", "/posts/42", method, "tok123").into_string();
            assert!(html.contains(r#"method="post""#), "{html}");
            assert!(
                html.contains(&format!(r#"name="_method" value="{value}""#)),
                "{html}"
            );
            assert!(html.contains(r#"name="_csrf""#), "{html}");
        }
    }

    #[test]
    fn button_to_get_renders_plain_get_form() {
        let html = button_to("Search", "/search", Method::GET, "tok123").into_string();
        assert!(html.contains(r#"method="get""#), "{html}");
        assert!(!html.contains("_csrf"), "{html}");
        assert!(!html.contains("tok123"), "{html}");
        assert!(!html.contains("_method"), "{html}");
        assert!(
            html.contains(r#"<button type="submit">Search</button>"#),
            "{html}"
        );
    }

    #[test]
    fn button_to_escapes_label_and_action() {
        let html = button_to(
            "<b>Delete</b>",
            r#"/x?a=1&b="><script>"#,
            Method::DELETE,
            "tok",
        )
        .into_string();
        assert!(html.contains("&lt;b&gt;"), "{html}");
        assert!(!html.contains("<b>"), "{html}");
        assert!(html.contains("&amp;"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn button_to_custom_csrf_field() {
        let options = ButtonToOptions::new().csrf_field("csrf_tok");
        let html =
            button_to_with("Delete", "/posts/42", Method::DELETE, "tok123", &options).into_string();
        assert!(html.contains(r#"name="csrf_tok" value="tok123""#), "{html}");
        assert!(!html.contains(r#"name="_csrf""#), "{html}");
    }

    #[test]
    fn button_to_button_and_form_classes() {
        let options = ButtonToOptions::new()
            .class("btn btn-danger")
            .form_class("inline-form");
        let html =
            button_to_with("Delete", "/posts/42", Method::DELETE, "tok123", &options).into_string();
        assert!(html.contains(r#"class="inline-form""#), "{html}");
        assert!(html.contains(r#"class="btn btn-danger""#), "{html}");
    }

    #[test]
    fn button_to_extra_attrs_on_button() {
        // The admin-plugin delete-button shape (templates.rs): htmx handles
        // the interaction when JS is on; the form fallback keeps working
        // without it.
        let options = ButtonToOptions::new().class("btn btn-danger").attrs(&[
            ("hx-delete", "/admin/widgets/42"),
            ("hx-confirm", "Are you sure you want to delete this Widget?"),
            ("hx-target", "body"),
        ]);
        let html = button_to_with(
            "Delete",
            "/admin/widgets/42",
            Method::DELETE,
            "tok123",
            &options,
        )
        .into_string();
        assert!(html.contains(r#"hx-delete="/admin/widgets/42""#), "{html}");
        assert!(html.contains(r#"hx-target="body""#), "{html}");
        assert!(
            html.contains("hx-confirm=\"Are you sure you want to delete this Widget?\""),
            "{html}"
        );
        assert!(html.contains(r#"name="_method" value="DELETE""#), "{html}");
        assert!(html.contains(r#"name="_csrf" value="tok123""#), "{html}");
    }

    #[test]
    #[should_panic(expected = "invalid HTML attribute name")]
    fn button_to_invalid_attr_name_panics() {
        let options = ButtonToOptions::new().attrs(&[("foo=bar", "x")]);
        let _ = button_to_with("x", "/x", Method::POST, "tok", &options);
    }
}
