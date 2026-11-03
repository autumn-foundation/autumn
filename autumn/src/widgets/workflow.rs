//! Workflow widgets: transition controls and bulk actions.
//!
//! Needs the `widgets-workflow` feature (on by default).

// ── transition_controls ────────────────────────────────────────────────────

/// Render one CSRF-protected, no-JS `POST` form + button per state-machine
/// transition whose `from` matches `current`.
///
/// This wires the field-level `#[state_machine]` attribute (on `#[model]`
/// structs) into a detail view: the macro emits, per state-machine `String`
/// field `status`, a public `Model::__AUTUMN_SM_STATUS_TRANSITIONS` constant of
/// `(from, to, Option<guard>)` tuples plus a `Model::can_transition_status_to`
/// predicate. Pass those two directly:
///
/// - `transitions` is the generated `__AUTUMN_SM_<FIELD>_TRANSITIONS` constant
///   (`&'static [(&'static str, &'static str, Option<&'static str>)]` coerces to
///   the `&[(&str, &str, Option<&str>)]` parameter, so it passes as-is).
/// - `can` is `|to| record.can_transition_<field>_to(to)`.
///
/// Because those items are per-field *inherent* items (not a trait), the helper
/// takes the already-extracted data and the call site supplies the
/// field-specific bits.
///
/// # Behavior
///
/// - Only edges whose `from == current` are rendered; edges from any other state
///   are omitted entirely.
/// - Each rendered edge is an independent `<form method="post" action=(action)>`
///   carrying the hidden CSRF input (mirroring the scaffold's `csrf_input`), a
///   hidden `<input name=(field) value=(to)>` so the server route reads the
///   target state from the form body, and a single `<button type="submit">`.
/// - A button whose `can(to)` returns `false` (a legal edge whose guard
///   currently fails) is still rendered but carries the `disabled` attribute, so
///   the UI shows the transition as visible-but-blocked. A guard-less edge, or
///   one whose guard passes, renders an enabled button.
/// - No JavaScript: plain HTML forms and buttons only.
/// - When no edge matches `current` (a terminal state), an empty grouping
///   container is rendered (the `role="group"` `<div>` with no forms inside).
///
/// # CSRF
///
/// Pass the handler's `csrf: Option<&CsrfToken>` and
/// `csrf_field: Option<&CsrfFormField>` through unchanged. When `csrf` is
/// `Some`, each form gets a hidden `<input type="hidden">` named after
/// `csrf_field` (defaulting to `"_csrf"`), exactly like scaffolded forms.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |----------|---------|
/// | `.autumn-transition-controls` | Grouping `<div role="group">` |
/// | `.autumn-transition` | Each per-edge `<form>` |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::transition_controls;
///
/// // `Order::__AUTUMN_SM_STATUS_TRANSITIONS` would supply this at a call site.
/// let transitions: &[(&str, &str, Option<&str>)] = &[
///     ("draft", "published", Some("can_publish")),
///     ("published", "archived", None),
/// ];
/// let markup = transition_controls(
///     "/orders/42/transitions/status",
///     "status",
///     "draft",
///     transitions,
///     |to| to == "published", // |to| order.can_transition_status_to(to)
///     None,
///     None,
/// );
/// let html = markup.into_string();
/// assert!(html.contains(r#"class="autumn-transition-controls""#));
/// assert!(html.contains(r#"action="/orders/42/transitions/status""#));
/// assert!(html.contains(r#"<input type="hidden" name="status" value="published">"#));
/// // The `draft -> published` edge is shown; `published -> archived` is not.
/// assert!(!html.contains(r#"value="archived""#));
/// ```
#[must_use]
pub fn transition_controls(
    action: &str,
    field: &str,
    current: &str,
    transitions: &[(&str, &str, Option<&str>)],
    can: impl Fn(&str) -> bool,
    csrf: Option<&crate::security::CsrfToken>,
    csrf_field: Option<&crate::security::CsrfFormField>,
) -> maud::Markup {
    transition_controls_with_labels(
        action,
        field,
        current,
        transitions,
        can,
        csrf,
        csrf_field,
        &TransitionLabels::new(),
    )
}

/// Group and per-edge button labels for [`transition_controls`].
///
/// An edge with no override keeps autumn-web's default English text.
/// Build with [`TransitionLabels::new`] and chain the `const` builder
/// methods.
#[derive(Clone, Copy, Default)]
pub struct TransitionLabels<'a> {
    group: Option<&'a str>,
    buttons: &'a [(&'a str, &'a str)],
}

impl<'a> TransitionLabels<'a> {
    /// Make labels with no overrides.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            group: None,
            buttons: &[],
        }
    }

    /// Set the group label. Replaces `"{field} transitions"`.
    #[must_use]
    pub const fn group(mut self, label: &'a str) -> Self {
        self.group = Some(label);
        self
    }

    /// Set button labels by target state.
    ///
    /// Each pair is `(target_state, label)`. A state not listed keeps
    /// `"Mark as {state}"`.
    #[must_use]
    pub const fn buttons(mut self, buttons: &'a [(&'a str, &'a str)]) -> Self {
        self.buttons = buttons;
        self
    }

    /// Get the group label for the given field.
    fn group_label(&self, field: &str) -> String {
        self.group
            .map_or_else(|| format!("{field} transitions"), ToString::to_string)
    }

    /// Get the button label for the given target state.
    fn button_label(&self, to: &str) -> String {
        self.buttons
            .iter()
            .find(|(state, _)| *state == to)
            .map_or_else(
                || format!("Mark as {to}"),
                |(_, label)| (*label).to_string(),
            )
    }
}

/// Same as [`transition_controls`], with the group and button text open to
/// override via `labels`.
///
/// Pass [`TransitionLabels::new`] for the same output as
/// [`transition_controls`]. See [`TransitionLabels`] for the override fields.
#[must_use]
#[allow(clippy::too_many_arguments)] // Same shape as `transition_controls`, plus `labels`.
pub fn transition_controls_with_labels(
    action: &str,
    field: &str,
    current: &str,
    transitions: &[(&str, &str, Option<&str>)],
    can: impl Fn(&str) -> bool,
    csrf: Option<&crate::security::CsrfToken>,
    csrf_field: Option<&crate::security::CsrfFormField>,
    labels: &TransitionLabels<'_>,
) -> maud::Markup {
    let csrf_field_name = csrf_field.map_or("_csrf", |f| f.0.as_str());
    maud::html! {
        div class="autumn-transition-controls" role="group" aria-label=(labels.group_label(field)) {
            @for (from, to, _guard) in transitions {
                @if *from == current {
                    form method="post" action=(action) class="autumn-transition" {
                        @if let Some(tok) = csrf {
                            input type="hidden" name=(csrf_field_name) value=(tok.token());
                        }
                        input type="hidden" name=(field) value=(to);
                        button type="submit" disabled[!can(to)] { (labels.button_label(to)) }
                    }
                }
            }
        }
    }
}

// ── bulk actions (#1312) ──────────────────────────────────────────────────

/// Configuration for the bulk-select / bulk-actions widgets.
///
/// Build with [`BulkActionsConfig::new`] (the form `action` URL) and chain the
/// builder methods for optional overrides.
#[derive(Debug, Clone)]
pub struct BulkActionsConfig<'a> {
    /// Form `action` URL the bulk submit posts to (e.g. `/posts/bulk_delete`).
    pub action: &'a str,
    /// Name of the per-row checkbox field (default `"ids"`).
    pub field_name: &'a str,
    /// Label on the bulk submit button (default `"Delete selected"`).
    pub submit_label: &'a str,
    /// Prefix of each checkbox's `aria-label` (default `"Select row"`).
    pub select_label: &'a str,
}

impl<'a> BulkActionsConfig<'a> {
    /// Create a new config posting to `action`, with sensible defaults.
    #[must_use]
    pub const fn new(action: &'a str) -> Self {
        Self {
            action,
            field_name: "ids",
            submit_label: "Delete selected",
            select_label: "Select row",
        }
    }

    /// Override the per-row checkbox field name (default `"ids"`).
    #[must_use]
    pub const fn field_name(mut self, field_name: &'a str) -> Self {
        self.field_name = field_name;
        self
    }

    /// Override the bulk submit button label (default `"Delete selected"`).
    #[must_use]
    pub const fn submit_label(mut self, submit_label: &'a str) -> Self {
        self.submit_label = submit_label;
        self
    }

    /// Override the checkbox `aria-label` prefix (default `"Select row"`).
    #[must_use]
    pub const fn select_label(mut self, select_label: &'a str) -> Self {
        self.select_label = select_label;
        self
    }
}

/// Render one row's bulk-select checkbox (issue #1312).
///
/// One of these goes in the first cell of every row inside a
/// [`bulk_actions_form`]. Every checkbox shares the same `name`
/// ([`BulkActionsConfig::field_name`], default `"ids"`), so a browser submits
/// the checked rows as repeated `ids=<value>` pairs — no JavaScript, no
/// per-row form. The server side reads them into a `Vec` field with
/// [`ChangesetForm`](crate::form::ChangesetForm) or
/// [`from_urlencoded`](crate::form::from_urlencoded) (the scaffold's
/// `BulkSelection { ids: Vec<i64> }` also accepts the `ids[]` spelling).
///
/// `value` is anything [`Display`](std::fmt::Display) — typically the row's
/// primary key. It is HTML-escaped by Maud, both in `value` and in the
/// `aria-label` (`"{select_label} {value}"`), so a screen reader announces
/// which row each checkbox selects rather than a wall of unlabelled controls.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |----------|---------|
/// | `.autumn-bulk-select` | The per-row `<input type="checkbox">` |
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::widgets::{BulkActionsConfig, Column, bulk_select_checkbox};
///
/// let action = paths::bulk_delete();
/// let cfg = BulkActionsConfig::new(&action);
/// let mut columns: Vec<Column<Post>> = post_columns();
/// columns.insert(
///     0,
///     Column::new("", |row: &Post| maud::html! { (bulk_select_checkbox(row.id, &cfg)) }),
/// );
/// ```
///
/// ```rust
/// use autumn_web::widgets::{BulkActionsConfig, bulk_select_checkbox};
///
/// let cfg = BulkActionsConfig::new("/posts/bulk_delete");
/// let html = bulk_select_checkbox(42, &cfg).into_string();
/// assert!(html.contains(r#"type="checkbox""#));
/// assert!(html.contains(r#"name="ids""#));
/// assert!(html.contains(r#"value="42""#));
/// assert!(html.contains(r#"aria-label="Select row 42""#));
/// ```
#[must_use]
pub fn bulk_select_checkbox(
    value: impl std::fmt::Display,
    config: &BulkActionsConfig<'_>,
) -> maud::Markup {
    let value = value.to_string();
    let aria_label = format!("{} {value}", config.select_label);
    maud::html! {
        input type="checkbox" name=(config.field_name) value=(value)
            class="autumn-bulk-select" aria-label=(aria_label);
    }
}

/// Render the bulk-actions toolbar — the submit button that applies the
/// selection (issue #1312).
///
/// Rendered automatically at the end of a [`bulk_actions_form`]; call it
/// directly only when hand-building the surrounding `<form>`.
///
/// # No-JavaScript guarantee
///
/// The button is a plain `<button type="submit">` — no inline handler, no
/// scripting of any kind, so the control works with JavaScript disabled and
/// the server stays the enforcement point.
///
/// # Confirming a bulk action
///
/// This widget deliberately emits **no** confirmation prompt. The framework's
/// answer to `window.confirm()` is [`confirm_action`], whose dialog is
/// server-rendered and therefore assertable from a
/// [`TestClient`](crate::test::TestClient) test — but it submits its own
/// single-action `<form>`, so it cannot carry a bulk form's checkbox
/// selection (HTML forbids nesting one form in another). An inline
/// `onclick="return confirm(..)"` is not an option either: Autumn's default
/// CSP is `script-src 'self'` with no `'unsafe-inline'`, so the browser
/// blocks inline handlers and the destructive form would submit with no
/// prompt at all — worse than not promising one.
///
/// To confirm a batch, post the selection to an interstitial page that lists
/// the affected rows and asks for a second, explicit submit.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |----------|---------|
/// | `.autumn-bulk-actions` | Grouping `<div role="group">` around the submit button |
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::widgets::{BulkActionsConfig, bulk_actions_toolbar};
///
/// let cfg = BulkActionsConfig::new("/posts/bulk_delete")
///     .submit_label("Delete selected");
/// let toolbar = bulk_actions_toolbar(&cfg);
/// ```
///
/// ```rust
/// use autumn_web::widgets::{BulkActionsConfig, bulk_actions_toolbar};
///
/// let cfg = BulkActionsConfig::new("/posts/bulk_delete");
/// let html = bulk_actions_toolbar(&cfg).into_string();
/// assert!(html.contains(r#"class="autumn-bulk-actions""#));
/// assert!(html.contains(r#"type="submit""#));
/// assert!(html.contains("Delete selected"));
/// // Nothing scripted: no inline handler and no prompt hook.
/// assert!(!html.contains("onclick"));
/// ```
#[must_use]
pub fn bulk_actions_toolbar(config: &BulkActionsConfig<'_>) -> maud::Markup {
    maud::html! {
        div class="autumn-bulk-actions" role="group" {
            button type="submit" { (config.submit_label) }
        }
    }
}

/// Wrap a rendered list in the bulk-actions `<form>` (issue #1312).
///
/// This is the outer half of the no-JavaScript bulk-select flow: a plain
/// `POST` form whose `action` is [`BulkActionsConfig::action`], containing (in
/// order) the hidden one-time submit-token field, the hidden CSRF field, the
/// caller's `content` — a table, list, or anything else carrying one
/// [`bulk_select_checkbox`] per row — and the [`bulk_actions_toolbar`] submit
/// button.
///
/// Keep page furniture that is *not* part of the selection (a "New record"
/// link, a search box) outside the form: anything inside is submitted with the
/// selection.
///
/// # CSRF
///
/// Pass the handler's token as `csrf_token` and the configured field name as
/// `csrf_field` (both `Option<&str>`, e.g.
/// `csrf.as_ref().map(|t| t.token())` and
/// `csrf_field.as_ref().map(|f| f.0.as_str())`). When `csrf_token` is `None`
/// no hidden input is rendered at all; when it is `Some`, the field name
/// defaults to `"_csrf"`.
///
/// # Double-submit protection
///
/// Pass the handler's [`SubmitToken`](crate::security::SubmitToken) as
/// `submit_token` and the configured field name as `submit_field` (again both
/// `Option<&str>`). `SubmitTokenLayer` consumes the token once and replays the
/// first response for any repeat, so a double-click or a Back→resubmit deletes
/// the batch once instead of running the whole destructive path — including
/// hooks and dependent deletes — twice. A request carrying *no* token passes
/// through unguarded, so omitting this on a destructive form silently gives up
/// the protection.
///
/// The token input is emitted at the very front of the form, ahead of the
/// per-row checkboxes: `SubmitTokenLayer` only scans the first chunk of the
/// URL-encoded body, and a large selection could otherwise push a trailing
/// token past the scan cap.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |----------|---------|
/// | `.autumn-bulk-form` | The wrapping `<form method="post">` |
/// | `.autumn-bulk-actions` | Grouping `<div role="group">` around the submit button |
/// | `.autumn-bulk-select` | Each row's `<input type="checkbox">` |
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::widgets::{BulkActionsConfig, bulk_actions_form, data_table, DataTableConfig};
///
/// let action = paths::bulk_delete();
/// let cfg = BulkActionsConfig::new(&action);
/// Ok(layout("Posts", html! {
///     a href=(paths::new()) { "New Post" } // outside the form
///     (bulk_actions_form(
///         &cfg,
///         csrf.as_ref().map(|t| t.token()),
///         csrf_field.as_ref().map(|f| f.0.as_str()),
///         submit_token.as_ref().map(|t| t.token()),
///         submit_field.as_ref().map(|f| f.0.as_str()),
///         html! {
///             (data_table(&page.content, &columns, &DataTableConfig::new("No posts yet.")))
///         },
///     ))
/// }))
/// ```
///
/// ```rust
/// use autumn_web::widgets::{BulkActionsConfig, bulk_actions_form};
///
/// let cfg = BulkActionsConfig::new("/posts/bulk_delete");
/// let html = bulk_actions_form(
///     &cfg,
///     Some("tok-123"),
///     None,
///     Some("submit-456"),
///     None,
///     maud::html! { p { "rows" } },
/// )
/// .into_string();
/// assert!(html.contains(r#"method="post""#));
/// assert!(html.contains(r#"action="/posts/bulk_delete""#));
/// assert!(html.contains(r#"name="_csrf""#));
/// assert!(html.contains(r#"value="tok-123""#));
/// assert!(html.contains(r#"name="_submit_token""#));
/// assert!(html.contains(r#"value="submit-456""#));
/// // Tokens ahead of the rows, content next, toolbar last, all inside the form.
/// assert!(html.find("_submit_token") < html.find("rows"));
/// assert!(html.find("rows") < html.find("autumn-bulk-actions"));
/// assert!(html.trim_end().ends_with("</form>"));
/// ```
// `content` is taken by value so `bulk_actions_form(&cfg, .., html! { .. })`
// reads without a `&` at the call site (matching `alert_with`); Maud renders it
// by reference, hence the allow.
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn bulk_actions_form(
    config: &BulkActionsConfig<'_>,
    csrf_token: Option<&str>,
    csrf_field: Option<&str>,
    submit_token: Option<&str>,
    submit_field: Option<&str>,
    content: maud::Markup,
) -> maud::Markup {
    let csrf_field_name = csrf_field.unwrap_or("_csrf");
    let submit_field_name = submit_field.unwrap_or("_submit_token");
    maud::html! {
        form method="post" action=(config.action) class="autumn-bulk-form" {
            // Both hidden fields lead the body: `SubmitTokenLayer` only scans
            // its first chunk, so a long selection of checkboxes must not be
            // able to push the token past the scan cap.
            @if let Some(token) = submit_token {
                input type="hidden" name=(submit_field_name) value=(token);
            }
            @if let Some(token) = csrf_token {
                input type="hidden" name=(csrf_field_name) value=(token);
            }
            (content)
            (bulk_actions_toolbar(config))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── transition_controls ────────────────────────────────────────────

    /// A plain literal transition graph: `draft -> published` (guarded),
    /// `published -> archived` (guard-less). No real model required.
    const fn sample_transitions() -> &'static [(&'static str, &'static str, Option<&'static str>)] {
        &[
            ("draft", "published", Some("can_publish")),
            ("published", "archived", None),
        ]
    }

    #[test]
    fn transition_controls_only_renders_edges_from_current() {
        // current = "draft": only the draft -> published edge should appear.
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |_to| true,
            None,
            None,
        )
        .into_string();
        assert!(html.contains(r#"value="published""#), "{html}");
        // The published -> archived edge is from a different state: absent.
        assert!(!html.contains(r#"value="archived""#), "{html}");
    }

    #[test]
    fn transition_controls_renders_terminal_current_as_empty_group() {
        // current = "archived" is terminal: no edges match, empty group only.
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "archived",
            sample_transitions(),
            |_to| true,
            None,
            None,
        )
        .into_string();
        assert!(
            html.contains(r#"class="autumn-transition-controls""#),
            "{html}"
        );
        assert!(!html.contains("<form"), "{html}");
        assert!(!html.contains("<button"), "{html}");
    }

    #[test]
    fn transition_controls_guard_true_button_is_enabled() {
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |to| to == "published", // guard passes for the draft -> published edge
            None,
            None,
        )
        .into_string();
        assert!(
            html.contains("<button type=\"submit\">Mark as published</button>"),
            "{html}"
        );
        assert!(!html.contains("disabled"), "{html}");
    }

    #[test]
    fn transition_controls_guard_false_button_is_disabled() {
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |_to| false, // guard fails: legal edge, but currently blocked
            None,
            None,
        )
        .into_string();
        // Still rendered, but disabled.
        assert!(html.contains(r#"value="published""#), "{html}");
        assert!(html.contains("disabled"), "{html}");
    }

    #[test]
    fn transition_controls_guardless_edge_from_current_is_enabled() {
        // current = "published": the guard-less published -> archived edge shows,
        // enabled (its guard slot is None, and `can` returns true).
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "published",
            sample_transitions(),
            |_to| true,
            None,
            None,
        )
        .into_string();
        assert!(html.contains(r#"value="archived""#), "{html}");
        assert!(
            html.contains("<button type=\"submit\">Mark as archived</button>"),
            "{html}"
        );
        assert!(!html.contains("disabled"), "{html}");
    }

    #[test]
    fn transition_controls_form_is_post_no_js_and_carries_target_in_body() {
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |_to| true,
            None,
            None,
        )
        .into_string();
        assert!(
            html.contains(r#"<form method="post" action="/orders/42/transitions/status""#),
            "{html}"
        );
        // Target state carried in the body as a hidden input under the field name.
        assert!(
            html.contains(r#"<input type="hidden" name="status" value="published">"#),
            "{html}"
        );
        // No JavaScript.
        assert!(!html.contains("onclick"), "{html}");
        assert!(!html.contains("hx-"), "{html}");
        assert!(!html.contains("<script"), "{html}");
    }

    #[test]
    fn transition_controls_csrf_hidden_input_present_when_token_supplied() {
        let token = crate::security::CsrfToken::new("secret-token".to_string());
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |_to| true,
            Some(&token),
            None,
        )
        .into_string();
        assert!(
            html.contains(r#"<input type="hidden" name="_csrf" value="secret-token">"#),
            "{html}"
        );
    }

    #[test]
    fn transition_controls_csrf_absent_when_no_token() {
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |_to| true,
            None,
            None,
        )
        .into_string();
        assert!(!html.contains("_csrf"), "{html}");
    }

    #[test]
    fn transition_controls_honors_custom_csrf_field_name() {
        let token = crate::security::CsrfToken::new("secret-token".to_string());
        let field = crate::security::CsrfFormField("csrf_custom".to_string());
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |_to| true,
            Some(&token),
            Some(&field),
        )
        .into_string();
        assert!(
            html.contains(r#"<input type="hidden" name="csrf_custom" value="secret-token">"#),
            "{html}"
        );
        assert!(!html.contains(r#"name="_csrf""#), "{html}");
    }

    #[test]
    fn transition_controls_with_labels_overrides_the_group_label() {
        let html = transition_controls_with_labels(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |_to| true,
            None,
            None,
            &TransitionLabels::new().group("Passer à"),
        )
        .into_string();
        assert!(html.contains("Passer à"), "{html}");
        assert!(!html.contains("status transitions"), "{html}");
    }

    #[test]
    fn transition_controls_with_labels_overrides_a_single_button_by_state() {
        // current = "pending" has two edges here, so one override leaves the
        // other edge's button on the default text.
        let transitions: &[(&str, &str, Option<&str>)] =
            &[("pending", "approved", None), ("pending", "rejected", None)];
        let html = transition_controls_with_labels(
            "/orders/42/transitions/status",
            "status",
            "pending",
            transitions,
            |_to| true,
            None,
            None,
            &TransitionLabels::new().buttons(&[("approved", "Approve it")]),
        )
        .into_string();
        assert!(
            html.contains("<button type=\"submit\">Approve it</button>"),
            "{html}"
        );
        // "rejected" has no override: it keeps the default "Mark as {state}" text.
        assert!(
            html.contains("<button type=\"submit\">Mark as rejected</button>"),
            "{html}"
        );
    }

    #[test]
    fn transition_controls_with_labels_default_is_byte_identical_to_transition_controls() {
        let plain = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |_to| true,
            None,
            None,
        )
        .into_string();
        let with_labels = transition_controls_with_labels(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |_to| true,
            None,
            None,
            &TransitionLabels::new(),
        )
        .into_string();
        assert_eq!(plain, with_labels);
    }

    // ── bulk actions (#1312) ───────────────────────────────────────────

    #[test]
    fn bulk_actions_config_defaults() {
        let cfg = BulkActionsConfig::new("/posts/bulk_delete");
        assert_eq!(cfg.action, "/posts/bulk_delete");
        assert_eq!(cfg.field_name, "ids");
        assert_eq!(cfg.submit_label, "Delete selected");
        assert_eq!(cfg.select_label, "Select row");
    }

    #[test]
    fn bulk_actions_config_builders() {
        let cfg = BulkActionsConfig::new("/posts/bulk_delete")
            .field_name("post_ids")
            .submit_label("Archive selected")
            .select_label("Select post");
        assert_eq!(cfg.action, "/posts/bulk_delete");
        assert_eq!(cfg.field_name, "post_ids");
        assert_eq!(cfg.submit_label, "Archive selected");
        assert_eq!(cfg.select_label, "Select post");
    }

    #[test]
    fn bulk_select_checkbox_emits_name_value_and_aria_label() {
        let cfg = BulkActionsConfig::new("/posts/bulk_delete");
        let html = bulk_select_checkbox(42, &cfg).into_string();
        assert!(html.contains(r#"type="checkbox""#), "{html}");
        assert!(html.contains(r#"name="ids""#), "{html}");
        assert!(html.contains(r#"value="42""#), "{html}");
        assert!(html.contains("autumn-bulk-select"), "{html}");
        assert!(html.contains(r#"aria-label="Select row 42""#), "{html}");
    }

    #[test]
    fn bulk_select_checkbox_honours_custom_field_name() {
        let cfg = BulkActionsConfig::new("/posts/bulk_delete")
            .field_name("post_ids")
            .select_label("Select post");
        let html = bulk_select_checkbox(7, &cfg).into_string();
        assert!(html.contains(r#"name="post_ids""#), "{html}");
        assert!(!html.contains(r#"name="ids""#), "{html}");
        assert!(html.contains(r#"aria-label="Select post 7""#), "{html}");
    }

    #[test]
    fn bulk_actions_form_renders_csrf_hidden_input() {
        let cfg = BulkActionsConfig::new("/posts/bulk_delete");
        let html = bulk_actions_form(&cfg, Some("tok-123"), None, None, None, maud::html! {})
            .into_string();
        assert!(html.contains(r#"type="hidden""#), "{html}");
        // The csrf field name defaults to `_csrf` when none is supplied.
        assert!(html.contains(r#"name="_csrf""#), "{html}");
        assert!(html.contains(r#"value="tok-123""#), "{html}");
        // A configured field name wins over the default.
        let named = bulk_actions_form(
            &cfg,
            Some("tok-123"),
            Some("authenticity"),
            None,
            None,
            maud::html! {},
        )
        .into_string();
        assert!(named.contains(r#"name="authenticity""#), "{named}");
        assert!(!named.contains(r#"name="_csrf""#), "{named}");
    }

    #[test]
    fn bulk_actions_form_omits_csrf_input_when_none() {
        let cfg = BulkActionsConfig::new("/posts/bulk_delete");
        let html = bulk_actions_form(&cfg, None, None, None, None, maud::html! {}).into_string();
        assert!(!html.contains(r#"type="hidden""#), "{html}");
        assert!(!html.contains("_csrf"), "{html}");
        // The form itself is still rendered.
        assert!(html.contains("<form"), "{html}");
    }

    /// `SubmitTokenLayer` passes a tokenless request straight through, so the
    /// hidden field is what stands between a double-clicked bulk delete and a
    /// second run of the whole destructive path.
    #[test]
    fn bulk_actions_form_renders_submit_token_hidden_input() {
        let cfg = BulkActionsConfig::new("/posts/bulk_delete");
        let html = bulk_actions_form(&cfg, None, None, Some("submit-456"), None, maud::html! {})
            .into_string();
        // The field name defaults to `_submit_token` when none is supplied.
        assert!(html.contains(r#"name="_submit_token""#), "{html}");
        assert!(html.contains(r#"value="submit-456""#), "{html}");
        // A configured field name wins over the default, matching whatever
        // `security.submit_token.field_name` the layer scans for.
        let named = bulk_actions_form(
            &cfg,
            None,
            None,
            Some("submit-456"),
            Some("_once"),
            maud::html! {},
        )
        .into_string();
        assert!(named.contains(r#"name="_once""#), "{named}");
        assert!(!named.contains(r#"name="_submit_token""#), "{named}");
        // Omitted entirely when the handler has no token.
        let without = bulk_actions_form(&cfg, None, None, None, None, maud::html! {}).into_string();
        assert!(!without.contains("_submit_token"), "{without}");
    }

    /// The layer only scans the first chunk of the URL-encoded body, so a long
    /// selection must not be able to push the token past the scan cap.
    #[test]
    fn bulk_actions_form_puts_submit_token_ahead_of_the_rows() {
        let cfg = BulkActionsConfig::new("/posts/bulk_delete");
        let rows = maud::html! {
            @for id in 1..200 {
                (bulk_select_checkbox(id, &cfg))
            }
        };
        let html = bulk_actions_form(&cfg, Some("csrf-1"), None, Some("submit-456"), None, rows)
            .into_string();
        let token_at = html.find("_submit_token").expect("token rendered");
        let first_row_at = html.find("autumn-bulk-select").expect("rows rendered");
        assert!(
            token_at < first_row_at,
            "the submit token must lead the body: {html}"
        );
    }

    #[test]
    fn bulk_actions_form_posts_to_action_and_wraps_content() {
        let cfg = BulkActionsConfig::new("/posts/bulk_delete");
        let content = maud::html! { p { "row-marker" } };
        let html = bulk_actions_form(&cfg, None, None, None, None, content).into_string();
        assert!(html.contains(r#"method="post""#), "{html}");
        assert!(html.contains(r#"action="/posts/bulk_delete""#), "{html}");
        assert!(html.contains("autumn-bulk-form"), "{html}");
        assert!(html.contains("row-marker"), "{html}");
        // The toolbar is rendered inside the form, after the content.
        let content_at = html.find("row-marker").expect("content rendered");
        let toolbar_at = html
            .find("autumn-bulk-actions")
            .expect("toolbar rendered inside the form");
        assert!(
            content_at < toolbar_at,
            "toolbar must follow content: {html}"
        );
        assert!(html.trim_end().ends_with("</form>"), "{html}");
    }

    #[test]
    fn bulk_actions_toolbar_button_is_submit_and_labelled() {
        let cfg = BulkActionsConfig::new("/posts/bulk_delete");
        let html = bulk_actions_toolbar(&cfg).into_string();
        assert!(html.contains("autumn-bulk-actions"), "{html}");
        assert!(html.contains(r#"role="group""#), "{html}");
        assert!(html.contains(r#"type="submit""#), "{html}");
        assert!(html.contains("Delete selected"), "{html}");
    }

    /// The framework bans `window.confirm()` in its runtime (see
    /// `confirm_action`, the server-rendered replacement), and the default
    /// `script-src 'self'` CSP blocks inline handlers — so this toolbar
    /// promises no confirmation rather than one that silently never fires.
    #[test]
    fn bulk_actions_toolbar_emits_nothing_scripted() {
        let cfg = BulkActionsConfig::new("/posts/bulk_delete");
        let html = bulk_actions_toolbar(&cfg).into_string();
        assert!(!html.contains("onclick"), "{html}");
        assert!(!html.contains("data-autumn-confirm"), "{html}");
        assert!(!html.contains("<script"), "{html}");
    }
}
