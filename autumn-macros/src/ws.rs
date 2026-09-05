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

    let (leading_guard_items, input_fn) = match parse::parse_async_handler_with_leading_items(item)
    {
        Ok(v) => v,
        Err(err) => return err,
    };

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
    fn ws_compiles_when_secured_guard_expands_first() {
        // `#[secured("admin")]` above `#[ws]`: the guard expands first and
        // inserts a leading `_: __AutumnSecuredGate_echo` gate parameter.
        // Before the fix, `ws_macro` echoed that parameter's wildcard
        // pattern (`_`) straight back as a call argument, producing
        // `echo (_)` — invalid Rust, since `_` is a pattern, not an
        // expression (#2513 Codex review). It must instead bind the
        // parameter to a generated identifier and forward that.
        let secured = crate::secured::secured_macro(
            quote! { "admin" },
            quote! {
                async fn echo() -> impl WsHandler { |socket| async move {} }
            },
        );
        let generated = ws_macro(quote! { "/echo" }, secured).to_string();

        assert!(
            !generated.contains("echo (_ )") && !generated.contains("echo (_)"),
            "the gate's wildcard pattern must never be forwarded as a call argument: {generated}"
        );
        assert!(
            generated.contains("__autumn_ws_extractor_0"),
            "the gate parameter must be bound to a generated identifier and forwarded by \
             name: {generated}"
        );
        assert!(
            generated.contains("secured : true"),
            "a #[secured]-above-#[ws] route must record secured = true: {generated}"
        );
        assert!(
            generated.contains(r#"required_roles : & ["admin"]"#),
            "roles from a #[secured] guard must survive into the ws route's ApiDoc: {generated}"
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
