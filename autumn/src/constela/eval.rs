//! Evaluating Constela expressions.
//!
//! # Fidelity, and the one place it is deliberately broken
//!
//! Upstream Constela evaluates expressions in JavaScript, so its operators
//! inherit JavaScript's coercion rules. This module reproduces those rules
//! exactly — `"a" + 1` is `"a1"`, `[1,2] + ""` is `"1,2"`, `{} + ""` is
//! `"[object Object]"`, `<` on non-numbers compares stringified operands — so
//! that a document rendered on the server and the same document rendered by
//! the upstream client runtime agree.
//!
//! The exception is division and remainder by zero. JavaScript yields `NaN` and
//! `±Infinity`; JSON has no way to spell either, and
//! [`serde_json::Number`] will not hold them. Rather than pick a lossy stand-in
//! that would round-trip wrong, this module yields [`Value::Null`], which
//! renders as the empty string. It is the only intentional divergence, and it
//! only reaches a document that divides by zero.
//!
//! # Totality
//!
//! Evaluation never fails on a missing name. A `state`, `var`, `param` or
//! `route` reference that resolves to nothing yields [`Value::Null`], matching
//! JavaScript's `undefined` — and by the time a document is rendered,
//! [`validate`](super::validate) has already rejected every reference that
//! *statically* cannot resolve, so a null here means a value genuinely absent
//! at runtime (an optional route query parameter, say) rather than a typo.
//! The two failure modes are exceeding
//! [`RenderLimits::max_depth`](super::RenderLimits::max_depth), which is a
//! guard against a document engineered to overflow the stack, and exceeding
//! the byte budget.
//!
//! # Copies
//!
//! A read borrows. `eval_ref` returns a [`Cow`]: a `lit`, `state`, `var` or
//! `param` read is a borrow, and so is `get` or `index` on one. A value is
//! copied only when the caller keeps it, and the copy charges the budget
//! first. See `docs/adr/0025-constela-retention-accounting.md`.

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

use serde_json::{Map, Value};

use super::ast::{BinaryOp, Expr, RouteSource, StylePreset};
use super::budget::{Budget, SLOT_BYTES};
use super::error::{ConstelaError, Diagnostic, codes};
use super::path::Path;

/// The route values an [`Expr::Route`] reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteValues {
    /// Path parameters, e.g. `id` for `/posts/{id}`.
    pub params: BTreeMap<String, String>,
    /// Query-string parameters.
    pub query: BTreeMap<String, String>,
    /// The request path.
    pub path: String,
}

/// A value bound in an [`Env`]. Cloning one never copies the value.
#[derive(Debug, Clone)]
pub(crate) enum Bound<'a> {
    /// Borrowed from the document, the state, the payload or an outer scope.
    Borrowed(&'a Value),
    /// Built during the render, and shared by each scope that binds it.
    Shared(Rc<Value>),
}

impl Bound<'_> {
    /// The bound value.
    pub(crate) fn get(&self) -> &Value {
        match self {
            Self::Borrowed(value) => value,
            Self::Shared(value) => value,
        }
    }
}

impl<'a> From<Cow<'a, Value>> for Bound<'a> {
    fn from(value: Cow<'a, Value>) -> Self {
        match value {
            Cow::Borrowed(value) => Self::Borrowed(value),
            Cow::Owned(value) => Self::Shared(Rc::new(value)),
        }
    }
}

/// The lexical environment a view node is evaluated in: the component
/// parameters in scope, and the loop bindings in scope.
///
/// Cloned when entering a component or a loop body, so a nested scope can
/// never leak a binding back out to its parent. A clone copies no value: a
/// loop pass shares the outer bindings.
#[derive(Debug, Clone, Default)]
pub(crate) struct Env<'a> {
    /// Parameters of the innermost enclosing component, empty at the top level.
    pub params: Rc<BTreeMap<&'a str, Bound<'a>>>,
    /// Loop and payload bindings, innermost last.
    pub vars: Vec<(&'a str, Bound<'a>)>,
}

impl<'a> Env<'a> {
    /// Look up a `var` binding, innermost first.
    fn var(&self, name: &str) -> Option<&Value> {
        self.vars
            .iter()
            .rev()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.get())
    }

    /// Look up a component parameter.
    fn param(&self, name: &str) -> Option<&Value> {
        self.params.get(name).map(Bound::get)
    }

    /// Bind `name` for the duration of a nested render.
    pub(crate) fn with_var(&self, name: &'a str, value: Bound<'a>) -> Self {
        let mut next = self.clone();
        next.vars.push((name, value));
        next
    }
}

/// Everything an expression can read.
#[derive(Clone, Copy)]
pub(crate) struct EvalCtx<'a> {
    /// The current state values.
    pub state: &'a Map<String, Value>,
    /// The lexical environment.
    pub env: &'a Env<'a>,
    /// Route values.
    pub route: &'a RouteValues,
    /// Style presets, for [`Expr::Style`].
    pub styles: &'a BTreeMap<String, StylePreset>,
    /// Remaining recursion budget.
    pub depth: usize,
    /// Bytes an expression may still build or copy. Shared by every
    /// expression in the render or the dispatch. See [`Budget`].
    pub budget: &'a Budget,
}

impl EvalCtx<'_> {
    /// A context one level deeper, or an error if the depth or the fuel is
    /// spent. Burns one unit of fuel: each `eval_ref` call comes through here.
    fn descend(&self, path: Path<'_>) -> Result<Self, ConstelaError> {
        self.budget.burn(1, path)?;
        if self.depth == 0 {
            return Err(ConstelaError::Render(Diagnostic::new(
                path.to_string(),
                codes::RENDER_LIMIT,
                "expression nests deeper than the configured render depth limit",
            )));
        }
        Ok(Self {
            // Guarded by the zero check above; `saturating_sub` rather than
            // `-` so a future edit to that guard cannot turn this into a panic
            // on a hostile document.
            depth: self.depth.saturating_sub(1),
            ..*self
        })
    }
}

/// JavaScript truthiness.
///
/// `false`, `0`, `-0`, `NaN`, `""` and `null` are falsy; **everything else**,
/// including `[]` and `{}`, is truthy. The empty-array case is the one that
/// surprises people, and it is exactly why it is spelled out here rather than
/// left to a `is_empty()` somewhere.
#[must_use]
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JavaScript `String(value)`.
///
/// Used by `+` when either operand is not a number, and by `concat`. Note
/// `null` becomes the four characters `null` here, which is correct for string
/// concatenation and wrong for rendering — see [`text_of`].
#[must_use]
pub fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(true) => "true".to_string(),
        Value::Bool(false) => "false".to_string(),
        Value::Number(n) => number_to_string(n),
        Value::String(s) => s.clone(),
        // `String([1,null,2])` is `"1,,2"`: elements are joined with "," and
        // nullish elements contribute nothing.
        Value::Array(items) => items
            .iter()
            .map(|item| {
                if item.is_null() {
                    String::new()
                } else {
                    js_string(item)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// The text a value contributes to the document.
///
/// `null` renders as nothing, matching the upstream renderer's
/// `String(value ?? '')`. Everything else follows [`js_string`].
#[must_use]
pub fn text_of(value: &Value) -> String {
    if value.is_null() {
        String::new()
    } else {
        js_string(value)
    }
}

/// [`js_string`], without a copy for a string. Another value is charged to
/// `budget` before it is converted, because the conversion allocates. The
/// result is then owned and already charged: a caller that keeps it charges
/// only a borrowed result.
pub(crate) fn coerce<'v>(
    value: &'v Value,
    budget: &Budget,
    path: Path<'_>,
) -> Result<Cow<'v, str>, ConstelaError> {
    match value {
        Value::String(text) => Ok(Cow::Borrowed(text)),
        other => {
            budget.charge_value(other, path)?;
            Ok(Cow::Owned(js_string(other)))
        }
    }
}

/// [`text_of`], without a copy for a string. As [`coerce`], `null` aside.
pub(crate) fn text_coerce<'v>(
    value: &'v Value,
    budget: &Budget,
    path: Path<'_>,
) -> Result<Cow<'v, str>, ConstelaError> {
    if value.is_null() {
        Ok(Cow::Borrowed(""))
    } else {
        coerce(value, budget, path)
    }
}

/// Format a number the way JavaScript's `String` does for the common cases:
/// an integral value has no fractional part.
fn number_to_string(n: &serde_json::Number) -> String {
    let Some(f) = n.as_f64() else {
        return n.to_string();
    };
    if f == 0.0 {
        // Covers -0.0, which JavaScript also renders as "0".
        return "0".to_string();
    }

    // ECMAScript switches to exponential notation at |x| >= 1e21 and again at
    // 0 < |x| < 1e-6; Rust's `f64::to_string` switches nowhere, so it prints
    // 1e21 as twenty-two digits and 1e-7 as 0.0000001. Inside those thresholds
    // both agree, and Rust's shortest-round-trip digits are what ECMAScript
    // specifies too.
    let magnitude = f.abs();
    if !(1e-6..1e21).contains(&magnitude) {
        let exponential = format!("{f:e}");
        // Rust writes `1e21`; ECMAScript writes `1e+21`.
        return match exponential.split_once('e') {
            Some((mantissa, exponent)) if !exponent.starts_with('-') => {
                format!("{mantissa}e+{exponent}")
            }
            _ => exponential,
        };
    }

    if f.fract() == 0.0 {
        format!("{f:.0}")
    } else {
        f.to_string()
    }
}

/// The numeric value of `value`, or `0.0` — JavaScript's behaviour for the
/// arithmetic operators after Constela's own `typeof x === 'number'` guard,
/// which treats every non-number operand as zero rather than coercing it.
fn as_number(value: &Value) -> f64 {
    value.as_f64().unwrap_or(0.0)
}

/// Wrap an `f64` back into a JSON number, or `null` when it has no JSON
/// spelling (`NaN`, `±Infinity`). See this module's docs.
///
/// An integral result is narrowed back to an integer. JavaScript has one
/// number type and does not distinguish `3` from `3.0`, but `serde_json` does —
/// it keeps them as different `Number` representations — so leaving `1 + 2` as
/// the float `3.0` would make it serialize as `3.0` and, worse, compare unequal
/// to the literal `3` a document wrote. Narrowing here keeps arithmetic results
/// indistinguishable from the constants they equal.
pub(crate) fn number_value(f: f64) -> Value {
    if f.fract() == 0.0 && f >= -(2f64.powi(53)) && f <= 2f64.powi(53) {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "guarded above: integral and within i64's exactly-representable range"
        )]
        return Value::Number((f as i64).into());
    }
    serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)
}

/// Strict equality, JavaScript's `===`.
///
/// Numbers compare by value rather than by `serde_json` representation, so a
/// computed `3` equals a literal `3` however each was spelled. Everything else
/// is structural equality — JavaScript compares objects and arrays by
/// reference, which has no meaning in a value-based evaluator, and structural
/// equality is the only total answer available.
fn strict_eq(left: &Value, right: &Value) -> bool {
    match (left.as_f64(), right.as_f64()) {
        (Some(left), Some(right)) if left.is_finite() && right.is_finite() => left == right,
        _ => left == right,
    }
}

/// Follow a dotted path into a value, yielding `null` at the first miss.
///
/// A numeric segment indexes an array (`"items.0.title"`); any segment
/// indexes an object.
fn follow_path<'a>(mut current: &'a Value, path: &str) -> &'a Value {
    for segment in path.split('.') {
        if segment.is_empty() {
            continue;
        }
        current = match current {
            Value::Object(map) => map.get(segment).unwrap_or(&Value::Null),
            Value::Array(items) => segment
                .parse::<usize>()
                .ok()
                .and_then(|i| items.get(i))
                .unwrap_or(&Value::Null),
            _ => &Value::Null,
        };
        if current.is_null() {
            return &Value::Null;
        }
    }
    current
}

/// [`follow_path`] on an owned value: move the target out, so no copy is
/// made.
fn take_path(mut current: Value, path: &str) -> Value {
    for segment in path.split('.') {
        if segment.is_empty() {
            continue;
        }
        current = match current {
            Value::Object(mut map) => map.remove(segment).unwrap_or(Value::Null),
            Value::Array(items) => segment
                .parse::<usize>()
                .ok()
                .and_then(|i| items.into_iter().nth(i))
                .unwrap_or(Value::Null),
            _ => Value::Null,
        };
        if current.is_null() {
            return Value::Null;
        }
    }
    current
}

/// Apply a dotted path to a value, borrowed or owned. Neither form copies.
fn select_path<'a>(base: Cow<'a, Value>, path: &str) -> Cow<'a, Value> {
    match base {
        Cow::Borrowed(base) => Cow::Borrowed(follow_path(base, path)),
        Cow::Owned(base) => Cow::Owned(take_path(base, path)),
    }
}

/// Spend fuel for the length of a dotted path that a lookup will walk.
fn burn_sub(budget: &Budget, sub: Option<&str>, path: Path<'_>) -> Result<(), ConstelaError> {
    sub.map_or(Ok(()), |sub| budget.burn_scan(sub.len(), path))
}

/// Read a binding, then follow its optional dotted path. Borrows.
fn read<'a>(
    base: Option<&'a Value>,
    sub: Option<&str>,
    budget: &Budget,
    path: Path<'_>,
) -> Result<Cow<'a, Value>, ConstelaError> {
    burn_sub(budget, sub, path)?;
    Ok(lookup(base, sub))
}

/// Apply an optional dotted path to a looked-up base value. Borrows.
fn lookup<'a>(base: Option<&'a Value>, path: Option<&str>) -> Cow<'a, Value> {
    let Some(base) = base else {
        return null();
    };
    Cow::Borrowed(path.map_or(base, |path| follow_path(base, path)))
}

/// Evaluate `expr` to a value the caller owns.
///
/// A borrowed result is charged to the budget, then copied. This is the call
/// for a value that is kept: a state write, an effect operand, an `array`
/// element.
///
/// # Errors
///
/// As [`eval_ref`].
pub(crate) fn eval(expr: &Expr, ctx: &EvalCtx<'_>, path: Path<'_>) -> Result<Value, ConstelaError> {
    let value = eval_ref(expr, ctx, path)?;
    ctx.budget.own(value, path)
}

/// Evaluate `expr`, and borrow the result where possible.
///
/// A `lit`, `state`, `var` or `param` read borrows. So do `get`, `index`,
/// `cond`, `&&` and `||` on a borrowed operand. A form that builds a value
/// charges the budget before it allocates. This is the call for a value that
/// is only read: a text node, a condition, an attribute.
///
/// `path` locates the expression in the submitted document and is used only
/// for diagnostics.
///
/// # Errors
///
/// [`ConstelaError::Render`] when evaluation nests deeper than the context's
/// remaining depth budget, or builds more than its byte budget allows. Every
/// other failure mode resolves to [`Value::Null`]; see the module docs.
pub(crate) fn eval_ref<'a>(
    expr: &'a Expr,
    ctx: &EvalCtx<'a>,
    path: Path<'_>,
) -> Result<Cow<'a, Value>, ConstelaError> {
    let inner = ctx.descend(path)?;
    match expr {
        Expr::Lit { value } => Ok(Cow::Borrowed(value)),
        Expr::State { name, path: sub } => {
            read(ctx.state.get(name), sub.as_deref(), ctx.budget, path)
        }
        Expr::Var { name, path: sub } => read(ctx.env.var(name), sub.as_deref(), ctx.budget, path),
        Expr::Param { name, path: sub } => {
            read(ctx.env.param(name), sub.as_deref(), ctx.budget, path)
        }
        Expr::Route { name, source } => {
            let looked_up = match source {
                RouteSource::Param => ctx.route.params.get(name).map(String::as_str),
                RouteSource::Query => ctx.route.query.get(name).map(String::as_str),
                RouteSource::Path => Some(ctx.route.path.as_str()),
            };
            let Some(text) = looked_up else {
                return Ok(null());
            };
            ctx.budget.charge(text.len(), path)?;
            Ok(Cow::Owned(Value::String(text.to_string())))
        }
        Expr::Not { operand } => {
            let value = eval_ref(operand, &inner, path.key("operand"))?;
            Ok(Cow::Owned(Value::Bool(!truthy(&value))))
        }
        Expr::Cond {
            condition,
            then,
            otherwise,
        } => {
            let test = eval_ref(condition, &inner, path.key("if"))?;
            if truthy(&test) {
                eval_ref(then, &inner, path.key("then"))
            } else {
                eval_ref(otherwise, &inner, path.key("else"))
            }
        }
        Expr::Get { base, path: sub } => {
            let base = eval_ref(base, &inner, path.key("base"))?;
            burn_sub(ctx.budget, Some(sub), path)?;
            Ok(select_path(base, sub))
        }
        Expr::Index { base, key } => {
            let base = eval_ref(base, &inner, path.key("base"))?;
            let key = eval_ref(key, &inner, path.key("key"))?;
            index_into(base, &key, ctx.budget, path)
        }
        Expr::Concat { items } => {
            let mut out = String::new();
            for (i, item) in items.iter().enumerate() {
                let value = eval_ref(item, &inner, path.key("items").index(i))?;
                // Charged per piece, so the string is refused as it grows
                // rather than after it is whole. A converted piece was
                // charged when it was converted.
                match text_coerce(&value, ctx.budget, path)? {
                    Cow::Borrowed(piece) => {
                        ctx.budget.charge(piece.len(), path)?;
                        out.push_str(piece);
                    }
                    Cow::Owned(piece) => out.push_str(&piece),
                }
            }
            Ok(Cow::Owned(Value::String(out)))
        }
        Expr::Array { elements } => {
            ctx.budget
                .charge(SLOT_BYTES.saturating_mul(elements.len()), path)?;
            let mut out = Vec::with_capacity(elements.len());
            for (i, element) in elements.iter().enumerate() {
                // `eval`, not `eval_ref`: the element is kept, so a borrowed
                // one is charged and copied.
                out.push(eval(element, &inner, path.key("elements").index(i))?);
            }
            Ok(Cow::Owned(Value::Array(out)))
        }
        Expr::Obj { props } => {
            let mut out = Map::new();
            for (key, value) in props {
                ctx.budget
                    .charge(key.len().saturating_add(SLOT_BYTES), path)?;
                let value = eval(value, &inner, path.key("props").name(key))?;
                out.insert(key.clone(), value);
            }
            Ok(Cow::Owned(Value::Object(out)))
        }
        Expr::Bin { op, left, right } => eval_binary(*op, left, right, &inner, path),
        Expr::Style { name, variants } => {
            let Some(preset) = ctx.styles.get(name) else {
                return Ok(Cow::Owned(Value::String(String::new())));
            };
            let mut values = Vec::with_capacity(variants.len());
            for (axis, expr) in variants {
                let value = eval_ref(expr, &inner, path.key("variants").name(axis))?;
                values.push((axis.as_str(), value));
            }
            let mut selected = BTreeMap::new();
            for (axis, value) in &values {
                selected.insert(*axis, text_coerce(value, ctx.budget, path)?);
            }
            let classes = resolve_style(preset, &selected);
            ctx.budget.charge(classes.len(), path)?;
            Ok(Cow::Owned(Value::String(classes)))
        }
    }
}

/// `null`, borrowed.
const fn null<'a>() -> Cow<'a, Value> {
    Cow::Borrowed(&Value::Null)
}

/// Read a key as an array index: non-negative and integral.
fn array_slot(key: &Value) -> Option<usize> {
    let index = key.as_f64()?;
    if index < 0.0 || index.fract() != 0.0 {
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "guarded above: non-negative and integral"
    )]
    let index = index as usize;
    Some(index)
}

/// One character of a string, as a string.
///
/// JS indexes a string by code unit; indexing by chars is the closest total
/// equivalent that cannot split a UTF-8 sequence.
fn string_char<'a>(text: &str, key: &Value) -> Cow<'a, Value> {
    key.as_u64()
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| text.chars().nth(index))
        .map_or_else(null, |c| Cow::Owned(Value::String(c.to_string())))
}

/// Dynamic member access: a number indexes an array, anything else is
/// stringified and looks up an object key. Neither form copies the element:
/// a borrowed base yields a borrow, an owned base gives up the element.
fn index_into<'a>(
    base: Cow<'a, Value>,
    key: &Value,
    budget: &Budget,
    path: Path<'_>,
) -> Result<Cow<'a, Value>, ConstelaError> {
    Ok(match base {
        Cow::Borrowed(base) => match base {
            Value::Array(items) => array_slot(key)
                .and_then(|index| items.get(index))
                .map_or_else(null, Cow::Borrowed),
            Value::Object(map) => {
                let key = coerce(key, budget, path)?;
                map.get(key.as_ref()).map_or_else(null, Cow::Borrowed)
            }
            Value::String(text) => {
                budget.burn_scan(text.len(), path)?;
                string_char(text, key)
            }
            _ => null(),
        },
        Cow::Owned(base) => match base {
            Value::Array(items) => array_slot(key)
                .and_then(|index| items.into_iter().nth(index))
                .map_or_else(null, Cow::Owned),
            Value::Object(mut map) => {
                let key = coerce(key, budget, path)?;
                map.remove(key.as_ref()).map_or_else(null, Cow::Owned)
            }
            Value::String(text) => {
                budget.burn_scan(text.len(), path)?;
                string_char(&text, key)
            }
            _ => null(),
        },
    })
}

/// Assemble a style preset's class string for the selected variants.
///
/// Axes the caller did not select fall back to `defaultVariants`; a selection
/// naming an axis or option the preset does not have contributes nothing,
/// rather than failing the render.
fn resolve_style(preset: &StylePreset, selected: &BTreeMap<&str, Cow<'_, str>>) -> String {
    let mut classes = Vec::new();
    if !preset.base.is_empty() {
        classes.push(preset.base.as_str());
    }
    for (axis, options) in &preset.variants {
        let choice = selected
            .get(axis.as_str())
            .map(AsRef::as_ref)
            .or_else(|| preset.default_variants.get(axis).map(String::as_str));
        if let Some(class) = choice.and_then(|choice| options.get(choice))
            && !class.is_empty()
        {
            classes.push(class.as_str());
        }
    }
    classes.join(" ")
}

/// Evaluate a binary operation, short-circuiting `&&` and `||`.
fn eval_binary<'a>(
    op: BinaryOp,
    left: &'a Expr,
    right: &'a Expr,
    ctx: &EvalCtx<'a>,
    path: Path<'_>,
) -> Result<Cow<'a, Value>, ConstelaError> {
    let left_path = path.key("left");
    let right_path = path.key("right");

    // Short-circuit first, and yield the *operand*, not a boolean — `a || b`
    // is JavaScript's, not Rust's.
    if matches!(op, BinaryOp::And | BinaryOp::Or) {
        let left = eval_ref(left, ctx, left_path)?;
        let take_left = if op == BinaryOp::And {
            !truthy(&left)
        } else {
            truthy(&left)
        };
        return if take_left {
            Ok(left)
        } else {
            eval_ref(right, ctx, right_path)
        };
    }

    let left = eval_ref(left, ctx, left_path)?;
    let right = eval_ref(right, ctx, right_path)?;

    Ok(Cow::Owned(match op {
        BinaryOp::Add => {
            if left.is_number() && right.is_number() {
                number_value(as_number(&left) + as_number(&right))
            } else {
                let left = coerce(&left, ctx.budget, path)?;
                let right = coerce(&right, ctx.budget, path)?;
                // A converted operand was charged when it was converted.
                let borrowed = |text: &Cow<'_, str>| match text {
                    Cow::Borrowed(text) => text.len(),
                    Cow::Owned(_) => 0,
                };
                ctx.budget
                    .charge(borrowed(&left).saturating_add(borrowed(&right)), path)?;
                let len = left.len().saturating_add(right.len());
                let mut joined = String::with_capacity(len);
                joined.push_str(&left);
                joined.push_str(&right);
                Value::String(joined)
            }
        }
        BinaryOp::Sub => number_value(as_number(&left) - as_number(&right)),
        BinaryOp::Mul => number_value(as_number(&left) * as_number(&right)),
        BinaryOp::Div => {
            let divisor = as_number(&right);
            if divisor == 0.0 {
                // NaN / ±Infinity have no JSON spelling; see the module docs.
                Value::Null
            } else {
                number_value(as_number(&left) / divisor)
            }
        }
        BinaryOp::Rem => {
            let divisor = as_number(&right);
            if divisor == 0.0 {
                Value::Null
            } else {
                number_value(as_number(&left) % divisor)
            }
        }
        BinaryOp::Eq => {
            ctx.budget.burn_compare(&left, &right, path)?;
            Value::Bool(strict_eq(&left, &right))
        }
        BinaryOp::Ne => {
            ctx.budget.burn_compare(&left, &right, path)?;
            Value::Bool(!strict_eq(&left, &right))
        }
        BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            let ordering = if left.is_number() && right.is_number() {
                as_number(&left).partial_cmp(&as_number(&right))
            } else {
                let left = coerce(&left, ctx.budget, path)?;
                let right = coerce(&right, ctx.budget, path)?;
                ctx.budget.burn_scan(left.len().min(right.len()), path)?;
                Some(left.cmp(&right))
            };
            let Some(ordering) = ordering else {
                return Ok(Cow::Owned(Value::Bool(false)));
            };
            Value::Bool(match op {
                BinaryOp::Lt => ordering.is_lt(),
                BinaryOp::Le => ordering.is_le(),
                BinaryOp::Gt => ordering.is_gt(),
                _ => ordering.is_ge(),
            })
        }
        // Short-circuited at the top of this function, so this arm is dead.
        // It returns a value rather than panicking: an `unreachable!` here
        // would be a crash on a request carrying a hostile document if that
        // reasoning ever stopped holding, and `false` is the honest answer for
        // a logical operator that somehow reached the non-logical path.
        BinaryOp::And | BinaryOp::Or => {
            debug_assert!(false, "&& and || are short-circuited above");
            Value::Bool(false)
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn eval_str(source: &str) -> Value {
        eval_with(source, &Map::new(), &Env::default())
    }

    fn eval_with(source: &str, state: &Map<String, Value>, env: &Env) -> Value {
        let expr: Expr = serde_json::from_str(source).expect("expression parses");
        let styles = BTreeMap::new();
        let route = RouteValues::default();
        let budget = Budget::new(usize::MAX);
        let ctx = EvalCtx {
            state,
            env,
            route: &route,
            styles: &styles,
            depth: 64,
            budget: &budget,
        };
        eval(&expr, &ctx, Path::root("expr")).expect("evaluates")
    }

    #[test]
    fn truthiness_matches_javascript() {
        assert!(!truthy(&json!(null)));
        assert!(!truthy(&json!(false)));
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!("")));
        assert!(truthy(&json!("0")));
        // The two that catch people out: empty containers are truthy in JS.
        assert!(truthy(&json!([])));
        assert!(truthy(&json!({})));
    }

    #[test]
    fn js_string_matches_javascript() {
        assert_eq!(js_string(&json!(null)), "null");
        assert_eq!(js_string(&json!(true)), "true");
        assert_eq!(js_string(&json!(3)), "3");
        assert_eq!(js_string(&json!(3.5)), "3.5");
        assert_eq!(js_string(&json!([1, 2])), "1,2");
        assert_eq!(js_string(&json!([1, null, 2])), "1,,2");
        assert_eq!(js_string(&json!({"a": 1})), "[object Object]");
    }

    #[test]
    fn text_of_renders_null_as_nothing() {
        assert_eq!(text_of(&json!(null)), "");
        assert_eq!(text_of(&json!(0)), "0");
    }

    #[test]
    fn plus_adds_numbers_and_concatenates_everything_else() {
        assert_eq!(
            eval_str(
                r#"{"expr":"bin","op":"+","left":{"expr":"lit","value":1},"right":{"expr":"lit","value":2}}"#
            ),
            json!(3)
        );
        assert_eq!(
            eval_str(
                r#"{"expr":"bin","op":"+","left":{"expr":"lit","value":"a"},"right":{"expr":"lit","value":1}}"#
            ),
            json!("a1")
        );
        assert_eq!(
            eval_str(
                r#"{"expr":"bin","op":"+","left":{"expr":"lit","value":[1,2]},"right":{"expr":"lit","value":""}}"#
            ),
            json!("1,2")
        );
    }

    #[test]
    fn arithmetic_treats_non_numbers_as_zero() {
        assert_eq!(
            eval_str(
                r#"{"expr":"bin","op":"-","left":{"expr":"lit","value":"x"},"right":{"expr":"lit","value":2}}"#
            ),
            json!(-2)
        );
    }

    #[test]
    fn division_by_zero_is_null_not_nan() {
        for op in ["/", "%"] {
            let source = format!(
                r#"{{"expr":"bin","op":"{op}","left":{{"expr":"lit","value":1}},"right":{{"expr":"lit","value":0}}}}"#
            );
            assert_eq!(eval_str(&source), json!(null), "{op} by zero");
        }
    }

    #[test]
    fn equality_is_strict() {
        assert_eq!(
            eval_str(
                r#"{"expr":"bin","op":"==","left":{"expr":"lit","value":1},"right":{"expr":"lit","value":"1"}}"#
            ),
            json!(false)
        );
    }

    #[test]
    fn a_computed_number_equals_the_literal_it_is() {
        // `1 + 2` must equal `3`, and must serialize as `3` rather than `3.0`:
        // JavaScript has one number type, `serde_json` has two spellings.
        let sum = eval_str(
            r#"{"expr":"bin","op":"+","left":{"expr":"lit","value":1},"right":{"expr":"lit","value":2}}"#,
        );
        assert_eq!(sum.to_string(), "3");
        assert_eq!(
            eval_str(
                r#"{"expr":"bin","op":"==",
                    "left":{"expr":"bin","op":"+","left":{"expr":"lit","value":1},"right":{"expr":"lit","value":2}},
                    "right":{"expr":"lit","value":3}}"#
            ),
            json!(true)
        );
        // And a float that happens to equal an integer still compares equal.
        assert_eq!(
            eval_str(
                r#"{"expr":"bin","op":"==","left":{"expr":"lit","value":3.0},"right":{"expr":"lit","value":3}}"#
            ),
            json!(true)
        );
    }

    #[test]
    fn logical_operators_yield_the_operand() {
        assert_eq!(
            eval_str(
                r#"{"expr":"bin","op":"||","left":{"expr":"lit","value":""},"right":{"expr":"lit","value":"fallback"}}"#
            ),
            json!("fallback")
        );
        assert_eq!(
            eval_str(
                r#"{"expr":"bin","op":"&&","left":{"expr":"lit","value":0},"right":{"expr":"lit","value":"unused"}}"#
            ),
            json!(0)
        );
    }

    #[test]
    fn comparison_falls_back_to_string_ordering() {
        assert_eq!(
            eval_str(
                r#"{"expr":"bin","op":"<","left":{"expr":"lit","value":"apple"},"right":{"expr":"lit","value":"banana"}}"#
            ),
            json!(true)
        );
    }

    #[test]
    fn missing_references_are_null_not_errors() {
        assert_eq!(eval_str(r#"{"expr":"state","name":"nope"}"#), json!(null));
        assert_eq!(eval_str(r#"{"expr":"var","name":"nope"}"#), json!(null));
        assert_eq!(eval_str(r#"{"expr":"param","name":"nope"}"#), json!(null));
    }

    #[test]
    fn dotted_paths_walk_objects_and_arrays() {
        let mut state = Map::new();
        state.insert(
            "user".into(),
            json!({"address": {"city": "Kyoto"}, "tags": ["a", "b"]}),
        );
        assert_eq!(
            eval_with(
                r#"{"expr":"state","name":"user","path":"address.city"}"#,
                &state,
                &Env::default()
            ),
            json!("Kyoto")
        );
        assert_eq!(
            eval_with(
                r#"{"expr":"state","name":"user","path":"tags.1"}"#,
                &state,
                &Env::default()
            ),
            json!("b")
        );
        assert_eq!(
            eval_with(
                r#"{"expr":"state","name":"user","path":"address.zip"}"#,
                &state,
                &Env::default()
            ),
            json!(null)
        );
    }

    #[test]
    fn index_reads_arrays_objects_and_strings() {
        let mut state = Map::new();
        state.insert("items".into(), json!(["x", "y"]));
        let read = |key: &str| {
            eval_with(
                &format!(
                    r#"{{"expr":"index","base":{{"expr":"state","name":"items"}},"key":{{"expr":"lit","value":{key}}}}}"#
                ),
                &state,
                &Env::default(),
            )
        };
        assert_eq!(read("1"), json!("y"));
        assert_eq!(read("5"), json!(null));
        assert_eq!(read("-1"), json!(null));
        assert_eq!(read("1.5"), json!(null));
    }

    #[test]
    fn concat_skips_nulls() {
        assert_eq!(
            eval_str(
                r#"{"expr":"concat","items":[{"expr":"lit","value":"a"},{"expr":"state","name":"missing"},{"expr":"lit","value":"b"}]}"#
            ),
            json!("ab")
        );
    }

    #[test]
    fn style_preset_resolves_base_selection_and_defaults() {
        let preset: StylePreset = serde_json::from_str(
            r#"{"base":"btn","variants":{"size":{"sm":"btn-sm","lg":"btn-lg"},"tone":{"primary":"btn-primary"}},
                "defaultVariants":{"tone":"primary"}}"#,
        )
        .expect("preset parses");
        let mut selected = BTreeMap::new();
        selected.insert("size", Cow::Borrowed("lg"));
        assert_eq!(resolve_style(&preset, &selected), "btn btn-lg btn-primary");

        // An unknown option contributes nothing rather than failing.
        let mut unknown = BTreeMap::new();
        unknown.insert("size", Cow::Borrowed("xl"));
        assert_eq!(resolve_style(&preset, &unknown), "btn btn-primary");
    }

    #[test]
    fn depth_budget_is_enforced() {
        let mut source = String::new();
        for _ in 0..10 {
            source.push_str(r#"{"expr":"not","operand":"#);
        }
        source.push_str(r#"{"expr":"lit","value":true}"#);
        for _ in 0..10 {
            source.push('}');
        }
        let expr: Expr = serde_json::from_str(&source).expect("parses");
        let styles = BTreeMap::new();
        let route = RouteValues::default();
        let state = Map::new();
        let env = Env::default();
        let budget = Budget::new(usize::MAX);
        let ctx = EvalCtx {
            state: &state,
            env: &env,
            route: &route,
            styles: &styles,
            depth: 3,
            budget: &budget,
        };
        let err = eval(&expr, &ctx, Path::root("expr")).expect_err("over budget");
        assert_eq!(err.diagnostics()[0].code, codes::RENDER_LIMIT);
    }

    /// Evaluate with route values and style presets in scope, and a budget.
    fn eval_full(
        source: &str,
        route: &RouteValues,
        styles: &BTreeMap<String, StylePreset>,
        budget: &Budget,
    ) -> Result<Value, ConstelaError> {
        let expr: Expr = serde_json::from_str(source).expect("expression parses");
        let state = Map::new();
        let env = Env::default();
        let ctx = EvalCtx {
            state: &state,
            env: &env,
            route,
            styles,
            depth: 64,
            budget,
        };
        eval(&expr, &ctx, Path::root("expr"))
    }

    #[test]
    fn get_and_index_move_out_of_a_built_value() {
        let built = r#"{"expr":"obj","props":{"a":{"expr":"array","elements":[
            {"expr":"lit","value":"x"},{"expr":"lit","value":{"k":"v"}}]}}}"#;
        assert_eq!(
            eval_str(&format!(
                r#"{{"expr":"get","base":{built},"path":"a.1.k"}}"#
            )),
            json!("v")
        );
        assert_eq!(
            eval_str(&format!(r#"{{"expr":"get","base":{built},"path":"a.9"}}"#)),
            json!(null)
        );
        assert_eq!(
            eval_str(&format!(
                r#"{{"expr":"index","base":{built},"key":{{"expr":"lit","value":"a"}}}}"#
            )),
            json!(["x", {"k": "v"}])
        );
        let list =
            r#"{"expr":"array","elements":[{"expr":"lit","value":1},{"expr":"lit","value":2}]}"#;
        assert_eq!(
            eval_str(&format!(
                r#"{{"expr":"index","base":{list},"key":{{"expr":"lit","value":1}}}}"#
            )),
            json!(2)
        );
        assert_eq!(
            eval_str(&format!(
                r#"{{"expr":"index","base":{list},"key":{{"expr":"lit","value":5}}}}"#
            )),
            json!(null)
        );
    }

    #[test]
    fn index_reads_objects_strings_and_nothing_else() {
        let read = |base: &str, key: &str| {
            eval_str(&format!(
                r#"{{"expr":"index","base":{{"expr":"lit","value":{base}}},"key":{{"expr":"lit","value":{key}}}}}"#
            ))
        };
        assert_eq!(read(r#"{"1":"one"}"#, "1"), json!("one"));
        assert_eq!(read(r#"{"a":1}"#, r#""b""#), json!(null));
        assert_eq!(read(r#""héllo""#, "1"), json!("é"));
        assert_eq!(read(r#""abc""#, "9"), json!(null));
        assert_eq!(read(r#""abc""#, r#""x""#), json!(null));
        assert_eq!(read("7", "0"), json!(null));
        // A built string indexes the same way.
        assert_eq!(
            eval_str(
                r#"{"expr":"index","base":{"expr":"concat","items":[{"expr":"lit","value":"ab"}]},
                    "key":{"expr":"lit","value":1}}"#
            ),
            json!("b")
        );
    }

    #[test]
    fn route_values_are_read_by_source() {
        let mut route = RouteValues::default();
        route.params.insert("id".into(), "7".into());
        route.query.insert("q".into(), "tea".into());
        route.path = "/posts/7".into();
        let styles = BTreeMap::new();
        let budget = Budget::new(usize::MAX);
        let read = |source: &str| {
            eval_full(
                &format!(
                    r#"{{"expr":"route","name":"{}","source":"{source}"}}"#,
                    if source == "query" { "q" } else { "id" }
                ),
                &route,
                &styles,
                &budget,
            )
            .expect("evaluates")
        };
        assert_eq!(read("param"), json!("7"));
        assert_eq!(read("query"), json!("tea"));
        assert_eq!(read("path"), json!("/posts/7"));
        assert_eq!(
            eval_full(
                r#"{"expr":"route","name":"missing","source":"param"}"#,
                &route,
                &styles,
                &budget
            )
            .expect("evaluates"),
            json!(null)
        );
    }

    #[test]
    fn a_style_expression_resolves_its_preset() {
        let mut styles = BTreeMap::new();
        styles.insert(
            "btn".to_string(),
            serde_json::from_str::<StylePreset>(
                r#"{"base":"btn","variants":{"size":{"lg":"btn-lg"}}}"#,
            )
            .expect("preset parses"),
        );
        let route = RouteValues::default();
        let budget = Budget::new(usize::MAX);
        assert_eq!(
            eval_full(
                r#"{"expr":"style","name":"btn","variants":{"size":{"expr":"lit","value":"lg"}}}"#,
                &route,
                &styles,
                &budget
            )
            .expect("evaluates"),
            json!("btn btn-lg")
        );
        assert_eq!(
            eval_full(
                r#"{"expr":"style","name":"none"}"#,
                &route,
                &styles,
                &budget
            )
            .expect("evaluates"),
            json!("")
        );
    }

    #[test]
    fn coercions_follow_javascript() {
        let bin = |op: &str, left: &str, right: &str| {
            eval_str(&format!(
                r#"{{"expr":"bin","op":"{op}","left":{{"expr":"lit","value":{left}}},"right":{{"expr":"lit","value":{right}}}}}"#
            ))
        };
        assert_eq!(bin("+", "true", r#""x""#), json!("truex"));
        assert_eq!(bin("+", "null", r#""x""#), json!("nullx"));
        assert_eq!(bin("+", "[1,2]", "3"), json!("1,23"));
        assert_eq!(bin("<", "[1]", r#""2""#), json!(true));
        assert_eq!(bin(">=", r#""b""#, r#""a""#), json!(true));
        assert_eq!(bin("<=", "1", "1"), json!(true));
        assert_eq!(bin("!=", "1", "2"), json!(true));
        assert_eq!(bin("*", "3", "4"), json!(12));
        assert_eq!(bin("%", "7", "4"), json!(3));
        assert_eq!(bin("/", "7", "2"), json!(3.5));
        assert_eq!(
            eval_str(
                r#"{"expr":"concat","items":[{"expr":"lit","value":1},{"expr":"lit","value":null},{"expr":"lit","value":true}]}"#
            ),
            json!("1true")
        );
        assert_eq!(
            eval_str(
                r#"{"expr":"cond","if":{"expr":"lit","value":0},"then":{"expr":"lit","value":"a"},"else":{"expr":"lit","value":"b"}}"#
            ),
            json!("b")
        );
        assert_eq!(
            eval_str(r#"{"expr":"not","operand":{"expr":"lit","value":[]}}"#),
            json!(false)
        );
    }

    #[test]
    fn a_read_borrows_and_a_kept_copy_charges() {
        let mut state = Map::new();
        state.insert("big".into(), json!("x".repeat(100)));
        let expr: Expr = serde_json::from_str(r#"{"expr":"state","name":"big"}"#).expect("parses");
        let styles = BTreeMap::new();
        let route = RouteValues::default();
        let env = Env::default();
        let budget = Budget::new(10);
        let ctx = EvalCtx {
            state: &state,
            env: &env,
            route: &route,
            styles: &styles,
            depth: 64,
            budget: &budget,
        };
        // Borrowed: no charge, so a 10-byte budget is enough.
        let read = eval_ref(&expr, &ctx, Path::root("expr")).expect("borrows");
        assert!(matches!(read, Cow::Borrowed(_)));
        // Owned: the copy is charged, and 100 bytes is over the budget.
        let err = eval(&expr, &ctx, Path::root("expr")).expect_err("copy over budget");
        assert_eq!(err.diagnostics()[0].code, codes::RENDER_LIMIT);
    }

    #[test]
    fn coercing_a_non_string_charges_the_budget() {
        let budget = Budget::new(4);
        let value = json!([1, 2, 3]);
        coerce(&value, &budget, Path::root("p")).expect_err("over a 4-byte budget");
        let text = json!("plain");
        assert!(matches!(
            coerce(&text, &Budget::new(0), Path::root("p")).expect("strings borrow"),
            Cow::Borrowed("plain")
        ));
        assert_eq!(
            text_coerce(&json!(null), &Budget::new(0), Path::root("p")).expect("null is empty"),
            ""
        );
    }

    #[test]
    fn env_bindings_resolve_innermost_first() {
        let outer = json!("outer");
        let env = Env::default()
            .with_var("v", Bound::Borrowed(&outer))
            .with_var("v", Bound::Shared(Rc::new(json!("inner"))));
        assert_eq!(env.var("v"), Some(&json!("inner")));
        assert_eq!(env.var("missing"), None);
        let mut params = BTreeMap::new();
        params.insert("p", Bound::from(Cow::Owned(json!(1))));
        let env = Env {
            params: Rc::new(params),
            vars: Vec::new(),
        };
        assert_eq!(env.param("p"), Some(&json!(1)));
    }
}
