//! WebSocket route macro implementation.
//!
//! Generates a WebSocket upgrade handler from a user function that
//! follows the two-function pattern: the outer function runs at
//! upgrade time (with access to extractors) and returns a closure
//! implementing `WsHandler` that handles the live socket.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::parse;

/// Check if a type pattern looks like `AppState` (bare identifier).
/// Return a purpose-written compile error when a `#[ws]` attribute carries a
/// `seo(...)` argument, which the other route macros accept but this one
/// deliberately does not.
fn reject_seo_argument(attr: &TokenStream) -> Option<TokenStream> {
    let seo_ident = attr.clone().into_iter().find_map(|tree| match tree {
        proc_macro2::TokenTree::Ident(ident) if ident == "seo" => Some(ident),
        _ => None,
    })?;
    Some(
        syn::Error::new(
            seo_ident.span(),
            "`#[ws]` does not accept a `seo(...)` argument: a WebSocket upgrade \
             serves no crawlable document. Declare SEO defaults on the HTML route \
             that links to this socket instead.",
        )
        .to_compile_error(),
    )
}

/// Attribute names of the body-guard macros that unconditionally rewrite a
/// handler's return type to `Response` — incompatible with `#[ws]`'s
/// `impl WsHandler` return type. See `ws_macro`'s rejection of the
/// combination in either attribute order.
const INCOMPATIBLE_GUARD_ATTRS: [&str; 4] = ["secured", "step_up", "throttle", "authorize"];

/// Error message for `#[secured]`/`#[step_up]`/`#[throttle]`/`#[authorize]`
/// combined with `#[ws]`, in either attribute order.
pub const INCOMPATIBLE_GUARD_MSG: &str = "`#[secured]`/`#[step_up]`/`#[throttle]`/`#[authorize]` cannot be combined with `#[ws]`, \
     in either attribute order: those guards rewrite the handler to return an HTTP \
     `Response`, but a `#[ws]` handler must return `impl WsHandler`. Check authorization \
     inside the upgrade handler instead — add a `Session` (or other) extractor parameter and \
     reject the upgrade before returning a `WsHandler`.";

/// The exact fully qualified return type every guard
/// (`secured`/`step_up`/`throttle`/`authorize`) rewrites its wrapped
/// handler to (confirmed identical across `secured.rs`/`step_up.rs`/
/// `throttle.rs`/`authorize.rs`).
const GUARD_RESPONSE_PATH: [&str; 5] = ["autumn_web", "reexports", "axum", "response", "Response"];

/// Whether `path` is exactly the guard-generated
/// `::autumn_web::reexports::axum::response::Response` return type, matched
/// segment-by-segment rather than by last segment alone — a legitimate
/// `#[ws]` handler could otherwise return an unrelated user type merely
/// named `Response` (fourth Codex review pass on #2513).
fn is_guard_response_path(path: &syn::Path) -> bool {
    // `GUARD_RESPONSE_PATH[0]` is the literal `"autumn_web"` crate root, but
    // the guard macro's already-finalized output this recognizes may carry a
    // renamed or overridden root instead (#1828) — compare it against the
    // actively resolved name, or a genuine match is missed.
    path.segments.len() == GUARD_RESPONSE_PATH.len()
        && path.segments.first().is_some_and(|segment| {
            segment.ident == crate::crate_path::current_target_path_segment()
        })
        && path
            .segments
            .iter()
            .skip(1)
            .zip(&GUARD_RESPONSE_PATH[1..])
            .all(|(segment, expected)| segment.ident == *expected)
}

fn is_app_state_type(ty: &syn::Type) -> bool {
    if let syn::Type::Path(type_path) = ty
        && type_path.qself.is_none()
    {
        let segments: Vec<_> = type_path.path.segments.iter().collect();
        // Match `AppState` or `autumn_web::AppState` etc.
        if let Some(last) = segments.last() {
            return last.ident == "AppState" && last.arguments.is_none();
        }
    }
    false
}

/// Implementation of the `#[ws("/path")]` attribute macro.
///
/// Given a user function like:
///
/// ```ignore
/// #[ws("/echo")]
/// async fn echo(state: AppState) -> impl WsHandler {
///     |mut socket: WebSocket| async move { /* ... */ }
/// }
/// ```
///
/// Generates:
///
/// 1. The user's function (unchanged)
/// 2. A `__autumn_ws_upgrade_echo` handler that extracts `WebSocketUpgrade`
///    + `State<AppState>`, calls the user function, and upgrades.
/// 3. A `__autumn_route_info_echo` companion returning a `Route` (GET)
///    so `routes![]` works seamlessly.
///
/// The user's function parameters are treated as follows:
/// - `AppState` parameters receive the extracted app state directly
/// - All other parameters become Axum extractors on the upgrade handler
#[allow(clippy::too_many_lines)]
pub fn ws_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    // `#[ws]` takes a path and nothing else, so a stray `seo(...)` would fail
    // with syn's bare "unexpected token" pointing at the comma. Every other
    // misuse of `seo(...)` gets a purpose-written error; say why this one is
    // rejected rather than leaving the user to guess.
    if let Some(err) = reject_seo_argument(&attr) {
        return err;
    }
    // `#[ws]` is not a `route_macro` path, so an `#[edge]` marker would sit
    // inertly in the handler body and the author would believe the socket was
    // edge-eligible. Say no instead (#1790).
    if let Some(err) = crate::edge::reject_if_edge(
        &item,
        "`#[edge]` cannot be combined with `#[ws]`: a WebSocket upgrade holds a live \
         connection against origin state, which the edge capsule has no way to serve. \
         Serve this route from the origin.",
    ) {
        return err;
    }
    let path = match parse::parse_route_path(attr) {
        Ok(p) => p,
        Err(err) => return err,
    };

    let (leading_guard_items, mut input_fn) =
        match parse::parse_async_handler_with_leading_items(item) {
            Ok(v) => v,
            Err(err) => return err,
        };

    // A body guard (`#[secured]`/`#[step_up]`/`#[throttle]`/`#[authorize]`)
    // expanded above `#[ws]` leaves its `FromRequestParts` gate as a leading
    // sibling item (`parse::split_leading_items_and_fn`) — but every one of
    // those guards unconditionally rewrites the wrapped function's return
    // type to `Response` and threads its original return value through
    // `IntoResponse::into_response` (see `secured.rs`/`step_up.rs`/
    // `throttle.rs`/`authorize.rs`). That does not hold for a `#[ws]`
    // handler: it returns `impl WsHandler` (a plain closure), not something
    // `IntoResponse`, and this macro's two-function wrapper expects to call
    // it and get a `WsHandler` back, not a `Response` (#2513 Codex review —
    // binding the gate parameter to a real identifier fixed the forwarding
    // call but left this deeper return-type mismatch). Reject the
    // combination outright rather than emit code that fails to compile deep
    // inside guard-generated code with a confusing error.
    //
    // The same incompatibility exists in the *other* stacking order —
    // `#[ws]` outermost, the guard still a live, unexpanded attribute below
    // it — which leaves `leading_guard_items` empty (nothing has expanded
    // yet) but `input_fn.attrs` still carrying the guard attribute; that
    // guard then rewrites `echo`'s return type to `Response` *after* this
    // macro has already generated a wrapper expecting `impl WsHandler`
    // (second Codex review pass on #2513). Check both. This scan must also
    // see a guard hidden behind `#[cfg_attr(predicate, secured(..))]`:
    // `cfg_attr` is not resolved until after every attribute macro has
    // finished expanding, so a plain `attr.path()` name check sees only
    // `cfg_attr`, never the guard it wraps (Codex review on #2513, ninth
    // finding, same gap as `static_route.rs`'s identical scan) —
    // `param_helpers::attr_or_cfg_attr_matches_any` looks inside `cfg_attr`'s
    // own argument list for this reason.
    let unexpanded_guard_attr = input_fn.attrs.iter().find(|attr| {
        crate::param_helpers::attr_or_cfg_attr_matches_any(attr, &INCOMPATIBLE_GUARD_ATTRS)
    });
    // `#[authorize]` above `#[ws]` (already expanded) slips past both checks
    // above: unlike the other three guards it emits no separate gate sibling
    // item (so `leading_guard_items` is typically empty) and it *removes*
    // its own attribute once consumed (so `input_fn.attrs` no longer carries
    // it either) — see `authorize_macro`'s `#leading_items #input_fn`
    // emission vs. the other three guards' `#leading_items #gate_item
    // #input_fn` (third Codex review pass on #2513). Rather than keep
    // enumerating every guard's particular expansion shape, check the
    // actual invariant directly: all four guards rewrite the return type to
    // the exact same `-> ::autumn_web::reexports::axum::response::Response`
    // (confirmed identical across secured.rs/step_up.rs/throttle.rs/
    // authorize.rs), which a legitimate `#[ws]` handler — required to
    // return `impl WsHandler` — never would. Matched by the *full* path,
    // not just the last segment: a legitimate `#[ws]` handler could return
    // a user-defined type merely named `Response` that implements
    // `WsHandler` (e.g. `my_crate::Response`), and a last-segment-only
    // match would misclassify that as a guard-generated return type and
    // reject a perfectly valid handler (fourth Codex review pass on #2513).
    let already_returns_response = matches!(
        &input_fn.sig.output,
        syn::ReturnType::Type(_, ty)
            if matches!(ty.as_ref(), syn::Type::Path(p) if is_guard_response_path(&p.path))
    );
    if !leading_guard_items.is_empty()
        || unexpanded_guard_attr.is_some()
        || already_returns_response
    {
        let err = if let Some(attr) = unexpanded_guard_attr {
            syn::Error::new_spanned(attr, INCOMPATIBLE_GUARD_MSG)
        } else if !leading_guard_items.is_empty() {
            syn::Error::new_spanned(leading_guard_items, INCOMPATIBLE_GUARD_MSG)
        } else {
            syn::Error::new_spanned(&input_fn.sig.output, INCOMPATIBLE_GUARD_MSG)
        };
        return err.to_compile_error();
    }

    // No guard is visible by name or by expansion artifact at this point,
    // but a still-pending guard attribute imported under an alias (e.g.
    // `use ::autumn_web::secured as auth;` then `#[auth("admin")]`) is
    // exactly as live and unexpanded as a plainly-spelled one, and
    // completely invisible to `attr_or_cfg_attr_matches_any`'s name-based
    // scan — a proc-macro attribute runs before the compiler resolves
    // imports (Codex review on #2513, tenth finding). Leave a marker for
    // that guard's OWN macro to find once IT expands, regardless of what
    // name it was invoked under — see `param_helpers::WS_HANDLER_MARKER`'s
    // doc comment. Must happen before `fn_name`/`vis` below borrow from
    // `input_fn`, since those borrows need to outlive this mutation to
    // reach the final `quote!` at the bottom.
    crate::param_helpers::prepend_body_const_marker(
        &mut input_fn,
        crate::param_helpers::WS_HANDLER_MARKER,
    );

    let fn_name = &input_fn.sig.ident;
    let vis = &input_fn.vis;
    let upgrade_name = format_ident!("__autumn_ws_upgrade_{}", fn_name);
    let route_info_name = format_ident!("__autumn_route_info_{}", fn_name);

    // Honor a `#[public]` marker on the handler so the route audit gate can
    // classify this WebSocket route as `public` rather than `unclassified`.
    // Mirrors the standard `#[get]`/`#[post]` route macro (see
    // `crate::route`), which is the only place `is_public` was previously
    // wired.
    let is_public = crate::api_doc::is_public(&input_fn);

    // Derive `secured`/`required_roles`/`required_scopes` from any
    // `#[secured]` markers on the handler, the same way `crate::route` and
    // `crate::static_route` do (#2513 Codex review): otherwise a
    // `#[secured(...)]`-guarded WebSocket route is protected at runtime but
    // reported as `unclassified`/roleless to `routes audit`.
    let (secured, required_roles, required_scopes) =
        crate::api_doc::extract_secured_info(&input_fn);

    // Separate user params into AppState params (supplied from extracted state)
    // and extractor params (become Axum extractors on the upgrade handler).
    let mut extractor_params = Vec::new();
    let mut call_args = Vec::new();

    for (idx, arg) in input_fn.sig.inputs.iter().enumerate() {
        if let syn::FnArg::Typed(pat_type) = arg {
            if is_app_state_type(&pat_type.ty) {
                // AppState param — supply from our extracted state
                call_args.push(quote! { __autumn_state.clone() });
            } else {
                // Regular extractor — add to upgrade handler params, bound to
                // a freshly generated identifier rather than echoing the
                // original parameter's pattern back as a call argument: a
                // guard (`#[secured]`/`#[step_up]`/`#[throttle]`) expanded
                // above `#[ws]` inserts a leading `_: __AutumnXGate` parameter
                // (#2513 Codex review), and `_` is a pattern, not a valid
                // call-argument expression — `#fn_name(_)` does not compile.
                let attrs = &pat_type.attrs;
                let ty = &pat_type.ty;
                let local_ident = format_ident!("__autumn_ws_extractor_{idx}");
                extractor_params.push(quote! { #(#attrs)* #local_ident: #ty });
                call_args.push(quote! { #local_ident });
            }
        }
    }

    let upgrade_handler = if extractor_params.is_empty() {
        quote! {
            #[doc(hidden)]
            #vis async fn #upgrade_name(
                __autumn_ws: ::autumn_web::ws::WebSocketUpgrade,
                ::autumn_web::reexports::axum::extract::State(__autumn_state): ::autumn_web::reexports::axum::extract::State<::autumn_web::AppState>,
            ) -> impl ::autumn_web::reexports::axum::response::IntoResponse {
                let __autumn_shutdown = __autumn_state.shutdown_token();
                let handler = #fn_name(#(#call_args),*).await;
                __autumn_ws.on_upgrade(move |socket| async move {
                    ::autumn_web::ws::WsHandler::handle(handler, socket, __autumn_shutdown).await;
                })
            }
        }
    } else {
        quote! {
            #[doc(hidden)]
            #vis async fn #upgrade_name(
                __autumn_ws: ::autumn_web::ws::WebSocketUpgrade,
                ::autumn_web::reexports::axum::extract::State(__autumn_state): ::autumn_web::reexports::axum::extract::State<::autumn_web::AppState>,
                #(#extractor_params),*
            ) -> impl ::autumn_web::reexports::axum::response::IntoResponse {
                let __autumn_shutdown = __autumn_state.shutdown_token();
                let handler = #fn_name(#(#call_args),*).await;
                __autumn_ws.on_upgrade(move |socket| async move {
                    ::autumn_web::ws::WsHandler::handle(handler, socket, __autumn_shutdown).await;
                })
            }
        }
    };

    let path_value = path.value();
    let path_params = crate::api_doc::extract_path_params(&path_value);
    let path_params_tokens = crate::api_doc::emit_path_param_slice(&path_params);

    quote! {
        #leading_guard_items
        #input_fn

        #upgrade_handler

        #[doc(hidden)]
        #vis fn #route_info_name() -> ::autumn_web::Route {
            ::autumn_web::Route {
                method: ::autumn_web::reexports::http::Method::from_bytes(b"WS")
                    .expect("WS is a valid method token"),
                path: #path,
                handler: ::autumn_web::reexports::axum::routing::get(#upgrade_name),
                name: ::core::stringify!(#fn_name),
                // WebSocket upgrades don't have a meaningful JSON body, so
                // they are excluded from the generated OpenAPI spec by
                // default. Users wanting to document them can add their
                // own entries via `OpenApiConfig::register_schema`.
                api_doc: ::autumn_web::openapi::ApiDoc {
                    method: "GET",
                    path: #path,
                    operation_id: ::core::stringify!(#fn_name),
                    summary: ::core::option::Option::None,
                    description: ::core::option::Option::None,
                    tags: &[],
                    path_params: #path_params_tokens,
                    request_body: ::core::option::Option::None,
                    response: ::core::option::Option::None,
                    success_status: 101,
                    hidden: true,
                    query_schema: ::core::option::Option::None,
                    secured: #secured,
                    required_roles: #required_roles,
                    required_scopes: #required_scopes,
                    register_schemas: ::core::option::Option::None,
                    api_version: ::core::option::Option::None,
                    public: #is_public,
                    module_path: ::core::module_path!(),
                    source_file: ::core::file!(),
                    source_line: ::core::line!(),
                    ..::core::default::Default::default()
                },
                repository: ::core::option::Option::None,
                idempotency: ::autumn_web::RouteIdempotency::Direct,
                // The inbound timeout only wraps production of the upgrade
                // response head; the live socket future handed to `on_upgrade`
                // runs on a separate task and is never polled under the deadline.
                // So `Inherit` bounds a hung pre-upgrade handshake (async auth /
                // setup) under the global default without ever interrupting an
                // established WebSocket.
                //
                // `#[ws]` intentionally does NOT expose the per-route `timeout_ms`
                // / `timeout = "off"` attributes that `#[route]` supports (it
                // parses the path only, via `parse_route_path`). A WebSocket has no
                // request/response body lifecycle to size a per-route deadline
                // around: `"off"` would let a stalled handshake pin a worker
                // indefinitely, and a larger `timeout_ms` cannot extend to the
                // established socket (that future is unbounded by design, see
                // above) — it would only widen the handshake window. Handshakes
                // that need a tighter or looser bound than the global
                // `request_timeout_ms` should enforce it inside the upgrade
                // handler (e.g. wrap the async auth/setup in
                // `tokio::time::timeout`).
                timeout: ::autumn_web::RouteTimeout::Inherit,
                api_version: ::core::option::Option::None,
                sunset_opt_out: false,
                // A WebSocket upgrade serves no crawlable HTML document, so
                // `#[ws]` does not accept the `seo(...)` route argument.
                seo: ::autumn_web::seo::SeoRouteDefaults::EMPTY,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::ws_macro;

    #[test]
    fn secured_rejects_when_invoked_on_a_ws_handler_via_an_alias() {
        // Same gap as `static_route.rs`'s marker, for `#[ws]`: see
        // `secured::tests::secured_rejects_when_invoked_on_a_static_route_handler_via_an_alias`
        // for the full rationale (Codex review on #2513, tenth finding).
        let accepted = ws_macro(
            quote! { "/echo" },
            quote! {
                #[auth("admin")]
                async fn echo() -> impl WsHandler { |socket| async move {} }
            },
        );
        assert!(
            !accepted.to_string().contains("compile_error"),
            "ws_macro cannot recognize an aliased guard attribute by name: {accepted}"
        );

        let accepted_fn = crate::param_helpers::extract_fn_item(accepted, "echo");
        let generated =
            crate::secured::secured_macro(quote! { "admin" }, quote! { #accepted_fn }).to_string();

        assert!(
            generated.contains("compile_error"),
            "secured_macro must reject a handler already marked as a #[ws] route, regardless \
             of what alias attribute name the source used to invoke it: {generated}"
        );
    }

    #[test]
    fn ws_defaults_public_false() {
        let generated = ws_macro(
            quote! { "/echo" },
            quote! { async fn echo() -> impl WsHandler { |socket| async move {} } },
        )
        .to_string();
        assert!(
            generated.contains("public : false"),
            "an unannotated ws route must record public = false: {generated}"
        );
        // The handler's module path is captured for audit diagnostics.
        assert!(generated.contains("module_path"));
        assert!(
            generated.contains("secured : false"),
            "an unguarded ws route must record secured = false: {generated}"
        );
        assert!(
            generated.contains("required_roles : & []"),
            "an unguarded ws route must have no required roles: {generated}"
        );
    }

    #[test]
    fn ws_rejects_a_secured_guard_expanded_above_it() {
        // `#[secured("admin")]` above `#[ws]`: the guard expands first and
        // inserts a leading `_: __AutumnSecuredGate_echo` gate parameter.
        // Binding that parameter to a generated identifier (rather than
        // echoing its wildcard pattern back as a call argument) fixes the
        // immediate forwarding error, but `#[secured]` also unconditionally
        // rewrites the handler's return type to `Response` and wraps its
        // original return value through `IntoResponse` — which cannot hold
        // for a `#[ws]` handler, whose return type is `impl WsHandler`, a
        // plain closure (second Codex finding on #2513, deeper than the
        // first). Rather than emit code that fails to compile in
        // guard-generated internals, `#[ws]` must reject the combination
        // outright with a clear error.
        let secured = crate::secured::secured_macro(
            quote! { "admin" },
            quote! {
                async fn echo() -> impl WsHandler { |socket| async move {} }
            },
        );
        let generated = ws_macro(quote! { "/echo" }, secured).to_string();

        assert!(
            generated.contains("compile_error"),
            "a #[secured] guard stacked above #[ws] must be a compile error, not silently \
             broken codegen: {generated}"
        );
        assert!(
            generated.contains("cannot be combined with"),
            "the error must explain why the combination is rejected: {generated}"
        );
    }

    #[test]
    fn ws_rejects_a_live_unexpanded_secured_attribute() {
        // The *other* stacking order: `#[ws("/echo")]` outermost, `#[secured]`
        // still a live, unexpanded attribute below it. `#[ws]` sees a single
        // function with `#[secured]` still attached — `leading_guard_items`
        // is empty, since nothing has expanded yet — so the check above (on
        // `leading_guard_items`) alone misses this order entirely: `#[ws]`
        // would generate its wrapper assuming `impl WsHandler`, then
        // `#[secured]` expands afterward and rewrites `echo`'s return type
        // to `Response` out from under it (third Codex finding on #2513).
        let generated = ws_macro(
            quote! { "/echo" },
            quote! {
                #[secured("admin")]
                async fn echo() -> impl WsHandler { |socket| async move {} }
            },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "a live, unexpanded #[secured] attribute below #[ws] must also be a compile \
             error: {generated}"
        );
        assert!(
            generated.contains("cannot be combined with"),
            "the error must explain why the combination is rejected: {generated}"
        );
    }

    #[test]
    fn ws_rejects_a_secured_attribute_wrapped_in_cfg_attr() {
        // Same gap as `static_route.rs`'s identical scan: a guard hidden
        // behind `#[cfg_attr(predicate, secured(..))]` below `#[ws]` is just
        // as live and unexpanded as a bare `#[secured]` attribute in the
        // same position — `cfg_attr` is not resolved until after every
        // attribute macro has finished expanding. Ninth Codex finding on
        // #2513.
        let generated = ws_macro(
            quote! { "/echo" },
            quote! {
                #[cfg_attr(feature = "auth", secured("admin"))]
                async fn echo() -> impl WsHandler { |socket| async move {} }
            },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "a #[secured] guard wrapped in #[cfg_attr] below #[ws] must also be a compile \
             error: {generated}"
        );
    }

    #[test]
    fn ws_rejects_a_step_up_guard_expanded_above_it() {
        let stepped_up = crate::step_up::step_up_macro(
            quote! {},
            quote! {
                async fn echo() -> impl WsHandler { |socket| async move {} }
            },
        );
        let generated = ws_macro(quote! { "/echo" }, stepped_up).to_string();

        assert!(
            generated.contains("compile_error"),
            "a #[step_up] guard stacked above #[ws] must be a compile error: {generated}"
        );
    }

    #[test]
    fn ws_rejects_a_throttle_guard_expanded_above_it() {
        let throttled = crate::throttle::throttle_macro(
            quote! { limit = 10, per = "1m" },
            quote! {
                async fn echo() -> impl WsHandler { |socket| async move {} }
            },
        );
        let generated = ws_macro(quote! { "/echo" }, throttled).to_string();

        assert!(
            generated.contains("compile_error"),
            "a #[throttle] guard stacked above #[ws] must be a compile error: {generated}"
        );
    }

    #[test]
    fn ws_rejects_an_authorize_guard_expanded_above_it() {
        // `#[authorize]` above `#[ws]`: unlike `#[secured]`/`#[step_up]`/
        // `#[throttle]`, `authorize_macro` emits no separate gate sibling
        // item (just `#leading_items #input_fn`) and removes its own
        // attribute once consumed, so neither `leading_guard_items` nor a
        // live attribute on `input_fn` catches this ordering — only the
        // rewritten `-> Response` return type does (third Codex finding on
        // #2513, deeper than the first two).
        let authorized = crate::authorize::authorize_macro(
            quote! { "view", resource = Room },
            quote! {
                async fn echo() -> impl WsHandler { |socket| async move {} }
            },
        );
        let generated = ws_macro(quote! { "/echo" }, authorized).to_string();

        assert!(
            generated.contains("compile_error"),
            "an #[authorize] guard stacked above #[ws] must be a compile error: {generated}"
        );
        assert!(
            generated.contains("cannot be combined with"),
            "the error must explain why the combination is rejected: {generated}"
        );
    }

    #[test]
    fn ws_accepts_a_handler_returning_an_unrelated_response_type() {
        // A `#[ws]` handler returning a user-defined type merely *named*
        // `Response` (not the guard-generated
        // `::autumn_web::reexports::axum::response::Response`) must not be
        // misclassified as guard-incompatible: the return-type check must
        // match the full path, not just the last segment (fourth Codex
        // review pass on #2513).
        let generated = ws_macro(
            quote! { "/echo" },
            quote! {
                async fn echo() -> my_crate::Response { my_crate::Response::new() }
            },
        )
        .to_string();

        assert!(
            !generated.contains("compile_error"),
            "a handler returning an unrelated `Response`-named type must not be rejected: \
             {generated}"
        );
    }

    #[test]
    fn ws_marks_public_when_public_attribute_present() {
        // Ordering A: `#[ws]` outermost, `#[public]` still an attribute below.
        let generated = ws_macro(
            quote! { "/echo" },
            quote! {
                #[public]
                async fn echo() -> impl WsHandler { |socket| async move {} }
            },
        )
        .to_string();
        assert!(
            generated.contains("public : true"),
            "a #[public] ws route must record public = true: {generated}"
        );
    }

    #[test]
    fn ws_marks_public_from_expanded_marker() {
        // Ordering B: `#[public]` written above `#[ws]`, so it expands first and
        // injects the `__AUTUMN_PUBLIC` marker into the body; the ws route macro
        // must recognize the marker.
        let public_fn = crate::public::public_macro(
            quote! {},
            quote! { async fn echo() -> impl WsHandler { |socket| async move {} } },
        );
        let generated = ws_macro(quote! { "/echo" }, public_fn).to_string();
        assert!(
            generated.contains("public : true"),
            "a ws route macro over an expanded #[public] marker must record public = true: {generated}"
        );
    }

    #[test]
    fn ws_rejects_seo_argument_with_a_purpose_written_error() {
        // Every other route macro accepts `seo(...)`. `#[ws]` deliberately does
        // not, so it must say why rather than falling through to syn's bare
        // "unexpected token" on the comma (#1182).
        let generated = ws_macro(
            quote! { "/live", seo(title = "Live") },
            quote! { async fn live() -> impl WsHandler { |socket| async move {} } },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "seo(...) on #[ws] must be a compile error: {generated}"
        );
        assert!(
            generated.contains("does not accept a `seo(...)` argument"),
            "the error must explain why #[ws] rejects it: {generated}"
        );
    }

    #[test]
    fn ws_rejects_a_live_edge_attribute() {
        // A socket cannot be served from the edge capsule. Without this the
        // `#[edge]` marker would sit inertly in the body and the author would
        // believe the route was edge-eligible (#1790).
        let generated = ws_macro(
            quote! { "/live" },
            quote! {
                #[edge]
                async fn live() -> impl WsHandler { |socket| async move {} }
            },
        )
        .to_string();

        assert!(
            generated.contains("compile_error"),
            "#[edge] on a #[ws] route must be rejected: {generated}"
        );
        assert!(
            generated.contains("WebSocket"),
            "the error must explain why a socket cannot run at the edge: {generated}"
        );
    }

    #[test]
    fn ws_rejects_an_expanded_edge_marker() {
        let edged = crate::edge::edge_macro(
            quote! {},
            quote! { async fn live() -> impl WsHandler { |socket| async move {} } },
        );
        let generated = ws_macro(quote! { "/live" }, edged).to_string();

        assert!(
            generated.contains("compile_error"),
            "an already-expanded #[edge] must be rejected on #[ws] too: {generated}"
        );
    }

    #[test]
    fn ws_still_accepts_a_bare_path() {
        let generated = ws_macro(
            quote! { "/live" },
            quote! { async fn live() -> impl WsHandler { |socket| async move {} } },
        )
        .to_string();
        assert!(
            !generated.contains("compile_error"),
            "a plain #[ws(\"/path\")] must keep compiling: {generated}"
        );
    }
}
