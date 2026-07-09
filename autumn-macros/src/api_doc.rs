// OpenAPI appears many times in this module's docs as an acronym —
// silence clippy::doc_markdown for it locally. The other allows turn
// off pedantic style nits (`KeyValue::Foo` vs `Foo` inside the enum's
// own impl, `Option::map_or_else` vs `match`/`if let`) that would
// trade clarity for less-readable chained closure calls.
#![allow(
    clippy::doc_markdown,
    clippy::option_if_let_else,
    clippy::single_match_else,
    clippy::use_self
)]

//! `#[api_doc(...)]` attribute parsing for OpenAPI auto-generation.
//!
//! `#[api_doc]` is handled two ways:
//!
//! * As a **stored attribute** on route handlers (`#[get]`, `#[post]`, …):
//!   the route macro strips it from the function's attribute list,
//!   parses it here, and embeds the result in the generated `ApiDoc`
//!   struct.
//! * As a **standalone proc-macro** (see [`macro@crate::api_doc`]) so Rust
//!   accepts the attribute on its own. That entry point is a no-op
//!   wrapper — the real work happens when a route macro is also
//!   applied, since routes are the only places metadata is collected.
//!
//! Supported forms:
//!
//! ```ignore
//! #[api_doc(summary = "Fetch a user", tag = "users")]
//! #[api_doc(description = "...", tags = ["users", "admin"], status = 201)]
//! #[api_doc(hidden)]
//! ```
//!
//! Unknown keys are a compile error, so typos surface at build time.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Attribute, Expr, ExprLit, Ident, Lit, LitBool, LitStr, Token};

/// Parsed `#[api_doc(...)]` attribute arguments.
// Each bool models a distinct, orthogonal attribute flag (`hidden`, `mcp`,
// `mcp = false`, `stream`); grouping them would obscure rather than clarify.
#[allow(clippy::struct_excessive_bools)]
#[derive(Default)]
pub struct ApiDocAttr {
    pub summary: Option<LitStr>,
    pub description: Option<LitStr>,
    pub tags: Vec<LitStr>,
    pub operation_id: Option<LitStr>,
    pub status: Option<u16>,
    pub hidden: bool,
    /// `#[api_doc(mcp)]` / `#[api_doc(mcp = true)]` — opt this endpoint in
    /// as an MCP tool.
    pub mcp_tool: bool,
    /// `#[api_doc(mcp = false)]` — explicitly exclude from MCP, honored
    /// even under the whole-API hatch.
    pub mcp_exclude: bool,
    /// `#[api_doc(mcp, stream)]` — this MCP tool returns an Autumn `Sse`
    /// stream, projected onto the MCP Streamable-HTTP SSE channel as
    /// `notifications/progress` messages terminated by the final result.
    /// Only meaningful together with `mcp`; it also exempts the tool from
    /// the JSON-response eligibility gate (an `Sse` handler has no JSON
    /// response schema).
    pub mcp_stream: bool,
}

enum KeyValue {
    Summary(LitStr),
    Description(LitStr),
    Tag(LitStr),
    Tags(Vec<LitStr>),
    OperationId(LitStr),
    Status(u16),
    Hidden,
    /// `true` => opt in as a tool, `false` => explicit exclusion.
    Mcp(bool),
    /// `stream` flag — this MCP tool streams over SSE.
    Stream,
}

impl Parse for KeyValue {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let key: Ident = input.parse()?;
        let key_str = key.to_string();

        if key_str == "hidden" {
            if input.peek(Token![=]) {
                let _eq: Token![=] = input.parse()?;
                let value: LitBool = input.parse()?;
                return Ok(if value.value {
                    KeyValue::Hidden
                } else {
                    // `hidden = false` is equivalent to the default (visible).
                    // Return a distinguishable marker via Tags(vec![]) so
                    // ApiDocAttr::merge does nothing — this keeps parsing
                    // symmetric with other bool forms.
                    KeyValue::Tags(Vec::new())
                });
            }
            return Ok(KeyValue::Hidden);
        }

        if key_str == "mcp" {
            if input.peek(Token![=]) {
                let _eq: Token![=] = input.parse()?;
                let value: LitBool = input.parse()?;
                return Ok(KeyValue::Mcp(value.value));
            }
            // Bare `mcp` flag opts in.
            return Ok(KeyValue::Mcp(true));
        }

        if key_str == "stream" {
            if input.peek(Token![=]) {
                let _eq: Token![=] = input.parse()?;
                let value: LitBool = input.parse()?;
                return Ok(if value.value {
                    KeyValue::Stream
                } else {
                    // `stream = false` is the default; emit a no-op marker.
                    KeyValue::Tags(Vec::new())
                });
            }
            // Bare `stream` flag opts in.
            return Ok(KeyValue::Stream);
        }

        let _eq: Token![=] = input.parse()?;
        match key_str.as_str() {
            "summary" => Ok(KeyValue::Summary(input.parse()?)),
            "description" => Ok(KeyValue::Description(input.parse()?)),
            "tag" => Ok(KeyValue::Tag(input.parse()?)),
            "tags" => {
                // `tags = ["a", "b"]`
                let content;
                syn::bracketed!(content in input);
                let items =
                    syn::punctuated::Punctuated::<LitStr, Token![,]>::parse_terminated(&content)?;
                Ok(KeyValue::Tags(items.into_iter().collect()))
            }
            "operation_id" => Ok(KeyValue::OperationId(input.parse()?)),
            "status" => {
                let value: Expr = input.parse()?;
                let n = expect_u16(&value)?;
                Ok(KeyValue::Status(n))
            }
            other => Err(syn::Error::new(
                key.span(),
                format!(
                    "unknown key `{other}` in `#[api_doc(...)]`. \
                     Supported keys: summary, description, tag, tags, operation_id, status, hidden, mcp, stream."
                ),
            )),
        }
    }
}

fn expect_u16(expr: &Expr) -> syn::Result<u16> {
    if let Expr::Lit(ExprLit {
        lit: Lit::Int(int), ..
    }) = expr
    {
        int.base10_parse::<u16>()
    } else {
        Err(syn::Error::new_spanned(
            expr,
            "expected an integer HTTP status code (e.g. `status = 201`)",
        ))
    }
}

impl ApiDocAttr {
    fn merge(&mut self, kv: KeyValue) {
        match kv {
            KeyValue::Summary(v) => self.summary = Some(v),
            KeyValue::Description(v) => self.description = Some(v),
            KeyValue::Tag(v) => self.tags = vec![v],
            KeyValue::Tags(v) if !v.is_empty() => self.tags = v,
            KeyValue::Tags(_) => {}
            KeyValue::OperationId(v) => self.operation_id = Some(v),
            KeyValue::Status(n) => self.status = Some(n),
            KeyValue::Hidden => self.hidden = true,
            KeyValue::Mcp(true) => self.mcp_tool = true,
            KeyValue::Mcp(false) => self.mcp_exclude = true,
            KeyValue::Stream => self.mcp_stream = true,
        }
    }
}

impl Parse for ApiDocAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let items = syn::punctuated::Punctuated::<KeyValue, Token![,]>::parse_terminated(input)?;
        let mut out = ApiDocAttr::default();
        for kv in items {
            out.merge(kv);
        }
        Ok(out)
    }
}

/// Strip `#[api_doc(...)]` attributes from a handler's attribute list
/// and merge all of them into a single [`ApiDocAttr`].
///
/// Repeating the attribute is legal; later values override earlier ones
/// for scalar fields, and `tags = [..]` replaces the accumulated tags.
pub fn extract(attrs: &mut Vec<Attribute>) -> Result<ApiDocAttr, TokenStream> {
    let mut collected = ApiDocAttr::default();
    let mut error: Option<TokenStream> = None;

    attrs.retain(|attr| {
        if !attr.path().is_ident("api_doc") {
            return true;
        }
        // `#[api_doc]` with no arguments → mark visible with no overrides.
        let parsed: syn::Result<ApiDocAttr> = match &attr.meta {
            syn::Meta::Path(_) => Ok(ApiDocAttr::default()),
            syn::Meta::List(list) => syn::parse2(list.tokens.clone()),
            syn::Meta::NameValue(nv) => Err(syn::Error::new_spanned(
                nv,
                "expected `#[api_doc(...)]`, not `#[api_doc = ...]`",
            )),
        };
        match parsed {
            Ok(parsed) => {
                collected.absorb(parsed);
            }
            Err(err) => {
                if error.is_none() {
                    error = Some(err.to_compile_error());
                }
            }
        }
        false
    });

    if let Some(err) = error {
        return Err(err);
    }
    Ok(collected)
}

impl ApiDocAttr {
    fn absorb(&mut self, other: ApiDocAttr) {
        if other.summary.is_some() {
            self.summary = other.summary;
        }
        if other.description.is_some() {
            self.description = other.description;
        }
        if !other.tags.is_empty() {
            self.tags = other.tags;
        }
        if other.operation_id.is_some() {
            self.operation_id = other.operation_id;
        }
        if other.status.is_some() {
            self.status = other.status;
        }
        if other.hidden {
            self.hidden = true;
        }
        if other.mcp_tool {
            self.mcp_tool = true;
        }
        if other.mcp_exclude {
            self.mcp_exclude = true;
        }
        if other.mcp_stream {
            self.mcp_stream = true;
        }
    }

    /// Emit field initializers `summary: ..., description: ..., tags: ..., hidden: ...`
    /// for inclusion in an `ApiDoc { ... }` literal.
    ///
    /// `default_operation_id` is used when `operation_id` was not set on
    /// the attribute — typically the handler function's identifier.
    pub fn emit_ident_fields(&self, default_operation_id: &Ident) -> TokenStream {
        let summary = option_str(self.summary.as_ref());
        let description = option_str(self.description.as_ref());
        let tags = slice_str(&self.tags);
        let op_id = if let Some(id) = &self.operation_id {
            quote! { #id }
        } else {
            quote! { ::core::stringify!(#default_operation_id) }
        };
        let status = self.status.unwrap_or(200);
        let hidden = self.hidden;
        let mcp_tool = self.mcp_tool;
        let mcp_exclude = self.mcp_exclude;
        let mcp_stream = self.mcp_stream;
        quote! {
            operation_id: #op_id,
            summary: #summary,
            description: #description,
            tags: #tags,
            success_status: #status,
            hidden: #hidden,
            mcp_tool: #mcp_tool,
            mcp_exclude: #mcp_exclude,
            mcp_stream: #mcp_stream,
        }
    }
}

fn option_str(lit: Option<&LitStr>) -> TokenStream {
    match lit {
        Some(v) => quote! { ::core::option::Option::Some(#v) },
        None => quote! { ::core::option::Option::None },
    }
}

fn slice_str(items: &[LitStr]) -> TokenStream {
    if items.is_empty() {
        quote! { &[] }
    } else {
        let literals: Vec<_> = items.iter().map(|s| quote! { #s }).collect();
        quote! { &[#(#literals),*] }
    }
}

// ──────────────────────────────────────────────────────────────────
// Route path + signature inspection helpers
// ──────────────────────────────────────────────────────────────────

/// Extract `{name}` path parameters from a route template.
///
/// Closing braces without an opening brace are ignored. Segments that
/// contain regex (`{id:[0-9]+}`) take only the name before the colon.
/// Escaped braces (`{{` / `}}`) are treated as literal characters and skipped.
pub fn extract_path_params(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut remaining = path;

    while let Some(start) = remaining.find('{') {
        let after_brace = &remaining[start + 1..];
        // `{{` is an escaped literal brace — skip both characters and continue.
        if let Some(rest) = after_brace.strip_prefix('{') {
            remaining = rest;
            continue;
        }
        let Some(end_rel) = after_brace.find('}') else {
            break;
        };

        let inner = &after_brace[..end_rel];
        let name = inner.split(':').next().unwrap_or(inner).trim();
        if !name.is_empty() {
            out.push(name.to_owned());
        }

        remaining = &after_brace[end_rel + 1..];
    }

    out
}

/// Emit a `&'static [&'static str]` literal for a list of owned strings.
pub fn emit_path_param_slice(params: &[String]) -> TokenStream {
    if params.is_empty() {
        quote! { &[] }
    } else {
        let literals: Vec<_> = params
            .iter()
            .map(|p| LitStr::new(p, Span::call_site()))
            .collect();
        quote! { &[#(#literals),*] }
    }
}

/// Inspect the handler's parameter list for `Json<T>` request bodies.
///
/// Returns a `Some(tokens)` producing a `SchemaEntry` initializer for the
/// first JSON extractor seen, or `None` if the handler has no JSON body.
///
/// Recognizes Autumn's validation wrapper as well: a parameter typed
/// `Valid<Json<T>>` is treated the same as `Json<T>` so handlers using
/// the documented validator pattern still get a `requestBody` in the
/// generated spec.
pub fn infer_request_body(input_fn: &syn::ItemFn) -> Option<TokenStream> {
    for arg in &input_fn.sig.inputs {
        let syn::FnArg::Typed(pat) = arg else {
            continue;
        };
        if let Some(inner) = unwrap_json_body(&pat.ty) {
            return Some(schema_entry_for_type(&inner));
        }
    }
    None
}

/// Peel one layer of `Valid<...>` so that
/// `Valid<Json<NewPost>>` → `Json<NewPost>` → `NewPost`.
///
/// Matches either a bare `Json<T>` or `Valid<Json<T>>` and returns the
/// inner `T`. Any deeper nesting returns `None` — we intentionally
/// don't guess at unknown wrappers because mis-identifying them would
/// produce wrong schemas.
fn unwrap_json_body(ty: &syn::Type) -> Option<syn::Type> {
    if let Some(inner) = unwrap_single_generic(ty, "Json") {
        return Some(inner);
    }
    if let Some(inner) = unwrap_single_generic(ty, "Valid")
        && let Some(payload) = unwrap_single_generic(&inner, "Json")
    {
        return Some(payload);
    }
    None
}

/// Inspect the handler's return type for `Json<T>` to infer the success
/// response body. Handles several common Axum return-type patterns:
///
/// * `Json<T>` — plain JSON body
/// * `Result<Json<T>, _>` / `AutumnResult<Json<T>>` — fallible JSON
/// * `(StatusCode, Json<T>)` — JSON with a custom status code
/// * `Result<(StatusCode, Json<T>), _>` — the two combined
pub fn infer_response_body(input_fn: &syn::ItemFn) -> Option<TokenStream> {
    let syn::ReturnType::Type(_, ty) = &input_fn.sig.output else {
        return None;
    };
    let ty = unwrap_result_ok(ty).unwrap_or_else(|| (**ty).clone());
    find_json_in_type(&ty).map(|inner| schema_entry_for_type(&inner))
}

/// Look for `Json<T>` either directly or inside a tuple element.
///
/// Axum handlers often return tuples like `(StatusCode, Json<T>)` or
/// `([(HeaderName, _); N], Json<T>)` to attach status codes or
/// headers. We scan each tuple element so the generated schema still
/// reflects the JSON body.
fn find_json_in_type(ty: &syn::Type) -> Option<syn::Type> {
    if let Some(inner) = unwrap_single_generic(ty, "Json") {
        return Some(inner);
    }
    if let syn::Type::Tuple(tup) = ty {
        for elem in &tup.elems {
            if let Some(inner) = unwrap_single_generic(elem, "Json") {
                return Some(inner);
            }
        }
    }
    None
}

/// Peel a single layer of `Result<T, _>` / `AutumnResult<T>` so we can
/// inspect the `Ok` variant for a `Json<...>` wrapper.
fn unwrap_result_ok(ty: &syn::Type) -> Option<syn::Type> {
    let path = match ty {
        syn::Type::Path(p) => &p.path,
        _ => return None,
    };
    let last = path.segments.last()?;
    let name = last.ident.to_string();
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    match name.as_str() {
        "Result" => args.args.iter().find_map(|arg| match arg {
            syn::GenericArgument::Type(t) => Some(t.clone()),
            _ => None,
        }),
        "AutumnResult" => args.args.iter().find_map(|arg| match arg {
            syn::GenericArgument::Type(t) => Some(t.clone()),
            _ => None,
        }),
        _ => None,
    }
}

/// If `ty` is `Name<Inner>` (single generic argument), return `Inner`.
/// The outermost segment of `ty`'s path must match `wrapper`.
pub fn unwrap_single_generic(ty: &syn::Type, wrapper: &str) -> Option<syn::Type> {
    let syn::Type::Path(path) = ty else {
        return None;
    };
    let last = path.path.segments.last()?;
    if last.ident != wrapper {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    args.args.iter().find_map(|arg| match arg {
        syn::GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    })
}

/// Emit a `::autumn_web::openapi::SchemaEntry` initializer for a type.
///
/// Handles the following patterns:
///
/// * `Vec<T>`         → `SchemaKind::Array(&inner)`  (array of `T`)
/// * `Option<T>`      → `SchemaKind::Nullable(&inner)` (nullable `T`)
/// * known primitive  → `SchemaKind::Primitive("string"|"integer"|…)`
/// * everything else  → `SchemaKind::Ref` with the type's last path
///   segment as the schema name (back-filled by the spec generator)
fn schema_entry_for_type(ty: &syn::Type) -> TokenStream {
    // Vec<T> → array of <schema of T>.
    if let Some(inner) = unwrap_single_generic(ty, "Vec") {
        let inner_tokens = schema_entry_for_type(&inner);
        return quote! {
            ::autumn_web::openapi::SchemaEntry {
                name: "array",
                kind: ::autumn_web::openapi::SchemaKind::Array(&#inner_tokens),
            }
        };
    }
    // Option<T> → nullable <schema of T>.
    if let Some(inner) = unwrap_single_generic(ty, "Option") {
        let inner_tokens = schema_entry_for_type(&inner);
        return quote! {
            ::autumn_web::openapi::SchemaEntry {
                name: "nullable",
                kind: ::autumn_web::openapi::SchemaKind::Nullable(&#inner_tokens),
            }
        };
    }

    let name = last_segment_name(ty).unwrap_or_else(|| "Schema".to_owned());
    let name_lit = LitStr::new(&name, Span::call_site());
    if let Some(json_type) = primitive_json_type(&name) {
        let json_lit = LitStr::new(json_type, Span::call_site());
        quote! {
            ::autumn_web::openapi::SchemaEntry {
                name: #name_lit,
                kind: ::autumn_web::openapi::SchemaKind::Primitive(#json_lit),
            }
        }
    } else {
        quote! {
            ::autumn_web::openapi::SchemaEntry {
                name: #name_lit,
                kind: ::autumn_web::openapi::SchemaKind::Ref,
            }
        }
    }
}

/// Map a short Rust primitive name to its JSON-schema `type` keyword.
pub fn primitive_json_type(name: &str) -> Option<&'static str> {
    Some(match name {
        "String" | "str" => "string",
        "bool" => "boolean",
        "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "isize" | "usize" => {
            "integer"
        }
        "f32" | "f64" => "number",
        _ => return None,
    })
}

/// Return the final identifier in a type's path (e.g. `foo::Bar` → `"Bar"`).
pub fn last_segment_name(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        syn::Type::Reference(r) => last_segment_name(&r.elem),
        _ => None,
    }
}

/// Inspect the handler's parameter list for `Query<T>` query-string extractors.
///
/// Returns `Some(tokens)` producing a `SchemaEntry` initializer when a
/// `Query<T>` parameter is found, `None` otherwise. Only the first `Query`
/// extractor is used — multiple `Query<T>` parameters are uncommon and the
/// first one captures the intent.
pub fn infer_query_params(input_fn: &syn::ItemFn) -> Option<TokenStream> {
    for arg in &input_fn.sig.inputs {
        let syn::FnArg::Typed(pat) = arg else {
            continue;
        };
        if let Some(inner) = unwrap_single_generic(&pat.ty, "Query") {
            return Some(schema_entry_for_type(&inner));
        }
    }
    None
}

/// Detect `#[secured]` on a handler and return `(secured, required_roles)`.
///
/// Two detection strategies:
/// 1. `#[secured]` still in attrs (route macro is outermost; secured is below it).
/// 2. Function-local `__AUTUMN_SECURED_ROLES` marker present (secured was
///    above the route macro and already expanded its body).
/// 3. Legacy fallback: `__autumn_session` param present.
pub fn has_policy_check_in_stmts(stmts: &[syn::Stmt]) -> bool {
    for stmt in stmts {
        let s = quote::quote!(#stmt).to_string();
        if s.contains("__check_policy") {
            return true;
        }
    }
    false
}

pub fn extract_secured_info(input_fn: &syn::ItemFn) -> (bool, TokenStream, TokenStream) {
    // Case 1 — #[secured] or #[autumn_web::secured] visible as a remaining attribute.
    for attr in &input_fn.attrs {
        if attr.path().is_ident("secured")
            || attr
                .path()
                .segments
                .last()
                .is_some_and(|s| s.ident == "secured")
        {
            let roles = extract_secured_roles(attr);
            let scopes = extract_secured_scopes(attr);
            return (
                true,
                emit_static_str_slice(&roles),
                emit_static_str_slice(&scopes),
            );
        }
    }

    // Case 1b — #[authorize] or #[autumn_web::authorize] visible as a remaining attribute.
    for attr in &input_fn.attrs {
        if attr.path().is_ident("authorize")
            || attr
                .path()
                .segments
                .last()
                .is_some_and(|s| s.ident == "authorize")
        {
            return (true, quote! { &[] }, quote! { &[] });
        }
    }

    // Case 2 — #[secured] was above the route macro and already expanded;
    // read the markers emitted into the guarded function body.
    if let Some(roles) = extract_secured_roles_marker(input_fn) {
        let scopes = extract_secured_scopes_marker(input_fn).unwrap_or_default();
        return (
            true,
            emit_static_str_slice(&roles),
            emit_static_str_slice(&scopes),
        );
    }

    // Case 2b — #[authorize] was above the route macro and already expanded;
    // check if a policy check statement is present.
    if has_policy_check_in_stmts(&input_fn.block.stmts) {
        return (true, quote! { &[] }, quote! { &[] });
    }

    // Case 3 — compatibility fallback for expansions produced before the
    // marker existed. This can only recover that the route is secured.
    let has_session = input_fn.sig.inputs.iter().any(|param| {
        if let syn::FnArg::Typed(pt) = param
            && let syn::Pat::Ident(pi) = pt.pat.as_ref()
        {
            return pi.ident == "__autumn_session";
        }
        false
    });
    if has_session {
        return (true, quote! { &[] }, quote! { &[] });
    }

    (false, quote! { &[] }, quote! { &[] })
}

fn extract_secured_roles_marker(input_fn: &syn::ItemFn) -> Option<Vec<String>> {
    extract_secured_roles_marker_from_stmts(&input_fn.block.stmts)
}

fn extract_secured_roles_marker_from_stmts(stmts: &[syn::Stmt]) -> Option<Vec<String>> {
    stmts
        .iter()
        .find_map(extract_secured_roles_marker_from_stmt)
}

fn extract_secured_roles_marker_from_stmt(stmt: &syn::Stmt) -> Option<Vec<String>> {
    match stmt {
        syn::Stmt::Item(syn::Item::Const(item_const))
            if item_const.ident == "__AUTUMN_SECURED_ROLES" =>
        {
            extract_roles_from_marker_expr(&item_const.expr)
        }
        syn::Stmt::Expr(expr, _) => extract_secured_roles_marker_from_expr(expr),
        syn::Stmt::Local(local) => local
            .init
            .as_ref()
            .and_then(|init| extract_secured_roles_marker_from_expr(&init.expr)),
        _ => None,
    }
}

fn extract_secured_roles_marker_from_expr(expr: &syn::Expr) -> Option<Vec<String>> {
    match expr {
        syn::Expr::Block(block) => extract_secured_roles_marker_from_stmts(&block.block.stmts),
        syn::Expr::Async(block) => extract_secured_roles_marker_from_stmts(&block.block.stmts),
        syn::Expr::Unsafe(block) => extract_secured_roles_marker_from_stmts(&block.block.stmts),
        _ => None,
    }
}

fn extract_roles_from_marker_expr(expr: &syn::Expr) -> Option<Vec<String>> {
    let syn::Expr::Reference(reference) = expr else {
        return None;
    };
    let syn::Expr::Array(array) = reference.expr.as_ref() else {
        return None;
    };

    let mut roles = Vec::with_capacity(array.elems.len());
    for elem in &array.elems {
        let syn::Expr::Lit(lit) = elem else {
            return None;
        };
        let syn::Lit::Str(role) = &lit.lit else {
            return None;
        };
        roles.push(role.value());
    }
    Some(roles)
}

fn extract_secured_roles(attr: &syn::Attribute) -> Vec<String> {
    use proc_macro2::TokenTree;

    let syn::Meta::List(list) = &attr.meta else {
        return Vec::new();
    };
    // Roles are the leading bare string literals; a trailing `scopes = [...]`
    // (token abilities) may follow and is not a role, so peel literals
    // directly rather than parsing the whole list as `LitStr`s.
    let mut roles = Vec::new();
    let mut iter = list.tokens.clone().into_iter().peekable();
    while let Some(TokenTree::Literal(lit)) = iter.peek() {
        match syn::parse2::<syn::LitStr>(quote! { #lit }) {
            Ok(s) => roles.push(s.value()),
            Err(_) => break,
        }
        iter.next();
        if let Some(TokenTree::Punct(p)) = iter.peek()
            && p.as_char() == ','
        {
            iter.next();
        } else {
            break;
        }
    }
    roles
}

fn extract_secured_scopes(attr: &syn::Attribute) -> Vec<String> {
    use proc_macro2::TokenTree;

    let syn::Meta::List(list) = &attr.meta else {
        return Vec::new();
    };
    // Scopes appear as `scopes = ["scope1", "scope2"]` after any role literals.
    let mut iter = list.tokens.clone().into_iter();
    while let Some(tt) = iter.next() {
        let TokenTree::Ident(ident) = tt else {
            continue;
        };
        if ident != "scopes" {
            continue;
        }
        let Some(TokenTree::Punct(p)) = iter.next() else {
            continue;
        };
        if p.as_char() != '=' {
            continue;
        }
        let Some(TokenTree::Group(group)) = iter.next() else {
            continue;
        };
        let mut scopes = Vec::new();
        for inner_tt in group.stream() {
            if let TokenTree::Literal(lit) = inner_tt
                && let Ok(s) = syn::parse2::<syn::LitStr>(quote! { #lit })
            {
                scopes.push(s.value());
            }
        }
        return scopes;
    }
    Vec::new()
}

fn extract_secured_scopes_marker(input_fn: &syn::ItemFn) -> Option<Vec<String>> {
    extract_secured_scopes_marker_from_stmts(&input_fn.block.stmts)
}

fn extract_secured_scopes_marker_from_stmts(stmts: &[syn::Stmt]) -> Option<Vec<String>> {
    stmts
        .iter()
        .find_map(extract_secured_scopes_marker_from_stmt)
}

fn extract_secured_scopes_marker_from_stmt(stmt: &syn::Stmt) -> Option<Vec<String>> {
    match stmt {
        syn::Stmt::Item(syn::Item::Const(item_const))
            if item_const.ident == "__AUTUMN_SECURED_SCOPES" =>
        {
            extract_roles_from_marker_expr(&item_const.expr)
        }
        syn::Stmt::Expr(expr, _) => extract_secured_scopes_marker_from_expr(expr),
        syn::Stmt::Local(local) => local
            .init
            .as_ref()
            .and_then(|init| extract_secured_scopes_marker_from_expr(&init.expr)),
        _ => None,
    }
}

fn extract_secured_scopes_marker_from_expr(expr: &syn::Expr) -> Option<Vec<String>> {
    match expr {
        syn::Expr::Block(block) => extract_secured_scopes_marker_from_stmts(&block.block.stmts),
        syn::Expr::Async(block) => extract_secured_scopes_marker_from_stmts(&block.block.stmts),
        syn::Expr::Unsafe(block) => extract_secured_scopes_marker_from_stmts(&block.block.stmts),
        _ => None,
    }
}

fn emit_static_str_slice(items: &[String]) -> TokenStream {
    if items.is_empty() {
        quote! { &[] }
    } else {
        let lits: Vec<_> = items
            .iter()
            .map(|s| LitStr::new(s, Span::call_site()))
            .collect();
        quote! { &[#(#lits),*] }
    }
}

/// Convenience wrapper: emit an `Option<SchemaEntry>` expression.
pub fn schema_option(expr: Option<TokenStream>) -> TokenStream {
    match expr {
        Some(e) => quote! { ::core::option::Option::Some(#e) },
        None => quote! { ::core::option::Option::None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_path_params_handles_single() {
        assert_eq!(extract_path_params("/users/{id}"), vec!["id".to_owned()]);
    }

    #[test]
    fn extract_path_params_handles_multiple() {
        assert_eq!(
            extract_path_params("/posts/{year}/{slug}"),
            vec!["year".to_owned(), "slug".to_owned()]
        );
    }

    #[test]
    fn extract_path_params_handles_regex_prefix() {
        assert_eq!(
            extract_path_params("/users/{id:[0-9]+}"),
            vec!["id".to_owned()]
        );
    }

    #[test]
    fn extract_path_params_returns_empty_for_static() {
        assert!(extract_path_params("/hello").is_empty());
        assert!(extract_path_params("/").is_empty());
    }

    #[test]
    fn extract_path_params_ignores_unclosed_braces() {
        assert!(extract_path_params("/oops/{broken").is_empty());
    }

    #[test]
    fn extract_path_params_skips_escaped_braces() {
        // `{{hello}}` is a static route segment, not a path parameter.
        assert!(extract_path_params("/{{hello}}").is_empty());
        // Escaped brace followed by a real param.
        assert_eq!(
            extract_path_params("/{{literal}}/{id}"),
            vec!["id".to_owned()]
        );
    }

    #[test]
    fn primitive_json_type_matches_common() {
        assert_eq!(primitive_json_type("String"), Some("string"));
        assert_eq!(primitive_json_type("i64"), Some("integer"));
        assert_eq!(primitive_json_type("bool"), Some("boolean"));
        assert_eq!(primitive_json_type("Foo"), None);
    }

    #[test]
    fn secured_roles_marker_extracts_roles() {
        let input_fn: syn::ItemFn = syn::parse_quote! {
            async fn handler() {
                const __AUTUMN_SECURED_ROLES: &[&str] = &["admin", "editor"];
            }
        };

        assert_eq!(
            extract_secured_roles_marker(&input_fn),
            Some(vec!["admin".to_owned(), "editor".to_owned()])
        );
    }

    #[test]
    fn secured_roles_marker_extracts_empty_roles() {
        let input_fn: syn::ItemFn = syn::parse_quote! {
            async fn handler() {
                const __AUTUMN_SECURED_ROLES: &[&str] = &[];
            }
        };

        assert_eq!(extract_secured_roles_marker(&input_fn), Some(Vec::new()));
    }

    #[test]
    fn secured_roles_marker_extracts_nested_roles() {
        let input_fn: syn::ItemFn = syn::parse_quote! {
            async fn handler() {
                {
                    const __AUTUMN_SECURED_ROLES: &[&str] = &["admin"];
                }
            }
        };

        assert_eq!(
            extract_secured_roles_marker(&input_fn),
            Some(vec!["admin".to_owned()])
        );
    }

    // ── Scopes extraction (#1158) ────────────────────────────────────────────

    #[test]
    fn extract_secured_scopes_from_attribute_scopes_only() {
        let input_fn: syn::ItemFn = syn::parse_quote! {
            #[secured(scopes = ["posts:read", "posts:write"])]
            async fn handler() {}
        };
        let attr = input_fn
            .attrs
            .iter()
            .find(|a| a.path().is_ident("secured"))
            .unwrap();
        assert_eq!(
            extract_secured_scopes(attr),
            vec!["posts:read".to_owned(), "posts:write".to_owned()]
        );
    }

    #[test]
    fn extract_secured_scopes_from_attribute_roles_and_scopes() {
        let input_fn: syn::ItemFn = syn::parse_quote! {
            #[secured("admin", scopes = ["posts:write"])]
            async fn handler() {}
        };
        let attr = input_fn
            .attrs
            .iter()
            .find(|a| a.path().is_ident("secured"))
            .unwrap();
        assert_eq!(extract_secured_scopes(attr), vec!["posts:write".to_owned()]);
    }

    #[test]
    fn extract_secured_scopes_returns_empty_for_roles_only() {
        let input_fn: syn::ItemFn = syn::parse_quote! {
            #[secured("admin")]
            async fn handler() {}
        };
        let attr = input_fn
            .attrs
            .iter()
            .find(|a| a.path().is_ident("secured"))
            .unwrap();
        assert!(extract_secured_scopes(attr).is_empty());
    }

    #[test]
    fn scopes_marker_extracted_from_handler_body() {
        let input_fn: syn::ItemFn = syn::parse_quote! {
            async fn handler() {
                const __AUTUMN_SECURED_SCOPES: &[&str] = &["posts:write", "posts:delete"];
            }
        };
        assert_eq!(
            extract_secured_scopes_marker(&input_fn),
            Some(vec!["posts:write".to_owned(), "posts:delete".to_owned()])
        );
    }

    #[test]
    fn scopes_marker_empty_array_returns_some_empty_vec() {
        let input_fn: syn::ItemFn = syn::parse_quote! {
            async fn handler() {
                const __AUTUMN_SECURED_SCOPES: &[&str] = &[];
            }
        };
        assert_eq!(
            extract_secured_scopes_marker(&input_fn),
            Some(Vec::<String>::new())
        );
    }

    #[test]
    fn scopes_marker_absent_returns_none() {
        let input_fn: syn::ItemFn = syn::parse_quote! {
            async fn handler() {}
        };
        assert_eq!(extract_secured_scopes_marker(&input_fn), None);
    }
}
