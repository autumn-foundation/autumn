//! Running an action's **pure** steps on the server.
//!
//! # The line this module draws
//!
//! A Constela action is a list of steps, and they are not all the same kind of
//! thing. `set`, `update`, `setPath` and `if` are total functions from state to
//! state: given the same state and the same payload they produce the same
//! state, and they touch nothing else. `fetch`, `storage`, `navigate`,
//! `clipboard`, `delay`, `interval` and `focus` are requests to the *browser* —
//! they need a network stack the visitor's, not the server's, a DOM, a history
//! entry, a timer.
//!
//! [`dispatch`] runs the first group and *reports* the second as
//! [`Effect`]s. It does not perform them, and in particular it does not perform
//! `fetch`: a document that reached this point came from a language model, and
//! executing its outbound HTTP from inside the app would hand a prompt injection
//! the app's own network position — SSRF by construction. What the caller does
//! with a reported effect is the caller's decision, made with the caller's
//! knowledge of what the document is allowed to reach.
//!
//! # What that buys
//!
//! Enough to make a generated UI actually work with no client runtime at all.
//! Autumn is an htmx framework: an element rendered with
//! `data-constela-on-click="increment"` can be wired to a route that calls
//! [`dispatch`], re-renders, and swaps the fragment back. The counter
//! increments, the list filters, the form validates — server-side, in Rust,
//! with the state under the app's control the whole time. See
//! [`docs/guide/constela.md`] for the wiring.
//!
//! [`docs/guide/constela.md`]: https://github.com/autumn-foundation/autumn/blob/trunk/docs/guide/constela.md

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

use serde_json::{Map, Value};

use super::ast::{ActionStep, HttpMethod, StorageArea, StorageOperation, UpdateOperation};
use super::error::{ConstelaError, Diagnostic, codes};
use super::eval::{Env, EvalCtx, RouteValues, eval, js_string, number_value, truthy};

/// A browser-side step [`dispatch`] declined to perform, with its operands
/// already evaluated.
///
/// Handing these back rather than swallowing them keeps the decision where it
/// belongs: an app that wants a document's `fetch` to happen can make it happen
/// against its own allowlist, and an app that does not can ignore the effect
/// and still get the state transition the pure steps described.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Effect {
    /// The document asked for an HTTP request.
    Fetch {
        /// The evaluated URL. Guaranteed to have passed
        /// [`policy::is_allowed_url`](super::policy::is_allowed_url).
        url: String,
        /// The HTTP method.
        method: HttpMethod,
        /// The evaluated body, if any.
        body: Option<Value>,
        /// The name the response was to be bound to.
        result: Option<String>,
    },
    /// The document asked to read or write Web Storage.
    Storage {
        /// The operation.
        operation: StorageOperation,
        /// Which storage area.
        area: StorageArea,
        /// The evaluated key.
        key: String,
        /// The evaluated value, for `set`.
        value: Option<Value>,
        /// The name the read value was to be bound to, for `get`.
        result: Option<String>,
    },
    /// The document asked to navigate.
    Navigate {
        /// The evaluated URL. Scheme-checked, as with [`Self::Fetch`].
        url: String,
        /// Whether to replace the current history entry.
        replace: bool,
    },
    /// The document asked to run steps after a delay.
    Delay {
        /// The evaluated delay, in milliseconds.
        ms: f64,
    },
    /// The document asked to run an action on a repeating timer.
    Interval {
        /// The evaluated interval, in milliseconds.
        ms: f64,
        /// The action to run each tick.
        action: String,
    },
    /// The document asked to move focus.
    Focus {
        /// The `ref` name of the target element.
        target: String,
        /// The focus operation.
        operation: String,
    },
}

/// What running an action did.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Dispatched {
    /// The effects the server declined to perform, in the order they were
    /// reached. Empty when the action was entirely pure.
    pub effects: Vec<Effect>,
    /// The step at which execution stopped, if it did — the JSON path of the
    /// first effectful step reached.
    ///
    /// **Everything after that step was not run**, and that is a correctness
    /// requirement rather than caution. An effect can bind a `result` that
    /// later steps read; the server did not perform the effect, so it has no
    /// value to bind, and running on would evaluate those reads as `null` and
    /// commit the answer to state. A `fetch` that fails to load followed by
    /// `set data = var(res)` would silently write `null` over good data. More
    /// generally, once an effect is outstanding the rest of the action belongs
    /// to whoever performs it, so the server stops and hands back what it has.
    pub suspended_at: Option<String>,
}

impl Dispatched {
    /// Whether the action ran to completion without asking for anything the
    /// server would not do.
    #[must_use]
    pub const fn is_pure(&self) -> bool {
        self.effects.is_empty()
    }

    /// Whether execution stopped early at an effect, leaving later steps
    /// un-run. See [`Self::suspended_at`].
    #[must_use]
    pub const fn is_suspended(&self) -> bool {
        self.suspended_at.is_some()
    }
}

/// Everything an action reads while it runs.
pub struct DispatchCtx<'a> {
    pub route: &'a RouteValues,
    pub styles: &'a std::collections::BTreeMap<String, super::ast::StylePreset>,
    pub depth: usize,
}

/// Run `steps` against `state`.
///
/// State is mutated in place. When an effectful step is reached it is recorded
/// and **execution stops** — neither its nested `onSuccess`/`onError`/`then`
/// branches nor any later sibling step is run. The server does not know which
/// branch would have been taken, and it cannot bind the `result` the effect was
/// to produce, so continuing would evaluate later reads of that binding as
/// `null` and commit the answer to state. See [`Dispatched::suspended_at`].
pub fn run_steps(
    steps: &[ActionStep],
    state: &mut Map<String, Value>,
    env: &Env,
    ctx: &DispatchCtx<'_>,
    path: &str,
    out: &mut Dispatched,
) -> Result<(), ConstelaError> {
    for (i, step) in steps.iter().enumerate() {
        run_step(step, state, env, ctx, &format!("{path}[{i}]"), out)?;
        // Also catches a suspension inside an `if` branch, which recurses
        // through this same loop.
        if out.is_suspended() {
            return Ok(());
        }
    }
    Ok(())
}

fn run_step(
    step: &ActionStep,
    state: &mut Map<String, Value>,
    env: &Env,
    ctx: &DispatchCtx<'_>,
    path: &str,
    out: &mut Dispatched,
) -> Result<(), ConstelaError> {
    // Each expression is evaluated against the state as it stands *now*, so a
    // later step in the same action sees what an earlier one wrote.
    macro_rules! eval_in {
        ($expr:expr, $sub:expr) => {{
            let eval_ctx = EvalCtx {
                state,
                env,
                route: ctx.route,
                styles: ctx.styles,
                depth: ctx.depth,
            };
            eval($expr, &eval_ctx, $sub)?
        }};
    }

    match step {
        ActionStep::Set { target, value } => {
            let value = eval_in!(value, &format!("{path}.value"));
            state.insert(target.clone(), value);
        }
        ActionStep::Update {
            target,
            operation,
            value,
            index,
            delete_count,
        } => {
            let operand = match value {
                Some(expr) => Some(eval_in!(expr, &format!("{path}.value"))),
                None => None,
            };
            let index = match index {
                Some(expr) => Some(eval_in!(expr, &format!("{path}.index"))),
                None => None,
            };
            let count = match delete_count {
                Some(expr) => Some(eval_in!(expr, &format!("{path}.deleteCount"))),
                None => None,
            };
            apply_update(
                state,
                target,
                *operation,
                operand.as_ref(),
                index.as_ref(),
                count.as_ref(),
            );
        }
        ActionStep::SetPath {
            target,
            path: target_path,
            value,
        } => {
            let segments = eval_in!(target_path, &format!("{path}.path"));
            let value = eval_in!(value, &format!("{path}.value"));
            let segments = path_segments(&segments);

            // The depth of the *structure*, not just of the walk. `set_at_path`
            // is iterative, so writing through a long path is cheap — but it
            // creates one nested object per segment, and `serde_json::Value`
            // drops **recursively**. A 200 000-segment path would therefore
            // overflow the stack when that state is eventually freed, long
            // after the step that built it returned and somewhere with no
            // connection to the document that caused it. Refusing the write is
            // the only fix that holds: no legitimate UI addresses a field two
            // hundred thousand levels down.
            if segments.len() > ctx.depth {
                return Err(ConstelaError::Render(Diagnostic::new(
                    format!("{path}.path"),
                    codes::RENDER_LIMIT,
                    format!(
                        "`setPath` path has {} segments, over the {} the depth limit allows",
                        segments.len(),
                        ctx.depth
                    ),
                )));
            }

            if let Some(slot) = state.get_mut(target) {
                set_at_path(slot, &segments, value);
            }
        }
        ActionStep::If {
            condition,
            then,
            otherwise,
        } => {
            let test = eval_in!(condition, &format!("{path}.condition"));
            let branch = if truthy(&test) { then } else { otherwise };
            let branch_path = if truthy(&test) {
                format!("{path}.then")
            } else {
                format!("{path}.else")
            };
            run_steps(branch, state, env, ctx, &branch_path, out)?;
        }
        effectful => record_effect(effectful, state, env, ctx, path, out)?,
    }
    Ok(())
}

/// Evaluate a browser-side step's operands and record it as an [`Effect`],
/// without performing it.
///
/// Split from [`run_step`] so the pure transitions above and the effects here
/// read as the two halves they are — and so the one rule that matters is in
/// one place: **the nested branches are not walked.** A `fetch` has an
/// `onSuccess` and an `onError`, and the server, having not made the request,
/// does not know which one the browser would take. Running either would apply
/// a state transition the document did not ask for.
fn record_effect(
    step: &ActionStep,
    state: &Map<String, Value>,
    env: &Env,
    ctx: &DispatchCtx<'_>,
    path: &str,
    out: &mut Dispatched,
) -> Result<(), ConstelaError> {
    macro_rules! eval_in {
        ($expr:expr, $sub:expr) => {{
            let eval_ctx = EvalCtx {
                state,
                env,
                route: ctx.route,
                styles: ctx.styles,
                depth: ctx.depth,
            };
            eval($expr, &eval_ctx, $sub)?
        }};
    }

    let effect = match step {
        ActionStep::Fetch {
            url,
            method,
            body,
            result,
            ..
        } => {
            let url = js_string(&eval_in!(url, &format!("{path}.url")));
            check_url(&url, &format!("{path}.url"))?;
            let body = match body {
                Some(expr) => Some(eval_in!(expr, &format!("{path}.body"))),
                None => None,
            };
            Effect::Fetch {
                url,
                method: *method,
                body,
                result: result.clone(),
            }
        }
        ActionStep::Storage {
            operation,
            key,
            value,
            storage,
            result,
            ..
        } => {
            let key = js_string(&eval_in!(key, &format!("{path}.key")));
            let value = match value {
                Some(expr) => Some(eval_in!(expr, &format!("{path}.value"))),
                None => None,
            };
            Effect::Storage {
                operation: *operation,
                area: *storage,
                key,
                value,
                result: result.clone(),
            }
        }
        ActionStep::Navigate { url, replace } => {
            let url = js_string(&eval_in!(url, &format!("{path}.url")));
            check_url(&url, &format!("{path}.url"))?;
            Effect::Navigate {
                url,
                replace: *replace,
            }
        }
        ActionStep::Delay { ms, .. } => Effect::Delay {
            ms: eval_in!(ms, &format!("{path}.ms")).as_f64().unwrap_or(0.0),
        },
        ActionStep::Interval { ms, action } => Effect::Interval {
            ms: eval_in!(ms, &format!("{path}.ms")).as_f64().unwrap_or(0.0),
            action: action.clone(),
        },
        ActionStep::Focus { target, operation } => Effect::Focus {
            target: js_string(&eval_in!(target, &format!("{path}.target"))),
            operation: operation.clone(),
        },
        // The pure steps are handled by `run_step` and never reach here.
        pure => {
            debug_assert!(
                pure.is_pure(),
                "{} is neither pure nor an effect",
                pure.kind()
            );
            return Ok(());
        }
    };
    out.effects.push(effect);
    out.suspended_at = Some(path.to_string());
    Ok(())
}

/// Re-check a URL that was assembled at runtime.
///
/// The document's *literal* URLs were checked during validation; this covers
/// the ones built by `concat` out of state, which validation could not see.
fn check_url(url: &str, path: &str) -> Result<(), ConstelaError> {
    if super::policy::is_allowed_url(url) {
        Ok(())
    } else {
        Err(ConstelaError::Render(Diagnostic::new(
            path,
            codes::URL_SCHEME,
            format!("computed URL {url:?} uses a scheme that is not allowed"),
        )))
    }
}

/// Apply an `update` operation in place.
///
/// A mismatch between the operation and the value actually in the slot is a
/// no-op rather than an error: validation already checked the *declared* type,
/// so reaching here with a mismatch means the state was replaced at runtime
/// with something else, and dropping one update is a better failure than
/// refusing to render the page.
fn apply_update(
    state: &mut Map<String, Value>,
    target: &str,
    operation: UpdateOperation,
    value: Option<&Value>,
    index: Option<&Value>,
    delete_count: Option<&Value>,
) {
    let Some(slot) = state.get_mut(target) else {
        return;
    };

    match operation {
        UpdateOperation::Increment | UpdateOperation::Decrement => {
            let Some(current) = slot.as_f64() else { return };
            let step = value.and_then(Value::as_f64).unwrap_or(1.0);
            let next = if operation == UpdateOperation::Increment {
                current + step
            } else {
                current - step
            };
            // Through `number_value` so an integral result stays an integer,
            // the same normalization the evaluator's arithmetic uses.
            let next = number_value(next);
            if !next.is_null() {
                *slot = next;
            }
        }
        UpdateOperation::Toggle => {
            if let Some(current) = slot.as_bool() {
                *slot = Value::Bool(!current);
            }
        }
        UpdateOperation::Merge => {
            let (Some(target_map), Some(Value::Object(source))) = (slot.as_object_mut(), value)
            else {
                return;
            };
            for (key, value) in source {
                target_map.insert(key.clone(), value.clone());
            }
        }
        UpdateOperation::Push => {
            let (Some(items), Some(value)) = (slot.as_array_mut(), value) else {
                return;
            };
            items.push(value.clone());
        }
        UpdateOperation::Pop => {
            if let Some(items) = slot.as_array_mut() {
                items.pop();
            }
        }
        UpdateOperation::Remove => {
            let (Some(items), Some(value)) = (slot.as_array_mut(), value) else {
                return;
            };
            items.retain(|item| item != value);
        }
        UpdateOperation::ReplaceAt => {
            let (Some(items), Some(value), Some(at)) =
                (slot.as_array_mut(), value, index.and_then(array_index))
            else {
                return;
            };
            if let Some(entry) = items.get_mut(at) {
                *entry = value.clone();
            }
        }
        UpdateOperation::InsertAt => {
            let (Some(items), Some(value), Some(at)) =
                (slot.as_array_mut(), value, index.and_then(array_index))
            else {
                return;
            };
            items.insert(at.min(items.len()), value.clone());
        }
        UpdateOperation::Splice => {
            let (Some(items), Some(at), Some(count)) = (
                slot.as_array_mut(),
                index.and_then(array_index),
                delete_count.and_then(array_index),
            ) else {
                return;
            };
            let at = at.min(items.len());
            let end = at.saturating_add(count).min(items.len());
            let inserted: Vec<Value> = match value {
                Some(Value::Array(values)) => values.clone(),
                Some(other) => vec![other.clone()],
                None => Vec::new(),
            };
            items.splice(at..end, inserted);
        }
    }
}

/// Read a JSON value as a non-negative array index.
fn array_index(value: &Value) -> Option<usize> {
    let raw = value.as_f64()?;
    if raw < 0.0 || raw.fract() != 0.0 {
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "guarded above: non-negative and integral"
    )]
    let index = raw as usize;
    Some(index)
}

/// Read a `setPath` path as segments: an array of values, or a dotted string.
fn path_segments(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items.iter().map(js_string).collect(),
        other => js_string(other)
            .split('.')
            .filter(|segment| !segment.is_empty())
            .map(ToString::to_string)
            .collect(),
    }
}

/// Write `value` at `segments` within `slot`, creating intermediate objects
/// where the path does not exist yet.
///
/// An out-of-range array index is a no-op — a list does not grow to fit a write
/// past its end, matching JavaScript's `arr[10] = x` on a 2-element array being
/// something a UI never wants.
///
/// **Iterative, and that is not a style choice.** `segments` comes from the
/// document: a `setPath` whose `path` is a dotted string yields one segment per
/// `.`, so a half-megabyte of dots is a half-million segments. A recursive walk
/// would be a half-million stack frames and a crashed process, reachable by
/// anyone who can shape the document. The loop makes the cost linear in the
/// path length instead, which the parse limits already bound.
fn set_at_path(slot: &mut Value, segments: &[String], value: Value) {
    let Some((last, parents)) = segments.split_last() else {
        *slot = value;
        return;
    };

    let mut current = slot;
    for segment in parents {
        // A scalar cannot be indexed into; replace it with an object so the
        // write lands rather than silently disappearing.
        if !current.is_object() && !current.is_array() {
            *current = Value::Object(Map::new());
        }
        current = match current {
            Value::Array(items) => {
                let Ok(index) = segment.parse::<usize>() else {
                    return;
                };
                let Some(entry) = items.get_mut(index) else {
                    return;
                };
                entry
            }
            Value::Object(map) => map.entry(segment.clone()).or_insert(Value::Null),
            // Unreachable: normalized to an object immediately above.
            _ => return,
        };
    }

    if !current.is_object() && !current.is_array() {
        *current = Value::Object(Map::new());
    }
    match current {
        Value::Array(items) => {
            let Ok(index) = last.parse::<usize>() else {
                return;
            };
            if let Some(entry) = items.get_mut(index) {
                *entry = value;
            }
        }
        Value::Object(map) => {
            map.insert(last.clone(), value);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state_of(value: &Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "a test helper called with `json!(...)` literals; taking \
                  references would put `&` on every call site for nothing"
    )]
    fn update(
        initial: Value,
        operation: UpdateOperation,
        value: Option<Value>,
        index: Option<Value>,
        count: Option<Value>,
    ) -> Value {
        let mut state = state_of(&json!({ "x": initial }));
        apply_update(
            &mut state,
            "x",
            operation,
            value.as_ref(),
            index.as_ref(),
            count.as_ref(),
        );
        state["x"].clone()
    }

    #[test]
    fn increment_defaults_to_one_and_honours_an_operand() {
        assert_eq!(
            update(json!(1), UpdateOperation::Increment, None, None, None),
            json!(2)
        );
        assert_eq!(
            update(
                json!(1),
                UpdateOperation::Increment,
                Some(json!(5)),
                None,
                None
            ),
            json!(6)
        );
        assert_eq!(
            update(json!(1), UpdateOperation::Decrement, None, None, None),
            json!(0)
        );
    }

    #[test]
    fn list_operations() {
        assert_eq!(
            update(
                json!([1]),
                UpdateOperation::Push,
                Some(json!(2)),
                None,
                None
            ),
            json!([1, 2])
        );
        assert_eq!(
            update(json!([1, 2]), UpdateOperation::Pop, None, None, None),
            json!([1])
        );
        assert_eq!(
            update(
                json!([1, 2, 1]),
                UpdateOperation::Remove,
                Some(json!(1)),
                None,
                None
            ),
            json!([2])
        );
        assert_eq!(
            update(
                json!([1, 2]),
                UpdateOperation::ReplaceAt,
                Some(json!(9)),
                Some(json!(1)),
                None
            ),
            json!([1, 9])
        );
        assert_eq!(
            update(
                json!([1, 2]),
                UpdateOperation::InsertAt,
                Some(json!(9)),
                Some(json!(1)),
                None
            ),
            json!([1, 9, 2])
        );
        assert_eq!(
            update(
                json!([1, 2, 3]),
                UpdateOperation::Splice,
                Some(json!([8, 9])),
                Some(json!(1)),
                Some(json!(1))
            ),
            json!([1, 8, 9, 3])
        );
    }

    #[test]
    fn out_of_range_indices_are_no_ops_not_panics() {
        assert_eq!(
            update(
                json!([1]),
                UpdateOperation::ReplaceAt,
                Some(json!(9)),
                Some(json!(7)),
                None
            ),
            json!([1])
        );
        assert_eq!(
            update(
                json!([1]),
                UpdateOperation::InsertAt,
                Some(json!(9)),
                Some(json!(7)),
                None
            ),
            json!([1, 9])
        );
        assert_eq!(
            update(
                json!([1]),
                UpdateOperation::Splice,
                None,
                Some(json!(7)),
                Some(json!(7))
            ),
            json!([1])
        );
        assert_eq!(
            update(
                json!([1]),
                UpdateOperation::ReplaceAt,
                Some(json!(9)),
                Some(json!(-1)),
                None
            ),
            json!([1])
        );
    }

    #[test]
    fn toggle_and_merge() {
        assert_eq!(
            update(json!(true), UpdateOperation::Toggle, None, None, None),
            json!(false)
        );
        assert_eq!(
            update(
                json!({"a": 1}),
                UpdateOperation::Merge,
                Some(json!({"b": 2})),
                None,
                None
            ),
            json!({"a": 1, "b": 2})
        );
    }

    #[test]
    fn a_type_mismatch_at_runtime_is_dropped_not_fatal() {
        assert_eq!(
            update(json!("text"), UpdateOperation::Increment, None, None, None),
            json!("text")
        );
        assert_eq!(
            update(json!(3), UpdateOperation::Push, Some(json!(1)), None, None),
            json!(3)
        );
    }

    #[test]
    fn set_path_walks_and_creates_objects() {
        let mut slot = json!({"user": {"name": "a"}});
        set_at_path(&mut slot, &["user".into(), "name".into()], json!("b"));
        assert_eq!(slot, json!({"user": {"name": "b"}}));

        let mut created = json!({});
        set_at_path(&mut created, &["a".into(), "b".into()], json!(1));
        assert_eq!(created, json!({"a": {"b": 1}}));
    }

    #[test]
    fn set_path_indexes_arrays_and_ignores_out_of_range() {
        let mut slot = json!({"posts": [{"liked": false}]});
        set_at_path(
            &mut slot,
            &["posts".into(), "0".into(), "liked".into()],
            json!(true),
        );
        assert_eq!(slot, json!({"posts": [{"liked": true}]}));

        let mut untouched = json!({"posts": [{"liked": false}]});
        set_at_path(
            &mut untouched,
            &["posts".into(), "5".into(), "liked".into()],
            json!(true),
        );
        assert_eq!(untouched, json!({"posts": [{"liked": false}]}));
    }

    #[test]
    fn a_deep_path_is_written_iteratively() {
        // 1 000 segments: far past any real UI, and far past what a recursive
        // walk would survive in a debug build. Kept well under the depth guard
        // in `run_step` so this exercises the walk rather than the refusal —
        // and shallow enough that dropping the structure it builds is safe,
        // which is the very thing the guard exists to protect.
        let segments: Vec<String> = (0..1_000).map(|i| format!("s{i}")).collect();
        let mut slot = json!({});
        set_at_path(&mut slot, &segments, json!("deep"));

        let mut cursor = &slot;
        for segment in &segments {
            cursor = cursor.get(segment).expect("segment exists");
        }
        assert_eq!(cursor, &json!("deep"));
    }

    #[test]
    fn path_segments_accepts_dotted_strings_and_arrays() {
        assert_eq!(
            path_segments(&json!("a.b.c")),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!(
            path_segments(&json!(["a", 0, "c"])),
            vec!["a".to_string(), "0".to_string(), "c".to_string()]
        );
    }
}
