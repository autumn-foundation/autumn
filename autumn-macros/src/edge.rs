//! `#[edge]` proc macro implementation (#1790).
//!
//! `#[edge]` is a *marker* attribute: it declares that a read-path handler is
//! eligible to run inside the edge capsule (a `wasm32-wasip1` artifact built
//! alongside the native binary). Like [`#[public]`](crate::public) it injects
//! no runtime guard and does not rewrite the handler signature — it only emits
//! marker consts that the route macros (`#[get]`, …) read back:
//!
//! - `const __AUTUMN_EDGE: () = ();` — the route opted in;
//! - `const __AUTUMN_EDGE_NEEDS_KV: () = ();` — it also declared `needs(kv)`.
//!
//! The markers mirror `#[secured]`'s `__AUTUMN_SECURED_ROLES` mechanism so the
//! route macro can detect the declaration whether `#[edge]` sits above or below
//! the route attribute in the stack — attribute expansion order is not
//! guaranteed, and an undetected opt-in would silently drop the route from the
//! capsule.
//!
//! Everything this module knows about is *declaration*; the code generation for
//! an edge route lives in [`crate::route`], and the refusals for route macros
//! that can never serve from the edge live with those macros.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned as _;
use syn::{Ident, ItemFn, Stmt, Token, parse_quote};

/// Marker const injected for every `#[edge]` handler.
pub const EDGE_MARKER: &str = "__AUTUMN_EDGE";
/// Marker const injected for `#[edge(needs(kv))]`.
pub const EDGE_NEEDS_KV_MARKER: &str = "__AUTUMN_EDGE_NEEDS_KV";

/// The grammar `#[edge]` accepts, quoted verbatim in every rejection so a typo
/// is answered with the whole (small) surface rather than a hint.
const SUPPORTED_GRAMMAR: &str = "`#[edge]` or `#[edge(needs(kv))]`";

/// Capabilities that may appear inside `needs(...)`. The edge host mediates
/// each one; anything else is a platform seam that does not exist yet.
const SUPPORTED_CAPABILITIES: &[&str] = &["kv"];

/// Parsed `#[edge(...)]` arguments.
#[derive(Default, Clone, Copy)]
pub struct EdgeAttrArgs {
    /// Whether `needs(kv)` was declared.
    pub needs_kv: bool,
}

impl Parse for EdgeAttrArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut needs_kv = false;
        let mut seen_needs = false;

        while !input.is_empty() {
            let span = input.span();
            let key: Ident = input.parse().map_err(|_| {
                syn::Error::new(
                    span,
                    format!("unknown `#[edge]` option. Supported forms: {SUPPORTED_GRAMMAR}."),
                )
            })?;

            if key != "needs" {
                return Err(syn::Error::new(
                    key.span(),
                    format!(
                        "unknown `#[edge]` option `{key}`. Supported forms: {SUPPORTED_GRAMMAR}."
                    ),
                ));
            }
            if !input.peek(syn::token::Paren) {
                return Err(syn::Error::new(
                    key.span(),
                    "`needs` takes parenthesized capabilities, e.g. `needs(kv)`.",
                ));
            }
            if seen_needs {
                return Err(syn::Error::new(
                    key.span(),
                    "duplicate `needs(...)` argument. Declare all capabilities in one `needs(...)`.",
                ));
            }
            seen_needs = true;
            needs_kv = parse_needs_group(input)?;

            if input.peek(Token![,]) {
                let _comma: Token![,] = input.parse()?;
            } else {
                break;
            }
        }

        if !input.is_empty() {
            return Err(input.error(format!(
                "unexpected trailing tokens in `#[edge(...)]`. Supported forms: {SUPPORTED_GRAMMAR}."
            )));
        }

        Ok(Self { needs_kv })
    }
}

/// Parse the body of a `needs(...)` argument, returning whether `kv` was named.
///
/// An empty `needs()` is rejected for the same reason an empty `seo()` is: it
/// is almost certainly an unfinished edit, and silently treating it as "no
/// capabilities" would ship a route that faults at the edge instead of failing
/// the build.
fn parse_needs_group(input: ParseStream) -> syn::Result<bool> {
    let span = input.span();
    let content;
    syn::parenthesized!(content in input);

    let mut declared: Vec<String> = Vec::new();
    while !content.is_empty() {
        let capability: Ident = content.parse()?;
        let name = capability.to_string();
        let name = name.strip_prefix("r#").unwrap_or(&name).to_owned();
        if !SUPPORTED_CAPABILITIES.contains(&name.as_str()) {
            return Err(syn::Error::new(
                capability.span(),
                format!(
                    "unknown `needs(...)` capability `{name}`. Supported capabilities: `{}`.",
                    SUPPORTED_CAPABILITIES.join("`, `")
                ),
            ));
        }
        if declared.contains(&name) {
            return Err(syn::Error::new(
                capability.span(),
                format!("duplicate `needs(...)` capability `{name}`. Declare each one once."),
            ));
        }
        declared.push(name);

        if content.peek(Token![,]) {
            let _comma: Token![,] = content.parse()?;
        } else {
            break;
        }
    }

    if !content.is_empty() {
        return Err(content.error("expected `,` between `needs(...)` capabilities"));
    }
    if declared.is_empty() {
        return Err(syn::Error::new(
            span,
            format!(
                "empty `needs(...)`. Name at least one capability, e.g. `needs(kv)`, or drop the \
                 argument entirely. Supported capabilities: `{}`.",
                SUPPORTED_CAPABILITIES.join("`, `")
            ),
        ));
    }

    Ok(declared.iter().any(|name| name == "kv"))
}

/// Expand `#[edge]` / `#[edge(needs(kv))]` on a route handler.
pub fn edge_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args: EdgeAttrArgs = match syn::parse2(attr) {
        Ok(args) => args,
        Err(err) => return err.to_compile_error(),
    };

    let mut input_fn: ItemFn = match syn::parse2(item.clone()) {
        Ok(f) => f,
        Err(_) => {
            return syn::Error::new_spanned(
                item,
                "`#[edge]` can only be applied to route handler functions",
            )
            .to_compile_error();
        }
    };

    // Prepend the markers so a route macro that expands *after* this one can
    // read the declaration back out of the handler body. When the route macro
    // is outermost instead, it detects the `#[edge]` attribute directly (see
    // `detect`). Every other attribute on the handler is passed through
    // untouched — `#[edge]` composes, it does not consume.
    let marker: Stmt = parse_quote! {
        // Route macros read this marker when #[edge] expands before #[get].
        const __AUTUMN_EDGE: () = ();
    };
    input_fn.block.stmts.insert(0, marker);
    if args.needs_kv {
        let needs_kv: Stmt = parse_quote! {
            const __AUTUMN_EDGE_NEEDS_KV: () = ();
        };
        input_fn.block.stmts.insert(1, needs_kv);
    }

    quote! { #input_fn }
}

/// An `#[edge]` declaration found on a handler.
#[derive(Clone, Copy)]
pub struct EdgeMarking {
    /// Whether the handler declared `needs(kv)`.
    pub needs_kv: bool,
    /// Where to point a rejection: the `#[edge]` attribute when it is still
    /// live, otherwise the handler's name.
    pub span: Span,
}

/// Detect an `#[edge]` declaration on a handler.
///
/// Mirrors [`crate::api_doc::is_public`]'s attribute/marker duality:
/// 1. `#[edge]` (or `#[autumn_web::edge]`) still present as an attribute, which
///    happens when the route macro is outermost and `#[edge]` has not expanded
///    yet;
/// 2. the `__AUTUMN_EDGE` marker const emitted into the handler body when
///    `#[edge]` expanded before the route macro.
///
/// The marker is decoded structurally, so handler *text* that merely spells it
/// out never opts a route into the edge lane.
pub fn detect(input_fn: &ItemFn) -> Option<EdgeMarking> {
    for attr in &input_fn.attrs {
        if attr.path().is_ident("edge")
            || attr
                .path()
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "edge")
        {
            // A malformed argument list is reported by `#[edge]` itself when it
            // expands on the handler this macro re-emits; falling back to the
            // default here keeps the diagnostic single, not doubled.
            let args = match &attr.meta {
                syn::Meta::List(list) => {
                    syn::parse2::<EdgeAttrArgs>(list.tokens.clone()).unwrap_or_default()
                }
                _ => EdgeAttrArgs::default(),
            };
            return Some(EdgeMarking {
                needs_kv: args.needs_kv,
                span: attr.path().span(),
            });
        }
    }

    if stmts_have_marker(&input_fn.block.stmts, EDGE_MARKER) {
        return Some(EdgeMarking {
            needs_kv: stmts_have_marker(&input_fn.block.stmts, EDGE_NEEDS_KV_MARKER),
            span: input_fn.sig.ident.span(),
        });
    }

    None
}

/// Whether a generated marker const named `marker` is present in `stmts`.
///
/// Descends the wrappers other guards generate: `#[secured]`/`#[authorize]`/
/// `#[throttle]` re-home the original body inside
/// `let __autumn_inner: T = (async move { … }).await;`, which buries any marker
/// injected by an attribute that expanded before them.
pub fn stmts_have_marker(stmts: &[Stmt], marker: &str) -> bool {
    stmts.iter().any(|stmt| stmt_has_marker(stmt, marker))
}

fn stmt_has_marker(stmt: &Stmt, marker: &str) -> bool {
    match stmt {
        Stmt::Item(syn::Item::Const(item_const)) => item_const.ident == marker,
        Stmt::Expr(expr, _) => expr_has_marker(expr, marker),
        Stmt::Local(local) => local
            .init
            .as_ref()
            .is_some_and(|init| expr_has_marker(&init.expr, marker)),
        _ => false,
    }
}

fn expr_has_marker(expr: &syn::Expr, marker: &str) -> bool {
    match expr {
        syn::Expr::Block(block) => stmts_have_marker(&block.block.stmts, marker),
        syn::Expr::Unsafe(block) => stmts_have_marker(&block.block.stmts, marker),
        _ => crate::idempotency_guard::expr_nested_async_body(expr)
            .is_some_and(|block| stmts_have_marker(&block.stmts, marker)),
    }
}

/// Reject `#[edge]` on a route macro that can never serve from the edge lane.
///
/// `reason` is the complete diagnostic, spanned at the `#[edge]` attribute (or
/// at the handler's name once the attribute has expanded into its marker).
/// Returns `None` — carry on — when the handler is not edge-marked, and also
/// when the item is not a function, so the macro's own "this applies to
/// functions" error stays the one the author sees.
pub fn reject_if_edge(item: &TokenStream, reason: &str) -> Option<TokenStream> {
    let input_fn: ItemFn = syn::parse2(item.clone()).ok()?;
    let marking = detect(&input_fn)?;
    Some(syn::Error::new(marking.span, reason).to_compile_error())
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::{EDGE_MARKER, EDGE_NEEDS_KV_MARKER, detect, edge_macro};

    fn parse_fn(tokens: proc_macro2::TokenStream) -> syn::ItemFn {
        syn::parse2(tokens).expect("test handler should parse")
    }

    #[test]
    fn injects_the_edge_marker() {
        let generated =
            edge_macro(quote! {}, quote! { async fn h() -> &'static str { "ok" } }).to_string();
        assert!(
            generated.contains(EDGE_MARKER),
            "the edge marker must be emitted: {generated}"
        );
        assert!(
            !generated.contains(EDGE_NEEDS_KV_MARKER),
            "a bare #[edge] must not declare any capability: {generated}"
        );
    }

    #[test]
    fn injects_the_kv_capability_marker() {
        let generated = edge_macro(
            quote! { needs(kv) },
            quote! { async fn h() -> &'static str { "ok" } },
        )
        .to_string();
        assert!(
            generated.contains(EDGE_NEEDS_KV_MARKER),
            "needs(kv) must emit the capability marker: {generated}"
        );
    }

    #[test]
    fn preserves_handler_signature_and_other_attributes() {
        let generated = edge_macro(
            quote! {},
            quote! {
                #[public]
                #[api_doc(summary = "greet")]
                async fn h(id: Path<String>) -> &'static str { "ok" }
            },
        )
        .to_string();
        // No return-type rewrite and no injected extractors — the marker only.
        assert!(generated.contains("async fn h (id : Path < String >)"));
        assert!(!generated.contains("Response"));
        // Attributes below `#[edge]` must survive for the route macro to read.
        assert!(generated.contains("# [public]"), "{generated}");
        assert!(generated.contains("# [api_doc"), "{generated}");
    }

    #[test]
    fn marker_is_the_first_statement() {
        let generated = edge_macro(
            quote! { needs(kv) },
            quote! { async fn h() -> &'static str { "ok" } },
        )
        .to_string();
        let body = generated
            .split_once('{')
            .expect("handler should have a body")
            .1;
        assert!(
            body.trim_start().starts_with("const __AUTUMN_EDGE :"),
            "markers must lead the body so guards that wrap it cannot reorder them: {generated}"
        );
    }

    #[test]
    fn rejects_an_unknown_option() {
        let generated = edge_macro(
            quote! { cache = true },
            quote! { async fn h() -> &'static str { "ok" } },
        )
        .to_string();
        assert!(generated.contains("compile_error"), "{generated}");
        assert!(
            generated.contains("unknown `#[edge]` option `cache`"),
            "the error must name the offending option: {generated}"
        );
        assert!(
            generated.contains("`#[edge]` or `#[edge(needs(kv))]`"),
            "the error must spell out the supported grammar: {generated}"
        );
    }

    #[test]
    fn rejects_an_unknown_capability() {
        let generated = edge_macro(
            quote! { needs(db) },
            quote! { async fn h() -> &'static str { "ok" } },
        )
        .to_string();
        assert!(generated.contains("compile_error"), "{generated}");
        assert!(
            generated.contains("unknown `needs(...)` capability `db`"),
            "the error must name the offending capability: {generated}"
        );
        assert!(
            generated.contains("Supported capabilities: `kv`"),
            "the error must list what the edge host can mediate: {generated}"
        );
    }

    #[test]
    fn rejects_needs_without_parentheses() {
        let generated = edge_macro(
            quote! { needs = "kv" },
            quote! { async fn h() -> &'static str { "ok" } },
        )
        .to_string();
        assert!(generated.contains("compile_error"), "{generated}");
        assert!(
            generated.contains("parenthesized capabilities"),
            "the error must show the call-shaped form: {generated}"
        );
    }

    #[test]
    fn rejects_an_empty_needs_group() {
        let generated = edge_macro(
            quote! { needs() },
            quote! { async fn h() -> &'static str { "ok" } },
        )
        .to_string();
        assert!(generated.contains("compile_error"), "{generated}");
        assert!(generated.contains("empty `needs(...)`"), "{generated}");
    }

    #[test]
    fn rejects_a_duplicate_needs_group() {
        let generated = edge_macro(
            quote! { needs(kv), needs(kv) },
            quote! { async fn h() -> &'static str { "ok" } },
        )
        .to_string();
        assert!(generated.contains("compile_error"), "{generated}");
        assert!(
            generated.contains("duplicate `needs(...)` argument"),
            "{generated}"
        );
    }

    #[test]
    fn rejects_a_non_function_item() {
        let generated = edge_macro(quote! {}, quote! { struct NotAHandler; }).to_string();
        assert!(generated.contains("compile_error"), "{generated}");
        assert!(
            generated.contains("route handler functions"),
            "the error must say what #[edge] applies to: {generated}"
        );
    }

    #[test]
    fn detects_a_live_attribute() {
        let marking = detect(&parse_fn(quote! {
            #[edge]
            async fn h() -> &'static str { "ok" }
        }))
        .expect("a live #[edge] attribute must be detected");
        assert!(!marking.needs_kv);

        let marking = detect(&parse_fn(quote! {
            #[edge(needs(kv))]
            async fn h() -> &'static str { "ok" }
        }))
        .expect("a live #[edge(needs(kv))] attribute must be detected");
        assert!(marking.needs_kv);
    }

    #[test]
    fn detects_a_path_qualified_attribute() {
        assert!(
            detect(&parse_fn(quote! {
                #[autumn_web::edge]
                async fn h() -> &'static str { "ok" }
            }))
            .is_some(),
            "a path-qualified #[autumn_web::edge] must be detected"
        );
    }

    #[test]
    fn detects_an_expanded_marker() {
        let expanded = edge_macro(
            quote! { needs(kv) },
            quote! { async fn h() -> &'static str { "ok" } },
        );
        let marking = detect(&parse_fn(expanded)).expect("the marker must be detected");
        assert!(marking.needs_kv);
    }

    #[test]
    fn detects_a_marker_buried_by_a_later_guard() {
        // `#[edge]` above `#[secured]`: secured re-homes the body inside
        // `(async move { … }).await`, burying the marker one level down.
        let edged = edge_macro(quote! {}, quote! { async fn h() -> &'static str { "ok" } });
        let secured = crate::secured::secured_macro(quote! { "admin" }, edged);
        let secured_fn = crate::param_helpers::extract_fn_item(secured, "h");
        assert!(
            detect(&secured_fn).is_some(),
            "a buried #[edge] marker must still be detected"
        );
    }

    #[test]
    fn ignores_a_string_literal_that_spells_the_marker() {
        assert!(
            detect(&parse_fn(quote! {
                async fn h() -> &'static str {
                    let _ = "const __AUTUMN_EDGE: () = ();";
                    "ok"
                }
            }))
            .is_none(),
            "the marker is decoded structurally, never scanned for as text"
        );
    }

    #[test]
    fn ignores_an_unmarked_handler() {
        assert!(
            detect(&parse_fn(quote! {
                async fn h() -> &'static str { "ok" }
            }))
            .is_none()
        );
    }
}
