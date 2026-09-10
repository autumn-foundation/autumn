//! The Constela abstract syntax tree, as Autumn parses it.
//!
//! These types are `serde` mirrors of the upstream [Constela] `Program` AST
//! (`packages/core/src/types/ast.ts`), narrowed to the subset Autumn can
//! evaluate and render **on the server**. See the [module docs](super) for the
//! exact boundary and why it is drawn where it is.
//!
//! Two deserialization decisions are load bearing:
//!
//! 1. **Unknown fields are ignored.** Upstream Constela carries fields this
//!    subset has no meaning for (`transition`, `theme`, `debounce`, …), and new
//!    ones land there faster than here. Ignoring them keeps a document written
//!    against a newer upstream parsable instead of hard-failing on a field that
//!    would not have changed the output anyway. It is safe *because the
//!    renderer is an allowlist*: [`Document::render`](super::Document::render)
//!    emits only what it
//!    understands, so a field it drops can never become markup.
//! 2. **Unknown tag values are rejected.** The `expr`, `kind`, `do` and `type`
//!    discriminants are closed enums, so `{"expr": "call"}` fails to parse with
//!    a message naming the variants that do exist. That is the signal an LLM
//!    needs to repair its output, and it is the reason the unsupported
//!    expression forms are *absent* here rather than present-and-ignored.
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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A Constela binary operator.
///
/// The set is closed and matches upstream exactly; see
/// [`eval`](super::eval) for the evaluation semantics of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum BinaryOp {
    /// Numeric addition, or string concatenation when either side is not a
    /// number.
    #[serde(rename = "+")]
    Add,
    /// Numeric subtraction.
    #[serde(rename = "-")]
    Sub,
    /// Numeric multiplication.
    #[serde(rename = "*")]
    Mul,
    /// Numeric division.
    #[serde(rename = "/")]
    Div,
    /// Numeric remainder.
    #[serde(rename = "%")]
    Rem,
    /// Strict equality (no type juggling).
    #[serde(rename = "==")]
    Eq,
    /// Strict inequality.
    #[serde(rename = "!=")]
    Ne,
    /// Less than.
    #[serde(rename = "<")]
    Lt,
    /// Less than or equal.
    #[serde(rename = "<=")]
    Le,
    /// Greater than.
    #[serde(rename = ">")]
    Gt,
    /// Greater than or equal.
    #[serde(rename = ">=")]
    Ge,
    /// Short-circuiting logical and; yields the operand, not a boolean.
    #[serde(rename = "&&")]
    And,
    /// Short-circuiting logical or; yields the operand, not a boolean.
    #[serde(rename = "||")]
    Or,
}

impl BinaryOp {
    /// The operator as it appears in a document, for diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Rem => "%",
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::And => "&&",
            Self::Or => "||",
        }
    }
}

/// Where a [`Expr::Route`] expression reads from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RouteSource {
    /// A named path parameter (`/posts/{id}` → `id`). The default.
    #[default]
    Param,
    /// A named query-string parameter (`?q=hello` → `q`).
    Query,
    /// The request path itself; `name` is ignored.
    Path,
}

/// A Constela expression.
///
/// Every variant is total and side-effect free: evaluating an expression reads
/// the render scope and returns a value, and can do nothing else. The
/// side-effecting upstream forms (`call`, `lambda`, `ref`, `validity`,
/// `import`, `data`, `local`) are deliberately not represented — see the
/// [module docs](super).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "expr", rename_all = "lowercase")]
pub enum Expr {
    /// A JSON constant.
    Lit {
        /// The constant value.
        value: Value,
    },
    /// A read of a top-level state field, optionally into it.
    State {
        /// The state field name.
        name: String,
        /// Dotted sub-path within the field, e.g. `"address.city"`.
        #[serde(default)]
        path: Option<String>,
    },
    /// A read of a loop binding or an action-step result bound in scope.
    Var {
        /// The binding name.
        name: String,
        /// Dotted sub-path within the binding.
        #[serde(default)]
        path: Option<String>,
    },
    /// A read of a component parameter. Only in scope inside a component.
    Param {
        /// The parameter name, as declared in the component's `params`.
        name: String,
        /// Dotted sub-path within the parameter.
        #[serde(default)]
        path: Option<String>,
    },
    /// A binary operation.
    Bin {
        /// The operator.
        op: BinaryOp,
        /// Left operand.
        left: Box<Self>,
        /// Right operand.
        right: Box<Self>,
    },
    /// Logical negation of the operand's truthiness.
    Not {
        /// The operand.
        operand: Box<Self>,
    },
    /// A conditional.
    Cond {
        /// The condition, evaluated for truthiness.
        #[serde(rename = "if")]
        condition: Box<Self>,
        /// Evaluated when the condition is truthy.
        then: Box<Self>,
        /// Evaluated when the condition is falsy.
        #[serde(rename = "else")]
        otherwise: Box<Self>,
    },
    /// Static dotted property access into another expression's result.
    Get {
        /// The base expression.
        base: Box<Self>,
        /// Dotted path, e.g. `"user.name"`.
        path: String,
    },
    /// Dynamic property or array access.
    Index {
        /// The base expression.
        base: Box<Self>,
        /// The key or index, evaluated at access time.
        key: Box<Self>,
    },
    /// String concatenation of every item, in order.
    Concat {
        /// The items to concatenate.
        items: Vec<Self>,
    },
    /// Array construction.
    Array {
        /// The elements.
        elements: Vec<Self>,
    },
    /// Object construction.
    Obj {
        /// The properties.
        props: BTreeMap<String, Self>,
    },
    /// A read of a route parameter, query parameter, or the path.
    Route {
        /// The parameter name (ignored when `source` is `path`).
        name: String,
        /// Which part of the route to read.
        #[serde(default)]
        source: RouteSource,
    },
    /// A resolved reference to a named style preset, with variant selections.
    ///
    /// Evaluates to the preset's class string; see [`StylePreset`].
    Style {
        /// The preset name, as declared in the program's `styles`.
        name: String,
        /// Variant selections, each evaluated to a string.
        #[serde(default)]
        variants: BTreeMap<String, Self>,
    },
}

/// A CVA-style style preset: a base class string plus variant axes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct StylePreset {
    /// Classes always applied.
    #[serde(default)]
    pub base: String,
    /// Variant axes: axis name → option name → classes.
    #[serde(default)]
    pub variants: BTreeMap<String, BTreeMap<String, String>>,
    /// Default option per axis, used when a [`Expr::Style`] omits it.
    #[serde(default, rename = "defaultVariants")]
    pub default_variants: BTreeMap<String, String>,
}

/// The declared type of a state field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StateType {
    /// A JSON number.
    Number,
    /// A JSON string.
    String,
    /// A JSON array.
    List,
    /// A JSON boolean.
    Boolean,
    /// A JSON object.
    Object,
}

impl StateType {
    /// The type name as it appears in a document, for diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::String => "string",
            Self::List => "list",
            Self::Boolean => "boolean",
            Self::Object => "object",
        }
    }

    /// Whether `value` inhabits this type.
    #[must_use]
    pub fn accepts(self, value: &Value) -> bool {
        match self {
            Self::Number => value.is_number(),
            Self::String => value.is_string(),
            Self::List => value.is_array(),
            Self::Boolean => value.is_boolean(),
            Self::Object => value.is_object(),
        }
    }
}

/// A declared state field: its type and its initial value.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct StateField {
    /// The declared type.
    #[serde(rename = "type")]
    pub ty: StateType,
    /// The initial value. Validation requires it to inhabit `ty`.
    pub initial: Value,
}

/// The in-place mutation an [`ActionStep::Update`] performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UpdateOperation {
    /// Add `value` (default 1) to a number.
    Increment,
    /// Subtract `value` (default 1) from a number.
    Decrement,
    /// Append `value` to a list.
    Push,
    /// Remove the last element of a list.
    Pop,
    /// Remove elements of a list equal to `value`.
    Remove,
    /// Flip a boolean.
    Toggle,
    /// Shallow-merge `value` into an object.
    Merge,
    /// Replace the element at `index`.
    ReplaceAt,
    /// Insert `value` before `index`.
    InsertAt,
    /// Delete `deleteCount` elements at `index`, inserting `value` if given.
    Splice,
}

impl UpdateOperation {
    /// The operation name as it appears in a document, for diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Increment => "increment",
            Self::Decrement => "decrement",
            Self::Push => "push",
            Self::Pop => "pop",
            Self::Remove => "remove",
            Self::Toggle => "toggle",
            Self::Merge => "merge",
            Self::ReplaceAt => "replaceAt",
            Self::InsertAt => "insertAt",
            Self::Splice => "splice",
        }
    }

    /// The state type this operation requires its target to have, or `None`
    /// when the operation is polymorphic.
    #[must_use]
    pub const fn required_target_type(self) -> Option<StateType> {
        match self {
            Self::Increment | Self::Decrement => Some(StateType::Number),
            Self::Push
            | Self::Pop
            | Self::Remove
            | Self::ReplaceAt
            | Self::InsertAt
            | Self::Splice => Some(StateType::List),
            Self::Toggle => Some(StateType::Boolean),
            Self::Merge => Some(StateType::Object),
        }
    }
}

/// An HTTP method an [`ActionStep::Fetch`] may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// `GET`. The default.
    #[default]
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `DELETE`.
    Delete,
}

/// Which Web Storage area an [`ActionStep::Storage`] targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageArea {
    /// `window.localStorage`.
    Local,
    /// `window.sessionStorage`.
    Session,
}

/// The operation an [`ActionStep::Storage`] performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageOperation {
    /// Read a key.
    Get,
    /// Write a key.
    Set,
    /// Delete a key.
    Remove,
}

/// One step of an action.
///
/// Steps split into two groups, and the split is the whole reason this type is
/// public: [`Self::Set`], [`Self::Update`], [`Self::SetPath`] and [`Self::If`]
/// are *pure state transitions* that
/// [`Document::dispatch`](super::Document::dispatch) applies on
/// the server, while the rest describe browser effects the server declines to
/// perform and reports as [`Effect`](super::Effect)s instead.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "do", rename_all = "camelCase")]
pub enum ActionStep {
    /// Assign a state field.
    Set {
        /// The state field to assign.
        target: String,
        /// The new value.
        value: Expr,
    },
    /// Mutate a state field in place.
    Update {
        /// The state field to mutate.
        target: String,
        /// The mutation.
        operation: UpdateOperation,
        /// Operand, for the operations that take one.
        #[serde(default)]
        value: Option<Expr>,
        /// Array index, for `replaceAt`/`insertAt`/`splice`.
        #[serde(default)]
        index: Option<Expr>,
        /// Element count, for `splice`.
        #[serde(default, rename = "deleteCount")]
        delete_count: Option<Expr>,
    },
    /// Assign a nested path within a state field.
    SetPath {
        /// The state field.
        target: String,
        /// The path within it: a dotted string or an array of segments.
        path: Expr,
        /// The new value.
        value: Expr,
    },
    /// Branch on a condition.
    If {
        /// The condition, evaluated for truthiness.
        condition: Expr,
        /// Steps run when the condition is truthy.
        then: Vec<Self>,
        /// Steps run when the condition is falsy.
        #[serde(default, rename = "else")]
        otherwise: Vec<Self>,
    },
    /// An HTTP request. Not performed by the server; see [`Effect`](super::Effect).
    Fetch {
        /// The request URL.
        url: Expr,
        /// The HTTP method.
        #[serde(default)]
        method: HttpMethod,
        /// The request body.
        #[serde(default)]
        body: Option<Expr>,
        /// Name to bind the response to for later steps.
        #[serde(default)]
        result: Option<String>,
        /// Steps run on success.
        #[serde(default, rename = "onSuccess")]
        on_success: Vec<Self>,
        /// Steps run on failure.
        #[serde(default, rename = "onError")]
        on_error: Vec<Self>,
    },
    /// A Web Storage read or write. Not performed by the server.
    Storage {
        /// The operation.
        operation: StorageOperation,
        /// The storage key.
        key: Expr,
        /// The value, for `set`.
        #[serde(default)]
        value: Option<Expr>,
        /// Which storage area.
        storage: StorageArea,
        /// Name to bind the read value to, for `get`.
        #[serde(default)]
        result: Option<String>,
        /// Steps run on success.
        #[serde(default, rename = "onSuccess")]
        on_success: Vec<Self>,
        /// Steps run on failure.
        #[serde(default, rename = "onError")]
        on_error: Vec<Self>,
    },
    /// A navigation. Not performed by the server.
    Navigate {
        /// The destination URL.
        url: Expr,
        /// Whether to replace the history entry rather than push one.
        #[serde(default)]
        replace: bool,
    },
    /// A deferred run of `then`. Not performed by the server.
    Delay {
        /// The delay in milliseconds.
        ms: Expr,
        /// Steps run after the delay.
        then: Vec<Self>,
    },
    /// A repeating run of a named action. Not performed by the server.
    Interval {
        /// The interval in milliseconds.
        ms: Expr,
        /// The action to run each tick.
        action: String,
    },
    /// A focus/blur/select on a referenced element. Not performed by the server.
    Focus {
        /// The `ref` name of the target element.
        target: Expr,
        /// The focus operation (`focus`, `blur` or `select`).
        operation: String,
    },
}

impl ActionStep {
    /// Whether this step is a pure state transition the server applies.
    ///
    /// The complement — effectful steps — is what
    /// [`Document::dispatch`](super::Document::dispatch) reports rather than
    /// performs.
    #[must_use]
    pub const fn is_pure(&self) -> bool {
        matches!(
            self,
            Self::Set { .. } | Self::Update { .. } | Self::SetPath { .. } | Self::If { .. }
        )
    }

    /// The step's `do` discriminant, for diagnostics.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Set { .. } => "set",
            Self::Update { .. } => "update",
            Self::SetPath { .. } => "setPath",
            Self::If { .. } => "if",
            Self::Fetch { .. } => "fetch",
            Self::Storage { .. } => "storage",
            Self::Navigate { .. } => "navigate",
            Self::Delay { .. } => "delay",
            Self::Interval { .. } => "interval",
            Self::Focus { .. } => "focus",
        }
    }
}

/// A named sequence of steps, invoked by an [`EventHandler`] or a lifecycle hook.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ActionDefinition {
    /// The action name, unique within the program.
    pub name: String,
    /// The steps, run in order.
    #[serde(default)]
    pub steps: Vec<ActionStep>,
}

/// A DOM event bound to an action, as it appears in an element's `props`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct EventHandler {
    /// The DOM event name, e.g. `"click"`.
    pub event: String,
    /// The action to run.
    pub action: String,
    /// Payload handed to the action, available to it as `var` bindings.
    #[serde(default)]
    pub payload: Option<PayloadSpec>,
    /// Debounce window in milliseconds.
    #[serde(default)]
    pub debounce: Option<u32>,
    /// Throttle window in milliseconds.
    #[serde(default)]
    pub throttle: Option<u32>,
}

/// The payload an [`EventHandler`] hands to its action.
///
/// Upstream allows either a single expression or a map of them, and both
/// spellings appear in generated documents, so both are accepted.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum PayloadSpec {
    /// One expression.
    Single(Expr),
    /// A map of named expressions.
    Map(BTreeMap<String, Expr>),
}

impl<'de> Deserialize<'de> for PayloadSpec {
    /// Discriminates on the presence of an `"expr"` key, for the same reason
    /// [`Prop`] does: `#[serde(untagged)]` would swallow the inner error and
    /// report only that nothing matched.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let raw = Value::deserialize(deserializer)?;
        if raw.get("expr").is_some() {
            Expr::deserialize(raw)
                .map(Self::Single)
                .map_err(D::Error::custom)
        } else {
            BTreeMap::<String, Expr>::deserialize(raw)
                .map(Self::Map)
                .map_err(D::Error::custom)
        }
    }
}

/// The value of one entry in an element's `props`: either an attribute
/// expression or an event binding.
///
/// The two are told apart structurally — an object carrying `"event"` is a
/// handler, anything else is an expression. See this module's `Deserialize`
/// impl for why that is done by hand rather than with `#[serde(untagged)]`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Prop {
    /// An attribute whose value is computed.
    Value(Expr),
    /// An event binding.
    Handler(Box<EventHandler>),
}

impl<'de> Deserialize<'de> for Prop {
    /// Discriminates on the presence of an `"event"` key.
    ///
    /// `#[serde(untagged)]` would do the same discrimination by trial
    /// deserialization, but it discards the inner error and reports only "data
    /// did not match any variant", which tells the author of a malformed
    /// handler nothing about what was actually wrong with it. Routing through
    /// [`Value`] costs one allocation per prop and preserves the real message,
    /// which is the whole point of the diagnostics in this module.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let raw = Value::deserialize(deserializer)?;
        if raw.get("event").is_some() {
            let handler = EventHandler::deserialize(raw).map_err(D::Error::custom)?;
            Ok(Self::Handler(Box::new(handler)))
        } else {
            let expr = Expr::deserialize(raw).map_err(D::Error::custom)?;
            Ok(Self::Value(expr))
        }
    }
}

/// When an [`Node::Island`]'s client behaviour should be hydrated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IslandStrategy {
    /// As soon as the page loads. The default.
    #[default]
    Load,
    /// When the browser is idle.
    Idle,
    /// When the island scrolls into view.
    Visible,
    /// On first interaction with the island.
    Interaction,
    /// When a media query matches.
    Media,
    /// Never; the island stays static.
    Never,
}

impl IslandStrategy {
    /// The strategy name as it appears in a document.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Idle => "idle",
            Self::Visible => "visible",
            Self::Interaction => "interaction",
            Self::Media => "media",
            Self::Never => "never",
        }
    }
}

/// A node in the view tree.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Node {
    /// An HTML element. The tag and every prop go through the safety
    /// allowlist in [`policy`](super::policy).
    Element {
        /// The HTML tag name.
        tag: String,
        /// A name for the element, referenced by `focus` steps.
        #[serde(default, rename = "ref")]
        element_ref: Option<String>,
        /// Attributes and event bindings.
        #[serde(default)]
        props: BTreeMap<String, Prop>,
        /// Child nodes.
        #[serde(default)]
        children: Vec<Self>,
    },
    /// A text node. Always HTML-escaped on render.
    Text {
        /// The text, computed.
        value: Expr,
    },
    /// Conditional rendering.
    If {
        /// The condition, evaluated for truthiness.
        condition: Expr,
        /// Rendered when the condition is truthy.
        then: Box<Self>,
        /// Rendered when the condition is falsy; nothing if absent.
        #[serde(default, rename = "else")]
        otherwise: Option<Box<Self>>,
    },
    /// List rendering.
    Each {
        /// The list to iterate.
        items: Expr,
        /// Name bound to each element.
        #[serde(rename = "as")]
        binding: String,
        /// Name bound to each element's index.
        #[serde(default)]
        index: Option<String>,
        /// A per-item key. Unused by the server renderer; kept so a document
        /// round-trips.
        #[serde(default)]
        key: Option<Expr>,
        /// The body rendered per element.
        body: Box<Self>,
    },
    /// An invocation of a named component.
    Component {
        /// The component name, as declared in the program's `components`.
        name: String,
        /// Arguments bound to the component's declared `params`.
        #[serde(default)]
        props: BTreeMap<String, Expr>,
        /// Slot content.
        #[serde(default)]
        children: Vec<Self>,
    },
    /// The insertion point for a component invocation's children.
    Slot {
        /// The slot name, for components with more than one.
        #[serde(default)]
        name: Option<String>,
    },
    /// Markdown, rendered through Autumn's sanitizing user-content path.
    ///
    /// Requires the `markdown` feature; without it a document containing this
    /// node fails validation rather than silently rendering nothing.
    Markdown {
        /// The Markdown source, computed.
        content: Expr,
    },
    /// A code block. The content is escaped, never highlighted server-side.
    Code {
        /// The language hint, emitted as a `language-*` class.
        language: Expr,
        /// The code, computed.
        content: Expr,
    },
    /// Content rendered into a different part of the host page.
    ///
    /// The server renderer does not splice these into the body; it collects
    /// them onto [`RenderedUi::portals`](super::RenderedUi::portals) so the
    /// host layout decides where they land.
    Portal {
        /// The destination, e.g. `"head"` or `"body"`.
        target: String,
        /// The content.
        #[serde(default)]
        children: Vec<Self>,
    },
    /// An interactive region. Rendered statically, with its hydration
    /// strategy carried on the wrapper as data attributes.
    Island {
        /// The island id, unique within the program.
        id: String,
        /// When to hydrate.
        #[serde(default)]
        strategy: IslandStrategy,
        /// The content.
        content: Box<Self>,
    },
    /// An async boundary. The server renders `content`, never the fallback:
    /// server-side there is nothing left pending.
    Suspense {
        /// The boundary id.
        #[serde(default)]
        id: Option<String>,
        /// Shown client-side while content is pending.
        fallback: Box<Self>,
        /// The content.
        content: Box<Self>,
    },
    /// An error boundary. The server renders `content`; `fallback` is
    /// validated and carried but only a client runtime can trigger it.
    ErrorBoundary {
        /// Shown when the content errors.
        fallback: Box<Self>,
        /// The content.
        content: Box<Self>,
    },
}

impl Node {
    /// The node's `kind` discriminant, for diagnostics.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Element { .. } => "element",
            Self::Text { .. } => "text",
            Self::If { .. } => "if",
            Self::Each { .. } => "each",
            Self::Component { .. } => "component",
            Self::Slot { .. } => "slot",
            Self::Markdown { .. } => "markdown",
            Self::Code { .. } => "code",
            Self::Portal { .. } => "portal",
            Self::Island { .. } => "island",
            Self::Suspense { .. } => "suspense",
            Self::ErrorBoundary { .. } => "errorBoundary",
        }
    }
}

/// A declared component parameter.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ParamDef {
    /// The parameter type: `string`, `number`, `boolean` or `json`.
    #[serde(rename = "type")]
    pub ty: String,
    /// Whether the parameter must be supplied. Defaults to `true`.
    #[serde(default = "default_true")]
    pub required: bool,
}

const fn default_true() -> bool {
    true
}

/// A reusable view fragment with its own parameters.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ComponentDef {
    /// Declared parameters.
    #[serde(default)]
    pub params: BTreeMap<String, ParamDef>,
    /// The component's view.
    pub view: Node,
}

/// Lifecycle hooks, each naming an action.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct Lifecycle {
    /// Runs when the view mounts.
    #[serde(default, rename = "onMount")]
    pub on_mount: Option<String>,
    /// Runs when the view unmounts.
    #[serde(default, rename = "onUnmount")]
    pub on_unmount: Option<String>,
    /// Runs when the route is entered.
    #[serde(default, rename = "onRouteEnter")]
    pub on_route_enter: Option<String>,
    /// Runs when the route is left.
    #[serde(default, rename = "onRouteLeave")]
    pub on_route_leave: Option<String>,
}

/// Page-level route metadata.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
pub struct RouteDefinition {
    /// The route path this document is written for.
    #[serde(default)]
    pub path: Option<String>,
    /// The document title, computed.
    #[serde(default)]
    pub title: Option<Expr>,
    /// `<meta>` values, each computed.
    #[serde(default)]
    pub meta: BTreeMap<String, Expr>,
}

/// A parsed Constela program.
///
/// This is the *syntactic* form. It becomes a
/// [`Document`](super::Document) — the type the renderer accepts — only by
/// passing [`validate`](super::validate).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Program {
    /// The DSL version. Must be `"1.0"`.
    pub version: String,
    /// Route metadata.
    #[serde(default)]
    pub route: Option<RouteDefinition>,
    /// Style presets referenced by [`Expr::Style`].
    #[serde(default)]
    pub styles: BTreeMap<String, StylePreset>,
    /// Lifecycle hooks.
    #[serde(default)]
    pub lifecycle: Option<Lifecycle>,
    /// Declared state.
    #[serde(default)]
    pub state: BTreeMap<String, StateField>,
    /// Declared actions.
    #[serde(default)]
    pub actions: Vec<ActionDefinition>,
    /// The view tree.
    pub view: Node,
    /// Declared components.
    #[serde(default)]
    pub components: BTreeMap<String, ComponentDef>,
}
