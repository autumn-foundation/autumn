//! Social widgets: reaction controls, comment thread and infinite feed.
//!
//! Needs the `widgets-social` feature (on by default).

// ── reaction_controls ──────────────────────────────────────────────────────

/// Configuration for a [`reaction_controls`] widget.
///
/// Build with [`ReactionControls::votes`] (signed up/down voting, the
/// `#[votable(… aggregate = sum)]` shape) or [`ReactionControls::likes`] (a
/// single membership toggle, the `aggregate = count` shape), then chain the
/// builder methods for optional overrides.
///
/// The struct owns every string it renders (no lifetimes), so a handler can
/// build it from borrowed request data and hand it straight to the widget.
///
/// # `dom_id` is trusted
///
/// `dom_id` is rendered as the container's `id` **and** interpolated into the
/// default `hx-target` (`#{dom_id}`), where it is read as a CSS selector
/// fragment. Pass a value you construct — `format!("votes-{post_id}")` from a
/// typed id is the intended shape — never a raw request parameter or any other
/// attacker-controlled string. Maud escapes the attribute value, so this is not
/// an injection hole; it is a correctness one: a value containing whitespace,
/// `#`, `.` or a quote yields a selector that matches nothing and an htmx swap
/// that silently does not land.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::ReactionControls;
///
/// let config = ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
///     .aggregate(7)          // the target's persisted `score`
///     .current(Some(1))      // `repo.reaction_of(user_id, post_id).await?`
///     .label("Post score");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionControls {
    /// `id` of the grouping container, and the default `hx-target`.
    dom_id: String,
    /// Form action for the up (vote mode) or like (count mode) control.
    up_action: String,
    /// Form action for the down control. `None` selects like/count mode, in
    /// which only the single `up_action` control is rendered.
    down_action: Option<String>,
    /// The target's persisted aggregate (`score` / `{name}_count`).
    aggregate: i64,
    /// The viewer's own current reaction, as returned by `reaction_of()`.
    current: Option<i16>,
    /// `aria-label` of the `role="group"` container.
    label: String,
    /// Accessible name of the up button.
    up_label: String,
    /// Accessible name of the down button.
    down_label: String,
    /// Accessible name of the like button (count mode).
    like_label: String,
    /// `hx-target` every form swaps; defaults to `#{dom_id}`.
    hx_target: String,
    /// CSRF token value; `None` renders no hidden input.
    csrf_token: Option<String>,
    /// CSRF hidden-input name; defaults to `_csrf`.
    csrf_field: String,
    /// Emit `hx-preserve="true"` on the buttons, for shared/broadcast
    /// fragments that must not clobber a viewer's own pressed state.
    preserve_pressed_state: bool,
}

impl ReactionControls {
    /// Shared constructor: `down_action = None` selects like/count mode.
    fn new(dom_id: String, up_action: String, down_action: Option<String>) -> Self {
        let hx_target = format!("#{dom_id}");
        Self {
            dom_id,
            up_action,
            down_action,
            aggregate: 0,
            current: None,
            label: "Reactions".to_owned(),
            up_label: "Upvote".to_owned(),
            down_label: "Downvote".to_owned(),
            like_label: "Like".to_owned(),
            hx_target,
            csrf_token: None,
            csrf_field: "_csrf".to_owned(),
            preserve_pressed_state: false,
        }
    }

    /// Signed up/down controls — the `#[votable(…, aggregate = sum)]` shape.
    ///
    /// `dom_id` names the container the control replaces on submit;
    /// `up_action` / `down_action` are the `POST` routes for each direction
    /// (typically the generated `__autumn_path_*` helpers).
    #[must_use]
    pub fn votes(
        dom_id: impl Into<String>,
        up_action: impl Into<String>,
        down_action: impl Into<String>,
    ) -> Self {
        Self::new(dom_id.into(), up_action.into(), Some(down_action.into()))
    }

    /// A single toggle control — the `#[votable(…, aggregate = count)]` shape.
    ///
    /// One `POST` route serves both directions: the generated `react()` is a
    /// toggle, so re-submitting removes the reaction.
    #[must_use]
    pub fn likes(dom_id: impl Into<String>, action: impl Into<String>) -> Self {
        Self::new(dom_id.into(), action.into(), None)
    }

    /// Set the target's aggregate — its `score` (sum) or `{name}_count`
    /// (count). Rendered verbatim, so a downvoted target shows a negative.
    #[must_use]
    pub const fn aggregate(mut self, n: i64) -> Self {
        self.aggregate = n;
        self
    }

    /// Set the viewer's own current reaction (`reaction_of()`'s result).
    ///
    /// The rendered domain is `{None, Some(1), Some(-1)}`: `Some(1)` presses
    /// the up/like button, `Some(-1)` the down button, and `None` (the
    /// default — what a feed or a signed-out viewer passes) presses neither.
    /// Any other value presses nothing, and in like/count mode `Some(-1)`
    /// presses nothing either, since that mode renders no down direction.
    /// (`reaction_of()` only ever yields those three, so this matters just for
    /// hand-built configs.)
    #[must_use]
    pub const fn current(mut self, v: Option<i16>) -> Self {
        self.current = v;
        self
    }

    /// Set the container's `aria-label`. Default: `"Reactions"`.
    #[must_use]
    pub fn label(mut self, s: impl Into<String>) -> Self {
        self.label = s.into();
        self
    }

    /// Set the up button's accessible name. Default: `"Upvote"`.
    #[must_use]
    pub fn up_label(mut self, s: impl Into<String>) -> Self {
        self.up_label = s.into();
        self
    }

    /// Set the down button's accessible name. Default: `"Downvote"`.
    #[must_use]
    pub fn down_label(mut self, s: impl Into<String>) -> Self {
        self.down_label = s.into();
        self
    }

    /// Set the like button's accessible name. Default: `"Like"`.
    #[must_use]
    pub fn like_label(mut self, s: impl Into<String>) -> Self {
        self.like_label = s.into();
        self
    }

    /// Set the `hx-target` each form swaps. Default: `#{dom_id}` — the control
    /// replaces itself in place. The value is a CSS selector, so it is subject
    /// to the same "must not be attacker-controlled" rule as `dom_id`.
    #[must_use]
    pub fn hx_target(mut self, s: impl Into<String>) -> Self {
        self.hx_target = s.into();
        self
    }

    /// Set the CSRF token value rendered as a hidden input in every form.
    ///
    /// This is the primitive; [`ReactionControls::csrf`] is the sugar that
    /// takes the handler's extractors. Without a token no hidden input is
    /// rendered at all.
    #[must_use]
    pub fn csrf_token(mut self, token: impl Into<String>) -> Self {
        self.csrf_token = Some(token.into());
        self
    }

    /// Set the CSRF hidden input's `name`. Default: `"_csrf"`.
    #[must_use]
    pub fn csrf_field(mut self, field: impl Into<String>) -> Self {
        self.csrf_field = field.into();
        self
    }

    /// Emit `hx-preserve="true"` on the buttons, so an htmx swap that replaces
    /// this control keeps the *live* button elements — and with them the
    /// viewer's own `aria-pressed` / `.autumn-reaction-active` state.
    ///
    /// For **shared/broadcast fragments only** (an SSE fan-out re-rendering a
    /// card for every subscriber, where `current` is necessarily `None`): the
    /// swap then updates the aggregate and the rest of the card without
    /// un-pressing the viewer's buttons. Each button carries a stable id
    /// derived from `dom_id` (`{dom_id}-up` / `-down` / `-like`), which is
    /// what `hx-preserve` matches on.
    ///
    /// Never set this on a control returned from the vote route itself — the
    /// direct response *wants* to repaint the pressed state it just computed,
    /// and `hx-preserve` would resurrect the stale buttons instead.
    #[must_use]
    pub const fn preserve_pressed_state(mut self, preserve: bool) -> Self {
        self.preserve_pressed_state = preserve;
        self
    }

    /// Thread the handler's `Option<&CsrfToken>` / `Option<&CsrfFormField>`
    /// through unchanged — sugar over [`ReactionControls::csrf_token`] and
    /// [`ReactionControls::csrf_field`].
    ///
    /// Both values are copied out, so the config keeps no borrow. A `None`
    /// token renders no hidden input; a `None` field keeps the `"_csrf"`
    /// default. Mirrors `transition_controls`' CSRF parameters.
    #[must_use]
    pub fn csrf(
        mut self,
        token: Option<&crate::security::CsrfToken>,
        field: Option<&crate::security::CsrfFormField>,
    ) -> Self {
        if let Some(tok) = token {
            self = self.csrf_token(tok.token());
        }
        if let Some(name) = field {
            self = self.csrf_field(name.0.as_str());
        }
        self
    }
}

/// Render a no-JS reaction control: one `POST` form per direction (CSRF-
/// protected when a token is threaded) plus the target's live aggregate,
/// upgraded in place by htmx.
///
/// This is the view half of the `#[votable]` association (issue #1362). The
/// generated `repo.react(reactor_id, target_id, value)` returns a `Reaction`
/// carrying everything the widget needs, so a route re-renders with no extra
/// query:
///
/// ```rust,ignore
/// let reaction = posts.react(user_id, post_id, 1).await?;
/// Ok(reaction_controls(
///     &ReactionControls::votes(
///         format!("votes-{post_id}"),
///         format!("/posts/{post_id}/upvote"),
///         format!("/posts/{post_id}/downvote"),
///     )
///     .aggregate(reaction.aggregate)
///     .current(reaction.value),
/// ))
/// ```
///
/// # Behavior
///
/// - The container is a `<div role="group">` carrying the config's `dom_id`
///   and `aria-label`, so assistive tech announces the buttons as one control.
/// - [`ReactionControls::votes`] renders an up form, the aggregate, and a down
///   form; [`ReactionControls::likes`] renders a single like form and the
///   aggregate.
/// - Each direction is an independent `<form method="post" action=(action)>`
///   carrying a single `<button type="submit">` (plus the hidden CSRF input
///   when a token is threaded), so the control still submits with JavaScript
///   disabled. The same form also carries `hx-post` / `hx-target` /
///   `hx-swap="outerHTML"` and a shared `hx-sync="#{dom_id}:replace"` (a
///   second click aborts the in-flight request, so an older response can
///   never repaint over a newer click), so with htmx loaded the submit is
///   intercepted and
///   the response replaces the control in place.
/// - **The widget emits `<form>` elements.** HTML forbids nesting a form
///   inside another form, so never render this widget inside one — the browser
///   drops the inner form and the buttons submit the outer one instead. Place
///   it as a sibling.
/// - Each button is an ARIA toggle: `aria-pressed="true"` exactly on the
///   direction matching `current` (`Some(1)` → up/like, `Some(-1)` → down),
///   which also gets the `autumn-reaction-active` class. `current(None)` —
///   and, in like/count mode, `current(Some(-1))`, which has no direction to
///   match — presses neither.
/// - The glyph (`▲` / `▼` / `♥`) lives in a `<span aria-hidden="true">`, so
///   the button's accessible name comes from its explicit `aria-label` — a
///   screen reader never announces a nameless icon button.
/// - The aggregate is rendered verbatim (negatives are not clamped) inside an
///   `aria-live="polite"` span, so an htmx swap announces the new total.
/// - No JavaScript, no inline styles: plain HTML forms and buttons only.
///
/// # CSRF — required for the no-JS path
///
/// Call [`ReactionControls::csrf`] with the handler's `Option<&CsrfToken>` and
/// `Option<&CsrfFormField>` (or the [`ReactionControls::csrf_token`] /
/// [`ReactionControls::csrf_field`] string primitives). When a token is set,
/// every form gets a hidden `<input type="hidden">` named after the field
/// (defaulting to `"_csrf"`), exactly like scaffolded forms. With no token,
/// no hidden input is rendered.
///
/// **This is what makes the JavaScript-off fallback actually work.** With CSRF
/// protection enabled and no token threaded, the htmx path still succeeds —
/// the framework's `autumn-htmx-csrf.js` shim adds the token as a request
/// header — but the plain form POST carries no token and is rejected with
/// `403`. A control rendered without `.csrf(...)` is therefore htmx-only in a
/// CSRF-protected app. Thread the token on every page a no-JS visitor can
/// reach; the one place it is legitimately omitted is a fragment that only
/// reaches JS-enabled clients (an SSE/live payload).
///
/// # CSS hooks
///
/// | Selector | Element |
/// |----------|---------|
/// | `.autumn-reaction-controls` | Grouping `<div role="group">` |
/// | `.autumn-reaction` | Each per-direction `<form>` |
/// | `.autumn-reaction-up` | The upvote `<form>` |
/// | `.autumn-reaction-down` | The downvote `<form>` |
/// | `.autumn-reaction-like` | The like `<form>` (count mode) |
/// | `.autumn-reaction-button` | Each submit `<button>` |
/// | `.autumn-reaction-active` | The button matching the viewer's reaction |
/// | `.autumn-reaction-count` | The `aria-live` aggregate `<span>` |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{ReactionControls, reaction_controls};
///
/// let markup = reaction_controls(
///     &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
///         .aggregate(7)
///         .current(Some(1))
///         .label("Post score"),
/// );
/// let html = markup.into_string();
/// assert!(html.contains(r#"id="votes-42""#));
/// assert!(html.contains(r#"class="autumn-reaction-controls""#));
/// assert!(html.contains(r##"hx-target="#votes-42""##));
/// assert!(html.contains(r#"aria-label="Upvote""#));
/// assert!(html.contains(r#"aria-pressed="true""#));  // the up button is the current reaction
/// assert!(html.contains(r#"aria-pressed="false""#)); // the down button is not
/// assert!(html.contains(">7<"));
/// ```
#[must_use]
pub fn reaction_controls(cfg: &ReactionControls) -> maud::Markup {
    // Every class is a plain string literal (never `format!`), so the
    // widget-CSS coverage gate can extract it and prove `widgets.css` backs it.
    let up_pressed = cfg.current == Some(1);
    let down_pressed = cfg.current == Some(-1);
    let like_mode = cfg.down_action.is_none();

    let (primary_form_class, primary_glyph, primary_label) = if like_mode {
        ("autumn-reaction autumn-reaction-like", "♥", &cfg.like_label)
    } else {
        ("autumn-reaction autumn-reaction-up", "▲", &cfg.up_label)
    };
    let primary_button_class = if up_pressed {
        "autumn-reaction-button autumn-reaction-active"
    } else {
        "autumn-reaction-button"
    };
    let down_button_class = if down_pressed {
        "autumn-reaction-button autumn-reaction-active"
    } else {
        "autumn-reaction-button"
    };
    let primary_pressed = if up_pressed { "true" } else { "false" };
    let down_pressed_attr = if down_pressed { "true" } else { "false" };
    // Stable per-direction button ids: what `hx-preserve` matches on when a
    // shared/broadcast fragment must not clobber the viewer's pressed state.
    let primary_button_id = if like_mode {
        format!("{}-like", cfg.dom_id)
    } else {
        format!("{}-up", cfg.dom_id)
    };
    let down_button_id = format!("{}-down", cfg.dom_id);
    let preserve = cfg.preserve_pressed_state.then_some("true");
    // Both forms share one sync scope with the `replace` strategy: a second
    // click (up then down before the first response lands) aborts the
    // in-flight request, so only the LAST click's response repaints the
    // control. Without this the two independent forms race and the older
    // response can land second, leaving the stale direction pressed even
    // though the database holds the newer vote — the commits themselves are
    // already serialized by the target-row lock; this orders the *responses*.
    let hx_sync = format!("#{}:replace", cfg.dom_id);

    maud::html! {
        div id=(cfg.dom_id) class="autumn-reaction-controls" role="group" aria-label=(cfg.label) {
            form method="post" action=(cfg.up_action) class=(primary_form_class)
                hx-post=(cfg.up_action) hx-target=(cfg.hx_target) hx-swap="outerHTML"
                hx-sync=(hx_sync) {
                @if let Some(token) = &cfg.csrf_token {
                    input type="hidden" name=(cfg.csrf_field) value=(token);
                }
                button type="submit" id=(primary_button_id) class=(primary_button_class)
                    aria-pressed=(primary_pressed) aria-label=(primary_label)
                    hx-preserve=[preserve] {
                    span aria-hidden="true" { (primary_glyph) }
                }
            }
            span class="autumn-reaction-count" aria-live="polite" { (cfg.aggregate) }
            @if let Some(down_action) = &cfg.down_action {
                form method="post" action=(down_action) class="autumn-reaction autumn-reaction-down"
                    hx-post=(down_action) hx-target=(cfg.hx_target) hx-swap="outerHTML"
                    hx-sync=(hx_sync) {
                    @if let Some(token) = &cfg.csrf_token {
                        input type="hidden" name=(cfg.csrf_field) value=(token);
                    }
                    button type="submit" id=(down_button_id) class=(down_button_class)
                        aria-pressed=(down_pressed_attr) aria-label=(cfg.down_label)
                        hx-preserve=[preserve] {
                        span aria-hidden="true" { "▼" }
                    }
                }
            }
        }
    }
}

// ── comment_thread ─────────────────────────────────────────────────────────

/// One rendered comment, plus the replies nested under it.
///
/// Deliberately a *view* type rather than the database
/// [`Comment`](crate::commentable::Comment): the widget renders strings, and
/// keeping it independent lets it be built from any source (a fixture, a cache,
/// another store) and lets `widgets` compile without the `db` feature. Build it
/// from a real thread with [`CommentView::from_thread`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentView {
    /// The comment's id — rendered as part of each node's DOM id and as the
    /// reply form's `reply_to` value.
    pub id: i64,
    /// The author's display name, already resolved. Escaped on render.
    pub author: String,
    /// The comment body. Blank-line-separated paragraphs are rendered as
    /// separate `<p>`s; the text itself is escaped, never parsed as HTML.
    pub body: String,
    /// A machine-readable timestamp for `<time datetime="…">`, e.g. an RFC 3339
    /// string. `None` renders no `<time>` element.
    pub datetime: Option<String>,
    /// The human-readable timestamp shown inside `<time>`. Ignored when
    /// `datetime` is `None`.
    pub timestamp: String,
    /// Replies to this comment, in the order they should render.
    pub replies: Vec<Self>,
}

#[cfg(feature = "db")]
impl CommentView {
    /// Build renderable views from a
    /// [`comment_thread`](crate::commentable::comment_thread) result.
    ///
    /// `author_name` is used when the model declared
    /// `#[commentable(author_name = <column>)]`; otherwise the author falls
    /// back to `user #{id}`, because inventing a name would be worse than
    /// admitting there isn't one. Timestamps render as `YYYY-MM-DD HH:MM` UTC
    /// with a full RFC 3339 `datetime` attribute, so a client-side
    /// relative-time enhancement has something exact to read.
    #[must_use]
    pub fn from_thread(nodes: &[crate::commentable::CommentNode]) -> Vec<Self> {
        nodes
            .iter()
            .map(|node| Self {
                id: node.comment.id,
                author: node
                    .comment
                    .author_name
                    .clone()
                    .unwrap_or_else(|| format!("user #{}", node.comment.author_id)),
                body: node.comment.body.clone(),
                datetime: Some(
                    node.comment
                        .created_at
                        .and_utc()
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                ),
                timestamp: node.comment.created_at.format("%Y-%m-%d %H:%M").to_string(),
                replies: Self::from_thread(&node.replies),
            })
            .collect()
    }
}

/// Configuration for a [`comment_thread`] widget.
///
/// Build with [`CommentThread::new`] and chain the builder methods.
///
/// # `dom_id` is trusted
///
/// `dom_id` is rendered as the container's `id`, interpolated into the default
/// `hx-target` (`#{dom_id}`) where it is read as a CSS selector fragment, and
/// used as the prefix of every node's own id. Pass a value you construct —
/// `format!("comments-post-{post_id}")` from a typed id is the intended shape —
/// never a raw request parameter. Maud escapes the attribute value, so this is
/// not an injection hole; it is a correctness one: a value containing
/// whitespace, `#`, `.` or a quote yields a selector that matches nothing and
/// an htmx swap that silently does not land.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::CommentThread;
///
/// let config = CommentThread::new("comments-post-42", "/comments/Post/42")
///     .csrf_token("tok")
///     .max_depth(5);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentThread {
    /// `id` of the region, the default `hx-target`, and each node's id prefix.
    dom_id: String,
    /// Form action every comment and reply posts to.
    action: String,
    /// CSRF token value; `None` renders no hidden input.
    csrf_token: Option<String>,
    /// CSRF hidden-input name; defaults to `_csrf`.
    csrf_field: String,
    /// Name of the textarea carrying the comment body.
    body_field: String,
    /// Name of the hidden input carrying the id being replied to.
    reply_field: String,
    /// `aria-label` of the region landmark.
    label: String,
    /// Shown in place of the list when the thread is empty.
    empty_text: String,
    /// Label of the top-level submit button.
    submit_label: String,
    /// Label of the per-node reply disclosure and its submit button.
    reply_label: String,
    /// `placeholder` of the comment textarea.
    placeholder: String,
    /// `hx-target` every form swaps; defaults to `#{dom_id}`.
    hx_target: String,
    /// Deepest level that still offers a reply form. Matches the model's
    /// `#[commentable(max_depth = …)]`, so the UI never offers a reply the
    /// write path would refuse.
    max_depth: usize,
    /// Whether to render any form at all. `false` renders a read-only thread
    /// (plus [`CommentThread::sign_in_prompt`], when set) — the shape a signed
    /// out visitor sees.
    can_comment: bool,
    /// Shown in place of the form when `can_comment` is `false`.
    sign_in_prompt: Option<String>,
    /// Where a **non-htmx** submit should send the browser back to, rendered as
    /// a hidden `return_to` input. `None` leaves the handler to answer with the
    /// re-rendered fragment.
    return_to: Option<String>,
    /// `maxlength` on the textarea, from the model's `max_body` — so an
    /// over-long comment is refused in the browser rather than by a `422` htmx
    /// would not even swap.
    max_body_bytes: Option<usize>,
    /// A rejection to show above the form, rendered `role="alert"`.
    error: Option<String>,
    /// The rejected body, re-filled into the form that submitted it.
    ///
    /// A 422 re-renders the whole thread, and the htmx swap is `outerHTML` — so
    /// without this the user's typed comment is replaced by an empty textarea
    /// and simply gone. `maxlength` does not save them: it counts CHARACTERS
    /// while `max_body` counts BYTES, so an ordinary multibyte comment passes
    /// the browser and is refused by the server.
    draft: Option<String>,
    /// Which form the [`draft`](Self::draft) belongs to — `None` for the
    /// top-level one, `Some(id)` for the reply under that comment. A draft
    /// re-filled into every form would duplicate it down the page.
    draft_reply_to: Option<i64>,
    /// Whether this is a later page. Renders the "earlier" link.
    later_page: bool,
    /// The cursor of the next page. Renders the "more" item.
    next_page: Option<i64>,
    /// Text of the link to the next page.
    more_label: String,
    /// Text of the link to the first page.
    earlier_label: String,
}

impl CommentThread {
    /// A thread rendered into `dom_id`, whose comments and replies all post to
    /// `action`.
    ///
    /// `action` is a single endpoint for the whole thread: a reply is an
    /// ordinary comment carrying a `reply_to` value, which is what lets one
    /// framework route serve every commentable model.
    #[must_use]
    pub fn new(dom_id: impl Into<String>, action: impl Into<String>) -> Self {
        let dom_id = dom_id.into();
        let hx_target = format!("#{dom_id}");
        Self {
            dom_id,
            action: action.into(),
            csrf_token: None,
            csrf_field: "_csrf".to_owned(),
            body_field: "body".to_owned(),
            reply_field: "reply_to".to_owned(),
            label: "Comments".to_owned(),
            empty_text: "No comments yet.".to_owned(),
            submit_label: "Post comment".to_owned(),
            reply_label: "Reply".to_owned(),
            placeholder: "Add a comment…".to_owned(),
            hx_target,
            max_depth: crate::widgets::DEFAULT_COMMENT_MAX_DEPTH,
            can_comment: true,
            sign_in_prompt: None,
            return_to: None,
            max_body_bytes: None,
            error: None,
            draft: None,
            draft_reply_to: None,
            later_page: false,
            next_page: None,
            more_label: "More comments".to_owned(),
            earlier_label: "Earlier comments".to_owned(),
        }
    }

    /// A thread whose depth and body caps come straight from the model's
    /// `#[commentable]` declaration.
    ///
    /// Prefer this over [`new`](Self::new) plus hand-copied numbers: it is the
    /// only spelling that cannot drift from the write path, which refuses a
    /// reply past `max_depth` and a body past `max_body` with a `422` the UI
    /// would otherwise have invited.
    #[cfg(feature = "db")]
    #[must_use]
    pub fn from_spec(
        dom_id: impl Into<String>,
        action: impl Into<String>,
        spec: &crate::commentable::CommentableSpec,
    ) -> Self {
        Self::new(dom_id, action)
            .max_depth(usize::try_from(spec.max_depth).unwrap_or(usize::MAX))
            .max_body_bytes(spec.max_body_bytes)
    }

    /// Cap the textarea at `bytes` characters via `maxlength`.
    ///
    /// HTML counts UTF-16 code units where the server counts bytes, so this is
    /// an early, approximate guard — the server's check is still the one that
    /// decides.
    #[must_use]
    pub const fn max_body_bytes(mut self, bytes: usize) -> Self {
        self.max_body_bytes = Some(bytes);
        self
    }

    /// Show a rejection above the form.
    #[must_use]
    pub fn error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }

    /// Re-fill a rejected submission into the form that sent it.
    ///
    /// `reply_to` selects the form: `None` is the top-level one, `Some(id)` the
    /// reply under that comment — which is also opened, since a draft restored
    /// inside a collapsed disclosure looks exactly like a lost one.
    #[must_use]
    pub fn draft(mut self, reply_to: Option<i64>, body: impl Into<String>) -> Self {
        self.draft = Some(body.into());
        self.draft_reply_to = reply_to;
        self
    }

    /// Set the CSRF token embedded in every form.
    #[must_use]
    pub fn csrf_token(mut self, token: impl Into<String>) -> Self {
        self.csrf_token = Some(token.into());
        self
    }

    /// Override the CSRF hidden-input name (default `_csrf`).
    #[must_use]
    pub fn csrf_field(mut self, field: impl Into<String>) -> Self {
        self.csrf_field = field.into();
        self
    }

    /// Override the body textarea's `name` (default `body`).
    #[must_use]
    pub fn body_field(mut self, field: impl Into<String>) -> Self {
        self.body_field = field.into();
        self
    }

    /// Override the reply-target hidden input's `name` (default `reply_to`).
    #[must_use]
    pub fn reply_field(mut self, field: impl Into<String>) -> Self {
        self.reply_field = field.into();
        self
    }

    /// Override the region's `aria-label` (default `Comments`).
    #[must_use]
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// Override the empty-thread text.
    #[must_use]
    pub fn empty_text(mut self, text: impl Into<String>) -> Self {
        self.empty_text = text.into();
        self
    }

    /// Override the top-level submit button's label.
    #[must_use]
    pub fn submit_label(mut self, label: impl Into<String>) -> Self {
        self.submit_label = label.into();
        self
    }

    /// Override the per-node reply label.
    #[must_use]
    pub fn reply_label(mut self, label: impl Into<String>) -> Self {
        self.reply_label = label.into();
        self
    }

    /// Override the textarea placeholder.
    #[must_use]
    pub fn placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// Override the `hx-target` (default `#{dom_id}`).
    #[must_use]
    pub fn hx_target(mut self, target: impl Into<String>) -> Self {
        self.hx_target = target.into();
        self
    }

    /// Deepest level that still offers a reply form; pass the model's
    /// `max_depth` so the UI and the write path agree.
    #[must_use]
    pub const fn max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Render the thread read-only — the signed-out shape. The thread still
    /// renders; every form disappears.
    ///
    /// Pair it with [`sign_in_prompt`](Self::sign_in_prompt) to say why.
    #[must_use]
    pub const fn read_only(mut self) -> Self {
        self.can_comment = false;
        self
    }

    /// What to show in place of the form when the thread is
    /// [`read_only`](Self::read_only).
    #[must_use]
    pub fn sign_in_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.sign_in_prompt = Some(prompt.into());
        self
    }

    /// Render the page links of a paged thread.
    ///
    /// Pass the [`ThreadPage`](crate::commentable::ThreadPage) cursors.
    /// `after` is the cursor that the page starts after. `next` is the cursor
    /// of the next page.
    ///
    /// - `next`: the top-level list ends with a "more" item. With htmx, it
    ///   loads `{action}?after={next}` and puts the new top-level comments in
    ///   its place. Without htmx, it is a link to the host page
    ///   ([`return_to`](Self::return_to)) with `?after={next}`. With no host
    ///   page, the link goes to the router.
    /// - `after`: a link back to the first page comes before the list. Focus
    ///   goes to the first comment of the page.
    ///
    /// The host page must read `after` from its query string and pass it to
    /// `comment_thread_page`. Use the page size of
    /// [`CommentsConfig::page_size`](crate::commentable::CommentsConfig) on
    /// the host page too.
    #[must_use]
    pub const fn page(mut self, after: Option<i64>, next: Option<i64>) -> Self {
        self.later_page = after.is_some();
        self.next_page = next;
        self
    }

    /// Override the text of the link to the next page (default
    /// `More comments`).
    #[must_use]
    pub fn more_label(mut self, label: impl Into<String>) -> Self {
        self.more_label = label.into();
        self
    }

    /// Override the text of the link to the first page (default
    /// `Earlier comments`).
    #[must_use]
    pub fn earlier_label(mut self, label: impl Into<String>) -> Self {
        self.earlier_label = label.into();
        self
    }

    /// The no-JS address of the page after `after` (the first page when
    /// `None`): the host page when there is one, else the router.
    fn page_href(&self, after: Option<i64>) -> String {
        with_page_cursor(self.return_to.as_deref().unwrap_or(&self.action), after)
    }

    /// The htmx address of the page after `after`: always the router. It
    /// carries `return_to`, so the forms it renders keep it.
    fn page_hx_get(&self, after: Option<i64>) -> String {
        let mut url = with_page_cursor(&self.action, after);
        if let Some(return_to) = &self.return_to {
            let encoded = percent_encoding::utf8_percent_encode(
                return_to,
                percent_encoding::NON_ALPHANUMERIC,
            )
            .to_string();
            url = with_query(&url, "return_to", &encoded);
        }
        url
    }

    /// Where a submit **without** htmx should send the browser afterwards,
    /// carried as a hidden `return_to` input.
    ///
    /// Pass the host page's own path (`/r/rust/posts/hello`). The framework
    /// router honours it only when it is a relative, single-slash path, so a
    /// tampered value cannot become an open redirect — but it is still the host
    /// page, not the visitor, that should be choosing it.
    #[must_use]
    pub fn return_to(mut self, path: impl Into<String>) -> Self {
        self.return_to = Some(path.into());
        self
    }
}

/// `path` with `key=value` added to its query string. `value` must already be
/// URL-safe. `path` must have no fragment.
fn with_query(path: &str, key: &str, value: &str) -> String {
    let separator = if path.contains('?') { '&' } else { '?' };
    format!("{path}{separator}{key}={value}")
}

/// `url` with its comment-page cursor set to `after`.
///
/// The function removes each `after` pair that `url` has, then adds
/// `after={after}` when `after` is `Some`. A `#fragment` stays at the end.
/// Thus a host page can pass its current address as `return_to`.
pub(crate) fn with_page_cursor(url: &str, after: Option<i64>) -> String {
    let (address, fragment) = match url.split_once('#') {
        Some((address, fragment)) => (address, Some(fragment)),
        None => (url, None),
    };
    let (path, query) = address.split_once('?').unwrap_or((address, ""));
    let cursor = after.map(|after| format!("after={after}"));
    let pairs: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty() && *pair != "after" && !pair.starts_with("after="))
        .chain(cursor.as_deref())
        .collect();
    let mut out = path.to_owned();
    if !pairs.is_empty() {
        out.push('?');
        out.push_str(&pairs.join("&"));
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// The default deepest level offering a reply form, matching
/// `#[commentable]`'s own `max_depth` default.
pub const DEFAULT_COMMENT_MAX_DEPTH: usize = 5;

/// Render a nested comment thread with an inline reply form on every node.
///
/// No JavaScript is required for any of it: the reply forms are ordinary
/// `POST` forms inside `<details>` disclosures, so the widget works with
/// scripting off. When htmx *is* present, every form additionally carries
/// `hx-post` / `hx-target` / `hx-swap="outerHTML"`, so submitting a reply
/// replaces the whole `#{dom_id}` region with the re-rendered thread — no full
/// page reload — as long as the handler responds with this same widget.
///
/// The handler owns the pairing: render `comment_thread` for both the initial
/// page and the `POST` response, and the swap is seamless.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{CommentThread, CommentView, comment_thread};
///
/// let views = vec![CommentView {
///     id: 1,
///     author: "ada".into(),
///     body: "first!".into(),
///     datetime: None,
///     timestamp: String::new(),
///     replies: vec![CommentView {
///         id: 2,
///         author: "grace".into(),
///         body: "second".into(),
///         datetime: None,
///         timestamp: String::new(),
///         replies: Vec::new(),
///     }],
/// }];
/// let cfg = CommentThread::new("comments-post-42", "/comments/Post/42").csrf_token("tok");
/// let html = comment_thread(&cfg, &views).into_string();
///
/// assert!(html.contains(r#"id="comments-post-42""#));
/// assert!(html.contains(r##"hx-target="#comments-post-42""##));
/// assert!(html.contains(r#"name="reply_to" value="1""#));  // inline reply form per node
/// assert!(html.contains(r#"name="reply_to" value="2""#));
/// assert!(html.contains("first!"));
/// ```
#[must_use]
pub fn comment_thread(cfg: &CommentThread, comments: &[CommentView]) -> maud::Markup {
    // Every class is a plain string literal (never `format!`), so the
    // widget-CSS coverage gate can extract it and prove `widgets.css` backs it.
    maud::html! {
        section id=(cfg.dom_id) class="autumn-comments" role="region" aria-label=(cfg.label) {
            @if let Some(error) = &cfg.error {
                p class="autumn-comments-error" role="alert" { (error) }
            }
            @if cfg.later_page {
                p class="autumn-comments-earlier" {
                    a href=(cfg.page_href(None))
                        hx-get=(cfg.page_hx_get(None))
                        hx-target=(cfg.hx_target)
                        hx-swap="outerHTML" { (cfg.earlier_label) }
                }
            }
            @if comments.is_empty() {
                // A later page can be empty when its cursor is stale. The
                // thread is not empty, so the empty text would be wrong.
                @if !cfg.later_page {
                    p class="autumn-comments-empty" { (cfg.empty_text) }
                }
            } @else {
                (comment_list(cfg, comments, 0))
            }
            @if cfg.can_comment {
                (comment_form(cfg, None, None))
            } @else if let Some(prompt) = &cfg.sign_in_prompt {
                p class="autumn-comments-prompt" { (prompt) }
            }
        }
    }
}

/// One `<ol>` level of the thread.
///
/// The top level is `aria-live="polite"`, so an htmx swap — which replaces the
/// whole region without moving focus — is announced rather than silent.
fn comment_list(cfg: &CommentThread, comments: &[CommentView], depth: usize) -> maud::Markup {
    let live = (depth == 0).then_some("polite");
    let next = cfg.next_page.filter(|_| depth == 0);
    maud::html! {
        ol class="autumn-comment-list" aria-live=[live] {
            @for (index, comment) in comments.iter().enumerate() {
                // On a later page, focus moves to its first comment. The
                // "more" link that had focus is gone after the htmx swap.
                (comment_node(cfg, comment, depth, cfg.later_page && depth == 0 && index == 0))
            }
            @if let Some(next) = next {
                // htmx puts the next page's top-level items in place of this
                // one. `hx-select` takes them from the router's response.
                li class="autumn-comments-more" {
                    a href=(cfg.page_href(Some(next)))
                        hx-get=(cfg.page_hx_get(Some(next)))
                        hx-target="closest li"
                        hx-swap="outerHTML"
                        hx-select=(format!("[id=\"{}\"] > ol > li", cfg.dom_id)) { (cfg.more_label) }
                }
            }
        }
    }
}

/// The `id` [`comment_thread`] renders on one comment's `<li>`.
///
/// Exposed because anything that wants to *link* to a comment — a notification,
/// an email, a live-feed entry — needs the same string, and hand-assembling it
/// at each call site is how a link silently stops matching the markup.
#[must_use]
pub fn comment_dom_id(thread_dom_id: &str, comment_id: i64) -> String {
    format!("{thread_dom_id}-c{comment_id}")
}

/// One comment, its reply affordance, and its own replies.
/// Split a stored comment body into paragraphs on blank lines.
///
/// Splitting on `"\n\n"` alone was wrong for the ordinary case: an HTML form
/// submits a textarea with CRLF line endings, so a blank line arrives as
/// `"\r\n\r\n"` and the whole comment rendered as ONE paragraph — the break
/// collapsing to a space, because the CSS does not preserve whitespace.
///
/// `str::lines` is what makes this line-ending agnostic: it splits on `\n` and
/// strips a trailing `\r`, so LF and CRLF bodies produce identical paragraphs.
/// Runs of blank lines collapse to a single break rather than empty `<p>`s.
fn comment_paragraphs(body: &str) -> Vec<String> {
    body.lines()
        .collect::<Vec<_>>()
        .split(|line| line.trim().is_empty())
        .filter(|group| group.iter().any(|line| !line.trim().is_empty()))
        .map(|group| group.join("\n").trim().to_string())
        .collect()
}

fn comment_node(
    cfg: &CommentThread,
    comment: &CommentView,
    depth: usize,
    focus: bool,
) -> maud::Markup {
    let node_id = comment_dom_id(&cfg.dom_id, comment.id);
    // The write path refuses a reply deeper than `max_depth`, so offering the
    // form past it would be an invitation to a 422. `depth` is the parent's
    // level, and the reply lands one below.
    let can_reply = cfg.can_comment && depth < cfg.max_depth;
    // Distinct accessible names per node. Without the author, every disclosure
    // in a forty-comment thread announces the identical "Reply" and a screen
    // reader user has no way to tell which one they are on.
    let reply_label = format!("{} to {}", cfg.reply_label, comment.author);
    maud::html! {
        li id=(node_id) class="autumn-comment" tabindex=[focus.then_some("-1")] autofocus[focus] {
            div class="autumn-comment-meta" {
                span class="autumn-comment-author" { (comment.author) }
                @if let Some(datetime) = &comment.datetime {
                    " · "
                    time class="autumn-comment-time" datetime=(datetime) { (comment.timestamp) }
                }
            }
            div class="autumn-comment-body" {
                @for paragraph in comment_paragraphs(&comment.body) {
                    p { (paragraph) }
                }
            }
            @if can_reply {
                details class="autumn-comment-reply"
                    open[cfg.draft.is_some() && cfg.draft_reply_to == Some(comment.id)] {
                    summary class="autumn-comment-reply-toggle" aria-label=(reply_label) {
                        (cfg.reply_label)
                    }
                    (comment_form(cfg, Some(comment.id), Some(&comment.author)))
                }
            }
            @if !comment.replies.is_empty() {
                (comment_list(cfg, &comment.replies, depth + 1))
            }
        }
    }
}

/// The shared comment/reply form. `reply_to` distinguishes the two.
fn comment_form(
    cfg: &CommentThread,
    reply_to: Option<i64>,
    reply_to_author: Option<&str>,
) -> maud::Markup {
    let textarea_id = reply_to.map_or_else(
        || format!("{}-body", cfg.dom_id),
        |id| format!("{}-c{id}-body", cfg.dom_id),
    );
    let submit_label = if reply_to.is_some() {
        &cfg.reply_label
    } else {
        &cfg.submit_label
    };
    // Same reason as the disclosure's: one accessible name per form, so the
    // textarea says which comment it replies to.
    let field_label = reply_to_author.map_or_else(
        || cfg.placeholder.clone(),
        |author| format!("{} to {author}", cfg.reply_label),
    );
    let maxlength = cfg.max_body_bytes.map(|bytes| bytes.to_string());
    // One sync scope for the whole thread, `replace`: two quick replies would
    // otherwise race, and the older `outerHTML` response landing second would
    // drop the newer comment from view. The commits are already ordered by the
    // parent row lock; this orders the *responses*.
    let hx_sync = format!("#{}:replace", cfg.dom_id);
    maud::html! {
        form class="autumn-comment-form" method="post" action=(cfg.action)
            hx-post=(cfg.action) hx-target=(cfg.hx_target) hx-swap="outerHTML"
            hx-sync=(hx_sync) {
            @if let Some(token) = &cfg.csrf_token {
                input type="hidden" name=(cfg.csrf_field) value=(token);
            }
            @if let Some(reply_to) = reply_to {
                input type="hidden" name=(cfg.reply_field) value=(reply_to);
            }
            @if let Some(return_to) = &cfg.return_to {
                input type="hidden" name="return_to" value=(return_to);
            }
            label class="autumn-comment-label" for=(textarea_id) { (field_label) }
            textarea id=(textarea_id) class="autumn-comment-input" name=(cfg.body_field)
                rows="3" required maxlength=[maxlength] placeholder=(cfg.placeholder) {
                // Only the form that was actually submitted: `draft_reply_to`
                // is `None` for the top-level form and `Some(id)` for a reply,
                // which is exactly how `reply_to` identifies this one. Maud
                // escapes the text, so a body full of markup is inert.
                @if let Some(draft) = &cfg.draft
                    && reply_to == cfg.draft_reply_to
                {
                    (draft)
                }
            }
            button type="submit" class="autumn-comment-submit" { (submit_label) }
        }
    }
}

// ── infinite_feed (#1372) ───────────────────────────────────────────────────

/// How the continuation control of an [`infinite_feed`] loads the next page.
///
/// Both modes replace the sentinel with the next page's fragment (its items
/// plus a fresh sentinel), so appends never duplicate already-rendered rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FeedMode {
    /// Auto-load when the sentinel scrolls into view, and also on a manual
    /// click of the visible link (`hx-trigger="revealed, click"`). The classic
    /// infinite scroll. This is the default.
    #[default]
    Reveal,
    /// Require an explicit click on the keyboard-focusable "Load more" control
    /// (htmx's default `click` trigger); nothing loads until the user asks.
    Button,
}

/// Configuration for [`infinite_feed`] / [`feed_page`].
///
/// Build with [`FeedConfig::new`] and chain setters. `url` is the partial
/// handler that returns the next page fragment; the next cursor is appended to
/// it as a query parameter (`?cursor=…` by default).
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{FeedConfig, FeedMode};
///
/// let config = FeedConfig::new("/posts/feed")
///     .mode(FeedMode::Button)
///     .load_more_label("Show more posts");
/// assert_eq!(config.cursor_param, "cursor");
/// ```
#[derive(Debug, Clone)]
pub struct FeedConfig<'a> {
    /// URL of the partial handler that returns the next page fragment.
    pub url: &'a str,
    /// How the continuation control loads (default [`FeedMode::Reveal`]).
    pub mode: FeedMode,
    /// Query-parameter name carrying the cursor token (default `"cursor"`).
    pub cursor_param: &'a str,
    /// Visible label / accessible name of the continuation control
    /// (default `"Load more"`).
    pub load_more_label: &'a str,
}

impl<'a> FeedConfig<'a> {
    /// Start a config for the partial handler at `url` with sensible defaults
    /// (reveal mode, `cursor` param, "Load more" label).
    #[must_use]
    pub const fn new(url: &'a str) -> Self {
        Self {
            url,
            mode: FeedMode::Reveal,
            cursor_param: "cursor",
            load_more_label: "Load more",
        }
    }

    /// Set the load [`FeedMode`].
    #[must_use]
    pub const fn mode(mut self, mode: FeedMode) -> Self {
        self.mode = mode;
        self
    }

    /// Shorthand for `.mode(FeedMode::Button)`.
    #[must_use]
    pub const fn button(mut self) -> Self {
        self.mode = FeedMode::Button;
        self
    }

    /// Override the cursor query-parameter name (default `"cursor"`).
    #[must_use]
    pub const fn cursor_param(mut self, param: &'a str) -> Self {
        self.cursor_param = param;
        self
    }

    /// Override the continuation control's label (default `"Load more"`).
    #[must_use]
    pub const fn load_more_label(mut self, label: &'a str) -> Self {
        self.load_more_label = label;
        self
    }
}

/// Percent-encode a query-string component (param name or value).
///
/// Cursor tokens produced by [`CursorPage`](crate::pagination::CursorPage) are
/// URL-safe base64 (signed cursors add a `.`-delimited signature, also
/// URL-safe), but [`infinite_feed`]/[`feed_page`] accept an arbitrary
/// `Option<&str>`, so a token containing `&`, `#`, `=`, `+`, or a space must be
/// encoded to avoid corrupting the query string or injecting extra params. The
/// unreserved set (`A–Z a–z 0–9 - _ . ~`) — which covers every real cursor —
/// passes through unchanged.
fn encode_query_component(value: &str) -> String {
    use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
    // Everything outside the RFC 3986 unreserved set.
    const QUERY_COMPONENT: &AsciiSet = &CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'%')
        .add(b'&')
        .add(b'+')
        .add(b'/')
        .add(b';')
        .add(b'=')
        .add(b'?')
        .add(b'@')
        .add(b'[')
        .add(b'\\')
        .add(b']')
        .add(b'^')
        .add(b'`')
        .add(b'{')
        .add(b'|')
        .add(b'}')
        .add(b'<')
        .add(b'>');
    utf8_percent_encode(value, QUERY_COMPONENT).to_string()
}

/// Build the next-page URL by appending `?<cursor_param>=<cursor>` to the config
/// URL (using `&` if it already has a query string), with both the param name
/// and the cursor percent-encoded via [`encode_query_component`]. Any existing
/// `#fragment` is preserved after the query (the query must precede it). Maud
/// additionally HTML-escapes the whole value in the attribute.
fn feed_next_url(config: &FeedConfig<'_>, cursor: &str) -> String {
    // The query must go before any `#fragment`.
    let (base, fragment) = match config.url.split_once('#') {
        Some((base, frag)) => (base, Some(frag)),
        None => (config.url, None),
    };
    let sep = if base.contains('?') { '&' } else { '?' };
    let param = encode_query_component(config.cursor_param);
    let value = encode_query_component(cursor);
    let mut url = format!("{base}{sep}{param}={value}");
    if let Some(frag) = fragment {
        url.push('#');
        url.push_str(frag);
    }
    url
}

/// Render the continuation sentinel for a `next_cursor`.
///
/// A single control that is BOTH a progressive-enhancement `<a href>` to the
/// next-cursor URL (works with htmx/JS unavailable) AND htmx-wired to fetch
/// that URL and replace the whole sentinel (`hx-swap="outerHTML"`, targeting
/// the closest `.autumn-feed__sentinel`) with the returned fragment. In
/// [`FeedMode::Reveal`] it fires on `revealed, click` — auto-loading when
/// scrolled into view, but also swapping in place (rather than navigating to
/// the bare partial) if a user activates the visible link first; in
/// [`FeedMode::Button`] it uses htmx's default `click` trigger. htmx
/// `preventDefault`s the anchor navigation whenever it handles the trigger, so
/// the `<a href>` stays a pure no-JS fallback.
fn feed_sentinel(next_cursor: &str, config: &FeedConfig<'_>) -> maud::Markup {
    let next_url = feed_next_url(config, next_cursor);
    let trigger = match config.mode {
        // Include `click` so a user activating the visible "Load more" link
        // before `revealed` fires swaps in place instead of following `href`.
        FeedMode::Reveal => Some("revealed, click"),
        FeedMode::Button => None,
    };
    maud::html! {
        div class="autumn-feed__sentinel" {
            a class="autumn-feed__more" href=(next_url)
                hx-get=(next_url) hx-target="closest .autumn-feed__sentinel"
                hx-swap="outerHTML" hx-trigger=[trigger] {
                (config.load_more_label)
            }
        }
    }
}

/// Render just the next-page fragment: the already-rendered `items` followed by
/// a fresh sentinel (only when `next_cursor` is `Some`).
///
/// This is the companion helper a handler returns for **each append** — and it
/// is exactly what [`infinite_feed`] wraps for the initial view, so one partial
/// serves both. Because the sentinel swaps itself via `outerHTML`, returning
/// `items + new sentinel` inserts the new rows where the old sentinel was and
/// re-arms the loop; when `next_cursor` is `None` no sentinel is emitted, so no
/// further request fires (the feed terminates cleanly).
///
/// `items` is caller-rendered [`Markup`](maud::Markup) (already escaped by your
/// own Maud); pass the cursor from
/// [`CursorPage::next_cursor`](crate::pagination::CursorPage) via `.as_deref()`.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{feed_page, FeedConfig};
/// use maud::html;
///
/// let items = html! { article { "row 1" } article { "row 2" } };
/// let config = FeedConfig::new("/feed");
/// // Middle page: has a next cursor → emits a sentinel.
/// let more = feed_page(items, Some("eyJpZCI6Mn0"), &config).into_string();
/// assert!(more.contains("autumn-feed__sentinel"));
/// assert!(more.contains(r##"hx-get="/feed?cursor=eyJpZCI6Mn0""##));
///
/// // Last page: no next cursor → no sentinel, no further request.
/// let last = feed_page(html! { article { "row 3" } }, None, &config).into_string();
/// assert!(!last.contains("autumn-feed__sentinel"));
/// ```
// `items` is taken by value to keep `feed_page(html! { .. }, ..)` ergonomic (no
// `&` at the call site, matching `alert`); Maud renders it by reference, hence
// the allow.
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn feed_page(
    items: maud::Markup,
    next_cursor: Option<&str>,
    config: &FeedConfig<'_>,
) -> maud::Markup {
    maud::html! {
        (items)
        @if let Some(cursor) = next_cursor {
            (feed_sentinel(cursor, config))
        }
    }
}

/// Render an htmx infinite-scroll / "Load more" feed for a cursor-paginated
/// list: a `.autumn-feed` container holding the rendered `items` and, when
/// there is a next page, a continuation sentinel.
///
/// Driven by a [`CursorPage`](crate::pagination::CursorPage): pass your rendered
/// items and `page.next_cursor.as_deref()`. When the cursor is `Some`, the
/// sentinel issues a single `hx-get` to `config.url` carrying the cursor and
/// appends the returned fragment in place — no full-page reload, no duplicated
/// rows (keyset cursors are stable under concurrent inserts). When it is `None`
/// (`has_next == false`) no sentinel is emitted, so no terminal request fires.
///
/// Two modes ([`FeedMode`]): auto-load on reveal, or an explicit,
/// keyboard-focusable "Load more" control. Either way the control is a real
/// `<a href>` to the next-cursor URL, so the next page is reachable with
/// htmx/JS unavailable. The append fragment is produced by [`feed_page`], which
/// a handler returns for every subsequent page.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-feed` | Feed container |
/// | `.autumn-feed__sentinel` | Continuation sentinel (swapped out on load) |
/// | `.autumn-feed__more` | The `<a>`/"Load more" control |
///
/// # Progressive enhancement caveat
///
/// The `<a href>` fallback points at your partial handler (`config.url`), which
/// normally returns just the next-page *fragment*. With JavaScript off, clicking
/// it therefore lands on that bare fragment unless the handler detects a
/// non-htmx request (no `HX-Request` header) and renders a full page for it.
///
/// # Example: initial view + append handler
///
/// ```rust
/// use autumn_web::widgets::{infinite_feed, feed_page, FeedConfig, FeedMode};
/// use autumn_web::pagination::CursorPage;
/// use maud::html;
///
/// // Render each item however you like (this is caller-owned Markup):
/// fn render(rows: &[&str]) -> maud::Markup {
///     html! { @for r in rows { article class="post" { (r) } } }
/// }
///
/// // Page 1 (has a next page):
/// let page = CursorPage { content: vec!["a", "b"], size: 2,
///     next_cursor: Some("eyJpZCI6Mn0".into()), has_next: true };
/// let config = FeedConfig::new("/posts/feed").mode(FeedMode::Reveal);
/// let view = infinite_feed(render(&page.content), page.next_cursor.as_deref(), &config)
///     .into_string();
/// assert!(view.contains(r#"class="autumn-feed""#));
/// assert!(view.contains(r#"hx-trigger="revealed, click""#));
/// assert!(view.contains(r##"hx-get="/posts/feed?cursor=eyJpZCI6Mn0""##));
///
/// // Your `GET /posts/feed?cursor=…` handler returns ONE partial for each
/// // append — the same fragment shape as above, minus the outer container:
/// let last = CursorPage::<&str> { content: vec![], size: 2, next_cursor: None, has_next: false };
/// let append = feed_page(render(&last.content), last.next_cursor.as_deref(), &config)
///     .into_string();
/// assert!(!append.contains("autumn-feed__sentinel")); // last page → loop stops
/// ```
#[must_use]
pub fn infinite_feed(
    items: maud::Markup,
    next_cursor: Option<&str>,
    config: &FeedConfig<'_>,
) -> maud::Markup {
    maud::html! {
        div class="autumn-feed" {
            (feed_page(items, next_cursor, config))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A browser submits a textarea with CRLF, so paragraph splitting must not
    /// depend on the line ending. Splitting on `"\n\n"` alone rendered an
    /// ordinary two-paragraph comment as one.
    #[test]
    fn comment_paragraphs_split_on_blank_lines_of_either_line_ending() {
        let expected = vec!["first".to_string(), "second".to_string()];
        assert_eq!(comment_paragraphs("first\n\nsecond"), expected, "LF");
        assert_eq!(
            comment_paragraphs("first\r\n\r\nsecond"),
            expected,
            "CRLF — the case a real form submission produces"
        );
        // A run of blank lines is one break, not several empty paragraphs.
        assert_eq!(comment_paragraphs("first\r\n\r\n\r\n\r\nsecond"), expected);
        // A single newline stays INSIDE one paragraph, as before.
        assert_eq!(
            comment_paragraphs("one\r\ntwo"),
            vec!["one\ntwo".to_string()]
        );
        // Nothing but whitespace yields no paragraphs at all.
        assert!(comment_paragraphs("   \r\n\r\n  ").is_empty());
        assert!(comment_paragraphs("").is_empty());
    }

    /// A 422 re-renders the thread and htmx swaps `outerHTML`, so a draft that
    /// is not carried back is simply gone from the visitor's screen. It must
    /// land in the form that SENT it, and nowhere else.
    #[test]
    fn a_rejected_draft_is_refilled_into_the_form_that_sent_it() {
        let view = vec![CommentView {
            id: 7,
            author: "ada".into(),
            body: "parent".into(),
            datetime: None,
            timestamp: String::new(),
            replies: Vec::new(),
        }];

        // Top-level draft: the top-level textarea carries it, the reply does not.
        let widget = CommentThread::new("t", "/c").draft(None, "my long draft");
        let html = comment_thread(&widget, &view).into_string();
        assert_eq!(
            html.matches("my long draft").count(),
            1,
            "exactly one form is prefilled:\n{html}"
        );

        // Reply draft: it lands in that comment's reply form, which is OPEN —
        // a draft restored inside a collapsed disclosure reads as a lost one.
        let widget = CommentThread::new("t", "/c").draft(Some(7), "reply draft");
        let html = comment_thread(&widget, &view).into_string();
        assert_eq!(html.matches("reply draft").count(), 1, "{html}");
        assert!(
            html.contains("<details class=\"autumn-comment-reply\" open"),
            "{html}"
        );

        // No draft: every textarea is empty, so a SUCCESSFUL post never invites
        // the visitor to submit the same comment twice.
        let widget = CommentThread::new("t", "/c");
        let html = comment_thread(&widget, &view).into_string();
        assert!(html.contains("></textarea>"), "{html}");
        assert!(
            !html.contains("<details class=\"autumn-comment-reply\" open"),
            "{html}"
        );

        // The body is TEXT, not markup: maud escapes it.
        let widget = CommentThread::new("t", "/c").draft(None, "<script>x</script>");
        let html = comment_thread(&widget, &view).into_string();
        assert!(!html.contains("<script>x</script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    // ── comment_thread (#1367) ─────────────────────────────────────────

    /// A two-level fixture: one root with one reply.
    fn comment_fixture() -> Vec<CommentView> {
        vec![
            CommentView {
                id: 1,
                author: "ada".to_owned(),
                body: "first\n\nsecond para".to_owned(),
                datetime: Some("2026-06-21T17:10:33Z".to_owned()),
                timestamp: "2026-06-21 17:10".to_owned(),
                replies: vec![CommentView {
                    id: 2,
                    author: "grace".to_owned(),
                    body: "a reply".to_owned(),
                    datetime: None,
                    timestamp: String::new(),
                    replies: Vec::new(),
                }],
            },
            CommentView {
                id: 3,
                author: "hopper".to_owned(),
                body: "unrelated".to_owned(),
                datetime: None,
                timestamp: String::new(),
                replies: Vec::new(),
            },
        ]
    }

    /// AC4: the list nests, and **every** node carries its own inline reply
    /// form pre-addressed to that node.
    #[test]
    fn comment_thread_renders_an_inline_reply_form_per_node() {
        let cfg = CommentThread::new("comments-post-42", "/comments/Post/42").csrf_token("tok");
        let html = comment_thread(&cfg, &comment_fixture()).into_string();

        assert!(html.contains(r#"id="comments-post-42""#));
        assert!(html.contains(r#"class="autumn-comments""#));
        // One reply form per node, each naming its own target.
        for id in [1, 2, 3] {
            assert!(
                html.contains(&format!(r#"name="reply_to" value="{id}""#)),
                "no inline reply form addressed to comment {id}"
            );
        }
        // Plus the top-level form, which carries no reply target.
        assert_eq!(html.matches(r#"class="autumn-comment-form""#).count(), 4);
        // The CSRF field is on every one of them.
        assert_eq!(html.matches(r#"name="_csrf" value="tok""#).count(), 4);
    }

    /// AC4: submitting swaps the thread in place rather than reloading — and
    /// the same forms are ordinary `POST`s when htmx is absent.
    #[test]
    fn comment_thread_forms_are_htmx_swaps_and_plain_posts() {
        let cfg = CommentThread::new("comments-post-42", "/comments/Post/42");
        let html = comment_thread(&cfg, &comment_fixture()).into_string();

        assert!(html.contains(r##"hx-target="#comments-post-42""##));
        assert!(html.contains(r#"hx-swap="outerHTML""#));
        assert!(html.contains(r#"hx-post="/comments/Post/42""#));
        // No-JS: the same element is a real form with a real action.
        assert!(html.contains(r#"method="post" action="/comments/Post/42""#));
        // ...and the disclosure is `<details>`, not a script-driven toggle.
        assert!(html.contains("<details"));
    }

    /// Nesting is rendered as nested `<ol>`s, so depth is exposed to assistive
    /// technology rather than being a purely visual indent.
    #[test]
    fn comment_thread_nests_replies_in_a_child_list() {
        let cfg = CommentThread::new("c", "/comments/Post/1");
        let html = comment_thread(&cfg, &comment_fixture()).into_string();
        assert_eq!(html.matches(r#"class="autumn-comment-list""#).count(), 2);
        assert!(
            html.contains(r#"id="c-c2""#),
            "the reply gets its own node id"
        );
    }

    /// Blank-line-separated paragraphs render as separate `<p>`s, and the body
    /// is escaped rather than parsed as HTML.
    #[test]
    fn comment_thread_escapes_bodies_and_splits_paragraphs() {
        let views = vec![CommentView {
            id: 1,
            author: "<script>".to_owned(),
            body: "<img src=x onerror=alert(1)>\n\ntwo".to_owned(),
            datetime: None,
            timestamp: String::new(),
            replies: Vec::new(),
        }];
        let cfg = CommentThread::new("c", "/comments/Post/1");
        let html = comment_thread(&cfg, &views).into_string();
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&lt;img src=x"));
        assert!(html.contains("&lt;script&gt;"));
        assert_eq!(html.matches("<p>").count(), 2);
    }

    /// The UI must not offer a reply the write path would reject, so the
    /// disclosure stops at `max_depth`.
    #[test]
    fn comment_thread_stops_offering_replies_past_max_depth() {
        let deep = vec![CommentView {
            id: 1,
            author: "ada".to_owned(),
            body: "root".to_owned(),
            datetime: None,
            timestamp: String::new(),
            replies: vec![CommentView {
                id: 2,
                author: "ada".to_owned(),
                body: "child".to_owned(),
                datetime: None,
                timestamp: String::new(),
                replies: Vec::new(),
            }],
        }];
        let cfg = CommentThread::new("c", "/comments/Post/1").max_depth(1);
        let html = comment_thread(&cfg, &deep).into_string();
        assert!(html.contains(r#"name="reply_to" value="1""#));
        assert!(
            !html.contains(r#"name="reply_to" value="2""#),
            "a reply to the depth-1 node would exceed max_depth = 1"
        );
    }

    /// `from_thread` is the only bridge between the database rows and the
    /// widget, and it carries three documented behaviours: the `user #{id}`
    /// fallback, the RFC 3339 `datetime` attribute, and the recursive mapping.
    #[cfg(feature = "db")]
    #[test]
    fn comment_view_from_thread_maps_names_timestamps_and_replies() {
        use crate::commentable::{Comment, CommentNode};

        fn node(id: i64, author_name: Option<&str>, replies: Vec<CommentNode>) -> CommentNode {
            CommentNode {
                comment: Comment {
                    id,
                    parent_id: None,
                    author_id: 7,
                    body: "b".to_owned(),
                    created_at: chrono::DateTime::parse_from_rfc3339("2026-06-21T17:10:33Z")
                        .expect("timestamp")
                        .naive_utc(),
                    author_name: author_name.map(str::to_owned),
                },
                depth: 0,
                replies,
            }
        }

        let views = CommentView::from_thread(&[node(
            1,
            Some("ada"),
            vec![node(2, None, vec![node(3, Some("grace"), Vec::new())])],
        )]);

        assert_eq!(views[0].author, "ada");
        // No `author_name` column declared (or a missing author): the id, not
        // an invented name.
        assert_eq!(views[0].replies[0].author, "user #7");
        // Nesting survives to arbitrary depth.
        assert_eq!(views[0].replies[0].replies[0].author, "grace");
        assert_eq!(
            views[0].datetime.as_deref(),
            Some("2026-06-21T17:10:33Z"),
            "a machine-readable timestamp for <time datetime=…>"
        );
        assert_eq!(views[0].timestamp, "2026-06-21 17:10");
    }

    /// A signed-out visitor still reads the thread; the form is replaced by a
    /// prompt, and no node offers a reply.
    #[test]
    fn comment_thread_read_only_hides_every_form() {
        let cfg = CommentThread::new("c", "/comments/Post/1")
            .read_only()
            .sign_in_prompt("Sign in to comment.");
        let html = comment_thread(&cfg, &comment_fixture()).into_string();
        assert!(!html.contains("<form"));
        assert!(html.contains("Sign in to comment."));
        assert!(
            html.contains("unrelated"),
            "the thread itself still renders"
        );
    }

    #[test]
    fn comment_thread_renders_the_empty_state() {
        let cfg = CommentThread::new("c", "/comments/Post/1").empty_text("Nothing here.");
        let html = comment_thread(&cfg, &[]).into_string();
        assert!(html.contains("Nothing here."));
        assert!(html.contains(r#"class="autumn-comments-empty""#));
        assert!(
            html.contains("<form"),
            "an empty thread still invites a first comment"
        );
    }

    /// Each node's reply affordance names the comment it replies to. Without
    /// that, forty disclosures in a thread all announce the identical "Reply"
    /// and a screen-reader user cannot tell which one they are on.
    #[test]
    fn comment_thread_gives_every_reply_control_a_distinct_accessible_name() {
        let cfg = CommentThread::new("c", "/comments/Post/1");
        let html = comment_thread(&cfg, &comment_fixture()).into_string();
        assert!(html.contains(r#"aria-label="Reply to ada""#), "{html}");
        assert!(html.contains(r#"aria-label="Reply to grace""#), "{html}");
        assert!(html.contains(r#"aria-label="Reply to hopper""#), "{html}");
        // The textarea's own label says it too, so the form is identifiable
        // without the disclosure.
        assert!(html.contains(">Reply to ada</label>"), "{html}");
        // The swap replaces the whole region without moving focus, so the list
        // announces politely rather than silently.
        assert!(html.contains(r#"aria-live="polite""#), "{html}");
        assert!(html.contains(r#"role="region""#), "{html}");
    }

    /// Two quick replies would otherwise race, and the older `outerHTML`
    /// response landing second would drop the newer comment from view.
    #[test]
    fn comment_thread_forms_share_one_htmx_sync_scope() {
        let cfg = CommentThread::new("comments-post-42", "/comments/Post/42");
        let html = comment_thread(&cfg, &comment_fixture()).into_string();
        assert_eq!(
            html.matches(r##"hx-sync="#comments-post-42:replace""##)
                .count(),
            4,
            "every form, so no pair of them can race:\n{html}"
        );
    }

    /// The UI must not invite a body the write path would reject with a `422`
    /// htmx would not even swap.
    #[test]
    fn comment_thread_caps_the_textarea_at_the_models_body_limit() {
        let cfg = CommentThread::new("c", "/comments/Post/1").max_body_bytes(280);
        let html = comment_thread(&cfg, &comment_fixture()).into_string();
        assert_eq!(html.matches(r#"maxlength="280""#).count(), 4, "{html}");

        // No cap declared, no attribute — the server still decides.
        let plain = comment_thread(
            &CommentThread::new("c", "/comments/Post/1"),
            &comment_fixture(),
        )
        .into_string();
        assert!(!plain.contains("maxlength"), "{plain}");
    }

    /// A rejected comment is shown, not swallowed: htmx does not swap a
    /// non-2xx response, so the router re-renders with the message instead.
    #[test]
    fn comment_thread_renders_a_rejection_above_the_form() {
        let cfg = CommentThread::new("c", "/comments/Post/1").error("Comment cannot be empty");
        let html = comment_thread(&cfg, &comment_fixture()).into_string();
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("Comment cannot be empty"), "{html}");
        assert!(html.contains(r#"class="autumn-comments-error""#), "{html}");
    }

    /// `return_to` is the no-JS round trip: the host page names where to come
    /// back to, and it is carried on every form.
    #[test]
    fn comment_thread_carries_return_to_on_every_form() {
        let cfg = CommentThread::new("c", "/comments/Post/1").return_to("/r/rust/posts/hello");
        let html = comment_thread(&cfg, &comment_fixture()).into_string();
        assert_eq!(
            html.matches(r#"name="return_to" value="/r/rust/posts/hello""#)
                .count(),
            4
        );
    }

    // ── comment_thread paging (#3325) ──────────────────────────────────

    /// A page with more after it ends its top-level list with a "More" item.
    /// htmx loads the next page into that item. Without htmx, it is a link.
    #[test]
    fn comment_thread_links_the_next_page() {
        let cfg = CommentThread::new("c", "/comments/Post/1").page(None, Some(3));
        let html = comment_thread(&cfg, &comment_fixture()).into_string();
        assert!(
            html.contains(r#"<li class="autumn-comments-more">"#),
            "{html}"
        );
        assert!(
            html.contains(r#"href="/comments/Post/1?after=3""#),
            "{html}"
        );
        assert!(
            html.contains(r#"hx-get="/comments/Post/1?after=3""#),
            "{html}"
        );
        assert!(html.contains(r#"hx-target="closest li""#), "{html}");
        assert!(
            html.contains(r#"hx-select="[id=&quot;c&quot;] &gt; ol &gt; li""#),
            "{html}"
        );
        assert!(html.contains("More comments"), "{html}");
        assert!(!html.contains("autumn-comments-earlier"), "{html}");
        // The item is the last child of the top-level list.
        let more = html.find("autumn-comments-more").expect("more");
        let close = html.rfind("</ol>").expect("list end");
        assert!(more < close, "{html}");
    }

    /// With `return_to`, the no-JS link goes to the host page. The htmx
    /// request carries `return_to`, so the new forms keep it.
    #[test]
    fn comment_thread_page_links_use_the_host_page() {
        let cfg = CommentThread::new("c", "/comments/Post/1")
            .return_to("/r/rust/posts/hello?sort=new")
            .page(Some(1), Some(3))
            .more_label("Load more")
            .earlier_label("Back to start");
        let html = comment_thread(&cfg, &comment_fixture()).into_string();
        assert!(
            html.contains(r#"href="/r/rust/posts/hello?sort=new&amp;after=3""#),
            "{html}"
        );
        assert!(
            html.contains(
                r#"hx-get="/comments/Post/1?after=3&amp;return_to=%2Fr%2Frust%2Fposts%2Fhello%3Fsort%3Dnew""#
            ),
            "{html}"
        );
        assert!(html.contains("Load more"), "{html}");
        assert!(
            html.contains(r#"class="autumn-comments-earlier""#),
            "{html}"
        );
        assert!(
            html.contains(r#"href="/r/rust/posts/hello?sort=new">"#)
                || html.contains(r#"href="/r/rust/posts/hello?sort=new" "#),
            "{html}"
        );
        assert!(
            html.contains(
                r#"hx-get="/comments/Post/1?return_to=%2Fr%2Frust%2Fposts%2Fhello%3Fsort%3Dnew""#
            ),
            "{html}"
        );
        assert!(html.contains(r##"hx-target="#c""##), "{html}");
        assert!(html.contains("Back to start"), "{html}");
    }

    /// A page that is not the first offers a way back to the first page.
    #[test]
    fn comment_thread_links_back_to_the_first_page() {
        let cfg = CommentThread::new("c", "/comments/Post/1")
            .read_only()
            .page(Some(9), None);
        let html = comment_thread(&cfg, &comment_fixture()).into_string();
        assert!(
            html.contains(r#"class="autumn-comments-earlier""#),
            "{html}"
        );
        assert!(html.contains(r#"href="/comments/Post/1""#), "{html}");
        assert!(html.contains("Earlier comments"), "{html}");
        assert!(!html.contains("autumn-comments-more"), "{html}");
    }

    /// The cursor helper replaces an `after` the address already has, and
    /// keeps a fragment last. A host page can pass its current address.
    #[test]
    fn with_page_cursor_replaces_the_cursor_and_keeps_the_fragment() {
        for (url, after, expected) in [
            ("/p", Some(3), "/p?after=3"),
            ("/p?after=3", Some(7), "/p?after=7"),
            ("/p?after=3&sort=new", None, "/p?sort=new"),
            ("/p?after", None, "/p"),
            ("/p?afterward=1", Some(2), "/p?afterward=1&after=2"),
            ("/p#comments", Some(4), "/p?after=4#comments"),
            ("/p?x=1&after=2#c", Some(5), "/p?x=1&after=5#c"),
        ] {
            assert_eq!(with_page_cursor(url, after), expected, "{url}");
        }
    }

    /// On a later page, focus goes to the first top-level comment, so a
    /// keyboard user does not lose their place after "More".
    #[test]
    fn a_later_page_focuses_its_first_comment() {
        let cfg = CommentThread::new("c", "/comments/Post/1").page(Some(9), None);
        let html = comment_thread(&cfg, &comment_fixture()).into_string();
        assert_eq!(html.matches("autofocus").count(), 1, "{html}");
        assert!(
            html.contains(r#"id="c-c1" class="autumn-comment" tabindex="-1" autofocus"#),
            "{html}"
        );

        let first = CommentThread::new("c", "/comments/Post/1").page(None, Some(3));
        let html = comment_thread(&first, &comment_fixture()).into_string();
        assert!(!html.contains("autofocus"), "{html}");
    }

    /// An empty later page does not say the thread is empty.
    #[test]
    fn an_empty_later_page_has_no_empty_text() {
        let cfg = CommentThread::new("c", "/comments/Post/1").page(Some(9), None);
        let html = comment_thread(&cfg, &[]).into_string();
        assert!(!html.contains("autumn-comments-empty"), "{html}");
        assert!(html.contains("autumn-comments-earlier"), "{html}");
    }

    /// The default is one unpaged thread: no page links.
    #[test]
    fn comment_thread_renders_no_page_links_by_default() {
        let cfg = CommentThread::new("c", "/comments/Post/1");
        let html = comment_thread(&cfg, &comment_fixture()).into_string();
        assert!(!html.contains("autumn-comments-more"), "{html}");
        assert!(!html.contains("autumn-comments-earlier"), "{html}");
    }

    // ── reaction_controls CSRF sugar ───────────────────────────────────

    /// The `.csrf(Option<&CsrfToken>, Option<&CsrfFormField>)` sugar is the
    /// path every real handler uses (a `CsrfToken` cannot be constructed
    /// outside this crate, so the integration tests can only exercise the
    /// `.csrf_token(&str)` primitive). reddit-clone's `vote_controls` calls it
    /// with the page's extractor, which is what puts the hidden input in the
    /// post-detail markup and makes the no-JS form POST pass CSRF.
    #[test]
    fn reaction_controls_csrf_sugar_renders_the_hidden_input_in_every_form() {
        let token = crate::security::CsrfToken::new("secret-token".to_string());
        let html = reaction_controls(
            &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
                .aggregate(7)
                .csrf(Some(&token), None),
        )
        .into_string();
        assert_eq!(
            html.matches(r#"<input type="hidden" name="_csrf" value="secret-token">"#)
                .count(),
            2,
            "{html}"
        );

        // `None` is the SSE-fragment case: htmx-only, no hidden input.
        let anonymous = reaction_controls(
            &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
                .aggregate(7)
                .csrf(None, None),
        )
        .into_string();
        assert!(!anonymous.contains("_csrf"), "{anonymous}");
    }

    #[test]
    fn reaction_controls_csrf_sugar_honors_a_custom_field_name() {
        let token = crate::security::CsrfToken::new("secret-token".to_string());
        let field = crate::security::CsrfFormField("csrf_custom".to_string());
        let html = reaction_controls(
            &ReactionControls::likes("likes-42", "/posts/42/like").csrf(Some(&token), Some(&field)),
        )
        .into_string();
        assert!(
            html.contains(r#"<input type="hidden" name="csrf_custom" value="secret-token">"#),
            "{html}"
        );
        assert!(!html.contains(r#"name="_csrf""#), "{html}");
    }
}
