//! Server-side rendering of a validated [`Document`](super::Document).
//!
//! # Why this builds a string instead of using `html!`
//!
//! Maud's `html!` macro wants literal tag and attribute names — it compiles
//! them into the output — and a Constela document supplies both at runtime.
//! So the markup is assembled here and handed back as
//! [`PreEscaped`](maud::PreEscaped), which puts the escaping obligation on this
//! module rather than on the macro.
//!
//! That obligation is discharged in exactly two places, [`escape_text`] and
//! [`escape_attr`], and **every** byte of document-derived output goes through
//! one of them. Nothing else in this module writes an unescaped value:
//! - tag names are written only after [`policy::is_allowed_tag`],
//! - attribute names only after [`policy::is_allowed_attr`],
//! - attribute values only through [`escape_attr`], always inside double
//!   quotes,
//! - text only through [`escape_text`],
//! - and the single node that yields markup rather than text,
//!   [`Node::Markdown`], is rendered by
//!   [`markdown::render_user_content`](crate::markdown::render_user_content),
//!   whose allowlist sanitizer is the same one Autumn's user-submitted
//!   rich-text path relies on.
//!
//! # What the server does not do
//!
//! Autumn ships no Constela client runtime. Actions are not executed by the
//! browser as a result of anything here: an event binding is rendered as
//! `data-constela-on-*` attributes describing *what the document asked for*,
//! and it is the host app that decides whether to wire those to htmx, to its
//! own script, or to nothing at all. See the
//! [module docs](super#interactivity) for the intended pattern, and
//! [`Document::dispatch`](super::Document::dispatch) for running an action's
//! pure steps on the server.

// autumn-panic-gate: request-path module — it parses, validates and renders a
// document written by a language model, so every panic in it is reachable by
// hostile input and would be a 500 rather than a diagnostic. Production code
// path must be panic-free. See CONTRIBUTING.md "Request-path panic gate".
// Justify exceptions with #[allow(clippy::<lint>, reason = "…")] at the
// narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::rc::Rc;

use maud::{Markup, PreEscaped};
use serde_json::{Map, Value};

use super::ast::{EventHandler, Expr, Node, PayloadSpec, Prop};
use super::budget::{Budget, ChargingWriter, Output};
use super::error::{ConstelaError, Diagnostic, codes, excerpt};
use super::eval::{Bound, Env, EvalCtx, RouteValues, eval_ref, text_coerce, truthy};
use super::policy;

/// Bounds on how much work one render may do.
///
/// Separate from [`Limits`](super::Limits) because they bound different
/// things: parse limits bound the *document*, these bound the *expansion* of
/// that document against runtime state. A 2 KB document that loops over a
/// 100 000-element state list is small and expensive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderLimits {
    /// Maximum view-tree nesting, counting component expansions. Default 128.
    pub max_depth: usize,
    /// Maximum number of nodes rendered in total. Default 50 000.
    pub max_nodes: usize,
    /// Maximum iterations of a single `each`. Default 5 000.
    pub max_each_items: usize,
    /// Maximum bytes of markup one render may produce, across the body and
    /// every portal. Default 4 MiB.
    ///
    /// The node and iteration counts do not imply this one, which is why it
    /// exists separately: a *single* text node can emit as much as the
    /// document is allowed to be. A 512 KiB document can declare a ~480 KiB
    /// string in state and render that field from a 5 000-iteration `each` —
    /// a few dozen nodes, well inside every other bound, and about 2.4 GiB of
    /// output. Counting nodes does not see that; counting bytes does.
    ///
    /// The same number also bounds the bytes one render may build, copy or
    /// keep outside its markup: values that expressions build or copy, route
    /// metadata, handler payloads, portal targets and rendered ids. A
    /// dispatch uses the default for its own budget.
    pub max_output_bytes: usize,
}

impl Default for RenderLimits {
    fn default() -> Self {
        Self {
            max_depth: 128,
            max_nodes: 50_000,
            max_each_items: 5_000,
            max_output_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Everything a render reads that is not in the document.
#[derive(Debug, Clone)]
pub struct RenderContext {
    /// The state to render against.
    ///
    /// Seeded from the document's declared initials by
    /// [`Document::initial_state`](super::Document::initial_state); replace or
    /// patch entries to render a document against state the server owns.
    pub state: Map<String, Value>,
    /// Route parameters, query parameters and path.
    pub route: RouteValues,
    /// Prefix applied to every element id the document writes, and to every
    /// attribute that references one (`for`, `aria-labelledby`, …).
    ///
    /// This is what makes it safe to let a generated document use `id` at all.
    /// Without it, a document could name an element `id="login"` and shadow
    /// `document.getElementById("login")` in the host page's own scripts — the
    /// DOM-clobbering problem that makes Autumn's rich-text path ban `id`
    /// outright. Prefixing keeps `<label for>` and `aria-labelledby` working
    /// *within* the fragment while making a collision with the host page
    /// impossible.
    ///
    /// Set it to something unique per fragment if a page embeds more than one
    /// document. Defaults to `"c-"`.
    pub id_prefix: String,
    /// Bounds on the render.
    pub limits: RenderLimits,
}

impl Default for RenderContext {
    fn default() -> Self {
        Self {
            state: Map::new(),
            route: RouteValues::default(),
            id_prefix: "c-".to_string(),
            limits: RenderLimits::default(),
        }
    }
}

/// Content a [`Node::Portal`] asked to be placed elsewhere in the host page.
#[derive(Debug, Clone)]
pub struct RenderedPortal {
    /// The requested destination, verbatim from the document — typically
    /// `"head"` or `"body"`.
    pub target: String,
    /// The rendered content.
    pub content: Markup,
}

/// The result of rendering a document.
#[derive(Debug, Clone)]
pub struct RenderedUi {
    /// The main markup. Safe to embed in a page.
    pub body: Markup,
    /// Portal content, in document order.
    ///
    /// Deliberately *not* spliced into [`Self::body`]: a portal names a
    /// destination in the host page, and only the host layout knows where that
    /// is. Place them, or drop them.
    pub portals: Vec<RenderedPortal>,
    /// The document's `route.title`, evaluated.
    pub title: Option<String>,
    /// The document's `route.meta`, evaluated.
    pub meta: BTreeMap<String, String>,
}

impl RenderedUi {
    /// The portals asking for `target`, in document order.
    #[must_use]
    pub fn portals_for(&self, target: &str) -> Vec<&RenderedPortal> {
        self.portals
            .iter()
            .filter(|portal| portal.target == target)
            .collect()
    }
}

/// HTML-escape text content.
///
/// Escapes the full five-character set rather than the three a text node
/// strictly needs, so the same function is correct in an attribute too and
/// there is no way to reach for the wrong one.
fn escape_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// The length of `escape_text(input)`, measured without building it.
fn escaped_len(input: &str) -> usize {
    input
        .bytes()
        .map(|byte| match byte {
            b'&' | b'\'' => 5,
            b'<' | b'>' => 4,
            b'"' => 6,
            _ => 1,
        })
        .fold(0usize, usize::saturating_add)
}

/// HTML-escape an attribute value. Identical to [`escape_text`]; named
/// separately so call sites read as the context they are in.
fn escape_attr(input: &str) -> String {
    escape_text(input)
}

/// The slot content in scope, and the scope it was written in.
///
/// A linked list rather than a single value: a component invocation nested
/// inside another component's slot content needs the *outer* invocation's
/// children when its own view reaches a `slot`.
///
/// `'d` is the document, which the children borrow. `'s` is the scope.
struct SlotCtx<'d, 's> {
    children: &'d [Node],
    env: Env<'s>,
    outer: Option<&'s Self>,
}

/// Carries the render state that is not per-node.
pub struct Renderer<'a> {
    pub document: &'a super::Document,
    pub ctx: &'a RenderContext,
    nodes: usize,
    portals: Vec<RenderedPortal>,
    /// Bytes the **whole render** may build, copy or keep outside its markup.
    ///
    /// One pool for every expression, every portal target, every id, and the
    /// route metadata. A pool per expression bounds each value and not the
    /// sum. See [`Budget`].
    budget: &'a Budget,
    /// Markup bytes, for the body and every portal together. Each write goes
    /// through it. See [`Output`].
    output: &'a Output,
    /// Each element id this render wrote, prefixed, and the path of its first
    /// use. See [`Self::record_id`].
    ids: BTreeMap<String, String>,
}

/// Render `document` against `ctx`.
///
/// # Errors
///
/// [`ConstelaError::Render`] when the render exceeds a [`RenderLimits`] bound,
/// when a computed URL turns out to use a scheme [`policy::is_allowed_url`]
/// rejects, or when two elements get the same id.
pub fn render(
    document: &super::Document,
    ctx: &RenderContext,
) -> Result<RenderedUi, ConstelaError> {
    let budget = Budget::new(ctx.limits.max_output_bytes);
    let output = Output::new(ctx.limits.max_output_bytes);
    let mut renderer = Renderer {
        document,
        ctx,
        nodes: 0,
        portals: Vec::new(),
        budget: &budget,
        output: &output,
        ids: BTreeMap::new(),
    };
    let env = Env::default();

    let mut body = String::new();
    renderer.node(
        &document.program().view,
        &env,
        None,
        "view",
        ctx.limits.max_depth,
        &mut body,
    )?;

    let (title, meta) = renderer.route_metadata(&env)?;

    Ok(RenderedUi {
        body: PreEscaped(body),
        portals: renderer.portals,
        title,
        meta,
    })
}

impl<'a> Renderer<'a> {
    /// Evaluate `expr` without a copy where possible. See [`eval_ref`].
    ///
    /// The result borrows the document, the state and `env`, never `self`,
    /// so the caller can keep it across a `&mut self` call.
    fn eval_ref<'c>(
        &self,
        expr: &'c Expr,
        env: &'c Env<'c>,
        path: &str,
        depth: usize,
    ) -> Result<Cow<'c, Value>, ConstelaError>
    where
        'a: 'c,
    {
        let ctx: &'a RenderContext = self.ctx;
        let document: &'a super::Document = self.document;
        let eval_ctx = EvalCtx {
            state: &ctx.state,
            env,
            route: &ctx.route,
            styles: &document.program().styles,
            depth,
            budget: self.budget,
        };
        eval_ref(expr, &eval_ctx, path)
    }

    /// The text of `value`, without a copy for a string.
    fn text<'v>(&self, value: &'v Value, path: &str) -> Result<Cow<'v, str>, ConstelaError> {
        text_coerce(value, self.budget, path)
    }

    /// Evaluate `route.title` and `route.meta`.
    fn route_metadata(
        &self,
        env: &Env<'_>,
    ) -> Result<(Option<String>, BTreeMap<String, String>), ConstelaError> {
        let Some(route) = &self.document.program().route else {
            return Ok((None, BTreeMap::new()));
        };
        let depth = self.ctx.limits.max_depth;
        let title = match &route.title {
            Some(expr) => {
                let value = self.eval_ref(expr, env, "route.title", depth)?;
                let text = self.text(&value, "route.title")?;
                // Kept and handed back, so charged like any other value kept.
                Some(self.budget.own_str(text, "route.title")?)
            }
            None => None,
        };
        let mut meta = BTreeMap::new();
        for (key, expr) in &route.meta {
            let path = format!("route.meta.{key}");
            let value = self.eval_ref(expr, env, &path, depth)?;
            let text = self.text(&value, &path)?;
            self.budget.charge(key.len(), &path)?;
            meta.insert(key.clone(), self.budget.own_str(text, &path)?);
        }
        Ok((title, meta))
    }

    /// Append fixed or already-escaped markup to `out`, through the output
    /// meter.
    fn write(&self, out: &mut String, markup: &str, path: &str) -> Result<(), ConstelaError> {
        self.output.write(out, markup, path)
    }

    /// Append `text` to `out`, escaped, refusing the write if it would breach
    /// the byte budget.
    ///
    /// The escaped length is measured first, so an oversized value is refused
    /// before its escaped copy, which can be six times larger, is allocated.
    fn push_escaped(&self, out: &mut String, text: &str, path: &str) -> Result<(), ConstelaError> {
        self.output.check(escaped_len(text), path)?;
        self.write(out, &escape_text(text), path)
    }

    /// Count one node against [`RenderLimits::max_nodes`].
    ///
    /// Bytes are not checked here: [`Output`] sees each write as it happens.
    fn count_node(&mut self, path: &str) -> Result<(), ConstelaError> {
        self.nodes = self.nodes.saturating_add(1);
        if self.nodes > self.ctx.limits.max_nodes {
            return Err(ConstelaError::Render(Diagnostic::new(
                path,
                codes::RENDER_LIMIT,
                format!(
                    "render produced more than {} nodes",
                    self.ctx.limits.max_nodes
                ),
            )));
        }
        Ok(())
    }

    /// Render one node into `out`.
    fn node(
        &mut self,
        node: &'a Node,
        env: &Env<'_>,
        slot: Option<&SlotCtx<'a, '_>>,
        path: &str,
        depth: usize,
        out: &mut String,
    ) -> Result<(), ConstelaError> {
        self.count_node(path)?;
        let Some(depth) = depth.checked_sub(1) else {
            return Err(ConstelaError::Render(Diagnostic::new(
                path,
                codes::RENDER_LIMIT,
                format!(
                    "view nests deeper than the {}-level render limit",
                    self.ctx.limits.max_depth
                ),
            )));
        };

        match node {
            Node::Text { value } => {
                let value = self.eval_ref(value, env, &format!("{path}.value"), depth)?;
                let text = self.text(&value, path)?;
                self.push_escaped(out, &text, path)?;
            }
            Node::Element {
                tag,
                element_ref,
                props,
                children,
            } => self.element(
                tag,
                element_ref.as_deref(),
                props,
                children,
                env,
                slot,
                path,
                depth,
                out,
            )?,
            Node::If {
                condition,
                then,
                otherwise,
            } => {
                let test = self.eval_ref(condition, env, &format!("{path}.condition"), depth)?;
                let (branch, label) = if truthy(&test) {
                    (Some(&**then), "then")
                } else {
                    (otherwise.as_deref(), "else")
                };
                if let Some(branch) = branch {
                    self.node(branch, env, slot, &format!("{path}.{label}"), depth, out)?;
                }
            }
            Node::Each {
                items,
                binding,
                index,
                body,
                ..
            } => self.each(
                items,
                binding,
                index.as_deref(),
                body,
                env,
                slot,
                path,
                depth,
                out,
            )?,
            Node::Component {
                name,
                props,
                children,
            } => self.component(name, props, children, env, slot, path, depth, out)?,
            Node::Slot { .. } => self.fill_slot(slot, path, depth, out)?,
            Node::Code { language, content } => {
                self.code(language, content, env, path, depth, out)?;
            }
            Node::Markdown { content } => {
                let content = self.eval_ref(content, env, &format!("{path}.content"), depth)?;
                let source = self.text(&content, path)?;
                // Checked on the source before rendering: the sanitizer would
                // otherwise build the whole HTML string first.
                self.output.check(source.len(), path)?;
                let rendered = render_markdown(&source, self.output.remaining(), path)?;
                self.write(out, &rendered, path)?;
            }
            Node::Portal { target, children } => {
                self.portal(target, children, env, slot, path, depth)?;
            }
            Node::Island {
                id,
                strategy,
                content,
            } => self.island(id, *strategy, content, env, slot, path, depth, out)?,
            // Server-side there is nothing pending and nothing has thrown, so
            // both boundaries render their content. The fallbacks are still
            // validated, and are the client runtime's to use.
            Node::Suspense { content, .. } | Node::ErrorBoundary { content, .. } => {
                self.node(content, env, slot, &format!("{path}.content"), depth, out)?;
            }
        }
        Ok(())
    }

    /// Render an island: its content, inside a wrapper carrying the hydration
    /// strategy the document asked for.
    ///
    /// The wrapper is all the server does. Hydrating it — deciding what
    /// `strategy="visible"` should actually cost — is the host app's business,
    /// because Autumn ships no Constela client runtime to make that decision
    /// for it.
    #[expect(
        clippy::too_many_arguments,
        reason = "the island's own parts plus the render state threaded \
                  through every node; see `element`"
    )]
    fn island(
        &mut self,
        id: &str,
        strategy: super::ast::IslandStrategy,
        content: &'a Node,
        env: &Env<'_>,
        slot: Option<&SlotCtx<'a, '_>>,
        path: &str,
        depth: usize,
        out: &mut String,
    ) -> Result<(), ConstelaError> {
        self.write(out, "<div data-constela-island=\"", path)?;
        self.push_escaped(out, id, path)?;
        self.write(out, "\" data-constela-strategy=\"", path)?;
        self.write(out, strategy.as_str(), path)?;
        self.write(out, "\">", path)?;
        self.node(content, env, slot, &format!("{path}.content"), depth, out)?;
        self.write(out, "</div>", path)
    }

    /// Render the slot content in scope.
    ///
    /// The children are rendered in the *caller's* environment, and with the
    /// caller's own slot context, because that is where they were written: a
    /// `var` inside slot content reads the loop the invocation sat in, not any
    /// loop inside the component it was passed to.
    fn fill_slot(
        &mut self,
        slot: Option<&SlotCtx<'a, '_>>,
        path: &str,
        depth: usize,
        out: &mut String,
    ) -> Result<(), ConstelaError> {
        // Validation rejects a `slot` outside a component definition, so a
        // `None` here means a component's view was rendered as the document
        // root, with no invocation to take children from.
        let Some(slot) = slot else {
            return Ok(());
        };
        for (i, child) in slot.children.iter().enumerate() {
            self.node(
                child,
                &slot.env,
                slot.outer,
                &format!("{path}.slot[{i}]"),
                depth,
                out,
            )?;
        }
        Ok(())
    }

    /// Render a portal's children into their own buffer and record them.
    ///
    /// Deliberately not written into `out`: a portal names a destination in
    /// the host page, which only the host layout knows how to reach. See
    /// [`RenderedUi::portals`].
    ///
    /// The target and the record are kept, so they charge the budget. An
    /// `each` of empty portals writes no markup, but each one keeps its
    /// target.
    fn portal(
        &mut self,
        target: &str,
        children: &'a [Node],
        env: &Env<'_>,
        slot: Option<&SlotCtx<'a, '_>>,
        path: &str,
        depth: usize,
    ) -> Result<(), ConstelaError> {
        self.budget.charge(
            target.len().saturating_add(size_of::<RenderedPortal>()),
            &format!("{path}.target"),
        )?;
        // Its own buffer, but the same output meter as the body.
        let mut inner = String::new();
        for (i, child) in children.iter().enumerate() {
            self.node(
                child,
                env,
                slot,
                &format!("{path}.children[{i}]"),
                depth,
                &mut inner,
            )?;
        }
        self.portals.push(RenderedPortal {
            target: target.to_string(),
            content: PreEscaped(inner),
        });
        Ok(())
    }

    /// Render an `each`: evaluate the list, then render the body once per
    /// element with the loop bindings added to the environment.
    ///
    /// A borrowed list binds borrowed items, and a built list gives its items
    /// up, so no pass copies an item.
    #[expect(
        clippy::too_many_arguments,
        reason = "the loop's own parts plus the render state threaded through \
                  every node; see `element`"
    )]
    fn each(
        &mut self,
        items: &'a Expr,
        binding: &'a str,
        index: Option<&'a str>,
        body: &'a Node,
        env: &Env<'_>,
        slot: Option<&SlotCtx<'a, '_>>,
        path: &str,
        depth: usize,
        out: &mut String,
    ) -> Result<(), ConstelaError> {
        let items = self.eval_ref(items, env, &format!("{path}.items"), depth)?;
        let passes: Box<dyn Iterator<Item = Bound<'_>> + '_> = match items {
            Cow::Borrowed(Value::Array(list)) => {
                self.check_iterations(list.len(), path)?;
                Box::new(list.iter().map(Bound::Borrowed))
            }
            Cow::Owned(Value::Array(list)) => {
                self.check_iterations(list.len(), path)?;
                Box::new(list.into_iter().map(|item| Bound::Shared(Rc::new(item))))
            }
            // A non-list renders nothing. Upstream's client runtime would
            // throw here; rendering an empty list is the behaviour that
            // degrades a generated page rather than losing it.
            _ => return Ok(()),
        };
        for (i, item) in passes.enumerate() {
            let mut inner = env.with_var(binding, item);
            if let Some(index_name) = index {
                inner = inner.with_var(index_name, Bound::Shared(Rc::new(Value::from(i))));
            }
            self.node(body, &inner, slot, &format!("{path}.body[{i}]"), depth, out)?;
        }
        Ok(())
    }

    /// Fail if an `each` would run more than [`RenderLimits::max_each_items`]
    /// passes.
    fn check_iterations(&self, len: usize, path: &str) -> Result<(), ConstelaError> {
        if len > self.ctx.limits.max_each_items {
            return Err(ConstelaError::Render(Diagnostic::new(
                path,
                codes::RENDER_LIMIT,
                format!(
                    "`each` would iterate {len} times, over the {} limit",
                    self.ctx.limits.max_each_items
                ),
            )));
        }
        Ok(())
    }

    /// Render a component invocation.
    ///
    /// The component's view is rendered in a *fresh* environment holding only
    /// its evaluated params — a component cannot see the caller's loop
    /// bindings — while its children are remembered as slot content together
    /// with the caller's environment, which is the scope they were written in.
    #[expect(
        clippy::too_many_arguments,
        reason = "the invocation's own parts plus the render state threaded \
                  through every node; see `element`"
    )]
    fn component(
        &mut self,
        name: &str,
        props: &'a BTreeMap<String, Expr>,
        children: &'a [Node],
        env: &Env<'_>,
        slot: Option<&SlotCtx<'a, '_>>,
        path: &str,
        depth: usize,
        out: &mut String,
    ) -> Result<(), ConstelaError> {
        // Validation proved this resolves. Looked up rather than indexed all
        // the same: a panic here is a 500 on a request carrying a document
        // someone generated, so the one place that reasoning could be wrong is
        // an error, not a crash.
        let document: &'a super::Document = self.document;
        let Some(component) = document.program().components.get(name) else {
            return Err(ConstelaError::Render(Diagnostic::new(
                format!("{path}.name"),
                codes::UNKNOWN_REF,
                format!("no component named {name:?} is declared"),
            )));
        };
        let mut params = BTreeMap::new();
        for (key, expr) in props {
            let value = self.eval_ref(expr, env, &format!("{path}.props.{key}"), depth)?;
            params.insert(key.as_str(), Bound::from(value));
        }
        let inner = Env {
            params: Rc::new(params),
            vars: Vec::new(),
        };
        let nested = SlotCtx {
            children,
            env: env.clone(),
            outer: slot,
        };
        self.node(
            &component.view,
            &inner,
            Some(&nested),
            &format!("{path}<{name}>"),
            depth,
            out,
        )
    }

    /// Render an element: its tag, its props, and its children.
    #[expect(
        clippy::too_many_arguments,
        reason = "the element's own parts plus the render state threaded \
                  through every node; bundling them would only move the \
                  arguments into a struct built at one call site"
    )]
    fn element(
        &mut self,
        tag: &str,
        element_ref: Option<&str>,
        props: &'a BTreeMap<String, Prop>,
        children: &'a [Node],
        env: &Env<'_>,
        slot: Option<&SlotCtx<'a, '_>>,
        path: &str,
        depth: usize,
        out: &mut String,
    ) -> Result<(), ConstelaError> {
        let tag = tag.to_ascii_lowercase();
        // Validation proved the tag is on the allowlist. Re-checking costs a
        // lookup and means a future path that reaches the renderer without
        // going through validation still cannot emit a `<script>`.
        if !policy::is_allowed_tag(&tag) {
            return Err(ConstelaError::Render(Diagnostic::new(
                path,
                codes::TAG_NOT_ALLOWED,
                format!("tag {tag:?} is not on the allowlist"),
            )));
        }

        let mut attrs: Vec<(String, Option<String>)> = Vec::new();
        let mut handlers: Vec<(&str, &EventHandler)> = Vec::new();
        // Escaped attribute bytes not yet written. Each value is checked with
        // these added, so many large attributes cannot pile up in `attrs`
        // before the element is written.
        let mut pending = 0usize;

        for (raw_name, prop) in props {
            match prop {
                Prop::Handler(handler) => handlers.push((raw_name.as_str(), handler)),
                Prop::Value(expr) => {
                    let name = policy::canonical_attr(raw_name);
                    // Reserved first: a `data-constela-*` name passes the
                    // `data-` family rule below, and letting one through would
                    // put a forged event binding next to the renderer's own.
                    if policy::is_reserved_attr(&name) || !policy::is_allowed_attr(&name) {
                        return Err(ConstelaError::Render(Diagnostic::new(
                            format!("{path}.props.{raw_name}"),
                            codes::ATTR_NOT_ALLOWED,
                            format!("attribute {name:?} is not on the allowlist"),
                        )));
                    }
                    let attr_path = format!("{path}.props.{raw_name}");
                    let value = self.eval_ref(expr, env, &attr_path, depth)?;
                    if name == "id" && !self.record_id(&value, &attr_path)? {
                        continue;
                    }
                    if let Some(rendered) =
                        self.attribute_value(&name, &value, pending, &attr_path)?
                    {
                        pending = pending.saturating_add(rendered.len());
                        attrs.push((name, Some(rendered)));
                    }
                }
            }
        }

        if let Some(name) = element_ref {
            attrs.push(("data-constela-ref".to_string(), Some(escape_attr(name))));
        }
        for (_, handler) in &handlers {
            self.push_handler_attrs(handler, env, path, depth, &mut attrs, &mut pending)?;
        }
        harden_blank_target(&mut attrs);

        let attr_bytes: usize = attrs
            .iter()
            .map(|(name, value)| {
                name.len()
                    .saturating_add(value.as_ref().map_or(0, String::len))
                    .saturating_add(4) // ` name=""`
            })
            .fold(0usize, usize::saturating_add);
        self.output
            .check(attr_bytes.saturating_add(tag.len()).saturating_add(2), path)?;

        self.write(out, "<", path)?;
        self.write(out, &tag, path)?;
        for (name, value) in &attrs {
            match value {
                // An empty value is written as the bare attribute name. That
                // is right for both readings: a boolean prop that evaluated to
                // `true` (`disabled`), and a string prop that evaluated to ""
                // (`alt=""` on a decorative image) — HTML parses an attribute
                // with no value as the empty string, so the two coincide.
                Some(value) if value.is_empty() => {
                    self.write(out, " ", path)?;
                    self.write(out, name, path)?;
                }
                Some(value) => {
                    self.write(out, " ", path)?;
                    self.write(out, name, path)?;
                    self.write(out, "=\"", path)?;
                    self.write(out, value, path)?;
                    self.write(out, "\"", path)?;
                }
                None => {}
            }
        }
        self.write(out, ">", path)?;

        if policy::is_void_tag(&tag) {
            return Ok(());
        }

        for (i, child) in children.iter().enumerate() {
            self.node(
                child,
                env,
                slot,
                &format!("{path}.children[{i}]"),
                depth,
                out,
            )?;
        }
        self.write(out, "</", path)?;
        self.write(out, &tag, path)?;
        self.write(out, ">", path)
    }

    /// Check an element id, and record it for the whole render. Returns
    /// whether to write the attribute.
    ///
    /// `false`, `null`, `true` and `""` name no element, so the attribute is
    /// omitted. An id with whitespace is an error: an id is one token. A
    /// second element with the same id is an error: HTML ids are unique, and a
    /// `<label for>` finds only the first. Ids are not renamed per call; see
    /// ADR 0025.
    ///
    /// The map is kept for the whole render, so each entry charges the
    /// budget.
    fn record_id(&mut self, value: &Value, path: &str) -> Result<bool, ConstelaError> {
        let id = match value {
            Value::Bool(_) | Value::Null => return Ok(false),
            other => self.text(other, path)?,
        };
        if id.is_empty() {
            return Ok(false);
        }
        if id.chars().any(char::is_whitespace) {
            return Err(ConstelaError::Render(Diagnostic::new(
                path,
                codes::EVAL,
                format!("id {:?} has whitespace; an id is one token", excerpt(&id)),
            )));
        }
        let prefix = &self.ctx.id_prefix;
        self.budget.charge(
            prefix
                .len()
                .saturating_add(id.len())
                .saturating_add(path.len())
                .saturating_add(size_of::<String>().saturating_mul(2)),
            path,
        )?;
        let rendered = format!("{prefix}{id}");
        if let Some(first) = self.ids.get(&rendered) {
            return Err(ConstelaError::Render(Diagnostic::new(
                path,
                codes::DUPLICATE,
                format!(
                    "id {:?} (rendered {:?}) is already used at {}; give each use its own id: pass a different param value to each use, or build it from the loop index",
                    excerpt(&id),
                    excerpt(&rendered),
                    excerpt(first)
                ),
            )));
        }
        self.ids.insert(rendered, path.to_string());
        Ok(true)
    }

    /// Turn an evaluated prop into an escaped attribute value, or `None` to
    /// omit the attribute entirely.
    ///
    /// Follows the upstream client renderer: `true` is a bare boolean
    /// attribute, `false`/`null` removes it, and `data-*` always renders even
    /// when nullish.
    ///
    /// `pending` is the escaped bytes of this element's earlier attributes,
    /// which are not written yet.
    fn attribute_value(
        &self,
        name: &str,
        value: &Value,
        pending: usize,
        path: &str,
    ) -> Result<Option<String>, ConstelaError> {
        let is_data = name.starts_with("data-");
        let text = match value {
            Value::Bool(true) => return Ok(Some(String::new())),
            Value::Bool(false) | Value::Null if !is_data => return Ok(None),
            other => self.text(other, path)?,
        };

        if policy::is_url_attr(name) && !policy::is_allowed_url(&text) {
            // The last line of defence for a URL assembled at runtime, which
            // validation could only see as an expression.
            return Err(ConstelaError::Render(Diagnostic::new(
                path,
                codes::URL_SCHEME,
                format!(
                    "computed URL {:?} uses a scheme that is not allowed",
                    excerpt(&text)
                ),
            )));
        }

        // The raw value, before the id prefix allocates a copy of it.
        self.output
            .check(pending.saturating_add(text.len()), path)?;

        let text = if policy::is_id_ref_attr(name) {
            let prefix = &self.ctx.id_prefix;
            // Measured first: a value of many short ids grows by one prefix
            // per id.
            self.output
                .check(pending.saturating_add(prefixed_len(prefix, &text)), path)?;
            Cow::Owned(prefix_id_refs(prefix, &text))
        } else if policy::is_url_attr(name) {
            prefix_fragment_link(&self.ctx.id_prefix, text)
        } else {
            text
        };

        // Measured before escaping allocates a copy.
        self.output
            .check(pending.saturating_add(escaped_len(&text)), path)?;
        Ok(Some(escape_attr(&text)))
    }

    /// Emit the `data-constela-*` attributes describing one event binding.
    fn push_handler_attrs(
        &self,
        handler: &'a EventHandler,
        env: &Env<'_>,
        path: &str,
        depth: usize,
        attrs: &mut Vec<(String, Option<String>)>,
        pending: &mut usize,
    ) -> Result<(), ConstelaError> {
        let event = policy::canonical_event(&handler.event);
        if event.is_empty() {
            return Ok(());
        }
        attrs.push((
            format!("data-constela-on-{event}"),
            Some(escape_attr(&handler.action)),
        ));
        if let Some(payload) = &handler.payload {
            let payload_path = format!("{path}.payload");
            // Serialized from borrowed values, so no value is copied. The text
            // is kept, so it charges the budget for each chunk as it is
            // written.
            let writer = ChargingWriter::new(self.budget, &payload_path);
            let serialized = match payload {
                PayloadSpec::Single(expr) => {
                    let value = self.eval_ref(expr, env, &payload_path, depth)?;
                    writer.serialize(&value)?
                }
                PayloadSpec::Map(props) => {
                    let mut map = BTreeMap::new();
                    for (key, expr) in props {
                        let entry_path = format!("{payload_path}.{key}");
                        map.insert(key.as_str(), self.eval_ref(expr, env, &entry_path, depth)?);
                    }
                    writer.serialize(&map)?
                }
            };
            let escaped_bytes = escaped_len(&serialized);
            self.output
                .check(pending.saturating_add(escaped_bytes), &payload_path)?;
            *pending = pending.saturating_add(escaped_bytes);
            attrs.push((
                format!("data-constela-payload-{event}"),
                Some(escape_attr(&serialized)),
            ));
        }
        if let Some(ms) = handler.debounce {
            attrs.push((
                format!("data-constela-debounce-{event}"),
                Some(ms.to_string()),
            ));
        }
        if let Some(ms) = handler.throttle {
            attrs.push((
                format!("data-constela-throttle-{event}"),
                Some(ms.to_string()),
            ));
        }
        Ok(())
    }

    /// Render a code block: `<pre><code>`, with the language hint reduced to
    /// something safe to put in a class name.
    ///
    /// The content comes from state and is the unbounded part, so it goes
    /// through the checked, escaping write.
    fn code(
        &self,
        language: &Expr,
        content: &Expr,
        env: &Env<'_>,
        path: &str,
        depth: usize,
        out: &mut String,
    ) -> Result<(), ConstelaError> {
        let language = self.eval_ref(language, env, &format!("{path}.language"), depth)?;
        let content = self.eval_ref(content, env, &format!("{path}.content"), depth)?;
        self.write(out, "<pre><code", path)?;
        let language = sanitize_language(&self.text(&language, path)?);
        if !language.is_empty() {
            // The class is derived from a document value, so it is escaped like
            // any other attribute.
            self.write(out, " class=\"language-", path)?;
            self.push_escaped(out, &language, path)?;
            self.write(out, "\"", path)?;
        }
        self.write(out, ">", path)?;
        self.push_escaped(out, &self.text(&content, path)?, path)?;
        self.write(out, "</code></pre>", path)
    }
}

/// Render a Markdown node through the sanitizing user-content path — the same
/// allowlist Autumn's user-submitted rich text goes through.
///
/// The intermediate HTML is bounded by `max_html`, so a small source cannot
/// build a large string before the output meter sees it.
#[cfg(feature = "markdown")]
fn render_markdown(source: &str, max_html: usize, path: &str) -> Result<String, ConstelaError> {
    crate::markdown::render_user_content_html_bounded(source, max_html).map_err(|err| {
        ConstelaError::Render(Diagnostic::new(path, codes::RENDER_LIMIT, err.to_string()))
    })
}

/// Without the `markdown` feature this is unreachable: validation rejects a
/// `markdown` node with [`codes::FEATURE_REQUIRED`] before a document
/// containing one can become a [`Document`](super::Document).
#[cfg(not(feature = "markdown"))]
const fn render_markdown(
    source: &str,
    max_html: usize,
    path: &str,
) -> Result<String, ConstelaError> {
    let _ = (source, max_html, path);
    Ok(String::new())
}

/// Reduce a language hint to something safe to put in a `language-*` class.
fn sanitize_language(language: &str) -> String {
    language
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '+' || *c == '#')
        .take(32)
        .collect()
}

/// Prefix the target of a same-document fragment link, so `href="#section"`
/// still reaches the element the document called `id="section"`.
///
/// Without this the two halves disagree: the id is rewritten to `c-section`
/// and the link is not, so in-fragment navigation silently stops working —
/// and, worse, `#section` then resolves against whatever the *host* page
/// happens to call `section`. Prefixing both keeps the pair consistent and
/// keeps the fragment inside the document, which is the same property
/// [`RenderContext::id_prefix`] exists to give.
///
/// Only a *pure* fragment is rewritten. `href="/other#section"` points into a
/// different document whose ids this render did not write, and `href="#"` has
/// no target to prefix.
fn prefix_fragment_link<'t>(prefix: &str, value: Cow<'t, str>) -> Cow<'t, str> {
    match value.strip_prefix('#') {
        Some(fragment) if !fragment.is_empty() => Cow::Owned(format!("#{prefix}{fragment}")),
        _ => value,
    }
}

/// The length of `prefix_id_refs(prefix, value)`, measured without building
/// it.
fn prefixed_len(prefix: &str, value: &str) -> usize {
    value
        .split_whitespace()
        .map(|id| prefix.len().saturating_add(id.len()).saturating_add(1))
        .fold(0usize, usize::saturating_add)
        .saturating_sub(1)
}

/// Prefix each whitespace-separated id in `value`, into one buffer.
fn prefix_id_refs(prefix: &str, value: &str) -> String {
    let mut out = String::with_capacity(prefixed_len(prefix, value));
    for id in value.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(prefix);
        out.push_str(id);
    }
    out
}

/// Give every `target="_blank"` link `rel="noopener noreferrer"`.
///
/// Without `noopener` the opened page gets a live `window.opener` handle back
/// to the host page and can navigate it — reverse tabnabbing. A generated
/// document is exactly the place this gets forgotten, so it is added here
/// rather than left to the document to remember.
fn harden_blank_target(attrs: &mut Vec<(String, Option<String>)>) {
    let opens_new_tab = attrs.iter().any(|(name, value)| {
        name == "target" && value.as_deref().is_some_and(|value| value == "_blank")
    });
    if !opens_new_tab {
        return;
    }
    match attrs.iter_mut().find(|(name, _)| name == "rel") {
        Some((_, value)) => {
            let existing = value.clone().unwrap_or_default();
            let mut tokens: Vec<&str> = existing.split_whitespace().collect();
            for required in ["noopener", "noreferrer"] {
                if !tokens.contains(&required) {
                    tokens.push(required);
                }
            }
            *value = Some(tokens.join(" "));
        }
        None => attrs.push(("rel".to_string(), Some("noopener noreferrer".to_string()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_covers_the_five_character_set() {
        assert_eq!(
            escape_text(r#"<a href="x">&'"#),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }

    #[test]
    fn escaped_len_matches_the_escaped_text() {
        for input in ["", "plain", r#"<a href="x">&'"#, "€ & <€>"] {
            assert_eq!(escaped_len(input), escape_text(input).len(), "{input}");
        }
    }

    #[test]
    fn language_hints_are_reduced_to_class_safe_text() {
        assert_eq!(sanitize_language("rust"), "rust");
        assert_eq!(sanitize_language("c++"), "c++");
        assert_eq!(sanitize_language("a\" onload=\"x"), "aonloadx");
    }

    #[test]
    fn id_refs_are_prefixed_token_by_token() {
        assert_eq!(prefix_id_refs("c-", "a"), "c-a");
        assert_eq!(prefix_id_refs("c-", "a  b"), "c-a c-b");
        assert_eq!(prefix_id_refs("c-", ""), "");
        for value in ["", "a", "a  b", " a b c "] {
            assert_eq!(
                prefixed_len("c-", value),
                prefix_id_refs("c-", value).len(),
                "{value:?}"
            );
        }
    }

    #[test]
    fn same_document_fragment_links_are_prefixed_like_the_ids_they_target() {
        let link = |value: &'static str| prefix_fragment_link("c-", Cow::Borrowed(value));
        assert_eq!(link("#section"), "#c-section");
        // A bare `#` has no target, and a cross-document fragment points at a
        // document whose ids this render did not write.
        assert_eq!(link("#"), "#");
        assert_eq!(link("/other#section"), "/other#section");
        assert_eq!(link("https://x/y#z"), "https://x/y#z");
        assert_eq!(link(""), "");
    }

    #[test]
    fn blank_targets_get_noopener() {
        let mut attrs = vec![("target".into(), Some("_blank".into()))];
        harden_blank_target(&mut attrs);
        assert_eq!(
            attrs.iter().find(|(n, _)| n == "rel").unwrap().1.as_deref(),
            Some("noopener noreferrer")
        );
    }

    #[test]
    fn existing_rel_is_extended_not_replaced() {
        let mut attrs = vec![
            ("target".into(), Some("_blank".into())),
            ("rel".into(), Some("nofollow noopener".into())),
        ];
        harden_blank_target(&mut attrs);
        assert_eq!(
            attrs.iter().find(|(n, _)| n == "rel").unwrap().1.as_deref(),
            Some("nofollow noopener noreferrer")
        );
    }

    #[test]
    fn same_tab_links_are_left_alone() {
        let mut attrs = vec![("target".into(), Some("_self".into()))];
        harden_blank_target(&mut attrs);
        assert!(attrs.iter().all(|(n, _)| n != "rel"));
    }
}
