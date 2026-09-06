//! Shared helpers for attribute macros that inject hidden extractor
//! parameters (`#[secured]`, `#[authorize]`).
//!
//! Both macros want to add `__autumn_session: Session` and
//! `__autumn_state: State<AppState>` to a handler's argument list.
//! Stacking them is the documented common
//! pattern (`#[secured]` answers "are you in?", `#[authorize]`
//! answers "are you allowed?"), but each macro must avoid double-
//! injecting a parameter that the other already added — duplicate
//! parameter names are a compile error.
//!
//! Both attribute orderings need to work:
//!
//! - `#[secured]` outer / `#[authorize]` inner: `#[secured]` runs
//!   first, injects `__autumn_session` and `__autumn_state`.
//!   `#[authorize]` then runs on the modified function, sees the
//!   existing parameters, and skips re-injection.
//! - `#[authorize]` outer / `#[secured]` inner: `#[authorize]`
//!   runs first, injects `__autumn_session` and `__autumn_state`.
//!   `#[secured]` then runs on the modified function, sees the
//!   existing parameters, and skips re-injection.

use syn::ItemFn;

/// Return `true` when `func` already has a parameter bound to a
/// pattern with the given identifier name.
pub fn has_input_named(func: &ItemFn, name: &str) -> bool {
    func.sig.inputs.iter().any(|arg| match arg {
        syn::FnArg::Typed(pt) => pat_binds_name(&pt.pat, name),
        syn::FnArg::Receiver(_) => false,
    })
}

fn pat_binds_name(pat: &syn::Pat, name: &str) -> bool {
    match pat {
        syn::Pat::Ident(i) => i.ident == name,
        // `State(__autumn_state)`: walk the inner pattern.
        syn::Pat::TupleStruct(ts) => ts.elems.iter().any(|p| pat_binds_name(p, name)),
        syn::Pat::Tuple(t) => t.elems.iter().any(|p| pat_binds_name(p, name)),
        syn::Pat::Struct(s) => s.fields.iter().any(|fp| pat_binds_name(&fp.pat, name)),
        syn::Pat::Reference(r) => pat_binds_name(&r.pat, name),
        _ => false,
    }
}

/// Type-name prefixes of the pre-body `FromRequestParts` gate parameter each
/// body-guard macro inserts (issue #1668): `#[secured]`, `#[step_up]`, and
/// `#[throttle]` each mint a handler-unique gate type named
/// `__Autumn{Kind}Gate_{fn_name}` and insert it as a new leading parameter, so
/// its check runs — and can reject — before Axum's body extractor ever runs.
const GUARD_GATE_TYPE_PREFIXES: &[&str] = &[
    "__AutumnSecuredGate_",
    "__AutumnStepUpGate_",
    "__AutumnThrottleGate_",
];

/// Whether `func` already carries another guard's pre-body gate parameter.
///
/// Used by each of `#[secured]`/`#[step_up]`/`#[throttle]` to decide whether
/// ITS OWN gate should own idempotency-replay serving: whichever gate is
/// applied to a still-unguarded function (no earlier gate parameter, and per
/// [`crate::idempotency_guard::block_has_replay_guard`] no earlier in-body
/// guard either) is the one whose check every other stacked guard's check is
/// guaranteed to have already passed by the time it runs, so it — and only
/// it — may serve a cached replay.
pub fn has_any_guard_gate_param(func: &ItemFn) -> bool {
    GUARD_GATE_TYPE_PREFIXES
        .iter()
        .any(|prefix| has_guard_gate_param_with_prefix(func, prefix))
}

/// Whether `func` has a parameter whose type name starts with `prefix` — one
/// of the [`GUARD_GATE_TYPE_PREFIXES`]. Exposed separately from
/// [`has_any_guard_gate_param`] so a caller that only cares about ONE guard
/// kind (e.g. the route macro distinguishing `#[step_up]` from `#[throttle]`
/// for its own diagnostics) doesn't have to re-derive the naming convention.
pub fn has_guard_gate_param_with_prefix(func: &ItemFn, prefix: &str) -> bool {
    func.sig.inputs.iter().any(|arg| {
        let syn::FnArg::Typed(pat_type) = arg else {
            return false;
        };
        type_name_starts_with(&pat_type.ty, prefix)
    })
}

fn type_name_starts_with(ty: &syn::Type, prefix: &str) -> bool {
    let syn::Type::Path(type_path) = ty else {
        return false;
    };
    let Some(segment) = type_path.path.segments.last() else {
        return false;
    };
    segment.ident.to_string().starts_with(prefix)
}

/// Whether `attr`'s path — or, if `attr` is `#[cfg_attr(predicate, ...)]`,
/// any attribute it conditionally applies — has a last path segment in
/// `names`.
///
/// `cfg_attr` is a built-in attribute the compiler does not resolve until
/// after every attribute *macro* has finished expanding, so a still-live
/// `#[cfg_attr(feature = "auth", secured("admin"))]` sitting below an outer
/// macro like `#[static_get]`/`#[ws]` reaches that macro's `input_fn.attrs`
/// completely unexpanded — its path is `cfg_attr`, not `secured`. A scan
/// that only compares `attr.path()` against a guard's own name never sees
/// it, so a guard gated behind a feature flag this way would silently slip
/// past a "reject this attribute combination" check that every plainly-
/// written guard attribute is caught by (Codex review on #2513, ninth
/// finding).
pub fn attr_or_cfg_attr_matches_any(attr: &syn::Attribute, names: &[&str]) -> bool {
    let path_matches = |path: &syn::Path| {
        path.segments
            .last()
            .is_some_and(|segment| names.contains(&segment.ident.to_string().as_str()))
    };
    if attr.path().is_ident("cfg_attr") {
        // `cfg_attr(predicate, attr1, attr2, ...)` — the first item is the
        // cfg predicate itself, everything after it is an attribute to
        // apply when the predicate holds.
        let Ok(nested) = attr.parse_args_with(
            syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
        ) else {
            return false;
        };
        return nested.iter().skip(1).any(|meta| path_matches(meta.path()));
    }
    path_matches(attr.path())
}

/// Marker `#[static_get]` injects into the body of every handler it accepts
/// (whenever it isn't already rejecting a guard it recognized by name or by
/// expansion artifact) so that a still-unexpanded guard attribute below it —
/// including one imported under an alias, e.g.
/// `use ::autumn_web::secured as auth;` then `#[auth("admin")]` — can, once
/// IT expands in turn, detect that it is wrapping a function `#[static_get]`
/// already committed to serving from a static-first cache the guard's check
/// can never run against.
///
/// `attr_or_cfg_attr_matches_any` catches every *plainly spelled* guard
/// attribute below `#[static_get]`, expanded or not, but a proc-macro
/// attribute is invoked on raw syntax before the compiler resolves imports —
/// there is no API for a proc macro to ask "does this path actually name
/// `autumn_web::secured`", so an aliased import's spelling is fundamentally
/// invisible to a name-based scan (Codex review on #2513, tenth finding).
/// This marker sidesteps that: it doesn't matter what name a guard was
/// invoked under, because the guard's OWN macro (`secured_macro`/
/// `step_up_macro`/`throttle_macro`/`authorize_macro`, via
/// `reject_if_incompatible_route_marker`) checks for it — mirroring the
/// `__AUTUMN_STEP_UP_MAX_AGE`/`__AUTUMN_THROTTLE_ROUTE_ID` body-marker-const
/// technique this crate already uses to communicate across a macro
/// expansion boundary.
pub const STATIC_ROUTE_HANDLER_MARKER: &str = "__AUTUMN_STATIC_ROUTE_HANDLER_MARKER";

/// Same purpose as [`STATIC_ROUTE_HANDLER_MARKER`], emitted by `#[ws]`
/// instead: lets a still-unexpanded guard attribute below `#[ws]` — alias
/// included — detect, once it expands, that it is wrapping a WebSocket
/// upgrade handler whose `impl WsHandler` return type the guard's
/// unconditional rewrite to `Response` is incompatible with.
pub const WS_HANDLER_MARKER: &str = "__AUTUMN_WS_HANDLER_MARKER";

/// Whether `func`'s body contains a `const` item statement named
/// `marker_name`, at the top level or nested inside a wrapper another
/// attribute macro that expanded in between generated — an intervening
/// `#[cached]`, for instance, re-homes the entire original body (marker
/// included) inside a `(|| async move { … })().await` closure IIFE
/// (`cached_macro`'s `compute`), one level deeper than a top-level-only scan
/// would look (Codex review on #2513, eleventh finding). Delegates to
/// `edge::stmts_have_marker`, the same recursive walk `#[edge]`'s own marker
/// detection already relies on, so every wrapper shape this crate's macros
/// generate is handled in exactly one place.
pub fn has_body_const_marker(func: &ItemFn, marker_name: &str) -> bool {
    crate::edge::stmts_have_marker(&func.block.stmts, marker_name)
}

/// Emit an inert `#[allow(dead_code)] const #marker: () = ();` as the first
/// statement of `func`'s body — the write side of [`has_body_const_marker`].
pub fn prepend_body_const_marker(func: &mut ItemFn, marker_name: &str) {
    let marker_ident = syn::Ident::new(marker_name, proc_macro2::Span::call_site());
    func.block.stmts.insert(
        0,
        syn::parse_quote! {
            #[allow(dead_code)]
            const #marker_ident: () = ();
        },
    );
}

/// Called by each of `secured_macro`/`step_up_macro`/`throttle_macro`/
/// `authorize_macro` immediately after parsing their own `input_fn`: if it
/// already carries [`STATIC_ROUTE_HANDLER_MARKER`] or [`WS_HANDLER_MARKER`],
/// this guard is expanding on a function a route macro further out already
/// committed to `#[static_get]`/`#[ws]` — reject with the same
/// incompatibility error those macros use for a guard they can identify by
/// name, so an aliased import gets caught too (see
/// [`STATIC_ROUTE_HANDLER_MARKER`]'s doc comment).
pub fn reject_if_incompatible_route_marker(func: &ItemFn) -> Option<proc_macro2::TokenStream> {
    if has_body_const_marker(func, STATIC_ROUTE_HANDLER_MARKER) {
        return Some(
            syn::Error::new_spanned(&func.sig, crate::static_route::INCOMPATIBLE_GUARD_MSG)
                .to_compile_error(),
        );
    }
    if has_body_const_marker(func, WS_HANDLER_MARKER) {
        return Some(
            syn::Error::new_spanned(&func.sig, crate::ws::INCOMPATIBLE_GUARD_MSG)
                .to_compile_error(),
        );
    }
    None
}

/// Test-only helper: pull the `fn` named `name` out of a macro's generated
/// output.
///
/// Since issue #1668, `#[secured]`/`#[step_up]`/`#[throttle]` each emit a
/// SIBLING gate item (a marker struct plus its `FromRequestParts` impl)
/// alongside the transformed handler `fn`, rather than a single transformed
/// item. That mirrors how the real compiler feeds stacked attribute macros:
/// each attribute macro is invoked with the tokens of the ONE item it is
/// still attached to, never a bundle of sibling items another macro emitted
/// alongside it — a still-pending attribute (e.g. `#[get]` written above one
/// of these guards) travels along on that single `fn` item, and the compiler
/// re-invokes it with just that item's tokens. A test that hand-simulates
/// stacking by feeding one macro's raw output into another must reproduce
/// that same single-item slice, or it exercises a shape the compiler would
/// never actually produce.
#[cfg(test)]
pub fn extract_fn_item(tokens: proc_macro2::TokenStream, name: &str) -> ItemFn {
    let file: syn::File = syn::parse2(tokens).expect("generated tokens must parse as a file");
    file.items
        .into_iter()
        .find_map(|item| match item {
            syn::Item::Fn(f) if f.sig.ident == name => Some(f),
            _ => None,
        })
        .unwrap_or_else(|| panic!("fn `{name}` not found among the generated items"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    #[test]
    fn detects_simple_ident_binding() {
        let f: ItemFn = parse_quote! {
            async fn h(__autumn_session: Session) {}
        };
        assert!(has_input_named(&f, "__autumn_session"));
        assert!(!has_input_named(&f, "__autumn_state"));
    }

    #[test]
    fn detects_tuple_struct_binding_for_state() {
        let f: ItemFn = parse_quote! {
            async fn h(State(__autumn_state): State<AppState>) {}
        };
        assert!(has_input_named(&f, "__autumn_state"));
        assert!(!has_input_named(&f, "__autumn_session"));
    }

    #[test]
    fn no_inputs_returns_false() {
        let f: ItemFn = parse_quote! { async fn h() {} };
        assert!(!has_input_named(&f, "__autumn_session"));
    }

    #[test]
    fn finds_in_later_position() {
        let f: ItemFn = parse_quote! {
            async fn h(other: i32, __autumn_session: Session) {}
        };
        assert!(has_input_named(&f, "__autumn_session"));
    }

    #[test]
    fn has_any_guard_gate_param_detects_each_kind() {
        let secured: ItemFn = parse_quote! {
            async fn h(_g: __AutumnSecuredGate_h) {}
        };
        let step_up: ItemFn = parse_quote! {
            async fn h(_g: __AutumnStepUpGate_h) {}
        };
        let throttle: ItemFn = parse_quote! {
            async fn h(_g: __AutumnThrottleGate_h) {}
        };
        assert!(has_any_guard_gate_param(&secured));
        assert!(has_any_guard_gate_param(&step_up));
        assert!(has_any_guard_gate_param(&throttle));
    }

    #[test]
    fn has_any_guard_gate_param_false_without_one() {
        let f: ItemFn = parse_quote! {
            async fn h(Json(body): Json<T>) {}
        };
        assert!(!has_any_guard_gate_param(&f));
    }
}
