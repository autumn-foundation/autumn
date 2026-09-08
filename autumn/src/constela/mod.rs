//! Parse, validate and render [Constela] documents — the constrained JSON UI
//! language — so an Autumn app can serve an interface a language model wrote.
//!
//! Enable with the Cargo feature `constela`.
//!
//! # The problem this solves
//!
//! Asking a model for a user interface and running what comes back is, in the
//! general case, remote code execution with extra steps: HTML from a model is
//! HTML from a stranger, and JSX or JavaScript from a model is worse. The usual
//! mitigations — sanitize the output, or sandbox an iframe — either throw away
//! the interactivity that made it worth generating or move the whole thing out
//! of the app.
//!
//! Constela takes the other route: the model does not emit code, it emits a
//! *description* in a JSON language with no escape hatch. There is no
//! expression form that calls a function, no attribute that holds script, no
//! way to spell `eval`. What a document can express is exactly what this module
//! can check, and this module checks all of it before anything renders.
//!
//! # Pipeline
//!
//! ```text
//!  JSON  ──parse──▶  Program  ──validate──▶  Document  ──render──▶  Markup
//!         limits              references,               escaping,
//!         depth               allowlists,               id prefixing,
//!         size                shapes                    render limits
//! ```
//!
//! [`parse`] runs the first two stages. Both constructors of [`Document`] —
//! that one and [`Document::from_program`] — go through
//! [`validate`], and there is no third, so a value of that type is a proof
//! that the checks ran.
//!
//! # Quick start
//!
//! ```
//! use autumn_web::constela::{Document, Limits, RenderContext};
//!
//! let source = r#"{
//!   "version": "1.0",
//!   "state": { "greeting": { "type": "string", "initial": "Hello, Autumn!" } },
//!   "actions": [],
//!   "view": {
//!     "kind": "element",
//!     "tag": "p",
//!     "props": { "class": { "expr": "lit", "value": "greeting" } },
//!     "children": [{ "kind": "text", "value": { "expr": "state", "name": "greeting" } }]
//!   }
//! }"#;
//!
//! let document = Document::parse(source, &Limits::default())?;
//! let mut ctx = RenderContext::default();
//! ctx.state = document.initial_state();
//!
//! let ui = document.render(&ctx)?;
//! assert_eq!(ui.body.0, r#"<p class="greeting">Hello, Autumn!</p>"#);
//! # Ok::<(), autumn_web::constela::ConstelaError>(())
//! ```
//!
//! # Safety
//!
//! The guarantee, and the four independent controls that make it, are stated in
//! [`policy`]. In summary: allowlisted tags, allowlisted attributes with every
//! `on*` handler rejected, allowlisted URL schemes checked both statically and
//! again on every computed value, and every byte of document-derived output
//! routed through one of the renderer's two escape functions — with tag and
//! attribute names written only after passing the allowlists, so they are
//! fixed strings from a fixed set. On top of that, every element
//! id a document writes is rewritten with
//! [`RenderContext::id_prefix`], so a generated fragment cannot collide with —
//! or clobber — an id the host page's own scripts depend on.
//!
//! # Interactivity
//!
//! Autumn ships **no Constela client runtime**, and this module does not
//! generate JavaScript. What it does instead fits the framework it is in:
//!
//! - The view renders on the server, with expressions evaluated against state
//!   the *app* owns.
//! - An event binding becomes `data-constela-on-{event}="{action}"` on the
//!   element, so the app can wire it — to htmx, most naturally.
//! - [`Document::dispatch`] runs an action's pure state steps (`set`, `update`,
//!   `setPath`, `if`) on the server and hands back the browser-side ones as
//!   [`Effect`]s rather than performing them.
//!
//! Together those are enough for a working interactive UI over htmx with no
//! client-side interpreter: post the action name, dispatch it, re-render, swap
//! the fragment. `docs/guide/constela.md` has the route.
//!
//! Effects are reported rather than performed on purpose. `fetch` is the clear
//! case: running a model-authored URL from inside the app would give a prompt
//! injection the app's own network position, which is SSRF by construction. The
//! app decides, against its own allowlist, whether any of that should happen.
//!
//! # The supported subset
//!
//! Autumn implements the part of the upstream AST that a *server* can be
//! faithful to. Present: the `lit`, `state`, `var`, `param`, `bin`, `not`,
//! `cond`, `get`, `index`, `concat`, `array`, `obj`, `route` and `style`
//! expressions; all twelve view node kinds; the five state types; and the
//! `set`, `update`, `setPath`, `if`, `fetch`, `storage`, `navigate`, `delay`,
//! `interval` and `focus` steps.
//!
//! Absent, and rejected at parse time with a message naming what *is* supported:
//! `call` and `lambda` (a function registry), `ref` and `validity` (a live
//! DOM), `import` and `data` (a build-time loader), and `local` (component-local
//! state). A document using one fails loudly rather than rendering something
//! subtly different from what it asked for.
//!
//! Evaluation follows upstream's JavaScript semantics exactly — coercion,
//! truthiness, operator behaviour — with one documented divergence for division
//! by zero, which JSON cannot represent. See [`eval`].
//!
//! [Constela]: https://github.com/yuuichieguchi/constela

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

pub mod ast;
mod dispatch;
mod error;
pub mod eval;
mod parse;
pub mod policy;
mod render;
mod validate;

use serde_json::{Map, Value};

pub use ast::Program;
pub use dispatch::{Dispatched, Effect};
pub use error::{ConstelaError, Diagnostic, codes};
pub use eval::RouteValues;
pub use parse::{Limits, parse_program};
pub use render::{RenderContext, RenderLimits, RenderedPortal, RenderedUi};
pub use validate::{SUPPORTED_VERSIONS, validate};

/// A Constela program that has passed [`validate`].
///
/// The type exists to make the check unforgeable: there is no constructor that
/// skips validation, so a function taking a `Document` never has to ask whether
/// the names in it resolve or whether its tags are on the allowlist. They do,
/// and they are.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    program: Program,
}

impl Document {
    /// Parse and validate `source`.
    ///
    /// # Errors
    ///
    /// [`ConstelaError::Limit`] when `source` is over a bound in `limits`,
    /// [`ConstelaError::Syntax`] when it is not a well-formed document, or
    /// [`ConstelaError::Invalid`] carrying **every** semantic and safety
    /// violation found — see [`ConstelaError::to_json`] for handing that list
    /// back to a generator to repair.
    pub fn parse(source: &str, limits: &Limits) -> Result<Self, ConstelaError> {
        let program = parse_program(source, limits)?;
        Self::from_program(program)
    }

    /// Validate an already-parsed [`Program`].
    ///
    /// # Errors
    ///
    /// [`ConstelaError::Invalid`] carrying every violation found.
    pub fn from_program(program: Program) -> Result<Self, ConstelaError> {
        validate(&program)?;
        Ok(Self { program })
    }

    /// The validated program.
    #[must_use]
    pub const fn program(&self) -> &Program {
        &self.program
    }

    /// The state the document declares, as a fresh map of its initial values.
    ///
    /// This is the starting point for [`RenderContext::state`]. An app that
    /// owns the state across requests stores the map this returns, mutates it
    /// with [`Self::dispatch`], and renders against it.
    #[must_use]
    pub fn initial_state(&self) -> Map<String, Value> {
        self.program
            .state
            .iter()
            .map(|(name, field)| (name.clone(), field.initial.clone()))
            .collect()
    }

    /// The names of every action the document declares, in document order.
    #[must_use]
    pub fn action_names(&self) -> Vec<&str> {
        self.program
            .actions
            .iter()
            .map(|action| action.name.as_str())
            .collect()
    }

    /// Render the document against `ctx`.
    ///
    /// # Errors
    ///
    /// [`ConstelaError::Render`] when the render exceeds a bound in
    /// [`RenderContext::limits`], or when a URL assembled at runtime turns out
    /// to use a scheme the [`policy`] allowlist rejects.
    pub fn render(&self, ctx: &RenderContext) -> Result<RenderedUi, ConstelaError> {
        render::render(self, ctx)
    }

    /// Run the named action's pure steps against `state`.
    ///
    /// `state` is mutated in place. Browser-side steps are not performed; they
    /// are returned as [`Effect`]s on [`Dispatched::effects`], and the branches
    /// nested under them (`onSuccess`, `onError`, `then`) are not run, because
    /// the server does not know which would have been taken.
    ///
    /// `payload` is bound as `var` references for the action's expressions,
    /// mirroring what an event handler's `payload` supplies in the browser.
    ///
    /// # Errors
    ///
    /// [`ConstelaError::Invalid`] when no action of that name is declared, or
    /// [`ConstelaError::Render`] when a step assembles a URL with a scheme the
    /// [`policy`] allowlist rejects.
    ///
    /// ```
    /// # use autumn_web::constela::{Document, Limits};
    /// let document = Document::parse(r#"{
    ///   "version": "1.0",
    ///   "state": { "count": { "type": "number", "initial": 0 } },
    ///   "actions": [{ "name": "increment",
    ///                 "steps": [{ "do": "update", "target": "count", "operation": "increment" }] }],
    ///   "view": { "kind": "text", "value": { "expr": "state", "name": "count" } }
    /// }"#, &Limits::default())?;
    ///
    /// let mut state = document.initial_state();
    /// let outcome = document.dispatch("increment", &mut state, &Default::default())?;
    /// assert!(outcome.is_pure());
    /// assert_eq!(state["count"], 1);
    /// # Ok::<(), autumn_web::constela::ConstelaError>(())
    /// ```
    pub fn dispatch(
        &self,
        action: &str,
        state: &mut Map<String, Value>,
        payload: &Map<String, Value>,
    ) -> Result<Dispatched, ConstelaError> {
        self.dispatch_with(action, state, payload, &RouteValues::default())
    }

    /// [`Self::dispatch`], with route values in scope for the action's `route`
    /// expressions.
    ///
    /// # Errors
    ///
    /// As [`Self::dispatch`].
    pub fn dispatch_with(
        &self,
        action: &str,
        state: &mut Map<String, Value>,
        payload: &Map<String, Value>,
        route: &RouteValues,
    ) -> Result<Dispatched, ConstelaError> {
        let Some(definition) = self
            .program
            .actions
            .iter()
            .find(|candidate| candidate.name == action)
        else {
            return Err(ConstelaError::Invalid(vec![Diagnostic::new(
                "actions",
                codes::UNKNOWN_REF,
                format!("no action named {action:?} is declared"),
            )]));
        };

        let mut env = eval::Env::default();
        env.vars
            .extend(payload.iter().map(|(k, v)| (k.clone(), v.clone())));

        let ctx = dispatch::DispatchCtx {
            route,
            styles: &self.program.styles,
            // An action's expressions are bounded by the same depth budget a
            // render uses. It is not configurable here because it is not a
            // knob worth turning: `Limits::max_depth` already bounded the
            // document's nesting at parse time, and this is the backstop for
            // the case where that was raised.
            depth: RenderLimits::default().max_depth,
        };
        let mut outcome = Dispatched::default();
        dispatch::run_steps(
            &definition.steps,
            state,
            &env,
            &ctx,
            &format!("actions.{action}.steps"),
            &mut outcome,
        )?;
        Ok(outcome)
    }
}

/// Parse and validate a Constela document.
///
/// Shorthand for [`Document::parse`].
///
/// # Errors
///
/// As [`Document::parse`].
pub fn parse(source: &str, limits: &Limits) -> Result<Document, ConstelaError> {
    Document::parse(source, limits)
}
