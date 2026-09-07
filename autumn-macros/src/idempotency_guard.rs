use syn::{Block, Expr, ExprIf, Item, Pat, Stmt, Type};

const REPLAY_GUARD_IDENT: &str = "__AUTUMN_IDEMPOTENCY_REPLAY_GUARD";

pub fn block_has_replay_guard(block: &Block) -> bool {
    block_has_generated_replay_guard(block)
}

/// Whether a pre-body `FromRequestParts` gate macro (`#[secured]`,
/// `#[step_up]`, `#[throttle]` — issue #1668) about to attach its own gate to
/// `input_fn` should also make that gate responsible for serving a cached
/// idempotency replay.
///
/// Exactly one guard may own replay-serving, and it must be the one every
/// OTHER stacked guard's check is already guaranteed to have passed by the
/// time it runs. That is never true when:
///
/// - another guard's gate parameter is already present — [`has_any_guard_gate_param`];
/// - an earlier-expanded in-body guard (`#[authorize]`, or a gate that
///   deferred for one of these same reasons) already owns replay —
///   [`block_has_replay_guard`];
/// - `#[authorize]` is still an unexpanded attribute and will run AFTER this
///   gate: its policy check lives inside the handler body, which only runs
///   once every extractor — including this gate — has already succeeded, so
///   a gate that served a cached replay itself would return before
///   `#[authorize]`'s check ever ran.
pub fn should_own_replay(input_fn: &syn::ItemFn) -> bool {
    !crate::param_helpers::has_any_guard_gate_param(input_fn)
        && !block_has_replay_guard(&input_fn.block)
        && !has_pending_authorize_attr(input_fn)
}

fn has_pending_authorize_attr(input_fn: &syn::ItemFn) -> bool {
    input_fn.attrs.iter().any(|attr| {
        attr.path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "authorize")
    })
}

fn block_has_generated_replay_guard(block: &Block) -> bool {
    let mut index = 0;
    while let Some(stmt) = block.stmts.get(index) {
        if stmt_is_replay_guard_marker(stmt)
            && block
                .stmts
                .get(index + 1)
                .is_some_and(stmt_is_generated_replay_guard)
        {
            return true;
        }

        if stmt_is_generated_auth_prologue(stmt)
            || stmt_is_generated_throttle_prologue(stmt)
            || stmt_is_generated_sunset_check_prologue(stmt)
        {
            index += 1;
            continue;
        }

        if let Some(nested) = generated_nested_response_body(block, index) {
            return block_has_generated_replay_guard(nested);
        }

        return false;
    }

    false
}

fn stmt_is_replay_guard_marker(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::Item(Item::Const(item)) if item.ident == REPLAY_GUARD_IDENT
    )
}

fn stmt_is_generated_replay_guard(stmt: &Stmt) -> bool {
    let Stmt::Expr(Expr::If(expr_if), _) = stmt else {
        return false;
    };

    if_let_replays_and_returns(expr_if)
}

fn stmt_is_generated_auth_prologue(stmt: &Stmt) -> bool {
    // Marker consts the auth guards prepend ahead of their check. They carry no
    // behaviour, so the scan must step over them: a prologue statement that is
    // not recognized stops the walk, the replay guard behind it reads as absent,
    // and an enclosing guard emits a second one *ahead* of the auth check —
    // serving a cached response before authentication/authorization runs.
    if matches!(
        stmt,
        Stmt::Item(Item::Const(item))
            if item.ident == "__AUTUMN_SECURED_ROLES"
                || item.ident == "__AUTUMN_SECURED_SCOPES"
                || item.ident == "__AUTUMN_AUTHORIZE_BINDINGS"
    ) {
        return true;
    }

    let Stmt::Expr(Expr::If(expr_if), _) = stmt else {
        return false;
    };

    if_let_generated_check_returns_error(expr_if)
}

/// Skips the two statements the `#[throttle]` macro prepends ahead of the
/// idempotency-replay guard when it expands BEFORE the route macro (i.e.
/// `#[throttle]` written above `#[get]`/`#[post]`): the
/// `const __AUTUMN_THROTTLE_ROUTE_ID` bucket-namespace constant and the
/// `if let Err(__autumn_throttle_response) = …__check_throttle(…).await { return … }`
/// guard. Recognizing them lets the body scan reach the generated replay guard
/// that follows, so the route macro suppresses the outer
/// `IdempotencyReplayLayer` under this attribute ordering exactly as it already
/// does for `#[secured]`/`#[authorize]` (which expand the same way when written
/// above the route attribute).
fn stmt_is_generated_throttle_prologue(stmt: &Stmt) -> bool {
    if matches!(
        stmt,
        Stmt::Item(Item::Const(item)) if item.ident == "__AUTUMN_THROTTLE_ROUTE_ID"
    ) {
        return true;
    }

    let Stmt::Expr(Expr::If(expr_if), _) = stmt else {
        return false;
    };

    if_let_throttle_check_returns(expr_if)
}

/// Skips the version-sunset check `#[authorize]` emits between its policy
/// check and its replay stop:
/// `if let Some(Extension(meta)) = &__autumn_route_version { if let
/// Some(resp) = check_sunset(&state, meta) { return resp; } }`. It carries
/// real behaviour (an old route version can still reject a request), but
/// nothing downstream of it depends on that outcome the way replay-serving
/// does, so the scan must step over it to reach the replay guard that
/// follows: an unrecognized statement here stops the walk, the replay guard
/// reads as absent, and a gate macro stacked above `#[authorize]` would then
/// claim replay ownership for a `FromRequestParts` extractor that runs
/// BEFORE authorize's policy re-check ever does.
fn stmt_is_generated_sunset_check_prologue(stmt: &Stmt) -> bool {
    let Stmt::Expr(Expr::If(expr_if), _) = stmt else {
        return false;
    };

    if_let_sunset_check(expr_if)
}

fn if_let_sunset_check(expr_if: &ExprIf) -> bool {
    let Expr::Let(expr_let) = expr_if.cond.as_ref() else {
        return false;
    };

    expr_if.else_branch.is_none()
        && pat_is_some_route_version_extension(&expr_let.pat)
        && expr_is_ref_to_ident(&expr_let.expr, "__autumn_route_version")
        && block_is_sunset_check_body(&expr_if.then_branch)
}

fn pat_is_some_route_version_extension(pat: &Pat) -> bool {
    let Pat::TupleStruct(tuple) = pat else {
        return false;
    };

    path_matches(&tuple.path, &["core", "option", "Option", "Some"])
        && tuple.elems.len() == 1
        && pat_is_extension_binding(&tuple.elems[0], "__autumn_meta")
}

fn pat_is_extension_binding(pat: &Pat, expected: &str) -> bool {
    let Pat::TupleStruct(tuple) = pat else {
        return false;
    };

    path_ends_with(&tuple.path, "Extension")
        && tuple.elems.len() == 1
        && pat_binds_ident(&tuple.elems[0], expected)
}

fn block_is_sunset_check_body(block: &Block) -> bool {
    match block.stmts.as_slice() {
        [Stmt::Expr(Expr::If(inner_if), _)] => if_let_sunset_response_returns(inner_if),
        _ => false,
    }
}

fn if_let_sunset_response_returns(expr_if: &ExprIf) -> bool {
    let Expr::Let(expr_let) = expr_if.cond.as_ref() else {
        return false;
    };

    expr_if.else_branch.is_none()
        && pat_is_some_replay_response(&expr_let.pat)
        && expr_is_check_sunset_call(&expr_let.expr)
        && block_returns_ident(&expr_if.then_branch, "__autumn_response")
}

fn expr_is_check_sunset_call(expr: &Expr) -> bool {
    match expr {
        Expr::Call(call) => {
            path_expr_matches(&call.func, &["autumn_web", "__private", "check_sunset"])
        }
        Expr::Group(group) => expr_is_check_sunset_call(&group.expr),
        Expr::Paren(paren) => expr_is_check_sunset_call(&paren.expr),
        _ => false,
    }
}

fn if_let_throttle_check_returns(expr_if: &ExprIf) -> bool {
    let Expr::Let(expr_let) = expr_if.cond.as_ref() else {
        return false;
    };

    pat_is_err_throttle_response(&expr_let.pat)
        && expr_is_generated_throttle_check_call(&expr_let.expr)
        && block_returns_ident(&expr_if.then_branch, "__autumn_throttle_response")
}

fn pat_is_err_throttle_response(pat: &Pat) -> bool {
    match pat {
        Pat::TupleStruct(tuple) => {
            path_matches(&tuple.path, &["core", "result", "Result", "Err"])
                && tuple.elems.len() == 1
                && pat_binds_ident(&tuple.elems[0], "__autumn_throttle_response")
        }
        _ => false,
    }
}

fn expr_is_generated_throttle_check_call(expr: &Expr) -> bool {
    match expr {
        Expr::Await(await_expr) => expr_is_generated_throttle_check_call(&await_expr.base),
        Expr::Call(call) => {
            path_expr_matches(&call.func, &["autumn_web", "security", "__check_throttle"])
                && call
                    .args
                    .first()
                    .is_some_and(|arg| expr_is_ref_to_ident(arg, "__autumn_state"))
        }
        Expr::Group(group) => expr_is_generated_throttle_check_call(&group.expr),
        Expr::Paren(paren) => expr_is_generated_throttle_check_call(&paren.expr),
        _ => false,
    }
}

fn if_let_replays_and_returns(expr_if: &ExprIf) -> bool {
    let Expr::Let(expr_let) = expr_if.cond.as_ref() else {
        return false;
    };

    pat_is_some_replay_response(&expr_let.pat)
        && expr_is_replay_response_call(&expr_let.expr)
        && block_returns_ident(&expr_if.then_branch, "__autumn_response")
}

fn if_let_generated_check_returns_error(expr_if: &ExprIf) -> bool {
    let Expr::Let(expr_let) = expr_if.cond.as_ref() else {
        return false;
    };

    pat_is_err_autumn_error(&expr_let.pat)
        && expr_is_generated_auth_check_call(&expr_let.expr)
        && block_is_generated_auth_failure_response(&expr_if.then_branch)
}

fn pat_is_some_replay_response(pat: &Pat) -> bool {
    match pat {
        Pat::TupleStruct(tuple) => {
            path_matches(&tuple.path, &["core", "option", "Option", "Some"])
                && tuple.elems.len() == 1
                && pat_binds_ident(&tuple.elems[0], "__autumn_response")
        }
        _ => false,
    }
}

fn pat_is_err_autumn_error(pat: &Pat) -> bool {
    match pat {
        Pat::TupleStruct(tuple) => {
            path_matches(&tuple.path, &["core", "result", "Result", "Err"])
                && tuple.elems.len() == 1
                && pat_binds_ident(&tuple.elems[0], "__autumn_error")
        }
        _ => false,
    }
}

fn pat_binds_ident(pat: &Pat, expected: &str) -> bool {
    matches!(pat, Pat::Ident(ident) if ident.ident == expected)
}

fn expr_is_replay_response_call(expr: &Expr) -> bool {
    match expr {
        Expr::Call(call) => path_expr_matches(
            &call.func,
            &["autumn_web", "idempotency", "__replay_response"],
        ),
        Expr::Group(group) => expr_is_replay_response_call(&group.expr),
        Expr::Paren(paren) => expr_is_replay_response_call(&paren.expr),
        _ => false,
    }
}

fn expr_is_generated_auth_check_call(expr: &Expr) -> bool {
    match expr {
        Expr::Await(await_expr) => expr_is_generated_auth_check_call(&await_expr.base),
        Expr::Call(call)
            if path_expr_matches(
                &call.func,
                &["autumn_web", "auth", "__check_secured_with_key"],
            ) =>
        {
            call.args.len() == 3
                && call
                    .args
                    .first()
                    .is_some_and(|arg| expr_is_ref_to_ident(arg, "__autumn_session"))
                && call
                    .args
                    .iter()
                    .nth(1)
                    .is_some_and(expr_is_auth_session_key_call)
                && call
                    .args
                    .iter()
                    .nth(2)
                    .is_some_and(|arg| path_expr_ends_with(arg, "__AUTUMN_SECURED_ROLES"))
        }
        Expr::Call(call)
            if path_expr_matches(
                &call.func,
                &["autumn_web", "auth", "__check_secured_scopes"],
            ) =>
        {
            // __check_secured_scopes(__autumn_token_scopes…, __AUTUMN_SECURED_SCOPES)
            call.args.len() == 2
                && call
                    .args
                    .iter()
                    .nth(1)
                    .is_some_and(|arg| path_expr_ends_with(arg, "__AUTUMN_SECURED_SCOPES"))
        }
        Expr::Call(call)
            if path_expr_matches(
                &call.func,
                &["autumn_web", "authorization", "__check_policy"],
            ) =>
        {
            call.args.len() == 4
                && call
                    .args
                    .first()
                    .is_some_and(|arg| expr_is_ref_to_ident(arg, "__autumn_state"))
                && call
                    .args
                    .iter()
                    .nth(1)
                    .is_some_and(|arg| expr_is_ref_to_ident(arg, "__autumn_session"))
                && call.args.iter().nth(2).is_some_and(expr_is_string_literal)
                && call.args.iter().nth(3).is_some_and(expr_is_ref_to_path)
        }
        Expr::Call(call)
            if path_expr_matches(
                &call.func,
                &["autumn_web", "authorization", "__check_policy_scoped"],
            ) =>
        {
            // __check_policy_scoped(&state, &session, scopes_map, "action", &resource)
            call.args.len() == 5
                && call
                    .args
                    .first()
                    .is_some_and(|arg| expr_is_ref_to_ident(arg, "__autumn_state"))
                && call
                    .args
                    .iter()
                    .nth(1)
                    .is_some_and(|arg| expr_is_ref_to_ident(arg, "__autumn_session"))
                && call.args.iter().nth(2).is_some_and(expr_is_scopes_map_arg)
                && call.args.iter().nth(3).is_some_and(expr_is_string_literal)
                && call.args.iter().nth(4).is_some_and(expr_is_ref_to_path)
        }
        Expr::Group(group) => expr_is_generated_auth_check_call(&group.expr),
        Expr::Paren(paren) => expr_is_generated_auth_check_call(&paren.expr),
        _ => false,
    }
}

/// Recognizes `__autumn_token_scopes.as_ref().map(|__e| &__e.0)` — the
/// generated scopes argument emitted by `#[authorize]` for `__check_policy_scoped`.
fn expr_is_scopes_map_arg(expr: &Expr) -> bool {
    let Expr::MethodCall(outer) = expr else {
        return false;
    };
    if outer.method != "map" {
        return false;
    }
    let Expr::MethodCall(inner) = outer.receiver.as_ref() else {
        return false;
    };
    inner.method == "as_ref" && path_expr_ends_with(&inner.receiver, "__autumn_token_scopes")
}

fn block_returns_ident(block: &Block, expected: &str) -> bool {
    block.stmts.iter().any(|stmt| match stmt {
        Stmt::Expr(Expr::Return(ret), _) => ret
            .expr
            .as_ref()
            .is_some_and(|expr| path_expr_ends_with(expr, expected)),
        _ => false,
    })
}

fn block_is_generated_auth_failure_response(block: &Block) -> bool {
    match block.stmts.as_slice() {
        [Stmt::Expr(Expr::Return(ret), _)] => ret
            .expr
            .as_ref()
            .is_some_and(|expr| expr_is_autumn_error_response(expr)),
        [
            Stmt::Expr(Expr::If(replay_if), _),
            Stmt::Expr(Expr::Return(ret), _),
        ] => {
            if_let_replays_finalized_session_response(replay_if)
                && ret
                    .expr
                    .as_ref()
                    .is_some_and(|expr| expr_is_autumn_error_response(expr))
        }
        _ => false,
    }
}

fn expr_is_autumn_error_response(expr: &Expr) -> bool {
    match expr {
        Expr::Call(call) => {
            path_expr_matches(
                &call.func,
                &[
                    "autumn_web",
                    "reexports",
                    "axum",
                    "response",
                    "IntoResponse",
                    "into_response",
                ],
            ) && call.args.len() == 1
                && call
                    .args
                    .first()
                    .is_some_and(|arg| path_expr_ends_with(arg, "__autumn_error"))
        }
        Expr::Group(group) => expr_is_autumn_error_response(&group.expr),
        Expr::Paren(paren) => expr_is_autumn_error_response(&paren.expr),
        _ => false,
    }
}

fn if_let_replays_finalized_session_response(expr_if: &ExprIf) -> bool {
    let Expr::Let(expr_let) = expr_if.cond.as_ref() else {
        return false;
    };

    pat_is_some_replay_response(&expr_let.pat)
        && expr_is_finalized_session_replay_call(&expr_let.expr)
        && block_returns_ident(&expr_if.then_branch, "__autumn_response")
}

fn expr_is_finalized_session_replay_call(expr: &Expr) -> bool {
    match expr {
        Expr::Await(await_expr) => expr_is_finalized_session_replay_call(&await_expr.base),
        Expr::Call(call)
            if path_expr_matches(
                &call.func,
                &[
                    "autumn_web",
                    "idempotency",
                    "__replay_finalized_session_response",
                ],
            ) =>
        {
            call.args.len() == 1
                && call
                    .args
                    .first()
                    .is_some_and(|arg| expr_is_ref_to_ident(arg, "__autumn_idempotency_replay"))
        }
        Expr::Call(call)
            if path_expr_matches(
                &call.func,
                &[
                    "autumn_web",
                    "idempotency",
                    "__replay_finalized_session_response_for_anonymous",
                ],
            ) =>
        {
            call.args.len() == 3
                && call
                    .args
                    .first()
                    .is_some_and(|arg| expr_is_ref_to_ident(arg, "__autumn_session"))
                && call
                    .args
                    .iter()
                    .nth(1)
                    .is_some_and(expr_is_auth_session_key_call)
                && call
                    .args
                    .iter()
                    .nth(2)
                    .is_some_and(|arg| expr_is_ref_to_ident(arg, "__autumn_idempotency_replay"))
        }
        Expr::Group(group) => expr_is_finalized_session_replay_call(&group.expr),
        Expr::Paren(paren) => expr_is_finalized_session_replay_call(&paren.expr),
        _ => false,
    }
}

fn expr_is_ref_to_ident(expr: &Expr, expected: &str) -> bool {
    let Expr::Reference(reference) = expr else {
        return false;
    };

    path_expr_ends_with(&reference.expr, expected)
}

fn expr_is_ref_to_path(expr: &Expr) -> bool {
    matches!(expr, Expr::Reference(reference) if matches!(reference.expr.as_ref(), Expr::Path(_)))
}

fn expr_is_auth_session_key_call(expr: &Expr) -> bool {
    let Expr::MethodCall(call) = expr else {
        return false;
    };

    call.method == "auth_session_key"
        && call.args.is_empty()
        && path_expr_ends_with(&call.receiver, "__autumn_state")
}

const fn expr_is_string_literal(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(_),
            ..
        })
    )
}

fn generated_nested_response_body(block: &Block, index: usize) -> Option<&Block> {
    let stmt = block.stmts.get(index)?;
    match stmt {
        Stmt::Local(local) if pat_binds_inner_response(&local.pat) => local
            .init
            .as_ref()
            .and_then(|init| expr_nested_async_body(&init.expr))
            .filter(|_| {
                index + 2 == block.stmts.len()
                    && block
                        .stmts
                        .get(index + 1)
                        .is_some_and(stmt_is_inner_response_tail)
            }),
        Stmt::Expr(expr, None) if index + 1 == block.stmts.len() => expr_nested_async_body(expr),
        _ => None,
    }
}

fn pat_binds_inner_response(pat: &Pat) -> bool {
    match pat {
        Pat::Ident(ident) => ident.ident == "__autumn_inner",
        Pat::Type(typed) => pat_binds_inner_response(&typed.pat),
        _ => false,
    }
}

/// Unwrap the `(async move { … }).await` wrapper the body guards (`#[secured]`,
/// `#[authorize]`, `#[throttle]`, …) put a handler's original body inside —
/// including the `IntoResponse::into_response(…)` call, parens, and invisible
/// groups they may sit behind — and yield the inner block.
///
/// Also unwraps the unrelated `(|| async move { … })().await` closure-IIFE
/// wrapper `#[cached]` puts a handler's original body inside
/// (`cached_macro`'s `compute`): a marker injected by an earlier-expanding
/// attribute (e.g. `#[static_get]`'s `STATIC_ROUTE_HANDLER_MARKER`) ends up
/// buried one level deeper than usual when `#[cached]` sits between it and
/// the guard that needs to see it, same as the guard-generated wrapper above
/// (Codex review on #2513, eleventh finding).
///
/// Shared with `api_doc`'s marker walks: each guard or wrapper that expands
/// *before* another buries the earlier one's marker consts one wrapper level
/// deeper, so a walk that does not descend through this shape silently loses
/// them.
pub fn expr_nested_async_body(expr: &Expr) -> Option<&Block> {
    match expr {
        Expr::Async(expr_async) => Some(&expr_async.block),
        Expr::Await(await_expr) => expr_nested_async_body(&await_expr.base),
        Expr::Call(call)
            if path_expr_matches(
                &call.func,
                &[
                    "autumn_web",
                    "reexports",
                    "axum",
                    "response",
                    "IntoResponse",
                    "into_response",
                ],
            ) && call.args.len() == 1 =>
        {
            call.args.first().and_then(expr_nested_async_body)
        }
        // `(|| async move { … })()` / `(|| async move { … })().await` — a
        // zero-argument call of an inline closure, the `#[cached]` IIFE
        // shape. The closure itself is never `async`; its body is the async
        // block being wrapped.
        Expr::Call(call) if call.args.is_empty() => match unwrap_group_paren(&call.func) {
            Expr::Closure(closure) => expr_nested_async_body(&closure.body),
            _ => None,
        },
        Expr::Group(group) => expr_nested_async_body(&group.expr),
        Expr::Paren(paren) => expr_nested_async_body(&paren.expr),
        _ => None,
    }
}

/// Strip any invisible `Group`/`Paren` wrapper down to the expression they
/// enclose, without following any other wrapper shape (unlike
/// [`expr_nested_async_body`], which only unwraps a specific known set of
/// *async* wrappers and returns the inner `Block` rather than the `Expr`
/// itself).
fn unwrap_group_paren(expr: &Expr) -> &Expr {
    match expr {
        Expr::Group(group) => unwrap_group_paren(&group.expr),
        Expr::Paren(paren) => unwrap_group_paren(&paren.expr),
        other => other,
    }
}

/// Marker consts a body guard (`#[secured]`, `#[step_up]`, `#[authorize]`,
/// `#[throttle]`) unconditionally emits into the same block as its
/// `__autumn_inner` binding whenever it rewrites a handler's return type —
/// see `secured_macro`'s `check_call`, `step_up_macro`'s `build_check_call`,
/// `authorize_macro`'s block prologue, and `throttle_macro`'s `check_call`.
/// One of these always precedes the binding in real guard output, so its
/// presence is what [`generated_inner_response_binding`] uses to tell a real
/// guard's generated wrapper apart from a block that merely has the same
/// `let __autumn_inner: T = (async move { … }).await; IntoResponse::into_response(__autumn_inner)`
/// shape by coincidence.
const RESPONSE_REWRITING_GUARD_MARKERS: &[&str] = &[
    "__AUTUMN_SECURED_ROLES",
    "__AUTUMN_STEP_UP_MAX_AGE",
    "__AUTUMN_THROTTLE_ROUTE_ID",
    "__AUTUMN_AUTHORIZE_BINDINGS",
];

/// Whether any statement in `stmts` is a marker const from
/// [`RESPONSE_REWRITING_GUARD_MARKERS`].
fn stmts_have_response_rewriting_guard_marker(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|stmt| {
        matches!(
            stmt,
            Stmt::Item(Item::Const(item))
                if RESPONSE_REWRITING_GUARD_MARKERS.contains(&item.ident.to_string().as_str())
        )
    })
}

/// The exact shape a body guard (`#[secured]`, `#[step_up]`, `#[authorize]`,
/// `#[throttle]`) emits when it rewrites a non-unit, non-`impl Trait` return
/// type: a `let __autumn_inner: T = <init>;` binding sitting exactly one
/// statement before the end of `block`, immediately followed by the
/// generated `IntoResponse::into_response(__autumn_inner)` tail, with one of
/// [`RESPONSE_REWRITING_GUARD_MARKERS`] present earlier in the same block, or
/// `markerless_gate_budget` above zero (issue #1677's
/// `api_doc::infer_response_body` recovery relies on this).
///
/// `markerless_gate_budget` covers `#[step_up]`/`#[throttle]`: their marker
/// consts moved out of the body and into their gate's own impl block
/// (#1668), so they can never appear in `block` for this scan to find. The
/// caller counts it once, from the enclosing function's signature — one unit
/// per such gate parameter present — and this function returns the balance
/// left after spending one unit to accept the *current* level, so a caller
/// recursing into a deeper, nested block cannot spend the same real guard's
/// unit a second time on an unrelated coincidental match there (a marker-const
/// acceptance spends nothing, since a real marker is level-local proof on its
/// own).
///
/// Deliberately structural rather than a bare name-and-type scan. Matching
/// the required *position* (second-to-last, with that exact tail following)
/// alone rules out a handler that happens to declare its own unrelated local
/// named `__autumn_inner` elsewhere in the body. But the guard's own
/// generated wrapper contains the *user's original body* verbatim one level
/// deeper (`(async move { #original_body }).await`) — so if that original
/// body itself independently ends in the same two-statement shape, position
/// and tail alone cannot tell a real guard's own binding apart from the
/// user's body coincidentally mimicking it. Requiring a marker const (or an
/// unspent budget unit) to also be present closes that gap: no guard ever
/// emits the `__autumn_inner` binding without one, a handler's own code has
/// no reason to declare an identifier the framework treats as reserved, and
/// the budget cap means a coincidence *beyond* the real guards' own levels
/// still has nothing left to spend.
///
/// Returns the local's declared type, its initializer expression, and the
/// remaining budget, so a caller can read the type, recurse into a deeper
/// nested guard via [`expr_nested_async_body`], and thread the correct
/// balance through that recursion.
pub fn generated_inner_response_binding(
    block: &Block,
    markerless_gate_budget: usize,
) -> Option<(&Type, &Expr, usize)> {
    let len = block.stmts.len();
    if len < 2 {
        return None;
    }
    let index = len - 2;
    let Stmt::Local(local) = &block.stmts[index] else {
        return None;
    };
    if !pat_binds_inner_response(&local.pat) {
        return None;
    }
    let Pat::Type(pat_type) = &local.pat else {
        return None;
    };
    if !stmt_is_inner_response_tail(&block.stmts[index + 1]) {
        return None;
    }
    let remaining_budget = if stmts_have_response_rewriting_guard_marker(&block.stmts[..index]) {
        markerless_gate_budget
    } else if markerless_gate_budget > 0 {
        markerless_gate_budget - 1
    } else {
        return None;
    };
    let init_expr = &local.init.as_ref()?.expr;
    Some((&pat_type.ty, init_expr, remaining_budget))
}

fn stmt_is_inner_response_tail(stmt: &Stmt) -> bool {
    let Stmt::Expr(Expr::Call(call), None) = stmt else {
        return false;
    };

    path_expr_matches(
        &call.func,
        &[
            "autumn_web",
            "reexports",
            "axum",
            "response",
            "IntoResponse",
            "into_response",
        ],
    ) && call.args.len() == 1
        && call
            .args
            .first()
            .is_some_and(|arg| path_expr_ends_with(arg, "__autumn_inner"))
}

fn path_expr_ends_with(expr: &Expr, expected: &str) -> bool {
    let Expr::Path(path) = expr else {
        return false;
    };

    path_ends_with(&path.path, expected)
}

fn path_expr_matches(expr: &Expr, expected: &[&str]) -> bool {
    let Expr::Path(path) = expr else {
        return false;
    };

    path_matches(&path.path, expected)
}

fn path_matches(path: &syn::Path, expected: &[&str]) -> bool {
    if path.segments.len() != expected.len() {
        return false;
    }
    // Only an `expected` path actually rooted at the framework crate (some
    // call sites match a `core::...`/`std::...` path instead, e.g. the
    // `Option`/`Result` patterns above) needs its leading segment compared
    // against the actively resolved name rather than the literal
    // `"autumn_web"` — once a rename or `crate = "..."` override (#1828) is
    // in effect, that's what an earlier-expanded stacked macro's own
    // (already-finalized) output is rooted at instead (Codex review, #2552).
    let root_matches = if expected.first() == Some(&"autumn_web") {
        path.segments.first().is_some_and(|segment| {
            segment.ident == crate::crate_path::current_target_path_segment()
        })
    } else {
        path.segments
            .first()
            .zip(expected.first())
            .is_some_and(|(segment, expected)| segment.ident == *expected)
    };
    root_matches
        && path
            .segments
            .iter()
            .skip(1)
            .zip(&expected[1..])
            .all(|(segment, expected)| segment.ident == *expected)
}

fn path_ends_with(path: &syn::Path, expected: &str) -> bool {
    path.segments
        .last()
        .is_some_and(|segment| segment.ident == expected)
}

#[cfg(test)]
mod tests {
    use super::{block_has_replay_guard, should_own_replay};

    #[test]
    fn string_literal_does_not_count_as_replay_guard() {
        let block: syn::Block = syn::parse_quote!({
            let _ = "__AUTUMN_IDEMPOTENCY_REPLAY_GUARD";
        });

        assert!(!block_has_replay_guard(&block));
    }

    #[test]
    fn marker_const_without_replay_call_does_not_count_as_replay_guard() {
        let block: syn::Block = syn::parse_quote!({
            const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
            let _ = "plain user const";
        });

        assert!(!block_has_replay_guard(&block));
    }

    #[test]
    fn marker_const_and_non_returned_replay_call_do_not_count_as_replay_guard() {
        let block: syn::Block = syn::parse_quote!({
            const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
            let _ignored =
                ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay);
        });

        assert!(!block_has_replay_guard(&block));
    }

    #[test]
    fn generated_shape_after_user_statement_does_not_count_as_replay_guard() {
        let block: syn::Block = syn::parse_quote!({
            mutate_before_replay_stop();
            const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
            if let ::core::option::Option::Some(__autumn_response) =
                ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
            {
                return __autumn_response;
            }
        });

        assert!(!block_has_replay_guard(&block));
    }

    #[test]
    fn nested_generated_shape_with_semicolon_does_not_count_as_replay_guard() {
        let block: syn::Block = syn::parse_quote!({
            ::autumn_web::reexports::axum::response::IntoResponse::into_response(
                (async move {
                    const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
                    if let ::core::option::Option::Some(__autumn_response) =
                        ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
                    {
                        return __autumn_response;
                    }
                })
                .await,
            );
            mutate_after_dropped_replay_response();
        });

        assert!(!block_has_replay_guard(&block));
    }

    #[test]
    fn nested_generated_shape_without_tail_response_does_not_count_as_replay_guard() {
        let block: syn::Block = syn::parse_quote!({
            let __autumn_inner: ::autumn_web::reexports::axum::response::Response = (async move {
                const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
                if let ::core::option::Option::Some(__autumn_response) =
                    ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
                {
                    return __autumn_response;
                }
            })
            .await;
            mutate_after_nested_replay_response();
        });

        assert!(!block_has_replay_guard(&block));
    }

    #[test]
    fn nested_guard_in_non_autumn_into_response_does_not_count() {
        let block: syn::Block = syn::parse_quote!({
            evil::IntoResponse::into_response(
                (async move {
                    const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
                    if let ::core::option::Option::Some(__autumn_response) =
                        ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
                    {
                        return __autumn_response;
                    }
                })
                .await,
            )
        });

        assert!(!block_has_replay_guard(&block));
    }

    #[test]
    fn nested_guard_in_extra_into_response_argument_does_not_count() {
        let block: syn::Block = syn::parse_quote!({
            ::autumn_web::reexports::axum::response::IntoResponse::into_response(
                side_effect_before_replay_stop(),
                (async move {
                    const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
                    if let ::core::option::Option::Some(__autumn_response) =
                        ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
                    {
                        return __autumn_response;
                    }
                })
                .await,
            )
        });

        assert!(!block_has_replay_guard(&block));
    }

    #[test]
    fn secured_prologue_with_side_effect_argument_does_not_count() {
        let block: syn::Block = syn::parse_quote!({
            const __AUTUMN_SECURED_ROLES: &[&str] = &["admin"];
            if let ::core::result::Result::Err(__autumn_error) =
                ::autumn_web::auth::__check_secured_with_key(
                    side_effect_before_replay_stop(),
                    __autumn_state.auth_session_key(),
                    __AUTUMN_SECURED_ROLES,
                )
                .await
            {
                return ::autumn_web::reexports::axum::response::IntoResponse::into_response(
                    __autumn_error,
                );
            }
            const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
            if let ::core::option::Option::Some(__autumn_response) =
                ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
            {
                return __autumn_response;
            }
        });

        assert!(!block_has_replay_guard(&block));
    }

    #[test]
    fn authorize_prologue_with_side_effect_argument_does_not_count() {
        let block: syn::Block = syn::parse_quote!({
            if let ::core::result::Result::Err(__autumn_error) =
                ::autumn_web::authorization::__check_policy::<Post>(
                    &__autumn_state,
                    &__autumn_session,
                    side_effect_before_replay_stop(),
                    &post,
                )
                .await
            {
                return ::autumn_web::reexports::axum::response::IntoResponse::into_response(
                    __autumn_error,
                );
            }
            const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
            if let ::core::option::Option::Some(__autumn_response) =
                ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
            {
                return __autumn_response;
            }
        });

        assert!(!block_has_replay_guard(&block));
    }

    #[test]
    fn generated_const_and_replay_call_count_as_replay_guard() {
        let block: syn::Block = syn::parse_quote!({
            const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
            if let ::core::option::Option::Some(__autumn_response) =
                ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
            {
                return __autumn_response;
            }
        });

        assert!(block_has_replay_guard(&block));
    }

    #[test]
    fn generated_throttle_prologue_before_replay_guard_counts() {
        // `#[throttle]` written above `#[post]` expands first: it prepends the
        // route-id const and the throttle check, then the replay guard. The scan
        // must skip the throttle prologue and still recognize the replay guard so
        // the route macro suppresses the outer IdempotencyReplayLayer.
        let block: syn::Block = syn::parse_quote!({
            const __AUTUMN_THROTTLE_ROUTE_ID: &str =
                ::core::concat!(::core::module_path!(), "::", "handler");
            if let ::core::result::Result::Err(__autumn_throttle_response) =
                ::autumn_web::security::__check_throttle(
                    &__autumn_state,
                    __AUTUMN_THROTTLE_ROUTE_ID,
                    ::autumn_web::security::ThrottleSpec::Inline {
                        limit: 1,
                        per_secs: 60,
                        key: ::core::option::Option::None,
                    },
                    &__autumn_throttle_headers,
                    __autumn_throttle_peer.as_ref().map(|ext| ext.0.0),
                    __autumn_throttle_principal.as_ref().map(|e| &e.0),
                    __autumn_throttle_session.as_ref().map(|e| &e.0),
                    __autumn_throttle_exempt.is_some(),
                )
                .await
            {
                return __autumn_throttle_response;
            }
            const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
            if let ::core::option::Option::Some(__autumn_response) =
                ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
            {
                return __autumn_response;
            }
        });

        assert!(block_has_replay_guard(&block));
    }

    #[test]
    fn throttle_prologue_with_side_effect_argument_does_not_count() {
        // A throttle-shaped prologue whose check call takes a side-effecting first
        // argument (not `&__autumn_state`) must NOT be skipped, so a hand-written
        // look-alike cannot trick the route macro into dropping the replay layer.
        let block: syn::Block = syn::parse_quote!({
            const __AUTUMN_THROTTLE_ROUTE_ID: &str = "handler";
            if let ::core::result::Result::Err(__autumn_throttle_response) =
                ::autumn_web::security::__check_throttle(
                    side_effect_before_replay_stop(),
                    __AUTUMN_THROTTLE_ROUTE_ID,
                )
                .await
            {
                return __autumn_throttle_response;
            }
            const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
            if let ::core::option::Option::Some(__autumn_response) =
                ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
            {
                return __autumn_response;
            }
        });

        assert!(!block_has_replay_guard(&block));
    }

    #[test]
    fn generated_secured_prologue_before_replay_guard_counts() {
        let block: syn::Block = syn::parse_quote!({
            const __AUTUMN_SECURED_ROLES: &[&str] = &["admin"];
            if let ::core::result::Result::Err(__autumn_error) =
                ::autumn_web::auth::__check_secured_with_key(
                    &__autumn_session,
                    __autumn_state.auth_session_key(),
                    __AUTUMN_SECURED_ROLES,
                )
                .await
            {
                return ::autumn_web::reexports::axum::response::IntoResponse::into_response(
                    __autumn_error,
                );
            }
            const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
            if let ::core::option::Option::Some(__autumn_response) =
                ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
            {
                return __autumn_response;
            }
        });

        assert!(block_has_replay_guard(&block));
    }

    #[test]
    fn generated_authorize_prologue_with_anonymous_session_replay_counts() {
        let block: syn::Block = syn::parse_quote!({
            if let ::core::result::Result::Err(__autumn_error) =
                ::autumn_web::authorization::__check_policy::<Post>(
                    &__autumn_state,
                    &__autumn_session,
                    "update",
                    &post,
                )
                .await
            {
                if let ::core::option::Option::Some(__autumn_response) =
                    ::autumn_web::idempotency::__replay_finalized_session_response_for_anonymous(
                        &__autumn_session,
                        __autumn_state.auth_session_key(),
                        &__autumn_idempotency_replay,
                    )
                    .await
                {
                    return __autumn_response;
                }
                return ::autumn_web::reexports::axum::response::IntoResponse::into_response(
                    __autumn_error,
                );
            }
            const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
            if let ::core::option::Option::Some(__autumn_response) =
                ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
            {
                return __autumn_response;
            }
        });

        assert!(block_has_replay_guard(&block));
    }

    #[test]
    fn generated_authorize_scoped_prologue_before_replay_guard_counts() {
        let block: syn::Block = syn::parse_quote!({
            if let ::core::result::Result::Err(__autumn_error) =
                ::autumn_web::authorization::__check_policy_scoped::<Post>(
                    &__autumn_state,
                    &__autumn_session,
                    __autumn_token_scopes.as_ref().map(|__e| &__e.0),
                    "update",
                    &post,
                )
                .await
            {
                return ::autumn_web::reexports::axum::response::IntoResponse::into_response(
                    __autumn_error,
                );
            }
            const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
            if let ::core::option::Option::Some(__autumn_response) =
                ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
            {
                return __autumn_response;
            }
        });

        assert!(block_has_replay_guard(&block));
    }

    #[test]
    fn generated_authorize_wrapper_can_find_nested_secured_replay_guard() {
        let block: syn::Block = syn::parse_quote!({
            if let ::core::result::Result::Err(__autumn_error) =
                ::autumn_web::authorization::__check_policy::<Post>(
                    &__autumn_state,
                    &__autumn_session,
                    "update",
                    &post,
                )
                .await
            {
                return ::autumn_web::reexports::axum::response::IntoResponse::into_response(
                    __autumn_error,
                );
            }
            let __autumn_inner: ::autumn_web::reexports::axum::response::Response = (async move {
                const __AUTUMN_SECURED_ROLES: &[&str] = &["admin"];
                if let ::core::result::Result::Err(__autumn_error) =
                    ::autumn_web::auth::__check_secured_with_key(
                        &__autumn_session,
                        __autumn_state.auth_session_key(),
                        __AUTUMN_SECURED_ROLES,
                    )
                    .await
                {
                    return ::autumn_web::reexports::axum::response::IntoResponse::into_response(
                        __autumn_error,
                    );
                }
                const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
                if let ::core::option::Option::Some(__autumn_response) =
                    ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
                {
                    return __autumn_response;
                }
            })
            .await;
            ::autumn_web::reexports::axum::response::IntoResponse::into_response(__autumn_inner)
        });

        assert!(block_has_replay_guard(&block));
    }

    #[test]
    fn generated_authorize_bindings_marker_does_not_hide_replay_guard() {
        // `#[authorize]` prepends a binding marker const ahead of its policy
        // check (#1627). Like the `#[secured]` role/scope markers, it must be
        // transparent to this scan: a prologue statement that is not skipped
        // stops the walk, so the replay guard behind it would look absent and
        // the enclosing guard would emit a second one ahead of the policy
        // check — serving a cached response before authorization runs.
        //
        // Scope of this test: it pins the SKIP-LIST behavior on the
        // marker-plus-policy-check prologue prefix, via a reduced hand-written
        // block that stops short of the sunset-check statement `#[authorize]`
        // emits next. `should_own_replay_defers_to_authorize_across_the_sunset_check`
        // below exercises the real full expansion, sunset check included.
        let generated = crate::authorize::authorize_macro(
            quote::quote! { "update", resource = Post },
            quote::quote! {
                async fn update_post(post: Post) -> &'static str { "ok" }
            },
        );
        let generated_fn: syn::ItemFn =
            syn::parse2(generated).expect("#[authorize] must emit a parseable function");
        assert!(
            matches!(
                generated_fn.block.stmts.first(),
                Some(syn::Stmt::Item(syn::Item::Const(item)))
                    if item.ident == "__AUTUMN_AUTHORIZE_BINDINGS"
            ),
            "the reduced block's FIRST statement must keep matching what #[authorize] emits \
             (the rest of the real expansion is deliberately not walked here — see above)"
        );

        let block: syn::Block = syn::parse_quote!({
            const __AUTUMN_AUTHORIZE_BINDINGS: &[(&str, &str)] = &[("update", "Post")];
            if let ::core::result::Result::Err(__autumn_error) =
                ::autumn_web::authorization::__check_policy_scoped::<Post>(
                    &__autumn_state,
                    &__autumn_session,
                    __autumn_token_scopes.as_ref().map(|__e| &__e.0),
                    "update",
                    &post,
                )
                .await
            {
                return ::autumn_web::reexports::axum::response::IntoResponse::into_response(
                    __autumn_error,
                );
            }
            const __AUTUMN_IDEMPOTENCY_REPLAY_GUARD: () = ();
            if let ::core::option::Option::Some(__autumn_response) =
                ::autumn_web::idempotency::__replay_response(&__autumn_idempotency_replay)
            {
                return __autumn_response;
            }
        });

        assert!(block_has_replay_guard(&block));
    }

    #[test]
    fn should_own_replay_defers_to_authorize_across_the_sunset_check() {
        // Real stacking order `#[authorize("update", resource = Post)]` above
        // `#[secured]`/`#[step_up]`/`#[throttle]`: authorize expands FIRST
        // (it's topmost) and already owns replay-serving via its own in-body
        // check. Between that check and its replay stop it also emits a
        // sunset-version check — a statement the scan must see past, or it
        // stops early, reports no replay guard, and lets the about-to-attach
        // gate claim replay ownership for itself.
        //
        // That would no longer be the harmless redundancy it was pre-#1668:
        // the gate is a `FromRequestParts` extractor that runs BEFORE the
        // handler body — i.e. before `#[authorize]`'s policy re-check, which
        // lives entirely inside the body. A gate that wrongly owns replay
        // would serve a cached success response for a retried mutation
        // without ever re-running that policy check, even after the
        // requester's authorization has since been revoked.
        let generated = crate::authorize::authorize_macro(
            quote::quote! { "update", resource = Post },
            quote::quote! {
                async fn update_post(post: Post) -> &'static str { "ok" }
            },
        );
        let generated_fn: syn::ItemFn =
            syn::parse2(generated).expect("#[authorize] must emit a parseable function");

        assert!(
            block_has_replay_guard(&generated_fn.block),
            "the real #[authorize] expansion already owns replay-serving; the scan must \
             see past the sunset-check statement to find it"
        );
        assert!(
            !should_own_replay(&generated_fn),
            "a gate macro stacking on top of real #[authorize] output must defer replay \
             ownership to authorize, not claim it for a pre-body gate"
        );
    }
}
