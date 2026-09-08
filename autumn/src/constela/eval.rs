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
//! The one failure mode is exceeding
//! [`RenderLimits::max_depth`](super::RenderLimits::max_depth), which is a
//! guard against a document engineered to overflow the stack.

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

use serde_json::{Map, Value};

use super::ast::{BinaryOp, Expr, RouteSource, StylePreset};
use super::error::{ConstelaError, Diagnostic, codes};

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

/// The lexical environment a view node is evaluated in: the component
/// parameters in scope, and the loop bindings in scope.
///
/// Cloned when entering a component or a loop body, so a nested scope can
/// never leak a binding back out to its parent.
#[derive(Debug, Clone, Default)]
pub(crate) struct Env {
    /// Parameters of the innermost enclosing component, empty at the top level.
    pub params: BTreeMap<String, Value>,
    /// Loop and payload bindings, innermost last.
    pub vars: Vec<(String, Value)>,
}

impl Env {
    /// Look up a `var` binding, innermost first.
    fn var(&self, name: &str) -> Option<&Value> {
        self.vars
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    /// Bind `name` for the duration of a nested render.
    pub(crate) fn with_var(&self, name: &str, value: Value) -> Self {
        let mut next = self.clone();
        next.vars.push((name.to_string(), value));
        next
    }
}

/// Everything an expression can read.
pub(crate) struct EvalCtx<'a> {
    /// The current state values.
    pub state: &'a Map<String, Value>,
    /// The lexical environment.
    pub env: &'a Env,
    /// Route values.
    pub route: &'a RouteValues,
    /// Style presets, for [`Expr::Style`].
    pub styles: &'a BTreeMap<String, StylePreset>,
    /// Remaining recursion budget.
    pub depth: usize,
}

impl EvalCtx<'_> {
    /// A context one level deeper, or an error if the budget is spent.
    fn descend(&self, path: &str) -> Result<EvalCtx<'_>, ConstelaError> {
        if self.depth == 0 {
            return Err(ConstelaError::Render(Diagnostic::new(
                path,
                codes::RENDER_LIMIT,
                "expression nests deeper than the configured render depth limit",
            )));
        }
        Ok(EvalCtx {
            state: self.state,
            env: self.env,
            route: self.route,
            styles: self.styles,
            // Guarded by the zero check above; `saturating_sub` rather than
            // `-` so a future edit to that guard cannot turn this into a panic
            // on a hostile document.
            depth: self.depth.saturating_sub(1),
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

/// Format a number the way JavaScript's `String` does for the common cases:
/// an integral value has no fractional part.
fn number_to_string(n: &serde_json::Number) -> String {
    n.as_f64().map_or_else(
        || n.to_string(),
        |f| {
            if f.fract() == 0.0 && f.abs() < 1e21 {
                format!("{f:.0}")
            } else {
                f.to_string()
            }
        },
    )
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

/// Apply an optional dotted path to a looked-up base value.
fn resolve(base: Option<&Value>, path: Option<&String>) -> Value {
    let Some(base) = base else {
        return Value::Null;
    };
    path.map_or_else(|| base.clone(), |path| follow_path(base, path).clone())
}

/// Evaluate `expr`.
///
/// `path` locates the expression in the submitted document and is used only
/// for diagnostics.
///
/// # Errors
///
/// [`ConstelaError::Render`] when evaluation nests deeper than the context's
/// remaining depth budget. Every other failure mode resolves to
/// [`Value::Null`]; see the module docs.
pub(crate) fn eval(expr: &Expr, ctx: &EvalCtx<'_>, path: &str) -> Result<Value, ConstelaError> {
    let inner = ctx.descend(path)?;
    match expr {
        Expr::Lit { value } => Ok(value.clone()),
        Expr::State { name, path: sub } => Ok(resolve(ctx.state.get(name), sub.as_ref())),
        Expr::Var { name, path: sub } => Ok(resolve(ctx.env.var(name), sub.as_ref())),
        Expr::Param { name, path: sub } => Ok(resolve(ctx.env.params.get(name), sub.as_ref())),
        Expr::Route { name, source } => {
            let looked_up = match source {
                RouteSource::Param => ctx.route.params.get(name).cloned(),
                RouteSource::Query => ctx.route.query.get(name).cloned(),
                RouteSource::Path => Some(ctx.route.path.clone()),
            };
            Ok(looked_up.map_or(Value::Null, Value::String))
        }
        Expr::Not { operand } => {
            let value = eval(operand, &inner, &format!("{path}.operand"))?;
            Ok(Value::Bool(!truthy(&value)))
        }
        Expr::Cond {
            condition,
            then,
            otherwise,
        } => {
            let test = eval(condition, &inner, &format!("{path}.if"))?;
            if truthy(&test) {
                eval(then, &inner, &format!("{path}.then"))
            } else {
                eval(otherwise, &inner, &format!("{path}.else"))
            }
        }
        Expr::Get { base, path: sub } => {
            let base = eval(base, &inner, &format!("{path}.base"))?;
            Ok(follow_path(&base, sub).clone())
        }
        Expr::Index { base, key } => {
            let base = eval(base, &inner, &format!("{path}.base"))?;
            let key = eval(key, &inner, &format!("{path}.key"))?;
            Ok(index_into(&base, &key))
        }
        Expr::Concat { items } => {
            let mut out = String::new();
            for (i, item) in items.iter().enumerate() {
                let value = eval(item, &inner, &format!("{path}.items[{i}]"))?;
                out.push_str(&text_of(&value));
            }
            Ok(Value::String(out))
        }
        Expr::Array { elements } => {
            let mut out = Vec::with_capacity(elements.len());
            for (i, element) in elements.iter().enumerate() {
                out.push(eval(element, &inner, &format!("{path}.elements[{i}]"))?);
            }
            Ok(Value::Array(out))
        }
        Expr::Obj { props } => {
            let mut out = Map::new();
            for (key, value) in props {
                out.insert(
                    key.clone(),
                    eval(value, &inner, &format!("{path}.props.{key}"))?,
                );
            }
            Ok(Value::Object(out))
        }
        Expr::Bin { op, left, right } => eval_binary(*op, left, right, &inner, path),
        Expr::Style { name, variants } => {
            let Some(preset) = ctx.styles.get(name) else {
                return Ok(Value::String(String::new()));
            };
            let mut selected = BTreeMap::new();
            for (axis, expr) in variants {
                let value = eval(expr, &inner, &format!("{path}.variants.{axis}"))?;
                selected.insert(axis.clone(), text_of(&value));
            }
            Ok(Value::String(resolve_style(preset, &selected)))
        }
    }
}

/// Dynamic member access: a number indexes an array, anything else is
/// stringified and looks up an object key.
fn index_into(base: &Value, key: &Value) -> Value {
    match base {
        Value::Array(items) => {
            let Some(index) = key.as_f64() else {
                return Value::Null;
            };
            if index < 0.0 || index.fract() != 0.0 {
                return Value::Null;
            }
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "guarded above: non-negative and integral"
            )]
            let index = index as usize;
            items.get(index).cloned().unwrap_or(Value::Null)
        }
        Value::Object(map) => map.get(&js_string(key)).cloned().unwrap_or(Value::Null),
        Value::String(s) => {
            // JS indexes a string by code unit; indexing by chars is the
            // closest total equivalent that cannot split a UTF-8 sequence.
            let Some(index) = key.as_u64() else {
                return Value::Null;
            };
            usize::try_from(index)
                .ok()
                .and_then(|i| s.chars().nth(i))
                .map_or(Value::Null, |c| Value::String(c.to_string()))
        }
        _ => Value::Null,
    }
}

/// Assemble a style preset's class string for the selected variants.
///
/// Axes the caller did not select fall back to `defaultVariants`; a selection
/// naming an axis or option the preset does not have contributes nothing,
/// rather than failing the render.
fn resolve_style(preset: &StylePreset, selected: &BTreeMap<String, String>) -> String {
    let mut classes = Vec::new();
    if !preset.base.is_empty() {
        classes.push(preset.base.as_str());
    }
    for (axis, options) in &preset.variants {
        let choice = selected
            .get(axis)
            .or_else(|| preset.default_variants.get(axis));
        if let Some(class) = choice.and_then(|choice| options.get(choice))
            && !class.is_empty()
        {
            classes.push(class.as_str());
        }
    }
    classes.join(" ")
}

/// Evaluate a binary operation, short-circuiting `&&` and `||`.
fn eval_binary(
    op: BinaryOp,
    left: &Expr,
    right: &Expr,
    ctx: &EvalCtx<'_>,
    path: &str,
) -> Result<Value, ConstelaError> {
    let left_path = format!("{path}.left");
    let right_path = format!("{path}.right");

    // Short-circuit first, and yield the *operand*, not a boolean — `a || b`
    // is JavaScript's, not Rust's.
    if matches!(op, BinaryOp::And | BinaryOp::Or) {
        let left = eval(left, ctx, &left_path)?;
        let take_left = if op == BinaryOp::And {
            !truthy(&left)
        } else {
            truthy(&left)
        };
        return if take_left {
            Ok(left)
        } else {
            eval(right, ctx, &right_path)
        };
    }

    let left = eval(left, ctx, &left_path)?;
    let right = eval(right, ctx, &right_path)?;

    Ok(match op {
        BinaryOp::Add => {
            if left.is_number() && right.is_number() {
                number_value(as_number(&left) + as_number(&right))
            } else {
                Value::String(format!("{}{}", js_string(&left), js_string(&right)))
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
        BinaryOp::Eq => Value::Bool(strict_eq(&left, &right)),
        BinaryOp::Ne => Value::Bool(!strict_eq(&left, &right)),
        BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            let ordering = if left.is_number() && right.is_number() {
                as_number(&left).partial_cmp(&as_number(&right))
            } else {
                Some(js_string(&left).cmp(&js_string(&right)))
            };
            let Some(ordering) = ordering else {
                return Ok(Value::Bool(false));
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
    })
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
        let ctx = EvalCtx {
            state,
            env,
            route: &route,
            styles: &styles,
            depth: 64,
        };
        eval(&expr, &ctx, "expr").expect("evaluates")
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
        selected.insert("size".to_string(), "lg".to_string());
        assert_eq!(resolve_style(&preset, &selected), "btn btn-lg btn-primary");

        // An unknown option contributes nothing rather than failing.
        let mut unknown = BTreeMap::new();
        unknown.insert("size".to_string(), "xl".to_string());
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
        let ctx = EvalCtx {
            state: &state,
            env: &env,
            route: &route,
            styles: &styles,
            depth: 3,
        };
        let err = eval(&expr, &ctx, "expr").expect_err("over budget");
        assert_eq!(err.diagnostics()[0].code, codes::RENDER_LIMIT);
    }
}
