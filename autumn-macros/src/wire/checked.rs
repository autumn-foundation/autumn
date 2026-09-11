//! `#[contract_checked]` — hold a caller's call sites to the callee's contract.
//!
//! The attribute reads the function it is on. For every call through a
//! generated client it collects two sets:
//!
//! * the **read-set** — every response field the caller names, whether off a
//!   binding (`item.name`), a destructuring `let`, or the call expression
//!   itself (`…await?.name`);
//! * the **write-set** — every request field an inline struct literal sets.
//!
//! Each set becomes a const assertion against the callee's own const field
//! table, so the check is a cross-crate compile-time fact rather than a
//! snapshot: rustc rebuilds the caller whenever the callee's table changes.
//! Const-eval messages must be literals, so the macro writes them itself —
//! which is why each names the call site, the endpoint and the field.
//!
//! When the endpoint's JSON descriptor is on disk the macro also runs the full
//! check itself, which lets it name a *missing required* field the const
//! assertion can only count. The const assertions stay either way: a missing
//! descriptor must degrade the diagnostic, never the guarantee.

use std::collections::BTreeMap;

use proc_macro2::{Span, TokenStream};
use quote::{quote, quote_spanned};
use syn::visit::{self, Visit};
use syn::{Expr, Ident, ItemFn, LitStr, Path, Stmt, Token};

use crate::wire::check::{self, CallSite, ViolationKind};
use crate::wire::client::endpoint_alias_ident;
use crate::wire::store;

/// One client call found in the annotated function.
#[derive(Debug)]
struct Call {
    /// The syntax node this call was found at, used as its identity.
    node: *const syn::ExprMethodCall,
    /// The client this call goes through.
    client: usize,
    /// The generated method's name — also the endpoint's name.
    method: Ident,
    /// Where the call is written.
    span: Span,
    /// Response fields the caller names.
    reads: Vec<String>,
    /// Request fields the caller sets, or `None` when the request is not an
    /// inline struct literal.
    writes: Option<Vec<String>>,
    /// Whether the request literal names every field (no `..rest`).
    writes_exhaustive: bool,
}

/// Parsed `#[contract_checked(...)]` arguments.
struct Args {
    clients: Vec<Path>,
}

impl syn::parse::Parse for Args {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let mut clients = Vec::new();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            if key == "client" {
                clients.push(input.parse::<Path>()?);
            } else {
                return Err(syn::Error::new_spanned(
                    &key,
                    format!(
                        "unknown #[contract_checked] argument `{key}`; expected `client = \
                         <ClientType>`"
                    ),
                ));
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        if clients.is_empty() {
            return Err(syn::Error::new(
                Span::call_site(),
                "#[contract_checked] needs a client: #[contract_checked(client = CatalogClient)]",
            ));
        }
        Ok(Self { clients })
    }
}

pub fn contract_checked_macro(attr: TokenStream, item: &TokenStream) -> TokenStream {
    match expand(attr, item) {
        Ok(ts) => ts,
        Err(err) => {
            let err = err.to_compile_error();
            quote! { #err #item }
        }
    }
}

fn expand(attr: TokenStream, item: &TokenStream) -> Result<TokenStream, syn::Error> {
    let args: Args = syn::parse2(attr)?;
    let mut func: ItemFn = syn::parse2(item.clone())?;
    let caller = func.sig.ident.to_string();

    let client_idents: Vec<Ident> = args.clients.iter().map(|p| last_ident(p).clone()).collect();
    let bindings = bindings_for(&func, &client_idents);

    // A client with no binding here means the annotation is aimed at the wrong
    // function, or the client is reached through a shape this attribute cannot
    // see. Either way, silently checking nothing is the one outcome that must
    // not happen.
    for (index, client) in args.clients.iter().enumerate() {
        if !bindings.values().any(|c| *c == index) {
            return Err(syn::Error::new_spanned(
                client,
                format!(
                    "#[contract_checked] found no `{}` value in `{caller}` to check. Name it in \
                     the signature (`catalog: {0}`), in a typed `let`, or build it with \
                     `{0}::new(…)`.",
                    client_idents[index]
                ),
            ));
        }
    }

    let calls = collect_calls(&func, &bindings);
    let mut items = Vec::new();
    for call in &calls {
        items.extend(assertions_for(call, &args.clients[call.client], &caller));
    }

    let stmts: Vec<Stmt> = items
        .into_iter()
        .map(|ts| syn::parse2::<Stmt>(ts).expect("generated const item parses"))
        .collect();
    func.block.stmts.splice(0..0, stmts);
    Ok(quote! { #func })
}

/// The last segment of a path — a client's or marker's own name.
fn last_ident(path: &Path) -> &Ident {
    &path
        .segments
        .last()
        .expect("a parsed path has at least one segment")
        .ident
}

/// Every local name that holds one of the declared clients, mapped to its index.
///
/// Three shapes are recognised, covering how a client actually reaches a
/// handler: a typed parameter, a typed `let`, and a `let` initialised from one
/// of the client's own associated functions.
fn bindings_for(func: &ItemFn, clients: &[Ident]) -> BTreeMap<String, usize> {
    let mut found = BTreeMap::new();
    let index_of = |ty: &syn::Type| -> Option<usize> {
        let syn::Type::Path(tp) = strip_refs(ty) else {
            return None;
        };
        let last = &tp.path.segments.last()?.ident;
        clients.iter().position(|c| c == last)
    };

    for arg in &func.sig.inputs {
        if let syn::FnArg::Typed(pat) = arg
            && let syn::Pat::Ident(ident) = pat.pat.as_ref()
            && let Some(index) = index_of(&pat.ty)
        {
            found.insert(ident.ident.to_string(), index);
        }
    }

    let mut locals = Locals {
        clients,
        found: &mut found,
    };
    locals.visit_block(&func.block);
    found
}

/// Visitor half of [`bindings_for`]: the `let` shapes that name a client.
struct Locals<'a> {
    clients: &'a [Ident],
    found: &'a mut BTreeMap<String, usize>,
}
impl Visit<'_> for Locals<'_> {
    fn visit_local(&mut self, local: &syn::Local) {
        visit::visit_local(self, local);
        let syn::Pat::Ident(ident) = strip_pat_type(&local.pat) else {
            return;
        };
        let name = ident.ident.to_string();
        if let syn::Pat::Type(pt) = &local.pat
            && let syn::Type::Path(tp) = strip_refs(&pt.ty)
            && let Some(last) = tp.path.segments.last()
            && let Some(index) = self.clients.iter().position(|c| *c == last.ident)
        {
            self.found.insert(name, index);
            return;
        }
        // `let catalog = CatalogClient::new(…);` — the constructor names
        // the type even though the binding does not.
        if let Some(init) = &local.init
            && let Some(index) = constructor_client(&init.expr, self.clients)
        {
            self.found.insert(name, index);
        }
    }
}

/// The client index when `expr` calls one of a client's associated functions.
fn constructor_client(expr: &Expr, clients: &[Ident]) -> Option<usize> {
    let Expr::Call(call) = peel(expr) else {
        return None;
    };
    let Expr::Path(path) = call.func.as_ref() else {
        return None;
    };
    let segments = &path.path.segments;
    if segments.len() < 2 {
        return None;
    }
    let owner = &segments[segments.len() - 2].ident;
    clients.iter().position(|c| c == owner)
}

/// Peel the wrappers that sit between a call and the value it produces.
fn peel(expr: &Expr) -> &Expr {
    match expr {
        Expr::Await(e) => peel(&e.base),
        Expr::Try(e) => peel(&e.expr),
        Expr::Paren(e) => peel(&e.expr),
        Expr::Group(e) => peel(&e.expr),
        Expr::Reference(e) => peel(&e.expr),
        Expr::Unary(e) if matches!(e.op, syn::UnOp::Deref(_)) => peel(&e.expr),
        // `.unwrap()` / `.expect(…)` on the result of a call, which is still
        // that call's value.
        Expr::MethodCall(e) if e.method == "unwrap" || e.method == "expect" => peel(&e.receiver),
        other => other,
    }
}

/// Strip `&`/`&mut` from a type.
fn strip_refs(ty: &syn::Type) -> &syn::Type {
    match ty {
        syn::Type::Reference(r) => strip_refs(&r.elem),
        syn::Type::Paren(p) => strip_refs(&p.elem),
        other => other,
    }
}

/// Look through a `let x: T` pattern to the name it binds.
fn strip_pat_type(pat: &syn::Pat) -> &syn::Pat {
    match pat {
        syn::Pat::Type(p) => strip_pat_type(&p.pat),
        other => other,
    }
}

/// Find every client call in the function, with its read- and write-set.
fn collect_calls(func: &ItemFn, bindings: &BTreeMap<String, usize>) -> Vec<Call> {
    let mut calls = Vec::new();
    Collector {
        bindings,
        calls: &mut calls,
    }
    .visit_block(&func.block);

    // A `let` whose initializer is a client call names the response; every
    // `binding.field` in the function is then a read of that call.
    let mut response_bindings: BTreeMap<String, usize> = BTreeMap::new();
    let mut destructured: Vec<(usize, Vec<String>)> = Vec::new();
    LetBinder {
        bindings,
        calls: &calls,
        response_bindings: &mut response_bindings,
        destructured: &mut destructured,
    }
    .visit_block(&func.block);
    for (index, fields) in destructured {
        calls[index].reads.extend(fields);
    }

    let mut reads: Vec<(usize, String)> = Vec::new();
    FieldReader {
        bindings,
        calls: &calls,
        response_bindings: &response_bindings,
        reads: &mut reads,
    }
    .visit_block(&func.block);
    for (index, field) in reads {
        calls[index].reads.push(field);
    }

    for call in &mut calls {
        call.reads.sort();
        call.reads.dedup();
    }
    calls
}

/// Pass one: every method call whose receiver is a client binding.
struct Collector<'a> {
    bindings: &'a BTreeMap<String, usize>,
    calls: &'a mut Vec<Call>,
}

impl Visit<'_> for Collector<'_> {
    fn visit_expr_method_call(&mut self, call: &syn::ExprMethodCall) {
        visit::visit_expr_method_call(self, call);
        let Some(client) = receiver_binding(&call.receiver, self.bindings) else {
            return;
        };
        if CLIENT_OWN_METHODS.contains(&call.method.to_string().as_str()) {
            return;
        }
        let (writes, writes_exhaustive) = request_shape(call.args.last());
        self.calls.push(Call {
            node: std::ptr::from_ref(call),
            client,
            method: call.method.clone(),
            // The method name, not the whole expression: a multi-line call
            // chain's own span points at the receiver, which says nothing
            // about which call is at fault.
            span: call.method.span(),
            reads: Vec::new(),
            writes,
            writes_exhaustive,
        });
    }
}

/// Methods the generated client owns that are not endpoints. A call to one of
/// these has no contract to check.
const CLIENT_OWN_METHODS: [&str; 6] = ["new", "base_url", "clone", "to_owned", "eq", "fmt"];

/// The client index when `expr` is a plain path naming a client binding.
fn receiver_binding(expr: &Expr, bindings: &BTreeMap<String, usize>) -> Option<usize> {
    let Expr::Path(path) = peel(expr) else {
        return None;
    };
    let ident = path.path.get_ident()?;
    bindings.get(&ident.to_string()).copied()
}

/// The write-set of a request argument, and whether it is exhaustive.
fn request_shape(arg: Option<&Expr>) -> (Option<Vec<String>>, bool) {
    let Some(Expr::Struct(lit)) = arg.map(peel) else {
        return (None, false);
    };
    let fields = lit
        .fields
        .iter()
        .filter_map(|f| match &f.member {
            syn::Member::Named(ident) => Some(ident.to_string()),
            syn::Member::Unnamed(_) => None,
        })
        .collect();
    (Some(fields), lit.rest.is_none())
}

/// Pass two: bind `let` names (and destructured fields) to the call they came
/// from.
struct LetBinder<'a> {
    bindings: &'a BTreeMap<String, usize>,
    calls: &'a [Call],
    response_bindings: &'a mut BTreeMap<String, usize>,
    destructured: &'a mut Vec<(usize, Vec<String>)>,
}

impl Visit<'_> for LetBinder<'_> {
    fn visit_local(&mut self, local: &syn::Local) {
        visit::visit_local(self, local);
        let Some(init) = &local.init else {
            return;
        };
        let Some(index) = call_index(&init.expr, self.bindings, self.calls) else {
            return;
        };
        match strip_pat_type(&local.pat) {
            syn::Pat::Ident(ident) => {
                self.response_bindings
                    .insert(ident.ident.to_string(), index);
            }
            syn::Pat::Struct(pat) => {
                let fields = pat
                    .fields
                    .iter()
                    .filter_map(|f| match &f.member {
                        syn::Member::Named(ident) => Some(ident.to_string()),
                        syn::Member::Unnamed(_) => None,
                    })
                    .collect();
                self.destructured.push((index, fields));
            }
            _ => {}
        }
    }
}

/// The index of the client call `expr` evaluates to, if it is one.
fn call_index(expr: &Expr, bindings: &BTreeMap<String, usize>, calls: &[Call]) -> Option<usize> {
    let Expr::MethodCall(call) = peel(expr) else {
        return None;
    };
    receiver_binding(&call.receiver, bindings)?;
    let node = std::ptr::from_ref(call);
    calls.iter().position(|c| c.node == node)
}

/// Pass three: every `x.field` that reads a response.
struct FieldReader<'a> {
    bindings: &'a BTreeMap<String, usize>,
    calls: &'a [Call],
    response_bindings: &'a BTreeMap<String, usize>,
    reads: &'a mut Vec<(usize, String)>,
}

impl Visit<'_> for FieldReader<'_> {
    /// A macro body is an unexpanded token stream, so `visit_expr_field` never
    /// reaches it — and in an Autumn app most response fields are read inside
    /// one (`html! { (item.name) }`, `format!("{}", item.name)`). Scan the
    /// tokens for `binding . field` instead.
    fn visit_macro(&mut self, mac: &syn::Macro) {
        visit::visit_macro(self, mac);
        scan_tokens(mac.tokens.clone(), self.response_bindings, self.reads);
    }

    fn visit_expr_field(&mut self, field: &syn::ExprField) {
        visit::visit_expr_field(self, field);
        let syn::Member::Named(name) = &field.member else {
            return;
        };
        let base = peel(&field.base);
        // `item.name`, where `item` came out of a client call.
        if let Expr::Path(path) = base
            && let Some(ident) = path.path.get_ident()
            && let Some(index) = self.response_bindings.get(&ident.to_string())
        {
            self.reads.push((*index, name.to_string()));
            return;
        }
        // `catalog.get_item(…).await?.name`, with no binding in between.
        if let Some(index) = call_index(base, self.bindings, self.calls) {
            self.reads.push((index, name.to_string()));
        }
    }
}

/// Record every `binding.field` in a macro's tokens as a read.
///
/// Token-level, because the body is not parsed Rust. Two shapes that look the
/// same are excluded: `binding.method(…)` (a call, not a field) and
/// `other.binding.field` (where `binding` is itself a field).
fn scan_tokens(
    tokens: proc_macro2::TokenStream,
    response_bindings: &BTreeMap<String, usize>,
    reads: &mut Vec<(usize, String)>,
) {
    let tts: Vec<proc_macro2::TokenTree> = tokens.into_iter().collect();
    for (i, tt) in tts.iter().enumerate() {
        if let proc_macro2::TokenTree::Group(group) = tt {
            scan_tokens(group.stream(), response_bindings, reads);
            continue;
        }
        let proc_macro2::TokenTree::Ident(base) = tt else {
            continue;
        };
        if matches!(tts.get(i.wrapping_sub(1)), Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == '.')
        {
            continue;
        }
        let Some(index) = response_bindings.get(&base.to_string()) else {
            continue;
        };
        if !matches!(tts.get(i + 1), Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == '.') {
            continue;
        }
        let Some(proc_macro2::TokenTree::Ident(field)) = tts.get(i + 2) else {
            continue;
        };
        if field == "await" {
            continue;
        }
        if matches!(
            tts.get(i + 3),
            Some(proc_macro2::TokenTree::Group(g))
                if g.delimiter() == proc_macro2::Delimiter::Parenthesis
        ) {
            continue;
        }
        reads.push((*index, field.to_string()));
    }
}

/// The const assertions (and any richer diagnostic) for one call site.
fn assertions_for(call: &Call, client: &Path, caller: &str) -> Vec<TokenStream> {
    let span = call.span;
    let method = &call.method;
    // The alias sits beside the client, so the user's own path to the client
    // also reaches it — including `crate::api::CatalogClient`.
    let endpoint_path = {
        let mut path = client.clone();
        let alias = endpoint_alias_ident(last_ident(client), method);
        let last = path
            .segments
            .last_mut()
            .expect("a parsed path has at least one segment");
        last.ident = alias;
        last.arguments = syn::PathArguments::None;
        quote_spanned! { span => #path }
    };
    let snippet = snippet_of(method);
    let site = CallSite {
        caller: caller.to_owned(),
        snippet: snippet.clone(),
        method: method.to_string(),
        reads: call.reads.clone(),
        writes: call.writes.clone(),
        writes_exhaustive: call.writes_exhaustive,
    };
    let named = named_violations(&site);

    // The descriptor already said which field is at fault, with a message no
    // const assertion could compose. Report those, and skip the assertions
    // that would repeat them.
    let mut out: Vec<TokenStream> = named
        .iter()
        .map(|(_, message)| {
            let message = LitStr::new(message, span);
            quote_spanned! { span => const _: () = ::core::compile_error!(#message); }
        })
        .collect();
    let reported = |kind: &ViolationKind| named.iter().any(|(k, _)| k == kind);

    for read in &call.reads {
        if reported(&ViolationKind::ResponseFieldMissing(read.clone())) {
            continue;
        }
        out.push(field_assertion(
            &endpoint_path,
            span,
            &quote_spanned! { span => RESPONSE_FIELDS },
            read,
            &format!(
                "wire contract broken in `{caller}` at `{snippet}`: reads response field \
                 `{read}` from endpoint `{method}`, which it no longer produces"
            ),
        ));
    }

    let Some(writes) = &call.writes else {
        return out;
    };
    for write in writes {
        if reported(&ViolationKind::RequestFieldUnknown(write.clone())) {
            continue;
        }
        out.push(field_assertion(
            &endpoint_path,
            span,
            &quote_spanned! { span => REQUEST_FIELDS },
            write,
            &format!(
                "wire contract broken in `{caller}` at `{snippet}`: sets request field \
                 `{write}` on endpoint `{method}`, which does not accept it"
            ),
        ));
    }

    // A `..rest` initializer is the one request shape that can omit a required
    // field without the type checker noticing.
    let missing_reported = named
        .iter()
        .any(|(k, _)| matches!(k, ViolationKind::RequestFieldMissing(_)));
    if !call.writes_exhaustive && !missing_reported {
        let supplied: Vec<LitStr> = writes.iter().map(|w| LitStr::new(w, span)).collect();
        let message = LitStr::new(
            &format!(
                "wire contract broken in `{caller}` at `{snippet}`: builds the request for \
                 endpoint `{method}` with a `..rest` initializer that supplies only [{}], and the \
                 endpoint requires a field outside that set",
                writes.join(", ")
            ),
            span,
        );
        out.push(quote_spanned! { span =>
            const _: () = ::core::assert!(
                ::autumn_web::wire::required_covered(
                    <#endpoint_path as ::autumn_web::wire::Endpoint>::REQUEST_FIELDS,
                    &[#(#supplied),*],
                ),
                #message
            );
        });
    }
    out
}

/// One `const _: () = assert!(has_field(…))` against an endpoint's field table.
fn field_assertion(
    endpoint_path: &TokenStream,
    span: Span,
    table: &TokenStream,
    field: &str,
    message: &str,
) -> TokenStream {
    let field = LitStr::new(field, span);
    let message = LitStr::new(message, span);
    quote_spanned! { span =>
        const _: () = ::core::assert!(
            ::autumn_web::wire::has_field(
                <#endpoint_path as ::autumn_web::wire::Endpoint>::#table,
                #field,
            ),
            #message
        );
    }
}

/// Violations the on-disk descriptor can name, if exactly one endpoint matches.
///
/// Only an endpoint from *another* crate is consulted. Cargo builds a
/// dependency before its dependant, so a cross-crate descriptor is always the
/// one that was just written; a descriptor for the crate being compiled right
/// now is being written by this same run and may be half-written or left over.
/// Ambiguity (two crates with the same marker name) and absence both yield
/// nothing, leaving the const assertions to hold the contract on their own.
fn named_violations(site: &CallSite) -> Vec<(ViolationKind, String)> {
    let Some(dir) = store::contract_dir() else {
        return Vec::new();
    };
    let this_crate = std::env::var("CARGO_PKG_NAME").unwrap_or_default();
    let found: Vec<_> = store::find_by_ident(&dir, &format!("{}_endpoint", site.method))
        .into_iter()
        .filter(|e| e.endpoint.krate != this_crate)
        .collect();
    let [endpoint] = found.as_slice() else {
        return Vec::new();
    };
    // An unresolved shape describes nothing, so it can only produce noise.
    if endpoint.response.serialized.is_empty() && endpoint.request.deserialized.is_empty() {
        return Vec::new();
    }
    check::check(site, endpoint)
        .into_iter()
        .map(|v| (v.kind.clone(), v.message()))
        .collect()
}

/// How a diagnostic names the offending call. The compiler adds the file and
/// line from the span each assertion carries, so this only has to say which
/// call in the function is at fault.
fn snippet_of(method: &Ident) -> String {
    format!("{method}(…)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand_str(attr: &str, item: &str) -> String {
        let attr: TokenStream = attr.parse().expect("attr parses");
        let item: TokenStream = item.parse().expect("item parses");
        contract_checked_macro(attr, &item).to_string()
    }

    fn calls_of(item: &str) -> Vec<Call> {
        let func: ItemFn = syn::parse_str(item).expect("fixture parses");
        let clients = vec![Ident::new("CatalogClient", Span::call_site())];
        let bindings = bindings_for(&func, &clients);
        collect_calls(&func, &bindings)
    }

    #[test]
    fn a_field_read_off_the_response_binding_is_in_the_read_set() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); let _ = item.name; }",
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].reads, ["name"]);
    }

    #[test]
    fn a_field_read_straight_off_the_call_is_in_the_read_set() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) -> Result<()> { let _ = catalog.get_item(&id, NoBody).await?.name; Ok(()) }",
        );
        assert_eq!(calls[0].reads, ["name"]);
    }

    #[test]
    fn a_destructuring_let_names_the_fields_it_reads() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let Item { id, name, .. } = catalog.get_item(&x, NoBody).await.unwrap(); }",
        );
        assert_eq!(calls[0].reads, ["id", "name"]);
    }

    #[test]
    fn a_client_built_in_the_body_is_recognised() {
        let calls = calls_of(
            "async fn page(http: Client) { let catalog = CatalogClient::new(url, http); let item = catalog.get_item(&id, NoBody).await.unwrap(); let _ = item.name; }",
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].reads, ["name"]);
    }

    #[test]
    fn a_typed_let_is_recognised() {
        let calls = calls_of(
            "async fn page() { let catalog: CatalogClient = build(); let _ = catalog.get_item(&id, NoBody); }",
        );
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn an_inline_request_literal_gives_the_write_set() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let _ = catalog.create_item(NewItem { name, price_cents }); }",
        );
        assert_eq!(
            calls[0].writes.as_deref(),
            Some(["name".to_owned(), "price_cents".to_owned()].as_slice())
        );
        assert!(calls[0].writes_exhaustive);
    }

    #[test]
    fn a_rest_initializer_is_marked_non_exhaustive() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let _ = catalog.create_item(NewItem { name, ..Default::default() }); }",
        );
        assert_eq!(
            calls[0].writes.as_deref(),
            Some(["name".to_owned()].as_slice())
        );
        assert!(!calls[0].writes_exhaustive);
    }

    #[test]
    fn a_request_that_is_not_a_literal_has_an_unknown_write_set() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let _ = catalog.create_item(body); }",
        );
        assert!(calls[0].writes.is_none());
    }

    #[test]
    fn a_field_read_inside_a_macro_body_is_in_the_read_set() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); html! { h1 { (item.name) } p { (item.price_cents) } } }",
        );
        assert_eq!(calls[0].reads, ["name", "price_cents"]);
    }

    #[test]
    fn a_method_call_inside_a_macro_body_is_not_a_field_read() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); html! { (item.clone()) (item.name) (item.await) } }",
        );
        assert_eq!(calls[0].reads, ["name"]);
    }

    #[test]
    fn a_field_of_a_field_inside_a_macro_body_is_not_read_as_the_binding() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); html! { (other.item.name) } }",
        );
        assert!(calls[0].reads.is_empty(), "{:?}", calls[0].reads);
    }

    #[test]
    fn a_call_on_something_that_is_not_a_client_is_ignored() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient, other: Vec<u8>) { let _ = other.len(); }",
        );
        assert!(calls.is_empty());
    }

    #[test]
    fn two_calls_keep_separate_read_sets() {
        let calls = calls_of(
            "async fn page(catalog: CatalogClient) { let a = catalog.get_item(&x, NoBody).await.unwrap(); let b = catalog.list_items(NoBody).await.unwrap(); let _ = (a.name, b.total); }",
        );
        assert_eq!(calls.len(), 2);
        let by_method: Vec<(String, Vec<String>)> = calls
            .iter()
            .map(|c| (c.method.to_string(), c.reads.clone()))
            .collect();
        assert!(
            by_method.contains(&("get_item".to_owned(), vec!["name".to_owned()])),
            "{by_method:?}"
        );
        assert!(
            by_method.contains(&("list_items".to_owned(), vec!["total".to_owned()])),
            "{by_method:?}"
        );
    }

    #[test]
    fn the_expansion_emits_a_const_assertion_per_read() {
        let out = expand_str(
            "client = CatalogClient",
            "async fn page(catalog: CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); let _ = item.name; }",
        );
        assert!(
            out.contains("__autumn_wire_ep_CatalogClient_get_item"),
            "{out}"
        );
        assert!(out.contains("RESPONSE_FIELDS"), "{out}");
        assert!(out.contains("reads response field `name`"), "{out}");
    }

    #[test]
    fn the_expansion_emits_a_required_covered_assertion_for_a_rest_initializer() {
        let out = expand_str(
            "client = CatalogClient",
            "async fn page(catalog: CatalogClient) { let _ = catalog.create_item(NewItem { name, ..Default::default() }); }",
        );
        assert!(out.contains("required_covered"), "{out}");
        assert!(out.contains("REQUEST_FIELDS"), "{out}");
    }

    #[test]
    fn an_exhaustive_literal_gets_no_required_covered_assertion() {
        let out = expand_str(
            "client = CatalogClient",
            "async fn page(catalog: CatalogClient) { let _ = catalog.create_item(NewItem { name }); }",
        );
        assert!(!out.contains("required_covered"), "{out}");
        assert!(out.contains("sets request field `name`"), "{out}");
    }

    #[test]
    fn a_client_with_no_binding_in_the_function_is_refused() {
        let out = expand_str("client = CatalogClient", "async fn page() { let _ = 1; }");
        assert!(out.contains("found no `CatalogClient` value"), "{out}");
    }

    #[test]
    fn a_missing_client_argument_is_refused() {
        let out = expand_str("", "async fn page() {}");
        assert!(out.contains("needs a client"), "{out}");
    }

    #[test]
    fn an_unknown_argument_is_refused_rather_than_ignored() {
        let out = expand_str("clietn = CatalogClient", "async fn page() {}");
        assert!(
            out.contains("unknown #[contract_checked] argument"),
            "{out}"
        );
    }

    #[test]
    fn a_qualified_client_path_keeps_its_module_prefix() {
        let out = expand_str(
            "client = crate::api::CatalogClient",
            "async fn page(catalog: crate::api::CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); let _ = item.name; }",
        );
        assert!(
            out.contains("crate :: api :: __autumn_wire_ep_CatalogClient_get_item"),
            "{out}"
        );
    }

    #[test]
    fn the_original_body_is_preserved() {
        let out = expand_str(
            "client = CatalogClient",
            "async fn page(catalog: CatalogClient) { let item = catalog.get_item(&id, NoBody).await.unwrap(); item.name }",
        );
        assert!(out.contains("item . name"), "{out}");
    }
}
