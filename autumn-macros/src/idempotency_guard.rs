use syn::{Block, Expr, ExprIf, Item, Pat, Stmt};

const REPLAY_GUARD_IDENT: &str = "__AUTUMN_IDEMPOTENCY_REPLAY_GUARD";

pub fn block_has_replay_guard(block: &Block) -> bool {
    block_has_generated_replay_guard(block)
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

        if stmt_is_generated_auth_prologue(stmt) {
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
    if matches!(
        stmt,
        Stmt::Item(Item::Const(item))
            if item.ident == "__AUTUMN_SECURED_ROLES" || item.ident == "__AUTUMN_SECURED_SCOPES"
    ) {
        return true;
    }

    let Stmt::Expr(Expr::If(expr_if), _) = stmt else {
        return false;
    };

    if_let_generated_check_returns_error(expr_if)
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

fn expr_nested_async_body(expr: &Expr) -> Option<&Block> {
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
        Expr::Group(group) => expr_nested_async_body(&group.expr),
        Expr::Paren(paren) => expr_nested_async_body(&paren.expr),
        _ => None,
    }
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
    path.segments.len() == expected.len()
        && path
            .segments
            .iter()
            .zip(expected)
            .all(|(segment, expected)| segment.ident == expected)
}

fn path_ends_with(path: &syn::Path, expected: &str) -> bool {
    path.segments
        .last()
        .is_some_and(|segment| segment.ident == expected)
}

#[cfg(test)]
mod tests {
    use super::block_has_replay_guard;

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
}
